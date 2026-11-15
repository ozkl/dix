//! Memory: "frames" are reference-counted heap pages, so ruxis's page cache
//! works unchanged.

pub mod pagecache;

pub mod pressure {
    /// The kernel slows writers down under memory pressure; not here.
    pub fn throttle() {}
}

/// A frame: the address of a heap-allocated [`frame::Frame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhysAddr(usize);

pub mod frame {
    use super::PhysAddr;
    use crate::ext4::kernel::arch::paging::PAGE_SIZE;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub struct Frame {
        refs: AtomicUsize,
        data: [u8; PAGE_SIZE],
    }

    fn frame<'a>(pa: PhysAddr) -> &'a Frame {
        // SAFETY: `pa` came from `alloc_user_zeroed` and is alive while it
        // has references (the page cache's discipline, as in the kernel).
        unsafe { &*(pa.0 as *const Frame) }
    }

    /// A zeroed frame with one reference.
    pub fn alloc_user_zeroed() -> Option<PhysAddr> {
        let f = Box::new(Frame { refs: AtomicUsize::new(1), data: [0; PAGE_SIZE] });
        Some(PhysAddr(Box::into_raw(f) as usize))
    }

    /// One more reference.
    pub fn get(pa: PhysAddr) {
        frame(pa).refs.fetch_add(1, Ordering::AcqRel);
    }

    /// Drop a reference; the last one frees the frame.
    pub fn free(pa: PhysAddr) {
        if frame(pa).refs.fetch_sub(1, Ordering::AcqRel) == 1 {
            // SAFETY: that was the last reference.
            drop(unsafe { Box::from_raw(pa.0 as *mut Frame) });
        }
    }

    pub fn refs(pa: PhysAddr) -> usize {
        frame(pa).refs.load(Ordering::Acquire)
    }

    /// # Safety
    /// As `frame_bytes`: the caller holds a reference.
    pub unsafe fn bytes<'a>(pa: PhysAddr) -> &'a mut [u8] {
        unsafe { &mut (*(pa.0 as *mut Frame)).data }
    }
}
