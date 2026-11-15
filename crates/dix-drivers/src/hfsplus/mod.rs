//! HFS+ (Mac OS Extended), HFSX and classic HFS, read-only.
//!
//! The volume header (HFS: the master directory block) at byte 1024 gives
//! the allocation block size and the extents of the special files. Three
//! of them are B-trees (see [`btree`]):
//!
//! * the **catalog**: every file and folder under the key (parent folder
//!   ID, name), and a *thread* record under (own ID, "") leading back to
//!   the parent. A folder's contents are the records after its thread;
//! * the **extents overflow** file: a fork's extents after the first eight
//!   (HFS: three) kept in its catalog record, keyed by (file ID, fork, first
//!   block);
//! * the **attributes** file (HFS+): extended attributes, among them
//!   `com.apple.decmpfs`, which marks a file compressed by macOS (see
//!   [`crate::decmpfs`]).
//!
//! What is read: data and resource forks (the resource fork is shown as a
//! hidden `name:rsrc` next to its file), files compressed by macOS (zlib,
//! LZVN, LZFSE), symbolic links, hard links to files and folders (Time
//! Machine), BSD owners, modes and dates, HFSX (case-sensitive) names,
//! volumes inside an HFS wrapper, and classic HFS volumes (Mac OS 8 and
//! before; names in Mac Roman). A journaled volume whose journal holds
//! transactions not yet written is read as it is, with a warning.

mod btree;
#[cfg(all(test, unix))]
mod tests;

use btree::{Header, LEAF, Node, descend, split_key};
use dix_core::{
    AccessMode, BlockDevice, Capabilities, DirEntry, Driver, DriverInfo, EntryKind, Error, ErrorKind, FileSystem,
    FsInfo, Metadata, MountOptions, ProbeScore, Result, VfsPath,
};
use std::collections::HashMap;
use std::sync::Arc;

const CAPS: Capabilities =
    Capabilities::READ.union(Capabilities::SYMLINKS).union(Capabilities::PERMISSIONS).union(Capabilities::TIMESTAMPS);

/// Seconds from 1904-01-01 (the Mac epoch) to 1970-01-01.
const MAC_EPOCH: i64 = 2_082_844_800;

const ROOT_PARENT: u32 = 1;
const ROOT: u32 = 2;
const EXTENTS_ID: u32 = 3;
const CATALOG_ID: u32 = 4;
const ATTRIBUTES_ID: u32 = 8;

/// BSD owner flag: the file is compressed (decmpfs).
const UF_COMPRESSED: u8 = 0x20;
/// Finder flag: invisible.
const INVISIBLE: u16 = 0x4000;

const PRIVATE_FILES: &str = "\0\0\0\0HFS+ Private Data";
const PRIVATE_DIRS: &str = ".HFS+ Private Directory Data\r";

pub(crate) fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}
pub(crate) fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

fn damaged(what: impl std::fmt::Display) -> Error {
    Error::corrupted(what)
}

pub struct HfsDriver;

/// Where the HFS+ volume is in `dev`: its byte offset, if it is in an HFS
/// wrapper; or whether it is classic HFS.
enum Found {
    Plus { offset: u64, wrapped: bool },
    Classic,
}

fn find(dev: &mut dyn BlockDevice) -> Result<Option<Found>> {
    if dev.size() < 1536 {
        return Ok(None);
    }
    let mut h = [0u8; 512];
    dev.read_at(1024, &mut h)?;
    match &h[..2] {
        b"H+" | b"HX" => Ok(Some(Found::Plus { offset: 0, wrapped: false })),
        b"BD" => {
            if &h[124..126] == b"H+" {
                let block = be32(&h, 20) as u64;
                let first = be16(&h, 28) as u64 * 512;
                let start = be16(&h, 126) as u64;
                Ok(Some(Found::Plus { offset: first + start * block, wrapped: true }))
            } else {
                Ok(Some(Found::Classic))
            }
        }
        _ => Ok(None),
    }
}

impl Driver for HfsDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: "hfsplus",
            name: "HFS+",
            description: "Mac OS Extended (HFS+, HFSX) and Mac OS Standard (HFS), read-only",
            capabilities: CAPS,
        }
    }

    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
        Ok(match find(dev)? {
            Some(Found::Plus { offset, .. }) if offset + 1536 <= dev.size() => {
                let mut h = [0u8; 2];
                dev.read_at(offset + 1024, &mut h)?;
                if matches!(&h, b"H+" | b"HX") { 95 } else { 0 }
            }
            Some(Found::Classic) => 80,
            _ => 0,
        })
    }

    fn mount(&self, dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
        if opts.mode == AccessMode::ReadWrite {
            opts.log.info("HFS volumes are read-only in this version: the volume is opened read-only.");
        }
        let v = Volume::open(dev, &opts.log)?;
        Ok(Box::new(HfsFs { v }))
    }
}

// ---------------------------------------------------------------------------
// Forks and records.

/// A fork: its size and its extents (start block, block count).
#[derive(Clone, Debug, Default)]
pub(crate) struct Fork {
    pub size: u64,
    pub extents: Vec<(u32, u32)>,
}

impl Fork {
    fn blocks(&self) -> u64 {
        self.extents.iter().map(|e| e.1 as u64).sum()
    }

    fn parse_plus(b: &[u8]) -> Self {
        let extents = (0..8).map(|i| (be32(b, 16 + i * 8), be32(b, 20 + i * 8))).filter(|e| e.1 > 0).collect();
        Fork { size: be64(b, 0), extents }
    }

    fn parse_hfs(size: u32, b: &[u8]) -> Self {
        let extents = (0..3).map(|i| (be16(b, i * 4) as u32, be16(b, i * 4 + 2) as u32)).filter(|e| e.1 > 0).collect();
        Fork { size: size as u64, extents }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Folder,
    File,
}

/// A catalog record: a file or a folder.
#[derive(Clone, Debug)]
pub(crate) struct Rec {
    id: u32,
    kind: Kind,
    name: String,
    data: Fork,
    rsrc: Fork,
    created: Option<i64>,
    modified: Option<i64>,
    accessed: Option<i64>,
    /// HFS+ BSD info: owner, group, mode (0: never set), owner flags, and
    /// the "special" word (a hard link's target, a device's number).
    uid: u32,
    gid: u32,
    mode: u16,
    owner_flags: u8,
    special: u32,
    fd_type: [u8; 4],
    fd_creator: [u8; 4],
    fd_flags: u16,
    plus: bool,
}

impl Rec {
    fn placeholder() -> Self {
        let mut r = [0u8; 88];
        r[1] = 1;
        parse_record(true, String::new(), &r).expect("a folder record")
    }

    fn is_symlink(&self) -> bool {
        self.kind == Kind::File
            && (self.mode & 0o170000 == 0o120000 || (&self.fd_type == b"slnk" && &self.fd_creator == b"rhap"))
    }
    fn file_link(&self) -> bool {
        self.kind == Kind::File && &self.fd_type == b"hlnk" && &self.fd_creator == b"hfs+"
    }
    fn folder_link(&self) -> bool {
        self.kind == Kind::File && &self.fd_type == b"fdrp" && &self.fd_creator == b"MACS"
    }
    fn compressed(&self) -> bool {
        self.plus && self.owner_flags & UF_COMPRESSED != 0
    }
}

fn mac_time(t: u32) -> Option<i64> {
    (t != 0).then(|| t as i64 - MAC_EPOCH)
}

fn utf16_name(b: &[u8]) -> String {
    use unicode_normalization::UnicodeNormalization;
    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    // The catalog keeps the Finder's '/' where POSIX names have ':', and
    // names decomposed; they are shown composed, as macOS and Linux do.
    String::from_utf16_lossy(&units).replace('/', ":").nfc().collect()
}

/// Mac OS Roman, 0x80–0xFF.
const MAC_ROMAN: &str = "ÄÅÇÉÑÖÜáàâäãåçéèêëíìîïñóòôöõúùûü†°¢£§•¶ß®©™´¨≠ÆØ∞±≤≥¥µ∂∑∏π∫ªºΩæø¿¡¬√ƒ≈∆«»…\u{a0}ÀÃÕŒœ–—“”‘’÷◊ÿŸ⁄€‹›ﬁﬂ‡·‚„‰ÂÊÁËÈÍÎÏÌÓÔ\u{f8ff}ÒÚÛÙıˆ˜¯˘˙˚¸˝˛ˇ";

fn mac_roman(b: &[u8]) -> String {
    let high: Vec<char> = MAC_ROMAN.chars().collect();
    b.iter()
        .map(|&c| match c {
            b'/' => ':',
            0..=0x7F => c as char,
            _ => high[(c - 0x80) as usize],
        })
        .collect()
}

/// A catalog key: (parent ID, name).
fn cat_key(plus: bool, key: &[u8]) -> Option<(u32, String)> {
    if plus {
        if key.len() < 6 {
            return None;
        }
        let n = be16(key, 4) as usize * 2;
        Some((be32(key, 0), utf16_name(key.get(6..6 + n)?)))
    } else {
        if key.len() < 6 {
            return None;
        }
        let n = key[5] as usize;
        Some((be32(key, 1), mac_roman(key.get(6..6 + n)?)))
    }
}

/// The parent ID of a key, without decoding its name.
fn key_parent(plus: bool, key: &[u8]) -> u32 {
    if key.len() < 5 {
        return 0;
    }
    if plus { be32(key, 0) } else { be32(key, 1) }
}

fn key_name_empty(plus: bool, key: &[u8]) -> bool {
    if plus { key.len() < 6 || be16(key, 4) == 0 } else { key.len() < 6 || key[5] == 0 }
}

fn parse_record(plus: bool, name: String, r: &[u8]) -> Option<Rec> {
    let mut rec = Rec {
        id: 0,
        kind: Kind::File,
        name,
        data: Fork::default(),
        rsrc: Fork::default(),
        created: None,
        modified: None,
        accessed: None,
        uid: 0,
        gid: 0,
        mode: 0,
        owner_flags: 0,
        special: 0,
        fd_type: [0; 4],
        fd_creator: [0; 4],
        fd_flags: 0,
        plus,
    };
    if plus {
        let ty = be16(r, 0);
        let need = match ty {
            1 => 88,
            2 => 248,
            _ => return None,
        };
        if r.len() < need {
            return None;
        }
        rec.kind = if ty == 1 { Kind::Folder } else { Kind::File };
        rec.id = be32(r, 8);
        rec.created = mac_time(be32(r, 12));
        rec.modified = mac_time(be32(r, 16));
        rec.accessed = mac_time(be32(r, 24));
        rec.uid = be32(r, 32);
        rec.gid = be32(r, 36);
        rec.owner_flags = r[41];
        rec.mode = be16(r, 42);
        rec.special = be32(r, 44);
        rec.fd_type.copy_from_slice(&r[48..52]);
        rec.fd_creator.copy_from_slice(&r[52..56]);
        rec.fd_flags = be16(r, 56);
        if ty == 2 {
            rec.data = Fork::parse_plus(&r[88..168]);
            rec.rsrc = Fork::parse_plus(&r[168..248]);
        }
    } else {
        match r.first()? {
            1 if r.len() >= 70 => {
                rec.kind = Kind::Folder;
                rec.id = be32(r, 6);
                rec.created = mac_time(be32(r, 10));
                rec.modified = mac_time(be32(r, 14));
                rec.fd_flags = be16(r, 22 + 8);
            }
            2 if r.len() >= 102 => {
                rec.id = be32(r, 20);
                rec.fd_type.copy_from_slice(&r[4..8]);
                rec.fd_creator.copy_from_slice(&r[8..12]);
                rec.fd_flags = be16(r, 12);
                rec.data = Fork::parse_hfs(be32(r, 26), &r[74..86]);
                rec.rsrc = Fork::parse_hfs(be32(r, 36), &r[86..98]);
                rec.created = mac_time(be32(r, 44));
                rec.modified = mac_time(be32(r, 48));
            }
            _ => return None,
        }
    }
    Some(rec)
}

// ---------------------------------------------------------------------------
// The volume.

struct Tree {
    h: Header,
    fork: Fork,
}

pub(crate) struct Volume {
    dev: Box<dyn BlockDevice>,
    /// Byte offset of allocation block 0.
    base: u64,
    block: u64,
    plus: bool,
    hfsx: bool,
    case_sensitive: bool,
    wrapped: bool,
    journaled: bool,
    cat: Tree,
    ext: Tree,
    attr: Option<Tree>,
    total: u64,
    free: u64,
    label: Option<String>,
    root: Rec,
    nodes: HashMap<(u32, u32), Arc<Node>>,
    dirs: HashMap<u32, Arc<Vec<Rec>>>,
    forks: HashMap<(u32, u8), Arc<Fork>>,
    private_files: Option<u32>,
    private_dirs: Option<u32>,
    chunks: crate::decmpfs::ChunkCache,
}

impl Volume {
    fn open(mut dev: Box<dyn BlockDevice>, log: &dix_core::MountLog) -> Result<Self> {
        let found = find(dev.as_mut())?.ok_or_else(|| damaged("not an HFS or HFS+ volume"))?;
        let mut h = [0u8; 512];
        let (mut v, journal) = match found {
            Found::Plus { offset, wrapped } => {
                dev.read_at(offset + 1024, &mut h)?;
                if !matches!(&h[..2], b"H+" | b"HX") {
                    return Err(damaged("the HFS wrapper's HFS+ volume is missing"));
                }
                let block = be32(&h, 40) as u64;
                if !block.is_power_of_two() || !(512..=1 << 20).contains(&block) {
                    return Err(damaged(format!("an allocation block size of {block}")));
                }
                let attrs = be32(&h, 4);
                if attrs & (1 << 8) == 0 {
                    log.warn("The volume was not unmounted cleanly: it is read as it is.");
                }
                if attrs & (1 << 15) != 0 {
                    log.info("The volume is locked (read-only) by its software lock.");
                }
                let journaled = attrs & (1 << 13) != 0;
                let fork = |at: usize| Fork::parse_plus(&h[at..at + 80]);
                let v = Volume {
                    base: offset,
                    block,
                    plus: true,
                    hfsx: &h[..2] == b"HX",
                    case_sensitive: false,
                    wrapped,
                    journaled,
                    cat: Tree { h: Header::placeholder(), fork: fork(272) },
                    ext: Tree { h: Header::placeholder(), fork: fork(192) },
                    attr: Some(Tree { h: Header::placeholder(), fork: fork(352) }).filter(|t| t.fork.size > 0),
                    total: be32(&h, 44) as u64 * block,
                    free: be32(&h, 48) as u64 * block,
                    label: None,
                    root: Rec::placeholder(),
                    nodes: HashMap::new(),
                    dirs: HashMap::new(),
                    forks: HashMap::new(),
                    private_files: None,
                    private_dirs: None,
                    chunks: Default::default(),
                    dev,
                };
                (v, journaled.then(|| be32(&h, 12)))
            }
            Found::Classic => {
                dev.read_at(1024, &mut h)?;
                let block = be32(&h, 20) as u64;
                if block == 0 || !block.is_multiple_of(512) {
                    return Err(damaged(format!("an allocation block size of {block}")));
                }
                if be16(&h, 10) & (1 << 8) == 0 {
                    log.warn("The volume was not unmounted cleanly: it is read as it is.");
                }
                let v = Volume {
                    base: be16(&h, 28) as u64 * 512,
                    block,
                    plus: false,
                    hfsx: false,
                    case_sensitive: false,
                    wrapped: false,
                    journaled: false,
                    cat: Tree { h: Header::placeholder(), fork: Fork::parse_hfs(be32(&h, 146), &h[150..162]) },
                    ext: Tree { h: Header::placeholder(), fork: Fork::parse_hfs(be32(&h, 130), &h[134..146]) },
                    attr: None,
                    total: be16(&h, 18) as u64 * block,
                    free: be16(&h, 34) as u64 * block,
                    label: Some(mac_roman(&h[37..37 + (h[36] as usize).min(27)])),
                    root: Rec::placeholder(),
                    nodes: HashMap::new(),
                    dirs: HashMap::new(),
                    forks: HashMap::new(),
                    private_files: None,
                    private_dirs: None,
                    chunks: Default::default(),
                    dev,
                };
                (v, None)
            }
        };
        // The B-trees' headers; the catalog's and attributes' extents may
        // continue in the extents file.
        v.ext.h = v.tree_header(EXTENTS_ID)?;
        v.cat.fork = v.full_fork(CATALOG_ID, 0, &v.cat.fork.clone())?;
        v.cat.h = v.tree_header(CATALOG_ID)?;
        v.case_sensitive = v.hfsx && v.cat.h.compare == 0xBC;
        if let Some(a) = &v.attr {
            let f = a.fork.clone();
            let full = v.full_fork(ATTRIBUTES_ID, 0, &f)?;
            v.attr.as_mut().unwrap().fork = full;
            let h = v.tree_header(ATTRIBUTES_ID)?;
            v.attr.as_mut().unwrap().h = h;
        }
        // The root folder, under its parent "1".
        let top = v.list(ROOT_PARENT)?;
        let root = top.iter().find(|r| r.id == ROOT).cloned().ok_or_else(|| damaged("the root folder is missing"))?;
        if v.plus {
            v.label = Some(root.name.replace(':', "/"));
        }
        v.root = root;
        let entries = v.list(ROOT)?;
        v.private_files = entries.iter().find(|r| r.name == PRIVATE_FILES && r.kind == Kind::Folder).map(|r| r.id);
        v.private_dirs = entries.iter().find(|r| r.name == PRIVATE_DIRS && r.kind == Kind::Folder).map(|r| r.id);
        if let Some(block) = journal {
            v.check_journal(block, log);
        }
        Ok(v)
    }

    /// Warns when the journal holds transactions not yet written in place.
    fn check_journal(&mut self, info_block: u32, log: &dix_core::MountLog) {
        let mut jib = [0u8; 52];
        if self.dev.read_at(self.base + info_block as u64 * self.block, &mut jib).is_err() {
            return;
        }
        if be32(&jib, 0) & 2 != 0 {
            log.info("The journal is on another device.");
            return;
        }
        let at = be64(&jib, 36);
        let mut jh = [0u8; 24];
        if self.dev.read_at(self.base + at, &mut jh).is_err() {
            return;
        }
        let le = &jh[..4] == b"xLNJ";
        let rd = |o: usize| {
            let b: [u8; 8] = jh[o..o + 8].try_into().unwrap();
            if le { u64::from_le_bytes(b) } else { u64::from_be_bytes(b) }
        };
        if (&jh[..4] == b"JNLx" || le) && rd(8) != rd(16) {
            log.warn(
                "The journal holds changes that were not written to the volume yet (macOS did not unmount it \
                 cleanly): they are not replayed, and what is shown may be out of date.",
            );
        }
    }

    // --- Reading ---------------------------------------------------------

    /// Reads from a fork, within its logical size. Returns bytes read.
    fn fork_read(&mut self, fork: &Fork, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= fork.size {
            return Ok(0);
        }
        let want = buf.len().min((fork.size - offset) as usize);
        let mut done = 0usize;
        let mut start = 0u64; // byte offset of the extent in the fork
        for &(first, count) in &fork.extents {
            let len = count as u64 * self.block;
            let end = start + len;
            let at = offset + done as u64;
            if at < end && at >= start {
                let n = ((end - at) as usize).min(want - done);
                let disk = self.base + first as u64 * self.block + (at - start);
                self.dev.read_at(disk, &mut buf[done..done + n])?;
                done += n;
                if done == want {
                    break;
                }
            }
            start = end;
        }
        if done < want {
            return Err(damaged("a fork is shorter than its size"));
        }
        Ok(done)
    }

    fn tree(&self, id: u32) -> &Tree {
        match id {
            EXTENTS_ID => &self.ext,
            CATALOG_ID => &self.cat,
            _ => self.attr.as_ref().unwrap(),
        }
    }

    fn tree_header(&mut self, id: u32) -> Result<Header> {
        let fork = self.tree(id).fork.clone();
        let mut b = vec![0u8; 512];
        self.fork_read(&fork, 0, &mut b)?;
        Header::parse(&b, !self.plus)
    }

    fn node(&mut self, id: u32, n: u32) -> Result<Arc<Node>> {
        if let Some(node) = self.nodes.get(&(id, n)) {
            return Ok(node.clone());
        }
        let (fork, size) = {
            let t = self.tree(id);
            (t.fork.clone(), t.h.node_size)
        };
        let mut b = vec![0u8; size as usize];
        let got = self.fork_read(&fork, n as u64 * size as u64, &mut b)?;
        if got < b.len() {
            return Err(damaged("a B-tree node past the end of its file"));
        }
        let node = Arc::new(Node::parse(b)?);
        if self.nodes.len() > 512 {
            self.nodes.clear();
        }
        self.nodes.insert((id, n), node.clone());
        Ok(node)
    }

    /// Calls `f` on every leaf record from the leaf where keys `<= target`
    /// end, until it returns false.
    fn scan(
        &mut self,
        id: u32,
        le: impl Fn(&[u8]) -> bool,
        mut f: impl FnMut(&[u8], &[u8]) -> Result<bool>,
    ) -> Result<()> {
        let h = self.tree(id).h.clone();
        if h.root == 0 {
            return Ok(()); // an empty tree
        }
        let mut at = {
            let this = &mut *self;
            descend(&h, |n| this.node(id, n), le)?
        };
        let mut hops = 0;
        while at != 0 {
            let node = self.node(id, at)?;
            if node.kind != LEAF {
                return Err(damaged("a leaf chain leads to a non-leaf node"));
            }
            for i in 0..node.len() {
                let (key, data) = split_key(&h, node.record(i)?, false)?;
                if !f(key, data)? {
                    return Ok(());
                }
            }
            at = node.next;
            hops += 1;
            if hops > h.total_nodes.max(1) {
                return Err(damaged("the leaf chain loops"));
            }
        }
        Ok(())
    }

    /// The folders and files in folder `parent` (cached).
    fn list(&mut self, parent: u32) -> Result<Arc<Vec<Rec>>> {
        if let Some(d) = self.dirs.get(&parent) {
            return Ok(d.clone());
        }
        let plus = self.plus;
        let mut out = Vec::new();
        self.scan(
            CATALOG_ID,
            |k| {
                let p = key_parent(plus, k);
                p < parent || (p == parent && key_name_empty(plus, k))
            },
            |k, d| {
                let p = key_parent(plus, k);
                if p < parent {
                    return Ok(true);
                }
                if p > parent {
                    return Ok(false);
                }
                if let Some((_, name)) = cat_key(plus, k)
                    && let Some(rec) = parse_record(plus, name, d)
                {
                    out.push(rec);
                }
                Ok(true)
            },
        )?;
        let out = Arc::new(out);
        if self.dirs.len() > 256 {
            self.dirs.clear();
        }
        self.dirs.insert(parent, out.clone());
        Ok(out)
    }

    /// A fork with all its extents: those after the first eight (three)
    /// come from the extents overflow file.
    fn full_fork(&mut self, id: u32, which: u8, fork: &Fork) -> Result<Fork> {
        let need = fork.size.div_ceil(self.block);
        if fork.blocks() >= need || id == EXTENTS_ID {
            return Ok(fork.clone());
        }
        if let Some(f) = self.forks.get(&(id, which)) {
            return Ok((**f).clone());
        }
        let mut full = fork.clone();
        let plus = self.plus;
        let parse = |k: &[u8]| -> (u32, u8, u32) {
            if plus { (be32(k, 2), k[0], be32(k, 6)) } else { (be32(k, 1), k[0], be16(k, 5) as u32) }
        };
        let target = (id, which, full.blocks() as u32);
        let mut more = Vec::new();
        self.scan(
            EXTENTS_ID,
            |k| k.len() >= 7 && parse(k) <= target,
            |k, d| {
                if k.len() < 7 {
                    return Ok(false);
                }
                let (fid, fw, start) = parse(k);
                if (fid, fw) < (id, which) || start < target.2 {
                    return Ok(true);
                }
                if (fid, fw) > (id, which) {
                    return Ok(false);
                }
                let ext: Vec<(u32, u32)> = if plus {
                    (0..8).filter(|i| d.len() >= i * 8 + 8).map(|i| (be32(d, i * 8), be32(d, i * 8 + 4))).collect()
                } else {
                    (0..3)
                        .filter(|i| d.len() >= i * 4 + 4)
                        .map(|i| (be16(d, i * 4) as u32, be16(d, i * 4 + 2) as u32))
                        .collect()
                };
                more.extend(ext.into_iter().filter(|e| e.1 > 0));
                Ok(true)
            },
        )?;
        full.extents.extend(more);
        if full.blocks() < need {
            return Err(damaged(format!("file {id}: extents for {} of {need} blocks", full.blocks())));
        }
        self.forks.insert((id, which), Arc::new(full.clone()));
        Ok(full)
    }

    /// The record a path names; for a hard link, the file or folder it
    /// links to (under the link's name). `Err(NotFound)` when it is not
    /// there.
    fn resolve(&mut self, path: &VfsPath) -> Result<(Rec, Option<&'static str>)> {
        let mut rec = self.root.clone();
        let parts: Vec<&str> = path.components().collect();
        for (i, part) in parts.iter().enumerate() {
            if rec.kind != Kind::Folder {
                return Err(Error::not_a_directory(path));
            }
            let list = self.list(rec.id)?;
            let part: String = if self.plus {
                use unicode_normalization::UnicodeNormalization;
                part.nfc().collect()
            } else {
                part.to_string()
            };
            let part = part.as_str();
            let found = list.iter().find(|r| r.name == part).or_else(|| {
                if self.case_sensitive {
                    None
                } else {
                    let low = part.to_lowercase();
                    list.iter().find(|r| r.name.to_lowercase() == low)
                }
            });
            match found {
                Some(r) => rec = self.follow(r.clone())?,
                None => {
                    // "name:rsrc", the resource fork of "name".
                    if i + 1 == parts.len()
                        && let Some(base) = part.strip_suffix(":rsrc")
                        && let Some(r) = list.iter().find(|r| r.name == base && r.kind == Kind::File)
                    {
                        return Ok((self.follow(r.clone())?, Some("rsrc")));
                    }
                    return Err(Error::not_found(path));
                }
            }
        }
        Ok((rec, None))
    }

    /// A hard link's target (with the link's name), else the record itself.
    fn follow(&mut self, rec: Rec) -> Result<Rec> {
        let (dir, name) = if rec.file_link() {
            (self.private_files, format!("iNode{}", rec.special))
        } else if rec.folder_link() {
            (self.private_dirs, format!("dir_{}", rec.special))
        } else {
            return Ok(rec);
        };
        let Some(dir) = dir else { return Ok(rec) };
        let list = self.list(dir)?;
        match list.iter().find(|r| r.name == name) {
            Some(t) => Ok(Rec { name: rec.name, ..t.clone() }),
            None => Ok(rec),
        }
    }

    fn data_fork(&mut self, rec: &Rec) -> Result<Fork> {
        self.full_fork(rec.id, 0, &rec.data)
    }

    fn rsrc_fork(&mut self, rec: &Rec) -> Result<Fork> {
        self.full_fork(rec.id, 0xFF, &rec.rsrc)
    }

    // --- Extended attributes -----------------------------------------------

    /// An extended attribute's value (inline, or in its own fork).
    fn xattr(&mut self, id: u32, name: &str) -> Result<Option<Vec<u8>>> {
        if self.attr.is_none() {
            return Ok(None);
        }
        let want: Vec<u16> = name.encode_utf16().collect();
        let key_of = |k: &[u8]| -> Option<(u32, Vec<u16>, u32)> {
            if k.len() < 12 {
                return None;
            }
            let n = be16(k, 10) as usize;
            let units = k.get(12..12 + n * 2)?.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            Some((be32(k, 2), units, be32(k, 6)))
        };
        let target = (id, want.clone(), 0u32);
        let mut found: Option<Vec<u8>> = None;
        self.scan(
            ATTRIBUTES_ID,
            |k| key_of(k).is_some_and(|key| key <= target),
            |k, d| {
                let Some(key) = key_of(k) else { return Ok(false) };
                if (key.0, &key.1) < (id, &want) {
                    return Ok(true);
                }
                if (key.0, &key.1) == (id, &want) {
                    found = Some(d.to_vec());
                }
                Ok(false)
            },
        )?;
        let Some(d) = found else { return Ok(None) };
        match be32(&d, 0) {
            0x10 => {
                let n = be32(&d, 12) as usize;
                Ok(Some(d.get(16..16 + n).ok_or_else(|| damaged("an attribute record is cut short"))?.to_vec()))
            }
            0x20 if d.len() >= 88 => {
                let fork = Fork::parse_plus(&d[8..88]);
                if fork.size > 1 << 30 {
                    return Err(damaged("an attribute is too large"));
                }
                // Extents past the first eight are in the attributes file
                // too (0x30 records); rare enough to refuse.
                if fork.blocks() < fork.size.div_ceil(self.block) {
                    return Err(Error::unsupported("attributes in more than eight extents"));
                }
                let mut v = vec![0u8; fork.size as usize];
                self.fork_read(&fork, 0, &mut v)?;
                Ok(Some(v))
            }
            t => Err(damaged(format!("an attribute record of type {t:#x}"))),
        }
    }

    /// For a compressed file: its decmpfs header and where its data is.
    fn decmpfs(&mut self, rec: &Rec) -> Result<Option<crate::decmpfs::Compressed>> {
        if !rec.compressed() {
            return Ok(None);
        }
        let Some(x) = self.xattr(rec.id, "com.apple.decmpfs")? else { return Ok(None) };
        crate::decmpfs::Compressed::parse(x).map(Some)
    }

    fn metadata(&mut self, rec: &Rec) -> Metadata {
        let kind = if rec.kind == Kind::Folder {
            EntryKind::Directory
        } else if rec.is_symlink() {
            EntryKind::Symlink
        } else if rec.plus && matches!(rec.mode & 0o170000, 0o020000 | 0o060000 | 0o010000 | 0o140000) {
            EntryKind::Other
        } else {
            EntryKind::File
        };
        let mut size = if kind == EntryKind::Directory { 0 } else { rec.data.size };
        if let Ok(Some(c)) = self.decmpfs(rec) {
            size = c.size;
        }
        let mut m = Metadata::new(kind, size);
        m.created = rec.created;
        m.modified = rec.modified;
        m.accessed = rec.accessed.or(rec.modified);
        m.inode = Some(rec.id as u64);
        m.hidden = rec.name.starts_with('.') || rec.fd_flags & INVISIBLE != 0;
        let default = if kind == EntryKind::Directory { 0o40755 } else { 0o100644 };
        if rec.plus && rec.mode != 0 {
            m.mode = Some(rec.mode as u32);
            m.uid = Some(rec.uid);
            m.gid = Some(rec.gid);
        } else {
            m.mode = Some(default);
        }
        if kind == EntryKind::Symlink
            && let Ok(fork) = self.data_fork(rec)
        {
            let mut b = vec![0u8; fork.size.min(4096) as usize];
            if self.fork_read(&fork, 0, &mut b).is_ok() {
                m.symlink_target = Some(String::from_utf8_lossy(&b).into_owned());
            }
        }
        m
    }
}

impl Header {
    fn placeholder() -> Self {
        Header {
            depth: 0,
            root: 0,
            node_size: 512,
            max_key: 0,
            total_nodes: 0,
            big_keys: true,
            variable_index_keys: true,
            compare: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// The file system.

struct HfsFs {
    v: Volume,
}

impl FileSystem for HfsFs {
    fn info(&self) -> FsInfo {
        let v = &self.v;
        let name = match (v.plus, v.hfsx) {
            (false, _) => "HFS",
            (true, false) => "HFS+",
            (true, true) => "HFSX",
        };
        let mut details = vec![(
            "Format".to_string(),
            match (v.plus, v.case_sensitive) {
                (false, _) => "Mac OS Standard (HFS)".to_string(),
                (true, true) => "Mac OS Extended (case-sensitive)".to_string(),
                (true, false) => "Mac OS Extended".to_string(),
            },
        )];
        if v.journaled {
            details.push(("Journaled".into(), "yes".into()));
        }
        if v.wrapped {
            details.push(("HFS wrapper".into(), "yes (an HFS+ volume inside an HFS one)".into()));
        }
        FsInfo {
            driver_id: "hfsplus".into(),
            fs_name: name.into(),
            label: v.label.clone(),
            total_bytes: Some(v.total),
            free_bytes: Some(v.free),
            block_size: Some(v.block as u32),
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
        let (dir, fork) = self.v.resolve(path)?;
        if dir.kind != Kind::Folder || fork.is_some() {
            return Err(Error::not_a_directory(path));
        }
        let list = self.v.list(dir.id)?;
        let mut out = Vec::new();
        for r in list.iter() {
            if dir.id == ROOT && (r.name == PRIVATE_FILES || r.name == PRIVATE_DIRS) {
                continue;
            }
            let name = r.name.clone();
            let r = match self.v.follow(r.clone()) {
                Ok(r) => r,
                Err(e) if e.kind() == ErrorKind::Corrupted => r.clone(),
                Err(e) => return Err(e),
            };
            let meta = self.v.metadata(&r);
            if r.kind == Kind::File && r.rsrc.size > 0 && !r.compressed() {
                let mut m = Metadata::new(EntryKind::File, r.rsrc.size);
                m.modified = meta.modified;
                m.created = meta.created;
                m.hidden = true;
                out.push(DirEntry { name: format!("{name}:rsrc"), metadata: m });
            }
            out.push(DirEntry { name, metadata: meta });
        }
        Ok(out)
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        let (rec, fork) = self.v.resolve(path)?;
        let mut m = self.v.metadata(&rec);
        if fork.is_some() {
            m = Metadata { kind: EntryKind::File, size: rec.rsrc.size, symlink_target: None, hidden: true, ..m };
        }
        Ok(m)
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let (rec, fork) = self.v.resolve(path)?;
        if rec.kind == Kind::Folder {
            return Err(Error::is_a_directory(path));
        }
        if fork.is_some() {
            let f = self.v.rsrc_fork(&rec)?;
            return self.v.fork_read(&f, offset, buf);
        }
        if let Some(c) = self.v.decmpfs(&rec)? {
            let v = &mut self.v;
            let rsrc = if c.in_resource_fork() { v.rsrc_fork(&rec)? } else { Fork::default() };
            let mut cache = std::mem::take(&mut v.chunks);
            let r = c.read(&mut |o, b| v.fork_read(&rsrc, o, b), rsrc.size, &mut cache, rec.id as u64, offset, buf);
            v.chunks = cache;
            return r;
        }
        let f = self.v.data_fork(&rec)?;
        self.v.fork_read(&f, offset, buf)
    }
}
