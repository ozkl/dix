//! XFS, read-only: version 4 and version 5 (with metadata checksums, the
//! default since 2014) file systems, as Linux servers, NAS boxes and RHEL
//! make them.
//!
//! The volume is split into **allocation groups** (AGs). An inode number
//! says where the inode is: its AG, the block in the AG and the slot in
//! the block; block numbers in a file's map are (AG << agblklog) | block
//! too, so they are not linear on disk. Every on-disk number is
//! big-endian.
//!
//! An inode's data fork holds, by its format: the bytes themselves
//! (*local*: small folders, short link targets), a list of **extents**
//! (start in the file, start on disk, length; 16 bytes each, packed), or
//! the root of a **B+tree** of extents (files with many). Unwritten
//! (preallocated) extents and holes read as zeros.
//!
//! Folders are *short form* (in the inode) or made of directory blocks
//! (1 << dirblklog file system blocks) in the folder's first 32 GiB of
//! address space; their entries (inode, name, file type) are read from
//! there; the hash index and free-space blocks after it are not needed to
//! list them. Long link targets are in blocks of their own (with a header
//! on v5).
//!
//! Not read: extended attributes, files on a separate realtime device.
//! The journal is not replayed: a volume that was not unmounted cleanly
//! shows what was last written in place.

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

const MAGIC: &[u8; 4] = b"XFSB";
/// Directory data lives below this offset in a folder (32 GiB).
const DIR_LEAF_OFFSET: u64 = 32 << 30;
const CACHE: usize = 512;

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

// Data fork formats.
const FMT_DEV: u8 = 0;
const FMT_LOCAL: u8 = 1;
const FMT_EXTENTS: u8 = 2;
const FMT_BTREE: u8 = 3;

const DIFLAG_REALTIME: u16 = 0x1;
const DIFLAG2_BIGTIME: u64 = 1 << 3;
const DIFLAG2_NREXT64: u64 = 1 << 4;

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

fn damaged(what: impl std::fmt::Display) -> Error {
    Error::corrupted(format!("XFS: {what}"))
}

pub struct XfsDriver;

impl Driver for XfsDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: "xfs",
            name: "XFS",
            description: "XFS v4 and v5 (Linux servers, NAS, RHEL); read-only",
            capabilities: CAPS,
        }
    }

    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
        if dev.size() < 4096 {
            return Ok(0);
        }
        let mut b = [0u8; 8];
        dev.read_at(0, &mut b)?;
        Ok(if &b[..4] == MAGIC { 95 } else { 0 })
    }

    fn mount(&self, dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
        if opts.mode == AccessMode::ReadWrite {
            opts.log.info("XFS is read-only here: the volume is opened read-only.");
        }
        Ok(Box::new(Xfs { v: Volume::open(dev)? }))
    }
}

/// A file's piece: where in the file (blocks), where on disk (byte), how
/// many blocks, and whether it is only preallocated.
#[derive(Debug, Clone, Copy)]
struct Extent {
    off: u64,
    at: u64,
    len: u64,
    unwritten: bool,
}

#[derive(Debug, Clone)]
enum Fork {
    /// Bytes held in the inode.
    Local(Vec<u8>),
    /// Extents, sorted by their place in the file.
    Extents(Vec<Extent>),
    Device,
}

#[derive(Debug, Clone)]
struct Inode {
    number: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    size: u64,
    atime: i64,
    mtime: i64,
    ctime: i64,
    crtime: Option<i64>,
    fork: Fork,
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
    inode: u64,
}

struct Volume {
    dev: Box<dyn BlockDevice>,
    bs: u64,
    v5: bool,
    ftype: bool,
    dblocks: u64,
    fdblocks: u64,
    icount: u64,
    agblocks: u64,
    agcount: u32,
    agblklog: u32,
    inopblog: u32,
    inode_size: u64,
    dir_bs: u64,
    root: u64,
    label: String,
    features: Vec<&'static str>,
    inodes: HashMap<u64, Arc<Inode>>,
    dirs: HashMap<u64, Arc<Vec<Child>>>,
}

impl Volume {
    fn open(mut dev: Box<dyn BlockDevice>) -> Result<Volume> {
        let mut sb = vec![0u8; 512];
        dev.read_at(0, &mut sb)?;
        if &sb[..4] != MAGIC {
            return Err(Error::new(dix_core::ErrorKind::UnknownFormat, "not an XFS file system"));
        }
        let bs = be32(&sb, 4) as u64;
        let version = be16(&sb, 100) & 0xF;
        if !(512..=65536).contains(&bs) || !bs.is_power_of_two() || !(4..=5).contains(&version) {
            return Err(damaged(format!("unknown version {version} or block size {bs}")));
        }
        let v5 = version == 5;
        let features2 = be32(&sb, 200);
        let (ro_compat, incompat) = if v5 { (be32(&sb, 212), be32(&sb, 216)) } else { (0, 0) };
        // What this reader understands of the incompatible features:
        // ftype, sparse inodes, meta uuid, bigtime, needsrepair, nrext64,
        // exchange range, parent pointers (it reads past them).
        const KNOWN: u32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x80;
        if incompat & !KNOWN != 0 {
            return Err(Error::unsupported(format!("XFS features {:#x}", incompat & !KNOWN)));
        }
        let ftype = if v5 { incompat & 0x1 != 0 } else { features2 & 0x200 != 0 };
        let inode_size = be16(&sb, 104) as u64;
        let agblocks = be32(&sb, 84) as u64;
        let agcount = be32(&sb, 88);
        let (agblklog, inopblog) = (sb[124] as u32, sb[123] as u32);
        let min_inode = if v5 { 512 } else { 256 };
        if !inode_size.is_power_of_two() || !(min_inode..=2048).contains(&inode_size) || agblocks == 0 || agcount == 0 {
            return Err(damaged("an impossible geometry"));
        }
        let mut features = vec![if v5 { "v5 (checksums)" } else { "v4" }];
        for (on, name) in [
            (ro_compat & 0x1 != 0, "free inode btree"),
            (ro_compat & 0x2 != 0, "reverse mapping"),
            (ro_compat & 0x4 != 0, "reflink"),
            (incompat & 0x2 != 0, "sparse inodes"),
            (incompat & 0x8 != 0, "big timestamps"),
            (incompat & 0x20 != 0, "large extent counts"),
        ] {
            if on {
                features.push(name);
            }
        }
        let label = String::from_utf8_lossy(&sb[108..120]).trim_end_matches('\0').trim().to_owned();
        Ok(Volume {
            dev,
            bs,
            v5,
            ftype,
            dblocks: be64(&sb, 8),
            fdblocks: be64(&sb, 144),
            icount: be64(&sb, 128),
            agblocks,
            agcount,
            agblklog,
            inopblog,
            inode_size,
            dir_bs: bs << sb[192].min(8),
            root: be64(&sb, 56),
            label,
            features,
            inodes: HashMap::new(),
            dirs: HashMap::new(),
        })
    }

    /// The byte where file system block `fsb` ((AG << agblklog) | block) is.
    fn fsb_to_byte(&self, fsb: u64) -> u64 {
        let ag = fsb >> self.agblklog;
        let block = fsb & ((1u64 << self.agblklog) - 1);
        (ag * self.agblocks + block) * self.bs
    }

    fn inode(&mut self, number: u64) -> Result<Arc<Inode>> {
        if let Some(n) = self.inodes.get(&number) {
            return Ok(n.clone());
        }
        let shift = self.agblklog + self.inopblog;
        let ag = number >> shift;
        let block = (number >> self.inopblog) & ((1u64 << self.agblklog) - 1);
        let slot = number & ((1u64 << self.inopblog) - 1);
        if ag >= self.agcount as u64 {
            return Err(damaged(format!("inode {number} is outside the volume")));
        }
        let at = (ag * self.agblocks + block) * self.bs + slot * self.inode_size;
        let mut raw = vec![0u8; self.inode_size as usize];
        self.dev.read_at(at, &mut raw)?;
        let n = Arc::new(self.parse_inode(number, &raw)?);
        if self.inodes.len() >= CACHE {
            self.inodes.clear();
        }
        self.inodes.insert(number, n.clone());
        Ok(n)
    }

    fn parse_inode(&mut self, number: u64, d: &[u8]) -> Result<Inode> {
        if &d[..2] != b"IN" {
            return Err(damaged(format!("inode {number} is not an inode")));
        }
        let version = d[4];
        let format = d[5];
        let v3 = version >= 3;
        let core = if v3 { 176 } else { 100 };
        let flags = be16(d, 90);
        let flags2 = if v3 { be64(d, 120) } else { 0 };
        let nextents = if flags2 & DIFLAG2_NREXT64 != 0 { be64(d, 24) } else { be32(d, 76) as u64 };
        let forkoff = d[82] as usize * 8;
        let fork_len = if forkoff != 0 { forkoff } else { d.len() - core };
        let fork = d.get(core..core + fork_len).ok_or_else(|| damaged("an inode's fork is too large"))?;
        let time = |at: usize| -> i64 {
            if flags2 & DIFLAG2_BIGTIME != 0 {
                (be64(d, at) / 1_000_000_000) as i64 - (1i64 << 31)
            } else {
                be32(d, at) as i32 as i64
            }
        };
        let mode = be16(d, 2) as u32;
        let size = be64(d, 56);
        if flags & DIFLAG_REALTIME != 0 && mode & S_IFMT == S_IFREG {
            return Err(Error::unsupported("files on an XFS realtime device"));
        }
        let fork = match format {
            FMT_DEV => Fork::Device,
            FMT_LOCAL => Fork::Local(fork.to_vec()),
            FMT_EXTENTS => {
                let n = (nextents as usize).min(fork.len() / 16);
                Fork::Extents((0..n).map(|i| extent(&fork[i * 16..])).collect())
            }
            FMT_BTREE => Fork::Extents(self.btree_extents(fork)?),
            f => return Err(damaged(format!("inode {number}: unknown format {f}"))),
        };
        let mut fork = fork;
        if let Fork::Extents(e) = &mut fork {
            e.sort_by_key(|x| x.off);
        }
        Ok(Inode {
            number,
            mode,
            uid: be32(d, 8),
            gid: be32(d, 12),
            nlink: if version == 1 { be16(d, 6) as u32 } else { be32(d, 16) },
            size,
            atime: time(32),
            mtime: time(40),
            ctime: time(48),
            crtime: v3.then(|| time(144)),
            fork,
        })
    }

    /// Every extent of a B+tree whose root is the fork `root`.
    fn btree_extents(&mut self, root: &[u8]) -> Result<Vec<Extent>> {
        let level = be16(root, 0);
        let n = be16(root, 2) as usize;
        let max = (root.len() - 4) / 16;
        if n > max {
            return Err(damaged("a file's extent tree root is too full"));
        }
        let ptrs: Vec<u64> = (0..n).map(|i| be64(root, 4 + max * 8 + i * 8)).collect();
        let mut out = Vec::new();
        for p in ptrs {
            self.btree_block(p, level.saturating_sub(1), &mut out, 0)?;
        }
        Ok(out)
    }

    fn btree_block(&mut self, fsb: u64, level: u16, out: &mut Vec<Extent>, depth: u32) -> Result<()> {
        if depth > 16 {
            return Err(damaged("a file's extent tree is too deep"));
        }
        let mut b = vec![0u8; self.bs as usize];
        self.dev.read_at(self.fsb_to_byte(fsb), &mut b)?;
        let magic = &b[..4];
        if magic != b"BMAP" && magic != b"BMA3" {
            return Err(damaged("a file's extent tree block has no header"));
        }
        let hdr = if magic == b"BMA3" { 72 } else { 24 };
        let (lv, n) = (be16(&b, 4), be16(&b, 6) as usize);
        if lv != level {
            return Err(damaged("a file's extent tree has a wrong level"));
        }
        if lv == 0 {
            for i in 0..n.min((b.len() - hdr) / 16) {
                out.push(extent(&b[hdr + i * 16..]));
            }
        } else {
            let max = (b.len() - hdr) / 16;
            for i in 0..n.min(max) {
                let p = be64(&b, hdr + max * 8 + i * 8);
                self.btree_block(p, lv - 1, out, depth + 1)?;
            }
        }
        Ok(())
    }

    /// Reads `buf.len()` bytes from `offset` of a fork mapped by extents
    /// (holes and unwritten extents read as zeros).
    fn read_mapped(&mut self, ext: &[Extent], offset: u64, buf: &mut [u8]) -> Result<()> {
        let bs = self.bs;
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let blk = pos / bs;
            let i = ext.partition_point(|e| e.off + e.len <= blk);
            match ext.get(i) {
                Some(e) if e.off <= blk => {
                    let end = (e.off + e.len) * bs;
                    let k = ((end - pos) as usize).min(buf.len() - done);
                    if e.unwritten {
                        buf[done..done + k].fill(0);
                    } else {
                        let at = self.fsb_to_byte(e.at) + (pos - e.off * bs);
                        self.dev.read_at(at, &mut buf[done..done + k])?;
                    }
                    done += k;
                }
                next => {
                    // A hole, up to the next extent.
                    let end = next.map_or(u64::MAX, |e| e.off * bs);
                    let k = (end.saturating_sub(pos).min((buf.len() - done) as u64)) as usize;
                    buf[done..done + k].fill(0);
                    done += k;
                }
            }
        }
        Ok(())
    }

    fn read(&mut self, n: &Inode, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= n.size {
            return Ok(0);
        }
        let want = (n.size - offset).min(buf.len() as u64) as usize;
        match &n.fork {
            Fork::Extents(e) => self.read_mapped(e, offset, &mut buf[..want])?,
            Fork::Local(d) => {
                let src =
                    d.get(offset as usize..offset as usize + want).ok_or_else(|| damaged("inline data is short"))?;
                buf[..want].copy_from_slice(src);
            }
            Fork::Device => return Ok(0),
        }
        Ok(want)
    }

    fn link_target(&mut self, n: &Inode) -> Result<String> {
        let size = n.size as usize;
        let bytes = match &n.fork {
            Fork::Local(d) => d.get(..size).ok_or_else(|| damaged("a link target is short"))?.to_vec(),
            Fork::Extents(ext) => {
                // Remote: in blocks of its own, each with a header on v5
                // (mkfs.xfs -p writes them without one: read as they are).
                let mut out = Vec::with_capacity(size);
                let mut blk = 0u64;
                while out.len() < size && blk < 64 {
                    let mut b = vec![0u8; self.bs as usize];
                    self.read_mapped(ext, blk * self.bs, &mut b)?;
                    let hdr = if self.v5 && &b[..4] == b"XSLM" { 56 } else { 0 };
                    let take = (size - out.len()).min(b.len() - hdr);
                    out.extend_from_slice(&b[hdr..hdr + take]);
                    blk += 1;
                }
                out
            }
            Fork::Device => Vec::new(),
        };
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn list(&mut self, number: u64) -> Result<Arc<Vec<Child>>> {
        if let Some(d) = self.dirs.get(&number) {
            return Ok(d.clone());
        }
        let n = self.inode(number)?;
        if n.kind() != EntryKind::Directory {
            return Err(Error::not_a_directory(format!("XFS inode {number}")));
        }
        let mut out = Vec::new();
        match &n.fork {
            Fork::Local(d) => self.short_form(d, &mut out)?,
            Fork::Extents(ext) => {
                let ext = ext.clone();
                let per = self.dir_bs / self.bs;
                // Every directory block that is there, below the hash index.
                let mut blocks: Vec<u64> = Vec::new();
                for e in &ext {
                    let mut b = e.off / per * per;
                    while b < e.off + e.len && b * self.bs < DIR_LEAF_OFFSET {
                        if blocks.last() != Some(&b) {
                            blocks.push(b);
                        }
                        b += per;
                    }
                }
                let mut buf = vec![0u8; self.dir_bs as usize];
                for b in blocks {
                    self.read_mapped(&ext, b * self.bs, &mut buf)?;
                    self.dir_block(&buf, &mut out)?;
                }
            }
            Fork::Device => {}
        }
        let out = Arc::new(out);
        if self.dirs.len() >= CACHE {
            self.dirs.clear();
        }
        self.dirs.insert(number, out.clone());
        Ok(out)
    }

    /// A folder held in its inode.
    fn short_form(&self, d: &[u8], out: &mut Vec<Child>) -> Result<()> {
        let short = || damaged("a short-form folder ends early");
        let count = *d.first().ok_or_else(short)? as usize;
        let wide = *d.get(1).ok_or_else(short)? != 0;
        let isz = if wide { 8 } else { 4 };
        let mut pos = 2 + isz;
        for _ in 0..count {
            let len = *d.get(pos).ok_or_else(short)? as usize;
            let name = d.get(pos + 3..pos + 3 + len).ok_or_else(short)?;
            let mut at = pos + 3 + len;
            if self.ftype {
                at += 1;
            }
            let ino = d.get(at..at + isz).ok_or_else(short)?;
            let inode = if wide { be64(ino, 0) } else { be32(ino, 0) as u64 };
            out.push(Child { name: String::from_utf8_lossy(name).replace('/', "_"), inode });
            pos = at + isz;
        }
        Ok(())
    }

    /// The entries of one directory data block.
    fn dir_block(&self, b: &[u8], out: &mut Vec<Child>) -> Result<()> {
        let magic = &b[..4];
        let (hdr, block_form) = match magic {
            b"XD2B" => (16, true),
            b"XD2D" => (16, false),
            b"XDB3" => (64, true),
            b"XDD3" => (64, false),
            // A hole or a block of another kind: nothing listed.
            _ => return Ok(()),
        };
        let end = if block_form {
            // The hash index and its tail end the block.
            let count = be32(b, b.len() - 8) as usize;
            b.len().saturating_sub(8 + count * 8)
        } else {
            b.len()
        };
        let mut pos = hdr;
        while pos + 8 <= end {
            if be16(b, pos) == 0xFFFF {
                let len = be16(b, pos + 2) as usize;
                if len < 8 || !len.is_multiple_of(8) {
                    return Err(damaged("a free space record in a folder is wrong"));
                }
                pos += len;
                continue;
            }
            let inode = be64(b, pos);
            let len = b[pos + 8] as usize;
            let name = b.get(pos + 9..pos + 9 + len).ok_or_else(|| damaged("a folder entry is cut"))?;
            let size = (8 + 1 + len + usize::from(self.ftype) + 2).div_ceil(8) * 8;
            if name != b"." && name != b".." {
                out.push(Child { name: String::from_utf8_lossy(name).replace('/', "_"), inode });
            }
            pos += size;
        }
        Ok(())
    }

    fn resolve(&mut self, path: &VfsPath) -> Result<u64> {
        let mut at = self.root;
        for name in path.components() {
            let list = self.list(at)?;
            at = list.iter().find(|c| c.name == name).ok_or_else(|| Error::not_found(path))?.inode;
        }
        Ok(at)
    }

    fn metadata(&mut self, n: &Inode) -> Result<Metadata> {
        let kind = n.kind();
        let mut m = Metadata::new(kind, if kind == EntryKind::Directory { 0 } else { n.size });
        m.modified = Some(n.mtime);
        m.accessed = Some(n.atime);
        m.created = n.crtime;
        m.mode = Some(n.mode);
        m.uid = Some(n.uid);
        m.gid = Some(n.gid);
        m.inode = Some(n.number);
        if kind == EntryKind::Symlink {
            m.symlink_target = Some(self.link_target(n)?);
        }
        let _ = (n.nlink, n.ctime);
        Ok(m)
    }
}

/// The free space of an XFS volume, as byte ranges: what a copy of only
/// the used space leaves out. Each allocation group's free-space B+tree by
/// block number (its root in the group's AGF header) lists its free
/// extents. Blocks on the AG free list are kept (they are reserved for the
/// trees). The journal is not replayed: copy a volume that was unmounted
/// cleanly.
pub fn free_space(dev: Box<dyn BlockDevice>) -> Result<Vec<(u64, u64)>> {
    let mut v = Volume::open(dev)?;
    let mut sb = vec![0u8; 512];
    v.dev.read_at(0, &mut sb)?;
    let sectsize = be16(&sb, 102) as u64;
    let mut out = Vec::new();
    for ag in 0..v.agcount as u64 {
        let base = ag * v.agblocks * v.bs;
        let mut agf = vec![0u8; 512];
        v.dev.read_at(base + sectsize, &mut agf)?;
        if &agf[..4] != b"XAGF" {
            return Err(damaged(format!("allocation group {ag} has no AGF header")));
        }
        let (root, level) = (be32(&agf, 16) as u64, be32(&agf, 28));
        v.free_tree(base, root, level.saturating_sub(1), &mut out, 0)?;
    }
    Ok(dix_core::imaging::merge_ranges(out))
}

impl Volume {
    /// The records of an AG free-space B+tree block (block `agbno` of the
    /// group at byte `base`), as byte ranges.
    fn free_tree(&mut self, base: u64, agbno: u64, level: u32, out: &mut Vec<(u64, u64)>, depth: u32) -> Result<()> {
        if depth > 10 {
            return Err(damaged("a free-space tree is too deep"));
        }
        let mut b = vec![0u8; self.bs as usize];
        self.dev.read_at(base + agbno * self.bs, &mut b)?;
        let hdr = match &b[..4] {
            b"ABTB" => 16,
            b"AB3B" => 56,
            _ => return Err(damaged("a free-space tree block has no header")),
        };
        let (lv, n) = (be16(&b, 4) as u32, be16(&b, 6) as usize);
        if lv != level {
            return Err(damaged("a free-space tree has a wrong level"));
        }
        if lv == 0 {
            for i in 0..n.min((b.len() - hdr) / 8) {
                let (start, count) = (be32(&b, hdr + i * 8) as u64, be32(&b, hdr + i * 8 + 4) as u64);
                out.push((base + start * self.bs, base + (start + count) * self.bs));
            }
        } else {
            let max = (b.len() - hdr) / 12;
            for i in 0..n.min(max) {
                let p = be32(&b, hdr + max * 8 + i * 4) as u64;
                self.free_tree(base, p, lv - 1, out, depth + 1)?;
            }
        }
        Ok(())
    }
}

/// An extent record: 128 bits, big-endian, packed.
fn extent(r: &[u8]) -> Extent {
    let (l0, l1) = (be64(r, 0), be64(r, 8));
    Extent {
        unwritten: l0 >> 63 != 0,
        off: (l0 & ((1u64 << 63) - 1)) >> 9,
        at: ((l0 & 0x1FF) << 43) | (l1 >> 21),
        len: l1 & 0x1F_FFFF,
    }
}

pub struct Xfs {
    v: Volume,
}

impl FileSystem for Xfs {
    fn info(&self) -> FsInfo {
        let v = &self.v;
        FsInfo {
            driver_id: "xfs".into(),
            fs_name: "XFS".into(),
            label: (!v.label.is_empty()).then(|| v.label.clone()),
            total_bytes: Some(v.dblocks * v.bs),
            free_bytes: Some(v.fdblocks * v.bs),
            block_size: Some(v.bs as u32),
            details: vec![
                ("Format".into(), v.features.join(", ")),
                ("Allocation groups".into(), v.agcount.to_string()),
                ("Inode size".into(), format!("{} bytes", v.inode_size)),
                ("Inodes".into(), v.icount.to_string()),
            ],
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
            out.push(DirEntry { name: c.name.clone(), metadata: self.v.metadata(&n)? });
        }
        Ok(out)
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        let r = self.v.resolve(path)?;
        let n = self.v.inode(r)?;
        self.v.metadata(&n)
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let r = self.v.resolve(path)?;
        let n = self.v.inode(r)?;
        match n.kind() {
            EntryKind::Directory => Err(Error::is_a_directory(path)),
            EntryKind::File => self.v.read(&n, offset, buf),
            _ => Ok(0),
        }
    }
}
