//! Btrfs, read-only: the default of Fedora, openSUSE and Synology NAS
//! boxes.
//!
//! Everything is in copy-on-write B-trees of a fixed node size, addressed
//! by **logical** byte numbers. The *chunk tree* maps logical ranges to
//! places on the device(s); the chunks it needs itself are in the
//! superblock (the *system chunk array*). The *root tree* lists the other
//! trees, among them a *file system tree* per subvolume (the top one is
//! tree 5). Items are sorted by key (object id, type, offset); a file
//! system tree holds per inode its inode item, its folder entries
//! (`DIR_INDEX`: name, file type, the inode or subvolume it leads to) and
//! its file extents (`EXTENT_DATA`: inline bytes, or a range of a data
//! extent on disk, maybe compressed with zlib, LZO or zstd, or
//! preallocated; missing ranges are holes).
//!
//! Subvolumes and snapshots show as the folders they are mounted at.
//! Data on one device (single, DUP and RAID1 profiles) is read; striped
//! profiles over several devices (RAID0/10/5/6) are not. Checksums are not
//! checked; a log tree waiting to be replayed (the last fsync'd writes) is
//! not applied. Extended attributes are not shown.

#[cfg(all(test, unix))]
mod tests;

use dix_core::{
    AccessMode, BlockDevice, Capabilities, DirEntry, Driver, DriverInfo, EntryKind, Error, FileSystem, FsInfo,
    Metadata, MountOptions, ProbeScore, Result, VfsPath,
};
use std::collections::HashMap;
use std::sync::Arc;

const CAPS: Capabilities =
    Capabilities::READ.union(Capabilities::SYMLINKS).union(Capabilities::PERMISSIONS).union(Capabilities::TIMESTAMPS);

const SUPER_AT: u64 = 0x10000;
const MAGIC: &[u8; 8] = b"_BHRfS_M";
const HEADER: usize = 101;
const CACHE: usize = 1024;

// Item types.
const INODE_ITEM: u8 = 1;
const DIR_INDEX: u8 = 96;
const EXTENT_DATA: u8 = 108;
const ROOT_ITEM: u8 = 132;
const CHUNK_ITEM: u8 = 228;

const FS_TREE: u64 = 5;
const FIRST_CHUNK_TREE: u64 = 256;
const FIRST_FREE: u64 = 256;
const LAST_FREE: u64 = u64::MAX - 256;

// Block group profiles that stripe data over devices.
const STRIPED: u64 = (1 << 3) | (1 << 6) | (1 << 7) | (1 << 8);

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn damaged(what: impl std::fmt::Display) -> Error {
    Error::corrupted(format!("Btrfs: {what}"))
}

/// An item key: object id, type, offset (the order of items).
type Key = (u64, u8, u64);

fn key(b: &[u8], at: usize) -> Key {
    (le64(b, at), b[at + 8], le64(b, at + 9))
}

pub struct BtrfsDriver;

impl Driver for BtrfsDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: "btrfs",
            name: "Btrfs",
            description: "Btrfs (Fedora, openSUSE, Synology): subvolumes, snapshots, zlib/LZO/zstd; read-only",
            capabilities: CAPS,
        }
    }

    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
        if dev.size() < SUPER_AT + 4096 {
            return Ok(0);
        }
        let mut b = [0u8; 8];
        dev.read_at(SUPER_AT + 0x40, &mut b)?;
        Ok(if &b == MAGIC { 95 } else { 0 })
    }

    fn mount(&self, dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
        if opts.mode == AccessMode::ReadWrite {
            opts.log.info("Btrfs is read-only here: the volume is opened read-only.");
        }
        let v = Volume::open(dev)?;
        if v.log_root != 0 {
            opts.log.warn(
                "The volume has a log tree waiting to be replayed: the last changes written with fsync are not shown.",
            );
        }
        Ok(Box::new(Btrfs { v }))
    }
}

/// A logical range and where it is on this device.
#[derive(Debug, Clone, Copy)]
struct Chunk {
    logical: u64,
    length: u64,
    physical: u64,
    striped: bool,
}

/// A tree's root node and its level.
#[derive(Debug, Clone, Copy)]
struct Root {
    bytenr: u64,
    dirid: u64,
}

#[derive(Debug, Clone)]
struct Inode {
    tree: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    atime: i64,
    mtime: i64,
    otime: i64,
}

impl Inode {
    fn kind(&self) -> EntryKind {
        match self.mode & S_IFMT {
            S_IFDIR => EntryKind::Directory,
            S_IFREG => EntryKind::File,
            S_IFLNK => EntryKind::Symlink,
            _ => EntryKind::Other,
        }
    }
}

#[derive(Debug, Clone)]
struct Child {
    name: String,
    /// (tree, inode) it leads to.
    to: (u64, u64),
}

#[derive(Debug, Clone)]
enum Data {
    /// Bytes in the item (unpacked).
    Inline(Arc<Vec<u8>>),
    /// A range of a data extent: (its logical start, length on disk,
    /// unpacked length, compression), and where in its unpacked bytes.
    Disk { at: u64, disk_len: u64, ram: u64, compression: u8, offset: u64 },
    /// A hole or preallocated space.
    Zeros,
}

#[derive(Debug, Clone)]
struct Extent {
    file_off: u64,
    len: u64,
    data: Data,
}

struct Volume {
    dev: Box<dyn BlockDevice>,
    devid: u64,
    nodesize: u64,
    sectorsize: u64,
    total: u64,
    used: u64,
    label: String,
    log_root: u64,
    csum: u16,
    features: Vec<&'static str>,
    chunks: Vec<Chunk>,
    /// Every copy of each chunk on this device (DUP: two), by logical start.
    copies: HashMap<u64, Vec<u64>>,
    root_tree: u64,
    /// File system trees: the top one (5) and each subvolume.
    trees: HashMap<u64, Root>,
    nodes: HashMap<u64, Arc<Vec<u8>>>,
    inodes: HashMap<(u64, u64), Arc<Inode>>,
    dirs: HashMap<(u64, u64), Arc<Vec<Child>>>,
    extents: HashMap<(u64, u64), Arc<Vec<Extent>>>,
    /// The last unpacked data extent: (logical start, bytes).
    unpacked: Option<(u64, Arc<Vec<u8>>)>,
}

impl Volume {
    fn open(mut dev: Box<dyn BlockDevice>) -> Result<Volume> {
        let mut sb = vec![0u8; 4096];
        dev.read_at(SUPER_AT, &mut sb)?;
        if &sb[0x40..0x48] != MAGIC {
            return Err(Error::new(dix_core::ErrorKind::UnknownFormat, "not a Btrfs file system"));
        }
        let nodesize = le32(&sb, 0x94) as u64;
        let sectorsize = le32(&sb, 0x90) as u64;
        if !(4096..=65536).contains(&nodesize) || !nodesize.is_power_of_two() || !sectorsize.is_power_of_two() {
            return Err(damaged("an impossible node or sector size"));
        }
        let incompat = le64(&sb, 0xBC);
        // Extent tree v2 and the RAID stripe tree change how data is found.
        if incompat & ((1 << 13) | (1 << 14)) != 0 {
            return Err(Error::unsupported(format!("Btrfs features {:#x}", incompat & ((1 << 13) | (1 << 14)))));
        }
        let compat_ro = le64(&sb, 0xB4);
        let mut features = Vec::new();
        for (on, name) in [
            (incompat & (1 << 3) != 0, "LZO"),
            (incompat & (1 << 4) != 0, "zstd"),
            (incompat & (1 << 8) != 0, "skinny metadata"),
            (incompat & (1 << 9) != 0, "no holes"),
            (incompat & (1 << 12) != 0, "zoned"),
            (compat_ro & 1 != 0, "free space tree"),
            (compat_ro & (1 << 3) != 0, "block group tree"),
        ] {
            if on {
                features.push(name);
            }
        }
        let label = {
            let l = &sb[0x12B..0x12B + 256];
            let end = l.iter().position(|&c| c == 0).unwrap_or(l.len());
            String::from_utf8_lossy(&l[..end]).into_owned()
        };
        let mut v = Volume {
            dev,
            devid: le64(&sb, 0xC9),
            nodesize,
            sectorsize,
            total: le64(&sb, 0x70),
            used: le64(&sb, 0x78),
            label,
            log_root: le64(&sb, 0x60),
            csum: le16(&sb, 0xC4),
            features,
            chunks: Vec::new(),
            copies: HashMap::new(),
            root_tree: le64(&sb, 0x50),
            trees: HashMap::new(),
            nodes: HashMap::new(),
            inodes: HashMap::new(),
            dirs: HashMap::new(),
            extents: HashMap::new(),
            unpacked: None,
        };
        // The chunks the chunk tree needs, from the superblock.
        let n = (le32(&sb, 0xA0) as usize).min(2048);
        let arr = sb[0x32B..0x32B + n].to_vec();
        let mut pos = 0;
        while pos + 17 + 48 <= arr.len() {
            let k = key(&arr, pos);
            pos += 17;
            if k.1 != CHUNK_ITEM {
                return Err(damaged("the system chunk array holds something else"));
            }
            let len = 48 + 32 * le16(&arr, pos + 44) as usize;
            let item = arr.get(pos..pos + len).ok_or_else(|| damaged("the system chunk array is cut"))?;
            v.add_chunk(k.2, item)?;
            pos += len;
        }
        // Then every chunk.
        let chunk_root = le64(&sb, 0x58);
        let mut items = Vec::new();
        v.collect(
            chunk_root,
            (FIRST_CHUNK_TREE, CHUNK_ITEM, 0),
            (FIRST_CHUNK_TREE, CHUNK_ITEM, u64::MAX),
            &mut items,
            0,
        )?;
        for (k, item) in items {
            v.add_chunk(k.2, &item)?;
        }
        // The file system trees.
        let root_tree = le64(&sb, 0x50);
        let mut items = Vec::new();
        v.collect(root_tree, (FS_TREE, ROOT_ITEM, 0), (LAST_FREE, ROOT_ITEM, u64::MAX), &mut items, 0)?;
        for (k, item) in items {
            let tree = k.0;
            if k.1 == ROOT_ITEM && (tree == FS_TREE || (FIRST_FREE..=LAST_FREE).contains(&tree)) && item.len() >= 239 {
                v.trees.insert(tree, Root { bytenr: le64(&item, 176), dirid: le64(&item, 168) });
            }
        }
        if !v.trees.contains_key(&FS_TREE) {
            return Err(damaged("there is no file system tree"));
        }
        Ok(v)
    }

    fn add_chunk(&mut self, logical: u64, item: &[u8]) -> Result<()> {
        let length = le64(item, 0);
        let ty = le64(item, 24);
        let stripes = le16(item, 44) as usize;
        // The copy on this device (DUP keeps two there; RAID1 one per device).
        let mut here = Vec::new();
        for s in 0..stripes {
            let at = 48 + s * 32;
            let st = item.get(at..at + 32).ok_or_else(|| damaged("a chunk is cut"))?;
            if le64(st, 0) == self.devid || stripes == 1 {
                here.push(le64(st, 8));
            }
        }
        let Some(&physical) = here.first() else { return Ok(()) };
        if self.chunks.iter().any(|c| c.logical == logical) {
            return Ok(());
        }
        self.copies.insert(logical, here);
        let striped = ty & STRIPED != 0 && stripes > 1;
        self.chunks.push(Chunk { logical, length, physical, striped });
        self.chunks.sort_by_key(|c| c.logical);
        Ok(())
    }

    /// Reads `buf.len()` bytes at logical address `at`.
    fn read_logical(&mut self, mut at: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let i = self.chunks.partition_point(|c| c.logical + c.length <= at);
            let c = *self
                .chunks
                .get(i)
                .filter(|c| c.logical <= at)
                .ok_or_else(|| damaged(format!("logical address {at} is in no chunk")))?;
            if c.striped {
                return Err(Error::unsupported("Btrfs data striped over several devices (RAID0/10/5/6)"));
            }
            let k = ((c.logical + c.length - at) as usize).min(buf.len() - done);
            self.dev.read_at(c.physical + (at - c.logical), &mut buf[done..done + k])?;
            done += k;
            at += k as u64;
        }
        Ok(())
    }

    fn node(&mut self, bytenr: u64) -> Result<Arc<Vec<u8>>> {
        if let Some(n) = self.nodes.get(&bytenr) {
            return Ok(n.clone());
        }
        let mut b = vec![0u8; self.nodesize as usize];
        self.read_logical(bytenr, &mut b)?;
        if le64(&b, 0x30) != bytenr {
            return Err(damaged(format!("the tree node at {bytenr} says it is elsewhere")));
        }
        let b = Arc::new(b);
        if self.nodes.len() >= CACHE {
            self.nodes.clear();
        }
        self.nodes.insert(bytenr, b.clone());
        Ok(b)
    }

    /// Every item of the tree at `bytenr` with a key in `lo..=hi`.
    fn collect(&mut self, bytenr: u64, lo: Key, hi: Key, out: &mut Vec<(Key, Vec<u8>)>, depth: u32) -> Result<()> {
        if depth > 8 {
            return Err(damaged("a tree is too deep"));
        }
        let node = self.node(bytenr)?;
        let n = le32(&node, 0x60) as usize;
        let level = node[0x64];
        if level == 0 {
            for i in 0..n {
                let at = HEADER + i * 25;
                if at + 25 > node.len() {
                    return Err(damaged("a leaf is overfull"));
                }
                let k = key(&node, at);
                if k < lo {
                    continue;
                }
                if k > hi {
                    break;
                }
                let (off, size) = (le32(&node, at + 17) as usize, le32(&node, at + 21) as usize);
                let data = node.get(HEADER + off..HEADER + off + size).ok_or_else(|| damaged("an item is cut"))?;
                out.push((k, data.to_vec()));
            }
        } else {
            let ptr = |i: usize| HEADER + i * 33;
            for i in 0..n {
                if ptr(i) + 33 > node.len() {
                    return Err(damaged("a node is overfull"));
                }
                let k = key(&node, ptr(i));
                if k > hi {
                    break;
                }
                if i + 1 < n && key(&node, ptr(i + 1)) <= lo {
                    continue;
                }
                let child = le64(&node, ptr(i) + 17);
                self.collect(child, lo, hi, out, depth + 1)?;
            }
        }
        Ok(())
    }

    fn items(&mut self, tree: u64, lo: Key, hi: Key) -> Result<Vec<(Key, Vec<u8>)>> {
        let root = self.trees.get(&tree).ok_or_else(|| damaged(format!("no tree {tree}")))?.bytenr;
        let mut out = Vec::new();
        self.collect(root, lo, hi, &mut out, 0)?;
        Ok(out)
    }

    fn inode(&mut self, tree: u64, ino: u64) -> Result<Arc<Inode>> {
        if let Some(n) = self.inodes.get(&(tree, ino)) {
            return Ok(n.clone());
        }
        let items = self.items(tree, (ino, INODE_ITEM, 0), (ino, INODE_ITEM, u64::MAX))?;
        let (_, d) =
            items.into_iter().next().ok_or_else(|| damaged(format!("inode {ino} of tree {tree} is missing")))?;
        if d.len() < 160 {
            return Err(damaged("an inode item is short"));
        }
        let n = Arc::new(Inode {
            tree,
            ino,
            size: le64(&d, 16),
            uid: le32(&d, 44),
            gid: le32(&d, 48),
            mode: le32(&d, 52),
            atime: le64(&d, 112) as i64,
            mtime: le64(&d, 136) as i64,
            otime: le64(&d, 148) as i64,
        });
        if self.inodes.len() >= CACHE {
            self.inodes.clear();
        }
        self.inodes.insert((tree, ino), n.clone());
        Ok(n)
    }

    fn list(&mut self, tree: u64, ino: u64) -> Result<Arc<Vec<Child>>> {
        if let Some(d) = self.dirs.get(&(tree, ino)) {
            return Ok(d.clone());
        }
        let mut out = Vec::new();
        for (_, d) in self.items(tree, (ino, DIR_INDEX, 0), (ino, DIR_INDEX, u64::MAX))? {
            if d.len() < 30 {
                return Err(damaged("a folder entry is short"));
            }
            let loc = key(&d, 0);
            let name_len = le16(&d, 27) as usize;
            let name = d.get(30..30 + name_len).ok_or_else(|| damaged("a folder entry's name is cut"))?;
            let to = match loc.1 {
                INODE_ITEM => (tree, loc.0),
                // A subvolume: its own tree, from its top folder.
                ROOT_ITEM => match self.trees.get(&loc.0) {
                    Some(r) => (loc.0, r.dirid),
                    None => continue, // deleted
                },
                _ => continue,
            };
            out.push(Child { name: String::from_utf8_lossy(name).replace('/', "_"), to });
        }
        let out = Arc::new(out);
        if self.dirs.len() >= CACHE {
            self.dirs.clear();
        }
        self.dirs.insert((tree, ino), out.clone());
        Ok(out)
    }

    fn resolve(&mut self, path: &VfsPath) -> Result<(u64, u64)> {
        let mut at = (FS_TREE, self.trees[&FS_TREE].dirid);
        for name in path.components() {
            let list = self.list(at.0, at.1)?;
            at = list.iter().find(|c| c.name == name).ok_or_else(|| Error::not_found(path))?.to;
        }
        Ok(at)
    }

    fn file_extents(&mut self, n: &Inode) -> Result<Arc<Vec<Extent>>> {
        let id = (n.tree, n.ino);
        if let Some(e) = self.extents.get(&id) {
            return Ok(e.clone());
        }
        let mut out = Vec::new();
        for (k, d) in self.items(n.tree, (n.ino, EXTENT_DATA, 0), (n.ino, EXTENT_DATA, u64::MAX))? {
            if d.len() < 21 {
                return Err(damaged("a file extent is short"));
            }
            let ram = le64(&d, 8);
            let compression = d[16];
            if d[17] != 0 {
                return Err(Error::unsupported("encrypted Btrfs files"));
            }
            match d[20] {
                0 => {
                    let raw = &d[21..];
                    let bytes = if compression == 0 { raw.to_vec() } else { self.unpack(compression, raw, ram)? };
                    out.push(Extent { file_off: k.2, len: bytes.len() as u64, data: Data::Inline(Arc::new(bytes)) });
                }
                t @ (1 | 2) => {
                    if d.len() < 53 {
                        return Err(damaged("a file extent is short"));
                    }
                    let (at, disk_len, offset, len) = (le64(&d, 21), le64(&d, 29), le64(&d, 37), le64(&d, 45));
                    let data = if t == 2 || at == 0 {
                        Data::Zeros
                    } else {
                        Data::Disk { at, disk_len, ram, compression, offset }
                    };
                    out.push(Extent { file_off: k.2, len, data });
                }
                t => return Err(damaged(format!("unknown file extent type {t}"))),
            }
        }
        let out = Arc::new(out);
        if self.extents.len() >= 64 {
            self.extents.clear();
        }
        self.extents.insert(id, out.clone());
        Ok(out)
    }

    fn unpack(&self, compression: u8, src: &[u8], ram: u64) -> Result<Vec<u8>> {
        let ram = ram as usize;
        let mut out = match compression {
            1 => miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(src, ram.max(1))
                .or_else(|e| {
                    if e.status == miniz_oxide::inflate::TINFLStatus::HasMoreOutput { Ok(e.output) } else { Err(e) }
                })
                .map_err(|e| damaged(format!("zlib data: {e}")))?,
            2 => self.unlzo(src, ram)?,
            3 => {
                let mut out = Vec::with_capacity(ram);
                let mut input = src;
                let mut dec = ruzstd::decoding::StreamingDecoder::new(&mut input)
                    .map_err(|e| damaged(format!("zstd data: {e}")))?;
                let mut chunk = vec![0u8; 65536];
                while out.len() < ram {
                    let n =
                        std::io::Read::read(&mut dec, &mut chunk).map_err(|e| damaged(format!("zstd data: {e}")))?;
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&chunk[..n]);
                }
                out
            }
            c => return Err(Error::unsupported(format!("Btrfs compression {c}"))),
        };
        out.resize(ram, 0);
        Ok(out)
    }

    /// Btrfs's LZO framing: the total length, then segments (length, LZO1X
    /// data of at most a sector) whose length never straddles a sector end.
    fn unlzo(&self, src: &[u8], ram: usize) -> Result<Vec<u8>> {
        let sector = self.sectorsize as usize;
        let total = (src.len() >= 4).then(|| le32(src, 0) as usize).ok_or_else(|| damaged("LZO data is short"))?;
        let total = total.min(src.len());
        let mut pos = 4;
        let mut out = Vec::with_capacity(ram);
        while pos < total && out.len() < ram {
            if sector - pos % sector < 4 {
                pos = pos.next_multiple_of(sector);
                continue;
            }
            let len = le32(src, pos) as usize;
            pos += 4;
            let seg = src.get(pos..pos + len).ok_or_else(|| damaged("an LZO segment is cut"))?;
            out.extend(crate::lzo::lzo1x(seg, sector.max(4096))?);
            pos += len;
        }
        Ok(out)
    }

    fn read(&mut self, n: &Inode, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= n.size {
            return Ok(0);
        }
        let want = (n.size - offset).min(buf.len() as u64) as usize;
        let ext = self.file_extents(n)?;
        let mut done = 0;
        while done < want {
            let pos = offset + done as u64;
            let i = ext.partition_point(|e| e.file_off + e.len <= pos);
            match ext.get(i).filter(|e| e.file_off <= pos) {
                Some(e) => {
                    let k = ((e.file_off + e.len - pos) as usize).min(want - done);
                    let within = pos - e.file_off;
                    let dst = &mut buf[done..done + k];
                    match &e.data {
                        Data::Inline(b) => {
                            let from = within as usize;
                            let have = b.len().saturating_sub(from).min(k);
                            dst[..have].copy_from_slice(&b[from..from + have]);
                            dst[have..].fill(0);
                        }
                        Data::Zeros => dst.fill(0),
                        Data::Disk { at, compression: 0, offset, .. } => {
                            self.read_logical(at + offset + within, dst)?
                        }
                        Data::Disk { at, disk_len, ram, compression, offset } => {
                            let bytes = match &self.unpacked {
                                Some((a, b)) if a == at => b.clone(),
                                _ => {
                                    let mut raw = vec![0u8; *disk_len as usize];
                                    self.read_logical(*at, &mut raw)?;
                                    let b = Arc::new(self.unpack(*compression, &raw, *ram)?);
                                    self.unpacked = Some((*at, b.clone()));
                                    b
                                }
                            };
                            let from = (offset + within) as usize;
                            let src =
                                bytes.get(from..from + k).ok_or_else(|| damaged("a compressed extent is short"))?;
                            dst.copy_from_slice(src);
                        }
                    }
                    done += k;
                }
                None => {
                    // A hole (not stored with the "no holes" feature).
                    let end = ext.get(i).map_or(u64::MAX, |e| e.file_off);
                    let k = (end - pos).min((want - done) as u64) as usize;
                    buf[done..done + k].fill(0);
                    done += k;
                }
            }
        }
        Ok(want)
    }

    fn metadata(&mut self, n: &Inode) -> Result<Metadata> {
        let kind = n.kind();
        let mut m = Metadata::new(kind, if kind == EntryKind::Directory { 0 } else { n.size });
        m.modified = Some(n.mtime);
        m.accessed = Some(n.atime);
        m.created = Some(n.otime);
        m.mode = Some(n.mode);
        m.uid = Some(n.uid);
        m.gid = Some(n.gid);
        // Inode numbers start again in each subvolume.
        m.inode = Some(if n.tree == FS_TREE { n.ino } else { (n.tree << 40) ^ n.ino });
        if kind == EntryKind::Symlink {
            let mut b = vec![0u8; n.size.min(1 << 16) as usize];
            let k = self.read(n, 0, &mut b)?;
            m.symlink_target = Some(String::from_utf8_lossy(&b[..k]).into_owned());
        }
        Ok(m)
    }
}

/// The free space of a Btrfs volume on its device, as byte ranges: what a
/// copy of only the used space leaves out. Used is every extent the extent
/// tree lists (data, and tree nodes), on each copy of its chunk on this
/// device; the device's first MiB and the superblock copies are kept too.
/// Free is the rest: unused space in chunks, and space in no chunk. None
/// when that cannot be trusted: a log tree waiting to be replayed (its
/// blocks are not in the extent tree), or data striped over devices.
pub fn free_space(dev: Box<dyn BlockDevice>) -> Result<Option<Vec<(u64, u64)>>> {
    let size = dev.size();
    let mut v = Volume::open(dev)?;
    if v.log_root != 0 || v.chunks.iter().any(|c| c.striped) {
        return Ok(None);
    }
    let mut roots = Vec::new();
    let root_tree = v.root_tree;
    v.collect(root_tree, (2, ROOT_ITEM, 0), (2, ROOT_ITEM, u64::MAX), &mut roots, 0)?;
    let Some((_, root)) = roots.pop() else { return Ok(None) };
    if root.len() < 239 {
        return Err(damaged("the extent tree's root item is short"));
    }
    let mut items = Vec::new();
    v.collect(le64(&root, 176), (0, 0, 0), (u64::MAX, u8::MAX, u64::MAX), &mut items, 0)?;
    let mut used: Vec<(u64, u64)> = vec![(0, 1 << 20)];
    for at in [SUPER_AT, 64 << 20, 256 << 30] {
        used.push((at, at + 4096));
    }
    for (k, _) in items {
        let (start, len) = match k.1 {
            168 => (k.0, k.2),        // EXTENT_ITEM: offset is the length
            169 => (k.0, v.nodesize), // METADATA_ITEM (skinny): a tree node
            _ => continue,
        };
        let i = v.chunks.partition_point(|c| c.logical + c.length <= start);
        let Some(c) = v.chunks.get(i).filter(|c| c.logical <= start) else {
            return Err(damaged(format!("extent {start} is in no chunk")));
        };
        for &copy in v.copies.get(&c.logical).into_iter().flatten() {
            let at = copy + (start - c.logical);
            used.push((at, at + len));
        }
    }
    Ok(Some(dix_core::imaging::complement(&used, size)))
}

pub struct Btrfs {
    v: Volume,
}

impl FileSystem for Btrfs {
    fn info(&self) -> FsInfo {
        let v = &self.v;
        let csum = match v.csum {
            0 => "crc32c",
            1 => "xxhash64",
            2 => "sha256",
            3 => "blake2b",
            _ => "unknown",
        };
        let mut details = vec![
            ("Subvolumes".to_string(), (v.trees.len() - 1).to_string()),
            ("Node size".into(), format!("{} bytes", v.nodesize)),
            ("Checksums".into(), csum.into()),
        ];
        if !v.features.is_empty() {
            details.push(("Features".into(), v.features.join(", ")));
        }
        FsInfo {
            driver_id: "btrfs".into(),
            fs_name: "Btrfs".into(),
            label: (!v.label.is_empty()).then(|| v.label.clone()),
            total_bytes: Some(v.total),
            free_bytes: Some(v.total.saturating_sub(v.used)),
            block_size: Some(v.sectorsize as u32),
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
        let (tree, ino) = self.v.resolve(path)?;
        let n = self.v.inode(tree, ino)?;
        if n.kind() != EntryKind::Directory {
            return Err(Error::not_a_directory(path));
        }
        let list = self.v.list(tree, ino)?;
        let mut out = Vec::with_capacity(list.len());
        for c in list.iter() {
            let n = self.v.inode(c.to.0, c.to.1)?;
            out.push(DirEntry { name: c.name.clone(), metadata: self.v.metadata(&n)? });
        }
        Ok(out)
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        let (tree, ino) = self.v.resolve(path)?;
        let n = self.v.inode(tree, ino)?;
        self.v.metadata(&n)
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let (tree, ino) = self.v.resolve(path)?;
        let n = self.v.inode(tree, ino)?;
        match n.kind() {
            EntryKind::Directory => Err(Error::is_a_directory(path)),
            EntryKind::File => self.v.read(&n, offset, buf),
            _ => Ok(0),
        }
    }
}
