//! Hashed directory indexes (htree, `dir_index`), as Linux keeps them.
//!
//! Block 0 of an indexed directory is the root: "." and ".." (the latter
//! over the rest of the block), the root info (hash version, levels), then
//! index entries (hash, logical block), sorted by hash: the first's hash
//! slot holds the limit and count. Below it, 0 to 2 levels of index nodes
//! (an empty entry over the whole block, then entries the same way), then
//! leaves: ordinary directory blocks, each holding the names whose hashes
//! lie between its entry's hash and the next one's. With
//! `metadata_csum`, root and nodes end in a `dx_tail` with a crc32c.
//!
//! A name goes into the leaf its hash leads to. A full leaf splits: its
//! entries are sorted by hash and the upper ones (about half the bytes)
//! move to a new block, whose index entry starts at the first moved hash
//! (plus 1 when equal hashes straddle the split, the collision bit). A
//! full node splits in half, a full root gets another level (two levels at
//! most, three with `largedir`). A linear directory of one full block
//! becomes indexed (Linux's make_indexed_dir). Removing names leaves the
//! index as it is, as Linux does.

use alloc::vec;
use alloc::vec::Vec;

use super::inode::{DiskInode, INDEX_FL};
use super::{Ext4Fs, Meta, csum, le16, le32, put16, put32};
use crate::ext4::kernel::errno::{Errno, KResult};
use crate::ext4::kernel::fs::vfs::FileType;

pub const COMPAT_DIR_INDEX: u32 = 0x0020;

// ----- hashes (Linux's fs/ext4/hash.c) ---------------------------------------

const DEFAULT_SEED: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

fn tea_transform(buf: &mut [u32; 4], inp: &[u32]) {
    let (mut sum, mut b0, mut b1) = (0u32, buf[0], buf[1]);
    let (a, b, c, d) = (inp[0], inp[1], inp[2], inp[3]);
    for _ in 0..16 {
        sum = sum.wrapping_add(0x9E37_79B9);
        b0 = b0.wrapping_add((b1 << 4).wrapping_add(a) ^ b1.wrapping_add(sum) ^ (b1 >> 5).wrapping_add(b));
        b1 = b1.wrapping_add((b0 << 4).wrapping_add(c) ^ b0.wrapping_add(sum) ^ (b0 >> 5).wrapping_add(d));
    }
    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}

/// A round of half MD4: its function, constant, and (word, shift) of
/// each step.
type Round<'a> = (&'a dyn Fn(u32, u32, u32) -> u32, u32, [(usize, u32); 8]);

fn half_md4_transform(buf: &mut [u32; 4], inp: &[u32]) {
    let f = |x: u32, y: u32, z: u32| z ^ (x & (y ^ z));
    let g = |x: u32, y: u32, z: u32| (x & y).wrapping_add((x ^ y) & z);
    let h = |x: u32, y: u32, z: u32| x ^ y ^ z;
    let (mut a, mut b, mut c, mut d) = (buf[0], buf[1], buf[2], buf[3]);
    // (function, constant, [(word, shift)] for a, d, c, b in turn)
    let rounds: [Round; 3] = [
        (&f, 0, [(0, 3), (1, 7), (2, 11), (3, 19), (4, 3), (5, 7), (6, 11), (7, 19)]),
        (&g, 0o13240474631, [(1, 3), (3, 5), (5, 9), (7, 13), (0, 3), (2, 5), (4, 9), (6, 13)]),
        (&h, 0o15666365641, [(3, 3), (7, 9), (2, 11), (6, 15), (1, 3), (5, 9), (0, 11), (4, 15)]),
    ];
    for (func, k, steps) in rounds {
        for (j, &(i, s)) in steps.iter().enumerate() {
            let x = inp[i].wrapping_add(k);
            match j % 4 {
                0 => a = a.wrapping_add(func(b, c, d)).wrapping_add(x).rotate_left(s),
                1 => d = d.wrapping_add(func(a, b, c)).wrapping_add(x).rotate_left(s),
                2 => c = c.wrapping_add(func(d, a, b)).wrapping_add(x).rotate_left(s),
                _ => b = b.wrapping_add(func(c, d, a)).wrapping_add(x).rotate_left(s),
            }
        }
    }
    buf[0] = buf[0].wrapping_add(a);
    buf[1] = buf[1].wrapping_add(b);
    buf[2] = buf[2].wrapping_add(c);
    buf[3] = buf[3].wrapping_add(d);
}

/// The name's bytes as `num` words (Linux's str2hashbuf).
fn str2hashbuf(msg: &[u8], num: usize, signed: bool) -> [u32; 8] {
    let len = msg.len() as u32;
    let mut pad = len | (len << 8);
    pad |= pad << 16;
    let mut val = pad;
    let mut out = [0u32; 8];
    let mut n = 0;
    for (i, &c) in msg.iter().take(num * 4).enumerate() {
        let c = if signed { c as i8 as i32 as u32 } else { c as u32 };
        val = c.wrapping_add(val << 8);
        if i % 4 == 3 {
            out[n] = val;
            n += 1;
            val = pad;
        }
    }
    if n < num {
        out[n] = val;
        n += 1;
    }
    while n < num {
        out[n] = pad;
        n += 1;
    }
    out
}

fn legacy_hash(name: &[u8], signed: bool) -> u32 {
    let (mut h0, mut h1) = (0x12a3_fe2du32, 0x37ab_e8f9u32);
    for &c in name {
        let c = if signed { c as i8 as i32 as u32 } else { c as u32 };
        let mut h = h1.wrapping_add(h0 ^ c.wrapping_mul(7_152_373));
        if h & 0x8000_0000 != 0 {
            h = h.wrapping_sub(0x7fff_ffff);
        }
        h1 = h0;
        h0 = h;
    }
    h0 << 1
}

/// The hash of `name` with hash version `version` (0-2: legacy, half_md4,
/// tea; 3-5 the same with unsigned characters) and `seed`; `None`: a
/// version this doesn't know.
pub fn dx_hash(version: u8, seed: &[u32; 4], name: &[u8]) -> Option<u32> {
    dx_hashes(version, seed, name).map(|h| h.0)
}

/// The (major, minor) hashes of `name` (legacy: no minor hash).
fn dx_hashes(version: u8, seed: &[u32; 4], name: &[u8]) -> Option<(u32, u32)> {
    let mut buf = if seed.iter().any(|&w| w != 0) { *seed } else { DEFAULT_SEED };
    let signed = version < 3;
    let (hash, minor) = match version % 3 {
        _ if version > 5 => return None,
        0 => (legacy_hash(name, signed), 0),
        1 => {
            let mut p = name;
            loop {
                half_md4_transform(&mut buf, &str2hashbuf(p, 8, signed));
                if p.len() <= 32 {
                    break;
                }
                p = &p[32..];
            }
            (buf[1], buf[2])
        }
        _ => {
            let mut p = name;
            loop {
                tea_transform(&mut buf, &str2hashbuf(p, 4, signed));
                if p.len() <= 16 {
                    break;
                }
                p = &p[16..];
            }
            (buf[0], buf[1])
        }
    };
    let hash = hash & !1;
    // (The end-of-directory cookie is never a hash.)
    Some((if hash == 0xffff_fffe { 0xffff_fffc } else { hash }, minor))
}

// ----- index blocks ----------------------------------------------------------

/// An index block on the way from the root to a leaf.
struct Frame {
    pb: u64,
    data: Vec<u8>,
    /// Where the limit and count are (32 in the root, 8 in a node).
    off: usize,
    /// The entry taken.
    at: usize,
}

fn count(f: &Frame) -> usize {
    le16(&f.data, f.off + 2) as usize
}

fn limit(f: &Frame) -> usize {
    le16(&f.data, f.off) as usize
}

fn hash_at(b: &[u8], off: usize, i: usize) -> u32 {
    if i == 0 { 0 } else { le32(b, off + 8 * i) }
}

fn block_at(b: &[u8], off: usize, i: usize) -> u32 {
    le32(b, off + 8 * i + 4)
}

fn set_entry(b: &mut [u8], off: usize, i: usize, hash: u32, block: u32) {
    if i > 0 {
        put32(b, off + 8 * i, hash);
    }
    put32(b, off + 8 * i + 4, block);
}

/// Insert entry (`hash`, `block`) at `pos` (after the first) of a frame
/// with room.
fn insert_at(f: &mut Frame, pos: usize, hash: u32, block: u32) {
    let (off, n) = (f.off, count(f));
    f.data.copy_within(off + 8 * pos..off + 8 * n, off + 8 * (pos + 1));
    set_entry(&mut f.data, off, pos, hash, block);
    put16(&mut f.data, off + 2, (n + 1) as u16);
}

/// A listed entry: (inode, file type, name, position).
type Listed = (u32, u8, Vec<u8>, u64);

/// A live entry of a leaf being split: (hash, inode, file type, name).
type Moved = (u32, u32, u8, Vec<u8>);

impl Ext4Fs {
    fn dx_tail(&self) -> usize {
        if self.csum { 8 } else { 0 }
    }

    fn dx_root_limit(&self) -> usize {
        (self.block_size - 32 - self.dx_tail()) / 8
    }

    fn dx_node_limit(&self) -> usize {
        (self.block_size - 8 - self.dx_tail()) / 8
    }

    /// Levels of index nodes a tree may have below its root.
    fn dx_max_levels(&self) -> u8 {
        if self.has_incompat(super::INCOMPAT_LARGEDIR) { 2 } else { 1 }
    }

    /// Where the limit and count are, if `b` is an index root or node
    /// (Linux's get_dx_countlimit).
    pub(super) fn dx_count_offset(&self, b: &[u8]) -> Option<usize> {
        let bs = self.block_size;
        let rec = le16(b, 4) as usize;
        if rec == bs && le32(b, 0) == 0 {
            Some(8)
        } else if rec == 12 && le16(b, 16) as usize == bs - 12 && le32(b, 24) == 0 && b[29] == 8 {
            Some(32)
        } else {
            None
        }
    }

    /// An index block's checksum (Linux's ext4_dx_csum).
    fn dx_csum(&self, ino: u32, generation: u32, b: &[u8], off: usize) -> Option<u32> {
        let (lim, n) = (le16(b, off) as usize, le16(b, off + 2) as usize);
        let tail = off + 8 * lim;
        if n > lim || tail + 8 > b.len() {
            return None;
        }
        let c = csum::crc32c(self.inode_seed(ino, generation), &b[..off + 8 * n]);
        let c = csum::crc32c(c, &b[tail..tail + 4]);
        Some(csum::crc32c(c, &[0; 4]))
    }

    pub(super) fn set_dx_csum(&self, ino: u32, generation: u32, b: &mut [u8], off: usize) {
        if let Some(c) = self.dx_csum(ino, generation, b, off) {
            let tail = off + 8 * le16(b, off) as usize;
            put32(b, tail + 4, c);
        }
    }

    pub(super) fn dx_csum_ok(&self, ino: u32, generation: u32, b: &[u8], off: usize) -> bool {
        match self.dx_csum(ino, generation, b, off) {
            Some(c) => c == le32(b, off + 8 * le16(b, off) as usize + 4),
            None => false,
        }
    }

    /// The hash version a directory's names are hashed with (its root's,
    /// unsigned where the file system says so).
    fn dx_version(&self, root: &[u8]) -> u8 {
        let v = root[28];
        if v <= 2 { v + self.hash_unsigned } else { v }
    }

    fn dx_bad<T>(&self, dir: u32, what: &str) -> KResult<T> {
        super::corrupt(&self.disk_name, format_args!("inode #{dir}: htree: {what}"), Errno::EUCLEAN)
    }

    fn dx_frame(&self, meta: &mut Meta, dir: u32, d: &mut DiskInode, lb: u64) -> KResult<Frame> {
        let pb = self.bmap(meta, dir, d, lb, false)?;
        if pb == 0 {
            return self.dx_bad(dir, "index block missing");
        }
        let data = self.cache.read(pb, |b| b.to_vec())?;
        let Some(off) = self.dx_count_offset(&data) else { return self.dx_bad(dir, "not an index block") };
        let want = if lb == 0 { 32 } else { 8 };
        let max = if off == 32 { self.dx_root_limit() } else { self.dx_node_limit() };
        let f = Frame { pb, data, off, at: 0 };
        if off != want || limit(&f) != max || count(&f) == 0 || count(&f) > limit(&f) {
            return self.dx_bad(dir, "bad count or limit");
        }
        if self.csum && !self.dx_csum_ok(dir, d.generation, &f.data, off) {
            return super::corrupt(
                &self.disk_name,
                format_args!("inode #{dir}: htree block {lb}: checksum invalid"),
                Errno::EBADMSG,
            );
        }
        Ok(f)
    }

    /// The way from the root to the leaf of `hash` (Linux's dx_probe).
    fn dx_probe(&self, meta: &mut Meta, dir: u32, d: &mut DiskInode, hash: u32) -> KResult<Vec<Frame>> {
        let mut frames = vec![self.dx_frame(meta, dir, d, 0)?];
        let levels = frames[0].data[30];
        if levels > self.dx_max_levels() {
            return self.dx_bad(dir, "too many levels");
        }
        loop {
            let depth = frames.len();
            let f = frames.last_mut().unwrap();
            let n = count(f);
            let mut at = 0;
            while at + 1 < n && hash_at(&f.data, f.off, at + 1) <= hash {
                at += 1;
            }
            f.at = at;
            if depth > levels as usize {
                return Ok(frames);
            }
            let next = block_at(&f.data, f.off, at) as u64;
            let node = self.dx_frame(meta, dir, d, next)?;
            frames.push(node);
        }
    }

    fn dx_write(&self, dir: u32, generation: u32, f: &Frame) -> KResult<()> {
        let mut data = f.data.clone();
        if self.csum {
            self.set_dx_csum(dir, generation, &mut data, f.off);
        }
        self.cache.modify(f.pb, |b| b.copy_from_slice(&data))
    }

    /// A new block at the end of directory `dir`: (logical, disk block).
    fn dir_append(&self, meta: &mut Meta, dir: u32, d: &mut DiskInode) -> KResult<(u64, u64)> {
        let lb = d.size / self.block_size as u64;
        let pb = self.bmap(meta, dir, d, lb, true)?;
        self.cache.zero(pb)?;
        d.size += self.block_size as u64;
        Ok((lb, pb))
    }

    /// Room for one more entry in `frames[level]`: a full node splits
    /// (its parent made room first), a full root gets a level below it.
    fn dx_make_room(
        &self,
        meta: &mut Meta,
        dir: u32,
        d: &mut DiskInode,
        frames: &mut Vec<Frame>,
        level: usize,
    ) -> KResult<()> {
        if count(&frames[level]) < limit(&frames[level]) {
            return Ok(());
        }
        let generation = d.generation;
        if level == 0 {
            let levels = frames[0].data[30];
            if levels >= self.dx_max_levels() {
                kinfo!("EXT4-fs warning ({}): inode #{dir}: Directory index full!", self.disk_name);
                return Err(Errno::ENOSPC);
            }
            // The root's entries move to a new node under it.
            let (lb, pb) = self.dir_append(meta, dir, d)?;
            let n = count(&frames[0]);
            let mut node = Frame { pb, data: vec![0u8; self.block_size], off: 8, at: frames[0].at };
            put16(&mut node.data, 4, self.block_size as u16);
            put16(&mut node.data, 8, self.dx_node_limit() as u16);
            put16(&mut node.data, 10, n as u16);
            for i in 0..n {
                let (h, b) = (hash_at(&frames[0].data, 32, i), block_at(&frames[0].data, 32, i));
                set_entry(&mut node.data, 8, i, h, b);
            }
            let root = &mut frames[0];
            put16(&mut root.data, 34, 1);
            set_entry(&mut root.data, 32, 0, 0, lb as u32);
            root.data[30] = levels + 1;
            root.at = 0;
            self.dx_write(dir, generation, &node)?;
            self.dx_write(dir, generation, &frames[0])?;
            frames.insert(1, node);
            return Ok(());
        }
        // A node: its parent gets the entry for the upper half.
        let before = frames.len();
        self.dx_make_room(meta, dir, d, frames, level - 1)?;
        let level = level + (frames.len() - before);
        let (lb, pb) = self.dir_append(meta, dir, d)?;
        let n = count(&frames[level]);
        let half = n / 2;
        let mut upper = Frame { pb, data: vec![0u8; self.block_size], off: 8, at: 0 };
        put16(&mut upper.data, 4, self.block_size as u16);
        put16(&mut upper.data, 8, self.dx_node_limit() as u16);
        put16(&mut upper.data, 10, (n - half) as u16);
        let key = hash_at(&frames[level].data, 8, half);
        for i in half..n {
            let (h, b) = (hash_at(&frames[level].data, 8, i), block_at(&frames[level].data, 8, i));
            set_entry(&mut upper.data, 8, i - half, if i == half { 0 } else { h }, b);
        }
        put16(&mut frames[level].data, 10, half as u16);
        let parent_at = frames[level - 1].at;
        insert_at(&mut frames[level - 1], parent_at + 1, key, lb as u32);
        self.dx_write(dir, generation, &frames[level - 1])?;
        if frames[level].at >= half {
            upper.at = frames[level].at - half;
            self.dx_write(dir, generation, &frames[level])?;
            frames[level] = upper;
            frames[level - 1].at += 1;
        } else {
            self.dx_write(dir, generation, &upper)?;
        }
        self.dx_write(dir, generation, &frames[level])
    }

    /// The live entries of leaf block `b`, hashed.
    fn dx_entries(&self, version: u8, b: &[u8]) -> Vec<Moved> {
        self.entries(b)
            .filter(|e| e.1 != 0)
            .map(|(_, ino, _, _, kind, name)| {
                (dx_hash(version, &self.hash_seed, name).unwrap_or(0), ino, kind, name.to_vec())
            })
            .collect()
    }

    /// Leaf block contents of `entries`, packed.
    fn dx_pack(&self, entries: &[Moved]) -> Vec<u8> {
        let (bs, limit) = (self.block_size, self.dir_limit());
        let mut b = vec![0u8; bs];
        let mut off = 0;
        for (i, (_, ino, kind, name)) in entries.iter().enumerate() {
            let need = (8 + name.len()).next_multiple_of(4);
            let rec = if i + 1 == entries.len() { limit - off } else { need };
            put32(&mut b, off, *ino);
            put16(&mut b, off + 4, rec as u16);
            b[off + 6] = name.len() as u8;
            b[off + 7] = *kind;
            b[off + 8..off + 8 + name.len()].copy_from_slice(name);
            off += rec;
        }
        if entries.is_empty() {
            put16(&mut b, 4, limit as u16);
        }
        if self.csum {
            super::dir::write_tail(&mut b);
        }
        b
    }

    /// Look `name` up in indexed directory `dir` (Linux's
    /// ext4_dx_find_entry): the leaf its hash leads to, then, while the
    /// next leaf starts with the same hash (a collision split them), that
    /// one. (inode, block holding the entry).
    pub(super) fn dx_find(
        &self,
        meta: &mut Meta,
        dir: u32,
        d: &mut DiskInode,
        name: &str,
    ) -> KResult<Option<(u32, u64)>> {
        let root = self.dx_frame(meta, dir, d, 0)?;
        let Some(hash) = dx_hash(self.dx_version(&root.data), &self.hash_seed, name.as_bytes()) else {
            return self.dx_bad(dir, "unknown hash version");
        };
        let mut frames = self.dx_probe(meta, dir, d, hash)?;
        loop {
            let leaf = {
                let f = frames.last().unwrap();
                block_at(&f.data, f.off, f.at) as u64
            };
            let pb = self.bmap(meta, dir, d, leaf, false)?;
            if pb == 0 {
                return self.dx_bad(dir, "leaf block missing");
            }
            let b = self.cache.read(pb, |b| b.to_vec())?;
            if !self.dir_block_ok(dir, d.generation, &b) {
                return super::corrupt(
                    &self.disk_name,
                    format_args!("inode #{dir}: directory block {leaf}: checksum invalid"),
                    Errno::EBADMSG,
                );
            }
            if let Some(ino) = self.entries(&b).find(|e| e.1 != 0 && e.5 == name.as_bytes()).map(|e| e.1) {
                return Ok(Some((ino, pb)));
            }
            // The next leaf, if it continues this hash (htree_next_block).
            let mut level = frames.len() - 1;
            while frames[level].at + 1 >= count(&frames[level]) {
                if level == 0 {
                    return Ok(None);
                }
                level -= 1;
            }
            frames[level].at += 1;
            let f = &frames[level];
            if hash_at(&f.data, f.off, f.at) & !1 != hash {
                return Ok(None);
            }
            for l in level + 1..frames.len() {
                let up = &frames[l - 1];
                let child = block_at(&up.data, up.off, up.at) as u64;
                frames[l] = self.dx_frame(meta, dir, d, child)?;
            }
        }
    }

    /// The names of indexed directory `dir` in hash order (Linux's
    /// ext4_dx_readdir), each with the position Linux gives it: the major
    /// hash (without its lowest bit) and the minor one (hash2pos).
    /// (inode, file type, name, position); "." and ".." are left out.
    pub(super) fn dx_list(&self, meta: &mut Meta, dir: u32, d: &mut DiskInode) -> KResult<Vec<Listed>> {
        let root = self.dx_frame(meta, dir, d, 0)?;
        let version = self.dx_version(&root.data);
        if version > 5 {
            return self.dx_bad(dir, "unknown hash version");
        }
        let mut out = Vec::new();
        self.scan_dir(meta, dir, d, |_, b| {
            for (_, ino, _, _, kind, name) in self.entries(b) {
                if ino == 0 || name == b"." || name == b".." {
                    continue;
                }
                let (major, minor) = dx_hashes(version, &self.hash_seed, name).unwrap_or((0, 0));
                out.push(((major as u64 >> 1) << 32 | minor as u64, ino, kind, name.to_vec()));
            }
            None::<()>
        })?;
        out.sort();
        Ok(out.into_iter().map(|(pos, ino, kind, name)| (ino, kind, name, pos)).collect())
    }

    /// Add `name` -> `ino` to indexed directory `dir`: into its leaf, split
    /// when full. Returns the leaf block that holds it now.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn dx_add(
        &self,
        meta: &mut Meta,
        dir: u32,
        d: &mut DiskInode,
        name: &str,
        ino: u32,
        kind: FileType,
    ) -> KResult<u64> {
        let root = self.dx_frame(meta, dir, d, 0)?;
        let version = self.dx_version(&root.data);
        let Some(hash) = dx_hash(version, &self.hash_seed, name.as_bytes()) else {
            return self.dx_bad(dir, "unknown hash version");
        };
        let mut frames = self.dx_probe(meta, dir, d, hash)?;
        let leaf_lb = {
            let f = frames.last().unwrap();
            block_at(&f.data, f.off, f.at) as u64
        };
        let leaf_pb = self.bmap(meta, dir, d, leaf_lb, false)?;
        if leaf_pb == 0 {
            return self.dx_bad(dir, "leaf block missing");
        }
        if self.dir_insert(dir, d.generation, leaf_pb, name, ino, kind)? {
            return Ok(leaf_pb);
        }
        // The leaf is full: room in its index node first, then the split.
        let last = frames.len() - 1;
        self.dx_make_room(meta, dir, d, &mut frames, last)?;
        let generation = d.generation;
        let old = self.cache.read(leaf_pb, |b| b.to_vec())?;
        let mut map = self.dx_entries(version, &old);
        map.sort_by_key(|e| e.0);
        // Where to cut the names (in hash order): where the ones before
        // reach half of all their bytes; when that falls inside a run of
        // equal hashes, at the nearer end of the run instead, if that still
        // leaves each block between a quarter and three quarters of the
        // bytes (else the run straddles the cut: the collision bit below).
        let n = map.len();
        if n < 2 {
            return self.dx_bad(dir, "cannot split a leaf");
        }
        let bytes: Vec<usize> = map.iter().map(|e| (8 + e.3.len()).next_multiple_of(4)).collect();
        let total: usize = bytes.iter().sum();
        // before[k]: the bytes of the names before position k.
        let before: Vec<usize> = core::iter::once(0)
            .chain(bytes.iter().scan(0, |sum, &b| {
                *sum += b;
                Some(*sum)
            }))
            .collect();
        let mut split = (1..n).find(|&k| 2 * before[k] >= total).unwrap_or(n - 1);
        if map[split - 1].0 == map[split].0 {
            let boundary = |k: &usize| map[*k - 1].0 != map[*k].0;
            let balanced = |k: &usize| 4 * before[*k] >= total && 4 * before[*k] <= 3 * total;
            let later = (split + 1..n).find(boundary).filter(balanced);
            let earlier = (1..split).rev().find(boundary).filter(balanced);
            split = match (earlier, later) {
                (Some(e), Some(l)) => {
                    if split - e <= l - split {
                        e
                    } else {
                        l
                    }
                }
                (Some(k), None) | (None, Some(k)) => k,
                (None, None) => split,
            };
        }
        let hash2 = map[split].0;
        let continued = (map[split - 1].0 == hash2) as u32;
        let (new_lb, new_pb) = self.dir_append(meta, dir, d)?;
        let (low, high) = map.split_at(split);
        let (low_block, high_block) = (self.dx_pack(low), self.dx_pack(high));
        for (pb, data) in [(leaf_pb, &low_block), (new_pb, &high_block)] {
            self.cache.modify(pb, |b| {
                b.copy_from_slice(data);
                self.set_dir_csum(dir, generation, b);
            })?;
        }
        let f = frames.last_mut().unwrap();
        let at = f.at;
        insert_at(f, at + 1, hash2 + continued, new_lb as u32);
        self.dx_write(dir, generation, frames.last().unwrap())?;
        // Where the moved names are now.
        if let Some(index) = meta.dirs.get_mut(&dir) {
            for (_, e_ino, _, e_name) in high {
                index.names.insert(e_name.clone(), (*e_ino, new_pb));
            }
            index.room.insert(leaf_pb, self.block_room(&low_block));
            index.room.insert(new_pb, self.block_room(&high_block));
        }
        let target = if hash >= hash2 { new_pb } else { leaf_pb };
        if !self.dir_insert(dir, generation, target, name, ino, kind)? {
            return self.dx_bad(dir, "no room after a split");
        }
        Ok(target)
    }

    /// Make linear directory `dir`, one full block, an indexed one: its
    /// entries move to a new block 1, block 0 becomes the root.
    pub(super) fn dx_make_indexed(&self, meta: &mut Meta, dir: u32, d: &mut DiskInode) -> KResult<bool> {
        let pb0 = self.bmap(meta, dir, d, 0, false)?;
        if pb0 == 0 {
            return Ok(false);
        }
        let b0 = self.cache.read(pb0, |b| b.to_vec())?;
        let ents: Vec<_> = self.entries(&b0).map(|e| (e.0, e.2, e.5.to_vec())).collect();
        if ents.len() < 2 || ents[0].2 != b"." || ents[0].1 != 12 || ents[1].2 != b".." || ents[1].0 != 12 {
            return Ok(false);
        }
        let start = 12 + ents[1].1;
        let limit = self.dir_limit();
        let generation = d.generation;
        let (_, pb1) = self.dir_append(meta, dir, d)?;
        // Block 1: the entries after "..", the last one up to the end.
        let mut b1 = vec![0u8; self.block_size];
        let len = limit - start;
        b1[..len].copy_from_slice(&b0[start..limit]);
        let mut last = 0;
        let mut off = 0;
        while off < len {
            let rec = le16(&b1, off + 4) as usize;
            if rec < 8 {
                return self.dx_bad(dir, "bad entry while indexing");
            }
            last = off;
            off += rec;
        }
        put16(&mut b1, last + 4, (limit - last) as u16);
        if self.csum {
            super::dir::write_tail(&mut b1);
        }
        self.cache.modify(pb1, |b| {
            b.copy_from_slice(&b1);
            self.set_dir_csum(dir, generation, b);
        })?;
        // Block 0: the root.
        let mut root = b0[..24].to_vec();
        root.resize(self.block_size, 0);
        put16(&mut root, 16, (self.block_size - 12) as u16);
        root[28] = self.def_hash_version;
        root[29] = 8;
        put16(&mut root, 32, self.dx_root_limit() as u16);
        put16(&mut root, 34, 1);
        set_entry(&mut root, 32, 0, 0, 1);
        let f = Frame { pb: pb0, data: root, off: 32, at: 0 };
        self.dx_write(dir, generation, &f)?;
        d.flags |= INDEX_FL;
        meta.dirs.remove(&dir);
        Ok(true)
    }
}
