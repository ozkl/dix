//! Block maps (ext2 and ext3 files, and ext4 files without extents): 12
//! direct blocks, then single, double and triple indirect blocks of 32-bit
//! block numbers. And the entry points that pick between them and extents.

use alloc::vec::Vec;

use super::inode::DiskInode;
use super::{Ext4Fs, Meta, le32, put32};
use crate::ext4::kernel::errno::{Errno, KResult};

const DIRECT: u64 = 12;

impl Ext4Fs {
    /// The disk block of logical block `lblock` of inode `ino` (`d`); 0: a
    /// hole (or, for reading, an unwritten extent). With `create`, a
    /// missing block (and the metadata to map it) is allocated and `d`
    /// updated (the caller writes it back).
    pub(super) fn bmap(
        &self,
        meta: &mut Meta,
        ino: u32,
        d: &mut DiskInode,
        lblock: u64,
        create: bool,
    ) -> KResult<u64> {
        if d.uses_extents() {
            self.ext_bmap(meta, ino, d, lblock, create)
        } else {
            self.ind_bmap(meta, ino, d, lblock, create)
        }
    }

    /// Free the blocks of inode `ino` from logical block `keep` on.
    pub(super) fn truncate_blocks(
        &self,
        meta: &mut Meta,
        ino: u32,
        d: &mut DiskInode,
        keep: u64,
    ) -> KResult<()> {
        if d.uses_extents() {
            self.ext_truncate(meta, ino, d, keep)
        } else {
            self.ind_truncate(meta, d, keep)
        }
    }

    /// How many blocks a file can have.
    pub(super) fn max_blocks(&self, d: &DiskInode) -> u64 {
        if d.uses_extents() {
            // (Linux's EXT_MAX_BLOCKS.)
            0xffff_ffff
        } else {
            let per = self.per_block();
            (DIRECT + per + per * per + per * per * per).min(0xffff_ffff)
        }
    }

    fn per_block(&self) -> u64 {
        (self.block_size / 4) as u64
    }

    fn ind_bmap(
        &self,
        meta: &mut Meta,
        ino: u32,
        d: &mut DiskInode,
        lblock: u64,
        create: bool,
    ) -> KResult<u64> {
        let per = self.per_block();
        let spb = self.sectors_per_block();
        let goal = self.group_of(ino);
        if lblock < DIRECT {
            let slot = &mut d.block[lblock as usize];
            if *slot == 0 && create {
                *slot = self.alloc_block32(meta, goal)?;
                d.blocks += spb;
            }
            return Ok(*slot as u64);
        }
        // Which tree (single, double, triple indirect) and the index in it.
        let mut rest = lblock - DIRECT;
        let mut level = 1;
        let mut span = per;
        while rest >= span {
            rest -= span;
            level += 1;
            span *= per;
            if level > 3 {
                return Err(Errno::EFBIG);
            }
        }
        let root = d.block[11 + level];
        let root = if root != 0 {
            root
        } else {
            if !create {
                return Ok(0);
            }
            let b = self.alloc_block32(meta, goal)?;
            self.cache.zero(b as u64)?;
            d.block[11 + level] = b;
            d.blocks += spb;
            b
        };
        let mut table = root;
        for l in (0..level).rev() {
            let index = ((rest / per.pow(l as u32)) % per) as usize;
            let mut entry = self.cache.read(table as u64, |b| le32(b, 4 * index))?;
            if entry == 0 {
                if !create {
                    return Ok(0);
                }
                entry = self.alloc_block32(meta, goal)?;
                d.blocks += spb;
                if l > 0 {
                    self.cache.zero(entry as u64)?;
                }
                self.cache.modify(table as u64, |b| put32(b, 4 * index, entry))?;
            }
            table = entry;
        }
        Ok(table as u64)
    }

    /// A block for a block map, whose numbers are 32 bits.
    fn alloc_block32(&self, meta: &mut Meta, goal: usize) -> KResult<u32> {
        let b = self.alloc_block(meta, goal, None)?;
        u32::try_from(b).map_err(|_| Errno::EFBIG)
    }

    fn ind_truncate(&self, meta: &mut Meta, d: &mut DiskInode, keep: u64) -> KResult<()> {
        let spb = self.sectors_per_block();
        for i in keep.min(DIRECT)..DIRECT {
            let b = core::mem::take(&mut d.block[i as usize]);
            if b != 0 {
                self.free_blocks(meta, b as u64, 1)?;
                d.blocks -= spb;
            }
        }
        let per = self.per_block();
        let mut base = DIRECT;
        let mut span = per;
        for level in 1..=3u32 {
            let slot = 11 + level as usize;
            if d.block[slot] != 0 {
                if keep <= base {
                    let b = core::mem::take(&mut d.block[slot]);
                    self.free_tree(meta, d, b, level)?;
                } else if keep < base + span {
                    self.free_partial(meta, d, d.block[slot], level, keep - base)?;
                }
            }
            base += span;
            span *= per;
        }
        Ok(())
    }

    /// Free indirect block `block` (of depth `level`) and everything below.
    fn free_tree(&self, meta: &mut Meta, d: &mut DiskInode, block: u32, level: u32) -> KResult<()> {
        if level > 0 {
            let entries: Vec<u32> = self.cache.read(block as u64, |b| {
                (0..self.block_size / 4).map(|i| le32(b, 4 * i)).filter(|&e| e != 0).collect()
            })?;
            for e in entries {
                self.free_tree(meta, d, e, level - 1)?;
            }
        }
        self.free_blocks(meta, block as u64, 1)?;
        d.blocks -= self.sectors_per_block();
        Ok(())
    }

    /// In indirect block `table` of depth `level`, free what maps logical
    /// blocks `keep..` (relative to the table).
    fn free_partial(
        &self,
        meta: &mut Meta,
        d: &mut DiskInode,
        table: u32,
        level: u32,
        keep: u64,
    ) -> KResult<()> {
        let per = self.per_block();
        let child_span = per.pow(level - 1);
        let entries: Vec<u32> =
            self.cache.read(table as u64, |b| (0..self.block_size / 4).map(|i| le32(b, 4 * i)).collect())?;
        for (i, &e) in entries.iter().enumerate() {
            let first = i as u64 * child_span;
            if e == 0 || first + child_span <= keep {
                continue;
            }
            if first >= keep {
                self.free_tree(meta, d, e, level - 1)?;
                self.cache.modify(table as u64, |b| put32(b, 4 * i, 0))?;
            } else if level > 1 {
                self.free_partial(meta, d, e, level - 1, keep - first)?;
            }
        }
        Ok(())
    }
}
