//! The buffer cache: fixed-size blocks of a disk in memory, for a file
//! system's metadata (superblock, bitmaps, inode tables, directories,
//! indirect blocks). File contents go through the page cache instead.
//!
//! Changed blocks are written back by [`BufCache::sync`] (or when evicted).
//! A block the file system frees must be [`BufCache::forget`]ten, so that a
//! stale copy is never written over the block's next use.
//!
//! A journaling file system [`BufCache::hold_dirty`]: changed blocks are
//! then never written by eviction (they must reach the journal first);
//! it logs [`BufCache::dirty_blocks`], then writes them in place with
//! `sync`.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::Disk;
use crate::ext4::kernel::errno::KResult;
use crate::ext4::kernel::sync::Mutex;

/// Cached blocks at most (clean ones are dropped beyond this).
const MAX_BUFFERS: usize = 4096;

struct Buf {
    data: Box<[u8]>,
    dirty: bool,
    last_use: u64,
}

/// Bytes of all buffer caches (`Buffers` in `/proc/meminfo`).
static BUFFER_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Bytes the buffer caches hold.
pub fn buffer_bytes() -> usize {
    BUFFER_BYTES.load(Ordering::Relaxed)
}

impl Drop for Buf {
    fn drop(&mut self) {
        BUFFER_BYTES.fetch_sub(self.data.len(), Ordering::Relaxed);
    }
}

struct Inner {
    bufs: BTreeMap<u64, Buf>,
    clock: u64,
}

pub struct BufCache {
    disk: Arc<Disk>,
    block_size: usize,
    inner: Mutex<Inner>,
    /// Changed blocks stay until `sync` (a journal logs them first).
    hold_dirty: AtomicBool,
}

impl BufCache {
    pub fn new(disk: Arc<Disk>, block_size: usize) -> Self {
        Self {
            disk,
            block_size,
            inner: Mutex::new(Inner { bufs: BTreeMap::new(), clock: 0 }),
            hold_dirty: AtomicBool::new(false),
        }
    }

    pub fn disk(&self) -> &Arc<Disk> {
        &self.disk
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// The cached buffer of `block`, read from the disk if needed.
    fn buf<'a>(&self, inner: &'a mut Inner, block: u64, read: bool) -> KResult<&'a mut Buf> {
        inner.clock += 1;
        let clock = inner.clock;
        if !inner.bufs.contains_key(&block) {
            if inner.bufs.len() >= MAX_BUFFERS {
                self.evict(inner)?;
            }
            let mut data = vec![0u8; self.block_size].into_boxed_slice();
            if read {
                self.disk.read_bytes(block * self.block_size as u64, &mut data)?;
            }
            BUFFER_BYTES.fetch_add(data.len(), Ordering::Relaxed);
            inner.bufs.insert(block, Buf { data, dirty: !read, last_use: clock });
        }
        let buf = inner.bufs.get_mut(&block).unwrap();
        buf.last_use = clock;
        Ok(buf)
    }

    /// Drop the least recently used quarter of the buffers, writing the
    /// dirty ones first (held ones stay).
    fn evict(&self, inner: &mut Inner) -> KResult<()> {
        let hold = self.hold_dirty.load(Ordering::Relaxed);
        let mut by_age: Vec<(u64, u64)> =
            inner.bufs.iter().filter(|(_, buf)| !(hold && buf.dirty)).map(|(&b, buf)| (buf.last_use, b)).collect();
        by_age.sort_unstable();
        for &(_, block) in by_age.iter().take(MAX_BUFFERS / 4) {
            let buf = inner.bufs.get(&block).unwrap();
            if buf.dirty {
                self.disk.write_bytes(block * self.block_size as u64, &buf.data)?;
            }
            inner.bufs.remove(&block);
        }
        Ok(())
    }

    /// Look at `block`.
    pub fn read<R>(&self, block: u64, f: impl FnOnce(&[u8]) -> R) -> KResult<R> {
        let mut inner = self.inner.lock();
        Ok(f(&self.buf(&mut inner, block, true)?.data))
    }

    /// Change `block`; it is written back later.
    pub fn modify<R>(&self, block: u64, f: impl FnOnce(&mut [u8]) -> R) -> KResult<R> {
        let mut inner = self.inner.lock();
        let buf = self.buf(&mut inner, block, true)?;
        buf.dirty = true;
        Ok(f(&mut buf.data))
    }

    /// A newly allocated block: all zeroes, not read from the disk.
    pub fn zero(&self, block: u64) -> KResult<()> {
        let mut inner = self.inner.lock();
        let buf = self.buf(&mut inner, block, false)?;
        buf.data.fill(0);
        buf.dirty = true;
        Ok(())
    }

    /// The block was freed: drop it without writing it.
    pub fn forget(&self, block: u64) {
        self.inner.lock().bufs.remove(&block);
    }

    /// Write all changed blocks to the disk, in block order.
    pub fn sync(&self) -> KResult<()> {
        let mut inner = self.inner.lock();
        for (&block, buf) in inner.bufs.iter_mut().filter(|(_, b)| b.dirty) {
            self.disk.write_bytes(block * self.block_size as u64, &buf.data)?;
            buf.dirty = false;
        }
        Ok(())
    }

    /// Forget the clean blocks (`drop_caches`); changed ones stay.
    pub fn drop_clean(&self) {
        self.inner.lock().bufs.retain(|_, b| b.dirty);
    }

    pub fn has_dirty(&self) -> bool {
        self.inner.lock().bufs.values().any(|b| b.dirty)
    }

    /// Keep changed blocks until `sync` (a journal logs them first).
    pub fn hold_dirty(&self, hold: bool) {
        self.hold_dirty.store(hold, Ordering::Relaxed);
    }

    /// How many blocks are changed.
    pub fn dirty_count(&self) -> usize {
        self.inner.lock().bufs.values().filter(|b| b.dirty).count()
    }

    /// Blocks written elsewhere (a journal's checkpoint): clean, if they
    /// are still as they were.
    pub fn mark_clean(&self, blocks: &[(u64, Vec<u8>)]) {
        let mut inner = self.inner.lock();
        for (n, data) in blocks {
            if let Some(b) = inner.bufs.get_mut(n)
                && b.data[..] == data[..]
            {
                b.dirty = false;
            }
        }
    }

    /// The changed blocks: (block, contents), in block order.
    pub fn dirty_blocks(&self) -> Vec<(u64, Vec<u8>)> {
        self.inner.lock().bufs.iter().filter(|(_, b)| b.dirty).map(|(&n, b)| (n, b.data.to_vec())).collect()
    }
}
