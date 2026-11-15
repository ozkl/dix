//! Directories: blocks of entries (inode, record length, name length,
//! file type, name), searched linearly, with an in-memory index of the
//! directories in use ([`DirIndex`]).
//!
//! With `metadata_csum` each block ends in a 12-byte fake entry, the tail,
//! holding the block's checksum. Indexed (`dir_index`, htree) directories
//! are kept indexed ([`super::htree`]): names are added, looked up and
//! removed where their hash leads. Linear ones get an in-memory index,
//! built by one scan. Listing reads every block (an indexed directory's
//! index blocks look like empty entries). A structurally broken index
//! falls back to linear lookups; a change then fails with `metadata_csum`
//! (EUCLEAN), else the directory goes on as a linear one, as on Linux.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use super::htree::COMPAT_DIR_INDEX;
use super::inode::{DiskInode, INDEX_FL, Time};
use super::{Ext4Fs, INCOMPAT_FILETYPE, Meta, csum, le16, le32, put16, put32};
use crate::ext4::kernel::errno::{Errno, KResult};
use crate::ext4::kernel::fs::DirEntry;
use crate::ext4::kernel::fs::vfs::FileType;

/// What a directory holds, so that looking up, adding and removing an
/// entry do not scan every block of a large one. Built by one scan on
/// first use, kept up to date by [`Ext4Fs::dir_add`] and
/// [`Ext4Fs::dir_remove`]; dropped by anything else that changes the
/// directory's blocks, and by `drop_caches`.
#[derive(Default)]
pub struct DirIndex {
    /// Name -> (inode, block holding the entry).
    pub(super) names: BTreeMap<Vec<u8>, (u32, u64)>,
    /// Block -> the largest new entry it has room for.
    pub(super) room: BTreeMap<u64, usize>,
}

/// Directory entry file type codes (with the `filetype` feature).
fn dirent_type(kind: FileType) -> u8 {
    match kind {
        FileType::Regular => 1,
        FileType::Directory => 2,
        FileType::CharDevice => 3,
        FileType::BlockDevice => 4,
        FileType::Fifo => 5,
        FileType::Socket => 6,
        FileType::Symlink => 7,
    }
}

/// A file type code as a d_type (without the filetype feature the type is
/// unknown, DT_UNKNOWN).
fn d_type(kind: u8) -> u8 {
    match kind {
        1 => 8,
        2 => 4,
        3 => 2,
        4 => 6,
        5 => 1,
        6 => 12,
        7 => 10,
        _ => 0,
    }
}

/// Is the end of block `b` a checksum tail?
fn has_tail(b: &[u8]) -> bool {
    let o = b.len() - 12;
    le32(b, o) == 0 && le16(b, o + 4) == 12 && b[o + 6] == 0 && b[o + 7] == 0xDE
}

pub(super) fn write_tail(b: &mut [u8]) {
    let o = b.len() - 12;
    b[o..].fill(0);
    put16(b, o + 4, 12);
    b[o + 7] = 0xDE;
}

/// One entry: (offset, inode, rec_len, name_len, file type, name).
type Entry<'a> = (usize, u32, usize, usize, u8, &'a [u8]);

impl Ext4Fs {
    /// Where entries may go in a block (before the tail).
    pub(super) fn dir_limit(&self) -> usize {
        if self.csum { self.block_size - 12 } else { self.block_size }
    }

    fn filetype(&self) -> bool {
        self.incompat & INCOMPAT_FILETYPE != 0
    }

    fn dir_csum(&self, ino: u32, generation: u32, b: &[u8]) -> u32 {
        csum::crc32c(self.inode_seed(ino, generation), &b[..b.len() - 12])
    }

    /// A changed block of directory `ino`: its checksum again (a leaf's
    /// tail, or an index block's).
    pub(super) fn set_dir_csum(&self, ino: u32, generation: u32, b: &mut [u8]) {
        if !self.csum {
            return;
        }
        if has_tail(b) {
            let c = self.dir_csum(ino, generation, b);
            let n = b.len();
            put32(b, n - 4, c);
        } else if let Some(off) = self.dx_count_offset(b) {
            self.set_dx_csum(ino, generation, b, off);
        }
    }

    /// Change block `block` of directory `ino` (and its checksum).
    fn modify_dir_block<R>(
        &self,
        ino: u32,
        generation: u32,
        block: u64,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> KResult<R> {
        self.cache.modify(block, |b| {
            let r = f(b);
            self.set_dir_csum(ino, generation, b);
            r
        })
    }

    /// Is directory block `b` of `ino` as its checksum says (a leaf's
    /// tail, an index block's; without checksums: yes)?
    pub(super) fn dir_block_ok(&self, ino: u32, generation: u32, b: &[u8]) -> bool {
        if !self.csum {
            true
        } else if has_tail(b) {
            le32(b, b.len() - 4) == self.dir_csum(ino, generation, b)
        } else if let Some(off) = self.dx_count_offset(b) {
            self.dx_csum_ok(ino, generation, b, off)
        } else {
            true
        }
    }

    /// Call `f(block number, block bytes)` for each block of directory
    /// `ino` (`d`) until it returns `Some`; blocks with tails are checked.
    pub(super) fn scan_dir<R>(
        &self,
        meta: &mut Meta,
        ino: u32,
        d: &mut DiskInode,
        mut f: impl FnMut(u64, &[u8]) -> Option<R>,
    ) -> KResult<Option<R>> {
        let blocks = d.size / self.block_size as u64;
        for lb in 0..blocks {
            let pb = self.bmap(meta, ino, d, lb, false)?;
            if pb == 0 {
                continue;
            }
            let b = self.cache.read(pb, |b| b.to_vec())?;
            let ok = self.dir_block_ok(ino, d.generation, &b);
            if !ok {
                return super::corrupt(
                    &self.disk_name,
                    format_args!("inode #{ino}: directory block {lb}: checksum invalid"),
                    Errno::EBADMSG,
                );
            }
            if let Some(r) = f(pb, &b) {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    /// The entries of a directory block (up to its tail).
    pub(super) fn entries<'a>(&self, b: &'a [u8]) -> impl Iterator<Item = Entry<'a>> + 'a {
        let filetype = self.filetype();
        let tail = if self.csum && has_tail(b) { b.len() - 12 } else { b.len() };
        let mut off = 0;
        core::iter::from_fn(move || {
            if off + 8 > tail {
                return None;
            }
            let ino = le32(b, off);
            let rec_len = le16(b, off + 4) as usize;
            let (name_len, kind) =
                if filetype { (b[off + 6] as usize, b[off + 7]) } else { (le16(b, off + 6) as usize, 0) };
            if rec_len < 8 || off + rec_len > b.len() || 8 + name_len > rec_len {
                return None;
            }
            let item = (off, ino, rec_len, name_len, kind, &b[off + 8..off + 8 + name_len]);
            off += rec_len;
            Some(item)
        })
    }

    /// The largest entry block `b` has room for (an unused entry, or the
    /// slack after one).
    pub(super) fn block_room(&self, b: &[u8]) -> usize {
        self.entries(b)
            .map(|(_, ino, rec_len, name_len, _, _)| {
                let used = if ino == 0 { 0 } else { (8 + name_len).next_multiple_of(4) };
                rec_len - used
            })
            .max()
            .unwrap_or(0)
    }

    /// The index of directory `dir` (inode `d`), built if needed.
    fn dir_index<'m>(&self, meta: &'m mut Meta, dir: u32, d: &mut DiskInode) -> KResult<&'m mut DirIndex> {
        if !meta.dirs.contains_key(&dir) {
            let mut index = DirIndex::default();
            let indexed = d.flags & INDEX_FL != 0;
            self.scan_dir(meta, dir, d, |block, b| {
                for (_, ino, _, _, _, name) in self.entries(b) {
                    if ino != 0 {
                        index.names.insert(name.to_vec(), (ino, block));
                    }
                }
                // (Index blocks hold no names: no room there.)
                if !indexed || self.dx_count_offset(b).is_none() {
                    index.room.insert(block, self.block_room(b));
                }
                None::<()>
            })?;
            meta.dirs.insert(dir, index);
        }
        Ok(meta.dirs.get_mut(&dir).unwrap())
    }

    pub(super) fn dir_lookup(
        &self,
        meta: &mut Meta,
        dir: u32,
        d: &mut DiskInode,
        name: &str,
    ) -> KResult<Option<u32>> {
        Ok(self.dir_find(meta, dir, d, name)?.map(|(ino, _)| ino))
    }

    /// Is directory `dir` (`d`) used through its hashed index?
    fn uses_dx(&self, d: &DiskInode) -> bool {
        d.flags & INDEX_FL != 0 && self.compat & COMPAT_DIR_INDEX != 0
    }

    /// Entry `name` of directory `dir`: (inode, block holding it). An
    /// indexed directory is searched through its index (the leaf the hash
    /// leads to), a structurally broken index falling back to a linear
    /// search as Linux does; others through the in-memory index.
    fn dir_find(
        &self,
        meta: &mut Meta,
        dir: u32,
        d: &mut DiskInode,
        name: &str,
    ) -> KResult<Option<(u32, u64)>> {
        if self.uses_dx(d) && !meta.dirs.contains_key(&dir) {
            match self.dx_find(meta, dir, d, name) {
                Err(Errno::EUCLEAN) => {
                    kinfo!(
                        "EXT4-fs ({}): inode #{dir}: htree lookup failed, searching linearly",
                        self.disk_name
                    )
                }
                r => return r,
            }
        }
        Ok(self.dir_index(meta, dir, d)?.names.get(name.as_bytes()).copied())
    }

    /// The in-memory index of `dir`, kept up to date where there is one:
    /// built for a linear directory, left alone (absent) for an indexed one.
    fn known_index<'m>(
        &self,
        meta: &'m mut Meta,
        dir: u32,
        d: &mut DiskInode,
    ) -> KResult<Option<&'m mut DirIndex>> {
        if self.uses_dx(d) {
            return Ok(meta.dirs.get_mut(&dir));
        }
        self.dir_index(meta, dir, d).map(Some)
    }

    /// The first block of new directory `ino` (`d`): "." and "..".
    pub(super) fn dir_init(&self, meta: &mut Meta, ino: u32, d: &mut DiskInode, parent: u32) -> KResult<()> {
        let pb = self.bmap(meta, ino, d, 0, true)?;
        self.cache.zero(pb)?;
        let (limit, ft, csum) = (self.dir_limit(), self.filetype(), self.csum);
        self.modify_dir_block(ino, d.generation, pb, |b| {
            put32(b, 0, ino);
            put16(b, 4, 12);
            b[6] = 1;
            b[7] = if ft { 2 } else { 0 };
            b[8] = b'.';
            put32(b, 12, parent);
            put16(b, 16, (limit - 12) as u16);
            b[18] = 2;
            b[19] = if ft { 2 } else { 0 };
            b[20] = b'.';
            b[21] = b'.';
            if csum {
                write_tail(b);
            }
        })?;
        d.size = self.block_size as u64;
        Ok(())
    }

    /// Put entry `name` -> `ino` into block `block` of directory `dir`, if
    /// it has room (an unused entry, or the slack after one).
    pub(super) fn dir_insert(
        &self,
        dir: u32,
        generation: u32,
        block: u64,
        name: &str,
        ino: u32,
        kind: FileType,
    ) -> KResult<bool> {
        let need = (8 + name.len()).next_multiple_of(4);
        let ftype = if self.filetype() { dirent_type(kind) } else { 0 };
        let slot = self.cache.read(block, |b| {
            self.entries(b).find_map(|(off, e_ino, rec_len, name_len, _, _)| {
                let used = if e_ino == 0 { 0 } else { (8 + name_len).next_multiple_of(4) };
                (rec_len - used >= need).then_some((off, used, rec_len))
            })
        })?;
        let Some((off, used, rec_len)) = slot else { return Ok(false) };
        self.modify_dir_block(dir, generation, block, |b| {
            let (at, rec) = if used == 0 {
                (off, rec_len)
            } else {
                put16(b, off + 4, used as u16);
                (off + used, rec_len - used)
            };
            put32(b, at, ino);
            put16(b, at + 4, rec as u16);
            b[at + 6] = name.len() as u8;
            b[at + 7] = ftype;
            b[at + 8..at + 8 + name.len()].copy_from_slice(name.as_bytes());
        })?;
        Ok(true)
    }

    /// Add entry `name` -> `ino` to directory `dir` (inode `d`): where its
    /// hash leads in an indexed directory; else into a block with room, a
    /// one-block directory that is full becoming indexed (with
    /// `dir_index`), others growing by a block.
    pub(super) fn dir_add(
        &self,
        meta: &mut Meta,
        dir: u32,
        d: &mut DiskInode,
        name: &str,
        ino: u32,
        kind: FileType,
    ) -> KResult<()> {
        let dir_index = self.compat & COMPAT_DIR_INDEX != 0;
        let generation = d.generation;
        let mut block = None;
        if d.flags & INDEX_FL != 0 {
            if !dir_index {
                d.flags &= !INDEX_FL;
                meta.dirs.remove(&dir);
            } else {
                match self.dx_add(meta, dir, d, name, ino, kind) {
                    Ok(b) => block = Some(b),
                    // A broken index: on as a linear directory (as Linux,
                    // without checksums; with them it is an error).
                    Err(Errno::EUCLEAN) if !self.csum => {
                        kinfo!(
                            "EXT4-fs ({}): inode #{dir}: htree ignored, the directory is linear",
                            self.disk_name
                        );
                        d.flags &= !INDEX_FL;
                        meta.dirs.remove(&dir);
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        if block.is_none() {
            // Room in an existing block (the index says which).
            let need = (8 + name.len()).next_multiple_of(4);
            let index = self.dir_index(meta, dir, d)?;
            let candidate = index.room.iter().find(|&(_, &room)| room >= need).map(|(&b, _)| b);
            if let Some(b) = candidate
                && self.dir_insert(dir, generation, b, name, ino, kind)?
            {
                block = Some(b);
            }
        }
        if block.is_none()
            && dir_index
            && d.size == self.block_size as u64
            && self.dx_make_indexed(meta, dir, d)?
        {
            block = Some(self.dx_add(meta, dir, d, name, ino, kind)?);
        }
        let block = match block {
            Some(b) => b,
            None => {
                // A new block at the end of the directory.
                let lb = d.size / self.block_size as u64;
                let pb = self.bmap(meta, dir, d, lb, true)?;
                self.cache.zero(pb)?;
                let (limit, csum) = (self.dir_limit(), self.csum);
                self.modify_dir_block(dir, generation, pb, |b| {
                    put16(b, 4, limit as u16);
                    if csum {
                        write_tail(b);
                    }
                })?;
                d.size += self.block_size as u64;
                if !self.dir_insert(dir, generation, pb, name, ino, kind)? {
                    return Err(Errno::EUCLEAN);
                }
                pb
            }
        };
        let room = self.cache.read(block, |b| self.block_room(b))?;
        if let Some(index) = self.known_index(meta, dir, d)? {
            index.names.insert(name.as_bytes().to_vec(), (ino, block));
            index.room.insert(block, room);
        }
        d.mtime = Time::now();
        d.ctime = d.mtime;
        self.write_inode(dir, d)
    }

    /// Remove entry `name` from directory `dir`; returns its inode number.
    pub(super) fn dir_remove(
        &self,
        meta: &mut Meta,
        dir: u32,
        d: &mut DiskInode,
        name: &str,
    ) -> KResult<u32> {
        // The index (hashed or in memory) says which block holds it.
        let (_, block) = self.dir_find(meta, dir, d, name)?.ok_or(Errno::ENOENT)?;
        let found = self.cache.read(block, |b| {
            let mut prev = None;
            for (off, ino, rec_len, _, _, n) in self.entries(b) {
                if ino != 0 && n == name.as_bytes() {
                    return Some((off, ino, rec_len, prev));
                }
                prev = Some(off);
            }
            None
        })?;
        let (off, ino, rec_len, prev) = found.ok_or(Errno::EUCLEAN)?;
        self.modify_dir_block(dir, d.generation, block, |b| match prev {
            // Merge into the previous entry.
            Some(p) => {
                let len = le16(b, p + 4) as usize + rec_len;
                put16(b, p + 4, len as u16);
            }
            None => put32(b, off, 0),
        })?;
        let room = self.cache.read(block, |b| self.block_room(b))?;
        if let Some(index) = self.known_index(meta, dir, d)? {
            index.names.remove(name.as_bytes());
            index.room.insert(block, room);
        }
        d.mtime = Time::now();
        d.ctime = d.mtime;
        self.write_inode(dir, d)?;
        Ok(ino)
    }

    /// Point the `..` entry of directory `ino` (`d`) at `parent`.
    pub(super) fn set_dotdot(
        &self,
        meta: &mut Meta,
        ino: u32,
        d: &mut DiskInode,
        parent: u32,
    ) -> KResult<()> {
        let found = self.scan_dir(meta, ino, d, |block, b| {
            self.entries(b).find(|e| e.5 == b"..").map(|e| (block, e.0))
        })?;
        let (block, off) = found.ok_or(Errno::EUCLEAN)?;
        self.modify_dir_block(ino, d.generation, block, |b| put32(b, off, parent))?;
        // ("..": not worth keeping the index up to date for.)
        meta.dirs.remove(&ino);
        self.write_inode(ino, d)
    }

    pub(super) fn dir_is_empty(&self, meta: &mut Meta, ino: u32, d: &mut DiskInode) -> KResult<bool> {
        let other = self.scan_dir(meta, ino, d, |_, b| {
            self.entries(b).find(|e| e.1 != 0 && e.5 != b"." && e.5 != b"..").map(|_| ())
        })?;
        Ok(other.is_none())
    }

    /// The entries of directory `ino`, for readdir. An indexed directory
    /// lists in hash order, positions the hashes (as Linux's
    /// ext4_dx_readdir; a broken index: block order, as Linux falls back
    /// to); others in block order, positions the entries' byte offsets
    /// (blocks are only ever appended). Either way deleting entries moves
    /// no other's position.
    pub(super) fn dir_list(&self, meta: &mut Meta, ino: u32, d: &mut DiskInode) -> KResult<Vec<DirEntry>> {
        if self.uses_dx(d) {
            match self.dx_list(meta, ino, d) {
                Ok(list) => {
                    return Ok(list
                        .into_iter()
                        .map(|(e_ino, kind, name, pos)| DirEntry {
                            pos: Some(pos),
                            name: String::from_utf8_lossy(&name).into_owned(),
                            ino: e_ino as u64,
                            kind: d_type(kind),
                        })
                        .collect());
                }
                Err(Errno::EUCLEAN) => {}
                Err(e) => return Err(e),
            }
        }
        let mut out = Vec::new();
        let mut base = 0u64;
        self.scan_dir(meta, ino, d, |_, b| {
            let block_base = base;
            base += b.len() as u64;
            for (off, ino, _, _, kind, name) in self.entries(b) {
                if ino == 0 || name == b"." || name == b".." {
                    continue;
                }
                out.push(DirEntry {
                    pos: Some(block_base + off as u64),
                    name: String::from_utf8_lossy(name).into_owned(),
                    ino: ino as u64,
                    kind: d_type(kind),
                });
            }
            None::<()>
        })?;
        Ok(out)
    }
}
