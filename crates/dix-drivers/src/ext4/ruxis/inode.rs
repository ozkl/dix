//! Inodes: the on-disk format ([`DiskInode`]) and the VFS's side
//! ([`Ext4Inode`]).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{Ext4Fs, LINK_MAX, Meta, le16, le32, put16, put32, xattr};
use crate::ext4::kernel::arch::paging::PAGE_SIZE;
use crate::ext4::kernel::errno::{Errno, KResult};
use crate::ext4::kernel::fs::vfs::{FileType, Inode, StatFs};
use crate::ext4::kernel::fs::{DirEntry, S_IFMT, Stat};
use crate::ext4::kernel::mm::pagecache::{PageCache, frame_bytes};
use crate::ext4::kernel::mm::{PhysAddr, frame};
use crate::ext4::kernel::sync::{IrqLock, Mutex};

pub const INDEX_FL: u32 = 0x1000;
pub const HUGE_FILE_FL: u32 = 0x40000;
pub const EXTENTS_FL: u32 = 0x80000;

/// A timestamp: seconds (34 bits on the disk with the extra fields) and
/// nanoseconds.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Time {
    pub sec: i64,
    pub nsec: u32,
}

impl Time {
    pub fn now() -> Time {
        let ns = crate::ext4::kernel::time::realtime_ns();
        Time { sec: (ns / 1_000_000_000) as i64, nsec: (ns % 1_000_000_000) as u32 }
    }

    pub fn secs(sec: i64) -> Time {
        Time { sec, nsec: 0 }
    }
}

/// An on-disk inode, decoded. Bytes this doesn't know (and in-inode
/// attributes) stay as they are on the disk.
#[derive(Clone)]
pub struct DiskInode {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: Time,
    pub ctime: Time,
    pub mtime: Time,
    pub crtime: Time,
    /// Deletion time; while on the orphan list, the next orphan.
    pub dtime: u32,
    pub links: u16,
    /// In 512-byte units, including metadata blocks (extent tree,
    /// indirect blocks, the attribute block).
    pub blocks: u64,
    pub flags: u32,
    /// Block map, extent tree root, or a fast symlink's target.
    pub block: [u32; 15],
    pub generation: u32,
    /// The attribute block.
    pub file_acl: u64,
    /// Bytes of extra fields after the first 128 (0: a 128-byte inode).
    pub extra_isize: u16,
}

impl DiskInode {
    /// Does the field ending at byte `end` exist in this inode?
    fn has(&self, end: usize) -> bool {
        128 + self.extra_isize as usize >= end
    }

    pub fn decode(raw: &[u8], fs: &Ext4Fs) -> KResult<DiskInode> {
        let mut d = DiskInode {
            mode: le16(raw, 0),
            uid: le16(raw, 2) as u32 | (le16(raw, 120) as u32) << 16,
            gid: le16(raw, 24) as u32 | (le16(raw, 122) as u32) << 16,
            size: le32(raw, 4) as u64 | (le32(raw, 108) as u64) << 32,
            atime: Time::default(),
            ctime: Time::default(),
            mtime: Time::default(),
            crtime: Time::default(),
            dtime: le32(raw, 20),
            links: le16(raw, 26),
            blocks: 0,
            flags: le32(raw, 32),
            block: core::array::from_fn(|i| le32(raw, 40 + 4 * i)),
            generation: le32(raw, 100),
            file_acl: le32(raw, 104) as u64,
            extra_isize: 0,
        };
        if fs.inode_size > 128 {
            d.extra_isize = le16(raw, 128);
            if 128 + d.extra_isize as usize > fs.inode_size || !d.extra_isize.is_multiple_of(4) {
                return Err(Errno::EUCLEAN);
            }
        }
        // Seconds since the epoch, signed; with the extra fields, two more
        // bits and the nanoseconds (Linux's ext4_decode_extra_time).
        let extra_end = 128 + d.extra_isize as usize;
        let time = |lo: usize, extra: usize| {
            let mut t = Time::secs(le32(raw, lo) as i32 as i64);
            if extra_end >= extra + 4 {
                let e = le32(raw, extra);
                t.sec += ((e & 3) as i64) << 32;
                t.nsec = e >> 2;
            }
            t
        };
        d.ctime = time(12, 132);
        d.mtime = time(16, 136);
        d.atime = time(8, 140);
        if extra_end >= 148 {
            d.crtime = time(144, 148);
        }
        let mut blocks = le32(raw, 28) as u64;
        if fs.ro_compat & super::RO_COMPAT_HUGE_FILE != 0 {
            blocks |= (le16(raw, 116) as u64) << 32;
            if d.flags & HUGE_FILE_FL != 0 {
                blocks *= fs.sectors_per_block();
            }
        }
        d.blocks = blocks;
        if fs.has_incompat(super::INCOMPAT_64BIT) {
            d.file_acl |= (le16(raw, 118) as u64) << 32;
        }
        Ok(d)
    }

    /// Write the fields into `raw` (the on-disk inode, all of it).
    pub fn encode(&self, raw: &mut [u8], fs: &Ext4Fs) {
        put16(raw, 0, self.mode);
        put16(raw, 2, self.uid as u16);
        put32(raw, 4, self.size as u32);
        put32(raw, 108, (self.size >> 32) as u32);
        put32(raw, 20, self.dtime);
        put16(raw, 24, self.gid as u16);
        put16(raw, 26, self.links);
        for (i, &b) in self.block.iter().enumerate() {
            put32(raw, 40 + 4 * i, b);
        }
        put32(raw, 100, self.generation);
        put32(raw, 104, self.file_acl as u32);
        if fs.has_incompat(super::INCOMPAT_64BIT) {
            put16(raw, 118, (self.file_acl >> 32) as u16);
        }
        put16(raw, 120, (self.uid >> 16) as u16);
        put16(raw, 122, (self.gid >> 16) as u16);
        if fs.inode_size > 128 {
            put16(raw, 128, self.extra_isize);
        }
        let has = |end: usize| self.has(end);
        let mut time = |t: Time, lo: usize, extra: usize| {
            put32(raw, lo, t.sec as u32);
            if has(extra + 4) {
                let epoch = ((t.sec - t.sec as i32 as i64) >> 32) as u32 & 3;
                put32(raw, extra, epoch | t.nsec << 2);
            }
        };
        time(self.ctime, 12, 132);
        time(self.mtime, 16, 136);
        time(self.atime, 8, 140);
        if has(148) {
            time(self.crtime, 144, 148);
        }
        // i_blocks: 32 bits, 48 with huge_file, and beyond that in
        // blocks rather than sectors (HUGE_FILE_FL).
        let mut flags = self.flags & !HUGE_FILE_FL;
        let huge = fs.ro_compat & super::RO_COMPAT_HUGE_FILE != 0;
        let (lo, hi) = if self.blocks <= u32::MAX as u64 || !huge {
            (self.blocks as u32, 0)
        } else if self.blocks < 1 << 48 {
            (self.blocks as u32, (self.blocks >> 32) as u16)
        } else {
            flags |= HUGE_FILE_FL;
            let b = self.blocks / fs.sectors_per_block();
            (b as u32, (b >> 32) as u16)
        };
        put32(raw, 28, lo);
        if huge {
            put16(raw, 116, hi);
        }
        put32(raw, 32, flags);
    }

    pub fn kind(&self) -> FileType {
        FileType::from_mode(self.mode as u32)
    }

    /// A symbolic link whose target is stored in the block pointers.
    pub fn is_fast_symlink(&self, fs: &Ext4Fs) -> bool {
        // An attribute block counts in i_blocks too.
        let ea = if self.file_acl != 0 { fs.sectors_per_block() } else { 0 };
        self.kind() == FileType::Symlink && self.blocks == ea && self.size < 60
    }

    pub fn uses_extents(&self) -> bool {
        self.flags & EXTENTS_FL != 0
    }
}

// ----- the VFS's inode -------------------------------------------------------

pub struct Ext4Inode {
    fs: Arc<Ext4Fs>,
    ino: u32,
    meta: IrqLock<DiskInode>,
    pages: PageCache,
    /// Where the last read ended: a read starting there is sequential and
    /// reads ahead.
    read_end: AtomicU64,
    /// Held by writeback (from looking up the blocks of the dirty pages to
    /// writing them) and by truncation and deletion: blocks are never
    /// freed under a write on its way to them.
    io: Mutex<()>,
}

/// Pages read ahead of a sequential reader (128 KiB).
const READAHEAD_PAGES: u64 = 32;

fn downcast(inode: &Arc<dyn Inode>) -> Option<&Ext4Inode> {
    inode.as_any().downcast_ref::<Ext4Inode>()
}

impl Ext4Inode {
    pub(super) fn new(fs: Arc<Ext4Fs>, ino: u32, d: DiskInode) -> Ext4Inode {
        Ext4Inode {
            fs,
            ino,
            meta: IrqLock::new(d),
            pages: PageCache::new(),
            read_end: AtomicU64::new(0),
            io: Mutex::new(()),
        }
    }

    pub(super) fn fs(&self) -> &Arc<Ext4Fs> {
        &self.fs
    }

    pub(super) fn disk(&self) -> DiskInode {
        self.meta.lock().clone()
    }

    pub(super) fn store(&self, d: DiskInode) {
        *self.meta.lock() = d;
    }

    /// Not used by anyone but the cache: deleted, or (`all`) clean.
    pub(super) fn unused(&self, all: bool) -> bool {
        self.pages.unshare();
        self.meta.lock().links == 0 || all && !self.pages.has_dirty()
    }

    pub(super) fn shrink_pages(&self) {
        self.pages.shrink();
    }

    /// Run `f` on this inode's fields with the file system locked, then
    /// store and write back the result.
    fn change<R>(&self, f: impl FnOnce(&Ext4Fs, &mut Meta, &mut DiskInode) -> KResult<R>) -> KResult<R> {
        let fs = &self.fs;
        fs.writable()?;
        let mut meta = fs.begin()?;
        let mut d = self.disk();
        let result = f(fs, &mut meta, &mut d);
        self.store(d.clone());
        fs.write_inode(self.ino, &d)?;
        result
    }

    /// Bring pages `first..last` into the cache, those missing read with
    /// one request per run of consecutive blocks.
    fn read_pages(&self, first: u64, last: u64) -> KResult<()> {
        let fs = &self.fs;
        let bs = fs.block_size;
        let per_page = (PAGE_SIZE / bs) as u64;
        let missing: Vec<u64> = (first..last).filter(|&i| !self.pages.contains(i)).collect();
        if missing.is_empty() {
            return Ok(());
        }
        let size_blocks = self.meta.lock().size.div_ceil(bs as u64);
        // (disk block or 0 for a hole, page, offset in the page)
        let blocks: Vec<(u64, usize, usize)> = {
            let mut meta = fs.lock.lock();
            let mut d = self.disk();
            let mut out = Vec::new();
            for (k, &index) in missing.iter().enumerate() {
                let start = index * per_page;
                for lb in start..(start + per_page).min(size_blocks) {
                    out.push((
                        fs.bmap(&mut meta, self.ino, &mut d, lb, false)?,
                        k,
                        (lb - start) as usize * bs,
                    ));
                }
            }
            out
        };
        let mut frames = Vec::with_capacity(missing.len());
        for _ in &missing {
            match frame::alloc_user_zeroed() {
                Some(pa) => frames.push(pa),
                None => {
                    frames.into_iter().for_each(frame::free);
                    return Ok(());
                }
            }
        }
        let disk = fs.cache.disk();
        let mut buf = Vec::new();
        let mut i = 0;
        let mut result = Ok(());
        while i < blocks.len() {
            if blocks[i].0 == 0 {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < blocks.len() && blocks[j].0 == blocks[j - 1].0 + 1 && (j - i + 1) * bs <= Self::MAX_IO {
                j += 1;
            }
            // (Read-ahead is optional: without memory for the buffer, stop.)
            buf.clear();
            if buf.try_reserve_exact((j - i) * bs).is_err() {
                break;
            }
            buf.resize((j - i) * bs, 0);
            if let Err(e) = disk.read_bytes(blocks[i].0 * bs as u64, &mut buf) {
                result = Err(e);
                break;
            }
            for (n, &(_, k, off)) in blocks[i..j].iter().enumerate() {
                let page = unsafe { frame_bytes(frames[k]) };
                page[off..off + bs].copy_from_slice(&buf[n * bs..(n + 1) * bs]);
            }
            i = j;
        }
        // Pages that appeared meanwhile (written, or read by someone else)
        // win.
        for (k, pa) in frames.into_iter().enumerate() {
            if result.is_err() || !self.pages.insert_if_absent(missing[k], pa) {
                frame::free(pa);
            }
        }
        result
    }

    /// Fill page `index` from the disk (holes and unwritten extents read
    /// as zeroes).
    fn fill(&self, index: u64, page: &mut [u8]) -> KResult<()> {
        let fs = &self.fs;
        let bs = fs.block_size;
        let per_page = (PAGE_SIZE / bs) as u64;
        let size_blocks = self.meta.lock().size.div_ceil(bs as u64);
        let first = index * per_page;
        let blocks: Vec<u64> = {
            let mut meta = fs.lock.lock();
            let mut d = self.disk();
            (first..(first + per_page).min(size_blocks))
                .map(|lb| fs.bmap(&mut meta, self.ino, &mut d, lb, false))
                .collect::<KResult<_>>()?
        };
        let disk = fs.cache.disk();
        // Read runs of consecutive blocks in one go; holes stay zero.
        let mut i = 0;
        while i < blocks.len() {
            if blocks[i] == 0 {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < blocks.len() && blocks[j] == blocks[j - 1] + 1 {
                j += 1;
            }
            disk.read_bytes(blocks[i] * bs as u64, &mut page[i * bs..j * bs])?;
            i = j;
        }
        Ok(())
    }

    /// Write the dirty pages back: their blocks are looked up (holes of
    /// pages written through a shared mapping allocated) in one go, and
    /// runs of consecutive blocks, across pages, written with one request.
    pub(super) fn writeback(&self) -> KResult<()> {
        if self.fs.dead.load(Ordering::Relaxed) {
            return Ok(());
        }
        let _io = self.io.lock();
        let dirty = self.pages.take_dirty();
        if dirty.is_empty() {
            return Ok(());
        }
        let result = self.write_pages(&dirty);
        if result.is_err() {
            for &(index, _) in &dirty {
                self.pages.redirty(index);
            }
        }
        for &(_, pa) in &dirty {
            frame::free(pa);
        }
        result
    }

    /// Largest single write (bytes).
    const MAX_IO: usize = 256 * 1024;

    fn write_pages(&self, pages: &[(u64, PhysAddr)]) -> KResult<()> {
        let fs = &self.fs;
        let bs = fs.block_size;
        let per_page = (PAGE_SIZE / bs) as u64;
        let ino = self.ino;
        // (disk block, page, offset in the page) of every block to write.
        let blocks: Vec<(u64, PhysAddr, usize)> = self.change(|fs, meta, d| {
            let size_blocks = d.size.div_ceil(bs as u64);
            let mut out = Vec::new();
            for &(index, pa) in pages {
                let first = index * per_page;
                for lb in first..(first + per_page).min(size_blocks) {
                    let pb = fs.bmap(meta, ino, d, lb, true)?;
                    out.push((pb, pa, (lb - first) as usize * bs));
                }
            }
            Ok(out)
        })?;
        let disk = fs.cache.disk();
        let mut buf = Vec::new();
        let mut i = 0;
        while i < blocks.len() {
            let mut j = i + 1;
            while j < blocks.len() && blocks[j].0 == blocks[j - 1].0 + 1 && (j - i + 1) * bs <= Self::MAX_IO {
                j += 1;
            }
            buf.clear();
            if buf.try_reserve_exact((j - i) * bs).is_err() {
                // No memory for one big request: the blocks one by one,
                // straight from their pages.
                for &(pb, pa, off) in &blocks[i..j] {
                    disk.write_bytes(pb * bs as u64, &unsafe { frame_bytes(pa) }[off..off + bs])?;
                }
                i = j;
                continue;
            }
            for &(_, pa, off) in &blocks[i..j] {
                buf.extend_from_slice(&unsafe { frame_bytes(pa) }[off..off + bs]);
            }
            disk.write_bytes(blocks[i].0 * bs as u64, &buf)?;
            i = j;
        }
        Ok(())
    }

    /// Free the blocks and the inode of a deleted file nobody uses.
    pub(super) fn delete(&self) -> KResult<()> {
        let _io = self.io.lock();
        self.pages.truncate(0);
        let fs = &self.fs;
        let mut meta = fs.begin()?;
        let mut d = self.disk();
        if !d.is_fast_symlink(fs) {
            fs.truncate_blocks(&mut meta, self.ino, &mut d, 0)?;
        }
        // Its attribute block (or its share of one).
        if d.file_acl != 0 {
            fs.release_xattr_block(&mut meta, d.file_acl)?;
            d.file_acl = 0;
            d.blocks = d.blocks.saturating_sub(fs.sectors_per_block());
        }
        fs.orphan_del(self.ino)?;
        d.size = 0;
        d.dtime = super::now_secs();
        self.store(d.clone());
        fs.write_inode(self.ino, &d)?;
        fs.modify_inode_raw(self.ino, |raw| put32(raw, 20, d.dtime))?;
        fs.free_inode(&mut meta, self.ino, d.kind() == FileType::Directory)
    }

    /// Free the blocks beyond the end of the file (an orphan whose
    /// truncation was cut short).
    pub(super) fn truncate_to_size(&self) -> KResult<()> {
        let bs = self.fs.block_size as u64;
        let _io = self.io.lock();
        self.change(|fs, meta, d| {
            if d.kind() == FileType::Regular {
                fs.truncate_blocks(meta, self.ino, d, d.size.div_ceil(bs))?;
            }
            d.dtime = 0;
            Ok(())
        })
    }

    fn new_inode(&self, meta: &mut Meta, kind: FileType, mode: u32) -> KResult<(u32, DiskInode)> {
        let fs = &self.fs;
        let ino = fs.alloc_inode(meta, kind == FileType::Directory, fs.group_of(self.ino))?;
        let c = crate::ext4::kernel::cred::current();
        let t = Time::now();
        let mut d = DiskInode {
            mode: (kind.mode() | (mode & 0o7777)) as u16,
            uid: c.fsuid,
            gid: c.fsgid,
            size: 0,
            atime: t,
            ctime: t,
            mtime: t,
            crtime: t,
            dtime: 0,
            links: 1,
            blocks: 0,
            flags: 0,
            block: [0; 15],
            generation: crate::ext4::kernel::arch::entropy() as u32,
            file_acl: 0,
            extra_isize: fs.want_extra_isize,
        };
        // A setgid directory hands its group (and the bit, to directories) down.
        let parent = self.disk();
        if parent.mode & 0o2000 != 0 {
            d.gid = parent.gid;
            if kind == FileType::Directory {
                d.mode |= 0o2000;
            }
        }
        // Files and directories map their blocks with extents (where the
        // file system has them).
        if fs.has_incompat(super::INCOMPAT_EXTENTS) && matches!(kind, FileType::Regular | FileType::Directory)
        {
            super::extent::init_root(&mut d);
        }
        Ok((ino, d))
    }

    /// Link a freshly created inode into this directory and cache it.
    fn finish_create(
        &self,
        meta: &mut Meta,
        d_dir: &mut DiskInode,
        name: &str,
        ino: u32,
        d: DiskInode,
    ) -> KResult<Arc<Ext4Inode>> {
        let fs = &self.fs;
        let kind = d.kind();
        fs.write_inode(ino, &d)?;
        if let Err(e) = fs.dir_add(meta, self.ino, d_dir, name, ino, kind) {
            let _ = fs.free_inode(meta, ino, kind == FileType::Directory);
            return Err(e);
        }
        let inode = Arc::new(Ext4Inode::new(fs.arc(), ino, d));
        fs.inodes.lock().insert(ino, inode.clone());
        Ok(inode)
    }

    fn check_name(&self, name: &str) -> KResult<()> {
        if name.len() > 255 {
            return Err(Errno::ENAMETOOLONG);
        }
        if self.disk().kind() != FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        Ok(())
    }

    fn rdev(&self) -> u64 {
        let d = self.meta.lock();
        if !matches!(d.kind(), FileType::CharDevice | FileType::BlockDevice) {
            return 0;
        }
        // Old (16-bit) encoding in block[0], new one in block[1].
        match (d.block[0], d.block[1]) {
            (0, new) => {
                let (major, minor) = ((new >> 8) & 0xfff, (new & 0xff) | ((new >> 12) & 0xfff00));
                crate::ext4::kernel::fs::makedev(major as u64, minor as u64)
            }
            (old, _) => crate::ext4::kernel::fs::makedev((old >> 8 & 0xff) as u64, (old & 0xff) as u64),
        }
    }
}

impl Inode for Ext4Inode {
    fn xattrs(&self) -> KResult<Vec<(String, Vec<u8>)>> {
        let fs = &self.fs;
        let mut out = Vec::new();
        fs.inline_xattrs(self.ino, &mut out)?;
        let acl = self.meta.lock().file_acl;
        if acl != 0 {
            let b = fs.cache.read(acl, |b| b.to_vec())?;
            if le32(&b, 0) != xattr::XATTR_MAGIC || le32(&b, 8) != 1 {
                return super::corrupt(
                    &fs.disk_name,
                    format_args!("inode {}: bad attribute block", self.ino),
                    Errno::EUCLEAN,
                );
            }
            if fs.csum && !xattr::block_csum_ok(fs.csum_seed, acl, &b) {
                return super::corrupt(
                    &fs.disk_name,
                    format_args!("inode {}: attribute block checksum invalid", self.ino),
                    Errno::EBADMSG,
                );
            }
            xattr::parse(&b, 32, 0, &mut out)?;
        }
        Ok(out)
    }

    /// All of them in the attribute block (a new one if the old is shared),
    /// none left in the inode; no attributes: no block.
    fn set_xattrs(&self, all: Vec<(String, Vec<u8>)>) -> KResult<()> {
        let mut entries: Vec<(u8, Vec<u8>, Vec<u8>)> = all
            .into_iter()
            .map(|(n, v)| {
                let (index, rest) = xattr::split(&n);
                (index, rest.as_bytes().to_vec(), v)
            })
            .collect();
        entries.sort_by(|a, b| (a.0, a.1.len(), &a.1).cmp(&(b.0, b.1.len(), &b.1)));
        let block = if entries.is_empty() {
            None
        } else {
            Some(xattr::build_block(&entries, self.fs.block_size).ok_or(Errno::ENOSPC)?)
        };
        let ino = self.ino;
        self.change(|fs, meta, d| {
            fs.clear_inline_xattrs(ino)?;
            let old = d.file_acl;
            let spb = fs.sectors_per_block();
            match block {
                None => {
                    if old != 0 {
                        fs.release_xattr_block(meta, old)?;
                        d.file_acl = 0;
                        d.blocks = d.blocks.saturating_sub(spb);
                    }
                }
                Some(mut b) => {
                    let shared = old != 0 && fs.cache.read(old, |b| le32(b, 4))? > 1;
                    let target = if old != 0 && !shared {
                        old
                    } else {
                        let new = fs.alloc_block(meta, fs.group_of(ino), None)?;
                        if old != 0 {
                            fs.release_xattr_block(meta, old)?;
                        } else {
                            d.blocks += spb;
                        }
                        new
                    };
                    if fs.csum {
                        xattr::set_block_csum(fs.csum_seed, target, &mut b);
                    }
                    fs.cache.modify(target, |dst| dst.copy_from_slice(&b))?;
                    d.file_acl = target;
                    fs.enable_ext_attr()?;
                }
            }
            d.ctime = Time::now();
            Ok(())
        })
    }

    fn stat(&self) -> Stat {
        let d = self.meta.lock().clone();
        Stat {
            st_dev: self.fs.dev,
            st_ino: self.ino as u64,
            st_nlink: d.links as u64,
            st_mode: d.mode as u32,
            st_uid: d.uid,
            st_gid: d.gid,
            st_rdev: self.rdev(),
            st_size: d.size as i64,
            st_blksize: self.fs.block_size as i64,
            st_blocks: d.blocks as i64,
            st_atime: d.atime.sec,
            st_atime_nsec: d.atime.nsec as i64,
            st_mtime: d.mtime.sec,
            st_mtime_nsec: d.mtime.nsec as i64,
            st_ctime: d.ctime.sec,
            st_ctime_nsec: d.ctime.nsec as i64,
            ..Stat::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn statfs(&self) -> StatFs {
        let fs = &self.fs;
        let free = fs.free_blocks.load(Ordering::Relaxed);
        // The file system id: the two halves of the UUID, exclusive-or'ed
        // (as Linux's ext4_statfs).
        let lo = u64::from_le_bytes(fs.uuid[..8].try_into().unwrap());
        let hi = u64::from_le_bytes(fs.uuid[8..].try_into().unwrap());
        let fsid = lo ^ hi;
        StatFs {
            f_type: super::EXT4_MAGIC as i64,
            f_bsize: fs.block_size as i64,
            f_blocks: fs.blocks_count.saturating_sub(fs.overhead),
            f_bfree: free,
            f_bavail: free.saturating_sub(fs.reserved_blocks),
            f_files: fs.inodes_count as u64,
            f_ffree: fs.free_inodes.load(Ordering::Relaxed),
            f_fsid: [fsid as i32, (fsid >> 32) as i32],
            f_namelen: 255,
            f_frsize: fs.block_size as i64,
            f_flags: if fs.read_only { 1 } else { 0 },
            ..StatFs::default()
        }
    }

    fn lookup(&self, name: &str) -> KResult<Arc<dyn Inode>> {
        let fs = &self.fs;
        if self.disk().kind() != FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        let ino = {
            let mut meta = fs.lock.lock();
            // (Its fields as they are with the file system locked: a copy
            // from before may not map a block just added to it.)
            let mut d = self.disk();
            fs.dir_lookup(&mut meta, self.ino, &mut d, name)?
        };
        Ok(fs.iget(ino.ok_or(Errno::ENOENT)?)?)
    }

    fn readdir(&self) -> KResult<Vec<DirEntry>> {
        let fs = &self.fs;
        if self.disk().kind() != FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        let mut meta = fs.lock.lock();
        let mut d = self.disk();
        fs.dir_list(&mut meta, self.ino, &mut d)
    }

    fn create(&self, name: &str, kind: FileType, mode: u32) -> KResult<Arc<dyn Inode>> {
        self.check_name(name)?;
        let fs = &self.fs;
        fs.writable()?;
        let mut meta = fs.begin()?;
        let mut d_dir = self.disk();
        if fs.dir_lookup(&mut meta, self.ino, &mut d_dir, name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        if !matches!(kind, FileType::Regular | FileType::Directory | FileType::Fifo | FileType::Socket) {
            return Err(Errno::EPERM);
        }
        if kind == FileType::Directory && d_dir.links >= LINK_MAX {
            return Err(Errno::EMLINK);
        }
        let (ino, mut d) = self.new_inode(&mut meta, kind, mode)?;
        if kind == FileType::Directory {
            if let Err(e) = fs.dir_init(&mut meta, ino, &mut d, self.ino) {
                let _ = fs.free_inode(&mut meta, ino, true);
                return Err(e);
            }
            d.links = 2;
            d_dir.links += 1;
        }
        let inode = self.finish_create(&mut meta, &mut d_dir, name, ino, d)?;
        self.store(d_dir.clone());
        fs.write_inode(self.ino, &d_dir)?;
        Ok(inode)
    }

    fn symlink(&self, name: &str, target: &str) -> KResult<()> {
        self.check_name(name)?;
        if target.is_empty() || target.len() >= self.fs.block_size {
            return Err(Errno::ENAMETOOLONG);
        }
        let fs = &self.fs;
        fs.writable()?;
        let mut meta = fs.begin()?;
        let mut d_dir = self.disk();
        if fs.dir_lookup(&mut meta, self.ino, &mut d_dir, name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        let (ino, mut d) = self.new_inode(&mut meta, FileType::Symlink, 0o777)?;
        d.size = target.len() as u64;
        if target.len() < 60 {
            // A fast symlink: the target in place of the block pointers.
            let mut bytes = [0u8; 60];
            bytes[..target.len()].copy_from_slice(target.as_bytes());
            for (i, chunk) in bytes.chunks(4).enumerate() {
                d.block[i] = le32(chunk, 0);
            }
        } else {
            if fs.has_incompat(super::INCOMPAT_EXTENTS) {
                super::extent::init_root(&mut d);
            }
            let pb = fs.bmap(&mut meta, ino, &mut d, 0, true)?;
            fs.cache.zero(pb)?;
            fs.cache.modify(pb, |b| b[..target.len()].copy_from_slice(target.as_bytes()))?;
        }
        self.finish_create(&mut meta, &mut d_dir, name, ino, d)?;
        self.store(d_dir);
        Ok(())
    }

    fn link(&self, name: &str, inode: &Arc<dyn Inode>) -> KResult<()> {
        self.check_name(name)?;
        let target = downcast(inode).ok_or(Errno::EXDEV)?;
        if !Arc::ptr_eq(&target.fs, &self.fs) {
            return Err(Errno::EXDEV);
        }
        let fs = &self.fs;
        fs.writable()?;
        let mut meta = fs.begin()?;
        let mut d_dir = self.disk();
        if fs.dir_lookup(&mut meta, self.ino, &mut d_dir, name)?.is_some() {
            return Err(Errno::EEXIST);
        }
        let mut d = target.disk();
        if d.links >= LINK_MAX {
            return Err(Errno::EMLINK);
        }
        fs.dir_add(&mut meta, self.ino, &mut d_dir, name, target.ino, d.kind())?;
        self.store(d_dir);
        // A file without names (O_TMPFILE) given one: off the orphan list,
        // as Linux's ext4_link.
        if d.links == 0 {
            fs.orphan_del(target.ino)?;
            d.dtime = 0;
        }
        d.links += 1;
        d.ctime = Time::now();
        target.store(d.clone());
        fs.write_inode(target.ino, &d)
    }

    fn unlink(&self, name: &str, dir: bool) -> KResult<()> {
        let fs = &self.fs;
        fs.writable()?;
        let ino = {
            let mut meta = fs.lock.lock();
            let mut d_dir = self.disk();
            fs.dir_lookup(&mut meta, self.ino, &mut d_dir, name)?.ok_or(Errno::ENOENT)?
        };
        let target = fs.iget(ino)?;
        let mut meta = fs.begin()?;
        let mut d = target.disk();
        let is_dir = d.kind() == FileType::Directory;
        match (dir, is_dir) {
            (true, false) => return Err(Errno::ENOTDIR),
            (false, true) => return Err(Errno::EISDIR),
            (true, true) if !fs.dir_is_empty(&mut meta, ino, &mut d)? => return Err(Errno::ENOTEMPTY),
            _ => {}
        }
        let mut d_dir = self.disk();
        fs.dir_remove(&mut meta, self.ino, &mut d_dir, name)?;
        if is_dir {
            d_dir.links = d_dir.links.saturating_sub(1);
            d.links = 0;
        } else {
            d.links = d.links.saturating_sub(1);
        }
        self.store(d_dir.clone());
        fs.write_inode(self.ino, &d_dir)?;
        d.ctime = Time::now();
        // The blocks and the inode are freed once nobody uses it (`reap`);
        // until then it is an orphan.
        if d.links == 0 {
            fs.orphan_add(ino, &mut d)?;
        }
        target.store(d.clone());
        fs.write_inode(ino, &d)
    }

    fn rename(&self, old: &str, new_dir: &Arc<dyn Inode>, new: &str) -> KResult<()> {
        let target_dir = downcast(new_dir).ok_or(Errno::EXDEV)?;
        if !Arc::ptr_eq(&target_dir.fs, &self.fs) {
            return Err(Errno::EXDEV);
        }
        target_dir.check_name(new)?;
        let fs = &self.fs;
        fs.writable()?;
        let moved_ino = {
            let mut meta = fs.lock.lock();
            fs.dir_lookup(&mut meta, self.ino, &mut self.disk(), old)?.ok_or(Errno::ENOENT)?
        };
        let moved = fs.iget(moved_ino)?;
        let moving_dir = moved.disk().kind() == FileType::Directory;
        // Replace an existing target, if allowed.
        let existing = {
            let mut meta = fs.lock.lock();
            fs.dir_lookup(&mut meta, target_dir.ino, &mut target_dir.disk(), new)?
        };
        if let Some(existing) = existing {
            if existing == moved_ino {
                return Ok(());
            }
            let e = fs.iget(existing)?;
            match (moving_dir, e.disk().kind() == FileType::Directory) {
                (true, false) => return Err(Errno::ENOTDIR),
                (false, true) => return Err(Errno::EISDIR),
                _ => {}
            }
            target_dir.unlink(new, moving_dir)?;
        }
        let mut meta = fs.begin()?;
        let same_dir = self.ino == target_dir.ino;
        let mut d_new = target_dir.disk();
        if moving_dir && !same_dir && d_new.links >= LINK_MAX {
            return Err(Errno::EMLINK);
        }
        let kind = moved.disk().kind();
        fs.dir_add(&mut meta, target_dir.ino, &mut d_new, new, moved_ino, kind)?;
        target_dir.store(d_new.clone());
        let mut d_old = self.disk();
        fs.dir_remove(&mut meta, self.ino, &mut d_old, old)?;
        if moving_dir && !same_dir {
            let mut dm = moved.disk();
            fs.set_dotdot(&mut meta, moved_ino, &mut dm, target_dir.ino)?;
            moved.store(dm);
            d_old.links = d_old.links.saturating_sub(1);
            let mut d_new = target_dir.disk();
            d_new.links += 1;
            target_dir.store(d_new.clone());
            fs.write_inode(target_dir.ino, &d_new)?;
        }
        self.store(d_old.clone());
        fs.write_inode(self.ino, &d_old)?;
        let mut dm = moved.disk();
        dm.ctime = Time::now();
        moved.store(dm.clone());
        fs.write_inode(moved_ino, &dm)
    }

    fn readlink(&self) -> KResult<String> {
        let fs = &self.fs;
        let d = self.disk();
        if d.kind() != FileType::Symlink {
            return Err(Errno::EINVAL);
        }
        let len = d.size as usize;
        let bytes = if d.is_fast_symlink(fs) {
            d.block.iter().flat_map(|b| b.to_le_bytes()).take(len).collect::<Vec<u8>>()
        } else {
            let mut meta = fs.lock.lock();
            let mut d = self.disk();
            let pb = fs.bmap(&mut meta, self.ino, &mut d, 0, false)?;
            drop(meta);
            if pb == 0 || len > fs.block_size {
                return Err(Errno::EUCLEAN);
            }
            fs.cache.read(pb, |b| b[..len].to_vec())?
        };
        String::from_utf8(bytes).map_err(|_| Errno::EUCLEAN)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> KResult<usize> {
        let size = {
            let d = self.meta.lock();
            match d.kind() {
                FileType::Regular => d.size,
                FileType::Directory => return Err(Errno::EISDIR),
                _ => return Err(Errno::EINVAL),
            }
        };
        crate::ext4::kernel::mm::pressure::throttle();
        // Reading on from where the last read ended: read ahead.
        let page = PAGE_SIZE as u64;
        let sequential = self.read_end.swap(offset + buf.len() as u64, Ordering::Relaxed) == offset;
        let ahead = if sequential { READAHEAD_PAGES } else { 0 };
        let first = offset / page;
        let last = (offset + buf.len() as u64).min(size).div_ceil(page) + ahead;
        if last > first + 1 {
            self.read_pages(first, last.min(size.div_ceil(page)))?;
        }
        self.pages.read(offset, buf, size, |index, page| self.fill(index, page))
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> KResult<usize> {
        let fs = &self.fs;
        let end = offset.checked_add(buf.len() as u64).ok_or(Errno::EFBIG)?;
        let (old_size, max) = {
            let d = self.meta.lock();
            match d.kind() {
                FileType::Regular => (d.size, fs.max_blocks(&d)),
                FileType::Directory => return Err(Errno::EISDIR),
                _ => return Err(Errno::EINVAL),
            }
        };
        if end.div_ceil(fs.block_size as u64) > max {
            return Err(Errno::EFBIG);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        crate::ext4::kernel::mm::pressure::throttle();
        // Partly written pages of the file as it was: read now, before
        // their holes get blocks (whose old contents must not show).
        let page = PAGE_SIZE as u64;
        for index in [offset / page, (end - 1) / page] {
            let whole = index * page >= offset && (index + 1) * page <= end;
            if !whole && index * page < old_size && !self.pages.contains(index) {
                let pa = self.pages.get(index, |p| self.fill(index, p))?;
                frame::free(pa);
            }
        }
        // Allocate the blocks first, so that a full disk is reported now.
        let bs = fs.block_size as u64;
        let ino = self.ino;
        self.change(|fs, meta, d| {
            for lb in offset / bs..end.div_ceil(bs) {
                fs.bmap(meta, ino, d, lb, true)?;
            }
            d.size = d.size.max(end);
            d.mtime = Time::now();
            d.ctime = d.mtime;
            Ok(())
        })?;
        self.pages.write(offset, buf, old_size, |index, page| self.fill(index, page))?;
        Ok(buf.len())
    }

    fn truncate(&self, len: u64) -> KResult<()> {
        let fs = &self.fs;
        if self.disk().kind() != FileType::Regular {
            return Err(Errno::EINVAL);
        }
        if len.div_ceil(fs.block_size as u64) > fs.max_blocks(&self.disk()) {
            return Err(Errno::EFBIG);
        }
        let bs = fs.block_size as u64;
        // A partial last page keeps its head; the rest of it must read as
        // zeroes later, so it is cached, cleared and written back.
        if !len.is_multiple_of(PAGE_SIZE as u64) && len < self.disk().size {
            let index = len / PAGE_SIZE as u64;
            let pa = self.pages.get(index, |page| self.fill(index, page))?;
            frame::free(pa);
        }
        // No writeback meanwhile; the pages go before their blocks do (as
        // Linux's truncate_pagecache).
        let _io = self.io.lock();
        self.pages.truncate(len);
        if !len.is_multiple_of(PAGE_SIZE as u64) {
            self.pages.mark_dirty(len / PAGE_SIZE as u64);
        }
        let ino = self.ino;
        self.change(|fs, meta, d| {
            if len < d.size {
                fs.truncate_blocks(meta, ino, d, len.div_ceil(bs))?;
            }
            d.size = len;
            d.mtime = Time::now();
            d.ctime = d.mtime;
            Ok(())
        })
    }

    fn can_mmap(&self) -> bool {
        self.meta.lock().kind() == FileType::Regular
    }

    fn get_page(&self, index: u64, shared_write: bool) -> KResult<PhysAddr> {
        let size = self.meta.lock().size;
        if index >= size.div_ceil(PAGE_SIZE as u64) {
            return Err(Errno::EFAULT);
        }
        if shared_write && self.fs.read_only {
            return Err(Errno::EROFS);
        }
        let pa = self.pages.get(index, |page| self.fill(index, page))?;
        if shared_write {
            self.pages.mark_shared_writable(index);
        }
        Ok(pa)
    }

    fn chmod(&self, mode: u32) -> KResult<()> {
        self.change(|_, _, d| {
            d.mode = ((d.mode as u32 & S_IFMT) | (mode & 0o7777)) as u16;
            d.ctime = Time::now();
            Ok(())
        })
    }

    fn chown(&self, uid: Option<u32>, gid: Option<u32>) -> KResult<()> {
        self.change(|_, _, d| {
            if let Some(u) = uid {
                d.uid = u;
            }
            if let Some(g) = gid {
                d.gid = g;
            }
            if d.mode & 0o111 != 0 {
                d.mode &= !0o6000;
            }
            d.ctime = Time::now();
            Ok(())
        })
    }

    fn set_times(&self, atime: Option<i64>, mtime: Option<i64>) -> KResult<()> {
        self.change(|_, _, d| {
            if let Some(a) = atime {
                d.atime = Time::secs(a);
            }
            if let Some(m) = mtime {
                d.mtime = Time::secs(m);
            }
            d.ctime = Time::now();
            Ok(())
        })
    }

    fn sync(&self) -> KResult<()> {
        if self.fs.read_only {
            return Ok(());
        }
        self.writeback()?;
        let mut meta = self.fs.lock.lock();
        self.fs.commit(&mut meta)
    }

    fn sync_fs(&self) -> KResult<()> {
        self.fs.sync_all()
    }

    fn drop_caches(&self) -> KResult<()> {
        self.fs.drop_caches()
    }

    fn unmount(&self) -> KResult<()> {
        self.fs.unmount()
    }

    fn shutdown(&self) -> KResult<()> {
        self.fs.shutdown()
    }

    fn open(&self, _flags: u32) -> KResult<Option<Arc<dyn crate::ext4::kernel::fs::FileOps>>> {
        // Device nodes on ext4 are not connected to drivers (use /dev).
        match self.disk().kind() {
            FileType::CharDevice | FileType::BlockDevice => Err(Errno::ENXIO),
            _ => Ok(None),
        }
    }
}

impl core::fmt::Debug for Ext4Inode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ext4 inode {}", self.ino)
    }
}

// ----- extended attributes, file system side ---------------------------------

impl Ext4Fs {
    /// An inode lets go of attribute block `block`: freed if it was the
    /// last to use it, else its count goes down.
    pub(super) fn release_xattr_block(&self, meta: &mut Meta, block: u64) -> KResult<()> {
        let refs = self.cache.read(block, |b| le32(b, 4))?;
        if refs <= 1 {
            self.free_blocks(meta, block, 1)
        } else {
            let (csum, seed) = (self.csum, self.csum_seed);
            self.cache.modify(block, |b| {
                put32(b, 4, refs - 1);
                if csum {
                    xattr::set_block_csum(seed, block, b);
                }
            })
        }
    }

    /// Attributes are on the file system now (`ext_attr`, revision 1 on).
    pub(super) fn enable_ext_attr(&self) -> KResult<()> {
        if self.compat & super::COMPAT_EXT_ATTR != 0 {
            return Ok(());
        }
        self.update_superblock(|sb| {
            let compat = le32(sb, 92);
            if le32(sb, 76) >= 1 {
                put32(sb, 92, compat | super::COMPAT_EXT_ATTR);
            }
        })
    }

    /// The in-inode attributes of `ino` (none in 128-byte inodes).
    pub(super) fn inline_xattrs(&self, ino: u32, out: &mut Vec<(String, Vec<u8>)>) -> KResult<()> {
        if self.inode_size <= 128 {
            return Ok(());
        }
        let raw = self.read_inode_raw(ino)?;
        let extra = &raw[128..];
        let isize = le16(extra, 0) as usize;
        if isize.is_multiple_of(4) && isize + 8 <= extra.len() && le32(extra, isize) == xattr::XATTR_MAGIC {
            // (Values are at offsets from the first entry.)
            xattr::parse(extra, isize + 4, isize + 4, out)?;
        }
        Ok(())
    }

    /// The in-inode attributes of `ino` go (they are in the block now).
    pub(super) fn clear_inline_xattrs(&self, ino: u32) -> KResult<()> {
        if self.inode_size <= 128 {
            return Ok(());
        }
        self.modify_inode_raw(ino, |raw| {
            let extra = &mut raw[128..];
            let isize = le16(extra, 0) as usize;
            if isize.is_multiple_of(4) && isize + 4 <= extra.len() && le32(extra, isize) == xattr::XATTR_MAGIC
            {
                extra[isize..].fill(0);
            }
        })
    }
}
