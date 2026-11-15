use super::{check_range, BlockDevice, DeviceInfo, DeviceKind};
use crate::error::{Error, Result};

/// A window `[offset, offset + len)` into another device, e.g. a partition
/// inside a whole-disk image.
pub struct SliceDevice<D> {
    inner: D,
    offset: u64,
    len: u64,
    name: String,
}

impl<D: BlockDevice> SliceDevice<D> {
    pub fn new(inner: D, offset: u64, len: u64, name: impl Into<String>) -> Result<Self> {
        let end = offset.checked_add(len).ok_or_else(|| Error::invalid_input("slice overflows"))?;
        if end > inner.size() {
            return Err(Error::invalid_input("slice extends beyond the device"));
        }
        Ok(Self { inner, offset, len, name: name.into() })
    }

    pub fn into_inner(self) -> D {
        self.inner
    }
}

impl<D: BlockDevice> BlockDevice for SliceDevice<D> {
    fn info(&self) -> DeviceInfo {
        let inner = self.inner.info();
        DeviceInfo {
            name: self.name.clone(),
            kind: DeviceKind::Slice,
            size: self.len,
            sector_size: inner.sector_size,
            read_only: inner.read_only,
            resizable: false,
        }
    }
    fn size(&self) -> u64 {
        self.len
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        self.inner.read_at(self.offset + offset, buf)
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        check_range(offset, data.len(), self.len)?;
        self.inner.write_at(self.offset + offset, data)
    }
    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }
}
