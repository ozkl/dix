use super::{check_range, BlockDevice, DeviceInfo};
use crate::error::Result;

/// Translates arbitrary reads/writes into accesses aligned to `align` bytes.
///
/// Raw disks on Windows (`\\.\PhysicalDriveN`) and macOS (`/dev/rdiskN`)
/// reject I/O that is not sector aligned. Using 4096 as the alignment works
/// for both 512-byte and 4K-native drives.
pub struct AlignedDevice<D> {
    inner: D,
    align: u64,
}

impl<D: BlockDevice> AlignedDevice<D> {
    pub fn new(inner: D, align: u64) -> Self {
        assert!(align.is_power_of_two());
        Self { inner, align }
    }

    fn bounds(&self, offset: u64, len: usize) -> (u64, u64) {
        let start = offset & !(self.align - 1);
        let end = (offset + len as u64).div_ceil(self.align) * self.align;
        (start, end.min(self.inner.size()))
    }

    fn is_aligned(&self, offset: u64, len: usize) -> bool {
        offset.is_multiple_of(self.align)
            && ((len as u64).is_multiple_of(self.align) || offset + len as u64 == self.inner.size())
    }
}

impl<D: BlockDevice> BlockDevice for AlignedDevice<D> {
    fn info(&self) -> DeviceInfo {
        self.inner.info()
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.size())?;
        if buf.is_empty() {
            return Ok(());
        }
        if self.is_aligned(offset, buf.len()) {
            return self.inner.read_at(offset, buf);
        }
        let (start, end) = self.bounds(offset, buf.len());
        let mut tmp = vec![0u8; (end - start) as usize];
        self.inner.read_at(start, &mut tmp)?;
        let skip = (offset - start) as usize;
        buf.copy_from_slice(&tmp[skip..skip + buf.len()]);
        Ok(())
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        check_range(offset, data.len(), self.size())?;
        if data.is_empty() {
            return Ok(());
        }
        if self.is_aligned(offset, data.len()) {
            return self.inner.write_at(offset, data);
        }
        // Read-modify-write of the surrounding aligned range.
        let (start, end) = self.bounds(offset, data.len());
        let mut tmp = vec![0u8; (end - start) as usize];
        self.inner.read_at(start, &mut tmp)?;
        let skip = (offset - start) as usize;
        tmp[skip..skip + data.len()].copy_from_slice(data);
        self.inner.write_at(start, &tmp)
    }
    fn resize(&mut self, new_size: u64) -> Result<()> {
        self.inner.resize(new_size)
    }
    fn flush(&mut self) -> Result<()> {
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
    fn unaligned_round_trip() {
        let mut d = AlignedDevice::new(MemoryDevice::zeroed("t", 4096 * 3), 4096);
        d.write_at(4090, b"hello world").unwrap();
        let mut b = [0u8; 11];
        d.read_at(4090, &mut b).unwrap();
        assert_eq!(&b, b"hello world");
    }
}
