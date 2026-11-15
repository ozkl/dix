//! SquashFS 4.0, read-only: the compressed file system of Linux live CDs
//! (`casper/filesystem.squashfs`), snaps, AppImages, Android and router
//! firmware.
//!
//! The superblock gives the block size, the compressor and where the
//! tables are. Inodes and directories live in **metadata blocks** (8 KiB
//! before compression, each with a 2-byte header: size, and whether it is
//! stored as is); a reference to them is (block's offset in its table << 16
//! | offset in the unpacked block). A file's data is a run of blocks of the
//! block size (each compressed alone, or stored, or sparse: size 0), and its
//! tail may be in a **fragment** block shared with other small files. User
//! and group ids are indices into the id table; fragments and ids are found
//! through lookup tables of metadata block positions.
//!
//! What is read: files (also sparse, and over 4 GiB), folders (also large
//! ones, with their index), symbolic links, device nodes, FIFOs and
//! sockets, hard links (the inode number), permissions, owners and
//! modification times; every compressor (gzip, lzma, lzo, xz with BCJ
//! filters, lz4, zstd). Extended attributes are not shown.

mod codec;
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

const MAGIC: &[u8; 4] = b"hsqs";
const META: usize = 8192;
/// A block stored without compression (data and fragment sizes).
const STORED: u32 = 1 << 24;
/// Blocks kept unpacked (metadata and data).
const CACHE: usize = 256;

const NO_FRAGMENT: u32 = 0xFFFF_FFFF;

// Superblock flags.
const NO_FRAGMENTS: u16 = 0x0010;

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
    Error::corrupted(format!("SquashFS: {what}"))
}

/// `n` (64-bit) capped at `rest`.
fn upto(n: u64, rest: usize) -> usize {
    n.min(rest as u64) as usize
}

pub struct SquashDriver;

impl Driver for SquashDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: "squashfs",
            name: "SquashFS",
            description: "SquashFS 4.0 (live CDs, snaps, AppImages, firmware): gzip, lzma, lzo, xz, lz4, zstd; read-only",
            capabilities: CAPS,
        }
    }

    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
        if dev.size() < 96 {
            return Ok(0);
        }
        let mut b = [0u8; 32];
        dev.read_at(0, &mut b)?;
        if &b[..4] == MAGIC && le16(&b, 28) == 4 {
            return Ok(95);
        }
        Ok(if appimage(dev)?.is_some() { 90 } else { 0 })
    }

    fn mount(&self, dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
        if opts.mode == AccessMode::ReadWrite {
            opts.log.info("SquashFS is a read-only file system: the volume is opened read-only.");
        }
        let mut dev = dev;
        let runtime = appimage(dev.as_mut())?;
        if let Some(at) = runtime {
            opts.log.info(format!("An AppImage: its {at}-byte launcher is followed by the SquashFS shown."));
            let (len, name) = (dev.size() - at, dev.info().name);
            dev = Box::new(dix_core::device::SliceDevice::new(dev, at, len, name)?);
        }
        Ok(Box::new(SquashFs { v: Volume::open(dev)?, runtime }))
    }
}

/// Whether a superblock starts at `at` (a SquashFS 4.0 one whose size
/// fits the device).
fn superblock_at(dev: &mut dyn BlockDevice, at: u64) -> Result<bool> {
    if at + 96 > dev.size() {
        return Ok(false);
    }
    let mut b = [0u8; 48];
    dev.read_at(at, &mut b)?;
    Ok(&b[..4] == MAGIC && le16(&b, 28) == 4 && le64(&b, 40) <= dev.size() - at)
}

/// An AppImage (type 2): an ELF launcher with a SquashFS appended — where
/// the SquashFS starts. It follows the ELF's section header table (the
/// end the ELF header gives); failing that, the first 4 MiB are searched.
fn appimage(dev: &mut dyn BlockDevice) -> Result<Option<u64>> {
    let mut e = [0u8; 64];
    if dev.size() < 4096 {
        return Ok(None);
    }
    dev.read_at(0, &mut e)?;
    if &e[..4] != b"\x7fELF" {
        return Ok(None);
    }
    let big = e[5] == 2;
    let u16_at =
        |o: usize| if big { u16::from_be_bytes([e[o], e[o + 1]]) } else { u16::from_le_bytes([e[o], e[o + 1]]) } as u64;
    let u32_at = |o: usize| {
        let b: [u8; 4] = e[o..o + 4].try_into().unwrap();
        (if big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) }) as u64
    };
    let u64_at = |o: usize| {
        let b: [u8; 8] = e[o..o + 8].try_into().unwrap();
        if big { u64::from_be_bytes(b) } else { u64::from_le_bytes(b) }
    };
    let end = match e[4] {
        1 => u32_at(0x20) + u16_at(0x2E) * u16_at(0x30),
        2 => u64_at(0x28) + u16_at(0x3A) * u16_at(0x3C),
        _ => return Ok(None),
    };
    if end > 0 && superblock_at(dev, end)? {
        return Ok(Some(end));
    }
    // (Only for what says it is an AppImage: "AI" 2 in the ELF header.)
    if &e[8..11] != b"AI\x02" {
        return Ok(None);
    }
    let limit = dev.size().min(4 << 20);
    let mut buf = vec![0u8; limit as usize];
    dev.read_at(0, &mut buf)?;
    for at in (64..buf.len().saturating_sub(4)).filter(|&i| &buf[i..i + 4] == MAGIC) {
        if superblock_at(dev, at as u64)? {
            return Ok(Some(at as u64));
        }
    }
    Ok(None)
}

/// An inode.
#[derive(Debug, Clone)]
struct Inode {
    kind: EntryKind,
    /// The Unix type bits (S_IFDIR, …).
    type_bits: u32,
    mode: u16,
    uid: u32,
    gid: u32,
    mtime: u32,
    number: u32,
    nlink: u32,
    size: u64,
    /// Files: the first data block and each block's stored size.
    start: u64,
    blocks: Vec<u32>,
    /// Files: the fragment (index, offset in it), if the tail is in one.
    fragment: Option<(u32, u32)>,
    /// Folders: where the listing is (block in the directory table, offset).
    dir: (u32, u16),
    link: Option<String>,
    rdev: Option<u32>,
}

#[derive(Debug, Clone)]
struct Child {
    name: String,
    inode: u64,
}

struct Volume {
    dev: Box<dyn BlockDevice>,
    block_size: u64,
    compressor: u16,
    inode_count: u32,
    mod_time: u32,
    bytes_used: u64,
    root: u64,
    inode_table: u64,
    dir_table: u64,
    ids: Vec<u32>,
    /// (start, stored size) of each fragment block.
    fragments: Vec<(u64, u32)>,
    /// Unpacked metadata blocks: position → (data, next block's position).
    meta: HashMap<u64, (Arc<Vec<u8>>, u64)>,
    /// Unpacked data and fragment blocks by position.
    data: HashMap<u64, Arc<Vec<u8>>>,
    dirs: HashMap<u64, Arc<Vec<Child>>>,
}

impl Volume {
    fn open(mut dev: Box<dyn BlockDevice>) -> Result<Volume> {
        let mut sb = [0u8; 96];
        dev.read_at(0, &mut sb)?;
        if &sb[..4] != MAGIC {
            return Err(damaged("no superblock"));
        }
        let (major, minor) = (le16(&sb, 28), le16(&sb, 30));
        if major != 4 {
            return Err(Error::unsupported(format!("SquashFS {major}.{minor} (only 4.0 is read)")));
        }
        let block_size = le32(&sb, 12) as u64;
        if !block_size.is_power_of_two() || !(4096..=1 << 20).contains(&block_size) {
            return Err(damaged(format!("block size {block_size}")));
        }
        let compressor = le16(&sb, 20);
        if !(1..=6).contains(&compressor) {
            return Err(Error::unsupported(format!("SquashFS compressor {compressor}")));
        }
        let flags = le16(&sb, 24);
        let mut v = Volume {
            dev,
            block_size,
            compressor,
            inode_count: le32(&sb, 4),
            mod_time: le32(&sb, 8),
            bytes_used: le64(&sb, 40),
            root: le64(&sb, 32),
            inode_table: le64(&sb, 64),
            dir_table: le64(&sb, 72),
            ids: Vec::new(),
            fragments: Vec::new(),
            meta: HashMap::new(),
            data: HashMap::new(),
            dirs: HashMap::new(),
        };
        // The id table.
        let id_count = le16(&sb, 26) as usize;
        let raw = v.lookup_table(le64(&sb, 48), id_count * 4)?;
        v.ids = raw.chunks_exact(4).map(|c| le32(c, 0)).collect();
        // The fragment table.
        let frag_count = le32(&sb, 16) as usize;
        if flags & NO_FRAGMENTS == 0 && frag_count > 0 {
            let raw = v.lookup_table(le64(&sb, 80), frag_count * 16)?;
            v.fragments = raw.chunks_exact(16).map(|c| (le64(c, 0), le32(c, 8))).collect();
        }
        Ok(v)
    }

    // -- metadata ---------------------------------------------------------------

    /// The metadata block at `at`: its unpacked bytes and the next block's
    /// position.
    fn meta_block(&mut self, at: u64) -> Result<(Arc<Vec<u8>>, u64)> {
        if let Some(b) = self.meta.get(&at) {
            return Ok(b.clone());
        }
        let mut h = [0u8; 2];
        self.dev.read_at(at, &mut h)?;
        let h = u16::from_le_bytes(h);
        let len = (h & 0x7FFF) as usize;
        if len == 0 || len > META {
            return Err(damaged(format!("metadata block at {at}")));
        }
        let mut raw = vec![0u8; len];
        self.dev.read_at(at + 2, &mut raw)?;
        let data = if h & 0x8000 != 0 { raw } else { codec::decompress(self.compressor, &raw, META)? };
        let b = (Arc::new(data), at + 2 + len as u64);
        if self.meta.len() >= CACHE {
            self.meta.clear();
        }
        self.meta.insert(at, b.clone());
        Ok(b)
    }

    /// `len` bytes of metadata from (block at `at`, offset `off`).
    fn read_meta(&mut self, mut at: u64, mut off: usize, len: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let (b, next) = self.meta_block(at)?;
            if off > b.len() {
                return Err(damaged("metadata reference past its block"));
            }
            let n = (len - out.len()).min(b.len() - off);
            out.extend_from_slice(&b[off..off + n]);
            if out.len() < len {
                if next == at {
                    return Err(damaged("metadata loops"));
                }
                (at, off) = (next, 0);
            }
        }
        Ok(out)
    }

    /// A table behind a lookup table (of metadata block positions).
    fn lookup_table(&mut self, at: u64, len: usize) -> Result<Vec<u8>> {
        let blocks = len.div_ceil(META);
        let mut idx = vec![0u8; blocks * 8];
        self.dev.read_at(at, &mut idx)?;
        let mut out = Vec::with_capacity(len);
        for c in idx.chunks_exact(8) {
            let (b, _) = self.meta_block(le64(c, 0))?;
            out.extend_from_slice(&b);
        }
        if out.len() < len {
            return Err(damaged("a table is shorter than it should be"));
        }
        out.truncate(len);
        Ok(out)
    }

    // -- inodes -----------------------------------------------------------------

    fn inode(&mut self, r: u64) -> Result<Inode> {
        let at = self.inode_table + (r >> 16);
        let off = (r & 0xFFFF) as usize;
        let h = self.read_meta(at, off, 16)?;
        let ty = le16(&h, 0);
        let id = |v: &Volume, i: u16| v.ids.get(i as usize).copied().unwrap_or(0);
        let mut n = Inode {
            kind: EntryKind::Other,
            type_bits: 0,
            mode: le16(&h, 2) & 0o7777,
            uid: id(self, le16(&h, 4)),
            gid: id(self, le16(&h, 6)),
            mtime: le32(&h, 8),
            number: le32(&h, 12),
            nlink: 1,
            size: 0,
            start: 0,
            blocks: Vec::new(),
            fragment: None,
            dir: (0, 0),
            link: None,
            rdev: None,
        };
        let more = |v: &mut Volume, len: usize| v.read_meta(at, off, 16 + len);
        match ty {
            1 => {
                let b = more(self, 16)?;
                n.kind = EntryKind::Directory;
                n.dir = (le32(&b, 16), le16(&b, 26));
                n.nlink = le32(&b, 20);
                n.size = le16(&b, 24) as u64;
            }
            8 => {
                let b = more(self, 24)?;
                n.kind = EntryKind::Directory;
                n.nlink = le32(&b, 16);
                n.size = le32(&b, 20) as u64;
                n.dir = (le32(&b, 24), le16(&b, 34));
            }
            2 | 9 => {
                let (size, start, frag, frag_off, head) = if ty == 2 {
                    let b = more(self, 16)?;
                    (le32(&b, 28) as u64, le32(&b, 16) as u64, le32(&b, 20), le32(&b, 24), 32)
                } else {
                    let b = more(self, 40)?;
                    n.nlink = le32(&b, 40);
                    (le64(&b, 24), le64(&b, 16), le32(&b, 44), le32(&b, 48), 56)
                };
                n.kind = EntryKind::File;
                n.size = size;
                n.start = start;
                let has_frag = frag != NO_FRAGMENT;
                let count = if has_frag { size / self.block_size } else { size.div_ceil(self.block_size) };
                if count > (1 << 32) {
                    return Err(damaged("a file with too many blocks"));
                }
                let b = more(self, head - 16 + 4 * count as usize)?;
                n.blocks = b[head..].chunks_exact(4).map(|c| le32(c, 0)).collect();
                if has_frag {
                    n.fragment = Some((frag, frag_off));
                }
            }
            3 | 10 => {
                let b = more(self, 8)?;
                n.kind = EntryKind::Symlink;
                n.nlink = le32(&b, 16);
                let len = le32(&b, 20) as usize;
                if len > 1 << 16 {
                    return Err(damaged("a link target too long"));
                }
                let b = more(self, 8 + len)?;
                let target = String::from_utf8_lossy(&b[24..24 + len]).into_owned();
                n.size = len as u64;
                n.link = Some(target);
            }
            4 | 5 | 11 | 12 => {
                let b = more(self, 8)?;
                n.nlink = le32(&b, 16);
                n.rdev = Some(le32(&b, 20));
                n.type_bits = if ty == 4 || ty == 11 { 0o060000 } else { 0o020000 };
            }
            6 | 7 | 13 | 14 => {
                let b = more(self, 4)?;
                n.nlink = le32(&b, 16);
                n.type_bits = if ty == 6 || ty == 13 { 0o010000 } else { 0o140000 };
            }
            _ => return Err(damaged(format!("inode type {ty}"))),
        }
        n.type_bits = match n.kind {
            EntryKind::Directory => 0o040000,
            EntryKind::File => 0o100000,
            EntryKind::Symlink => 0o120000,
            EntryKind::Other => n.type_bits,
        };
        Ok(n)
    }

    // -- directories --------------------------------------------------------------

    fn list(&mut self, r: u64) -> Result<Arc<Vec<Child>>> {
        if let Some(d) = self.dirs.get(&r) {
            return Ok(d.clone());
        }
        let n = self.inode(r)?;
        if n.kind != EntryKind::Directory {
            return Err(Error::not_a_directory("SquashFS entry"));
        }
        // The size counts "." and ".." (3 bytes) that are not stored.
        let len = (n.size as usize).saturating_sub(3);
        let data = self.read_meta(self.dir_table + n.dir.0 as u64, n.dir.1 as usize, len)?;
        let mut out = Vec::new();
        let mut pos = 0;
        while pos + 12 <= data.len() {
            let count = le32(&data, pos) as usize + 1;
            let start = le32(&data, pos + 4) as u64;
            pos += 12;
            for _ in 0..count {
                let e = data.get(pos..pos + 8).ok_or_else(|| damaged("a folder ends early"))?;
                let offset = le16(e, 0) as u64;
                let name_len = le16(e, 6) as usize + 1;
                let name = data.get(pos + 8..pos + 8 + name_len).ok_or_else(|| damaged("a folder ends early"))?;
                pos += 8 + name_len;
                let name = String::from_utf8_lossy(name).replace('/', "_");
                out.push(Child { name, inode: (start << 16) | offset });
            }
        }
        let out = Arc::new(out);
        if self.dirs.len() >= CACHE {
            self.dirs.clear();
        }
        self.dirs.insert(r, out.clone());
        Ok(out)
    }

    fn resolve(&mut self, path: &VfsPath) -> Result<u64> {
        let mut at = self.root;
        for name in path.components() {
            let list = self.list(at)?;
            at = list.iter().find(|c| c.name == name).ok_or_else(|| Error::not_found(path))?.inode;
        }
        Ok(at)
    }

    // -- data -----------------------------------------------------------------------

    /// A data or fragment block: stored size `raw` at `at`, unpacking to at
    /// most `max` bytes.
    fn block(&mut self, at: u64, raw: u32, max: usize) -> Result<Arc<Vec<u8>>> {
        if let Some(b) = self.data.get(&at) {
            return Ok(b.clone());
        }
        let len = (raw & (STORED - 1)) as usize;
        let mut buf = vec![0u8; len];
        self.dev.read_at(at, &mut buf)?;
        let data = if raw & STORED != 0 { buf } else { codec::decompress(self.compressor, &buf, max)? };
        let data = Arc::new(data);
        if self.data.len() >= 16 {
            self.data.clear();
        }
        self.data.insert(at, data.clone());
        Ok(data)
    }

    fn read(&mut self, n: &Inode, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= n.size {
            return Ok(0);
        }
        let want = upto(n.size - offset, buf.len());
        let bs = self.block_size;
        let mut done = 0;
        // Where each full block starts.
        let block_at =
            |i: usize| -> u64 { n.start + n.blocks[..i].iter().map(|s| (s & (STORED - 1)) as u64).sum::<u64>() };
        let mut cursor = (usize::MAX, 0u64);
        while done < want {
            let pos = offset + done as u64;
            let i = (pos / bs) as usize;
            let within = (pos % bs) as usize;
            let this_len = (n.size - i as u64 * bs).min(bs) as usize;
            let k = (this_len - within).min(want - done);
            if i < n.blocks.len() {
                let raw = n.blocks[i];
                let at = if cursor.0 == i { cursor.1 } else { block_at(i) };
                cursor = (i + 1, at + (raw & (STORED - 1)) as u64);
                if raw & (STORED - 1) == 0 {
                    buf[done..done + k].fill(0); // sparse
                } else {
                    let b = self.block(at, raw, bs as usize)?;
                    let src = b.get(within..within + k).ok_or_else(|| damaged("a data block is short"))?;
                    buf[done..done + k].copy_from_slice(src);
                }
            } else {
                let (idx, frag_off) = n.fragment.ok_or_else(|| damaged("a file's tail has no fragment"))?;
                let &(at, raw) =
                    self.fragments.get(idx as usize).ok_or_else(|| damaged(format!("no fragment {idx}")))?;
                let b = self.block(at, raw, bs as usize)?;
                let from = frag_off as usize + within;
                let src = b.get(from..from + k).ok_or_else(|| damaged("a fragment is short"))?;
                buf[done..done + k].copy_from_slice(src);
            }
            done += k;
        }
        Ok(want)
    }

    fn metadata(&self, n: &Inode) -> Metadata {
        let mut m = Metadata::new(n.kind, if n.kind == EntryKind::Directory { 0 } else { n.size });
        m.modified = Some(n.mtime as i64);
        m.mode = Some(n.type_bits | n.mode as u32);
        m.uid = Some(n.uid);
        m.gid = Some(n.gid);
        m.inode = Some(n.number as u64);
        m.symlink_target = n.link.clone();
        let _ = (n.nlink, n.rdev);
        m
    }
}

pub struct SquashFs {
    v: Volume,
    /// An AppImage: the size of its launcher before the SquashFS.
    runtime: Option<u64>,
}

impl FileSystem for SquashFs {
    fn info(&self) -> FsInfo {
        let v = &self.v;
        let details = vec![
            ("Compression".to_string(), codec::name(v.compressor).to_string()),
            ("Inodes".into(), v.inode_count.to_string()),
            ("Fragments".into(), v.fragments.len().to_string()),
            ("Made".into(), dix_core::time::format_unix(v.mod_time as i64)),
        ];
        let mut details = details;
        if let Some(at) = self.runtime {
            details.push(("AppImage".into(), format!("launcher of {at} bytes before the file system")));
        }
        FsInfo {
            driver_id: "squashfs".into(),
            fs_name: "SquashFS".into(),
            label: None,
            total_bytes: Some(v.bytes_used),
            free_bytes: Some(0),
            block_size: Some(v.block_size as u32),
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
        let r = self.v.resolve(path)?;
        let list = self.v.list(r)?;
        let mut out = Vec::with_capacity(list.len());
        for c in list.iter() {
            let n = self.v.inode(c.inode)?;
            out.push(DirEntry { name: c.name.clone(), metadata: self.v.metadata(&n) });
        }
        Ok(out)
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        let r = self.v.resolve(path)?;
        let n = self.v.inode(r)?;
        Ok(self.v.metadata(&n))
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let r = self.v.resolve(path)?;
        let n = self.v.inode(r)?;
        match n.kind {
            EntryKind::Directory => Err(Error::is_a_directory(path)),
            EntryKind::File => self.v.read(&n, offset, buf),
            _ => Ok(0),
        }
    }
}
