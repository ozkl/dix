//! Deleted files on FAT12/16/32.
//!
//! Deleting a file marks its directory entries deleted (first byte 0xE5,
//! long-name parts too) and frees its clusters in the FAT; the entries keep
//! the size, the times and the first cluster, the data stays where it was
//! until reused. A deleted file is taken to be stored in one piece from
//! its first cluster (the FAT chain is gone); its clusters still free in
//! the FAT say how likely it is to be intact. The long name's checksum
//! gives back the short name's lost first character. Deleted folders are
//! followed the same way (when their first cluster still starts with the
//! "." and ".." entries).

use super::bpb::{FatType, Layout};
use super::dir::{ATTR_DIR, ATTR_LFN, ATTR_VOLUME, checksum, le16, le32, short_display};
use super::table::Fat;
use crate::recover::{Chance, Found, Recovered, Runs};
use dix_core::{BlockDevice, Result};

/// Where a folder's entries are.
#[derive(Clone)]
enum Place {
    /// FAT12/16's fixed root.
    FixedRoot,
    /// A live folder: its cluster chain.
    Chain(u32),
    /// A deleted folder: clusters from here on, one piece.
    Deleted(u32),
}

/// Longest deleted folder followed (clusters).
const MAX_DELETED_DIR: u32 = 256;

pub fn scan(mut dev: Box<dyn BlockDevice>) -> Result<Recovered> {
    let mut b = [0u8; 512];
    dev.read_at(0, &mut b)?;
    let l = Layout::parse(&b)?;
    let mut fat = Fat::new(&l);
    let cs = l.cluster_size as u64;
    let mut found = Vec::new();
    let mut files: Vec<Vec<(Option<u64>, u64)>> = Vec::new();
    let root = if l.fat_type == FatType::Fat32 { Place::Chain(l.root_cluster) } else { Place::FixedRoot };
    let mut stack: Vec<(Vec<String>, Place, bool)> = vec![(Vec::new(), root, false)];
    let mut seen = std::collections::HashSet::new();
    while let Some((path, place, in_deleted)) = stack.pop() {
        let slots = match slots(dev.as_mut(), &l, &mut fat, &place) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Long-name parts before the entry they belong to.
        let mut lfn: Vec<[u8; 32]> = Vec::new();
        for e in slots {
            if e[0] == 0x00 {
                break;
            }
            let deleted = e[0] == 0xE5;
            if e[11] & 0x3F == ATTR_LFN {
                // Each entry checks that the parts are its own (checksum).
                lfn.push(e);
                continue;
            }
            let parts = std::mem::take(&mut lfn);
            if e[11] & ATTR_VOLUME != 0 || &e[..2] == b". " || &e[..3] == b".. " {
                continue;
            }
            let dir = e[11] & ATTR_DIR != 0;
            let first =
                le16(&e, 26) as u32 | if l.fat_type == FatType::Fat32 { (le16(&e, 20) as u32) << 16 } else { 0 };
            if !deleted && !in_deleted {
                // A live folder: looked through for what was deleted in it.
                if dir && first >= 2 && seen.insert(first) {
                    let mut p = path.clone();
                    p.push(live_name(&parts, &e).unwrap_or_else(|| short_display(&e)));
                    stack.push((p, Place::Chain(first), false));
                }
                continue;
            }
            let name = deleted_name(&parts, &e);
            let mut p = path.clone();
            p.push(name);
            let modified = dix_core::time::from_dos(le16(&e, 24), le16(&e, 22));
            let created = dix_core::time::from_dos(le16(&e, 16), le16(&e, 14));
            if dir {
                // Its contents, if its first cluster still holds them.
                let followed = first >= 2
                    && first <= l.max_cluster()
                    && seen.insert(first)
                    && starts_with_dots(dev.as_mut(), &l, first);
                found.push(Found {
                    path: p.clone(),
                    dir: true,
                    size: 0,
                    modified,
                    created,
                    chance: Chance::Folder,
                    id: 0,
                });
                if followed {
                    stack.push((p, Place::Deleted(first), true));
                }
                continue;
            }
            let size = le32(&e, 28) as u64;
            let (runs, chance) = if size == 0 {
                (Vec::new(), Chance::Intact)
            } else if first < 2 || first > l.max_cluster() {
                (vec![(None, size)], Chance::Lost)
            } else {
                // One piece from the first cluster, as far as the volume goes.
                let n = size.div_ceil(cs).min((l.max_cluster() - first + 1) as u64) as u32;
                let (mut free, mut used) = (0u64, 0u64);
                for c in first..first + n {
                    if fat.get(dev.as_mut(), c)? == 0 {
                        free += 1;
                    } else {
                        used += 1;
                    }
                }
                let avail = (n as u64 * cs).min(size);
                let mut runs = vec![(Some(l.cluster_offset(first)), avail)];
                if avail < size {
                    runs.push((None, size - avail));
                }
                (runs, Chance::of(free, used))
            };
            found.push(Found { path: p, dir: false, size, modified, created, chance, id: files.len() });
            files.push(runs);
        }
    }
    let name = l.fat_type.name();
    Ok(Recovered::new(name, found, Box::new(Runs { dev, files })))
}

/// The 32-byte entries of a folder.
fn slots(dev: &mut dyn BlockDevice, l: &Layout, fat: &mut Fat, place: &Place) -> Result<Vec<[u8; 32]>> {
    let bytes = match place {
        Place::FixedRoot => {
            let mut v = vec![0u8; l.root_bytes as usize];
            dev.read_at(l.root_offset, &mut v)?;
            v
        }
        Place::Chain(first) => {
            let mut v = Vec::new();
            for c in fat.chain(dev, *first)? {
                let mut b = vec![0u8; l.cluster_size as usize];
                dev.read_at(l.cluster_offset(c), &mut b)?;
                v.extend(b);
            }
            v
        }
        Place::Deleted(first) => {
            // Cluster after cluster, until the end-of-folder entry.
            let mut v = Vec::new();
            for c in *first..(*first + MAX_DELETED_DIR).min(l.max_cluster() + 1) {
                let mut b = vec![0u8; l.cluster_size as usize];
                dev.read_at(l.cluster_offset(c), &mut b)?;
                let end = b.chunks_exact(32).any(|e| e[0] == 0);
                v.extend(b);
                if end {
                    break;
                }
            }
            v
        }
    };
    Ok(bytes.chunks_exact(32).map(|c| c.try_into().unwrap()).collect())
}

/// Whether a (deleted) folder's first cluster still holds its "." and "..".
fn starts_with_dots(dev: &mut dyn BlockDevice, l: &Layout, first: u32) -> bool {
    let mut b = [0u8; 64];
    dev.read_at(l.cluster_offset(first), &mut b).is_ok()
        && &b[..11] == b".          "
        && &b[32..43] == b"..         "
        && b[11] & ATTR_DIR != 0
}

/// The long name of a live entry (parts in order on disk: last part first).
fn live_name(parts: &[[u8; 32]], e: &[u8; 32]) -> Option<String> {
    let short: [u8; 11] = e[..11].try_into().unwrap();
    let sum = checksum(&short);
    if parts.is_empty() || parts.iter().any(|p| p[13] != sum) {
        return None;
    }
    Some(units(parts))
}

/// A deleted entry's name: its long name (whose checksum also tells the
/// short name's first character), else the short name with "_" first.
fn deleted_name(parts: &[[u8; 32]], e: &[u8; 32]) -> String {
    let mut short: [u8; 11] = e[..11].try_into().unwrap();
    if let Some(sum) = parts.first().map(|p| p[13]).filter(|s| parts.iter().all(|p| p[13] == *s)) {
        // Which first character gives the checksum the long name has?
        for c in 0x20u8..=0xFF {
            short[0] = c;
            if checksum(&short) == sum {
                let name = units(parts);
                if !name.is_empty() {
                    return name;
                }
                break;
            }
        }
    }
    let mut raw = *e;
    if raw[0] == 0xE5 {
        raw[0] = b'_';
    }
    short_display(&raw)
}

/// The UTF-16 name in long-name parts (on disk: last part first).
fn units(parts: &[[u8; 32]]) -> String {
    let mut u: Vec<u16> = Vec::new();
    for p in parts.iter().rev() {
        let at = (1..11).step_by(2).chain((14..26).step_by(2)).chain((28..32).step_by(2));
        u.extend(at.map(|o| le16(p, o)));
    }
    let end = u.iter().position(|&c| c == 0 || c == 0xFFFF).unwrap_or(u.len());
    String::from_utf16_lossy(&u[..end])
}
