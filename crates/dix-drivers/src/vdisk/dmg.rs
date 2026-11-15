//! Apple disk images (`.dmg`, UDIF), read-only.
//!
//! A UDIF image ends with a 512-byte big-endian trailer (`koly`) giving
//! where its data fork is and where its block tables are: in an XML
//! property list (`resource-fork` → `blkx`, each entry's `Data` a
//! base64 `mish` table), or in a classic resource fork (older images).
//! Each `mish` table covers a run of 512-byte sectors of the disk — usually
//! one per partition, the partition map among them — cut into chunks
//! (typically 1 MiB), each stored one way:
//!
//! | type | chunk |
//! |------|-------|
//! | `0x00000000`, `0x00000002` | zeros (not stored) |
//! | `0x00000001` | raw |
//! | `0x80000004` | ADC (Apple Data Compression, old images) |
//! | `0x80000005` | zlib (UDZO) |
//! | `0x80000006` | bzip2 (UDBZ) |
//! | `0x80000007` | LZFSE (ULFO) |
//! | `0x80000008` | LZMA, in an xz stream (ULMO) |
//!
//! The disk inside usually holds an Apple partition map or a GPT with one
//! HFS+ or APFS partition, or the file system alone. Encrypted images
//! (`encrcdsa`), sparse images and segmented images are not read.
//!
//! **Writing** rewrites the whole image, as archives are: changed 1 MiB
//! pages are kept in memory over the image, and when it is flushed (on
//! sync and eject, or when the pages pass 256 MiB) the file is written
//! again as a zlib-compressed image (UDZO): one block table for the disk,
//! chunks not changed copied as they were, changed ones compressed again,
//! zeros stored as nothing; checksums as macOS checks them. New images are
//! made the same way.

use super::{Disk, Layout, be32, be64, damaged, read, unsupported};
use dix_core::Result;
use dix_core::container::{DiskFormat, OpenContext};
use dix_core::device::{BlockDevice, DeviceInfo};

const ZERO: u32 = 0;
const RAW: u32 = 1;
const IGNORE: u32 = 2;
const ADC: u32 = 0x8000_0004;
const ZLIB: u32 = 0x8000_0005;
const BZIP2: u32 = 0x8000_0006;
const LZFSE: u32 = 0x8000_0007;
const LZMA: u32 = 0x8000_0008;
const COMMENT: u32 = 0x7FFF_FFFE;
const END: u32 = 0xFFFF_FFFF;

pub struct DmgFormat;

fn trailer(dev: &mut dyn BlockDevice) -> Option<Vec<u8>> {
    let size = dev.size();
    if size < 512 {
        return None;
    }
    super::read_some(dev, size - 512, 512).filter(|t| t.starts_with(b"koly"))
}

impl DiskFormat for DmgFormat {
    fn name(&self) -> &'static str {
        "DMG"
    }
    fn probe(&self, dev: &mut dyn BlockDevice) -> bool {
        trailer(dev).is_some() || super::read_some(dev, 0, 8).is_some_and(|h| h == b"encrcdsa")
    }
    fn open(&self, dev: Box<dyn BlockDevice>, _ctx: &OpenContext<'_>) -> Result<Box<dyn BlockDevice>> {
        Ok(Box::new(Disk(Dmg::open(dev)?)))
    }

    fn creates(&self) -> Option<(&'static str, &'static str)> {
        Some(("dmg", "Apple disk image (DMG, zlib-compressed): rewritten when saved"))
    }

    /// An empty zlib-compressed image (UDZO): all zeros, so no chunk data.
    fn create(&self, file: &mut dyn BlockDevice, size: u64, _name: &str) -> Result<()> {
        super::check_new_size(size)?;
        let size = size.div_ceil(512) * 512;
        let sectors = size / 512;
        let table: Vec<_> =
            (0..sectors).step_by(ZERO_RUN as usize).map(|s| (IGNORE, s, ZERO_RUN.min(sectors - s), 0, 0)).collect();
        super::put(file, 0, &udif(Vec::new(), &table, size, 0))
    }
}

#[derive(Clone, Copy, Debug)]
struct Chunk {
    /// First byte and length on the disk.
    start: u64,
    len: u64,
    kind: u32,
    /// Where its data is in the file, and how long.
    at: u64,
    stored: u64,
}

struct Dmg {
    dev: Box<dyn BlockDevice>,
    size: u64,
    chunks: Vec<Chunk>,
    /// The last chunks decompressed, by their start.
    cache: Vec<(u64, Vec<u8>)>,
    /// Changed pages (index → PAGE bytes), not written to the file yet.
    pages: std::collections::BTreeMap<u64, Vec<u8>>,
    writable: bool,
}

/// The unit of changes, and of the chunks written: 1 MiB (2048 sectors),
/// as hdiutil's.
const PAGE: u64 = 1 << 20;
/// Changes kept in memory before the image is rewritten anyway.
const MAX_PENDING: usize = 256;
/// The longest run of zeros one chunk stands for, in sectors (32 MiB).
const ZERO_RUN: u64 = 65536;

impl Dmg {
    fn open(mut dev: Box<dyn BlockDevice>) -> Result<Self> {
        if super::read_some(dev.as_mut(), 0, 8).is_some_and(|h| h == b"encrcdsa") {
            return Err(unsupported("an encrypted disk image"));
        }
        let t = trailer(dev.as_mut()).ok_or_else(|| damaged("the UDIF trailer is missing"))?;
        if be32(&t, 60) > 1 {
            return Err(unsupported("a segmented disk image (.dmgpart)"));
        }
        let data_fork = be64(&t, 24);
        let (xml_at, xml_len) = (be64(&t, 216), be64(&t, 224));
        let (rsrc_at, rsrc_len) = (be64(&t, 40), be64(&t, 48));
        let tables = if xml_len > 0 {
            if xml_len > 64 << 20 {
                return Err(damaged("the property list is too large"));
            }
            let xml = read(dev.as_mut(), xml_at, xml_len as usize).map_err(|e| e.with_context("the property list"))?;
            blkx_from_plist(&String::from_utf8_lossy(&xml))?
        } else if rsrc_len > 0 {
            if rsrc_len > 64 << 20 {
                return Err(damaged("the resource fork is too large"));
            }
            let r = read(dev.as_mut(), rsrc_at, rsrc_len as usize).map_err(|e| e.with_context("the resource fork"))?;
            blkx_from_resource_fork(&r)?
        } else {
            return Err(damaged("the image has no block tables"));
        };
        let mut chunks = Vec::new();
        for m in &tables {
            if m.len() < 204 || &m[..4] != b"mish" {
                return Err(damaged("a block table is damaged"));
            }
            let (first, base) = (be64(m, 8), be64(m, 24));
            let n = be32(m, 200) as usize;
            for i in 0..n {
                let c = m.get(204 + i * 40..244 + i * 40).ok_or_else(|| damaged("a block table is cut short"))?;
                let kind = be32(c, 0);
                if kind == COMMENT || kind == END {
                    continue;
                }
                let (sector, count) = (be64(c, 8), be64(c, 16));
                if count == 0 {
                    continue;
                }
                chunks.push(Chunk {
                    start: (first + sector) * 512,
                    len: count * 512,
                    kind,
                    at: data_fork + base + be64(c, 24),
                    stored: be64(c, 32),
                });
            }
        }
        chunks.sort_by_key(|c| c.start);
        let end = chunks.last().map_or(0, |c| c.start + c.len);
        let sectors = be64(&t, 492);
        let size = if sectors > 0 { (sectors * 512).max(end) } else { end };
        for c in &chunks {
            if !matches!(c.kind, ZERO | IGNORE) && c.at + c.stored > dev.size() {
                return Err(damaged("a chunk lies past the end of the image"));
            }
            if (!matches!(c.kind, ZERO | IGNORE) && c.len > 64 << 20) || c.stored > 128 << 20 {
                return Err(damaged("a chunk is too large"));
            }
        }
        // Writable when every chunk can be read here (to write it again).
        let known = chunks.iter().all(|c| matches!(c.kind, ZERO | IGNORE | RAW | ADC | ZLIB | BZIP2 | LZFSE | LZMA));
        let writable = !dev.is_read_only() && known;
        Ok(Self { dev, size, chunks, cache: Vec::new(), pages: Default::default(), writable })
    }

    fn unpack(&mut self, c: Chunk) -> Result<&[u8]> {
        if let Some(i) = self.cache.iter().position(|(s, _)| *s == c.start) {
            return Ok(&self.cache[i].1);
        }
        let packed = read(self.dev.as_mut(), c.at, c.stored as usize)?;
        let n = c.len as usize;
        let bad = || damaged(format!("a chunk at {} does not decompress", c.start));
        let mut out = match c.kind {
            ZLIB => miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&packed, n).map_err(|_| bad())?,
            ADC => adc(&packed, n).ok_or_else(bad)?,
            BZIP2 => bunzip2(&packed, n)?,
            LZFSE => unlzfse(&packed, n)?,
            LZMA => unxz(&packed, n)?,
            k => return Err(unsupported(format!("disk image chunks of type {k:#010x}"))),
        };
        // A short chunk ends in zeros.
        out.resize(n, 0);
        if self.cache.len() >= 4 {
            self.cache.remove(0);
        }
        self.cache.push((c.start, out));
        Ok(&self.cache.last().unwrap().1)
    }

    fn methods(&self) -> String {
        let mut names: Vec<&str> = Vec::new();
        for c in &self.chunks {
            let n = match c.kind {
                ZERO | IGNORE => continue,
                RAW => "raw",
                ADC => "ADC",
                ZLIB => "zlib",
                BZIP2 => "bzip2",
                LZFSE => "LZFSE",
                LZMA => "LZMA",
                _ => "other",
            };
            if !names.contains(&n) {
                names.push(n);
            }
        }
        names.join(", ")
    }
}

impl Dmg {
    /// Reads from the image as it is in the file (not the pending pages).
    fn read_stored(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset + done as u64;
            let i = self.chunks.partition_point(|c| c.start + c.len <= at);
            let c = self.chunks.get(i).copied();
            match c {
                Some(c) if c.start <= at => {
                    let inner = (at - c.start) as usize;
                    let n = (c.len as usize - inner).min(buf.len() - done);
                    let part = &mut buf[done..done + n];
                    match c.kind {
                        ZERO | IGNORE => part.fill(0),
                        RAW => {
                            // Raw chunks may be stored shorter than they cover.
                            let have = (c.stored as usize).saturating_sub(inner).min(n);
                            self.dev.read_at(c.at + inner as u64, &mut part[..have])?;
                            part[have..].fill(0);
                        }
                        _ => part.copy_from_slice(&self.unpack(c)?[inner..inner + n]),
                    }
                    done += n;
                }
                // Between (or after) the tables: zeros.
                next => {
                    let gap_end = next.map_or(self.size, |c| c.start);
                    let n = super::upto(gap_end - at, buf.len() - done).max(1);
                    buf[done..done + n].fill(0);
                    done += n;
                }
            }
        }
        Ok(())
    }

    fn page_len(&self, page: u64) -> usize {
        (self.size - page * PAGE).min(PAGE) as usize
    }

    /// Writes the image again with the pending pages (see the module docs).
    fn rewrite(&mut self) -> Result<()> {
        let pages = self.size.div_ceil(PAGE);
        let mut out: Vec<u8> = Vec::new();
        let mut table: Vec<(u32, u64, u64, u64, u64)> = Vec::new(); // kind, sector, sectors, at, len
        let mut crc = 0u32;
        let mut data = vec![0u8; PAGE as usize];
        for p in 0..pages {
            let len = self.page_len(p);
            let (start, sectors) = (p * PAGE, len as u64 / 512);
            // An unchanged chunk covering exactly this page: as it was.
            let same = if self.pages.contains_key(&p) {
                None
            } else {
                let i = self.chunks.partition_point(|c| c.start < start);
                self.chunks.get(i).copied().filter(|c| c.start == start && c.len == len as u64)
            };
            let data = &mut data[..len];
            match self.pages.get(&p) {
                Some(d) => data.copy_from_slice(&d[..len]),
                None => self.read_stored(start, data)?,
            }
            if data.iter().all(|&b| b == 0) {
                // Zero runs merged, up to 32 MiB a chunk.
                match table.last_mut() {
                    Some(t) if t.0 == IGNORE && t.2 + sectors <= ZERO_RUN => t.2 += sectors,
                    _ => table.push((IGNORE, start / 512, sectors, out.len() as u64, 0)),
                }
                continue;
            }
            crc = dix_core::partition::crc32_continue(crc, data);
            let (kind, bytes) = match same.filter(|c| !matches!(c.kind, ZERO | IGNORE)) {
                Some(c) => (c.kind, read(self.dev.as_mut(), c.at, c.stored as usize)?),
                None => {
                    let z = miniz_oxide::deflate::compress_to_vec_zlib(data, 6);
                    if z.len() < len { (ZLIB, z) } else { (RAW, data.to_vec()) }
                }
            };
            table.push((kind, start / 512, sectors, out.len() as u64, bytes.len() as u64));
            out.extend_from_slice(&bytes);
        }
        let image = udif(out, &table, self.size, crc);
        if (self.dev.size()) < image.len() as u64 {
            self.dev.resize(image.len() as u64)?;
        }
        self.dev.write_at(0, &image)?;
        if self.dev.size() > image.len() as u64 {
            self.dev.resize(image.len() as u64)?;
        }
        self.dev.flush()?;
        // What the file now holds.
        self.chunks = table
            .iter()
            .map(|&(kind, sector, sectors, at, len)| Chunk {
                start: sector * 512,
                len: sectors * 512,
                kind,
                at,
                stored: len,
            })
            .collect();
        self.cache.clear();
        self.pages.clear();
        Ok(())
    }
}

/// A whole UDIF image: `data` (the data fork) with its block table, a
/// property list and the trailer. `table`: (kind, first sector, sectors,
/// offset in `data`, length) per chunk; `crc`: of the disk's data outside
/// the zero chunks.
fn udif(mut data: Vec<u8>, table: &[(u32, u64, u64, u64, u64)], size: u64, crc: u32) -> Vec<u8> {
    let sectors = size / 512;
    let mut m = vec![0u8; 204];
    m[..4].copy_from_slice(b"mish");
    m[4..8].copy_from_slice(&1u32.to_be_bytes());
    m[16..24].copy_from_slice(&sectors.to_be_bytes());
    m[32..36].copy_from_slice(&0x208u32.to_be_bytes());
    m[64..68].copy_from_slice(&2u32.to_be_bytes());
    m[68..72].copy_from_slice(&32u32.to_be_bytes());
    m[72..76].copy_from_slice(&crc.to_be_bytes());
    m[200..204].copy_from_slice(&(table.len() as u32 + 1).to_be_bytes());
    for &(kind, sector, count, at, len) in table.iter().chain([&(END, sectors, 0, data.len() as u64, 0)]) {
        for v in [kind.to_be_bytes().to_vec(), vec![0; 4]] {
            m.extend_from_slice(&v);
        }
        for v in [sector, count, at, len] {
            m.extend_from_slice(&v.to_be_bytes());
        }
    }
    let data_len = data.len() as u64;
    let data_crc = dix_core::partition::crc32(&data);
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\
         \t<key>resource-fork</key>\n\t<dict>\n\t\t<key>blkx</key>\n\t\t<array>\n\t\t\t<dict>\n\
         \t\t\t\t<key>Attributes</key>\n\t\t\t\t<string>0x0050</string>\n\
         \t\t\t\t<key>CFName</key>\n\t\t\t\t<string>whole disk (Apple_HFS : 0)</string>\n\
         \t\t\t\t<key>Data</key>\n\t\t\t\t<data>\n{}\t\t\t\t</data>\n\
         \t\t\t\t<key>ID</key>\n\t\t\t\t<string>0</string>\n\
         \t\t\t\t<key>Name</key>\n\t\t\t\t<string>whole disk (Apple_HFS : 0)</string>\n\
         \t\t\t</dict>\n\t\t</array>\n\t</dict>\n</dict>\n</plist>\n",
        to_base64(&m)
    );
    let xml_at = data.len() as u64;
    data.extend_from_slice(plist.as_bytes());
    let mut t = vec![0u8; 512];
    t[..4].copy_from_slice(b"koly");
    t[4..8].copy_from_slice(&4u32.to_be_bytes());
    t[8..12].copy_from_slice(&512u32.to_be_bytes());
    t[12..16].copy_from_slice(&1u32.to_be_bytes());
    t[32..40].copy_from_slice(&data_len.to_be_bytes());
    t[56..60].copy_from_slice(&1u32.to_be_bytes());
    t[60..64].copy_from_slice(&1u32.to_be_bytes());
    t[64..80].copy_from_slice(&dix_core::partition::edit::random_guid());
    t[80..84].copy_from_slice(&2u32.to_be_bytes());
    t[84..88].copy_from_slice(&32u32.to_be_bytes());
    t[88..92].copy_from_slice(&data_crc.to_be_bytes());
    t[216..224].copy_from_slice(&xml_at.to_be_bytes());
    t[224..232].copy_from_slice(&(plist.len() as u64).to_be_bytes());
    t[352..356].copy_from_slice(&2u32.to_be_bytes());
    t[356..360].copy_from_slice(&32u32.to_be_bytes());
    t[360..364].copy_from_slice(&dix_core::partition::crc32(&crc.to_be_bytes()).to_be_bytes());
    t[488..492].copy_from_slice(&1u32.to_be_bytes());
    t[492..500].copy_from_slice(&sectors.to_be_bytes());
    data.extend_from_slice(&t);
    data
}

/// Base64 in lines of 52 characters, as property lists have it.
fn to_base64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len() * 4 / 3 + data.len() / 39 * 6 + 8);
    for (i, c) in data.chunks(3).enumerate() {
        if i % 13 == 0 {
            if i > 0 {
                s.push('\n');
            }
            s.push_str("\t\t\t\t");
        }
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            s.push(if k <= c.len() { A[(n >> (18 - 6 * k) & 63) as usize] as char } else { '=' });
        }
    }
    s.push('\n');
    s
}

impl Layout for Dmg {
    fn size(&self) -> u64 {
        self.size
    }
    fn block(&self) -> u64 {
        1 << 62
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if self.pages.is_empty() {
            return self.read_stored(offset, buf);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset + done as u64;
            let (p, inner) = (at / PAGE, (at % PAGE) as usize);
            let n = (PAGE as usize - inner).min(buf.len() - done);
            match self.pages.get(&p) {
                Some(d) => buf[done..done + n].copy_from_slice(&d[inner..inner + n]),
                None => self.read_stored(at, &mut buf[done..done + n])?,
            }
            done += n;
        }
        Ok(())
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let mut done = 0usize;
        while done < data.len() {
            let at = offset + done as u64;
            let (p, inner) = (at / PAGE, (at % PAGE) as usize);
            let n = (PAGE as usize - inner).min(data.len() - done);
            if !self.pages.contains_key(&p) {
                let mut page = vec![0u8; PAGE as usize];
                let len = self.page_len(p);
                self.read_stored(p * PAGE, &mut page[..len])?;
                self.pages.insert(p, page);
            }
            self.pages.get_mut(&p).unwrap()[inner..inner + n].copy_from_slice(&data[done..done + n]);
            done += n;
        }
        if self.pages.len() > MAX_PENDING {
            self.rewrite()?;
        }
        Ok(())
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn flush(&mut self) -> Result<()> {
        if !self.pages.is_empty() {
            self.rewrite()?;
        }
        Ok(())
    }

    fn file(&self) -> DeviceInfo {
        self.dev.info()
    }
    fn describe(&self) -> String {
        match self.methods().as_str() {
            "" | "raw" => "Apple disk image (UDIF), uncompressed".to_string(),
            m => format!("Apple disk image (UDIF), compressed: {m}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The block tables.

/// The `Data` of every `blkx` entry of the property list.
fn blkx_from_plist(xml: &str) -> Result<Vec<Vec<u8>>> {
    let start = xml.find("<key>blkx</key>").ok_or_else(|| damaged("the property list has no blkx entry"))?;
    let rest = &xml[start..];
    let end = rest.find("</array>").ok_or_else(|| damaged("the property list is cut short"))?;
    let mut out = Vec::new();
    let mut s = &rest[..end];
    while let Some(k) = s.find("<key>Data</key>") {
        s = &s[k..];
        let a = s.find("<data>").ok_or_else(|| damaged("a blkx entry without data"))? + 6;
        let b = s[a..].find("</data>").ok_or_else(|| damaged("a blkx entry is cut short"))? + a;
        out.push(base64(&s[a..b]).ok_or_else(|| damaged("a blkx entry is not base64"))?);
        s = &s[b..];
    }
    Ok(out)
}

/// The `blkx` resources of a classic resource fork.
fn blkx_from_resource_fork(r: &[u8]) -> Result<Vec<Vec<u8>>> {
    let bad = || damaged("the resource fork is damaged");
    let get16 = |at: usize| r.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize).ok_or_else(bad);
    let get32 = |at: usize| r.get(at..at + 4).map(|b| be32(b, 0) as usize).ok_or_else(bad);
    let (data, map) = (get32(0)?, get32(4)?);
    let types = map + get16(map + 24)?;
    let ntypes = get16(types)?.wrapping_add(1) & 0xFFFF;
    let mut out = Vec::new();
    for t in 0..ntypes {
        let e = types + 2 + t * 8;
        if r.get(e..e + 4) != Some(b"blkx") {
            continue;
        }
        let count = get16(e + 4)? + 1;
        let refs = types + get16(e + 6)?;
        for i in 0..count {
            let f = refs + i * 12;
            let off = get32(f + 4)? & 0x00FF_FFFF;
            let at = data + off;
            let len = get32(at)?;
            out.push(r.get(at + 4..at + 4 + len).ok_or_else(bad)?.to_vec());
        }
    }
    if out.is_empty() {
        return Err(damaged("the resource fork has no blkx resources"));
    }
    Ok(out)
}

fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Decoders.

/// Apple Data Compression: literal runs (`1nnnnnnn`, n + 1 bytes) and
/// copies from 1–1024 bytes back (`0nnnnndd dddddddd`, n + 3 bytes) or
/// 1–65536 bytes back (`01nnnnnn d16`, n + 4 bytes).
fn adc(src: &[u8], limit: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(limit);
    let mut i = 0;
    while i < src.len() && out.len() < limit {
        let b = src[i];
        if b & 0x80 != 0 {
            let n = (b & 0x7F) as usize + 1;
            out.extend_from_slice(src.get(i + 1..i + 1 + n)?);
            i += 1 + n;
            continue;
        }
        let (n, d) = if b & 0x40 != 0 {
            let d = u16::from_be_bytes([*src.get(i + 1)?, *src.get(i + 2)?]) as usize;
            i += 3;
            ((b & 0x3F) as usize + 4, d)
        } else {
            let d = ((b & 0x03) as usize) << 8 | *src.get(i + 1)? as usize;
            i += 2;
            (((b & 0x3C) >> 2) as usize + 3, d)
        };
        let from = out.len().checked_sub(d + 1)?;
        for k in 0..n {
            let x = out[from + k];
            out.push(x);
        }
    }
    out.truncate(limit);
    Some(out)
}

#[cfg(feature = "archive-bzip2")]
fn bunzip2(src: &[u8], limit: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::with_capacity(limit);
    bzip2::read::BzDecoder::new(src)
        .take(limit as u64)
        .read_to_end(&mut out)
        .map_err(|_| damaged("a bzip2 chunk does not decompress"))?;
    Ok(out)
}

#[cfg(not(feature = "archive-bzip2"))]
fn bunzip2(_: &[u8], _: usize) -> Result<Vec<u8>> {
    Err(unsupported("bzip2-compressed disk images in this build (no archive-bzip2)"))
}

#[cfg(any(feature = "hfsplus", feature = "apfs"))]
fn unlzfse(src: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(limit);
    crate::decmpfs::lzfse::decode(src, &mut out, limit)?;
    Ok(out)
}

#[cfg(not(any(feature = "hfsplus", feature = "apfs")))]
fn unlzfse(_: &[u8], _: usize) -> Result<Vec<u8>> {
    Err(unsupported("LZFSE-compressed disk images in this build (no hfsplus or apfs)"))
}

#[cfg(feature = "archive-xz")]
fn unxz(src: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(limit);
    lzma_rs::xz_decompress(&mut std::io::BufReader::new(src), &mut out)
        .map_err(|_| damaged("an LZMA chunk does not decompress"))?;
    out.truncate(limit);
    Ok(out)
}

#[cfg(not(feature = "archive-xz"))]
fn unxz(_: &[u8], _: usize) -> Result<Vec<u8>> {
    Err(unsupported("LZMA-compressed disk images in this build (no archive-xz)"))
}

#[cfg(test)]
pub(crate) mod encode {
    //! An ADC encoder, for the tests (literal runs and short copies).
    pub fn adc(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut lit: Vec<u8> = Vec::new();
        let flush = |lit: &mut Vec<u8>, out: &mut Vec<u8>| {
            for run in lit.chunks(128) {
                out.push(0x80 | (run.len() - 1) as u8);
                out.extend_from_slice(run);
            }
            lit.clear();
        };
        let mut i = 0;
        while i < data.len() {
            // A copy from the last 1024 bytes (two-byte code), or from up to
            // 65536 back (three-byte code), when 3+ bytes repeat.
            let mut best = (0usize, 0usize);
            for d in [1usize, 2, 4, 8, 16, 61, 300, 1000, 1024, 4000, 65536] {
                if d > i {
                    break;
                }
                let mut n = 0;
                let max = if d <= 1024 { 18 } else { 67 };
                while n < max && i + n < data.len() && data[i + n] == data[i + n - d] {
                    n += 1;
                }
                if n > best.0 {
                    best = (n, d);
                }
            }
            let (n, d) = best;
            if n >= 3 && (d <= 1024 || n >= 4) {
                flush(&mut lit, &mut out);
                if d <= 1024 && n <= 18 {
                    out.push((((n - 3) << 2) | ((d - 1) >> 8)) as u8);
                    out.push((d - 1) as u8);
                } else {
                    let n = n.max(4);
                    out.push(0x40 | (n - 4) as u8);
                    out.extend_from_slice(&((d - 1) as u16).to_be_bytes());
                }
                i += n;
            } else {
                lit.push(data[i]);
                i += 1;
            }
        }
        flush(&mut lit, &mut out);
        out
    }
}
