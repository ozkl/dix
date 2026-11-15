//! **SimpleFS** – a deliberately tiny file system used to test the whole
//! stack (create image → mount read-write → modify → save → reload).
//!
//! It is *not* meant for real data. The whole tree is loaded into memory on
//! mount and written back (write-through) after every modification.
//!
//! On-disk layout (little endian):
//!
//! ```text
//! offset 0, 512 bytes: superblock
//!   0  [u8; 8]  magic "DIXSIMP1"
//!   8  u32      version (1)
//!   12 u32      entry count
//!   16 u64      payload length
//!   24 u32      payload checksum (FNV-1a)
//!   28 u32      flags (bit 0 = mounted read-write: not cleanly unmounted)
//!   32 [u8; 64] volume label (UTF-8, NUL padded)
//!   96 i64      creation time
//!   508 u32     superblock checksum (FNV-1a of bytes 0..508)
//! offset 512: payload = entries, parents before children
//!   u16 path length, path bytes ("/a/b"), u8 kind (1 = file, 2 = dir),
//!   i64 mtime, u64 data length, data
//! ```

mod format;

use dix_core::{
    AccessMode, BlockDevice, Capabilities, DirEntry, Driver, DriverInfo, EntryKind, Error, ErrorKind, FileSystem,
    FormatOptions, FsInfo, Metadata, MountOptions, ProbeScore, Result, VfsPath, time::unix_now,
};
use format::{HEADER_SIZE, Node, Tree};

pub use format::MAGIC;

pub struct SimpleFsDriver;

const CAPS: Capabilities = Capabilities::READ
    .union(Capabilities::CHECK)
    .union(Capabilities::WRITE)
    .union(Capabilities::CREATE_DIR)
    .union(Capabilities::REMOVE)
    .union(Capabilities::RENAME)
    .union(Capabilities::TIMESTAMPS)
    .union(Capabilities::FORMAT);

/// Default size of a freshly created image.
pub const DEFAULT_IMAGE_SIZE: u64 = 1024 * 1024;

impl Driver for SimpleFsDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: "simplefs",
            name: "SimpleFS",
            description: "Minimal test file system (read-write). Good for trying the app out.",
            capabilities: CAPS,
        }
    }

    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
        if dev.size() < HEADER_SIZE as u64 {
            return Ok(0);
        }
        let mut hdr = [0u8; HEADER_SIZE];
        dev.read_at(0, &mut hdr)?;
        Ok(match format::Header::decode(&hdr) {
            Ok(_) => 100,
            Err(_) if hdr.starts_with(MAGIC) => 50, // damaged superblock, but ours
            Err(_) => 0,
        })
    }

    fn mount(&self, mut dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
        let log = &opts.log;
        let header = format::read_header(dev.as_mut())?;
        let mode = if dev.is_read_only() { AccessMode::ReadOnly } else { opts.mode };
        let unclean = header.flags & format::FLAG_IN_USE != 0;
        if unclean {
            log.warn(
                "The volume was not cleanly unmounted (it was force-ejected, or the app stopped while it was mounted).",
            );
            log.info("SimpleFS has no journal; checking the superblock and data checksums instead…");
        }
        let tree = match format::load(dev.as_mut()) {
            Ok(t) => t,
            Err(e) => {
                log.error(format!("The volume is damaged: {e}"));
                if unclean {
                    log.error("A write was interrupted. Without a journal SimpleFS cannot repair this.");
                }
                return Err(e);
            }
        };
        if unclean {
            log.info(format!("Checksums are valid, {} entries readable. No damage found.", tree.entry_count()));
            if !mode.is_writable() {
                log.info("Mounted read-only: the volume is left marked as not cleanly unmounted.");
            }
        }
        if mode.is_writable() {
            // Mark as in use until a clean unmount, so a crash is detected.
            format::write_flags(dev.as_mut(), format::FLAG_IN_USE)?;
        }
        Ok(Box::new(SimpleFs { dev, tree, mode, dirty: false, unmounted: false }))
    }

    fn format(&self, dev: &mut dyn BlockDevice, opts: &FormatOptions) -> Result<()> {
        let mut tree = Tree::new(opts.label.clone().unwrap_or_default(), unix_now());
        format::store(dev, &mut tree, false)
    }

    fn min_format_size(&self) -> u64 {
        HEADER_SIZE as u64
    }

    /// SimpleFS is checksummed as a whole: loading it is the check.
    fn check(&self, mut dev: Box<dyn BlockDevice>, opts: &dix_core::CheckOptions) -> Result<dix_core::CheckReport> {
        let log = &opts.log;
        log.info("Checking the superblock and data checksums");
        let mut report = dix_core::CheckReport::default();
        let header = match format::read_header(dev.as_mut()) {
            Ok(h) => h,
            Err(e) => {
                log.error(format!("Superblock: {e}"));
                report.errors = 1;
                report.summary = "damaged superblock".into();
                return Ok(report);
            }
        };
        if header.flags & format::FLAG_IN_USE != 0 {
            log.warn("The volume was not cleanly unmounted.");
            report.warnings += 1;
        }
        match format::load(dev.as_mut()) {
            Ok(tree) => {
                report.summary = format!("{} entries, {} bytes of data", tree.entry_count(), tree.encoded_len());
                log.info(format!("{}: no problems found.", report.summary));
            }
            Err(e) => {
                log.error(format!("Data: {e}"));
                report.errors = 1;
                report.summary = "damaged data".into();
            }
        }
        Ok(report)
    }
}

/// Changes are kept in memory and written to the device by
/// [`FileSystem::sync`] (and, as a fallback, when the volume is dropped), so
/// bulk operations such as importing thousands of files stay fast.
pub struct SimpleFs {
    dev: Box<dyn BlockDevice>,
    tree: Tree,
    mode: AccessMode,
    /// The in-memory tree has changes that are not on the device yet.
    dirty: bool,
    /// [`FileSystem::unmount`] completed.
    unmounted: bool,
}

impl Drop for SimpleFs {
    /// Last-chance write when the volume is dropped without unmounting. The
    /// in-use flag stays set: this was not a clean unmount. (After a forced
    /// eject the device is unplugged, so this write fails and the image stays
    /// exactly as it was at that moment.)
    fn drop(&mut self) {
        if self.dirty && !self.unmounted {
            let _ = format::store(self.dev.as_mut(), &mut self.tree, true);
        }
    }
}

impl SimpleFs {
    fn ensure_writable(&self) -> Result<()> {
        if self.mode.is_writable() { Ok(()) } else { Err(Error::read_only()) }
    }

    fn node(&self, path: &VfsPath) -> Result<&Node> {
        self.tree.get(path).ok_or_else(|| Error::not_found(path))
    }

    fn ensure_parent_dir(&self, path: &VfsPath) -> Result<()> {
        let parent = path.parent().ok_or_else(|| Error::new(ErrorKind::InvalidPath, "the root has no parent"))?;
        match self.tree.get(&parent) {
            Some(n) if n.is_dir() => Ok(()),
            Some(_) => Err(Error::not_a_directory(&parent)),
            None => Err(Error::not_found(&parent)),
        }
    }

    /// Applies a change that grows the payload by `grow` bytes. Space is
    /// checked up front so a change that cannot be stored is never made.
    fn mutate(&mut self, grow: i64, f: impl FnOnce(&mut Tree)) -> Result<()> {
        self.ensure_writable()?;
        let needed = (HEADER_SIZE as u64 + self.tree.encoded_len()).saturating_add_signed(grow);
        if grow > 0 && needed > self.dev.size() && !self.dev.info().resizable {
            return Err(Error::new(
                ErrorKind::NoSpace,
                format!("SimpleFS needs {needed} bytes but the device has {}", self.dev.size()),
            ));
        }
        if self.unmounted {
            // First change after a clean unmount/checkpoint: mark the volume
            // as in use on disk *before* writing anything, so an interruption
            // from here on is detected (like ext4's "needs recovery").
            format::write_flags(self.dev.as_mut(), format::FLAG_IN_USE)?;
            self.unmounted = false;
        }
        f(&mut self.tree);
        self.dirty = true;
        Ok(())
    }

    fn meta_of(node: &Node) -> Metadata {
        let mut m =
            Metadata::new(if node.is_dir() { EntryKind::Directory } else { EntryKind::File }, node.data.len() as u64);
        m.modified = Some(node.mtime);
        m
    }
}

impl FileSystem for SimpleFs {
    fn info(&self) -> FsInfo {
        let total = self.dev.size().saturating_sub(HEADER_SIZE as u64);
        let used = self.tree.encoded_len();
        FsInfo {
            driver_id: "simplefs".into(),
            fs_name: "SimpleFS".into(),
            label: Some(self.tree.label.clone()).filter(|l| !l.is_empty()),
            total_bytes: Some(total),
            free_bytes: Some(total.saturating_sub(used)),
            block_size: Some(1),
            details: Vec::new(),
        }
    }

    fn capabilities(&self) -> Capabilities {
        CAPS.difference(Capabilities::FORMAT.union(Capabilities::CHECK)).for_mode(self.mode)
    }

    fn access_mode(&self) -> AccessMode {
        self.mode
    }

    fn read_dir(&mut self, path: &VfsPath) -> Result<Vec<DirEntry>> {
        if !self.node(path)?.is_dir() {
            return Err(Error::not_a_directory(path));
        }
        Ok(self
            .tree
            .children(path)
            .map(|(p, n)| DirEntry { name: p.file_name().unwrap_or_default().to_owned(), metadata: Self::meta_of(n) })
            .collect())
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        Ok(Self::meta_of(self.node(path)?))
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let node = self.node(path)?;
        if node.is_dir() {
            return Err(Error::is_a_directory(path));
        }
        let len = node.data.len() as u64;
        if offset >= len {
            return Ok(0);
        }
        let n = (len - offset).min((buf.len()) as u64) as usize;
        buf[..n].copy_from_slice(&node.data[offset as usize..offset as usize + n]);
        Ok(n)
    }

    fn write_file(&mut self, path: &VfsPath, data: &[u8]) -> Result<()> {
        self.ensure_writable()?;
        self.ensure_parent_dir(path)?;
        if let Some(n) = self.tree.get(path)
            && n.is_dir()
        {
            return Err(Error::is_a_directory(path));
        }
        let now = unix_now();
        let old = self.tree.get(path).map(|n| format::entry_len(path, n.data.len())).unwrap_or(0);
        let grow = format::entry_len(path, data.len()) as i64 - old as i64;
        self.mutate(grow, |t| {
            t.insert(path.clone(), Node::file(data.to_vec(), now));
            t.touch_parent(path, now);
        })
    }

    fn write_at(&mut self, path: &VfsPath, offset: u64, data: &[u8]) -> Result<()> {
        self.ensure_writable()?;
        let node = self.node(path)?;
        if node.is_dir() {
            return Err(Error::is_a_directory(path));
        }
        let old = node.data.len();
        let end = (offset as usize).checked_add(data.len()).ok_or_else(|| Error::invalid_input("offset"))?;
        let grow = end.saturating_sub(old) as i64;
        let now = unix_now();
        self.mutate(grow, |t| {
            let mut n = t.take(path).expect("checked above");
            if n.data.len() < end {
                n.data.resize(end, 0);
            }
            n.data[offset as usize..end].copy_from_slice(data);
            n.mtime = now;
            t.insert(path.clone(), n);
        })
    }

    fn create_dir(&mut self, path: &VfsPath) -> Result<()> {
        self.ensure_writable()?;
        if path.is_root() || self.tree.get(path).is_some() {
            return Err(Error::already_exists(path));
        }
        self.ensure_parent_dir(path)?;
        let now = unix_now();
        self.mutate(format::entry_len(path, 0) as i64, |t| {
            t.insert(path.clone(), Node::dir(now));
            t.touch_parent(path, now);
        })
    }

    fn remove_file(&mut self, path: &VfsPath) -> Result<()> {
        self.ensure_writable()?;
        if self.node(path)?.is_dir() {
            return Err(Error::is_a_directory(path));
        }
        let now = unix_now();
        self.mutate(0, |t| {
            t.remove(path);
            t.touch_parent(path, now);
        })
    }

    fn remove_dir(&mut self, path: &VfsPath) -> Result<()> {
        self.ensure_writable()?;
        if path.is_root() {
            return Err(Error::invalid_input("cannot remove the root directory"));
        }
        if !self.node(path)?.is_dir() {
            return Err(Error::not_a_directory(path));
        }
        if self.tree.children(path).next().is_some() {
            return Err(Error::new(ErrorKind::DirectoryNotEmpty, format!("{path}: directory not empty")));
        }
        let now = unix_now();
        self.mutate(0, |t| {
            t.remove(path);
            t.touch_parent(path, now);
        })
    }

    fn rename(&mut self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        self.ensure_writable()?;
        if from.is_root() || to.is_root() {
            return Err(Error::invalid_input("cannot rename the root directory"));
        }
        self.node(from)?;
        if from == to {
            return Ok(());
        }
        if to.starts_with(from) {
            return Err(Error::invalid_input("cannot move a directory into itself"));
        }
        if self.tree.get(to).is_some() {
            return Err(Error::already_exists(to));
        }
        self.ensure_parent_dir(to)?;
        let now = unix_now();
        let grow = self.tree.subtree_len(from) as i64 * (to.as_str().len() as i64 - from.as_str().len() as i64);
        self.mutate(grow, |t| {
            t.move_subtree(from, to);
            t.touch_parent(from, now);
            t.touch_parent(to, now);
        })
    }

    fn sync(&mut self) -> Result<()> {
        if self.dirty {
            format::store(self.dev.as_mut(), &mut self.tree, true)?;
            self.dirty = false;
        }
        self.dev.flush()
    }

    fn unmount(&mut self) -> Result<()> {
        if self.mode.is_writable() && !self.unmounted {
            if self.dirty {
                format::store(self.dev.as_mut(), &mut self.tree, false)?;
                self.dirty = false;
            } else {
                format::write_flags(self.dev.as_mut(), 0)?;
            }
        }
        self.dev.flush()?;
        self.unmounted = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dix_core::device::{MemoryDevice, SharedBuffer};
    use dix_core::{DriverRegistry, ops};

    fn registry() -> DriverRegistry {
        crate::default_registry()
    }

    fn p(s: &str) -> VfsPath {
        VfsPath::parse(s).unwrap()
    }

    fn fresh() -> (SharedBuffer, Box<dyn FileSystem>) {
        let reg = registry();
        let mut dev = MemoryDevice::zeroed("test.img", 64 * 1024);
        let buf = dev.buffer();
        reg.format(&mut dev, "simplefs", &FormatOptions { label: Some("TEST".into()), variant: None }).unwrap();
        let m = reg.mount(Box::new(dev), None, AccessMode::ReadWrite).unwrap();
        assert_eq!(m.driver.info().id, "simplefs");
        (buf, m.fs)
    }

    #[test]
    fn save_and_load_round_trip() {
        let (buf, mut fs) = fresh();
        fs.create_dir(&p("/docs")).unwrap();
        fs.create_dir(&p("/docs/sub")).unwrap();
        fs.write_file(&p("/docs/a.txt"), b"hello").unwrap();
        fs.write_file(&p("/docs/sub/b.bin"), &[7u8; 3000]).unwrap();
        fs.write_file(&p("/top.txt"), b"top").unwrap();
        fs.rename(&p("/docs"), &p("/papers")).unwrap();
        fs.sync().unwrap();
        drop(fs);

        // Reload from the bytes only.
        let reg = registry();
        let dev = MemoryDevice::new("reload.img", buf.snapshot());
        let mut fs = reg.mount(Box::new(dev), None, AccessMode::ReadOnly).unwrap().fs;
        assert_eq!(fs.info().label.as_deref(), Some("TEST"));
        let names: Vec<_> = fs.read_dir(&VfsPath::root()).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["papers", "top.txt"]);
        assert_eq!(fs.read_to_vec(&p("/papers/a.txt")).unwrap(), b"hello");
        assert_eq!(fs.read_to_vec(&p("/papers/sub/b.bin")).unwrap(), vec![7u8; 3000]);
        assert!(fs.metadata(&p("/papers/sub")).unwrap().is_dir());
    }

    #[test]
    fn read_only_mount_rejects_writes() {
        let (buf, fs) = fresh();
        drop(fs);
        let before = buf.snapshot();
        let reg = registry();
        let dev = MemoryDevice::from_shared("x", buf.clone());
        let mut fs = reg.mount(Box::new(dev), None, AccessMode::ReadOnly).unwrap().fs;
        assert!(!fs.capabilities().is_writable());
        assert_eq!(fs.create_dir(&p("/x")).unwrap_err().kind(), ErrorKind::ReadOnly);
        assert_eq!(fs.write_file(&p("/x"), b"1").unwrap_err().kind(), ErrorKind::ReadOnly);
        assert_eq!(buf.snapshot(), before);
    }

    #[test]
    fn errors_and_recursive_ops() {
        let (_buf, mut fs) = fresh();
        fs.create_dir(&p("/d")).unwrap();
        fs.write_file(&p("/d/f"), b"1").unwrap();
        assert_eq!(fs.remove_dir(&p("/d")).unwrap_err().kind(), ErrorKind::DirectoryNotEmpty);
        assert_eq!(fs.create_dir(&p("/d")).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs.write_file(&p("/nope/f"), b"1").unwrap_err().kind(), ErrorKind::NotFound);
        assert_eq!(fs.rename(&p("/d"), &p("/d/e")).unwrap_err().kind(), ErrorKind::InvalidInput);

        let bundle = ops::read_bundle(fs.as_mut(), &p("/d")).unwrap();
        let name = ops::unique_name(fs.as_mut(), &VfsPath::root(), "d").unwrap();
        assert_eq!(name, "d (2)");
        ops::write_bundle(fs.as_mut(), &VfsPath::root(), &bundle.with_name(name)).unwrap();
        assert_eq!(fs.read_to_vec(&p("/d (2)/f")).unwrap(), b"1");
        ops::remove_all(fs.as_mut(), &p("/d")).unwrap();
        assert!(!fs.exists(&p("/d")));
        assert_eq!(ops::tree_stats(fs.as_mut(), &VfsPath::root()).unwrap(), (1, 2, 1));
    }

    #[test]
    fn fixed_size_device_reports_no_space() {
        let reg = registry();
        let mut dev = MemoryDevice::zeroed("small", 1024).with_resizable(false);
        reg.format(&mut dev, "simplefs", &FormatOptions::default()).unwrap();
        let mut fs = reg.mount(Box::new(dev), None, AccessMode::ReadWrite).unwrap().fs;
        assert_eq!(fs.write_file(&p("/big"), &[0u8; 4096]).unwrap_err().kind(), ErrorKind::NoSpace);
        // The refused write left no trace.
        assert!(!fs.exists(&p("/big")));
        fs.write_file(&p("/small"), b"ok").unwrap();
        fs.sync().unwrap();
    }

    #[test]
    fn listing_ignores_names_sharing_a_prefix() {
        let (_buf, mut fs) = fresh();
        for d in ["/a", "/a-b", "/a/x", "/a-b/y", "/a/x/z", "/ab"] {
            fs.create_dir(&p(d)).unwrap();
        }
        let names = |fs: &mut Box<dyn FileSystem>, d: &str| -> Vec<String> {
            fs.read_dir(&p(d)).unwrap().into_iter().map(|e| e.name).collect()
        };
        assert_eq!(names(&mut fs, "/"), vec!["a", "a-b", "ab"]);
        assert_eq!(names(&mut fs, "/a"), vec!["x"]);
        assert_eq!(names(&mut fs, "/a-b"), vec!["y"]);
        fs.rename(&p("/a"), &p("/c")).unwrap();
        assert_eq!(names(&mut fs, "/c/x"), vec!["z"]);
        assert_eq!(names(&mut fs, "/a-b"), vec!["y"]);
    }

    #[test]
    fn unsynced_changes_are_written_on_drop() {
        let (buf, mut fs) = fresh();
        fs.write_file(&p("/late.txt"), b"saved by drop").unwrap();
        drop(fs);
        let mut fs =
            registry().mount(Box::new(MemoryDevice::new("r", buf.snapshot())), None, AccessMode::ReadOnly).unwrap().fs;
        assert_eq!(fs.read_to_vec(&p("/late.txt")).unwrap(), b"saved by drop");
    }

    #[test]
    fn unclean_unmount_is_detected_and_logged() {
        use dix_core::device::DetachableDevice;
        use dix_core::{LogLevel, MountLog, MountOptions};
        let reg = registry();
        let mut mem = MemoryDevice::zeroed("t", 64 * 1024);
        reg.format(&mut mem, "simplefs", &FormatOptions::default()).unwrap();
        let buf = mem.buffer();
        let mount = |mode| {
            let log = MountLog::new();
            let dev = MemoryDevice::from_shared("t", buf.clone());
            let fs = reg.mount_with(Box::new(dev), None, MountOptions { mode, log: log.clone() }).map(|m| m.fs);
            (fs, log)
        };

        // Clean cycle: no warnings.
        let (fs, _) = mount(AccessMode::ReadWrite);
        let mut fs = fs.unwrap();
        fs.write_file(&p("/a"), b"1").unwrap();
        fs.unmount().unwrap();
        drop(fs);
        let (fs, log) = mount(AccessMode::ReadOnly);
        assert!(fs.is_ok());
        assert!(log.max_level() < Some(LogLevel::Warning), "{:?}", log.entries());

        // Synced write, then the device is yanked (forced eject).
        let (dev, plug) = DetachableDevice::new(MemoryDevice::from_shared("t", buf.clone()));
        let mut fs = reg.mount(Box::new(dev), None, AccessMode::ReadWrite).unwrap().fs;
        fs.write_file(&p("/b"), b"2").unwrap();
        fs.sync().unwrap();
        fs.write_file(&p("/lost"), b"never synced").unwrap();
        plug.pull();
        drop(fs); // the last-chance write fails: device is gone

        let (fs, log) = mount(AccessMode::ReadOnly);
        let mut fs = fs.unwrap();
        assert_eq!(log.max_level(), Some(LogLevel::Warning));
        assert!(log.entries().iter().any(|e| e.message.contains("not cleanly unmounted")));
        assert!(fs.exists(&p("/b")));
        assert!(!fs.exists(&p("/lost")));

        // A torn write (new payload, old superblock) is reported as damage.
        let mut bytes = buf.snapshot();
        bytes[HEADER_SIZE + 4] ^= 0xff; // inside the payload
        let log = MountLog::new();
        let r = reg.mount_with(
            Box::new(MemoryDevice::new("t", bytes)),
            Some("simplefs"),
            MountOptions { mode: AccessMode::ReadOnly, log: log.clone() },
        );
        assert_eq!(r.err().unwrap().kind(), ErrorKind::Corrupted);
        assert_eq!(log.max_level(), Some(LogLevel::Error));
    }

    #[test]
    fn unknown_images_are_refused() {
        let reg = registry();
        let mut dev = MemoryDevice::zeroed("blank.img", 4096);
        assert!(reg.detect(&mut dev).is_empty());
        let err = reg.mount(Box::new(dev), None, AccessMode::ReadOnly).err().unwrap();
        assert_eq!(err.kind(), ErrorKind::UnknownFormat);
    }
}
