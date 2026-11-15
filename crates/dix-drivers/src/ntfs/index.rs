//! Directory indexes (`$I30`): a B+ tree of `$FILE_NAME` keys.
//!
//! The root node lives in the directory's `$INDEX_ROOT` attribute; larger
//! directories have further nodes in "INDX" blocks of `$INDEX_ALLOCATION`.
//! A node is a sorted run of entries ending in an empty "last" entry; an
//! entry may point at a child node holding the names that sort before it.
//! Names sort by their upper-case form ($UpCase).

use super::record::{le16, le32, le64, record_of};
use dix_core::{Error, Result};
use std::cmp::Ordering;

/// A `$FILE_NAME` value (attribute 0x30, and the key of index entries).
#[derive(Debug, Clone)]
pub struct FileName {
    pub parent: u64,
    /// 0 POSIX, 1 Win32, 2 DOS (8.3 alias), 3 Win32 and DOS.
    pub namespace: u8,
    pub name: Vec<u16>,
}

impl FileName {
    pub fn parse(b: &[u8]) -> Option<FileName> {
        if b.len() < 66 {
            return None;
        }
        let n = b[64] as usize;
        if b.len() < 66 + 2 * n {
            return None;
        }
        Some(FileName { parent: le64(b, 0), namespace: b[65], name: (0..n).map(|k| le16(b, 66 + 2 * k)).collect() })
    }

    pub fn name(&self) -> String {
        String::from_utf16_lossy(&self.name)
    }
}

/// One index entry: the file it names and the name.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Full file reference (record number and sequence number).
    pub file_ref: u64,
    pub key: FileName,
}

impl Entry {
    pub fn record(&self) -> u64 {
        record_of(self.file_ref)
    }

    pub fn seq(&self) -> u16 {
        (self.file_ref >> 48) as u16
    }
}

/// A parsed node: its entries in order, each with the child to its left,
/// and the child after the last entry.
#[derive(Debug, Default)]
pub struct Node {
    pub entries: Vec<(Entry, Option<u64>)>,
    pub last_child: Option<u64>,
}

/// Parses the entries of a node whose header (entries offset, used size)
/// starts at `hdr` in `b`.
pub fn parse_node(b: &[u8], hdr: usize) -> Result<Node> {
    let bad = || Error::corrupted("a damaged directory index");
    if hdr + 16 > b.len() {
        return Err(bad());
    }
    let start = hdr + le32(b, hdr) as usize;
    let end = (hdr + le32(b, hdr + 4) as usize).min(b.len());
    let mut node = Node::default();
    let mut at = start;
    loop {
        if at + 16 > end {
            return Err(bad());
        }
        let len = le16(b, at + 8) as usize;
        let key_len = le16(b, at + 10) as usize;
        let flags = le32(b, at + 12);
        if len < 16 || at + len > end {
            return Err(bad());
        }
        let child = if flags & 1 != 0 {
            if len < 24 {
                return Err(bad());
            }
            Some(le64(b, at + len - 8))
        } else {
            None
        };
        if flags & 2 != 0 {
            node.last_child = child;
            return Ok(node);
        }
        let key = b.get(at + 16..at + 16 + key_len).and_then(FileName::parse).ok_or_else(bad)?;
        node.entries.push((Entry { file_ref: le64(b, at), key }, child));
        at += len;
    }
}

/// Upper-casing for name comparisons, from `$UpCase` (65536 UTF-16 units).
pub struct UpCase(Vec<u16>);

impl UpCase {
    pub fn from_bytes(b: &[u8]) -> UpCase {
        let mut t: Vec<u16> = (0..=u16::MAX).collect();
        for (i, c) in b.chunks_exact(2).take(65536).enumerate() {
            t[i] = u16::from_le_bytes([c[0], c[1]]);
        }
        UpCase(t)
    }

    /// For a volume whose table cannot be read: ASCII only.
    pub fn ascii() -> UpCase {
        UpCase((0..=u16::MAX).map(|c| if (b'a' as u16..=b'z' as u16).contains(&c) { c - 32 } else { c }).collect())
    }

    pub fn cmp(&self, a: &[u16], b: &[u16]) -> Ordering {
        let up = |c: &u16| self.0[*c as usize];
        a.iter().map(up).cmp(b.iter().map(up))
    }
}
