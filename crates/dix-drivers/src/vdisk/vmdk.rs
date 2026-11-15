//! VMDK, VMware's virtual disk.
//!
//! A VMDK is described by a text *descriptor*: its `createType` and a list
//! of extents, each a number of sectors of one kind:
//!
//! * `FLAT` (and `VMFS`): a raw file, from a sector offset in it;
//! * `SPARSE`: a hosted sparse file (`KDMV` header): a grain directory
//!   points to grain tables, whose 32-bit entries give each grain's sector
//!   (64 KiB grains by default; 0: not allocated, 1: zeros). Stream-optimized
//!   disks compress each grain (zlib) behind a 12-byte marker, and keep the
//!   grain directory at the end (in a footer);
//! * `ZERO`: zeros.
//!
//! The descriptor is a file of its own (`twoGbMaxExtentSparse`,
//! `monolithicFlat`, ESXi's `x.vmdk` + `x-flat.vmdk`) or is embedded in a
//! sparse file that is also the only extent (`monolithicSparse`,
//! `streamOptimized`). A snapshot names its parent in
//! `parentFileNameHint`; grains it does not have are read from there.
//!
//! Flat extents and uncompressed sparse ones can be written: a new grain
//! is appended to its extent file and entered in both grain table copies.

use super::{Cache, Disk, Layout, damaged, le32, le64, read, read_parent, size_label, unsupported};
use dix_core::container::{DiskFormat, OpenContext};
use dix_core::device::{BlockDevice, DeviceInfo};
use dix_core::{Error, Result};

const KDMV: u32 = 0x564D_444B;
const GD_AT_END: u64 = u64::MAX;
const FLAG_RGD: u32 = 1 << 1;
const FLAG_ZEROED_GTE: u32 = 1 << 2;
const FLAG_COMPRESSED: u32 = 1 << 16;
const FLAG_MARKERS: u32 = 1 << 17;

pub struct VmdkFormat;

fn is_descriptor(b: &[u8]) -> bool {
    let text = String::from_utf8_lossy(b);
    let t = text.trim_start();
    t.starts_with("# Disk DescriptorFile") || (t.starts_with('#') && text.contains("createType="))
}

impl DiskFormat for VmdkFormat {
    fn name(&self) -> &'static str {
        "VMDK"
    }
    fn probe(&self, dev: &mut dyn BlockDevice) -> bool {
        match super::read_some(dev, 0, 512) {
            Some(h) if h.len() >= 4 && le32(&h, 0) == KDMV => true,
            // A descriptor file is small text.
            Some(h) => dev.size() <= 1 << 20 && is_descriptor(&h),
            None => false,
        }
    }
    fn open(&self, dev: Box<dyn BlockDevice>, ctx: &OpenContext<'_>) -> Result<Box<dyn BlockDevice>> {
        Ok(Box::new(Disk(Vmdk::open(dev, ctx)?)))
    }

    fn creates(&self) -> Option<(&'static str, &'static str)> {
        Some(("vmdk", "VMware (VMDK, monolithic sparse): grows as it is filled"))
    }

    /// A monolithic sparse VMDK laid out as qemu-img makes one: the header,
    /// the embedded descriptor, the redundant grain directory and its
    /// grain tables, the grain directory and its tables (all tables made
    /// up front, empty), then room for 64 KiB grains.
    fn create(&self, file: &mut dyn BlockDevice, size: u64, name: &str) -> Result<()> {
        super::check_new_size(size)?;
        const GRAIN: u64 = 128; // sectors
        const GTES: u64 = 512;
        let sectors = size.div_ceil(512);
        let gt_count = sectors.div_ceil(GRAIN).div_ceil(GTES);
        let gd_sectors = (gt_count * 4).div_ceil(512);
        let gt_sectors = GTES * 4 / 512;
        let (desc_at, desc_sectors) = (1u64, 20u64);
        let rgd = desc_at + desc_sectors;
        let gd = rgd + gd_sectors + gt_count * gt_sectors;
        let overhead = (gd + gd_sectors + gt_count * gt_sectors).div_ceil(GRAIN) * GRAIN;

        let mut h = vec![0u8; 512];
        h[..4].copy_from_slice(&KDMV.to_le_bytes());
        h[4..8].copy_from_slice(&1u32.to_le_bytes());
        h[8..12].copy_from_slice(&3u32.to_le_bytes()); // newline test, redundant tables
        h[12..20].copy_from_slice(&sectors.to_le_bytes());
        h[20..28].copy_from_slice(&GRAIN.to_le_bytes());
        h[28..36].copy_from_slice(&desc_at.to_le_bytes());
        h[36..44].copy_from_slice(&desc_sectors.to_le_bytes());
        h[44..48].copy_from_slice(&(GTES as u32).to_le_bytes());
        h[48..56].copy_from_slice(&rgd.to_le_bytes());
        h[56..64].copy_from_slice(&gd.to_le_bytes());
        h[64..72].copy_from_slice(&overhead.to_le_bytes());
        h[73..77].copy_from_slice(b"\n \r\n");

        let cylinders = (sectors / (16 * 63)).clamp(1, 65535);
        let cid = u32::from_le_bytes(dix_core::partition::edit::random_guid()[..4].try_into().unwrap());
        let file_name = name.rsplit(['/', '\\']).next().unwrap_or(name).replace('"', "");
        let desc = format!(
            "# Disk DescriptorFile\nversion=1\nCID={cid:08x}\nparentCID=ffffffff\ncreateType=\"monolithicSparse\"\n\n\
             # Extent description\nRW {sectors} SPARSE \"{file_name}\"\n\n# The Disk Data Base\n#DDB\n\n\
             ddb.virtualHWVersion = \"4\"\nddb.geometry.cylinders = \"{cylinders}\"\nddb.geometry.heads = \"16\"\n\
             ddb.geometry.sectors = \"63\"\nddb.adapterType = \"ide\"\n"
        );
        let mut d = desc.into_bytes();
        d.resize((desc_sectors * 512) as usize, 0);

        // A directory and its tables: entry i points to table i.
        let tables = |dir: u64| -> Vec<u8> {
            let mut b = vec![0u8; ((gd_sectors + gt_count * gt_sectors) * 512) as usize];
            for i in 0..gt_count {
                let t = (dir + gd_sectors + i * gt_sectors) as u32;
                b[(i * 4) as usize..(i * 4 + 4) as usize].copy_from_slice(&t.to_le_bytes());
            }
            b
        };
        super::put(file, 0, &h)?;
        super::put(file, desc_at * 512, &d)?;
        super::put(file, rgd * 512, &tables(rgd))?;
        super::put(file, gd * 512, &tables(gd))?;
        if file.size() < overhead * 512 {
            file.resize(overhead * 512)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The descriptor.

struct ExtentLine {
    access: String,
    sectors: u64,
    kind: String,
    file: Option<String>,
    offset: u64,
}

struct Descriptor {
    create_type: String,
    parent: Option<String>,
    extents: Vec<ExtentLine>,
}

fn parse_descriptor(text: &str) -> Result<Descriptor> {
    let mut d = Descriptor { create_type: String::new(), parent: None, extents: Vec::new() };
    let unquote = |v: &str| v.trim().trim_matches('"').to_string();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && !matches!(line.split_whitespace().next(), Some("RW" | "RDONLY" | "NOACCESS"))
        {
            match k.trim() {
                "createType" => d.create_type = unquote(v),
                "parentFileNameHint" => d.parent = Some(unquote(v)).filter(|p| !p.is_empty()),
                _ => {}
            }
            continue;
        }
        let mut words = line.splitn(3, char::is_whitespace);
        let (Some(access), Some(sectors), Some(rest)) = (words.next(), words.next(), words.next()) else { continue };
        if !matches!(access, "RW" | "RDONLY" | "NOACCESS") {
            continue;
        }
        let sectors: u64 = sectors.parse().map_err(|_| damaged(format!("an extent line: {line}")))?;
        let rest = rest.trim();
        let (kind, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let rest = rest.trim();
        let (file, tail) = if let Some(r) = rest.strip_prefix('"') {
            let end = r.find('"').ok_or_else(|| damaged(format!("an extent line: {line}")))?;
            (Some(r[..end].to_string()), r[end + 1..].trim())
        } else {
            (None, rest)
        };
        let offset = tail.split_whitespace().next().and_then(|o| o.parse().ok()).unwrap_or(0);
        d.extents.push(ExtentLine { access: access.into(), sectors, kind: kind.to_ascii_uppercase(), file, offset });
    }
    if d.extents.is_empty() {
        return Err(damaged("the descriptor lists no extents"));
    }
    Ok(d)
}

// ---------------------------------------------------------------------------
// A hosted sparse extent.

enum Grain {
    Absent,
    Zero,
    At(u64),
    Packed(u64),
}

struct Sparse {
    dev: Box<dyn BlockDevice>,
    capacity: u64,
    grain: u64,
    gtes: u64,
    gd: Vec<u32>,
    rgd: Option<Vec<u32>>,
    compressed: bool,
    markers: bool,
    zeroed: bool,
    tables: Cache<Vec<u32>>,
    unpacked: Option<(u64, Vec<u8>)>,
}

/// The 512-byte header of a sparse file, read from where it is current
/// (a stream-optimized disk keeps the real one in its footer).
fn sparse_header(dev: &mut dyn BlockDevice) -> Result<Vec<u8>> {
    let h = read(dev, 0, 512)?;
    if le32(&h, 0) != KDMV {
        return Err(damaged("not a sparse extent"));
    }
    if le64(&h, 56) == GD_AT_END {
        let size = dev.size();
        if size < 1536 {
            return Err(damaged("the footer is missing"));
        }
        let f = read(dev, size - 1024, 512)?;
        if le32(&f, 0) != KDMV {
            return Err(damaged("the footer is missing"));
        }
        return Ok(f);
    }
    Ok(h)
}

impl Sparse {
    fn open(mut dev: Box<dyn BlockDevice>) -> Result<Self> {
        let h = sparse_header(dev.as_mut())?;
        let version = le32(&h, 4);
        if !(1..=3).contains(&version) {
            return Err(unsupported(format!("sparse extent version {version}")));
        }
        let flags = le32(&h, 8);
        let capacity = le64(&h, 12);
        let grain_sectors = le64(&h, 20);
        let gtes = le32(&h, 44) as u64;
        let (rgd_at, gd_at) = (le64(&h, 48), le64(&h, 56));
        if !grain_sectors.is_power_of_two() || grain_sectors > 1 << 16 || gtes == 0 || gtes > 1 << 16 {
            return Err(damaged("grain size or grain table size out of range"));
        }
        let compressed = flags & FLAG_COMPRESSED != 0;
        if compressed && le16_at(&h, 77) > 1 {
            return Err(unsupported(format!("grain compression {}", le16_at(&h, 77))));
        }
        let grain = grain_sectors * 512;
        let gd_len = capacity.div_ceil(grain_sectors).div_ceil(gtes);
        if gd_len > 1 << 24 {
            return Err(damaged("the grain directory is too large"));
        }
        let load = |dev: &mut dyn BlockDevice, at: u64| -> Result<Vec<u32>> {
            let raw = read(dev, at * 512, (gd_len * 4) as usize).map_err(|e| e.with_context("the grain directory"))?;
            Ok((0..gd_len as usize).map(|i| le32(&raw, i * 4)).collect())
        };
        let gd = load(dev.as_mut(), gd_at)?;
        let rgd =
            if flags & FLAG_RGD != 0 && rgd_at != 0 && !compressed { Some(load(dev.as_mut(), rgd_at)?) } else { None };
        Ok(Self {
            dev,
            capacity: capacity * 512,
            grain,
            gtes,
            gd,
            rgd,
            compressed,
            markers: flags & FLAG_MARKERS != 0,
            zeroed: flags & FLAG_ZEROED_GTE != 0,
            tables: Cache::new(32),
            unpacked: None,
        })
    }

    fn table(&mut self, i: usize) -> Result<Option<&mut Vec<u32>>> {
        let at = self.gd.get(i).copied().unwrap_or(0) as u64;
        if at == 0 {
            return Ok(None);
        }
        let (dev, n) = (&mut self.dev, self.gtes as usize);
        let t = self.tables.get_or(at, || {
            let raw = read(dev.as_mut(), at * 512, n * 4).map_err(|e| e.with_context("a grain table"))?;
            Ok((0..n).map(|i| le32(&raw, i * 4)).collect())
        })?;
        Ok(Some(t))
    }

    fn lookup(&mut self, g: u64) -> Result<Grain> {
        let (compressed, zeroed, gtes) = (self.compressed, self.zeroed, self.gtes);
        let Some(t) = self.table((g / gtes) as usize)? else { return Ok(Grain::Absent) };
        Ok(match t[(g % gtes) as usize] as u64 {
            0 => Grain::Absent,
            1 if zeroed || compressed => Grain::Zero,
            s if compressed => Grain::Packed(s),
            s => Grain::At(s * 512),
        })
    }

    fn unpack(&mut self, sector: u64) -> Result<&[u8]> {
        if self.unpacked.as_ref().is_none_or(|(k, _)| *k != sector) {
            let at = sector * 512;
            let (data_at, len) = if self.markers {
                let m = read(self.dev.as_mut(), at, 12)?;
                (at + 12, le32(&m, 8) as u64)
            } else {
                (at, (self.grain * 2).min(self.dev.size().saturating_sub(at)))
            };
            if len > self.grain * 2 + 4096 {
                return Err(damaged("a compressed grain is too large"));
            }
            let packed = read(self.dev.as_mut(), data_at, len as usize)?;
            let mut out = vec![0u8; self.grain as usize];
            super::inflate_zlib(&packed, &mut out)?;
            self.unpacked = Some((sector, out));
        }
        Ok(&self.unpacked.as_ref().unwrap().1)
    }

    /// Appends `bytes` (sector-aligned) at the end of the file.
    fn append(&mut self, bytes: &[u8]) -> Result<u64> {
        let at = self.dev.size().div_ceil(512) * 512;
        self.dev.resize(at + bytes.len() as u64)?;
        self.dev.write_at(at, bytes)?;
        Ok(at)
    }

    /// Makes grain `g` present, holding `content` (a whole grain).
    fn add_grain(&mut self, g: u64, content: &[u8]) -> Result<()> {
        let ti = (g / self.gtes) as usize;
        let ei = (g % self.gtes) as usize;
        let table_bytes = (self.gtes * 4).div_ceil(512) * 512;
        if self.gd[ti] == 0 {
            // New grain tables (and their redundant copies).
            let zeros = vec![0u8; table_bytes as usize];
            let h = sparse_header(self.dev.as_mut())?;
            let (rgd_at, gd_at) = (le64(&h, 48), le64(&h, 56));
            if self.rgd.is_some() {
                let at = (self.append(&zeros)? / 512) as u32;
                self.rgd.as_mut().unwrap()[ti] = at;
                self.dev.write_at(rgd_at * 512 + ti as u64 * 4, &at.to_le_bytes())?;
            }
            let at = (self.append(&zeros)? / 512) as u32;
            self.gd[ti] = at;
            self.dev.write_at(gd_at * 512 + ti as u64 * 4, &at.to_le_bytes())?;
        }
        let sector = u32::try_from(self.append(content)? / 512).map_err(|_| damaged("the extent file is too large"))?;
        let t = self.gd[ti] as u64;
        self.dev.write_at(t * 512 + ei as u64 * 4, &sector.to_le_bytes())?;
        if let Some(r) = self.rgd.as_ref().map(|r| r[ti] as u64).filter(|&r| r != 0) {
            self.dev.write_at(r * 512 + ei as u64 * 4, &sector.to_le_bytes())?;
        }
        if let Some(t) = self.table(ti)? {
            t[ei] = sector;
        }
        Ok(())
    }
}

fn le16_at(b: &[u8], at: usize) -> u16 {
    super::le16(b, at)
}

// ---------------------------------------------------------------------------
// The disk: its extents one after another.

enum Extent {
    Flat { dev: Box<dyn BlockDevice>, offset: u64, writable: bool },
    Zero,
    Sparse(Box<Sparse>),
}

struct Vmdk {
    file: DeviceInfo,
    create_type: String,
    /// Each extent with where it starts on the disk; the size last.
    extents: Vec<(u64, Extent)>,
    size: u64,
    parent: Option<(Box<dyn BlockDevice>, String)>,
    writable: bool,
}

impl Vmdk {
    fn open(mut dev: Box<dyn BlockDevice>, ctx: &OpenContext<'_>) -> Result<Self> {
        let file = dev.info();
        let writable = !dev.is_read_only();
        let n = 512.min(dev.size() as usize);
        let head = read(dev.as_mut(), 0, n)?;
        let sparse_file = head.len() >= 4 && le32(&head, 0) == KDMV;
        let text = if sparse_file {
            let (at, len) = (le64(&head, 28), le64(&head, 36));
            if at == 0 || len == 0 {
                None
            } else {
                Some(read(dev.as_mut(), at * 512, (len * 512).min(1 << 20) as usize)?)
            }
        } else {
            let n = dev.size().min(1 << 20) as usize;
            Some(read(dev.as_mut(), 0, n)?)
        };
        let text = text.map(|t| String::from_utf8_lossy(&t).trim_end_matches('\0').to_string());
        let desc = match &text {
            Some(t) => parse_descriptor(t)?,
            // A sparse file without a descriptor: the extent alone.
            None => Descriptor {
                create_type: "monolithicSparse".into(),
                parent: None,
                extents: vec![ExtentLine {
                    access: "RW".into(),
                    sectors: le64(&head, 12),
                    kind: "SPARSE".into(),
                    file: None,
                    offset: 0,
                }],
            },
        };
        let mut own = Some(dev);
        let mut extents = Vec::new();
        let mut start = 0u64;
        let single = desc.extents.len() == 1;
        for e in &desc.extents {
            let rw = writable && e.access == "RW";
            let mut open_file = |name: &Option<String>| -> Result<Box<dyn BlockDevice>> {
                // A sparse file holding its own descriptor is its extent.
                if sparse_file
                    && single
                    && let Some(d) = own.take()
                {
                    return Ok(d);
                }
                let name = name.as_deref().ok_or_else(|| damaged("an extent without a file name"))?;
                ctx.open_file(name, rw)
            };
            let extent = match e.kind.as_str() {
                "FLAT" | "VMFS" => {
                    let d = open_file(&e.file)?;
                    if (e.offset + e.sectors) * 512 > d.size() {
                        return Err(damaged("a flat extent is shorter than the descriptor says"));
                    }
                    Extent::Flat { writable: rw && !d.is_read_only(), dev: d, offset: e.offset * 512 }
                }
                "ZERO" => Extent::Zero,
                "SPARSE" => {
                    let s = Sparse::open(open_file(&e.file)?)?;
                    if s.capacity < e.sectors * 512 {
                        return Err(damaged("a sparse extent is smaller than the descriptor says"));
                    }
                    Extent::Sparse(Box::new(s))
                }
                k => return Err(unsupported(format!("VMDK extents of type {k}"))),
            };
            extents.push((start, extent));
            start += e.sectors * 512;
        }
        let parent = match &desc.parent {
            Some(name) => Some((ctx.open_parent(name)?, name.clone())),
            None => None,
        };
        let all_writable = extents.iter().all(|(_, e)| match e {
            Extent::Flat { writable, .. } => *writable,
            Extent::Zero => false,
            Extent::Sparse(s) => !s.compressed && !s.dev.is_read_only(),
        });
        Ok(Self {
            file,
            create_type: desc.create_type,
            extents,
            size: start,
            parent,
            writable: writable && all_writable,
        })
    }

    /// The extent holding `offset`, and where it starts and ends.
    fn find(&self, offset: u64) -> (usize, u64, u64) {
        let i = self.extents.partition_point(|(s, _)| *s <= offset) - 1;
        let end = self.extents.get(i + 1).map_or(self.size, |(s, _)| *s);
        (i, self.extents[i].0, end)
    }
}

impl Layout for Vmdk {
    fn size(&self) -> u64 {
        self.size
    }
    fn block(&self) -> u64 {
        1 << 62
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset + done as u64;
            let (i, start, end) = self.find(at);
            let inner = at - start;
            let mut n = super::upto(end - at, buf.len() - done);
            let parent = self.parent.as_mut().map(|p| p.0.as_mut());
            match &mut self.extents[i].1 {
                Extent::Flat { dev, offset, .. } => dev.read_at(*offset + inner, &mut buf[done..done + n])?,
                Extent::Zero => buf[done..done + n].fill(0),
                Extent::Sparse(s) => {
                    let g = inner / s.grain;
                    let gi = (inner % s.grain) as usize;
                    n = n.min(s.grain as usize - gi);
                    let part = &mut buf[done..done + n];
                    match s.lookup(g)? {
                        Grain::Absent => read_parent(parent, at, part)?,
                        Grain::Zero => part.fill(0),
                        Grain::At(b) => s.dev.read_at(b + gi as u64, part)?,
                        Grain::Packed(sector) => part.copy_from_slice(&s.unpack(sector)?[gi..gi + n]),
                    }
                }
            }
            done += n;
        }
        Ok(())
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let mut done = 0usize;
        while done < data.len() {
            let at = offset + done as u64;
            let (i, start, end) = self.find(at);
            let inner = at - start;
            let mut n = super::upto(end - at, data.len() - done);
            match &mut self.extents[i].1 {
                Extent::Flat { dev, offset, .. } => dev.write_at(*offset + inner, &data[done..done + n])?,
                Extent::Zero => return Err(Error::read_only()),
                Extent::Sparse(s) => {
                    let g = inner / s.grain;
                    let gi = (inner % s.grain) as usize;
                    n = n.min(s.grain as usize - gi);
                    match s.lookup(g)? {
                        Grain::At(b) => s.dev.write_at(b + gi as u64, &data[done..done + n])?,
                        Grain::Packed(_) => return Err(Error::read_only()),
                        old => {
                            let mut grain = vec![0u8; s.grain as usize];
                            if matches!(old, Grain::Absent) {
                                read_parent(self.parent.as_mut().map(|p| p.0.as_mut()), at - gi as u64, &mut grain)?;
                            }
                            grain[gi..gi + n].copy_from_slice(&data[done..done + n]);
                            s.add_grain(g, &grain)?;
                        }
                    }
                }
            }
            done += n;
        }
        Ok(())
    }

    fn writable(&self) -> bool {
        self.writable
    }
    fn flush(&mut self) -> Result<()> {
        for (_, e) in &mut self.extents {
            match e {
                Extent::Flat { dev, .. } => dev.flush()?,
                Extent::Sparse(s) => s.dev.flush()?,
                Extent::Zero => {}
            }
        }
        Ok(())
    }
    fn file(&self) -> DeviceInfo {
        self.file.clone()
    }
    fn describe(&self) -> String {
        let mut s = format!("VMDK, {}", if self.create_type.is_empty() { "sparse" } else { &self.create_type });
        if let Some(Extent::Sparse(sp)) = self.extents.first().map(|e| &e.1) {
            s.push_str(&format!(", {} grains", size_label(sp.grain)));
        }
        if self.extents.len() > 1 {
            s.push_str(&format!(", {} extents", self.extents.len()));
        }
        if let Some((_, name)) = &self.parent {
            s.push_str(&format!(", parent {name}"));
        }
        s
    }
}
