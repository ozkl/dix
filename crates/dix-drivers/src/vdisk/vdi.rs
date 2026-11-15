//! VDI, VirtualBox's disk image.
//!
//! A text banner (`<<< Oracle VM VirtualBox Disk Image >>>`), the
//! signature `0xBEDA107F` and a little-endian header (version 1.1) giving
//! the disk size, the block size (1 MiB) and where the block map and the
//! data are. The block map has one 32-bit entry per block: the block's
//! index in the data area, `0xFFFFFFFF` for a block not allocated and
//! `0xFFFFFFFE` for a block of zeros.
//!
//! Fixed and dynamic ("normal") images can be written: a new block is
//! appended and counted in the header. Differencing images (snapshots)
//! name their parent by UUID only, which cannot be looked up here.

use super::{Disk, Layout, damaged, le32, le64, read, size_label, unsupported};
use dix_core::Result;
use dix_core::container::{DiskFormat, OpenContext};
use dix_core::device::{BlockDevice, DeviceInfo};

const SIGNATURE: u32 = 0xBEDA_107F;
const FREE: u32 = 0xFFFF_FFFF;
const ZERO: u32 = 0xFFFF_FFFE;

pub struct VdiFormat;

impl DiskFormat for VdiFormat {
    fn name(&self) -> &'static str {
        "VDI"
    }
    fn probe(&self, dev: &mut dyn BlockDevice) -> bool {
        super::read_some(dev, 0, 72).is_some_and(|h| h.len() == 72 && le32(&h, 64) == SIGNATURE)
    }
    fn open(&self, dev: Box<dyn BlockDevice>, _ctx: &OpenContext<'_>) -> Result<Box<dyn BlockDevice>> {
        Ok(Box::new(Disk(Vdi::open(dev)?)))
    }

    fn creates(&self) -> Option<(&'static str, &'static str)> {
        Some(("vdi", "VirtualBox (VDI, dynamic): grows as it is filled"))
    }

    /// A dynamic VDI (1 MiB blocks, none allocated): the header, the block
    /// map, the data area starting at the next MiB, as VirtualBox does.
    fn create(&self, file: &mut dyn BlockDevice, size: u64, _name: &str) -> Result<()> {
        super::check_new_size(size)?;
        const BLOCK: u64 = 1 << 20;
        let size = size.div_ceil(512) * 512;
        let blocks = size.div_ceil(BLOCK);
        let data = (512 + blocks * 4).div_ceil(BLOCK) * BLOCK;
        let mut h = vec![0u8; 512];
        h[..40].copy_from_slice(b"<<< Oracle VM VirtualBox Disk Image >>>\n");
        h[64..68].copy_from_slice(&SIGNATURE.to_le_bytes());
        h[68..72].copy_from_slice(&0x0001_0001u32.to_le_bytes());
        h[72..76].copy_from_slice(&400u32.to_le_bytes());
        h[76..80].copy_from_slice(&1u32.to_le_bytes());
        h[340..344].copy_from_slice(&512u32.to_le_bytes());
        h[344..348].copy_from_slice(&(data as u32).to_le_bytes());
        h[360..364].copy_from_slice(&512u32.to_le_bytes()); // legacy geometry: sector size
        h[368..376].copy_from_slice(&size.to_le_bytes());
        h[376..380].copy_from_slice(&(BLOCK as u32).to_le_bytes());
        h[384..388].copy_from_slice(&(blocks as u32).to_le_bytes());
        h[392..408].copy_from_slice(&dix_core::partition::edit::random_guid());
        h[408..424].copy_from_slice(&dix_core::partition::edit::random_guid());
        super::put(file, 0, &h)?;
        super::put(file, 512, &vec![0xFF; (blocks * 4) as usize])?;
        if file.size() < data {
            file.resize(data)?;
        }
        Ok(())
    }
}

struct Vdi {
    dev: Box<dyn BlockDevice>,
    fixed: bool,
    size: u64,
    block: u64,
    /// Bytes before each block's data ("extra" data, normally none).
    extra: u64,
    map: Vec<u32>,
    map_offset: u64,
    data_offset: u64,
    allocated: u32,
    writable: bool,
}

impl Vdi {
    fn open(mut dev: Box<dyn BlockDevice>) -> Result<Self> {
        let n = 512.min(dev.size() as usize);
        let h = read(dev.as_mut(), 0, n)?;
        let version = le32(&h, 68);
        if version >> 16 != 1 {
            return Err(unsupported(format!("VDI version {}.{}", version >> 16, version & 0xFFFF)));
        }
        if h.len() < 400 {
            return Err(damaged("the header is cut short"));
        }
        let kind = le32(&h, 76);
        let fixed = match kind {
            1 => false,
            2 => true,
            4 => return Err(unsupported("a differencing VDI (a snapshot): open its base disk")),
            _ => return Err(unsupported(format!("VDI image type {kind}"))),
        };
        let map_offset = le32(&h, 340) as u64;
        let data_offset = le32(&h, 344) as u64;
        let size = le64(&h, 368);
        let block = le32(&h, 376) as u64;
        let extra = le32(&h, 380) as u64;
        let blocks = le32(&h, 384) as u64;
        let allocated = le32(&h, 388);
        if block == 0 || block > 1 << 28 || blocks > 1 << 26 || blocks * block < size {
            return Err(damaged("the block map does not cover the disk"));
        }
        let raw = read(dev.as_mut(), map_offset, (blocks * 4) as usize).map_err(|e| e.with_context("the block map"))?;
        let map = (0..blocks as usize).map(|i| le32(&raw, i * 4)).collect();
        Ok(Self {
            writable: !dev.is_read_only(),
            dev,
            fixed,
            size,
            block,
            extra,
            map,
            map_offset,
            data_offset,
            allocated,
        })
    }

    fn at(&self, index: u32) -> u64 {
        self.data_offset + index as u64 * (self.block + self.extra) + self.extra
    }
}

impl Layout for Vdi {
    fn size(&self) -> u64 {
        self.size
    }
    fn block(&self) -> u64 {
        self.block
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let i = (offset / self.block) as usize;
        match self.map[i] {
            FREE | ZERO => {
                buf.fill(0);
                Ok(())
            }
            b => self.dev.read_at(self.at(b) + offset % self.block, buf),
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let i = (offset / self.block) as usize;
        let inner = offset % self.block;
        let b = self.map[i];
        if b != FREE && b != ZERO {
            return self.dev.write_at(self.at(b) + inner, data);
        }
        // A new block after the last one.
        let index = self.allocated;
        let start = self.at(index) - self.extra;
        let end = start + self.extra + self.block;
        if self.dev.size() < end {
            self.dev.resize(end)?;
        }
        let mut block = vec![0u8; (self.extra + self.block) as usize];
        let d = (self.extra + inner) as usize;
        block[d..d + data.len()].copy_from_slice(data);
        self.dev.write_at(start, &block)?;
        self.map[i] = index;
        self.dev.write_at(self.map_offset + i as u64 * 4, &index.to_le_bytes())?;
        self.allocated += 1;
        self.dev.write_at(388, &self.allocated.to_le_bytes())
    }

    fn writable(&self) -> bool {
        self.writable
    }
    fn flush(&mut self) -> Result<()> {
        self.dev.flush()
    }
    fn file(&self) -> DeviceInfo {
        self.dev.info()
    }
    fn describe(&self) -> String {
        format!("VDI, {}, {} blocks", if self.fixed { "fixed" } else { "dynamic" }, size_label(self.block))
    }
}
