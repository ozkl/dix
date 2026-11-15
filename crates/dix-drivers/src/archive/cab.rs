//! Microsoft Cabinet files (`.cab`: drivers, Windows updates, installers),
//! read-only.
//!
//! A header ("MSCF"; optional reserved areas and the names of the cabinets
//! before and after it in a set), then folders — each a run of data blocks
//! compressed together: stored, MSZIP (each block a deflate stream that may
//! refer back into the 32 KiB before it) or LZX; Quantum is not read — and
//! files, each a range of a folder's unpacked bytes, with a DOS time,
//! attributes and a name (with backslashes; UTF-8 when flagged). Files
//! that continue from or into another cabinet of a set are left out.

use super::{Data, Node, Raw, Tree, clean_path, from_dos_time, lzx};
use dix_core::{EntryKind, Error, MountLog, Result};

pub(super) const MAGIC: &[u8; 4] = b"MSCF";

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn damaged(what: impl std::fmt::Display) -> Error {
    Error::corrupted(format!("cabinet: {what}"))
}

#[derive(Debug, Clone)]
pub(super) struct Folder {
    /// Where its first data block is, how many there are.
    first: u64,
    blocks: u16,
    /// Compression: 0 none, 1 MSZIP, 2 Quantum, 3 LZX (window in bits 8–12).
    method: u16,
    /// Its unpacked size (the end of its last file).
    size: u64,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Cabinet {
    folders: Vec<Folder>,
    /// Reserved bytes in each data block's header.
    data_reserve: usize,
}

/// Whether `head` starts a cabinet.
pub(super) fn is_cab(head: &[u8]) -> bool {
    head.len() >= 36 && head.starts_with(MAGIC) && le32(head, 4) == 0 && head[25] == 1
}

pub(super) fn parse(raw: &mut Raw<'_>, log: &MountLog) -> Result<(Tree, Cabinet)> {
    let size = raw.size();
    let mut h = [0u8; 36];
    raw.read_at(0, &mut h)?;
    if !is_cab(&h) {
        return Err(damaged("no header"));
    }
    let files_at = le32(&h, 16) as u64;
    let (folders, files) = (le16(&h, 26) as usize, le16(&h, 28) as usize);
    let flags = le16(&h, 30);
    let mut at = 36u64;
    let (mut folder_reserve, mut data_reserve) = (0usize, 0usize);
    if flags & 4 != 0 {
        let mut r = [0u8; 4];
        raw.read_at(at, &mut r)?;
        let header_reserve = le16(&r, 0) as u64;
        folder_reserve = r[2] as usize;
        data_reserve = r[3] as usize;
        at += 4 + header_reserve;
    }
    // The names of the previous and next cabinets (and their disks).
    let mut skip_strings = |n: usize, at: &mut u64| -> Result<()> {
        for _ in 0..n {
            let mut b = [0u8; 256];
            let k = (size.saturating_sub(*at)).min(256) as usize;
            raw.read_at(*at, &mut b[..k])?;
            let end = b[..k].iter().position(|&c| c == 0).ok_or_else(|| damaged("a name is too long"))?;
            *at += end as u64 + 1;
        }
        Ok(())
    };
    if flags & 1 != 0 {
        skip_strings(2, &mut at)?;
    }
    if flags & 2 != 0 {
        skip_strings(2, &mut at)?;
    }
    if flags & 3 != 0 {
        log.info("This cabinet is one of a set: files continued in the others are left out.");
    }
    let mut cab = Cabinet { folders: Vec::new(), data_reserve };
    for _ in 0..folders {
        let mut f = [0u8; 8];
        raw.read_at(at, &mut f)?;
        cab.folders.push(Folder { first: le32(&f, 0) as u64, blocks: le16(&f, 4), method: le16(&f, 6), size: 0 });
        at += 8 + folder_reserve as u64;
    }
    let mut tree = Tree::new();
    let mut at = files_at;
    let mut left_out = 0;
    for _ in 0..files {
        let mut f = [0u8; 16];
        raw.read_at(at, &mut f)?;
        let (len, offset, folder) = (le32(&f, 0) as u64, le32(&f, 4) as u64, le16(&f, 8));
        let (date, time, attrs) = (le16(&f, 10), le16(&f, 12), le16(&f, 14));
        let mut name = vec![0u8; (size - at - 16).min(512) as usize];
        raw.read_at(at + 16, &mut name)?;
        let end = name.iter().position(|&c| c == 0).ok_or_else(|| damaged("a file name is too long"))?;
        name.truncate(end);
        at += 16 + end as u64 + 1;
        // Not UTF-8: a code page; Latin-1 is the usual one.
        let name: String = if attrs & 0x80 != 0 {
            String::from_utf8_lossy(&name).into_owned()
        } else {
            name.iter().map(|&c| c as char).collect()
        };
        if folder >= 0xFFFD {
            left_out += 1;
            continue;
        }
        let Some(fo) = cab.folders.get_mut(folder as usize) else {
            return Err(damaged(format!("{name}: in folder {folder}, which is not there")));
        };
        fo.size = fo.size.max(offset + len);
        let Some(path) = clean_path(&name.replace('\\', "/")).filter(|p| p != "/") else { continue };
        let mut n = Node::new(EntryKind::File);
        n.size = len;
        n.mtime = from_dos_time(date, time);
        n.mode = Some(if attrs & 1 != 0 { 0o444 } else { 0o644 });
        n.data = Data::Cab { folder: folder as usize, offset, len };
        tree.insert(&path, n);
    }
    if left_out > 0 {
        log.info(format!("{left_out} file(s) continue from or into another cabinet of the set: left out."));
    }
    Ok((tree, cab))
}

/// A folder's unpacked bytes.
pub(super) fn folder_bytes(raw: &mut Raw<'_>, cab: &Cabinet, index: usize) -> Result<Vec<u8>> {
    let f = cab.folders.get(index).ok_or_else(|| damaged("no such folder"))?;
    // The data blocks: (compressed bytes, unpacked size).
    let mut blocks = Vec::with_capacity(f.blocks as usize);
    let mut at = f.first;
    for _ in 0..f.blocks {
        let mut h = [0u8; 8];
        raw.read_at(at, &mut h)?;
        let (len, unpacked) = (le16(&h, 4) as usize, le16(&h, 6) as usize);
        at += 8 + cab.data_reserve as u64;
        let mut b = vec![0u8; len];
        raw.read_at(at, &mut b)?;
        at += len as u64;
        blocks.push((b, unpacked));
    }
    let total: usize = blocks.iter().map(|b| b.1).sum();
    match f.method & 0x0F {
        0 => Ok(blocks.into_iter().flat_map(|b| b.0).collect()),
        1 => mszip(&blocks, total),
        3 => {
            let input: Vec<u8> = blocks.iter().flat_map(|b| b.0.iter().copied()).collect();
            lzx::decompress(&input, ((f.method >> 8) & 0x1F) as u32, total)
        }
        2 => Err(Error::unsupported("Quantum-compressed cabinets")),
        m => Err(damaged(format!("unknown compression {m}"))),
    }
}

/// MSZIP: each block "CK" and a deflate stream, which may refer back into
/// the 32 KiB unpacked before it.
fn mszip(blocks: &[(Vec<u8>, usize)], total: usize) -> Result<Vec<u8>> {
    use miniz_oxide::inflate::TINFLStatus;
    use miniz_oxide::inflate::core::{DecompressorOxide, decompress, inflate_flags};
    const HISTORY: usize = 32768;
    let mut out = Vec::with_capacity(total);
    for (b, unpacked) in blocks {
        if !b.starts_with(b"CK") {
            return Err(damaged("an MSZIP block without its signature"));
        }
        let keep = out.len().min(HISTORY);
        let mut buf = out[out.len() - keep..].to_vec();
        buf.resize(keep + unpacked, 0);
        let mut state = Box::<DecompressorOxide>::default();
        let flags = inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        let (status, _, made) = decompress(&mut state, &b[2..], &mut buf, keep, flags);
        // (A block need not end its stream: the next one goes on.)
        if !matches!(status, TINFLStatus::Done | TINFLStatus::NeedsMoreInput) || made != *unpacked {
            return Err(damaged(format!("an MSZIP block does not unpack ({status:?})")));
        }
        out.extend_from_slice(&buf[keep..keep + made]);
    }
    Ok(out)
}
