//! Extent trees: where an ext4 file's blocks are.
//!
//! The root is in the inode's block array (60 bytes: a header and up to
//! four entries); deeper nodes are blocks (a header, entries, and with
//! `metadata_csum` a checksum after the last possible entry). Index nodes
//! point to nodes one level down, by first logical block; leaves hold
//! extents (logical block, length, disk block). An extent longer than
//! 32768 is unwritten (preallocated): its blocks read as zeroes.
//!
//! Allocation grows the extent before the new block when they are
//! consecutive on the disk (a file written in order keeps few extents);
//! otherwise a new extent is inserted. A full node splits at the
//! insertion point (appending fills nodes completely), a full root moves
//! into a new block and the tree gets deeper. Writing into an unwritten
//! extent splits it, the written block zeroed first.

use alloc::vec;
use alloc::vec::Vec;

use super::inode::{DiskInode, EXTENTS_FL};
use super::{Ext4Fs, Meta, csum, le16, le32, put16, put32};
use crate::ext4::kernel::errno::{Errno, KResult};

const MAGIC: u16 = 0xF30A;
/// Longest initialised and unwritten extents.
const INIT_MAX: u32 = 32768;
const MAX_DEPTH: usize = 5;

#[derive(Clone, Copy, Debug)]
struct Extent {
    block: u32,
    len: u32,
    start: u64,
    unwritten: bool,
}

fn count(n: &[u8]) -> usize {
    le16(n, 2) as usize
}

fn capacity(n: &[u8]) -> usize {
    le16(n, 4) as usize
}

fn depth(n: &[u8]) -> u16 {
    le16(n, 6)
}

fn set_header(n: &mut [u8], entries: usize, max: usize, depth: u16) {
    put16(n, 0, MAGIC);
    put16(n, 2, entries as u16);
    put16(n, 4, max as u16);
    put16(n, 6, depth);
}

fn key(n: &[u8], i: usize) -> u32 {
    le32(n, 12 + 12 * i)
}

fn get_ext(n: &[u8], i: usize) -> Extent {
    let o = 12 + 12 * i;
    let raw = le16(n, o + 4) as u32;
    let (len, unwritten) = if raw > INIT_MAX { (raw - INIT_MAX, true) } else { (raw, false) };
    Extent { block: le32(n, o), len, start: le32(n, o + 8) as u64 | (le16(n, o + 6) as u64) << 32, unwritten }
}

fn encode_ext(e: Extent) -> [u8; 12] {
    let mut r = [0u8; 12];
    put32(&mut r, 0, e.block);
    put16(&mut r, 4, if e.unwritten { e.len + INIT_MAX } else { e.len } as u16);
    put16(&mut r, 6, (e.start >> 32) as u16);
    put32(&mut r, 8, e.start as u32);
    r
}

fn put_ext(n: &mut [u8], i: usize, e: Extent) {
    n[12 + 12 * i..24 + 12 * i].copy_from_slice(&encode_ext(e));
}

fn child(n: &[u8], i: usize) -> u64 {
    let o = 12 + 12 * i;
    le32(n, o + 4) as u64 | (le16(n, o + 8) as u64) << 32
}

fn encode_index(key: u32, child: u64) -> [u8; 12] {
    let mut r = [0u8; 12];
    put32(&mut r, 0, key);
    put32(&mut r, 4, child as u32);
    put16(&mut r, 8, (child >> 32) as u16);
    r
}

fn raw_entry(n: &[u8], i: usize) -> [u8; 12] {
    n[12 + 12 * i..24 + 12 * i].try_into().unwrap()
}

fn insert_raw(n: &mut [u8], pos: usize, raw: [u8; 12]) {
    let c = count(n);
    n.copy_within(12 + 12 * pos..12 + 12 * c, 24 + 12 * pos);
    n[12 + 12 * pos..24 + 12 * pos].copy_from_slice(&raw);
    put16(n, 2, (c + 1) as u16);
}

fn remove_raw(n: &mut [u8], pos: usize) {
    let c = count(n);
    n.copy_within(24 + 12 * pos..12 + 12 * c, 12 + 12 * pos);
    n[12 * c..12 * c + 12].fill(0);
    put16(n, 2, (c - 1) as u16);
}

fn root_bytes(d: &DiskInode) -> Vec<u8> {
    d.block.iter().flat_map(|b| b.to_le_bytes()).collect()
}

fn set_root(d: &mut DiskInode, n: &[u8]) {
    for (i, b) in d.block.iter_mut().enumerate() {
        *b = le32(n, 4 * i);
    }
}

/// An empty tree: the file uses extents from now on.
pub fn init_root(d: &mut DiskInode) {
    let mut n = vec![0u8; 60];
    set_header(&mut n, 0, 4, 0);
    set_root(d, &n);
    d.flags |= EXTENTS_FL;
}

/// A node on the way from the root to a leaf: where it is (`None`: the
/// root, in the inode), its bytes, and which entry the way took.
struct Step {
    block: Option<u64>,
    data: Vec<u8>,
    at: usize,
}

impl Ext4Fs {
    /// Entries a node block holds (the checksum fits after them).
    fn node_capacity(&self) -> usize {
        (self.block_size - 12) / 12
    }

    fn node_csum(&self, ino: u32, generation: u32, n: &[u8]) -> u32 {
        csum::crc32c(self.inode_seed(ino, generation), &n[..12 + 12 * capacity(n)])
    }

    /// Node block `block` of inode `ino`, checked: magic, sizes, `depth`,
    /// checksum.
    fn read_node(&self, ino: u32, generation: u32, block: u64, want_depth: u16) -> KResult<Vec<u8>> {
        let n = self.cache.read(block, |b| b.to_vec())?;
        let ok = le16(&n, 0) == MAGIC
            && capacity(&n) > 0
            && count(&n) <= capacity(&n)
            && 12 + 12 * capacity(&n) <= self.block_size
            && depth(&n) == want_depth;
        if !ok {
            return super::corrupt(
                &self.disk_name,
                format_args!("inode #{ino}: bad extent tree node {block}"),
                Errno::EUCLEAN,
            );
        }
        let off = 12 + 12 * capacity(&n);
        if self.csum && off + 4 <= self.block_size && le32(&n, off) != self.node_csum(ino, generation, &n) {
            return super::corrupt(
                &self.disk_name,
                format_args!("inode #{ino}: extent tree block {block}: checksum invalid"),
                Errno::EBADMSG,
            );
        }
        Ok(n)
    }

    fn write_node(&self, ino: u32, generation: u32, block: u64, n: &[u8]) -> KResult<()> {
        let mut data = n.to_vec();
        let off = 12 + 12 * capacity(n);
        if self.csum && off + 4 <= self.block_size {
            let c = self.node_csum(ino, generation, n);
            put32(&mut data, off, c);
        }
        self.cache.modify(block, |b| b.copy_from_slice(&data))
    }

    fn write_step(&self, ino: u32, d: &mut DiskInode, s: &Step) -> KResult<()> {
        match s.block {
            None => {
                set_root(d, &s.data);
                Ok(())
            }
            Some(b) => self.write_node(ino, d.generation, b, &s.data),
        }
    }

    /// The way from the root to the leaf that holds (or would hold)
    /// logical block `lblock`.
    fn ext_path(&self, ino: u32, d: &DiskInode, lblock: u64) -> KResult<Vec<Step>> {
        let root = root_bytes(d);
        if le16(&root, 0) != MAGIC || count(&root) > capacity(&root) || capacity(&root) > 4 {
            return super::corrupt(
                &self.disk_name,
                format_args!("inode #{ino}: bad extent tree root"),
                Errno::EUCLEAN,
            );
        }
        let mut path = vec![Step { block: None, data: root, at: 0 }];
        loop {
            let n = &path.last().unwrap().data;
            let dep = depth(n);
            if dep == 0 {
                return Ok(path);
            }
            let c = count(n);
            if c == 0 || path.len() > MAX_DEPTH {
                return super::corrupt(
                    &self.disk_name,
                    format_args!("inode #{ino}: bad extent index"),
                    Errno::EUCLEAN,
                );
            }
            let mut i = 0;
            while i + 1 < c && key(n, i + 1) as u64 <= lblock {
                i += 1;
            }
            let next = child(n, i);
            path.last_mut().unwrap().at = i;
            let data = self.read_node(ino, d.generation, next, dep - 1)?;
            path.push(Step { block: Some(next), data, at: 0 });
        }
    }

    pub(super) fn ext_bmap(
        &self,
        meta: &mut Meta,
        ino: u32,
        d: &mut DiskInode,
        lblock: u64,
        create: bool,
    ) -> KResult<u64> {
        if lblock >= 0xffff_ffff {
            return Err(Errno::EFBIG);
        }
        let mut path = self.ext_path(ino, d, lblock)?;
        let leaf = &path.last().unwrap().data;
        let n = count(leaf);
        if let Some(i) = (0..n).find(|&i| {
            let e = get_ext(leaf, i);
            (e.block as u64..e.block as u64 + e.len as u64).contains(&lblock)
        }) {
            let e = get_ext(leaf, i);
            if !e.unwritten {
                return Ok(e.start + (lblock - e.block as u64));
            }
            if !create {
                return Ok(0);
            }
            return self.ext_convert(meta, ino, d, path, i, lblock);
        }
        if !create {
            return Ok(0);
        }
        // Next to the extent before it on the disk, if it can be.
        let pos = (0..n).find(|&i| key(leaf, i) as u64 > lblock).unwrap_or(n);
        let left = (pos > 0).then(|| get_ext(leaf, pos - 1));
        let near = left.map(|l| l.start + (lblock - l.block as u64));
        let p = self.alloc_block(meta, self.group_of(ino), near)?;
        d.blocks += self.sectors_per_block();
        if let Some(l) = left
            && !l.unwritten
            && l.block as u64 + l.len as u64 == lblock
            && l.start + l.len as u64 == p
            && l.len < INIT_MAX
        {
            let step = path.last_mut().unwrap();
            put_ext(&mut step.data, pos - 1, Extent { len: l.len + 1, ..l });
            let step = path.pop().unwrap();
            self.write_step(ino, d, &step)?;
            return Ok(p);
        }
        if let Err(e) =
            self.ext_insert(meta, ino, d, Extent { block: lblock as u32, len: 1, start: p, unwritten: false })
        {
            // No room in the tree for it (no block for a new node): the
            // block goes back, so that a failed write leaks nothing.
            self.free_blocks(meta, p, 1)?;
            d.blocks = d.blocks.saturating_sub(self.sectors_per_block());
            return Err(e);
        }
        Ok(p)
    }

    /// Write into unwritten extent `i` of the leaf at the end of `path`, at
    /// `lblock`: that block is zeroed and becomes initialised (merged into
    /// the extent before when it can), the rest stays unwritten.
    fn ext_convert(
        &self,
        meta: &mut Meta,
        ino: u32,
        d: &mut DiskInode,
        mut path: Vec<Step>,
        i: usize,
        lblock: u64,
    ) -> KResult<u64> {
        let mut leaf = path.pop().unwrap();
        let e = get_ext(&leaf.data, i);
        let lb = lblock as u32;
        let p = e.start + (lb - e.block) as u64;
        // Its old contents must never show.
        let zeroes = vec![0u8; self.block_size];
        self.cache.disk().write_bytes(p * self.block_size as u64, &zeroes)?;
        let init = |block: u32, len: u32, start: u64| Extent { block, len, start, unwritten: false };
        let end = e.block + e.len;
        if e.len == 1 {
            put_ext(&mut leaf.data, i, Extent { unwritten: false, ..e });
            return self.write_step(ino, d, &leaf).map(|_| p);
        }
        if lb == e.block {
            if i > 0 {
                let l = get_ext(&leaf.data, i - 1);
                if !l.unwritten && l.block + l.len == lb && l.start + l.len as u64 == p && l.len < INIT_MAX {
                    put_ext(&mut leaf.data, i - 1, Extent { len: l.len + 1, ..l });
                    put_ext(
                        &mut leaf.data,
                        i,
                        Extent { block: lb + 1, len: e.len - 1, start: p + 1, unwritten: true },
                    );
                    return self.write_step(ino, d, &leaf).map(|_| p);
                }
            }
            put_ext(&mut leaf.data, i, init(lb, 1, p));
            self.write_step(ino, d, &leaf)?;
            self.ext_insert(
                meta,
                ino,
                d,
                Extent { block: lb + 1, len: e.len - 1, start: p + 1, unwritten: true },
            )?;
            return Ok(p);
        }
        if lb == end - 1 {
            put_ext(&mut leaf.data, i, Extent { len: e.len - 1, ..e });
            self.write_step(ino, d, &leaf)?;
            self.ext_insert(meta, ino, d, init(lb, 1, p))?;
            return Ok(p);
        }
        put_ext(&mut leaf.data, i, Extent { len: lb - e.block, ..e });
        self.write_step(ino, d, &leaf)?;
        self.ext_insert(meta, ino, d, init(lb, 1, p))?;
        self.ext_insert(
            meta,
            ino,
            d,
            Extent { block: lb + 1, len: end - lb - 1, start: p + 1, unwritten: true },
        )?;
        Ok(p)
    }

    /// Insert extent `e` (it overlaps none).
    fn ext_insert(&self, meta: &mut Meta, ino: u32, d: &mut DiskInode, e: Extent) -> KResult<()> {
        let mut path = self.ext_path(ino, d, e.block as u64)?;
        let level = path.len() - 1;
        let leaf = &path[level].data;
        let pos = (0..count(leaf)).find(|&i| key(leaf, i) > e.block).unwrap_or(count(leaf));
        self.insert_entry(meta, ino, d, &mut path, level, pos, encode_ext(e))
    }

    /// Insert entry `raw` at `pos` of node `level` of `path`, splitting
    /// full nodes on the way up (and growing the tree at the root).
    #[allow(clippy::too_many_arguments)]
    fn insert_entry(
        &self,
        meta: &mut Meta,
        ino: u32,
        d: &mut DiskInode,
        path: &mut Vec<Step>,
        level: usize,
        pos: usize,
        raw: [u8; 12],
    ) -> KResult<()> {
        let spb = self.sectors_per_block();
        let n = count(&path[level].data);
        if n < capacity(&path[level].data) {
            insert_raw(&mut path[level].data, pos, raw);
            self.write_step(ino, d, &path[level])?;
            if pos == 0 && level > 0 {
                self.fix_keys(ino, d, path, level)?;
            }
            return Ok(());
        }
        if level == 0 {
            // The root is full: its entries move to a new block below it.
            let nb = self.alloc_block(meta, self.group_of(ino), None)?;
            d.blocks += spb;
            let root = &path[0].data;
            let dep = depth(root);
            let mut node = vec![0u8; self.block_size];
            set_header(&mut node, n, self.node_capacity(), dep);
            node[12..12 + 12 * n].copy_from_slice(&root[12..12 + 12 * n]);
            let mut new_root = vec![0u8; 60];
            set_header(&mut new_root, 1, 4, dep + 1);
            new_root[12..24].copy_from_slice(&encode_index(key(root, 0), nb));
            let at = path[0].at;
            path[0] = Step { block: None, data: new_root, at: 0 };
            path.insert(1, Step { block: Some(nb), data: node, at });
            self.write_step(ino, d, &path[1])?;
            self.write_step(ino, d, &path[0])?;
            return self.insert_entry(meta, ino, d, path, 1, pos, raw);
        }
        // A full block: split at `pos`, the new entry ending the left part
        // (or, appended, starting the right one).
        let nb = self.alloc_block(meta, self.group_of(ino), None)?;
        d.blocks += spb;
        let all: Vec<[u8; 12]> = (0..n).map(|i| raw_entry(&path[level].data, i)).collect();
        let (left, right) = if pos == n {
            (all, vec![raw])
        } else {
            let mut l = all[..pos].to_vec();
            l.push(raw);
            (l, all[pos..].to_vec())
        };
        let dep = depth(&path[level].data);
        let cap = self.node_capacity();
        let mut rnode = vec![0u8; self.block_size];
        set_header(&mut rnode, right.len(), cap, dep);
        for (i, r) in right.iter().enumerate() {
            rnode[12 + 12 * i..24 + 12 * i].copy_from_slice(r);
        }
        let lnode = &mut path[level].data;
        lnode[12..].fill(0);
        set_header(lnode, left.len(), cap, dep);
        for (i, r) in left.iter().enumerate() {
            lnode[12 + 12 * i..24 + 12 * i].copy_from_slice(r);
        }
        self.write_step(ino, d, &path[level])?;
        self.write_node(ino, d.generation, nb, &rnode)?;
        if pos == 0 {
            self.fix_keys(ino, d, path, level)?;
        }
        let parent = level - 1;
        let ppos = path[parent].at + 1;
        self.insert_entry(meta, ino, d, path, parent, ppos, encode_index(key(&rnode, 0), nb))
    }

    /// Node `level`'s first key changed: the index entries leading to it
    /// (up to where it is not the first) follow.
    fn fix_keys(&self, ino: u32, d: &mut DiskInode, path: &mut [Step], level: usize) -> KResult<()> {
        let k = key(&path[level].data, 0);
        for l in (0..level).rev() {
            let at = path[l].at;
            put32(&mut path[l].data, 12 + 12 * at, k);
            self.write_step(ino, d, &path[l])?;
            if at != 0 {
                break;
            }
        }
        Ok(())
    }

    /// Free the blocks from logical block `keep` on (and the nodes left
    /// empty).
    pub(super) fn ext_truncate(
        &self,
        meta: &mut Meta,
        ino: u32,
        d: &mut DiskInode,
        keep: u64,
    ) -> KResult<()> {
        let mut root = root_bytes(d);
        if le16(&root, 0) != MAGIC {
            return super::corrupt(
                &self.disk_name,
                format_args!("inode #{ino}: bad extent tree root"),
                Errno::EUCLEAN,
            );
        }
        let generation = d.generation;
        self.trim(meta, ino, generation, d, &mut root, keep)?;
        if count(&root) == 0 {
            set_header(&mut root, 0, 4, 0);
        }
        set_root(d, &root);
        Ok(())
    }

    fn trim(
        &self,
        meta: &mut Meta,
        ino: u32,
        generation: u32,
        d: &mut DiskInode,
        n: &mut [u8],
        keep: u64,
    ) -> KResult<()> {
        let spb = self.sectors_per_block();
        let dep = depth(n);
        let mut i = count(n);
        while i > 0 {
            i -= 1;
            if dep == 0 {
                let e = get_ext(n, i);
                let (eb, el) = (e.block as u64, e.len as u64);
                if eb >= keep {
                    self.free_blocks(meta, e.start, el)?;
                    d.blocks = d.blocks.saturating_sub(el * spb);
                    remove_raw(n, i);
                    continue;
                }
                if eb + el > keep {
                    let cut = eb + el - keep;
                    self.free_blocks(meta, e.start + (keep - eb), cut)?;
                    d.blocks = d.blocks.saturating_sub(cut * spb);
                    put_ext(n, i, Extent { len: (keep - eb) as u32, ..e });
                }
                break;
            }
            let c = child(n, i);
            if key(n, i) as u64 >= keep {
                self.free_subtree(meta, ino, generation, d, c, dep - 1)?;
                remove_raw(n, i);
                continue;
            }
            let mut cn = self.read_node(ino, generation, c, dep - 1)?;
            self.trim(meta, ino, generation, d, &mut cn, keep)?;
            if count(&cn) == 0 {
                self.free_blocks(meta, c, 1)?;
                d.blocks = d.blocks.saturating_sub(spb);
                remove_raw(n, i);
            } else {
                self.write_node(ino, generation, c, &cn)?;
            }
            break;
        }
        Ok(())
    }

    fn free_subtree(
        &self,
        meta: &mut Meta,
        ino: u32,
        generation: u32,
        d: &mut DiskInode,
        block: u64,
        dep: u16,
    ) -> KResult<()> {
        let spb = self.sectors_per_block();
        let n = self.read_node(ino, generation, block, dep)?;
        for i in 0..count(&n) {
            if dep == 0 {
                let e = get_ext(&n, i);
                self.free_blocks(meta, e.start, e.len as u64)?;
                d.blocks = d.blocks.saturating_sub(e.len as u64 * spb);
            } else {
                self.free_subtree(meta, ino, generation, d, child(&n, i), dep - 1)?;
            }
        }
        self.free_blocks(meta, block, 1)?;
        d.blocks = d.blocks.saturating_sub(spb);
        Ok(())
    }
}
