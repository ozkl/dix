//! The page cache: the pages of a file's contents, in memory.
//!
//! Every regular file with contents (tmpfs, ext2) keeps a [`PageCache`].
//! `read`/`write` copy through it, and `mmap` maps its frames into
//! processes (`MAP_SHARED`), or maps them read-only and copies on the
//! first write (`MAP_PRIVATE`). A cache page holds one frame reference;
//! every mapping of it holds another.
//!
//! Pages missing from the cache are filled by the file system's `fill`
//! callback (zeroes for tmpfs, the file's blocks for ext2), which may
//! block, so it runs without any lock held. Modified pages are marked
//! dirty; file systems with a backing store write them back with
//! [`PageCache::take_dirty`].

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::{PhysAddr, frame};
use crate::ext4::kernel::arch::paging::PAGE_SIZE;
use crate::ext4::kernel::errno::{Errno, KResult};
use crate::ext4::kernel::sync::IrqLock;

const PAGE: u64 = PAGE_SIZE as u64;

#[derive(Clone, Copy)]
struct Page {
    pa: PhysAddr,
    dirty: bool,
    /// Mapped writable with `MAP_SHARED`: the page may change at any time,
    /// so it is written back on every sync.
    shared_writable: bool,
}

pub struct PageCache {
    pages: IrqLock<BTreeMap<u64, Page>>,
}

/// Pages in all page caches (`Cached` in `/proc/meminfo`).
static CACHED: AtomicUsize = AtomicUsize::new(0);

/// Pages the page caches hold.
pub fn cached_pages() -> usize {
    CACHED.load(Ordering::Relaxed)
}

fn uncount(n: usize) {
    CACHED.fetch_sub(n, Ordering::Relaxed);
}

/// The bytes of a frame.
///
/// # Safety
/// The caller must hold a reference to the frame and make sure nobody else
/// writes it concurrently (or tolerate torn data, like `read` racing with
/// a write through a shared mapping does on Linux).
pub unsafe fn frame_bytes<'a>(pa: PhysAddr) -> &'a mut [u8] {
    // Userspace: a "frame" is a heap page (see `super::frame`).
    unsafe { frame::bytes(pa) }
}

impl Default for PageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PageCache {
    pub const fn new() -> Self {
        Self { pages: IrqLock::new(BTreeMap::new()) }
    }

    /// Page `index`, with an extra frame reference for the caller (who
    /// must eventually `frame::free` it). A missing page is filled by
    /// `fill`, which gets a zeroed page.
    pub fn get(&self, index: u64, fill: impl FnOnce(&mut [u8]) -> KResult<()>) -> KResult<PhysAddr> {
        if let Some(p) = self.pages.lock().get(&index) {
            frame::get(p.pa);
            return Ok(p.pa);
        }
        let pa = frame::alloc_user_zeroed().ok_or(Errno::ENOMEM)?;
        if let Err(e) = fill(unsafe { frame_bytes(pa) }) {
            frame::free(pa);
            return Err(e);
        }
        let mut pages = self.pages.lock();
        // Somebody else may have filled it in the meantime.
        if let Some(p) = pages.get(&index) {
            let existing = p.pa;
            frame::get(existing);
            drop(pages);
            frame::free(pa);
            return Ok(existing);
        }
        pages.insert(index, Page { pa, dirty: false, shared_writable: false });
        CACHED.fetch_add(1, Ordering::Relaxed);
        frame::get(pa);
        Ok(pa)
    }

    /// Copy from offset `offset` of a file of `size` bytes into `buf`.
    pub fn read(
        &self,
        offset: u64,
        buf: &mut [u8],
        size: u64,
        mut fill: impl FnMut(u64, &mut [u8]) -> KResult<()>,
    ) -> KResult<usize> {
        let end = size.min(offset.saturating_add(buf.len() as u64));
        let mut pos = offset;
        while pos < end {
            let index = pos / PAGE;
            let off = (pos % PAGE) as usize;
            let n = (end - pos).min(PAGE - off as u64) as usize;
            let pa = self.get(index, |page| fill(index, page))?;
            let at = (pos - offset) as usize;
            buf[at..at + n].copy_from_slice(&unsafe { frame_bytes(pa) }[off..off + n]);
            frame::free(pa);
            pos += n as u64;
        }
        Ok((end.max(offset) - offset) as usize)
    }

    /// Copy `data` to offset `offset`; the pages become dirty. `fill`
    /// provides the old contents of partially written pages (`size` is the
    /// file size before the write: pages entirely past it start as zeroes).
    pub fn write(
        &self,
        offset: u64,
        data: &[u8],
        size: u64,
        mut fill: impl FnMut(u64, &mut [u8]) -> KResult<()>,
    ) -> KResult<()> {
        let mut pos = offset;
        let end = offset + data.len() as u64;
        while pos < end {
            let index = pos / PAGE;
            let off = (pos % PAGE) as usize;
            let n = (end - pos).min(PAGE - off as u64) as usize;
            let whole = off == 0 && n == PAGE_SIZE;
            let pa = self.get(index, |page| if whole || index * PAGE >= size { Ok(()) } else { fill(index, page) })?;
            let at = (pos - offset) as usize;
            let page = unsafe { frame_bytes(pa) };
            page[off..off + n].copy_from_slice(&data[at..at + n]);
            frame::free(pa);
            self.mark_dirty(index);
            pos += n as u64;
        }
        Ok(())
    }

    /// Copy `data` to offset `offset` into the pages that are cached
    /// (they stay clean: a write-through cache, FUSE's: the data went to
    /// the file already).
    pub fn update_cached(&self, offset: u64, data: &[u8]) {
        let mut pos = offset;
        let end = offset + data.len() as u64;
        while pos < end {
            let index = pos / PAGE;
            let off = (pos % PAGE) as usize;
            let n = (end - pos).min(PAGE - off as u64) as usize;
            let pa = self.pages.lock().get(&index).map(|p| {
                frame::get(p.pa);
                p.pa
            });
            if let Some(pa) = pa {
                let at = (pos - offset) as usize;
                let page = unsafe { frame_bytes(pa) };
                page[off..off + n].copy_from_slice(&data[at..at + n]);
                frame::free(pa);
            }
            pos += n as u64;
        }
    }

    /// The file changed behind the cache's back: forget the clean pages
    /// nobody maps (mapped ones stay, as Linux's
    /// `invalidate_mapping_pages`; dirty ones too: write them first).
    pub fn invalidate(&self) {
        self.unshare_unmapped();
        self.shrink();
    }

    /// [`Self::invalidate`] of pages `first..=last` only.
    pub fn invalidate_range(&self, first: u64, last: u64) {
        self.unshare_unmapped();
        let mut pages = self.pages.lock();
        let doomed: Vec<u64> = pages
            .range(first..=last)
            .filter(|(_, p)| !p.dirty && !p.shared_writable && frame::refs(p.pa) == 1)
            .map(|(&i, _)| i)
            .collect();
        for i in &doomed {
            frame::free(pages.remove(i).unwrap().pa);
        }
        uncount(doomed.len());
    }

    /// Pages mapped shared and writable that nobody maps any more can't
    /// change behind the cache's back now.
    pub fn unshare_unmapped(&self) {
        for p in self.pages.lock().values_mut() {
            if frame::refs(p.pa) == 1 {
                p.shared_writable = false;
            }
        }
    }

    /// Is page `index` cached?
    pub fn contains(&self, index: u64) -> bool {
        self.pages.lock().contains_key(&index)
    }

    /// Cache `pa` (filled, its reference handed over) as page `index`,
    /// unless the page is there already (then `false`: `pa` is still the
    /// caller's).
    pub fn insert_if_absent(&self, index: u64, pa: PhysAddr) -> bool {
        let mut pages = self.pages.lock();
        if pages.contains_key(&index) {
            return false;
        }
        pages.insert(index, Page { pa, dirty: false, shared_writable: false });
        CACHED.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub fn mark_dirty(&self, index: u64) {
        if let Some(p) = self.pages.lock().get_mut(&index) {
            p.dirty = true;
        }
    }

    /// The page is mapped writable and shared (see [`Page`]).
    pub fn mark_shared_writable(&self, index: u64) {
        if let Some(p) = self.pages.lock().get_mut(&index) {
            p.shared_writable = true;
            p.dirty = true;
        }
    }

    /// The pages to write back, clearing their dirty state: (index, frame)
    /// with a reference each for the caller.
    pub fn take_dirty(&self) -> Vec<(u64, PhysAddr)> {
        let mut pages = self.pages.lock();
        let mut out = Vec::new();
        for (&index, p) in pages.iter_mut() {
            if p.dirty || p.shared_writable {
                p.dirty = false;
                frame::get(p.pa);
                out.push((index, p.pa));
            }
        }
        out
    }

    /// Nobody maps the file any more: shared pages can no longer change
    /// behind the cache's back.
    pub fn unshare(&self) {
        for p in self.pages.lock().values_mut() {
            p.shared_writable = false;
        }
    }

    /// Put a page back on the dirty list (its write-back failed).
    pub fn redirty(&self, index: u64) {
        self.mark_dirty(index);
    }

    pub fn has_dirty(&self) -> bool {
        self.pages.lock().values().any(|p| p.dirty || p.shared_writable)
    }

    /// The file shrank (or grew) to `size` bytes: drop the pages past the
    /// end and zero the tail of the last one.
    pub fn truncate(&self, size: u64) {
        let first_gone = size.div_ceil(PAGE);
        let gone: Vec<Page> = {
            let mut pages = self.pages.lock();
            let tail = pages.split_off(&first_gone);
            if !size.is_multiple_of(PAGE)
                && let Some(p) = pages.get(&(size / PAGE))
            {
                let page = unsafe { frame_bytes(p.pa) };
                page[(size % PAGE) as usize..].fill(0);
            }
            tail.into_values().collect()
        };
        uncount(gone.len());
        for p in gone {
            frame::free(p.pa);
        }
    }

    /// Number of cached pages.
    pub fn len(&self) -> usize {
        self.pages.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop clean pages nobody maps (to free memory).
    pub fn shrink(&self) -> usize {
        let mut pages = self.pages.lock();
        let doomed: Vec<u64> = pages
            .iter()
            .filter(|(_, p)| !p.dirty && !p.shared_writable && frame::refs(p.pa) == 1)
            .map(|(&i, _)| i)
            .collect();
        for i in &doomed {
            frame::free(pages.remove(i).unwrap().pa);
        }
        uncount(doomed.len());
        doomed.len()
    }
}

impl Drop for PageCache {
    fn drop(&mut self) {
        let pages = self.pages.get_mut();
        uncount(pages.len());
        for p in pages.values() {
            frame::free(p.pa);
        }
    }
}
