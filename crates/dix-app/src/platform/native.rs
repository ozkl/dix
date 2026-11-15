//! Native host access: OS file dialogs (rfd) and `std::fs`.

use super::{DropTarget, EventSender, HostEvent, ImageSource};
use crate::model::VolumeId;
use dix_core::VfsPath;
use std::path::PathBuf;

pub use dix_core::device::host::list_devices;

// No file type filters: images are recognised by their content, and come
// with any name (disk.img, rootfs.ext4, initrd, backup.zip, …).

/// The picked file goes into the open dialog, as a dropped one does.
pub fn pick_image(tx: EventSender) {
    let picked = rfd::FileDialog::new().set_title("Open file system image").pick_file();
    if let Some(path) = picked {
        tx.send(HostEvent::ImageDropped(ImageSource::Path(path)));
    }
}

/// Lets the user choose where a new image file is created.
pub fn pick_new_image_path(default_name: &str) -> Option<PathBuf> {
    rfd::FileDialog::new().set_title("Create image file").set_file_name(default_name).save_file()
}

/// Shows a file picker for an image path (used by the open dialog's "Browse…").
pub fn browse_image_path() -> Option<PathBuf> {
    rfd::FileDialog::new().set_title("Open file system image").pick_file()
}

pub fn pick_import_files(tx: EventSender, volume: VolumeId, dir: VfsPath) {
    let Some(paths) = rfd::FileDialog::new().set_title("Import files into the image").pick_files() else { return };
    import_paths(&tx, volume, dir, &paths);
}

pub fn pick_import_folder(tx: EventSender, volume: VolumeId, dir: VfsPath) {
    let Some(path) = rfd::FileDialog::new().set_title("Import a folder into the image").pick_folder() else { return };
    import_paths(&tx, volume, dir, &[path]);
}

fn import_paths(tx: &EventSender, volume: VolumeId, dir: VfsPath, paths: &[PathBuf]) {
    // Nothing is read here: the copy job reads the files as it writes them.
    tx.send(HostEvent::ImportPaths { volume, dir, paths: paths.to_vec() });
}

/// Asks where to export: a "Save as" for one file (`single`: its name), a
/// folder otherwise.
pub fn pick_export_target(single: Option<&str>) -> Option<crate::model::ExportTarget> {
    match single {
        Some(name) => rfd::FileDialog::new()
            .set_title("Export file")
            .set_file_name(name)
            .save_file()
            .map(crate::model::ExportTarget::File),
        None => {
            rfd::FileDialog::new().set_title("Export to folder").pick_folder().map(crate::model::ExportTarget::Folder)
        }
    }
}

pub fn save_bytes(tx: EventSender, name: &str, bytes: Vec<u8>) {
    let Some(target) = rfd::FileDialog::new().set_title("Save image").set_file_name(name).save_file() else { return };
    let new = !target.exists();
    let written = std::fs::write(&target, bytes);
    if new && written.is_ok() {
        dix_core::device::host::give_to_sudo_user(&target);
    }
    match written {
        Ok(()) => tx.info(format!("Saved {}", target.display())),
        Err(e) => tx.error(format!("Save failed: {e}")),
    }
}

pub fn read_dropped(tx: EventSender, files: Vec<eframe::egui::DroppedFileHandle>, target: DropTarget) {
    match &target {
        // Natively we know the paths: open them in place (no copy into memory).
        DropTarget::OpenDialog => {
            for f in files {
                tx.send(HostEvent::ImageDropped(ImageSource::Path(f.path().to_owned())));
            }
        }
        DropTarget::Import { volume, dir } => {
            let paths: Vec<PathBuf> = files.iter().map(|f| f.path().to_owned()).collect();
            import_paths(&tx, *volume, dir.clone(), &paths);
        }
    }
}
