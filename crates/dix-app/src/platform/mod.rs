//! Access to the *host* (the machine/browser the app runs on): picking image
//! files, importing host files into an image, exporting files out of it.
//!
//! Native builds use synchronous OS dialogs and `std::fs`; the web build has
//! no file system, so files are uploaded through the browser's file picker
//! and downloaded as blobs. Both report back through [`HostEvent`]s so the
//! rest of the app is identical on every platform.

use crate::model::VolumeId;
use dix_core::ops::Bundle;
use dix_core::{AccessMode, VfsPath};
use std::sync::mpsc::{channel, Receiver, Sender};

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(not(target_arch = "wasm32"))]
pub use native::{list_devices, pick_export_target};

/// An image chosen by the user.
pub enum ImageSource {
    /// A host path: image file or raw device (native only).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Path(std::path::PathBuf),
    /// Uploaded / dropped bytes.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    Bytes { name: String, data: Vec<u8> },
}

/// How to open an image once it has been picked.
#[derive(Clone, Default)]
pub struct OpenRequest {
    pub mode: AccessMode,
    pub driver: Option<String>,
    /// For a whole-disk image: the partition to open.
    pub partition: dix_core::partition::PartitionChoice,
}

pub enum HostEvent {
    /// An image was dropped onto the window: let the user confirm how to open it.
    ImageDropped(ImageSource),
    /// Files read into memory (the browser).
    Import {
        volume: VolumeId,
        dir: VfsPath,
        items: Vec<Bundle>,
    },
    /// Host files and folders to copy in (natively: read while copying).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    ImportPaths {
        volume: VolumeId,
        dir: VfsPath,
        paths: Vec<std::path::PathBuf>,
    },
    Info(String),
    Error(String),
}

/// Platform services. Results arrive asynchronously via [`HostIo::poll`].
pub struct HostIo {
    tx: Sender<HostEvent>,
    rx: Receiver<HostEvent>,
    ctx: eframe::egui::Context,
}

impl HostIo {
    pub fn new(ctx: eframe::egui::Context) -> Self {
        let (tx, rx) = channel();
        Self { tx, rx, ctx }
    }

    pub fn poll(&self) -> Vec<HostEvent> {
        self.rx.try_iter().collect()
    }

    fn sender(&self) -> EventSender {
        EventSender { tx: self.tx.clone(), ctx: self.ctx.clone() }
    }

    /// Lets the user pick an image file; it comes back as
    /// [`HostEvent::ImageDropped`], into the open dialog.
    pub fn pick_image(&self) {
        imp::pick_image(self.sender());
    }

    /// Lets the user pick host files to copy into `dir` of `volume`.
    pub fn pick_import_files(&self, volume: VolumeId, dir: VfsPath) {
        imp::pick_import_files(self.sender(), volume, dir);
    }

    /// Whole folders can only be imported natively.
    pub fn can_import_folders(&self) -> bool {
        cfg!(not(target_arch = "wasm32"))
    }

    pub fn pick_import_folder(&self, volume: VolumeId, dir: VfsPath) {
        #[cfg(not(target_arch = "wasm32"))]
        native::pick_import_folder(self.sender(), volume, dir);
        #[cfg(target_arch = "wasm32")]
        let _ = (volume, dir);
    }

    /// Saves entries read from an image to the host (save dialog natively,
    /// browser downloads on the web).
    /// (Natively exports are copy jobs into the host: see
    /// `Workspace::export_to_host`.)
    #[cfg(target_arch = "wasm32")]
    pub fn export(&self, items: Vec<Bundle>) {
        imp::export(self.sender(), items);
    }

    /// Saves raw bytes (e.g. a whole in-memory image).
    pub fn save_bytes(&self, name: &str, bytes: Vec<u8>) {
        imp::save_bytes(self.sender(), name, bytes);
    }

    /// Reads files dropped onto the window.
    pub fn read_dropped(&self, files: Vec<eframe::egui::DroppedFileHandle>, then: DropTarget) {
        imp::read_dropped(self.sender(), files, then);
    }
}

/// What dropped files should be used for.
#[derive(Clone)]
pub enum DropTarget {
    /// Show the open dialog for each file (the user picks read-only/read-write).
    OpenDialog,
    Import {
        volume: VolumeId,
        dir: VfsPath,
    },
}

#[derive(Clone)]
pub(crate) struct EventSender {
    tx: Sender<HostEvent>,
    ctx: eframe::egui::Context,
}

impl EventSender {
    pub fn send(&self, ev: HostEvent) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }

    pub fn error(&self, msg: impl Into<String>) {
        self.send(HostEvent::Error(msg.into()));
    }

    pub fn info(&self, msg: impl Into<String>) {
        self.send(HostEvent::Info(msg.into()));
    }

    /// Delivers files picked/dropped for `target`.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn deliver(&self, target: DropTarget, files: Vec<(String, Vec<u8>)>) {
        match target {
            DropTarget::OpenDialog => {
                for (name, data) in files {
                    self.send(HostEvent::ImageDropped(ImageSource::Bytes { name, data }));
                }
            }
            DropTarget::Import { volume, dir } => {
                let items = files.into_iter().map(|(name, data)| Bundle::File { name, data, modified: None }).collect();
                self.send(HostEvent::Import { volume, dir, items });
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
use native as imp;
#[cfg(target_arch = "wasm32")]
use web as imp;

/// Native "Browse…" for the open dialog.
#[cfg(not(target_arch = "wasm32"))]
pub fn native_browse_image_path() -> Option<std::path::PathBuf> {
    native::browse_image_path()
}

/// Native "Create image file" location picker.
#[cfg(not(target_arch = "wasm32"))]
pub fn native_pick_new_image_path(name: &str) -> Option<std::path::PathBuf> {
    native::pick_new_image_path(name)
}

/// Lets the user choose where a converted disk image is saved.
#[cfg(not(target_arch = "wasm32"))]
pub fn native_pick_save_path(title: &str, name: &str) -> Option<std::path::PathBuf> {
    rfd::FileDialog::new().set_title(title).set_file_name(name).save_file()
}

/// Browser only: asks the browser to confirm before the page is closed or
/// reloaded (used while in-memory images have unsaved changes). No-op natively,
/// where the app's own quit dialog does this.
pub fn set_leave_warning(enabled: bool) {
    #[cfg(target_arch = "wasm32")]
    web::set_leave_warning(enabled);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = enabled;
}
