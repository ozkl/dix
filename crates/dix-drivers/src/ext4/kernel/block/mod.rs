//! A disk for the driver: a [`dix_core::BlockDevice`] with byte-granular
//! reads and writes, and (read-only mounts) an in-memory overlay.

pub mod cache;

use crate::ext4::kernel::errno::{Errno, KResult, io_error};
use crate::ext4::kernel::sync::Mutex;
use dix_core::BlockDevice as Device;
use std::collections::BTreeMap;

const SECTOR: u64 = 512;

/// The device behind a [`Disk`] (`disk.dev` in the driver).
pub struct DiskDevice {
    dev: Mutex<Box<dyn Device>>,
    /// Read-only mounts: sectors the driver "wrote" (journal recovery) live
    /// here, so the recovered state is visible but the image is untouched.
    overlay: Option<Mutex<BTreeMap<u64, Box<[u8]>>>>,
}

impl DiskDevice {
    pub fn flush(&self) -> KResult<()> {
        if self.overlay.is_some() {
            return Ok(());
        }
        self.dev.lock().flush().map_err(io_error)
    }
}

pub struct Disk {
    pub name: String,
    pub rdev: u64,
    pub dev: DiskDevice,
}

impl Disk {
    /// `overlay`: keep every write in memory (read-only mounts).
    pub fn new(dev: Box<dyn Device>, overlay: bool) -> Disk {
        // Messages name the image by its file name, as the kernel names a disk.
        let full = dev.info().name;
        let name = full.rsplit(['/', '\\']).next().filter(|n| !n.is_empty()).unwrap_or(&full).to_owned();
        Disk {
            name,
            rdev: 0,
            dev: DiskDevice { dev: Mutex::new(dev), overlay: overlay.then(|| Mutex::new(BTreeMap::new())) },
        }
    }

    pub fn size(&self) -> u64 {
        self.dev.dev.lock().size()
    }

    /// Number of sectors held in the overlay.
    pub fn overlay_sectors(&self) -> usize {
        self.dev.overlay.as_ref().map_or(0, |o| o.lock().len())
    }

    pub fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> KResult<()> {
        if offset.checked_add(buf.len() as u64).is_none_or(|end| end > self.size()) {
            return Err(Errno::EIO);
        }
        self.dev.dev.lock().read_at(offset, buf).map_err(io_error)?;
        if let Some(overlay) = &self.dev.overlay {
            let overlay = overlay.lock();
            let (first, last) = (offset / SECTOR, (offset + buf.len() as u64).div_ceil(SECTOR));
            for (&s, data) in overlay.range(first..last) {
                // Copy the overlapping part of sector `s`.
                let s_start = s * SECTOR;
                let from = s_start.max(offset);
                let to = (s_start + SECTOR).min(offset + buf.len() as u64);
                buf[(from - offset) as usize..(to - offset) as usize]
                    .copy_from_slice(&data[(from - s_start) as usize..(to - s_start) as usize]);
            }
        }
        Ok(())
    }

    pub fn write_bytes(&self, offset: u64, data: &[u8]) -> KResult<()> {
        if offset.checked_add(data.len() as u64).is_none_or(|end| end > self.size()) {
            return Err(Errno::ENOSPC);
        }
        let Some(overlay) = &self.dev.overlay else {
            return self.dev.dev.lock().write_at(offset, data).map_err(io_error);
        };
        let (first, last) = (offset / SECTOR, (offset + data.len() as u64).div_ceil(SECTOR));
        for s in first..last {
            let s_start = s * SECTOR;
            let mut sector = vec![0u8; SECTOR as usize];
            self.read_bytes(s_start, &mut sector)?;
            let from = s_start.max(offset);
            let to = (s_start + SECTOR).min(offset + data.len() as u64);
            sector[(from - s_start) as usize..(to - s_start) as usize]
                .copy_from_slice(&data[(from - offset) as usize..(to - offset) as usize]);
            overlay.lock().insert(s, sector.into_boxed_slice());
        }
        Ok(())
    }
}
