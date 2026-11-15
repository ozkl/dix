//! ext2/3/4 driver, adapted from the ruxis kernel (MIT, see
//! `ruxis/LICENSE-ruxis`): the driver code in [`ruxis`] is the kernel's,
//! with only its `crate::` paths changed; [`kernel`] provides the few
//! kernel facilities it uses.

/// The kernel's log macro: messages go to the mount log (see [`kernel::log`]).
macro_rules! kinfo {
    ($($arg:tt)*) => {
        $crate::ext4::kernel::log(format!($($arg)*))
    };
}

pub mod fsck;
pub mod kernel;
pub mod mkfs;
// The kernel's code, kept as it is (its own formatting, its own lints).
#[rustfmt::skip]
#[allow(dead_code, clippy::all)]
mod ruxis;

use dix_core::{
    AccessMode, BlockDevice, Capabilities, DirEntry, Driver, DriverInfo, EntryKind, Error, ErrorKind, FileSystem,
    FormatOptions, FormatVariant, FsInfo, Metadata, MountOptions, ProbeScore, Result, VfsPath,
};
use kernel::block::Disk;
use kernel::fs::vfs::{FileType, Inode};
use std::sync::Arc;

const SB_OFFSET: u64 = 1024;
const MAGIC: u16 = 0xEF53;
const COMPAT_HAS_JOURNAL: u32 = 0x4;
const INCOMPAT_RECOVER: u32 = 0x4;
const STATE_VALID: u16 = 1;

const CAPS: Capabilities = Capabilities::READ
    .union(Capabilities::WRITE)
    .union(Capabilities::CREATE_DIR)
    .union(Capabilities::REMOVE)
    .union(Capabilities::RENAME)
    .union(Capabilities::SYMLINKS)
    .union(Capabilities::PERMISSIONS)
    .union(Capabilities::TIMESTAMPS);

const VARIANTS: &[FormatVariant] = &[
    FormatVariant {
        id: "ext4",
        name: "ext4",
        description: "Journal, extents, checksums (metadata_csum)",
        min_size: mkfs::MIN_SIZE_JOURNAL,
        extension: ".img",
        sized: true,
    },
    FormatVariant {
        id: "ext3",
        name: "ext3",
        description: "ext2 with a journal",
        min_size: mkfs::MIN_SIZE_JOURNAL,
        extension: ".img",
        sized: true,
    },
    FormatVariant {
        id: "ext2",
        name: "ext2",
        description: "No journal",
        min_size: mkfs::MIN_SIZE,
        extension: ".img",
        sized: true,
    },
];

/// ext2, ext3 and ext4 (read-write, with journal replay).
pub struct Ext4Driver;

struct Superblock {
    compat: u32,
    incompat: u32,
    state: u16,
    label: String,
}

fn read_superblock(dev: &mut dyn BlockDevice) -> Result<Option<Superblock>> {
    if dev.size() < SB_OFFSET + 1024 {
        return Ok(None);
    }
    let mut sb = [0u8; 1024];
    dev.read_at(SB_OFFSET, &mut sb)?;
    if u16::from_le_bytes([sb[56], sb[57]]) != MAGIC {
        return Ok(None);
    }
    let le32 = |o: usize| u32::from_le_bytes(sb[o..o + 4].try_into().unwrap());
    let rev = le32(76);
    let label_raw = &sb[0x78..0x88];
    let end = label_raw.iter().position(|&c| c == 0).unwrap_or(16);
    Ok(Some(Superblock {
        compat: if rev == 0 { 0 } else { le32(92) },
        incompat: if rev == 0 { 0 } else { le32(96) },
        state: u16::from_le_bytes([sb[58], sb[59]]),
        label: String::from_utf8_lossy(&label_raw[..end]).into_owned(),
    }))
}

impl Driver for Ext4Driver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: "ext4",
            name: "ext2/3/4",
            description: "Linux ext2, ext3 and ext4: read-write, journal replay, creating new ones.",
            capabilities: CAPS.union(Capabilities::FORMAT).union(Capabilities::CHECK),
        }
    }

    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
        Ok(if read_superblock(dev)?.is_some() { 90 } else { 0 })
    }

    fn format(&self, dev: &mut dyn BlockDevice, opts: &FormatOptions) -> Result<()> {
        let id = opts.variant.as_deref().unwrap_or("ext4");
        let variant =
            mkfs::Variant::parse(id).ok_or_else(|| Error::invalid_input(format!("unknown variant '{id}'")))?;
        mkfs::mkfs(dev, variant, opts.label.as_deref().unwrap_or(""))
    }

    fn min_format_size(&self) -> u64 {
        mkfs::MIN_SIZE_JOURNAL
    }

    fn check(&self, dev: Box<dyn BlockDevice>, opts: &dix_core::CheckOptions) -> Result<dix_core::CheckReport> {
        fsck::check(dev, &opts.log, opts.repair)
    }

    fn format_variants(&self) -> &'static [FormatVariant] {
        VARIANTS
    }

    fn mount(&self, dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
        Ok(Box::new(Ext4Volume::open(dev, opts)?))
    }
}

impl Ext4Volume {
    /// Mounts `dev` (see [`Driver::mount`]).
    pub fn open(mut dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Ext4Volume> {
        let log = &opts.log;
        let sb = read_superblock(dev.as_mut())?
            .ok_or_else(|| Error::new(ErrorKind::UnknownFormat, "not an ext2/3/4 file system"))?;
        let journal = sb.compat & COMPAT_HAS_JOURNAL != 0;
        let read_only = !opts.mode.is_writable() || dev.is_read_only();
        if journal && sb.incompat & INCOMPAT_RECOVER != 0 {
            log.warn("The volume was not cleanly unmounted: replaying the journal…");
            if read_only {
                log.info("Read-only mount: the journal is replayed in memory only; the image is not modified.");
            }
        } else if !journal && sb.state & STATE_VALID == 0 {
            log.warn("The volume was not cleanly unmounted and has no journal (ext2): it may be inconsistent; check it with e2fsck.");
        }
        // Read-only: every write (journal recovery) goes to a memory overlay.
        let disk = Arc::new(Disk::new(dev, read_only));
        let (root, name) =
            kernel::with_log(log, || ruxis::mount(disk.clone(), read_only, "auto")).map_err(Error::from)?;
        let fs = ruxis::fs_of(&root).ok_or_else(|| Error::new(ErrorKind::Other, "ext4: unexpected root inode"))?;
        if journal
            && sb.incompat & INCOMPAT_RECOVER != 0
            && !log.entries().iter().any(|e| e.message.contains("JBD2: recovery"))
        {
            log.info("The journal held no unfinished transactions: nothing to replay. The volume is consistent.");
        }
        if read_only && disk.overlay_sectors() > 0 {
            log.info(format!("{} KiB of recovered metadata are held in memory.", disk.overlay_sectors() / 2));
        }
        let mode = if fs.is_read_only() { AccessMode::ReadOnly } else { AccessMode::ReadWrite };
        if opts.mode.is_writable() && !mode.is_writable() {
            log.warn("Mounted read-only: the file system uses features this driver does not write.");
        }
        Ok(Ext4Volume { root, fs, name, label: sb.label, mode, checkpointed: false, released: false })
    }
}

/// A mounted ext2/3/4 volume, seen through paths.
pub struct Ext4Volume {
    root: Arc<dyn Inode>,
    fs: Arc<ruxis::Ext4Fs>,
    /// "ext2", "ext3" or "ext4", as the driver mounted it.
    name: &'static str,
    label: String,
    mode: AccessMode,
    /// A clean checkpoint was made; the next change marks the volume in use again.
    checkpointed: bool,
    released: bool,
}

impl Drop for Ext4Volume {
    /// Dropped without `unmount` (forced eject): nothing is written, the
    /// volume stays marked as in use, and the next mount recovers it.
    fn drop(&mut self) {
        if !self.released {
            self.fs.release();
        }
    }
}

impl Ext4Volume {
    fn walk(&self, path: &VfsPath) -> Result<Arc<dyn Inode>> {
        let mut cur = self.root.clone();
        for c in path.components() {
            cur = cur.lookup(c).map_err(|e| Error::from(e).with_context(path))?;
        }
        Ok(cur)
    }

    /// The parent directory of `path` and the name in it.
    fn parent<'p>(&self, path: &'p VfsPath) -> Result<(Arc<dyn Inode>, &'p str)> {
        let name = path.file_name().ok_or_else(|| Error::invalid_input("the root has no name"))?;
        let parent = path.parent().unwrap_or_default();
        let dir = self.walk(&parent)?;
        if dir.file_type() != FileType::Directory {
            return Err(Error::not_a_directory(&parent));
        }
        Ok((dir, name))
    }

    /// Before a change: writable, and in use on disk again after a checkpoint.
    fn begin_change(&mut self) -> Result<()> {
        if !self.mode.is_writable() {
            return Err(Error::read_only());
        }
        if self.checkpointed {
            self.fs.mark_in_use().map_err(Error::from)?;
            self.checkpointed = false;
        }
        Ok(())
    }

    fn meta(inode: &Arc<dyn Inode>) -> Metadata {
        let st = inode.stat();
        let kind = match FileType::from_mode(st.st_mode) {
            FileType::Regular => EntryKind::File,
            FileType::Directory => EntryKind::Directory,
            FileType::Symlink => EntryKind::Symlink,
            _ => EntryKind::Other,
        };
        let mut m = Metadata::new(kind, st.st_size.max(0) as u64);
        m.modified = Some(st.st_mtime);
        m.accessed = Some(st.st_atime);
        m.mode = Some(st.st_mode);
        m.uid = Some(st.st_uid);
        m.gid = Some(st.st_gid);
        m.inode = Some(st.st_ino);
        if kind == EntryKind::Symlink {
            m.symlink_target = inode.readlink().ok();
        }
        m
    }
}

impl FileSystem for Ext4Volume {
    fn info(&self) -> FsInfo {
        let s = self.root.statfs();
        FsInfo {
            driver_id: "ext4".into(),
            fs_name: self.name.into(),
            label: Some(self.label.clone()).filter(|l| !l.is_empty()),
            total_bytes: Some(s.f_blocks * s.f_bsize as u64),
            free_bytes: Some(s.f_bavail * s.f_bsize as u64),
            block_size: Some(s.f_bsize as u32),
            details: Vec::new(),
        }
    }

    fn capabilities(&self) -> Capabilities {
        CAPS.for_mode(self.mode)
    }

    fn access_mode(&self) -> AccessMode {
        self.mode
    }

    fn read_dir(&mut self, path: &VfsPath) -> Result<Vec<DirEntry>> {
        let dir = self.walk(path)?;
        if dir.file_type() != FileType::Directory {
            return Err(Error::not_a_directory(path));
        }
        let entries = dir.readdir().map_err(Error::from)?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            if e.name == "." || e.name == ".." {
                continue;
            }
            let child =
                dir.lookup(&e.name).map_err(|err| Error::from(err).with_context(format!("{path}/{}", e.name)))?;
            out.push(DirEntry { metadata: Self::meta(&child), name: e.name });
        }
        Ok(out)
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        Ok(Self::meta(&self.walk(path)?))
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let inode = self.walk(path)?;
        if inode.file_type() == FileType::Directory {
            return Err(Error::is_a_directory(path));
        }
        inode.read_at(offset, buf).map_err(Error::from)
    }

    fn write_file(&mut self, path: &VfsPath, data: &[u8]) -> Result<()> {
        self.begin_change()?;
        let (dir, name) = self.parent(path)?;
        let file = match dir.lookup(name) {
            Ok(f) if f.file_type() == FileType::Directory => return Err(Error::is_a_directory(path)),
            Ok(f) => {
                f.truncate(0).map_err(Error::from)?;
                f
            }
            Err(kernel::errno::Errno::ENOENT) => dir.create(name, FileType::Regular, 0o644).map_err(Error::from)?,
            Err(e) => return Err(Error::from(e)),
        };
        let mut off = 0usize;
        while off < data.len() {
            let n = file.write_at(off as u64, &data[off..]).map_err(Error::from)?;
            if n == 0 {
                return Err(Error::new(ErrorKind::Io, "ext4: short write"));
            }
            off += n;
        }
        Ok(())
    }

    fn write_at(&mut self, path: &VfsPath, offset: u64, data: &[u8]) -> Result<()> {
        self.begin_change()?;
        let file = self.walk(path)?;
        if file.file_type() == FileType::Directory {
            return Err(Error::is_a_directory(path));
        }
        let mut done = 0usize;
        while done < data.len() {
            let n = file.write_at(offset + done as u64, &data[done..]).map_err(Error::from)?;
            if n == 0 {
                return Err(Error::new(ErrorKind::Io, "ext4: short write"));
            }
            done += n;
        }
        Ok(())
    }

    fn release_cache(&mut self, path: &VfsPath) {
        if let Ok(inode) = self.walk(path) {
            ruxis::shrink_pages(&inode);
        }
    }

    fn create_dir(&mut self, path: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let (dir, name) = self.parent(path)?;
        dir.create(name, FileType::Directory, 0o755).map(|_| ()).map_err(Error::from)
    }

    fn remove_file(&mut self, path: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let (dir, name) = self.parent(path)?;
        dir.unlink(name, false).map_err(Error::from)
    }

    fn remove_dir(&mut self, path: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let (dir, name) = self.parent(path)?;
        dir.unlink(name, true).map_err(Error::from)
    }

    fn rename(&mut self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let (src_dir, old) = self.parent(from)?;
        let (dst_dir, new) = self.parent(to)?;
        if dst_dir.lookup(new).is_ok() {
            return Err(Error::already_exists(to));
        }
        src_dir.rename(old, &dst_dir, new).map_err(Error::from)
    }

    fn sync(&mut self) -> Result<()> {
        if !self.mode.is_writable() {
            return Ok(());
        }
        self.root.sync_fs().map_err(Error::from)
    }

    fn unmount(&mut self) -> Result<()> {
        if self.mode.is_writable() && !self.checkpointed {
            self.fs.checkpoint().map_err(Error::from)?;
            self.checkpointed = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
