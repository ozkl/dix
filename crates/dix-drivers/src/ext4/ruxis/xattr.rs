//! Extended attributes on the disk.
//!
//! An inode's attributes are in one block (`i_file_acl`): a 32-byte header
//! (magic, reference count, blocks = 1, hash, checksum), entries from
//! offset 32 (16 bytes and the name, 4-aligned; four zero bytes end them)
//! sorted by name index, name length and name, values packed from the end
//! of the block down (4-aligned). Inodes with the same attributes may share
//! a block (its reference count): a shared one is never changed, a copy is
//! made. Large inodes may hold attributes in their extra space too (ext4's
//! in-inode attributes, which mke2fs -d and Linux write): those are read,
//! and moved to the block when the attributes change (Linux reads both).

use alloc::string::String;
use alloc::vec::Vec;

use super::{le16, le32, put16, put32};
use crate::ext4::kernel::errno::{Errno, KResult};

pub const XATTR_MAGIC: u32 = 0xEA02_0000;

/// Name indexes and their prefixes (`system.posix_acl_*`: whole names).
const XATTR_PREFIXES: [(u8, &str); 6] = [
    (2, "system.posix_acl_access"),
    (3, "system.posix_acl_default"),
    (1, "user."),
    (4, "trusted."),
    (6, "security."),
    (7, "system."),
];

fn xattr_name(index: u8, suffix: &[u8]) -> Option<String> {
    let suffix = core::str::from_utf8(suffix).ok()?;
    if index == 0 {
        return Some(suffix.into());
    }
    let (_, prefix) = XATTR_PREFIXES.iter().find(|(i, _)| *i == index)?;
    Some(alloc::format!("{prefix}{suffix}"))
}

/// (name index, the rest) of a full name.
pub fn split(name: &str) -> (u8, &str) {
    for &(index, prefix) in &XATTR_PREFIXES {
        if let Some(rest) = name.strip_prefix(prefix) {
            return (index, rest);
        }
    }
    (0, name)
}

/// The entries from offset `start` of `b` (values at `value_base` plus
/// their offset).
pub fn parse(b: &[u8], start: usize, value_base: usize, out: &mut Vec<(String, Vec<u8>)>) -> KResult<()> {
    let mut o = start;
    loop {
        if o + 4 > b.len() {
            return Err(Errno::EUCLEAN);
        }
        if le32(b, o) == 0 {
            return Ok(());
        }
        let name_len = b[o] as usize;
        if o + 16 + name_len > b.len() {
            return Err(Errno::EUCLEAN);
        }
        let (index, value_offs, inum, size) =
            (b[o + 1], le16(b, o + 2) as usize, le32(b, o + 4), le32(b, o + 8) as usize);
        let v0 = value_base + value_offs;
        // (Values in their own inodes, `ea_inode`, are not supported.)
        if inum != 0 || v0 + size > b.len() {
            return Err(Errno::EUCLEAN);
        }
        if let Some(name) = xattr_name(index, &b[o + 16..o + 16 + name_len]) {
            out.push((name, b[v0..v0 + size].to_vec()));
        }
        o += (16 + name_len).next_multiple_of(4);
    }
}

/// An entry's hash: its name's characters (signed, as Linux's `char`),
/// then its value's 32-bit words.
fn entry_hash(name: &[u8], value: &[u8]) -> u32 {
    let mut h = 0u32;
    for &c in name {
        h = (h << 5) ^ (h >> 27) ^ (c as i8 as i32 as u32);
    }
    for w in value.chunks(4) {
        let mut word = [0u8; 4];
        word[..w.len()].copy_from_slice(w);
        h = (h << 16) ^ (h >> 16) ^ u32::from_le_bytes(word);
    }
    h
}

/// The attribute block of `attrs` ((index, name, value), sorted), or
/// `None` if they don't fit in a block. (Its checksum, which depends on
/// where it goes, is set by [`set_block_csum`].)
pub fn build_block(attrs: &[(u8, Vec<u8>, Vec<u8>)], block_size: usize) -> Option<Vec<u8>> {
    let mut b = alloc::vec![0u8; block_size];
    put32(&mut b, 0, XATTR_MAGIC);
    put32(&mut b, 4, 1);
    put32(&mut b, 8, 1);
    let (mut o, mut values) = (32usize, block_size);
    let mut block_hash = 0u32;
    for (index, name, value) in attrs {
        let len = (16 + name.len()).next_multiple_of(4);
        let vlen = value.len().next_multiple_of(4);
        if name.len() > 255 || vlen > values || o + len + 4 > values - vlen {
            return None;
        }
        values -= vlen;
        b[values..values + value.len()].copy_from_slice(value);
        b[o] = name.len() as u8;
        b[o + 1] = *index;
        put16(&mut b, o + 2, if value.is_empty() { 0 } else { values as u16 });
        put32(&mut b, o + 8, value.len() as u32);
        let hash = entry_hash(name, value);
        put32(&mut b, o + 12, hash);
        b[o + 16..o + 16 + name.len()].copy_from_slice(name);
        block_hash = (block_hash << 16) ^ (block_hash >> 16) ^ hash;
        o += len;
    }
    put32(&mut b, 12, block_hash);
    Some(b)
}

/// The checksum of attribute block `b` at `block` (`metadata_csum`): the
/// block number, then the block with the checksum field as zero.
fn block_csum(seed: u32, block: u64, b: &[u8]) -> u32 {
    let c = super::csum::crc32c(seed, &block.to_le_bytes());
    let c = super::csum::crc32c(c, &b[..16]);
    let c = super::csum::crc32c(c, &[0; 4]);
    super::csum::crc32c(c, &b[20..])
}

pub fn set_block_csum(seed: u32, block: u64, b: &mut [u8]) {
    let c = block_csum(seed, block, b);
    put32(b, 16, c);
}

pub fn block_csum_ok(seed: u32, block: u64, b: &[u8]) -> bool {
    block_csum(seed, block, b) == le32(b, 16)
}
