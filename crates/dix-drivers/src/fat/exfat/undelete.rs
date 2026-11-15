//! Deleted files on exFAT.
//!
//! Deleting clears the in-use bit of a file's entry set (0x85 → 0x05,
//! stream 0xC0 → 0x40, names 0xC1 → 0x41) and its clusters' bits in the
//! allocation bitmap; the set keeps the name, times, size and first
//! cluster. Files stored in one piece (the stream's "no FAT chain" flag,
//! as most are) are read from there; others along their FAT chain if it is
//! still whole, else in one piece. The bitmap says which of their clusters
//! are in use again.

use super::{Boot, le16, le32, le64, timestamp};
use crate::fat::table::Fat;
use crate::recover::{Chance, Found, Recovered, Runs};
use dix_core::{BlockDevice, Result};

const ATTR_DIR: u16 = 0x10;
const NO_FAT_CHAIN: u8 = 0x02;
/// Longest folder read (clusters).
const MAX_DIR: u32 = 4096;

pub fn scan(mut dev: Box<dyn BlockDevice>) -> Result<Recovered> {
    let mut b = [0u8; 512];
    dev.read_at(0, &mut b)?;
    let boot = Boot::parse(&b)?;
    let mut fat = Fat::exfat(boot.fat_offset, boot.fat_bytes, boot.max_cluster());
    let cs = boot.cluster_size as u64;
    // The allocation bitmap (from the root).
    let root_runs = chain_runs(dev.as_mut(), &mut fat, &boot, boot.root_cluster, false, 0)?;
    let root = slots(dev.as_mut(), &boot, &root_runs)?;
    let bitmap = match root.iter().find(|e| e[0] == 0x81) {
        Some(e) => {
            let runs = chain_runs(dev.as_mut(), &mut fat, &boot, le32(e, 20), false, le64(e, 24))?;
            read_runs(dev.as_mut(), &boot, &runs, le64(e, 24))?
        }
        None => Vec::new(),
    };
    let used = |c: u32| -> bool {
        let i = c.wrapping_sub(2) as usize;
        bitmap.get(i / 8).is_none_or(|b| b & (1 << (i % 8)) != 0)
    };
    let mut found = Vec::new();
    let mut files: Vec<Vec<(Option<u64>, u64)>> = Vec::new();
    // Folders to look through: (path, clusters, deleted itself).
    type Folder = (Vec<String>, Vec<(u32, u32)>, bool);
    let mut stack: Vec<Folder> = vec![(Vec::new(), root_runs, false)];
    let mut seen = std::collections::HashSet::new();
    while let Some((path, runs, in_deleted)) = stack.pop() {
        let Ok(entries) = slots(dev.as_mut(), &boot, &runs) else { continue };
        let mut i = 0;
        while i < entries.len() {
            let e = &entries[i];
            if e[0] == 0 {
                break;
            }
            let (t, n) = (e[0], e[1] as usize);
            let file = t == 0x85 || t == 0x05;
            if !file || n < 2 || i + n >= entries.len() {
                i += 1;
                continue;
            }
            let deleted = t == 0x05;
            let set = &entries[i..=i + n];
            // The secondaries must match the primary's state.
            let (stream, name_t) = if deleted { (0x40, 0x41) } else { (0xC0, 0xC1) };
            if set[1][0] != stream || set[2..].iter().any(|s| s[0] != name_t) {
                i += 1;
                continue;
            }
            i += n + 1;
            let len = set[1][3] as usize;
            let units: Vec<u16> =
                set[2..].iter().flat_map(|s| (0..15).map(move |k| le16(s, 2 + 2 * k))).take(len).collect();
            let name = String::from_utf16_lossy(&units);
            let attrs = le16(&set[0], 4);
            let dir = attrs & ATTR_DIR != 0;
            let nofat = set[1][1] & NO_FAT_CHAIN != 0;
            let (first, size) = (le32(&set[1], 20), le64(&set[1], 24));
            let mut p = path.clone();
            p.push(name);
            let in_volume = first >= 2 && first <= boot.max_cluster();
            if !deleted && !in_deleted {
                if dir
                    && in_volume
                    && seen.insert(first)
                    && let Ok(r) = chain_runs(dev.as_mut(), &mut fat, &boot, first, nofat, size)
                {
                    stack.push((p, r, false));
                }
                continue;
            }
            let modified = timestamp(&set[0], 12, Some(21), 23);
            let created = timestamp(&set[0], 8, Some(20), 22);
            // Where the data is: the chain if it is still whole, else one piece.
            let need = size.div_ceil(cs) as u32;
            let runs = if !in_volume || need == 0 {
                Vec::new()
            } else {
                let chained = (!nofat)
                    .then(|| chain_runs(dev.as_mut(), &mut fat, &boot, first, false, size).ok())
                    .flatten()
                    .filter(|r| r.iter().map(|(_, n)| *n).sum::<u32>() == need);
                chained.unwrap_or_else(|| vec![(first, need.min(boot.max_cluster() - first + 1))])
            };
            if dir {
                found.push(Found {
                    path: p.clone(),
                    dir: true,
                    size: 0,
                    modified,
                    created,
                    chance: Chance::Folder,
                    id: 0,
                });
                if !runs.is_empty() && seen.insert(first) {
                    stack.push((p, runs, true));
                }
                continue;
            }
            let (mut free, mut reused) = (0u64, 0u64);
            for &(s, k) in &runs {
                for c in s..s + k {
                    if used(c) { reused += 1 } else { free += 1 }
                }
            }
            let chance = if size == 0 {
                Chance::Intact
            } else if runs.is_empty() {
                Chance::Lost
            } else {
                Chance::of(free, reused)
            };
            let mut data: Vec<(Option<u64>, u64)> = Vec::new();
            let mut left = size;
            for &(s, k) in &runs {
                let len = (k as u64 * cs).min(left);
                data.push((Some(boot.cluster_offset(s)), len));
                left -= len;
            }
            if left > 0 {
                data.push((None, left));
            }
            found.push(Found { path: p, dir: false, size, modified, created, chance, id: files.len() });
            files.push(data);
        }
    }
    Ok(Recovered::new("exFAT", found, Box::new(Runs { dev, files })))
}

/// The free space of an exFAT volume (clusters its allocation bitmap
/// marks free), as byte ranges: what a copy of only the used space leaves
/// out. Without a bitmap nothing is free.
pub fn free_space(mut dev: Box<dyn BlockDevice>) -> Result<Vec<(u64, u64)>> {
    let mut b = [0u8; 512];
    dev.read_at(0, &mut b)?;
    let boot = Boot::parse(&b)?;
    let mut fat = Fat::exfat(boot.fat_offset, boot.fat_bytes, boot.max_cluster());
    let root_runs = chain_runs(dev.as_mut(), &mut fat, &boot, boot.root_cluster, false, 0)?;
    let root = slots(dev.as_mut(), &boot, &root_runs)?;
    let Some(e) = root.iter().find(|e| e[0] == 0x81) else { return Ok(Vec::new()) };
    let len = le64(e, 24);
    if len * 8 < boot.clusters as u64 {
        return Ok(Vec::new());
    }
    let runs = chain_runs(dev.as_mut(), &mut fat, &boot, le32(e, 20), false, len)?;
    let bitmap = read_runs(dev.as_mut(), &boot, &runs, len)?;
    let cs = boot.cluster_size as u64;
    let mut out: Vec<(u64, u64)> = Vec::new();
    for i in 0..boot.clusters {
        if bitmap[(i / 8) as usize] >> (i % 8) & 1 != 0 {
            continue;
        }
        let at = boot.cluster_offset(i + 2);
        match out.last_mut() {
            Some(r) if r.1 == at => r.1 += cs,
            _ => out.push((at, at + cs)),
        }
    }
    Ok(out)
}

/// A chain's runs (first cluster, count): in one piece, or along the FAT.
fn chain_runs(
    dev: &mut dyn BlockDevice,
    fat: &mut Fat,
    boot: &Boot,
    first: u32,
    nofat: bool,
    size: u64,
) -> Result<Vec<(u32, u32)>> {
    if nofat {
        let n = size.div_ceil(boot.cluster_size as u64) as u32;
        return Ok(vec![(first, n.min(boot.max_cluster() - first + 1))]);
    }
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for c in fat.chain(dev, first)?.into_iter().take(1 << 24) {
        match runs.last_mut() {
            Some((s, n)) if *s + *n == c => *n += 1,
            _ => runs.push((c, 1)),
        }
    }
    Ok(runs)
}

fn read_runs(dev: &mut dyn BlockDevice, boot: &Boot, runs: &[(u32, u32)], len: u64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for &(s, n) in runs {
        let mut b = vec![0u8; (n as u64 * boot.cluster_size as u64) as usize];
        dev.read_at(boot.cluster_offset(s), &mut b)?;
        out.extend(b);
    }
    out.truncate(len as usize);
    Ok(out)
}

/// A folder's entries.
fn slots(dev: &mut dyn BlockDevice, boot: &Boot, runs: &[(u32, u32)]) -> Result<Vec<[u8; 32]>> {
    let mut out = Vec::new();
    let mut clusters = 0;
    for &(s, n) in runs {
        let n = n.min(MAX_DIR - clusters);
        clusters += n;
        let mut b = vec![0u8; (n as u64 * boot.cluster_size as u64) as usize];
        dev.read_at(boot.cluster_offset(s), &mut b)?;
        out.extend(b.chunks_exact(32).map(|c| <[u8; 32]>::try_from(c).unwrap()));
        if clusters >= MAX_DIR {
            break;
        }
    }
    Ok(out)
}
