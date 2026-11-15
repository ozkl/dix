//! APFS (Apple File System), read-only.
//!
//! A *container* (block 0 and the checkpoint area: the superblock `NXSB`
//! with the highest transaction ID wins) holds one or more *volumes*
//! (`APSB`). Objects are found by ID through object maps: the container's
//! maps the volumes' superblocks, each volume's maps the nodes of its file
//! system tree. Every object starts with a Fletcher-64 checksum, checked
//! here. See [`btree`] for nodes.
//!
//! A volume's file system tree holds records keyed by (object ID, type):
//! inodes, directory entries, extended attributes and file extents. A
//! directory's entries are the records (its ID, entry); a file's data
//! is the extents of its data stream.
//!
//! What is read: files (sparse ones too), folders, symbolic links, hard
//! links, owners, modes and the four dates (to the nanosecond, kept to the
//! second), extended attributes as data streams or inline, files compressed
//! by macOS (see [`crate::decmpfs`]; the resource fork is an attribute in
//! APFS), case-insensitive and case-sensitive volumes, and sealed system
//! volumes (macOS 11 and later). A container with several volumes shows
//! each as a folder. FileVault-encrypted volumes are listed but cannot be
//! opened; snapshots are not shown (only the live file system).

mod btree;
#[cfg(all(test, unix))]
mod tests;

use btree::Node;
use dix_core::{
    AccessMode, BlockDevice, Capabilities, DirEntry, Driver, DriverInfo, EntryKind, Error, ErrorKind, FileSystem,
    FsInfo, Metadata, MountOptions, ProbeScore, Result, VfsPath,
};
use std::collections::HashMap;
use std::sync::Arc;

const CAPS: Capabilities =
    Capabilities::READ.union(Capabilities::SYMLINKS).union(Capabilities::PERMISSIONS).union(Capabilities::TIMESTAMPS);

pub(crate) fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
pub(crate) fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn damaged(what: impl std::fmt::Display) -> Error {
    Error::corrupted(what)
}

/// Fletcher-64 over 32-bit words, as APFS checks its objects.
fn fletcher64(data: &[u8]) -> u64 {
    let (mut a, mut b) = (0u64, 0u64);
    for w in data.chunks_exact(4) {
        a = (a + u32::from_le_bytes(w.try_into().unwrap()) as u64) % 0xFFFF_FFFF;
        b = (b + a) % 0xFFFF_FFFF;
    }
    let c1 = 0xFFFF_FFFF - ((a + b) % 0xFFFF_FFFF);
    let c2 = 0xFFFF_FFFF - ((a + c1) % 0xFFFF_FFFF);
    (c2 << 32) | c1
}

fn valid(block: &[u8]) -> bool {
    block.len() > 32 && le64(block, 0) == fletcher64(&block[8..]) && block[8..].iter().any(|&b| b != 0)
}

// Object types (low 16 bits of o_type).
const OBJECT_TYPE_NX_SUPERBLOCK: u32 = 1;
const OBJECT_TYPE_FS: u32 = 13;
/// Storage kind of a tree's nodes (high bits of the tree type).
const OBJ_PHYSICAL: u32 = 0x4000_0000;

// File system record types.
const T_INODE: u8 = 3;
const T_XATTR: u8 = 4;
const T_FILE_EXTENT: u8 = 8;
const T_DIR_REC: u8 = 9;

// Volume flags and features.
const FS_UNENCRYPTED: u64 = 1;
const INCOMPAT_CASE_INSENSITIVE: u64 = 1;
const INCOMPAT_NORMALIZATION_INSENSITIVE: u64 = 8;
const INCOMPAT_SEALED: u64 = 0x20;

const ROOT_DIR: u64 = 2;
const UF_COMPRESSED: u32 = 0x20;

pub struct ApfsDriver;

impl Driver for ApfsDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo { id: "apfs", name: "APFS", description: "Apple File System (read-only)", capabilities: CAPS }
    }

    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
        if dev.size() < 4096 {
            return Ok(0);
        }
        let mut b = vec![0u8; 4096];
        dev.read_at(0, &mut b)?;
        if &b[32..36] != b"NXSB" {
            return Ok(0);
        }
        let bs = le32(&b, 36) as usize;
        if (4096..=65536).contains(&bs) && bs.is_power_of_two() {
            let mut blk = vec![0u8; bs];
            if dev.size() >= bs as u64 {
                dev.read_at(0, &mut blk)?;
                if valid(&blk) {
                    return Ok(95);
                }
            }
        }
        Ok(30)
    }

    fn mount(&self, dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
        if opts.mode == AccessMode::ReadWrite {
            opts.log.info("APFS is read-only in this version: the volume is opened read-only.");
        }
        let c = Container::open(dev, &opts.log)?;
        Ok(Box::new(ApfsFs { c }))
    }
}

// ---------------------------------------------------------------------------
// The container.

/// What a tree scan calls on each record (key, value); false stops it.
type Visit<'a> = dyn FnMut(&[u8], &[u8]) -> Result<bool> + 'a;

/// A B-tree: its root and how its child pointers resolve.
#[derive(Clone, Copy)]
struct Tree {
    root: u64,
    /// Child IDs are block addresses (else virtual, through `omap`).
    physical: bool,
    /// The object map for virtual IDs (its tree's root block).
    omap: u64,
    fixed: Option<(usize, usize)>,
}

struct Volume {
    name: String,
    role: u16,
    tree: Tree,
    /// Sealed volumes keep file extents in a tree of their own.
    fext: Option<Tree>,
    encrypted: bool,
    case_insensitive: bool,
    hashed: bool,
    sealed: bool,
    files: u64,
    folders: u64,
}

pub(crate) struct Container {
    dev: Box<dyn BlockDevice>,
    bs: u64,
    blocks: u64,
    xid: u64,
    free: Option<u64>,
    vols: Vec<Volume>,
    nodes: HashMap<u64, Arc<Node>>,
    inodes: HashMap<(usize, u64), Arc<Inode>>,
    dirs: HashMap<(usize, u64), Arc<Vec<Dirent>>>,
    extents: HashMap<(usize, u64), Arc<Vec<Extent>>>,
    chunks: crate::decmpfs::ChunkCache,
}

#[derive(Clone, Debug)]
struct Dirent {
    name: String,
    id: u64,
}

#[derive(Clone, Copy, Debug)]
struct Extent {
    logical: u64,
    len: u64,
    /// 0: a hole (zeros).
    block: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Inode {
    id: u64,
    private_id: u64,
    created: i64,
    modified: i64,
    accessed: i64,
    bsd_flags: u32,
    uid: u32,
    gid: u32,
    mode: u16,
    size: u64,
}

fn secs(ns: u64) -> i64 {
    (ns / 1_000_000_000) as i64
}

impl Container {
    fn open(mut dev: Box<dyn BlockDevice>, log: &dix_core::MountLog) -> Result<Self> {
        let mut b0 = vec![0u8; 4096];
        dev.read_at(0, &mut b0)?;
        if &b0[32..36] != b"NXSB" {
            return Err(damaged("not an APFS container"));
        }
        let bs = le32(&b0, 36) as u64;
        if !(4096..=65536).contains(&bs) || !bs.is_power_of_two() {
            return Err(damaged(format!("a block size of {bs}")));
        }
        let mut c = Container {
            dev,
            bs,
            blocks: le64(&b0, 40),
            xid: 0,
            free: None,
            vols: Vec::new(),
            nodes: HashMap::new(),
            inodes: HashMap::new(),
            dirs: HashMap::new(),
            extents: HashMap::new(),
            chunks: Default::default(),
        };
        let first = c.block(0)?;
        // The newest valid superblock in the checkpoint descriptor area.
        let mut sb = if valid(&first) { Some(first.clone()) } else { None };
        let (desc_blocks, desc_base) = (le32(&first, 104), le64(&first, 112));
        let mut desc_at = 0u32;
        if desc_blocks & 0x8000_0000 == 0 && desc_blocks < 1 << 20 {
            for i in 0..desc_blocks {
                let Ok(b) = c.block(desc_base + i as u64) else { continue };
                if valid(&b)
                    && le32(&b, 24) & 0xFFFF == OBJECT_TYPE_NX_SUPERBLOCK
                    && &b[32..36] == b"NXSB"
                    && sb.as_ref().is_none_or(|s| le64(&b, 16) > le64(s, 16))
                {
                    sb = Some(b);
                    desc_at = i;
                }
            }
        }
        let sb = sb.ok_or_else(|| damaged("no valid container superblock"))?;
        c.xid = le64(&sb, 16);
        c.blocks = le64(&sb, 40);
        let incompat = le64(&sb, 64);
        if incompat & 0x100 != 0 {
            return Err(Error::unsupported("Fusion drives"));
        }
        c.free = c.free_blocks(&sb, desc_base, desc_blocks, desc_at).ok();
        let omap = c.omap_root(le64(&sb, 160))?;
        let max = (le32(&sb, 180) as usize).min(100);
        for i in 0..max {
            let oid = le64(&sb, 184 + i * 8);
            if oid == 0 {
                continue;
            }
            match c.volume(omap, oid) {
                Ok(v) => c.vols.push(v),
                Err(e) => log.warn(format!("Volume {}: {e}", i + 1)),
            }
        }
        if c.vols.is_empty() {
            return Err(damaged("the container has no volume that can be read"));
        }
        for v in &c.vols {
            if v.encrypted {
                log.warn(format!("“{}” is encrypted (FileVault): its files cannot be read.", v.name));
            }
            if v.sealed {
                log.info(format!("“{}” is a sealed system volume.", v.name));
            }
        }
        Ok(c)
    }

    /// Block `n`, as it is.
    fn block(&mut self, n: u64) -> Result<Vec<u8>> {
        let mut b = vec![0u8; self.bs as usize];
        self.dev.read_at(n * self.bs, &mut b)?;
        Ok(b)
    }

    /// Block `n`, which must be a valid object.
    fn object(&mut self, n: u64) -> Result<Vec<u8>> {
        let b = self.block(n)?;
        if !valid(&b) {
            return Err(damaged(format!("the object at block {n} fails its checksum")));
        }
        Ok(b)
    }

    /// The root of an object map's tree.
    fn omap_root(&mut self, omap: u64) -> Result<u64> {
        let o = self.object(omap)?;
        Ok(le64(&o, 48))
    }

    fn node(&mut self, n: u64, fixed: Option<(usize, usize)>) -> Result<Arc<Node>> {
        if let Some(node) = self.nodes.get(&n) {
            return Ok(node.clone());
        }
        let b = self.object(n)?;
        // A fixed-size tree's sizes are in its root's tree info.
        let fixed = if le16(&b, 32) & btree::ROOT != 0 && le16(&b, 32) & btree::FIXED != 0 {
            Some(Node::info_sizes(&b))
        } else {
            fixed
        };
        let node = Arc::new(Node::parse(b, fixed)?);
        if self.nodes.len() > 1024 {
            self.nodes.clear();
        }
        self.nodes.insert(n, node.clone());
        Ok(node)
    }

    /// Looks `oid` up in an object map (the newest version not after the
    /// container's transaction): its block.
    fn omap_lookup(&mut self, root: u64, oid: u64) -> Result<u64> {
        let xid = self.xid;
        let mut at = root;
        let mut fixed = None;
        for _ in 0..16 {
            let node = self.node(at, fixed)?;
            fixed = node.fixed_sizes();
            let mut best: Option<&[u8]> = None;
            for i in 0..node.count {
                let (k, v) = node.entry(i)?;
                let (ko, kx) = (le64(k, 0), le64(k, 8));
                if (ko, kx) > (oid, xid) {
                    break;
                }
                if node.is_leaf() && ko != oid {
                    continue;
                }
                best = Some(v);
            }
            let v = best.ok_or_else(|| damaged(format!("object {oid} is not in the object map")))?;
            if node.is_leaf() {
                if le32(v, 0) & 1 != 0 {
                    return Err(damaged(format!("object {oid} was deleted")));
                }
                return Ok(le64(v, 8));
            }
            at = le64(v, 0);
        }
        Err(damaged("the object map is too deep"))
    }

    fn volume(&mut self, omap: u64, oid: u64) -> Result<Volume> {
        let at = self.omap_lookup(omap, oid)?;
        let b = self.object(at)?;
        if &b[32..36] != b"APSB" || le32(&b, 24) & 0xFFFF != OBJECT_TYPE_FS {
            return Err(damaged("a volume superblock is missing"));
        }
        let incompat = le64(&b, 56);
        let name_bytes = &b[704..960];
        let end = name_bytes.iter().position(|&c| c == 0).unwrap_or(256);
        let name = String::from_utf8_lossy(&name_bytes[..end]).into_owned();
        let vomap = self.omap_root(le64(&b, 128))?;
        let tree_type = le32(&b, 116);
        let physical = tree_type & OBJ_PHYSICAL != 0;
        let root_oid = le64(&b, 136);
        let root = if physical { root_oid } else { self.omap_lookup(vomap, root_oid)? };
        let sealed = incompat & INCOMPAT_SEALED != 0;
        let fext = if sealed && b.len() > 1048 && le64(&b, 1032) != 0 {
            Some(Tree { root: le64(&b, 1032), physical: true, omap: vomap, fixed: None })
        } else {
            None
        };
        Ok(Volume {
            name,
            role: le16(&b, 964),
            tree: Tree { root, physical, omap: vomap, fixed: None },
            fext,
            encrypted: le64(&b, 264) & FS_UNENCRYPTED == 0,
            case_insensitive: incompat & INCOMPAT_CASE_INSENSITIVE != 0,
            hashed: incompat & (INCOMPAT_CASE_INSENSITIVE | INCOMPAT_NORMALIZATION_INSENSITIVE) != 0,
            sealed,
            files: le64(&b, 184),
            folders: le64(&b, 192),
        })
    }

    /// Free blocks, from the space manager (an ephemeral object, found
    /// through the checkpoint map before the superblock).
    fn free_blocks(&mut self, sb: &[u8], base: u64, blocks: u32, at: u32) -> Result<u64> {
        let sm_oid = le64(sb, 152);
        let (index, len) = (le32(sb, 136), le32(sb, 140));
        let _ = at;
        for k in 0..len.saturating_sub(1) {
            let b = self.object(base + ((index + k) % blocks.max(1)) as u64)?;
            if le32(&b, 24) & 0xFFFF != 0x0C {
                continue; // not a checkpoint map
            }
            let count = le32(&b, 36) as usize;
            for e in 0..count.min((b.len() - 40) / 40) {
                let m = &b[40 + e * 40..80 + e * 40];
                if le64(m, 24) == sm_oid {
                    let sm = self.object(le64(m, 32))?;
                    return Ok(le64(&sm, 72));
                }
            }
        }
        Err(damaged("the space manager is not in the checkpoint map"))
    }

    /// Calls `f` on every record of a volume tree whose key `cmp` says is
    /// in range (`Equal`), in order, until it returns false.
    fn scan(&mut self, tree: Tree, cmp: &dyn Fn(&[u8]) -> std::cmp::Ordering, f: &mut Visit<'_>) -> Result<()> {
        self.walk(tree, tree.root, tree.fixed, cmp, f, 0).map(|_| ())
    }

    /// Returns false once `f` asked to stop or the range is past.
    fn walk(
        &mut self,
        tree: Tree,
        at: u64,
        fixed: Option<(usize, usize)>,
        cmp: &dyn Fn(&[u8]) -> std::cmp::Ordering,
        f: &mut Visit<'_>,
        depth: u32,
    ) -> Result<bool> {
        use std::cmp::Ordering::*;
        if depth > 16 {
            return Err(damaged("an APFS tree is too deep"));
        }
        let node = self.node(at, fixed)?;
        let fixed = node.fixed_sizes();
        if node.is_leaf() {
            for i in 0..node.count {
                let (k, v) = node.entry(i)?;
                match cmp(k) {
                    Less => {}
                    Equal => {
                        if !f(k, v)? {
                            return Ok(false);
                        }
                    }
                    Greater => return Ok(false),
                }
            }
            return Ok(true);
        }
        for i in 0..node.count {
            let (k, v) = node.entry(i)?;
            if cmp(k) == Greater {
                return Ok(false);
            }
            // Skip a child whose keys all come before the range.
            if i + 1 < node.count && cmp(node.entry(i + 1)?.0) == Less {
                continue;
            }
            let child = le64(v, 0);
            let child = if tree.physical { child } else { self.omap_lookup(tree.omap, child)? };
            if !self.walk(tree, child, fixed, cmp, f, depth + 1)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    // --- File system records ---------------------------------------------

    fn inode(&mut self, vol: usize, id: u64) -> Result<Arc<Inode>> {
        if let Some(i) = self.inodes.get(&(vol, id)) {
            return Ok(i.clone());
        }
        let tree = self.vols[vol].tree;
        let mut found = None;
        self.scan(tree, &|k| key_cmp(k, id, T_INODE), &mut |_, v| {
            found = Some(v.to_vec());
            Ok(false)
        })?;
        let v = found.ok_or_else(|| Error::not_found(format!("inode {id}")))?;
        let ino = Arc::new(parse_inode(id, &v)?);
        if self.inodes.len() > 4096 {
            self.inodes.clear();
        }
        self.inodes.insert((vol, id), ino.clone());
        Ok(ino)
    }

    fn list(&mut self, vol: usize, dir: u64) -> Result<Arc<Vec<Dirent>>> {
        if let Some(d) = self.dirs.get(&(vol, dir)) {
            return Ok(d.clone());
        }
        let tree = self.vols[vol].tree;
        let hashed = self.vols[vol].hashed;
        let mut out = Vec::new();
        self.scan(tree, &|k| key_cmp(k, dir, T_DIR_REC), &mut |k, v| {
            let name = if hashed {
                let n = (le32(k, 8) & 0x3FF) as usize;
                k.get(12..12 + n)
            } else {
                let n = le16(k, 8) as usize;
                k.get(10..10 + n)
            }
            .ok_or_else(|| damaged("a directory entry is cut short"))?;
            let name = name.strip_suffix(&[0]).unwrap_or(name);
            out.push(Dirent { name: nfc(&String::from_utf8_lossy(name)), id: le64(v, 0) });
            Ok(true)
        })?;
        let out = Arc::new(out);
        if self.dirs.len() > 256 {
            self.dirs.clear();
        }
        self.dirs.insert((vol, dir), out.clone());
        Ok(out)
    }

    /// The extents of a data stream.
    fn extents(&mut self, vol: usize, stream: u64) -> Result<Arc<Vec<Extent>>> {
        if let Some(e) = self.extents.get(&(vol, stream)) {
            return Ok(e.clone());
        }
        let mut out = Vec::new();
        let mut push = |logical: u64, lf: u64, block: u64| {
            out.push(Extent { logical, len: lf & 0x00FF_FFFF_FFFF_FFFF, block });
            Ok(true)
        };
        match self.vols[vol].fext {
            Some(fext) => {
                self.scan(fext, &|k| le64(k, 0).cmp(&stream), &mut |k, v| push(le64(k, 8), le64(v, 0), le64(v, 8)))?
            }
            None => {
                let tree = self.vols[vol].tree;
                self.scan(tree, &|k| key_cmp(k, stream, T_FILE_EXTENT), &mut |k, v| {
                    push(le64(k, 8), le64(v, 0), le64(v, 8))
                })?
            }
        }
        out.sort_by_key(|e| e.logical);
        let out = Arc::new(out);
        if self.extents.len() > 256 {
            self.extents.clear();
        }
        self.extents.insert((vol, stream), out.clone());
        Ok(out)
    }

    /// Reads a data stream of `size` bytes.
    fn read_stream(&mut self, vol: usize, stream: u64, size: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= size {
            return Ok(0);
        }
        if self.vols[vol].encrypted {
            return Err(Error::unsupported("reading FileVault-encrypted volumes"));
        }
        let want = buf.len().min((size - offset) as usize);
        let ext = self.extents(vol, stream)?;
        buf[..want].fill(0);
        let first = ext.partition_point(|e| e.logical + e.len <= offset);
        for e in &ext[first..] {
            let end = offset + want as u64;
            if e.logical >= end {
                break;
            }
            let a = e.logical.max(offset);
            let b = (e.logical + e.len).min(end);
            if a >= b || e.block == 0 {
                continue;
            }
            let disk = e.block * self.bs + (a - e.logical);
            self.dev.read_at(disk, &mut buf[(a - offset) as usize..(b - offset) as usize])?;
        }
        Ok(want)
    }

    /// An extended attribute: inline data, or (stream id, size).
    fn xattr(&mut self, vol: usize, id: u64, name: &str) -> Result<Option<Xattr>> {
        let tree = self.vols[vol].tree;
        let mut found = None;
        self.scan(tree, &|k| key_cmp(k, id, T_XATTR), &mut |k, v| {
            let n = le16(k, 8) as usize;
            let kname = k.get(10..10 + n).unwrap_or(&[]);
            let kname = kname.strip_suffix(&[0]).unwrap_or(kname);
            if kname == name.as_bytes() {
                let (flags, len) = (le16(v, 0), le16(v, 2) as usize);
                let data = v.get(4..4 + len).ok_or_else(|| damaged("an attribute is cut short"))?;
                found = Some(if flags & 1 != 0 {
                    Xattr::Stream { id: le64(data, 0), size: le64(data, 8) }
                } else {
                    Xattr::Inline(data.to_vec())
                });
                return Ok(false);
            }
            Ok(true)
        })?;
        Ok(found)
    }

    fn xattr_bytes(&mut self, vol: usize, id: u64, name: &str) -> Result<Option<Vec<u8>>> {
        match self.xattr(vol, id, name)? {
            None => Ok(None),
            Some(Xattr::Inline(d)) => Ok(Some(d)),
            Some(Xattr::Stream { id, size }) => {
                if size > 1 << 30 {
                    return Err(damaged("an attribute is too large"));
                }
                let mut b = vec![0u8; size as usize];
                self.read_stream(vol, id, size, 0, &mut b)?;
                Ok(Some(b))
            }
        }
    }

    fn decmpfs(&mut self, vol: usize, ino: &Inode) -> Result<Option<crate::decmpfs::Compressed>> {
        if ino.bsd_flags & UF_COMPRESSED == 0 {
            return Ok(None);
        }
        match self.xattr_bytes(vol, ino.id, "com.apple.decmpfs")? {
            Some(x) => crate::decmpfs::Compressed::parse(x).map(Some),
            None => Ok(None),
        }
    }

    fn metadata(&mut self, vol: usize, ino: &Inode) -> Metadata {
        let kind = match ino.mode & 0o170000 {
            0o040000 => EntryKind::Directory,
            0o120000 => EntryKind::Symlink,
            0o100000 | 0 => EntryKind::File,
            _ => EntryKind::Other,
        };
        let mut size = if kind == EntryKind::Directory { 0 } else { ino.size };
        if kind == EntryKind::File
            && let Ok(Some(c)) = self.decmpfs(vol, ino)
        {
            size = c.size;
        }
        let mut m = Metadata::new(kind, size);
        m.created = Some(secs(ino.created as u64));
        m.modified = Some(secs(ino.modified as u64));
        m.accessed = Some(secs(ino.accessed as u64));
        m.mode = Some(ino.mode as u32);
        m.uid = Some(ino.uid);
        m.gid = Some(ino.gid);
        m.inode = Some(ino.id);
        if kind == EntryKind::Symlink
            && let Ok(Some(t)) = self.xattr_bytes(vol, ino.id, "com.apple.fs.symlink")
        {
            let t = t.strip_suffix(&[0]).unwrap_or(&t);
            m.symlink_target = Some(String::from_utf8_lossy(t).into_owned());
            m.size = t.len() as u64;
        }
        // UF_HIDDEN
        m.hidden = ino.bsd_flags & 0x8000 != 0;
        m
    }
}

enum Xattr {
    Inline(Vec<u8>),
    Stream { id: u64, size: u64 },
}

/// Compares a file system key with (oid, type), ignoring what follows.
fn key_cmp(k: &[u8], oid: u64, ty: u8) -> std::cmp::Ordering {
    if k.len() < 8 {
        return std::cmp::Ordering::Less;
    }
    let h = le64(k, 0);
    (h & 0x0FFF_FFFF_FFFF_FFFF, (h >> 60) as u8).cmp(&(oid, ty))
}

fn nfc(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfc().collect()
}

fn parse_inode(id: u64, v: &[u8]) -> Result<Inode> {
    if v.len() < 92 {
        return Err(damaged("an inode record is cut short"));
    }
    let mut ino = Inode {
        id,
        private_id: le64(v, 8),
        created: le64(v, 16) as i64,
        modified: le64(v, 24) as i64,
        accessed: le64(v, 40) as i64,
        bsd_flags: le32(v, 68),
        uid: le32(v, 72),
        gid: le32(v, 76),
        mode: le16(v, 80),
        size: 0,
    };
    // Extended fields: a count, their headers, then their data (each
    // padded to 8 bytes). The data stream's size is one of them.
    if v.len() >= 96 {
        let n = le16(v, 92) as usize;
        let mut data = 96 + n * 4;
        for i in 0..n {
            let h = 96 + i * 4;
            if h + 4 > v.len() {
                break;
            }
            let (ty, size) = (v[h], le16(v, h + 2) as usize);
            if ty == 8 && data + 8 <= v.len() {
                ino.size = le64(v, data);
            }
            data += size.div_ceil(8) * 8;
        }
    }
    Ok(ino)
}

// ---------------------------------------------------------------------------
// The file system: one volume at the root, or one folder per volume.

struct ApfsFs {
    c: Container,
}

impl ApfsFs {
    fn single(&self) -> bool {
        self.c.vols.len() == 1
    }

    /// Folder names for the volumes of a container with several.
    fn volume_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for v in &self.c.vols {
            let mut n = if v.name.is_empty() { "Volume".to_string() } else { v.name.replace('/', ":") };
            while names.contains(&n) {
                n.push('_');
            }
            names.push(n);
        }
        names
    }

    /// The volume and inode a path names; `None` for the container's root
    /// when it shows several volumes.
    fn resolve(&mut self, path: &VfsPath) -> Result<Option<(usize, Arc<Inode>)>> {
        let mut parts: Vec<&str> = path.components().collect();
        let vol = if self.single() {
            0
        } else {
            if parts.is_empty() {
                return Ok(None);
            }
            let names = self.volume_names();
            let first = parts.remove(0);
            names.iter().position(|n| n == first).ok_or_else(|| Error::not_found(path))?
        };
        if self.c.vols[vol].encrypted && !parts.is_empty() {
            return Err(Error::unsupported("reading FileVault-encrypted volumes"));
        }
        let mut ino = self.c.inode(vol, ROOT_DIR)?;
        for part in parts {
            if ino.mode & 0o170000 != 0o040000 {
                return Err(Error::not_a_directory(path));
            }
            let list = self.c.list(vol, ino.id)?;
            let want = nfc(part);
            let ci = self.c.vols[vol].case_insensitive;
            let hit = list.iter().find(|d| d.name == want).or_else(|| {
                if ci {
                    let low = want.to_lowercase();
                    list.iter().find(|d| d.name.to_lowercase() == low)
                } else {
                    None
                }
            });
            let id = hit.ok_or_else(|| Error::not_found(path))?.id;
            ino = self.c.inode(vol, id)?;
        }
        Ok(Some((vol, ino)))
    }
}

impl FileSystem for ApfsFs {
    fn info(&self) -> FsInfo {
        let c = &self.c;
        let main = &c.vols[0];
        let mut details = Vec::new();
        for v in &c.vols {
            let mut what = vec![if v.case_insensitive { "case-insensitive" } else { "case-sensitive" }.to_string()];
            if v.encrypted {
                what.push("encrypted".into());
            }
            if v.sealed {
                what.push("sealed".into());
            }
            if let Some(r) = role_name(v.role) {
                what.push(r.into());
            }
            what.push(format!("{} files, {} folders", v.files, v.folders));
            details.push((format!("Volume “{}”", v.name), what.join(", ")));
        }
        FsInfo {
            driver_id: "apfs".into(),
            fs_name: "APFS".into(),
            label: Some(if self.single() { main.name.clone() } else { "APFS container".into() }),
            total_bytes: Some(c.blocks * c.bs),
            free_bytes: c.free.map(|f| f * c.bs),
            block_size: Some(c.bs as u32),
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
        let Some((vol, dir)) = self.resolve(path)? else {
            // The container: one folder per volume.
            let names = self.volume_names();
            let mut out = Vec::new();
            for (i, name) in names.into_iter().enumerate() {
                let mut m = Metadata::new(EntryKind::Directory, 0);
                if let Ok(root) = self.c.inode(i, ROOT_DIR) {
                    m = self.c.metadata(i, &root);
                }
                out.push(DirEntry { name, metadata: m });
            }
            return Ok(out);
        };
        if dir.mode & 0o170000 != 0o040000 {
            return Err(Error::not_a_directory(path));
        }
        let list = self.c.list(vol, dir.id)?;
        let mut out = Vec::with_capacity(list.len());
        for d in list.iter() {
            let meta = match self.c.inode(vol, d.id) {
                Ok(ino) => self.c.metadata(vol, &ino),
                Err(e) if matches!(e.kind(), ErrorKind::Corrupted | ErrorKind::NotFound) => {
                    Metadata::new(EntryKind::Other, 0)
                }
                Err(e) => return Err(e),
            };
            out.push(DirEntry { name: d.name.clone(), metadata: meta });
        }
        Ok(out)
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        match self.resolve(path)? {
            Some((vol, ino)) => Ok(self.c.metadata(vol, &ino)),
            None => Ok(Metadata::new(EntryKind::Directory, 0)),
        }
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let (vol, ino) = self.resolve(path)?.ok_or_else(|| Error::is_a_directory(path))?;
        if ino.mode & 0o170000 == 0o040000 {
            return Err(Error::is_a_directory(path));
        }
        if let Some(cmp) = self.c.decmpfs(vol, &ino)? {
            let c = &mut self.c;
            let rsrc = if cmp.in_resource_fork() {
                c.xattr(vol, ino.id, "com.apple.ResourceFork")?
                    .ok_or_else(|| damaged("the resource fork is missing"))?
            } else {
                Xattr::Inline(Vec::new())
            };
            let mut cache = std::mem::take(&mut c.chunks);
            let r = match rsrc {
                Xattr::Inline(d) => {
                    let n = d.len() as u64;
                    cmp.read(
                        &mut |o, b| {
                            let o = o as usize;
                            let end = (o + b.len()).min(d.len());
                            let k = end.saturating_sub(o);
                            b[..k].copy_from_slice(&d[o..o + k]);
                            Ok(k)
                        },
                        n,
                        &mut cache,
                        ino.id,
                        offset,
                        buf,
                    )
                }
                Xattr::Stream { id, size } => {
                    cmp.read(&mut |o, b| c.read_stream(vol, id, size, o, b), size, &mut cache, ino.id, offset, buf)
                }
            };
            self.c.chunks = cache;
            return r;
        }
        self.c.read_stream(vol, ino.private_id, ino.size, offset, buf)
    }
}

fn role_name(role: u16) -> Option<&'static str> {
    // The role is a bit field in the low 6 bits, an enumeration above.
    Some(match role {
        0 => return None,
        1 => "system",
        2 => "user",
        4 => "recovery",
        8 => "virtual memory",
        16 => "preboot",
        32 => "installer",
        64 => "data",
        128 => "baseband",
        192 => "update",
        256 => "xART",
        320 => "hardware",
        384 => "backup",
        _ => return None,
    })
}
