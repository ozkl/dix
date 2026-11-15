//! Block devices: random-access byte stores that hold a file system image.
//!
//! Everything a driver reads or writes goes through [`BlockDevice`]. Where the
//! bytes live is up to the implementation:
//!
//! * [`MemoryDevice`] – a buffer in RAM (browser uploads, freshly created images)
//! * `FileDevice` – a regular image file on the host (native only)
//! * raw disks / partitions such as `/dev/sdb5`, `/dev/disk4s1`,
//!   `\\.\PhysicalDrive1`, `\\.\E:` (native only, see [`host`])
//! * [`SliceDevice`] – a window into another device (e.g. one partition)
//! * [`ReadOnlyDevice`] – rejects every write (enforces "open read-only")
//! * [`AlignedDevice`] – turns arbitrary accesses into sector aligned ones
//! * [`DetachableDevice`] – can be "unplugged" at any moment (forced eject)
//! * [`JoinedDevice`] – several files read as one (a split archive or image)
//! * [`SharedDevice`] – one device behind several handles (the partitions
//!   of one disk image, mounted as separate volumes)

mod aligned;
mod detach;
mod joined;
mod memory;
mod readonly;
mod shared;
mod slice;

#[cfg(not(target_arch = "wasm32"))]
mod disks;
#[cfg(not(target_arch = "wasm32"))]
pub mod host;

pub use aligned::AlignedDevice;
pub use detach::{DetachableDevice, DevicePlug};
pub use joined::{split_set, JoinedDevice};
pub use memory::{MemoryDevice, SharedBuffer};
pub use readonly::ReadOnlyDevice;
pub use shared::{SharedDevice, WeakDevice};
pub use slice::SliceDevice;

use crate::error::{Error, ErrorKind, Result};

/// Where a device's bytes come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    /// A buffer in memory (uploaded in the browser or created by the app).
    Memory,
    /// A regular file on the host file system.
    ImageFile,
    /// A raw disk, partition or volume of the host.
    RawDisk,
    /// A sub-range of another device.
    Slice,
}

impl DeviceKind {
    pub fn label(self) -> &'static str {
        match self {
            DeviceKind::Memory => "In-memory image",
            DeviceKind::ImageFile => "Image file",
            DeviceKind::RawDisk => "Raw device",
            DeviceKind::Slice => "Partition",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Display name (file name, device path, …).
    pub name: String,
    pub kind: DeviceKind,
    pub size: u64,
    /// Preferred I/O granularity (512 for most images).
    pub sector_size: u32,
    pub read_only: bool,
    /// [`BlockDevice::resize`] is supported (image files and memory buffers).
    pub resizable: bool,
}

/// Random-access storage of a fixed (or growable) size.
///
/// Implementations must be `Send` so mounted volumes can be moved to worker
/// threads in the future.
pub trait BlockDevice: Send {
    fn info(&self) -> DeviceInfo;

    fn size(&self) -> u64;

    /// Fills `buf` completely from `offset`. Reading past the end is an
    /// [`ErrorKind::OutOfBounds`] error.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Writes all of `data` at `offset`. Writing past the end is an error
    /// (call [`resize`](Self::resize) first on resizable devices).
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()>;

    /// Changes the size of the device, if supported.
    fn resize(&mut self, _new_size: u64) -> Result<()> {
        Err(Error::unsupported("resizing this device"))
    }

    /// Makes sure everything written so far reached the medium.
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn is_read_only(&self) -> bool {
        self.info().read_only
    }

    /// For a device that joins several files (the parts of a split
    /// archive or image, see [`JoinedDevice`]): the size of each, in order.
    fn parts(&self) -> Option<Vec<u64>> {
        None
    }

    /// For the virtual disk of a container (a VM disk such as qcow2 or
    /// VHD, see [`crate::container`]): what it is, e.g. "qcow2 v3".
    fn container(&self) -> Option<String> {
        None
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for Box<T> {
    fn info(&self) -> DeviceInfo {
        (**self).info()
    }
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        (**self).write_at(offset, data)
    }
    fn resize(&mut self, new_size: u64) -> Result<()> {
        (**self).resize(new_size)
    }
    fn flush(&mut self) -> Result<()> {
        (**self).flush()
    }
    fn is_read_only(&self) -> bool {
        (**self).is_read_only()
    }
    fn parts(&self) -> Option<Vec<u64>> {
        (**self).parts()
    }
    fn container(&self) -> Option<String> {
        (**self).container()
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for &mut T {
    fn info(&self) -> DeviceInfo {
        (**self).info()
    }
    fn size(&self) -> u64 {
        (**self).size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        (**self).read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        (**self).write_at(offset, data)
    }
    fn resize(&mut self, new_size: u64) -> Result<()> {
        (**self).resize(new_size)
    }
    fn flush(&mut self) -> Result<()> {
        (**self).flush()
    }
    fn is_read_only(&self) -> bool {
        (**self).is_read_only()
    }
    fn parts(&self) -> Option<Vec<u64>> {
        (**self).parts()
    }
    fn container(&self) -> Option<String> {
        (**self).container()
    }
}

/// Validates that `[offset, offset + len)` lies inside a device of `size` bytes.
pub fn check_range(offset: u64, len: usize, size: u64) -> Result<()> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= size => Ok(()),
        _ => Err(Error::new(
            ErrorKind::OutOfBounds,
            format!("access at {offset}+{len} is outside the device ({size} bytes)"),
        )),
    }
}

/// Helper for drivers: reads `len` bytes into a new vector.
pub fn read_vec(dev: &mut dyn BlockDevice, offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut v = vec![0u8; len];
    dev.read_at(offset, &mut v)?;
    Ok(v)
}

/// Copies the whole content of a device into memory (used for "save a copy"
/// / browser downloads of an image).
pub fn read_all(dev: &mut dyn BlockDevice) -> Result<Vec<u8>> {
    let size = usize::try_from(dev.size())
        .map_err(|_| Error::new(ErrorKind::NoSpace, "device too large to load into memory"))?;
    read_vec(dev, 0, size)
}
