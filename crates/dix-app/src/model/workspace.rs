use super::jobs::{bundle_ops, JobEnd, JobHandle, Op, Side, WriteJob};
use super::mount::MountTask;
use super::tab::Tab;
use super::volume::{Volume, VolumeId, VolumeSource};
use dix_core::device::{DetachableDevice, MemoryDevice, SharedBuffer};
use dix_core::ops::{self, Bundle};
use dix_core::partition::{Partition, PartitionChoice};
use dix_core::{
    AccessMode, BlockDevice, Capabilities, DriverRegistry, Error, ErrorKind, FormatOptions, LogEntry, MountLog,
    MountOptions, Mounted, Result, VfsPath,
};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Entries copied or cut to the internal clipboard.
#[derive(Debug, Clone)]
pub struct Clipboard {
    pub volume: VolumeId,
    pub paths: Vec<VfsPath>,
    pub cut: bool,
}

/// Everything that is open: volumes (mounted images) and tabs (views into them).
pub struct Workspace {
    pub registry: DriverRegistry,
    pub volumes: BTreeMap<VolumeId, Volume>,
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub clipboard: Option<Clipboard>,
    /// Background write operations.
    pub jobs: Vec<RunningJob>,
    /// Mounts in progress.
    pub mounting: Vec<MountTask>,
    /// File system checks in progress.
    pub checking: Vec<super::CheckTask>,
    /// Looking for deleted files, in the background.
    recovering: Vec<Recovering>,
    next_id: u32,
    next_job: u64,
    /// Called from background jobs when they have news (wakes the UI).
    notify: Arc<dyn Fn() + Send + Sync>,
    /// Host images in use (canonical path, access): the partitions of one
    /// disk image share one open file (and its lock).
    #[cfg(not(target_arch = "wasm32"))]
    host_images: Vec<(std::path::PathBuf, AccessMode, dix_core::device::WeakDevice)>,
}

/// A write job and what to do when it ends.
pub struct RunningJob {
    pub id: u64,
    /// "Copying 3 items to /docs"
    pub title: String,
    /// Volumes the job writes to (busy while it runs).
    pub writes: Vec<VolumeId>,
    pub handle: JobHandle,
    /// Select these names in the destination folder afterwards.
    select: Option<(VolumeId, VfsPath, Vec<String>)>,
    clear_clipboard: bool,
    /// A volume it writes to was force-ejected.
    pub interrupted: bool,
    last_refresh: f64,
}

/// How a job ended (for the UI to report).
pub struct JobOutcome {
    pub title: String,
    pub end: JobEnd,
    pub interrupted: bool,
}

/// Where an export goes (chosen in the host's save / folder dialog).
#[cfg(not(target_arch = "wasm32"))]
pub enum ExportTarget {
    /// One file, saved as this path.
    File(std::path::PathBuf),
    /// Into this folder.
    Folder(std::path::PathBuf),
}

/// How far a scan for photos has come, and how to stop it.
#[derive(Clone)]
pub struct ScanProgress {
    pub done: Arc<std::sync::atomic::AtomicU64>,
    pub total: u64,
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
}

/// A search for deleted files under way.
struct Recovering {
    name: String,
    source: VolumeSource,
    plug: dix_core::device::DevicePlug,
    rx: std::sync::mpsc::Receiver<Result<Box<dyn dix_core::FileSystem>>>,
}

/// How a background mount ended.
pub struct MountOutcome {
    pub name: String,
    pub result: Result<VolumeId>,
    pub log: Vec<LogEntry>,
}

/// What `path` opens, for comparing: the resolved path, of the first part
/// when it is one part of a split file (any part opens them all).
#[cfg(not(target_arch = "wasm32"))]
fn same_file(path: &std::path::Path) -> std::path::PathBuf {
    let first = dix_core::device::host::split_parts(path).and_then(|p| p.into_iter().next());
    let path = first.as_deref().unwrap_or(path);
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

impl Workspace {
    pub fn new(registry: DriverRegistry) -> Self {
        Self {
            registry,
            volumes: BTreeMap::new(),
            tabs: Vec::new(),
            active: 0,
            clipboard: None,
            jobs: Vec::new(),
            mounting: Vec::new(),
            checking: Vec::new(),
            recovering: Vec::new(),
            next_id: 1,
            next_job: 1,
            notify: Arc::new(|| {}),
            #[cfg(not(target_arch = "wasm32"))]
            host_images: Vec::new(),
        }
    }

    // ----------------------------------------------------------------- volumes

    /// Mounts a device right away and opens a tab for it. (The app uses
    /// [`begin_mount`](Self::begin_mount) to mount in the background.)
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn mount(
        &mut self,
        dev: Box<dyn BlockDevice>,
        name: String,
        driver: Option<&str>,
        mode: AccessMode,
        buffer: Option<SharedBuffer>,
    ) -> Result<VolumeId> {
        self.mount_partition(dev, None, name, driver, mode, buffer)
    }

    /// Mounts partition `part` of a disk (tests).
    #[cfg(all(test, unix, feature = "ext4", feature = "fat"))]
    pub fn mount_partition_for_test(&mut self, dev: Box<dyn BlockDevice>, part: Partition, name: &str) -> VolumeId {
        self.mount_partition(dev, Some(part), name.into(), None, AccessMode::ReadWrite, None).unwrap()
    }

    /// Mounts partition `part` of `dev` (or all of it) right away.
    fn mount_partition(
        &mut self,
        dev: Box<dyn BlockDevice>,
        part: Option<Partition>,
        name: String,
        driver: Option<&str>,
        mode: AccessMode,
        buffer: Option<SharedBuffer>,
    ) -> Result<VolumeId> {
        let info = dev.info();
        let container = dev.container();
        let dev = match &part {
            Some(p) => dix_core::partition::slice(dev, p)?,
            None => dev,
        };
        let (dev, plug) = DetachableDevice::new(dev);
        let log = MountLog::new();
        let mounted = self.registry.mount_with(Box::new(dev), driver, MountOptions { mode, log: log.clone() })?;
        Ok(self.add_volume(mounted, name, &info, container, plug, buffer, log.entries(), part))
    }

    #[allow(clippy::too_many_arguments)]
    fn add_volume(
        &mut self,
        mounted: Mounted,
        name: String,
        info: &dix_core::DeviceInfo,
        container: Option<String>,
        plug: dix_core::device::DevicePlug,
        buffer: Option<SharedBuffer>,
        log: Vec<LogEntry>,
        partition: Option<Partition>,
    ) -> VolumeId {
        let source = VolumeSource {
            kind: info.kind,
            location: info.name.clone(),
            container,
            size: partition.as_ref().map_or(info.size, |p| p.size),
            partition: partition.as_ref().map(|p| p.number),
        };
        let name = match &partition {
            Some(p) => dix_core::partition::volume_name(&name, p),
            None => name,
        };
        let id = VolumeId(self.next_id);
        self.next_id += 1;
        let volume = Volume::new(id, name, source, mounted.driver.info(), mounted.fs, buffer, plug, log);
        self.volumes.insert(id, volume);
        self.open_tab(id, VfsPath::root());
        id
    }

    /// Opens an image file or device of the host for a volume. If another
    /// volume uses the same image (another partition of it), its open file
    /// is shared: a second open would be refused by the image's lock.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_host(&mut self, path: &std::path::Path, mode: AccessMode) -> Result<Box<dyn BlockDevice>> {
        self.host_images.retain(|(_, _, w)| w.upgrade().is_some());
        let canon = same_file(path);
        if let Some((_, open_mode, weak)) = self.host_images.iter().find(|(p, _, _)| *p == canon) {
            if let Some(dev) = weak.upgrade() {
                if mode.is_writable() && !open_mode.is_writable() {
                    return Err(Error::new(
                        ErrorKind::InUse,
                        "another partition of this image is open read-only: eject it to open this one read-write",
                    ));
                }
                return Ok(Box::new(dev));
            }
        }
        let dev = dix_core::device::SharedDevice::new(dix_core::device::host::open_disk(&self.registry, path, mode)?);
        self.host_images.push((canon, mode, dev.downgrade()));
        Ok(Box::new(dev))
    }

    /// The partition table of a host image (through its open file if a
    /// volume uses it), for the open dialog.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn scan_host(&self, path: &std::path::Path) -> Result<Option<dix_core::partition::PartitionScan>> {
        dix_core::partition::scan(&self.registry, self.host_reader(path)?.as_mut())
    }

    /// A host image or disk to read: through its open file if a volume uses
    /// it (its lock would refuse a second open), else opened read-only.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn host_reader(&self, path: &std::path::Path) -> Result<Box<dyn BlockDevice>> {
        let canon = same_file(path);
        let shared = self.host_images.iter().find(|(p, _, _)| *p == canon).and_then(|(_, _, w)| w.upgrade());
        match shared {
            Some(dev) => Ok(Box::new(dev)),
            None => dix_core::device::host::open_disk(&self.registry, path, AccessMode::ReadOnly),
        }
    }

    /// The open (or opening) volume backed by host path `path` that is (or
    /// overlaps) the partition `partition` of it, if any. Paths are compared
    /// after resolving symlinks and `..`; `Auto` matches any volume of the
    /// path, and a whole-image volume overlaps every partition.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn find_open_path(
        &self,
        path: &std::path::Path,
        partition: PartitionChoice,
    ) -> Option<Result<VolumeId, String>> {
        let canon = same_file;
        let wanted = canon(path);
        let same = |location: &str| canon(std::path::Path::new(location)) == wanted;
        let part = |p: Option<u32>| match partition {
            PartitionChoice::Auto | PartitionChoice::Whole => true,
            PartitionChoice::Number(n) => p.is_none() || p == Some(n),
        };
        if let Some(v) =
            self.volumes.values().find(|v| v.buffer.is_none() && same(&v.source.location) && part(v.source.partition))
        {
            return Some(Ok(v.id));
        }
        let asked = |c: PartitionChoice| match (partition, c) {
            (PartitionChoice::Number(a), PartitionChoice::Number(b)) => a == b,
            _ => true,
        };
        self.mounting
            .iter()
            .find(|m| m.buffer.is_none() && same(&m.info.name) && asked(m.partition))
            .map(|m| Err(m.name.clone()))
    }

    /// Starts mounting in the background; see [`poll_mounts`](Self::poll_mounts).
    pub fn begin_mount(
        &mut self,
        dev: Box<dyn BlockDevice>,
        name: String,
        driver: Option<String>,
        mode: AccessMode,
        buffer: Option<SharedBuffer>,
        partition: PartitionChoice,
    ) {
        let task =
            MountTask::start(self.registry.clone(), dev, name, driver, mode, buffer, partition, self.notify.clone());
        self.mounting.push(task);
    }

    /// `dev`, or the partition of it that `choice` means.
    pub fn in_partition(&self, dev: Box<dyn BlockDevice>, choice: PartitionChoice) -> Result<Box<dyn BlockDevice>> {
        in_partition(&self.registry, dev, choice)
    }

    /// Starts checking (or, with `repair`, safely repairing) `dev`.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_check(
        &mut self,
        dev: Box<dyn BlockDevice>,
        name: String,
        driver: Option<String>,
        repair: bool,
        reopen: Option<super::Reopen>,
        target: Option<super::RepairTarget>,
    ) {
        let task = super::CheckTask::start(
            self.registry.clone(),
            dev,
            name,
            driver,
            repair,
            reopen,
            target,
            self.notify.clone(),
        );
        self.checking.push(task);
    }

    /// Checks an open volume. An in-memory image is checked from a snapshot
    /// and stays open; an image on the host is ejected cleanly first (it is
    /// locked while mounted) and opened again afterwards.
    pub fn check_volume(&mut self, id: VolumeId) -> Result<()> {
        let v = self.volumes.get(&id).ok_or_else(|| Error::not_found("volume"))?;
        if !self.activities(id).is_empty() {
            return Err(Error::new(ErrorKind::InUse, "the image is busy: wait for the running operation to finish"));
        }
        let (name, driver) = (v.name.clone(), v.driver.id.to_owned());
        let partition = choice(v.source.partition);
        if let Some(buffer) = v.buffer.clone() {
            v.fs().sync()?;
            let dev = MemoryDevice::new(name.clone(), buffer.snapshot());
            let dev = self.registry.open_disk(Box::new(dev), None)?;
            let dev = in_partition(&self.registry, dev, partition)?;
            let target = Some(super::RepairTarget::Volume(id));
            self.begin_check(dev, name, Some(driver), false, None, target);
            return Ok(());
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = std::path::PathBuf::from(&v.source.location);
            let mode = v.mode();
            self.close_volume(id)?;
            let dev = self.open_host(&path, AccessMode::ReadOnly)?;
            let dev = in_partition(&self.registry, dev, partition)?;
            let reopen = super::Reopen::Path { path: path.clone(), partition, mode, driver: driver.clone() };
            let target = super::RepairTarget::Path { path, partition };
            self.begin_check(dev, name, Some(driver), false, Some(reopen), Some(target));
            Ok(())
        }
        #[cfg(target_arch = "wasm32")]
        Err(Error::unsupported("checking this image"))
    }

    /// Safely repairs a checked image (see `CheckOptions::repair`). It is
    /// ejected cleanly if open, and opened again afterwards.
    pub fn repair(&mut self, target: &super::RepairTarget) -> Result<()> {
        match target {
            super::RepairTarget::Volume(id) => {
                let v = self.volumes.get(id).ok_or_else(|| Error::not_found("the image is no longer open"))?;
                let buffer = v.buffer.clone().ok_or_else(|| Error::invalid_input("not an in-memory image"))?;
                let (name, mode, driver) = (v.name.clone(), v.mode(), v.driver.id.to_owned());
                let partition = choice(v.source.partition);
                // The image's own name (the volume's names its partition).
                let image = v.source.location.clone();
                if !self.activities(*id).is_empty() {
                    return Err(Error::new(ErrorKind::InUse, "the image is busy"));
                }
                self.close_volume(*id)?;
                let dev = MemoryDevice::from_shared(image.clone(), buffer.clone());
                let dev = self.registry.open_disk(Box::new(dev), None)?;
                let dev = in_partition(&self.registry, dev, partition)?;
                // Repaired bytes exist only in memory: unsaved changes.
                let reopen = super::Reopen::Memory {
                    name: image,
                    buffer,
                    partition,
                    mode,
                    driver: driver.clone(),
                    modified: true,
                };
                self.begin_check(dev, name, Some(driver), true, Some(reopen), None);
                Ok(())
            }
            #[cfg(not(target_arch = "wasm32"))]
            super::RepairTarget::Path { path, partition } => {
                let partition = *partition;
                let mut reopen = None;
                let mut driver = None;
                // The repair needs the file system to itself: eject its volume
                // (other partitions of the image may stay open).
                if let Some(Ok(id)) = self.find_open_path(path, partition) {
                    if !self.activities(id).is_empty() {
                        return Err(Error::new(ErrorKind::InUse, "the image is busy"));
                    }
                    let v = &self.volumes[&id];
                    let d = v.driver.id.to_owned();
                    let (path, mode, partition) = (path.clone(), v.mode(), choice(v.source.partition));
                    reopen = Some(super::Reopen::Path { path, partition, mode, driver: d.clone() });
                    driver = Some(d);
                    self.close_volume(id)?;
                }
                let dev = self.open_host(path, AccessMode::ReadWrite)?;
                let dev = in_partition(&self.registry, dev, partition)?;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                self.begin_check(dev, name, driver, true, reopen, None);
                Ok(())
            }
            #[cfg(target_arch = "wasm32")]
            super::RepairTarget::Path { .. } => Err(Error::unsupported("repairing host files in the browser")),
        }
    }

    /// Finished checks and repairs.
    pub fn poll_checks(&mut self) -> Vec<super::CheckOutcome> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.checking.len() {
            match self.checking[i].take_result() {
                Some(result) => {
                    let t = self.checking.remove(i);
                    out.push(super::CheckOutcome {
                        name: t.name,
                        result,
                        log: t.log.entries(),
                        reopen: t.reopen,
                        target: t.target,
                        repaired: t.repair,
                    });
                }
                None => i += 1,
            }
        }
        out
    }

    /// Mounts an in-memory image again from its buffer (after a repair).
    pub fn remount_memory(
        &mut self,
        name: String,
        buffer: SharedBuffer,
        partition: PartitionChoice,
        mode: AccessMode,
        driver: String,
        modified: bool,
    ) {
        let dev = MemoryDevice::from_shared(name.clone(), buffer.clone());
        // A VM disk again as its disk (it opened before: it opens now).
        let dev = match self.registry.open_disk(Box::new(dev), None) {
            Ok(d) => d,
            Err(_) => Box::new(MemoryDevice::from_shared(name.clone(), buffer.clone())),
        };
        self.begin_mount(dev, name, Some(driver), mode, Some(buffer), partition);
        if let Some(t) = self.mounting.last_mut() {
            t.keep_modified = modified;
        }
    }

    /// Finished background mounts (successful ones get a volume and a tab).
    pub fn poll_mounts(&mut self) -> Vec<MountOutcome> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.mounting.len() {
            let Some(result) = self.mounting[i].take_result() else {
                i += 1;
                continue;
            };
            let task = self.mounting.remove(i);
            let log = task.log.entries();
            let keep_modified = task.keep_modified;
            let result = result.map(|(m, part)| {
                let id = self.add_volume(
                    m,
                    task.name.clone(),
                    &task.info,
                    task.container.clone(),
                    task.plug,
                    task.buffer,
                    log.clone(),
                    part,
                );
                // A remounted in-memory image still has unsaved changes.
                self.volumes.get_mut(&id).unwrap().modified |= keep_modified;
                id
            });
            out.push(MountOutcome { name: task.name, result, log });
        }
        out
    }

    /// Mounts bytes as an in-memory image right away (tests; the app mounts
    /// uploads in the background).
    #[cfg(test)]
    #[allow(dead_code)] // (used by tests that need every driver)
    pub fn mount_bytes(
        &mut self,
        name: String,
        bytes: Vec<u8>,
        driver: Option<&str>,
        mode: AccessMode,
    ) -> Result<VolumeId> {
        let dev = MemoryDevice::new(name.clone(), bytes);
        let buffer = dev.buffer();
        let dev = self.registry.open_disk(Box::new(dev), None)?;
        self.mount(dev, name, driver, mode, Some(buffer))
    }

    /// Opens an image file or raw device of the host (native only).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn mount_path(&mut self, path: &std::path::Path, driver: Option<&str>, mode: AccessMode) -> Result<VolumeId> {
        let dev = dix_core::device::host::open_disk(&self.registry, path, mode)?;
        let name =
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        self.mount(dev, name, driver, mode, None)
    }

    /// Creates a new, empty in-memory image formatted with `driver` (with
    /// `table`: a disk image with one partition, formatted).
    #[cfg(test)]
    pub fn create_memory_image(
        &mut self,
        name: String,
        size: u64,
        driver: &str,
        opts: FormatOptions,
        table: Option<dix_core::partition::Scheme>,
    ) -> Result<VolumeId> {
        self.create_memory_disk(name, size, driver, opts, table, None)
    }

    /// Like [`create_memory_image`](Self::create_memory_image), the disk
    /// kept in a virtual machine format (`container`: a format's name).
    pub fn create_memory_disk(
        &mut self,
        name: String,
        size: u64,
        driver: &str,
        opts: FormatOptions,
        table: Option<dix_core::partition::Scheme>,
        container: Option<&str>,
    ) -> Result<VolumeId> {
        let (mut dev, buffer): (Box<dyn BlockDevice>, SharedBuffer) = match container {
            None => {
                let size = usize::try_from(size).map_err(|_| Error::invalid_input("image too large"))?;
                let dev = MemoryDevice::zeroed(name.clone(), size);
                let buffer = dev.buffer();
                (Box::new(dev), buffer)
            }
            Some(c) => {
                let mut file = MemoryDevice::new(name.clone(), Vec::new());
                self.container_format(c)?.create(&mut file, size, &name)?;
                let buffer = file.buffer();
                (self.registry.open_disk(Box::new(file), None)?, buffer)
            }
        };
        let part = match table {
            Some(scheme) => Some(super::disk::partitioned_image(&self.registry, dev.as_mut(), scheme, driver, &opts)?),
            None => {
                self.registry.format(dev.as_mut(), driver, &opts)?;
                None
            }
        };
        let id = self.mount_partition(dev, part, name, Some(driver), AccessMode::ReadWrite, Some(buffer))?;
        self.volumes.get_mut(&id).unwrap().modified = true;
        Ok(id)
    }

    /// Creates a new image file on the host formatted with `driver`.
    #[cfg(all(test, not(target_arch = "wasm32")))]
    pub fn create_file_image(
        &mut self,
        path: &std::path::Path,
        size: u64,
        driver: &str,
        opts: FormatOptions,
        table: Option<dix_core::partition::Scheme>,
    ) -> Result<VolumeId> {
        self.create_file_disk(path, size, driver, opts, table, None)
    }

    /// Like [`create_file_image`](Self::create_file_image), the disk kept
    /// in a virtual machine format (`container`: a format's name, e.g.
    /// "qcow2"): the file only grows by what is written.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn create_file_disk(
        &mut self,
        path: &std::path::Path,
        size: u64,
        driver: &str,
        opts: FormatOptions,
        table: Option<dix_core::partition::Scheme>,
        container: Option<&str>,
    ) -> Result<VolumeId> {
        let mut dev: Box<dyn BlockDevice> = match container {
            None => Box::new(dix_core::device::host::FileDevice::create(path, size)?),
            Some(c) => {
                let format = self.container_format(c)?;
                let mut file = dix_core::device::host::FileDevice::create(path, 0)?;
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                format.create(&mut file, size, &name)?;
                file.flush()?;
                drop(file);
                dix_core::device::host::open_disk(&self.registry, path, AccessMode::ReadWrite)?
            }
        };
        let part = match table {
            Some(scheme) => Some(super::disk::partitioned_image(&self.registry, dev.as_mut(), scheme, driver, &opts)?),
            None => {
                self.registry.format(dev.as_mut(), driver, &opts)?;
                None
            }
        };
        dev.flush()?;
        drop(dev);
        let Some(part) = part else { return self.mount_path(path, Some(driver), AccessMode::ReadWrite) };
        let dev = self.open_host(path, AccessMode::ReadWrite)?;
        let name =
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        self.mount_partition(dev, Some(part), name, Some(driver), AccessMode::ReadWrite, None)
    }

    /// The container format called `name` that can make new disks.
    fn container_format(&self, name: &str) -> Result<std::sync::Arc<dyn dix_core::container::DiskFormat>> {
        self.registry
            .formats()
            .iter()
            .find(|f| f.name() == name && f.creates().is_some())
            .cloned()
            .ok_or_else(|| Error::unsupported(format!("creating {name} disks")))
    }

    /// Write operations currently using a volume. Ejecting while there are
    /// any would interrupt a write. (Searches only read; they are simply
    /// stopped on eject.)
    pub fn activities(&self, id: VolumeId) -> Vec<String> {
        self.jobs
            .iter()
            .filter(|j| j.writes.contains(&id) && j.handle.end().is_none())
            .map(|j| {
                let p = j.handle.progress();
                format!("{} ({} items, {} written so far)", j.title, p.items, crate::ui::format_size(p.bytes))
            })
            .collect()
    }

    fn stop_searches(&mut self, id: VolumeId) {
        for t in self.tabs.iter_mut().filter(|t| t.volume == id) {
            t.clear_search();
        }
    }

    /// In-memory image with changes that exist nowhere else.
    pub fn has_unsaved_memory_image(&self, id: VolumeId) -> bool {
        self.volumes.get(&id).is_some_and(|v| v.modified && v.buffer.is_some())
    }

    /// Clean eject: unmount (write everything, mark the volume clean), then
    /// close. If unmounting fails the volume stays open and the error is
    /// returned, so nothing is lost silently. Call only when the volume has
    /// no running write jobs.
    pub fn close_volume(&mut self, id: VolumeId) -> Result<()> {
        self.stop_searches(id);
        if let Some(v) = self.volumes.get(&id) {
            v.fs().unmount()?;
        }
        self.remove_volume(id);
        Ok(())
    }

    /// Forced eject: the device is unplugged immediately, in the middle of
    /// whatever is being written. Nothing more reaches the image, exactly as
    /// if the disk were pulled out; the next mount has to recover.
    pub fn force_close_volume(&mut self, id: VolumeId) {
        self.stop_searches(id);
        if let Some(v) = self.volumes.get(&id) {
            v.plug.pull();
        }
        for j in self.jobs.iter_mut().filter(|j| j.writes.contains(&id)) {
            j.interrupted = true;
        }
        self.remove_volume(id);
    }

    fn remove_volume(&mut self, id: VolumeId) {
        self.volumes.remove(&id);
        self.tabs.retain(|t| t.volume != id);
        if self.clipboard.as_ref().is_some_and(|c| c.volume == id) {
            self.clipboard = None;
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
    }

    /// Reasons not to quit right now, one line each: running writes and
    /// unsaved in-memory images.
    pub fn quit_blockers(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (&id, v) in &self.volumes {
            out.extend(self.activities(id).into_iter().map(|a| format!("{}: {a}", v.name)));
            if self.has_unsaved_memory_image(id) {
                out.push(format!("{}: in-memory image with unsaved changes", v.name));
            }
        }
        out
    }

    /// Volumes with running write jobs.
    pub fn busy_volumes(&self) -> Vec<VolumeId> {
        self.volumes.keys().copied().filter(|&id| !self.activities(id).is_empty()).collect()
    }

    /// Cleanly unmounts every volume (on quit). Returns the failures.
    pub fn unmount_all(&mut self) -> Vec<String> {
        let ids: Vec<VolumeId> = self.volumes.keys().copied().collect();
        let mut errors = Vec::new();
        for id in ids {
            let name = self.volumes[&id].name.clone();
            if let Err(e) = self.close_volume(id) {
                errors.push(format!("{name}: changes could not be written ({e})"));
            }
        }
        errors
    }

    // -------------------------------------------------------------------- tabs

    pub fn open_tab(&mut self, volume: VolumeId, path: VfsPath) {
        let mut tab = Tab::new(volume, path);
        if let Some(v) = self.volumes.get_mut(&volume) {
            tab.refresh(v.fs().as_mut());
        }
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    pub fn close_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.tabs.remove(idx);
            if self.active > idx || self.active >= self.tabs.len() {
                self.active = self.active.saturating_sub(1);
            }
        }
    }

    pub fn tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    /// The active tab together with its volume.
    pub fn current(&mut self) -> Option<(&mut Tab, &mut Volume)> {
        let tab = self.tabs.get_mut(self.active)?;
        let vol = self.volumes.get_mut(&tab.volume)?;
        Some((tab, vol))
    }

    pub fn current_volume(&self) -> Option<&Volume> {
        self.volumes.get(&self.tab()?.volume)
    }

    /// Runs `f` on the active tab with the file system of its volume.
    pub fn with_tab<R>(&mut self, f: impl FnOnce(&mut Tab, &mut dyn dix_core::FileSystem) -> R) -> Option<R> {
        let (tab, vol) = self.current()?;
        Some(f(tab, vol.fs().as_mut()))
    }

    /// Call after an operation changed `volume`: writes pending changes to the
    /// device (drivers may buffer them) and refreshes every tab showing it.
    /// Tabs are refreshed even when writing fails.
    /// Changes a volume's settings (see [`dix_core::FileSystem::settings`])
    /// and writes them.
    pub fn apply_settings(&mut self, volume: VolumeId, values: &[(String, String)]) -> Result<()> {
        if !self.activities(volume).is_empty() {
            return Err(Error::new(ErrorKind::InUse, "the image is busy: wait for the running operation to finish"));
        }
        let v = self.volumes.get(&volume).ok_or_else(|| Error::not_found("volume"))?;
        v.fs().apply_settings(values)?;
        self.volume_changed(volume)
    }

    /// A volume that wants a password asked for (after mounting one with
    /// encrypted content, or after a read failed for lack of one), and what
    /// to say. Volumes busy with a job are looked at later.
    pub fn password_request(&mut self) -> Option<(VolumeId, String)> {
        self.volumes.values().filter(|v| v.uses_passwords()).find_map(|v| {
            let request = v.try_fs()?.take_password_request()?;
            Some((v.id, request))
        })
    }

    /// Gives `volume` a password; its tabs are read again (encrypted
    /// links now have targets).
    pub fn add_password(&mut self, volume: VolumeId, password: &str) -> Result<()> {
        let v = self.volumes.get(&volume).ok_or_else(|| Error::not_found("volume"))?;
        v.fs().add_password(password)?;
        self.refresh_volume(volume);
        Ok(())
    }

    pub fn volume_changed(&mut self, volume: VolumeId) -> Result<()> {
        let Some(v) = self.volumes.get_mut(&volume) else { return Ok(()) };
        let synced = v.fs().sync();
        v.mark_modified();
        for t in self.tabs.iter_mut().filter(|t| t.volume == volume) {
            t.refresh(v.fs().as_mut());
        }
        synced
    }

    pub fn refresh_all(&mut self) {
        for v in self.volumes.values_mut() {
            v.invalidate();
        }
        for t in &mut self.tabs {
            if let Some(v) = self.volumes.get_mut(&t.volume) {
                t.refresh(v.fs().as_mut());
            }
        }
    }

    // ------------------------------------------------------------------ search

    /// Wakes the UI (from a background thread).
    pub fn notifier(&self) -> Arc<dyn Fn() + Send + Sync> {
        self.notify.clone()
    }

    pub fn set_notifier(&mut self, notify: Arc<dyn Fn() + Send + Sync>) {
        self.notify = notify;
    }

    /// Starts (or clears) the search of the active tab from its query and mode.
    pub fn start_search(&mut self) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        tab.search_pending = false;
        tab.result_sel = None;
        let query = tab.search_query.trim().to_owned();
        if query.is_empty() {
            tab.search = None;
            return;
        }
        let Some(vol) = self.volumes.get(&tab.volume) else { return };
        tab.selection.clear();
        // Replacing the old search drops (cancels) it.
        tab.search =
            Some(super::Search::start(vol.shared_fs(), tab.path.clone(), tab.search_mode, query, self.notify.clone()));
    }

    /// Collects results of running searches. Returns true while any is running.
    pub fn poll_searches(&mut self) -> bool {
        let mut running = false;
        for t in &mut self.tabs {
            if let Some(s) = &mut t.search {
                let added = s.poll();
                running |= s.is_running();
                if added {
                    t.rebuild_hits_view();
                }
            }
        }
        running
    }

    // -------------------------------------------------------------- operations

    fn require(&self, volume: VolumeId, cap: Capabilities, what: &str) -> Result<()> {
        let v = self.volumes.get(&volume).ok_or_else(|| Error::not_found("volume"))?;
        if v.caps().contains(cap) {
            Ok(())
        } else if !v.mode().is_writable() && cap.is_writable() {
            Err(Error::new(ErrorKind::ReadOnly, format!("cannot {what}: the image is opened read-only")))
        } else {
            Err(Error::unsupported(format!("{what} ({} driver)", v.driver.name)))
        }
    }

    /// Creates "New folder", "New folder (2)", … in the current directory and
    /// returns its name.
    pub fn new_folder(&mut self) -> Result<String> {
        let tab = self.tab().ok_or_else(|| Error::not_found("tab"))?;
        let vid = tab.volume;
        self.require(vid, Capabilities::CREATE_DIR, "create folders")?;
        let (tab, vol) = self.current().unwrap();
        let name = ops::unique_name(vol.fs().as_mut(), &tab.path, "New folder")?;
        vol.fs().create_dir(&tab.path.join(&name)?)?;
        self.volume_changed(vid)?;
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.select_only(&name);
        }
        Ok(name)
    }

    pub fn rename(&mut self, old: &str, new: &str) -> Result<()> {
        if old == new {
            return Ok(());
        }
        VfsPath::validate_name(new)?;
        let vid = self.tab().ok_or_else(|| Error::not_found("tab"))?.volume;
        self.require(vid, Capabilities::RENAME, "rename")?;
        let (tab, vol) = self.current().unwrap();
        let (from, to) = (tab.path.join(old)?, tab.path.join(new)?);
        vol.fs().rename(&from, &to)?;
        self.volume_changed(vid)?;
        if let Some(t) = self.tabs.get_mut(self.active) {
            t.select_only(new);
        }
        Ok(())
    }

    /// Deletes the selection of the active tab (recursively), in the background.
    pub fn delete_selection(&mut self) -> Result<usize> {
        let tab = self.tab().ok_or_else(|| Error::not_found("tab"))?;
        let (vid, paths) = (tab.volume, tab.selected_paths());
        self.require(vid, Capabilities::REMOVE, "delete")?;
        let n = paths.len();
        let ops = paths.into_iter().map(|path| Op::Remove { side: Side::Dst, path }).collect();
        self.start_job(format!("Deleting {n} item{}", plural(n)), vid, None, ops, Vec::new(), None, false);
        Ok(n)
    }

    pub fn copy_selection(&mut self, cut: bool) -> usize {
        let Some(tab) = self.tab() else { return 0 };
        let paths = tab.selected_paths();
        let n = paths.len();
        if n > 0 {
            self.clipboard = Some(Clipboard { volume: tab.volume, paths, cut });
        }
        n
    }

    /// Pastes the clipboard into the active tab's directory. Moves inside a
    /// volume are instant renames; copies (also between images) and moves
    /// between images run as a background job. Returns the number of entries.
    pub fn paste(&mut self) -> Result<usize> {
        let clip = self.clipboard.clone().ok_or_else(|| Error::invalid_input("the clipboard is empty"))?;
        let tab = self.tab().ok_or_else(|| Error::not_found("tab"))?;
        let (dst_vid, dst_dir) = (tab.volume, tab.path.clone());
        self.require(dst_vid, Capabilities::WRITE | Capabilities::CREATE_DIR, "paste")?;
        if clip.cut {
            self.require(clip.volume, Capabilities::REMOVE, "move")?;
        }
        let same_volume = clip.volume == dst_vid;
        let mut renamed = Vec::new();
        let (mut ops, mut then, mut names) = (Vec::new(), Vec::new(), Vec::new());
        for src in &clip.paths {
            let src_name = src.file_name().unwrap_or("root");
            if same_volume && dst_dir.starts_with(src) {
                let verb = if clip.cut { "move" } else { "copy" };
                return Err(Error::invalid_input(format!("cannot {verb} {src} into itself")));
            }
            if clip.cut && same_volume {
                // Moving inside one volume: a cheap rename, done right away.
                if src.parent().as_ref() != Some(&dst_dir) {
                    let vol = &self.volumes[&dst_vid];
                    vol.fs().rename(src, &dst_dir.join(src_name)?)?;
                    renamed.push(src_name.to_owned());
                }
                continue;
            }
            let name = ops::unique_name(self.volumes[&dst_vid].fs().as_mut(), &dst_dir, src_name)?;
            ops.push(Op::Copy { src: src.clone(), dst: dst_dir.join(&name)? });
            if clip.cut {
                then.push(Op::Remove { side: Side::Src, path: src.clone() });
            }
            names.push(name);
        }
        if !renamed.is_empty() {
            self.clipboard = None;
            self.volume_changed(dst_vid)?;
            if let Some(t) = self.tabs.get_mut(self.active) {
                t.selection = renamed.iter().cloned().collect();
            }
        }
        let queued = names.len();
        if !ops.is_empty() {
            let n = names.len();
            let verb = if clip.cut { "Moving" } else { "Copying" };
            let title = format!("{verb} {n} item{} to {dst_dir}", plural(n));
            let src = (!same_volume).then_some(clip.volume);
            self.start_job(title, dst_vid, src, ops, then, Some((dst_vid, dst_dir, names)), clip.cut);
        }
        Ok(renamed.len() + queued)
    }

    /// Copies host files and folders into `dir` of `volume`, in the
    /// background and in pieces: they are read from the host as they are
    /// written, so their size does not matter.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn import_host(&mut self, volume: VolumeId, dir: &VfsPath, paths: &[std::path::PathBuf]) -> Result<usize> {
        self.require(volume, Capabilities::WRITE, "import")?;
        // One job per host folder the items are in (usually just one).
        let mut groups: BTreeMap<std::path::PathBuf, Vec<String>> = BTreeMap::new();
        for p in paths {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
            let (Some(parent), Some(name)) = (p.parent(), name) else {
                return Err(Error::invalid_input(format!("{}: cannot import this", p.display())));
            };
            VfsPath::validate_name(&name)?;
            groups.entry(parent.to_owned()).or_default().push(name);
        }
        let mut total = 0;
        let mut taken: Vec<String> = Vec::new();
        for (parent, names) in groups {
            let host: super::volume::SharedFs =
                Arc::new(std::sync::Mutex::new(Box::new(dix_core::hostfs::HostFs::new(&parent)?)));
            let (mut ops, mut made) = (Vec::new(), Vec::new());
            for name in names {
                let unique = ops::unique_name(self.volumes[&volume].fs().as_mut(), dir, &name)?;
                // Two items with the same name (from different folders): keep both.
                let unique = if taken.contains(&unique) { format!("{unique} ({})", taken.len() + 1) } else { unique };
                ops.push(Op::Copy { src: VfsPath::root().join(&name)?, dst: dir.join(&unique)? });
                taken.push(unique.clone());
                made.push(unique);
            }
            let n = made.len();
            total += n;
            let title = format!("Importing {n} item{} to {dir}", plural(n));
            let dst = self.volumes[&volume].shared_fs();
            let select = Some((volume, dir.clone(), made));
            self.start_job_with(title, dst, Some(host), vec![volume], ops, Vec::new(), select, false);
        }
        Ok(total)
    }

    /// Copies the selection of the active tab to the host, in the
    /// background and in pieces: into `folder` (keeping names, unique), or
    /// one file to `file` (a "Save as" target, replaced if it exists).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn export_to_host(&mut self, target: ExportTarget) -> Result<usize> {
        let (tab, vol) = self.current().ok_or_else(|| Error::not_found("tab"))?;
        let paths = tab.selected_paths();
        let src = vol.shared_fs();
        if paths.is_empty() {
            return Err(Error::invalid_input("nothing selected"));
        }
        let (folder, ops) = match target {
            ExportTarget::File(file) => {
                let [one] = paths.as_slice() else { return Err(Error::invalid_input("save as: one item only")) };
                let parent = file.parent().ok_or_else(|| Error::invalid_input("no folder"))?.to_owned();
                let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                (parent, vec![Op::Copy { src: one.clone(), dst: VfsPath::root().join(&name)? }])
            }
            ExportTarget::Folder(folder) => {
                let mut host = dix_core::hostfs::HostFs::new(&folder)?;
                let mut ops = Vec::new();
                let mut taken = Vec::new();
                for p in &paths {
                    let name = p.file_name().unwrap_or("root");
                    let unique = ops::unique_name(&mut host, &VfsPath::root(), name)?;
                    let unique =
                        if taken.contains(&unique) { format!("{unique} ({})", taken.len() + 1) } else { unique };
                    ops.push(Op::Copy { src: p.clone(), dst: VfsPath::root().join(&unique)? });
                    taken.push(unique);
                }
                (folder, ops)
            }
        };
        let n = ops.len();
        let host: super::volume::SharedFs =
            Arc::new(std::sync::Mutex::new(Box::new(dix_core::hostfs::HostFs::new(&folder)?)));
        let title = format!("Exporting {n} item{} to {}", plural(n), folder.display());
        self.start_job_with(title, host, Some(src), Vec::new(), ops, Vec::new(), None, false);
        Ok(n)
    }

    /// Copies host files/folders into `dir` of `volume`, in the background.
    pub fn import(&mut self, volume: VolumeId, dir: &VfsPath, bundles: Vec<Bundle>) -> Result<usize> {
        self.require(volume, Capabilities::WRITE, "import")?;
        let n = bundles.len();
        let (mut ops, mut names) = (Vec::new(), Vec::new());
        for b in bundles {
            let name = ops::unique_name(self.volumes[&volume].fs().as_mut(), dir, b.name())?;
            // Two dropped files with the same name: keep both.
            let name = if names.contains(&name) { format!("{} ({})", name, names.len() + 1) } else { name };
            bundle_ops(dir, b.with_name(name.clone()), &mut ops)?;
            names.push(name);
        }
        let title = format!("Importing {n} item{} to {dir}", plural(n));
        self.start_job(title, volume, None, ops, Vec::new(), Some((volume, dir.clone(), names)), false);
        Ok(n)
    }

    #[allow(clippy::too_many_arguments)]
    fn start_job(
        &mut self,
        title: String,
        dst: VolumeId,
        src: Option<VolumeId>,
        ops: Vec<Op>,
        then: Vec<Op>,
        select: Option<(VolumeId, VfsPath, Vec<String>)>,
        clear_clipboard: bool,
    ) {
        let dst_fs = self.volumes[&dst].shared_fs();
        let src_fs = src.and_then(|s| self.volumes.get(&s)).map(|v| v.shared_fs());
        // A move between images also writes (deletes) on the source.
        let mut writes = vec![dst];
        if let Some(s) = src.filter(|_| !then.is_empty()) {
            writes.push(s);
        }
        self.start_job_with(title, dst_fs, src_fs, writes, ops, then, select, clear_clipboard);
    }

    /// [`start_job`](Self::start_job) between any two file systems (the
    /// host's too); `writes`: the volumes it changes (busy meanwhile).
    #[allow(clippy::too_many_arguments)]
    fn start_job_with(
        &mut self,
        title: String,
        dst_fs: super::volume::SharedFs,
        src_fs: Option<super::volume::SharedFs>,
        writes: Vec<VolumeId>,
        ops: Vec<Op>,
        then: Vec<Op>,
        select: Option<(VolumeId, VfsPath, Vec<String>)>,
        clear_clipboard: bool,
    ) {
        let handle = JobHandle::start(WriteJob::new(dst_fs, src_fs, ops, then), self.notify.clone());
        let id = self.next_job;
        self.next_job += 1;
        self.jobs.push(RunningJob {
            id,
            title,
            writes,
            handle,
            select,
            clear_clipboard,
            interrupted: false,
            last_refresh: 0.0,
        });
    }

    /// Asks a job to stop after its current step.
    pub fn cancel_job(&self, id: u64) {
        if let Some(j) = self.jobs.iter().find(|j| j.id == id) {
            j.handle.cancel();
        }
    }

    /// Advances/collects jobs. Tabs of volumes being written are refreshed
    /// about once a second so new files show up while a job runs.
    pub fn poll_jobs(&mut self, now: f64) -> Vec<JobOutcome> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.jobs.len() {
            self.jobs[i].handle.pump();
            let Some(end) = self.jobs[i].handle.end() else {
                if now - self.jobs[i].last_refresh > 1.0 {
                    self.jobs[i].last_refresh = now;
                    let writes = self.jobs[i].writes.clone();
                    for v in writes {
                        self.refresh_volume(v);
                    }
                }
                i += 1;
                continue;
            };
            let job = self.jobs.remove(i);
            for &v in &job.writes {
                let _ = self.volume_changed(v);
            }
            if end == JobEnd::Done {
                if job.clear_clipboard {
                    self.clipboard = None;
                }
                if let Some((vol, dir, names)) = job.select {
                    if let Some(t) = self.tabs.get_mut(self.active).filter(|t| t.volume == vol && t.path == dir) {
                        t.selection = names.into_iter().collect();
                    }
                }
            }
            out.push(JobOutcome { title: job.title, end, interrupted: job.interrupted });
        }
        out
    }

    /// Re-reads the tabs of a volume (no sync).
    fn refresh_volume(&mut self, volume: VolumeId) {
        if let Some(v) = self.volumes.get_mut(&volume) {
            v.invalidate();
            for t in self.tabs.iter_mut().filter(|t| t.volume == volume) {
                t.refresh(v.fs().as_mut());
            }
        }
    }

    /// Tests: runs all jobs to completion.
    #[cfg(test)]
    #[allow(dead_code)] // (used by tests that need every driver)
    pub fn finish_jobs(&mut self) -> Vec<JobOutcome> {
        let mut out = Vec::new();
        while !self.jobs.is_empty() {
            out.extend(self.poll_jobs(0.0));
            std::thread::yield_now();
        }
        out
    }

    /// The name of the selected entry, if exactly one file is selected.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn selected_single_file(&mut self) -> Option<String> {
        let (tab, vol) = self.current()?;
        let [one] = tab.selected_paths().try_into().ok()?;
        let one: VfsPath = one;
        let is_file = vol.fs().metadata(&one).ok()?.kind == dix_core::EntryKind::File;
        is_file.then(|| one.file_name().unwrap_or("file").to_owned())
    }

    /// Reads the selection of the active tab for exporting to the host.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn export_selection(&mut self) -> Result<Vec<Bundle>> {
        let (tab, vol) = self.current().ok_or_else(|| Error::not_found("tab"))?;
        let paths = tab.selected_paths();
        if paths.is_empty() {
            return Err(Error::invalid_input("nothing selected"));
        }
        paths.iter().map(|p| ops::read_bundle(vol.fs().as_mut(), p)).collect()
    }

    /// The size of the whole disk a volume is on (its container read
    /// through), without writing anything.
    pub fn whole_disk_size(&self, id: VolumeId) -> Option<u64> {
        let v = self.volumes.get(&id)?;
        if let Some(buf) = &v.buffer {
            let dev = MemoryDevice::from_shared(v.source.location.clone(), buf.clone());
            return self.registry.open_disk(Box::new(dev), None).ok().map(|d| d.size());
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.host_reader(std::path::Path::new(&v.source.location)).ok().map(|d| d.size())
        }
        #[cfg(target_arch = "wasm32")]
        None
    }

    /// The whole disk a volume is on (all partitions, its container read
    /// through), after writing the volume's pending changes.
    pub fn whole_disk(&mut self, id: VolumeId) -> Result<Box<dyn BlockDevice>> {
        let v = self.volumes.get(&id).ok_or_else(|| Error::not_found("the image is no longer open"))?;
        if v.mode().is_writable() {
            v.fs().sync()?;
        }
        if let Some(buf) = &v.buffer {
            let dev = MemoryDevice::from_shared(v.source.location.clone(), buf.clone());
            return self.registry.open_disk(Box::new(dev), None);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = std::path::PathBuf::from(&v.source.location);
            self.open_host(&path, AccessMode::ReadOnly)
        }
        #[cfg(target_arch = "wasm32")]
        Err(Error::unsupported("host files in the browser"))
    }

    /// Starts looking for the deleted files of a volume (NTFS, FAT,
    /// exFAT): its partition is read (after writing pending changes) on a
    /// thread; [`poll_recoveries`](Self::poll_recoveries) opens what was
    /// found as a read-only volume.
    pub fn begin_recover(&mut self, id: VolumeId) -> Result<()> {
        let v = self.volumes.get(&id).ok_or_else(|| Error::not_found("the image is no longer open"))?;
        if !dix_drivers::recover::supports(v.driver.id) {
            return Err(Error::unsupported(format!(
                "finding deleted files on {} (NTFS, FAT and exFAT are supported)",
                v.driver.name
            )));
        }
        if v.mode().is_writable() {
            v.fs().sync()?;
        }
        let name = format!("{} — deleted files", v.name);
        let mut source = v.source.clone();
        source.container = None;
        let dev = self.raw_device(id)?;
        let (dev, plug) = DetachableDevice::new(dev);
        let (tx, rx) = std::sync::mpsc::channel();
        let notify = self.notify.clone();
        let work = move || {
            let _ = tx.send(dix_drivers::recover::deleted_files(Box::new(dev)));
            notify();
        };
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::spawn(work);
        #[cfg(target_arch = "wasm32")]
        work();
        self.recovering.push(Recovering { name, source, plug, rx });
        Ok(())
    }

    /// Starts looking for photos and videos by their content on `dev` (a
    /// volume's partition, or a whole image that may not even open);
    /// [`poll_recoveries`](Self::poll_recoveries) opens what was found.
    pub fn begin_media_scan(&mut self, dev: Box<dyn BlockDevice>, name: String, source: VolumeSource) -> ScanProgress {
        let total = dev.size();
        let progress = ScanProgress {
            done: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            total,
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let (dev, plug) = DetachableDevice::new(dev);
        let (tx, rx) = std::sync::mpsc::channel();
        let notify = self.notify.clone();
        let (done, cancel) = (progress.done.clone(), progress.cancel.clone());
        let work = move || {
            let mut report = |at: u64, _: u64| {
                done.store(at, std::sync::atomic::Ordering::Relaxed);
                !cancel.load(std::sync::atomic::Ordering::Relaxed)
            };
            let r = dix_drivers::recover::carve::scan(Box::new(dev), &mut report)
                .map(|fs| Box::new(fs) as Box<dyn dix_core::FileSystem>);
            let _ = tx.send(r);
            notify();
        };
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::spawn(work);
        #[cfg(target_arch = "wasm32")]
        work();
        self.recovering.push(Recovering { name, source, plug, rx });
        progress
    }

    /// Scans a volume's partition for photos and videos (see
    /// [`begin_media_scan`](Self::begin_media_scan)).
    pub fn begin_media_scan_of(&mut self, id: VolumeId) -> Result<ScanProgress> {
        let v = self.volumes.get(&id).ok_or_else(|| Error::not_found("the image is no longer open"))?;
        if v.mode().is_writable() {
            v.fs().sync()?;
        }
        let name = format!("{} — found photos", v.name);
        let mut source = v.source.clone();
        source.container = None;
        let dev = self.raw_device(id)?;
        Ok(self.begin_media_scan(dev, name, source))
    }

    /// Whether deleted files are being looked for.
    pub fn is_recovering(&self) -> bool {
        !self.recovering.is_empty()
    }

    /// Finished searches for deleted files: each opened as a volume.
    pub fn poll_recoveries(&mut self) -> Vec<MountOutcome> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.recovering.len() {
            let Ok(result) = self.recovering[i].rx.try_recv() else {
                i += 1;
                continue;
            };
            let r = self.recovering.remove(i);
            let result = result.map(|fs| {
                let id = VolumeId(self.next_id);
                self.next_id += 1;
                let driver = dix_core::DriverInfo {
                    id: "recovered",
                    name: "Deleted files",
                    description: "Files found in deleted entries (read-only)",
                    capabilities: fs.capabilities(),
                };
                let volume = Volume::new(id, r.name.clone(), r.source, driver, fs, None, r.plug, Vec::new());
                self.volumes.insert(id, volume);
                self.open_tab(id, VfsPath::root());
                id
            });
            out.push(MountOutcome { name: r.name, result, log: Vec::new() });
        }
        out
    }

    /// A volume's raw bytes, read-only: its image (through the open file
    /// it uses, or its memory) and, for a partition, that partition.
    pub fn raw_device(&mut self, id: VolumeId) -> Result<Box<dyn BlockDevice>> {
        let v = self.volumes.get(&id).ok_or_else(|| Error::not_found("the image is no longer open"))?;
        let part = choice(v.source.partition);
        if let Some(buf) = &v.buffer {
            let dev = MemoryDevice::from_shared(v.source.location.clone(), buf.clone());
            let dev = self.registry.open_disk(Box::new(dev), None)?;
            return in_partition(&self.registry, dev, part);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = std::path::PathBuf::from(&v.source.location);
            let dev = self.open_host(&path, AccessMode::ReadOnly)?;
            in_partition(&self.registry, dev, part)
        }
        #[cfg(target_arch = "wasm32")]
        Err(Error::unsupported("raw views of host files in the browser"))
    }

    /// The current bytes of an in-memory image, with the image's own file
    /// name (not the volume's, which names its partition: `disk.img #2`).
    pub fn image_bytes(&mut self, volume: VolumeId) -> Result<(String, Vec<u8>)> {
        let vol = self.volumes.get_mut(&volume).ok_or_else(|| Error::not_found("volume"))?;
        // A clean checkpoint, so the saved image is not flagged as "not
        // cleanly unmounted". The volume stays usable.
        vol.fs().unmount()?;
        let buf = vol.buffer.as_ref().ok_or_else(|| Error::unsupported("downloading a host image"))?;
        let bytes = buf.snapshot();
        vol.modified = false;
        let name = if vol.source.location.is_empty() { vol.name.clone() } else { vol.source.location.clone() };
        Ok((name, bytes))
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// The choice that reopens partition `p` (None: the whole device).
fn choice(p: Option<u32>) -> PartitionChoice {
    p.map_or(PartitionChoice::Whole, PartitionChoice::Number)
}

/// `dev`, or the partition of it `choice` means.
fn in_partition(
    registry: &DriverRegistry,
    mut dev: Box<dyn BlockDevice>,
    choice: PartitionChoice,
) -> Result<Box<dyn BlockDevice>> {
    match dix_core::partition::resolve(registry, dev.as_mut(), choice)? {
        Some(p) => dix_core::partition::slice(dev, &p),
        None => Ok(dev),
    }
}

#[cfg(all(test, feature = "ext4", feature = "simplefs", feature = "archive"))]
mod tests {
    use super::*;

    fn ws_with_image() -> (Workspace, VolumeId) {
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let id = ws.create_memory_image("a.img".into(), 64 * 1024, "simplefs", FormatOptions::default(), None).unwrap();
        (ws, id)
    }

    fn file(name: &str, data: &[u8]) -> Bundle {
        Bundle::File { name: name.into(), data: data.to_vec(), modified: None }
    }

    fn names(ws: &Workspace, tab: usize) -> Vec<String> {
        ws.tabs[tab].visible().map(|e| e.name.clone()).collect()
    }

    #[test]
    fn copy_paste_between_volumes_and_round_trip() {
        let (mut ws, a) = ws_with_image();
        let name = ws.new_folder().unwrap();
        assert_eq!(name, "New folder");
        ws.rename("New folder", "docs").unwrap();
        ws.import(a, &VfsPath::parse("/docs").unwrap(), vec![file("x.txt", b"hi")]).unwrap();
        assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        ws.tabs[0].select_only("docs");
        assert_eq!(ws.copy_selection(false), 1);

        let b = ws.create_memory_image("b.img".into(), 64 * 1024, "simplefs", FormatOptions::default(), None).unwrap();
        assert_eq!(ws.active, 1);
        assert_eq!(ws.paste().unwrap(), 1);
        ws.finish_jobs();
        assert_eq!(ws.paste().unwrap(), 1); // second paste gets a unique name
        ws.finish_jobs();
        assert_eq!(names(&ws, 1), vec!["docs", "docs (2)"]);
        assert!(ws.tabs[1].selection.contains("docs (2)"));

        // Round trip through the image bytes.
        let (_, bytes) = ws.image_bytes(b).unwrap();
        let log = {
            let id = ws.mount_bytes("copy.img".into(), bytes, None, AccessMode::ReadOnly).unwrap();
            let v = &ws.volumes[&id];
            assert_eq!(v.fs().read_to_vec(&VfsPath::parse("/docs (2)/x.txt").unwrap()).unwrap(), b"hi");
            v.mount_log.clone()
        };
        // Saved images are clean checkpoints.
        assert!(log.iter().all(|e| e.level == dix_core::LogLevel::Info), "{log:?}");
        // Read-only volume refuses modifications.
        assert_eq!(ws.new_folder().unwrap_err().kind(), ErrorKind::ReadOnly);
    }

    /// A file that does not fit fails the import without leaving a
    /// half-written copy behind; the files before it stay, and the space
    /// the failed one took is free again.
    #[test]
    fn a_failed_import_leaves_no_partial_file() {
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let opts = FormatOptions { label: None, variant: Some("ext4".into()) };
        let id = ws.create_memory_image("small.img".into(), 8 << 20, "ext4", opts, None).unwrap();
        let free = |ws: &Workspace| ws.volumes[&id].fs().info().free_bytes.unwrap();
        let before = free(&ws);
        let root = VfsPath::root();
        ws.import(id, &root, vec![file("fits.txt", b"kept"), file("too-big.bin", &vec![7u8; 9 << 20])]).unwrap();
        let ends = ws.finish_jobs();
        assert!(matches!(&ends[0].end, JobEnd::Failed(e) if e.contains("space")), "{:?}", ends[0].end);
        assert_eq!(names(&ws, 0), vec!["lost+found", "fits.txt"]);
        let mut fs = ws.volumes[&id].fs();
        assert_eq!(fs.read_to_vec(&VfsPath::parse("/fits.txt").unwrap()).unwrap(), b"kept");
        drop(fs);
        // Only fits.txt's block (and its inode) are in use now.
        assert!(before - free(&ws) <= 8192, "{before} -> {}", free(&ws));
        // And the image is consistent.
        ws.check_volume(id).unwrap();
        let o = wait_check(&mut ws);
        assert!(o.result.unwrap().is_clean(), "{:?}", o.log);
    }

    #[test]
    fn copying_a_folder_into_itself_is_refused() {
        let (mut ws, _) = ws_with_image();
        ws.new_folder().unwrap();
        ws.tabs[0].select_only("New folder");
        ws.copy_selection(false);
        let p = VfsPath::parse("/New folder").unwrap();
        ws.with_tab(|t, fs| t.navigate(fs, p));
        assert_eq!(ws.paste().unwrap_err().kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn writes_make_a_volume_busy_and_searches_do_not() {
        let (mut ws, id) = ws_with_image();
        // A memory image created in the app counts as unsaved.
        assert!(ws.has_unsaved_memory_image(id));
        assert_eq!(ws.quit_blockers().len(), 1);
        ws.image_bytes(id).unwrap(); // "downloaded"
        assert!(!ws.has_unsaved_memory_image(id));
        assert!(ws.quit_blockers().is_empty());

        // Searching is reading: never busy.
        ws.tabs[0].search_query = "x".into();
        ws.start_search();
        assert!(ws.activities(id).is_empty());

        // A write job is busy until it ends.
        let many: Vec<Bundle> = (0..3000).map(|i| file(&format!("f{i}"), &[1u8; 64])).collect();
        ws.import(id, &VfsPath::root(), many).unwrap();
        assert!(!ws.jobs.is_empty());
        ws.finish_jobs();
        assert!(ws.activities(id).is_empty());

        ws.close_volume(id).unwrap();
        assert!(ws.volumes.is_empty() && ws.tabs.is_empty());
    }

    /// The scenario for testing crash recovery: force-eject in the middle of a
    /// write and look at what the next mount reports.
    #[test]
    fn forced_eject_mid_write_is_detected_on_next_mount() {
        let (mut ws, a) = ws_with_image();
        ws.image_bytes(a).unwrap();
        let buffer = ws.volumes[&a].buffer.clone().unwrap();
        let many: Vec<Bundle> = (0..5000).map(|i| file(&format!("f{i}"), &[7u8; 256])).collect();
        ws.import(a, &VfsPath::root(), many).unwrap();
        // Wait until the job has started writing, then yank.
        while ws.jobs[0].handle.progress().items == 0 && ws.jobs[0].handle.end().is_none() {
            std::thread::yield_now();
        }
        ws.force_close_volume(a);
        let outcomes = ws.finish_jobs();
        assert!(outcomes[0].interrupted);

        let id = ws.mount_bytes("after.img".into(), buffer.snapshot(), None, AccessMode::ReadOnly).unwrap();
        let log = &ws.volumes[&id].mount_log;
        assert!(log.iter().any(|e| e.message.contains("not cleanly unmounted")), "{log:?}");

        // A clean eject leaves no such warning.
        let (mut ws2, b) = ws_with_image();
        let buffer = ws2.volumes[&b].buffer.clone().unwrap();
        ws2.new_folder().unwrap();
        ws2.close_volume(b).unwrap();
        let id = ws2.mount_bytes("clean.img".into(), buffer.snapshot(), None, AccessMode::ReadOnly).unwrap();
        assert!(ws2.volumes[&id].mount_log.iter().all(|e| e.level == dix_core::LogLevel::Info));
    }

    #[test]
    fn moves_inside_and_between_volumes() {
        let (mut ws, _) = ws_with_image();
        ws.new_folder().unwrap();
        ws.new_folder().unwrap();
        // Inside one volume: an instant rename.
        ws.tabs[0].select_only("New folder (2)");
        ws.copy_selection(true);
        let p = VfsPath::parse("/New folder").unwrap();
        ws.with_tab(|t, fs| t.navigate(fs, p));
        ws.paste().unwrap();
        assert!(ws.jobs.is_empty());
        assert!(ws.clipboard.is_none());
        ws.with_tab(|t, fs| t.go_up(fs));
        assert_eq!(names(&ws, 0), vec!["New folder"]);

        // Between volumes: copy, then delete the source.
        ws.tabs[0].select_only("New folder");
        ws.copy_selection(true);
        ws.create_memory_image("b.img".into(), 64 * 1024, "simplefs", FormatOptions::default(), None).unwrap();
        ws.paste().unwrap();
        assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        assert_eq!(names(&ws, 1), vec!["New folder"]);
        assert!(names(&ws, 0).is_empty());
        let p = VfsPath::parse("/New folder/New folder (2)").unwrap();
        assert!(ws.volumes.values().last().unwrap().fs().exists(&p));
    }

    #[test]
    fn an_image_file_is_open_at_most_once() {
        let dir = std::env::temp_dir().join(format!("dix-once-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let path = dir.join("disk.img");
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let id = retry(|| ws.create_file_image(&path, 64 * 1024, "simplefs", FormatOptions::default(), None));
        // Same file through a different spelling of the path.
        let other = dir.join("sub").join("..").join("disk.img");
        assert_eq!(ws.find_open_path(&other, PartitionChoice::Auto), Some(Ok(id)));
        // And the OS lock refuses a second open even outside the workspace.
        let again = dix_core::device::host::open_path(&path, AccessMode::ReadOnly);
        assert_eq!(again.err().unwrap().kind(), ErrorKind::InUse);
        ws.close_volume(id).unwrap();
        assert_eq!(ws.find_open_path(&path, PartitionChoice::Auto), None);
        assert!(dix_core::device::host::open_path(&path, AccessMode::ReadWrite).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_split_image_opens_from_any_part_once() {
        let dir = std::env::temp_dir().join(format!("dix-split-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let whole = dir.join("disk.img");
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let id = retry(|| ws.create_file_image(&whole, 64 * 1024, "simplefs", FormatOptions::default(), None));
        ws.new_folder().unwrap();
        ws.close_volume(id).unwrap();
        // disk.img.001, .002, .003
        let bytes = std::fs::read(&whole).unwrap();
        std::fs::remove_file(&whole).unwrap();
        for (i, piece) in bytes.chunks(25_000).enumerate() {
            std::fs::write(dir.join(format!("disk.img.{:03}", i + 1)), piece).unwrap();
        }
        let id = ws.mount_path(&dir.join("disk.img.002"), None, AccessMode::ReadOnly).unwrap();
        assert_eq!(ws.volumes[&id].driver.id, "simplefs");
        assert!(ws.volumes[&id].fs().exists(&VfsPath::parse("/New folder").unwrap()));
        for part in ["disk.img.001", "disk.img.003"] {
            assert_eq!(ws.find_open_path(&dir.join(part), PartitionChoice::Auto), Some(Ok(id)), "{part}");
        }
        // The parts are only read.
        let e = ws.mount_path(&dir.join("disk.img.003"), None, AccessMode::ReadWrite).err().unwrap();
        assert_eq!(e.kind(), ErrorKind::ReadOnly);
        ws.close_volume(id).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recovers_deleted_files_into_a_volume() {
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let opts = FormatOptions { label: None, variant: Some("fat16".into()) };
        let id = ws.create_memory_image("card.img".into(), 32 << 20, "fat", opts, None).unwrap();
        let photo: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        ws.import(id, &VfsPath::root(), vec![file("PHOTO.JPG", &photo), file("keep.txt", b"kept")]).unwrap();
        assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        ws.volumes[&id].fs().remove_file(&VfsPath::parse("/PHOTO.JPG").unwrap()).unwrap();
        ws.begin_recover(id).unwrap();
        let mut outcome = Vec::new();
        for _ in 0..200 {
            outcome = ws.poll_recoveries();
            if !outcome.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let rid = outcome.pop().unwrap().result.unwrap();
        let v = &ws.volumes[&rid];
        assert_eq!(v.name, "card.img — deleted files");
        assert_eq!(v.driver.id, "recovered");
        assert_eq!(v.mode(), AccessMode::ReadOnly);
        let list = v.fs().read_dir(&VfsPath::root()).unwrap();
        let names: Vec<_> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["_HOTO.JPG"], "the 8.3 name's first letter is lost");
        assert_eq!(list[0].metadata.note.as_deref(), Some("deleted · intact"));
        assert_eq!(v.fs().read_to_vec(&VfsPath::parse("/_HOTO.JPG").unwrap()).unwrap(), photo);
        // A file system without undelete support says so.
        let other =
            ws.create_memory_image("x.img".into(), 1 << 20, "simplefs", FormatOptions::default(), None).unwrap();
        assert!(ws.begin_recover(other).is_err());
    }

    #[test]
    fn converts_the_whole_disk_of_a_partition() {
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let gpt = Some(dix_core::partition::Scheme::Gpt);
        let id = ws.create_memory_image("disk.img".into(), 16 << 20, "ext4", FormatOptions::default(), gpt).unwrap();
        ws.import(id, &VfsPath::root(), vec![file("a.txt", b"inside the partition")]).unwrap();
        assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        // The whole disk (with the partition table), changes written.
        let mut src = ws.whole_disk(id).unwrap();
        assert_eq!(src.size(), 16 << 20);
        let formats = ws.registry.formats().to_vec();
        let qcow2 = formats.iter().find(|f| f.name() == "qcow2").unwrap().clone();
        let mem = MemoryDevice::new("out.qcow2", Vec::new());
        let buffer = mem.buffer();
        dix_core::container::convert(
            src.as_mut(),
            Box::new(mem),
            Some(qcow2.as_ref()),
            "out.qcow2",
            &formats,
            &mut |_, _| true,
        )
        .unwrap();
        let bytes = buffer.snapshot();
        assert!(bytes.starts_with(b"QFI\xfb") && bytes.len() < 8 << 20);
        let disk = ws.registry.open_disk(Box::new(MemoryDevice::new("out.qcow2", bytes)), None).unwrap();
        let p = dix_core::partition::open_partition(disk, 1).unwrap();
        let mut m = ws.registry.mount(p, Some("ext4"), AccessMode::ReadOnly).unwrap();
        assert_eq!(m.fs.read_to_vec(&VfsPath::parse("/a.txt").unwrap()).unwrap(), b"inside the partition");
    }

    #[test]
    fn downloading_a_partition_downloads_the_image_by_its_name() {
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let gpt = Some(dix_core::partition::Scheme::Gpt);
        let id = ws.create_memory_image("disk.img".into(), 16 << 20, "ext4", FormatOptions::default(), gpt).unwrap();
        assert_eq!(ws.volumes[&id].name, "disk.img #1");
        let (name, bytes) = ws.image_bytes(id).unwrap();
        assert_eq!(name, "disk.img");
        assert_eq!(bytes.len(), 16 << 20, "the whole disk, not the partition");
    }

    #[test]
    fn new_ext4_memory_image_round_trip() {
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let opts = FormatOptions { label: Some("MEM".into()), variant: Some("ext4".into()) };
        let id = ws.create_memory_image("mem.img".into(), 16 << 20, "ext4", opts, None).unwrap();
        assert_eq!(ws.volumes[&id].info().fs_name, "ext4");
        assert!(ws.volumes[&id].caps().contains(Capabilities::WRITE));
        ws.new_folder().unwrap();
        ws.import(id, &VfsPath::parse("/New folder").unwrap(), vec![file("a.txt", b"in ext4")]).unwrap();
        assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        // "Save image": a clean checkpoint that mounts without warnings.
        let (_, bytes) = ws.image_bytes(id).unwrap();
        let copy = ws.mount_bytes("copy.img".into(), bytes, None, AccessMode::ReadOnly).unwrap();
        let v = &ws.volumes[&copy];
        assert_eq!(v.driver.id, "ext4");
        assert!(v.mount_log.iter().all(|e| e.level == dix_core::LogLevel::Info), "{:?}", v.mount_log);
        assert_eq!(v.fs().read_to_vec(&VfsPath::parse("/New folder/a.txt").unwrap()).unwrap(), b"in ext4");
        assert_eq!(v.info().label.as_deref(), Some("MEM"));
    }

    #[test]
    fn checking_an_open_memory_image() {
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let opts = FormatOptions { label: None, variant: Some("ext4".into()) };
        let id = ws.create_memory_image("mem.img".into(), 16 << 20, "ext4", opts, None).unwrap();
        ws.import(id, &VfsPath::root(), (0..50).map(|i| file(&format!("f{i}"), b"data")).collect()).unwrap();
        ws.finish_jobs();
        ws.check_volume(id).unwrap();
        let mut done = Vec::new();
        while done.is_empty() {
            done = ws.poll_checks();
            std::thread::yield_now();
        }
        let o = done.pop().unwrap();
        let report = o.result.unwrap();
        assert!(report.is_clean(), "{:?}", o.log);
        assert!(o.reopen.is_none());
        assert!(ws.volumes.contains_key(&id), "an in-memory image stays open while it is checked");
    }

    fn wait_check(ws: &mut Workspace) -> super::super::CheckOutcome {
        loop {
            if let Some(o) = ws.poll_checks().pop() {
                return o;
            }
            std::thread::yield_now();
        }
    }

    /// Runs `f` again while a forked child of another test still holds an
    /// image's lock (inherited for a moment, until it execs).
    fn retry<T>(mut f: impl FnMut() -> Result<T>) -> T {
        for _ in 0..100 {
            match f() {
                Err(e) if e.kind() == ErrorKind::InUse => std::thread::sleep(std::time::Duration::from_millis(50)),
                r => return r.unwrap(),
            }
        }
        f().unwrap()
    }

    /// Opens a host image, retrying briefly while a forked child of another
    /// test still holds its lock.
    fn open_retry(path: &std::path::Path, mode: AccessMode) -> Box<dyn BlockDevice> {
        for _ in 0..100 {
            match dix_core::device::host::open_path(path, mode) {
                Err(e) if e.kind() == ErrorKind::InUse => std::thread::sleep(std::time::Duration::from_millis(50)),
                r => return r.unwrap(),
            }
        }
        panic!("{} stays locked", path.display());
    }

    fn wait_mount(ws: &mut Workspace) -> MountOutcome {
        loop {
            if let Some(o) = ws.poll_mounts().pop() {
                return o;
            }
            std::thread::yield_now();
        }
    }

    /// A GPT disk image (made by sgdisk) with ext4 in partitions 2 and 3:
    /// opened by partition, automatically, twice read-only, checked.
    #[test]
    fn partitions_of_a_disk_image() {
        let Some(sgdisk) = ["/usr/sbin/sgdisk", "/sbin/sgdisk"].into_iter().find(|p| std::path::Path::new(p).exists())
        else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("dix-parts-ws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.img");
        std::fs::File::create(&path).unwrap().set_len(48 << 20).unwrap();
        let st = std::process::Command::new(sgdisk)
            .args(["-n1:0:+4M", "-t1:ef00", "-n2:0:+20M", "-t2:8300", "-c2:root", "-n3:0:0", "-t3:8300"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(st.status.success());
        for (n, label) in [(2, "ROOT"), (3, "DATA")] {
            let dev = open_retry(&path, AccessMode::ReadWrite);
            let mut part = dix_core::partition::open_partition(dev, n).unwrap();
            dix_drivers::ext4::mkfs::mkfs(part.as_mut(), dix_drivers::ext4::mkfs::Variant::Ext4, label).unwrap();
        }
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let open = |ws: &mut Workspace, choice, mode| {
            let dev = open_retry(&path, mode);
            ws.begin_mount(dev, "disk.img".into(), None, mode, None, choice);
            wait_mount(ws)
        };
        // Auto: the first partition a driver recognises (not the EFI one).
        let a = open(&mut ws, PartitionChoice::Auto, AccessMode::ReadOnly).result.unwrap();
        assert_eq!(ws.volumes[&a].name, "disk.img #2 (root)");
        assert_eq!(ws.volumes[&a].source.partition, Some(2));
        assert_eq!(ws.volumes[&a].fs().info().label.as_deref(), Some("ROOT"));
        // Another partition of the same image, read-only too (shared lock).
        let b = open(&mut ws, PartitionChoice::Number(3), AccessMode::ReadOnly).result.unwrap();
        assert_eq!(ws.volumes[&b].name, "disk.img #3");
        assert_eq!(ws.volumes[&b].fs().info().label.as_deref(), Some("DATA"));
        // "Already open" is per partition.
        assert_eq!(ws.find_open_path(&path, PartitionChoice::Number(2)), Some(Ok(a)));
        assert_eq!(ws.find_open_path(&path, PartitionChoice::Number(3)), Some(Ok(b)));
        assert_eq!(ws.find_open_path(&path, PartitionChoice::Number(1)), None);
        // A partition with no file system says so.
        ws.close_volume(a).unwrap();
        ws.close_volume(b).unwrap();
        let o = open(&mut ws, PartitionChoice::Number(1), AccessMode::ReadOnly);
        assert_eq!(o.result.unwrap_err().kind(), ErrorKind::UnknownFormat);
        // Checking an open partition: ejected, checked, reopened as itself.
        let c = open(&mut ws, PartitionChoice::Number(3), AccessMode::ReadWrite).result.unwrap();
        ws.check_volume(c).unwrap();
        let o = wait_check(&mut ws);
        assert!(o.result.unwrap().is_clean(), "{:?}", o.log);
        assert!(matches!(
            o.reopen,
            Some(super::super::Reopen::Path { partition: PartitionChoice::Number(3), mode: AccessMode::ReadWrite, .. })
        ));

        // Two partitions of one image open read-write at once: they share
        // the image's open file (a second open would hit its lock).
        let mount_host = |ws: &mut Workspace, n: u32, mode| {
            let dev = ws.open_host(&path, mode)?;
            ws.begin_mount(dev, "disk.img".into(), None, mode, None, PartitionChoice::Number(n));
            wait_mount(ws).result
        };
        let p2 = mount_host(&mut ws, 2, AccessMode::ReadWrite).unwrap();
        let p3 = mount_host(&mut ws, 3, AccessMode::ReadWrite).unwrap();
        for (id, name) in [(p2, "two.txt"), (p3, "three.txt")] {
            ws.import(id, &VfsPath::root(), vec![file(name, name.as_bytes())]).unwrap();
            assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        }
        // The dialog can still read the table through the shared file.
        assert_eq!(ws.scan_host(&path).unwrap().unwrap().drivers.iter().flatten().count(), 2);
        // Checking one partition leaves the other open and reopens itself.
        ws.check_volume(p2).unwrap();
        let o = wait_check(&mut ws);
        assert!(o.result.unwrap().is_clean(), "{:?}", o.log);
        assert!(ws.volumes.contains_key(&p3));
        ws.close_volume(p3).unwrap();
        // Read-only first: read-write for another partition is refused, clearly.
        let ro = mount_host(&mut ws, 2, AccessMode::ReadOnly).unwrap();
        let e = ws.open_host(&path, AccessMode::ReadWrite).err().unwrap();
        assert_eq!(e.kind(), ErrorKind::InUse);
        assert!(e.to_string().contains("read-only"), "{e}");
        ws.close_volume(ro).unwrap();
        // Everything closed: the lock is gone, both files are there.
        for (n, name) in [(2, "/two.txt"), (3, "/three.txt")] {
            let dev = open_retry(&path, AccessMode::ReadOnly);
            let part = dix_core::partition::open_partition(dev, n).unwrap();
            let mut m = ws.registry.mount(part, None, AccessMode::ReadOnly).unwrap();
            assert_eq!(m.fs.read_to_vec(&VfsPath::parse(name).unwrap()).unwrap(), &name.as_bytes()[1..]);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A GPT disk inside a qcow2 (made by qemu-img), both ext4 partitions
    /// open read-write at once: they share one open qcow2, whose clusters
    /// and refcounts both write to. qemu-img and e2fsck check the result.
    #[test]
    fn partitions_inside_a_qcow2() {
        let Some(sgdisk) = ["/usr/sbin/sgdisk", "/sbin/sgdisk"].into_iter().find(|p| std::path::Path::new(p).exists())
        else {
            return;
        };
        let qemu = |args: &[&str]| std::process::Command::new("qemu-img").args(args).output();
        if qemu(&["--version"]).is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("dix-qcow2-ws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("disk.raw");
        std::fs::File::create(&raw).unwrap().set_len(48 << 20).unwrap();
        let st = std::process::Command::new(sgdisk).args(["-n1:0:+20M", "-n2:0:0"]).arg(&raw).output().unwrap();
        assert!(st.status.success());
        for (n, label) in [(1, "ONE"), (2, "TWO")] {
            let dev = open_retry(&raw, AccessMode::ReadWrite);
            let mut part = dix_core::partition::open_partition(dev, n).unwrap();
            dix_drivers::ext4::mkfs::mkfs(part.as_mut(), dix_drivers::ext4::mkfs::Variant::Ext4, label).unwrap();
        }
        let path = dir.join("disk.qcow2");
        let (r, p) = (raw.to_str().unwrap(), path.to_str().unwrap());
        assert!(qemu(&["convert", "-O", "qcow2", r, p]).unwrap().status.success());
        let mut ws = Workspace::new(dix_drivers::default_registry());
        // The open dialog sees the partitions inside.
        let scan = retry(|| ws.scan_host(&path)).unwrap();
        assert_eq!(scan.drivers.iter().flatten().count(), 2);
        let mut ids = Vec::new();
        for n in [1, 2] {
            let dev = retry(|| ws.open_host(&path, AccessMode::ReadWrite));
            ws.begin_mount(dev, "disk.qcow2".into(), None, AccessMode::ReadWrite, None, PartitionChoice::Number(n));
            ids.push(wait_mount(&mut ws).result.unwrap());
        }
        assert!(ws.volumes[&ids[0]].source.container.as_deref().unwrap().starts_with("qcow2"));
        let big: Vec<u8> = (0..2_000_000u32).map(|i| (i * 13 % 251) as u8).collect();
        for (id, name) in [(ids[0], "one.bin"), (ids[1], "two.bin")] {
            ws.import(id, &VfsPath::root(), vec![file(name, &big)]).unwrap();
            assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        }
        for id in ids {
            ws.close_volume(id).unwrap();
        }
        let check = qemu(&["check", p]).unwrap();
        assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stdout));
        assert!(qemu(&["convert", "-O", "raw", p, r]).unwrap().status.success());
        for (n, name) in [(1, "/one.bin"), (2, "/two.bin")] {
            let dev = open_retry(&raw, AccessMode::ReadOnly);
            let part = dix_core::partition::open_partition(dev, n).unwrap();
            let mut m = ws.registry.mount(part, None, AccessMode::ReadOnly).unwrap();
            assert!(m.fs.read_to_vec(&VfsPath::parse(name).unwrap()).unwrap() == big);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// "New image…" as a qcow2 (GPT, ext4) and as an in-memory VHD: files
    /// written in, qemu-img checks the qcow2, the memory one opens again.
    #[test]
    fn new_vm_disks() {
        if std::process::Command::new("qemu-img").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("dix-newvm-ws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("new.qcow2");
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let opts = FormatOptions { label: Some("VM".into()), variant: Some("ext4".into()) };
        let scheme = Some(dix_core::partition::Scheme::Gpt);
        let id = retry(|| ws.create_file_disk(&path, 64 << 20, "ext4", opts.clone(), scheme, Some("qcow2")));
        assert!(ws.volumes[&id].source.container.as_deref().unwrap().starts_with("qcow2"));
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 253) as u8).collect();
        ws.import(id, &VfsPath::root(), vec![file("data.bin", &data)]).unwrap();
        assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        ws.close_volume(id).unwrap();
        // Small on disk: only what was written.
        assert!(std::fs::metadata(&path).unwrap().len() < 20 << 20);
        let out = std::process::Command::new("qemu-img").arg("check").arg(&path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        let dev = retry(|| ws.open_host(&path, AccessMode::ReadOnly));
        ws.begin_mount(dev, "new.qcow2".into(), None, AccessMode::ReadOnly, None, PartitionChoice::Auto);
        let id = wait_mount(&mut ws).result.unwrap();
        assert_eq!(ws.volumes[&id].fs().read_to_vec(&VfsPath::parse("/data.bin").unwrap()).unwrap(), data);
        ws.close_volume(id).unwrap();

        // A DMG with FAT32 in a GPT: rewritten when saved, read back.
        let dmg = dir.join("new.dmg");
        let fat = FormatOptions { label: Some("MAC".into()), variant: Some("fat32".into()) };
        let id = retry(|| ws.create_file_disk(&dmg, 64 << 20, "fat", fat.clone(), scheme, Some("DMG")));
        ws.import(id, &VfsPath::root(), vec![file("data.bin", &data)]).unwrap();
        assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        ws.close_volume(id).unwrap();
        assert!(std::fs::metadata(&dmg).unwrap().len() < 8 << 20, "compressed");
        let dev = retry(|| ws.open_host(&dmg, AccessMode::ReadOnly));
        ws.begin_mount(dev, "new.dmg".into(), None, AccessMode::ReadOnly, None, PartitionChoice::Auto);
        let id = wait_mount(&mut ws).result.unwrap();
        assert!(ws.volumes[&id].source.container.as_deref().unwrap().contains("zlib"));
        assert_eq!(ws.volumes[&id].fs().read_to_vec(&VfsPath::parse("/data.bin").unwrap()).unwrap(), data);
        ws.close_volume(id).unwrap();

        let id = ws.create_memory_disk("m.vhd".into(), 32 << 20, "ext4", opts, None, Some("VHD")).unwrap();
        ws.import(id, &VfsPath::root(), vec![file("a.txt", b"in a vhd")]).unwrap();
        assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        ws.volumes[&id].fs().sync().unwrap();
        let bytes = ws.volumes[&id].buffer.as_ref().unwrap().snapshot();
        assert!(bytes.len() < 16 << 20, "a dynamic VHD in memory");
        let disk = ws.registry.open_disk(Box::new(MemoryDevice::new("m.vhd", bytes)), None).unwrap();
        let mut m = ws.registry.mount(disk, None, AccessMode::ReadOnly).unwrap();
        assert_eq!(m.fs.read_to_vec(&VfsPath::parse("/a.txt").unwrap()).unwrap(), b"in a vhd");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An archive as the app opens it (a shared, detachable host file): it
    /// is rewritten, grows and shrinks, and the result is a zip.
    #[test]
    fn a_zip_archive_in_the_app() {
        let dir = std::env::temp_dir().join(format!("dix-zip-ws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("new.zip");
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let opts = FormatOptions { label: Some("made here".into()), variant: Some("zip".into()) };
        let id = retry(|| ws.create_file_image(&path, 0, "archive", opts.clone(), None));
        ws.close_volume(id).unwrap();
        let dev = ws.open_host(&path, AccessMode::ReadWrite).unwrap();
        ws.begin_mount(dev, "new.zip".into(), None, AccessMode::ReadWrite, None, PartitionChoice::Auto);
        let id = wait_mount(&mut ws).result.unwrap();
        assert_eq!(ws.volumes[&id].fs().info().fs_name, "zip");
        let big: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 256) as u8).collect();
        ws.import(id, &VfsPath::root(), vec![file("a.txt", b"hello"), file("big.bin", &big)]).unwrap();
        assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        ws.close_volume(id).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"PK\x03\x04"));
        let id = retry(|| ws.mount_path(&path, None, AccessMode::ReadOnly));
        let mut fs = ws.volumes[&id].fs();
        assert_eq!(fs.read_to_vec(&VfsPath::parse("/big.bin").unwrap()).unwrap(), big);
        assert_eq!(fs.info().label.as_deref(), Some("made here"));
        drop(fs);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Large files go in, within and out of an ext4 image in pieces, and
    /// arrive whole: import from the host, copy inside the image, export to
    /// a folder and as one "Save as" file.
    #[test]
    fn large_files_are_copied_in_pieces() {
        let dir = std::env::temp_dir().join(format!("dix-stream-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("out")).unwrap();
        // 13 MiB + a bit: three full pieces and a short one.
        let big: Vec<u8> = (0..(13u32 << 20) + 12345).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        std::fs::write(dir.join("big.bin"), &big).unwrap();
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let opts = FormatOptions { label: None, variant: Some("ext4".into()) };
        let id = retry(|| ws.create_file_image(&dir.join("disk.img"), 64 << 20, "ext4", opts.clone(), None));
        // Import from the host.
        ws.import_host(id, &VfsPath::root(), &[dir.join("big.bin")]).unwrap();
        let ends = ws.finish_jobs();
        assert!(matches!(ends[0].end, JobEnd::Done), "{:?}", ends[0].end);
        let p = |s: &str| VfsPath::parse(s).unwrap();
        assert!(ws.volumes[&id].fs().read_to_vec(&p("/big.bin")).unwrap() == big);
        // Copy inside the same image (one lock, taken in turn).
        ws.new_folder().unwrap();
        ws.tabs[0].select_only("big.bin");
        ws.copy_selection(false);
        let folder = p("/New folder");
        ws.with_tab(|t, fs| t.navigate(fs, folder));
        ws.paste().unwrap();
        assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        assert!(ws.volumes[&id].fs().read_to_vec(&p("/New folder/big.bin")).unwrap() == big);
        // Export: into a folder, and as one file.
        ws.tabs[0].select_only("big.bin");
        ws.export_to_host(ExportTarget::Folder(dir.join("out"))).unwrap();
        assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        assert!(std::fs::read(dir.join("out/big.bin")).unwrap() == big);
        ws.export_to_host(ExportTarget::File(dir.join("out/saved-as.bin"))).unwrap();
        assert!(matches!(ws.finish_jobs()[0].end, JobEnd::Done));
        assert!(std::fs::read(dir.join("out/saved-as.bin")).unwrap() == big);
        // Exporting again into the folder keeps both.
        ws.export_to_host(ExportTarget::Folder(dir.join("out"))).unwrap();
        ws.finish_jobs();
        assert!(dir.join("out/big (2).bin").exists());
        ws.close_volume(id).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The process's peak memory, in MB (Linux).
    fn peak_mb() -> u64 {
        let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        s.lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
            .unwrap_or(0)
            / 1024
    }

    /// Memory while a 400 MB file goes host → ext4 → ext4 → host, and
    /// zip → ext4. Run alone (the peak is the whole process's):
    /// `cargo test --release -p dix-app -- --ignored --test-threads 1 memory`
    #[test]
    #[ignore]
    fn memory_while_copying_large_files() {
        let dir = std::env::temp_dir().join(format!("dix-memory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let size = 400u64 << 20;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(dir.join("big.bin")).unwrap();
            let block: Vec<u8> = (0..1u32 << 20).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
            for _ in 0..size >> 20 {
                f.write_all(&block).unwrap();
            }
        }
        let start = peak_mb();
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let opts = FormatOptions { label: None, variant: Some("ext4".into()) };
        let id = retry(|| ws.create_file_image(&dir.join("disk.img"), 2 << 30, "ext4", opts.clone(), None));
        let step = |ws: &mut Workspace, what: &str| {
            let ends = ws.finish_jobs();
            assert!(
                ends.iter().all(|e| e.end == JobEnd::Done),
                "{what}: {:?}",
                ends.iter().map(|e| &e.end).collect::<Vec<_>>()
            );
            eprintln!("{what}: peak {} MB (started at {start} MB)", peak_mb());
        };
        ws.import_host(id, &VfsPath::root(), &[dir.join("big.bin")]).unwrap();
        step(&mut ws, "import 400 MB host → ext4");
        ws.new_folder().unwrap();
        ws.tabs[0].select_only("big.bin");
        ws.copy_selection(false);
        let folder = VfsPath::parse("/New folder").unwrap();
        ws.with_tab(|t, fs| t.navigate(fs, folder));
        ws.paste().unwrap();
        step(&mut ws, "copy 400 MB ext4 → ext4");
        ws.tabs[0].select_only("big.bin");
        ws.export_to_host(ExportTarget::Folder(dir.join("out"))).unwrap();
        step(&mut ws, "export 400 MB ext4 → host");
        assert_eq!(std::fs::metadata(dir.join("out/big.bin")).unwrap().len(), size);
        // zip → ext4 (stored member, streamed out of the zip).
        let st = std::process::Command::new("zip").args(["-q", "-0", "big.zip", "big.bin"]).current_dir(&dir).status();
        if st.is_ok_and(|s| s.success()) {
            let zid = ws.mount_path(&dir.join("big.zip"), None, AccessMode::ReadOnly).unwrap();
            let t = ws.tabs.iter().position(|t| t.volume == zid).unwrap();
            ws.active = t;
            ws.tabs[t].select_only("big.bin");
            ws.copy_selection(false);
            ws.active = 0;
            ws.with_tab(|t, fs| t.navigate(fs, VfsPath::root()));
            ws.paste().unwrap();
            step(&mut ws, "copy 400 MB zip → ext4");
        }
        let grew = peak_mb().saturating_sub(start);
        assert!(grew < 150, "memory grew by {grew} MB copying 400 MB files");
        ws.close_volume(id).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn repairing_an_open_image_file() {
        let debugfs = ["/usr/sbin/debugfs", "/sbin/debugfs"].into_iter().find(|p| std::path::Path::new(p).exists());
        let Some(debugfs) = debugfs else { return };
        let dir = std::env::temp_dir().join(format!("dix-repair-ws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.img");
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let opts = FormatOptions { label: None, variant: Some("ext4".into()) };
        let id = retry(|| ws.create_file_image(&path, 16 << 20, "ext4", opts.clone(), None));
        ws.import(id, &VfsPath::root(), vec![file("a.txt", b"a")]).unwrap();
        ws.finish_jobs();
        ws.close_volume(id).unwrap();
        // Damage it (a safe kind), open it again read-write, then repair.
        let st = std::process::Command::new(debugfs)
            .args(["-w", "-R", "sif /a.txt links_count 4"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(st.status.success());
        ws.mount_path(&path, None, AccessMode::ReadWrite).unwrap();
        let target = super::super::RepairTarget::Path { path: path.clone(), partition: PartitionChoice::Auto };
        ws.repair(&target).unwrap();
        assert!(ws.volumes.is_empty(), "ejected for the repair");
        let o = wait_check(&mut ws);
        let report = o.result.unwrap();
        assert!(report.is_clean() && report.fixed > 0, "{:?}", o.log);
        assert!(matches!(o.reopen, Some(super::super::Reopen::Path { mode: AccessMode::ReadWrite, .. })));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn repairing_an_in_memory_image_remounts_it() {
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let opts = FormatOptions { label: None, variant: Some("ext4".into()) };
        let id = ws.create_memory_image("mem.img".into(), 16 << 20, "ext4", opts, None).unwrap();
        ws.repair(&super::super::RepairTarget::Volume(id)).unwrap();
        assert!(ws.volumes.is_empty());
        let o = wait_check(&mut ws);
        assert!(o.result.unwrap().is_clean());
        let Some(super::super::Reopen::Memory { name, buffer, partition, mode, driver, modified }) = o.reopen else {
            panic!("no reopen")
        };
        ws.remount_memory(name, buffer, partition, mode, driver, modified);
        loop {
            if !ws.poll_mounts().is_empty() {
                break;
            }
            std::thread::yield_now();
        }
        let v = ws.volumes.values().next().unwrap();
        assert!(v.modified, "still unsaved after the repair");
        assert_eq!(v.mode(), AccessMode::ReadWrite);
    }
}
