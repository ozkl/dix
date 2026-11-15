//! The B-trees of HFS and HFS+ (catalog, extents overflow, attributes).
//!
//! A B-tree file is cut into nodes of one size (512 bytes to 32 KiB).
//! Node 0 is the header node: the tree's depth, root, first leaf, node size
//! and key layout. Every node starts with a 14-byte descriptor (next and
//! previous node, kind, height, record count); its records' offsets are
//! 16-bit numbers at the end of the node, from the last byte backwards.
//! Index nodes hold the first key of each child and its node number; leaf
//! nodes hold the records, in key order, chained left to right.

use super::be16;
use super::be32;
use dix_core::{Error, Result};

pub const LEAF: i8 = -1;
pub const INDEX: i8 = 0;

/// What the header node says.
#[derive(Clone, Debug)]
pub struct Header {
    #[allow(dead_code)]
    pub depth: u16,
    pub root: u32,
    pub node_size: u32,
    pub max_key: u16,
    pub total_nodes: u32,
    /// Keys start with a 16-bit length (HFS+); else an 8-bit one (HFS).
    pub big_keys: bool,
    /// Index keys are as long as they are, not padded to `max_key`.
    pub variable_index_keys: bool,
    /// 0xBC: names compared as binary (HFSX case-sensitive); 0xCF: case folded.
    pub compare: u8,
}

impl Header {
    /// Parses the header record of node 0 (`node` holds at least its first
    /// 512 bytes). `hfs`: an HFS tree (8-bit key lengths).
    pub fn parse(node: &[u8], hfs: bool) -> Result<Self> {
        if node.len() < 14 + 106 || node[8] as i8 != 1 {
            return Err(Error::corrupted("a B-tree without its header node"));
        }
        let h = &node[14..];
        let node_size = be16(h, 18) as u32;
        if !node_size.is_power_of_two() || !(512..=32768).contains(&node_size) {
            return Err(Error::corrupted(format!("a B-tree node size of {node_size}")));
        }
        let attrs = be32(h, 38);
        Ok(Self {
            depth: be16(h, 0),
            root: be32(h, 2),
            node_size,
            max_key: be16(h, 20),
            total_nodes: be32(h, 22),
            big_keys: !hfs,
            variable_index_keys: !hfs && attrs & 4 != 0,
            compare: h[37],
        })
    }
}

/// One node, checked enough to walk safely.
pub struct Node {
    pub data: Vec<u8>,
    pub kind: i8,
    pub next: u32,
    count: usize,
}

impl Node {
    pub fn parse(data: Vec<u8>) -> Result<Self> {
        let n = data.len();
        let count = be16(&data, 10) as usize;
        if 14 + 2 * (count + 1) > n {
            return Err(Error::corrupted("a B-tree node with too many records"));
        }
        Ok(Self { kind: data[8] as i8, next: be32(&data, 0), count, data })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    /// Record `i`, as the bytes up to the next record.
    pub fn record(&self, i: usize) -> Result<&[u8]> {
        let n = self.data.len();
        let at = |k: usize| be16(&self.data, n - 2 * (k + 1)) as usize;
        let (start, end) = (at(i), at(i + 1));
        if start < 14 || start > end || end > n - 2 * (self.count + 1) {
            return Err(Error::corrupted("a B-tree record out of place"));
        }
        Ok(&self.data[start..end])
    }
}

/// A key and what follows it in a record.
pub fn split_key<'a>(h: &Header, rec: &'a [u8], index: bool) -> Result<(&'a [u8], &'a [u8])> {
    let (len, skip) = if h.big_keys { (be16(rec, 0) as usize, 2) } else { (*rec.first().unwrap_or(&0) as usize, 1) };
    let data_at = if h.big_keys {
        // HFS+ index keys are padded to the maximum unless variable.
        2 + if index && !h.variable_index_keys { h.max_key as usize } else { len }
    } else {
        // HFS: index keys are always the maximum length; every key is
        // padded to an even size.
        (1 + if index { (h.max_key as usize).max(len) } else { len } + 1) & !1
    };
    if rec.len() < skip + len || rec.len() < data_at {
        return Err(Error::corrupted("a B-tree key longer than its record"));
    }
    Ok((&rec[skip..skip + len], &rec[data_at..]))
}

/// Walks a tree from the root to the leaf where keys `<= target` end:
/// `le(key)` says whether a key is at most the target. Returns the leaf
/// node's number.
pub fn descend(
    h: &Header,
    mut read: impl FnMut(u32) -> Result<std::sync::Arc<Node>>,
    le: impl Fn(&[u8]) -> bool,
) -> Result<u32> {
    let mut at = h.root;
    for _ in 0..16 {
        let node = read(at)?;
        match node.kind {
            LEAF => return Ok(at),
            INDEX => {
                let mut child = None;
                for i in 0..node.len() {
                    let (key, rest) = split_key(h, node.record(i)?, true)?;
                    if i > 0 && !le(key) {
                        break;
                    }
                    if rest.len() < 4 {
                        return Err(Error::corrupted("an index record without its child"));
                    }
                    child = Some(be32(rest, 0));
                }
                at = child.ok_or_else(|| Error::corrupted("an empty index node"))?;
            }
            k => return Err(Error::corrupted(format!("a B-tree node of kind {k} in the tree"))),
        }
    }
    Err(Error::corrupted("the B-tree is too deep (or loops)"))
}
