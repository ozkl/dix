//! egui presentation layer.
//!
//! Drawing functions read the [`Workspace`](crate::model::Workspace), may
//! change purely visual state (selection, scrolling, dialogs) and emit
//! [`Action`]s. The [`app`](crate::app) executes the actions after the frame
//! is drawn, so all file system work happens in one place.

pub mod convert;
#[cfg(not(target_arch = "wasm32"))]
pub mod copy;
pub mod dialogs;
pub mod disk;
pub mod file_list;
pub mod hex;
pub mod icons;
pub mod licenses;
pub mod preview;
pub mod sidebar;
pub mod status;
pub mod theme;
pub mod top;

use crate::model::VolumeId;
use dix_core::VfsPath;
use std::collections::HashSet;

/// Something the user asked for.
#[derive(Debug, Clone)]
pub enum Action {
    // tabs & navigation
    SelectTab(usize),
    CloseTab(usize),
    NewTabForVolume(VolumeId),
    Navigate(VfsPath),
    Back,
    Forward,
    Up,
    Refresh,
    /// Double click / Enter on an entry of the current directory.
    OpenEntry(String),
    /// Open a file in the volume as an image of its own (a SquashFS on a
    /// live CD, a disk image inside an archive): read into memory.
    OpenEntryAsImage(String),
    /// A file of the current folder in a hex viewer.
    HexFile(String),
    /// A volume's raw bytes (its image or partition) in a hex viewer.
    HexVolume(VolumeId),
    /// Look for a volume's deleted files (opened as a read-only volume).
    RecoverDeleted(VolumeId),
    /// Look for photos and videos by their content on a volume's partition.
    ScanPhotos(VolumeId),
    /// The same, on the image chosen in the open dialog (which may not open).
    ScanPhotosFromDialog,
    /// Open a lost partition found in the Partitions dialog (start, size),
    /// read-only.
    OpenLostPartition(u64, u64),
    /// The convert dialog for a volume's disk.
    ShowConvert(VolumeId),
    /// Start the conversion the convert dialog describes.
    StartConvert,
    /// *Copy disk…*: save a disk as an image, or write an image to a disk
    /// (native builds).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    ShowCopyDisk,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    StartSaveDisk,
    /// Checks before writing an image onto a disk (then asks to confirm).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    CheckWriteImage,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    StartWriteImage {
        unmount: bool,
    },
    /// Keep a running copy going without its dialog.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    HideCopy,
    ShowHiddenCopy,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    OpenCopiedImage(std::path::PathBuf),
    /// A message in a toast.
    Info(String),
    // file operations on the current tab
    NewFolder,
    BeginRename,
    CommitRename {
        old: String,
        new: String,
    },
    RequestDelete,
    ConfirmDelete,
    Cut,
    Copy,
    Paste,
    ImportFiles,
    ImportFolder,
    Export,
    Properties,
    /// Open a search result (folder: go there; file: reveal + preview).
    OpenSearchHit(usize),
    /// Go to the folder containing a search result and select it.
    RevealSearchHit(usize),
    SelectAll,
    /// Keyboard navigation: move by rows, optionally extending the selection.
    MoveCursor(isize, bool),
    // volumes
    ShowOpenDialog,
    ShowNewImageDialog,
    ShowVolumeInfo(VolumeId),
    SaveImage(VolumeId),
    /// Eject = clean unmount (asks first if it is busy or has unsaved changes).
    Eject(VolumeId),
    /// Force eject = unclean unmount: disconnect the device immediately.
    ForceEject(VolumeId),
    /// Quit even though something is busy or unsaved (writes in progress are cut off).
    QuitAnyway,
    /// "Force Eject…": show the unclean-unmount warning first.
    WarnForceEject(VolumeId),
    /// "Quit Anyway" while writes run: show the data-loss warning first.
    WarnForceQuit,
    /// Stop a background write job (cleanly, between two steps).
    CancelJob(u64),
    // dialogs
    OpenFromDialog,
    /// Check (fsck -n) the image chosen in the open dialog, without opening it.
    CheckFromDialog,
    /// Check an open image's file system.
    CheckVolume(VolumeId),
    /// Safely repair a checked image (like fsck -p).
    Repair(crate::model::RepairTarget),
    CreateFromDialog,
    /// Edit the partitions of this disk or image (from the open dialog).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    ShowDiskDialog(String),
    /// The partitions of the image the open dialog has in memory.
    ShowDiskDialogForUpload,
    /// The open dialog for an image in memory (name, bytes), a partition
    /// preselected.
    ShowOpenDialogFor(String, Vec<u8>, Option<u32>),
    /// Back to the open dialog for this path, with this partition chosen.
    ShowOpenDialogAt(String, Option<u32>),
    /// Write the changes made in the partition editor.
    ApplyDisk,
    /// Unmount the disk's volumes (the system uses them), then apply.
    UnmountAndApplyDisk,
    /// Go where the link at this path (in the current tab's volume) leads:
    /// into a folder, or to a file's folder with the file selected.
    GoToLinkTarget(VfsPath),
    /// Show a volume's settings (boot options of an ISO image, …).
    ShowSettings(VolumeId),
    /// Ask for a password for a volume's encrypted files.
    ShowPassword(VolumeId),
    /// Try this password on them.
    SubmitPassword(VolumeId, String),
    /// Change them: (key, value) pairs.
    ApplySettings(VolumeId, Vec<(String, String)>),
    /// Open a partition shown in the partition editor.
    OpenPartitionFromDisk(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Details,
    Icons,
}

pub struct RenameState {
    pub original: String,
    pub buffer: String,
    pub focus_requested: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Info,
    Error,
}

pub struct Toast {
    pub level: ToastLevel,
    pub text: String,
    pub until: f64,
}

/// Visual state that is not part of the model.
pub struct UiState {
    pub view: ViewMode,
    pub show_preview: bool,
    /// Widths of the Date, Type and Size columns.
    pub columns: [f32; 3],
    pub renaming: Option<RenameState>,
    pub address_edit: Option<String>,
    pub dialog: Option<dialogs::Dialog>,
    /// Open hex viewer windows.
    pub hex: Vec<hex::HexView>,
    /// The Licenses window is open.
    pub licenses: bool,
    pub toasts: Vec<Toast>,
    pub expanded: HashSet<(VolumeId, VfsPath)>,
    pub preview: preview::PreviewCache,
    /// Scroll the file list so this view index is visible.
    pub scroll_to: Option<usize>,
    /// Scroll offset and viewport height of the file list (previous frame).
    pub list_scroll: (f32, f32),
    pub now: f64,
    /// When the search text last changed (for debouncing).
    pub search_changed_at: f64,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            view: ViewMode::Details,
            show_preview: false,
            columns: [170.0, 130.0, 90.0],
            renaming: None,
            address_edit: None,
            dialog: None,
            hex: Vec::new(),
            licenses: false,
            toasts: Vec::new(),
            expanded: HashSet::new(),
            preview: Default::default(),
            scroll_to: None,
            list_scroll: (0.0, 0.0),
            now: 0.0,
            search_changed_at: 0.0,
        }
    }
}

impl UiState {
    pub fn info(&mut self, text: impl Into<String>) {
        self.toast(ToastLevel::Info, text.into(), 4.0);
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.toast(ToastLevel::Error, text.into(), 8.0);
    }

    fn toast(&mut self, level: ToastLevel, text: String, secs: f64) {
        self.toasts.push(Toast { level, text, until: self.now + secs });
        if self.toasts.len() > 5 {
            self.toasts.remove(0);
        }
    }
}

/// `21.4 KB` style sizes.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["bytes", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[unit])
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

pub fn format_time(ts: Option<i64>) -> String {
    ts.map(dix_core::time::format_unix).unwrap_or_default()
}

/// `rwxr-xr-x` from unix mode bits.
pub fn format_mode(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(12), "12 bytes");
        assert_eq!(format_size(21_914), "21.4 KB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GB");
        assert_eq!(format_mode(0o755), "rwxr-xr-x");
    }
}
