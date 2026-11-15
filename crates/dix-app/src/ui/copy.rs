//! *Copy disk…* (native builds): save a disk as an image file — all of it,
//! only the space its file systems use, or rescued from a failing disk —
//! or write an image onto a disk (a USB stick, an SD card), checked by
//! reading it back. A running copy can be hidden (the status bar shows it)
//! while the rest of the app is used.

use super::{theme, Action};
use crate::model::imaging::{self, ImageInfo, Job, Outcome};
use crate::model::Workspace;
use dix_core::device::host::{HostDevice, HostDeviceKind};
use dix_core::imaging::Status;
use eframe::egui::{self, Button, RichText, Ui};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Save,
    Write,
}

pub struct CopyDialog {
    pub tab: Tab,
    pub devices: Option<Vec<HostDevice>>,
    // Save a disk as an image.
    pub source: String,
    /// Container format name; None: raw.
    pub format: Option<String>,
    pub used_only: bool,
    pub rescue: bool,
    pub retries: u32,
    /// Where the image is saved.
    pub dest: String,
    /// What is at `dest` already (for the source, place and mode it was
    /// looked at for): a note, and whether it is a warning.
    pub dest_note: Option<(String, Option<(String, bool)>)>,
    // Write an image onto a disk.
    pub image: String,
    /// What `image` is (for the path it was read for).
    pub image_info: Option<(String, Result<ImageInfo, String>)>,
    pub disk: String,
    pub show_all: bool,
    pub verify: bool,
    /// The last step before writing: what the system has mounted from it.
    pub confirm: Option<Vec<String>>,
    pub error: Option<String>,
    pub job: Option<Job>,
    pub finished: Option<Result<Outcome, String>>,
}

impl CopyDialog {
    pub fn new(tab: Tab) -> Self {
        CopyDialog {
            tab,
            devices: None,
            source: String::new(),
            format: None,
            used_only: false,
            rescue: false,
            retries: 1,
            dest: String::new(),
            dest_note: None,
            image: String::new(),
            image_info: None,
            disk: String::new(),
            show_all: false,
            verify: true,
            confirm: None,
            error: None,
            job: None,
            finished: None,
        }
    }

    /// The job's result, once it has ended.
    pub fn poll(&mut self) {
        if let Some(job) = &self.job {
            match job.result.try_recv() {
                Ok(r) => {
                    self.finished = Some(r);
                    self.job = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(_) => {
                    self.finished = Some(Err("the copy ended without a result".into()));
                    self.job = None;
                }
            }
        }
    }

    /// One line for the status bar: what runs, how far.
    pub fn status_line(&self) -> Option<String> {
        let job = self.job.as_ref()?;
        let p = &job.progress;
        let (done, total) = (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
        let pct = if total > 0 { format!(" {:.0}%", done as f64 * 100.0 / total as f64) } else { String::new() };
        Some(format!("{} — {}{pct}", job.title, p.phase.lock().unwrap()))
    }
}

/// The default file name for an image of `source` with extension `ext`
/// ("sdb.img").
pub fn default_name(source: &str, ext: &str) -> String {
    let p = std::path::Path::new(source.trim_end_matches(['/', '\\']));
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| source.trim_start_matches(r"\\.\").replace([':', '\\'], ""));
    let stem = if stem.is_empty() { "disk".to_owned() } else { stem };
    format!("{stem}.{ext}")
}

fn device_rows(
    ui: &mut Ui,
    id: &str,
    devices: &[HostDevice],
    selected: &mut String,
    filter: impl Fn(&HostDevice) -> bool,
) -> usize {
    let mut shown = 0;
    egui::ScrollArea::vertical().id_salt(id).max_height(170.0).show(ui, |ui| {
        egui::Grid::new(id).num_columns(3).spacing([12.0, 2.0]).show(ui, |ui| {
            for dev in devices.iter().filter(|d| filter(d)) {
                shown += 1;
                let path = if dev.parent.is_some() {
                    RichText::new(format!("    {}", dev.path))
                } else {
                    RichText::new(&dev.path).strong()
                };
                if ui.selectable_label(*selected == dev.path, path).clicked() {
                    *selected = dev.path.clone();
                }
                let size = egui::vec2(270.0, ui.spacing().interact_size.y);
                ui.allocate_ui_with_layout(size, egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let text = RichText::new(&dev.description).color(theme::TEXT_DIM);
                    ui.add(egui::Label::new(text).truncate()).on_hover_text(&dev.description);
                });
                ui.label(RichText::new(dev.size.map(super::format_size).unwrap_or_default()).color(theme::TEXT_DIM));
                ui.end_row();
            }
        });
    });
    shown
}

/// Disks an image is usually written to: removable, USB, SD cards.
fn is_removable(d: &HostDevice) -> bool {
    d.removable || matches!(d.bus.as_deref(), Some("USB" | "SD card"))
}

pub fn show(ui: &mut Ui, ws: &Workspace, d: &mut CopyDialog, actions: &mut Vec<Action>, close: &mut bool) {
    d.poll();
    if d.job.is_some() || d.finished.is_some() {
        running(ui, d, actions, close);
        return;
    }
    ui.heading("Copy disk");
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let before = d.tab;
        ui.selectable_value(&mut d.tab, Tab::Save, "Save disk as image");
        ui.selectable_value(&mut d.tab, Tab::Write, "Write image to disk");
        if d.tab != before {
            d.error = None;
            d.confirm = None;
        }
    });
    ui.separator();
    if d.devices.is_none() {
        d.devices = Some(crate::platform::list_devices());
    }
    match d.tab {
        Tab::Save => save_tab(ui, ws, d, actions, close),
        Tab::Write => write_tab(ui, ws, d, actions, close),
    }
}

fn save_tab(ui: &mut Ui, ws: &Workspace, d: &mut CopyDialog, actions: &mut Vec<Action>, close: &mut bool) {
    ui.label("The whole disk (every partition) is copied into an image file.");
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Disk:");
        ui.add(
            egui::TextEdit::singleline(&mut d.source)
                .hint_text(r"/dev/sdb, \\.\PhysicalDrive1 or an image file")
                .desired_width(330.0),
        );
        if ui.small_button("Refresh").clicked() {
            d.devices = None;
        }
    });
    let devices = d.devices.clone().unwrap_or_default();
    if device_rows(ui, "save_devices", &devices, &mut d.source, |_| true) == 0 {
        ui.label(
            RichText::new("No disks found (reading disks usually needs administrator / root rights).")
                .color(theme::TEXT_DIM),
        );
    }
    ui.add_space(6.0);
    ui.checkbox(&mut d.used_only, "Only the used space").on_hover_text(
        "Free space inside FAT, exFAT, ext2/3/4, NTFS, XFS and Btrfs file systems is left out (it reads as zeros in the image): \
         faster, and a smaller image. Everything else — partition tables, other file systems, the space between \
         partitions — is copied.",
    );
    ui.checkbox(&mut d.rescue, "Rescue mode, for a failing disk").on_hover_text(
        "Reads what can be read first and skips past unreadable places, then comes back for them sector by sector \
         (as GNU ddrescue). Unreadable sectors stay zeros in the image. A map file next to the image records what \
         was read, so the copy can be stopped and continued later.",
    );
    if d.rescue {
        ui.indent("rescue", |ui| {
            ui.horizontal(|ui| {
                ui.label("Passes over bad sectors at the end:");
                ui.add(egui::DragValue::new(&mut d.retries).range(0..=9));
            });
            ui.label(
                RichText::new(
                    "Rescues make raw images (convert them afterwards if needed). To continue a stopped rescue, \
                     save to the same image file again.",
                )
                .small()
                .color(theme::TEXT_DIM),
            );
        });
    }
    let list = super::convert::targets(ws);
    if d.rescue {
        d.format = None;
    }
    ui.horizontal(|ui| {
        ui.label("Format:");
        let current = list.iter().find(|t| t.0 == d.format).map(|t| t.2).unwrap_or("");
        let name = d.format.clone().unwrap_or_else(|| "Raw".into());
        ui.add_enabled_ui(!d.rescue, |ui| {
            egui::ComboBox::from_id_salt("copy_format")
                .selected_text(format!("{name} — {current}"))
                .width(330.0)
                .show_ui(ui, |ui| {
                    for (f, ext, about) in &list {
                        let label = format!("{} (.{ext}) — {about}", f.as_deref().unwrap_or("Raw"));
                        ui.selectable_value(&mut d.format, f.clone(), label);
                    }
                });
        });
    });
    let ext = list.iter().find(|t| t.0 == d.format).map_or("img", |t| t.1);
    // The file name follows the format chosen.
    let dest = d.dest.trim().to_owned();
    if !dest.is_empty() {
        let p = std::path::Path::new(&dest);
        let known = list.iter().any(|t| p.extension().is_some_and(|e| e == t.1));
        if known && p.extension().is_some_and(|e| e != ext) {
            d.dest = p.with_extension(ext).display().to_string();
        }
    }
    ui.horizontal(|ui| {
        ui.label("Save to:");
        ui.add(egui::TextEdit::singleline(&mut d.dest).hint_text(format!("/path/disk.{ext}")).desired_width(400.0));
        if ui.button("Browse…").clicked() {
            let name = default_name(d.source.trim(), ext);
            if let Some(p) = crate::platform::native_pick_save_path("Save the disk image", &name) {
                d.dest = p.display().to_string();
            }
        }
    });
    let key = format!("{}\n{}\n{}", d.source.trim(), d.dest.trim(), d.rescue);
    if d.dest_note.as_ref().is_none_or(|(k, _)| *k != key) {
        d.dest_note = Some((key, dest_note(d)));
    }
    if let Some((_, Some((note, warn)))) = &d.dest_note {
        ui.label(RichText::new(note).color(if *warn { theme::WARN } else { theme::OK }));
    }
    if let Some(e) = &d.error {
        ui.colored_label(theme::DANGER, e);
    }
    super::dialogs::buttons(ui, |ui| {
        if ui.button("Cancel").clicked() {
            *close = true;
        }
        let label = if d.rescue { "Rescue" } else { "Save" };
        let ready = !d.source.trim().is_empty() && !d.dest.trim().is_empty();
        if ui.add_enabled(ready, Button::new(RichText::new(label).strong())).clicked() {
            d.error = None;
            actions.push(Action::StartSaveDisk);
        }
    });
}

/// What saving to `d.dest` does to a file already there.
fn dest_note(d: &CopyDialog) -> Option<(String, bool)> {
    let dest = PathBuf::from(d.dest.trim());
    if d.dest.trim().is_empty() || !dest.is_file() {
        return None;
    }
    if d.rescue {
        let source = d.source.trim();
        let size = d
            .devices
            .as_ref()
            .and_then(|l| l.iter().find(|x| x.path == source))
            .and_then(|x| x.size)
            .or_else(|| std::fs::metadata(source).ok().filter(|m| m.is_file()).map(|m| m.len()));
        if let Some(map) = size.and_then(|s| imaging::earlier_rescue(&dest, s)) {
            let t = map.totals();
            return Some((
                format!(
                    "An earlier rescue of this disk is continued: {} read, {} unreadable, {} left to try.",
                    super::format_size(t.finished),
                    super::format_size(t.bad),
                    super::format_size(t.non_tried + t.non_trimmed + t.non_scraped)
                ),
                false,
            ));
        }
    }
    Some(("This file exists and will be replaced.".into(), true))
}

fn write_tab(ui: &mut Ui, ws: &Workspace, d: &mut CopyDialog, actions: &mut Vec<Action>, close: &mut bool) {
    if let Some(mounted) = d.confirm.clone() {
        return confirm_write(ui, d, &mounted, actions);
    }
    ui.label(
        "Everything on the disk is replaced by the image: for a bootable USB stick, or an SD card for a Raspberry Pi.",
    );
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Image:");
        ui.add(
            egui::TextEdit::singleline(&mut d.image)
                .hint_text("/path/image.img (or .iso, .img.xz, .qcow2 …)")
                .desired_width(400.0),
        );
        if ui.button("Browse…").clicked() {
            if let Some(p) = crate::platform::native_browse_image_path() {
                d.image = p.display().to_string();
            }
        }
    });
    let path = d.image.trim().to_owned();
    if d.image_info.as_ref().is_none_or(|(p, _)| *p != path) {
        d.error = None;
        d.image_info = (!path.is_empty() && std::path::Path::new(&path).is_file())
            .then(|| (path.clone(), imaging::describe_image(&ws.registry, std::path::Path::new(&path))));
    }
    match d.image_info.as_ref().map(|(_, i)| i) {
        Some(Ok(i)) => {
            let what = match i.size {
                Some(s) if s != i.file_size => {
                    format!("{} · {} ({} file)", i.kind, super::format_size(s), super::format_size(i.file_size))
                }
                Some(s) => format!("{} · {}", i.kind, super::format_size(s)),
                None => format!("{} · {} (unpacked while it is written)", i.kind, super::format_size(i.file_size)),
            };
            ui.label(RichText::new(what).color(theme::TEXT_DIM));
        }
        Some(Err(e)) => {
            ui.colored_label(theme::DANGER, e);
        }
        None => {
            ui.label(
                RichText::new(
                    "Raw images (.img, .iso), VM disks (qcow2, VHD, VHDX, VMDK, VDI) and compressed images \
                     (.img.xz, .gz, .zst, .bz2).",
                )
                .color(theme::TEXT_DIM),
            );
        }
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Disk:");
        ui.checkbox(&mut d.show_all, "Show all disks")
            .on_hover_text("Also internal disks. Writing an image to the disk the system runs on destroys it.");
        if ui.small_button("Refresh").clicked() {
            d.devices = None;
        }
    });
    let devices = d.devices.clone().unwrap_or_default();
    let show_all = d.show_all;
    let n = device_rows(ui, "write_devices", &devices, &mut d.disk, |x| {
        x.kind == HostDeviceKind::Disk && (show_all || is_removable(x))
    });
    if n == 0 {
        let what =
            if show_all { "No disks found." } else { "No USB disks or SD cards found: plug one in and Refresh." };
        ui.label(RichText::new(what).color(theme::TEXT_DIM));
    }
    ui.checkbox(&mut d.verify, "Check the disk afterwards (read it back and compare)");
    if let Some(e) = &d.error {
        ui.colored_label(theme::DANGER, e);
    }
    super::dialogs::buttons(ui, |ui| {
        if ui.button("Cancel").clicked() {
            *close = true;
        }
        let ready = d.image_info.as_ref().is_some_and(|(_, i)| i.is_ok()) && !d.disk.is_empty();
        if ui.add_enabled(ready, Button::new(RichText::new("Write…").strong())).clicked() {
            d.error = None;
            actions.push(Action::CheckWriteImage);
        }
    });
}

fn confirm_write(ui: &mut Ui, d: &mut CopyDialog, mounted: &[String], actions: &mut Vec<Action>) {
    let dev = d.devices.as_ref().and_then(|l| l.iter().find(|x| x.path == d.disk)).cloned();
    ui.heading(format!("Erase {}?", d.disk));
    ui.add_space(4.0);
    if let Some(dev) = &dev {
        let size = dev.size.map(super::format_size).unwrap_or_default();
        ui.label(RichText::new(format!("{} · {size}", dev.description)).strong());
    }
    ui.label(format!("Everything on it is replaced by {}. This cannot be undone.", d.image.trim()));
    if dev.as_ref().is_some_and(|x| !is_removable(x)) {
        ui.colored_label(theme::DANGER, "This is not a removable disk. Make sure it is the right one.");
    }
    let unmount = !mounted.is_empty();
    if unmount {
        ui.add_space(4.0);
        ui.label("Mounted now:");
        for m in mounted {
            ui.label(RichText::new(format!("  • {m}")).color(theme::TEXT_DIM));
        }
        let how = if cfg!(windows) {
            "Its volumes are dismounted while it is written."
        } else {
            "They are unmounted first (the disk stays connected)."
        };
        ui.label(RichText::new(how).color(theme::TEXT_DIM));
    }
    if let Some(e) = &d.error {
        ui.colored_label(theme::DANGER, e);
    }
    super::dialogs::buttons(ui, |ui| {
        if ui.button("Back").clicked() {
            d.confirm = None;
            d.error = None;
        }
        let label = if unmount && !cfg!(windows) { "Unmount, erase and write" } else { "Erase and write" };
        let b = Button::new(RichText::new(label).color(egui::Color32::WHITE)).fill(theme::DANGER);
        if ui.add(b).clicked() {
            actions.push(Action::StartWriteImage { unmount: unmount && !cfg!(windows) });
        }
    });
}

fn running(ui: &mut Ui, d: &mut CopyDialog, actions: &mut Vec<Action>, close: &mut bool) {
    if let Some(r) = &d.finished {
        ui.heading("Copy disk");
        ui.add_space(4.0);
        match r {
            Ok(o) => {
                ui.colored_label(if o.partial { theme::WARN } else { theme::OK }, &o.message);
            }
            Err(e) => {
                ui.colored_label(theme::DANGER, e);
            }
        }
        let image = r.as_ref().ok().and_then(|o| o.image.clone());
        super::dialogs::buttons(ui, |ui| {
            if ui.button("Close").clicked() {
                *close = true;
            }
            if let Some(p) = image {
                if ui.button("Open the image").clicked() {
                    actions.push(Action::OpenCopiedImage(p));
                }
            }
        });
        return;
    }
    let Some(job) = &d.job else { return };
    let p = &job.progress;
    ui.heading(&job.title);
    ui.add_space(4.0);
    let phase = p.phase.lock().unwrap().clone();
    ui.label(RichText::new(&phase).strong());
    let (done, total) = (p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
    let frac = if total == 0 { 0.0 } else { done as f32 / total as f32 };
    ui.add(egui::ProgressBar::new(frac).show_percentage());
    let secs = job.started.elapsed().as_secs_f64();
    let base = p.base.load(Ordering::Relaxed).min(done);
    let rate = if secs > 1.0 { (done - base) as f64 / secs } else { 0.0 };
    let mut line = format!("{} of {}", super::format_size(done), super::format_size(total));
    if rate > 0.0 && done < total {
        let left = (total - done) as f64 / rate;
        line.push_str(&format!(" · {}/s · about {} left", super::format_size(rate as u64), duration(left)));
    }
    ui.label(RichText::new(line).color(theme::TEXT_DIM));
    if let Some(map) = p.map.lock().unwrap().clone() {
        let t = map.totals();
        ui.add_space(4.0);
        ui.label(format!(
            "Read: {} · unreadable: {} in {} place{} · not read yet: {} · failed reads: {}",
            super::format_size(t.finished),
            super::format_size(t.bad),
            t.bad_areas,
            if t.bad_areas == 1 { "" } else { "s" },
            super::format_size(t.non_tried + t.non_trimmed + t.non_scraped),
            p.errors.load(Ordering::Relaxed)
        ));
        map_view(ui, &map);
    }
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
    let stopping = p.cancel.load(Ordering::Relaxed);
    let (writes, resumable) = (job.writes_disk, job.resumable);
    super::dialogs::buttons(ui, |ui| {
        if ui.button("Hide").on_hover_text("Keep it running; the status bar shows it").clicked() {
            actions.push(Action::HideCopy);
        }
        let stop = ui.add_enabled(!stopping, Button::new(if stopping { "Stopping…" } else { "Stop" }));
        let stop = if writes {
            stop.on_hover_text("The disk is left partly written")
        } else if resumable {
            stop.on_hover_text("What was read is kept; save to the same image file again to go on")
        } else {
            stop
        };
        if stop.clicked() {
            p.cancel.store(true, Ordering::Relaxed);
        }
    });
}

fn duration(secs: f64) -> String {
    let s = secs as u64;
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s.div_ceil(60))
    } else {
        format!("{} h {} min", s / 3600, (s % 3600) / 60)
    }
}

/// The rescue map: the disk as a grid of cells, each in the colour of the
/// worst state in it.
fn map_view(ui: &mut Ui, map: &dix_core::imaging::Map) {
    let (cols, rows) = (80usize, 6usize);
    let width = ui.available_width().min(560.0);
    let cell = width / cols as f32;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, cell * rows as f32), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    for (i, s) in map.cells(cols * rows).into_iter().enumerate() {
        let (x, y) = ((i % cols) as f32, (i / cols) as f32);
        let r =
            egui::Rect::from_min_size(rect.min + egui::vec2(x * cell, y * cell), egui::vec2(cell - 1.0, cell - 1.0));
        painter.rect_filled(r, 1.0, colour(s));
    }
    ui.horizontal(|ui| {
        for (s, what) in [
            (Status::Finished, "read"),
            (Status::NonTried, "not read yet"),
            (Status::NonTrimmed, "to read closer"),
            (Status::Bad, "unreadable"),
        ] {
            let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().rect_filled(r, 1.0, colour(s));
            ui.label(RichText::new(what).small().color(theme::TEXT_DIM));
        }
    });
}

fn colour(s: Status) -> egui::Color32 {
    match s {
        Status::Finished => theme::OK,
        Status::NonTried => theme::BUTTON_BG,
        Status::NonTrimmed | Status::NonScraped => theme::WARN,
        Status::Bad => theme::DANGER,
    }
}
