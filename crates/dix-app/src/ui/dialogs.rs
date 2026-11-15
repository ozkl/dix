//! Modal dialogs: open image, new image, confirmations, properties.

use super::{theme, Action, UiState};
use crate::model::{VolumeId, Workspace};
use dix_core::partition::{PartitionChoice, PartitionScan};
use dix_core::{AccessMode, Capabilities};
use eframe::egui::{self, Button, RichText, TextEdit, Ui};

pub enum Dialog {
    Open(OpenDialog),
    Disk(Box<super::disk::DiskDialog>),
    Convert(Box<super::convert::ConvertDialog>),
    #[cfg(not(target_arch = "wasm32"))]
    Copy(Box<super::copy::CopyDialog>),
    /// A scan for photos under way.
    Scanning {
        name: String,
        progress: crate::model::ScanProgress,
    },
    NewImage(NewImageDialog),
    ConfirmDelete {
        names: Vec<String>,
    },
    ConfirmClose(VolumeId),
    /// Eject requested while background work is using the image.
    Busy {
        volume: VolumeId,
        activities: Vec<String>,
    },
    /// Writing pending changes failed while ejecting.
    EjectFailed {
        volume: VolumeId,
        error: String,
    },
    /// Quit requested while something is busy or unsaved. `writing`: write
    /// jobs are running (quitting anyway needs the data-loss warning).
    Quit {
        reasons: Vec<String>,
        writing: bool,
    },
    /// Second confirmation before cutting off a write in progress.
    DataLoss {
        target: ForceTarget,
    },
    /// The result of a file system check.
    CheckReport {
        name: String,
        result: Result<dix_core::CheckReport, String>,
        log: Vec<dix_core::LogEntry>,
        /// What a repair would work on (None: not offered).
        target: Option<crate::model::RepairTarget>,
        /// The report of a repair run.
        repaired: bool,
    },
    /// What the driver reported while mounting (shown when it warned or failed).
    MountReport {
        name: String,
        log: Vec<dix_core::LogEntry>,
        error: Option<String>,
    },
    Properties {
        title: String,
        rows: Vec<(String, String)>,
        /// A link that leads somewhere: offer to go there.
        link: Option<dix_core::VfsPath>,
    },
    /// A volume's settings being edited.
    Settings {
        volume: VolumeId,
        items: Vec<dix_core::Setting>,
        error: Option<String>,
    },
    VolumeInfo(VolumeId),
    /// A password for a volume's encrypted files.
    Password(PasswordDialog),
}

pub struct PasswordDialog {
    pub volume: VolumeId,
    /// Why it is asked for (which file, how many are encrypted).
    pub reason: String,
    pub password: String,
    pub show: bool,
    pub error: Option<String>,
    /// The field has been focused once.
    pub focused: bool,
}

impl PasswordDialog {
    pub fn new(volume: VolumeId, reason: String) -> Self {
        Self { volume, reason, password: String::new(), show: false, error: None, focused: false }
    }
}

/// What a forced action would interrupt.
#[derive(Clone, Copy)]
pub enum ForceTarget {
    Eject(VolumeId),
    Quit,
}

/// A host device row (native only; empty on the web).
#[derive(Clone)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub struct DeviceRow {
    pub path: String,
    pub description: String,
    pub size: Option<u64>,
    /// A partition or volume of a disk (shown under it).
    pub is_part: bool,
}

pub struct OpenDialog {
    pub path: String,
    pub mode: AccessMode,
    /// `None` = auto-detect.
    pub driver: Option<String>,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub devices: Option<Vec<DeviceRow>>,
    /// A dropped/uploaded image waiting to be opened (name, bytes).
    pub upload: Option<(String, Vec<u8>)>,
    /// The partition picked (when the image has a partition table).
    pub partition: PartitionChoice,
    /// The partition table of the chosen image, and what it was read for
    /// (the path, or the upload's name and size).
    pub scan: Option<(String, Result<Option<PartitionScan>, String>)>,
    /// The partition to choose once the table is read (back from the
    /// partition editor).
    pub prefer: Option<u32>,
    /// The chosen file is one part of a split archive or image: the names
    /// of all its parts (opened together, read-only).
    pub split: Option<Vec<String>>,
}

impl Default for OpenDialog {
    fn default() -> Self {
        Self {
            path: String::new(),
            mode: AccessMode::ReadOnly,
            driver: None,
            devices: None,
            upload: None,
            partition: PartitionChoice::Auto,
            scan: None,
            prefer: None,
            split: None,
        }
    }
}

impl OpenDialog {
    /// Points the dialog at a dropped/uploaded image, replacing what it had
    /// (its partitions are read again).
    pub fn fill(&mut self, src: crate::platform::ImageSource) {
        self.scan = None;
        match src {
            crate::platform::ImageSource::Path(p) => {
                self.path = p.display().to_string();
                self.upload = None;
            }
            crate::platform::ImageSource::Bytes { name, data } => {
                self.path.clear();
                self.upload = Some((name, data));
            }
        }
    }
}

pub struct NewImageDialog {
    pub name: String,
    pub size_mib: f64,
    pub driver: String,
    /// The driver's flavour (e.g. "ext4"), if it has several.
    pub variant: Option<String>,
    pub label: String,
    pub in_memory: bool,
    /// The chosen type has a size to choose (archives do not).
    pub sized: bool,
    /// Make a disk image: this partition table, one partition.
    pub table: Option<dix_core::partition::Scheme>,
    /// Store the disk in this virtual machine format ("qcow2", "VHD", …;
    /// a container format's name), else as a raw image.
    pub container: Option<String>,
    /// The disk format the name's extension was last set for.
    named_for: Option<String>,
}

impl Default for NewImageDialog {
    fn default() -> Self {
        Self {
            name: "new-image.img".into(),
            size_mib: 64.0,
            driver: "ext4".into(),
            variant: Some("ext4".into()),
            label: String::new(),
            in_memory: cfg!(target_arch = "wasm32"),
            sized: true,
            table: None,
            container: None,
            named_for: None,
        }
    }
}

pub(crate) fn buttons(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space(10.0);
    // Bounded height: a bare right-to-left layout would take all remaining space
    // and stretch the modal over the whole window.
    let size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y + 4.0);
    ui.allocate_ui_with_layout(size, egui::Layout::right_to_left(egui::Align::Center), add);
}

/// Shows the active dialog. Closing is done by setting `state.dialog = None`.
pub fn show(ctx: &egui::Context, ws: &Workspace, state: &mut UiState, actions: &mut Vec<Action>) {
    let Some(dialog) = state.dialog.as_mut() else { return };
    let mut close = false;
    let wide = matches!(dialog, Dialog::Disk(_));
    #[cfg(not(target_arch = "wasm32"))]
    let wide = wide || matches!(dialog, Dialog::Copy(_));
    // Below popups (both default to Foreground: a combo box opened a second
    // time would appear behind the dialog, which was raised by a click).
    let id = egui::Id::unique("dialog");
    let area = egui::Modal::default_area(id).order(egui::Order::Middle);
    let modal = egui::Modal::new(id).area(area).show(ctx, |ui| {
        ui.set_min_width(if wide { 720.0 } else { 440.0 });
        ui.set_max_width(if wide { 820.0 } else { 560.0 });
        match dialog {
            Dialog::Open(d) => open_dialog(ui, ws, d, actions, &mut close),
            Dialog::Disk(d) => super::disk::show(ui, ws, d, actions, &mut close),
            Dialog::Convert(d) => super::convert::show(ui, ws, d, actions, &mut close),
            #[cfg(not(target_arch = "wasm32"))]
            Dialog::Copy(d) => super::copy::show(ui, ws, d, actions, &mut close),
            Dialog::Scanning { name, progress } => {
                use std::sync::atomic::Ordering;
                ui.heading("Looking for photos and videos");
                ui.add_space(4.0);
                ui.label(RichText::new(name.as_str()).color(theme::TEXT_DIM));
                ui.label(
                    RichText::new(
                        "Every sector is read for the start of a photo or video (JPEG, PNG, GIF, WebP, HEIC, camera RAW, \
                         MP4, MOV); what is found opens as a read-only volume.",
                    )
                    .color(theme::TEXT_DIM),
                );
                ui.add_space(6.0);
                let done = progress.done.load(Ordering::Relaxed);
                let frac = if progress.total == 0 { 1.0 } else { done as f32 / progress.total as f32 };
                ui.add(egui::ProgressBar::new(frac).show_percentage());
                ui.label(format!("{} of {}", super::format_size(done), super::format_size(progress.total)));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
                let stopping = progress.cancel.load(Ordering::Relaxed);
                buttons(ui, |ui| {
                    if ui.add_enabled(!stopping, egui::Button::new("Stop")).on_hover_text("Keep what was found so far").clicked() {
                        progress.cancel.store(true, Ordering::Relaxed);
                    }
                });
            }
            Dialog::Settings { volume, items, error } => {
                settings_dialog(ui, ws, *volume, items, error.as_deref(), actions, &mut close)
            }
            Dialog::NewImage(d) => new_image_dialog(ui, ws, d, actions, &mut close),
            Dialog::Password(d) => password_dialog(ui, ws, d, actions, &mut close),
            Dialog::ConfirmDelete { names } => {
                ui.heading("Delete permanently?");
                ui.add_space(4.0);
                let n = names.len();
                ui.label(format!(
                    "{n} item{} will be deleted from the image. This cannot be undone.",
                    if n == 1 { "" } else { "s" }
                ));
                for name in names.iter().take(6) {
                    ui.label(RichText::new(format!("  • {name}")).color(theme::TEXT_DIM));
                }
                if n > 6 {
                    ui.label(RichText::new(format!("  … and {} more", n - 6)).color(theme::TEXT_DIM));
                }
                buttons(ui, |ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    let del =
                        ui.add(Button::new(RichText::new("Delete").color(egui::Color32::WHITE)).fill(theme::DANGER));
                    if del.clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        actions.push(Action::ConfirmDelete);
                        close = true;
                    }
                });
            }
            Dialog::ConfirmClose(id) => {
                let name = ws.volumes.get(id).map(|v| v.name.clone()).unwrap_or_default();
                ui.heading("Unsaved image");
                ui.label(format!("\"{name}\" only exists in memory and has changes that were not saved."));
                buttons(ui, |ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    if ui.button("Close without saving").clicked() {
                        actions.push(Action::ForceEject(*id));
                        close = true;
                    }
                    let save = if cfg!(target_arch = "wasm32") { "Download" } else { "Save…" };
                    if ui.button(save).clicked() {
                        actions.push(Action::SaveImage(*id));
                        close = true;
                    }
                });
            }
            Dialog::Busy { volume, activities } => {
                let name = ws.volumes.get(volume).map(|v| v.name.clone()).unwrap_or_default();
                ui.heading("Image is busy");
                ui.label(format!("\"{name}\" can't be ejected right now because it is in use:"));
                for a in activities.iter() {
                    ui.label(RichText::new(format!("  • {a}")).color(theme::TEXT_DIM));
                }
                ui.label("Wait for it to finish, or cancel it from the status bar.");
                buttons(ui, |ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    if ui
                        .add(Button::new(RichText::new("Force Eject…").color(egui::Color32::WHITE)).fill(theme::DANGER))
                        .clicked()
                    {
                        actions.push(Action::WarnForceEject(*volume));
                        close = true;
                    }
                });
            }
            Dialog::EjectFailed { volume, error } => {
                let name = ws.volumes.get(volume).map(|v| v.name.clone()).unwrap_or_default();
                ui.heading("Could not eject");
                ui.label(format!("Writing the last changes to \"{name}\" failed:"));
                ui.colored_label(theme::DANGER, error.as_str());
                ui.label("The image is still open. Force ejecting leaves it without a clean unmount and may lose these changes.");
                buttons(ui, |ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    if ui.button("Try Again").clicked() {
                        actions.push(Action::Eject(*volume));
                        close = true;
                    }
                    if ui
                        .add(Button::new(RichText::new("Force Eject").color(egui::Color32::WHITE)).fill(theme::DANGER))
                        .clicked()
                    {
                        actions.push(Action::ForceEject(*volume));
                        close = true;
                    }
                });
            }
            Dialog::DataLoss { target } => {
                // Force eject = unclean unmount. Writing or not decides how bad it is.
                let (name, writing, unsaved) = match target {
                    ForceTarget::Eject(id) => (
                        ws.volumes.get(id).map(|v| v.name.clone()).unwrap_or_default(),
                        !ws.activities(*id).is_empty(),
                        ws.has_unsaved_memory_image(*id),
                    ),
                    ForceTarget::Quit => (String::new(), true, false),
                };
                let (title, button) = match target {
                    ForceTarget::Eject(_) if writing => ("Force eject while writing?".to_owned(), "Force Eject"),
                    ForceTarget::Eject(_) => (format!("Force eject \"{name}\"?"), "Force Eject"),
                    ForceTarget::Quit => ("Quit while writing?".to_owned(), "Quit Now"),
                };
                ui.heading(RichText::new(title).color(theme::WARN));
                ui.add_space(4.0);
                ui.label(
                    "A force eject is an unclean unmount: the image is disconnected at once, like pulling out a disk, \
                     without writing pending changes or marking it as cleanly unmounted.",
                );
                ui.add_space(4.0);
                let lines: &[&str] = if writing {
                    ui.label(RichText::new("A write is in progress. This can corrupt data:").strong().color(theme::DANGER));
                    &[
                        "files being written will be incomplete or missing",
                        "the file system may be left inconsistent",
                        "a journaling file system will have to recover on the next mount; others may not be repairable",
                    ]
                } else {
                    ui.label(RichText::new("Nothing is being written right now, but:").strong());
                    &[
                        "changes not yet written to the image are lost",
                        "the next mount will find the volume not cleanly unmounted and check or recover it",
                    ]
                };
                for line in lines {
                    ui.label(RichText::new(format!("  • {line}")).color(theme::TEXT_DIM));
                }
                if unsaved {
                    ui.label(RichText::new("  • this in-memory image has unsaved changes; they are discarded").color(theme::TEXT_DIM));
                }
                buttons(ui, |ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    if ui.add(Button::new(RichText::new(button).color(egui::Color32::WHITE)).fill(theme::DANGER)).clicked() {
                        actions.push(match target {
                            ForceTarget::Eject(id) => Action::ForceEject(*id),
                            ForceTarget::Quit => Action::QuitAnyway,
                        });
                        close = true;
                    }
                });
            }
            Dialog::CheckReport { name, result, log, target, repaired } => {
                let (title, color) = match result {
                    Ok(r) if r.is_clean() && r.fixed > 0 => (format!("{name}: repaired, no problems left"), theme::OK),
                    Ok(r) if r.is_clean() => (format!("{name}: no problems found"), theme::OK),
                    Ok(r) => (format!("{name}: {} problem(s) found", r.errors), theme::DANGER),
                    Err(_) if *repaired => (format!("{name}: could not be repaired"), theme::DANGER),
                    Err(_) => (format!("{name}: could not be checked"), theme::DANGER),
                };
                ui.heading(RichText::new(title).color(color));
                match result {
                    Ok(r) => {
                        ui.label(RichText::new(&r.summary).color(theme::TEXT_DIM));
                        if r.warnings > 0 {
                            ui.label(RichText::new(format!("{} warning(s)", r.warnings)).color(theme::WARN));
                        }
                        if r.fixed > 0 {
                            ui.label(RichText::new(format!("{} item(s) repaired, then checked again.", r.fixed)).color(theme::OK));
                        }
                        if !r.is_clean() && r.repairable {
                            ui.label("Every problem found can be repaired safely (counts, bitmaps, link counts: like fsck -p).");
                        } else if !r.is_clean() {
                            ui.label(
                                "Some problems need a decision (reconnecting, clearing or rebuilding things), which is not \
                                 done here: use e2fsck to repair them. Nothing was changed.",
                            );
                        }
                    }
                    Err(e) => {
                        ui.colored_label(theme::DANGER, e.as_str());
                    }
                }
                ui.add_space(4.0);
                log_view(ui, log);
                let offer = match (result, target) {
                    (Ok(r), Some(t)) if r.repairable && !*repaired => Some(t.clone()),
                    _ => None,
                };
                buttons(ui, |ui| {
                    close |= ui.button("OK").clicked();
                    if let Some(t) = offer {
                        let r = ui
                            .add(Button::new(RichText::new("Repair").color(egui::Color32::WHITE)).fill(theme::ACCENT))
                            .on_hover_text("Fix the problems found (the image is ejected for it and opened again)");
                        if r.clicked() {
                            actions.push(Action::Repair(t));
                            close = true;
                        }
                    }
                });
            }
            Dialog::MountReport { name, log, error } => {
                let failed = error.is_some();
                let title = if failed { format!("Could not open {name}") } else { format!("{name} needed attention") };
                ui.heading(title);
                ui.add_space(4.0);
                log_view(ui, log);
                if let Some(e) = error {
                    ui.add_space(4.0);
                    ui.colored_label(theme::DANGER, e.as_str());
                }
                buttons(ui, |ui| close |= ui.button("OK").clicked());
            }
            Dialog::Quit { reasons, writing } => {
                ui.heading(format!("Quit {}?", crate::APP_NAME));
                ui.label("Some images are still in use or have unsaved changes:");
                for r in reasons.iter() {
                    ui.label(RichText::new(format!("  • {r}")).color(theme::TEXT_DIM));
                }
                buttons(ui, |ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    let label = if *writing { "Quit Anyway…" } else { "Quit Anyway" };
                    if ui.add(Button::new(RichText::new(label).color(egui::Color32::WHITE)).fill(theme::DANGER)).clicked() {
                        actions.push(if *writing { Action::WarnForceQuit } else { Action::QuitAnyway });
                        close = true;
                    }
                });
            }
            Dialog::Properties { title, rows, link } => {
                ui.heading(title.as_str());
                ui.add_space(4.0);
                kv_grid(ui, rows.iter().map(|(k, v)| (k.as_str(), v.clone())));
                buttons(ui, |ui| {
                    close |= ui.button("Close").clicked();
                    if let Some(p) = link {
                        if ui.button("Go to target").on_hover_text("Open where the link leads").clicked() {
                            actions.push(Action::GoToLinkTarget(p.clone()));
                            close = true;
                        }
                    }
                });
            }
            Dialog::VolumeInfo(id) => {
                if let Some(v) = ws.volumes.get(id) {
                    ui.heading(&v.name);
                    ui.add_space(4.0);
                    let info = v.info();
                    let mut rows = vec![
                        ("Source", v.source.kind.label().to_owned()),
                        ("Location", v.source.location.clone()),
                        ("Disk format", v.source.container.clone().unwrap_or_else(|| "raw".to_owned())),
                        ("Partition", v.source.partition.map_or("—".to_owned(), |n| n.to_string())),
                        (
                            if v.source.partition.is_some() { "Partition size" } else { "Image size" },
                            super::format_size(v.source.size),
                        ),
                        ("Driver", format!("{} ({})", v.driver.name, v.driver.id)),
                        ("File system", info.fs_name.clone()),
                        ("Label", info.label.clone().unwrap_or_default()),
                        ("Access", v.mode().label().to_owned()),
                        ("Capabilities", v.caps().to_string()),
                        ("Driver supports", v.driver.capabilities.to_string()),
                    ];
                    if let Some(t) = info.total_bytes {
                        rows.push(("Capacity", super::format_size(t)));
                    }
                    if let Some(f) = info.free_bytes {
                        rows.push(("Free", super::format_size(f)));
                    }
                    // What the driver has to say about this volume.
                    for (k, val) in &info.details {
                        rows.push((k.as_str(), val.clone()));
                    }
                    kv_grid(ui, rows.into_iter());
                    if !v.mount_log.is_empty() {
                        egui::CollapsingHeader::new("Mount log").default_open(
                            v.mount_log.iter().any(|e| e.level >= dix_core::LogLevel::Warning),
                        )
                        .show(ui, |ui| log_view(ui, &v.mount_log));
                    }
                    let (id, has_buffer) = (v.id, v.buffer.is_some());
                    buttons(ui, |ui| {
                        close |= ui.button("Close").clicked();
                        if has_buffer {
                            let label =
                                if cfg!(target_arch = "wasm32") { "Download image" } else { "Save image as…" };
                            if ui.button(label).clicked() {
                                actions.push(Action::SaveImage(id));
                            }
                        }
                    });
                } else {
                    close = true;
                }
            }
        }
    });
    if close || modal.should_close() {
        match &state.dialog {
            // Not while writing; back to the open dialog afterwards.
            Some(Dialog::Disk(d)) if d.applying.is_some() => return,
            // A running copy goes on in the background.
            #[cfg(not(target_arch = "wasm32"))]
            Some(Dialog::Copy(d)) if d.job.is_some() => {
                actions.push(Action::HideCopy);
                return;
            }
            Some(Dialog::Disk(d)) => {
                let part = match d.selected {
                    Some(super::disk::Selection::Partition(n)) => Some(n),
                    _ => None,
                };
                match &d.memory {
                    Some((name, data)) => actions.push(Action::ShowOpenDialogFor(name.clone(), data.clone(), part)),
                    None => actions.push(Action::ShowOpenDialogAt(d.path.clone(), part)),
                }
            }
            _ => {}
        }
        state.dialog = None;
    }
}

/// A volume's settings, grouped, with the driver's explanations.
fn password_dialog(ui: &mut Ui, ws: &Workspace, d: &mut PasswordDialog, actions: &mut Vec<Action>, close: &mut bool) {
    let name = ws.volumes.get(&d.volume).map(|v| v.name.clone()).unwrap_or_default();
    ui.heading(format!("Password for {name}"));
    ui.add_space(4.0);
    if !d.reason.is_empty() {
        ui.label(&d.reason);
    }
    ui.label(
        RichText::new("Files encrypted with other passwords can be opened by entering those too.")
            .color(theme::TEXT_DIM),
    );
    ui.add_space(6.0);
    let mut submit = false;
    ui.horizontal(|ui| {
        ui.label("Password:");
        let field = ui.add(TextEdit::singleline(&mut d.password).password(!d.show).desired_width(240.0));
        if !d.focused {
            field.request_focus();
            d.focused = true;
        }
        if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            submit = true;
        }
        ui.checkbox(&mut d.show, "Show");
    });
    if let Some(e) = &d.error {
        ui.add_space(4.0);
        ui.label(RichText::new(e).color(theme::DANGER));
    }
    buttons(ui, |ui| {
        if ui.button("Cancel").clicked() {
            *close = true;
        }
        if ui.add_enabled(!d.password.is_empty(), Button::new("OK")).clicked() {
            submit = true;
        }
    });
    if submit && !d.password.is_empty() {
        actions.push(Action::SubmitPassword(d.volume, d.password.clone()));
    }
}

fn settings_dialog(
    ui: &mut Ui,
    ws: &Workspace,
    volume: VolumeId,
    items: &mut [dix_core::Setting],
    error: Option<&str>,
    actions: &mut Vec<Action>,
    close: &mut bool,
) {
    use dix_core::SettingKind;
    let name = ws.volumes.get(&volume).map(|v| v.name.clone()).unwrap_or_default();
    ui.heading(format!("Settings of {name}"));
    // The file selected in a tab of this volume ("Use selection").
    let selected = ws
        .tabs
        .iter()
        .filter(|t| t.volume == volume)
        .map(|t| t.selected_paths())
        .find(|p| p.len() == 1)
        .map(|p| p[0].as_str().to_owned());
    let mut groups: Vec<String> = Vec::new();
    for s in items.iter() {
        if !groups.contains(&s.group) {
            groups.push(s.group.clone());
        }
    }
    for g in groups {
        ui.add_space(6.0);
        ui.label(RichText::new(&g).strong());
        egui::Grid::new(("settings", &g)).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            for s in items.iter_mut().filter(|s| s.group == g) {
                ui.label(&s.label).on_hover_text(&s.help);
                ui.vertical(|ui| {
                    match s.kind {
                        SettingKind::Flag => {
                            let mut on = s.value == "true";
                            if ui.checkbox(&mut on, "").changed() {
                                s.value = on.to_string();
                            }
                        }
                        SettingKind::File => {
                            ui.horizontal(|ui| {
                                ui.add(TextEdit::singleline(&mut s.value).hint_text("none").desired_width(260.0));
                                let pick = ui
                                    .add_enabled(selected.is_some(), Button::new("Use selection").small())
                                    .on_hover_text("The file selected in the file list")
                                    .on_disabled_hover_text("Select the file in the file list first");
                                if pick.clicked() {
                                    s.value = selected.clone().unwrap_or_default();
                                }
                                if !s.value.is_empty() && ui.small_button("×").on_hover_text("None").clicked() {
                                    s.value.clear();
                                }
                            });
                        }
                        SettingKind::Number => {
                            ui.add(TextEdit::singleline(&mut s.value).desired_width(80.0));
                        }
                        SettingKind::Text => {
                            ui.add(TextEdit::singleline(&mut s.value).desired_width(260.0));
                        }
                        SettingKind::Secret => {
                            ui.add(
                                TextEdit::singleline(&mut s.value)
                                    .password(true)
                                    .hint_text("unchanged")
                                    .desired_width(260.0),
                            );
                        }
                    }
                    ui.label(RichText::new(&s.help).small().color(theme::TEXT_DIM));
                });
                ui.end_row();
            }
        });
    }
    if let Some(e) = error {
        ui.add_space(4.0);
        ui.colored_label(theme::DANGER, e);
    }
    buttons(ui, |ui| {
        if ui.button("Cancel").clicked() {
            *close = true;
        }
        if ui.button(RichText::new("Apply").strong()).on_hover_text("Write the image with these settings").clicked() {
            let values = items.iter().map(|s| (s.key.clone(), s.value.clone())).collect();
            actions.push(Action::ApplySettings(volume, values));
        }
    });
}

/// Driver log lines, coloured by level.
fn log_view(ui: &mut Ui, log: &[dix_core::LogEntry]) {
    egui::ScrollArea::vertical().max_height(220.0).stick_to_bottom(true).show(ui, |ui| {
        for e in log {
            let (prefix, color) = match e.level {
                dix_core::LogLevel::Info => ("", theme::TEXT),
                dix_core::LogLevel::Warning => ("Warning: ", theme::WARN),
                dix_core::LogLevel::Error => ("Error: ", theme::DANGER),
            };
            ui.add(egui::Label::new(RichText::new(format!("{prefix}{}", e.message)).color(color)).wrap());
        }
    });
}

/// "Checking …" / "Opening …" window with the live log while checks and
/// mounts run.
pub fn mounting(ctx: &egui::Context, ws: &Workspace) {
    if let Some(task) = ws.checking.first() {
        progress_window(ctx, &format!("Checking {}…", task.name), &task.log, ws.checking.len() - 1);
        return;
    }
    let Some(task) = ws.mounting.first() else { return };
    progress_window(ctx, &format!("Opening {}…", task.name), &task.log, ws.mounting.len() - 1);
}

fn progress_window(ctx: &egui::Context, title: &str, log: &dix_core::MountLog, more: usize) {
    egui::Modal::new(egui::Id::unique("progress_window")).show(ctx, |ui| {
        ui.set_min_width(440.0);
        ui.set_max_width(560.0);
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new());
            ui.heading(title);
        });
        ui.add_space(4.0);
        if let Some((done, total)) = log.current_progress() {
            let frac = if total == 0 { 0.0 } else { done as f32 / total as f32 };
            ui.add(egui::ProgressBar::new(frac).text(format!("{done} / {total}")));
        }
        log_view(ui, &log.entries());
        if more > 0 {
            ui.label(RichText::new(format!("{more} more waiting")).color(theme::TEXT_DIM));
        }
    });
    ctx.request_repaint_after(std::time::Duration::from_millis(100));
}

fn kv_grid<'a>(ui: &mut Ui, rows: impl Iterator<Item = (&'a str, String)>) {
    egui::Grid::new("kv").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
        for (k, v) in rows {
            if v.is_empty() {
                continue;
            }
            ui.label(RichText::new(k).color(theme::TEXT_DIM));
            ui.add(egui::Label::new(v).wrap());
            ui.end_row();
        }
    });
}

fn driver_combo(ui: &mut Ui, ws: &Workspace, id: &str, selected: &mut Option<String>, auto: bool) {
    let text = match selected {
        None => "Auto-detect".to_owned(),
        Some(s) => ws.registry.get(s).map(|d| d.info().name.to_owned()).unwrap_or_else(|| s.clone()),
    };
    egui::ComboBox::from_id_salt(id).selected_text(text).width(260.0).show_ui(ui, |ui| {
        if auto {
            ui.selectable_value(selected, None, "Auto-detect");
        }
        for d in ws.registry.drivers() {
            let info = d.info();
            let rw = if info.capabilities.is_writable() { "read-write" } else { "read-only" };
            ui.selectable_value(selected, Some(info.id.to_owned()), format!("{}  ({rw})", info.name))
                .on_hover_text(info.description);
        }
    });
}

fn open_dialog(ui: &mut Ui, ws: &Workspace, d: &mut OpenDialog, actions: &mut Vec<Action>, close: &mut bool) {
    let web = cfg!(target_arch = "wasm32");
    ui.heading("Open file system image");
    ui.add_space(6.0);

    let has_upload = d.upload.is_some();
    if let Some((name, data)) = &d.upload {
        ui.horizontal(|ui| {
            ui.label("Image:");
            ui.label(RichText::new(name).strong());
            ui.label(RichText::new(super::format_size(data.len() as u64)).color(theme::TEXT_DIM));
        });
        if web {
            ui.label(
                RichText::new("The image is kept in memory; after changes you can download it.").color(theme::TEXT_DIM),
            );
        }
    } else if web {
        ui.label("Choose an image file from your computer. It is loaded into the browser's memory; after changes you can download the modified image.");
    } else {
        ui.label("Image file or device:");
        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut d.path)
                    .hint_text(if cfg!(windows) {
                        r"C:\images\disk.img  or  \\.\PhysicalDrive1"
                    } else {
                        "/path/disk.img  or  /dev/sdb1"
                    })
                    .desired_width(330.0),
            );
            #[cfg(not(target_arch = "wasm32"))]
            if ui.button("Browse…").clicked() {
                if let Some(p) = crate::platform::native_browse_image_path() {
                    d.path = p.display().to_string();
                }
            }
        });
        #[cfg(not(target_arch = "wasm32"))]
        egui::CollapsingHeader::new("Host disks and partitions").show(ui, |ui| {
            if d.devices.is_none() || ui.small_button("Refresh").clicked() {
                d.devices = Some(
                    crate::platform::list_devices()
                        .into_iter()
                        .map(|h| DeviceRow {
                            path: h.path,
                            description: h.description,
                            size: h.size,
                            is_part: h.parent.is_some(),
                        })
                        .collect(),
                );
            }
            ui.label(
                RichText::new("Raw devices usually require administrator / root rights.")
                    .small()
                    .color(theme::TEXT_DIM),
            );
            egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                egui::Grid::new("host_devices").num_columns(3).spacing([12.0, 2.0]).show(ui, |ui| {
                    for dev in d.devices.clone().unwrap_or_default() {
                        // Disks in bold, their partitions indented below them.
                        let path = if dev.is_part {
                            RichText::new(format!("    {}", dev.path))
                        } else {
                            RichText::new(&dev.path).strong()
                        };
                        let r = ui.selectable_label(d.path == dev.path, path);
                        // A fixed width: a truncating label in a grid shrinks to nothing.
                        let size = egui::vec2(270.0, ui.spacing().interact_size.y);
                        ui.allocate_ui_with_layout(size, egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            let text = RichText::new(&dev.description).color(theme::TEXT_DIM);
                            ui.add(egui::Label::new(text).truncate()).on_hover_text(&dev.description);
                        });
                        let size = dev.size.map(super::format_size).unwrap_or_default();
                        ui.label(RichText::new(size).color(theme::TEXT_DIM));
                        ui.end_row();
                        if r.clicked() {
                            d.path = dev.path.clone();
                        }
                    }
                });
                if d.devices.as_ref().is_some_and(|v| v.is_empty()) {
                    ui.label(RichText::new("No devices found").color(theme::TEXT_DIM));
                }
            });
        });
    }

    refresh_partitions(ws, d);
    partitions_ui(ui, d);
    if let Some(parts) = &d.split {
        ui.add_space(6.0);
        let (first, last) = (parts.first().cloned().unwrap_or_default(), parts.last().cloned().unwrap_or_default());
        ui.label(format!(
            "One part of a split file: all {} parts ({first} … {last}) are opened together, read-only.",
            parts.len()
        ))
        .on_hover_text(parts.join("\n"));
    }

    ui.add_space(8.0);
    egui::Grid::new("open_opts").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("File system:");
        driver_combo(ui, ws, "open_driver", &mut d.driver, true);
        ui.end_row();

        let rw_supported = d.split.is_none()
            && match &d.driver {
                None => true,
                Some(id) => ws.registry.get(id).is_some_and(|dr| dr.info().capabilities.is_writable()),
            };
        if !rw_supported {
            d.mode = AccessMode::ReadOnly;
        }
        ui.label("Access:");
        ui.vertical(|ui| {
            ui.radio_value(&mut d.mode, AccessMode::ReadOnly, "Read-only — the image is never modified");
            ui.add_enabled_ui(rw_supported, |ui| {
                let text = if web {
                    "Read-write — changes stay in memory until you download"
                } else if d.upload.is_some() {
                    "Read-write — changes stay in memory until you save the image"
                } else {
                    "Read-write — changes are written to the image immediately"
                };
                let why = if d.split.is_some() {
                    "The parts of a split file are only read"
                } else {
                    "The selected driver only supports read-only access"
                };
                ui.radio_value(&mut d.mode, AccessMode::ReadWrite, text).on_disabled_hover_text(why);
            });
        });
        ui.end_row();
    });

    buttons(ui, |ui| {
        if ui.button("Cancel").clicked() {
            *close = true;
        }
        let picks_file = web && !has_upload;
        let label = if picks_file { "Choose file…" } else { "Open" };
        let enabled = web || has_upload || !d.path.trim().is_empty();
        let r = ui.add_enabled(enabled, Button::new(RichText::new(label).strong()));
        if r.clicked() || (enabled && !picks_file && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
            actions.push(Action::OpenFromDialog);
        }
        if ui
            .add_enabled(has_upload || (!web && !d.path.trim().is_empty()), Button::new("Partitions…"))
            .on_hover_text("Create, delete and format partitions of this disk or image")
            .clicked()
        {
            if has_upload {
                actions.push(Action::ShowDiskDialogForUpload);
            } else {
                actions.push(Action::ShowDiskDialog(d.path.trim().to_owned()));
            }
        }
        // Check without opening (also works for images that do not mount).
        let can_check = !picks_file && enabled;
        if ui
            .add_enabled(can_check, Button::new("Check"))
            .on_hover_text("Check the file system for consistency (read-only, like fsck -n) without opening it")
            .clicked()
        {
            actions.push(Action::CheckFromDialog);
        }
        if ui
            .add_enabled(can_check, Button::new("Scan for photos…"))
            .on_hover_text(
                "Look for photos and videos by their content, even on a formatted or damaged card (read-only)",
            )
            .clicked()
        {
            actions.push(Action::ScanPhotosFromDialog);
        }
    });
}

/// Bytes of an upload, seen as a (read-only) device without copying them.
struct BytesDevice<'a>(&'a [u8]);

impl dix_core::BlockDevice for BytesDevice<'_> {
    fn info(&self) -> dix_core::DeviceInfo {
        dix_core::DeviceInfo {
            name: "upload".into(),
            kind: dix_core::DeviceKind::Memory,
            size: self.0.len() as u64,
            sector_size: 512,
            read_only: true,
            resizable: false,
        }
    }
    fn size(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> dix_core::Result<()> {
        dix_core::device::check_range(offset, buf.len(), self.size())?;
        buf.copy_from_slice(&self.0[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }
    fn write_at(&mut self, _: u64, _: &[u8]) -> dix_core::Result<()> {
        Err(dix_core::Error::new(dix_core::ErrorKind::ReadOnly, "read-only"))
    }
}

/// Reads the partition table of the chosen image when the choice changes,
/// and preselects the partition opening would pick by itself.
/// The parts of the split set a host file belongs to (see `split_parts`).
#[cfg(not(target_arch = "wasm32"))]
fn split_names(upload: &Option<(String, Vec<u8>)>, path: &str) -> Option<Vec<String>> {
    if upload.is_some() || path.is_empty() {
        return None;
    }
    let parts = dix_core::device::host::split_parts(std::path::Path::new(path))?;
    Some(parts.iter().map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).collect())
}

#[cfg(target_arch = "wasm32")]
fn split_names(_: &Option<(String, Vec<u8>)>, _: &str) -> Option<Vec<String>> {
    None
}

fn refresh_partitions(ws: &Workspace, d: &mut OpenDialog) {
    let key = match &d.upload {
        Some((name, data)) => format!("upload:{name}:{}", data.len()),
        None => d.path.trim().to_owned(),
    };
    if d.scan.as_ref().is_some_and(|(k, _)| *k == key) {
        return;
    }
    d.partition = PartitionChoice::Auto;
    d.split = split_names(&d.upload, &key);
    let result = match &d.upload {
        Some((name, data)) => scan_upload(ws, name, data),
        None => scan_path(ws, &key),
    };
    if let Ok(Some(s)) = &result {
        if let Some(n) = s.default_choice() {
            d.partition = PartitionChoice::Number(n);
        }
        if let Some(n) = d.prefer.take().filter(|n| s.table.get(*n).is_some()) {
            d.partition = PartitionChoice::Number(n);
        }
    }
    d.scan = Some((key, result));
}

/// The partitions of an uploaded image; of the disk inside it when it is a
/// virtual machine disk or DMG (as opening it will see it).
fn scan_upload(ws: &Workspace, name: &str, data: &[u8]) -> Result<Option<PartitionScan>, String> {
    let mut bytes = BytesDevice(data);
    let container = ws.registry.formats().iter().any(|f| f.probe(&mut bytes));
    if !container {
        return dix_core::partition::scan(&ws.registry, &mut bytes).map_err(|e| e.to_string());
    }
    // Containers need a device of their own: a copy (they are compact).
    let dev = dix_core::device::MemoryDevice::new(name, data.to_vec()).with_read_only(true);
    let mut disk = ws.registry.open_disk(Box::new(dev), None).map_err(|e| e.to_string())?;
    dix_core::partition::scan(&ws.registry, disk.as_mut()).map_err(|e| e.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn scan_path(ws: &Workspace, path: &str) -> Result<Option<PartitionScan>, String> {
    let p = std::path::Path::new(path);
    // Only things that exist (not every prefix typed), and never a folder.
    if path.is_empty() || !p.exists() || p.is_dir() {
        return Ok(None);
    }
    match ws.scan_host(p) {
        Ok(scan) => Ok(scan),
        // Locked by another program.
        Err(e) if e.kind() == dix_core::ErrorKind::InUse => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(target_arch = "wasm32")]
fn scan_path(_: &Workspace, _: &str) -> Result<Option<PartitionScan>, String> {
    Ok(None)
}

/// The partitions of the chosen image, to pick one.
fn partitions_ui(ui: &mut Ui, d: &mut OpenDialog) {
    let Some((_, result)) = &d.scan else { return };
    let scan = match result {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(e) => {
            ui.add_space(4.0);
            ui.label(RichText::new(format!("Could not read the image: {e}")).small().color(theme::TEXT_DIM));
            return;
        }
    };
    ui.add_space(8.0);
    let t = &scan.table;
    ui.label(format!("Partitions ({}, {}-byte sectors):", t.scheme.label(), t.sector_size));
    for note in &t.notes {
        ui.label(RichText::new(format!("⚠ {note}")).small().color(theme::WARN));
    }
    if t.partitions.is_empty() {
        ui.label(RichText::new("The partition table is empty.").color(theme::TEXT_DIM));
        return;
    }
    egui::ScrollArea::vertical().id_salt("partitions").max_height(150.0).show(ui, |ui| {
        egui::Grid::new("partitions").num_columns(3).spacing([14.0, 4.0]).show(ui, |ui| {
            for (p, driver) in t.partitions.iter().zip(&scan.drivers) {
                let usable = p.may_hold_a_file_system() && !p.truncated;
                let mut choice = d.partition;
                ui.add_enabled_ui(usable, |ui| {
                    ui.radio_value(&mut choice, PartitionChoice::Number(p.number), p.label());
                })
                .response
                .on_disabled_hover_text(if p.truncated {
                    "This partition extends beyond the end of the image"
                } else {
                    "This partition does not hold a file system"
                });
                d.partition = choice;
                ui.label(RichText::new(super::format_size(p.size)).color(theme::TEXT_DIM));
                let fs = match driver {
                    Some(name) => RichText::new(name.as_str()),
                    None if usable => RichText::new("not recognised").color(theme::TEXT_DIM),
                    None => RichText::new("—").color(theme::TEXT_DIM),
                };
                ui.label(fs);
                ui.end_row();
            }
        });
    });
}

fn new_image_dialog(ui: &mut Ui, ws: &Workspace, d: &mut NewImageDialog, actions: &mut Vec<Action>, close: &mut bool) {
    ui.heading("Create a new image");
    ui.add_space(6.0);
    let formatters: Vec<_> = ws.registry.formatters().map(|f| f.info()).collect();
    if formatters.is_empty() {
        ui.label("No driver can create images yet.");
        buttons(ui, |ui| *close |= ui.button("Close").clicked());
        return;
    }
    if !formatters.iter().any(|f| f.id == d.driver) {
        d.driver = formatters[0].id.to_owned();
    }
    let before = (d.driver.clone(), d.variant.clone());
    egui::Grid::new("new_opts").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
        ui.label("File system:");
        let current = formatters.iter().find(|f| f.id == d.driver).map(|f| f.name).unwrap_or("");
        egui::ComboBox::from_id_salt("new_driver").selected_text(current).show_ui(ui, |ui| {
            for f in &formatters {
                ui.selectable_value(&mut d.driver, f.id.to_owned(), f.name).on_hover_text(f.description);
            }
        });
        ui.end_row();
        let variants = ws.registry.get(&d.driver).map(|dr| dr.format_variants()).unwrap_or(&[]);
        let mut extension = ".img";
        d.sized = true;
        if variants.is_empty() {
            d.variant = None;
        } else {
            if !variants.iter().any(|v| Some(v.id) == d.variant.as_deref()) {
                d.variant = Some(variants[0].id.to_owned());
            }
            ui.label("Type:");
            let cur = variants.iter().find(|v| Some(v.id) == d.variant.as_deref()).unwrap_or(&variants[0]);
            egui::ComboBox::from_id_salt("new_variant")
                .selected_text(format!("{} — {}", cur.name, cur.description))
                .show_ui(ui, |ui| {
                    for v in variants {
                        ui.selectable_value(
                            &mut d.variant,
                            Some(v.id.to_owned()),
                            format!("{} — {}", v.name, v.description),
                        );
                    }
                });
            ui.end_row();
            let cur = variants.iter().find(|v| Some(v.id) == d.variant.as_deref()).unwrap_or(&variants[0]);
            // A partition table and alignment take up to 3 MiB more.
            let extra = if d.table.is_some() { 3.0 } else { 0.0 };
            let min_mib = cur.min_size as f64 / (1024.0 * 1024.0) + extra;
            if d.size_mib < min_mib {
                d.size_mib = min_mib;
            }
            extension = cur.extension;
            d.sized = cur.sized;
        }
        // A virtual machine disk: its extension instead.
        let vm_ext = d.container.as_deref().and_then(|c| container_ext(ws, c)).map(|e| format!(".{e}"));
        let extension: &str = match (&vm_ext, d.sized) {
            (Some(e), true) => e.as_str(),
            _ => extension,
        };
        // A different type: the name gets its extension ("x.img" → "x.zip").
        if (d.driver.clone(), d.variant.clone()) != before || d.container != d.named_for {
            d.named_for = d.container.clone();
            let mut known: Vec<String> = ws
                .registry
                .formatters()
                .flat_map(|f| f.format_variants().iter().map(|v| v.extension.to_owned()))
                .collect();
            known.extend(ws.registry.formats().iter().filter_map(|f| f.creates()).map(|(e, _)| format!(".{e}")));
            known.push(".img".into());
            known.sort_by_key(|e| std::cmp::Reverse(e.len()));
            let stem = known.iter().find_map(|e| d.name.strip_suffix(e.as_str())).unwrap_or(&d.name).to_owned();
            d.name = format!("{stem}{extension}");
        }
        ui.label("Name:");
        ui.text_edit_singleline(&mut d.name);
        ui.end_row();
        if d.sized {
            ui.label("Size:");
            ui.add(egui::DragValue::new(&mut d.size_mib).range(0.0625..=4096.0).speed(0.25).suffix(" MiB"));
            ui.end_row();
        }
        ui.label("Volume label:");
        ui.text_edit_singleline(&mut d.label);
        ui.end_row();
        if d.sized {
            use dix_core::partition::Scheme;
            ui.label("Partitions:");
            let text = match d.table {
                None => "None — just the file system",
                Some(Scheme::Gpt) => "GPT, one partition",
                Some(Scheme::Mbr) => "MBR, one partition",
                // Not offered (Apple partition maps are only read).
                Some(Scheme::Apm) => "APM",
            };
            egui::ComboBox::from_id_salt("new_table")
                .selected_text(text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut d.table, None, "None — just the file system");
                    ui.selectable_value(&mut d.table, Some(Scheme::Gpt), "GPT, one partition");
                    ui.selectable_value(&mut d.table, Some(Scheme::Mbr), "MBR, one partition");
                })
                .response
                .on_hover_text("A disk image with a partition table, like a USB stick or a hard disk");
            ui.end_row();
        } else {
            d.table = None;
        }
        if d.sized {
            ui.label("Disk format:");
            let creators: Vec<(String, String)> = ws
                .registry
                .formats()
                .iter()
                .filter_map(|f| f.creates().map(|(_, about)| (f.name().to_owned(), about.to_owned())))
                .collect();
            let text = d
                .container
                .as_deref()
                .and_then(|c| creators.iter().find(|(n, _)| n == c).map(|(_, a)| a.clone()))
                .unwrap_or_else(|| "Raw image — takes its whole size".into());
            egui::ComboBox::from_id_salt("new_container")
                .selected_text(text)
                .width(320.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut d.container, None, "Raw image — takes its whole size");
                    for (name, about) in &creators {
                        ui.selectable_value(&mut d.container, Some(name.clone()), about);
                    }
                })
                .response
                .on_hover_text("A virtual machine disk only takes the space of what is written in it");
            ui.end_row();
        } else {
            d.container = None;
        }
        if !cfg!(target_arch = "wasm32") {
            ui.label("Store:");
            ui.vertical(|ui| {
                ui.radio_value(&mut d.in_memory, false, "As an image file (choose location)");
                ui.radio_value(&mut d.in_memory, true, "In memory (save later)");
            });
            ui.end_row();
        }
    });
    let caps = ws.registry.get(&d.driver).map(|dr| dr.info().capabilities).unwrap_or_default();
    if !caps.contains(Capabilities::WRITE) {
        ui.colored_label(theme::WARN, "This driver cannot write files; the new image will stay empty.");
    }
    buttons(ui, |ui| {
        if ui.button("Cancel").clicked() {
            *close = true;
        }
        if ui.add_enabled(!d.name.trim().is_empty(), Button::new(RichText::new("Create").strong())).clicked() {
            actions.push(Action::CreateFromDialog);
        }
    });
}

/// The file extension of container format `name`, if it makes new disks.
fn container_ext(ws: &Workspace, name: &str) -> Option<&'static str> {
    ws.registry.formats().iter().find(|f| f.name() == name).and_then(|f| f.creates()).map(|(e, _)| e)
}

#[cfg(all(test, feature = "ext4", feature = "vdisk"))]
mod upload_tests {
    use super::*;

    /// An uploaded qcow2 with a GPT inside: the dialog lists the partition
    /// inside it (and would pick it), as for a file on the desktop.
    #[test]
    fn uploaded_vm_disks_show_their_partitions() {
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let opts = dix_core::FormatOptions { label: Some("IN".into()), variant: Some("ext4".into()) };
        let gpt = Some(dix_core::partition::Scheme::Gpt);
        let id = ws.create_memory_disk("d.qcow2".into(), 32 << 20, "ext4", opts, gpt, Some("qcow2")).unwrap();
        ws.volumes[&id].fs().sync().unwrap();
        let bytes = ws.volumes[&id].buffer.as_ref().unwrap().snapshot();
        assert!(bytes.starts_with(b"QFI\xfb"));
        let scan = scan_upload(&ws, "d.qcow2", &bytes).unwrap().expect("a partition table");
        assert_eq!(scan.table.partitions.len(), 1);
        assert_eq!(scan.default_choice(), Some(1));
        // A raw upload is scanned as it is.
        let raw = scan_upload(&ws, "raw.img", &vec![0u8; 1 << 20]).unwrap();
        assert!(raw.is_none());
    }
}

#[cfg(all(test, feature = "exfat", feature = "vdisk"))]
mod dmg_exfat_tests {
    use super::*;
    /// The settings that hung the web version (exFAT, GPT, DMG, in memory).
    /// The hang was a 32-bit truncation (see `vdisk::upto`); here, at
    /// least, the same path runs through.
    #[test]
    fn exfat_gpt_dmg_in_memory() {
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let opts = dix_core::FormatOptions { label: None, variant: Some("exfat".into()) };
        let gpt = Some(dix_core::partition::Scheme::Gpt);
        let id = ws.create_memory_disk("new-image.dmg".into(), 64 << 20, "fat", opts, gpt, Some("DMG")).unwrap();
        assert_eq!(ws.volumes[&id].fs().info().fs_name, "exFAT");
    }
}
