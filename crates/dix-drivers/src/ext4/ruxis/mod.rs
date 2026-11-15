//! ext4, and ext2 and ext3 with it, read and write: one driver for the
//! three, as Linux's ext4 driver mounts them all (`mount -t ext2|ext3|ext4`
//! checks the features the way Linux does for that name; `auto` and the
//! root file system take the first of ext3, ext2, ext4 that fits).
//!
//! Supported (read and write): block sizes of 1, 2 and 4 KiB; revision 0
//! and 1; `filetype`, `sparse_super`, `large_file`, `meta_bg`, `flex_bg`,
//! `64bit`, `extent` (files of extents; new files get them), `huge_file`,
//! `dir_nlink`, `extra_isize` (nanosecond and creation times),
//! `uninit_bg`/`gdt_csum` and `metadata_csum` (every checksum is verified
//! on read and written on change), `csum_seed`, `largedir`, extended
//! attributes (in the inode and in blocks), and the journal (`has_journal`:
//! replayed at mount, metadata committed through it, see [`journal`]),
//! with the orphan list for files deleted while open, and hashed
//! directory indexes (`dir_index`, htree: kept up to date, see [`htree`]).
//! File systems with
//! read-only-compatible features this driver doesn't write (quota,
//! bigalloc, ...) are mounted read-only; ones with incompatible features
//! it can't read (inline data, encryption, casefolding, ...) are refused.
//!
//! Layout of the code:
//!
//! * [`Ext4Fs`] (here): the superblock and group descriptors, block and
//!   inode allocation, the inode cache, commits and the orphan list.
//!   Metadata blocks go through the buffer cache ([`BufCache`]), file
//!   contents through each inode's page cache.
//! * [`inode`]: the on-disk inode and [`inode::Ext4Inode`], the VFS side.
//! * [`extent`] and [`indirect`]: where a file's blocks are.
//! * [`dir`]: directory blocks; [`htree`]: their hashed indexes.
//! * [`journal`]: jbd2, the journal's format.
//! * [`xattr`]: attribute blocks; [`csum`]: the checksums.
//!
//! Locking: `Ext4Fs::lock` (a sleeping lock) serialises all changes of
//! metadata: allocation, block maps, directories, commits. The page cache
//! is used without it, and its fill callback takes it only to look up
//! block numbers. Inode fields are behind an `IrqLock` so `stat` never
//! blocks.
//!
//! The driver only uses the block layer, the buffer and page caches and
//! the VFS's [`Inode`] trait, so it could become a loadable driver.

mod csum;
mod dir;
mod extent;
mod htree;
mod indirect;
mod inode;
mod journal;
mod xattr;

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use self::inode::{DiskInode, Ext4Inode};
use crate::ext4::kernel::fs::vfs::Inode;
use crate::ext4::kernel::block::Disk;
use crate::ext4::kernel::block::cache::BufCache;
use crate::ext4::kernel::errno::{Errno, KResult};
use crate::ext4::kernel::sync::{Mutex, MutexGuard};
use crate::ext4::kernel::time;

pub const EXT4_MAGIC: u16 = 0xef53;
const ROOT_INO: u32 = 2;
/// Linux's EXT4_LINK_MAX: links a file (or subdirectories a directory)
/// may have.
const LINK_MAX: u16 = 65000;
const STATE_VALID: u16 = 1;

const COMPAT_HAS_JOURNAL: u32 = 0x0004;
const COMPAT_EXT_ATTR: u32 = 0x0008;
const COMPAT_SPARSE_SUPER2: u32 = 0x0200;

const INCOMPAT_FILETYPE: u32 = 0x0002;
const INCOMPAT_RECOVER: u32 = 0x0004;
const INCOMPAT_JOURNAL_DEV: u32 = 0x0008;
const INCOMPAT_META_BG: u32 = 0x0010;
const INCOMPAT_EXTENTS: u32 = 0x0040;
const INCOMPAT_64BIT: u32 = 0x0080;
const INCOMPAT_FLEX_BG: u32 = 0x0200;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_LARGEDIR: u32 = 0x4000;

const RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
const RO_COMPAT_LARGE_FILE: u32 = 0x0002;
const RO_COMPAT_BTREE_DIR: u32 = 0x0004;
const RO_COMPAT_HUGE_FILE: u32 = 0x0008;
const RO_COMPAT_GDT_CSUM: u32 = 0x0010;
const RO_COMPAT_DIR_NLINK: u32 = 0x0020;
const RO_COMPAT_EXTRA_ISIZE: u32 = 0x0040;
const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;

/// Incompatible features this driver reads and writes.
const INCOMPAT_SUPPORTED: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_RECOVER
    | INCOMPAT_META_BG
    | INCOMPAT_EXTENTS
    | INCOMPAT_64BIT
    | INCOMPAT_FLEX_BG
    | INCOMPAT_CSUM_SEED
    | INCOMPAT_LARGEDIR;
/// Read-only compatible features it writes correctly.
const RO_COMPAT_SUPPORTED: u32 = RO_COMPAT_SPARSE_SUPER
    | RO_COMPAT_LARGE_FILE
    | RO_COMPAT_BTREE_DIR
    | RO_COMPAT_HUGE_FILE
    | RO_COMPAT_GDT_CSUM
    | RO_COMPAT_DIR_NLINK
    | RO_COMPAT_EXTRA_ISIZE
    | RO_COMPAT_METADATA_CSUM;
/// What Linux allows when mounting as ext2 (and as ext3, with the
/// journal's recovery flag).
const EXT2_INCOMPAT: u32 = INCOMPAT_FILETYPE | INCOMPAT_META_BG;
const EXT2_RO_COMPAT: u32 = RO_COMPAT_SPARSE_SUPER | RO_COMPAT_LARGE_FILE | RO_COMPAT_BTREE_DIR;

/// Group descriptor flags.
const BG_INODE_UNINIT: u16 = 1;
const BG_BLOCK_UNINIT: u16 = 2;

// Superblock fields.
const SB_FREE_BLOCKS: usize = 12;
const SB_FREE_INODES: usize = 16;
const SB_STATE: usize = 58;
const SB_INCOMPAT: usize = 96;
const SB_LAST_ORPHAN: usize = 0xE8;
const SB_FREE_BLOCKS_HI: usize = 0x158;
const SB_CHECKSUM: usize = 0x3FC;

pub(super) fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

pub(super) fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

pub(super) fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

pub(super) fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn now_secs() -> u32 {
    time::realtime_secs() as u32
}

/// Report metadata corruption, as Linux's `ext4_error`, and fail with
/// `err`.
fn corrupt<T>(dev: &str, what: core::fmt::Arguments<'_>, err: Errno) -> KResult<T> {
    kinfo!("EXT4-fs error (device {dev}): {what}");
    Err(err)
}

#[derive(Clone, Copy, Default)]
struct Group {
    block_bitmap: u64,
    inode_bitmap: u64,
    inode_table: u64,
    free_blocks: u32,
    free_inodes: u32,
    used_dirs: u32,
    flags: u16,
    itable_unused: u32,
    block_bitmap_csum: u32,
    inode_bitmap_csum: u32,
}

impl Group {
    fn decode(d: &[u8], wide: bool) -> Group {
        let hi32 = |o: usize| if wide { (le32(d, o) as u64) << 32 } else { 0 };
        let hi16 = |o: usize| if wide { (le16(d, o) as u32) << 16 } else { 0 };
        Group {
            block_bitmap: le32(d, 0) as u64 | hi32(32),
            inode_bitmap: le32(d, 4) as u64 | hi32(36),
            inode_table: le32(d, 8) as u64 | hi32(40),
            free_blocks: le16(d, 12) as u32 | hi16(44),
            free_inodes: le16(d, 14) as u32 | hi16(46),
            used_dirs: le16(d, 16) as u32 | hi16(48),
            flags: le16(d, 18),
            itable_unused: le16(d, 28) as u32 | hi16(50),
            block_bitmap_csum: le16(d, 24) as u32 | hi16(56),
            inode_bitmap_csum: le16(d, 26) as u32 | hi16(58),
        }
    }
}

/// What the big lock protects.
pub(super) struct Meta {
    groups: Vec<Group>,
    free_blocks: u64,
    free_inodes: u32,
    /// Per group: where the last block allocation ended. The next one
    /// starts looking there, so that a file written sequentially gets
    /// consecutive blocks (and nobody rescans the full start of a bitmap).
    block_hint: Vec<u32>,
    /// Per group: bitmaps whose checksum was verified (1: blocks, 2:
    /// inodes).
    checked: Vec<u8>,
    /// In-memory indexes of the directories in use, by inode number.
    dirs: BTreeMap<u32, dir::DirIndex>,
}

pub struct Ext4Fs {
    me: Weak<Ext4Fs>,
    cache: BufCache,
    dev: u64,
    /// The disk's name, for messages.
    disk_name: alloc::string::String,
    block_size: usize,
    blocks_count: u64,
    /// Blocks only root may use (`s_r_blocks_count`).
    reserved_blocks: u64,
    inodes_count: u32,
    blocks_per_group: u32,
    inodes_per_group: u32,
    first_data_block: u32,
    inode_size: usize,
    first_ino: u32,
    desc_size: usize,
    /// Group descriptors per block.
    desc_per_block: usize,
    /// Group descriptor blocks before the first `meta_bg` group.
    gdt_blocks: u64,
    reserved_gdt_blocks: u64,
    first_meta_bg: u64,
    /// Inode table blocks per group.
    itable_blocks: u64,
    /// Each group's inode table (fixed for the life of the file system).
    inode_tables: Vec<u64>,
    compat: u32,
    incompat: u32,
    ro_compat: u32,
    backup_bgs: [u32; 2],
    /// `metadata_csum`, and its seed.
    csum: bool,
    csum_seed: u32,
    /// `gdt_csum` (or `metadata_csum`): group descriptors checksummed,
    /// groups may be uninitialised.
    group_csum: bool,
    uuid: [u8; 16],
    /// New inodes' `i_extra_isize`.
    want_extra_isize: u16,
    /// Directory hashing: the seed, the version new indexes get, and 3
    /// where characters hash unsigned (else 0).
    hash_seed: [u32; 4],
    def_hash_version: u8,
    hash_unsigned: u8,
    read_only: bool,
    /// Metadata blocks (statfs leaves them out of the size).
    overhead: u64,
    journal: Option<journal::Journal>,
    lock: Mutex<Meta>,
    /// Free counts, readable without the lock (statfs).
    free_blocks: AtomicU64,
    free_inodes: AtomicU64,
    /// Every inode in use. Entries nobody else holds are dropped (and
    /// deleted inodes freed) by `sync_fs`.
    inodes: Mutex<BTreeMap<u32, Arc<Ext4Inode>>>,
    /// Held while syncing (which holds extra inode references) or
    /// unmounting (which checks that nobody else holds any).
    sync_lock: Mutex<()>,
    /// The test of a crash ([`simulate_crash`]): the next commit stops
    /// once logged, and nothing is written after it.
    crashing: AtomicBool,
    dead: AtomicBool,
}

/// For the self tests: commit what has changed in the file system of
/// `root` to the journal only and stop writing, as if the machine had
/// gone down between the commit and the checkpoint. The next mount must
/// replay it (and finish deleting the orphans).
pub fn simulate_crash(root: &Arc<dyn Inode>) -> KResult<()> {
    let inode = root.as_any().downcast_ref::<Ext4Inode>().ok_or(Errno::EINVAL)?;
    let fs = inode.fs();
    if fs.journal.is_none() {
        return Err(Errno::EINVAL);
    }
    let inodes: Vec<Arc<Ext4Inode>> = fs.inodes.lock().values().cloned().collect();
    for i in &inodes {
        i.writeback()?;
    }
    fs.crashing.store(true, Ordering::Relaxed);
    let mut meta = fs.lock.lock();
    fs.commit(&mut meta)
}

/// Can a file system with these features be mounted as `fstype` (Linux's
/// checks: ext2 and ext3 know fewer features, ext3 needs a journal)?
/// Unknown read-only-compatible features matter only for writing.
fn type_fits(fstype: &str, compat: u32, incompat: u32, ro_compat: u32, read_only: bool) -> bool {
    match fstype {
        "ext2" => incompat & !EXT2_INCOMPAT == 0 && (read_only || ro_compat & !EXT2_RO_COMPAT == 0),
        "ext3" => {
            compat & COMPAT_HAS_JOURNAL != 0
                && incompat & !(EXT2_INCOMPAT | INCOMPAT_RECOVER) == 0
                && (read_only || ro_compat & !EXT2_RO_COMPAT == 0)
        }
        _ => incompat & !INCOMPAT_SUPPORTED == 0,
    }
}

/// Mount the file system on `disk` as `fstype` (ext2, ext3, ext4, or
/// auto); returns its root directory and the type it is mounted as.
pub fn mount(disk: Arc<Disk>, read_only: bool, fstype: &str) -> KResult<(Arc<dyn Inode>, &'static str)> {
    let mut sb = [0u8; 1024];
    disk.read_bytes(1024, &mut sb)?;
    if le16(&sb, 56) != EXT4_MAGIC {
        return Err(Errno::EINVAL);
    }
    let dev = disk.name.clone();
    let (compat, incompat, ro_compat) =
        if le32(&sb, 76) == 0 { (0, 0, 0) } else { (le32(&sb, 92), le32(&sb, 96), le32(&sb, 100)) };
    let name: &'static str = match fstype {
        "auto" => ["ext3", "ext2", "ext4"]
            .into_iter()
            .find(|t| type_fits(t, compat, incompat, ro_compat, read_only))
            .ok_or(Errno::EINVAL)?,
        "ext2" => "ext2",
        "ext3" => "ext3",
        _ => "ext4",
    };
    if !type_fits(name, compat, incompat, ro_compat, read_only) || incompat & !INCOMPAT_SUPPORTED != 0 {
        kinfo!(
            "EXT4-fs ({dev}): couldn't mount as {name} due to feature incompatibilities ({:#x})",
            incompat & !INCOMPAT_SUPPORTED
        );
        return Err(Errno::EINVAL);
    }
    if incompat & INCOMPAT_JOURNAL_DEV != 0 || (compat & COMPAT_HAS_JOURNAL != 0 && le32(&sb, 0xE0) == 0) {
        kinfo!("EXT4-fs ({dev}): external journals are not supported");
        return Err(Errno::EINVAL);
    }
    if ro_compat & RO_COMPAT_METADATA_CSUM != 0 {
        if sb[0x175] != 1 {
            kinfo!("EXT4-fs ({dev}): unknown checksum algorithm {}", sb[0x175]);
            return Err(Errno::EINVAL);
        }
        if csum::crc32c(!0, &sb[..SB_CHECKSUM]) != le32(&sb, SB_CHECKSUM) {
            kinfo!("EXT4-fs ({dev}): VFS: Found ext4 filesystem with invalid superblock checksum");
            return Err(Errno::EBADMSG);
        }
    }
    // A journal with transactions to replay: replay them first (even for
    // a read-only mount, as Linux does), then read the superblock again.
    let recovered = compat & COMPAT_HAS_JOURNAL != 0 && incompat & INCOMPAT_RECOVER != 0;
    if compat & COMPAT_HAS_JOURNAL != 0 {
        journal::recover(&disk, &sb)?;
        disk.read_bytes(1024, &mut sb)?;
    }
    let fs = Ext4Fs::new(disk, sb, read_only)?;
    if fs.read_only {
        // Recovered for a read-only mount: nothing to recover any more
        // (Linux's ext4_mark_recovery_complete).
        if recovered {
            fs.update_superblock(|sb| put32(sb, SB_INCOMPAT, le32(sb, SB_INCOMPAT) & !INCOMPAT_RECOVER))?;
            fs.cache.sync()?;
            fs.cache.disk().dev.flush()?;
        }
    } else {
        // Mounted: not clean until unmounted (with a journal: the journal
        // must be looked at, as Linux marks it).
        let journaled = fs.journal.is_some();
        fs.update_superblock(|sb| {
            if journaled {
                put32(sb, SB_INCOMPAT, le32(sb, SB_INCOMPAT) | INCOMPAT_RECOVER);
            } else {
                put16(sb, SB_STATE, le16(sb, SB_STATE) & !STATE_VALID);
            }
            put16(sb, 52, le16(sb, 52).wrapping_add(1));
            put32(sb, 44, now_secs());
        })?;
        let mut meta = fs.lock.lock();
        fs.commit(&mut meta)?;
        drop(meta);
        fs.orphan_cleanup()?;
    }
    kinfo!(
        "{name}: {}: {} blocks of {} bytes, {} inodes{}{}",
        fs.disk_name,
        fs.blocks_count,
        fs.block_size,
        fs.inodes_count,
        if fs.journal.is_some() { ", journal" } else { "" },
        if fs.read_only { ", read-only" } else { "" }
    );
    Ok((fs.iget(ROOT_INO)?, name))
}

impl Ext4Fs {
    fn new(disk: Arc<Disk>, sb: [u8; 1024], read_only: bool) -> KResult<Arc<Ext4Fs>> {
        let dev = disk.name.clone();
        let log = le32(&sb, 24);
        if log > 2 {
            kinfo!("EXT4-fs ({dev}): block size {} larger than a page", 1024 << log.min(16));
            return Err(Errno::EINVAL);
        }
        let block_size = 1024usize << log;
        let rev = le32(&sb, 76);
        let (inode_size, first_ino, compat, incompat, ro_compat) = if rev == 0 {
            (128, 11, 0, 0, 0)
        } else {
            (le16(&sb, 88) as usize, le32(&sb, 84), le32(&sb, 92), le32(&sb, 96), le32(&sb, 100))
        };
        if !(128..=block_size).contains(&inode_size) || !inode_size.is_power_of_two() {
            return Err(Errno::EINVAL);
        }
        let read_only = read_only || ro_compat & !RO_COMPAT_SUPPORTED != 0;
        if ro_compat & !RO_COMPAT_SUPPORTED != 0 {
            kinfo!(
                "EXT4-fs ({dev}): read-only: features {:#x} are not written here",
                ro_compat & !RO_COMPAT_SUPPORTED
            );
        }
        let wide = incompat & INCOMPAT_64BIT != 0;
        let blocks_count = le32(&sb, 4) as u64 | if wide { (le32(&sb, 0x150) as u64) << 32 } else { 0 };
        let reserved_blocks = le32(&sb, 8) as u64 | if wide { (le32(&sb, 0x154) as u64) << 32 } else { 0 };
        let inodes_count = le32(&sb, 0);
        let first_data_block = le32(&sb, 20);
        let blocks_per_group = le32(&sb, 32);
        let inodes_per_group = le32(&sb, 40);
        let desc_size = if wide { le16(&sb, 0xFE) as usize } else { 32 };
        if blocks_per_group == 0
            || inodes_per_group == 0
            || blocks_per_group > 8 * block_size as u32
            || inodes_per_group > 8 * block_size as u32
            || !(32..=block_size).contains(&desc_size)
            || !desc_size.is_power_of_two()
            || blocks_count <= first_data_block as u64
        {
            return Err(Errno::EINVAL);
        }
        let ngroups = (blocks_count - first_data_block as u64).div_ceil(blocks_per_group as u64) as usize;
        if (ngroups as u64) * inodes_per_group as u64 != inodes_count as u64 {
            kinfo!("EXT4-fs ({dev}): inode count doesn't match the groups");
            return Err(Errno::EINVAL);
        }
        let csum = ro_compat & RO_COMPAT_METADATA_CSUM != 0;
        let uuid: [u8; 16] = sb[104..120].try_into().unwrap();
        let csum_seed =
            if incompat & INCOMPAT_CSUM_SEED != 0 { le32(&sb, 0x270) } else { csum::crc32c(!0, &uuid) };
        let desc_per_block = block_size / desc_size;
        let meta_bg = incompat & INCOMPAT_META_BG != 0;
        let first_meta_bg = if meta_bg { le32(&sb, 0x104) as u64 } else { u64::MAX };
        let gdt_blocks = (ngroups as u64).div_ceil(desc_per_block as u64).min(first_meta_bg);
        let want_extra_isize =
            if inode_size > 128 { le16(&sb, 0x15E).max(32).min(inode_size as u16 - 128) } else { 0 };
        let mut fs = Ext4Fs {
            me: Weak::new(),
            cache: BufCache::new(disk.clone(), block_size),
            dev: disk.rdev,
            disk_name: dev.clone(),
            block_size,
            blocks_count,
            reserved_blocks,
            inodes_count,
            blocks_per_group,
            inodes_per_group,
            first_data_block,
            inode_size,
            first_ino,
            desc_size,
            desc_per_block,
            gdt_blocks,
            reserved_gdt_blocks: le16(&sb, 0xCE) as u64,
            first_meta_bg,
            itable_blocks: (inodes_per_group as u64 * inode_size as u64).div_ceil(block_size as u64),
            inode_tables: Vec::new(),
            compat,
            incompat,
            ro_compat,
            backup_bgs: [le32(&sb, 0x24C), le32(&sb, 0x250)],
            csum,
            csum_seed,
            group_csum: csum || ro_compat & RO_COMPAT_GDT_CSUM != 0,
            uuid,
            want_extra_isize,
            hash_seed: core::array::from_fn(|i| le32(&sb, 0xEC + 4 * i)),
            def_hash_version: sb[0xFC],
            hash_unsigned: match le32(&sb, 0x160) & 3 {
                2 => 3,
                1 => 0,
                // Neither said: what the CPU's char is (Linux records it).
                _ => {
                    if cfg!(target_arch = "aarch64") {
                        3
                    } else {
                        0
                    }
                }
            },
            read_only,
            overhead: 0,
            journal: None,
            lock: Mutex::new(Meta {
                groups: Vec::new(),
                free_blocks: 0,
                free_inodes: 0,
                block_hint: alloc::vec![0; ngroups],
                checked: alloc::vec![0; ngroups],
                dirs: BTreeMap::new(),
            }),
            free_blocks: AtomicU64::new(0),
            free_inodes: AtomicU64::new(0),
            inodes: Mutex::new(BTreeMap::new()),
            sync_lock: Mutex::new(()),
            crashing: AtomicBool::new(false),
            dead: AtomicBool::new(false),
        };
        // The group descriptors, their checksums checked (as Linux's
        // ext4_check_descriptors: a bad one fails a writable mount).
        let mut groups = Vec::with_capacity(ngroups);
        for g in 0..ngroups {
            let (block, off) = fs.desc_location(g);
            let d = fs.cache.read(block, |b| b[off..off + desc_size].to_vec())?;
            let gd = Group::decode(&d, desc_size >= 64);
            let in_fs = |b: u64| b >= first_data_block as u64 && b < blocks_count;
            if !in_fs(gd.block_bitmap) || !in_fs(gd.inode_bitmap) || !in_fs(gd.inode_table) {
                return corrupt(
                    &dev,
                    format_args!("group {g}: metadata outside the file system"),
                    Errno::EUCLEAN,
                );
            }
            if fs.group_csum && fs.desc_csum(g, &d) != le16(&d, 30) {
                kinfo!("EXT4-fs ({dev}): ext4_check_descriptors: Checksum for group {g} failed");
                if !fs.read_only {
                    return Err(Errno::EBADMSG);
                }
            }
            groups.push(gd);
        }
        let free_blocks = groups.iter().map(|g| g.free_blocks as u64).sum();
        let free_inodes = groups.iter().map(|g| g.free_inodes).sum();
        fs.free_blocks.store(free_blocks, Ordering::Relaxed);
        fs.free_inodes.store(free_inodes as u64, Ordering::Relaxed);
        fs.inode_tables = groups.iter().map(|g| g.inode_table).collect();
        {
            let mut meta = fs.lock.lock();
            meta.groups = groups;
            meta.free_blocks = free_blocks;
            meta.free_inodes = free_inodes;
        }
        fs.overhead = match le32(&sb, 0x248) {
            0 => fs.compute_overhead(&sb),
            n => n as u64,
        };
        if compat & COMPAT_HAS_JOURNAL != 0 && !read_only {
            fs.journal = Some(journal::Journal::open(&fs, &sb)?);
            fs.cache.hold_dirty(true);
        }
        Ok(Arc::new_cyclic(|me| {
            fs.me = me.clone();
            fs
        }))
    }

    fn arc(&self) -> Arc<Ext4Fs> {
        self.me.upgrade().expect("ext4: file system gone")
    }

    fn has_incompat(&self, f: u32) -> bool {
        self.incompat & f != 0
    }

    fn sectors_per_block(&self) -> u64 {
        (self.block_size / 512) as u64
    }

    fn writable(&self) -> KResult<()> {
        if self.read_only { Err(Errno::EROFS) } else { Ok(()) }
    }

    /// Lock the metadata for a change. With a journal, what is waiting to
    /// be committed is committed first if a transaction could otherwise
    /// outgrow the journal.
    fn begin(&self) -> KResult<MutexGuard<'_, Meta>> {
        let mut meta = self.lock.lock();
        if let Some(j) = &self.journal
            && self.cache.dirty_count() > j.limit()
        {
            self.commit(&mut meta)?;
        }
        Ok(meta)
    }

    /// Make the changed metadata durable: through the journal (one
    /// transaction, then written in place), or written in place.
    fn commit(&self, _meta: &mut Meta) -> KResult<()> {
        if self.read_only || self.dead.load(Ordering::Relaxed) {
            return Ok(());
        }
        match &self.journal {
            Some(j) if self.crashing.load(Ordering::Relaxed) => {
                j.log_only(self)?;
                self.dead.store(true, Ordering::Relaxed);
                kinfo!(
                    "EXT4-fs ({}): crash simulated: transaction logged, not written in place",
                    self.disk_name
                );
                Ok(())
            }
            Some(j) => j.commit(self),
            None => {
                self.cache.sync()?;
                self.cache.disk().dev.flush()
            }
        }
    }

    /// Change the primary superblock (in the buffer cache), and its
    /// checksum.
    fn update_superblock(&self, f: impl FnOnce(&mut [u8])) -> KResult<()> {
        let (block, off) = ((1024 / self.block_size) as u64, 1024 % self.block_size);
        let csum = self.csum;
        self.cache.modify(block, |b| {
            let sb = &mut b[off..off + 1024];
            f(sb);
            if csum {
                let c = csum::crc32c(!0, &sb[..SB_CHECKSUM]);
                put32(sb, SB_CHECKSUM, c);
            }
        })
    }

    fn read_superblock<R>(&self, f: impl FnOnce(&[u8]) -> R) -> KResult<R> {
        let (block, off) = ((1024 / self.block_size) as u64, 1024 % self.block_size);
        self.cache.read(block, |b| f(&b[off..off + 1024]))
    }

    // ----- groups ----------------------------------------------------------

    fn group_first_block(&self, g: usize) -> u64 {
        self.first_data_block as u64 + g as u64 * self.blocks_per_group as u64
    }

    fn group_blocks(&self, g: usize) -> u32 {
        (self.blocks_count - self.group_first_block(g)).min(self.blocks_per_group as u64) as u32
    }

    fn group_of_block(&self, block: u64) -> usize {
        ((block - self.first_data_block as u64) / self.blocks_per_group as u64) as usize
    }

    fn group_of(&self, ino: u32) -> usize {
        ((ino - 1) / self.inodes_per_group) as usize
    }

    /// Does group `g` hold a backup of the superblock (and group
    /// descriptors)?
    fn has_super(&self, g: usize) -> bool {
        let g = g as u32;
        if g == 0 {
            return true;
        }
        if self.compat & COMPAT_SPARSE_SUPER2 != 0 {
            return g == self.backup_bgs[0] || g == self.backup_bgs[1];
        }
        if g <= 1 || self.ro_compat & RO_COMPAT_SPARSE_SUPER == 0 {
            return true;
        }
        if g.is_multiple_of(2) {
            return false;
        }
        [3u32, 5, 7].iter().any(|&p| {
            let mut n = g;
            while n.is_multiple_of(p) {
                n /= p;
            }
            n == 1
        })
    }

    /// Where group `g`'s descriptor is: (block, offset). With `meta_bg`,
    /// each block of descriptors is in the first group it describes
    /// (after a superblock backup), Linux's descriptor_loc.
    fn desc_location(&self, g: usize) -> (u64, usize) {
        let dpb = self.desc_per_block;
        let off = (g % dpb) * self.desc_size;
        let index = (g / dpb) as u64;
        if index < self.first_meta_bg {
            return (self.first_data_block as u64 + 1 + index, off);
        }
        let first = index as usize * dpb;
        (self.group_first_block(first) + self.has_super(first) as u64, off)
    }

    /// The superblock and descriptor blocks at the start of group `g`
    /// (Linux's ext4_num_base_meta_blocks).
    fn base_meta_blocks(&self, g: usize) -> u64 {
        let mut n = self.has_super(g) as u64;
        let dpb = self.desc_per_block;
        if (g / dpb) as u64 >= self.first_meta_bg {
            let first = g / dpb * dpb;
            if g == first || g == first + 1 || g == first + dpb - 1 {
                n += 1;
            }
        } else if n != 0 {
            n += self.gdt_blocks + self.reserved_gdt_blocks;
        }
        n
    }

    /// All metadata blocks: superblocks, descriptors, bitmaps, inode
    /// tables, the journal (what Linux's statfs leaves out).
    fn compute_overhead(&self, sb: &[u8]) -> u64 {
        let ngroups = self.lock_groups_len();
        let mut n = self.first_data_block as u64;
        for g in 0..ngroups {
            n += self.base_meta_blocks(g) + 2 + self.itable_blocks;
        }
        if self.compat & COMPAT_HAS_JOURNAL != 0 {
            // The journal's size: its inode's i_size (the backup copy in
            // the superblock: s_jnl_blocks[15] is i_size_high, [16] i_size).
            n += (le32(sb, 0x10C + 16 * 4) as u64 | (le32(sb, 0x10C + 15 * 4) as u64) << 32)
                / self.block_size as u64;
        }
        n
    }

    fn lock_groups_len(&self) -> usize {
        (self.blocks_count - self.first_data_block as u64).div_ceil(self.blocks_per_group as u64) as usize
    }

    /// Group descriptor `d` of group `g`'s checksum (crc32c with
    /// `metadata_csum`, else crc16).
    fn desc_csum(&self, g: usize, d: &[u8]) -> u16 {
        let ds = self.desc_size;
        let group = (g as u32).to_le_bytes();
        if self.csum {
            let c = csum::crc32c(self.csum_seed, &group);
            let c = csum::crc32c(c, &d[..30]);
            let mut c = csum::crc32c(c, &[0, 0]);
            if ds > 32 {
                c = csum::crc32c(c, &d[32..ds]);
            }
            c as u16
        } else {
            let c = csum::crc16(!0, &self.uuid);
            let c = csum::crc16(c, &group);
            let mut c = csum::crc16(c, &d[..30]);
            if ds > 32 {
                c = csum::crc16(c, &d[32..ds]);
            }
            c
        }
    }

    /// The checksum of a bitmap's first `bits` bits.
    fn bitmap_csum(&self, block: u64, bits: u32) -> KResult<u32> {
        let n = (bits / 8) as usize;
        self.cache.read(block, |b| csum::crc32c(self.csum_seed, &b[..n]))
    }

    /// Write group descriptor `g` (with its bitmaps' checksums and its
    /// own) and the superblock's free counts.
    fn write_group(&self, meta: &mut Meta, g: usize) -> KResult<()> {
        let mut gd = meta.groups[g];
        if self.csum {
            if gd.flags & BG_BLOCK_UNINIT == 0 {
                gd.block_bitmap_csum = self.bitmap_csum(gd.block_bitmap, self.blocks_per_group)?;
            }
            if gd.flags & BG_INODE_UNINIT == 0 {
                gd.inode_bitmap_csum = self.bitmap_csum(gd.inode_bitmap, self.inodes_per_group)?;
            }
            meta.groups[g] = gd;
        }
        let (block, off) = self.desc_location(g);
        let ds = self.desc_size;
        let wide = ds >= 64;
        let (csum, group_csum) = (self.csum, self.group_csum);
        let sum = self.cache.modify(block, |b| {
            let d = &mut b[off..off + ds];
            put16(d, 12, gd.free_blocks as u16);
            put16(d, 14, gd.free_inodes as u16);
            put16(d, 16, gd.used_dirs as u16);
            put16(d, 18, gd.flags);
            put16(d, 28, gd.itable_unused as u16);
            if csum {
                put16(d, 24, gd.block_bitmap_csum as u16);
                put16(d, 26, gd.inode_bitmap_csum as u16);
            }
            if wide {
                put16(d, 44, (gd.free_blocks >> 16) as u16);
                put16(d, 46, (gd.free_inodes >> 16) as u16);
                put16(d, 48, (gd.used_dirs >> 16) as u16);
                put16(d, 50, (gd.itable_unused >> 16) as u16);
                if csum {
                    put16(d, 56, (gd.block_bitmap_csum >> 16) as u16);
                    put16(d, 58, (gd.inode_bitmap_csum >> 16) as u16);
                }
            }
            d.to_vec()
        })?;
        if group_csum {
            let c = self.desc_csum(g, &sum);
            self.cache.modify(block, |b| put16(&mut b[off..], 30, c))?;
        }
        let (fb, fi) = (meta.free_blocks, meta.free_inodes);
        self.free_blocks.store(fb, Ordering::Relaxed);
        self.free_inodes.store(fi as u64, Ordering::Relaxed);
        let wide_sb = self.has_incompat(INCOMPAT_64BIT);
        self.update_superblock(|sb| {
            put32(sb, SB_FREE_BLOCKS, fb as u32);
            if wide_sb {
                put32(sb, SB_FREE_BLOCKS_HI, (fb >> 32) as u32);
            }
            put32(sb, SB_FREE_INODES, fi);
        })
    }

    /// Group `g`'s block bitmap, ready for use: an uninitialised one
    /// (`BLOCK_UNINIT`) is built (its metadata blocks in use, as Linux's
    /// ext4_init_block_bitmap), an initialised one's checksum checked once.
    fn block_bitmap(&self, meta: &mut Meta, g: usize) -> KResult<u64> {
        let gd = meta.groups[g];
        if gd.flags & BG_BLOCK_UNINIT != 0 && self.group_csum {
            let start = self.group_first_block(g);
            let mut used = Vec::new();
            for i in 0..self.base_meta_blocks(g) {
                used.push(i);
            }
            let flex = self.has_incompat(INCOMPAT_FLEX_BG);
            let mut mine = |b: u64| {
                if !flex || (b >= start && b < start + self.group_blocks(g) as u64) {
                    used.push(b - start);
                }
            };
            mine(gd.block_bitmap);
            mine(gd.inode_bitmap);
            for t in 0..self.itable_blocks {
                mine(gd.inode_table + t);
            }
            let (end, bits) = (self.group_blocks(g) as u64, 8 * self.block_size as u64);
            self.cache.zero(gd.block_bitmap)?;
            self.cache.modify(gd.block_bitmap, |b| {
                for &i in used.iter().chain(&(end..bits).collect::<Vec<u64>>()) {
                    b[(i / 8) as usize] |= 1 << (i % 8);
                }
            })?;
            meta.groups[g].flags &= !BG_BLOCK_UNINIT;
            meta.checked[g] |= 1;
            self.write_group(meta, g)?;
        } else if self.csum && meta.checked[g] & 1 == 0 {
            let c = self.bitmap_csum(gd.block_bitmap, self.blocks_per_group)?;
            let want =
                if self.desc_size >= 64 { gd.block_bitmap_csum } else { gd.block_bitmap_csum & 0xffff };
            let got = if self.desc_size >= 64 { c } else { c & 0xffff };
            if got != want {
                return corrupt(
                    &self.disk_name,
                    format_args!("group {g}: bad block bitmap checksum"),
                    Errno::EBADMSG,
                );
            }
            meta.checked[g] |= 1;
        }
        Ok(meta.groups[g].block_bitmap)
    }

    /// Group `g`'s inode bitmap, ready for use (as [`Self::block_bitmap`]).
    fn inode_bitmap(&self, meta: &mut Meta, g: usize) -> KResult<u64> {
        let gd = meta.groups[g];
        if gd.flags & BG_INODE_UNINIT != 0 && self.group_csum {
            let (ipg, bits) = (self.inodes_per_group as usize, 8 * self.block_size);
            self.cache.zero(gd.inode_bitmap)?;
            self.cache.modify(gd.inode_bitmap, |b| {
                for i in ipg..bits {
                    b[i / 8] |= 1 << (i % 8);
                }
            })?;
            meta.groups[g].flags &= !BG_INODE_UNINIT;
            meta.checked[g] |= 2;
            self.write_group(meta, g)?;
        } else if self.csum && meta.checked[g] & 2 == 0 {
            let c = self.bitmap_csum(gd.inode_bitmap, self.inodes_per_group)?;
            let want =
                if self.desc_size >= 64 { gd.inode_bitmap_csum } else { gd.inode_bitmap_csum & 0xffff };
            let got = if self.desc_size >= 64 { c } else { c & 0xffff };
            if got != want {
                return corrupt(
                    &self.disk_name,
                    format_args!("group {g}: bad inode bitmap checksum"),
                    Errno::EBADMSG,
                );
            }
            meta.checked[g] |= 2;
        }
        Ok(meta.groups[g].inode_bitmap)
    }

    // ----- allocation ------------------------------------------------------

    /// Find and set a clear bit among the first `bits` of bitmap block
    /// `block`, looking from bit `start` on (then from the beginning).
    /// Full bytes are skipped whole.
    fn take_bit(&self, block: u64, bits: u32, start: u32) -> KResult<Option<u32>> {
        self.cache.modify(block, |b| {
            let start = if start < bits { start } else { 0 };
            for (from, to) in [(start, bits), (0, start)] {
                let mut i = from;
                while i < to {
                    let byte = (i / 8) as usize;
                    if i.is_multiple_of(8) && b[byte] == 0xff {
                        i += 8;
                        continue;
                    }
                    let mask = 1u8 << (i % 8);
                    if b[byte] & mask == 0 {
                        b[byte] |= mask;
                        return Some(i);
                    }
                    i += 1;
                }
            }
            None
        })
    }

    /// Set bit `bit` if it is clear.
    fn take_this_bit(&self, block: u64, bit: u32) -> KResult<bool> {
        self.cache.modify(block, |b| {
            let (byte, mask) = ((bit / 8) as usize, 1u8 << (bit % 8));
            let free = b[byte] & mask == 0;
            b[byte] |= mask;
            free
        })
    }

    /// Allocate a block: `near` if it is free, else in group `goal` or
    /// after. The blocks reserved for root (`s_r_blocks_count`) are left
    /// to callers with CAP_SYS_RESOURCE, as on Linux.
    fn alloc_block(&self, meta: &mut Meta, goal: usize, near: Option<u64>) -> KResult<u64> {
        const CAP_SYS_RESOURCE: u32 = 24;
        if meta.free_blocks <= self.reserved_blocks && !crate::ext4::kernel::process::capable(CAP_SYS_RESOURCE) {
            return Err(Errno::ENOSPC);
        }
        let n = meta.groups.len();
        let found = 'found: {
            if let Some(b) = near.filter(|&b| b >= self.first_data_block as u64 && b < self.blocks_count) {
                let g = self.group_of_block(b);
                if meta.groups[g].free_blocks > 0 {
                    let bitmap = self.block_bitmap(meta, g)?;
                    let bit = (b - self.group_first_block(g)) as u32;
                    if self.take_this_bit(bitmap, bit)? {
                        break 'found Some((g, bit));
                    }
                }
            }
            let goal = near.map_or(goal, |b| {
                self.group_of_block(b.clamp(self.first_data_block as u64, self.blocks_count - 1))
            });
            for i in 0..n {
                let g = (goal + i) % n;
                if meta.groups[g].free_blocks == 0 {
                    continue;
                }
                let bitmap = self.block_bitmap(meta, g)?;
                if let Some(bit) = self.take_bit(bitmap, self.group_blocks(g), meta.block_hint[g])? {
                    break 'found Some((g, bit));
                }
            }
            None
        };
        let (g, bit) = found.ok_or(Errno::ENOSPC)?;
        meta.block_hint[g] = bit + 1;
        meta.groups[g].free_blocks -= 1;
        meta.free_blocks -= 1;
        self.write_group(meta, g)?;
        let block = self.group_first_block(g) + bit as u64;
        // Never let a stale cached copy be written over it.
        self.cache.forget(block);
        Ok(block)
    }

    /// Free `count` blocks from `start`.
    fn free_blocks(&self, meta: &mut Meta, start: u64, count: u64) -> KResult<()> {
        if start < self.first_data_block as u64 || start + count > self.blocks_count || count == 0 {
            return corrupt(
                &self.disk_name,
                format_args!("freeing blocks {start}+{count} outside the file system"),
                Errno::EUCLEAN,
            );
        }
        let mut b = start;
        let end = start + count;
        while b < end {
            let g = self.group_of_block(b);
            let group_end = (self.group_first_block(g) + self.group_blocks(g) as u64).min(end);
            let bitmap = self.block_bitmap(meta, g)?;
            let first = (b - self.group_first_block(g)) as u32;
            let n = (group_end - b) as u32;
            let freed = self.cache.modify(bitmap, |m| {
                let mut freed = 0u32;
                for bit in first..first + n {
                    let (byte, mask) = ((bit / 8) as usize, 1u8 << (bit % 8));
                    if m[byte] & mask != 0 {
                        m[byte] &= !mask;
                        freed += 1;
                    }
                }
                freed
            })?;
            for blk in b..group_end {
                self.cache.forget(blk);
            }
            meta.groups[g].free_blocks += freed;
            meta.free_blocks += freed as u64;
            meta.block_hint[g] = meta.block_hint[g].min(first);
            self.write_group(meta, g)?;
            b = group_end;
        }
        Ok(())
    }

    /// Allocate an inode number: directories go to the group with the most
    /// free inodes, other files near their directory.
    fn alloc_inode(&self, meta: &mut Meta, dir: bool, parent_group: usize) -> KResult<u32> {
        let n = meta.groups.len();
        let order: Vec<usize> = if dir {
            let mut v: Vec<usize> = (0..n).collect();
            v.sort_by_key(|&g| core::cmp::Reverse(meta.groups[g].free_inodes));
            v
        } else {
            (0..n).map(|i| (parent_group + i) % n).collect()
        };
        for g in order {
            if meta.groups[g].free_inodes == 0 {
                continue;
            }
            let bitmap = self.inode_bitmap(meta, g)?;
            // (Linux initialises the group's block bitmap too.)
            if self.group_csum && meta.groups[g].flags & BG_BLOCK_UNINIT != 0 {
                self.block_bitmap(meta, g)?;
            }
            let Some(bit) = self.take_bit(bitmap, self.inodes_per_group, 0)? else { continue };
            let ino = g as u32 * self.inodes_per_group + bit + 1;
            if ino < self.first_ino {
                continue;
            }
            let gd = &mut meta.groups[g];
            gd.free_inodes -= 1;
            meta.free_inodes -= 1;
            if dir {
                gd.used_dirs += 1;
            }
            // The inode table is in use up to here (lazily zeroed tables:
            // Linux's itable_unused).
            if self.group_csum && bit >= self.inodes_per_group - gd.itable_unused {
                gd.itable_unused = self.inodes_per_group - bit - 1;
            }
            self.write_group(meta, g)?;
            // A fresh inode: all zeroes (including the extra fields).
            let (block, off) = self.inode_location(ino)?;
            let size = self.inode_size;
            self.cache.modify(block, |b| b[off..off + size].fill(0))?;
            return Ok(ino);
        }
        Err(Errno::ENOSPC)
    }

    fn free_inode(&self, meta: &mut Meta, ino: u32, dir: bool) -> KResult<()> {
        // (Its number may be a new directory's next.)
        meta.dirs.remove(&ino);
        let g = self.group_of(ino);
        let bitmap = self.inode_bitmap(meta, g)?;
        let bit = (ino - 1) % self.inodes_per_group;
        let was = self.cache.modify(bitmap, |b| {
            let (byte, mask) = ((bit / 8) as usize, 1u8 << (bit % 8));
            let was = b[byte] & mask != 0;
            b[byte] &= !mask;
            was
        })?;
        if was {
            meta.groups[g].free_inodes += 1;
            meta.free_inodes += 1;
            if dir {
                meta.groups[g].used_dirs = meta.groups[g].used_dirs.saturating_sub(1);
            }
            self.write_group(meta, g)?;
        }
        Ok(())
    }

    // ----- inodes ----------------------------------------------------------

    /// Where inode `ino` lives: (block, offset in the block).
    fn inode_location(&self, ino: u32) -> KResult<(u64, usize)> {
        if ino == 0 || ino > self.inodes_count {
            return corrupt(&self.disk_name, format_args!("inode {ino} out of range"), Errno::EUCLEAN);
        }
        let group = self.group_of(ino);
        let index = ((ino - 1) % self.inodes_per_group) as usize;
        let byte = index * self.inode_size;
        let table = self.inode_tables[group];
        Ok((table + (byte / self.block_size) as u64, byte % self.block_size))
    }

    /// The checksum seed of inode `ino` (its number and generation).
    fn inode_seed(&self, ino: u32, generation: u32) -> u32 {
        csum::crc32c(csum::crc32c(self.csum_seed, &ino.to_le_bytes()), &generation.to_le_bytes())
    }

    /// The checksum of on-disk inode `raw` (all `inode_size` bytes, the
    /// checksum fields counted as zero): Linux's ext4_inode_csum.
    fn inode_csum(&self, ino: u32, raw: &[u8]) -> u32 {
        let seed = self.inode_seed(ino, le32(raw, 100));
        let mut c = csum::crc32c(seed, &raw[..124]);
        c = csum::crc32c(c, &[0, 0]);
        c = csum::crc32c(c, &raw[126..128]);
        if self.inode_size > 128 {
            c = csum::crc32c(c, &raw[128..130]);
            let extra = le16(raw, 128) as usize;
            let from = if extra >= 4 {
                c = csum::crc32c(c, &[0, 0]);
                132
            } else {
                130
            };
            c = csum::crc32c(c, &raw[from..self.inode_size]);
        }
        c
    }

    fn set_inode_csum(&self, ino: u32, raw: &mut [u8]) {
        if !self.csum {
            return;
        }
        let c = self.inode_csum(ino, raw);
        put16(raw, 124, c as u16);
        if self.inode_size > 128 && le16(raw, 128) >= 4 {
            put16(raw, 130, (c >> 16) as u16);
        }
    }

    fn inode_csum_ok(&self, ino: u32, raw: &[u8]) -> bool {
        let c = self.inode_csum(ino, raw);
        if self.inode_size > 128 && le16(raw, 128) >= 4 {
            c == (le16(raw, 124) as u32 | (le16(raw, 130) as u32) << 16)
        } else {
            c as u16 == le16(raw, 124)
        }
    }

    /// The in-memory inode `ino`, read from the disk the first time.
    fn iget(&self, ino: u32) -> KResult<Arc<Ext4Inode>> {
        if let Some(i) = self.inodes.lock().get(&ino) {
            return Ok(i.clone());
        }
        let (block, off) = self.inode_location(ino)?;
        let raw = self.cache.read(block, |b| b[off..off + self.inode_size].to_vec())?;
        if self.csum && !self.inode_csum_ok(ino, &raw) {
            return corrupt(
                &self.disk_name,
                format_args!("ext4_lookup:{ino}: inode #{ino}: comm: iget: checksum invalid"),
                Errno::EBADMSG,
            );
        }
        let d = DiskInode::decode(&raw, self)?;
        let inode = Arc::new(Ext4Inode::new(self.arc(), ino, d));
        // Somebody may have read it meanwhile: there must be one copy.
        Ok(self.inodes.lock().entry(ino).or_insert(inode).clone())
    }

    fn write_inode(&self, ino: u32, d: &DiskInode) -> KResult<()> {
        let (block, off) = self.inode_location(ino)?;
        let size = self.inode_size;
        self.cache.modify(block, |b| {
            let raw = &mut b[off..off + size];
            // A file without names: its `i_dtime` is the orphan list's
            // link, which only the orphan list code (and deletion) sets, on
            // the disk; a copy of the inode taken before it changed must
            // not write it back.
            let dtime = le32(raw, 20);
            d.encode(raw, self);
            if d.links == 0 {
                put32(raw, 20, dtime);
            }
            self.set_inode_csum(ino, raw);
        })
    }

    /// Change the bytes of inode `ino` on the disk (in-inode attributes),
    /// and its checksum.
    fn modify_inode_raw<R>(&self, ino: u32, f: impl FnOnce(&mut [u8]) -> R) -> KResult<R> {
        let (block, off) = self.inode_location(ino)?;
        let size = self.inode_size;
        self.cache.modify(block, |b| {
            let raw = &mut b[off..off + size];
            let r = f(raw);
            self.set_inode_csum(ino, raw);
            r
        })
    }

    fn read_inode_raw(&self, ino: u32) -> KResult<Vec<u8>> {
        let (block, off) = self.inode_location(ino)?;
        self.cache.read(block, |b| b[off..off + self.inode_size].to_vec())
    }

    // ----- the orphan list -------------------------------------------------
    //
    // A file without names that is still in use is on the orphan list
    // (`s_last_orphan`, then each inode's `i_dtime`) until it is deleted,
    // so that it is deleted at the next mount (or by e2fsck) after a
    // crash, as Linux keeps it with a journal.

    /// Put inode `ino` (links now 0) on the orphan list.
    fn orphan_add(&self, ino: u32, d: &mut DiskInode) -> KResult<()> {
        if self.journal.is_none() {
            return Ok(());
        }
        let head = self.read_superblock(|sb| le32(sb, SB_LAST_ORPHAN))?;
        if head == ino {
            return Ok(());
        }
        d.dtime = head;
        self.modify_inode_raw(ino, |raw| put32(raw, 20, head))?;
        self.update_superblock(|sb| put32(sb, SB_LAST_ORPHAN, ino))
    }

    /// The orphan after `ino` (from the disk, where the list always is:
    /// an inode on its way out may be in memory no more, or stale).
    fn orphan_next(&self, ino: u32) -> KResult<u32> {
        Ok(le32(&self.read_inode_raw(ino)?, 20))
    }

    fn set_orphan_next(&self, ino: u32, next: u32) -> KResult<()> {
        let cached = self.inodes.lock().get(&ino).cloned();
        if let Some(i) = cached {
            let mut d = i.disk();
            d.dtime = next;
            i.store(d);
        }
        self.modify_inode_raw(ino, |raw| put32(raw, 20, next))
    }

    /// Take inode `ino` off the orphan list (if it is on it).
    fn orphan_del(&self, ino: u32) -> KResult<()> {
        if self.journal.is_none() {
            return Ok(());
        }
        let head = self.read_superblock(|sb| le32(sb, SB_LAST_ORPHAN))?;
        if head == 0 {
            return Ok(());
        }
        let next = self.orphan_next(ino)?;
        if head == ino {
            return self.update_superblock(|sb| put32(sb, SB_LAST_ORPHAN, next));
        }
        let mut cur = head;
        for _ in 0..self.inodes_count.min(1 << 20) {
            if cur == 0 || cur > self.inodes_count {
                break;
            }
            let after = self.orphan_next(cur)?;
            if after == ino {
                return self.set_orphan_next(cur, next);
            }
            cur = after;
        }
        Ok(())
    }

    /// Finish what the orphan list says was left undone (Linux's
    /// ext4_orphan_cleanup): files without names are deleted, files whose
    /// truncation was cut short truncated.
    fn orphan_cleanup(&self) -> KResult<()> {
        let mut cur = self.read_superblock(|sb| le32(sb, SB_LAST_ORPHAN))?;
        let mut deleted = 0;
        let mut truncated = 0;
        while cur != 0 && cur <= self.inodes_count {
            let next = self.orphan_next(cur)?;
            let inode = self.iget(cur)?;
            if inode.disk().links == 0 {
                self.inodes.lock().remove(&cur);
                inode.delete()?;
                deleted += 1;
            } else {
                self.orphan_del(cur)?;
                inode.truncate_to_size()?;
                truncated += 1;
            }
            cur = next;
        }
        if deleted + truncated > 0 {
            kinfo!("EXT4-fs ({}): {deleted} orphan inodes deleted, {truncated} truncated", self.disk_name);
            let mut meta = self.lock.lock();
            self.commit(&mut meta)?;
        }
        Ok(())
    }

    // ----- sync and unmount ------------------------------------------------

    /// Write back everything: file pages, inodes, metadata; free deleted
    /// inodes nobody uses any more. Unused files stay cached (their pages
    /// too) until [`Self::drop_caches`].
    fn sync_all(&self) -> KResult<()> {
        let _syncing = self.sync_lock.lock();
        self.sync_locked()
    }

    /// Write back, then forget the files nobody uses, with their pages
    /// (memory is short, or for a clean count).
    fn drop_caches(&self) -> KResult<()> {
        let _syncing = self.sync_lock.lock();
        self.sync_locked()?;
        self.reap(true)?;
        // Files in use keep their pages mapped or dirty; the others go.
        let inodes: Vec<Arc<Ext4Inode>> = self.inodes.lock().values().cloned().collect();
        for inode in inodes {
            inode.shrink_pages();
        }
        // (Deleting inodes changed metadata: write it before forgetting.)
        let mut meta = self.lock.lock();
        self.commit(&mut meta)?;
        self.cache.drop_clean();
        meta.dirs.clear();
        Ok(())
    }

    fn sync_locked(&self) -> KResult<()> {
        if self.read_only || self.dead.load(Ordering::Relaxed) {
            return Ok(());
        }
        let inodes: Vec<Arc<Ext4Inode>> = self.inodes.lock().values().cloned().collect();
        for inode in &inodes {
            inode.writeback()?;
        }
        drop(inodes);
        self.reap(false)?;
        let mut meta = self.lock.lock();
        self.commit(&mut meta)
    }

    /// Free deleted inodes nobody uses; with `all`, also drop the other
    /// unused (clean) ones from the cache.
    fn reap(&self, all: bool) -> KResult<()> {
        let victims: Vec<Arc<Ext4Inode>> = {
            let mut inodes = self.inodes.lock();
            // Only the cache holds these: no open files, no mappings.
            let unused: Vec<u32> = inodes
                .iter()
                .filter(|&(&ino, i)| ino != ROOT_INO && Arc::strong_count(i) == 1)
                .filter(|(_, i)| i.unused(all))
                .map(|(&ino, _)| ino)
                .collect();
            unused.into_iter().filter_map(|ino| inodes.remove(&ino)).collect()
        };
        for inode in victims {
            if inode.disk().links == 0 && !self.read_only {
                inode.delete()?;
            }
        }
        Ok(())
    }

    /// Write back and mark the file system clean, in use or not.
    fn shutdown(&self) -> KResult<()> {
        let _syncing = self.sync_lock.lock();
        if self.read_only || self.dead.load(Ordering::Relaxed) {
            return Ok(());
        }
        self.sync_locked()?;
        self.mark_clean()
    }

    /// Clean: nothing to recover from the journal (or, without one, the
    /// valid state).
    fn mark_clean(&self) -> KResult<()> {
        let journaled = self.journal.is_some();
        self.update_superblock(|sb| {
            if journaled {
                put32(sb, SB_INCOMPAT, le32(sb, SB_INCOMPAT) & !INCOMPAT_RECOVER);
            } else {
                put16(sb, SB_STATE, le16(sb, SB_STATE) | STATE_VALID);
            }
            put32(sb, 48, now_secs());
        })?;
        let mut meta = self.lock.lock();
        self.commit(&mut meta)
    }

    fn unmount(&self) -> KResult<()> {
        let _syncing = self.sync_lock.lock();
        if self.inodes.lock().iter().any(|(ino, i)| *ino != ROOT_INO && Arc::strong_count(i) > 1) {
            return Err(Errno::EBUSY);
        }
        self.sync_locked()?;
        if !self.read_only && !self.dead.load(Ordering::Relaxed) {
            self.mark_clean()?;
        }
        // The inodes refer to the file system: break the cycle.
        self.inodes.lock().clear();
        Ok(())
    }
}

// ----- added for Disk Image Explorer ----------------------------------------
// (everything above this line is the ruxis driver as it is, apart from paths)

impl Ext4Fs {
    /// Mark the file system as in use again (as `mount` does) after a clean
    /// checkpoint (`shutdown`), so a crash from now on is recovered.
    pub(crate) fn mark_in_use(&self) -> KResult<()> {
        if self.read_only || self.dead.load(Ordering::Relaxed) {
            return Ok(());
        }
        let journaled = self.journal.is_some();
        self.update_superblock(|sb| {
            if journaled {
                put32(sb, SB_INCOMPAT, le32(sb, SB_INCOMPAT) | INCOMPAT_RECOVER);
            } else {
                put16(sb, SB_STATE, le16(sb, SB_STATE) & !STATE_VALID);
            }
        })?;
        let mut meta = self.lock.lock();
        self.commit(&mut meta)
    }

    /// Clean checkpoint: everything written, the file system marked clean,
    /// still usable (Linux's freeze, in effect).
    pub(crate) fn checkpoint(&self) -> KResult<()> {
        self.shutdown()
    }

    /// Forget the cached inodes without writing anything (they refer to the
    /// file system, so this breaks the cycle and lets the device close).
    pub(crate) fn release(&self) {
        self.inodes.lock().clear();
    }

    pub(crate) fn is_read_only(&self) -> bool {
        self.read_only
    }
}

/// crc32c and crc16 as the driver computes them (used by `mkfs`, `fsck`).
pub(crate) use csum::{crc16, crc32c};

/// Drops the clean cached pages of a file (after a long read or a written
/// back write), so copying a big file does not keep all of it in memory.
pub(crate) fn shrink_pages(inode: &Arc<dyn Inode>) {
    if let Some(i) = inode.as_any().downcast_ref::<Ext4Inode>() {
        i.shrink_pages();
    }
}

/// The file system an inode of this driver belongs to.
pub(crate) fn fs_of(inode: &Arc<dyn Inode>) -> Option<Arc<Ext4Fs>> {
    inode.as_any().downcast_ref::<Ext4Inode>().map(|i| i.fs().clone())
}

/// File system names for `mount -t` (and /proc/filesystems).
pub const NAMES: [&str; 3] = ["ext3", "ext2", "ext4"];
