use dix_core::device::{DevicePlug, SharedBuffer};
use dix_core::{AccessMode, Capabilities, DeviceKind, DriverInfo, FileSystem, FsInfo, LogEntry, Result, VfsPath};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// A mounted file system shared between the UI thread and background jobs
/// (search). Every user takes the lock for one operation at a time.
pub type SharedFs = Arc<Mutex<Box<dyn FileSystem>>>;

/// Locks a shared file system (a panic in another holder does not make the
/// volume unusable).
pub fn lock_fs(fs: &SharedFs) -> MutexGuard<'_, Box<dyn FileSystem>> {
    fs.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VolumeId(pub u32);

/// Where the image of a volume came from.
#[derive(Debug, Clone)]
pub struct VolumeSource {
    pub kind: DeviceKind,
    /// Host path, device path or uploaded file name.
    pub location: String,
    /// For a virtual machine disk (qcow2, VHD, …): its format.
    pub container: Option<String>,
    pub size: u64,
    /// The partition of `location` the volume is in (None: all of it).
    pub partition: Option<u32>,
}

/// Where a volume made from a whole device (`info`) came from.
pub fn volume_source(info: &dix_core::DeviceInfo, size: u64) -> VolumeSource {
    VolumeSource { kind: info.kind, location: info.name.clone(), container: None, size, partition: None }
}

/// A mounted image.
pub struct Volume {
    pub id: VolumeId,
    /// Display name (file name of the image).
    pub name: String,
    pub source: VolumeSource,
    pub driver: DriverInfo,
    fs: SharedFs,
    /// Bytes of in-memory images (browser uploads, new images) so they can be
    /// downloaded / saved after modifications.
    pub buffer: Option<SharedBuffer>,
    /// Modified since it was opened (relevant for in-memory images).
    pub modified: bool,
    /// Pulling this disconnects the device (forced eject).
    pub plug: DevicePlug,
    /// What the driver reported while mounting.
    pub mount_log: Vec<LogEntry>,
    info: FsInfo,
    caps: Capabilities,
    /// The driver offers settings for this volume.
    has_settings: bool,
    /// It holds encrypted content a password could open.
    uses_passwords: bool,
    mode: AccessMode,
    subdir_cache: HashMap<VfsPath, Result<Arc<[String]>>>,
}

impl Volume {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: VolumeId,
        name: String,
        source: VolumeSource,
        driver: DriverInfo,
        fs: Box<dyn FileSystem>,
        buffer: Option<SharedBuffer>,
        plug: DevicePlug,
        mount_log: Vec<LogEntry>,
    ) -> Self {
        let (info, caps, mode) = (fs.info(), fs.capabilities(), fs.access_mode());
        let has_settings = !fs.settings().is_empty();
        let uses_passwords = fs.uses_passwords();
        let fs = Arc::new(Mutex::new(fs));
        Self {
            id,
            name,
            source,
            driver,
            fs,
            buffer,
            modified: false,
            plug,
            mount_log,
            info,
            caps,
            has_settings,
            uses_passwords,
            mode,
            subdir_cache: HashMap::new(),
        }
    }

    /// Locks the file system for an operation.
    pub fn fs(&self) -> MutexGuard<'_, Box<dyn FileSystem>> {
        lock_fs(&self.fs)
    }

    /// A handle for background jobs.
    pub fn shared_fs(&self) -> SharedFs {
        self.fs.clone()
    }

    pub fn caps(&self) -> Capabilities {
        self.caps
    }

    pub fn mode(&self) -> AccessMode {
        self.mode
    }

    pub fn info(&self) -> &FsInfo {
        &self.info
    }

    /// Sorted sub directory names of `path` (cached; used by the folder tree
    /// and the breadcrumb menus), without those marked hidden or system
    /// (FAT, NTFS: `$Extend`, `System Volume Information`); the file list
    /// shows them with "Show hidden". Cloning the result is cheap.
    pub fn subdirs(&mut self, path: &VfsPath) -> &Result<Arc<[String]>> {
        if !self.subdir_cache.contains_key(path) {
            let r = self.fs().read_dir(path).map(|entries| {
                let mut v: Vec<String> =
                    entries.into_iter().filter(|e| e.metadata.is_dir() && !e.metadata.hidden).map(|e| e.name).collect();
                v.sort_by(|a, b| super::tab::natural_cmp(a, b));
                Arc::from(v)
            });
            self.subdir_cache.insert(path.clone(), r);
        }
        &self.subdir_cache[path]
    }

    /// Sub directories of `path` if they were already read (never touches the disk).
    pub fn cached_subdirs(&self, path: &VfsPath) -> Option<&Result<Arc<[String]>>> {
        self.subdir_cache.get(path)
    }

    /// Call after anything changed on the volume.
    pub fn invalidate(&mut self) {
        self.subdir_cache.clear();
        let (info, has_settings) = {
            let fs = self.fs();
            (fs.info(), !fs.settings().is_empty())
        };
        self.info = info;
        self.has_settings = has_settings;
    }

    pub fn has_settings(&self) -> bool {
        self.has_settings
    }

    pub fn uses_passwords(&self) -> bool {
        self.uses_passwords
    }

    /// The file system, unless a background job holds it right now.
    pub fn try_fs(&self) -> Option<MutexGuard<'_, Box<dyn FileSystem>>> {
        match self.fs.try_lock() {
            Ok(g) => Some(g),
            Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    pub fn mark_modified(&mut self) {
        self.modified = true;
        self.invalidate();
    }
}
