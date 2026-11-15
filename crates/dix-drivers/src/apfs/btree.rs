//! APFS B-tree nodes.
//!
//! A node is one block: the object header, flags, level and key count,
//! then a table of contents locating each key (in the key area after the
//! table) and each value (counted back from the end of the block, or from
//! the tree info that ends a root node). Trees with fixed-size keys and
//! values (object maps, the sealed volumes' extent trees) keep two 16-bit
//! offsets per entry; the others an offset and a length for both.

use super::{le16, le32};
use dix_core::{Error, Result};

pub const ROOT: u16 = 1;
pub const LEAF: u16 = 2;
pub const FIXED: u16 = 4;

fn bad() -> Error {
    Error::corrupted("an APFS B-tree node is damaged")
}

pub struct Node {
    pub b: Vec<u8>,
    pub flags: u16,
    pub count: usize,
    toc: usize,
    keys: usize,
    vals_end: usize,
    /// Key and value sizes of a fixed-size tree (values in index nodes are
    /// child object IDs, 8 bytes).
    fixed: Option<(usize, usize)>,
}

impl Node {
    /// `fixed`: the tree's key and leaf value sizes, if fixed.
    pub fn parse(b: Vec<u8>, fixed: Option<(usize, usize)>) -> Result<Self> {
        if b.len() < 56 + 40 {
            return Err(bad());
        }
        let flags = le16(&b, 32);
        let count = le32(&b, 36) as usize;
        let (toc_off, toc_len) = (le16(&b, 40) as usize, le16(&b, 42) as usize);
        let toc = 56 + toc_off;
        let keys = toc + toc_len;
        let vals_end = if flags & ROOT != 0 { b.len() - 40 } else { b.len() };
        let per = if flags & FIXED != 0 { 4 } else { 8 };
        if keys > vals_end || toc + count * per > keys {
            return Err(bad());
        }
        let fixed = if flags & FIXED != 0 { Some(fixed.ok_or_else(bad)?) } else { None };
        Ok(Self { b, flags, count, toc, keys, vals_end, fixed })
    }

    /// The fixed key and leaf value sizes, for the node's children.
    pub fn fixed_sizes(&self) -> Option<(usize, usize)> {
        self.fixed
    }

    pub fn is_leaf(&self) -> bool {
        self.flags & LEAF != 0
    }

    /// Entry `i`: its key and its value.
    pub fn entry(&self, i: usize) -> Result<(&[u8], &[u8])> {
        let b = &self.b;
        let (k, klen, v, vlen) = match self.fixed {
            Some((ks, vs)) => {
                let at = self.toc + i * 4;
                let vs = if self.is_leaf() { vs } else { 8 };
                (le16(b, at) as usize, ks, le16(b, at + 2) as usize, vs)
            }
            None => {
                let at = self.toc + i * 8;
                (le16(b, at) as usize, le16(b, at + 2) as usize, le16(b, at + 4) as usize, le16(b, at + 6) as usize)
            }
        };
        let key = b.get(self.keys + k..self.keys + k + klen).ok_or_else(bad)?;
        if v == 0xFFFF || v > self.vals_end || v < vlen {
            return Err(bad());
        }
        let start = self.vals_end - v;
        Ok((key, b.get(start..start + vlen).ok_or_else(bad)?))
    }

    /// The tree's fixed key and value sizes, from a root node's tree info.
    pub fn info_sizes(b: &[u8]) -> (usize, usize) {
        let info = &b[b.len() - 40..];
        (le32(info, 8) as usize, le32(info, 12) as usize)
    }
}
