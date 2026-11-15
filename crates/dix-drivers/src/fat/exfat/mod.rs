//! **exFAT** (read-write).
//!
//! * Boot region: 12 sectors (boot sector, 8 extended boot sectors, OEM
//!   parameters, reserved, a checksum sector), and a backup of it.
//! * The FAT: 32-bit entries, only needed for fragmented files: a file
//!   whose clusters are contiguous says so ("NoFatChain") and has none.
//! * The allocation bitmap (a file in the cluster heap, listed in the root):
//!   one bit per cluster, the authority on what is free.
//! * The up-case table (also in the root): how names compare without case.
//! * Directories: entry sets, a File entry, a Stream Extension (first
//!   cluster, sizes, the name's hash) and File Name entries (15 UTF-16 units
//!   each), with a checksum over the set.
//!
//! Writing keeps files contiguous when it can (NoFatChain); a contiguous
//! file that cannot grow in place is turned into an ordinary FAT chain.
//! The boot sector's VolumeDirty flag marks the volume in use until a clean
//! unmount, as Windows and Linux do.

pub mod check;
pub mod mkfs;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
pub mod upcase;

pub mod undelete;
use super::table::Fat;
use dix_core::{
    AccessMode, BlockDevice, Capabilities, DirEntry, EntryKind, Error, ErrorKind, FileSystem, FsInfo, Metadata,
    MountOptions, Result, VfsPath,
};
use upcase::Upcase;

pub const OEM: &[u8; 8] = b"EXFAT   ";

const CAPS: Capabilities = Capabilities::READ
    .union(Capabilities::WRITE)
    .union(Capabilities::CREATE_DIR)
    .union(Capabilities::REMOVE)
    .union(Capabilities::RENAME)
    .union(Capabilities::TIMESTAMPS);

pub(super) fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

pub(super) fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

pub(super) fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

const ATTR_DIR: u16 = 0x10;
const ATTR_HIDDEN: u16 = 0x02;
const ATTR_READ_ONLY: u16 = 0x01;
const ATTR_ARCHIVE: u16 = 0x20;
/// Stream extension flags.
const ALLOC_POSSIBLE: u8 = 0x01;
const NO_FAT_CHAIN: u8 = 0x02;
/// VolumeFlags.
const VOLUME_DIRTY: u16 = 0x02;

/// The boot sector's numbers.
#[derive(Debug, Clone)]
pub struct Boot {
    pub bytes_per_sector: u32,
    pub cluster_size: u32,
    pub volume_sectors: u64,
    pub fat_offset: u64,
    pub fat_bytes: u64,
    pub heap_offset: u64,
    pub clusters: u32,
    pub root_cluster: u32,
    pub serial: u32,
    pub flags: u16,
    pub fats: u8,
}

impl Boot {
    pub fn parse(b: &[u8]) -> Result<Boot> {
        let bad = |what: &str| Error::corrupted(format!("not an exFAT boot sector ({what})"));
        if b.len() < 512 || &b[3..11] != OEM {
            return Err(bad("signature"));
        }
        if b[11..64].iter().any(|&x| x != 0) {
            return Err(bad("the legacy BPB area is not zero"));
        }
        let (bps_shift, spc_shift) = (b[108] as u32, b[109] as u32);
        if !(9..=12).contains(&bps_shift) || bps_shift + spc_shift > 25 {
            return Err(bad("sector / cluster size"));
        }
        let bps = 1u32 << bps_shift;
        let fats = b[110];
        if !(1..=2).contains(&fats) {
            return Err(bad("number of FATs"));
        }
        let (fat_off, fat_len, heap, clusters, root) =
            (le32(b, 80) as u64, le32(b, 84) as u64, le32(b, 88) as u64, le32(b, 92), le32(b, 96));
        let volume_sectors = le64(b, 72);
        if fat_off < 24 || heap < fat_off + fat_len * fats as u64 || clusters == 0 {
            return Err(bad("layout"));
        }
        if (fat_len * bps as u64) < (clusters as u64 + 2) * 4 || root < 2 || root > clusters + 1 {
            return Err(bad("FAT or root"));
        }
        let flags = le16(b, 106);
        let active = if fats == 2 && flags & 1 != 0 { 1 } else { 0 };
        Ok(Boot {
            bytes_per_sector: bps,
            cluster_size: bps << spc_shift,
            volume_sectors,
            fat_offset: (fat_off + active * fat_len) * bps as u64,
            fat_bytes: fat_len * bps as u64,
            heap_offset: heap * bps as u64,
            clusters,
            root_cluster: root,
            serial: le32(b, 100),
            flags,
            fats,
        })
    }

    pub fn cluster_offset(&self, c: u32) -> u64 {
        self.heap_offset + (c as u64 - 2) * self.cluster_size as u64
    }

    pub fn max_cluster(&self) -> u32 {
        self.clusters + 1
    }
}

/// The boot region checksum (VolumeFlags and PercentInUse left out).
pub fn boot_checksum(region: &[u8]) -> u32 {
    region
        .iter()
        .enumerate()
        .filter(|(i, _)| !matches!(i, 106 | 107 | 112))
        .fold(0u32, |c, (_, &b)| c.rotate_right(1).wrapping_add(b as u32))
}

pub fn set_checksum(entries: &[[u8; 32]]) -> u16 {
    let mut c = 0u16;
    for (k, e) in entries.iter().enumerate() {
        for (j, &b) in e.iter().enumerate() {
            if k == 0 && (j == 2 || j == 3) {
                continue;
            }
            c = c.rotate_right(1).wrapping_add(b as u16);
        }
    }
    c
}

pub fn name_hash(up: &Upcase, name: &str) -> u16 {
    up.upper(name).iter().flat_map(|u| u.to_le_bytes()).fold(0u16, |h, b| h.rotate_right(1).wrapping_add(b as u16))
}

/// A timestamp (DOS layout, with 10 ms steps and a UTC offset).
fn timestamp(e: &[u8], at: usize, ms10: Option<usize>, utc: usize) -> Option<i64> {
    let v = le32(e, at);
    let mut t = dix_core::time::from_dos((v >> 16) as u16, v as u16)?;
    if let Some(m) = ms10 {
        t += e[m] as i64 / 100;
    }
    if e[utc] & 0x80 != 0 {
        let quarters = ((e[utc] << 1) as i8 >> 1) as i64;
        t -= quarters * 15 * 60;
    }
    Some(t)
}

fn put_timestamp(e: &mut [u8; 32], at: usize, ms10: Option<usize>, utc: usize, t: i64) {
    let (time, date) = dix_core::time::to_dos(t);
    e[at..at + 4].copy_from_slice(&(((date as u32) << 16) | time as u32).to_le_bytes());
    if let Some(m) = ms10 {
        e[m] = if t.rem_euclid(2) == 1 { 100 } else { 0 };
    }
    e[utc] = 0x80; // UTC
}

/// A file's entry set as found.
#[derive(Debug, Clone)]
pub struct Set {
    pub name: String,
    pub raw: Vec<[u8; 32]>,
    pub offsets: Vec<u64>,
}

impl Set {
    pub fn attrs(&self) -> u16 {
        le16(&self.raw[0], 4)
    }
    pub fn is_dir(&self) -> bool {
        self.attrs() & ATTR_DIR != 0
    }
    pub fn flags(&self) -> u8 {
        self.raw[1][1]
    }
    pub fn nofat(&self) -> bool {
        self.flags() & NO_FAT_CHAIN != 0
    }
    pub fn valid(&self) -> u64 {
        le64(&self.raw[1], 8)
    }
    pub fn first(&self) -> u32 {
        le32(&self.raw[1], 20)
    }
    pub fn size(&self) -> u64 {
        le64(&self.raw[1], 24)
    }
    pub fn name_hash(&self) -> u16 {
        le16(&self.raw[1], 4)
    }

    fn meta(&self) -> Metadata {
        let e = &self.raw[0];
        let mut m = Metadata::new(if self.is_dir() { EntryKind::Directory } else { EntryKind::File }, self.size());
        if self.is_dir() {
            m.size = 0;
        }
        m.created = timestamp(e, 8, Some(20), 22);
        m.modified = timestamp(e, 12, Some(21), 23);
        m.accessed = timestamp(e, 16, None, 24);
        m.hidden = self.attrs() & ATTR_HIDDEN != 0;
        m.inode = Some(self.first() as u64);
        let w = if self.attrs() & ATTR_READ_ONLY != 0 { 0 } else { 0o200 };
        m.mode = Some(if self.is_dir() { 0o040555 | w } else { 0o100444 | w });
        m
    }

    /// Changes the stream extension: where the data is and how much.
    fn set_data(&mut self, first: u32, nofat: bool, size: u64) {
        let s = &mut self.raw[1];
        s[1] = if first == 0 { ALLOC_POSSIBLE } else { ALLOC_POSSIBLE | if nofat { NO_FAT_CHAIN } else { 0 } };
        s[8..16].copy_from_slice(&size.to_le_bytes());
        s[20..24].copy_from_slice(&first.to_le_bytes());
        s[24..32].copy_from_slice(&size.to_le_bytes());
    }

    fn seal(&mut self) {
        let c = set_checksum(&self.raw);
        self.raw[0][2..4].copy_from_slice(&c.to_le_bytes());
    }
}

/// What a directory holds.
#[derive(Default)]
pub struct Listing {
    pub sets: Vec<Set>,
    pub label: Option<String>,
    /// (first cluster, length) of the allocation bitmap (root only).
    pub bitmap: Option<(u32, u64)>,
    /// (checksum, first cluster, length) of the up-case table (root only).
    pub upcase: Option<(u32, u32, u64)>,
    /// Entry sets skipped as damaged: (offset, why).
    pub damaged: Vec<(u64, String)>,
}

pub fn parse(slots: &[(u64, [u8; 32])], up: Option<&Upcase>) -> Listing {
    let mut l = Listing::default();
    let mut i = 0;
    while i < slots.len() {
        let (at, e) = &slots[i];
        let t = e[0];
        if t == 0 {
            break;
        }
        if t & 0x80 == 0 {
            i += 1;
            continue;
        }
        match t {
            0x81 if l.bitmap.is_none() => l.bitmap = Some((le32(e, 20), le64(e, 24))),
            0x82 => l.upcase = Some((le32(e, 4), le32(e, 20), le64(e, 24))),
            0x83 => {
                let n = (e[1] as usize).min(11);
                let units: Vec<u16> = (0..n).map(|k| le16(e, 2 + 2 * k)).collect();
                l.label = Some(String::from_utf16_lossy(&units)).filter(|s| !s.is_empty());
            }
            0x85 => {
                let n = e[1] as usize;
                let why = if n < 2 || i + n >= slots.len() {
                    Some("the set is cut short".to_owned())
                } else {
                    let raw: Vec<[u8; 32]> = slots[i..=i + n].iter().map(|s| s.1).collect();
                    if raw[1][0] != 0xC0 || raw[2..].iter().any(|s| s[0] != 0xC1) {
                        Some("unexpected entries in the set".to_owned())
                    } else if set_checksum(&raw) != le16(&raw[0], 2) {
                        Some("set checksum mismatch".to_owned())
                    } else {
                        let len = raw[1][3] as usize;
                        let units: Vec<u16> =
                            raw[2..].iter().flat_map(|s| (0..15).map(move |k| le16(s, 2 + 2 * k))).take(len).collect();
                        let name = String::from_utf16_lossy(&units);
                        match up {
                            Some(up) if name_hash(up, &name) != le16(&raw[1], 4) => {
                                Some(format!("{name}: name hash mismatch"))
                            }
                            _ => {
                                let offsets = slots[i..=i + n].iter().map(|s| s.0).collect();
                                l.sets.push(Set { name, raw, offsets });
                                None
                            }
                        }
                    }
                };
                match why {
                    Some(w) => {
                        l.damaged.push((*at, w));
                        i += 1;
                    }
                    None => i += n + 1,
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    l
}

/// A new entry set.
pub fn build_set(up: &Upcase, name: &str, attrs: u16, now: i64) -> Result<Set> {
    let units: Vec<u16> = name.encode_utf16().collect();
    if units.len() > 255 {
        return Err(Error::invalid_input("exFAT names are limited to 255 characters"));
    }
    let names = units.len().div_ceil(15);
    let mut file = [0u8; 32];
    file[0] = 0x85;
    file[1] = 1 + names as u8;
    file[4..6].copy_from_slice(&attrs.to_le_bytes());
    put_timestamp(&mut file, 8, Some(20), 22, now);
    put_timestamp(&mut file, 12, Some(21), 23, now);
    put_timestamp(&mut file, 16, None, 24, now);
    let mut stream = [0u8; 32];
    stream[0] = 0xC0;
    stream[1] = ALLOC_POSSIBLE;
    stream[3] = units.len() as u8;
    stream[4..6].copy_from_slice(&name_hash(up, name).to_le_bytes());
    let mut raw = vec![file, stream];
    for chunk in units.chunks(15) {
        let mut e = [0u8; 32];
        e[0] = 0xC1;
        for (k, u) in chunk.iter().enumerate() {
            e[2 + 2 * k..4 + 2 * k].copy_from_slice(&u.to_le_bytes());
        }
        raw.push(e);
    }
    let mut s = Set { name: name.to_owned(), raw, offsets: Vec::new() };
    s.seal();
    Ok(s)
}

/// A directory: the root (a FAT chain), or a folder's set.
#[derive(Clone)]
enum Dir {
    Root,
    Sub(Set),
}

pub struct ExfatFs {
    dev: Box<dyn BlockDevice>,
    boot: Boot,
    fat: Fat,
    bitmap: Vec<u8>,
    /// Where the bitmap is: runs of clusters.
    bitmap_runs: Vec<(u32, u32)>,
    /// Bitmap blocks (4 KiB) changed since the last sync.
    bitmap_dirty: std::collections::BTreeSet<usize>,
    up: Upcase,
    label: Option<String>,
    mode: AccessMode,
    in_use: bool,
    free: u32,
    next_free: u32,
}

/// Mounts an exFAT volume.
pub fn mount(mut dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
    let log = &opts.log;
    let mut b = [0u8; 512];
    dev.read_at(0, &mut b)?;
    let boot = Boot::parse(&b)?;
    let mode = if dev.is_read_only() { AccessMode::ReadOnly } else { opts.mode };
    // The boot region checksum.
    let bps = boot.bytes_per_sector as usize;
    let mut region = vec![0u8; 12 * bps];
    dev.read_at(0, &mut region)?;
    if boot_checksum(&region[..11 * bps]) != le32(&region, 11 * bps) {
        log.warn("The boot region checksum does not match (the boot sector may be damaged).");
    }
    let dirty = boot.flags & VOLUME_DIRTY != 0;
    if dirty {
        log.warn("The volume was not cleanly unmounted (it was removed or the system stopped while it was in use).");
        log.info("exFAT has no journal: check the file system to find what an interrupted write left behind.");
    }
    let fat = Fat::exfat(boot.fat_offset, boot.fat_bytes, boot.max_cluster());
    let mut fs = ExfatFs {
        dev,
        boot,
        fat,
        bitmap: Vec::new(),
        bitmap_runs: Vec::new(),
        bitmap_dirty: Default::default(),
        up: Upcase::generate(),
        label: None,
        mode,
        in_use: dirty,
        free: 0,
        next_free: 2,
    };
    let root = fs.listing(&Dir::Root, false)?;
    let (ck, first, len) = root.upcase.ok_or_else(|| Error::corrupted("exFAT: the root has no up-case table"))?;
    let bytes = fs.read_chain_bytes(first, len)?;
    if upcase::checksum(&bytes) != ck {
        log.warn("The up-case table checksum does not match.");
    }
    fs.up = Upcase::parse(&bytes);
    let (bfirst, blen) = root.bitmap.ok_or_else(|| Error::corrupted("exFAT: the root has no allocation bitmap"))?;
    fs.bitmap_runs = fs.runs(bfirst, false, blen)?;
    fs.bitmap = fs.read_chain_bytes(bfirst, blen)?;
    if (fs.bitmap.len() as u64) * 8 < fs.boot.clusters as u64 {
        return Err(Error::corrupted("exFAT: the allocation bitmap is too small"));
    }
    fs.free = (0..fs.boot.clusters).filter(|&i| fs.bitmap[i as usize / 8] & (1 << (i % 8)) == 0).count() as u32;
    fs.label = root.label;
    log.info(format!("exFAT: {} clusters of {} bytes, {} free", fs.boot.clusters, fs.boot.cluster_size, fs.free));
    Ok(Box::new(fs))
}

impl ExfatFs {
    fn cs(&self) -> u64 {
        self.boot.cluster_size as u64
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.mode.is_writable() { Ok(()) } else { Err(Error::read_only()) }
    }

    fn begin_change(&mut self) -> Result<()> {
        self.ensure_writable()?;
        if !self.in_use {
            self.set_dirty(true)?;
            self.in_use = true;
        }
        Ok(())
    }

    fn set_dirty(&mut self, dirty: bool) -> Result<()> {
        let mut f = [0u8; 2];
        self.dev.read_at(106, &mut f)?;
        let v = u16::from_le_bytes(f);
        let v = if dirty { v | VOLUME_DIRTY } else { v & !VOLUME_DIRTY };
        self.dev.write_at(106, &v.to_le_bytes())?;
        if !dirty {
            let used = self.boot.clusters - self.free;
            let percent = (used as u64 * 100 / self.boot.clusters.max(1) as u64) as u8;
            self.dev.write_at(112, &[percent])?;
        }
        self.dev.flush()
    }

    /// The runs of a chain: contiguous (NoFatChain) or through the FAT.
    fn runs(&mut self, first: u32, nofat: bool, size: u64) -> Result<Vec<(u32, u32)>> {
        if first == 0 {
            return Ok(Vec::new());
        }
        if first < 2 || first > self.boot.max_cluster() {
            return Err(Error::corrupted(format!("exFAT: a chain starts outside the volume ({first})")));
        }
        if nofat {
            let n = size.div_ceil(self.cs()) as u32;
            if first as u64 + n as u64 > self.boot.max_cluster() as u64 + 1 {
                return Err(Error::corrupted("exFAT: a contiguous file extends beyond the volume"));
            }
            return Ok(if n == 0 { Vec::new() } else { vec![(first, n)] });
        }
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for c in self.fat.chain(self.dev.as_mut(), first)? {
            match runs.last_mut() {
                Some((s, n)) if *s + *n == c => *n += 1,
                _ => runs.push((c, 1)),
            }
        }
        Ok(runs)
    }

    fn read_chain_bytes(&mut self, first: u32, len: u64) -> Result<Vec<u8>> {
        let runs = self.runs(first, false, len)?;
        let mut out = vec![0u8; len as usize];
        for (at, r) in spans(&self.boot, &runs, 0, len as usize) {
            self.dev.read_at(at, &mut out[r])?;
        }
        Ok(out)
    }

    fn dir_runs(&mut self, d: &Dir) -> Result<Vec<(u32, u32)>> {
        match d {
            Dir::Root => self.runs(self.boot.root_cluster, false, 0),
            Dir::Sub(s) => self.runs(s.first(), s.nofat(), s.size()),
        }
    }

    fn slots(&mut self, d: &Dir) -> Result<Vec<(u64, [u8; 32])>> {
        let mut out = Vec::new();
        for (start, n) in self.dir_runs(d)? {
            let at = self.boot.cluster_offset(start);
            let mut v = vec![0u8; (n as u64 * self.cs()) as usize];
            self.dev.read_at(at, &mut v)?;
            out.extend(v.chunks_exact(32).enumerate().map(|(i, e)| (at + i as u64 * 32, e.try_into().unwrap())));
        }
        Ok(out)
    }

    fn listing(&mut self, d: &Dir, hash: bool) -> Result<Listing> {
        let slots = self.slots(d)?;
        Ok(parse(&slots, if hash { Some(&self.up) } else { None }))
    }

    fn sets(&mut self, d: &Dir) -> Result<Vec<Set>> {
        Ok(self.listing(d, false)?.sets)
    }

    fn find(&self, sets: &[Set], name: &str) -> Option<usize> {
        let want = self.up.upper(name);
        sets.iter().position(|s| s.name == name).or_else(|| sets.iter().position(|s| self.up.upper(&s.name) == want))
    }

    fn resolve_dir(&mut self, path: &VfsPath) -> Result<Dir> {
        let mut d = Dir::Root;
        for c in path.components() {
            let sets = self.sets(&d)?;
            let i = self.find(&sets, c).ok_or_else(|| Error::not_found(path))?;
            if !sets[i].is_dir() {
                return Err(Error::not_a_directory(path));
            }
            d = Dir::Sub(sets[i].clone());
        }
        Ok(d)
    }

    fn lookup(&mut self, path: &VfsPath) -> Result<(Dir, Set)> {
        let parent = path.parent().ok_or_else(|| Error::invalid_input("the root has no entry"))?;
        let d = self.resolve_dir(&parent)?;
        let sets = self.sets(&d)?;
        let i = self.find(&sets, path.file_name().unwrap_or_default()).ok_or_else(|| Error::not_found(path))?;
        Ok((d, sets[i].clone()))
    }

    // ----- allocation -------------------------------------------------------

    fn is_used(&self, c: u32) -> bool {
        let i = (c - 2) as usize;
        self.bitmap[i / 8] & (1 << (i % 8)) != 0
    }

    fn mark(&mut self, c: u32, used: bool) {
        let i = (c - 2) as usize;
        if used {
            self.bitmap[i / 8] |= 1 << (i % 8);
        } else {
            self.bitmap[i / 8] &= !(1 << (i % 8));
        }
        self.bitmap_dirty.insert(i / 8 / 4096);
    }

    /// `n` clusters: at `at` if all free there, else the first contiguous
    /// run that fits, else wherever they are. Runs, in order.
    fn alloc(&mut self, n: u32, at: Option<u32>) -> Result<Vec<(u32, u32)>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        if n > self.free {
            return Err(Error::new(ErrorKind::NoSpace, "the volume is full"));
        }
        let max = self.boot.max_cluster();
        let fits = |fs: &Self, s: u32| s as u64 + n as u64 <= max as u64 + 1 && (s..s + n).all(|c| !fs.is_used(c));
        let mut runs = Vec::new();
        if let Some(s) = at.filter(|&s| s >= 2 && fits(self, s)) {
            runs.push((s, n));
        } else {
            // First fit, from the hint, of a run of n.
            let mut start = None;
            let (mut c, mut len) = (self.next_free.clamp(2, max), 0u32);
            let mut seen = 0u32;
            while seen < self.boot.clusters {
                if !self.is_used(c) {
                    len += 1;
                    if len == n {
                        start = Some(c + 1 - n);
                        break;
                    }
                } else {
                    len = 0;
                }
                c = if c >= max {
                    len = 0;
                    2
                } else {
                    c + 1
                };
                seen += 1;
            }
            match start {
                Some(s) => runs.push((s, n)),
                None => {
                    // Fragmented: take free clusters as they come.
                    let mut left = n;
                    let mut c = 2;
                    while left > 0 && c <= max {
                        if !self.is_used(c) {
                            match runs.last_mut() {
                                Some((s, k)) if *s + *k == c => *k += 1,
                                _ => runs.push((c, 1)),
                            }
                            left -= 1;
                        }
                        c += 1;
                    }
                }
            }
        }
        for &(s, k) in &runs {
            for c in s..s + k {
                self.mark(c, true);
            }
        }
        self.free -= n;
        let (s, k) = *runs.last().unwrap();
        self.next_free = if s + k > max { 2 } else { s + k };
        Ok(runs)
    }

    fn release(&mut self, runs: &[(u32, u32)]) {
        for &(s, k) in runs {
            for c in s..s + k {
                if self.is_used(c) {
                    self.mark(c, false);
                    self.free += 1;
                }
            }
        }
    }

    /// Writes `runs` as a FAT chain, linked after `after`.
    fn link(&mut self, runs: &[(u32, u32)], after: Option<u32>) -> Result<()> {
        let clusters: Vec<u32> = runs.iter().flat_map(|&(s, k)| s..s + k).collect();
        let eoc = self.fat.eoc();
        if let (Some(a), Some(&f)) = (after, clusters.first()) {
            self.fat.set(self.dev.as_mut(), a, f)?;
        }
        for w in clusters.windows(2) {
            self.fat.set(self.dev.as_mut(), w[0], w[1])?;
        }
        if let Some(&l) = clusters.last() {
            self.fat.set(self.dev.as_mut(), l, eoc)?;
        }
        Ok(())
    }

    /// Grows a chain to `need` clusters: (first, nofat).
    fn grow(&mut self, first: u32, nofat: bool, have: &[(u32, u32)], need: u32) -> Result<(u32, bool)> {
        let count: u32 = have.iter().map(|r| r.1).sum();
        if need <= count {
            return Ok((first, nofat));
        }
        let more = need - count;
        let last = have.last().map(|&(s, k)| s + k - 1);
        let added = self.alloc(more, last.map(|l| l + 1))?;
        if first == 0 {
            // A new chain: contiguous if it came in one run.
            if added.len() == 1 {
                return Ok((added[0].0, true));
            }
            self.link(&added, None)?;
            return Ok((added[0].0, false));
        }
        if nofat && added.len() == 1 && Some(added[0].0) == last.map(|l| l + 1) {
            return Ok((first, true));
        }
        // Not contiguous any more: the FAT holds the chain from now on.
        if nofat {
            self.link(have, None)?;
        }
        self.link(&added, last)?;
        Ok((first, false))
    }

    fn zero_runs(&mut self, runs: &[(u32, u32)]) -> Result<()> {
        let z = vec![0u8; self.cs() as usize];
        for &(s, k) in runs {
            for c in s..s + k {
                self.dev.write_at(self.boot.cluster_offset(c), &z)?;
            }
        }
        Ok(())
    }

    // ----- entry sets -------------------------------------------------------

    fn write_set(&mut self, s: &Set) -> Result<()> {
        for (at, e) in s.offsets.iter().zip(&s.raw) {
            self.dev.write_at(*at, e)?;
        }
        Ok(())
    }

    fn delete_set(&mut self, s: &Set) -> Result<()> {
        for (at, e) in s.offsets.iter().zip(&s.raw) {
            self.dev.write_at(*at, &[e[0] & 0x7F])?;
        }
        Ok(())
    }

    /// Puts a new set into a directory (growing it when full).
    fn add_set(&mut self, d: &Dir, mut s: Set) -> Result<Set> {
        let need = s.raw.len();
        let mut d = d.clone();
        for _ in 0..2 {
            let slots = self.slots(&d)?;
            let mut run = Vec::new();
            for (at, e) in &slots {
                if e[0] & 0x80 == 0 {
                    run.push(*at);
                    if run.len() == need {
                        break;
                    }
                } else {
                    run.clear();
                }
            }
            if run.len() == need {
                s.offsets = run;
                self.write_set(&s)?;
                return Ok(s);
            }
            d = self.grow_dir(&d, need.div_ceil((self.cs() / 32) as usize) as u32)?;
        }
        Err(Error::new(ErrorKind::NoSpace, "the directory is full"))
    }

    /// Adds `n` zeroed clusters to a directory; the directory as it is now.
    fn grow_dir(&mut self, d: &Dir, n: u32) -> Result<Dir> {
        let runs = self.dir_runs(d)?;
        let count: u32 = runs.iter().map(|r| r.1).sum();
        match d {
            Dir::Root => {
                let added = self.alloc(n, runs.last().map(|&(s, k)| s + k))?;
                self.zero_runs(&added)?;
                self.link(&added, runs.last().map(|&(s, k)| s + k - 1))?;
                Ok(Dir::Root)
            }
            Dir::Sub(set) => {
                let (first, nofat) = self.grow(set.first(), set.nofat(), &runs, count + n)?;
                let new_runs = self.runs(first, nofat, (count + n) as u64 * self.cs())?;
                let added: Vec<(u32, u32)> = clusters_after(&new_runs, count);
                self.zero_runs(&added)?;
                let mut set = set.clone();
                set.set_data(first, nofat, (count + n) as u64 * self.cs());
                set.seal();
                self.write_set(&set)?;
                Ok(Dir::Sub(set))
            }
        }
    }

    fn write_bitmap(&mut self) -> Result<()> {
        let blocks = std::mem::take(&mut self.bitmap_dirty);
        for b in blocks {
            let start = b * 4096;
            let end = (start + 4096).min(self.bitmap.len());
            let runs = self.bitmap_runs.clone();
            for (at, r) in spans(&self.boot, &runs, start as u64, end - start) {
                let (lo, hi) = (start + r.start, start + r.end);
                self.dev.write_at(at, &self.bitmap[lo..hi])?;
            }
        }
        Ok(())
    }
}

/// The clusters of `runs` after the first `skip`, as runs.
fn clusters_after(runs: &[(u32, u32)], mut skip: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for &(s, k) in runs {
        if skip >= k {
            skip -= k;
            continue;
        }
        out.push((s + skip, k - skip));
        skip = 0;
    }
    out
}

/// (device offset, range in the buffer) for bytes `[offset, offset+len)`
/// of a chain.
fn spans(boot: &Boot, runs: &[(u32, u32)], offset: u64, len: usize) -> Vec<(u64, std::ops::Range<usize>)> {
    let cs = boot.cluster_size as u64;
    let mut out = Vec::new();
    let (mut pos, mut done) = (0u64, 0usize);
    for &(start, n) in runs {
        let bytes = n as u64 * cs;
        let lo = offset + done as u64;
        if lo >= pos + bytes {
            pos += bytes;
            continue;
        }
        let within = lo - pos;
        let take = (bytes - within).min((len - done) as u64) as usize;
        out.push((boot.cluster_offset(start) + within, done..done + take));
        done += take;
        pos += bytes;
        if done == len {
            break;
        }
    }
    out
}

impl FileSystem for ExfatFs {
    fn info(&self) -> FsInfo {
        let cs = self.cs();
        FsInfo {
            driver_id: "fat".into(),
            fs_name: "exFAT".into(),
            label: self.label.clone(),
            total_bytes: Some(self.boot.clusters as u64 * cs),
            free_bytes: Some(self.free as u64 * cs),
            block_size: Some(cs as u32),
            details: Vec::new(),
        }
    }

    fn capabilities(&self) -> Capabilities {
        CAPS.for_mode(self.mode)
    }

    fn access_mode(&self) -> AccessMode {
        self.mode
    }

    fn read_dir(&mut self, path: &VfsPath) -> Result<Vec<DirEntry>> {
        let d = self.resolve_dir(path)?;
        Ok(self.sets(&d)?.iter().map(|s| DirEntry { name: s.name.clone(), metadata: s.meta() }).collect())
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        if path.is_root() {
            return Ok(Metadata::new(EntryKind::Directory, 0));
        }
        Ok(self.lookup(path)?.1.meta())
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let (_, s) = self.lookup(path)?;
        if s.is_dir() {
            return Err(Error::is_a_directory(path));
        }
        if offset >= s.size() {
            return Ok(0);
        }
        let n = buf.len().min((s.size() - offset) as usize);
        // Past the valid data length: zeroes.
        let valid = s.valid().min(s.size());
        let real = if offset >= valid { 0 } else { n.min((valid - offset) as usize) };
        buf[real..n].fill(0);
        if real > 0 {
            let runs = self.runs(s.first(), s.nofat(), s.size())?;
            let spans = spans(&self.boot, &runs, offset, real);
            if spans.last().map_or(0, |x| x.1.end) < real {
                return Err(Error::corrupted(format!("{path}: its clusters are fewer than its size")));
            }
            for (at, r) in spans {
                self.dev.read_at(at, &mut buf[r])?;
            }
        }
        Ok(n)
    }

    fn write_file(&mut self, path: &VfsPath, data: &[u8]) -> Result<()> {
        self.create_file(path)?;
        if !data.is_empty() {
            self.write_at(path, 0, data)?;
        }
        Ok(())
    }

    fn create_file(&mut self, path: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let now = dix_core::time::unix_now();
        match self.lookup(path) {
            Ok((_, s)) if s.is_dir() => Err(Error::is_a_directory(path)),
            Ok((_, mut s)) => {
                let runs = self.runs(s.first(), s.nofat(), s.size())?;
                self.release(&runs);
                s.set_data(0, false, 0);
                put_timestamp(&mut s.raw[0], 12, Some(21), 23, now);
                s.seal();
                self.write_set(&s)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let d = self.resolve_dir(&path.parent().unwrap_or_default())?;
                let s = build_set(&self.up, path.file_name().unwrap_or_default(), ATTR_ARCHIVE, now)?;
                self.add_set(&d, s).map(|_| ())
            }
            Err(e) => Err(e),
        }
    }

    fn write_at(&mut self, path: &VfsPath, offset: u64, data: &[u8]) -> Result<()> {
        self.begin_change()?;
        let (_, mut s) = self.lookup(path)?;
        if s.is_dir() {
            return Err(Error::is_a_directory(path));
        }
        let old = s.valid().min(s.size());
        let end = offset + data.len() as u64;
        let new_size = s.size().max(end);
        let have = self.runs(s.first(), s.nofat(), s.size())?;
        let need = new_size.div_ceil(self.cs()) as u32;
        let (first, nofat) = self.grow(s.first(), s.nofat(), &have, need)?;
        let runs = self.runs(first, nofat, new_size)?;
        // Between the valid data and the write: zeroes.
        if offset > old {
            let zeros = vec![0u8; (offset - old).min(1 << 20) as usize];
            let mut at = old;
            while at < offset {
                let n = (offset - at).min((zeros.len()) as u64) as usize;
                for (dev_at, r) in spans(&self.boot, &runs, at, n) {
                    self.dev.write_at(dev_at, &zeros[r])?;
                }
                at += n as u64;
            }
        }
        for (dev_at, r) in spans(&self.boot, &runs, offset, data.len()) {
            self.dev.write_at(dev_at, &data[r])?;
        }
        s.set_data(first, nofat, new_size);
        put_timestamp(&mut s.raw[0], 12, Some(21), 23, dix_core::time::unix_now());
        let attrs = s.attrs() | ATTR_ARCHIVE;
        s.raw[0][4..6].copy_from_slice(&attrs.to_le_bytes());
        s.seal();
        self.write_set(&s)
    }

    fn create_dir(&mut self, path: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let parent = path.parent().ok_or_else(|| Error::already_exists("/"))?;
        let d = self.resolve_dir(&parent)?;
        let name = path.file_name().unwrap_or_default();
        let sets = self.sets(&d)?;
        if self.find(&sets, name).is_some() {
            return Err(Error::already_exists(path));
        }
        let runs = self.alloc(1, None)?;
        self.zero_runs(&runs)?;
        let mut s = build_set(&self.up, name, ATTR_DIR, dix_core::time::unix_now())?;
        s.set_data(runs[0].0, true, self.cs());
        s.seal();
        if let Err(e) = self.add_set(&d, s) {
            self.release(&runs);
            return Err(e);
        }
        Ok(())
    }

    fn remove_file(&mut self, path: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let (_, s) = self.lookup(path)?;
        if s.is_dir() {
            return Err(Error::is_a_directory(path));
        }
        self.delete_set(&s)?;
        let runs = self.runs(s.first(), s.nofat(), s.size())?;
        self.release(&runs);
        Ok(())
    }

    fn remove_dir(&mut self, path: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let (_, s) = self.lookup(path)?;
        if !s.is_dir() {
            return Err(Error::not_a_directory(path));
        }
        if !self.sets(&Dir::Sub(s.clone()))?.is_empty() {
            return Err(Error::new(ErrorKind::DirectoryNotEmpty, format!("{path} is not empty")));
        }
        self.delete_set(&s)?;
        let runs = self.runs(s.first(), s.nofat(), s.size())?;
        self.release(&runs);
        Ok(())
    }

    fn rename(&mut self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let (from_dir, old) = self.lookup(from)?;
        if to.starts_with(from) && to != from {
            return Err(Error::invalid_input("cannot move a folder into itself"));
        }
        let to_dir = self.resolve_dir(&to.parent().ok_or_else(|| Error::invalid_input("the root"))?)?;
        let to_name = to.file_name().unwrap_or_default();
        let same_dir = match (&from_dir, &to_dir) {
            (Dir::Root, Dir::Root) => true,
            (Dir::Sub(a), Dir::Sub(b)) => a.offsets == b.offsets,
            _ => false,
        };
        let same_name = same_dir && self.up.upper(to_name) == self.up.upper(&old.name);
        let sets = self.sets(&to_dir)?;
        if !same_name && self.find(&sets, to_name).is_some() {
            return Err(Error::already_exists(to));
        }
        // The new set: the old one's fields under the new name.
        let mut s = build_set(&self.up, to_name, old.attrs(), 0)?;
        s.raw[0][4..32].copy_from_slice(&old.raw[0][4..32]);
        s.raw[1][1] = old.raw[1][1];
        s.raw[1][8..32].copy_from_slice(&old.raw[1][8..32]);
        s.seal();
        if same_name {
            self.delete_set(&old)?;
            return self.add_set(&to_dir, s).map(|_| ());
        }
        // New first: an interruption leaves two names, not none.
        self.add_set(&to_dir, s)?;
        // The old set may have moved if the directory grew: find it again.
        let (_, old) = self.lookup(from)?;
        self.delete_set(&old)
    }

    fn sync(&mut self) -> Result<()> {
        if !self.mode.is_writable() {
            return Ok(());
        }
        self.fat.flush(self.dev.as_mut())?;
        self.write_bitmap()?;
        self.fat.trim();
        self.dev.flush()
    }

    fn unmount(&mut self) -> Result<()> {
        if self.mode.is_writable() && self.in_use {
            self.sync()?;
            self.set_dirty(false)?;
            self.in_use = false;
        }
        Ok(())
    }
}

impl Drop for ExfatFs {
    fn drop(&mut self) {
        if self.mode.is_writable() && self.in_use {
            let _ = self.sync();
        }
    }
}
