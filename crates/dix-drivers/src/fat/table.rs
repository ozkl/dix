//! The file allocation table: one entry per cluster, either free (0), the
//! next cluster of a chain, bad, or end-of-chain.
//!
//! The table is read in 64 KiB pages on demand (FAT12, whose 12-bit entries
//! straddle bytes, as one page) and written back to every copy on
//! [`Fat::flush`]. Entry 1 holds the volume's "clean shutdown" and "no I/O
//! errors" bits on FAT16/32.

use super::bpb::{FatType, Layout};
use dix_core::{BlockDevice, Error, ErrorKind, Result};
use std::collections::BTreeMap;

const PAGE: u64 = 64 << 10;

/// Bits per entry: 12, 16, 28 (FAT32: the top 4 are reserved) or 32
/// (exFAT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Width {
    W12,
    W16,
    W28,
    W32,
}

pub struct Fat {
    ty: Width,
    /// Where it is read from (the active copy), and every copy written.
    read_at: u64,
    copies: Vec<u64>,
    bytes: u64,
    page_size: u64,
    pages: BTreeMap<u64, (Vec<u8>, bool)>,
    max_cluster: u32,
    /// Where the next allocation starts looking.
    pub next_free: u32,
    /// Free clusters, once known.
    pub free: Option<u32>,
}

impl Fat {
    pub fn new(l: &Layout) -> Fat {
        let copies = l.fat_copies();
        let ty = match l.fat_type {
            FatType::Fat12 => Width::W12,
            FatType::Fat16 => Width::W16,
            FatType::Fat32 => Width::W28,
        };
        Fat {
            ty,
            read_at: copies[0],
            copies,
            bytes: l.fat_bytes(),
            page_size: if ty == Width::W12 { l.fat_bytes() } else { PAGE },
            pages: BTreeMap::new(),
            max_cluster: l.max_cluster(),
            next_free: 2,
            free: None,
        }
    }

    /// An exFAT FAT (32-bit entries; one copy in use).
    pub fn exfat(offset: u64, bytes: u64, max_cluster: u32) -> Fat {
        Fat {
            ty: Width::W32,
            read_at: offset,
            copies: vec![offset],
            bytes,
            page_size: PAGE,
            pages: BTreeMap::new(),
            max_cluster,
            next_free: 2,
            free: None,
        }
    }

    pub fn eoc(&self) -> u32 {
        match self.ty {
            Width::W12 => 0xFFF,
            Width::W16 => 0xFFFF,
            Width::W28 => 0x0FFF_FFFF,
            Width::W32 => 0xFFFF_FFFF,
        }
    }

    /// End of chain (or something past the last cluster used as such).
    pub fn is_eoc(&self, v: u32) -> bool {
        match self.ty {
            Width::W12 => v >= 0xFF8,
            Width::W16 => v >= 0xFFF8,
            Width::W28 => v >= 0x0FFF_FFF8,
            Width::W32 => v >= 0xFFFF_FFF8,
        }
    }

    pub fn is_bad(&self, v: u32) -> bool {
        match self.ty {
            Width::W12 => v == 0xFF7,
            Width::W16 => v == 0xFFF7,
            Width::W28 => v == 0x0FFF_FFF7,
            Width::W32 => v == 0xFFFF_FFF7,
        }
    }

    pub fn max_cluster(&self) -> u32 {
        self.max_cluster
    }

    fn page(&mut self, dev: &mut dyn BlockDevice, index: u64) -> Result<&mut (Vec<u8>, bool)> {
        if !self.pages.contains_key(&index) {
            let start = index * self.page_size;
            let len = (self.bytes - start).min(self.page_size) as usize;
            let mut v = vec![0u8; len];
            dev.read_at(self.read_at + start, &mut v)?;
            self.pages.insert(index, (v, false));
        }
        Ok(self.pages.get_mut(&index).expect("just inserted"))
    }

    /// (byte offset in the FAT, entry bytes).
    fn place(&self, n: u32) -> (u64, usize) {
        match self.ty {
            Width::W12 => (n as u64 + n as u64 / 2, 2),
            Width::W16 => (n as u64 * 2, 2),
            Width::W28 | Width::W32 => (n as u64 * 4, 4),
        }
    }

    pub fn get(&mut self, dev: &mut dyn BlockDevice, n: u32) -> Result<u32> {
        let (off, len) = self.place(n);
        let ty = self.ty;
        let ps = self.page_size;
        let p = &self.page(dev, off / ps)?.0;
        let o = (off % ps) as usize;
        Ok(match ty {
            Width::W12 => {
                let v = u16::from_le_bytes([p[o], p[o + 1]]) as u32;
                if n & 1 == 1 { v >> 4 } else { v & 0xFFF }
            }
            Width::W16 => u16::from_le_bytes([p[o], p[o + 1]]) as u32,
            Width::W28 => u32::from_le_bytes(p[o..o + len].try_into().unwrap()) & 0x0FFF_FFFF,
            Width::W32 => u32::from_le_bytes(p[o..o + len].try_into().unwrap()),
        })
    }

    pub fn set(&mut self, dev: &mut dyn BlockDevice, n: u32, v: u32) -> Result<()> {
        let (off, _) = self.place(n);
        let ty = self.ty;
        let ps = self.page_size;
        let (p, dirty) = self.page(dev, off / ps)?;
        *dirty = true;
        let o = (off % ps) as usize;
        match ty {
            Width::W12 => {
                let old = u16::from_le_bytes([p[o], p[o + 1]]);
                let new = if n & 1 == 1 {
                    (old & 0x000F) | ((v as u16 & 0xFFF) << 4)
                } else {
                    (old & 0xF000) | (v as u16 & 0xFFF)
                };
                p[o..o + 2].copy_from_slice(&new.to_le_bytes());
            }
            Width::W16 => p[o..o + 2].copy_from_slice(&(v as u16).to_le_bytes()),
            Width::W28 => {
                // The top four bits are reserved: keep them.
                let old = u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
                p[o..o + 4].copy_from_slice(&((old & 0xF000_0000) | (v & 0x0FFF_FFFF)).to_le_bytes());
            }
            Width::W32 => p[o..o + 4].copy_from_slice(&v.to_le_bytes()),
        }
        Ok(())
    }

    /// The clusters of the chain starting at `start` (empty for 0).
    pub fn chain(&mut self, dev: &mut dyn BlockDevice, start: u32) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut c = start;
        if c == 0 {
            return Ok(out);
        }
        loop {
            if c < 2 || c > self.max_cluster {
                return Err(Error::corrupted(format!("a cluster chain points outside the volume ({c})")));
            }
            out.push(c);
            if out.len() > self.max_cluster as usize {
                return Err(Error::corrupted("a cluster chain loops"));
            }
            let next = self.get(dev, c)?;
            if self.is_eoc(next) {
                return Ok(out);
            }
            if next == 0 || self.is_bad(next) {
                return Err(Error::corrupted(format!("a cluster chain is broken at cluster {c}")));
            }
            c = next;
        }
    }

    /// Allocates `count` clusters as a chain, linked after `after` if given;
    /// returns them in order. Nothing changes if there is not enough room.
    pub fn alloc(&mut self, dev: &mut dyn BlockDevice, count: u32, after: Option<u32>) -> Result<Vec<u32>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        if self.free.is_some_and(|f| f < count) {
            return Err(Error::new(ErrorKind::NoSpace, "the volume is full"));
        }
        let total = self.max_cluster - 1;
        let mut found = Vec::with_capacity(count as usize);
        let start = self.next_free.clamp(2, self.max_cluster);
        let mut c = start;
        for _ in 0..total {
            if self.get(dev, c)? == 0 {
                found.push(c);
                if found.len() == count as usize {
                    break;
                }
            }
            c = if c >= self.max_cluster { 2 } else { c + 1 };
        }
        if found.len() < count as usize {
            self.free = Some(found.len() as u32);
            return Err(Error::new(ErrorKind::NoSpace, "the volume is full"));
        }
        let eoc = self.eoc();
        for w in found.windows(2) {
            self.set(dev, w[0], w[1])?;
        }
        self.set(dev, *found.last().unwrap(), eoc)?;
        if let Some(prev) = after {
            self.set(dev, prev, found[0])?;
        }
        let last = *found.last().unwrap();
        self.next_free = if last >= self.max_cluster { 2 } else { last + 1 };
        if let Some(f) = &mut self.free {
            *f -= count;
        }
        Ok(found)
    }

    /// Frees a whole chain.
    pub fn free_chain(&mut self, dev: &mut dyn BlockDevice, start: u32) -> Result<()> {
        for c in self.chain(dev, start)? {
            self.set(dev, c, 0)?;
            if let Some(f) = &mut self.free {
                *f += 1;
            }
        }
        Ok(())
    }

    /// Ends a chain at `last` and frees what came after it.
    pub fn truncate_after(&mut self, dev: &mut dyn BlockDevice, last: u32) -> Result<()> {
        let next = self.get(dev, last)?;
        let eoc = self.eoc();
        self.set(dev, last, eoc)?;
        if !self.is_eoc(next) && next >= 2 {
            self.free_chain(dev, next)?;
        }
        Ok(())
    }

    pub fn count_free(&mut self, dev: &mut dyn BlockDevice) -> Result<u32> {
        let mut n = 0;
        for c in 2..=self.max_cluster {
            if self.get(dev, c)? == 0 {
                n += 1;
            }
        }
        self.free = Some(n);
        Ok(n)
    }

    /// FAT16/32: entry 1's "clean shutdown" bit (`None` on FAT12).
    pub fn clean_bit(&mut self, dev: &mut dyn BlockDevice) -> Result<Option<bool>> {
        Ok(match self.ty {
            Width::W12 => None,
            Width::W16 => Some(self.get(dev, 1)? & 0x8000 != 0),
            Width::W28 => Some(self.get(dev, 1)? & 0x0800_0000 != 0),
            Width::W32 => None,
        })
    }

    pub fn set_clean_bit(&mut self, dev: &mut dyn BlockDevice, clean: bool) -> Result<()> {
        let bit = match self.ty {
            Width::W12 | Width::W32 => return Ok(()),
            Width::W16 => 0x8000,
            Width::W28 => 0x0800_0000,
        };
        let v = self.get(dev, 1)?;
        self.set(dev, 1, if clean { v | bit } else { v & !bit })
    }

    /// Writes changed pages to every FAT copy.
    pub fn flush(&mut self, dev: &mut dyn BlockDevice) -> Result<()> {
        for (&i, (data, dirty)) in self.pages.iter_mut() {
            if *dirty {
                for &copy in &self.copies {
                    dev.write_at(copy + i * self.page_size, data)?;
                }
                *dirty = false;
            }
        }
        Ok(())
    }

    /// Drops unchanged pages (after a long scan) to free memory.
    pub fn trim(&mut self) {
        if self.pages.len() > 256 {
            self.pages.retain(|_, (_, dirty)| *dirty);
        }
    }
}
