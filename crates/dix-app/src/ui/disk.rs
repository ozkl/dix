//! The partition editor ("Partitions…" in the open dialog): a bar and a
//! list of the disk's partitions and free space, forms to create, change,
//! format and delete them, the list of changes, and Apply.

use super::{format_size, theme, Action};
use crate::model::disk::{formatters, DiskEditor, FormatPlan, Formatter};
use crate::model::Workspace;
use dix_core::partition::edit::{Free, TypeCode, KNOWN_TYPES};
use dix_core::partition::Scheme;
use dix_drivers::recover::partitions::Lost;
use eframe::egui::{self, Button, Color32, RichText, Sense, Stroke, StrokeKind, Ui};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

/// What is selected in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Partition(u32),
    /// Free space starting at this sector.
    Free(u64),
}

/// A form being filled in.
pub enum Form {
    New { free: Free, size_mib: f64, type_idx: Option<usize>, name: String, format: Option<usize>, label: String },
    Format { number: u32, format: usize, label: String },
    Name { number: u32, name: String },
    Type { number: u32, type_idx: usize },
}

pub struct DiskDialog {
    pub path: String,
    pub editor: Result<DiskEditor, String>,
    pub selected: Option<Selection>,
    pub form: Option<Form>,
    /// Showing the "write these changes?" confirmation.
    pub confirm: bool,
    /// Apply running in the background.
    pub applying: Option<Receiver<Result<Vec<String>, String>>>,
    /// Why the last action failed.
    pub error: Option<String>,
    /// What the last apply said.
    pub notes: Vec<String>,
    /// The last apply was refused because the operating system has volumes
    /// of the disk mounted: offer to unmount them.
    pub system_uses: bool,
    /// An image in memory (an upload, as in the browser): its name and
    /// bytes, edited here instead of a host disk.
    pub memory: Option<(String, Vec<u8>)>,
    /// Looking for lost partitions, or what was found.
    pub lost: Option<LostSearch>,
}

/// A search for lost partitions: running (progress, stop) or done.
pub struct LostSearch {
    pub done: Arc<AtomicU64>,
    pub total: u64,
    pub cancel: Arc<AtomicBool>,
    pub rx: Option<Receiver<Result<Vec<Lost>, String>>>,
    pub found: Option<Result<Vec<Lost>, String>>,
    /// Starts of those restored (queued) already.
    pub restored: Vec<u64>,
}

impl DiskDialog {
    /// The partitions of an image in memory (`name`, its bytes).
    pub fn open_memory(ws: &Workspace, name: String, data: Vec<u8>) -> DiskDialog {
        let mut d = DiskDialog::open_with(Err(String::new()), &name);
        d.memory = Some((name, data));
        d.editor = d.read(ws);
        d
    }

    pub fn open(ws: &Workspace, path: &str) -> DiskDialog {
        let editor = open_editor(ws, path);
        DiskDialog::open_with(editor, path)
    }

    /// The disk again, as it is now.
    pub fn read(&self, ws: &Workspace) -> Result<DiskEditor, String> {
        match &self.memory {
            Some((name, data)) => {
                let dev = dix_core::device::MemoryDevice::new(name.clone(), data.clone()).with_read_only(true);
                ws.registry
                    .open_disk(Box::new(dev), None)
                    .and_then(|dev| DiskEditor::read(&ws.registry, std::path::Path::new(name), dev))
                    .map_err(|e| e.to_string())
            }
            None => open_editor(ws, &self.path),
        }
    }

    /// The disk to read (read-only): the image in memory, or the host
    /// disk or file.
    pub fn device(&self, ws: &Workspace) -> dix_core::Result<Box<dyn dix_core::BlockDevice>> {
        match &self.memory {
            Some((name, data)) => {
                let dev = dix_core::device::MemoryDevice::new(name.clone(), data.clone()).with_read_only(true);
                ws.registry.open_disk(Box::new(dev), None)
            }
            None => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    ws.host_reader(std::path::Path::new(&self.path))
                }
                #[cfg(target_arch = "wasm32")]
                Err(dix_core::Error::unsupported("host disks in the browser"))
            }
        }
    }

    /// Starts looking for lost partitions in the space no partition uses
    /// (the whole disk without a table).
    fn find_lost(&mut self, ws: &Workspace) {
        let regions: Vec<(u64, u64)> = match &self.editor {
            Ok(e) => match &e.original {
                Some(l) => l.free().iter().map(|f| (f.first * l.sector_size, (f.last + 1) * l.sector_size)).collect(),
                None => vec![(0, e.size)],
            },
            Err(_) => return,
        };
        let dev = match self.device(ws) {
            Ok(d) => d,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        let total: u64 = regions.iter().map(|(a, b)| b - a).sum();
        let done = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let (d2, c2) = (done.clone(), cancel.clone());
        let ctx = ws_repaint(ws);
        let work = move || {
            let mut report = |at: u64, _: u64| {
                d2.store(at, Ordering::Relaxed);
                !c2.load(Ordering::Relaxed)
            };
            let r = dix_drivers::recover::partitions::find(dev, &regions, &mut report).map_err(|e| e.to_string());
            let _ = tx.send(r);
            ctx();
        };
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::spawn(work);
        #[cfg(target_arch = "wasm32")]
        work();
        self.lost = Some(LostSearch { done, total, cancel, rx: Some(rx), found: None, restored: Vec::new() });
    }

    fn open_with(editor: Result<DiskEditor, String>, path: &str) -> DiskDialog {
        DiskDialog {
            path: path.to_owned(),
            editor,
            selected: None,
            form: None,
            confirm: false,
            applying: None,
            system_uses: false,
            memory: None,
            lost: None,
            error: None,
            notes: Vec::new(),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn open_editor(ws: &Workspace, path: &str) -> Result<DiskEditor, String> {
    let p = std::path::Path::new(path);
    ws.host_reader(p).and_then(|dev| DiskEditor::read(&ws.registry, p, dev)).map_err(|e| e.to_string())
}

#[cfg(target_arch = "wasm32")]
fn open_editor(_: &Workspace, _: &str) -> Result<DiskEditor, String> {
    Err("partitions can only be edited in the desktop app".into())
}

/// A row of the list: a partition or free space, indented inside the
/// extended partition.
struct Row {
    sel: Selection,
    first: u64,
    last: u64,
    logical: bool,
}

fn rows(e: &DiskEditor) -> Vec<Row> {
    let Some(l) = &e.layout else { return Vec::new() };
    let mut out: Vec<Row> = l
        .slots
        .iter()
        .map(|s| Row { sel: Selection::Partition(s.number), first: s.first, last: s.last, logical: s.is_logical() })
        .chain(l.free().into_iter().map(|f| Row {
            sel: Selection::Free(f.first),
            first: f.first,
            last: f.last,
            logical: f.logical,
        }))
        .collect();
    // Disk order; the extended partition before what is inside it.
    out.sort_by_key(|r| (r.first, r.logical));
    out
}

fn color_of(t: TypeCode, fs: Option<&str>) -> Color32 {
    let name = format!("{} {}", t.name(), fs.unwrap_or("")).to_lowercase();
    if t.is_extended() {
        Color32::from_rgb(70, 70, 70)
    } else if name.contains("swap") {
        Color32::from_rgb(170, 80, 70)
    } else if name.contains("efi") {
        Color32::from_rgb(200, 140, 50)
    } else if name.contains("linux") || name.contains("ext") {
        Color32::from_rgb(70, 150, 100)
    } else if ["windows", "microsoft", "fat", "ntfs"].iter().any(|w| name.contains(w)) {
        Color32::from_rgb(60, 120, 190)
    } else if name.contains("apple") {
        Color32::from_rgb(150, 150, 160)
    } else {
        Color32::from_rgb(130, 100, 160)
    }
}

/// The partitions as a bar, proportional to their size (at least a few
/// pixels each). Clicking selects.
fn bar(ui: &mut Ui, e: &DiskEditor, sel: &mut Option<Selection>) {
    let Some(l) = &e.layout else { return };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 4.0, theme::FIELD_BG);
    let x = |s: u64| rect.left() + (s as f64 / l.sectors.max(1) as f64 * rect.width() as f64) as f32;
    for r in rows(e) {
        let (mut x0, mut x1) = (x(r.first), x(r.last + 1));
        if x1 - x0 < 4.0 {
            x1 = x0 + 4.0;
        }
        let (y0, y1) =
            if r.logical { (rect.top() + 6.0, rect.bottom() - 6.0) } else { (rect.top() + 2.0, rect.bottom() - 2.0) };
        x0 += 1.0;
        let seg = egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1 - 1.0, y1));
        let resp = ui.interact(seg, ui.scope_id().with(("seg", r.first, r.logical)), Sense::click());
        let fill = match r.sel {
            Selection::Free(_) => Color32::from_rgb(45, 45, 45),
            Selection::Partition(n) => {
                let s = l.get(n).unwrap();
                color_of(s.type_code, e.file_system(s.first).as_deref())
            }
        };
        p.rect_filled(seg, 3.0, if resp.hovered() { fill.gamma_multiply(1.25) } else { fill });
        if *sel == Some(r.sel) {
            p.rect_stroke(seg, 3.0, Stroke::new(2.0, Color32::WHITE), StrokeKind::Inside);
        }
        if seg.width() > 40.0 {
            let text = match r.sel {
                Selection::Partition(n) => format!("{n}"),
                Selection::Free(_) => "free".into(),
            };
            p.text(seg.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(12.0), Color32::WHITE);
        }
        if resp.clicked() {
            *sel = Some(r.sel);
        }
        let ss = l.sector_size;
        resp.on_hover_text(match r.sel {
            Selection::Partition(n) => format!("Partition {n}: {}", format_size((r.last - r.first + 1) * ss)),
            Selection::Free(_) => format!("Free: {}", format_size((r.last - r.first + 1) * ss)),
        });
    }
}

fn table(ui: &mut Ui, e: &DiskEditor, sel: &mut Option<Selection>) {
    let Some(l) = &e.layout else { return };
    let ss = l.sector_size;
    egui::ScrollArea::vertical().id_salt("disk_rows").max_height(220.0).show(ui, |ui| {
        egui::Grid::new("disk_grid").num_columns(6).spacing([14.0, 4.0]).striped(true).show(ui, |ui| {
            for h in ["#", "Type", "Name", "File system", "Size", "Start"] {
                ui.label(RichText::new(h).color(theme::TEXT_DIM));
            }
            ui.end_row();
            for r in rows(e) {
                let indent = if r.logical { "    " } else { "" };
                let selected = *sel == Some(r.sel);
                let size = format_size((r.last - r.first + 1) * ss);
                let start = format_size(r.first * ss);
                match r.sel {
                    Selection::Partition(n) => {
                        let s = l.get(n).unwrap();
                        let mut num = format!("{indent}{n}");
                        if s.bootable() {
                            num.push_str(" ★");
                        }
                        if ui.selectable_label(selected, num).clicked() {
                            *sel = Some(r.sel);
                        }
                        ui.label(s.type_code.name());
                        ui.label(&s.name);
                        ui.label(e.file_system(s.first).unwrap_or_else(|| {
                            if s.is_extended() {
                                "—".into()
                            } else {
                                String::new()
                            }
                        }));
                        ui.label(size);
                        ui.label(RichText::new(start).color(theme::TEXT_DIM));
                    }
                    Selection::Free(_) => {
                        if ui
                            .selectable_label(selected, RichText::new(format!("{indent}—")).color(theme::TEXT_DIM))
                            .clicked()
                        {
                            *sel = Some(r.sel);
                        }
                        let what = if r.logical { "Free (logical)" } else { "Free space" };
                        ui.label(RichText::new(what).color(theme::TEXT_DIM));
                        ui.label("");
                        ui.label("");
                        ui.label(RichText::new(size).color(theme::TEXT_DIM));
                        ui.label(RichText::new(start).color(theme::TEXT_DIM));
                    }
                }
                ui.end_row();
            }
        });
    });
}

/// The types that fit the table, in the order offered.
fn types(scheme: Scheme) -> Vec<(String, TypeCode)> {
    KNOWN_TYPES.iter().filter_map(|t| Some((t.name.to_owned(), t.code(scheme)?))).collect()
}

fn format_combo(ui: &mut Ui, id: &str, fmts: &[Formatter], choice: &mut Option<usize>, allow_none: bool) {
    let text = choice.map_or("Don't format".to_owned(), |i| fmts[i].name.clone());
    egui::ComboBox::from_id_salt(id).selected_text(text).show_ui(ui, |ui| {
        if allow_none {
            ui.selectable_value(choice, None, "Don't format");
        }
        for (i, f) in fmts.iter().enumerate() {
            ui.selectable_value(choice, Some(i), &f.name);
        }
    });
}

fn plan(f: &Formatter, label: &str) -> FormatPlan {
    FormatPlan { driver: f.driver.clone(), variant: f.variant.clone(), label: label.to_owned(), name: f.name.clone() }
}

/// The form in progress; returns true when it is done (or cancelled).
fn form_ui(
    ui: &mut Ui,
    e: &mut DiskEditor,
    form: &mut Form,
    fmts: &[Formatter],
    error: &mut Option<String>,
    select: &mut Option<Selection>,
) -> bool {
    let Some(scheme) = e.layout.as_ref().map(|l| l.scheme) else { return true };
    let tys = types(scheme);
    let mut done = false;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        match form {
            Form::New { free, size_mib, type_idx, name, format, label } => {
                let ss = e.layout.as_ref().unwrap().sector_size;
                let max = (free.sectors() * ss) as f64 / (1 << 20) as f64;
                ui.label(RichText::new(if free.logical { "New logical partition" } else { "New partition" }).strong());
                egui::Grid::new("new_part").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("Size:");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(size_mib).range(1.0..=max).speed(1.0).suffix(" MiB"));
                        if ui.small_button("All").clicked() {
                            *size_mib = max;
                        }
                        ui.label(
                            RichText::new(format!("of {}", format_size(free.sectors() * ss))).color(theme::TEXT_DIM),
                        );
                    });
                    ui.end_row();
                    ui.label("File system:");
                    format_combo(ui, "new_fmt", fmts, format, true);
                    ui.end_row();
                    if format.is_some() {
                        ui.label("Volume label:");
                        ui.text_edit_singleline(label);
                        ui.end_row();
                    }
                    ui.label("Type:");
                    let default = format!("Default ({})", tys[0].0);
                    let auto = if format.is_some() { "Suits the file system" } else { default.as_str() };
                    let text = type_idx.map_or(auto.to_owned(), |i| tys[i].0.clone());
                    egui::ComboBox::from_id_salt("new_type").selected_text(text).show_ui(ui, |ui| {
                        ui.selectable_value(type_idx, None, auto);
                        for (i, (n, _)) in tys.iter().enumerate() {
                            ui.selectable_value(type_idx, Some(i), n);
                        }
                    });
                    ui.end_row();
                    if scheme == Scheme::Gpt {
                        ui.label("Name:");
                        ui.text_edit_singleline(name);
                        ui.end_row();
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                    if ui.button(RichText::new("Add").strong()).clicked() {
                        let bytes = if *size_mib >= max - 0.001 {
                            free.sectors() * ss
                        } else {
                            (*size_mib * (1 << 20) as f64) as u64
                        };
                        let t = type_idx.map(|i| tys[i].1);
                        let p = format.map(|i| plan(&fmts[i], label));
                        match e.add(*free, bytes, t, name, p, fmts) {
                            Ok(n) => {
                                *select = Some(Selection::Partition(n));
                                done = true;
                            }
                            Err(err) => *error = Some(err.to_string()),
                        }
                    }
                });
            }
            Form::Format { number, format, label } => {
                ui.label(RichText::new(format!("Format partition {number}")).strong());
                ui.colored_label(theme::WARN, "Everything in the partition will be erased.");
                egui::Grid::new("fmt_part").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                    ui.label("File system:");
                    let mut c = Some(*format);
                    format_combo(ui, "fmt_fmt", fmts, &mut c, false);
                    *format = c.unwrap_or(0);
                    ui.end_row();
                    ui.label("Volume label:");
                    ui.text_edit_singleline(label);
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                    if ui.button(RichText::new("Format").strong()).clicked() {
                        match e.format(*number, plan(&fmts[*format], label), fmts) {
                            Ok(()) => done = true,
                            Err(err) => *error = Some(err.to_string()),
                        }
                    }
                });
            }
            Form::Name { number, name } => {
                ui.horizontal(|ui| {
                    ui.label(format!("Name of partition {number}:"));
                    ui.text_edit_singleline(name);
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                    if ui.button(RichText::new("Rename").strong()).clicked() {
                        match e.set_name(*number, name) {
                            Ok(()) => done = true,
                            Err(err) => *error = Some(err.to_string()),
                        }
                    }
                });
            }
            Form::Type { number, type_idx } => {
                ui.horizontal(|ui| {
                    ui.label(format!("Type of partition {number}:"));
                    egui::ComboBox::from_id_salt("set_type").selected_text(&tys[*type_idx].0).show_ui(ui, |ui| {
                        for (i, (n, _)) in tys.iter().enumerate() {
                            ui.selectable_value(type_idx, i, n);
                        }
                    });
                    if ui.button("Cancel").clicked() {
                        done = true;
                    }
                    if ui.button(RichText::new("Set").strong()).clicked() {
                        match e.set_type(*number, tys[*type_idx].1) {
                            Ok(()) => done = true,
                            Err(err) => *error = Some(err.to_string()),
                        }
                    }
                });
            }
        }
    });
    done
}

/// Draws the dialog. `close`: leave it (back to the open dialog).
pub fn show(ui: &mut Ui, ws: &Workspace, d: &mut DiskDialog, actions: &mut Vec<Action>, close: &mut bool) {
    ui.heading("Partitions");
    ui.label(RichText::new(&d.path).color(theme::TEXT_DIM));
    ui.add_space(4.0);

    // A finished apply: read the disk again.
    if let Some(rx) = &d.applying {
        match rx.try_recv() {
            Ok(r) => {
                d.applying = None;
                match r {
                    Ok(notes) => {
                        d.notes = notes;
                        d.notes.insert(0, "The changes were written.".into());
                        d.editor = d.read(ws);
                        d.selected = None;
                    }
                    Err(e) => {
                        d.error = Some(format!("Writing failed: {e}"));
                        // What is on the disk now.
                        if let Ok(fresh) = d.read(ws) {
                            d.editor = Ok(fresh);
                        }
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Writing the partition table and creating file systems…");
                });
                return;
            }
            Err(_) => d.applying = None,
        }
    }

    let e = match &mut d.editor {
        Ok(e) => e,
        Err(err) => {
            ui.colored_label(theme::DANGER, format!("Could not read the disk: {err}"));
            if d.path.starts_with("/dev/") && !std::path::Path::new(&d.path).exists() {
                ui.label(
                    "The disk is no longer connected. If it was ejected (in macOS's Finder, Eject \
                     disconnects the whole disk), unplug it and plug it in again, then open it from \
                     the list. To change a disk the system uses, use “Unmount and apply” here instead.",
                );
            }
            super::dialogs::buttons(ui, |ui| *close |= ui.button("Back").clicked());
            return;
        }
    };
    let fmts = formatters(&ws.registry);

    if d.confirm {
        ui.label(RichText::new("Write these changes?").strong());
        for c in &e.changes {
            ui.label(format!("  • {c}"));
        }
        ui.add_space(4.0);
        ui.colored_label(
            theme::WARN,
            "Data in deleted, re-created or formatted partitions is lost. The partitions that stay, and restored ones, keep their data.",
        );
        super::dialogs::buttons(ui, |ui| {
            if ui.button("Back").clicked() {
                d.confirm = false;
            }
            let b = Button::new(
                RichText::new(if d.memory.is_some() { "Write to image" } else { "Write to disk" })
                    .color(Color32::WHITE),
            )
            .fill(theme::DANGER);
            if ui.add(b).clicked() {
                d.confirm = false;
                d.error = None;
                d.notes.clear();
                actions.push(Action::ApplyDisk);
            }
        });
        return;
    }

    ui.horizontal(|ui| {
        ui.label(format!("{} · ", format_size(e.size)));
        match &e.layout {
            Some(l) => ui.label(format!("{} partition table, {}-byte sectors", l.scheme.label(), l.sector_size)),
            None => ui.label(RichText::new("No partition table").color(theme::WARN)),
        };
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.menu_button("New table…", |ui| {
                ui.label(RichText::new("Replaces every partition").small().color(theme::WARN));
                for (s, what) in [(Scheme::Gpt, "GPT (UEFI, any size)"), (Scheme::Mbr, "MBR (DOS, up to 2 TiB)")] {
                    if ui.button(what).clicked() {
                        if let Err(err) = e.new_table(s) {
                            d.error = Some(err.to_string());
                        }
                        d.selected = None;
                        d.form = None;
                        ui.close();
                    }
                }
            });
        });
    });
    if e.layout.is_none() {
        ui.label("Create a partition table to add partitions (\"New table…\").");
    }
    bar(ui, e, &mut d.selected);
    ui.add_space(4.0);
    table(ui, e, &mut d.selected);

    // Actions for the selection.
    let layout = e.layout.clone();
    if let (Some(sel), Some(l), None) = (d.selected, &layout, &d.form) {
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| match sel {
            Selection::Free(first) => {
                if let Some(f) = l.free().into_iter().find(|f| f.first == first) {
                    let can_extend = l.scheme == Scheme::Mbr && !f.logical && l.extended().is_none();
                    if ui.button("New partition…").clicked() {
                        let size_mib = (f.sectors() * l.sector_size) as f64 / (1 << 20) as f64;
                        d.form = Some(Form::New {
                            free: f,
                            size_mib,
                            type_idx: None,
                            name: String::new(),
                            format: None,
                            label: String::new(),
                        });
                    }
                    if can_extend
                        && ui
                            .button("New extended partition")
                            .on_hover_text("A container for logical partitions (an MBR holds only four primary ones)")
                            .clicked()
                    {
                        let all = f.sectors() * l.sector_size;
                        if let Err(err) = e.add(f, all, Some(TypeCode::Mbr(0x0F)), "", None, &fmts) {
                            d.error = Some(err.to_string());
                        }
                    }
                }
            }
            Selection::Partition(n) => {
                let Some(s) = l.get(n) else { return };
                if !s.is_extended() && ui.button("Format…").clicked() && !fmts.is_empty() {
                    d.form = Some(Form::Format { number: n, format: 0, label: String::new() });
                }
                if ui.button("Type…").clicked() {
                    let tys = types(l.scheme);
                    let type_idx = tys.iter().position(|(_, t)| *t == s.type_code).unwrap_or(0);
                    d.form = Some(Form::Type { number: n, type_idx });
                }
                if l.scheme == Scheme::Gpt && ui.button("Name…").clicked() {
                    d.form = Some(Form::Name { number: n, name: s.name.clone() });
                }
                let primary = !s.is_logical() && !s.is_extended();
                if (l.scheme == Scheme::Gpt || primary)
                    && ui.button(if s.bootable() { "Not bootable" } else { "Bootable" }).clicked()
                {
                    if let Err(err) = e.set_bootable(n, !s.bootable()) {
                        d.error = Some(err.to_string());
                    }
                }
                if ui.add(Button::new(RichText::new("Delete").color(theme::DANGER))).clicked() {
                    match e.delete(n) {
                        Ok(()) => d.selected = None,
                        Err(err) => d.error = Some(err.to_string()),
                    }
                }
                if !e.changed() && !s.is_extended() {
                    ui.separator();
                    if ui.button("Open").on_hover_text("Browse this partition").clicked() {
                        actions.push(Action::OpenPartitionFromDisk(n));
                    }
                }
            }
        });
    }
    if let Some(form) = &mut d.form {
        ui.add_space(4.0);
        if form_ui(ui, e, form, &fmts, &mut d.error, &mut d.selected) {
            d.form = None;
        }
    }

    lost_section(ui, ws, d, actions);
    let Ok(e) = &mut d.editor else { return };
    if let Some(err) = &d.error {
        ui.colored_label(theme::DANGER, err);
    }
    if d.system_uses && d.error.is_some() && dix_core_can_unmount() {
        ui.horizontal(|ui| {
            if ui
                .button("Unmount and apply")
                .on_hover_text("Unmount the disk's volumes (it stays connected, unlike Eject), then write the changes")
                .clicked()
            {
                d.error = None;
                d.system_uses = false;
                actions.push(Action::UnmountAndApplyDisk);
            }
            ui.label(RichText::new("(don't Eject the disk in the Finder: that disconnects it)").color(theme::TEXT_DIM));
        });
    }
    for n in &d.notes {
        ui.label(RichText::new(n).color(theme::OK));
    }
    if !e.changes.is_empty() {
        ui.add_space(6.0);
        ui.label(RichText::new("Changes to write:").strong());
        for c in &e.changes {
            ui.label(RichText::new(format!("  • {c}")).color(theme::TEXT_DIM));
        }
    }
    let changed = e.changed();
    super::dialogs::buttons(ui, |ui| {
        if ui.button("Close").clicked() {
            *close = true;
        }
        if ui.add_enabled(changed, Button::new(RichText::new("Apply…").strong())).clicked() {
            d.confirm = true;
            d.error = None;
        }
        if ui.add_enabled(changed, Button::new("Undo all")).clicked() {
            e.reset();
            d.selected = None;
            d.form = None;
            d.error = None;
        }
    });
}

/// Whether the disk's volumes can be unmounted from here.
fn dix_core_can_unmount() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        dix_core::device::host::CAN_UNMOUNT
    }
    #[cfg(target_arch = "wasm32")]
    {
        false
    }
}

/// A way to wake the UI from the search's thread.
fn ws_repaint(ws: &Workspace) -> Arc<dyn Fn() + Send + Sync> {
    ws.notifier()
}

/// "Lost partitions": the search, its progress, what it found (Open,
/// Restore).
fn lost_section(ui: &mut Ui, ws: &Workspace, d: &mut DiskDialog, actions: &mut Vec<Action>) {
    ui.add_space(6.0);
    let has_table = d.editor.as_ref().is_ok_and(|e| e.layout.is_some());
    // A finished search.
    if let Some(l) = &mut d.lost {
        if let Some(Ok(r)) = l.rx.as_ref().map(|rx| rx.try_recv()) {
            l.found = Some(r);
            l.rx = None;
        }
    }
    let searching = d.lost.as_ref().is_some_and(|l| l.rx.is_some());
    ui.horizontal(|ui| {
        ui.label(RichText::new("Lost partitions").strong());
        if !searching
            && ui
                .button(if d.lost.is_some() { "Search again" } else { "Find lost partitions" })
                .on_hover_text(
                    "Look in the space no partition uses for file systems whose partition was deleted or whose \
                     table was overwritten (read-only)",
                )
                .clicked()
        {
            d.error = None;
            d.find_lost(ws);
        }
    });
    let Some(l) = &mut d.lost else { return };
    if l.rx.is_some() {
        let done = l.done.load(Ordering::Relaxed);
        let frac = if l.total == 0 { 1.0 } else { done as f32 / l.total as f32 };
        ui.horizontal(|ui| {
            ui.add(egui::ProgressBar::new(frac).show_percentage().desired_width(300.0));
            if ui.add_enabled(!l.cancel.load(Ordering::Relaxed), Button::new("Stop")).clicked() {
                l.cancel.store(true, Ordering::Relaxed);
            }
        });
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
        return;
    }
    match &l.found {
        Some(Err(e)) => {
            ui.colored_label(theme::DANGER, format!("The search failed: {e}"));
        }
        Some(Ok(found)) if found.is_empty() => {
            ui.label(RichText::new("No lost partitions were found in the free space.").color(theme::TEXT_DIM));
        }
        Some(Ok(found)) => {
            let mut restore = None;
            for (i, f) in found.iter().enumerate() {
                ui.horizontal(|ui| {
                    let label = f.label.as_deref().map(|l| format!(" “{l}”")).unwrap_or_default();
                    ui.label(format!("{}{label}", f.kind));
                    ui.label(
                        RichText::new(format!("{} at {}", format_size(f.size), format_size(f.start)))
                            .color(theme::TEXT_DIM),
                    );
                    ui.label(RichText::new("found").color(theme::OK));
                    if ui
                        .add_enabled(f.opens, Button::new("Open"))
                        .on_hover_text("Open it read-only to look inside before restoring it")
                        .clicked()
                    {
                        actions.push(Action::OpenLostPartition(f.start, f.size));
                    }
                    let done = l.restored.contains(&f.start);
                    let tip = if !has_table {
                        "The disk has no partition table: make one first (New table)"
                    } else if done {
                        "Restored: write it with Apply"
                    } else {
                        "Add it to the partition table again (written with Apply)"
                    };
                    if ui
                        .add_enabled(has_table && !done, Button::new(if done { "Restored" } else { "Restore" }))
                        .on_hover_text(tip)
                        .clicked()
                    {
                        restore = Some(i);
                    }
                });
            }
            if let (Some(i), Ok(e)) = (restore, &mut d.editor) {
                match e.restore(&found[i]) {
                    Ok(_) => l.restored.push(found[i].start),
                    Err(err) => d.error = Some(err.to_string()),
                }
            }
        }
        None => {}
    }
}
