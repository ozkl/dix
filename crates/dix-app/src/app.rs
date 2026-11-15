//! The eframe application: wires model, platform and UI together.

use crate::model::Workspace;
use crate::platform::{DropTarget, HostEvent, HostIo, ImageSource, OpenRequest};
use crate::ui::dialogs::{Dialog, NewImageDialog, OpenDialog, PasswordDialog};
use crate::ui::{self, file_list, theme, Action, ToastLevel, UiState};
use dix_core::{AccessMode, Capabilities, VfsPath};
use eframe::egui::{self, Frame, Key, Margin, Modifiers, RichText};
use std::collections::VecDeque;
use std::sync::Arc;

pub struct ExplorerApp {
    ws: Workspace,
    host: HostIo,
    state: UiState,
    ctx: egui::Context,
    /// Location shown last frame (to auto-expand the folder tree).
    last_location: Option<(crate::model::VolumeId, VfsPath)>,
    /// Dropped images waiting for the open dialog (one dialog at a time).
    open_queue: VecDeque<ImageSource>,
    /// The user chose "Quit Anyway".
    quit_confirmed: bool,
    /// Browser: whether the "Leave site?" warning is armed.
    leave_warning: bool,
    /// The app icon, shown on the welcome screen.
    logo: Option<egui::TextureHandle>,
    /// A disk copy running without its dialog.
    #[cfg(not(target_arch = "wasm32"))]
    copy_hidden: Option<Box<ui::copy::CopyDialog>>,
}

impl ExplorerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Vec<std::path::PathBuf>) -> Self {
        theme::apply(&cc.egui_ctx);
        let mut app = Self {
            ws: Workspace::new(dix_drivers::default_registry()),
            host: HostIo::new(cc.egui_ctx.clone()),
            state: UiState::default(),
            ctx: cc.egui_ctx.clone(),
            last_location: None,
            open_queue: VecDeque::new(),
            quit_confirmed: false,
            leave_warning: false,
            logo: logo(&cc.egui_ctx),
            #[cfg(not(target_arch = "wasm32"))]
            copy_hidden: None,
        };
        let ctx = cc.egui_ctx.clone();
        app.ws.set_notifier(Arc::new(move || ctx.request_repaint()));
        for path in args {
            app.open_source(ImageSource::Path(path), OpenRequest { mode: AccessMode::ReadOnly, ..Default::default() });
        }
        app
    }

    /// Starts mounting an image in the background. Returns false if the image
    /// could not even be opened (the error is shown).
    fn open_source(&mut self, src: ImageSource, req: OpenRequest) -> bool {
        match src {
            #[cfg(not(target_arch = "wasm32"))]
            ImageSource::Path(p) if self.ws.find_open_path(&p, req.partition).is_some() => {
                // One image (partition), one mount: never open it twice.
                match self.ws.find_open_path(&p, req.partition) {
                    Some(Ok(id)) => {
                        let name = self.ws.volumes[&id].name.clone();
                        self.state.info(format!("{name} is already open"));
                        match self.ws.tabs.iter().position(|t| t.volume == id) {
                            Some(i) => self.ws.active = i,
                            None => self.ws.open_tab(id, VfsPath::root()),
                        }
                    }
                    _ => self.state.info(format!("{} is already being opened", p.display())),
                }
                true
            }
            #[cfg(not(target_arch = "wasm32"))]
            ImageSource::Path(p) => match self.ws.open_host(&p, req.mode) {
                Ok(dev) => {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.display().to_string());
                    self.ws.begin_mount(dev, name, req.driver, req.mode, None, req.partition);
                    true
                }
                Err(e) => {
                    self.state.error(format!("{}: {e}", p.display()));
                    false
                }
            },
            #[cfg(target_arch = "wasm32")]
            ImageSource::Path(p) => {
                self.state.error(format!("{}: host paths are not available in the browser", p.display()));
                false
            }
            ImageSource::Bytes { name, data } => {
                let dev = dix_core::device::MemoryDevice::new(name.clone(), data);
                let buffer = dev.buffer();
                match self.ws.registry.open_disk(Box::new(dev), None) {
                    Ok(dev) => {
                        self.ws.begin_mount(dev, name, req.driver, req.mode, Some(buffer), req.partition);
                        true
                    }
                    Err(e) => {
                        self.state.error(format!("{name}: {e}"));
                        false
                    }
                }
            }
        }
    }

    /// Reports finished mounts. Anything the driver warned about (e.g. the
    /// volume was not cleanly unmounted) is shown in a mount report.
    fn handle_mounts(&mut self) {
        for m in self.ws.poll_recoveries() {
            if matches!(self.state.dialog, Some(Dialog::Scanning { .. })) {
                self.state.dialog = None;
            }
            match m.result {
                Ok(id) => {
                    let found = self.ws.volumes[&id]
                        .info()
                        .details
                        .iter()
                        .find(|(k, _)| k == "Files found")
                        .map(|(_, v)| v.clone())
                        .unwrap_or_default();
                    self.state.info(format!("{}: {found} found", m.name));
                }
                Err(e) => self.state.error(format!("{}: {e}", m.name)),
            }
        }
        if self.ws.is_recovering() {
            self.ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        for m in self.ws.poll_mounts() {
            let worst = m.log.iter().map(|e| e.level).max();
            let notable = worst.is_some_and(|l| l >= dix_core::LogLevel::Warning);
            match &m.result {
                Ok(id) if !notable => {
                    let v = &self.ws.volumes[id];
                    let msg = format!("Opened {} ({}, {})", v.name, v.driver.name, v.mode().label().to_lowercase());
                    self.state.info(msg);
                }
                Err(e) if !notable => self.state.error(format!("{}: {e}", m.name)),
                _ => {
                    let error = m.result.as_ref().err().map(|e| e.to_string());
                    self.state.dialog = Some(Dialog::MountReport { name: m.name, log: m.log, error });
                }
            }
        }
    }

    /// Reports finished checks and repairs, and opens images again.
    fn handle_checks(&mut self) {
        for o in self.ws.poll_checks() {
            let result = o.result.map_err(|e| e.to_string());
            let target = o.target.clone();
            self.state.dialog =
                Some(Dialog::CheckReport { name: o.name, result, log: o.log, target, repaired: o.repaired });
            match o.reopen {
                #[cfg(not(target_arch = "wasm32"))]
                Some(crate::model::Reopen::Path { path, partition, mode, driver }) => {
                    self.open_source(ImageSource::Path(path), OpenRequest { mode, driver: Some(driver), partition });
                }
                #[cfg(target_arch = "wasm32")]
                Some(crate::model::Reopen::Path { .. }) => {}
                Some(crate::model::Reopen::Memory { name, buffer, partition, mode, driver, modified }) => {
                    self.ws.remount_memory(name, buffer, partition, mode, driver, modified);
                }
                None => {}
            }
        }
    }

    #[cfg_attr(target_arch = "wasm32", allow(clippy::needless_return))]
    /// Scans the image chosen in the open dialog (all of it: a formatted
    /// card's partitions may be gone) for photos and videos.
    fn scan_photos_from_dialog(&mut self) {
        let Some(Dialog::Open(d)) = &mut self.state.dialog else { return };
        let (dev, name) = if let Some((name, data)) = d.upload.clone() {
            let dev = dix_core::device::MemoryDevice::new(name.clone(), data);
            (self.ws.registry.open_disk(Box::new(dev), None), name)
        } else {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let path = std::path::PathBuf::from(d.path.trim());
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                (self.ws.open_host(&path, AccessMode::ReadOnly), name)
            }
            #[cfg(target_arch = "wasm32")]
            (Err(dix_core::Error::invalid_input("choose an image file first")), String::new())
        };
        match dev {
            Ok(dev) => {
                let info = dev.info();
                let source = crate::model::volume_source(&info, dev.size());
                let title = format!("{name} — found photos");
                let progress = self.ws.begin_media_scan(dev, title, source);
                self.state.dialog = Some(Dialog::Scanning { name, progress });
            }
            Err(e) => self.state.error(format!("{name}: {e}")),
        }
    }

    fn check_from_dialog(&mut self) {
        let Some(Dialog::Open(d)) = &mut self.state.dialog else { return };
        let (driver, partition) = (d.driver.clone(), d.partition);
        if let Some((name, data)) = d.upload.clone() {
            let dev = dix_core::device::MemoryDevice::new(name.clone(), data);
            match self.ws.registry.open_disk(Box::new(dev), None).and_then(|dev| self.ws.in_partition(dev, partition)) {
                Ok(dev) => {
                    self.state.dialog = None;
                    self.ws.begin_check(dev, name, driver, false, None, None);
                }
                Err(e) => self.state.error(format!("{name}: {e}")),
            }
            // (The last statement in the browser, where no host path follows.)
            #[allow(clippy::needless_return)]
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = std::path::PathBuf::from(d.path.trim());
            if self.ws.find_open_path(&path, partition).is_some() {
                self.state.error("This image is open: right-click it in the sidebar and choose \"Check file system…\"");
                return;
            }
            let dev = self.ws.open_host(&path, AccessMode::ReadOnly);
            match dev.and_then(|dev| self.ws.in_partition(dev, partition)) {
                Ok(dev) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string());
                    self.state.dialog = None;
                    let target = Some(crate::model::RepairTarget::Path { path: path.clone(), partition });
                    self.ws.begin_check(dev, name, driver, false, None, target);
                }
                Err(e) => self.state.error(format!("{}: {e}", path.display())),
            }
        }
    }

    /// Asks for a password when a volume wants one (and nothing else is
    /// being asked).
    fn handle_password_requests(&mut self) {
        if self.state.dialog.is_some() {
            return;
        }
        if let Some((id, reason)) = self.ws.password_request() {
            self.state.dialog = Some(Dialog::Password(PasswordDialog::new(id, reason)));
        }
    }

    /// Reports finished write jobs.
    fn handle_jobs(&mut self) {
        use crate::model::JobEnd;
        for j in self.ws.poll_jobs(self.state.now) {
            match j.end {
                JobEnd::Done => self.state.info(format!("{} — done", j.title)),
                JobEnd::Cancelled => self.state.info(format!("{} — cancelled", j.title)),
                JobEnd::Failed(_) if j.interrupted => {
                    self.state.error(format!("{} — interrupted: the image was ejected", j.title))
                }
                JobEnd::Failed(e) => self.state.error(format!("{} — failed: {e}", j.title)),
            }
        }
    }

    fn handle_host_events(&mut self) {
        for ev in self.host.poll() {
            match ev {
                // An open dialog already showing takes the image (the latest
                // drop wins); otherwise one dialog per image, in turn.
                HostEvent::ImageDropped(src) => match &mut self.state.dialog {
                    Some(Dialog::Open(d)) => d.fill(src),
                    _ => self.open_queue.push_back(src),
                },
                HostEvent::Import { volume, dir, items } => {
                    if let Err(e) = self.ws.import(volume, &dir, items) {
                        self.state.error(format!("Import failed: {e}"));
                    }
                }
                HostEvent::ImportPaths { volume, dir, paths } => {
                    #[cfg(not(target_arch = "wasm32"))]
                    if let Err(e) = self.ws.import_host(volume, &dir, &paths) {
                        self.state.error(format!("Import failed: {e}"));
                    }
                    #[cfg(target_arch = "wasm32")]
                    let _ = (volume, dir, paths);
                }
                HostEvent::Info(m) => self.state.info(m),
                HostEvent::Error(m) => self.state.error(m),
            }
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
        if dropped.is_empty() {
            return;
        }
        // Context decides: with a writable image open, dropped files are copied
        // into the current folder; otherwise each one goes to the open dialog,
        // where the user picks read-only or read-write.
        // With the open dialog showing, a drop is always an image for it.
        let opening = matches!(self.state.dialog, Some(Dialog::Open(_)));
        let target = match self.ws.current_volume() {
            Some(v) if !opening && v.caps().contains(Capabilities::WRITE) => {
                let tab = self.ws.tab().expect("a volume is current only through a tab");
                DropTarget::Import { volume: tab.volume, dir: tab.path.clone() }
            }
            _ => DropTarget::OpenDialog,
        };
        self.host.read_dropped(dropped, target);
    }

    /// Shows the open dialog for the next dropped image, if any.
    fn next_dropped_image(&mut self) {
        if self.state.dialog.is_some() {
            return;
        }
        let Some(src) = self.open_queue.pop_front() else { return };
        let mut d = OpenDialog::default();
        d.fill(src);
        self.state.dialog = Some(Dialog::Open(d));
    }

    /// Window close: refuse while images are busy or unsaved, unless the user
    /// confirmed. Pending changes are written before the app goes away.
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.quit_confirmed {
            return;
        }
        #[allow(unused_mut)]
        let mut reasons = self.ws.quit_blockers();
        #[allow(unused_mut)]
        let mut writing = !self.ws.busy_volumes().is_empty();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let copy = match &self.state.dialog {
                Some(Dialog::Copy(d)) => d.job.as_ref(),
                _ => self.copy_hidden.as_ref().and_then(|d| d.job.as_ref()),
            };
            if let Some(job) = copy {
                reasons.push(format!("{}: still running", job.title));
                writing |= job.writes_disk;
            }
        }
        if !reasons.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.state.dialog = Some(Dialog::Quit { reasons, writing });
            return;
        }
        // Nothing in the way: unmount everything cleanly before exiting.
        let errors = self.ws.unmount_all();
        if !errors.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.state.dialog = Some(Dialog::Quit { reasons: errors, writing: false });
        }
    }

    /// Browser: there is no quit dialog, but closing the tab would lose
    /// in-memory images, so let the browser ask "Leave site?".
    fn update_leave_warning(&mut self) {
        let want = self
            .ws
            .volumes
            .keys()
            .any(|&id| self.ws.has_unsaved_memory_image(id) || !self.ws.activities(id).is_empty());
        if want != self.leave_warning {
            self.leave_warning = want;
            crate::platform::set_leave_warning(want);
        }
    }

    /// Debounced start of searches and collection of their results.
    fn update_search(&mut self, ctx: &egui::Context) {
        const DEBOUNCE: f64 = 0.3;
        if let Some(tab) = self.ws.tabs.get(self.ws.active) {
            if tab.search_pending {
                let waited = self.state.now - self.state.search_changed_at;
                if waited >= DEBOUNCE {
                    self.ws.start_search();
                } else {
                    ctx.request_repaint_after(std::time::Duration::from_secs_f64(DEBOUNCE - waited));
                }
            }
        }
        if self.ws.poll_searches() && cfg!(target_arch = "wasm32") {
            // The browser search advances a little every frame.
            ctx.request_repaint();
        }
    }

    fn handle_keyboard(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        if self.state.dialog.is_some() || self.ws.tabs.is_empty() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::O)) && self.state.dialog.is_none() {
                actions.push(Action::ShowOpenDialog);
            }
            return;
        }
        if ctx.text_edit_focused() {
            return;
        }
        ctx.input_mut(|i| {
            for ev in &i.events {
                match ev {
                    egui::Event::Copy => actions.push(Action::Copy),
                    egui::Event::Cut => actions.push(Action::Cut),
                    egui::Event::Paste(_) => actions.push(Action::Paste),
                    _ => {}
                }
            }
            let mut key = |m: Modifiers, k: Key, a: Action| {
                if i.consume_key(m, k) {
                    actions.push(a);
                }
            };
            key(Modifiers::COMMAND | Modifiers::SHIFT, Key::N, Action::NewFolder);
            key(Modifiers::COMMAND, Key::O, Action::ShowOpenDialog);
            key(Modifiers::COMMAND, Key::A, Action::SelectAll);
            key(Modifiers::NONE, Key::Delete, Action::RequestDelete);
            key(Modifiers::NONE, Key::F2, Action::BeginRename);
            key(Modifiers::NONE, Key::F5, Action::Refresh);
            key(Modifiers::NONE, Key::Backspace, Action::Up);
            key(Modifiers::ALT, Key::ArrowUp, Action::Up);
            key(Modifiers::ALT, Key::ArrowLeft, Action::Back);
            key(Modifiers::ALT, Key::ArrowRight, Action::Forward);
            key(Modifiers::NONE, Key::ArrowDown, Action::MoveCursor(1, false));
            key(Modifiers::NONE, Key::ArrowUp, Action::MoveCursor(-1, false));
            key(Modifiers::SHIFT, Key::ArrowDown, Action::MoveCursor(1, true));
            key(Modifiers::SHIFT, Key::ArrowUp, Action::MoveCursor(-1, true));
            key(Modifiers::NONE, Key::PageDown, Action::MoveCursor(20, false));
            key(Modifiers::NONE, Key::PageUp, Action::MoveCursor(-20, false));
            key(Modifiers::NONE, Key::Home, Action::MoveCursor(isize::MIN / 2, false));
            key(Modifiers::NONE, Key::End, Action::MoveCursor(isize::MAX / 2, false));
            if i.consume_key(Modifiers::COMMAND, Key::W) {
                actions.push(Action::CloseTab(self.ws.active));
            }
        });
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            if let Some(t) = self.ws.tabs.get_mut(self.ws.active) {
                t.clear_search();
            }
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
            if let Some(sel) = self.ws.tab().filter(|t| t.search.is_some()).and_then(|t| t.result_sel) {
                actions.push(Action::OpenSearchHit(sel));
            } else if let Some(e) = self.ws.tab().and_then(|t| t.single_selection()) {
                actions.push(Action::OpenEntry(e.name.clone()));
            }
        }
    }

    fn report<T>(&mut self, r: dix_core::Result<T>, ok: impl FnOnce(T) -> Option<String>) {
        match r {
            Ok(v) => {
                if let Some(m) = ok(v) {
                    self.state.info(m);
                }
            }
            Err(e) => self.state.error(e.to_string()),
        }
    }

    fn current_dir(&self) -> Option<(crate::model::VolumeId, VfsPath)> {
        self.ws.tab().map(|t| (t.volume, t.path.clone()))
    }

    /// The status bar's line for a disk copy running in the background.
    fn copy_status(&self) -> Option<String> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.copy_hidden.as_ref().and_then(|d| d.status_line())
        }
        #[cfg(target_arch = "wasm32")]
        None
    }

    /// A hidden disk copy: when it ends, its outcome as a message.
    #[cfg(not(target_arch = "wasm32"))]
    fn poll_hidden_copy(&mut self) {
        let Some(d) = &mut self.copy_hidden else { return };
        d.poll();
        match d.finished.take() {
            Some(Ok(o)) => {
                self.state.info(o.message);
                self.copy_hidden = None;
            }
            Some(Err(e)) => {
                self.state.error(e);
                self.copy_hidden = None;
            }
            None => self.ctx.request_repaint_after(std::time::Duration::from_millis(500)),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn copy_action(&mut self, action: Action) {
        use crate::model::imaging;
        use ui::copy::{CopyDialog, Tab};
        match action {
            Action::ShowCopyDisk | Action::ShowHiddenCopy => {
                if self.state.dialog.is_some() && matches!(action, Action::ShowHiddenCopy) {
                    return;
                }
                let d = self.copy_hidden.take().unwrap_or_else(|| Box::new(CopyDialog::new(Tab::Save)));
                self.state.dialog = Some(Dialog::Copy(d));
            }
            Action::HideCopy => {
                if let Some(Dialog::Copy(d)) = self.state.dialog.take() {
                    if d.job.is_some() {
                        self.copy_hidden = Some(d);
                    }
                }
            }
            Action::OpenCopiedImage(p) => {
                let mut d = OpenDialog::default();
                d.fill(ImageSource::Path(p));
                self.state.dialog = Some(Dialog::Open(d));
            }
            Action::StartSaveDisk => {
                let Some(Dialog::Copy(d)) = &mut self.state.dialog else { return };
                let source = d.source.trim().to_owned();
                let path = std::path::PathBuf::from(d.dest.trim());
                if path.is_dir() {
                    d.error = Some(format!("{} is a folder: add a file name.", path.display()));
                    return;
                }
                if imaging::same_file(&path, std::path::Path::new(&source)) {
                    d.error = Some("The image cannot be saved over what is copied.".into());
                    return;
                }
                if imaging::image_is_on(&path, &source) {
                    d.error = Some(format!(
                        "{} is on the disk being copied: save the image on another disk.",
                        path.display()
                    ));
                    return;
                }
                // A rescue reads the disk as it is; a copy reads through a VM disk's container.
                let src = if d.rescue {
                    dix_core::device::host::open_path(&source, AccessMode::ReadOnly)
                } else {
                    imaging::open_source(&self.ws.registry, &source)
                };
                let src = match src {
                    Ok(s) => s,
                    Err(e) => {
                        d.error = Some(e.to_string());
                        return;
                    }
                };
                let ctx = self.ctx.clone();
                d.job = Some(if d.rescue {
                    imaging::start_rescue(ctx, src, path, d.used_only, d.retries)
                } else {
                    let formats = self.ws.registry.formats().to_vec();
                    let format = d.format.as_ref().and_then(|n| formats.iter().find(|f| f.name() == n).cloned());
                    imaging::start_save(ctx, src, path, format, formats, d.used_only)
                });
            }
            Action::CheckWriteImage => {
                let Some(Dialog::Copy(d)) = &mut self.state.dialog else { return };
                let image = std::path::PathBuf::from(d.image.trim());
                let disk = d.disk.clone();
                if imaging::image_is_on(&image, &disk) {
                    d.error = Some(format!("The image is on {disk} itself: copy it to another disk first."));
                    return;
                }
                let ours: Vec<String> = self
                    .ws
                    .disk_blockers(std::path::Path::new(&disk))
                    .into_iter()
                    .filter(|b| !b.contains("the system uses it"))
                    .collect();
                if !ours.is_empty() {
                    d.error = Some(format!("Not now: {}.", ours.join("; ")));
                    return;
                }
                let disk_size = d.devices.as_ref().and_then(|l| l.iter().find(|x| x.path == disk)).and_then(|x| x.size);
                if let (Some((_, Ok(info))), Some(ds)) = (&d.image_info, disk_size) {
                    if info.size.is_some_and(|s| s > ds) {
                        d.error = Some(format!(
                            "The image ({}) is larger than the disk ({}).",
                            ui::format_size(info.size.unwrap_or(0)),
                            ui::format_size(ds)
                        ));
                        return;
                    }
                }
                d.confirm = Some(dix_core::device::host::os_uses(&disk));
            }
            Action::StartWriteImage { unmount } => {
                let Some(Dialog::Copy(d)) = &mut self.state.dialog else { return };
                let image = std::path::PathBuf::from(d.image.trim());
                d.confirm = None;
                d.job = Some(imaging::start_write(
                    self.ctx.clone(),
                    self.ws.registry.clone(),
                    image,
                    d.disk.clone(),
                    d.verify,
                    unmount,
                ));
            }
            _ => {}
        }
    }

    fn apply(&mut self, action: Action) {
        if matches!(
            action,
            Action::ShowCopyDisk
                | Action::StartSaveDisk
                | Action::CheckWriteImage
                | Action::StartWriteImage { .. }
                | Action::HideCopy
                | Action::ShowHiddenCopy
                | Action::OpenCopiedImage(_)
        ) {
            #[cfg(not(target_arch = "wasm32"))]
            self.copy_action(action);
            return;
        }
        let ws = &mut self.ws;
        match action {
            Action::SelectTab(i) => {
                if i < ws.tabs.len() {
                    ws.active = i;
                    self.state.renaming = None;
                    self.state.address_edit = None;
                }
            }
            Action::CloseTab(i) => ws.close_tab(i),
            Action::NewTabForVolume(id) => ws.open_tab(id, VfsPath::root()),
            Action::Navigate(p) => {
                ws.with_tab(|t, fs| t.navigate(fs, p));
                self.state.renaming = None;
            }
            Action::Back => {
                ws.with_tab(|t, fs| t.go_back(fs));
            }
            Action::Forward => {
                ws.with_tab(|t, fs| t.go_forward(fs));
            }
            Action::Up => {
                ws.with_tab(|t, fs| t.go_up(fs));
            }
            Action::Refresh => ws.refresh_all(),
            Action::OpenEntry(name) => {
                let Some((tab, _)) = ws.current() else { return };
                let Some(entry) = tab.entry(&name).cloned() else { return };
                if entry.metadata.is_dir() {
                    if let Ok(p) = tab.path.join(&name) {
                        ws.with_tab(|t, fs| t.navigate(fs, p));
                    }
                } else {
                    tab.select_only(&name);
                    self.state.show_preview = true;
                }
            }
            Action::OpenEntryAsImage(name) => {
                // Read into memory (as an uploaded image), then the open
                // dialog: partitions, driver, mode.
                let max: u64 = if cfg!(target_pointer_width = "32") { 1 << 30 } else { 16 << 30 };
                let read = ws.with_tab(|t, fs| -> dix_core::Result<Vec<u8>> {
                    let p = t.path.join(&name)?;
                    let size = fs.metadata(&p)?.size;
                    if size > max {
                        return Err(dix_core::Error::new(
                            dix_core::ErrorKind::NoSpace,
                            format!("{name} is too large to open from inside an image ({size} bytes); export it first"),
                        ));
                    }
                    fs.read_to_vec(&p)
                });
                match read {
                    Some(Ok(data)) => self.apply(Action::ShowOpenDialogFor(name, data, None)),
                    Some(Err(e)) => self.state.error(format!("{name}: {e}")),
                    None => {}
                }
            }
            Action::HexFile(name) => {
                let Some((tab, _)) = ws.current() else { return };
                let (volume, path) = (tab.volume, tab.path.join(&name));
                match path.map_err(|e| e.to_string()).and_then(|p| ui::hex::HexView::file(ws, volume, p)) {
                    Ok(v) => self.state.hex.push(v),
                    Err(e) => self.state.error(format!("{name}: {e}")),
                }
            }
            Action::HexVolume(id) => match ws.raw_device(id) {
                Ok(dev) => {
                    let name = ws.volumes.get(&id).map(|v| v.name.clone()).unwrap_or_default();
                    self.state.hex.push(ui::hex::HexView::device(format!("{name} (raw)"), dev));
                }
                Err(e) => self.state.error(e.to_string()),
            },
            Action::Info(text) => self.state.info(text),
            // Handled before (copy_action).
            Action::ShowCopyDisk
            | Action::StartSaveDisk
            | Action::CheckWriteImage
            | Action::StartWriteImage { .. }
            | Action::HideCopy
            | Action::ShowHiddenCopy
            | Action::OpenCopiedImage(_) => {}
            Action::ShowConvert(id) => {
                self.state.dialog = Some(Dialog::Convert(Box::new(ui::convert::ConvertDialog::new(ws, id))));
            }
            Action::StartConvert => {
                let Some(Dialog::Convert(d)) = &mut self.state.dialog else { return };
                let targets = ui::convert::targets(ws);
                let ext = targets.iter().find(|t| t.0 == d.format).map_or("img", |t| t.1);
                let Some(vol) = ws.volumes.get(&d.volume) else { return };
                let loc = std::path::Path::new(&vol.source.location);
                let stem = loc.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "disk".into());
                let mut name = format!("{stem}.{ext}");
                if loc.file_name().is_some_and(|n| n.to_string_lossy() == name) {
                    name = format!("{stem}-converted.{ext}");
                }
                let formats: Vec<Arc<dyn dix_core::container::DiskFormat>> = ws.registry.formats().to_vec();
                let format = d.format.as_ref().and_then(|n| formats.iter().find(|f| f.name() == n).cloned());
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let Some(path) = crate::platform::native_pick_save_path("Save the converted disk", &name) else {
                        return;
                    };
                    let mut src = match ws.whole_disk(d.volume) {
                        Ok(s) => s,
                        Err(e) => {
                            d.error = Some(e.to_string());
                            return;
                        }
                    };
                    let total = src.size();
                    let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
                    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let (tx, rx) = std::sync::mpsc::channel();
                    let (done2, cancel2) = (done.clone(), cancel.clone());
                    std::thread::spawn(move || {
                        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let mut run = || -> dix_core::Result<u64> {
                            let file = Box::new(dix_core::device::host::FileDevice::create(&path, 0)?);
                            let mut progress = |at: u64, _: u64| {
                                done2.store(at, std::sync::atomic::Ordering::Relaxed);
                                !cancel2.load(std::sync::atomic::Ordering::Relaxed)
                            };
                            dix_core::container::convert(
                                src.as_mut(),
                                file,
                                format.as_deref(),
                                &file_name,
                                &formats,
                                &mut progress,
                            )
                        };
                        let r = run();
                        if r.is_err() {
                            let _ = std::fs::remove_file(&path);
                        }
                        let _ = tx.send(r.map(|w| (file_name, w)).map_err(|e| e.to_string()));
                    });
                    d.running = Some(ui::convert::Job { done, total, cancel, result: rx });
                }
                #[cfg(target_arch = "wasm32")]
                {
                    let r = ws.whole_disk(d.volume).and_then(|mut src| {
                        let mem = dix_core::device::MemoryDevice::new(name.clone(), Vec::new());
                        let buffer = mem.buffer();
                        let w = dix_core::container::convert(
                            src.as_mut(),
                            Box::new(mem),
                            format.as_deref(),
                            &name,
                            &formats,
                            &mut |_: u64, _: u64| true,
                        )?;
                        Ok((buffer.snapshot(), w))
                    });
                    match r {
                        Ok((bytes, written)) => {
                            self.host.save_bytes(&name, bytes);
                            self.state.info(format!("Converted into {name} ({} of data)", ui::format_size(written)));
                            self.state.dialog = None;
                        }
                        Err(e) => d.error = Some(e.to_string()),
                    }
                }
            }
            Action::ScanPhotos(id) => match ws.begin_media_scan_of(id) {
                Ok(progress) => {
                    let name = ws.volumes.get(&id).map(|v| v.name.clone()).unwrap_or_default();
                    self.state.dialog = Some(Dialog::Scanning { name, progress });
                }
                Err(e) => self.state.error(e.to_string()),
            },
            Action::ScanPhotosFromDialog => self.scan_photos_from_dialog(),
            Action::OpenLostPartition(start, size) => {
                let Some(Dialog::Disk(d)) = &self.state.dialog else { return };
                let base = std::path::Path::new(&d.path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .or_else(|| d.memory.as_ref().map(|(n, _)| n.clone()))
                    .unwrap_or_default();
                let name = format!("{base} @ {}", ui::format_size(start));
                let dev = d.device(ws).and_then(|dev| {
                    dix_core::device::SliceDevice::new(dev, start, size, name.clone())
                        .map(|s| Box::new(s) as Box<dyn dix_core::BlockDevice>)
                });
                match dev {
                    Ok(dev) => {
                        self.state.dialog = None;
                        ws.begin_mount(
                            dev,
                            name,
                            None,
                            AccessMode::ReadOnly,
                            None,
                            dix_core::partition::PartitionChoice::Whole,
                        );
                    }
                    Err(e) => self.state.error(e.to_string()),
                }
            }
            Action::RecoverDeleted(id) => match ws.begin_recover(id) {
                Ok(()) => self.state.info("Looking for deleted files…"),
                Err(e) => self.state.error(e.to_string()),
            },
            Action::SelectAll => {
                if let Some(t) = ws.tabs.get_mut(ws.active) {
                    t.select_all();
                }
            }
            Action::MoveCursor(delta, extend) => {
                if let Some(t) = ws.tabs.get_mut(ws.active).filter(|t| t.search.is_some()) {
                    let n = t.hit_count() as isize;
                    if n > 0 {
                        let cur = t.result_sel.map(|c| c as isize).unwrap_or(-1);
                        let next = cur.saturating_add(delta).clamp(0, n - 1) as usize;
                        t.result_sel = Some(next);
                        self.state.scroll_to = Some(next);
                    }
                } else if let Some(t) = ws.tabs.get_mut(ws.active) {
                    t.move_cursor(delta, extend);
                    self.state.scroll_to = t.anchor;
                }
            }
            Action::NewFolder => match ws.new_folder() {
                Ok(name) => file_list::begin_rename(&mut self.state, &name),
                Err(e) => self.state.error(e.to_string()),
            },
            Action::BeginRename => {
                let caps = ws.current_volume().map(|v| v.caps()).unwrap_or_default();
                if let Some(e) = ws.tab().and_then(|t| t.single_selection()) {
                    if caps.contains(Capabilities::RENAME) {
                        let name = e.name.clone();
                        file_list::begin_rename(&mut self.state, &name);
                    }
                }
            }
            Action::CommitRename { old, new } => {
                let r = ws.rename(&old, &new);
                self.report(r, |_| None);
            }
            Action::RequestDelete => {
                let caps = ws.current_volume().map(|v| v.caps()).unwrap_or_default();
                let names: Vec<String> =
                    ws.tab().map(|t| t.selected_entries().map(|e| e.name.clone()).collect()).unwrap_or_default();
                if names.is_empty() {
                } else if !caps.contains(Capabilities::REMOVE) {
                    self.state.error("Cannot delete: the image is read-only or the driver does not support it");
                } else {
                    self.state.dialog = Some(Dialog::ConfirmDelete { names });
                }
            }
            Action::ConfirmDelete => {
                let r = ws.delete_selection();
                self.report(r, |_| None);
            }
            Action::Cut | Action::Copy => {
                let cut = matches!(action, Action::Cut);
                let paths: Vec<String> =
                    ws.tab().map(|t| t.selected_paths().iter().map(|p| p.to_string()).collect()).unwrap_or_default();
                if cut && !ws.current_volume().is_some_and(|v| v.caps().contains(Capabilities::REMOVE)) {
                    self.state.error("Cannot cut: the image is read-only");
                    return;
                }
                let n = ws.copy_selection(cut);
                if n > 0 {
                    // Also put the paths on the system clipboard (this makes
                    // Ctrl+V produce a paste event as well).
                    self.ctx.copy_text(paths.join("\n"));
                    self.state.info(format!("{} {n} item(s)", if cut { "Cut" } else { "Copied" }));
                }
            }
            Action::Paste => {
                if ws.clipboard.is_none() {
                    return;
                }
                let r = ws.paste();
                self.report(r, |_| None);
            }
            Action::ImportFiles | Action::ImportFolder => {
                let Some((vol, dir)) = ws.tab().map(|t| (t.volume, t.path.clone())) else { return };
                if !ws.volumes[&vol].caps().contains(Capabilities::WRITE) {
                    self.state.error("Cannot import: the image is read-only");
                } else if matches!(action, Action::ImportFiles) {
                    self.host.pick_import_files(vol, dir);
                } else {
                    self.host.pick_import_folder(vol, dir);
                }
            }
            // Natively: pick where, then copy in the background, in pieces.
            #[cfg(not(target_arch = "wasm32"))]
            Action::Export => {
                if ws.tab().is_none_or(|t| t.selection.is_empty()) {
                    return self.state.error("Export: nothing selected");
                }
                let single = ws.selected_single_file();
                if let Some(target) = crate::platform::pick_export_target(single.as_deref()) {
                    if let Err(e) = ws.export_to_host(target) {
                        self.state.error(format!("Export failed: {e}"));
                    }
                }
            }
            // In the browser a download is made from bytes in memory.
            #[cfg(target_arch = "wasm32")]
            Action::Export => match ws.export_selection() {
                Ok(items) => self.host.export(items),
                Err(e) => self.state.error(format!("Export failed: {e}")),
            },
            Action::Properties => self.show_properties(),
            Action::OpenSearchHit(i) | Action::RevealSearchHit(i) => {
                let reveal = matches!(action, Action::RevealSearchHit(_));
                let Some(hit) = ws.tab().and_then(|t| t.hit(i).cloned()) else { return };
                if hit.entry.metadata.is_dir() && !reveal {
                    ws.with_tab(|t, fs| t.navigate(fs, hit.path.clone()));
                } else if let Some(parent) = hit.path.parent() {
                    ws.with_tab(|t, fs| {
                        t.navigate(fs, parent);
                        t.select_only(&hit.entry.name);
                    });
                    self.state.scroll_to = ws.tab().and_then(|t| t.anchor);
                    if !reveal {
                        self.state.show_preview = true;
                    }
                }
            }
            Action::ShowOpenDialog => self.state.dialog = Some(Dialog::Open(OpenDialog::default())),
            Action::ShowNewImageDialog => self.state.dialog = Some(Dialog::NewImage(NewImageDialog::default())),
            Action::ShowVolumeInfo(id) => self.state.dialog = Some(Dialog::VolumeInfo(id)),
            Action::GoToLinkTarget(link) => self.go_to_link_target(&link),
            Action::ShowSettings(id) => {
                if let Some(v) = self.ws.volumes.get(&id) {
                    let items = v.fs().settings();
                    self.state.dialog = Some(Dialog::Settings { volume: id, items, error: None });
                }
            }
            Action::ShowPassword(id) => {
                let n = self.ws.volumes.get(&id).map(|v| v.name.clone()).unwrap_or_default();
                let reason = format!("{n} has encrypted files.");
                self.state.dialog = Some(Dialog::Password(PasswordDialog::new(id, reason)));
            }
            Action::SubmitPassword(id, password) => match self.ws.add_password(id, &password) {
                Ok(()) => {
                    self.state.dialog = None;
                    // The preview shows what could not be read before.
                    self.state.preview = Default::default();
                    self.state.info("Password accepted");
                }
                Err(e) => {
                    if let Some(Dialog::Password(d)) = &mut self.state.dialog {
                        d.error = Some(e.to_string());
                        d.password.clear();
                        d.focused = false;
                    }
                }
            },
            Action::ApplySettings(id, values) => match self.ws.apply_settings(id, &values) {
                Ok(()) => {
                    self.state.dialog = None;
                    self.state.info("Settings applied: the image was written");
                }
                Err(e) => {
                    if let Some(Dialog::Settings { error, .. }) = &mut self.state.dialog {
                        *error = Some(e.to_string());
                    }
                }
            },
            Action::SaveImage(id) => match ws.image_bytes(id) {
                Ok((name, bytes)) => self.host.save_bytes(&name, bytes),
                Err(e) => self.state.error(e.to_string()),
            },
            // Eject: busy? → unsaved in-memory image? → write pending changes.
            Action::Eject(id) => {
                let Some(name) = ws.volumes.get(&id).map(|v| v.name.clone()) else { return };
                let activities = ws.activities(id);
                if !activities.is_empty() {
                    self.state.dialog = Some(Dialog::Busy { volume: id, activities });
                } else if ws.has_unsaved_memory_image(id) {
                    self.state.dialog = Some(Dialog::ConfirmClose(id));
                } else {
                    match ws.close_volume(id) {
                        Ok(()) => self.state.info(format!("Ejected {name}")),
                        Err(e) => self.state.dialog = Some(Dialog::EjectFailed { volume: id, error: e.to_string() }),
                    }
                }
            }
            Action::ForceEject(id) => {
                if let Some(name) = ws.volumes.get(&id).map(|v| v.name.clone()) {
                    ws.force_close_volume(id);
                    self.state.info(format!("Force ejected {name} (unclean unmount)"));
                }
            }
            Action::QuitAnyway => {
                // Cut off running writes, unmount everything else cleanly.
                for id in ws.busy_volumes() {
                    ws.force_close_volume(id);
                }
                let _ = ws.unmount_all();
                self.quit_confirmed = true;
                self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Action::WarnForceEject(id) => {
                self.state.dialog = Some(Dialog::DataLoss { target: crate::ui::dialogs::ForceTarget::Eject(id) });
            }
            Action::WarnForceQuit => {
                self.state.dialog = Some(Dialog::DataLoss { target: crate::ui::dialogs::ForceTarget::Quit });
            }
            Action::CancelJob(id) => ws.cancel_job(id),
            Action::OpenFromDialog => self.open_from_dialog(),
            Action::CheckFromDialog => self.check_from_dialog(),
            Action::CheckVolume(id) => {
                if let Err(e) = ws.check_volume(id) {
                    self.state.error(format!("Cannot check: {e}"));
                }
            }
            Action::Repair(target) => {
                if let Err(e) = ws.repair(&target) {
                    self.state.error(format!("Cannot repair: {e}"));
                }
            }
            Action::CreateFromDialog => self.create_from_dialog(),
            Action::ShowDiskDialog(path) => {
                self.state.dialog = Some(Dialog::Disk(Box::new(crate::ui::disk::DiskDialog::open(&self.ws, &path))));
            }
            Action::ShowDiskDialogForUpload => {
                let Some(Dialog::Open(d)) = &mut self.state.dialog else { return };
                let Some((name, data)) = d.upload.take() else { return };
                let disk = crate::ui::disk::DiskDialog::open_memory(&self.ws, name, data);
                self.state.dialog = Some(Dialog::Disk(Box::new(disk)));
            }
            Action::ShowOpenDialogAt(path, prefer) => {
                let d = OpenDialog { path, prefer, ..Default::default() };
                self.state.dialog = Some(Dialog::Open(d));
            }
            Action::ShowOpenDialogFor(name, data, prefer) => {
                let mut d = OpenDialog { prefer, ..Default::default() };
                d.fill(ImageSource::Bytes { name, data });
                self.state.dialog = Some(Dialog::Open(d));
            }
            Action::ApplyDisk => self.apply_disk(false),
            Action::UnmountAndApplyDisk => self.apply_disk(true),
            Action::OpenPartitionFromDisk(n) => {
                let Some(Dialog::Disk(d)) = &mut self.state.dialog else { return };
                let mut open = OpenDialog { path: d.path.clone(), prefer: Some(n), ..Default::default() };
                if let Some((name, data)) = d.memory.take() {
                    open.path.clear();
                    open.fill(ImageSource::Bytes { name, data });
                }
                self.state.dialog = Some(Dialog::Open(open));
            }
        }
    }

    fn open_from_dialog(&mut self) {
        let Some(Dialog::Open(d)) = &mut self.state.dialog else { return };
        let req = OpenRequest { mode: d.mode, driver: d.driver.clone(), partition: d.partition };
        if let Some((name, data)) = d.upload.take() {
            self.state.dialog = None;
            self.open_source(ImageSource::Bytes { name, data }, req);
            return;
        }
        if cfg!(target_arch = "wasm32") {
            // The dialog stays: the chosen file comes back into it.
            self.host.pick_image();
            return;
        }
        let path = std::path::PathBuf::from(d.path.trim());
        if self.open_source(ImageSource::Path(path), req) {
            self.state.dialog = None;
        }
    }

    /// Writes the partition editor's changes on a background thread, unless
    /// something uses the disk. With `unmount`, the volumes the operating
    /// system mounted from it are unmounted first.
    /// Writes the changes into an image in memory (an upload), right away.
    fn apply_disk_in_memory(&mut self) {
        let Some(Dialog::Disk(d)) = &mut self.state.dialog else { return };
        let (Ok(editor), Some((name, data))) = (&d.editor, &mut d.memory) else { return };
        let result = editor.apply_to_bytes(&self.ws.registry, name, std::mem::take(data));
        let (tx, rx) = std::sync::mpsc::channel();
        let _ =
            tx.send(match result {
                Ok(bytes) => {
                    *data = bytes;
                    Ok(vec![
                        "The image in memory has the new partitions: open one, or download the image after.".to_owned()
                    ])
                }
                Err(e) => Err(e.to_string()),
            });
        d.applying = Some(rx);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn apply_disk(&mut self, unmount: bool) {
        if matches!(&self.state.dialog, Some(Dialog::Disk(d)) if d.memory.is_some()) {
            return self.apply_disk_in_memory();
        }
        let Some(Dialog::Disk(d)) = &mut self.state.dialog else { return };
        let Ok(editor) = &d.editor else { return };
        let path = editor.path.clone();
        let system = dix_core::device::host::os_uses(&path.to_string_lossy());
        let blockers = self.ws.disk_blockers(&path);
        // Volumes open here must be ejected here; the system's may be
        // unmounted on the way.
        let ours: Vec<&String> = blockers.iter().filter(|b| !b.contains("the system uses it")).collect();
        if !blockers.is_empty() && (!unmount || !ours.is_empty()) {
            d.error = Some(format!("Nothing was written: {}.", blockers.join("; ")));
            d.system_uses = ours.is_empty() && !system.is_empty();
            return;
        }
        let (editor, registry, ctx) = (editor.clone(), self.ws.registry.clone(), self.ctx.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = (|| {
                if unmount {
                    let p = path.to_string_lossy();
                    dix_core::device::host::unmount(&p)?;
                    let left = dix_core::device::host::os_uses(&p);
                    if !left.is_empty() {
                        return Err(format!("still in use after unmounting: {}", left.join("; ")));
                    }
                }
                editor.apply(&registry).map_err(|e| e.to_string())
            })();
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        d.applying = Some(rx);
    }

    #[cfg(target_arch = "wasm32")]
    fn apply_disk(&mut self, _unmount: bool) {
        self.apply_disk_in_memory();
    }

    fn create_from_dialog(&mut self) {
        let Some(Dialog::NewImage(d)) = &self.state.dialog else { return };
        // Archives size themselves.
        let size = if d.sized { (d.size_mib * 1024.0 * 1024.0) as u64 } else { 0 };
        let label = Some(d.label.trim().to_owned()).filter(|l| !l.is_empty());
        let opts = dix_core::FormatOptions { label, variant: d.variant.clone() };
        let (name, driver, in_memory, table) = (d.name.trim().to_owned(), d.driver.clone(), d.in_memory, d.table);
        let container = d.container.clone();
        let result = if in_memory {
            self.ws.create_memory_disk(name, size, &driver, opts, table, container.as_deref())
        } else {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let Some(path) = crate::platform::native_pick_new_image_path(&name) else { return };
                self.ws.create_file_disk(&path, size, &driver, opts, table, container.as_deref())
            }
            #[cfg(target_arch = "wasm32")]
            unreachable!("web images are always in memory")
        };
        match result {
            Ok(_) => {
                self.state.dialog = None;
                self.state.info("Image created");
            }
            Err(e) => self.state.error(format!("Could not create the image: {e}")),
        }
    }

    /// Follows a link in the current tab: into a folder, or to a file's
    /// folder with the file selected. Says why when it leads nowhere.
    fn go_to_link_target(&mut self, link: &VfsPath) {
        use crate::model::LinkEnd;
        let Some((_, vol)) = self.ws.current() else { return };
        let end = {
            let mut fs = vol.fs();
            match fs.metadata(link).map(|m| m.symlink_target) {
                Ok(Some(t)) => crate::model::resolve_link(fs.as_mut(), link, &t),
                Ok(None) => return self.state.error(format!("{link} is not a link")),
                Err(e) => return self.state.error(e.to_string()),
            }
        };
        match end {
            LinkEnd::Found(p, m) if m.is_dir() => {
                self.ws.with_tab(|t, fs| t.navigate(fs, p));
            }
            LinkEnd::Found(p, _) => {
                let (Some(parent), Some(name)) = (p.parent(), p.file_name().map(str::to_owned)) else { return };
                self.ws.with_tab(|t, fs| {
                    t.navigate(fs, parent);
                    t.select_only(&name);
                });
                self.state.scroll_to = self.ws.tab().and_then(|t| t.anchor);
            }
            LinkEnd::Missing(p) => self.state.error(format!("The link is broken: nothing at {p}")),
            LinkEnd::Loop => self.state.error("The links go round in a circle"),
            LinkEnd::Outside(t) => self.state.error(format!("{t} is outside this image")),
        }
    }

    fn show_properties(&mut self) {
        let Some((tab, vol)) = self.ws.current() else { return };
        let paths = tab.selected_paths();
        let path = match paths.as_slice() {
            [p] => p.clone(),
            [] => tab.path.clone(),
            many => {
                let (mut files, mut dirs, mut bytes) = (0, 0, 0);
                for p in many {
                    if let Ok((f, d, b)) = dix_core::ops::tree_stats(vol.fs().as_mut(), p) {
                        files += f;
                        dirs += d;
                        bytes += b;
                    }
                }
                let rows = vec![
                    ("Selected".into(), format!("{} items", many.len())),
                    ("Contains".into(), format!("{files} files, {} folders", dirs)),
                    ("Total size".into(), ui::format_size(bytes)),
                ];
                self.state.dialog = Some(Dialog::Properties { title: "Properties".into(), rows, link: None });
                return;
            }
        };
        let meta = match vol.fs().metadata(&path) {
            Ok(m) => m,
            Err(e) => return self.state.error(e.to_string()),
        };
        let name = path.file_name().map(str::to_owned).unwrap_or_else(|| vol.name.clone());
        let entry = dix_core::DirEntry { name: name.clone(), metadata: meta.clone() };
        let mut rows: Vec<(String, String)> = vec![
            ("Name".into(), name.clone()),
            ("Location".into(), path.parent().map(|p| p.to_string()).unwrap_or_else(|| "/".into())),
            ("Type".into(), crate::model::tab_type_label(&entry)),
        ];
        // A link: where it points, and where that leads in this image.
        let mut link = None;
        if let Some(target) = &meta.symlink_target {
            rows.push(("Link to".into(), target.clone()));
            use crate::model::LinkEnd;
            let end = crate::model::resolve_link(vol.fs().as_mut(), &path, target);
            let absolute = if target.starts_with('/') { " — an absolute path, looked up in this image" } else { "" };
            if matches!(end, LinkEnd::Found(..)) {
                link = Some(path.clone());
            }
            let text = match end {
                LinkEnd::Found(p, m) => match m.kind {
                    dix_core::EntryKind::Directory => format!("{p} (folder){absolute}"),
                    dix_core::EntryKind::File => format!("{p} (file, {}){absolute}", ui::format_size(m.size)),
                    _ => format!("{p} (special file){absolute}"),
                },
                LinkEnd::Missing(p) => format!("{p} — nothing there: the link is broken"),
                LinkEnd::Loop => "the links go round in a circle".into(),
                LinkEnd::Outside(t) => format!("{t} — a Windows path, outside this image"),
            };
            rows.push(("Resolves to".into(), text));
        }
        if meta.is_dir() {
            if let Ok((f, d, b)) = dix_core::ops::tree_stats(vol.fs().as_mut(), &path) {
                rows.push(("Contains".into(), format!("{f} files, {} folders", d - 1)));
                rows.push(("Size".into(), format!("{} ({b} bytes)", ui::format_size(b))));
            }
        } else {
            rows.push(("Size".into(), format!("{} ({} bytes)", ui::format_size(meta.size), meta.size)));
        }
        let opt = |v: Option<String>| v.unwrap_or_default();
        rows.push(("Modified".into(), ui::format_time(meta.modified)));
        rows.push(("Created".into(), ui::format_time(meta.created)));
        rows.push(("Accessed".into(), ui::format_time(meta.accessed)));
        rows.push(("Permissions".into(), opt(meta.mode.map(|m| format!("{} ({:o})", ui::format_mode(m), m & 0o7777)))));
        rows.push(("Owner (uid:gid)".into(), opt(meta.uid.zip(meta.gid).map(|(u, g)| format!("{u}:{g}")))));
        rows.push(("Inode".into(), opt(meta.inode.map(|i| i.to_string()))));
        self.state.dialog = Some(Dialog::Properties { title: format!("{name} properties"), rows, link });
    }

    fn welcome(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        // Scrolls when the window is too short for the lists.
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.welcome_body(ui, actions));
    }

    fn welcome_body(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.08).max(20.0));
            match &self.logo {
                Some(t) => {
                    ui.add(egui::Image::new(t).fit_to_exact_size(egui::vec2(96.0, 96.0)));
                }
                None => {
                    ui::icons::entry_widget(ui, dix_core::EntryKind::Directory, 72.0);
                }
            }
            ui.add_space(8.0);
            ui.label(RichText::new(crate::APP_NAME).size(26.0).strong());
            ui.label(RichText::new(concat!("Version ", env!("CARGO_PKG_VERSION"))).color(theme::TEXT_DIM).size(12.0));
            if ui.link(RichText::new("Licenses").size(12.0)).clicked() {
                self.state.licenses = true;
            }
            ui.add_space(4.0);
            ui.label(RichText::new("Browse and edit the contents of file system images.").color(theme::TEXT_DIM));
            ui.add_space(18.0);
            ui.horizontal(|ui| {
                let w = 150.0 * 2.0 + ui.spacing().item_spacing.x;
                ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                if ui.add_sized([150.0, 34.0], egui::Button::new(RichText::new("Open image…").size(15.0))).clicked() {
                    actions.push(Action::ShowOpenDialog);
                }
                if ui.add_sized([150.0, 34.0], egui::Button::new(RichText::new("New image…").size(15.0))).clicked() {
                    actions.push(Action::ShowNewImageDialog);
                }
            });
            ui.add_space(10.0);
            ui.label(RichText::new("…or drop an image file onto this window.").color(theme::TEXT_DIM));
            ui.add_space(30.0);
            ui.label(RichText::new("Available drivers").strong());
            ui.add_space(4.0);
            for d in self.ws.registry.drivers() {
                let info = d.info();
                let (txt, color) = if info.capabilities.is_writable() {
                    ("read-write", theme::WARN)
                } else {
                    ("read-only", theme::OK)
                };
                // One centred line, wrapped to the panel (a long description
                // must not widen the page, which would move the centre).
                let font = egui::TextStyle::Body.resolve(ui.style());
                let part =
                    |c: egui::Color32| egui::TextFormat { font_id: font.clone(), color: c, ..Default::default() };
                let mut job = egui::text::LayoutJob::default();
                job.append(info.name, 0.0, part(ui.visuals().strong_text_color()));
                job.append(txt, 8.0, part(color));
                job.append(info.description, 8.0, part(theme::TEXT_DIM));
                job.wrap.max_width = ui.available_width();
                job.halign = egui::Align::Center;
                ui.label(job);
                ui.add_space(6.0);
            }
            // Disk containers: opened like raw images, the file systems
            // inside them by the drivers above.
            let formats = self.ws.registry.formats();
            if !formats.is_empty() {
                ui.add_space(10.0);
                ui.label(RichText::new("Disk formats").strong());
                ui.add_space(4.0);
                let all: Vec<&str> = formats.iter().map(|f| f.name()).collect();
                let new: Vec<&str> = formats.iter().filter(|f| f.creates().is_some()).map(|f| f.name()).collect();
                ui.label(
                    RichText::new(format!(
                        "{} — virtual machine and Apple disk images, opened like raw images",
                        all.join(", ")
                    ))
                    .color(theme::TEXT_DIM),
                );
                if !new.is_empty() {
                    ui.label(
                        RichText::new(format!("New image… can make {} disks", new.join(", "))).color(theme::TEXT_DIM),
                    );
                }
            }
        });
    }

    fn toasts(&mut self, ctx: &egui::Context) {
        let now = self.state.now;
        self.state.toasts.retain(|t| t.until > now);
        if self.state.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::unique("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-14.0, -34.0))
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                for t in &self.state.toasts {
                    let color = match t.level {
                        ToastLevel::Info => theme::ACCENT_HOVER,
                        ToastLevel::Error => theme::DANGER,
                    };
                    Frame::new()
                        .fill(egui::Color32::from_rgb(44, 44, 44))
                        .stroke(egui::Stroke::new(1.0, color))
                        .corner_radius(6.0)
                        .inner_margin(Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.set_max_width(420.0);
                            ui.label(&t.text);
                        });
                    ui.add_space(6.0);
                }
            });
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}

impl eframe::App for ExplorerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.state.now = ctx.input(|i| i.time);
        self.handle_host_events();
        self.handle_drops(&ctx);
        self.next_dropped_image();
        self.update_search(&ctx);
        self.handle_mounts();
        self.handle_checks();
        self.handle_jobs();
        #[cfg(not(target_arch = "wasm32"))]
        self.poll_hidden_copy();
        self.handle_password_requests();
        if !self.ws.jobs.is_empty() && cfg!(target_arch = "wasm32") {
            ctx.request_repaint(); // browser jobs advance once per frame
        }
        self.handle_close_request(&ctx);
        self.update_leave_warning();

        let mut actions = Vec::new();
        self.handle_keyboard(&ctx, &mut actions);

        // Keep the folder tree expanded down to the current location.
        let loc = self.current_dir();
        if loc != self.last_location {
            if let Some((vol, path)) = &loc {
                for a in path.ancestors_inclusive() {
                    self.state.expanded.insert((*vol, a));
                }
            }
            self.last_location = loc;
        }
        if self.state.show_preview {
            ui::preview::update(&mut self.state.preview, &mut self.ws);
        }
        let can_import_folders = self.host.can_import_folders();

        egui::Panel::top("tabs")
            .frame(Frame::new().fill(theme::BAR_BG).inner_margin(Margin { left: 8, right: 8, top: 6, bottom: 6 }))
            .show(ui, |ui| ui::top::tab_strip(ui, &self.ws, &mut actions));
        egui::Panel::top("toolbar")
            .frame(Frame::new().fill(theme::BG).inner_margin(Margin { left: 8, right: 8, top: 6, bottom: 6 }))
            .show(ui, |ui| {
                ui::top::nav_bar(ui, &mut self.ws, &mut self.state, &mut actions);
                ui.add_space(4.0);
                ui::top::command_bar(ui, &mut self.ws, &mut self.state, can_import_folders, &mut actions);
            });
        egui::Panel::bottom("status")
            .frame(Frame::new().fill(theme::BAR_BG).inner_margin(Margin::symmetric(10, 2)))
            .exact_size(24.0)
            .show(ui, |ui| ui::status::show(ui, &self.ws, self.copy_status(), &mut actions));
        egui::Panel::left("sidebar")
            .frame(Frame::new().fill(theme::BG).inner_margin(Margin::symmetric(8, 4)))
            .resizable(true)
            .default_size(230.0)
            .size_range(150.0..=480.0)
            .show(ui, |ui| ui::sidebar::show(ui, &mut self.ws, &mut self.state, &mut actions));
        if self.state.show_preview && !self.ws.tabs.is_empty() {
            egui::Panel::right("preview")
                .frame(Frame::new().fill(theme::BG).inner_margin(Margin::symmetric(10, 4)))
                .resizable(true)
                .default_size(300.0)
                .size_range(200.0..=700.0)
                .show(ui, |ui| ui::preview::show(ui, &self.state.preview));
        }
        egui::CentralPanel::default()
            .frame(Frame::new().fill(theme::BG).inner_margin(Margin { left: 4, right: 4, top: 2, bottom: 0 }))
            .show(ui, |ui| {
                if self.ws.tabs.is_empty() {
                    self.welcome(ui, &mut actions);
                } else {
                    if self.ws.current_volume().is_some_and(|v| v.driver.id == "recovered") {
                        recovered_banner(ui);
                    }
                    file_list::show(ui, &mut self.ws, &mut self.state, can_import_folders, &mut actions);
                }
            });

        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let screen = ctx.content_rect();
            let painter =
                ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop_overlay")));
            painter.rect_filled(screen, 0.0, egui::Color32::from_black_alpha(170));
            let msg = match self.ws.current_volume() {
                Some(v) if v.caps().contains(Capabilities::WRITE) => {
                    format!("Drop to copy into {}", self.ws.tab().map(|t| t.title(&v.name)).unwrap_or_default())
                }
                _ => "Drop to open as an image".to_owned(),
            };
            painter.text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                msg,
                egui::FontId::proportional(20.0),
                egui::Color32::WHITE,
            );
        }

        ui::hex::show(&ctx, &self.ws, &mut self.state.hex);
        if self.state.licenses {
            ui::licenses::show(&ctx, &mut self.state.licenses);
        }
        ui::dialogs::show(&ctx, &self.ws, &mut self.state, &mut actions);
        if self.state.dialog.is_none() {
            ui::dialogs::mounting(&ctx, &self.ws);
        }
        self.toasts(&ctx);

        for a in actions {
            self.apply(a);
        }
    }
}

/// The app icon as a texture (None if it cannot be decoded).
fn logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(&include_bytes!("../assets/icon-256.png")[..]));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let size = [info.width as usize, info.height as usize];
    let pixels = &buf[..info.buffer_size()];
    let image = match info.color_type {
        png::ColorType::Rgba => egui::ColorImage::from_rgba_unmultiplied(size, pixels),
        png::ColorType::Rgb => egui::ColorImage::from_rgb(size, pixels),
        _ => return None,
    };
    Some(ctx.load_texture("app-logo", image, egui::TextureOptions::LINEAR))
}

/// The note over a volume of deleted files.
fn recovered_banner(ui: &mut egui::Ui) {
    egui::Frame::new().fill(theme::WARN.gamma_multiply(0.15)).inner_margin(Margin::symmetric(10, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(
            RichText::new(
                "Recovered files — read-only. Export what you want to keep to another disk: writing to the \
                 one they came from could overwrite the rest.",
            )
            .color(theme::WARN),
        );
    });
}
