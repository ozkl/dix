//! The journal (jbd2): metadata changes are first written to a log in the
//! journal's inode, then in place, so that a crash in between is undone
//! (the log replayed) at the next mount.
//!
//! The log's blocks follow its superblock (`s_first`..`s_maxlen`, then
//! wrapping), each transaction tagged with its sequence number: descriptor
//! blocks (the home of each logged block, in tags), the logged blocks,
//! revoke blocks (blocks not to replay from earlier transactions), and a
//! commit block ending it. With `csum_v2`/`csum_v3` descriptor, revoke and
//! commit blocks carry a crc32c, and (v3) each tag its block's.
//!
//! Recovery ([`recover`]) is Linux's three passes in one walk: the
//! committed transactions are found (checksums checked), revoke records
//! gathered, and each logged block written home unless a revoke from the
//! same or a later transaction covers it.
//!
//! Commits ([`Journal::commit`]) log every changed metadata block as one
//! transaction (descriptor blocks, the blocks, a commit block), make it
//! durable, write the blocks in place (the checkpoint) and mark the
//! journal empty again: one transaction in the log at a time, never a
//! revoke needed. The blocks wait in the buffer cache until then (it
//! holds them); one that starts with the journal's magic number is logged
//! escaped. Like Linux, the journal gets `csum_v3` on a `metadata_csum`
//! file system and `64bit` on a 64-bit one.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::{Ext4Fs, csum, le16, le32};
use crate::ext4::kernel::block::Disk;
use crate::ext4::kernel::errno::{Errno, KResult};
use crate::ext4::kernel::sync::Mutex;

const MAGIC: u32 = 0xC03B_3998;
const DESCRIPTOR: u32 = 1;
const COMMIT: u32 = 2;
const SUPERBLOCK_V1: u32 = 3;
const SUPERBLOCK_V2: u32 = 4;
const REVOKE: u32 = 5;

const INCOMPAT_REVOKE: u32 = 0x1;
const INCOMPAT_64BIT: u32 = 0x2;
const INCOMPAT_ASYNC_COMMIT: u32 = 0x4;
const INCOMPAT_CSUM_V2: u32 = 0x8;
const INCOMPAT_CSUM_V3: u32 = 0x10;
const INCOMPAT_FAST_COMMIT: u32 = 0x20;
const INCOMPAT_KNOWN: u32 = INCOMPAT_REVOKE
    | INCOMPAT_64BIT
    | INCOMPAT_ASYNC_COMMIT
    | INCOMPAT_CSUM_V2
    | INCOMPAT_CSUM_V3
    | INCOMPAT_FAST_COMMIT;
const COMPAT_CHECKSUM: u32 = 0x1;
const CRC32C_CHKSUM: u8 = 4;

const TAG_ESCAPE: u32 = 1;
const TAG_SAME_UUID: u32 = 2;
const TAG_LAST: u32 = 8;

// Journal superblock fields (big-endian).
const JSB_BLOCKSIZE: usize = 12;
const JSB_MAXLEN: usize = 16;
const JSB_FIRST: usize = 20;
const JSB_SEQUENCE: usize = 24;
const JSB_START: usize = 28;
const JSB_COMPAT: usize = 36;
const JSB_INCOMPAT: usize = 40;
const JSB_UUID: usize = 48;
const JSB_CHECKSUM_TYPE: usize = 0x50;
const JSB_CHECKSUM: usize = 0xFC;

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

fn put_be32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}

fn put_be16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_be_bytes());
}

/// The journal's format, from its superblock.
#[derive(Clone)]
struct Format {
    block_size: usize,
    first: u32,
    maxlen: u32,
    wide: bool,
    v2: bool,
    v3: bool,
    seed: u32,
    uuid: [u8; 16],
}

impl Format {
    fn parse(jsb: &[u8], block_size: usize) -> KResult<Format> {
        let kind = be32(jsb, 4);
        if be32(jsb, 0) != MAGIC || (kind != SUPERBLOCK_V1 && kind != SUPERBLOCK_V2) {
            kinfo!("JBD2: no valid journal superblock found");
            return Err(Errno::EINVAL);
        }
        if be32(jsb, JSB_BLOCKSIZE) as usize != block_size {
            kinfo!("JBD2: journal block size differs from the file system's");
            return Err(Errno::EINVAL);
        }
        let incompat = if kind == SUPERBLOCK_V2 { be32(jsb, JSB_INCOMPAT) } else { 0 };
        if incompat & !INCOMPAT_KNOWN != 0 {
            kinfo!("JBD2: unknown journal features {:#x}", incompat & !INCOMPAT_KNOWN);
            return Err(Errno::EINVAL);
        }
        let uuid: [u8; 16] = jsb[JSB_UUID..JSB_UUID + 16].try_into().unwrap();
        Ok(Format {
            block_size,
            first: be32(jsb, JSB_FIRST),
            maxlen: be32(jsb, JSB_MAXLEN),
            wide: incompat & INCOMPAT_64BIT != 0,
            v2: incompat & INCOMPAT_CSUM_V2 != 0,
            v3: incompat & INCOMPAT_CSUM_V3 != 0,
            seed: csum::crc32c(!0, &uuid),
            uuid,
        })
    }

    fn checksums(&self) -> bool {
        self.v2 || self.v3
    }

    /// Bytes of a block tag (without the UUID that may follow it). With
    /// csum_v3 always 16 (block number low, flags, block number high, a
    /// 32-bit checksum). Otherwise: block number low (4), a 16-bit
    /// checksum field and the flags (2 + 2), block number high with 64bit
    /// (4), and 2 more bytes with csum_v2 (the tag size the format gives
    /// that version).
    fn tag_bytes(&self) -> usize {
        if self.v3 {
            return 16;
        }
        let high = if self.wide { 4 } else { 0 };
        let v2_extra = if self.v2 { 2 } else { 0 };
        4 + 2 + 2 + high + v2_extra
    }

    /// Bytes at the end of descriptor and revoke blocks (the checksum).
    fn tail(&self) -> usize {
        if self.checksums() { 4 } else { 0 }
    }

    fn next(&self, pos: u32) -> u32 {
        if pos + 1 >= self.maxlen { self.first } else { pos + 1 }
    }

    /// A descriptor or revoke block's checksum (its tail as zero).
    fn block_csum(&self, b: &[u8]) -> u32 {
        let n = b.len();
        let c = csum::crc32c(self.seed, &b[..n - 4]);
        csum::crc32c(c, &[0; 4])
    }

    /// A commit block's checksum (the checksum field as zero).
    fn commit_csum(&self, b: &[u8]) -> u32 {
        let c = csum::crc32c(self.seed, &b[..16]);
        let c = csum::crc32c(c, &[0; 4]);
        csum::crc32c(c, &b[20..])
    }

    /// A logged block's checksum, as its tag holds it.
    fn data_csum(&self, sequence: u32, data: &[u8]) -> u32 {
        let c = csum::crc32c(self.seed, &sequence.to_be_bytes());
        csum::crc32c(c, data)
    }
}

/// The journal superblock's checksum (v2, v3).
fn superblock_csum(jsb: &[u8]) -> u32 {
    let c = csum::crc32c(!0, &jsb[..JSB_CHECKSUM]);
    let c = csum::crc32c(c, &[0; 4]);
    csum::crc32c(c, &jsb[JSB_CHECKSUM + 4..1024])
}

fn read_block(disk: &Disk, block: u64, bs: usize) -> KResult<Vec<u8>> {
    let mut b = vec![0u8; bs];
    disk.read_bytes(block * bs as u64, &mut b)?;
    Ok(b)
}

/// Where the journal inode's blocks are (from the superblock `sb`,
/// without the file system: recovery comes before it is read).
fn journal_map(disk: &Disk, sb: &[u8]) -> KResult<Vec<u64>> {
    let bs = 1024usize << le32(sb, 24);
    let ino = le32(sb, 0xE0);
    let ipg = le32(sb, 40);
    let inode_size = if le32(sb, 76) == 0 { 128 } else { le16(sb, 88) as usize };
    let wide = le32(sb, 96) & super::INCOMPAT_64BIT != 0;
    let desc_size = if wide { le16(sb, 0xFE) as usize } else { 32 };
    if ino == 0 || ipg == 0 {
        return Err(Errno::EINVAL);
    }
    // The inode's group descriptor (the journal inode is in an early group:
    // before any meta_bg).
    let g = ((ino - 1) / ipg) as usize;
    let gdt = le32(sb, 20) as u64 + 1;
    let block = gdt + (g * desc_size / bs) as u64;
    let d = read_block(disk, block, bs)?;
    let off = g * desc_size % bs;
    let table =
        le32(&d, off + 8) as u64 | if desc_size >= 64 { (le32(&d, off + 40) as u64) << 32 } else { 0 };
    let byte = ((ino - 1) % ipg) as usize * inode_size;
    let b = read_block(disk, table + (byte / bs) as u64, bs)?;
    let raw = &b[byte % bs..byte % bs + inode_size];
    let size = le32(raw, 4) as u64 | (le32(raw, 108) as u64) << 32;
    let nblocks = (size / bs as u64) as usize;
    let mut map = vec![0u64; nblocks];
    let block_words: Vec<u32> = (0..15).map(|i| le32(raw, 40 + 4 * i)).collect();
    if le32(raw, 32) & super::inode::EXTENTS_FL != 0 {
        let root: Vec<u8> = block_words.iter().flat_map(|w| w.to_le_bytes()).collect();
        map_extents(disk, bs, &root, &mut map, 0)?;
    } else {
        for (i, &b) in block_words.iter().take(12).enumerate() {
            if i < nblocks {
                map[i] = b as u64;
            }
        }
        let mut next = 12usize;
        for (level, &b) in block_words[12..15].iter().enumerate() {
            if b != 0 {
                map_indirect(disk, bs, b as u64, level as u32, &mut map, &mut next)?;
            } else {
                next += (bs / 4).pow(level as u32 + 1);
            }
        }
    }
    if map.is_empty() || map.contains(&0) {
        kinfo!("JBD2: journal inode {ino} has holes");
        return Err(Errno::EUCLEAN);
    }
    Ok(map)
}

fn map_extents(disk: &Disk, bs: usize, n: &[u8], map: &mut [u64], level: u32) -> KResult<()> {
    if le16(n, 0) != 0xF30A || level > 5 {
        return Err(Errno::EUCLEAN);
    }
    let (count, depth) = (le16(n, 2) as usize, le16(n, 6));
    for i in 0..count {
        let o = 12 + 12 * i;
        if depth > 0 {
            let child = le32(n, o + 4) as u64 | (le16(n, o + 8) as u64) << 32;
            let c = read_block(disk, child, bs)?;
            map_extents(disk, bs, &c, map, level + 1)?;
        } else {
            let (lb, mut len) = (le32(n, o) as usize, le16(n, o + 4) as u64);
            if len > 32768 {
                len -= 32768;
            }
            let start = le32(n, o + 8) as u64 | (le16(n, o + 6) as u64) << 32;
            for k in 0..len as usize {
                if let Some(slot) = map.get_mut(lb + k) {
                    *slot = start + k as u64;
                }
            }
        }
    }
    Ok(())
}

fn map_indirect(
    disk: &Disk,
    bs: usize,
    block: u64,
    level: u32,
    map: &mut [u64],
    next: &mut usize,
) -> KResult<()> {
    let t = read_block(disk, block, bs)?;
    for i in 0..bs / 4 {
        let e = le32(&t, 4 * i) as u64;
        if level == 0 {
            if let Some(slot) = map.get_mut(*next) {
                *slot = e;
            }
            *next += 1;
        } else if e != 0 {
            map_indirect(disk, bs, e, level - 1, map, next)?;
        } else {
            *next += (bs / 4).pow(level);
        }
        if *next >= map.len() {
            break;
        }
    }
    Ok(())
}

/// One logged block of a transaction: (home block, place in the log,
/// escaped, tag checksum).
type Logged = (u64, u32, bool, u32);

/// Is transaction `a` the same as or later than transaction `b`?
/// Sequence numbers are 32 bits and wrap around (after 0xffffffff comes
/// 0): the one ahead is the one less than half the number space in front.
pub(super) fn seq_at_or_after(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) as i32 >= 0
}

/// Replay the journal of the file system whose superblock is `sb` (an
/// empty one: nothing to do), then mark it empty.
pub fn recover(disk: &Arc<Disk>, sb: &[u8]) -> KResult<()> {
    let bs = 1024usize << le32(sb, 24);
    let map = journal_map(disk, sb)?;
    let mut jsb = read_block(disk, map[0], bs)?;
    let f = Format::parse(&jsb, bs)?;
    if f.maxlen as usize > map.len() || f.first == 0 || f.first >= f.maxlen {
        return Err(Errno::EUCLEAN);
    }
    if f.v3 && be32(&jsb, JSB_CHECKSUM) != superblock_csum(&jsb) {
        kinfo!("JBD2: journal checksum error");
        return Err(Errno::EBADMSG);
    }
    let start = be32(&jsb, JSB_START);
    if start == 0 {
        return Ok(());
    }
    let read = |pos: u32| read_block(disk, map[pos as usize], bs);
    let mut pos = start;
    let mut seq = be32(&jsb, JSB_SEQUENCE);
    let mut pending: Vec<Logged> = Vec::new();
    let mut pending_revokes: Vec<u64> = Vec::new();
    let mut committed: Vec<(u32, Vec<Logged>)> = Vec::new();
    let mut revoked: BTreeMap<u64, u32> = BTreeMap::new();
    // At most one walk around the log.
    for _ in 0..f.maxlen {
        let b = read(pos)?;
        if be32(&b, 0) != MAGIC || be32(&b, 8) != seq {
            break;
        }
        match be32(&b, 4) {
            DESCRIPTOR => {
                if f.checksums() && be32(&b, bs - 4) != f.block_csum(&b) {
                    kinfo!("JBD2: invalid checksum recovering descriptor block in transaction {seq}");
                    break;
                }
                let mut off = 12;
                while off + f.tag_bytes() <= bs - f.tail() {
                    let (flags, mut home) = if f.v3 {
                        (be32(&b, off + 4), be32(&b, off) as u64 | (be32(&b, off + 8) as u64) << 32)
                    } else {
                        let flags = u16::from_be_bytes([b[off + 6], b[off + 7]]) as u32;
                        (flags, be32(&b, off) as u64)
                    };
                    if !f.v3 && f.wide {
                        home |= (be32(&b, off + 8) as u64) << 32;
                    }
                    let tag_csum = if f.v3 {
                        be32(&b, off + 12)
                    } else if f.v2 {
                        u16::from_be_bytes([b[off + 4], b[off + 5]]) as u32
                    } else {
                        0
                    };
                    off += f.tag_bytes();
                    if flags & TAG_SAME_UUID == 0 {
                        off += 16;
                    }
                    pos = f.next(pos);
                    pending.push((home, pos, flags & TAG_ESCAPE != 0, tag_csum));
                    if flags & TAG_LAST != 0 {
                        break;
                    }
                }
            }
            REVOKE => {
                if f.checksums() && be32(&b, bs - 4) != f.block_csum(&b) {
                    kinfo!("JBD2: invalid checksum recovering revoke block in transaction {seq}");
                    break;
                }
                let used = (be32(&b, 12) as usize).min(bs - f.tail());
                let rec = if f.wide { 8 } else { 4 };
                let mut off = 16;
                while off + rec <= used {
                    let block = if f.wide {
                        u64::from_be_bytes(b[off..off + 8].try_into().unwrap())
                    } else {
                        be32(&b, off) as u64
                    };
                    pending_revokes.push(block);
                    off += rec;
                }
            }
            COMMIT => {
                if f.checksums() && be32(&b, 16) != f.commit_csum(&b) {
                    kinfo!("JBD2: invalid commit block checksum in transaction {seq}");
                    break;
                }
                for block in pending_revokes.drain(..) {
                    let r = revoked.entry(block).or_insert(seq);
                    if seq_at_or_after(seq, *r) {
                        *r = seq;
                    }
                }
                committed.push((seq, core::mem::take(&mut pending)));
                seq = seq.wrapping_add(1);
            }
            _ => break,
        }
        pos = f.next(pos);
    }
    // Replay: each committed block home, unless revoked by its own or a
    // later transaction.
    let mut replayed = 0usize;
    for (tid, blocks) in &committed {
        for &(home, at, escaped, tag_csum) in blocks {
            if revoked.get(&home).is_some_and(|&r| seq_at_or_after(r, *tid)) {
                continue;
            }
            let mut data = read(at)?;
            // (The checksum is of the block as logged, escaped.)
            if f.checksums() {
                let c = f.data_csum(*tid, &data);
                let ok = if f.v3 { c == tag_csum } else { c as u16 as u32 == tag_csum };
                if !ok {
                    kinfo!("JBD2: Invalid checksum recovering data block {home} in transaction {tid}");
                    continue;
                }
            }
            if escaped {
                put_be32(&mut data, 0, MAGIC);
            }
            disk.write_bytes(home * bs as u64, &data)?;
            replayed += 1;
        }
    }
    disk.dev.flush()?;
    kinfo!(
        "JBD2: recovery: {} transactions ({replayed} blocks) replayed, {} blocks revoked",
        committed.len(),
        revoked.len()
    );
    // Empty: the next transaction comes after all of these.
    put_be32(&mut jsb, JSB_START, 0);
    put_be32(&mut jsb, JSB_SEQUENCE, seq.wrapping_add(1));
    if f.v3 || f.v2 {
        let c = superblock_csum(&jsb);
        put_be32(&mut jsb, JSB_CHECKSUM, c);
    }
    disk.write_bytes(map[0] * bs as u64, &jsb)?;
    disk.dev.flush()
}

struct State {
    /// The next transaction's sequence number.
    sequence: u32,
    /// The journal superblock, as on the disk.
    jsb: Vec<u8>,
}

pub struct Journal {
    disk: Arc<Disk>,
    map: Vec<u64>,
    format: Format,
    state: Mutex<State>,
}

impl Journal {
    /// The (empty, recovered) journal of `fs`, ready for commits.
    pub fn open(fs: &Ext4Fs, sb: &[u8]) -> KResult<Journal> {
        let disk = fs.cache.disk().clone();
        let bs = fs.block_size;
        let map = journal_map(&disk, sb)?;
        let mut jsb = read_block(&disk, map[0], bs)?;
        let mut f = Format::parse(&jsb, bs)?;
        if f.maxlen as usize > map.len() || f.first == 0 || f.first >= f.maxlen || f.maxlen - f.first < 64 {
            kinfo!("JBD2: journal too small or bad");
            return Err(Errno::EUCLEAN);
        }
        if be32(&jsb, JSB_START) != 0 {
            kinfo!("JBD2: journal not empty");
            return Err(Errno::EUCLEAN);
        }
        // The features Linux gives the journal of such a file system.
        if be32(&jsb, 4) == SUPERBLOCK_V2 {
            let mut incompat = be32(&jsb, JSB_INCOMPAT);
            let old = (incompat, be32(&jsb, JSB_COMPAT), jsb[JSB_CHECKSUM_TYPE]);
            let mut compat = old.1;
            if fs.csum {
                incompat = (incompat | INCOMPAT_CSUM_V3) & !INCOMPAT_CSUM_V2;
                compat &= !COMPAT_CHECKSUM;
                jsb[JSB_CHECKSUM_TYPE] = CRC32C_CHKSUM;
            }
            if fs.has_incompat(super::INCOMPAT_64BIT) {
                incompat |= INCOMPAT_64BIT;
            }
            if (incompat, compat, jsb[JSB_CHECKSUM_TYPE]) != old {
                put_be32(&mut jsb, JSB_INCOMPAT, incompat);
                put_be32(&mut jsb, JSB_COMPAT, compat);
                f = Format::parse(&jsb, bs)?;
                if f.checksums() {
                    let c = superblock_csum(&jsb);
                    put_be32(&mut jsb, JSB_CHECKSUM, c);
                }
                disk.write_bytes(map[0] * bs as u64, &jsb)?;
                disk.dev.flush()?;
            }
        }
        let sequence = be32(&jsb, JSB_SEQUENCE);
        Ok(Journal { disk, map, format: f, state: Mutex::new(State { sequence, jsb }) })
    }

    /// Changed blocks to gather before committing (half the log: a
    /// transaction never outgrows it).
    pub fn limit(&self) -> usize {
        ((self.format.maxlen - self.format.first) / 2) as usize
    }

    fn write(&self, pos: u32, data: &[u8]) -> KResult<()> {
        self.disk.write_bytes(self.map[pos as usize] * self.format.block_size as u64, data)
    }

    fn write_superblock(&self, state: &mut State, start: u32, sequence: u32) -> KResult<()> {
        let jsb = &mut state.jsb;
        put_be32(jsb, JSB_START, start);
        put_be32(jsb, JSB_SEQUENCE, sequence);
        if self.format.checksums() {
            let c = superblock_csum(jsb);
            put_be32(jsb, JSB_CHECKSUM, c);
        }
        self.disk.write_bytes(self.map[0] * self.format.block_size as u64, jsb)
    }

    /// Log one transaction (`blocks` fit), at the start of the log.
    fn log(&self, state: &mut State, blocks: &[(u64, Vec<u8>)]) -> KResult<()> {
        let f = &self.format;
        let bs = f.block_size;
        let seq = state.sequence;
        let header = |b: &mut [u8], kind: u32| {
            put_be32(b, 0, MAGIC);
            put_be32(b, 4, kind);
            put_be32(b, 8, seq);
        };
        let mut pos = f.first;
        let mut i = 0;
        while i < blocks.len() {
            // A descriptor and the blocks it describes.
            let mut desc = vec![0u8; bs];
            header(&mut desc, DESCRIPTOR);
            let mut off = 12;
            let first_tag = i;
            let mut last_off = 0;
            while i < blocks.len() {
                let need = f.tag_bytes() + if i == first_tag { 16 } else { 0 };
                if off + need > bs - f.tail() {
                    break;
                }
                let (home, data) = &blocks[i];
                let escaped = be32(data, 0) == MAGIC;
                // (The tag's checksum is of the block as logged.)
                let logged_csum = if escaped {
                    let mut e = data.clone();
                    put_be32(&mut e, 0, 0);
                    f.data_csum(seq, &e)
                } else {
                    f.data_csum(seq, data)
                };
                let mut flags = if escaped { TAG_ESCAPE } else { 0 };
                if i != first_tag {
                    flags |= TAG_SAME_UUID;
                }
                if f.v3 {
                    put_be32(&mut desc, off, *home as u32);
                    put_be32(&mut desc, off + 4, flags);
                    put_be32(&mut desc, off + 8, (*home >> 32) as u32);
                    put_be32(&mut desc, off + 12, logged_csum);
                } else {
                    put_be32(&mut desc, off, *home as u32);
                    if f.v2 {
                        put_be16(&mut desc, off + 4, logged_csum as u16);
                    }
                    put_be16(&mut desc, off + 6, flags as u16);
                    if f.wide {
                        put_be32(&mut desc, off + 8, (*home >> 32) as u32);
                    }
                }
                last_off = off;
                off += f.tag_bytes();
                if i == first_tag {
                    desc[off..off + 16].copy_from_slice(&f.uuid);
                    off += 16;
                }
                i += 1;
            }
            // The last tag of the descriptor.
            if f.v3 {
                let flags = be32(&desc, last_off + 4) | TAG_LAST;
                put_be32(&mut desc, last_off + 4, flags);
            } else {
                let flags = u16::from_be_bytes([desc[last_off + 6], desc[last_off + 7]]) | TAG_LAST as u16;
                put_be16(&mut desc, last_off + 6, flags);
            }
            if f.checksums() {
                let c = f.block_csum(&desc);
                put_be32(&mut desc, bs - 4, c);
            }
            self.write(pos, &desc)?;
            pos = f.next(pos);
            for (_, data) in &blocks[first_tag..i] {
                if be32(data, 0) == MAGIC {
                    let mut escaped = data.clone();
                    put_be32(&mut escaped, 0, 0);
                    self.write(pos, &escaped)?;
                } else {
                    self.write(pos, data)?;
                }
                pos = f.next(pos);
            }
        }
        // Everything before the commit block is on the disk first.
        self.disk.dev.flush()?;
        let mut commit = vec![0u8; bs];
        header(&mut commit, COMMIT);
        let ns = crate::ext4::kernel::time::realtime_ns();
        commit[48..56].copy_from_slice(&(ns / 1_000_000_000).to_be_bytes());
        put_be32(&mut commit, 56, (ns % 1_000_000_000) as u32);
        if f.checksums() {
            let c = f.commit_csum(&commit);
            put_be32(&mut commit, 16, c);
        }
        self.write(pos, &commit)?;
        // The journal holds this transaction now.
        self.write_superblock(state, f.first, seq)?;
        self.disk.dev.flush()
    }

    /// How many blocks one transaction may log (the log less its
    /// descriptors and commit block).
    fn capacity(&self) -> usize {
        let f = &self.format;
        let space = (f.maxlen - f.first) as usize;
        let per_desc = ((f.block_size - 12 - f.tail() - 16) / f.tag_bytes()).max(1);
        space - 2 - space.div_ceil(per_desc)
    }

    /// Log the changed blocks of `fs` as one transaction and stop there
    /// (a crash before the checkpoint, for the self tests).
    pub fn log_only(&self, fs: &Ext4Fs) -> KResult<()> {
        let blocks = fs.cache.dirty_blocks();
        if blocks.is_empty() || blocks.len() > self.capacity() {
            return Err(Errno::EINVAL);
        }
        let mut state = self.state.lock();
        self.log(&mut state, &blocks)
    }

    /// Commit the changed metadata blocks of `fs` (its big lock held: no
    /// one changes them meanwhile).
    pub fn commit(&self, fs: &Ext4Fs) -> KResult<()> {
        let blocks = fs.cache.dirty_blocks();
        if blocks.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock();
        let cap = self.capacity();
        if blocks.len() > cap {
            kinfo!("JBD2: {} blocks to commit: more than one transaction", blocks.len());
        }
        let bs = self.format.block_size as u64;
        for chunk in blocks.chunks(cap) {
            self.log(&mut state, chunk)?;
            // The checkpoint: the blocks in place.
            for (home, data) in chunk {
                self.disk.write_bytes(home * bs, data)?;
            }
            self.disk.dev.flush()?;
            state.sequence = state.sequence.wrapping_add(1);
            let seq = state.sequence;
            self.write_superblock(&mut state, 0, seq)?;
        }
        self.disk.dev.flush()?;
        fs.cache.mark_clean(&blocks);
        Ok(())
    }
}
