//! littlefs v2: the flash file system of MicroPython (Raspberry Pi Pico
//! and other boards), Zephyr, Mbed OS and many microcontrollers.
//!
//! The file system is made of **metadata pairs**: two blocks written in
//! turn, each a revision count and a log of commits. A commit is a run of
//! tags — 32 bits, big-endian, each XORed with the one before; type, id,
//! length — with their data, closed by a CRC tag. The newest valid commit
//! of the newer block counts. Replaying the log gives the entries of the
//! pair: a *create* inserts an id, a *delete* removes one, name and struct
//! tags describe an id. A directory is a chain of pairs joined by hard
//! tails; every pair is on one list of soft tails from the root, used to
//! sum the global *move state* (an entry that is being moved is hidden
//! where it was).
//!
//! The superblock is entry 0 of the root pair (blocks 0 and 1): the name
//! "littlefs" and the block size and count. A file's data is inline (in its
//! tag) or a **CTZ skip list**: blocks from the last back to the first,
//! block n starting with ctz(n)+1 pointers to earlier ones.
//!
//! In a whole flash dump (`picotool save -a`), the file system is not at
//! the start: it is looked for on 4 KiB boundaries. MicroPython's
//! modification times (user attribute 1: nanoseconds since 1970) are shown.

#[cfg(all(test, unix))]
mod tests;
mod write;

use dix_core::{
    AccessMode, BlockDevice, Capabilities, DirEntry, Driver, DriverInfo, EntryKind, Error, FileSystem, FsInfo,
    Metadata, MountOptions, ProbeScore, Result, VfsPath,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

const CAPS: Capabilities = Capabilities::READ.union(Capabilities::TIMESTAMPS);
const WRITE_CAPS: Capabilities = CAPS
    .union(Capabilities::WRITE)
    .union(Capabilities::CREATE_DIR)
    .union(Capabilities::REMOVE)
    .union(Capabilities::RENAME);
/// Larger file systems are not changed (the new image is made in memory).
const WRITE_MAX: u64 = 256 << 20;

const MAGIC: &[u8; 8] = b"littlefs";
/// Flash dumps up to this size are searched for a file system.
const SCAN_MAX: u64 = 256 << 20;
const SCAN_STEP: u64 = 4096;

// Tag types (type3).
const TYPE_REG: u16 = 0x001;
const TYPE_DIR: u16 = 0x002;
const TYPE_SUPERBLOCK: u16 = 0x0ff;
const TYPE_DIRSTRUCT: u16 = 0x200;
const TYPE_INLINESTRUCT: u16 = 0x201;
const TYPE_CTZSTRUCT: u16 = 0x202;
const TYPE_MOVESTATE: u16 = 0x7ff;
/// MicroPython's modification time attribute (0x300 | 1).
const ATTR_MTIME: u16 = 0x301;

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn damaged(what: impl std::fmt::Display) -> Error {
    Error::corrupted(format!("littlefs: {what}"))
}

/// littlefs's CRC-32 (reflected 0x04c11db7, no final inversion).
pub(crate) fn crc32(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    crc
}

// Tag fields.
fn valid(t: u32) -> bool {
    t & 0x8000_0000 == 0
}
fn type1(t: u32) -> u16 {
    ((t & 0x7000_0000) >> 20) as u16
}
fn type2(t: u32) -> u16 {
    ((t & 0x7800_0000) >> 20) as u16
}
fn type3(t: u32) -> u16 {
    ((t & 0x7ff0_0000) >> 20) as u16
}
fn chunk(t: u32) -> u8 {
    ((t & 0x0ff0_0000) >> 20) as u8
}
fn id(t: u32) -> u16 {
    ((t & 0x000f_fc00) >> 10) as u16
}
fn size(t: u32) -> u32 {
    t & 0x3ff
}
fn deleted(t: u32) -> bool {
    size(t) == 0x3ff
}
/// Bytes the tag takes with its data.
fn dsize(t: u32) -> u32 {
    4 + if deleted(t) { 0 } else { size(t) }
}

/// Where the file system is in the device and its geometry.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Geometry {
    pub offset: u64,
    pub block_size: u32,
    pub block_count: u32,
}

/// The superblock found at `offset` (block 0 or 1 holds it).
fn superblock_at(dev: &mut dyn BlockDevice, offset: u64) -> Result<Option<(Geometry, u32, u32)>> {
    let mut head = [0u8; 16];
    for guess in [0u64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536] {
        let at = offset + guess;
        if at + 64 > dev.size() {
            break;
        }
        dev.read_at(at, &mut head)?;
        // The superblock's name is the first tag of the block: "littlefs"
        // right after the revision and the tag.
        if &head[8..16] != MAGIC {
            continue;
        }
        let t1 = be32(&head, 4) ^ 0xffff_ffff;
        if type3(t1) != TYPE_SUPERBLOCK || size(t1) != 8 || id(t1) != 0 {
            continue;
        }
        // Its geometry: entry 0's inline struct, further in the first
        // commit (after the other names when littlefs compacted it).
        let len = (dev.size() - at).min(65536) as usize;
        let mut b = vec![0u8; len];
        dev.read_at(at, &mut b)?;
        let (mut off, mut ptag) = (4usize, 0xffff_ffffu32);
        let mut geo = None;
        while off + 4 <= b.len() {
            let tag = be32(&b, off) ^ ptag;
            if !valid(tag) || off + dsize(tag) as usize > b.len() {
                break;
            }
            ptag = tag;
            if type3(tag) == TYPE_INLINESTRUCT && id(tag) == 0 && size(tag) >= 24 {
                let d = &b[off + 4..];
                geo = Some((le32(d, 0), le32(d, 4), le32(d, 8), le32(d, 12)));
                break;
            }
            if type2(tag) == 0x500 {
                break;
            }
            off += dsize(tag) as usize;
        }
        let Some((version, block_size, block_count, name_max)) = geo else { continue };
        if !(128..=1 << 20).contains(&block_size) || block_count < 2 {
            continue;
        }
        // In block 0, or in block 1 (block 0 being rewritten): either way
        // the file system starts at `offset`.
        if guess != 0 && guess != block_size as u64 {
            continue;
        }
        return Ok(Some((Geometry { offset, block_size, block_count }, version, name_max)));
    }
    Ok(None)
}

/// Finds the file system: at the start of `dev`, or (a flash dump) on a
/// 4 KiB boundary. The bool: found by searching.
pub(crate) fn find(dev: &mut dyn BlockDevice) -> Result<Option<(Geometry, u32, u32, bool)>> {
    if dev.size() < 256 {
        return Ok(None);
    }
    if let Some((g, v, n)) = superblock_at(dev, 0)? {
        return Ok(Some((g, v, n, false)));
    }
    if dev.size() > SCAN_MAX {
        return Ok(None);
    }
    let mut b = [0u8; 16];
    let mut at = SCAN_STEP;
    while at + 64 <= dev.size() {
        dev.read_at(at, &mut b)?;
        if &b[8..16] == MAGIC {
            // This block, or the one before it, is block 0.
            for start in [at, at.saturating_sub(SCAN_STEP)] {
                if let Some((g, v, n)) = superblock_at(dev, start)? {
                    let end = g.offset + g.block_size as u64 * g.block_count as u64;
                    if end <= dev.size() {
                        return Ok(Some((g, v, n, true)));
                    }
                }
            }
        }
        at += SCAN_STEP;
    }
    Ok(None)
}

pub struct LittleFsDriver;

impl Driver for LittleFsDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: "littlefs",
            name: "littlefs",
            description: "littlefs v2 (MicroPython on Raspberry Pi Pico, microcontrollers; also in flash dumps): \
                          read, change, create",
            capabilities: WRITE_CAPS.union(Capabilities::FORMAT),
        }
    }

    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
        Ok(match find(dev)? {
            Some((_, _, _, false)) => 95,
            // Inside a flash dump: anything recognised at the start wins.
            Some((_, _, _, true)) => 40,
            None => 0,
        })
    }

    fn mount(&self, dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
        let writable = opts.mode == AccessMode::ReadWrite && !dev.is_read_only();
        let v = Volume::open(dev, &opts.log)?;
        let mut fs = LittleFs { v, edit: None };
        if writable {
            let bytes = fs.v.geo.block_size as u64 * fs.v.geo.block_count as u64;
            if bytes > WRITE_MAX {
                return Err(Error::new(
                    dix_core::ErrorKind::ReadOnly,
                    "this littlefs volume is too large to change here (over 256 MiB): open it read-only",
                ));
            }
            fs.start_editing()?;
            opts.log.info("Changes are kept until saved; then the file system is written anew, compacted.");
        }
        Ok(Box::new(fs))
    }

    fn format(&self, dev: &mut dyn BlockDevice, _opts: &dix_core::FormatOptions) -> Result<()> {
        // The Pico's flash blocks; small images get smaller ones.
        let bs: u32 = if dev.size() >= 64 * 4096 { 4096 } else { 512 };
        let count = (dev.size() / bs as u64).min(u32::MAX as u64) as u32;
        if count < 8 {
            return Err(Error::new(dix_core::ErrorKind::NoSpace, "a littlefs volume needs at least 8 blocks"));
        }
        let root =
            write::WNode { name: Vec::new(), dir: true, data: Vec::new(), attrs: Vec::new(), children: Vec::new() };
        let img = write::build(&[root], bs, count, &DEFAULT_LIMITS)?;
        dev.write_at(0, &img)?;
        dev.flush()
    }

    fn min_format_size(&self) -> u64 {
        8 * 512
    }
}

/// An entry of a metadata pair, after replaying its log.
#[derive(Clone, Debug, Default)]
pub(crate) struct Slot {
    pub kind: u16,
    pub name: Vec<u8>,
    pub structure: Option<(u16, Vec<u8>)>,
    pub attrs: Vec<(u16, Vec<u8>)>,
}

/// A metadata pair, read.
#[derive(Clone, Debug, Default)]
pub(crate) struct Pair {
    pub slots: Vec<Slot>,
    /// The tail: (pair, hard: same directory).
    pub tail: Option<([u32; 2], bool)>,
    /// Its move state (the latest tag: its part of the global state).
    pub gstate: [u8; 12],
}

/// A file's data.
#[derive(Clone, Debug)]
enum Data {
    Inline(Vec<u8>),
    Ctz { head: u32, size: u32 },
}

#[derive(Clone, Debug)]
struct Node {
    dir: bool,
    /// Directories: their first pair.
    pair: [u32; 2],
    data: Data,
    mtime: Option<i64>,
    size: u64,
    attrs: Vec<(u16, Vec<u8>)>,
}

const DEFAULT_LIMITS: write::Limits =
    write::Limits { name_max: 255, file_max: 0x7fff_ffff, attr_max: 1022, inline_max: write::INLINE_MIN };

pub(crate) struct Volume {
    dev: Box<dyn BlockDevice>,
    pub geo: Geometry,
    version: u32,
    name_max: u32,
    limits: write::Limits,
    found_inside: bool,
    /// The pending move: (pair, id) hidden there.
    moving: Option<([u32; 2], u16)>,
    pairs: HashMap<[u32; 2], Arc<Pair>>,
    dirs: HashMap<[u32; 2], Arc<Vec<(String, Node)>>>,
    used_blocks: Option<u64>,
}

impl Volume {
    pub(crate) fn open(mut dev: Box<dyn BlockDevice>, log: &dix_core::log::MountLog) -> Result<Volume> {
        let (geo, version, name_max, inside) = find(dev.as_mut())?.ok_or_else(|| damaged("no superblock"))?;
        let major = version >> 16;
        if major != 2 {
            return Err(Error::unsupported(format!("littlefs {major}.{} (only version 2 is read)", version & 0xffff)));
        }
        if inside {
            log.info(format!(
                "littlefs found at offset {:#x} of the image ({} blocks of {} bytes).",
                geo.offset, geo.block_count, geo.block_size
            ));
        }
        let mut v = Volume {
            dev,
            geo,
            version,
            name_max,
            limits: write::Limits { name_max, file_max: 0x7fff_ffff, attr_max: 1022, inline_max: write::INLINE_MIN },
            found_inside: inside,
            moving: None,
            pairs: HashMap::new(),
            dirs: HashMap::new(),
            used_blocks: None,
        };
        // The global state: the XOR of every pair's move state, along the
        // tail list from the root.
        let mut g = [0u8; 12];
        let mut at = [0u32, 1];
        let mut seen = std::collections::HashSet::new();
        while seen.insert(at) {
            let p = v.pair(at)?;
            for (x, y) in g.iter_mut().zip(p.gstate) {
                *x ^= y;
            }
            match p.tail {
                Some((t, _)) if t[0] != u32::MAX => at = t,
                _ => break,
            }
            if seen.len() > geo.block_count as usize {
                return Err(damaged("the list of metadata pairs loops"));
            }
        }
        // Free space for the volume's properties (cheap on flash sizes).
        if geo.block_size as u64 * geo.block_count as u64 <= WRITE_MAX {
            let _ = v.used();
        }
        let tag = le32(&g, 0);
        if type1(tag) != 0 {
            v.moving = Some(([le32(&g, 4), le32(&g, 8)], id(tag)));
            log.warn("A move was interrupted: the moved entry is shown where it went.");
        }
        Ok(v)
    }

    fn read(&mut self, block: u32, off: u32, buf: &mut [u8]) -> Result<()> {
        if block >= self.geo.block_count || off as usize + buf.len() > self.geo.block_size as usize {
            return Err(damaged(format!("block {block} is outside the file system")));
        }
        let at = self.geo.offset + block as u64 * self.geo.block_size as u64 + off as u64;
        self.dev.read_at(at, buf)
    }

    fn block(&mut self, block: u32) -> Result<Vec<u8>> {
        let mut b = vec![0u8; self.geo.block_size as usize];
        self.read(block, 0, &mut b)?;
        Ok(b)
    }

    /// Reads the metadata pair `p` (the newer of its blocks that has a
    /// valid commit).
    pub(crate) fn pair(&mut self, p: [u32; 2]) -> Result<Arc<Pair>> {
        if let Some(x) = self.pairs.get(&p) {
            return Ok(x.clone());
        }
        let blocks = [self.block(p[0])?, self.block(p[1])?];
        let revs = [le32(&blocks[0], 0), le32(&blocks[1], 0)];
        // The newer first (revisions compare as sequence numbers).
        let order = if (revs[1].wrapping_sub(revs[0]) as i32) > 0 { [1, 0] } else { [0, 1] };
        let mut found = None;
        for i in order {
            if let Some(x) = parse_pair(&blocks[i])? {
                found = Some(x);
                break;
            }
        }
        let pair = Arc::new(found.ok_or_else(|| damaged(format!("no valid commit in pair {}/{}", p[0], p[1])))?);
        self.pairs.insert(p, pair.clone());
        Ok(pair)
    }

    /// The entries of the directory starting at pair `p`.
    fn list(&mut self, p: [u32; 2]) -> Result<Arc<Vec<(String, Node)>>> {
        if let Some(d) = self.dirs.get(&p) {
            return Ok(d.clone());
        }
        let mut out = Vec::new();
        let mut at = p;
        let mut hops = 0;
        loop {
            let pair = self.pair(at)?;
            for (i, s) in pair.slots.iter().enumerate() {
                if self.moving == Some((at, i as u16)) {
                    continue;
                }
                let dir = match s.kind {
                    TYPE_REG => false,
                    TYPE_DIR => true,
                    _ => continue,
                };
                let name = String::from_utf8_lossy(&s.name).replace('/', "_");
                let mtime = s
                    .attrs
                    .iter()
                    .find(|(t, d)| *t == ATTR_MTIME && d.len() == 8)
                    .map(|(_, d)| (u64::from_le_bytes(d[..8].try_into().unwrap()) / 1_000_000_000) as i64);
                let (pair, data, size) = match &s.structure {
                    Some((TYPE_DIRSTRUCT, d)) if d.len() >= 8 => {
                        ([le32(d, 0), le32(d, 4)], Data::Inline(Vec::new()), 0)
                    }
                    Some((TYPE_INLINESTRUCT, d)) => ([0, 0], Data::Inline(d.clone()), d.len() as u64),
                    Some((TYPE_CTZSTRUCT, d)) if d.len() >= 8 => {
                        let (head, size) = (le32(d, 0), le32(d, 4));
                        ([0, 0], Data::Ctz { head, size }, size as u64)
                    }
                    None if !dir => ([0, 0], Data::Inline(Vec::new()), 0),
                    _ => continue,
                };
                out.push((name, Node { dir, pair, data, mtime, size, attrs: s.attrs.clone() }));
            }
            match pair.tail {
                Some((t, true)) if t[0] != u32::MAX => {
                    hops += 1;
                    if hops > self.geo.block_count {
                        return Err(damaged("a directory loops"));
                    }
                    at = t;
                }
                _ => break,
            }
        }
        let out = Arc::new(out);
        self.dirs.insert(p, out.clone());
        Ok(out)
    }

    fn resolve(&mut self, path: &VfsPath) -> Result<Node> {
        let mut node =
            Node { dir: true, pair: [0, 1], data: Data::Inline(Vec::new()), mtime: None, size: 0, attrs: Vec::new() };
        for name in path.components() {
            if !node.dir {
                return Err(Error::not_a_directory(path));
            }
            let list = self.list(node.pair)?;
            node = list.iter().find(|(n, _)| n == name).ok_or_else(|| Error::not_found(path))?.1.clone();
        }
        Ok(node)
    }

    // -- CTZ skip lists -------------------------------------------------------------

    /// The index of the block holding byte `off` of a file, and the offset
    /// in that block's data (after its pointers).
    fn ctz_index(&self, off: u64) -> (u64, u64) {
        let b = self.geo.block_size as u64 - 8;
        let i = off / b;
        if i == 0 {
            return (0, off);
        }
        let i = (off - 4 * ((i - 1).count_ones() as u64 + 2)) / b;
        (i, off - b * i - 4 * i.count_ones() as u64)
    }

    /// The block and in-block offset of byte `pos` of a CTZ file.
    fn ctz_find(&mut self, head: u32, size: u32, pos: u64) -> Result<(u32, u32)> {
        let (mut current, _) = self.ctz_index(size as u64 - 1);
        let (target, off) = self.ctz_index(pos);
        let mut block = head;
        let mut hops = 0;
        while current > target {
            let skip = (64 - (current - target + 1 - 1).leading_zeros() - 1).min(current.trailing_zeros()) as u64;
            let mut p = [0u8; 4];
            self.read(block, 4 * skip as u32, &mut p)?;
            block = u32::from_le_bytes(p);
            current -= 1 << skip;
            hops += 1;
            if hops > 64 * 64 {
                return Err(damaged("a file's block list loops"));
            }
        }
        // The offset counts the block's pointers.
        Ok((block, off as u32))
    }

    fn read_file(&mut self, n: &Node, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= n.size {
            return Ok(0);
        }
        let want = (n.size - offset).min(buf.len() as u64) as usize;
        match &n.data {
            Data::Inline(d) => buf[..want].copy_from_slice(&d[offset as usize..offset as usize + want]),
            Data::Ctz { head, size } => {
                let mut done = 0;
                while done < want {
                    let (block, off) = self.ctz_find(*head, *size, offset + done as u64)?;
                    let k = ((self.geo.block_size - off) as usize).min(want - done);
                    let mut chunk = vec![0u8; k];
                    self.read(block, off, &mut chunk)?;
                    buf[done..done + k].copy_from_slice(&chunk);
                    done += k;
                }
            }
        }
        Ok(want)
    }

    /// Blocks in use: every metadata pair and every file's blocks.
    fn used(&mut self) -> Result<u64> {
        if let Some(u) = self.used_blocks {
            return Ok(u);
        }
        let mut used = std::collections::HashSet::new();
        let mut at = [0u32, 1];
        while used.insert(at[0]) {
            used.insert(at[1]);
            let p = self.pair(at)?;
            for s in &p.slots {
                if let Some((TYPE_CTZSTRUCT, d)) = &s.structure
                    && d.len() >= 8
                {
                    let (head, size) = (le32(d, 0), le32(d, 4));
                    if size == 0 {
                        continue;
                    }
                    // Walk back from the head along the first pointers.
                    let (last, _) = self.ctz_index(size as u64 - 1);
                    let mut block = head;
                    for i in (0..=last).rev() {
                        used.insert(block);
                        if i > 0 {
                            let mut b = [0u8; 4];
                            self.read(block, 0, &mut b)?;
                            block = u32::from_le_bytes(b);
                        }
                    }
                }
            }
            match p.tail {
                Some((t, _)) if t[0] != u32::MAX => at = t,
                _ => break,
            }
        }
        let n = used.len() as u64;
        self.used_blocks = Some(n);
        Ok(n)
    }
}

/// Replays the log of one block of a metadata pair; None if it has no
/// valid commit.
pub(crate) fn parse_pair(b: &[u8]) -> Result<Option<Pair>> {
    let bs = b.len() as u32;
    let mut off = 4u32;
    let mut ptag = 0xffff_ffffu32;
    let mut crc = crc32(0xffff_ffff, &b[..4]);
    // Tags of the commit being read, applied when its CRC checks out.
    let mut pending: Vec<(u32, u32)> = Vec::new();
    let mut committed: Vec<(u32, u32)> = Vec::new();
    let mut any = false;
    loop {
        if off + 4 > bs {
            break;
        }
        let raw = &b[off as usize..off as usize + 4];
        let tag = be32(raw, 0) ^ ptag;
        if !valid(tag) || off + dsize(tag) > bs {
            break;
        }
        crc = crc32(crc, raw);
        ptag = tag;
        if type2(tag) == 0x500 {
            // A commit's CRC.
            if size(tag) < 4 {
                break;
            }
            let dcrc = le32(b, off as usize + 4);
            if crc != dcrc {
                break;
            }
            ptag ^= ((chunk(tag) & 1) as u32) << 31;
            committed.append(&mut pending);
            any = true;
            crc = 0xffff_ffff;
            off += dsize(tag);
            continue;
        }
        let data_end = off + dsize(tag);
        crc = crc32(crc, &b[off as usize + 4..data_end as usize]);
        pending.push((tag, off + 4));
        off = data_end;
    }
    if !any {
        return Ok(None);
    }
    let mut p = Pair::default();
    for (tag, at) in committed {
        let data = || {
            let n = if deleted(tag) { 0 } else { size(tag) as usize };
            b[at as usize..at as usize + n].to_vec()
        };
        let i = id(tag) as usize;
        let ensure = |slots: &mut Vec<Slot>, i: usize| {
            if slots.len() <= i {
                slots.resize(i + 1, Slot::default());
            }
        };
        match type1(tag) {
            0x000 => {
                if i == 0x3ff {
                    continue;
                }
                ensure(&mut p.slots, i);
                if deleted(tag) {
                    p.slots[i].kind = 0;
                } else {
                    p.slots[i].kind = type3(tag);
                    p.slots[i].name = data();
                }
            }
            0x200 => {
                if i == 0x3ff {
                    continue;
                }
                ensure(&mut p.slots, i);
                p.slots[i].structure = if deleted(tag) { None } else { Some((type3(tag), data())) };
            }
            0x300 => {
                if i == 0x3ff {
                    continue;
                }
                ensure(&mut p.slots, i);
                let t = type3(tag);
                p.slots[i].attrs.retain(|(x, _)| *x != t);
                if !deleted(tag) {
                    p.slots[i].attrs.push((t, data()));
                }
            }
            0x400 => {
                let n = chunk(tag) as i8;
                if n > 0 {
                    if i <= p.slots.len() {
                        p.slots.insert(i, Slot::default());
                    } else {
                        ensure(&mut p.slots, i);
                    }
                } else if n < 0 && i < p.slots.len() {
                    p.slots.remove(i);
                }
            }
            0x600 => {
                let d = data();
                if d.len() >= 8 {
                    p.tail = Some(([le32(&d, 0), le32(&d, 4)], chunk(tag) & 1 == 1));
                }
            }
            0x700 if type3(tag) == TYPE_MOVESTATE => {
                let d = data();
                p.gstate = [0; 12];
                for (x, y) in p.gstate.iter_mut().zip(d.iter()) {
                    *x = *y;
                }
            }
            _ => {}
        }
    }
    Ok(Some(p))
}

pub struct LittleFs {
    v: Volume,
    /// Opened read-write: the tree being changed.
    edit: Option<Edit>,
}

/// MicroPython's modification time attribute, for now.
fn now_attr() -> (u16, Vec<u8>) {
    let ns = dix_core::time::unix_now_ns();
    (ATTR_MTIME, ns.to_le_bytes().to_vec())
}

/// A member of the tree being changed.
#[derive(Clone, Debug)]
struct ENode {
    dir: bool,
    /// None: the data as on the volume (`old`); Some: new.
    data: Option<Vec<u8>>,
    old: Option<Node>,
    size: u64,
    attrs: Vec<(u16, Vec<u8>)>,
}

impl ENode {
    fn metadata(&self) -> Metadata {
        let mut m = Metadata::new(if self.dir { EntryKind::Directory } else { EntryKind::File }, self.size);
        m.modified = self
            .attrs
            .iter()
            .find(|(t, d)| *t == ATTR_MTIME && d.len() == 8)
            .map(|(_, d)| (u64::from_le_bytes(d[..8].try_into().unwrap()) / 1_000_000_000) as i64);
        m
    }
}

struct Edit {
    nodes: BTreeMap<String, ENode>,
    dirty: bool,
}

fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

impl Edit {
    fn get(&self, path: &VfsPath) -> Result<&ENode> {
        self.nodes.get(path.as_str()).ok_or_else(|| Error::not_found(path))
    }

    fn children<'a>(&'a self, dir: &'a str) -> impl Iterator<Item = (&'a str, &'a ENode)> + 'a {
        let prefix = if dir == "/" { "/".to_owned() } else { format!("{dir}/") };
        let len = prefix.len();
        self.nodes
            .range(prefix.clone()..)
            .take_while(move |(p, _)| p.starts_with(&prefix))
            .filter(move |(p, _)| p.len() > len && !p[len..].contains('/'))
            .map(move |(p, n)| (&p[len..], n))
    }

    fn parent_dir(&self, path: &VfsPath) -> Result<()> {
        if path.is_root() {
            return Err(Error::invalid_input("the root"));
        }
        match self.nodes.get(parent_of(path.as_str())) {
            Some(n) if n.dir => Ok(()),
            Some(_) => Err(Error::not_a_directory(parent_of(path.as_str()))),
            None => Err(Error::not_found(parent_of(path.as_str()))),
        }
    }
}

impl LittleFs {
    /// Reads the whole tree for changing.
    fn start_editing(&mut self) -> Result<()> {
        let mut nodes = BTreeMap::new();
        nodes.insert("/".to_owned(), ENode { dir: true, data: None, old: None, size: 0, attrs: Vec::new() });
        let mut stack = vec![("/".to_owned(), [0u32, 1])];
        while let Some((path, pair)) = stack.pop() {
            for (name, n) in self.v.list(pair)?.iter() {
                let p = if path == "/" { format!("/{name}") } else { format!("{path}/{name}") };
                if n.dir {
                    stack.push((p.clone(), n.pair));
                }
                if let Data::Inline(d) = &n.data {
                    self.v.limits.inline_max = self.v.limits.inline_max.max(d.len());
                }
                let e = ENode { dir: n.dir, data: None, old: Some(n.clone()), size: n.size, attrs: n.attrs.clone() };
                nodes.insert(p, e);
            }
        }
        self.edit = Some(Edit { nodes, dirty: false });
        Ok(())
    }

    fn edit(&mut self) -> &mut Edit {
        self.edit.as_mut().expect("editing")
    }

    fn e_read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let n = self.edit.as_ref().unwrap().get(path)?.clone();
        if n.dir {
            return Err(Error::is_a_directory(path));
        }
        match (&n.data, &n.old) {
            (Some(d), _) => {
                let from = (offset as usize).min(d.len());
                let k = buf.len().min(d.len() - from);
                buf[..k].copy_from_slice(&d[from..from + k]);
                Ok(k)
            }
            (None, Some(old)) => self.v.read_file(old, offset, buf),
            (None, None) => Ok(0),
        }
    }

    /// The whole content of a file being changed.
    fn e_content(&mut self, n: &ENode) -> Result<Vec<u8>> {
        match (&n.data, &n.old) {
            (Some(d), _) => Ok(d.clone()),
            (None, Some(old)) => {
                let mut b = vec![0u8; old.size as usize];
                self.v.read_file(old, 0, &mut b)?;
                Ok(b)
            }
            (None, None) => Ok(Vec::new()),
        }
    }

    fn e_put(&mut self, path: &VfsPath, data: Vec<u8>) -> Result<()> {
        let ed = self.edit();
        ed.parent_dir(path)?;
        let mut attrs = match ed.nodes.get(path.as_str()) {
            Some(n) if n.dir => return Err(Error::is_a_directory(path)),
            Some(n) => n.attrs.clone(),
            None => Vec::new(),
        };
        attrs.retain(|(t, _)| *t != ATTR_MTIME);
        attrs.push(now_attr());
        let size = data.len() as u64;
        ed.nodes.insert(path.as_str().to_owned(), ENode { dir: false, data: Some(data), old: None, size, attrs });
        ed.dirty = true;
        Ok(())
    }

    /// Writes the file system anew (see `write`), then reads it again.
    fn save(&mut self) -> Result<()> {
        let Some(ed) = self.edit.as_ref() else { return Ok(()) };
        if !ed.dirty {
            return Ok(());
        }
        let entries: Vec<(String, ENode)> = ed.nodes.iter().map(|(p, n)| (p.clone(), n.clone())).collect();
        let mut nodes: Vec<write::WNode> = Vec::with_capacity(entries.len());
        let mut index: HashMap<String, usize> = HashMap::new();
        for (path, n) in &entries {
            let i = nodes.len();
            index.insert(path.clone(), i);
            let data = if n.dir { Vec::new() } else { self.e_content(n)? };
            let name = path.rsplit('/').next().unwrap_or("").as_bytes().to_vec();
            nodes.push(write::WNode { name, dir: n.dir, data, attrs: n.attrs.clone(), children: Vec::new() });
            if path != "/" {
                let p = index[parent_of(path)];
                nodes[p].children.push(i);
            }
        }
        let geo = self.v.geo;
        let img = write::build(&nodes, geo.block_size, geo.block_count, &self.v.limits)?;
        self.v.dev.write_at(geo.offset, &img)?;
        self.v.dev.flush()?;
        // Read it again (the caches hold the old one).
        let dev = std::mem::replace(&mut self.v.dev, Box::new(dix_core::device::MemoryDevice::new("gone", Vec::new())));
        self.v = Volume::open(dev, &dix_core::MountLog::new())?;
        self.start_editing()
    }
}

impl Drop for LittleFs {
    /// Last chance to write changes when dropped without unmounting.
    fn drop(&mut self) {
        if self.edit.as_ref().is_some_and(|e| e.dirty) {
            let _ = self.save();
        }
    }
}

impl FileSystem for LittleFs {
    fn info(&self) -> FsInfo {
        let v = &self.v;
        let total = v.geo.block_size as u64 * v.geo.block_count as u64;
        let mut details = vec![
            ("Format".to_string(), format!("littlefs {}.{}", v.version >> 16, v.version & 0xffff)),
            ("Blocks".into(), format!("{} of {} bytes", v.geo.block_count, v.geo.block_size)),
            ("Longest name".into(), format!("{} bytes", v.name_max)),
        ];
        if v.found_inside {
            details.push(("Offset in the image".into(), format!("{:#x}", v.geo.offset)));
        }
        FsInfo {
            driver_id: "littlefs".into(),
            fs_name: "littlefs".into(),
            label: None,
            total_bytes: Some(total),
            free_bytes: v.used_blocks.map(|u| total.saturating_sub(u * v.geo.block_size as u64)),
            block_size: Some(v.geo.block_size),
            details,
        }
    }

    fn capabilities(&self) -> Capabilities {
        if self.edit.is_some() { WRITE_CAPS } else { CAPS }
    }

    fn access_mode(&self) -> AccessMode {
        if self.edit.is_some() { AccessMode::ReadWrite } else { AccessMode::ReadOnly }
    }

    fn read_dir(&mut self, path: &VfsPath) -> Result<Vec<DirEntry>> {
        if self.v.used_blocks.is_none() {
            let _ = self.v.used();
        }
        if let Some(ed) = &self.edit {
            if !ed.get(path)?.dir {
                return Err(Error::not_a_directory(path));
            }
            return Ok(ed
                .children(path.as_str())
                .map(|(name, n)| DirEntry { name: name.to_owned(), metadata: n.metadata() })
                .collect());
        }
        let n = self.v.resolve(path)?;
        if !n.dir {
            return Err(Error::not_a_directory(path));
        }
        let list = self.v.list(n.pair)?;
        Ok(list
            .iter()
            .map(|(name, n)| {
                let mut m = Metadata::new(if n.dir { EntryKind::Directory } else { EntryKind::File }, n.size);
                m.modified = n.mtime;
                DirEntry { name: name.clone(), metadata: m }
            })
            .collect())
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        if let Some(ed) = &self.edit {
            return Ok(ed.get(path)?.metadata());
        }
        let n = self.v.resolve(path)?;
        let mut m = Metadata::new(if n.dir { EntryKind::Directory } else { EntryKind::File }, n.size);
        m.modified = n.mtime;
        Ok(m)
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if self.edit.is_some() {
            return self.e_read(path, offset, buf);
        }
        let n = self.v.resolve(path)?;
        if n.dir {
            return Err(Error::is_a_directory(path));
        }
        self.v.read_file(&n, offset, buf)
    }

    fn write_file(&mut self, path: &VfsPath, data: &[u8]) -> Result<()> {
        if self.edit.is_none() {
            return Err(Error::read_only());
        }
        self.e_put(path, data.to_vec())
    }

    fn create_file(&mut self, path: &VfsPath) -> Result<()> {
        if self.edit.is_none() {
            return Err(Error::read_only());
        }
        if self.edit().nodes.contains_key(path.as_str()) {
            return Err(Error::already_exists(path));
        }
        self.e_put(path, Vec::new())
    }

    fn write_at(&mut self, path: &VfsPath, offset: u64, data: &[u8]) -> Result<()> {
        if self.edit.is_none() {
            return Err(Error::read_only());
        }
        let n = self.edit.as_ref().unwrap().get(path)?.clone();
        if n.dir {
            return Err(Error::is_a_directory(path));
        }
        let mut content = match n.data {
            Some(d) => d,
            None => self.e_content(&n)?,
        };
        let end = offset as usize + data.len();
        if content.len() < end {
            content.resize(end, 0);
        }
        content[offset as usize..end].copy_from_slice(data);
        self.e_put(path, content)
    }

    fn create_dir(&mut self, path: &VfsPath) -> Result<()> {
        if self.edit.is_none() {
            return Err(Error::read_only());
        }
        let ed = self.edit();
        ed.parent_dir(path)?;
        if ed.nodes.contains_key(path.as_str()) {
            return Err(Error::already_exists(path));
        }
        let e = ENode { dir: true, data: None, old: None, size: 0, attrs: vec![now_attr()] };
        ed.nodes.insert(path.as_str().to_owned(), e);
        ed.dirty = true;
        Ok(())
    }

    fn remove_file(&mut self, path: &VfsPath) -> Result<()> {
        if self.edit.is_none() {
            return Err(Error::read_only());
        }
        let ed = self.edit();
        if ed.get(path)?.dir {
            return Err(Error::is_a_directory(path));
        }
        ed.nodes.remove(path.as_str());
        ed.dirty = true;
        Ok(())
    }

    fn remove_dir(&mut self, path: &VfsPath) -> Result<()> {
        if self.edit.is_none() {
            return Err(Error::read_only());
        }
        let ed = self.edit();
        if !ed.get(path)?.dir {
            return Err(Error::not_a_directory(path));
        }
        if path.is_root() {
            return Err(Error::invalid_input("the root cannot be removed"));
        }
        if ed.children(path.as_str()).next().is_some() {
            return Err(Error::new(dix_core::ErrorKind::DirectoryNotEmpty, format!("{path} is not empty")));
        }
        ed.nodes.remove(path.as_str());
        ed.dirty = true;
        Ok(())
    }

    fn rename(&mut self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        if self.edit.is_none() {
            return Err(Error::read_only());
        }
        let ed = self.edit();
        ed.get(from)?;
        ed.parent_dir(to)?;
        if from.is_root() || to.starts_with(from) && to != from {
            return Err(Error::invalid_input("a folder cannot move into itself"));
        }
        if ed.nodes.contains_key(to.as_str()) {
            return Err(Error::already_exists(to));
        }
        let prefix = format!("{}/", from.as_str());
        let moved: Vec<String> =
            ed.nodes.keys().filter(|k| *k == from.as_str() || k.starts_with(&prefix)).cloned().collect();
        for k in moved {
            let n = ed.nodes.remove(&k).unwrap();
            ed.nodes.insert(format!("{}{}", to.as_str(), &k[from.as_str().len()..]), n);
        }
        ed.dirty = true;
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        self.save()
    }

    fn unmount(&mut self) -> Result<()> {
        self.save()
    }
}
