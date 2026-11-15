//! Creating empty ext2, ext3 and ext4 file systems (like `mke2fs`).
//!
//! The group layout follows ruxis's ext2 image builder (`xtask/src/ext2.rs`)
//! and mke2fs's defaults:
//!
//! * ext2: revision 1, 256-byte inodes, `filetype`, `sparse_super`,
//!   `large_file`.
//! * ext3: + a jbd2 journal (inode 8, block-mapped), `dir_index`, `ext_attr`.
//! * ext4: + extents (the root, `lost+found` and the journal are extent
//!   trees), `huge_file`, `dir_nlink`, `extra_isize`, `metadata_csum`
//!   (superblock, group descriptors, bitmaps, inodes and directory blocks),
//!   journal checksums (`csum_v3`), `64bit` (64-byte group descriptors, no
//!   16 TiB limit) and `flex_bg` (the bitmaps and inode tables of 16 groups
//!   packed together). Groups other than the first start uninitialised
//!   (`INODE_UNINIT`, and `BLOCK_UNINIT` where nothing else lives in them),
//!   so their inode tables are never written: like mke2fs's lazy init, which
//!   is what makes creating a large file system quick.
//!
//! Used blocks are tracked as runs, so even a huge file system needs little
//! memory; zeroed areas (inode tables, the journal) go straight to the device.
//!
//! `meta_bg` (any variant) is turned on when the descriptor table would not
//! fit in a group (huge file systems, small blocks), or on request: each
//! block of descriptors then lives in the first, second and last group it
//! describes, instead of the whole table after every superblock copy.
//! `s_first_meta_bg` is 0, as mke2fs makes it without a resize inode.
//!
//! Not used: `uninit_bg` on ext2/ext3, the resize inode.

use super::ruxis::crc32c;
use dix_core::{BlockDevice, Error, ErrorKind, Result};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Ext2,
    Ext3,
    Ext4,
}

impl Variant {
    pub fn parse(s: &str) -> Option<Variant> {
        match s {
            "ext2" => Some(Variant::Ext2),
            "ext3" => Some(Variant::Ext3),
            "ext4" => Some(Variant::Ext4),
            _ => None,
        }
    }

    fn journal(self) -> bool {
        self != Variant::Ext2
    }

    fn ext4(self) -> bool {
        self == Variant::Ext4
    }
}

const INODE_SIZE: usize = 256;
const EXTRA_ISIZE: u16 = 32;
const ROOT_INO: u32 = 2;
const JOURNAL_INO: u32 = 8;
const LOST_FOUND_INO: u32 = 11;
const FIRST_INO: u32 = 11;
/// Groups per flex group (mke2fs's default `-G 16`).
const LOG_GROUPS_PER_FLEX: u32 = 4;

const COMPAT_EXT_ATTR: u32 = 0x8;
const COMPAT_HAS_JOURNAL: u32 = 0x4;
const COMPAT_DIR_INDEX: u32 = 0x20;
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_META_BG: u32 = 0x10;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
const RO_COMPAT_LARGE_FILE: u32 = 0x2;
const RO_COMPAT_HUGE_FILE: u32 = 0x8;
const RO_COMPAT_DIR_NLINK: u32 = 0x20;
const RO_COMPAT_EXTRA_ISIZE: u32 = 0x40;
const RO_COMPAT_METADATA_CSUM: u32 = 0x400;
const EXTENTS_FL: u32 = 0x80000;
const BG_INODE_UNINIT: u16 = 1;
const BG_BLOCK_UNINIT: u16 = 2;
const BG_INODE_ZEROED: u16 = 4;

/// Smallest image accepted (ext2 / with a journal).
pub const MIN_SIZE: u64 = 256 * 1024;
pub const MIN_SIZE_JOURNAL: u64 = 4 * 1024 * 1024;

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_be32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}

fn has_super(g: u64) -> bool {
    if g <= 1 {
        return true;
    }
    [3u64, 5, 7].iter().any(|&p| {
        let mut x = p;
        while x < g {
            x *= p;
        }
        x == g
    })
}

/// The groups holding a copy of descriptor block `m` with meta_bg: the
/// first, second and last group it describes (those that exist).
fn meta_bg_copies(m: u64, per_block: u64, groups: u64) -> Vec<u64> {
    let first = m * per_block;
    let mut at = vec![first, first + 1, first + per_block - 1];
    at.dedup();
    at.retain(|&g| g < groups);
    at
}

/// Choices beyond the variant (defaults: mke2fs's).
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// 1024, 2048 or 4096; default 4096 from 512 MiB up, else 1024.
    pub block_size: Option<u64>,
    /// `meta_bg`; default: only when the descriptor table would not fit in
    /// a group. `Some(false)` refuses such a layout instead.
    pub meta_bg: Option<bool>,
}

/// Where each group's bitmaps and inode table are.
#[derive(Clone, Copy, Default)]
struct GroupLayout {
    block_bitmap: u64,
    inode_bitmap: u64,
    inode_table: u64,
    /// Blocks in this group's range that are used (any owner).
    used: u64,
    /// Something other than its own superblock copy, descriptors, bitmaps
    /// and inode table lives in this group: it cannot be `BLOCK_UNINIT`.
    foreign: bool,
}

struct Fs<'d> {
    dev: &'d mut dyn BlockDevice,
    variant: Variant,
    bs: u64,
    blocks: u64,
    first_data_block: u64,
    bpg: u64,
    ipg: u32,
    desc_size: usize,
    itable_blocks: u64,
    /// Used block ranges: start -> length (no overlaps).
    runs: BTreeMap<u64, u64>,
    layout: Vec<GroupLayout>,
    used_inodes: Vec<u32>,
    used_dirs: Vec<u32>,
    /// Small blocks with contents (everything else written is zeroes).
    out: BTreeMap<u64, Vec<u8>>,
    now: u32,
    uuid: [u8; 16],
    csum_seed: u32,
}

impl Fs<'_> {
    fn group_start(&self, g: u64) -> u64 {
        self.first_data_block + g * self.bpg
    }

    fn group_len(&self, g: u64) -> u64 {
        (self.blocks - self.group_start(g)).min(self.bpg)
    }

    fn group_of(&self, b: u64) -> u64 {
        (b - self.first_data_block) / self.bpg
    }

    /// First used run overlapping or after `b`, if it overlaps `[b, b+n)`.
    fn overlap(&self, b: u64, n: u64) -> Option<(u64, u64)> {
        if let Some((&s, &l)) = self.runs.range(..=b).next_back()
            && s + l > b
        {
            return Some((s, l));
        }
        self.runs.range(b..b + n).next().map(|(&s, &l)| (s, l))
    }

    /// Records `[b, b+n)` as used, by group `owner` (None: data).
    fn mark(&mut self, b: u64, n: u64, owner: Option<u64>) {
        self.runs.insert(b, n);
        // Account per group (a run may cross group boundaries).
        let mut at = b;
        while at < b + n {
            let g = self.group_of(at);
            let end = (self.group_start(g) + self.group_len(g)).min(b + n);
            let layout = &mut self.layout[g as usize];
            layout.used += end - at;
            if owner != Some(g) {
                layout.foreign = true;
            }
            at = end;
        }
    }

    /// First free run of `n` blocks at or after `from` (and, if `limit`,
    /// ending before it), marked used.
    fn alloc(&mut self, from: u64, n: u64, limit: Option<u64>, owner: Option<u64>) -> Result<u64> {
        let mut b = from.max(self.first_data_block);
        loop {
            let end = limit.unwrap_or(self.blocks);
            if b + n > end {
                return Err(Error::new(ErrorKind::NoSpace, "the image is too small for this file system"));
            }
            match self.overlap(b, n) {
                Some((s, l)) => b = s + l,
                None => {
                    self.mark(b, n, owner);
                    return Ok(b);
                }
            }
        }
    }

    /// `count` blocks from the first free gaps, marked used: (start, len).
    fn alloc_scattered(&mut self, count: u64) -> Result<Vec<(u64, u64)>> {
        let mut pieces = Vec::new();
        let (mut at, mut left) = (self.first_data_block, count);
        for (&s, &l) in self.runs.range(self.first_data_block..).chain([(&self.blocks, &0)]) {
            if left == 0 {
                break;
            }
            if s > at {
                let n = (s - at).min(left);
                pieces.push((at, n));
                left -= n;
            }
            at = at.max(s + l);
        }
        if left > 0 {
            return Err(Error::new(ErrorKind::NoSpace, "the image is too small for this file system"));
        }
        for &(s, n) in &pieces {
            self.mark(s, n, None);
        }
        Ok(pieces)
    }

    fn block(&mut self, n: u64) -> &mut [u8] {
        let bs = self.bs as usize;
        self.out.entry(n).or_insert_with(|| vec![0u8; bs])
    }

    fn put_bytes(&mut self, offset: u64, data: &[u8]) {
        let mut done = 0;
        while done < data.len() {
            let at = offset + done as u64;
            let (n, off) = (at / self.bs, (at % self.bs) as usize);
            let len = (self.bs as usize - off).min(data.len() - done);
            self.block(n)[off..off + len].copy_from_slice(&data[done..done + len]);
            done += len;
        }
    }

    /// Writes zeroes to `[b, b+n)` directly (large areas are not buffered).
    fn zero(&mut self, b: u64, n: u64) -> Result<()> {
        const CHUNK: u64 = 1 << 20;
        let zeroes = vec![0u8; CHUNK as usize];
        let (mut at, end) = (b * self.bs, (b + n) * self.bs);
        while at < end {
            let len = (end - at).min(CHUNK);
            self.dev.write_at(at, &zeroes[..len as usize])?;
            at += len;
        }
        Ok(())
    }

    fn inode_offset(&self, ino: u32) -> u64 {
        let g = ((ino - 1) / self.ipg) as usize;
        let index = ((ino - 1) % self.ipg) as u64;
        self.layout[g].inode_table * self.bs + index * INODE_SIZE as u64
    }

    fn inode(&mut self, ino: u32) -> Vec<u8> {
        let o = self.inode_offset(ino);
        let (n, off) = (o / self.bs, (o % self.bs) as usize);
        self.block(n)[off..off + INODE_SIZE].to_vec()
    }

    fn set_inode(&mut self, ino: u32, mut raw: Vec<u8>) {
        if self.variant.ext4() {
            let c = self.inode_csum(ino, &raw);
            put16(&mut raw, 124, c as u16);
            // The high half exists only in inodes with room for it.
            if u16::from_le_bytes([raw[128], raw[129]]) >= 4 {
                put16(&mut raw, 130, (c >> 16) as u16);
            }
        }
        let o = self.inode_offset(ino);
        self.put_bytes(o, &raw);
    }

    fn inode_seed(&self, ino: u32, generation: u32) -> u32 {
        crc32c(crc32c(self.csum_seed, &ino.to_le_bytes()), &generation.to_le_bytes())
    }

    /// Linux's ext4_inode_csum (checksum fields counted as zero; the high
    /// half only exists when `i_extra_isize` leaves room for it).
    fn inode_csum(&self, ino: u32, raw: &[u8]) -> u32 {
        let generation = u32::from_le_bytes(raw[100..104].try_into().unwrap());
        let mut c = crc32c(self.inode_seed(ino, generation), &raw[..124]);
        c = crc32c(c, &[0, 0]);
        c = crc32c(c, &raw[126..130]);
        let from = if u16::from_le_bytes([raw[128], raw[129]]) >= 4 {
            c = crc32c(c, &[0, 0]);
            132
        } else {
            130
        };
        crc32c(c, &raw[from..INODE_SIZE])
    }

    /// Map `blocks` (logical order) into an inode's `i_block`: an extent
    /// list in the inode (ext4) or direct/indirect pointers. Returns the
    /// mapping and the extra blocks used for indirect trees.
    fn map(&mut self, blocks: &[u64]) -> Result<([u8; 60], u64)> {
        let mut ib = [0u8; 60];
        if self.variant.ext4() {
            // Runs of consecutive blocks; at most 4 fit in the inode.
            let mut runs: Vec<(u32, u64, u32)> = Vec::new(); // (logical, start, len)
            for (i, &b) in blocks.iter().enumerate() {
                match runs.last_mut() {
                    Some((_, start, len)) if *start + *len as u64 == b && *len < 32768 => *len += 1,
                    _ => runs.push((i as u32, b, 1)),
                }
            }
            if runs.len() > 4 {
                return Err(Error::new(ErrorKind::NoSpace, "mkfs: a metadata file is too fragmented"));
            }
            put16(&mut ib, 0, 0xF30A); // magic
            put16(&mut ib, 2, runs.len() as u16); // entries
            put16(&mut ib, 4, 4); // max
            put16(&mut ib, 6, 0); // depth
            for (k, (logical, start, len)) in runs.iter().enumerate() {
                let o = 12 + 12 * k;
                put32(&mut ib, o, *logical);
                put16(&mut ib, o + 4, *len as u16);
                put16(&mut ib, o + 6, (start >> 32) as u16);
                put32(&mut ib, o + 8, *start as u32);
            }
            return Ok((ib, 0));
        }
        let per = self.bs / 4;
        let mut extra = 0u64;
        for (i, &b) in blocks.iter().take(12).enumerate() {
            put32(&mut ib, 4 * i, b as u32);
        }
        let mut rest = &blocks[blocks.len().min(12)..];
        for (level, slot) in [(1u32, 12usize), (2, 13), (3, 14)] {
            if rest.is_empty() {
                break;
            }
            let take = rest.len().min(per.pow(level) as usize);
            let root = self.tree(&rest[..take], level, &mut extra)?;
            put32(&mut ib, 4 * slot, root as u32);
            rest = &rest[take..];
        }
        Ok((ib, extra))
    }

    fn tree(&mut self, blocks: &[u64], level: u32, extra: &mut u64) -> Result<u64> {
        let per = self.bs / 4;
        let table = self.alloc(self.first_data_block, 1, None, None)?;
        self.block(table);
        *extra += 1;
        let child_span = per.pow(level - 1) as usize;
        for (i, chunk) in blocks.chunks(child_span).enumerate() {
            let entry = if level == 1 { chunk[0] } else { self.tree(chunk, level - 1, extra)? };
            put32(self.block(table), 4 * i, entry as u32);
        }
        Ok(table)
    }

    #[allow(clippy::too_many_arguments)]
    fn write_inode(&mut self, ino: u32, mode: u16, size: u64, links: u16, nblocks: u64, extra: u64, ib: [u8; 60]) {
        let mut raw = self.inode(ino);
        put16(&mut raw, 0, mode);
        put32(&mut raw, 4, size as u32);
        for o in [8, 12, 16] {
            put32(&mut raw, o, self.now);
        }
        put16(&mut raw, 26, links);
        let sectors = (nblocks + extra) * (self.bs / 512);
        put32(&mut raw, 28, sectors as u32);
        put16(&mut raw, 116, (sectors >> 32) as u16);
        if self.variant.ext4() {
            put32(&mut raw, 32, EXTENTS_FL);
        }
        raw[40..100].copy_from_slice(&ib);
        put32(&mut raw, 108, (size >> 32) as u32);
        put16(&mut raw, 128, EXTRA_ISIZE);
        put32(&mut raw, 0x90, self.now); // crtime
        self.set_inode(ino, raw);
    }

    /// A directory: its blocks with `entries` packed in (the first blocks),
    /// each block ending in a checksum tail with metadata_csum.
    fn write_dir(&mut self, ino: u32, entries: &[(&str, u32, u8)], nblocks: u64, links: u16, mode: u16) -> Result<()> {
        let bs = self.bs as usize;
        let tail = if self.variant.ext4() { 12 } else { 0 };
        let limit = bs - tail;
        let start = self.alloc(self.first_data_block, nblocks, None, None)?;
        let blocks: Vec<u64> = (start..start + nblocks).collect();
        let mut data = vec![0u8; bs * nblocks as usize];
        let mut off = 0;
        let mut last = 0;
        for (name, eino, kind) in entries {
            let len = (8 + name.len()).next_multiple_of(4);
            assert!(off + len <= limit, "mkfs: initial directory too large");
            put32(&mut data, off, *eino);
            put16(&mut data, off + 4, len as u16);
            data[off + 6] = name.len() as u8;
            data[off + 7] = *kind;
            data[off + 8..off + 8 + name.len()].copy_from_slice(name.as_bytes());
            last = off;
            off += len;
        }
        put16(&mut data, last + 4, (limit - last) as u16);
        // Other blocks: one empty entry each.
        for k in 1..nblocks as usize {
            put16(&mut data, k * bs + 4, limit as u16);
        }
        for (k, &blk) in blocks.iter().enumerate() {
            let b = &mut data[k * bs..(k + 1) * bs];
            if tail > 0 {
                let o = bs - 12;
                put16(b, o + 4, 12);
                b[o + 7] = 0xDE;
                let c = crc32c(self.inode_seed(ino, 0), &b[..bs - 12]);
                put32(b, o + 8, c);
            }
            self.block(blk).copy_from_slice(b);
        }
        let (ib, extra) = self.map(&blocks)?;
        self.write_inode(ino, 0o040000 | mode, data.len() as u64, links, nblocks, extra, ib);
        self.used_dirs[((ino - 1) / self.ipg) as usize] += 1;
        Ok(())
    }

    /// The journal: inode 8, `count` blocks, a jbd2 (v2) superblock first.
    fn write_journal(&mut self, count: u64) -> Result<[u8; 68]> {
        // In one piece where it fits (always with flex_bg), else in the
        // first free gaps (block-mapped, so any blocks will do).
        let pieces = match self.alloc(self.first_data_block, count, None, None) {
            Ok(start) => vec![(start, count)],
            Err(_) if !self.variant.ext4() => self.alloc_scattered(count)?,
            Err(e) => return Err(e),
        };
        let mut blocks = Vec::with_capacity(count as usize);
        for &(start, n) in &pieces {
            self.zero(start, n)?;
            blocks.extend(start..start + n);
        }
        let start = blocks[0];
        let mut jsb = vec![0u8; self.bs as usize];
        put_be32(&mut jsb, 0, 0xC03B_3998); // magic
        put_be32(&mut jsb, 4, 4); // superblock v2
        put_be32(&mut jsb, 12, self.bs as u32); // block size
        put_be32(&mut jsb, 16, count as u32); // maxlen
        put_be32(&mut jsb, 20, 1); // first log block
        put_be32(&mut jsb, 24, 1); // sequence
        put_be32(&mut jsb, 28, 0); // start: empty
        jsb[48..64].copy_from_slice(&self.uuid);
        put_be32(&mut jsb, 64, 1); // nr_users
        jsb[0x100..0x110].copy_from_slice(&self.uuid);
        if self.variant.ext4() {
            // Journal checksums (csum_v3, crc32c) from the start, and 64-bit
            // block numbers in its tags (the file system is 64bit). Linux and
            // the driver would set these at the first read-write mount anyway.
            put_be32(&mut jsb, 40, 0x10 | 0x2); // incompat: CSUM_V3 | 64BIT
            jsb[0x50] = 4; // checksum type: crc32c
            let c = crc32c(!0, &jsb[..0xFC]);
            let c = crc32c(c, &[0; 4]);
            let c = crc32c(c, &jsb[0x100..1024]);
            put_be32(&mut jsb, 0xFC, c);
        }
        self.block(start).copy_from_slice(&jsb);
        let (ib, extra) = self.map(&blocks)?;
        let size = count * self.bs;
        self.write_inode(JOURNAL_INO, 0o100600, size, 1, count, extra, ib);
        // The superblock's backup of the journal inode's i_block and size.
        let mut backup = [0u8; 68];
        backup[..60].copy_from_slice(&ib);
        put32(&mut backup, 60, (size >> 32) as u32);
        put32(&mut backup, 64, size as u32);
        Ok(backup)
    }

    fn desc_csum(&self, g: u64, d: &[u8]) -> u16 {
        let c = crc32c(self.csum_seed, &(g as u32).to_le_bytes());
        let c = crc32c(c, &d[..30]);
        let mut c = crc32c(c, &[0, 0]);
        if self.desc_size > 32 {
            c = crc32c(c, &d[32..self.desc_size]);
        }
        c as u16
    }

    /// Group `g`'s block bitmap (from the used runs; padding bits set).
    fn block_bitmap(&self, g: u64) -> Vec<u8> {
        let (start, len) = (self.group_start(g), self.group_len(g));
        let mut bm = vec![0u8; self.bs as usize];
        let mut set = |i: u64| bm[(i / 8) as usize] |= 1 << (i % 8);
        let first = self.runs.range(..start).next_back().filter(|(s, l)| *s + *l > start).map(|(s, l)| (*s, *l));
        for (s, l) in first.into_iter().chain(self.runs.range(start..start + len).map(|(s, l)| (*s, *l))) {
            for b in s.max(start)..(s + l).min(start + len) {
                set(b - start);
            }
        }
        for i in len..8 * self.bs {
            set(i);
        }
        bm
    }
}

/// Journal size for a file system of `blocks` blocks (mke2fs's defaults,
/// capped at one 32768-block extent; it is always contiguous).
fn journal_blocks(blocks: u64, variant: Variant) -> u64 {
    match blocks {
        // Without flex_bg no group has 32768 free blocks in a row.
        4194304.. if !variant.ext4() => 16384,
        0..32768 => 1024,
        32768..262144 => 4096,
        262144..524288 => 8192,
        524288..4194304 => 16384,
        _ => 32768,
    }
}

/// Creates an empty `variant` file system on `dev`.
pub fn mkfs(dev: &mut dyn BlockDevice, variant: Variant, label: &str) -> Result<()> {
    mkfs_with(dev, variant, label, &Options::default())
}

/// [`mkfs`] with [`Options`].
pub fn mkfs_with(dev: &mut dyn BlockDevice, variant: Variant, label: &str, opts: &Options) -> Result<()> {
    let size = dev.size();
    let bs = opts.block_size.unwrap_or(if size >= 512 * 1024 * 1024 { 4096 } else { 1024 });
    let min = if variant.journal() { MIN_SIZE_JOURNAL } else { MIN_SIZE };
    if size < min {
        return Err(Error::new(ErrorKind::NoSpace, format!("{variant:?} needs at least {} KiB", min / 1024)));
    }
    if ![1024, 2048, 4096].contains(&bs) {
        return Err(Error::invalid_input("block size must be 1024, 2048 or 4096"));
    }
    let wide = variant.ext4();
    let first_data_block = u64::from(bs == 1024);
    let mut blocks = size / bs;
    if !wide && blocks > u32::MAX as u64 {
        return Err(Error::invalid_input("ext2/ext3 are limited to 2^32 blocks (16 TiB): use ext4"));
    }
    if blocks >= 1 << 48 {
        return Err(Error::invalid_input("image too large"));
    }
    let bpg = 8 * bs;
    let mut groups = (blocks - first_data_block).div_ceil(bpg);
    let per_block = (bs as usize / INODE_SIZE) as u32;
    // mke2fs's inode ratios: one inode per 8 KiB ("floppy", < 3 MiB),
    // 4 KiB ("small", < 512 MiB), else 16 KiB; whole inode table blocks,
    // and fewer than 2^32 inodes in all.
    let ratio = match size {
        0..0x30_0000 => 8192,
        0x30_0000..0x2000_0000 => 4096,
        _ => 16384,
    };
    let total_inodes = (size / ratio).max(128);
    let ipg_for = |groups: u64| {
        let want = total_inodes.div_ceil(groups).min(u32::MAX as u64 / groups) as u32;
        want.max(32).next_multiple_of(per_block.max(8)).min(8 * bs as u32)
    };
    // A last group too small for its metadata and some data is left out.
    let tail = (blocks - first_data_block) % bpg;
    if groups > 1 && tail != 0 && tail < (ipg_for(groups) / per_block) as u64 + 64 {
        blocks -= tail;
        groups -= 1;
    }
    let ipg = ipg_for(groups);
    let desc_size = if wide { 64 } else { 32 };
    let gdt_blocks = (groups * desc_size as u64).div_ceil(bs);
    // Without meta_bg every superblock copy has the whole descriptor table
    // after it, in its group.
    let fits = 1 + gdt_blocks + 64 <= bpg;
    let meta_bg = opts.meta_bg.unwrap_or(!fits);
    if !fits && !meta_bg {
        return Err(Error::invalid_input("too many block groups for this block size without meta_bg"));
    }
    let per_desc_block = bs / desc_size as u64;
    let now = dix_core::time::unix_now() as u32;
    let random = |salt: u64| {
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&super::kernel::arch::entropy().wrapping_add(salt).to_le_bytes());
        b[8..].copy_from_slice(&super::kernel::arch::entropy().to_le_bytes());
        b
    };
    let mut uuid = random(1);
    uuid[6] = (uuid[6] & 0x0f) | 0x40; // version 4
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    let mut fs = Fs {
        dev,
        variant,
        bs,
        blocks,
        first_data_block,
        bpg,
        ipg,
        desc_size,
        itable_blocks: ipg as u64 * INODE_SIZE as u64 / bs,
        runs: BTreeMap::new(),
        layout: vec![GroupLayout::default(); groups as usize],
        used_inodes: vec![0; groups as usize],
        used_dirs: vec![0; groups as usize],
        out: BTreeMap::new(),
        now,
        uuid,
        csum_seed: crc32c(!0, &uuid),
    };

    // Superblock copies (sparse_super) and descriptor tables: after each
    // copy, or (meta_bg) one block in three groups of those it describes.
    for g in (0..groups).filter(|&g| has_super(g)) {
        let start = fs.group_start(g);
        fs.mark(start, if meta_bg { 1 } else { 1 + gdt_blocks }, Some(g));
    }
    if meta_bg {
        for m in 0..gdt_blocks {
            for g in meta_bg_copies(m, per_desc_block, groups) {
                let at = fs.group_start(g) + u64::from(has_super(g));
                fs.mark(at, 1, Some(g));
            }
        }
    }
    // Bitmaps and inode tables: per group, or packed per flex group.
    let flex = if variant.ext4() { 1u64 << LOG_GROUPS_PER_FLEX } else { 1 };
    for f in (0..groups).step_by(flex as usize) {
        let members = f..(f + flex).min(groups);
        let from = fs.group_start(f);
        // Without flex_bg everything must stay inside the group.
        let limit = (flex == 1).then(|| fs.group_start(f) + fs.group_len(f));
        for g in members.clone() {
            fs.layout[g as usize].block_bitmap = fs.alloc(from, 1, limit, Some(g))?;
        }
        for g in members.clone() {
            fs.layout[g as usize].inode_bitmap = fs.alloc(from, 1, limit, Some(g))?;
        }
        for g in members {
            let n = fs.itable_blocks;
            fs.layout[g as usize].inode_table = fs.alloc(from, n, limit, Some(g))?;
        }
    }
    // Inode tables: written (zeroed) where initialised. ext4 initialises
    // only the first group's; the others are INODE_UNINIT (lazy init).
    let init_tables = if variant.ext4() { 1 } else { groups };
    for g in 0..init_tables {
        let (t, n) = (fs.layout[g as usize].inode_table, fs.itable_blocks);
        fs.zero(t, n)?;
    }

    // Inodes 1..=11 are taken: reserved ones, the root, lost+found.
    fs.used_inodes[0] = FIRST_INO;
    let journal_backup =
        if variant.journal() { Some(fs.write_journal(journal_blocks(blocks, variant))?) } else { None };
    fs.write_dir(ROOT_INO, &[(".", ROOT_INO, 2), ("..", ROOT_INO, 2), ("lost+found", LOST_FOUND_INO, 2)], 1, 3, 0o755)?;
    // lost+found gets 16 KiB of room, so e2fsck never needs to grow it.
    let lf_blocks = (16384 / bs).max(1);
    fs.write_dir(LOST_FOUND_INO, &[(".", LOST_FOUND_INO, 2), ("..", ROOT_INO, 2)], lf_blocks, 2, 0o700)?;
    if variant.ext4() {
        // Checksums for the unused reserved inodes too (they are "in use").
        for ino in (1..FIRST_INO).filter(|&i| i != ROOT_INO && i != JOURNAL_INO) {
            let raw = fs.inode(ino);
            fs.set_inode(ino, raw);
        }
    }

    // Bitmaps and group descriptors.
    let mut gdt = vec![0u8; (gdt_blocks * bs) as usize];
    let (mut free_blocks, mut free_inodes) = (0u64, 0u64);
    for g in 0..groups {
        let gl = fs.layout[g as usize];
        let len = fs.group_len(g);
        let gfree = len - gl.used;
        let used_ino = fs.used_inodes[g as usize];
        let ifree = (ipg - used_ino) as u64;
        free_blocks += gfree;
        free_inodes += ifree;
        let mut flags = 0u16;
        let mut itable_unused = 0u32;
        let mut ics = 0u32;
        if variant.ext4() {
            // Uninitialised unless something is in it (as mke2fs: never
            // the last group, never a group holding others' metadata).
            if used_ino == 0 {
                flags |= BG_INODE_UNINIT;
            } else {
                flags |= BG_INODE_ZEROED;
            }
            itable_unused = ipg - used_ino;
            if !gl.foreign && g != groups - 1 && g != 0 {
                flags |= BG_BLOCK_UNINIT;
            }
        }
        // (For BLOCK_UNINIT groups: the bitmap the driver and Linux build.)
        let bm = fs.block_bitmap(g);
        let bcs = crc32c(fs.csum_seed, &bm[..(bpg / 8) as usize]);
        if flags & BG_BLOCK_UNINIT == 0 {
            fs.block(gl.block_bitmap).copy_from_slice(&bm);
        }
        if flags & BG_INODE_UNINIT == 0 {
            let mut bm = vec![0u8; bs as usize];
            for bit in 0..8 * bs as u32 {
                if bit < used_ino || bit >= ipg {
                    bm[(bit / 8) as usize] |= 1 << (bit % 8);
                }
            }
            ics = crc32c(fs.csum_seed, &bm[..(ipg / 8) as usize]);
            fs.block(gl.inode_bitmap).copy_from_slice(&bm);
        }
        let ds = desc_size;
        let d = &mut gdt[g as usize * ds..(g as usize + 1) * ds];
        put32(d, 0, gl.block_bitmap as u32);
        put32(d, 4, gl.inode_bitmap as u32);
        put32(d, 8, gl.inode_table as u32);
        put16(d, 12, gfree as u16);
        put16(d, 14, ifree as u16);
        put16(d, 16, fs.used_dirs[g as usize] as u16);
        put16(d, 18, flags);
        put16(d, 28, itable_unused as u16);
        if variant.ext4() {
            put16(d, 24, bcs as u16);
            put16(d, 26, ics as u16);
        }
        if wide {
            put32(d, 0x20, (gl.block_bitmap >> 32) as u32);
            put32(d, 0x24, (gl.inode_bitmap >> 32) as u32);
            put32(d, 0x28, (gl.inode_table >> 32) as u32);
            put16(d, 0x2C, (gfree >> 16) as u16);
            put16(d, 0x2E, (ifree >> 16) as u16);
            put16(d, 0x30, (fs.used_dirs[g as usize] >> 16) as u16);
            put16(d, 0x32, (itable_unused >> 16) as u16);
            put16(d, 0x38, (bcs >> 16) as u16);
            put16(d, 0x3A, (ics >> 16) as u16);
        }
        if variant.ext4() {
            let c = fs.desc_csum(g, &gdt[g as usize * ds..(g as usize + 1) * ds]);
            put16(&mut gdt[g as usize * ds..(g as usize + 1) * ds], 30, c);
        }
    }

    // The superblock.
    let mut sb = [0u8; 1024];
    put32(&mut sb, 0, ipg * groups as u32);
    put32(&mut sb, 4, blocks as u32);
    put32(&mut sb, 12, free_blocks as u32);
    put32(&mut sb, 16, free_inodes as u32);
    put32(&mut sb, 20, first_data_block as u32);
    let log = (bs / 1024).trailing_zeros();
    put32(&mut sb, 24, log);
    put32(&mut sb, 28, log);
    put32(&mut sb, 32, bpg as u32);
    put32(&mut sb, 36, bpg as u32);
    put32(&mut sb, 40, ipg);
    put32(&mut sb, 48, now); // write time
    put16(&mut sb, 54, 0xffff); // max mount count: no forced checks
    put16(&mut sb, 56, 0xef53);
    put16(&mut sb, 58, 1); // clean
    put16(&mut sb, 60, 1); // errors: continue
    put32(&mut sb, 64, now); // last check
    put32(&mut sb, 76, 1); // revision 1
    put32(&mut sb, 84, FIRST_INO);
    put16(&mut sb, 88, INODE_SIZE as u16);
    let (mut compat, mut incompat, mut ro_compat) =
        (0, INCOMPAT_FILETYPE, RO_COMPAT_SPARSE_SUPER | RO_COMPAT_LARGE_FILE);
    if variant.journal() {
        compat |= COMPAT_HAS_JOURNAL | COMPAT_DIR_INDEX | COMPAT_EXT_ATTR;
        put32(&mut sb, 0xE0, JOURNAL_INO);
    }
    if variant.ext4() {
        incompat |= INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG;
        ro_compat |= RO_COMPAT_HUGE_FILE | RO_COMPAT_DIR_NLINK | RO_COMPAT_EXTRA_ISIZE | RO_COMPAT_METADATA_CSUM;
        put16(&mut sb, 0xFE, desc_size as u16);
        put32(&mut sb, 0x150, (blocks >> 32) as u32);
        put32(&mut sb, 0x158, (free_blocks >> 32) as u32);
        put16(&mut sb, 0x15C, EXTRA_ISIZE); // min extra isize
        put16(&mut sb, 0x15E, EXTRA_ISIZE); // want extra isize
        sb[0x174] = LOG_GROUPS_PER_FLEX as u8;
        sb[0x175] = 1; // checksum type: crc32c
    }
    if meta_bg {
        incompat |= INCOMPAT_META_BG;
        put32(&mut sb, 0x104, 0); // first meta_bg: all of them
    }
    put32(&mut sb, 92, compat);
    put32(&mut sb, 96, incompat);
    put32(&mut sb, 100, ro_compat);
    sb[104..120].copy_from_slice(&uuid);
    let label = label.as_bytes();
    sb[0x78..0x78 + label.len().min(16)].copy_from_slice(&label[..label.len().min(16)]);
    // Directory hashing (dir_index): a seed, half-MD4, signed characters
    // (what mke2fs records on x86).
    sb[0xEC..0xFC].copy_from_slice(&random(2));
    sb[0xFC] = 1;
    put32(&mut sb, 0x160, 1);
    put32(&mut sb, 0x108, now); // mkfs time
    if let Some(backup) = journal_backup {
        sb[0xFD] = 1; // journal backup: the inode's blocks
        sb[0x10C..0x10C + 68].copy_from_slice(&backup);
    }
    // Out it goes: runs of consecutive blocks in one write each.
    let out = std::mem::take(&mut fs.out);
    let mut run: Vec<u8> = Vec::new();
    let mut run_start = 0u64;
    let mut prev: Option<u64> = None;
    for (n, data) in out {
        if prev.is_some_and(|p| p + 1 != n) || run.len() >= 4 << 20 {
            fs.dev.write_at(run_start * bs, &run)?;
            run.clear();
        }
        if run.is_empty() {
            run_start = n;
        }
        run.extend_from_slice(&data);
        prev = Some(n);
    }
    if !run.is_empty() {
        fs.dev.write_at(run_start * bs, &run)?;
    }
    for g in (0..groups).filter(|&g| has_super(g)) {
        put16(&mut sb, 90, g as u16);
        if variant.ext4() {
            let c = crc32c(!0, &sb[..0x3FC]);
            put32(&mut sb, 0x3FC, c);
        }
        let start = fs.group_start(g);
        // Group 0's superblock is at byte 1024, whatever the block size.
        // A whole block each, so nothing old is left around it. (Straight
        // to the device: the descriptor tables of a huge file system are
        // large, and there are dozens of copies.)
        let mut block = vec![0u8; bs as usize];
        let at = if g == 0 && bs > 1024 { 1024 } else { 0 };
        block[at..at + 1024].copy_from_slice(&sb);
        fs.dev.write_at(start * bs, &block)?;
        if !meta_bg {
            fs.dev.write_at((start + 1) * bs, &gdt)?;
        }
    }
    if meta_bg {
        for m in 0..gdt_blocks {
            let data = &gdt[(m * bs) as usize..((m + 1) * bs) as usize];
            for g in meta_bg_copies(m, per_desc_block, groups) {
                let at = fs.group_start(g) + u64::from(has_super(g));
                fs.dev.write_at(at * bs, data)?;
            }
        }
    }

    fs.dev.flush()
}
