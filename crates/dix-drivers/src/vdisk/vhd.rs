//! VHD, the Virtual PC / Hyper-V disk (also QEMU's `vpc`, Azure's disks).
//!
//! Every VHD ends with a 512-byte big-endian footer (`conectix`) giving
//! the disk's size, geometry and type:
//!
//! * **fixed**: the disk itself, raw, then the footer;
//! * **dynamic**: a copy of the footer, a dynamic header (`cxsparse`) and
//!   the block allocation table (BAT): one 32-bit sector number per block
//!   (2 MiB by default), `0xFFFFFFFF` for a block not allocated (zeros).
//!   Each block starts with a sector bitmap, then its data;
//! * **differencing**: dynamic, over a parent disk; a sector whose bitmap
//!   bit is clear is read from the parent, found through the header's
//!   parent locators (a relative or absolute path) or its name.
//!
//! Fixed and dynamic disks can be written: a new block goes where the
//! footer was, and the footer moves after it.

use super::{Disk, Layout, be32, be64, damaged, read, read_parent, size_label, unsupported};
use dix_core::Result;
use dix_core::container::{DiskFormat, OpenContext};
use dix_core::device::{BlockDevice, DeviceInfo};

const COOKIE: &[u8; 8] = b"conectix";
const UNALLOCATED: u32 = 0xFFFF_FFFF;

pub struct VhdFormat;

/// Where the footer is: the last 512 bytes (or 511, in old images).
fn footer(dev: &mut dyn BlockDevice) -> Option<(u64, Vec<u8>)> {
    let size = dev.size();
    for back in [512u64, 511] {
        if size >= back {
            let f = super::read_some(dev, size - back, 512)?;
            if f.starts_with(COOKIE) {
                let mut f = f;
                f.resize(512, 0);
                return Some((size - back, f));
            }
        }
    }
    None
}

impl DiskFormat for VhdFormat {
    fn name(&self) -> &'static str {
        "VHD"
    }
    fn probe(&self, dev: &mut dyn BlockDevice) -> bool {
        footer(dev).is_some_and(|(_, f)| matches!(be32(&f, 60), 2..=4))
    }
    fn open(&self, dev: Box<dyn BlockDevice>, ctx: &OpenContext<'_>) -> Result<Box<dyn BlockDevice>> {
        Ok(Box::new(Disk(Vhd::open(dev, ctx)?)))
    }

    fn creates(&self) -> Option<(&'static str, &'static str)> {
        Some(("vhd", "Virtual PC / Hyper-V (VHD, dynamic): grows as it is filled"))
    }

    /// A dynamic VHD: a copy of the footer, the dynamic header, an empty
    /// block table (2 MiB blocks), the footer. The size is rounded down to
    /// a whole CHS geometry, so every reader agrees on it.
    fn create(&self, file: &mut dyn BlockDevice, size: u64, _name: &str) -> Result<()> {
        super::check_new_size(size)?;
        let (c, h, s) = geometry(size / 512);
        let size = c as u64 * h as u64 * s as u64 * 512;
        if size == 0 {
            return Err(dix_core::Error::invalid_input("too small for a VHD"));
        }
        const BLOCK: u64 = 2 << 20;
        let entries = size.div_ceil(BLOCK);
        let bat_bytes = (entries * 4).div_ceil(512) * 512;
        let mut f = vec![0u8; 512];
        f[..8].copy_from_slice(COOKIE);
        f[8..12].copy_from_slice(&2u32.to_be_bytes());
        f[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        f[16..24].copy_from_slice(&512u64.to_be_bytes());
        let since_2000 = dix_core::time::unix_now_ns() / 1_000_000_000;
        f[24..28].copy_from_slice(&(since_2000.saturating_sub(946_684_800) as u32).to_be_bytes());
        f[28..32].copy_from_slice(b"dix ");
        f[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        f[36..40].copy_from_slice(b"Wi2k");
        f[40..48].copy_from_slice(&size.to_be_bytes());
        f[48..56].copy_from_slice(&size.to_be_bytes());
        f[56..58].copy_from_slice(&c.to_be_bytes());
        f[58] = h;
        f[59] = s;
        f[60..64].copy_from_slice(&3u32.to_be_bytes());
        f[68..84].copy_from_slice(&dix_core::partition::edit::random_guid());
        let sum = !f.iter().fold(0u32, |a, &b| a.wrapping_add(b as u32));
        f[64..68].copy_from_slice(&sum.to_be_bytes());
        let mut d = vec![0u8; 1024];
        d[..8].copy_from_slice(b"cxsparse");
        d[8..16].copy_from_slice(&u64::MAX.to_be_bytes());
        d[16..24].copy_from_slice(&1536u64.to_be_bytes());
        d[24..28].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        d[28..32].copy_from_slice(&(entries as u32).to_be_bytes());
        d[32..36].copy_from_slice(&(BLOCK as u32).to_be_bytes());
        let sum = !d.iter().fold(0u32, |a, &b| a.wrapping_add(b as u32));
        d[36..40].copy_from_slice(&sum.to_be_bytes());
        super::put(file, 0, &f)?;
        super::put(file, 512, &d)?;
        super::put(file, 1536, &vec![0xFF; bat_bytes as usize])?;
        super::put(file, 1536 + bat_bytes, &f)
    }
}

/// The VHD specification's geometry for a disk of `sectors`: (cylinders,
/// heads, sectors per track), as close below the size as it goes.
fn geometry(sectors: u64) -> (u16, u8, u8) {
    let total = sectors.min(65535 * 16 * 255);
    let (spt, heads, cth);
    if total >= 65535 * 16 * 63 {
        spt = 255;
        heads = 16;
        cth = total / spt;
    } else {
        let mut s = 17;
        let mut c = total / s;
        let mut h = c.div_ceil(1024).max(4);
        if c >= h * 1024 || h > 16 {
            s = 31;
            h = 16;
            c = total / s;
        }
        if c >= h * 1024 {
            s = 63;
            h = 16;
            c = total / s;
        }
        spt = s;
        heads = h;
        cth = c;
    }
    ((cth / heads) as u16, heads as u8, spt as u8)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fixed,
    Dynamic,
    Differencing,
}

struct Vhd {
    dev: Box<dyn BlockDevice>,
    kind: Kind,
    size: u64,
    footer: Vec<u8>,
    footer_at: u64,
    // Dynamic and differencing disks.
    bat: Vec<u32>,
    bat_offset: u64,
    block: u64,
    bitmap_len: u64,
    parent: Option<(Box<dyn BlockDevice>, String)>,
    writable: bool,
}

/// The disk size the footer means. Virtual PC and QEMU's own VHDs size the
/// disk by its CHS geometry (the footer's size can be larger); others use
/// the size field.
fn disk_size(f: &[u8]) -> u64 {
    let current = be64(f, 48);
    let (cyl, heads, spt) = (u16::from_be_bytes([f[56], f[57]]) as u64, f[58] as u64, f[59] as u64);
    let chs = cyl * heads * spt * 512;
    let by_geometry = matches!(&f[28..32], b"vpc " | b"qemu");
    let max_geometry = cyl == 65535 && heads == 16 && spt == 255;
    if by_geometry && !max_geometry && chs > 0 { chs.min(current) } else { current }
}

impl Vhd {
    fn open(mut dev: Box<dyn BlockDevice>, ctx: &OpenContext<'_>) -> Result<Self> {
        let (footer_at, f) = footer(dev.as_mut()).ok_or_else(|| damaged("no footer"))?;
        let size = disk_size(&f);
        let kind = match be32(&f, 60) {
            2 => Kind::Fixed,
            3 => Kind::Dynamic,
            4 => Kind::Differencing,
            t => return Err(unsupported(format!("VHD disk type {t}"))),
        };
        let mut vhd = Self {
            writable: !dev.is_read_only() && kind != Kind::Differencing,
            dev,
            kind,
            size,
            footer: f.clone(),
            footer_at,
            bat: Vec::new(),
            bat_offset: 0,
            block: 1 << 21,
            bitmap_len: 512,
            parent: None,
        };
        if kind == Kind::Fixed {
            if size > footer_at {
                return Err(damaged("the file is shorter than its disk"));
            }
            return Ok(vhd);
        }
        let h = read(vhd.dev.as_mut(), be64(&f, 16), 1024).map_err(|e| e.with_context("the dynamic header"))?;
        if &h[..8] != b"cxsparse" {
            return Err(damaged("the dynamic header is missing"));
        }
        vhd.bat_offset = be64(&h, 16);
        let entries = be32(&h, 28) as u64;
        vhd.block = be32(&h, 32) as u64;
        if !vhd.block.is_power_of_two() || vhd.block < 512 || vhd.block > 1 << 28 {
            return Err(damaged(format!("block size {}", vhd.block)));
        }
        if entries * vhd.block < size || entries > 1 << 24 {
            return Err(damaged("the block table does not cover the disk"));
        }
        vhd.bitmap_len = (vhd.block / 512).div_ceil(8).div_ceil(512) * 512;
        let raw = read(vhd.dev.as_mut(), vhd.bat_offset, (entries * 4) as usize)
            .map_err(|e| e.with_context("the block table"))?;
        vhd.bat = (0..entries as usize).map(|i| be32(&raw, i * 4)).collect();
        if kind == Kind::Differencing {
            vhd.parent = Some(open_parent(vhd.dev.as_mut(), &h, ctx)?);
        }
        Ok(vhd)
    }

    fn data_at(&self, i: usize) -> Option<u64> {
        let s = self.bat[i];
        (s != UNALLOCATED).then(|| s as u64 * 512 + self.bitmap_len)
    }
}

/// The parent of a differencing disk: by its locators (relative path
/// first), else by the name in the header.
fn open_parent(dev: &mut dyn BlockDevice, h: &[u8], ctx: &OpenContext<'_>) -> Result<(Box<dyn BlockDevice>, String)> {
    let utf16 = |b: &[u8], le: bool| {
        let units: Vec<u16> = b
            .chunks_exact(2)
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .take_while(|&u| u != 0)
            .collect();
        String::from_utf16_lossy(&units)
    };
    let mut names = Vec::new();
    for pass in [b"W2ru", b"MacX", b"W2ku"] {
        for i in 0..8 {
            let l = &h[576 + i * 24..600 + i * 24];
            if &l[..4] != pass {
                continue;
            }
            let (len, at) = (be32(l, 8) as usize, be64(l, 16));
            let Some(b) = super::read_some(dev, at, len.min(2048)) else { continue };
            let name = if pass == b"MacX" {
                let s = String::from_utf8_lossy(&b).trim_end_matches('\0').to_string();
                s.strip_prefix("file://").unwrap_or(&s).to_string()
            } else {
                utf16(&b, true)
            };
            if !name.is_empty() {
                names.push(name.replace('\\', "/").trim_start_matches("./").to_string());
            }
        }
    }
    let unicode = utf16(&h[64..576], false);
    if !unicode.is_empty() {
        names.push(unicode);
    }
    let mut last = None;
    for name in &names {
        // An absolute Windows path is tried by its file name too.
        let short = name.rsplit('/').next().unwrap_or(name).to_string();
        for n in [name.clone(), short] {
            match ctx.open_parent(&n) {
                Ok(p) => return Ok((p, n)),
                Err(e) => last = Some(e),
            }
        }
    }
    Err(last.unwrap_or_else(|| damaged("a differencing disk without a parent locator")))
}

impl Layout for Vhd {
    fn size(&self) -> u64 {
        self.size
    }
    fn block(&self) -> u64 {
        if self.kind == Kind::Fixed { 1 << 30 } else { self.block }
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if self.kind == Kind::Fixed {
            return self.dev.read_at(offset, buf);
        }
        let i = (offset / self.block) as usize;
        let inner = offset % self.block;
        let Some(data) = self.data_at(i) else {
            return read_parent(self.parent.as_mut().map(|p| p.0.as_mut()), offset, buf);
        };
        if self.kind == Kind::Dynamic {
            return self.dev.read_at(data + inner, buf);
        }
        // Differencing: sector by sector, as the bitmap says.
        let bitmap = read(self.dev.as_mut(), self.bat[i] as u64 * 512, self.bitmap_len as usize)?;
        let mut done = 0usize;
        while done < buf.len() {
            let at = inner + done as u64;
            let sector = (at / 512) as usize;
            let here = bitmap[sector / 8] & (0x80 >> (sector % 8)) != 0;
            // A run of sectors with the same bit.
            let mut end = (sector as u64 + 1) * 512;
            while end < inner + buf.len() as u64 {
                let s = (end / 512) as usize;
                if (bitmap[s / 8] & (0x80 >> (s % 8)) != 0) != here {
                    break;
                }
                end += 512;
            }
            let n = super::upto(end - at, buf.len() - done);
            let part = &mut buf[done..done + n];
            if here {
                self.dev.read_at(data + at, part)?;
            } else {
                read_parent(self.parent.as_mut().map(|p| p.0.as_mut()), offset + done as u64, part)?;
            }
            done += n;
        }
        Ok(())
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if self.kind == Kind::Fixed {
            return self.dev.write_at(offset, data);
        }
        let i = (offset / self.block) as usize;
        let inner = offset % self.block;
        if let Some(at) = self.data_at(i) {
            return self.dev.write_at(at + inner, data);
        }
        // A new block where the footer is; the footer moves after it.
        let start = self.footer_at.div_ceil(512) * 512;
        let sector = u32::try_from(start / 512).map_err(|_| damaged("the file is too large for a VHD"))?;
        let end = start + self.bitmap_len + self.block;
        self.dev.resize(end + 512)?;
        let mut block = vec![0u8; (self.bitmap_len + self.block) as usize];
        block[..self.bitmap_len as usize].fill(0xFF);
        let d = (self.bitmap_len + inner) as usize;
        block[d..d + data.len()].copy_from_slice(data);
        self.dev.write_at(start, &block)?;
        self.dev.write_at(end, &self.footer)?;
        self.footer_at = end;
        self.bat[i] = sector;
        self.dev.write_at(self.bat_offset + i as u64 * 4, &sector.to_be_bytes())
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
        match self.kind {
            Kind::Fixed => "VHD, fixed".to_string(),
            Kind::Dynamic => format!("VHD, dynamic, {} blocks", size_label(self.block)),
            Kind::Differencing => {
                format!("VHD, differencing, parent {}", self.parent.as_ref().map_or("", |p| p.1.as_str()))
            }
        }
    }
}
