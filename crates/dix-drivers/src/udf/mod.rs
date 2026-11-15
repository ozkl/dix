//! UDF (Universal Disk Format, ECMA-167 / OSTA UDF 1.02–2.60), read-only.
//!
//! UDF is the file system of DVDs and Blu-ray discs, of packet-written
//! CD-RW/DVD±RW, and of some USB drives. A volume is found like this:
//!
//! * the **volume recognition sequence** at byte 32768 (`BEA01`, `NSR02` or
//!   `NSR03`, `TEA01`) says that the medium holds UDF;
//! * the **anchor** at block 256 (or the last block) points to the volume
//!   descriptor sequence, which gives the block size, the partitions and the
//!   logical volume: its *partition maps* and where the file set descriptor
//!   is. The block size is not known in advance: the anchor is looked for
//!   with 512 to 4096-byte blocks;
//! * the **file set descriptor** gives the root directory's ICB (a file
//!   entry). A file entry holds the owner, permissions, times and where the
//!   data is: allocation descriptors (short, long or extended) or the data
//!   itself, embedded; a directory's data is a list of file identifiers.
//!
//! Partition maps translate a (partition, logical block) address into a
//! block of the medium. All four kinds are read: plain (type 1),
//! **sparable** (rewritable discs: defective packets remapped through a
//! sparing table), **virtual** (CD-R/DVD-R written incrementally: blocks
//! go through the virtual allocation table, the VAT, found at the end of
//! the recorded area) and **metadata** (UDF 2.50+, Blu-ray: directories and
//! file entries live in a metadata file, with a mirror copy used if the
//! main one cannot be read).
//!
//! What is read: files (including sparse and multi-extent ones, files with
//! embedded data, files over 4 GiB), directories, symbolic links, hidden
//! entries, owner, permissions and times. Named streams and extended
//! attributes are not shown.

#[cfg(all(test, unix))]
mod tests;

use dix_core::{
    AccessMode, BlockDevice, Capabilities, DirEntry, Driver, DriverInfo, EntryKind, Error, FileSystem, FsInfo,
    Metadata, MountOptions, ProbeScore, Result, VfsPath,
};
use std::collections::HashMap;
use std::sync::Arc;

/// With the ISO 9660 driver built in, UDF-only images can be made and
/// changed (written anew by its writer).
#[cfg(feature = "iso9660")]
const DRIVER_CAPS: Capabilities = CAPS
    .union(Capabilities::WRITE)
    .union(Capabilities::CREATE_DIR)
    .union(Capabilities::REMOVE)
    .union(Capabilities::RENAME)
    .union(Capabilities::FORMAT);
#[cfg(not(feature = "iso9660"))]
const DRIVER_CAPS: Capabilities = CAPS;

#[cfg(feature = "iso9660")]
const VARIANTS: &[dix_core::FormatVariant] = &[dix_core::FormatVariant {
    id: "udf",
    name: "UDF",
    description: "UDF 1.02 disc image without ISO 9660 (as Blu-ray discs are): files of any size",
    min_size: 0,
    extension: ".iso",
    sized: false,
}];

const CAPS: Capabilities =
    Capabilities::READ.union(Capabilities::SYMLINKS).union(Capabilities::PERMISSIONS).union(Capabilities::TIMESTAMPS);

// Descriptor tag identifiers.
const TAG_PVD: u16 = 1;
const TAG_AVDP: u16 = 2;
const TAG_VDP: u16 = 3;
const TAG_PD: u16 = 5;
const TAG_LVD: u16 = 6;
const TAG_TD: u16 = 8;
const TAG_LVID: u16 = 9;
const TAG_FSD: u16 = 256;
const TAG_FID: u16 = 257;
const TAG_AED: u16 = 258;
const TAG_IE: u16 = 259;
const TAG_TE: u16 = 260;
const TAG_FE: u16 = 261;
const TAG_EFE: u16 = 266;

// File types (ICB tag).
const FT_DIR: u8 = 4;
const FT_FILE: u8 = 5;
const FT_SYMLINK: u8 = 12;
const FT_VAT: u8 = 248;
const FT_REALTIME: u8 = 249;
const FT_METADATA: u8 = 250;
const FT_METADATA_MIRROR: u8 = 251;

// File characteristics (file identifier descriptor).
const FID_HIDDEN: u8 = 1;
const FID_DELETED: u8 = 4;
const FID_PARENT: u8 = 8;

/// Directories and file entries kept in memory (cleared when full).
const CACHE: usize = 4096;
/// Longest allocation descriptor chain followed.
const MAX_AED: usize = 1 << 16;

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// `n` (64-bit), capped at `rest`: never 0 on 32-bit targets because of a
/// truncated cast.
fn upto(n: u64, rest: usize) -> usize {
    n.min(rest as u64) as usize
}

fn damaged(what: impl std::fmt::Display) -> Error {
    Error::corrupted(format!("UDF: {what}"))
}

/// The tag's identifier, if `b` starts with a descriptor tag whose checksum
/// is right.
fn tag(b: &[u8]) -> Option<u16> {
    if b.len() < 16 {
        return None;
    }
    let sum = b[..16].iter().enumerate().filter(|(i, _)| *i != 4).fold(0u8, |s, (_, &x)| s.wrapping_add(x));
    (sum == b[4] && b[5] == 0).then(|| le16(b, 0))
}

/// The tag's identifier, checking that it is `want`.
fn expect(b: &[u8], want: &[u16], what: &str) -> Result<u16> {
    match tag(b) {
        Some(t) if want.contains(&t) => Ok(t),
        Some(t) => Err(damaged(format!("{what}: descriptor {t} found instead"))),
        None => Err(damaged(format!("{what}: no valid descriptor"))),
    }
}

/// An OSTA compressed Unicode string (CS0): compression ID 8 (one byte per
/// character) or 16 (UTF-16BE); 254 and 255 are the same, for names UDF
/// 2.50+ marks as not to be changed.
fn cs0(b: &[u8]) -> String {
    let Some((&id, rest)) = b.split_first() else { return String::new() };
    match id {
        8 | 254 => rest.iter().map(|&c| char::from(c)).collect(),
        16 | 255 => {
            let units = rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]]));
            char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
        }
        _ => String::new(),
    }
}

/// A fixed-size `dstring` field: CS0 whose length is in the last byte.
fn dstring(field: &[u8]) -> String {
    let Some(&n) = field.last() else { return String::new() };
    let n = (n as usize).min(field.len() - 1);
    cs0(&field[..n]).trim_end_matches(['\0', ' ']).to_owned()
}

/// An `entity identifier`'s identifier text ("*UDF Sparable Partition").
fn regid(b: &[u8]) -> &[u8] {
    let id = &b[1..24];
    let end = id.iter().position(|&c| c == 0).unwrap_or(id.len());
    &id[..end]
}

/// A 12-byte timestamp, as Unix seconds.
fn timestamp(b: &[u8]) -> Option<i64> {
    let tz = le16(b, 0);
    let year = le16(b, 2) as i16 as i64;
    let (month, day) = (b[4] as u32, b[5] as u32);
    let (h, m, s) = (b[6] as i64, b[7] as i64, b[8] as i64);
    if year == 0 || !(1..=12).contains(&month) || !(1..=31).contains(&day) || h > 23 || m > 59 || s > 60 {
        return None;
    }
    let mut offset = 0;
    if tz >> 12 == 1 {
        // Minutes from UTC, a signed 12-bit number; -2047: not given.
        let o = ((tz & 0xfff) as i16) << 4 >> 4;
        if o != -2047 {
            offset = o as i64;
        }
    }
    let days = dix_core::time::days_from_civil(year, month, day);
    Some(days * 86400 + h * 3600 + m * 60 + s - offset * 60)
}

/// A long allocation descriptor's address: (partition reference, block).
type Addr = (u16, u32);

fn long_ad(b: &[u8]) -> (u32, Addr) {
    (le32(b, 0), (le16(b, 8), le32(b, 4)))
}

// ---------------------------------------------------------------------------
// The driver.

pub struct UdfDriver;

/// The block size, if `dev` has a UDF volume recognition sequence and an
/// anchor.
fn find(dev: &mut dyn BlockDevice) -> Result<Option<u64>> {
    if dev.size() < 257 * 512 || !recognised(dev)? {
        return Ok(None);
    }
    let mut b = [0u8; 512];
    for bs in [2048u64, 512, 1024, 4096, 8192, 16384, 32768] {
        let blocks = dev.size() / bs;
        for at in [256, blocks.wrapping_sub(1), blocks.wrapping_sub(257), 512] {
            if at < 256 || at >= blocks {
                continue;
            }
            dev.read_at(at * bs, &mut b)?;
            if tag(&b) == Some(TAG_AVDP) && le32(&b, 12) as u64 == at {
                return Ok(Some(bs));
            }
        }
    }
    Ok(None)
}

/// Whether the volume recognition sequence names an NSR (UDF) descriptor.
fn recognised(dev: &mut dyn BlockDevice) -> Result<bool> {
    let mut b = [0u8; 8];
    for stride in [2048u64, 4096, 8192, 16384, 32768] {
        let mut seen_bea = false;
        for i in 0..64u64 {
            let at = 32768 + i * stride;
            if at + 8 > dev.size() {
                break;
            }
            dev.read_at(at, &mut b)?;
            match &b[1..6] {
                b"NSR02" | b"NSR03" if seen_bea => return Ok(true),
                b"BEA01" => seen_bea = true,
                b"CD001" | b"CDW02" | b"BOOT2" => {}
                b"TEA01" => break,
                _ => break,
            }
        }
    }
    Ok(false)
}

/// Whether `dev` also has an ISO 9660 primary volume descriptor (a UDF
/// bridge disc: both file systems describe the same files).
fn has_iso9660(dev: &mut dyn BlockDevice) -> Result<bool> {
    let mut b = [0u8; 8];
    if dev.size() < 17 * 2048 {
        return Ok(false);
    }
    dev.read_at(16 * 2048, &mut b)?;
    Ok(&b[1..6] == b"CD001")
}

/// Whether every partition of the volume has read-only access (a finished
/// disc, as mastering tools and our writer make), not rewritable,
/// overwritable or write-once.
fn finished(dev: &mut dyn BlockDevice) -> Result<bool> {
    let Some(bs) = find(dev)? else { return Ok(false) };
    let blocks = dev.size() / bs;
    let mut b = vec![0u8; bs as usize];
    for at in [256, blocks - 1, blocks.saturating_sub(257), 512] {
        if at < 256 || at >= blocks {
            continue;
        }
        dev.read_at(at * bs, &mut b)?;
        if tag(&b) != Some(TAG_AVDP) || le32(&b, 12) as u64 != at {
            continue;
        }
        let (len, loc) = (le32(&b, 16) as u64, le32(&b, 20) as u64);
        let mut any = false;
        for k in 0..(len / bs).min(64) {
            dev.read_at((loc + k) * bs, &mut b)?;
            match tag(&b) {
                Some(TAG_PD) if le32(&b, 184) != 1 => return Ok(false),
                Some(TAG_PD) => any = true,
                Some(TAG_TD) | None => break,
                _ => {}
            }
        }
        return Ok(any);
    }
    Ok(false)
}

impl UdfDriver {
    /// Opens a UDF volume read-only.
    pub(crate) fn open_fs(dev: Box<dyn BlockDevice>, log: &dix_core::log::MountLog) -> Result<Box<dyn FileSystem>> {
        Ok(Box::new(UdfFs { v: Volume::open(dev, log)? }))
    }

    /// Why a UDF volume cannot be changed here (None: it can). Changing
    /// writes the image anew as a finished disc, so only finished
    /// (read-only access) UDF-only images on a file that can grow and
    /// shrink are changed.
    fn why_read_only(&self, dev: &mut dyn BlockDevice) -> Result<Option<String>> {
        if !cfg!(feature = "iso9660") {
            return Ok(Some("this build cannot write UDF".into()));
        }
        if dev.is_read_only() {
            return Ok(Some("the image cannot be written".into()));
        }
        if has_iso9660(dev)? {
            return Ok(Some("it also holds ISO 9660: open it with the ISO 9660 driver to change both".into()));
        }
        if !dev.info().resizable {
            return Ok(Some("it is on a device that cannot grow or shrink".into()));
        }
        if !finished(dev)? {
            return Ok(Some(
                "it is a rewritable or write-once volume (with free space), which changing would turn into a \
                 finished disc"
                    .into(),
            ));
        }
        Ok(None)
    }
}

impl Driver for UdfDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: "udf",
            name: "UDF",
            description: if cfg!(feature = "iso9660") {
                "UDF 1.02–2.60 (DVD, Blu-ray, packet-written discs); UDF-only disc images can be made and changed"
            } else {
                "UDF 1.02–2.60 (DVD, Blu-ray, packet-written discs), read-only"
            },
            capabilities: DRIVER_CAPS,
        }
    }

    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
        if find(dev)?.is_none() {
            return Ok(0);
        }
        // On a bridge disc the ISO 9660 driver (90), which can also edit,
        // goes first; UDF can still be chosen.
        Ok(if has_iso9660(dev)? { 85 } else { 95 })
    }

    fn mount(&self, mut dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
        if opts.mode == AccessMode::ReadWrite {
            match self.why_read_only(dev.as_mut())? {
                None => {
                    #[cfg(feature = "iso9660")]
                    {
                        let fs = crate::iso9660::open_udf_for_editing(dev, &opts.log)?;
                        opts.log.info("Changes are kept until saved; then the whole image is written anew.");
                        return Ok(fs);
                    }
                }
                Some(why) => opts.log.info(format!("This UDF volume is opened read-only: {why}.")),
            }
        }
        Self::open_fs(dev, &opts.log)
    }

    fn format(&self, dev: &mut dyn BlockDevice, opts: &dix_core::FormatOptions) -> Result<()> {
        #[cfg(feature = "iso9660")]
        {
            crate::iso9660::format_image(dev, opts.label.as_deref().unwrap_or("UDF"), false, true)
        }
        #[cfg(not(feature = "iso9660"))]
        {
            let _ = (dev, opts);
            Err(Error::unsupported("making UDF images (this build has no ISO 9660 writer)"))
        }
    }

    fn format_variants(&self) -> &'static [dix_core::FormatVariant] {
        #[cfg(feature = "iso9660")]
        {
            VARIANTS
        }
        #[cfg(not(feature = "iso9660"))]
        {
            &[]
        }
    }
}

// ---------------------------------------------------------------------------
// Partitions.

enum Map {
    /// Type 1: blocks from `start` (a block of the medium).
    Physical { start: u64, len: u64 },
    /// Rewritable media: packets of `packet` blocks, some moved elsewhere
    /// (original block relative to the partition → block of the medium).
    Sparable { start: u64, len: u64, packet: u32, moved: Vec<(u32, u32)> },
    /// Write-once media: block n is `vat[n]` of the partition `base`.
    Virtual { base: usize, vat: Vec<u32> },
    /// UDF 2.50+: block n is block n of the metadata file, whose data is in
    /// partition `base` (runs: first block in the file, count, block in base).
    Metadata { base: usize, runs: Vec<(u64, u64, u32)> },
}

impl Map {
    fn kind(&self) -> &'static str {
        match self {
            Map::Physical { .. } => "plain",
            Map::Sparable { .. } => "sparable",
            Map::Virtual { .. } => "virtual (VAT)",
            Map::Metadata { .. } => "metadata",
        }
    }
}

/// Where a file's data is.
#[derive(Clone, Debug)]
enum Data {
    Embedded(Vec<u8>),
    /// Byte length; block address, None for unrecorded (zero) extents.
    Extents(Vec<(u64, Option<Addr>)>),
}

/// A file entry.
#[derive(Clone, Debug)]
struct Node {
    ftype: u8,
    size: u64,
    uid: u32,
    gid: u32,
    perms: u32,
    accessed: Option<i64>,
    modified: Option<i64>,
    created: Option<i64>,
    unique: u64,
    data: Data,
}

/// A directory's entry: a file identifier.
#[derive(Clone, Debug)]
struct Child {
    name: String,
    icb: Addr,
    hidden: bool,
}

struct Volume {
    dev: Box<dyn BlockDevice>,
    bs: u64,
    maps: Vec<Map>,
    root: Addr,
    label: String,
    volume_set: String,
    revision: u16,
    total: u64,
    free: Option<u64>,
    files: Option<(u32, u32)>,
    dirs: HashMap<Addr, Arc<Vec<Child>>>,
    nodes: HashMap<Addr, Arc<Node>>,
}

impl Volume {
    fn open(mut dev: Box<dyn BlockDevice>, log: &dix_core::log::MountLog) -> Result<Volume> {
        let bs = find(dev.as_mut())?.ok_or_else(|| damaged("no anchor volume descriptor"))?;
        let mut v = Volume {
            dev,
            bs,
            maps: Vec::new(),
            root: (0, 0),
            label: String::new(),
            volume_set: String::new(),
            revision: 0,
            total: 0,
            free: None,
            files: None,
            dirs: HashMap::new(),
            nodes: HashMap::new(),
        };
        // The anchors: block 256, then the last blocks.
        let blocks = v.dev.size() / bs;
        let mut anchor = None;
        for at in [256, blocks - 1, blocks.saturating_sub(257), 512] {
            if at >= 256 && at < blocks {
                let b = v.block(at)?;
                if tag(&b) == Some(TAG_AVDP) && le32(&b, 12) as u64 == at {
                    anchor = Some(b);
                    break;
                }
            }
        }
        let anchor = anchor.ok_or_else(|| damaged("no anchor volume descriptor"))?;
        let main = (le32(&anchor, 16) as u64, le32(&anchor, 20) as u64);
        let reserve = (le32(&anchor, 24) as u64, le32(&anchor, 28) as u64);
        let seq = match v.descriptors(main) {
            Ok(s) if s.lvd.is_some() => s,
            first => {
                log.warn("The main volume descriptor sequence cannot be read: using the reserve copy.");
                match v.descriptors(reserve) {
                    Ok(s) if s.lvd.is_some() => s,
                    _ => return Err(first.err().unwrap_or_else(|| damaged("no logical volume descriptor"))),
                }
            }
        };
        v.setup(seq, log)?;
        Ok(v)
    }

    /// Reads the volume descriptor sequence in `extent` (bytes, block),
    /// keeping the latest copy of each descriptor.
    fn descriptors(&mut self, extent: (u64, u64)) -> Result<Sequence> {
        let mut s = Sequence::default();
        let (mut len, mut at) = extent;
        let mut seen = 0;
        while len >= self.bs && seen < 4096 {
            seen += 1;
            let b = self.block(at)?;
            let Some(t) = tag(&b) else { break };
            let num = le32(&b, 16);
            let newer = |old: &Option<(u32, Vec<u8>)>| old.as_ref().is_none_or(|(n, _)| num >= *n);
            match t {
                TAG_PVD if newer(&s.pvd) => s.pvd = Some((num, b)),
                TAG_LVD if newer(&s.lvd) => s.lvd = Some((num, b)),
                TAG_PD => {
                    let number = le16(&b, 22);
                    let e = s.pds.entry(number).or_insert((0, Vec::new()));
                    if e.1.is_empty() || num >= e.0 {
                        *e = (num, b);
                    }
                }
                TAG_VDP => {
                    (len, at) = (le32(&b, 20) as u64, le32(&b, 24) as u64);
                    continue;
                }
                TAG_TD => break,
                _ => {}
            }
            len -= self.bs;
            at += 1;
        }
        Ok(s)
    }

    fn setup(&mut self, s: Sequence, log: &dix_core::log::MountLog) -> Result<()> {
        let (_, lvd) = s.lvd.ok_or_else(|| damaged("no logical volume descriptor"))?;
        let lbs = le32(&lvd, 212) as u64;
        if lbs != self.bs {
            return Err(Error::unsupported(format!("UDF with {lbs}-byte logical blocks on {}-byte sectors", self.bs)));
        }
        self.label = dstring(&lvd[84..212]);
        if let Some((_, pvd)) = &s.pvd {
            self.volume_set = dstring(&pvd[72..200]);
            if self.label.is_empty() {
                self.label = dstring(&pvd[24..56]);
            }
        }
        self.revision = le16(&lvd, 216 + 24);
        let fsd_at = long_ad(&lvd[248..264]).1;
        let integrity = (le32(&lvd, 432) as u64, le32(&lvd, 436) as u64);
        let table_len = le32(&lvd, 264) as usize;
        let count = le32(&lvd, 268) as usize;
        let table = lvd.get(440..440 + table_len).ok_or_else(|| damaged("partition maps"))?;
        let physical = |number: u16| -> Result<(u64, u64)> {
            let (_, pd) = s.pds.get(&number).ok_or_else(|| damaged(format!("no partition {number}")))?;
            Ok((le32(pd, 188) as u64, le32(pd, 192) as u64))
        };
        // The maps that need other maps (virtual, metadata) come after.
        let mut later = Vec::new();
        let mut pos = 0;
        for i in 0..count {
            let m = table.get(pos..).filter(|m| m.len() >= 2).ok_or_else(|| damaged("partition maps"))?;
            let (kind, len) = (m[0], m[1] as usize);
            let m = m.get(..len).filter(|_| len >= 6).ok_or_else(|| damaged("partition maps"))?;
            pos += len;
            match kind {
                1 => {
                    let (start, len) = physical(le16(m, 4))?;
                    self.maps.push(Map::Physical { start, len });
                }
                2 if len >= 64 => {
                    let number = le16(m, 38);
                    match regid(&m[4..36]) {
                        b"*UDF Sparable Partition" => {
                            let (start, len) = physical(number)?;
                            let packet = le16(m, 40) as u32;
                            let tables = (m[42] as usize).min(4);
                            let size = le32(m, 44) as u64;
                            let locs: Vec<u64> = (0..tables).map(|t| le32(m, 48 + 4 * t) as u64).collect();
                            let moved = self.sparing_table(&locs, size).unwrap_or_else(|| {
                                log.warn("The sparing table cannot be read: remapped packets are not followed.");
                                Vec::new()
                            });
                            self.maps.push(Map::Sparable { start, len, packet: packet.max(1), moved });
                        }
                        b"*UDF Virtual Partition" | b"*UDF Metadata Partition" => {
                            self.maps.push(Map::Physical { start: 0, len: 0 });
                            later.push((i, m.to_vec(), number));
                        }
                        other => {
                            return Err(Error::unsupported(format!(
                                "UDF partition map {:?}",
                                String::from_utf8_lossy(other)
                            )));
                        }
                    }
                }
                _ => return Err(damaged(format!("partition map type {kind}"))),
            }
        }
        for (i, m, number) in later {
            // The partition holding it: the plain or sparable map of the
            // same partition number.
            let (want, _) = physical(number)?;
            let base = (0..self.maps.len())
                .find(|&j| {
                    j != i
                        && matches!(self.maps[j], Map::Physical { start, len } | Map::Sparable { start, len, .. }
                            if start == want && len > 0)
                })
                .ok_or_else(|| damaged("no partition for a virtual or metadata partition map"))?;
            self.maps[i] = if regid(&m[4..36]) == b"*UDF Virtual Partition" {
                Map::Virtual { base, vat: self.vat(base, log)? }
            } else {
                Map::Metadata { base, runs: self.metadata_file(base, &m, log)? }
            };
        }
        for m in &self.maps {
            if let Map::Physical { len, .. } | Map::Sparable { len, .. } = m {
                self.total += len * self.bs;
            }
        }
        self.integrity(integrity);
        // Write-once media: what is free is after the recorded end (the
        // integrity descriptor is not updated as the disc is written).
        let recorded = self.dev.size() / self.bs;
        for m in &self.maps {
            if let Map::Virtual { base, .. } = m
                && let Map::Physical { start, len } | Map::Sparable { start, len, .. } = self.maps[*base]
            {
                self.free = Some((start + len).saturating_sub(recorded) * self.bs);
            }
        }
        // The file set descriptor: the root directory.
        let fsd = self.read_block(fsd_at)?;
        expect(&fsd, &[TAG_FSD], "file set descriptor")?;
        self.root = long_ad(&fsd[400..416]).1;
        if self.label.is_empty() {
            self.label = dstring(&fsd[112..240]);
        }
        Ok(())
    }

    /// The sparing table: (original, moved) packet starts.
    fn sparing_table(&mut self, locs: &[u64], size: u64) -> Option<Vec<(u32, u32)>> {
        for &at in locs {
            let n = size.div_ceil(self.bs).clamp(1, 64);
            let mut b = vec![0u8; (n * self.bs) as usize];
            if self.dev.read_at(at * self.bs, &mut b).is_err() || &b[16 + 1..16 + 19] != b"*UDF Sparing Table" {
                continue;
            }
            let entries = le16(&b, 48) as usize;
            let mut out = Vec::new();
            for e in 0..entries {
                let Some(e) = b.get(56 + 8 * e..64 + 8 * e) else { break };
                let original = le32(e, 0);
                if original < 0xffff_fff0 {
                    out.push((original, le32(e, 4)));
                }
            }
            return Some(out);
        }
        None
    }

    /// Finds the VAT at the end of the recorded area of partition `base`.
    fn vat(&mut self, base: usize, log: &dix_core::log::MountLog) -> Result<Vec<u32>> {
        let (Map::Physical { start, .. } | Map::Sparable { start, .. }) = self.maps[base] else {
            return Err(damaged("virtual partition"));
        };
        let last = self.dev.size() / self.bs;
        for back in 1..=64u64 {
            let Some(at) = last.checked_sub(back).filter(|&a| a >= start) else { break };
            let b = self.block(at)?;
            if !matches!(tag(&b), Some(TAG_FE | TAG_EFE)) {
                continue;
            }
            let lb = (at - start) as u32;
            let Ok(node) = self.parse_node(&b, (base as u16, lb)) else { continue };
            let data = self.read_all(&node, 64 << 20)?;
            let entries = if node.ftype == FT_VAT {
                // UDF 2.x: a header first.
                let header = le16(&data, 0) as usize;
                if header < 152 || header > data.len() {
                    continue;
                }
                let id = dstring(&data[4..132]);
                if !id.is_empty() {
                    self.label = id;
                }
                &data[header..]
            } else if node.ftype == 0
                && data.len() >= 36
                && regid(&data[data.len() - 36..]) == b"*UDF Virtual Alloc Tbl"
            {
                &data[..data.len() - 36]
            } else {
                continue;
            };
            return Ok(entries.chunks_exact(4).map(|c| le32(c, 0)).collect());
        }
        log.warn("No virtual allocation table found: the disc cannot be read.");
        Err(damaged("no virtual allocation table (VAT) at the end of the disc"))
    }

    /// The metadata file's runs (or its mirror's).
    fn metadata_file(&mut self, base: usize, m: &[u8], log: &dix_core::log::MountLog) -> Result<Vec<(u64, u64, u32)>> {
        let mut first = None;
        for (at, want, which) in [(le32(m, 40), FT_METADATA, "main"), (le32(m, 44), FT_METADATA_MIRROR, "mirror")] {
            let node = self.read_block((base as u16, at)).and_then(|b| self.parse_node(&b, (base as u16, at)));
            match node {
                Ok(n) if n.ftype == want => {
                    let Data::Extents(ext) = n.data else { return Err(damaged("embedded metadata file")) };
                    let mut runs = Vec::new();
                    let mut blk = 0;
                    for (len, addr) in ext {
                        let count = len.div_ceil(self.bs);
                        if let Some((_, lb)) = addr {
                            runs.push((blk, count, lb));
                        }
                        blk += count;
                    }
                    if which == "mirror" {
                        log.warn("The metadata file cannot be read: using its mirror.");
                    }
                    return Ok(runs);
                }
                Ok(_) => first = first.or(Some(damaged(format!("{which} metadata file has the wrong type")))),
                Err(e) => first = first.or(Some(e)),
            }
        }
        Err(first.unwrap_or_else(|| damaged("metadata file")))
    }

    /// Free space and file counts from the logical volume integrity
    /// descriptor (if it can be read).
    fn integrity(&mut self, mut extent: (u64, u64)) {
        let mut last = None;
        let mut hops = 0;
        while extent.0 >= self.bs && hops < 256 {
            hops += 1;
            let Ok(b) = self.block(extent.1) else { break };
            match tag(&b) {
                Some(TAG_LVID) => {
                    let next = (le32(&b, 16) as u64, le32(&b, 20) as u64);
                    last = Some(b);
                    if next.0 > 0 {
                        extent = next;
                        continue;
                    }
                    extent = (extent.0 - self.bs, extent.1 + 1);
                }
                _ => break,
            }
        }
        let Some(b) = last else { return };
        let n = le32(&b, 72) as usize;
        let iu = le32(&b, 76) as usize;
        if 80 + 8 * n > b.len() {
            return;
        }
        let mut free = None::<u64>;
        for (i, m) in self.maps.iter().enumerate().take(n) {
            let f = le32(&b, 80 + 4 * i);
            if f != u32::MAX && matches!(m, Map::Physical { .. } | Map::Sparable { .. }) {
                *free.get_or_insert(0) += f as u64 * self.bs;
            }
        }
        self.free = free;
        let iu_at = 80 + 8 * n;
        if iu >= 46 && iu_at + 46 <= b.len() {
            self.files = Some((le32(&b, iu_at + 32), le32(&b, iu_at + 36)));
            let min_read = le16(&b, iu_at + 40);
            if min_read > self.revision {
                self.revision = min_read;
            }
        }
    }

    // -- Blocks ---------------------------------------------------------------

    /// Block `at` of the medium.
    fn block(&mut self, at: u64) -> Result<Vec<u8>> {
        let mut b = vec![0u8; self.bs as usize];
        self.dev.read_at(at * self.bs, &mut b)?;
        Ok(b)
    }

    /// The block of the medium that (partition, block) means.
    fn locate(&self, (part, lb): Addr) -> Result<u64> {
        let map = self.maps.get(part as usize).ok_or_else(|| damaged(format!("partition reference {part}")))?;
        match map {
            Map::Physical { start, .. } => Ok(start + lb as u64),
            Map::Sparable { start, packet, moved, .. } => {
                let off = lb % packet;
                let first = lb - off;
                match moved.iter().find(|(o, _)| *o == first) {
                    Some((_, to)) => Ok(*to as u64 + off as u64),
                    None => Ok(start + lb as u64),
                }
            }
            Map::Virtual { base, vat } => match vat.get(lb as usize) {
                Some(&to) if to != u32::MAX => self.locate((*base as u16, to)),
                _ => Err(damaged(format!("block {lb} is not in the VAT"))),
            },
            Map::Metadata { base, runs } => {
                let lb = lb as u64;
                let (first, _, at) = runs
                    .iter()
                    .find(|(f, n, _)| lb >= *f && lb < f + n)
                    .ok_or_else(|| damaged(format!("metadata block {lb} is outside the metadata file")))?;
                self.locate((*base as u16, at + (lb - first) as u32))
            }
        }
    }

    fn read_block(&mut self, addr: Addr) -> Result<Vec<u8>> {
        let at = self.locate(addr)?;
        self.block(at)
    }

    /// Reads `buf.len()` bytes of the extent starting at `addr`, from byte
    /// `off` of it.
    fn read_extent(&mut self, (part, lb): Addr, off: u64, buf: &mut [u8]) -> Result<()> {
        // A plain partition is contiguous; otherwise block by block.
        if let Some(Map::Physical { start, .. }) = self.maps.get(part as usize) {
            return self.dev.read_at((start + lb as u64) * self.bs + off, buf);
        }
        let mut done = 0;
        while done < buf.len() {
            let pos = off + done as u64;
            let blk = lb as u64 + pos / self.bs;
            let within = pos % self.bs;
            let n = upto(self.bs - within, buf.len() - done);
            let at = self.locate((part, blk as u32))?;
            self.dev.read_at(at * self.bs + within, &mut buf[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    // -- File entries ---------------------------------------------------------

    fn node(&mut self, addr: Addr) -> Result<Arc<Node>> {
        if let Some(n) = self.nodes.get(&addr) {
            return Ok(n.clone());
        }
        let mut at = addr;
        let mut hops = 0;
        let node = loop {
            let b = self.read_block(at)?;
            // An indirect entry points to the real one.
            if tag(&b) == Some(TAG_IE) && hops < 16 {
                hops += 1;
                at = long_ad(&b[36..52]).1;
                continue;
            }
            break self.parse_node(&b, at)?;
        };
        let node = Arc::new(node);
        if self.nodes.len() >= CACHE {
            self.nodes.clear();
        }
        self.nodes.insert(addr, node.clone());
        Ok(node)
    }

    /// Parses a file entry or extended file entry in block `b`, which is at
    /// `addr` (short allocation descriptors are in its partition).
    fn parse_node(&mut self, b: &[u8], addr: Addr) -> Result<Node> {
        let t = expect(b, &[TAG_FE, TAG_EFE, TAG_TE], "file entry")?;
        if t == TAG_TE {
            return Err(damaged("terminal entry where a file entry was expected"));
        }
        let ext = t == TAG_EFE;
        let ftype = b[16 + 11];
        let flags = le16(b, 16 + 18);
        let (times, ea_len_at) = if ext { (80, 208) } else { (72, 168) };
        let ea_len = le32(b, ea_len_at) as usize;
        let ad_len = le32(b, ea_len_at + 4) as usize;
        let ad_at = ea_len_at + 8 + ea_len;
        let ads = b.get(ad_at..ad_at + ad_len).ok_or_else(|| damaged("file entry: allocation descriptors"))?;
        let size = le64(b, 56);
        let data = match flags & 7 {
            3 => Data::Embedded(ads.to_vec()),
            kind => Data::Extents(self.extents(ads, kind, addr.0)?),
        };
        let (created, unique) = if ext { (timestamp(&b[104..116]), le64(b, 200)) } else { (None, le64(b, 160)) };
        Ok(Node {
            ftype,
            size,
            uid: le32(b, 36),
            gid: le32(b, 40),
            perms: le32(b, 44),
            accessed: timestamp(&b[times..times + 12]),
            modified: timestamp(&b[times + 12..times + 24]),
            created,
            unique,
            data,
        })
    }

    /// The extents of allocation descriptors of `kind` (0 short, 1 long, 2
    /// extended), following allocation extent descriptors.
    fn extents(&mut self, ads: &[u8], kind: u16, part: u16) -> Result<Vec<(u64, Option<Addr>)>> {
        let step = match kind {
            0 => 8,
            1 => 16,
            2 => 20,
            _ => return Err(damaged(format!("allocation descriptor type {kind}"))),
        };
        let mut out = Vec::new();
        let mut buf = ads.to_vec();
        let mut hops = 0;
        'chain: loop {
            for d in buf.chunks_exact(step) {
                let raw = le32(d, 0);
                let (len, ty) = ((raw & 0x3fff_ffff) as u64, raw >> 30);
                if len == 0 {
                    break 'chain;
                }
                let addr = match kind {
                    0 => (part, le32(d, 4)),
                    1 => (le16(d, 8), le32(d, 4)),
                    _ => (le16(d, 16), le32(d, 12)),
                };
                if ty == 3 {
                    // The descriptors go on in an allocation extent descriptor.
                    hops += 1;
                    if hops > MAX_AED {
                        return Err(damaged("allocation descriptor chain loops"));
                    }
                    let mut b = vec![0u8; len.clamp(24, 1 << 20) as usize];
                    self.read_extent(addr, 0, &mut b)?;
                    expect(&b, &[TAG_AED], "allocation extent")?;
                    let n = (le32(&b, 20) as usize).min(b.len() - 24);
                    buf = b[24..24 + n].to_vec();
                    continue 'chain;
                }
                out.push((len, (ty == 0).then_some(addr)));
            }
            break;
        }
        Ok(out)
    }

    /// Reads from a file's data.
    fn read(&mut self, node: &Node, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= node.size {
            return Ok(0);
        }
        let want = upto(node.size - offset, buf.len());
        let buf = &mut buf[..want];
        match &node.data {
            Data::Embedded(d) => {
                let from = (offset as usize).min(d.len());
                let n = want.min(d.len() - from);
                buf[..n].copy_from_slice(&d[from..from + n]);
                buf[n..].fill(0);
            }
            Data::Extents(ext) => {
                let mut pos = 0u64;
                let mut done = 0;
                for &(len, addr) in ext {
                    if done == want {
                        break;
                    }
                    let at = offset + done as u64;
                    if at >= pos + len {
                        pos += len;
                        continue;
                    }
                    let within = at - pos;
                    let n = upto(len - within, want - done);
                    match addr {
                        Some(a) => self.read_extent(a, within, &mut buf[done..done + n])?,
                        None => buf[done..done + n].fill(0),
                    }
                    done += n;
                    pos += len;
                }
                buf[done..].fill(0);
            }
        }
        Ok(want)
    }

    fn read_all(&mut self, node: &Node, limit: u64) -> Result<Vec<u8>> {
        if node.size > limit {
            return Err(damaged(format!("{} bytes of metadata", node.size)));
        }
        let mut b = vec![0u8; node.size as usize];
        self.read(node, 0, &mut b)?;
        Ok(b)
    }

    // -- Directories ----------------------------------------------------------

    fn list(&mut self, addr: Addr) -> Result<Arc<Vec<Child>>> {
        if let Some(d) = self.dirs.get(&addr) {
            return Ok(d.clone());
        }
        let node = self.node(addr)?;
        if node.ftype != FT_DIR {
            return Err(Error::not_a_directory("UDF file"));
        }
        let data = self.read_all(&node, 256 << 20)?;
        let mut out = Vec::new();
        let mut pos = 0;
        while pos + 38 <= data.len() {
            let d = &data[pos..];
            if tag(d) != Some(TAG_FID) {
                // Padding to the end of a block.
                let next = (pos as u64 / self.bs + 1) * self.bs;
                if next as usize >= data.len() || tag(&data[next as usize..]) != Some(TAG_FID) {
                    break;
                }
                pos = next as usize;
                continue;
            }
            let chars = d[18];
            let name_len = d[19] as usize;
            let iu = le16(d, 36) as usize;
            let end = 38 + iu + name_len;
            if end > d.len() {
                break;
            }
            pos += (end + 3) & !3;
            if chars & (FID_DELETED | FID_PARENT) != 0 || name_len == 0 {
                continue;
            }
            let name = cs0(&d[38 + iu..end]).replace(['/', '\0'], "_");
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            out.push(Child { name, icb: long_ad(&d[20..36]).1, hidden: chars & FID_HIDDEN != 0 });
        }
        let out = Arc::new(out);
        if self.dirs.len() >= CACHE {
            self.dirs.clear();
        }
        self.dirs.insert(addr, out.clone());
        Ok(out)
    }

    /// The file entry at `path` (symbolic links are not followed).
    fn resolve(&mut self, path: &VfsPath) -> Result<(Addr, bool)> {
        let mut at = self.root;
        let mut hidden = false;
        for name in path.components() {
            let list = self.list(at)?;
            let c = list
                .iter()
                .find(|c| c.name == name)
                .or_else(|| list.iter().find(|c| c.name.eq_ignore_ascii_case(name)))
                .ok_or_else(|| Error::not_found(path))?;
            (at, hidden) = (c.icb, c.hidden);
        }
        Ok((at, hidden))
    }

    fn metadata(&mut self, node: &Node, hidden: bool) -> Metadata {
        let kind = match node.ftype {
            FT_DIR => EntryKind::Directory,
            FT_FILE | FT_REALTIME | 0 => EntryKind::File,
            FT_SYMLINK => EntryKind::Symlink,
            _ => EntryKind::Other,
        };
        let mut m = Metadata::new(kind, if kind == EntryKind::Directory { 0 } else { node.size });
        m.modified = node.modified;
        m.accessed = node.accessed;
        m.created = node.created;
        let p = node.perms;
        let bit = |udf: u32, unix: u32| if p & (1 << udf) != 0 { unix } else { 0 };
        let mut mode = bit(12, 0o400) | bit(11, 0o200) | bit(10, 0o100);
        mode |= bit(7, 0o040) | bit(6, 0o020) | bit(5, 0o010);
        mode |= bit(2, 0o004) | bit(1, 0o002) | bit(0, 0o001);
        mode |= match kind {
            EntryKind::Directory => 0o040000,
            EntryKind::Symlink => 0o120000,
            _ => 0o100000,
        };
        m.mode = Some(mode);
        m.uid = (node.uid != u32::MAX).then_some(node.uid);
        m.gid = (node.gid != u32::MAX).then_some(node.gid);
        m.inode = Some(node.unique);
        m.hidden = hidden;
        if kind == EntryKind::Symlink {
            m.symlink_target = self.read_all(node, 1 << 16).ok().map(|d| symlink(&d));
        }
        m
    }
}

/// A symbolic link's path components (ECMA-167 4/14.16).
fn symlink(d: &[u8]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut absolute = false;
    let mut pos = 0;
    while pos + 4 <= d.len() {
        let (ty, len) = (d[pos], d[pos + 1] as usize);
        let name = d.get(pos + 4..pos + 4 + len).unwrap_or_default();
        pos += 4 + len;
        match ty {
            1 | 2 => {
                absolute = true;
                parts.clear();
            }
            3 => parts.push("..".into()),
            4 => parts.push(".".into()),
            5 => parts.push(cs0(name)),
            _ => {}
        }
    }
    let path = parts.join("/");
    if absolute { format!("/{path}") } else { path }
}

#[derive(Default)]
struct Sequence {
    pvd: Option<(u32, Vec<u8>)>,
    lvd: Option<(u32, Vec<u8>)>,
    pds: HashMap<u16, (u32, Vec<u8>)>,
}

/// What a UDF volume holds (path → kind, size), or None if `dev` has no
/// UDF. The ISO 9660 driver uses it to tell whether a bridge image can be
/// written anew from its ISO 9660 tree without losing anything.
#[cfg(feature = "iso9660")]
pub(crate) fn tree(
    mut dev: Box<dyn BlockDevice>,
) -> Result<Option<std::collections::BTreeMap<String, (EntryKind, u64)>>> {
    if find(dev.as_mut())?.is_none() {
        return Ok(None);
    }
    let mut fs = UdfFs { v: Volume::open(dev, &dix_core::MountLog::new())? };
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![VfsPath::root()];
    while let Some(dir) = stack.pop() {
        for e in fs.read_dir(&dir)? {
            let p = dir.join(&e.name)?;
            if e.metadata.kind == EntryKind::Directory {
                stack.push(p.clone());
            }
            out.insert(p.as_str().to_owned(), (e.metadata.kind, e.metadata.size));
        }
    }
    Ok(Some(out))
}

// ---------------------------------------------------------------------------
// FileSystem.

pub struct UdfFs {
    v: Volume,
}

impl FileSystem for UdfFs {
    fn info(&self) -> FsInfo {
        let v = &self.v;
        let rev = format!("{:x}.{:02x}", v.revision >> 8, v.revision & 0xff);
        let mut details = vec![("Format".to_string(), format!("UDF {rev}"))];
        let kinds: Vec<&str> = v.maps.iter().map(Map::kind).collect();
        details.push(("Partition maps".into(), kinds.join(", ")));
        if !v.volume_set.is_empty() {
            details.push(("Volume set".into(), v.volume_set.clone()));
        }
        if let Some((files, dirs)) = v.files {
            details.push(("Files".into(), files.to_string()));
            details.push(("Folders".into(), dirs.to_string()));
        }
        FsInfo {
            driver_id: "udf".into(),
            fs_name: "UDF".into(),
            label: (!v.label.is_empty()).then(|| v.label.clone()),
            total_bytes: Some(v.total),
            free_bytes: v.free,
            block_size: Some(v.bs as u32),
            details,
        }
    }

    fn capabilities(&self) -> Capabilities {
        CAPS
    }

    fn access_mode(&self) -> AccessMode {
        AccessMode::ReadOnly
    }

    fn read_dir(&mut self, path: &VfsPath) -> Result<Vec<DirEntry>> {
        let (at, _) = self.v.resolve(path)?;
        if self.v.node(at)?.ftype != FT_DIR {
            return Err(Error::not_a_directory(path));
        }
        let list = self.v.list(at)?;
        let mut out = Vec::with_capacity(list.len());
        for c in list.iter() {
            let metadata = match self.v.node(c.icb) {
                Ok(n) => self.v.metadata(&n, c.hidden),
                // An unreadable entry is still listed.
                Err(_) => Metadata { hidden: c.hidden, ..Metadata::new(EntryKind::Other, 0) },
            };
            out.push(DirEntry { name: c.name.clone(), metadata });
        }
        Ok(out)
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        let (at, hidden) = self.v.resolve(path)?;
        let n = self.v.node(at)?;
        Ok(self.v.metadata(&n, hidden))
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let (at, _) = self.v.resolve(path)?;
        let n = self.v.node(at)?;
        if n.ftype == FT_DIR {
            return Err(Error::is_a_directory(path));
        }
        self.v.read(&n, offset, buf)
    }
}
