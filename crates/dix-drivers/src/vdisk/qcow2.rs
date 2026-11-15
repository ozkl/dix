//! qcow2, QEMU's image format (versions 2 and 3).
//!
//! The disk is cut into clusters (512 bytes to 2 MiB, 64 KiB by default).
//! A two-level table maps them: the L1 table (in memory here) points to L2
//! tables of one cluster each, whose 64-bit big-endian entries give the
//! cluster's place in the file:
//!
//! * bits 9–55: the host offset (0: not allocated, read from the backing
//!   file or as zeros), bit 63 "copied" (its refcount is exactly 1, it may
//!   be written in place), and in v3 bit 0 "reads as zeros";
//! * with bit 62 set, a compressed cluster: the low bits give where the
//!   compressed bytes start and the next ones how many 512-byte sectors
//!   they take (raw deflate, or zstd when the header says so).
//!
//! Every host cluster in use has a reference count, in refcount blocks
//! found through the refcount table; a writer must keep them right (QEMU's
//! `qemu-img check` counts them). New clusters are added at the end of the
//! file, with their refcount set first, so a crash can only leak space.
//!
//! Writing is refused for images with internal snapshots (clusters shared
//! between them would need copy-on-write), external data files, dirty or
//! corrupt flags, and refcount widths under 8 bits.

use super::{Cache, Disk, Layout, be32, be64, damaged, read, read_parent, size_label, unsupported};
use dix_core::Result;
use dix_core::container::{DiskFormat, OpenContext};
use dix_core::device::{BlockDevice, DeviceInfo};

const MAGIC: &[u8; 4] = b"QFI\xfb";
/// The host offset in an L1 or (uncompressed) L2 entry.
const OFFSET_MASK: u64 = 0x00ff_ffff_ffff_fe00;
const COPIED: u64 = 1 << 63;
const COMPRESSED: u64 = 1 << 62;
const ZERO: u64 = 1;

/// Incompatible feature bits.
const INCOMPAT_DIRTY: u64 = 1;
const INCOMPAT_CORRUPT: u64 = 2;
const INCOMPAT_DATA_FILE: u64 = 4;
const INCOMPAT_COMPRESSION: u64 = 8;
const INCOMPAT_EXTL2: u64 = 16;
/// Autoclear bit: the external data file holds the disk as a raw image.
const AUTOCLEAR_DATA_FILE_RAW: u64 = 2;

pub struct Qcow2Format;

impl DiskFormat for Qcow2Format {
    fn name(&self) -> &'static str {
        "qcow2"
    }
    fn probe(&self, dev: &mut dyn BlockDevice) -> bool {
        super::read_some(dev, 0, 8).is_some_and(|h| h.len() == 8 && &h[..4] == MAGIC && matches!(be32(&h, 4), 2 | 3))
    }
    fn open(&self, dev: Box<dyn BlockDevice>, ctx: &OpenContext<'_>) -> Result<Box<dyn BlockDevice>> {
        Ok(Box::new(Disk(Qcow2::open(dev, ctx)?)))
    }

    fn creates(&self) -> Option<(&'static str, &'static str)> {
        Some(("qcow2", "QEMU (qcow2): grows as it is filled"))
    }

    /// A qcow2 v3 image as `qemu-img create` makes it: 64 KiB clusters; the
    /// header, the L1 table, the refcount table and one refcount block in
    /// the first four clusters (counted in it); no data clusters yet.
    fn create(&self, file: &mut dyn BlockDevice, size: u64, _name: &str) -> Result<()> {
        super::check_new_size(size)?;
        const CS: u64 = 1 << 16;
        let l1_size = size.div_ceil(CS).div_ceil(CS / 8);
        if l1_size * 8 > CS {
            return Err(unsupported("disks this large"));
        }
        let mut h = vec![0u8; CS as usize];
        h[..4].copy_from_slice(MAGIC);
        h[4..8].copy_from_slice(&3u32.to_be_bytes());
        h[20..24].copy_from_slice(&16u32.to_be_bytes());
        h[24..32].copy_from_slice(&size.to_be_bytes());
        h[36..40].copy_from_slice(&(l1_size as u32).to_be_bytes());
        h[40..48].copy_from_slice(&CS.to_be_bytes());
        h[48..56].copy_from_slice(&(2 * CS).to_be_bytes());
        h[56..60].copy_from_slice(&1u32.to_be_bytes());
        h[96..100].copy_from_slice(&4u32.to_be_bytes()); // 16-bit refcounts
        h[100..104].copy_from_slice(&104u32.to_be_bytes());
        let mut rest = vec![0u8; 3 * CS as usize];
        // The refcount table's first entry: the block in cluster 3.
        rest[CS as usize..CS as usize + 8].copy_from_slice(&(3 * CS).to_be_bytes());
        // That block: clusters 0–3 used once.
        for c in 0..4 {
            let at = 2 * CS as usize + c * 2;
            rest[at..at + 2].copy_from_slice(&1u16.to_be_bytes());
        }
        super::put(file, 0, &h)?;
        super::put(file, CS, &rest)
    }
}

struct Qcow2 {
    dev: Box<dyn BlockDevice>,
    /// An external data file (the clusters' data lives there).
    data: Option<Box<dyn BlockDevice>>,
    data_raw: bool,
    parent: Option<(Box<dyn BlockDevice>, String)>,
    version: u32,
    cluster_bits: u32,
    size: u64,
    l1: Vec<u64>,
    l1_offset: u64,
    l2: Cache<Vec<u64>>,
    zstd: bool,
    /// The last decompressed cluster, by its L2 entry.
    unpacked: Option<(u64, Vec<u8>)>,
    writable: bool,
    // Writing: the refcount table, its width and where the file ends.
    refcount_bits: u32,
    rt: Vec<u64>,
    rt_offset: u64,
    end: u64,
    /// Autoclear bits to drop before the first write (features we do not
    /// keep up to date, such as dirty bitmaps).
    autoclear: u64,
}

impl Qcow2 {
    fn open(mut dev: Box<dyn BlockDevice>, ctx: &OpenContext<'_>) -> Result<Self> {
        let n = 104.min(dev.size() as usize);
        let h = read(dev.as_mut(), 0, n)?;
        if h.len() < 72 {
            return Err(damaged("the header is cut short"));
        }
        let version = be32(&h, 4);
        let cluster_bits = be32(&h, 20);
        if !(9..=21).contains(&cluster_bits) {
            return Err(damaged(format!("cluster size 2^{cluster_bits}")));
        }
        let cs = 1u64 << cluster_bits;
        let size = be64(&h, 24);
        if be32(&h, 32) != 0 {
            return Err(unsupported("an encrypted qcow2 image"));
        }
        let l1_size = be32(&h, 36) as u64;
        let l1_offset = be64(&h, 40);
        let rt_offset = be64(&h, 48);
        let rt_clusters = be32(&h, 56) as u64;
        let snapshots = be32(&h, 60);
        let (mut incompat, mut autoclear, mut refcount_order, mut header_len) = (0, 0, 4, 72);
        if version >= 3 {
            if h.len() < 104 {
                return Err(damaged("the header is cut short"));
            }
            incompat = be64(&h, 72);
            autoclear = be64(&h, 88);
            refcount_order = be32(&h, 96);
            header_len = be32(&h, 100) as u64;
        }
        if incompat & INCOMPAT_EXTL2 != 0 {
            return Err(unsupported("qcow2 with extended L2 entries (subclusters)"));
        }
        let known = INCOMPAT_DIRTY | INCOMPAT_CORRUPT | INCOMPAT_DATA_FILE | INCOMPAT_COMPRESSION;
        if incompat & !known != 0 {
            return Err(unsupported(format!("qcow2 incompatible features {:#x}", incompat & !known)));
        }
        let mut zstd = false;
        if incompat & INCOMPAT_COMPRESSION != 0 && header_len > 104 {
            let t = read(dev.as_mut(), 104, 1)?[0];
            match t {
                0 => {}
                1 => zstd = true,
                _ => return Err(unsupported(format!("qcow2 compression type {t}"))),
            }
        }
        if size > 1 << 62 || l1_size > (1 << 26) {
            return Err(damaged("the disk size is out of range"));
        }
        let l2_entries = cs / 8;
        if l1_size < size.div_ceil(cs).div_ceil(l2_entries) {
            return Err(damaged("the L1 table is too small for the disk"));
        }
        let raw = read(dev.as_mut(), l1_offset, (l1_size * 8) as usize).map_err(|e| e.with_context("the L1 table"))?;
        let l1 = (0..l1_size as usize).map(|i| be64(&raw, i * 8)).collect();

        // Header extensions: the backing file's format, the data file's name.
        let (mut backing_fmt, mut data_name) = (None, None);
        let mut at = header_len;
        let file_end = dev.size();
        while at + 8 <= file_end.min(cs) {
            let e = read(dev.as_mut(), at, 8)?;
            let (kind, len) = (be32(&e, 0), be32(&e, 4) as u64);
            if kind == 0 || at + 8 + len > cs.min(file_end) {
                break;
            }
            let body = read(dev.as_mut(), at + 8, len as usize)?;
            let text = || String::from_utf8_lossy(&body).trim_end_matches('\0').to_string();
            match kind {
                0xE279_2ACA => backing_fmt = Some(text()),
                0x4441_5441 => data_name = Some(text()),
                0x0537_BE77 => return Err(unsupported("an encrypted qcow2 image")),
                _ => {}
            }
            at += 8 + len.div_ceil(8) * 8;
        }

        let backing_offset = be64(&h, 8);
        let backing_len = be32(&h, 16) as usize;
        let parent = if backing_offset != 0 && backing_len > 0 {
            let name =
                String::from_utf8_lossy(&read(dev.as_mut(), backing_offset, backing_len.min(4096))?).into_owned();
            let p = if backing_fmt.as_deref() == Some("raw") {
                ctx.open_file(&name, false)?
            } else {
                ctx.open_parent(&name)?
            };
            Some((p, name))
        } else {
            None
        };
        let data = if incompat & INCOMPAT_DATA_FILE != 0 {
            let name = data_name.ok_or_else(|| damaged("the external data file has no name"))?;
            Some(ctx.open_file(&name, false)?)
        } else {
            None
        };

        let rt = if rt_clusters > 0 && rt_clusters < (1 << 20) {
            let raw = read(dev.as_mut(), rt_offset, (rt_clusters * cs) as usize)
                .map_err(|e| e.with_context("the refcount table"))?;
            (0..raw.len() / 8).map(|i| be64(&raw, i * 8)).collect()
        } else {
            Vec::new()
        };
        let writable = !dev.is_read_only()
            && snapshots == 0
            && data.is_none()
            && incompat & (INCOMPAT_DIRTY | INCOMPAT_CORRUPT) == 0
            && (3..=6).contains(&refcount_order)
            && !rt.is_empty();
        Ok(Self {
            end: file_end.div_ceil(cs) * cs,
            dev,
            data,
            data_raw: autoclear & AUTOCLEAR_DATA_FILE_RAW != 0,
            parent,
            version,
            cluster_bits,
            size,
            l1,
            l1_offset,
            l2: Cache::new(32),
            zstd,
            unpacked: None,
            writable,
            refcount_bits: 1 << refcount_order,
            rt,
            rt_offset,
            autoclear: if version >= 3 { autoclear } else { 0 },
        })
    }

    fn cs(&self) -> u64 {
        1 << self.cluster_bits
    }

    /// The L2 entry of guest cluster `g` (0 when its L2 table is absent).
    fn entry(&mut self, g: u64) -> Result<u64> {
        let n = self.cs() / 8;
        let (i1, i2) = ((g / n) as usize, (g % n) as usize);
        let table = self.l1.get(i1).copied().unwrap_or(0) & OFFSET_MASK;
        if table == 0 {
            return Ok(0);
        }
        let cs = self.cs();
        let dev = &mut self.dev;
        let l2 = self.l2.get_or(table, || {
            let raw = read(dev.as_mut(), table, cs as usize).map_err(|e| e.with_context("an L2 table"))?;
            Ok((0..raw.len() / 8).map(|i| be64(&raw, i * 8)).collect())
        })?;
        Ok(l2[i2])
    }

    /// Where a compressed cluster's bytes are: (offset, sectors).
    fn compressed_at(&self, e: u64) -> (u64, u64) {
        let shift = 62 - (self.cluster_bits - 8);
        let offset = e & ((1 << shift) - 1);
        let sectors = ((e >> shift) & ((1 << (self.cluster_bits - 8)) - 1)) + 1;
        (offset, sectors)
    }

    fn unpack(&mut self, e: u64) -> Result<&[u8]> {
        if self.unpacked.as_ref().is_none_or(|(k, _)| *k != e) {
            let (offset, sectors) = self.compressed_at(e);
            let len = (sectors * 512 - (offset & 511)).min(self.dev.size().saturating_sub(offset));
            let packed = read(self.dev.as_mut(), offset, len as usize)?;
            let mut out = vec![0u8; self.cs() as usize];
            if self.zstd {
                unzstd(&packed, &mut out)?;
            } else {
                super::inflate_raw(&packed, &mut out)?;
            }
            self.unpacked = Some((e, out));
        }
        Ok(&self.unpacked.as_ref().unwrap().1)
    }

    // --- Writing ----------------------------------------------------------

    fn refcount_block_entries(&self) -> u64 {
        self.cs() * 8 / self.refcount_bits as u64
    }

    /// Takes `n` clusters at the end of the file (zeroed), without refcounts.
    fn take(&mut self, n: u64) -> Result<u64> {
        let at = self.end;
        self.end += n * self.cs();
        if self.dev.size() < self.end {
            self.dev.resize(self.end)?;
        }
        Ok(at)
    }

    fn refcount_place(&mut self, cluster: u64) -> Result<(u64, usize)> {
        let per = self.refcount_block_entries();
        let bi = (cluster / per) as usize;
        if bi >= self.rt.len() {
            self.grow_refcount_table(bi + 1)?;
        }
        if self.rt[bi] == 0 {
            let block = self.take(1)?;
            self.dev.write_at(block, &vec![0u8; self.cs() as usize])?;
            self.rt[bi] = block;
            self.dev.write_at(self.rt_offset + bi as u64 * 8, &block.to_be_bytes())?;
            // The new block counts itself (perhaps in itself).
            self.set_refcount(block >> self.cluster_bits, 1)?;
        }
        let bytes = (cluster % per) * self.refcount_bits as u64 / 8;
        Ok((self.rt[bi] + bytes, (self.refcount_bits / 8) as usize))
    }

    fn refcount(&mut self, cluster: u64) -> Result<u64> {
        let (at, w) = self.refcount_place(cluster)?;
        let b = read(self.dev.as_mut(), at, w)?;
        Ok(b.iter().fold(0u64, |v, &x| v << 8 | x as u64))
    }

    fn set_refcount(&mut self, cluster: u64, value: u64) -> Result<()> {
        let (at, w) = self.refcount_place(cluster)?;
        self.dev.write_at(at, &value.to_be_bytes()[8 - w..])
    }

    /// Moves the refcount table to a bigger place at the end of the file.
    fn grow_refcount_table(&mut self, need: usize) -> Result<()> {
        let per_cluster = (self.cs() / 8) as usize;
        let entries = (need * 2).max(self.rt.len() * 2).div_ceil(per_cluster) * per_cluster;
        let clusters = (entries / per_cluster) as u64;
        let (old_at, old_clusters) = (self.rt_offset, (self.rt.len() / per_cluster) as u64);
        let at = self.take(clusters)?;
        self.rt.resize(entries, 0);
        let raw: Vec<u8> = self.rt.iter().flat_map(|e| e.to_be_bytes()).collect();
        self.dev.write_at(at, &raw)?;
        self.dev.write_at(48, &at.to_be_bytes())?;
        self.dev.write_at(56, &(clusters as u32).to_be_bytes())?;
        self.rt_offset = at;
        for c in 0..clusters {
            self.set_refcount((at >> self.cluster_bits) + c, 1)?;
        }
        for c in 0..old_clusters {
            self.set_refcount((old_at >> self.cluster_bits) + c, 0)?;
        }
        Ok(())
    }

    /// A new cluster, counted, at the end of the file.
    fn alloc(&mut self) -> Result<u64> {
        let at = self.take(1)?;
        self.set_refcount(at >> self.cluster_bits, 1)?;
        Ok(at)
    }

    /// Drops one reference to each host cluster in `[offset, offset + len)`.
    fn release(&mut self, offset: u64, len: u64) -> Result<()> {
        let first = offset >> self.cluster_bits;
        let last = (offset + len - 1) >> self.cluster_bits;
        for c in first..=last {
            let n = self.refcount(c)?;
            self.set_refcount(c, n.saturating_sub(1))?;
        }
        Ok(())
    }

    fn set_entry(&mut self, g: u64, value: u64) -> Result<()> {
        let n = self.cs() / 8;
        let (i1, i2) = ((g / n) as usize, g % n);
        if i1 >= self.l1.len() {
            return Err(damaged("a write past the L1 table"));
        }
        let mut table = self.l1[i1] & OFFSET_MASK;
        if table == 0 {
            table = self.alloc()?;
            self.dev.write_at(table, &vec![0u8; self.cs() as usize])?;
            self.l1[i1] = table | COPIED;
            self.dev.write_at(self.l1_offset + i1 as u64 * 8, &self.l1[i1].to_be_bytes())?;
        } else if self.l1[i1] & COPIED == 0 {
            return Err(unsupported("writing to an L2 table shared with a snapshot"));
        }
        self.dev.write_at(table + i2 * 8, &value.to_be_bytes())?;
        let cs = self.cs();
        let dev = &mut self.dev;
        let l2 = self.l2.get_or(table, || {
            let raw = read(dev.as_mut(), table, cs as usize)?;
            Ok((0..raw.len() / 8).map(|i| be64(&raw, i * 8)).collect())
        })?;
        l2[i2 as usize] = value;
        Ok(())
    }
}

#[cfg(feature = "archive-zstd")]
fn unzstd(packed: &[u8], out: &mut [u8]) -> Result<()> {
    use std::io::Read;
    let mut src = packed;
    let mut dec = ruzstd::decoding::StreamingDecoder::new(&mut src)
        .map_err(|_| damaged("a zstd-compressed cluster does not decompress"))?;
    let mut n = 0;
    while n < out.len() {
        match dec.read(&mut out[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => return Err(damaged("a zstd-compressed cluster does not decompress")),
        }
    }
    out[n..].fill(0);
    Ok(())
}

#[cfg(not(feature = "archive-zstd"))]
fn unzstd(_: &[u8], _: &mut [u8]) -> Result<()> {
    Err(unsupported("zstd-compressed qcow2 clusters (this build has no zstd)"))
}

impl Layout for Qcow2 {
    fn size(&self) -> u64 {
        self.size
    }
    fn block(&self) -> u64 {
        self.cs()
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let g = offset >> self.cluster_bits;
        let inner = (offset & (self.cs() - 1)) as usize;
        let e = self.entry(g)?;
        if e & COMPRESSED != 0 {
            let c = self.unpack(e)?;
            buf.copy_from_slice(&c[inner..inner + buf.len()]);
            return Ok(());
        }
        let host = e & OFFSET_MASK;
        if self.version >= 3 && e & ZERO != 0 {
            buf.fill(0);
        } else if let Some(data) = self.data.as_mut() {
            if host == 0 && !self.data_raw {
                read_parent(self.parent.as_mut().map(|p| p.0.as_mut()), offset, buf)?;
            } else {
                let at = if self.data_raw { offset } else { host + inner as u64 };
                data.read_at(at, buf)?;
            }
        } else if host == 0 {
            read_parent(self.parent.as_mut().map(|p| p.0.as_mut()), offset, buf)?;
        } else {
            self.dev.read_at(host + inner as u64, buf)?;
        }
        Ok(())
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if self.autoclear != 0 {
            self.dev.write_at(88, &0u64.to_be_bytes())?;
            self.autoclear = 0;
        }
        let cs = self.cs();
        let g = offset >> self.cluster_bits;
        let inner = (offset & (cs - 1)) as usize;
        let e = self.entry(g)?;
        let host = e & OFFSET_MASK;
        let plain = e & COMPRESSED == 0 && !(self.version >= 3 && e & ZERO != 0);
        if plain && host != 0 && e & COPIED != 0 {
            return self.dev.write_at(host + inner as u64, data);
        }
        // A new cluster: what the guest sees there now, with the change.
        let mut cluster = vec![0u8; cs as usize];
        self.read(g << self.cluster_bits, &mut cluster)?;
        cluster[inner..inner + data.len()].copy_from_slice(data);
        let new = self.alloc()?;
        self.dev.write_at(new, &cluster)?;
        self.set_entry(g, new | COPIED)?;
        if e & COMPRESSED != 0 {
            let (at, sectors) = self.compressed_at(e);
            self.release(at & !511, sectors * 512)?;
            self.unpacked = None;
        } else if host != 0 {
            self.release(host, cs)?;
        }
        Ok(())
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
        let mut s = format!("qcow2 v{}, {} clusters", self.version, size_label(self.cs()));
        if let Some((_, name)) = &self.parent {
            s.push_str(&format!(", backing file {name}"));
        }
        if self.data.is_some() {
            s.push_str(", external data file");
        }
        s
    }
}
