//! SimpleFS on-disk encoding.

use dix_core::{BlockDevice, Error, ErrorKind, Result, VfsPath};
use std::collections::BTreeMap;

pub const MAGIC: &[u8; 8] = b"DIXSIMP1";
pub const VERSION: u32 = 1;
pub const HEADER_SIZE: usize = 512;
const LABEL_LEN: usize = 64;
const KIND_FILE: u8 = 1;
const KIND_DIR: u8 = 2;
/// path len + kind + mtime + data len
const ENTRY_OVERHEAD: u64 = 2 + 1 + 8 + 8;

pub fn fnv1a(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in data {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: u8,
    pub mtime: i64,
    pub data: Vec<u8>,
}

impl Node {
    pub fn file(data: Vec<u8>, mtime: i64) -> Self {
        Self { kind: KIND_FILE, mtime, data }
    }
    pub fn dir(mtime: i64) -> Self {
        Self { kind: KIND_DIR, mtime, data: Vec::new() }
    }
    pub fn is_dir(&self) -> bool {
        self.kind == KIND_DIR
    }
}

/// The whole file system in memory. The root is always present.
#[derive(Debug)]
pub struct Tree {
    pub label: String,
    pub created: i64,
    nodes: BTreeMap<VfsPath, Node>,
    /// Encoded payload size, kept up to date by every mutation.
    used: u64,
}

/// Encoded size of one entry.
pub fn entry_len(path: &VfsPath, data_len: usize) -> u64 {
    ENTRY_OVERHEAD + path.as_str().len() as u64 + data_len as u64
}

impl Tree {
    pub fn new(label: String, now: i64) -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(VfsPath::root(), Node::dir(now));
        Self { label, created: now, nodes, used: 0 }
    }

    pub fn get(&self, p: &VfsPath) -> Option<&Node> {
        self.nodes.get(p)
    }

    pub fn insert(&mut self, p: VfsPath, n: Node) {
        self.used += entry_len(&p, n.data.len());
        if let Some(old) = self.nodes.insert(p.clone(), n) {
            self.used -= entry_len(&p, old.data.len());
        }
    }

    /// Removes and returns a node (to change it and insert it again).
    pub fn take(&mut self, p: &VfsPath) -> Option<Node> {
        let old = self.nodes.remove(p)?;
        self.used -= entry_len(p, old.data.len());
        Some(old)
    }

    pub fn remove(&mut self, p: &VfsPath) {
        if let Some(old) = self.nodes.remove(p) {
            self.used -= entry_len(p, old.data.len());
        }
    }

    pub fn touch_parent(&mut self, p: &VfsPath, now: i64) {
        if let Some(n) = p.parent().and_then(|pp| self.nodes.get_mut(&pp)) {
            n.mtime = now;
        }
    }

    /// `dir` and everything below it. Descendants sort right after `dir`
    /// (only names that merely share its string prefix, like `/a-b` after
    /// `/a`, are interleaved), so this visits the subtree, not the volume.
    pub fn subtree<'a>(&'a self, dir: &'a VfsPath) -> impl Iterator<Item = (&'a VfsPath, &'a Node)> + 'a {
        self.nodes
            .range(dir.clone()..)
            .take_while(move |(p, _)| p.as_str().starts_with(dir.as_str()))
            .filter(move |(p, _)| p.starts_with(dir))
    }

    /// Direct children of `dir`.
    pub fn children<'a>(&'a self, dir: &'a VfsPath) -> impl Iterator<Item = (&'a VfsPath, &'a Node)> + 'a {
        let depth = dir.depth() + 1;
        self.subtree(dir).filter(move |(p, _)| p.depth() == depth)
    }

    /// Number of entries at or below `dir`.
    pub fn subtree_len(&self, dir: &VfsPath) -> u64 {
        self.subtree(dir).count() as u64
    }

    pub fn move_subtree(&mut self, from: &VfsPath, to: &VfsPath) {
        let keys: Vec<VfsPath> = self.subtree(from).map(|(p, _)| p.clone()).collect();
        for k in keys {
            if let Some(new) = k.rebase(from, to)
                && let Some(node) = self.nodes.remove(&k)
            {
                self.used -= entry_len(&k, node.data.len());
                self.insert(new, node);
            }
        }
    }

    fn entries(&self) -> impl Iterator<Item = (&VfsPath, &Node)> {
        self.nodes.iter().filter(|(p, _)| !p.is_root())
    }

    pub fn entry_count(&self) -> usize {
        self.nodes.len() - 1
    }

    pub fn encoded_len(&self) -> u64 {
        self.used
    }
}

/// Superblock flag: the volume is mounted read-write. Cleared by a clean
/// unmount; still set after a crash or forced eject.
pub const FLAG_IN_USE: u32 = 1;

pub struct Header {
    pub flags: u32,
    pub entry_count: u32,
    pub payload_len: u64,
    pub payload_sum: u32,
    pub label: String,
    pub created: i64,
}

impl Header {
    pub fn decode(b: &[u8; HEADER_SIZE]) -> Result<Self> {
        if &b[0..8] != MAGIC {
            return Err(Error::new(ErrorKind::UnknownFormat, "not a SimpleFS image"));
        }
        let stored = u32::from_le_bytes(b[508..512].try_into().unwrap());
        if stored != fnv1a(&b[..508]) {
            return Err(Error::corrupted("SimpleFS superblock checksum mismatch"));
        }
        let version = u32::from_le_bytes(b[8..12].try_into().unwrap());
        if version != VERSION {
            return Err(Error::corrupted(format!("unsupported SimpleFS version {version}")));
        }
        let label_raw = &b[32..32 + LABEL_LEN];
        let end = label_raw.iter().position(|&c| c == 0).unwrap_or(LABEL_LEN);
        Ok(Header {
            flags: u32::from_le_bytes(b[28..32].try_into().unwrap()),
            entry_count: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            payload_len: u64::from_le_bytes(b[16..24].try_into().unwrap()),
            payload_sum: u32::from_le_bytes(b[24..28].try_into().unwrap()),
            label: String::from_utf8_lossy(&label_raw[..end]).into_owned(),
            created: i64::from_le_bytes(b[96..104].try_into().unwrap()),
        })
    }

    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut b = [0u8; HEADER_SIZE];
        b[0..8].copy_from_slice(MAGIC);
        b[8..12].copy_from_slice(&VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&self.entry_count.to_le_bytes());
        b[16..24].copy_from_slice(&self.payload_len.to_le_bytes());
        b[24..28].copy_from_slice(&self.payload_sum.to_le_bytes());
        b[28..32].copy_from_slice(&self.flags.to_le_bytes());
        // Truncate the label on a char boundary.
        let mut label = self.label.as_str();
        while label.len() > LABEL_LEN {
            let mut cut = LABEL_LEN;
            while !label.is_char_boundary(cut) {
                cut -= 1;
            }
            label = &label[..cut];
        }
        b[32..32 + label.len()].copy_from_slice(label.as_bytes());
        b[96..104].copy_from_slice(&self.created.to_le_bytes());
        let sum = fnv1a(&b[..508]);
        b[508..512].copy_from_slice(&sum.to_le_bytes());
        b
    }
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.b.len());
        let end = end.ok_or_else(|| Error::corrupted("SimpleFS payload truncated"))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

pub fn read_header(dev: &mut dyn BlockDevice) -> Result<Header> {
    if dev.size() < HEADER_SIZE as u64 {
        return Err(Error::new(ErrorKind::UnknownFormat, "image too small for SimpleFS"));
    }
    let mut hdr = [0u8; HEADER_SIZE];
    dev.read_at(0, &mut hdr)?;
    Header::decode(&hdr)
}

/// Rewrites only the flags of the superblock.
pub fn write_flags(dev: &mut dyn BlockDevice, flags: u32) -> Result<()> {
    let mut h = read_header(dev)?;
    h.flags = flags;
    dev.write_at(0, &h.encode())?;
    dev.flush()
}

pub fn load(dev: &mut dyn BlockDevice) -> Result<Tree> {
    let h = read_header(dev)?;
    if h.payload_len > dev.size() - HEADER_SIZE as u64 {
        return Err(Error::corrupted("SimpleFS payload extends beyond the device"));
    }
    let payload = dix_core::device::read_vec(dev, HEADER_SIZE as u64, h.payload_len as usize)?;
    if fnv1a(&payload) != h.payload_sum {
        return Err(Error::corrupted("SimpleFS payload checksum mismatch"));
    }
    let mut tree = Tree::new(h.label, h.created);
    let mut r = Reader { b: &payload, pos: 0 };
    for _ in 0..h.entry_count {
        let plen = r.u16()? as usize;
        let path_str = std::str::from_utf8(r.take(plen)?).map_err(|_| Error::corrupted("invalid UTF-8 path"))?;
        let path = VfsPath::parse(path_str)?;
        let kind = r.u8()?;
        let mtime = r.u64()? as i64;
        let dlen = r.u64()? as usize;
        let data = r.take(dlen)?.to_vec();
        if kind != KIND_FILE && kind != KIND_DIR {
            return Err(Error::corrupted(format!("unknown entry kind {kind}")));
        }
        match path.parent().and_then(|pp| tree.get(&pp)) {
            Some(parent) if parent.is_dir() => {}
            _ => return Err(Error::corrupted(format!("{path}: orphan entry"))),
        }
        if path.is_root() {
            continue;
        }
        tree.insert(path, Node { kind, mtime, data });
    }
    Ok(tree)
}

/// Writes the whole tree. `in_use` is stored as [`FLAG_IN_USE`].
pub fn store(dev: &mut dyn BlockDevice, tree: &mut Tree, in_use: bool) -> Result<()> {
    let mut payload = Vec::with_capacity(tree.encoded_len() as usize);
    let mut count = 0u32;
    // BTreeMap order ("/a" < "/a/b") guarantees parents come before children.
    for (p, n) in tree.entries() {
        let ps = p.as_str().as_bytes();
        let plen = u16::try_from(ps.len()).map_err(|_| Error::invalid_input(format!("{p}: path too long")))?;
        payload.extend_from_slice(&plen.to_le_bytes());
        payload.extend_from_slice(ps);
        payload.push(n.kind);
        payload.extend_from_slice(&n.mtime.to_le_bytes());
        payload.extend_from_slice(&(n.data.len() as u64).to_le_bytes());
        payload.extend_from_slice(&n.data);
        count += 1;
    }
    let needed = HEADER_SIZE as u64 + payload.len() as u64;
    if needed > dev.size() {
        if dev.info().resizable {
            // Grow in 64 KiB steps to avoid resizing on every small write.
            dev.resize(needed.div_ceil(65536) * 65536)?;
        } else {
            return Err(Error::new(
                ErrorKind::NoSpace,
                format!("SimpleFS needs {needed} bytes but the device has {}", dev.size()),
            ));
        }
    }
    let header = Header {
        flags: if in_use { FLAG_IN_USE } else { 0 },
        entry_count: count,
        payload_len: payload.len() as u64,
        payload_sum: fnv1a(&payload),
        label: tree.label.clone(),
        created: tree.created,
    };
    dev.write_at(HEADER_SIZE as u64, &payload)?;
    dev.write_at(0, &header.encode())?;
    dev.flush()
}
