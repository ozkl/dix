//! NTFS, read-only.
//!
//! Everything is a file in the Master File Table (MFT), whose own extents
//! come from its first record; see [`record`] for records and attributes,
//! [`index`] for directories and [`lznt1`] for compressed files.
//!
//! What is read: resident and non-resident data, files split over several
//! records (attribute lists), sparse files, LZNT1-compressed files,
//! alternate data streams (listed as hidden `name:stream` entries next to
//! their file), hard links, symbolic links and junctions (reparse points,
//! and the Interix and WSL links ntfs-3g can make). Encrypted (EFS) and
//! WOF-compressed files are listed but cannot be read. Nothing is ever
//! written: a dirty volume (Windows did not shut down cleanly, or it is
//! hibernated) is reported and read as it is.

pub mod check;
mod index;
mod lznt1;
mod record;
#[cfg(all(test, unix))]
mod tests;

pub mod undelete;

/// The free space of an NTFS volume (clusters `$Bitmap` marks free), as
/// byte ranges: what a copy of only the used space leaves out.
pub fn free_space(dev: Box<dyn BlockDevice>) -> Result<Vec<(u64, u64)>> {
    Volume::open(dev)?.free_ranges()
}
use dix_core::{
    AccessMode, BlockDevice, Capabilities, DirEntry, Driver, DriverInfo, EntryKind, Error, ErrorKind, FileSystem,
    FsInfo, Metadata, MountOptions, ProbeScore, Result, VfsPath,
};
use index::{Entry, FileName, Node, UpCase};
use record::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const CAPS: Capabilities =
    Capabilities::READ.union(Capabilities::SYMLINKS).union(Capabilities::TIMESTAMPS).union(Capabilities::CHECK);

pub const ROOT: u64 = 5;
const EXTEND: u64 = 11;

const TAG_SYMLINK: u32 = 0xA000_000C;
const TAG_JUNCTION: u32 = 0xA000_0003;
const TAG_LX_SYMLINK: u32 = 0xA000_001D;
const TAG_WOF: u32 = 0x8000_0017;
const TAG_DEDUP: u32 = 0x8000_0013;

/// File attribute bits ($STANDARD_INFORMATION and $FILE_NAME).
const FA_HIDDEN: u32 = 0x2;
const FA_SYSTEM: u32 = 0x4;

pub struct NtfsDriver;

impl Driver for NtfsDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: "ntfs",
            name: "NTFS",
            description: "NTFS (read-only: compressed and sparse files, streams, links)",
            capabilities: CAPS,
        }
    }

    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
        if dev.size() < 512 {
            return Ok(0);
        }
        let mut b = [0u8; 512];
        dev.read_at(0, &mut b)?;
        Ok(match Boot::parse(&b) {
            Ok(boot) if boot.mft_lcn * boot.cluster < dev.size() => 95,
            Ok(_) => 40, // the MFT is past the end: a truncated image?
            Err(_) => 0,
        })
    }

    fn mount(&self, dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
        let log = &opts.log;
        if opts.mode == AccessMode::ReadWrite {
            log.info("NTFS is read-only in this version: the volume is opened read-only.");
        }
        let mut v = Volume::open(dev)?;
        let end = v.boot.total_sectors * v.boot.bytes_per_sector as u64;
        if end > v.dev.size() {
            log.warn(format!("The volume claims {end} bytes but the image has {}: it may be cut short.", v.dev.size()));
        }
        if let Some(info) = v.volume_info() {
            if info.dirty {
                log.warn(
                    "The volume is marked dirty: Windows did not shut down cleanly or wants it checked (chkdsk). \
                     It is read as it is.",
                );
            }
            log.info(format!("NTFS {}.{}", info.major, info.minor));
        }
        if v.hibernated() {
            log.warn(
                "Windows is hibernated on this volume (or shut down with Fast Startup): what is shown may be \
                 out of date, and it must not be changed.",
            );
        }
        let free = v.count_free().ok();
        let label = v.label();
        Ok(Box::new(NtfsFs { v, free, label }))
    }

    fn check(&self, dev: Box<dyn BlockDevice>, opts: &dix_core::CheckOptions) -> Result<dix_core::CheckReport> {
        check::check(dev, &opts.log, opts.repair)
    }
}

/// A file: its base record's header and every attribute, extension
/// records and extents merged.
pub(crate) struct File {
    pub rec: Record,
    pub attrs: Vec<Attr>,
}

impl File {
    pub fn attr(&self, ty: u32, name: &str) -> Option<&Attr> {
        self.attrs.iter().find(|a| a.ty == ty && a.name == name)
    }

    pub fn names(&self) -> impl Iterator<Item = FileName> + '_ {
        self.attrs.iter().filter(|a| a.ty == AT_FILE_NAME).filter_map(|a| a.value().and_then(FileName::parse))
    }

    /// File attribute flags from $STANDARD_INFORMATION.
    pub fn file_attributes(&self) -> u32 {
        self.attr(AT_STANDARD_INFORMATION, "")
            .and_then(|a| a.value())
            .filter(|v| v.len() >= 36)
            .map_or(0, |v| le32(v, 32))
    }
}

pub(crate) struct VolumeInfo {
    pub major: u8,
    pub minor: u8,
    pub dirty: bool,
}

/// What a reparse point (or a link ntfs-3g makes) says.
enum Link {
    To(String),
    /// A reparse point this driver cannot follow or read through.
    Other(u32),
}

/// The open volume: device, geometry, the MFT's extents and caches.
pub(crate) struct Volume {
    pub dev: Box<dyn BlockDevice>,
    pub boot: Boot,
    pub mft: Vec<Run>,
    pub mft_size: u64,
    pub upcase: UpCase,
    files: HashMap<u64, Arc<File>>,
    /// The last decompressed compression unit: (record, attribute id, unit, data).
    unit: Option<(u64, u16, u64, Vec<u8>)>,
}

impl Volume {
    pub fn open(mut dev: Box<dyn BlockDevice>) -> Result<Volume> {
        let mut b = [0u8; 512];
        dev.read_at(0, &mut b)?;
        let boot = Boot::parse(&b)?;
        // Record 0 ($MFT) is at the start of the MFT; its $DATA says where the
        // rest is.
        let mut raw = vec![0u8; boot.record_size as usize];
        dev.read_at(boot.mft_lcn * boot.cluster, &mut raw)?;
        fixup(&mut raw, b"FILE").map_err(|e| e.with_context("the MFT"))?;
        let rec0 = parse_record(&raw, 0)?;
        let data = rec0.attrs.iter().find(|a| a.ty == AT_DATA && a.name.is_empty());
        let Some(Attr { body: Body::NonResident { runs, size, .. }, .. }) = data else {
            return Err(Error::corrupted("the MFT has no data"));
        };
        let mut v = Volume {
            boot,
            mft: runs.clone(),
            mft_size: *size,
            upcase: UpCase::ascii(),
            dev,
            files: HashMap::new(),
            unit: None,
        };
        // A very fragmented MFT continues in extension records (which the
        // first extent reaches).
        if rec0.attrs.iter().any(|a| a.ty == AT_ATTRIBUTE_LIST) {
            let f = v.file(0)?;
            if let Some(Attr { body: Body::NonResident { runs, .. }, .. }) = f.attr(AT_DATA, "") {
                v.mft = runs.clone();
            }
            v.files.clear();
        }
        // $UpCase, for name lookups.
        if let Ok(f) = v.file(10)
            && let Some(a) = f.attr(AT_DATA, "")
            && let Ok(t) = v.attr_vec(10, a, 1 << 17)
            && t.len() == 1 << 17
        {
            v.upcase = UpCase::from_bytes(&t);
        }
        Ok(v)
    }

    pub fn cs(&self) -> u64 {
        self.boot.cluster
    }

    pub fn record_count(&self) -> u64 {
        self.mft_size / self.boot.record_size as u64
    }

    /// Reads record `n` as it is on disk (before the fixups).
    pub fn raw_record(&mut self, n: u64) -> Result<Vec<u8>> {
        let rs = self.boot.record_size as u64;
        if n >= self.record_count() {
            return Err(Error::corrupted(format!("record {n} is past the end of the MFT")));
        }
        let mut buf = vec![0u8; rs as usize];
        let (runs, size) = (std::mem::take(&mut self.mft), self.mft_size);
        let r = read_runs(self.dev.as_mut(), &runs, self.boot.cluster, size, size, n * rs, &mut buf);
        self.mft = runs;
        r?;
        Ok(buf)
    }

    pub fn record(&mut self, n: u64) -> Result<Record> {
        let mut raw = self.raw_record(n)?;
        fixup(&mut raw, b"FILE").map_err(|e| e.with_context(format!("record {n}")))?;
        parse_record(&raw, n)
    }

    /// A file by record number, with its attribute list resolved.
    pub fn file(&mut self, n: u64) -> Result<Arc<File>> {
        if let Some(f) = self.files.get(&n) {
            return Ok(f.clone());
        }
        let rec = self.record(n)?;
        if !rec.in_use() {
            return Err(Error::not_found(format!("record {n} (not in use)")));
        }
        let mut attrs = Vec::new();
        if let Some(list) = rec.attrs.iter().find(|a| a.ty == AT_ATTRIBUTE_LIST).cloned() {
            let v = self.attr_vec(n, &list, 1 << 24)?;
            let mut seen = HashSet::from([n]);
            for (_, r) in list_entries(&v) {
                if seen.insert(r) {
                    let ext = self.record(r)?;
                    if ext.base != n {
                        return Err(Error::corrupted(format!(
                            "record {r} is listed by record {n} but is not its extension"
                        )));
                    }
                    attrs.extend(ext.attrs);
                }
            }
            attrs.extend(rec.attrs.iter().filter(|a| a.ty != AT_ATTRIBUTE_LIST).cloned());
        } else {
            attrs = rec.attrs.clone();
        }
        let f = Arc::new(File { rec, attrs: merge(attrs) });
        if self.files.len() > 8192 {
            self.files.clear();
        }
        self.files.insert(n, f.clone());
        Ok(f)
    }

    /// Reads from an attribute's value; returns the bytes read (0 at the end).
    pub fn attr_read(&mut self, rec: u64, a: &Attr, off: u64, buf: &mut [u8]) -> Result<usize> {
        match &a.body {
            Body::Resident(v) => {
                if off >= v.len() as u64 {
                    return Ok(0);
                }
                let n = buf.len().min(v.len() - off as usize);
                buf[..n].copy_from_slice(&v[off as usize..off as usize + n]);
                Ok(n)
            }
            Body::NonResident { runs, size, init, unit, .. } => {
                if a.flags & FLAG_ENCRYPTED != 0 {
                    return Err(Error::unsupported("reading encrypted (EFS) files"));
                }
                if a.flags & FLAG_COMPRESSED != 0 && *unit != 0 {
                    return self.read_compressed(rec, a, off, buf);
                }
                read_runs(self.dev.as_mut(), runs, self.boot.cluster, *size, *init, off, buf)
            }
        }
    }

    /// A whole attribute value (at most `limit` bytes).
    pub fn attr_vec(&mut self, rec: u64, a: &Attr, limit: u64) -> Result<Vec<u8>> {
        let size = attr_size(a);
        if size > limit {
            return Err(Error::corrupted(format!("record {rec}: an attribute is implausibly large")));
        }
        let mut v = vec![0u8; size as usize];
        let n = self.attr_read(rec, a, 0, &mut v)?;
        v.truncate(n);
        Ok(v)
    }

    fn read_compressed(&mut self, rec: u64, a: &Attr, off: u64, buf: &mut [u8]) -> Result<usize> {
        let Body::NonResident { runs, size, init, unit, .. } = &a.body else { unreachable!() };
        let cs = self.boot.cluster;
        let unit_clusters = 1u64 << unit.min(&16);
        let ub = cs * unit_clusters;
        let end = (off + buf.len() as u64).min(*size);
        if off >= end {
            return Ok(0);
        }
        let mut pos = off;
        while pos < end {
            let u = pos / ub;
            let fresh = !matches!(&self.unit, Some((r, id, uu, _)) if *r == rec && *id == a.id && *uu == u);
            if fresh {
                let data = self.unit_data(runs, u * unit_clusters, unit_clusters)?;
                self.unit = Some((rec, a.id, u, data));
            }
            let data = &self.unit.as_ref().unwrap().3;
            let from = (pos - u * ub) as usize;
            let n = (end - pos).min((ub as usize - from) as u64) as usize;
            let dst = &mut buf[(pos - off) as usize..(pos - off) as usize + n];
            dst.copy_from_slice(&data[from..from + n]);
            // Past the initialised size reads as zeroes.
            if pos + n as u64 > *init {
                let z = init.saturating_sub(pos) as usize;
                dst[z..].fill(0);
            }
            pos += n as u64;
        }
        Ok((end - off) as usize)
    }

    /// One compression unit, decompressed: all clusters allocated means
    /// plain data, none means zeroes, some means LZNT1 in those.
    fn unit_data(&mut self, runs: &[Run], vcn: u64, clusters: u64) -> Result<Vec<u8>> {
        let cs = self.boot.cluster;
        let mut stored = Vec::new();
        for r in runs.iter().filter(|r| r.vcn < vcn + clusters && r.vcn + r.len > vcn) {
            let (s, e) = (r.vcn.max(vcn), (r.vcn + r.len).min(vcn + clusters));
            if let Some(lcn) = r.lcn {
                let at = stored.len();
                stored.resize(at + ((e - s) * cs) as usize, 0);
                self.dev.read_at((lcn + s - r.vcn) * cs, &mut stored[at..])?;
            }
        }
        let ub = (clusters * cs) as usize;
        let mut out = vec![0u8; ub];
        if stored.len() == ub {
            out = stored;
        } else if !stored.is_empty() {
            lznt1::decompress(&stored, &mut out)?;
        }
        Ok(out)
    }

    fn index_root(&self, dir: &File) -> Result<(Vec<u8>, u32)> {
        let root = dir.attr(AT_INDEX_ROOT, "$I30").and_then(|a| a.value()).filter(|v| v.len() >= 32);
        let root = root.ok_or_else(|| Error::corrupted(format!("directory {} has no index", dir.rec.number)))?;
        Ok((root.to_vec(), le32(root, 8)))
    }

    /// An index block ("INDX") of a directory, by VCN.
    fn index_block(&mut self, dir: &File, vcn: u64, size: u32) -> Result<Node> {
        let n = dir.rec.number;
        let alloc = dir.attr(AT_INDEX_ALLOCATION, "$I30");
        let alloc = alloc.ok_or_else(|| Error::corrupted(format!("directory {n}: an index node is missing")))?;
        // VCNs count clusters, or 512-byte blocks when clusters are larger
        // than index blocks.
        let unit = if size as u64 >= self.cs() { self.cs() } else { 512 };
        let mut b = vec![0u8; size as usize];
        if self.attr_read(n, alloc, vcn * unit, &mut b)? != b.len() {
            return Err(Error::corrupted(format!("directory {n}: an index node is past the end")));
        }
        fixup(&mut b, b"INDX").map_err(|e| e.with_context(format!("directory {n}")))?;
        if le64(&b, 16) != vcn {
            return Err(Error::corrupted(format!("directory {n}: an index node is out of place")));
        }
        index::parse_node(&b, 24)
    }

    /// Every entry of a directory, in order (DOS aliases included).
    pub fn dir_entries(&mut self, dir: &File) -> Result<Vec<Entry>> {
        let (root, size) = self.index_root(dir)?;
        let node = index::parse_node(&root, 16)?;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        self.walk(dir, node, size, 0, &mut seen, &mut out)?;
        Ok(out)
    }

    fn walk(
        &mut self,
        dir: &File,
        node: Node,
        size: u32,
        depth: u32,
        seen: &mut HashSet<u64>,
        out: &mut Vec<Entry>,
    ) -> Result<()> {
        if depth > 32 {
            return Err(Error::corrupted(format!("directory {}: the index is too deep", dir.rec.number)));
        }
        let mut visit = |v: &mut Self, out: &mut Vec<Entry>, child: Option<u64>| -> Result<()> {
            if let Some(vcn) = child {
                if !seen.insert(vcn) {
                    return Err(Error::corrupted(format!("directory {}: the index loops", dir.rec.number)));
                }
                let node = v.index_block(dir, vcn, size)?;
                v.walk(dir, node, size, depth + 1, seen, out)?;
            }
            Ok(())
        };
        for (e, child) in node.entries {
            visit(self, out, child)?;
            out.push(e);
        }
        visit(self, out, node.last_child)
    }

    /// Finds `name` in a directory: down the tree, comparing upper-cased;
    /// an exact (case-sensitive) match is preferred.
    pub fn lookup(&mut self, dir: &File, name: &str) -> Result<Option<Entry>> {
        let want: Vec<u16> = name.encode_utf16().collect();
        let (root, size) = self.index_root(dir)?;
        let mut node = index::parse_node(&root, 16)?;
        let mut loose = None;
        for _ in 0..32 {
            let mut next = node.last_child;
            for (e, child) in &node.entries {
                match self.upcase.cmp(&want, &e.key.name) {
                    std::cmp::Ordering::Equal => {
                        if e.key.name == want {
                            return Ok(Some(e.clone()));
                        }
                        loose = Some(e.clone());
                        // A name differing only in case may sit next to it.
                        next = None;
                        break;
                    }
                    std::cmp::Ordering::Less => {
                        next = *child;
                        break;
                    }
                    std::cmp::Ordering::Greater => {}
                }
            }
            match next {
                Some(vcn) => node = self.index_block(dir, vcn, size)?,
                None => break,
            }
        }
        if loose.is_some() {
            // Case-insensitively equal names: look for the exact one among all.
            let all = self.dir_entries(dir)?;
            if let Some(e) = all.iter().find(|e| e.key.name == want) {
                return Ok(Some(e.clone()));
            }
        }
        Ok(loose)
    }

    /// The record a path names, and the data stream (`file:stream`).
    pub fn resolve(&mut self, path: &VfsPath) -> Result<(u64, Option<String>)> {
        let mut cur = ROOT;
        let parts: Vec<&str> = path.components().collect();
        for (i, part) in parts.iter().enumerate() {
            let dir = self.file(cur)?;
            if !dir.rec.is_dir() {
                return Err(Error::not_a_directory(path));
            }
            if let Some(e) = self.lookup(&dir, part)? {
                cur = e.record();
                continue;
            }
            if i + 1 == parts.len()
                && let Some((base, stream)) = part.split_once(':')
                && let Some(e) = self.lookup(&dir, base)?
            {
                let f = self.file(e.record())?;
                if !stream.is_empty() && f.attr(AT_DATA, stream).is_some() {
                    return Ok((e.record(), Some(stream.to_string())));
                }
            }
            return Err(Error::not_found(path));
        }
        Ok((cur, None))
    }

    pub fn volume_info(&mut self) -> Option<VolumeInfo> {
        let f = self.file(3).ok()?;
        let v = f.attr(AT_VOLUME_INFORMATION, "")?.value()?;
        (v.len() >= 12).then(|| VolumeInfo { major: v[8], minor: v[9], dirty: le16(v, 10) & 1 != 0 })
    }

    fn label(&mut self) -> Option<String> {
        let f = self.file(3).ok()?;
        let v = f.attr(AT_VOLUME_NAME, "")?.value()?;
        let units: Vec<u16> = v.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let s = String::from_utf16_lossy(&units);
        (!s.is_empty()).then_some(s)
    }

    /// Whether `hiberfil.sys` holds a hibernated system.
    fn hibernated(&mut self) -> bool {
        let Ok(path) = VfsPath::parse("/hiberfil.sys") else { return false };
        let Ok((rec, _)) = self.resolve(&path) else { return false };
        let Ok(f) = self.file(rec) else { return false };
        let Some(a) = f.attr(AT_DATA, "") else { return false };
        let mut m = [0u8; 4];
        matches!(self.attr_read(rec, a, 0, &mut m), Ok(4)) && (m.eq_ignore_ascii_case(b"hibr") || &m == b"wake")
    }

    /// The free clusters, from `$Bitmap`, as byte ranges of the volume.
    pub fn free_ranges(&mut self) -> Result<Vec<(u64, u64)>> {
        let f = self.file(6)?;
        let a = f.attr(AT_DATA, "").ok_or_else(|| Error::corrupted("$Bitmap has no data"))?;
        let total = self.boot.total_clusters();
        let cs = self.boot.cluster;
        let mut out: Vec<(u64, u64)> = Vec::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut off = 0u64;
        while off * 8 < total {
            let n = self.attr_read(6, a, off, &mut buf)?;
            if n == 0 {
                break;
            }
            for (i, &b) in buf[..n].iter().enumerate() {
                let first = (off + i as u64) * 8;
                if b == 0xFF {
                    continue;
                }
                for bit in 0..8u64 {
                    let c = first + bit;
                    if c >= total || b >> bit & 1 != 0 {
                        continue;
                    }
                    match out.last_mut() {
                        Some(l) if l.1 == c * cs => l.1 += cs,
                        _ => out.push((c * cs, (c + 1) * cs)),
                    }
                }
            }
            off += n as u64;
        }
        Ok(out)
    }

    /// Free clusters, from `$Bitmap`.
    pub fn count_free(&mut self) -> Result<u64> {
        let f = self.file(6)?;
        let a = f.attr(AT_DATA, "").ok_or_else(|| Error::corrupted("$Bitmap has no data"))?;
        let total = self.boot.total_clusters();
        let mut used = 0u64;
        let mut buf = vec![0u8; 1 << 20];
        let mut off = 0u64;
        while off * 8 < total {
            let n = self.attr_read(6, a, off, &mut buf)?;
            if n == 0 {
                break;
            }
            for (i, &b) in buf[..n].iter().enumerate() {
                let first = (off + i as u64) * 8;
                if first + 8 <= total {
                    used += b.count_ones() as u64;
                } else {
                    for bit in 0..total.saturating_sub(first) {
                        used += (b >> bit & 1) as u64;
                    }
                }
            }
            off += n as u64;
        }
        Ok(total.saturating_sub(used))
    }

    /// Reads a reparse point's target (or the content of an Interix link).
    fn link(&mut self, f: &File) -> Option<Link> {
        let n = f.rec.number;
        if let Some(a) = f.attr(AT_REPARSE_POINT, "") {
            let v = self.attr_vec(n, a, 1 << 16).ok()?;
            if v.len() < 8 {
                return None;
            }
            let tag = le32(&v, 0);
            let d = &v[8..(8 + le16(&v, 4) as usize).min(v.len())];
            let utf16 = |b: &[u8], off: usize, len: usize| -> Option<String> {
                let s = b.get(off..off + len)?;
                Some(String::from_utf16_lossy(
                    &s.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>(),
                ))
            };
            let target = |d: &[u8], buf: usize| -> Option<String> {
                if d.len() < 8 {
                    return None;
                }
                let (so, sl, po, pl) =
                    (le16(d, 0) as usize, le16(d, 2) as usize, le16(d, 4) as usize, le16(d, 6) as usize);
                let print = utf16(d, buf + po, pl).filter(|s| !s.is_empty());
                let sub = utf16(d, buf + so, sl).map(|s| s.strip_prefix("\\??\\").map(str::to_string).unwrap_or(s));
                print.or(sub)
            };
            return Some(match tag {
                TAG_SYMLINK => {
                    let t = target(d, 12)?;
                    let relative = d.len() >= 12 && le32(d, 8) & 1 != 0;
                    Link::To(if relative { t.replace('\\', "/") } else { t })
                }
                TAG_JUNCTION => Link::To(target(d, 8)?),
                TAG_LX_SYMLINK if d.len() >= 4 => Link::To(String::from_utf8_lossy(&d[4..]).into_owned()),
                t => Link::Other(t),
            });
        }
        // Interix symbolic links: a system file holding "IntxLNK\1" and the
        // target in UTF-16.
        if f.file_attributes() & FA_SYSTEM != 0
            && !f.rec.is_dir()
            && let Some(a) = f.attr(AT_DATA, "")
            && (10..=4096).contains(&attr_size(a))
        {
            let v = self.attr_vec(n, a, 4096).ok()?;
            if v.starts_with(b"IntxLNK\x01") {
                return utf16_lossy(&v[8..]).map(Link::To);
            }
        }
        None
    }

    /// Metadata of a file (or of one of its streams).
    fn metadata(&mut self, f: &File, stream: Option<&str>) -> Metadata {
        let attrs = f.file_attributes();
        let mut m = if let Some(s) = stream {
            Metadata::new(EntryKind::File, f.attr(AT_DATA, s).map_or(0, attr_size))
        } else if f.rec.is_dir() {
            Metadata::new(EntryKind::Directory, 0)
        } else {
            Metadata::new(EntryKind::File, f.attr(AT_DATA, "").map_or(0, attr_size))
        };
        let mut hide = FA_HIDDEN | FA_SYSTEM;
        if stream.is_none()
            && let Some(Link::To(t)) = self.link(f)
        {
            m.kind = EntryKind::Symlink;
            m.size = t.len() as u64;
            m.symlink_target = Some(t);
            hide = FA_HIDDEN; // Interix links are "system" files
        }
        if let Some(si) = f.attr(AT_STANDARD_INFORMATION, "").and_then(|a| a.value()).filter(|v| v.len() >= 32) {
            m.created = filetime(le64(si, 0));
            m.modified = filetime(le64(si, 8));
            m.accessed = filetime(le64(si, 24));
        }
        m.inode = Some(f.rec.number);
        m.hidden = attrs & hide != 0 || stream.is_some();
        m
    }
}

fn utf16_lossy(b: &[u8]) -> Option<String> {
    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
    (!units.is_empty()).then(|| String::from_utf16_lossy(&units))
}

pub(crate) fn attr_size(a: &Attr) -> u64 {
    match &a.body {
        Body::Resident(v) => v.len() as u64,
        Body::NonResident { size, .. } => *size,
    }
}

/// Windows FILETIME (100 ns since 1601) to Unix seconds.
fn filetime(t: u64) -> Option<i64> {
    (t != 0).then(|| (t / 10_000_000) as i64 - 11_644_473_600)
}

/// Reads `[off, off + buf.len())` of a non-resident value (cut at `size`)
/// through its runs; sparse runs and what lies past `init` read as zeroes.
pub(crate) fn read_runs(
    dev: &mut dyn BlockDevice,
    runs: &[Run],
    cs: u64,
    size: u64,
    init: u64,
    off: u64,
    buf: &mut [u8],
) -> Result<usize> {
    let end = (off + buf.len() as u64).min(size);
    if off >= end {
        return Ok(0);
    }
    let n = (end - off) as usize;
    buf[..n].fill(0);
    let readable = end.min(init);
    let first = runs.partition_point(|r| (r.vcn + r.len) * cs <= off);
    for r in &runs[first..] {
        let (rs, re) = (r.vcn * cs, (r.vcn + r.len) * cs);
        if rs >= readable {
            break;
        }
        let (s, e) = (rs.max(off), re.min(readable));
        if s >= e {
            continue;
        }
        if let Some(lcn) = r.lcn {
            dev.read_at(lcn * cs + (s - rs), &mut buf[(s - off) as usize..(e - off) as usize])?;
        }
    }
    Ok(n)
}

pub struct NtfsFs {
    v: Volume,
    free: Option<u64>,
    label: Option<String>,
}

impl NtfsFs {
    /// The file (and stream) a path names.
    fn open(&mut self, path: &VfsPath) -> Result<(Arc<File>, Option<String>)> {
        let (rec, stream) = self.v.resolve(path)?;
        Ok((self.v.file(rec)?, stream))
    }
}

impl FileSystem for NtfsFs {
    fn info(&self) -> FsInfo {
        let cs = self.v.boot.cluster;
        FsInfo {
            driver_id: "ntfs".into(),
            fs_name: "NTFS".into(),
            label: self.label.clone(),
            total_bytes: Some(self.v.boot.total_clusters() * cs),
            free_bytes: self.free.map(|f| f * cs),
            block_size: Some(cs as u32),
            details: Vec::new(),
        }
    }

    fn capabilities(&self) -> Capabilities {
        CAPS
    }

    fn access_mode(&self) -> AccessMode {
        AccessMode::ReadOnly
    }

    fn read_dir(&mut self, path: &VfsPath) -> Result<Vec<DirEntry>> {
        let (dir, stream) = self.open(path)?;
        if stream.is_some() || !dir.rec.is_dir() {
            return Err(Error::not_a_directory(path));
        }
        let me = dir.rec.number;
        let mut out = Vec::new();
        for e in self.v.dir_entries(&dir)? {
            // DOS 8.3 aliases name the same file again; "." is the root itself.
            if e.key.namespace == 2 || e.record() == me {
                continue;
            }
            let name = e.key.name();
            let f = match self.v.file(e.record()) {
                Ok(f) => f,
                Err(err) if matches!(err.kind(), ErrorKind::Corrupted | ErrorKind::NotFound) => {
                    // Still list it, so the rest of the folder is usable.
                    let mut m = Metadata::new(EntryKind::Other, 0);
                    m.inode = Some(e.record());
                    out.push(DirEntry { name, metadata: m });
                    continue;
                }
                Err(err) => return Err(err),
            };
            let mut m = self.v.metadata(&f, None);
            if name.starts_with('$') && (e.record() < 16 || me == EXTEND) {
                m.hidden = true;
            }
            for s in f.attrs.iter().filter(|a| a.ty == AT_DATA && !a.name.is_empty()) {
                let meta = self.v.metadata(&f, Some(&s.name));
                out.push(DirEntry { name: format!("{name}:{}", s.name), metadata: meta });
            }
            out.push(DirEntry { name, metadata: m });
        }
        Ok(out)
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        let (f, stream) = self.open(path)?;
        Ok(self.v.metadata(&f, stream.as_deref()))
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let (f, stream) = self.open(path)?;
        if stream.is_none() && f.rec.is_dir() {
            return Err(Error::is_a_directory(path));
        }
        if stream.is_none() {
            match self.v.link(&f) {
                Some(Link::Other(TAG_WOF)) => {
                    return Err(Error::unsupported("reading files compressed by Windows (WOF / CompactOS)"));
                }
                Some(Link::Other(TAG_DEDUP)) => return Err(Error::unsupported("reading deduplicated files")),
                _ => {}
            }
        }
        let name = stream.unwrap_or_default();
        let a = f.attr(AT_DATA, &name).ok_or_else(|| Error::not_found(path))?;
        self.v.attr_read(f.rec.number, a, offset, buf)
    }

    fn release_cache(&mut self, _path: &VfsPath) {
        self.v.unit = None;
    }
}
