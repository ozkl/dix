use super::{check_range, BlockDevice, DeviceInfo, DeviceKind};
use crate::error::{Error, Result};
use std::sync::{Arc, Mutex, MutexGuard};

/// A cloneable handle to the bytes of a [`MemoryDevice`].
///
/// The device is moved into the mounted file system, but the application
/// keeps a `SharedBuffer` so it can still offer "Download image" after the
/// user modified the volume (the browser has no file to write back to).
#[derive(Clone, Default)]
pub struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedBuffer {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Arc::new(Mutex::new(bytes)))
    }

    fn lock(&self) -> MutexGuard<'_, Vec<u8>> {
        // A poisoned lock only means another holder panicked; data is still bytes.
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn len(&self) -> u64 {
        self.lock().len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A copy of the current content.
    pub fn snapshot(&self) -> Vec<u8> {
        self.lock().clone()
    }
}

/// A device backed by RAM.
pub struct MemoryDevice {
    name: String,
    buf: SharedBuffer,
    read_only: bool,
    resizable: bool,
}

impl MemoryDevice {
    pub fn new(name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self { name: name.into(), buf: SharedBuffer::new(bytes), read_only: false, resizable: true }
    }

    /// A zero filled device of `size` bytes.
    pub fn zeroed(name: impl Into<String>, size: usize) -> Self {
        Self::new(name, vec![0; size])
    }

    pub fn from_shared(name: impl Into<String>, buf: SharedBuffer) -> Self {
        Self { name: name.into(), buf, read_only: false, resizable: true }
    }

    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Fixed-size devices behave like a real disk (useful in tests).
    pub fn with_resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }

    pub fn buffer(&self) -> SharedBuffer {
        self.buf.clone()
    }
}

impl BlockDevice for MemoryDevice {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            name: self.name.clone(),
            kind: DeviceKind::Memory,
            size: self.buf.len(),
            sector_size: 512,
            read_only: self.read_only,
            resizable: self.resizable && !self.read_only,
        }
    }

    fn size(&self) -> u64 {
        self.buf.len()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let data = self.buf.lock();
        check_range(offset, buf.len(), data.len() as u64)?;
        let start = offset as usize;
        buf.copy_from_slice(&data[start..start + buf.len()]);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, src: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Error::read_only());
        }
        let mut data = self.buf.lock();
        check_range(offset, src.len(), data.len() as u64)?;
        let start = offset as usize;
        data[start..start + src.len()].copy_from_slice(src);
        Ok(())
    }

    fn resize(&mut self, new_size: u64) -> Result<()> {
        if self.read_only {
            return Err(Error::read_only());
        }
        if !self.resizable {
            return Err(Error::unsupported("resizing this device"));
        }
        let new_size = usize::try_from(new_size).map_err(|_| Error::invalid_input("size too large"))?;
        self.buf.lock().resize(new_size, 0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    #[test]
    fn read_write_bounds() {
        let mut d = MemoryDevice::zeroed("t", 16);
        d.write_at(4, b"abcd").unwrap();
        let mut b = [0u8; 4];
        d.read_at(4, &mut b).unwrap();
        assert_eq!(&b, b"abcd");
        assert_eq!(d.read_at(14, &mut b).unwrap_err().kind(), ErrorKind::OutOfBounds);
        let shared = d.buffer();
        d.resize(32).unwrap();
        assert_eq!(shared.len(), 32);
    }
}
