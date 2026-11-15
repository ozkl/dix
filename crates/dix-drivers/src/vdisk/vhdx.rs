//! VHDX, Hyper-V's disk format.
//!
//! The file starts with `vhdxfile`, two copies of the header (64 and
//! 128 KiB, the valid one with the higher sequence number wins) and two
//! copies of the region table (192 and 256 KiB), which locates the block
//! allocation table (BAT) and the metadata region. Everything is
//! little-endian and checked with CRC-32C.
//!
//! The metadata gives the disk size, the block size (1–256 MiB), the
//! logical sector size and, for a differencing disk, the parent's paths.
//! Each BAT entry holds a block's state and its offset in MiB: present,
//! absent (zeros, or the parent's data), or — in a differencing disk —
//! partly present, as a sector bitmap says. After every `chunk ratio`
//! payload entries comes the entry of the bitmap block covering them.
//!
//! Metadata changes go through a log first. A disk that was not closed
//! cleanly (Hyper-V crashed, the host lost power) still has entries in its
//! log; they are replayed here in memory, over what is read from the file,
//! without changing it.

use super::{Cache, Disk, Layout, damaged, le16, le32, le64, read_parent, size_label, unsupported};
use dix_core::Result;
use dix_core::container::{DiskFormat, OpenContext};
use dix_core::device::{BlockDevice, DeviceInfo};

const MIB: u64 = 1 << 20;

const BAT: &str = "2DC27766-F623-4200-9D64-115E9BFD4A08";
const METADATA: &str = "8B7CA206-4790-4B9A-B8FE-575F050F886E";
const FILE_PARAMETERS: &str = "CAA16737-FA36-4D43-B3B6-33F0AA44E76B";
const DISK_SIZE: &str = "2FA54224-CD1B-4876-B211-5DBED83BF4B8";
const LOGICAL_SECTOR: &str = "8141BF1D-A96F-4709-BA47-F233A8FAAB5F";
const PARENT_LOCATOR: &str = "A8D35F2D-B30B-454D-ABF7-D3D84834AB0C";

/// A GUID as it is stored: the first three fields little-endian.
fn guid(s: &str) -> [u8; 16] {
    let hex: Vec<u8> = s
        .split('-')
        .flat_map(|p| (0..p.len()).step_by(2).map(move |i| u8::from_str_radix(&p[i..i + 2], 16).unwrap()))
        .collect();
    let mut g = [0u8; 16];
    g[..4].copy_from_slice(&[hex[3], hex[2], hex[1], hex[0]]);
    g[4..6].copy_from_slice(&[hex[5], hex[4]]);
    g[6..8].copy_from_slice(&[hex[7], hex[6]]);
    g[8..].copy_from_slice(&hex[8..16]);
    g
}

const fn crc32c_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0x82F6_3B78 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC32C: [u32; 256] = crc32c_table();

/// CRC-32C of `data` with its 4-byte checksum field at `at` taken as zero.
pub(crate) fn checksum(data: &[u8], at: usize) -> u32 {
    let mut c = !0u32;
    for (i, &b) in data.iter().enumerate() {
        let b = if (at..at + 4).contains(&i) { 0 } else { b };
        c = CRC32C[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

fn valid(data: &[u8], sig: &[u8]) -> bool {
    data.starts_with(sig) && le32(data, 4) == checksum(data, 4)
}

pub struct VhdxFormat;

impl DiskFormat for VhdxFormat {
    fn name(&self) -> &'static str {
        "VHDX"
    }
    fn probe(&self, dev: &mut dyn BlockDevice) -> bool {
        super::read_some(dev, 0, 8).is_some_and(|h| h == b"vhdxfile")
    }
    fn open(&self, dev: Box<dyn BlockDevice>, ctx: &OpenContext<'_>) -> Result<Box<dyn BlockDevice>> {
        Ok(Box::new(Disk(Vhdx::open(dev, ctx)?)))
    }

    fn creates(&self) -> Option<(&'static str, &'static str)> {
        Some(("vhdx", "Hyper-V (VHDX, dynamic): grows as it is filled"))
    }

    /// A dynamic VHDX laid out as qemu-img and Hyper-V do: the file
    /// identifier, two headers, two region tables, an empty 1 MiB log, the
    /// metadata (2 MiB blocks, 512-byte logical and 4 KiB physical
    /// sectors) and an empty block table, each region 1 MiB aligned.
    fn create(&self, file: &mut dyn BlockDevice, size: u64, _name: &str) -> Result<()> {
        super::check_new_size(size)?;
        const BLOCK: u64 = 2 * MIB;
        let size = size.div_ceil(512) * 512;
        let blocks = size.div_ceil(BLOCK);
        let chunk_ratio = (1u64 << 23) * 512 / BLOCK;
        let entries = blocks + (blocks - 1) / chunk_ratio;
        let bat_len = (entries * 8).div_ceil(MIB) * MIB;
        let (log_at, meta_at, bat_at) = (MIB, 2 * MIB, 3 * MIB);
        let sum = |b: &mut [u8]| {
            let c = checksum(b, 4);
            b[4..8].copy_from_slice(&c.to_le_bytes());
        };

        let mut id = vec![0u8; 64 * 1024];
        id[..8].copy_from_slice(b"vhdxfile");
        for (k, u) in "Disk Image Explorer".encode_utf16().enumerate() {
            id[8 + k * 2..10 + k * 2].copy_from_slice(&u.to_le_bytes());
        }
        let mut head = vec![0u8; 4096];
        head[..4].copy_from_slice(b"head");
        head[16..32].copy_from_slice(&dix_core::partition::edit::random_guid());
        head[32..48].copy_from_slice(&dix_core::partition::edit::random_guid());
        head[66..68].copy_from_slice(&1u16.to_le_bytes());
        head[68..72].copy_from_slice(&(MIB as u32).to_le_bytes());
        head[72..80].copy_from_slice(&log_at.to_le_bytes());
        let (mut h1, mut h2) = (head.clone(), head);
        h1[8..16].copy_from_slice(&1u64.to_le_bytes());
        h2[8..16].copy_from_slice(&2u64.to_le_bytes());
        sum(&mut h1);
        sum(&mut h2);

        let mut regions = vec![0u8; 64 * 1024];
        regions[..4].copy_from_slice(b"regi");
        regions[8..12].copy_from_slice(&2u32.to_le_bytes());
        for (k, (g, at, len)) in [(BAT, bat_at, bat_len), (METADATA, meta_at, MIB)].into_iter().enumerate() {
            let e = &mut regions[16 + k * 32..48 + k * 32];
            e[..16].copy_from_slice(&guid(g));
            e[16..24].copy_from_slice(&at.to_le_bytes());
            e[24..28].copy_from_slice(&(len as u32).to_le_bytes());
            e[28..32].copy_from_slice(&1u32.to_le_bytes());
        }
        sum(&mut regions);

        // Metadata: the table, then the items from 64 KiB.
        let mut meta = vec![0u8; MIB as usize];
        meta[..8].copy_from_slice(b"metadata");
        let page83 = dix_core::partition::edit::random_guid();
        let items: [(&str, Vec<u8>, u32); 5] = [
            (FILE_PARAMETERS, [(BLOCK as u32).to_le_bytes(), 0u32.to_le_bytes()].concat(), 4),
            (DISK_SIZE, size.to_le_bytes().to_vec(), 6),
            ("BECA12AB-B2E6-4523-93EF-C309E000C746", page83.to_vec(), 6),
            (LOGICAL_SECTOR, 512u32.to_le_bytes().to_vec(), 6),
            ("CDA348C7-445D-4471-9CC9-E9885251C556", 4096u32.to_le_bytes().to_vec(), 6),
        ];
        meta[10..12].copy_from_slice(&(items.len() as u16).to_le_bytes());
        let mut at = 64 * 1024usize;
        for (k, (g, data, flags)) in items.iter().enumerate() {
            let e = &mut meta[32 + k * 32..64 + k * 32];
            e[..16].copy_from_slice(&guid(g));
            e[16..20].copy_from_slice(&(at as u32).to_le_bytes());
            e[20..24].copy_from_slice(&(data.len() as u32).to_le_bytes());
            e[24..28].copy_from_slice(&flags.to_le_bytes());
            meta[at..at + data.len()].copy_from_slice(data);
            at += data.len().div_ceil(8) * 8;
        }

        super::put(file, 0, &id)?;
        super::put(file, 64 * 1024, &h1)?;
        super::put(file, 128 * 1024, &h2)?;
        super::put(file, 192 * 1024, &regions)?;
        super::put(file, 256 * 1024, &regions)?;
        super::put(file, log_at, &vec![0u8; MIB as usize])?;
        super::put(file, meta_at, &meta)?;
        super::put(file, bat_at, &vec![0u8; bat_len as usize])
    }
}

/// The file with the log's changes laid over it.
struct File {
    dev: Box<dyn BlockDevice>,
    /// (offset, bytes), applied in order.
    patches: Vec<(u64, Vec<u8>)>,
}

impl File {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let size = self.dev.size();
        // Replayed changes may lie past the end of the file.
        let n = if offset >= size { 0 } else { super::upto(size - offset, buf.len()) };
        self.dev.read_at(offset, &mut buf[..n])?;
        buf[n..].fill(0);
        let end = offset + buf.len() as u64;
        for (at, bytes) in &self.patches {
            let (a, b) = (*at.max(&offset), (at + bytes.len() as u64).min(end));
            if a < b {
                buf[(a - offset) as usize..(b - offset) as usize]
                    .copy_from_slice(&bytes[(a - at) as usize..(b - at) as usize]);
            }
        }
        Ok(())
    }

    fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut v = vec![0u8; len];
        self.read_at(offset, &mut v)?;
        Ok(v)
    }
}

struct Vhdx {
    file: File,
    size: u64,
    block: u64,
    sector: u32,
    chunk_ratio: u64,
    bat: Vec<u64>,
    bitmaps: Cache<Vec<u8>>,
    parent: Option<(Box<dyn BlockDevice>, String)>,
    replayed: bool,
    /// The current header and where it is (for the first write's update).
    header: Vec<u8>,
    header_at: u64,
    bat_at: u64,
    writable: bool,
    /// The headers were updated for this session's writes.
    marked: bool,
}

impl Vhdx {
    fn open(dev: Box<dyn BlockDevice>, ctx: &OpenContext<'_>) -> Result<Self> {
        let mut file = File { dev, patches: Vec::new() };
        // The current header.
        let mut found: Option<(u64, Vec<u8>)> = None;
        for at in [64 * 1024, 128 * 1024] {
            let h = file.read(at, 4096)?;
            if valid(&h, b"head") && found.as_ref().is_none_or(|(_, b)| le64(&h, 8) > le64(b, 8)) {
                found = Some((at, h));
            }
        }
        let (header_at, header) = found.ok_or_else(|| damaged("no valid header"))?;
        if le16(&header, 66) != 1 {
            return Err(unsupported(format!("VHDX version {}", le16(&header, 66))));
        }
        let log_guid = &header[48..64];
        let replayed = if log_guid.iter().any(|&b| b != 0) {
            replay(&mut file, log_guid, le64(&header, 72), le32(&header, 68) as u64)?
        } else {
            false
        };

        let mut regions = None;
        for at in [192 * 1024, 256 * 1024] {
            let r = file.read(at, 64 * 1024)?;
            if valid(&r, b"regi") {
                regions = Some(r);
                break;
            }
        }
        let r = regions.ok_or_else(|| damaged("no valid region table"))?;
        let (mut bat_at, mut bat_len, mut meta_at) = (None, 0u64, None);
        for i in 0..(le32(&r, 8) as usize).min(2047) {
            let e = &r[16 + i * 32..48 + i * 32];
            let id: [u8; 16] = e[..16].try_into().unwrap();
            if id == guid(BAT) {
                bat_at = Some(le64(e, 16));
                bat_len = le32(e, 24) as u64;
            } else if id == guid(METADATA) {
                meta_at = Some(le64(e, 16));
            } else if le32(e, 28) & 1 != 0 {
                return Err(unsupported("a VHDX with an unknown required region"));
            }
        }
        let (bat_at, meta_at) = match (bat_at, meta_at) {
            (Some(b), Some(m)) => (b, m),
            _ => return Err(damaged("the BAT or the metadata region is missing")),
        };

        // Metadata items.
        let m = file.read(meta_at, 64 * 1024)?;
        if &m[..8] != b"metadata" {
            return Err(damaged("the metadata table is missing"));
        }
        let mut items = Vec::new();
        for i in 0..(le16(&m, 10) as usize).min(2047) {
            let e = &m[32 + i * 32..64 + i * 32];
            let id: [u8; 16] = e[..16].try_into().unwrap();
            items.push((id, meta_at + le32(e, 16) as u64, le32(e, 20) as usize, le32(e, 24)));
        }
        let item = |file: &mut File, id: &str| -> Result<Option<Vec<u8>>> {
            match items.iter().find(|i| i.0 == guid(id)) {
                Some(&(_, at, len, _)) => Ok(Some(file.read(at, len.min(1 << 20))?)),
                None => Ok(None),
            }
        };
        let params = item(&mut file, FILE_PARAMETERS)?.ok_or_else(|| damaged("the file parameters are missing"))?;
        let block = le32(&params, 0) as u64;
        let has_parent = le32(&params, 4) & 2 != 0;
        let size =
            item(&mut file, DISK_SIZE)?.map(|s| le64(&s, 0)).ok_or_else(|| damaged("the disk size is missing"))?;
        let sector = item(&mut file, LOGICAL_SECTOR)?.map_or(512, |s| le32(&s, 0));
        let locator = item(&mut file, PARENT_LOCATOR)?;
        for (id, .., flags) in &items {
            let known = [FILE_PARAMETERS, DISK_SIZE, LOGICAL_SECTOR, PARENT_LOCATOR].iter().any(|g| guid(g) == *id);
            if !known
                && flags & 4 != 0
                && *id != guid("BECA12AB-B2E6-4523-93EF-C309E000C746")
                && *id != guid("CDA348C7-445D-4471-9CC9-E9885251C556")
            {
                return Err(unsupported("a VHDX with unknown required metadata"));
            }
        }
        if !(MIB..=256 * MIB).contains(&block) || !block.is_power_of_two() {
            return Err(damaged(format!("block size {block}")));
        }
        if sector != 512 && sector != 4096 {
            return Err(damaged(format!("logical sector size {sector}")));
        }
        let chunk_ratio = (1u64 << 23) * sector as u64 / block;
        let blocks = size.div_ceil(block);
        let entries = if has_parent {
            blocks.div_ceil(chunk_ratio) * (chunk_ratio + 1)
        } else {
            blocks + (blocks - 1) / chunk_ratio
        };
        if entries * 8 > bat_len.max(entries * 8) || entries > 1 << 28 {
            return Err(damaged("the BAT is too small for the disk"));
        }
        let raw = file.read(bat_at, (entries * 8) as usize)?;
        let bat = (0..entries as usize).map(|i| le64(&raw, i * 8)).collect();
        let parent = if has_parent {
            let loc = locator.ok_or_else(|| damaged("a differencing disk without a parent locator"))?;
            Some(open_parent(&loc, ctx)?)
        } else {
            None
        };
        let writable = !file.dev.is_read_only() && !replayed && parent.is_none();
        Ok(Self {
            file,
            size,
            block,
            sector,
            chunk_ratio,
            bat,
            bitmaps: Cache::new(4),
            parent,
            replayed,
            header,
            header_at,
            bat_at,
            writable,
            marked: false,
        })
    }
}

/// The parent's paths from the locator, tried in order.
fn open_parent(loc: &[u8], ctx: &OpenContext<'_>) -> Result<(Box<dyn BlockDevice>, String)> {
    let count = le16(loc, 18) as usize;
    let mut paths = Vec::new();
    for i in 0..count.min(256) {
        let e = 20 + i * 12;
        if e + 12 > loc.len() {
            break;
        }
        let text = |at: usize, len: usize| {
            let b = loc.get(at..at + len).unwrap_or(&[]);
            String::from_utf16_lossy(&b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>())
        };
        let key = text(le32(loc, e) as usize, le16(loc, e + 8) as usize);
        let value = text(le32(loc, e + 4) as usize, le16(loc, e + 10) as usize);
        let rank = match key.as_str() {
            "relative_path" => 0,
            "absolute_win32_path" => 1,
            "volume_path" => 2,
            _ => continue,
        };
        paths.push((rank, value.replace('\\', "/")));
    }
    paths.sort();
    let mut last = None;
    for (_, p) in &paths {
        let short = p.rsplit('/').next().unwrap_or(p).to_string();
        for n in [p.trim_start_matches("./").to_string(), short] {
            match ctx.open_parent(&n) {
                Ok(d) => return Ok((d, n)),
                Err(e) => last = Some(e),
            }
        }
    }
    Err(last.unwrap_or_else(|| damaged("the parent locator has no path")))
}

/// Replays the log's active sequence into `file.patches`. Returns whether
/// there was anything to replay.
fn replay(file: &mut File, log_guid: &[u8], at: u64, len: u64) -> Result<bool> {
    if len == 0 || !len.is_multiple_of(4096) || len > 1 << 32 {
        return Err(damaged("the log is out of range"));
    }
    let log = file.read(at, len as usize)?;
    // A valid entry at offset `o` of the log: (sequence, length, tail).
    let entry = |o: u64| -> Option<(u64, u64, u64)> {
        let h = &log[o as usize..];
        if !h.starts_with(b"loge") || &h[32..48] != log_guid {
            return None;
        }
        let elen = le32(h, 8) as u64;
        if elen == 0 || !elen.is_multiple_of(4096) || elen > len {
            return None;
        }
        // An entry may wrap around the end of the log.
        let bytes: Vec<u8> = (0..elen).map(|i| log[((o + i) % len) as usize]).collect();
        (le32(&bytes, 4) == checksum(&bytes, 4)).then(|| (le64(h, 16), elen, le32(h, 12) as u64))
    };
    // The longest run of entries whose sequence numbers follow each other,
    // ending at the highest one.
    let mut best: Option<(u64, Vec<u64>)> = None;
    for start in (0..len).step_by(4096) {
        let Some((seq, mut elen, _)) = entry(start) else { continue };
        let mut run = vec![start];
        let (mut o, mut last) = (start, seq);
        loop {
            o = (o + elen) % len;
            match entry(o) {
                Some((s, l, _)) if s == last + 1 && !run.contains(&o) => {
                    run.push(o);
                    last = s;
                    elen = l;
                }
                _ => break,
            }
        }
        if best.as_ref().is_none_or(|(s, _)| last > *s) {
            best = Some((last, run));
        }
    }
    let Some((_, run)) = best else { return Ok(false) };
    // Replay from the head entry's tail.
    let head = *run.last().unwrap();
    let tail = entry(head).map_or(run[0], |e| e.2);
    let from = run.iter().position(|&o| o == tail).unwrap_or(0);
    for &o in &run[from..] {
        let (_, elen, _) = entry(o).unwrap();
        let bytes: Vec<u8> = (0..elen).map(|i| log[((o + i) % len) as usize]).collect();
        let count = le32(&bytes, 24) as usize;
        let mut data = (64 + 32 * count as u64).div_ceil(4096) * 4096;
        for d in 0..count {
            let desc = bytes.get(64 + d * 32..96 + d * 32).ok_or_else(|| damaged("a log entry is cut short"))?;
            let target = le64(desc, 16);
            if desc.starts_with(b"zero") {
                let n = le64(desc, 8).min(1 << 30) as usize;
                file.patches.push((target, vec![0u8; n]));
            } else if desc.starts_with(b"desc") {
                let s = bytes
                    .get(data as usize..data as usize + 4096)
                    .ok_or_else(|| damaged("a log entry is cut short"))?;
                if !s.starts_with(b"data") {
                    return Err(damaged("a log data sector is missing"));
                }
                let mut page = Vec::with_capacity(4096);
                page.extend_from_slice(&desc[8..16]);
                page.extend_from_slice(&s[8..4092]);
                page.extend_from_slice(&desc[4..8]);
                file.patches.push((target, page));
                data += 4096;
            } else {
                return Err(damaged("an unknown log descriptor"));
            }
        }
    }
    Ok(true)
}

impl Vhdx {
    /// Before the first write: new file and data write GUIDs, in the other
    /// header slot with the next sequence number (so it becomes current).
    fn mark(&mut self) -> Result<()> {
        let mut h = self.header.clone();
        let seq = le64(&h, 8) + 1;
        h[8..16].copy_from_slice(&seq.to_le_bytes());
        h[16..32].copy_from_slice(&dix_core::partition::edit::random_guid());
        h[32..48].copy_from_slice(&dix_core::partition::edit::random_guid());
        h[4..8].fill(0);
        let c = checksum(&h, 4);
        h[4..8].copy_from_slice(&c.to_le_bytes());
        let other = if self.header_at == 64 * 1024 { 128 * 1024 } else { 64 * 1024 };
        self.file.dev.write_at(other, &h)?;
        self.file.dev.flush()?;
        self.header = h;
        self.header_at = other;
        self.marked = true;
        Ok(())
    }
}

impl Layout for Vhdx {
    fn size(&self) -> u64 {
        self.size
    }
    fn block(&self) -> u64 {
        self.block
    }
    fn sector_size(&self) -> u32 {
        self.sector
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let i = offset / self.block;
        let inner = offset % self.block;
        let e = self.bat.get((i + i / self.chunk_ratio) as usize).copied().unwrap_or(0);
        let at = e & !(MIB - 1);
        match e & 7 {
            6 => self.file.read_at(at + inner, buf),
            7 => {
                // Partly present: the chunk's sector bitmap says where.
                let chunk = i / self.chunk_ratio;
                let be = self.bat[(chunk * (self.chunk_ratio + 1) + self.chunk_ratio) as usize];
                if be & 7 != 6 {
                    return Err(damaged("a partly present block without a sector bitmap"));
                }
                let bat_at = be & !(MIB - 1);
                let file = &mut self.file;
                let bitmap = self.bitmaps.get_or(bat_at, || file.read(bat_at, MIB as usize))?.clone();
                let sector = self.sector as u64;
                let chunk_start = chunk * self.chunk_ratio * self.block;
                let mut done = 0usize;
                while done < buf.len() {
                    let s = (offset + done as u64 - chunk_start) / sector;
                    let here = bitmap[(s / 8) as usize] & (1 << (s % 8)) != 0;
                    let pos = offset + done as u64;
                    let n = super::upto(sector - pos % sector, buf.len() - done);
                    let part = &mut buf[done..done + n];
                    if here {
                        self.file.read_at(at + (pos % self.block), part)?;
                    } else {
                        read_parent(self.parent.as_mut().map(|p| p.0.as_mut()), pos, part)?;
                    }
                    done += n;
                }
                Ok(())
            }
            // Zero / unmapped: zeros. Not present / undefined: the parent's
            // data (zeros without one).
            2 | 3 => {
                buf.fill(0);
                Ok(())
            }
            _ => read_parent(self.parent.as_mut().map(|p| p.0.as_mut()), offset, buf),
        }
    }

    /// A new block goes at the end of the file (1 MiB aligned), whole,
    /// and its entry in the block table says it is there. Metadata changes
    /// in VHDX are meant to go through the log first; written in place, a
    /// crash between the block and its entry only loses that block.
    fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if !self.marked {
            self.mark()?;
        }
        let i = offset / self.block;
        let inner = offset % self.block;
        let idx = (i + i / self.chunk_ratio) as usize;
        let e = *self.bat.get(idx).ok_or_else(|| damaged("a write past the block table"))?;
        if e & 7 == 6 {
            return self.file.dev.write_at((e & !(MIB - 1)) + inner, data);
        }
        if e & 7 == 7 {
            return Err(unsupported("writing to a partly present block"));
        }
        let at = self.file.dev.size().div_ceil(MIB) * MIB;
        let mut block = vec![0u8; self.block as usize];
        block[inner as usize..inner as usize + data.len()].copy_from_slice(data);
        super::put(self.file.dev.as_mut(), at, &block)?;
        let entry = at | 6;
        self.file.dev.write_at(self.bat_at + idx as u64 * 8, &entry.to_le_bytes())?;
        self.bat[idx] = entry;
        Ok(())
    }

    fn writable(&self) -> bool {
        self.writable
    }
    fn flush(&mut self) -> Result<()> {
        self.file.dev.flush()
    }

    fn file(&self) -> DeviceInfo {
        self.file.dev.info()
    }
    fn describe(&self) -> String {
        let mut s = format!(
            "VHDX, {}, {} blocks",
            if self.parent.is_some() { "differencing" } else { "dynamic or fixed" },
            size_label(self.block)
        );
        if self.sector != 512 {
            s.push_str(&format!(", {}-byte sectors", self.sector));
        }
        if self.replayed {
            s.push_str(", log replayed in memory");
        }
        if let Some((_, name)) = &self.parent {
            s.push_str(&format!(", parent {name}"));
        }
        s
    }
}
