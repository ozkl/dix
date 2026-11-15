//! Virtual machine disks: the containers that QEMU, VirtualBox, VMware and
//! Hyper-V keep a virtual disk in. Each is opened as the block device of
//! the disk inside (see [`dix_core::container`]), so its partitions and
//! file systems open like those of a raw image.
//!
//! | format | from | read | write |
//! |--------|------|------|-------|
//! | qcow2 (v2, v3) | QEMU | compressed clusters (zlib, zstd), backing files, external data files | allocating clusters, with refcounts (no snapshots) |
//! | VHD | Virtual PC, Hyper-V, Azure | fixed, dynamic, differencing | fixed, dynamic |
//! | VHDX | Hyper-V | fixed, dynamic, differencing, log replay | fixed, dynamic |
//! | VMDK | VMware | sparse (hosted), stream-optimized, flat, split, `ZERO` extents, snapshots | flat and hosted sparse extents |
//! | VDI | VirtualBox | fixed, dynamic | fixed, dynamic |
//! | DMG (UDIF) | macOS | raw, zlib, bzip2, LZFSE, LZMA, ADC chunks | rewritten as zlib (UDZO) when saved |
//!
//! Every format splits the disk into blocks (clusters, grains) that are
//! present in the file, absent (zeros, or the parent's data in a
//! differencing disk) or, in some, compressed. [`Disk`] turns the reads
//! and writes of a whole device into calls for one block at a time.

mod dmg;
mod qcow2;
mod vdi;
mod vhd;
mod vhdx;
mod vmdk;

#[cfg(all(test, unix, feature = "archive-bzip2", feature = "archive-xz"))]
mod dmg_tests;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;

pub use dmg::DmgFormat;
pub use qcow2::Qcow2Format;
pub use vdi::VdiFormat;
pub use vhd::VhdFormat;
pub use vhdx::VhdxFormat;
pub use vmdk::VmdkFormat;

use dix_core::device::{BlockDevice, DeviceInfo, DeviceKind};
use dix_core::{Error, ErrorKind, Result};

/// The layout of one container format: where each block of the virtual
/// disk is. `read`/`write` are called with ranges inside one block.
pub(crate) trait Layout: Send {
    /// The size of the virtual disk.
    fn size(&self) -> u64;
    /// The block size: no call crosses a multiple of it.
    fn block(&self) -> u64;
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;
    fn write(&mut self, _offset: u64, _data: &[u8]) -> Result<()> {
        Err(Error::read_only())
    }
    /// Can this disk be written (the container file is writable, and the
    /// format and its variant can be)?
    fn writable(&self) -> bool {
        false
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    /// The container file's own info (name, kind).
    fn file(&self) -> DeviceInfo;
    /// "qcow2 v3, 64 KiB clusters".
    fn describe(&self) -> String;
    /// The disk's logical sector size.
    fn sector_size(&self) -> u32 {
        512
    }
}

/// A container's virtual disk as a [`BlockDevice`].
pub(crate) struct Disk<L>(pub L);

impl<L: Layout> Disk<L> {
    fn each<F: FnMut(&mut L, u64, std::ops::Range<usize>) -> Result<()>>(
        &mut self,
        offset: u64,
        len: usize,
        mut f: F,
    ) -> Result<()> {
        dix_core::device::check_range(offset, len, self.0.size())?;
        let block = self.0.block();
        let mut done = 0usize;
        while done < len {
            let at = offset + done as u64;
            let n = upto(block - at % block, len - done);
            f(&mut self.0, at, done..done + n)?;
            done += n;
        }
        Ok(())
    }
}

impl<L: Layout> BlockDevice for Disk<L> {
    fn info(&self) -> DeviceInfo {
        let file = self.0.file();
        DeviceInfo {
            name: file.name,
            kind: if file.kind == DeviceKind::Memory { DeviceKind::Memory } else { DeviceKind::ImageFile },
            size: self.0.size(),
            sector_size: self.0.sector_size(),
            read_only: !self.0.writable(),
            resizable: false,
        }
    }
    fn size(&self) -> u64 {
        self.0.size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.each(offset, buf.len(), |l, at, r| l.read(at, &mut buf[r]))
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if !self.0.writable() {
            return Err(Error::read_only());
        }
        self.each(offset, data.len(), |l, at, r| l.write(at, &data[r]))
    }
    fn flush(&mut self) -> Result<()> {
        self.0.flush()
    }
    fn is_read_only(&self) -> bool {
        !self.0.writable()
    }
    fn container(&self) -> Option<String> {
        Some(self.0.describe())
    }
}

// ---------------------------------------------------------------------------
// Helpers shared by the formats.

/// `min(n, rest)` for a byte count `n` on the disk (64-bit) and a buffer
/// length `rest`: in 64 bits first, so a large `n` is not cut to 32 bits
/// where `usize` is (WebAssembly) — 2^62 would become 0.
pub(crate) fn upto(n: u64, rest: usize) -> usize {
    n.min(rest as u64) as usize
}

pub(crate) fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
pub(crate) fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
pub(crate) fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
pub(crate) fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
pub(crate) fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

pub(crate) fn read(dev: &mut dyn BlockDevice, offset: u64, len: usize) -> Result<Vec<u8>> {
    dix_core::device::read_vec(dev, offset, len)
}

/// Reads what there is of `[offset, offset + len)` (a probe of a file
/// that may be shorter).
pub(crate) fn read_some(dev: &mut dyn BlockDevice, offset: u64, len: usize) -> Option<Vec<u8>> {
    let size = dev.size();
    if offset >= size {
        return None;
    }
    read(dev, offset, len.min((size - offset) as usize)).ok()
}

pub(crate) fn damaged(what: impl std::fmt::Display) -> Error {
    Error::corrupted(what)
}

pub(crate) fn unsupported(what: impl std::fmt::Display) -> Error {
    Error::new(ErrorKind::Unsupported, format!("{what} is not supported"))
}

/// "64 KiB", "2 MiB".
pub(crate) fn size_label(n: u64) -> String {
    if n >= 1 << 20 && n.is_multiple_of(1 << 20) {
        format!("{} MiB", n >> 20)
    } else if n >= 1 << 10 && n.is_multiple_of(1 << 10) {
        format!("{} KiB", n >> 10)
    } else {
        format!("{n} bytes")
    }
}

/// Fills `buf` from the parent disk (a differencing disk's base), or with
/// zeros past its end or without one.
pub(crate) fn read_parent(parent: Option<&mut (dyn BlockDevice + 'static)>, offset: u64, buf: &mut [u8]) -> Result<()> {
    match parent {
        Some(p) => {
            let size = p.size();
            let n = if offset >= size { 0 } else { upto(size - offset, buf.len()) };
            p.read_at(offset, &mut buf[..n])?;
            buf[n..].fill(0);
            Ok(())
        }
        None => {
            buf.fill(0);
            Ok(())
        }
    }
}

/// A few recently used tables (L2 tables, grain tables), by their offset.
pub(crate) struct Cache<V> {
    items: Vec<(u64, V)>,
    cap: usize,
}

impl<V> Cache<V> {
    pub fn new(cap: usize) -> Self {
        Self { items: Vec::with_capacity(cap), cap }
    }

    pub fn get_or<F: FnOnce() -> Result<V>>(&mut self, key: u64, load: F) -> Result<&mut V> {
        let i = match self.items.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                let v = load()?;
                if self.items.len() == self.cap {
                    self.items.remove(0);
                }
                self.items.push((key, v));
                self.items.len() - 1
            }
        };
        // Most recently used last.
        let item = self.items.remove(i);
        self.items.push(item);
        Ok(&mut self.items.last_mut().unwrap().1)
    }
}

/// Raw deflate (no header), as qcow2 compresses clusters, into exactly
/// `out.len()` bytes (a short stream leaves zeros).
pub(crate) fn inflate_raw(data: &[u8], out: &mut [u8]) -> Result<()> {
    // Bytes after the end of the stream (the rest of its last sector) are
    // left alone by the decoder.
    let v = miniz_oxide::inflate::decompress_to_vec_with_limit(data, out.len())
        .map_err(|_| damaged("a compressed cluster does not decompress"))?;
    let n = v.len().min(out.len());
    out[..n].copy_from_slice(&v[..n]);
    out[n..].fill(0);
    Ok(())
}

/// A zlib stream (with its header), as VMDK compresses grains.
pub(crate) fn inflate_zlib(data: &[u8], out: &mut [u8]) -> Result<()> {
    let v = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, out.len())
        .map_err(|_| damaged("a compressed grain does not decompress"))?;
    let n = v.len().min(out.len());
    out[..n].copy_from_slice(&v[..n]);
    out[n..].fill(0);
    Ok(())
}

/// Writes `data` at `at` of a new container file, growing it.
pub(crate) fn put(file: &mut dyn BlockDevice, at: u64, data: &[u8]) -> Result<()> {
    let end = at + data.len() as u64;
    if file.size() < end {
        file.resize(end)?;
    }
    file.write_at(at, data)
}

/// The largest disk the creators make (2 TiB).
pub(crate) const MAX_NEW_DISK: u64 = 2 << 40;

pub(crate) fn check_new_size(size: u64) -> Result<()> {
    if size == 0 || size > MAX_NEW_DISK {
        return Err(Error::invalid_input(format!("a new disk can be 1 byte to {}", size_label(MAX_NEW_DISK))));
    }
    Ok(())
}

/// Registers every container format.
pub fn register(r: &mut dix_core::DriverRegistry) {
    r.register_format(Qcow2Format);
    r.register_format(VhdxFormat);
    r.register_format(VhdFormat);
    r.register_format(VmdkFormat);
    r.register_format(VdiFormat);
    r.register_format(DmgFormat);
}
