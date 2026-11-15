use super::{BlockDevice, DeviceInfo};
use crate::error::{Error, Result};

/// Wraps a device and rejects every modification.
///
/// The registry puts this around the device whenever the user chooses
/// "Open read-only", so even a buggy driver cannot touch the image.
pub struct ReadOnlyDevice<D>(pub D);

impl<D: BlockDevice> BlockDevice for ReadOnlyDevice<D> {
    fn info(&self) -> DeviceInfo {
        DeviceInfo { read_only: true, resizable: false, ..self.0.info() }
    }
    fn size(&self) -> u64 {
        self.0.size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_at(offset, buf)
    }
    fn write_at(&mut self, _offset: u64, _data: &[u8]) -> Result<()> {
        Err(Error::read_only())
    }
    fn resize(&mut self, _new_size: u64) -> Result<()> {
        Err(Error::read_only())
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn parts(&self) -> Option<Vec<u64>> {
        self.0.parts()
    }
    fn container(&self) -> Option<String> {
        self.0.container()
    }
}
