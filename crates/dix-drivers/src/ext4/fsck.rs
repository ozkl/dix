//! An e2fsck-style consistency checker for ext2/3/4 (read-only: problems
//! are reported, not repaired).
//!
//! It parses the on-disk structures itself instead of using the driver, so
//! it can judge what the driver wrote. Only a pending journal is replayed
//! with the driver's code first (into an in-memory overlay, as e2fsck
//! replays before checking; the image is not changed).
//!
//! The passes follow e2fsck:
//! 1. inodes, blocks and sizes;
//! 2. directory structure;
//! 3. connectivity;
//! 4. reference counts;
//! 5. group summary information (bitmaps, free counts, checksums).

use super::kernel::block::Disk;
use super::ruxis::{crc16, crc32c};
use dix_core::{BlockDevice, CheckReport, Error, ErrorKind, LogLevel, MountLog, Result};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// Feature bits.
const COMPAT_HAS_JOURNAL: u32 = 0x4;
const COMPAT_RESIZE_INODE: u32 = 0x10;
const COMPAT_DIR_INDEX: u32 = 0x20;
const COMPAT_SPARSE_SUPER2: u32 = 0x200;
const COMPAT_ORPHAN_FILE: u32 = 0x1000;
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_RECOVER: u32 = 0x4;
const INCOMPAT_JOURNAL_DEV: u32 = 0x8;
const INCOMPAT_META_BG: u32 = 0x10;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_MMP: u32 = 0x100;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_LARGEDIR: u32 = 0x4000;
const INCOMPAT_ENCRYPT: u32 = 0x10000;
const INCOMPAT_CASEFOLD: u32 = 0x20000;
/// What this checker understands.
const INCOMPAT_CHECKED: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_RECOVER
    | INCOMPAT_META_BG
    | INCOMPAT_EXTENTS
    | INCOMPAT_64BIT
    | INCOMPAT_MMP
    | INCOMPAT_FLEX_BG
    | INCOMPAT_CSUM_SEED
    | INCOMPAT_LARGEDIR
    | INCOMPAT_ENCRYPT
    | INCOMPAT_CASEFOLD;
const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
const RO_COMPAT_HUGE_FILE: u32 = 0x8;
const RO_COMPAT_GDT_CSUM: u32 = 0x10;
const RO_COMPAT_DIR_NLINK: u32 = 0x20;
const RO_COMPAT_QUOTA: u32 = 0x100;
const RO_COMPAT_BIGALLOC: u32 = 0x200;
const RO_COMPAT_METADATA_CSUM: u32 = 0x400;
const RO_COMPAT_PROJECT: u32 = 0x2000;

// Inode flags.
const INDEX_FL: u32 = 0x1000;
const HUGE_FILE_FL: u32 = 0x40000;
const EXTENTS_FL: u32 = 0x80000;
const INLINE_DATA_FL: u32 = 0x1000_0000;

const BG_INODE_UNINIT: u16 = 1;
const BG_BLOCK_UNINIT: u16 = 2;

const ROOT_INO: u32 = 2;
const RESIZE_INO: u32 = 7;
const JOURNAL_INO: u32 = 8;

const S_IFMT: u16 = 0o170000;
const S_IFDIR: u16 = 0o040000;
const S_IFREG: u16 = 0o100000;
const S_IFLNK: u16 = 0o120000;
const S_IFCHR: u16 = 0o020000;
const S_IFBLK: u16 = 0o060000;
const S_IFIFO: u16 = 0o010000;
const S_IFSOCK: u16 = 0o140000;

/// Problems reported in detail; beyond this they are only counted.
const MAX_REPORTED: u64 = 200;

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

/// A set of block numbers.
struct Bits(Vec<u64>);

impl Bits {
    fn new(n: u64) -> Bits {
        Bits(vec![0; n.div_ceil(64) as usize])
    }
    fn get(&self, i: u64) -> bool {
        self.0[(i / 64) as usize] >> (i % 64) & 1 != 0
    }
    /// Sets bit `i`; returns whether it was set already.
    fn set(&mut self, i: u64) -> bool {
        let w = &mut self.0[(i / 64) as usize];
        let was = *w >> (i % 64) & 1 != 0;
        *w |= 1 << (i % 64);
        was
    }
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

/// What passes 2–4 need of an inode in use.
struct InodeInfo {
    mode: u16,
    links: u16,
    /// Directories: their blocks, in logical order (0 = hole).
    dir_blocks: Vec<u64>,
    generation: u32,
    flags: u32,
}

struct Checker<'a> {
    disk: &'a Disk,
    log: &'a MountLog,
    reported: u64,
    errors: u64,
    warnings: u64,
    // Geometry and features.
    bs: u64,
    blocks: u64,
    first_data_block: u64,
    bpg: u64,
    ipg: u32,
    inodes_count: u32,
    inode_size: usize,
    first_ino: u32,
    groups: usize,
    desc_size: usize,
    compat: u32,
    incompat: u32,
    ro_compat: u32,
    csum: bool,
    group_csum: bool,
    csum_seed: u32,
    uuid: [u8; 16],
    reserved_gdt: u64,
    first_meta_bg: u64,
    backup_bgs: [u32; 2],
    sb: Vec<u8>,
    gd: Vec<Group>,
    /// Blocks in use: metadata and every block an inode owns.
    used: Bits,
    /// Metadata blocks only (for groups with uninitialised bitmaps).
    meta: Bits,
    duplicates: u64,
    inodes: HashMap<u32, InodeInfo>,
    /// Inodes on the orphan list (in use, but with no name).
    orphans: HashSet<u32>,
    /// xattr blocks: references counted while scanning inodes.
    xattr_refs: HashMap<u64, u32>,
    /// Errors whose fix is not fully determined: if any, nothing is repaired.
    unsafe_errors: u64,
    /// Safe fixes found: link counts, groups whose bitmaps or descriptor
    /// must be rewritten, and the superblock.
    fix_links: Vec<(u32, u16)>,
    fix_block_bitmaps: std::collections::BTreeSet<usize>,
    fix_inode_bitmaps: std::collections::BTreeSet<usize>,
    fix_descs: std::collections::BTreeSet<usize>,
    fix_super: bool,
    /// Per group, as counted: (free blocks, free inodes, directories).
    counted: Vec<(u64, u64, u64)>,
    /// Totals as counted (pass 5).
    totals: (u64, u64),
}

impl Checker<'_> {
    fn problem(&mut self, level: LogLevel, msg: String) {
        match level {
            LogLevel::Error => self.errors += 1,
            LogLevel::Warning => self.warnings += 1,
            LogLevel::Info => {}
        }
        if self.reported < MAX_REPORTED {
            self.reported += 1;
            self.log.log(level, msg);
        } else if self.reported == MAX_REPORTED {
            self.reported += 1;
            self.log.warn("Too many problems: the rest are only counted.");
        }
    }

    /// An error that needs judgment to repair (nothing will be repaired).
    fn error(&mut self, msg: String) {
        self.unsafe_errors += 1;
        self.problem(LogLevel::Error, msg);
    }

    /// An error whose fix is fully determined (recorded separately).
    fn fixable(&mut self, msg: String) {
        self.problem(LogLevel::Error, msg);
    }

    fn warn(&mut self, msg: String) {
        self.problem(LogLevel::Warning, msg);
    }

    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.disk.read_bytes(offset, buf).map_err(Error::from)
    }

    fn read_block(&self, n: u64) -> Result<Vec<u8>> {
        let mut b = vec![0u8; self.bs as usize];
        self.read(n * self.bs, &mut b)?;
        Ok(b)
    }

    fn group_start(&self, g: usize) -> u64 {
        self.first_data_block + g as u64 * self.bpg
    }

    fn group_len(&self, g: usize) -> u64 {
        (self.blocks - self.group_start(g)).min(self.bpg)
    }

    fn has_super(&self, g: usize) -> bool {
        if g == 0 {
            return true;
        }
        if self.compat & COMPAT_SPARSE_SUPER2 != 0 {
            return g as u32 == self.backup_bgs[0] || g as u32 == self.backup_bgs[1];
        }
        if self.ro_compat & RO_COMPAT_SPARSE_SUPER == 0 || g == 1 {
            return true;
        }
        [3usize, 5, 7].iter().any(|&p| {
            let mut x = p;
            while x < g {
                x *= p;
            }
            x == g
        })
    }

    fn desc_per_block(&self) -> u64 {
        self.bs / self.desc_size as u64
    }

    /// Group descriptor blocks of the "old" layout (before meta_bg).
    fn old_gdt_blocks(&self) -> u64 {
        let all = (self.groups as u64).div_ceil(self.desc_per_block());
        if self.incompat & INCOMPAT_META_BG != 0 { all.min(self.first_meta_bg) } else { all }
    }

    /// Where descriptor `g` is: (block, offset).
    fn desc_location(&self, g: usize) -> (u64, usize) {
        let dpb = self.desc_per_block();
        let index = g as u64 / dpb;
        let off = (g as u64 % dpb) as usize * self.desc_size;
        if self.incompat & INCOMPAT_META_BG == 0 || index < self.first_meta_bg {
            (self.first_data_block + 1 + index, off)
        } else {
            let first = (index * dpb) as usize;
            (self.group_start(first) + u64::from(self.has_super(first)), off)
        }
    }

    /// Marks a metadata block; a block claimed twice is a duplicate.
    fn claim_meta(&mut self, b: u64, what: &str) {
        if b < self.first_data_block || b >= self.blocks {
            self.error(format!("{what}: block {b} is outside the file system"));
            return;
        }
        self.meta.set(b);
        if self.used.set(b) {
            self.duplicates += 1;
            self.error(format!("{what}: block {b} is also used by other metadata"));
        }
    }

    /// Claims a block for inode `ino`. Returns false if it is invalid.
    fn claim(&mut self, ino: u32, b: u64, what: &str) -> bool {
        if b < self.first_data_block || b >= self.blocks {
            self.error(format!("inode {ino}: {what} block {b} is outside the file system"));
            return false;
        }
        if self.used.set(b) {
            self.duplicates += 1;
            if self.meta.get(b) {
                self.error(format!("inode {ino}: {what} block {b} overlaps file system metadata"));
            } else {
                self.error(format!("inode {ino}: {what} block {b} is claimed by another inode too"));
            }
        }
        true
    }

    // ----- pass 0: superblock and group descriptors -----------------------

    fn desc_csum(&self, g: usize, d: &[u8]) -> u16 {
        let group = (g as u32).to_le_bytes();
        let ds = self.desc_size;
        if self.csum {
            let c = crc32c(self.csum_seed, &group);
            let c = crc32c(c, &d[..30]);
            let mut c = crc32c(c, &[0, 0]);
            if ds > 32 {
                c = crc32c(c, &d[32..ds]);
            }
            c as u16
        } else {
            let c = crc16(!0, &self.uuid);
            let c = crc16(c, &group);
            let mut c = crc16(c, &d[..30]);
            if ds > 32 {
                c = crc16(c, &d[32..ds]);
            }
            c
        }
    }

    fn load_groups(&mut self) -> Result<()> {
        let wide = self.desc_size >= 64;
        for g in 0..self.groups {
            let (block, off) = self.desc_location(g);
            let mut d = vec![0u8; self.desc_size];
            self.read(block * self.bs + off as u64, &mut d)?;
            let hi32 = |o: usize| if wide { (le32(&d, o) as u64) << 32 } else { 0 };
            let hi16 = |o: usize| if wide { (le16(&d, o) as u32) << 16 } else { 0 };
            let gd = Group {
                block_bitmap: le32(&d, 0) as u64 | hi32(0x20),
                inode_bitmap: le32(&d, 4) as u64 | hi32(0x24),
                inode_table: le32(&d, 8) as u64 | hi32(0x28),
                free_blocks: le16(&d, 0xC) as u32 | hi16(0x2C),
                free_inodes: le16(&d, 0xE) as u32 | hi16(0x2E),
                used_dirs: le16(&d, 0x10) as u32 | hi16(0x30),
                flags: le16(&d, 0x12),
                itable_unused: le16(&d, 0x1C) as u32 | hi16(0x32),
                block_bitmap_csum: le16(&d, 0x18) as u32 | hi16(0x38),
                inode_bitmap_csum: le16(&d, 0x1A) as u32 | hi16(0x3A),
            };
            if self.group_csum && self.desc_csum(g, &d) != le16(&d, 0x1E) {
                // As e2fsck: when the descriptor's locations make sense, the
                // checksum is rewritten (everything else in it is counts,
                // recomputed anyway). Otherwise it needs a decision.
                let itable_blocks = (self.ipg as u64 * self.inode_size as u64).div_ceil(self.bs);
                let sane = |b: u64, n: u64| b >= self.first_data_block && b + n <= self.blocks;
                if sane(gd.block_bitmap, 1) && sane(gd.inode_bitmap, 1) && sane(gd.inode_table, itable_blocks) {
                    self.fixable(format!("group {g}: descriptor checksum does not match"));
                    self.fix_descs.insert(g);
                } else {
                    self.error(format!("group {g}: descriptor checksum does not match, and its locations are invalid"));
                }
            }
            if !self.group_csum && gd.flags & (BG_INODE_UNINIT | BG_BLOCK_UNINIT) != 0 {
                self.error(format!("group {g}: marked uninitialised without group checksums"));
            }
            self.gd.push(gd);
        }
        Ok(())
    }

    /// Superblock copies, descriptor blocks, bitmaps and inode tables.
    fn claim_group_metadata(&mut self) {
        let old_gdt = self.old_gdt_blocks();
        for g in 0..self.groups {
            if self.has_super(g) {
                let start = self.group_start(g);
                // Block 0 of a file system with 1 KiB blocks is the boot
                // block, outside every group: the superblock is block 1.
                self.claim_meta(start, &format!("group {g} superblock"));
                for i in 0..old_gdt + self.reserved_gdt {
                    self.claim_meta(start + 1 + i, &format!("group {g} descriptors"));
                }
            }
        }
        if self.incompat & INCOMPAT_META_BG != 0 {
            let dpb = self.desc_per_block();
            let meta_groups = (self.groups as u64).div_ceil(dpb);
            for mg in self.first_meta_bg..meta_groups {
                let first = (mg * dpb) as usize;
                let mut at = vec![first, first + 1, first + dpb as usize - 1];
                at.dedup();
                let groups = self.groups;
                for g in at.into_iter().filter(|&g| g < groups) {
                    let b = self.group_start(g) + u64::from(self.has_super(g));
                    self.claim_meta(b, &format!("meta_bg descriptors of group {g}"));
                }
            }
        }
        let itable_blocks = (self.ipg as u64 * self.inode_size as u64).div_ceil(self.bs);
        for g in 0..self.groups {
            let gd = self.gd[g];
            self.claim_meta(gd.block_bitmap, &format!("group {g} block bitmap"));
            self.claim_meta(gd.inode_bitmap, &format!("group {g} inode bitmap"));
            for b in gd.inode_table..gd.inode_table + itable_blocks {
                self.claim_meta(b, &format!("group {g} inode table"));
            }
        }
    }

    // ----- pass 1: inodes, blocks and sizes -------------------------------

    fn inode_seed(&self, ino: u32, generation: u32) -> u32 {
        crc32c(crc32c(self.csum_seed, &ino.to_le_bytes()), &generation.to_le_bytes())
    }

    fn inode_csum_ok(&self, ino: u32, raw: &[u8]) -> bool {
        let seed = self.inode_seed(ino, le32(raw, 100));
        let mut c = crc32c(seed, &raw[..124]);
        c = crc32c(c, &[0, 0]);
        c = crc32c(c, &raw[126..128]);
        let mut has_hi = false;
        if self.inode_size > 128 {
            c = crc32c(c, &raw[128..130]);
            let from = if le16(raw, 128) >= 4 {
                has_hi = true;
                c = crc32c(c, &[0, 0]);
                132
            } else {
                130
            };
            c = crc32c(c, &raw[from..self.inode_size]);
        }
        if has_hi { c == (le16(raw, 124) as u32 | (le16(raw, 130) as u32) << 16) } else { c as u16 == le16(raw, 124) }
    }

    /// Blocks of an inode's data, in logical order (holes as 0), via its
    /// extent tree. Counts the tree's own blocks into `meta`.
    #[allow(clippy::too_many_arguments)]
    fn walk_extents(
        &mut self,
        ino: u32,
        generation: u32,
        node: &[u8],
        depth_expected: Option<u16>,
        out: &mut Vec<(u64, u64, u64, bool)>,
        meta: &mut u64,
        in_inode: bool,
    ) -> Result<()> {
        if le16(node, 0) != 0xF30A {
            self.error(format!("inode {ino}: bad extent header"));
            return Ok(());
        }
        let (entries, max, depth) = (le16(node, 2) as usize, le16(node, 4) as usize, le16(node, 6));
        let room = (node.len() - 12) / 12;
        if max > room || entries > max {
            self.error(format!("inode {ino}: extent node with {entries}/{max} entries (room for {room})"));
            return Ok(());
        }
        if depth > 5 || depth_expected.is_some_and(|d| d != depth) {
            self.error(format!("inode {ino}: extent tree has an inconsistent depth"));
            return Ok(());
        }
        if !in_inode && self.csum {
            let tail = 12 + 12 * max;
            if tail + 4 <= node.len() {
                let c = crc32c(self.inode_seed(ino, generation), &node[..tail]);
                if c != le32(node, tail) {
                    self.error(format!("inode {ino}: extent tree block checksum does not match"));
                }
            }
        }
        let mut last_logical: Option<u64> = None;
        for k in 0..entries {
            let e = &node[12 + 12 * k..24 + 12 * k];
            let logical = le32(e, 0) as u64;
            if last_logical.is_some_and(|l| logical <= l) && (depth == 0 || logical != 0 || k != 0) {
                self.error(format!("inode {ino}: extents out of order"));
            }
            last_logical = Some(logical);
            if depth == 0 {
                let raw_len = le16(e, 4) as u64;
                let (len, uninit) = if raw_len > 32768 { (raw_len - 32768, true) } else { (raw_len, false) };
                let start = (le16(e, 6) as u64) << 32 | le32(e, 8) as u64;
                if len == 0 {
                    self.error(format!("inode {ino}: empty extent"));
                    continue;
                }
                out.push((logical, start, len, uninit));
            } else {
                let leaf = (le16(e, 8) as u64) << 32 | le32(e, 4) as u64;
                if !self.claim(ino, leaf, "extent tree") {
                    continue;
                }
                *meta += 1;
                let child = self.read_block(leaf)?;
                self.walk_extents(ino, generation, &child, Some(depth - 1), out, meta, false)?;
            }
        }
        Ok(())
    }

    /// Indirect block trees: data blocks `(logical, physical)`.
    fn walk_indirect(
        &mut self,
        ino: u32,
        block: u64,
        level: u32,
        first: u64,
        out: &mut Vec<(u64, u64)>,
        meta: &mut u64,
    ) -> Result<()> {
        if !self.claim(ino, block, "indirect") {
            return Ok(());
        }
        *meta += 1;
        let b = self.read_block(block)?;
        let per = self.bs / 4;
        let span = per.pow(level - 1);
        for i in 0..per {
            let p = le32(&b, (i * 4) as usize) as u64;
            if p == 0 {
                continue;
            }
            let logical = first + i * span;
            if level == 1 {
                out.push((logical, p));
            } else {
                self.walk_indirect(ino, p, level - 1, logical, out, meta)?;
            }
        }
        Ok(())
    }

    fn check_inode(&mut self, ino: u32, raw: &[u8]) -> Result<()> {
        let mode = le16(raw, 0);
        let links = le16(raw, 26);
        let flags = le32(raw, 32);
        let generation = le32(raw, 100);
        let size = le32(raw, 4) as u64 | (le32(raw, 108) as u64) << 32;
        let kind = mode & S_IFMT;
        if self.csum && !self.inode_csum_ok(ino, raw) {
            self.error(format!("inode {ino}: checksum does not match"));
        }
        if ![S_IFREG, S_IFDIR, S_IFLNK, S_IFCHR, S_IFBLK, S_IFIFO, S_IFSOCK].contains(&kind) && ino >= self.first_ino {
            self.error(format!("inode {ino}: invalid file type {:o}", mode));
            return Ok(());
        }
        if flags & INLINE_DATA_FL != 0 {
            self.warn(format!("inode {ino}: inline data is not checked"));
            self.inodes.insert(ino, InodeInfo { mode, links, dir_blocks: Vec::new(), generation, flags });
            return Ok(());
        }
        let file_acl = le32(raw, 104) as u64 | (le16(raw, 118) as u64) << 32;
        let stored_blocks = {
            let lo = le32(raw, 28) as u64;
            let hi = if self.ro_compat & RO_COMPAT_HUGE_FILE != 0 { (le16(raw, 116) as u64) << 32 } else { 0 };
            lo | hi
        };
        let acl_sectors = if file_acl != 0 { self.bs / 512 } else { 0 };
        // Device nodes, FIFOs, sockets and fast symlinks have no blocks.
        let fast_symlink = kind == S_IFLNK && flags & EXTENTS_FL == 0 && stored_blocks == acl_sectors;
        let no_blocks = matches!(kind, S_IFCHR | S_IFBLK | S_IFIFO | S_IFSOCK) || fast_symlink;
        let mut data: Vec<(u64, u64)> = Vec::new(); // (logical, physical)
        let mut meta_blocks = 0u64;
        let mut uninit_tail = 0u64; // logical end of uninitialised extents
        if fast_symlink {
            if size >= 60 {
                self.error(format!("inode {ino}: fast symlink of size {size} (must be under 60)"));
            }
        } else if ino == RESIZE_INO && self.compat & COMPAT_RESIZE_INODE != 0 {
            // Its tree only points at reserved descriptor blocks (claimed as
            // metadata); its own double-indirect block is the file's.
            let dind = le32(raw, 40 + 4 * 13) as u64;
            if dind != 0 && self.claim(ino, dind, "resize") {
                meta_blocks += 1;
            }
        } else if !no_blocks {
            if flags & EXTENTS_FL != 0 {
                if self.incompat & INCOMPAT_EXTENTS == 0 {
                    self.error(format!("inode {ino}: uses extents but the file system has no extent feature"));
                }
                let mut ext = Vec::new();
                let root = raw[40..100].to_vec();
                self.walk_extents(ino, generation, &root, None, &mut ext, &mut meta_blocks, true)?;
                for (logical, start, len, uninit) in ext {
                    for i in 0..len {
                        if self.claim(ino, start + i, "data") {
                            data.push((logical + i, start + i));
                        }
                    }
                    if uninit {
                        uninit_tail = uninit_tail.max(logical + len);
                    }
                }
            } else {
                for i in 0..12u64 {
                    let p = le32(raw, 40 + 4 * i as usize) as u64;
                    if p != 0 && self.claim(ino, p, "data") {
                        data.push((i, p));
                    }
                }
                let per = self.bs / 4;
                let mut first = 12u64;
                for (slot, level) in [(12usize, 1u32), (13, 2), (14, 3)] {
                    let p = le32(raw, 40 + 4 * slot) as u64;
                    if p != 0 {
                        self.walk_indirect(ino, p, level, first, &mut data, &mut meta_blocks)?;
                    }
                    first += per.pow(level);
                }
                // Data blocks found through indirect blocks are claimed here
                // (direct ones were claimed above).
                let mut valid = Vec::with_capacity(data.len());
                for (l, p) in std::mem::take(&mut data) {
                    if l < 12 || self.claim(ino, p, "data") {
                        valid.push((l, p));
                    }
                }
                data = valid;
            }
        }
        // Extended attribute block (possibly shared).
        if file_acl != 0 {
            if file_acl < self.first_data_block || file_acl >= self.blocks {
                self.error(format!("inode {ino}: extended attribute block {file_acl} is outside the file system"));
            } else {
                let refs = self.xattr_refs.entry(file_acl).or_insert(0);
                *refs += 1;
                if *refs == 1 {
                    self.claim(ino, file_acl, "extended attribute");
                }
            }
        }
        // i_blocks: data + tree blocks + attribute block.
        let counted = data.len() as u64 + meta_blocks + u64::from(file_acl != 0);
        let expected = if flags & HUGE_FILE_FL != 0 { counted } else { counted * self.bs / 512 };
        if !(ino == RESIZE_INO && self.compat & COMPAT_RESIZE_INODE != 0) && stored_blocks != expected && !fast_symlink
        {
            self.error(format!("inode {ino}: i_blocks is {stored_blocks}, should be {expected}"));
        }
        // Sizes.
        let last = data.iter().map(|&(l, _)| l + 1).max().unwrap_or(0);
        match kind {
            S_IFDIR => {
                if !size.is_multiple_of(self.bs) {
                    self.error(format!("directory inode {ino}: size {size} is not a multiple of the block size"));
                }
                if size / self.bs < last {
                    self.error(format!("directory inode {ino}: size {size} too small for its {last} blocks"));
                }
            }
            S_IFREG | S_IFLNK if !fast_symlink => {
                let in_size = size.div_ceil(self.bs);
                if last > in_size && last > uninit_tail {
                    self.error(format!("inode {ino}: size {size} but it has blocks up to {}", last * self.bs));
                }
                if kind == S_IFLNK && (size == 0 || size >= self.bs) {
                    self.error(format!("symlink inode {ino}: invalid size {size}"));
                }
            }
            _ => {}
        }
        let mut dir_blocks = Vec::new();
        if kind == S_IFDIR {
            let n = (size / self.bs) as usize;
            dir_blocks = vec![0; n.max(last as usize)];
            for &(l, p) in &data {
                dir_blocks[l as usize] = p;
            }
        }
        if ino == JOURNAL_INO && self.compat & COMPAT_HAS_JOURNAL != 0 {
            match data.iter().find(|&&(l, _)| l == 0) {
                Some(&(_, p)) => {
                    let jsb = self.read_block(p)?;
                    if be32(&jsb, 0) != 0xC03B_3998 {
                        self.error("the journal has no valid superblock".into());
                    }
                }
                None => self.error("the journal inode has no blocks".into()),
            }
        }
        self.inodes.insert(ino, InodeInfo { mode, links, dir_blocks, generation, flags });
        Ok(())
    }

    /// Inodes on the orphan list: in use, linked from nowhere.
    fn load_orphans(&mut self) -> Result<()> {
        let mut next = le32(&self.sb, 0xE8);
        let mut seen = 0;
        while next != 0 && seen < self.inodes_count {
            if next > self.inodes_count {
                self.error(format!("orphan list points to invalid inode {next}"));
                break;
            }
            self.orphans.insert(next);
            let raw = self.read_inode(next)?;
            next = le32(&raw, 20); // i_dtime is the link
            seen += 1;
        }
        if !self.orphans.is_empty() {
            self.warn(format!(
                "{} inode(s) on the orphan list (deleted or truncated while open): the next read-write mount finishes them",
                self.orphans.len()
            ));
        }
        Ok(())
    }

    fn read_inode(&self, ino: u32) -> Result<Vec<u8>> {
        let g = ((ino - 1) / self.ipg) as usize;
        let i = ((ino - 1) % self.ipg) as u64;
        let mut raw = vec![0u8; self.inode_size];
        self.read(self.gd[g].inode_table * self.bs + i * self.inode_size as u64, &mut raw)?;
        Ok(raw)
    }

    fn pass1(&mut self) -> Result<()> {
        self.log.info("Pass 1: checking inodes, blocks and sizes");
        self.load_orphans()?;
        for g in 0..self.groups {
            self.log.progress(g as u64, self.groups as u64);
            let gd = self.gd[g];
            if self.group_csum && gd.flags & BG_INODE_UNINIT != 0 {
                continue;
            }
            // Inodes past `itable_unused` were never initialised.
            let initialised = if self.group_csum { self.ipg.saturating_sub(gd.itable_unused) } else { self.ipg };
            let mut table = vec![0u8; initialised as usize * self.inode_size];
            self.read(gd.inode_table * self.bs, &mut table)?;
            for i in 0..initialised {
                let ino = g as u32 * self.ipg + i + 1;
                let raw = &table[i as usize * self.inode_size..(i as usize + 1) * self.inode_size];
                let links = le16(raw, 26);
                let reserved_in_use = ino < self.first_ino
                    && ino != ROOT_INO
                    && (le16(raw, 0) != 0 || le32(raw, 28) != 0 || le32(raw, 40) != 0);
                if links == 0 && !self.orphans.contains(&ino) && !reserved_in_use {
                    if ino >= self.first_ino && le16(raw, 0) != 0 && le32(raw, 20) == 0 && le32(raw, 28) != 0 {
                        self.error(format!("inode {ino}: deleted inode has zero dtime"));
                    }
                    continue;
                }
                let raw = raw.to_vec();
                self.check_inode(ino, &raw)?;
            }
        }
        self.log.clear_progress();
        // Shared attribute blocks: their reference count.
        let refs: Vec<(u64, u32)> = self.xattr_refs.iter().map(|(&b, &n)| (b, n)).collect();
        for (block, n) in refs {
            let b = self.read_block(block)?;
            if le32(&b, 0) != 0xEA02_0000 {
                self.error(format!("extended attribute block {block}: bad magic"));
                continue;
            }
            if le32(&b, 4) != n {
                self.error(format!(
                    "extended attribute block {block}: reference count {} but used {n} time(s)",
                    le32(&b, 4)
                ));
            }
            if self.csum {
                let c = crc32c(self.csum_seed, &block.to_le_bytes());
                let c = crc32c(c, &b[..0x10]);
                let c = crc32c(c, &[0; 4]);
                let c = crc32c(c, &b[0x14..]);
                if c != le32(&b, 0x10) {
                    self.error(format!("extended attribute block {block}: checksum does not match"));
                }
            }
        }
        if self.duplicates > 0 {
            self.error(format!("{} block(s) are claimed more than once", self.duplicates));
        }
        Ok(())
    }

    // ----- pass 2: directory structure -------------------------------------

    fn dirent_type(mode: u16) -> u8 {
        match mode & S_IFMT {
            S_IFREG => 1,
            S_IFDIR => 2,
            S_IFCHR => 3,
            S_IFBLK => 4,
            S_IFIFO => 5,
            S_IFSOCK => 6,
            S_IFLNK => 7,
            _ => 0,
        }
    }

    /// Returns, per directory, its entries (name, inode), without . and ..,
    /// and fills `refs` with every reference (. and .. included) and
    /// `dotdot` with each directory's '..'.
    fn pass2(
        &mut self,
        refs: &mut HashMap<u32, u32>,
        dotdot: &mut HashMap<u32, u32>,
    ) -> Result<HashMap<u32, Vec<(String, u32)>>> {
        self.log.info("Pass 2: checking directory structure");
        let filetype = self.incompat & INCOMPAT_FILETYPE != 0;
        let encrypted = self.incompat & INCOMPAT_ENCRYPT != 0;
        let mut dirs: Vec<u32> =
            self.inodes.iter().filter(|(_, i)| i.mode & S_IFMT == S_IFDIR).map(|(&n, _)| n).collect();
        dirs.sort_unstable();
        let mut all = HashMap::new();
        let total = dirs.len() as u64;
        for (k, dir) in dirs.into_iter().enumerate() {
            if k % 256 == 0 {
                self.log.progress(k as u64, total);
            }
            let (blocks, generation, flags) = {
                let i = &self.inodes[&dir];
                (i.dir_blocks.clone(), i.generation, i.flags)
            };
            if flags & INDEX_FL != 0 && self.compat & COMPAT_DIR_INDEX == 0 {
                self.error(format!("directory inode {dir}: has an index but the file system has no dir_index"));
            }
            let mut names: HashSet<Vec<u8>> = HashSet::new();
            let mut entries = Vec::new();
            for (lb, &pb) in blocks.iter().enumerate() {
                if pb == 0 {
                    self.error(format!("directory inode {dir}: block {lb} is a hole"));
                    continue;
                }
                let b = self.read_block(pb)?;
                let bs = self.bs as usize;
                // Index blocks (htree) hide in an entry spanning the block.
                let index_node = flags & INDEX_FL != 0 && lb > 0 && le32(&b, 0) == 0 && le16(&b, 4) as usize == bs;
                let has_tail = le32(&b, bs - 12) == 0 && le16(&b, bs - 8) == 12 && b[bs - 6] == 0 && b[bs - 5] == 0xDE;
                let limit = if self.csum && !index_node && !(flags & INDEX_FL != 0 && lb == 0) {
                    if !has_tail {
                        self.error(format!("directory inode {dir}: block {lb} has no checksum tail"));
                        bs
                    } else {
                        let c = crc32c(self.inode_seed(dir, generation), &b[..bs - 12]);
                        if c != le32(&b, bs - 4) {
                            self.error(format!("directory inode {dir}: block {lb} checksum does not match"));
                        }
                        bs - 12
                    }
                } else if has_tail && self.csum {
                    bs - 12
                } else {
                    bs
                };
                if index_node {
                    continue;
                }
                let mut off = 0usize;
                let mut n = 0;
                while off < limit {
                    if off + 8 > limit {
                        self.error(format!("directory inode {dir}: block {lb}: entry runs past the end"));
                        break;
                    }
                    let ino = le32(&b, off);
                    let rec_len = le16(&b, off + 4) as usize;
                    let name_len = if filetype { b[off + 6] as usize } else { le16(&b, off + 6) as usize };
                    let ftype = if filetype { b[off + 7] } else { 0 };
                    if rec_len < 8
                        || !rec_len.is_multiple_of(4)
                        || off + rec_len > limit
                        || (ino != 0 && 8 + name_len > rec_len)
                    {
                        self.error(format!(
                            "directory inode {dir}: block {lb} offset {off}: bad entry (length {rec_len}, name length {name_len})"
                        ));
                        break;
                    }
                    if ino != 0 {
                        let name = b[off + 8..off + 8 + name_len].to_vec();
                        let shown = String::from_utf8_lossy(&name).into_owned();
                        let first_entries = lb == 0 && n < 2;
                        if ino > self.inodes_count {
                            self.error(format!("directory inode {dir}: entry '{shown}' points to invalid inode {ino}"));
                        } else if let Some(target) = self.inodes.get(&ino) {
                            *refs.entry(ino).or_insert(0) += 1;
                            if filetype && ftype != Self::dirent_type(target.mode) {
                                self.error(format!("directory inode {dir}: entry '{shown}' has the wrong file type"));
                            }
                            if name == b"." {
                                if !(first_entries && n == 0) || ino != dir {
                                    self.error(format!("directory inode {dir}: misplaced or wrong '.' entry"));
                                }
                            } else if name == b".." {
                                if !(first_entries && n == 1) {
                                    self.error(format!("directory inode {dir}: misplaced '..' entry"));
                                }
                                dotdot.insert(dir, ino);
                            } else {
                                if name_len == 0 || (!encrypted && name.iter().any(|&c| c == b'/' || c == 0)) {
                                    self.error(format!("directory inode {dir}: entry with an invalid name '{shown}'"));
                                }
                                if !names.insert(name.clone()) {
                                    self.error(format!("directory inode {dir}: duplicate entry '{shown}'"));
                                }
                                entries.push((shown, ino));
                            }
                        } else {
                            self.error(format!("directory inode {dir}: entry '{shown}' points to unused inode {ino}"));
                        }
                        if lb == 0 && n == 0 && name != b"." {
                            self.error(format!("directory inode {dir}: missing '.'"));
                        }
                        n += 1;
                    } else if lb == 0 && off == 0 {
                        self.error(format!("directory inode {dir}: missing '.'"));
                    }
                    off += rec_len;
                }
            }
            if !blocks.is_empty() && !dotdot.contains_key(&dir) {
                self.error(format!("directory inode {dir}: missing '..'"));
            }
            all.insert(dir, entries);
        }
        self.log.clear_progress();
        Ok(all)
    }

    // ----- pass 3: connectivity --------------------------------------------

    fn pass3(&mut self, entries: &HashMap<u32, Vec<(String, u32)>>, dotdot: &HashMap<u32, u32>) {
        self.log.info("Pass 3: checking directory connectivity");
        match self.inodes.get(&ROOT_INO) {
            Some(i) if i.mode & S_IFMT == S_IFDIR => {}
            Some(_) => {
                self.error("the root inode is not a directory".into());
                return;
            }
            None => {
                self.error("the root inode is not in use".into());
                return;
            }
        }
        let mut parent: HashMap<u32, u32> = HashMap::new();
        parent.insert(ROOT_INO, ROOT_INO);
        let mut queue = vec![ROOT_INO];
        while let Some(d) = queue.pop() {
            for (name, ino) in entries.get(&d).into_iter().flatten() {
                if self.inodes.get(ino).is_some_and(|i| i.mode & S_IFMT == S_IFDIR) {
                    if let Some(p) = parent.get(ino) {
                        self.error(format!("directory inode {ino} ('{name}') is linked from directories {p} and {d}"));
                        continue;
                    }
                    parent.insert(*ino, d);
                    queue.push(*ino);
                }
            }
        }
        let mut dirs: Vec<u32> =
            self.inodes.iter().filter(|(_, i)| i.mode & S_IFMT == S_IFDIR).map(|(&n, _)| n).collect();
        dirs.sort_unstable();
        for d in dirs {
            match parent.get(&d) {
                None if !self.orphans.contains(&d) => {
                    self.error(format!("directory inode {d} is not connected to the root"))
                }
                Some(&p) if dotdot.get(&d).is_some_and(|&dd| dd != p) => {
                    self.error(format!("directory inode {d}: '..' is {} but the parent is {p}", dotdot[&d]))
                }
                _ => {}
            }
        }
        let lost_found = entries.get(&ROOT_INO).into_iter().flatten().any(|(n, _)| n == "lost+found");
        if !lost_found {
            self.warn("/lost+found is missing".into());
        }
    }

    // ----- pass 4: reference counts ------------------------------------------

    fn pass4(&mut self, refs: &HashMap<u32, u32>) {
        self.log.info("Pass 4: checking reference counts");
        let dir_nlink = self.ro_compat & RO_COMPAT_DIR_NLINK != 0;
        let special: HashSet<u32> = self.special_inodes();
        let mut list: Vec<(u32, u16, u16)> = self.inodes.iter().map(|(&n, i)| (n, i.links, i.mode)).collect();
        list.sort_unstable();
        for (ino, links, mode) in list {
            if special.contains(&ino) || (ino < self.first_ino && ino != ROOT_INO) {
                continue;
            }
            let counted = refs.get(&ino).copied().unwrap_or(0);
            if counted == 0 {
                if !self.orphans.contains(&ino) {
                    self.error(format!("inode {ino} is in use but not in any directory (unattached)"));
                }
                continue;
            }
            let is_dir = mode & S_IFMT == S_IFDIR;
            if links as u32 != counted && !(is_dir && dir_nlink && links == 1 && counted > 65000) {
                let target = if is_dir && dir_nlink && counted > 65000 { 1 } else { counted.min(65000) as u16 };
                self.fixable(format!("inode {ino}: link count is {links}, should be {counted}"));
                self.fix_links.push((ino, target));
            }
        }
    }

    /// Hidden system inodes (quota, ...) that live in no directory.
    fn special_inodes(&self) -> HashSet<u32> {
        let mut s = HashSet::new();
        if self.ro_compat & RO_COMPAT_QUOTA != 0 {
            s.insert(le32(&self.sb, 0x240));
            s.insert(le32(&self.sb, 0x244));
        }
        if self.ro_compat & RO_COMPAT_PROJECT != 0 {
            s.insert(le32(&self.sb, 0x26C));
        }
        s.insert(le32(&self.sb, 0xE0)); // journal
        s.remove(&0);
        s
    }

    // ----- pass 5: group summary -------------------------------------------

    fn pass5(&mut self) -> Result<(u64, u64)> {
        self.log.info("Pass 5: checking group summary information");
        let (mut free_blocks, mut free_inodes) = (0u64, 0u64);
        let mut block_diff: (u64, u64) = (0, 0); // (used but free on disk, free but used on disk)
        let mut inode_diff: (u64, u64) = (0, 0);
        let mut examples = Vec::new();
        for g in 0..self.groups {
            self.log.progress(g as u64, self.groups as u64);
            let gd = self.gd[g];
            let (start, len) = (self.group_start(g), self.group_len(g));
            // Blocks.
            let bitmap = if self.group_csum && gd.flags & BG_BLOCK_UNINIT != 0 {
                None
            } else {
                Some(self.read_block(gd.block_bitmap)?)
            };
            let mut gfree = 0u64;
            let block_diffs_before = block_diff;
            for i in 0..len {
                let b = start + i;
                let used = self.used.get(b);
                let on_disk = match &bitmap {
                    Some(bm) => bm[(i / 8) as usize] >> (i % 8) & 1 != 0,
                    None => self.meta.get(b),
                };
                if !used {
                    gfree += 1;
                }
                if used && !on_disk {
                    block_diff.0 += 1;
                    if examples.len() < 8 {
                        examples.push(format!("+{b}"));
                    }
                } else if !used && on_disk {
                    block_diff.1 += 1;
                    if examples.len() < 8 {
                        examples.push(format!("-{b}"));
                    }
                }
            }
            if block_diff != block_diffs_before {
                self.fix_block_bitmaps.insert(g);
            }
            if let Some(bm) = &bitmap
                && self.csum
            {
                let c = crc32c(self.csum_seed, &bm[..(self.bpg / 8) as usize]);
                let stored = if self.desc_size >= 64 { gd.block_bitmap_csum } else { gd.block_bitmap_csum & 0xFFFF };
                let c = if self.desc_size >= 64 { c } else { c & 0xFFFF };
                if c != stored {
                    // The bitmap is derived data: rebuilding it fixes this.
                    self.fixable(format!("group {g}: block bitmap checksum does not match"));
                    self.fix_block_bitmaps.insert(g);
                }
            }
            if gfree != gd.free_blocks as u64 {
                self.fixable(format!("group {g}: free blocks count is {}, counted {gfree}", gd.free_blocks));
                self.fix_descs.insert(g);
            }
            free_blocks += gfree;
            // Inodes.
            let ibitmap = if self.group_csum && gd.flags & BG_INODE_UNINIT != 0 {
                None
            } else {
                Some(self.read_block(gd.inode_bitmap)?)
            };
            let (mut ifree, mut dirs) = (0u64, 0u64);
            let inode_diffs_before = inode_diff;
            for i in 0..self.ipg {
                let ino = g as u32 * self.ipg + i + 1;
                let info = self.inodes.get(&ino);
                let used = info.is_some() || ino < self.first_ino;
                let on_disk = ibitmap.as_ref().is_some_and(|bm| bm[(i / 8) as usize] >> (i % 8) & 1 != 0);
                if !used {
                    ifree += 1;
                }
                if info.is_some_and(|i| i.mode & S_IFMT == S_IFDIR) {
                    dirs += 1;
                }
                if used && !on_disk {
                    inode_diff.0 += 1;
                } else if !used && on_disk {
                    inode_diff.1 += 1;
                }
            }
            if inode_diff != inode_diffs_before {
                self.fix_inode_bitmaps.insert(g);
            }
            if let Some(bm) = &ibitmap
                && self.csum
            {
                let c = crc32c(self.csum_seed, &bm[..(self.ipg / 8) as usize]);
                let stored = if self.desc_size >= 64 { gd.inode_bitmap_csum } else { gd.inode_bitmap_csum & 0xFFFF };
                let c = if self.desc_size >= 64 { c } else { c & 0xFFFF };
                if c != stored {
                    self.fixable(format!("group {g}: inode bitmap checksum does not match"));
                    self.fix_inode_bitmaps.insert(g);
                }
            }
            if ifree != gd.free_inodes as u64 {
                self.fixable(format!("group {g}: free inodes count is {}, counted {ifree}", gd.free_inodes));
                self.fix_descs.insert(g);
            }
            if dirs != gd.used_dirs as u64 {
                self.fixable(format!("group {g}: directories count is {}, counted {dirs}", gd.used_dirs));
                self.fix_descs.insert(g);
            }
            free_inodes += ifree;
            self.counted.push((gfree, ifree, dirs));
        }
        self.log.clear_progress();
        if block_diff != (0, 0) {
            self.fixable(format!(
                "block bitmap differences: {} in use but marked free, {} marked in use but free (e.g. {})",
                block_diff.0,
                block_diff.1,
                examples.join(" ")
            ));
        }
        if inode_diff != (0, 0) {
            self.fixable(format!(
                "inode bitmap differences: {} in use but marked free, {} marked in use but free",
                inode_diff.0, inode_diff.1
            ));
        }
        let sb_free_blocks = le32(&self.sb, 12) as u64
            | if self.incompat & INCOMPAT_64BIT != 0 { (le32(&self.sb, 0x158) as u64) << 32 } else { 0 };
        // The superblock's totals are maintained lazily by Linux (the group
        // counts are the real ones): like e2fsck, a mismatch is not damage.
        if sb_free_blocks != free_blocks {
            self.fix_super = true;
            self.warn(format!("free blocks count in the superblock is {sb_free_blocks}, counted {free_blocks}"));
        }
        let sb_free_inodes = le32(&self.sb, 16) as u64;
        if sb_free_inodes != free_inodes {
            self.fix_super = true;
            self.warn(format!("free inodes count in the superblock is {sb_free_inodes}, counted {free_inodes}"));
        }
        self.totals = (free_blocks, free_inodes);
        Ok((free_blocks, free_inodes))
    }
}

/// Checks the ext2/3/4 file system on `dev`. With `repair`, safe problems
/// are repaired (see [`Checker::repair`]) and the result is checked again;
/// without it nothing is written (`dev` is read-only then).
pub fn check(dev: Box<dyn BlockDevice>, log: &MountLog, repair: bool) -> Result<CheckReport> {
    let mut sb = vec![0u8; 1024];
    let mut dev = dev;
    dev.read_at(1024, &mut sb)?;
    if le16(&sb, 56) != 0xEF53 {
        return Err(Error::new(ErrorKind::UnknownFormat, "not an ext2/3/4 file system"));
    }
    let rev = le32(&sb, 76);
    let (compat, incompat, ro_compat) =
        if rev == 0 { (0, 0, 0) } else { (le32(&sb, 92), le32(&sb, 96), le32(&sb, 100)) };
    if incompat & !INCOMPAT_CHECKED != 0 || incompat & INCOMPAT_JOURNAL_DEV != 0 {
        log.error(format!("Features this checker does not understand: incompat {:#x}", incompat & !INCOMPAT_CHECKED));
        return Err(Error::unsupported("checking this ext4 feature set"));
    }
    if ro_compat & RO_COMPAT_BIGALLOC != 0 || compat & COMPAT_ORPHAN_FILE != 0 {
        log.error("bigalloc and orphan_file are not supported by this checker".to_string());
        return Err(Error::unsupported("checking this ext4 feature set"));
    }
    // Checking only: every read goes through an overlay, so a pending
    // journal is replayed by the driver into memory, never onto the image.
    // Repairing: the journal is replayed for real first (as e2fsck does).
    let disk = Arc::new(Disk::new(dev, !repair));
    if compat & COMPAT_HAS_JOURNAL != 0 && incompat & INCOMPAT_RECOVER != 0 {
        if repair {
            log.warn("The journal needs recovery: replaying it onto the image.");
        } else {
            log.warn("The journal needs recovery: replaying it in memory before checking (the image is not changed).");
        }
        match super::kernel::with_log(log, || super::ruxis::mount(disk.clone(), !repair, "auto")) {
            Ok((root, _)) => {
                if let Some(fs) = super::ruxis::fs_of(&root) {
                    if repair {
                        // Clean unmount: everything written, marked clean.
                        fs.checkpoint().map_err(Error::from)?;
                    }
                    fs.release();
                }
            }
            Err(e) => {
                log.error(format!("Replaying the journal failed: {}", Error::from(e)));
                return Ok(CheckReport { errors: 1, summary: "journal replay failed".into(), ..Default::default() });
            }
        }
    }
    let first = analyse(&disk, log)?;
    if !repair || first.errors == 0 && !first.repairable {
        return Ok(first);
    }
    if !first.repairable {
        log.error(
            "Not repaired: some problems need a decision (e.g. e2fsck -y would reconnect, clear or rebuild things). \
             Nothing was changed."
                .to_string(),
        );
        return Ok(first);
    }
    // Analyse again to get the fixes, apply them, and verify.
    let fixed = {
        let quiet = MountLog::new();
        let mut c = Checker::analyse(&disk, &quiet)?;
        c.repair()?
    };
    log.info(format!("Repaired {fixed} item(s). Checking again…"));
    let mut after = analyse(&disk, log)?;
    after.fixed = fixed;
    Ok(after)
}

/// The free space of an ext2/3/4 file system (blocks its group bitmaps
/// mark free), as byte ranges: what a copy of only the used space leaves
/// out. None when that cannot be trusted: a journal waiting to be replayed
/// (blocks it fills may look free), or features read differently
/// (bigalloc's bitmaps count clusters).
pub fn free_space(dev: Box<dyn BlockDevice>) -> Result<Option<Vec<(u64, u64)>>> {
    let mut sb = vec![0u8; 1024];
    let mut dev = dev;
    dev.read_at(1024, &mut sb)?;
    if le16(&sb, 56) != 0xEF53 {
        return Err(Error::new(ErrorKind::UnknownFormat, "not an ext2/3/4 file system"));
    }
    let (incompat, ro_compat) = if le32(&sb, 76) == 0 { (0, 0) } else { (le32(&sb, 96), le32(&sb, 100)) };
    if incompat & INCOMPAT_RECOVER != 0
        || incompat & !INCOMPAT_CHECKED != 0
        || incompat & INCOMPAT_JOURNAL_DEV != 0
        || ro_compat & RO_COMPAT_BIGALLOC != 0
    {
        return Ok(None);
    }
    let disk = Disk::new(dev, true);
    let quiet = MountLog::new();
    let c = Checker::load(&disk, &quiet)?;
    let mut out: Vec<(u64, u64)> = Vec::new();
    for g in 0..c.groups {
        let gd = c.gd[g];
        let (start, len) = (c.group_start(g), c.group_len(g));
        // An uninitialised group's bitmap is not on disk: only metadata is
        // in it (its own or, with flex_bg, other groups').
        let bitmap =
            if c.group_csum && gd.flags & BG_BLOCK_UNINIT != 0 { None } else { Some(c.read_block(gd.block_bitmap)?) };
        for i in 0..len {
            let b = start + i;
            let used = match &bitmap {
                Some(bm) => bm[(i / 8) as usize] >> (i % 8) & 1 != 0,
                None => c.meta.get(b),
            };
            if used {
                continue;
            }
            let at = b * c.bs;
            match out.last_mut() {
                Some(r) if r.1 == at => r.1 += c.bs,
                _ => out.push((at, at + c.bs)),
            }
        }
    }
    Ok(Some(out))
}

/// One full check: the report (errors, and whether all are safely repairable).
fn analyse(disk: &Disk, log: &MountLog) -> Result<CheckReport> {
    let c = Checker::analyse(disk, log)?;
    let (free_blocks, free_inodes) = c.totals;
    let summary = format!(
        "{}/{} inodes, {}/{} blocks",
        c.inodes_count as u64 - free_inodes,
        c.inodes_count,
        c.blocks - c.first_data_block - free_blocks,
        c.blocks - c.first_data_block
    );
    let verdict =
        if c.errors == 0 { "no problems found".to_string() } else { format!("{} problem(s) found", c.errors) };
    log.log(if c.errors == 0 { LogLevel::Info } else { LogLevel::Error }, format!("{summary}: {verdict}."));
    let repairable = c.unsafe_errors == 0 && (c.errors > 0 || c.fix_super || c.not_clean());
    if c.errors > 0 && repairable {
        log.info("All problems found can be repaired safely.");
    }
    Ok(CheckReport { errors: c.errors, warnings: c.warnings, summary, fixed: 0, repairable })
}

impl<'a> Checker<'a> {
    fn write(&self, offset: u64, data: &[u8]) -> Result<()> {
        self.disk.write_bytes(offset, data).map_err(Error::from)
    }

    /// Applies the safe fixes found by the analysis. Only called when there
    /// are no unsafe errors. Everything written here is derived data (counts,
    /// bitmaps, link counts and their checksums): no file contents change,
    /// and running it twice changes nothing more. Returns the items fixed.
    fn repair(&mut self) -> Result<u64> {
        debug_assert_eq!(self.unsafe_errors, 0);
        let mut fixed = 0u64;
        // Link counts (and the inode's checksum).
        for &(ino, links) in &self.fix_links.clone() {
            let mut raw = self.read_inode(ino)?;
            raw[26..28].copy_from_slice(&links.to_le_bytes());
            if self.csum {
                self.set_inode_csum(ino, &mut raw);
            }
            let g = ((ino - 1) / self.ipg) as usize;
            let i = ((ino - 1) % self.ipg) as u64;
            self.write(self.gd[g].inode_table * self.bs + i * self.inode_size as u64, &raw)?;
            fixed += 1;
        }
        let groups: std::collections::BTreeSet<usize> =
            self.fix_block_bitmaps.iter().chain(&self.fix_inode_bitmaps).chain(&self.fix_descs).copied().collect();
        let bs = self.bs as usize;
        for g in groups {
            let (start, len) = (self.group_start(g), self.group_len(g));
            let (block_csum, inode_csum) = {
                let mut gd = self.gd[g];
                if self.fix_block_bitmaps.contains(&g) {
                    // Rebuilt from what is in use; padding bits set.
                    let mut bm = vec![0u8; bs];
                    for i in 0..8 * bs as u64 {
                        if i >= len || self.used.get(start + i) {
                            bm[(i / 8) as usize] |= 1 << (i % 8);
                        }
                    }
                    self.write(gd.block_bitmap * self.bs, &bm)?;
                    gd.flags &= !BG_BLOCK_UNINIT;
                    fixed += 1;
                }
                if self.fix_inode_bitmaps.contains(&g) {
                    let mut bm = vec![0u8; bs];
                    for i in 0..8 * bs as u32 {
                        let ino = g as u32 * self.ipg + i + 1;
                        if i >= self.ipg || ino < self.first_ino || self.inodes.contains_key(&ino) {
                            bm[(i / 8) as usize] |= 1 << (i % 8);
                        }
                    }
                    self.write(gd.inode_bitmap * self.bs, &bm)?;
                    fixed += 1;
                }
                self.gd[g] = gd;
                // Bitmap checksums from what is on disk now.
                let bb = if gd.flags & BG_BLOCK_UNINIT != 0 && self.group_csum {
                    None
                } else {
                    Some(crc32c(self.csum_seed, &self.read_block(gd.block_bitmap)?[..(self.bpg / 8) as usize]))
                };
                let ib = if gd.flags & BG_INODE_UNINIT != 0 && self.group_csum {
                    None
                } else {
                    Some(crc32c(self.csum_seed, &self.read_block(gd.inode_bitmap)?[..(self.ipg / 8) as usize]))
                };
                (bb, ib)
            };
            // The descriptor: counts, flags, bitmap checksums, its checksum.
            let (block, off) = self.desc_location(g);
            let mut d = vec![0u8; self.desc_size];
            self.read(block * self.bs + off as u64, &mut d)?;
            let (free_b, free_i, dirs) = self.counted[g];
            let wide = self.desc_size >= 64;
            let put_split = |d: &mut [u8], lo: usize, hi: usize, v: u64| {
                d[lo..lo + 2].copy_from_slice(&(v as u16).to_le_bytes());
                if wide {
                    d[hi..hi + 2].copy_from_slice(&((v >> 16) as u16).to_le_bytes());
                }
            };
            put_split(&mut d, 0xC, 0x2C, free_b);
            put_split(&mut d, 0xE, 0x2E, free_i);
            put_split(&mut d, 0x10, 0x30, dirs);
            d[0x12..0x14].copy_from_slice(&self.gd[g].flags.to_le_bytes());
            if self.csum {
                if let Some(c) = block_csum {
                    put_split(&mut d, 0x18, 0x38, c as u64);
                }
                if let Some(c) = inode_csum {
                    put_split(&mut d, 0x1A, 0x3A, c as u64);
                }
            }
            if self.group_csum {
                let c = self.desc_csum(g, &d);
                d[0x1E..0x20].copy_from_slice(&c.to_le_bytes());
            }
            self.write(block * self.bs + off as u64, &d)?;
            if self.fix_descs.contains(&g) {
                fixed += 1;
            }
        }
        // Superblock: totals, clean, checked now.
        let mut sb = self.sb.clone();
        let (free_blocks, free_inodes) = self.totals;
        sb[12..16].copy_from_slice(&(free_blocks as u32).to_le_bytes());
        if self.incompat & INCOMPAT_64BIT != 0 {
            sb[0x158..0x15C].copy_from_slice(&((free_blocks >> 32) as u32).to_le_bytes());
        }
        sb[16..20].copy_from_slice(&(free_inodes as u32).to_le_bytes());
        let state = (le16(&sb, 58) | 1) & !2; // valid, no errors
        sb[58..60].copy_from_slice(&state.to_le_bytes());
        sb[64..68].copy_from_slice(&(dix_core::time::unix_now() as u32).to_le_bytes()); // last check
        sb[52..54].copy_from_slice(&0u16.to_le_bytes()); // mount count
        if self.csum {
            let c = crc32c(!0, &sb[..0x3FC]);
            sb[0x3FC..0x400].copy_from_slice(&c.to_le_bytes());
        }
        self.write(1024, &sb)?;
        if self.fix_super || self.not_clean() {
            fixed += 1;
        }
        self.disk.dev.flush().map_err(Error::from)?;
        Ok(fixed)
    }

    fn set_inode_csum(&self, ino: u32, raw: &mut [u8]) {
        let seed = self.inode_seed(ino, le32(raw, 100));
        let mut c = crc32c(seed, &raw[..124]);
        c = crc32c(c, &[0, 0]);
        c = crc32c(c, &raw[126..128]);
        let mut has_hi = false;
        if self.inode_size > 128 {
            c = crc32c(c, &raw[128..130]);
            let from = if le16(raw, 128) >= 4 {
                has_hi = true;
                c = crc32c(c, &[0, 0]);
                132
            } else {
                130
            };
            c = crc32c(c, &raw[from..self.inode_size]);
        }
        raw[124..126].copy_from_slice(&(c as u16).to_le_bytes());
        if has_hi {
            raw[130..132].copy_from_slice(&((c >> 16) as u16).to_le_bytes());
        }
    }

    fn not_clean(&self) -> bool {
        self.compat & COMPAT_HAS_JOURNAL == 0 && le16(&self.sb, 58) & 1 == 0
    }

    /// Runs all passes.
    fn analyse(disk: &'a Disk, log: &'a MountLog) -> Result<Checker<'a>> {
        let mut c = Checker::load(disk, log)?;
        c.pass1()?;
        let (mut refs, mut dotdot) = (HashMap::new(), HashMap::new());
        let entries = c.pass2(&mut refs, &mut dotdot)?;
        c.pass3(&entries, &dotdot);
        c.pass4(&refs);
        c.pass5()?;
        Ok(c)
    }

    /// The superblock and group descriptors, with every metadata block
    /// claimed (pass 0).
    fn load(disk: &'a Disk, log: &'a MountLog) -> Result<Checker<'a>> {
        let mut sb = vec![0u8; 1024];
        disk.read_bytes(1024, &mut sb).map_err(Error::from)?;
        let rev = le32(&sb, 76);
        let (compat, incompat, ro_compat) =
            if rev == 0 { (0, 0, 0) } else { (le32(&sb, 92), le32(&sb, 96), le32(&sb, 100)) };

        let log_bs = le32(&sb, 24);
        if log_bs > 6 {
            return Err(Error::corrupted("superblock: invalid block size"));
        }
        let bs = 1024u64 << log_bs;
        let wide = incompat & INCOMPAT_64BIT != 0;
        let blocks = le32(&sb, 4) as u64 | if wide { (le32(&sb, 0x150) as u64) << 32 } else { 0 };
        let first_data_block = le32(&sb, 20) as u64;
        let bpg = le32(&sb, 32) as u64;
        let ipg = le32(&sb, 40);
        let inodes_count = le32(&sb, 0);
        let inode_size = if rev == 0 { 128 } else { le16(&sb, 88) as usize };
        let first_ino = if rev == 0 { 11 } else { le32(&sb, 84) };
        let desc_size = if wide { le16(&sb, 0xFE) as usize } else { 32 };
        if bpg == 0
            || ipg == 0
            || blocks <= first_data_block
            || !(128..=bs as usize).contains(&inode_size)
            || desc_size < 32
        {
            log.error("The superblock describes an impossible geometry.".to_string());
            return Err(Error::corrupted("the superblock describes an impossible geometry"));
        }
        let groups = (blocks - first_data_block).div_ceil(bpg) as usize;
        let csum = ro_compat & RO_COMPAT_METADATA_CSUM != 0;
        let uuid: [u8; 16] = sb[104..120].try_into().unwrap();
        let csum_seed = if incompat & INCOMPAT_CSUM_SEED != 0 { le32(&sb, 0x270) } else { crc32c(!0, &uuid) };
        if disk.size() < blocks * bs {
            log.error(format!(
                "The file system ({} bytes) is larger than the image ({} bytes).",
                blocks * bs,
                disk.size()
            ));
        }
        let mut c = Checker {
            disk,
            log,
            reported: 0,
            errors: 0,
            warnings: 0,
            bs,
            blocks,
            first_data_block,
            bpg,
            ipg,
            inodes_count,
            inode_size,
            first_ino,
            groups,
            desc_size,
            compat,
            incompat,
            ro_compat,
            csum,
            group_csum: csum || ro_compat & RO_COMPAT_GDT_CSUM != 0,
            csum_seed,
            uuid,
            reserved_gdt: if compat & COMPAT_RESIZE_INODE != 0 { le16(&sb, 0xCE) as u64 } else { 0 },
            first_meta_bg: le32(&sb, 0x104) as u64,
            backup_bgs: [le32(&sb, 0x24C), le32(&sb, 0x250)],
            sb: sb.clone(),
            gd: Vec::new(),
            used: Bits::new(blocks),
            meta: Bits::new(blocks),
            duplicates: 0,
            inodes: HashMap::new(),
            orphans: HashSet::new(),
            xattr_refs: HashMap::new(),
            unsafe_errors: 0,
            fix_links: Vec::new(),
            fix_block_bitmaps: Default::default(),
            fix_inode_bitmaps: Default::default(),
            fix_descs: Default::default(),
            fix_super: false,
            counted: Vec::new(),
            totals: (0, 0),
        };
        if groups as u64 * ipg as u64 != inodes_count as u64 {
            c.error(format!("superblock: {inodes_count} inodes, but {groups} groups of {ipg}"));
        }
        if csum {
            if sb[0x175] != 1 {
                c.error(format!("superblock: unknown checksum type {}", sb[0x175]));
            } else if crc32c(!0, &sb[..0x3FC]) != le32(&sb, 0x3FC) {
                c.error("superblock checksum does not match".into());
            }
        }
        if compat & COMPAT_HAS_JOURNAL == 0 && le16(&sb, 58) & 1 == 0 {
            c.warn("The file system was not cleanly unmounted (it has no journal to recover from).".into());
        }
        if le16(&sb, 58) & 2 != 0 {
            c.warn("The file system is marked as having errors.".into());
        }
        c.load_groups()?;
        c.claim_group_metadata();
        Ok(c)
    }
}
