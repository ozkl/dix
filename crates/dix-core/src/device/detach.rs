use super::{BlockDevice, DeviceInfo};
use crate::error::{Error, ErrorKind, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The "cable" of a [`DetachableDevice`]. Pulling it makes every further
/// read, write and flush of the device fail, like yanking a USB stick.
///
/// Used for "Eject Anyway": the image is frozen exactly as it was at that
/// moment, and not even the driver's last-chance flush on drop can write.
/// This is what makes crash-recovery (journal replay) testable.
#[derive(Clone, Default)]
pub struct DevicePlug(Arc<AtomicBool>);

impl DevicePlug {
    pub fn pull(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_pulled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Wraps a device so it can be disconnected at any moment through its [`DevicePlug`].
pub struct DetachableDevice<D> {
    inner: D,
    plug: DevicePlug,
}

impl<D: BlockDevice> DetachableDevice<D> {
    pub fn new(inner: D) -> (Self, DevicePlug) {
        let plug = DevicePlug::default();
        (Self { inner, plug: plug.clone() }, plug)
    }

    fn check(&self) -> Result<()> {
        if self.plug.is_pulled() {
            Err(Error::new(ErrorKind::Io, "the device was removed"))
        } else {
            Ok(())
        }
    }
}

impl<D: BlockDevice> BlockDevice for DetachableDevice<D> {
    fn info(&self) -> DeviceInfo {
        self.inner.info()
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check()?;
        self.inner.read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.check()?;
        self.inner.write_at(offset, data)
    }
    fn resize(&mut self, new_size: u64) -> Result<()> {
        self.check()?;
        self.inner.resize(new_size)
    }
    fn flush(&mut self) -> Result<()> {
        self.check()?;
        self.inner.flush()
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
    fn parts(&self) -> Option<Vec<u64>> {
        self.inner.parts()
    }
    fn container(&self) -> Option<String> {
        self.inner.container()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemoryDevice;

    #[test]
    fn pulled_plug_blocks_io() {
        let mem = MemoryDevice::zeroed("t", 16);
        let buf = mem.buffer();
        let (mut d, plug) = DetachableDevice::new(mem);
        d.write_at(0, b"ok").unwrap();
        plug.pull();
        assert!(d.write_at(2, b"no").is_err());
        assert!(d.read_at(0, &mut [0u8; 2]).is_err());
        assert_eq!(&buf.snapshot()[..4], b"ok\0\0");
    }
}
