//! The mounted file system interface consumed by the UI.

use crate::caps::{AccessMode, Capabilities};
use crate::error::{Error, Result};
use crate::path::VfsPath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    /// Device nodes, FIFOs, sockets, …
    Other,
}

impl EntryKind {
    pub fn is_dir(self) -> bool {
        self == EntryKind::Directory
    }
}

/// Metadata of a file system entry. Every field a driver cannot provide is
/// `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub kind: EntryKind,
    pub size: u64,
    /// Unix seconds (UTC).
    pub modified: Option<i64>,
    pub created: Option<i64>,
    pub accessed: Option<i64>,
    /// Unix permission bits (including the file type bits if known).
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Inode / cluster / record number – whatever identifies the entry.
    pub inode: Option<u64>,
    pub symlink_target: Option<String>,
    /// Hidden attribute (FAT/NTFS). Dot files are treated as hidden by the UI anyway.
    pub hidden: bool,
    /// A short remark the UI shows with the entry (a recovered file's
    /// chance: "deleted · intact").
    pub note: Option<String>,
}

impl Metadata {
    pub fn new(kind: EntryKind, size: u64) -> Self {
        Self {
            kind,
            size,
            modified: None,
            created: None,
            accessed: None,
            mode: None,
            uid: None,
            gid: None,
            inode: None,
            symlink_target: None,
            hidden: false,
            note: None,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub metadata: Metadata,
}

/// Volume level information.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsInfo {
    /// Identifier of the driver that mounted the volume (`"simplefs"`, `"ext4"`, …).
    pub driver_id: String,
    /// Human readable file system name (`"ext4"`, `"FAT32"`, …).
    pub fs_name: String,
    pub label: Option<String>,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub block_size: Option<u32>,
    /// More about the volume, as (name, value) rows for its properties
    /// ("Extensions" → "Rock Ridge, Joliet"); the driver chooses.
    pub details: Vec<(String, String)>,
}

/// A setting a driver offers for a volume (how an ISO image boots, its
/// label, …), shown and changed in the volume's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    /// What [`FileSystem::apply_settings`] gets back.
    pub key: String,
    pub label: String,
    /// A sentence or two shown with it.
    pub help: String,
    pub kind: SettingKind,
    /// The current value: text, a number, "true"/"false", or a path in the
    /// volume (empty: none).
    pub value: String,
    /// Settings with the same group are shown together under it.
    pub group: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Text,
    Number,
    Flag,
    /// A file in the volume ("/isolinux/isolinux.bin"), or empty.
    File,
    /// A password: entered hidden; its value is never shown back (empty
    /// means "leave as it is").
    Secret,
}

/// A mounted volume. Paths are always absolute [`VfsPath`]s.
///
/// Only the read side is mandatory; every mutating method has a default
/// implementation returning [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported),
/// so a read-only driver implements just five methods. Callers check
/// [`capabilities`](Self::capabilities) before offering an operation.
///
/// Implementations must reject mutations with
/// [`ErrorKind::ReadOnly`](crate::ErrorKind::ReadOnly) when mounted with
/// [`AccessMode::ReadOnly`].
pub trait FileSystem: Send {
    fn info(&self) -> FsInfo;

    /// Effective capabilities (already reduced for read-only mounts).
    fn capabilities(&self) -> Capabilities;

    fn access_mode(&self) -> AccessMode;

    /// Lists a directory (without `.` and `..`).
    fn read_dir(&mut self, path: &VfsPath) -> Result<Vec<DirEntry>>;

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata>;

    /// Reads up to `buf.len()` bytes of a file starting at `offset`.
    /// Returns the number of bytes read (0 at end of file).
    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// Creates the file or replaces its content.
    fn write_file(&mut self, _path: &VfsPath, _data: &[u8]) -> Result<()> {
        Err(Error::unsupported("writing files"))
    }

    /// Makes `path` an empty file (replacing one that exists), to be
    /// filled in pieces with [`write_at`](Self::write_at).
    fn create_file(&mut self, path: &VfsPath) -> Result<()> {
        self.write_file(path, &[])
    }

    /// Writes `data` at `offset` of an existing file, extending it as
    /// needed. Large files are copied in pieces with this (create_file,
    /// then write_at at increasing offsets), so drivers should write in
    /// place; the default rewrites the whole file each time.
    fn write_at(&mut self, path: &VfsPath, offset: u64, data: &[u8]) -> Result<()> {
        let mut all = self.read_to_vec(path)?;
        let end = offset as usize + data.len();
        if all.len() < end {
            all.resize(end, 0);
        }
        all[offset as usize..end].copy_from_slice(data);
        self.write_file(path, &all)
    }

    /// Lets go of memory a long read or write of `path` left behind (cached
    /// pages of the file), once it has been written back. Called while
    /// large files are copied in pieces.
    fn release_cache(&mut self, _path: &VfsPath) {}

    fn create_dir(&mut self, _path: &VfsPath) -> Result<()> {
        Err(Error::unsupported("creating directories"))
    }

    fn remove_file(&mut self, _path: &VfsPath) -> Result<()> {
        Err(Error::unsupported("deleting files"))
    }

    /// Removes an *empty* directory.
    fn remove_dir(&mut self, _path: &VfsPath) -> Result<()> {
        Err(Error::unsupported("deleting directories"))
    }

    /// Renames or moves an entry within the volume.
    fn rename(&mut self, _from: &VfsPath, _to: &VfsPath) -> Result<()> {
        Err(Error::unsupported("renaming"))
    }

    /// Flushes all pending changes to the device. The volume stays mounted
    /// (and, for journaling file systems, marked as in use).
    fn sync(&mut self) -> Result<()> {
        Ok(())
    }

    /// Clean unmount: write everything and mark the volume as cleanly
    /// unmounted. Called on a normal eject and on quit. A volume that is just
    /// dropped (crash, forced eject) is *not* unmounted and should be
    /// detected as such by the next mount.
    fn unmount(&mut self) -> Result<()> {
        self.sync()
    }

    /// Settings the driver offers for this volume (none by default).
    fn settings(&self) -> Vec<Setting> {
        Vec::new()
    }

    /// Changes settings: (key, value) pairs as [`settings`](Self::settings)
    /// names them. Checked as a whole; nothing changes if one is wrong. The
    /// change may only be written by the next [`sync`](Self::sync).
    fn apply_settings(&mut self, _values: &[(String, String)]) -> Result<()> {
        Err(Error::unsupported("settings"))
    }

    /// The volume holds encrypted content that a password could open (an
    /// encrypted zip): offer to enter one.
    fn uses_passwords(&self) -> bool {
        false
    }

    /// Something to ask the user for a password with, once: after mounting
    /// a volume with encrypted content, and after a read failed with
    /// [`ErrorKind::PasswordRequired`](crate::ErrorKind::PasswordRequired).
    /// Each request is returned once.
    fn take_password_request(&mut self) -> Option<String> {
        None
    }

    /// Adds a password to try on encrypted content. It is kept only if it
    /// opens some of it (otherwise a `PasswordRequired` error says so);
    /// several can be added (members may have different ones).
    fn add_password(&mut self, _password: &str) -> Result<()> {
        Err(Error::unsupported("passwords"))
    }

    /// Reads a whole file into memory.
    fn read_to_vec(&mut self, path: &VfsPath) -> Result<Vec<u8>> {
        let meta = self.metadata(path)?;
        if meta.is_dir() {
            return Err(Error::is_a_directory(path));
        }
        let mut out = vec![0u8; meta.size as usize];
        let mut pos = 0usize;
        while pos < out.len() {
            let n = self.read(path, pos as u64, &mut out[pos..])?;
            if n == 0 {
                break;
            }
            pos += n;
        }
        out.truncate(pos);
        Ok(out)
    }

    fn exists(&mut self, path: &VfsPath) -> bool {
        self.metadata(path).is_ok()
    }
}
