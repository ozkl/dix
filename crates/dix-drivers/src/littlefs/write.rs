//! Writing a littlefs image anew from a tree, as a freshly compacted file
//! system: every folder's entries sorted by name in metadata pairs filled
//! to half a block (as littlefs compacts them, leaving room to append),
//! pairs of one folder joined by hard tails and every pair on one list of
//! soft tails from the root; small files inline, others as CTZ skip lists.
//! Each pair is one commit in its first block, padded to 512 bytes (so the
//! device's own later commits stay aligned to its program size); the
//! second block, like every unused one, is left erased (0xff).

use super::{TYPE_CTZSTRUCT, TYPE_DIR, TYPE_DIRSTRUCT, TYPE_INLINESTRUCT, TYPE_REG, TYPE_SUPERBLOCK, crc32};
use dix_core::{Error, ErrorKind, Result};

/// Commits end on this boundary.
const ALIGN: usize = 512;
/// Files up to this size are always inline (an inline file must fit the
/// reading device's cache: MicroPython's is 128 bytes at least).
pub(crate) const INLINE_MIN: usize = 64;
/// At most this many entries per pair (littlefs splits at 0xff).
const PAIR_ENTRIES: usize = 0xf0;

/// A member of the tree (entry 0: the root, a folder).
pub(crate) struct WNode {
    pub name: Vec<u8>,
    pub dir: bool,
    pub data: Vec<u8>,
    /// User attributes (type3, data), e.g. MicroPython's time (0x301).
    pub attrs: Vec<(u16, Vec<u8>)>,
    pub children: Vec<usize>,
}

/// The superblock's limits.
pub(crate) struct Limits {
    pub name_max: u32,
    pub file_max: u32,
    pub attr_max: u32,
    /// Files up to this size are stored inline: the largest the device
    /// itself stored inline (its cache holds them), at least INLINE_MIN.
    pub inline_max: usize,
}

/// One tag with its data (id filled in later).
struct Entry {
    tags: Vec<(u16, Vec<u8>)>,
}

/// Writes the image of `nodes`: `block_count` blocks of `block_size`.
pub(crate) fn build(nodes: &[WNode], block_size: u32, block_count: u32, limits: &Limits) -> Result<Vec<u8>> {
    let bs = block_size as usize;
    let mut img = vec![0xffu8; bs * block_count as usize];
    let mut next = 2u32; // blocks 0 and 1: the root pair
    let mut alloc = |n: u32| -> Result<u32> {
        let b = next;
        next += n;
        if next > block_count {
            return Err(Error::new(ErrorKind::NoSpace, "the files do not fit in the littlefs volume"));
        }
        Ok(b)
    };
    // File data first: each file's struct.
    let mut structs: Vec<Option<(u16, Vec<u8>)>> = Vec::with_capacity(nodes.len());
    for n in nodes {
        if n.dir {
            structs.push(None);
            continue;
        }
        if n.data.len() > limits.file_max as usize {
            return Err(Error::invalid_input("a file is larger than this littlefs allows"));
        }
        if n.data.len() <= limits.inline_max.max(INLINE_MIN).min(bs / 8) {
            structs.push(Some((TYPE_INLINESTRUCT, n.data.clone())));
            continue;
        }
        let head = write_ctz(&mut img, bs, &n.data, &mut alloc)?;
        let mut s = head.to_le_bytes().to_vec();
        s.extend((n.data.len() as u32).to_le_bytes());
        structs.push(Some((TYPE_CTZSTRUCT, s)));
    }
    // Folders: their entries, split into pairs.
    let dirs: Vec<usize> = {
        // Depth first from the root: the order of the soft-tail list.
        let mut order = Vec::new();
        let mut stack = vec![0usize];
        while let Some(d) = stack.pop() {
            order.push(d);
            let mut kids: Vec<usize> = nodes[d].children.iter().copied().filter(|&c| nodes[c].dir).collect();
            kids.sort_by(|a, b| nodes[*b].name.cmp(&nodes[*a].name));
            stack.extend(kids);
        }
        order
    };
    // Each folder's pairs: entries grouped to half a block.
    let budget = (bs / 2).min(bs - 64).max(64);
    let mut groups: Vec<Vec<Vec<Entry>>> = Vec::new();
    for &d in &dirs {
        let mut kids = nodes[d].children.clone();
        kids.sort_by(|a, b| compare(&nodes[*a].name, &nodes[*b].name));
        let mut entries = Vec::new();
        if d == 0 {
            let mut sb = Vec::new();
            for v in [0x0002_0000u32, block_size, block_count, limits.name_max, limits.file_max, limits.attr_max] {
                sb.extend(v.to_le_bytes());
            }
            entries.push(Entry { tags: vec![(TYPE_SUPERBLOCK, b"littlefs".to_vec()), (TYPE_INLINESTRUCT, sb)] });
        }
        for c in kids {
            let n = &nodes[c];
            if n.name.len() > limits.name_max as usize || n.name.is_empty() {
                return Err(Error::invalid_input(format!(
                    "{}: a name of {} bytes (littlefs allows 1 to {})",
                    String::from_utf8_lossy(&n.name),
                    n.name.len(),
                    limits.name_max
                )));
            }
            let mut tags = vec![(if n.dir { TYPE_DIR } else { TYPE_REG }, n.name.clone())];
            // Folders' structs are filled in once their pairs are placed.
            if let Some(s) = &structs[c] {
                tags.push(s.clone());
            } else {
                tags.push((TYPE_DIRSTRUCT, (c as u64).to_le_bytes().to_vec()));
            }
            for (t, a) in &n.attrs {
                if a.len() <= limits.attr_max as usize {
                    tags.push((*t, a.clone()));
                }
            }
            entries.push(Entry { tags });
        }
        let mut pairs: Vec<Vec<Entry>> = vec![Vec::new()];
        let mut used = 0;
        for e in entries {
            let size: usize = e.tags.iter().map(|(_, d)| 4 + d.len()).sum();
            let cur = pairs.last_mut().unwrap();
            if !cur.is_empty() && (used + size > budget || cur.len() >= PAIR_ENTRIES) {
                pairs.push(Vec::new());
                used = 0;
            }
            if size + 64 > bs {
                return Err(Error::invalid_input("an entry is too large for a littlefs metadata block"));
            }
            used += size;
            pairs.last_mut().unwrap().push(e);
        }
        groups.push(pairs);
    }
    // Place the pairs: the root's first at blocks 0/1.
    let mut placed: Vec<Vec<u32>> = Vec::new();
    for (k, g) in groups.iter().enumerate() {
        let mut v = Vec::new();
        for i in 0..g.len() {
            v.push(if k == 0 && i == 0 { 0 } else { alloc(2)? });
        }
        placed.push(v);
    }
    let first_pair: std::collections::HashMap<usize, u32> = dirs.iter().zip(&placed).map(|(&d, p)| (d, p[0])).collect();
    // Write each pair's commit.
    for (k, g) in groups.into_iter().enumerate() {
        let n_pairs = g.len();
        for (i, entries) in g.into_iter().enumerate() {
            let block = placed[k][i];
            let tail = if i + 1 < n_pairs {
                Some((0x601u16, placed[k][i + 1]))
            } else if k + 1 < placed.len() {
                Some((0x600u16, placed[k + 1][0]))
            } else {
                None
            };
            let mut tags: Vec<(u16, u16, Vec<u8>)> = Vec::new();
            for (id, e) in entries.into_iter().enumerate() {
                for (t, d) in e.tags {
                    let d = if t == TYPE_DIRSTRUCT {
                        let child = u64::from_le_bytes(d[..8].try_into().unwrap()) as usize;
                        let b = first_pair[&child];
                        let mut p = b.to_le_bytes().to_vec();
                        p.extend((b + 1).to_le_bytes());
                        p
                    } else {
                        d
                    };
                    tags.push((t, id as u16, d));
                }
            }
            if let Some((t, b)) = tail {
                let mut p = b.to_le_bytes().to_vec();
                p.extend((b + 1).to_le_bytes());
                tags.push((t, 0x3ff, p));
            }
            let commit = commit(&tags, bs)?;
            let at = block as usize * bs;
            img[at..at + commit.len()].copy_from_slice(&commit);
        }
    }
    Ok(img)
}

/// littlefs's name order: bytes, then the shorter first.
fn compare(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let n = a.len().min(b.len());
    a[..n].cmp(&b[..n]).then(a.len().cmp(&b.len()))
}

/// A metadata block's contents: revision 1 and one commit of `tags`
/// (type3, id, data), closed by a CRC and padded to ALIGN.
fn commit(tags: &[(u16, u16, Vec<u8>)], bs: usize) -> Result<Vec<u8>> {
    let mut out = 1u32.to_le_bytes().to_vec();
    let mut ptag = 0xffff_ffffu32;
    let mut put = |out: &mut Vec<u8>, tag: u32| {
        out.extend((tag ^ ptag).to_be_bytes());
        ptag = tag;
    };
    let mk = |t: u16, id: u16, len: usize| ((t as u32) << 20) | ((id as u32) << 10) | len as u32;
    for (t, id, d) in tags {
        if d.len() >= 0x3ff {
            return Err(Error::invalid_input("a littlefs tag of more than 1022 bytes"));
        }
        put(&mut out, mk(*t, *id, d.len()));
        out.extend(d);
    }
    // The CRC tag: its data is the CRC and padding to the boundary.
    let end = (out.len() + 8).next_multiple_of(ALIGN.min(bs));
    let len = end - out.len() - 4;
    if end > bs || len >= 0x3ff {
        return Err(Error::invalid_input("a littlefs metadata block overflows"));
    }
    // Chunk bit 0: the erased word after the commit (0xff…) reads as invalid.
    put(&mut out, mk(0x500, 0x3ff, len));
    let crc = crc32(0xffff_ffff, &out);
    out.extend(crc.to_le_bytes());
    out.resize(end, 0xff);
    Ok(out)
}

/// Writes `data` as a CTZ skip list; returns the last block (the head).
fn write_ctz(img: &mut [u8], bs: usize, data: &[u8], alloc: &mut dyn FnMut(u32) -> Result<u32>) -> Result<u32> {
    let mut blocks: Vec<u32> = Vec::new();
    let mut pos = 0;
    let mut i = 0u32;
    while pos < data.len() {
        let block = alloc(1)?;
        let at = block as usize * bs;
        let mut off = 0;
        if i > 0 {
            // ctz(i)+1 pointers: to blocks i-1, i-2, i-4, …
            for k in 0..=i.trailing_zeros() {
                let to = blocks[(i - (1 << k)) as usize];
                img[at + off..at + off + 4].copy_from_slice(&to.to_le_bytes());
                off += 4;
            }
        }
        let n = (bs - off).min(data.len() - pos);
        img[at + off..at + off + n].copy_from_slice(&data[pos..pos + n]);
        pos += n;
        blocks.push(block);
        i += 1;
    }
    Ok(*blocks.last().unwrap())
}
