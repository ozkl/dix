use super::{BlockDevice, DeviceInfo};
use crate::error::Result;
use std::sync::{Arc, Mutex, Weak};

type Inner = Arc<Mutex<Box<dyn BlockDevice>>>;

/// One device used through several handles, e.g. one disk image whose
/// partitions are mounted as separate volumes. Each handle is a
/// [`BlockDevice`]; the device (and the host file's lock) lives as long as
/// any handle does.
pub struct SharedDevice {
    inner: Inner,
}

impl SharedDevice {
    pub fn new(dev: Box<dyn BlockDevice>) -> Self {
        Self { inner: Arc::new(Mutex::new(dev)) }
    }

    /// Another handle to the same device.
    pub fn handle(&self) -> Self {
        Self { inner: self.inner.clone() }
    }

    /// A reference that does not keep the device alive.
    pub fn downgrade(&self) -> WeakDevice {
        WeakDevice(Arc::downgrade(&self.inner))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Box<dyn BlockDevice>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// See [`SharedDevice::downgrade`].
#[derive(Clone)]
pub struct WeakDevice(Weak<Mutex<Box<dyn BlockDevice>>>);

impl WeakDevice {
    /// A handle, if the device is still in use somewhere.
    pub fn upgrade(&self) -> Option<SharedDevice> {
        self.0.upgrade().map(|inner| SharedDevice { inner })
    }
}

impl BlockDevice for SharedDevice {
    fn info(&self) -> DeviceInfo {
        self.lock().info()
    }
    fn size(&self) -> u64 {
        self.lock().size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.lock().read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.lock().write_at(offset, data)
    }
    fn resize(&mut self, new_size: u64) -> Result<()> {
        self.lock().resize(new_size)
    }
    fn flush(&mut self) -> Result<()> {
        self.lock().flush()
    }
    fn is_read_only(&self) -> bool {
        self.lock().is_read_only()
    }
    fn parts(&self) -> Option<Vec<u64>> {
        self.lock().parts()
    }
    fn container(&self) -> Option<String> {
        self.lock().container()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemoryDevice;

    #[test]
    fn handles_share_one_device_and_keep_it_alive() {
        let a = SharedDevice::new(Box::new(MemoryDevice::new("m", vec![0; 16])));
        let mut b = a.handle();
        let weak = a.downgrade();
        b.write_at(3, b"x").unwrap();
        let mut c = weak.upgrade().unwrap();
        let mut buf = [0u8; 1];
        c.read_at(3, &mut buf).unwrap();
        assert_eq!(&buf, b"x");
        drop((a, b, c));
        assert!(weak.upgrade().is_none());
    }
}
