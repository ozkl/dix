//! Finding lost partitions (as TestDisk does): file systems whose
//! partition table entry is gone — deleted, or the whole table overwritten.
//!
//! The space no partition uses is read sector by sector for the start of a
//! file system: a boot sector (NTFS, exFAT, FAT12/16/32), a superblock at
//! its usual place from the start (ext2/3/4 and HFS+ at 1 KiB, Btrfs at
//! 64 KiB, the Linux swap signature at the end of its first page), or a
//! header at the start itself (APFS, XFS, LUKS). The file system's own
//! numbers give its size; a driver confirms it where there is one (and
//! reads its label). After a find the scan goes on after its end, which is
//! quick over the data and keeps a file system's own backups (FAT32's
//! boot sector copy, ext4's superblock copies) from being taken for more
//! partitions.

use dix_core::device::{SharedDevice, SliceDevice};
use dix_core::{AccessMode, BlockDevice, Result};

const SECTOR: u64 = 512;
const CHUNK: usize = 1 << 20;

/// A file system found where no partition is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lost {
    /// Byte offset and length on the disk.
    pub start: u64,
    pub size: u64,
    /// What it is: "NTFS", "FAT32", "ext4", "APFS", …
    pub kind: &'static str,
    /// The partition type that suits it (a `KNOWN_TYPES` name).
    pub type_name: &'static str,
    /// Its volume label, if a driver could read it.
    pub label: Option<String>,
    /// A driver here can open it.
    pub opens: bool,
}

/// Looks for file systems in `regions` (byte ranges, start..end) of `dev`.
/// `progress(done, total)` returns false to stop (what was found is kept).
pub fn find(
    dev: Box<dyn BlockDevice>,
    regions: &[(u64, u64)],
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<Lost>> {
    let shared = SharedDevice::new(dev);
    let mut dev = shared.handle();
    let disk = dev.size();
    let total: u64 = regions.iter().map(|(a, b)| b.saturating_sub(*a)).sum();
    let registry = crate::default_registry();
    let mut found: Vec<Lost> = Vec::new();
    let mut done = 0u64;
    let mut chunk = vec![0u8; CHUNK];
    'regions: for &(from, to) in regions {
        let mut pos = from.next_multiple_of(SECTOR);
        'region: while pos < to.min(disk) {
            let n = ((to.min(disk) - pos) as usize).min(CHUNK);
            dev.read_at(pos, &mut chunk[..n])?;
            let mut s = 0usize;
            while s + SECTOR as usize <= n {
                let at = pos + s as u64;
                if let Some((start, size, kind, type_name)) = signature(&chunk[s..s + SECTOR as usize], at)
                    && start >= from
                    && !found.iter().any(|f| start >= f.start && start < f.start + f.size)
                {
                    // Sizes not in the marked sector: swap's page count
                    // (1 KiB in); a LUKS volume's is unknown (it runs to
                    // the next find; the scan goes on after its header).
                    let known = size != 0 || kind == "Linux swap";
                    let size = if kind == "Linux swap" {
                        let mut p = [0u8; 4];
                        dev.read_at(start + 1028, &mut p)?;
                        (u32::from_le_bytes(p) as u64 + 1) * 4096
                    } else if size == 0 {
                        to.min(disk) - start
                    } else {
                        size
                    }
                    .min(disk - start);
                    // A driver to confirm it (and read its label).
                    let slice = SliceDevice::new(shared.handle(), start, size, format!("found at {start}"));
                    let (opens, label, confirmed) = match slice {
                        Ok(s) => match registry.mount(Box::new(s), None, AccessMode::ReadOnly) {
                            Ok(m) => (true, m.fs.info().label, true),
                            // No driver for it here (swap, LUKS, or one left out):
                            // its signature alone.
                            Err(_) => (false, None, !has_driver(kind)),
                        },
                        Err(_) => (false, None, false),
                    };
                    if confirmed {
                        found.push(Lost { start, size, kind, type_name, label, opens });
                        if !known {
                            s += SECTOR as usize;
                            continue;
                        }
                        let next = (start + size).next_multiple_of(SECTOR);
                        done += next.min(to).saturating_sub(pos);
                        if !progress(done.min(total), total) {
                            break 'regions;
                        }
                        pos = next;
                        continue 'region;
                    }
                }
                s += SECTOR as usize;
            }
            pos += n as u64;
            done += n as u64;
            if !progress(done.min(total), total) {
                break 'regions;
            }
        }
    }
    Ok(found)
}

/// Whether a driver here should be able to open this kind.
fn has_driver(kind: &str) -> bool {
    match kind {
        "XFS" => cfg!(feature = "xfs"),
        "Btrfs" => cfg!(feature = "btrfs"),
        k => matches!(k, "NTFS" | "exFAT" | "FAT12" | "FAT16" | "FAT32" | "ext2" | "ext3" | "ext4" | "HFS+" | "APFS"),
    }
}

fn le16(b: &[u8], o: usize) -> u64 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u64
}
fn le32(b: &[u8], o: usize) -> u64 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as u64
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn be32(b: &[u8], o: usize) -> u64 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap()) as u64
}
fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

/// A file system's mark in sector `b` (at byte `at` of the disk): where it
/// starts, its size (0: unknown), what it is, the partition type for it.
fn signature(b: &[u8], at: u64) -> Option<(u64, u64, &'static str, &'static str)> {
    // Boot sectors.
    if b[510] == 0x55 && b[511] == 0xAA {
        if &b[3..11] == b"NTFS    " {
            let bps = le16(b, 11);
            // The total leaves out the backup boot sector after it.
            let size = (le64(b, 40) + 1) * bps;
            return Some((at, size, "NTFS", "Windows data (NTFS / exFAT)"));
        }
        if &b[3..11] == b"EXFAT   " {
            let size = le64(b, 72) << b[108].min(12);
            return Some((at, size, "exFAT", "Windows data (NTFS / exFAT)"));
        }
        let bps = le16(b, 11);
        let spc = b[13] as u64;
        if matches!(b[0], 0xEB | 0xE9)
            && matches!(bps, 512 | 1024 | 2048 | 4096)
            && spc.is_power_of_two()
            && le16(b, 14) > 0
            && matches!(b[16], 1 | 2)
        {
            let total = if le16(b, 19) != 0 { le16(b, 19) } else { le32(b, 32) };
            let (kind, ty) = if &b[82..87] == b"FAT32" {
                ("FAT32", "Windows data (FAT32)")
            } else if &b[54..59] == b"FAT12" {
                ("FAT12", "Windows data (FAT16)")
            } else if &b[54..59] == b"FAT16" || &b[54..57] == b"FAT" {
                ("FAT16", "Windows data (FAT16)")
            } else {
                return None;
            };
            if total == 0 {
                return None;
            }
            return Some((at, total * bps, kind, ty));
        }
    }
    // ext2/3/4: the superblock 1 KiB in (its magic at 56).
    if at >= 1024 && le16(b, 56) == 0xEF53 && le32(b, 24) <= 6 && le32(b, 4) > 0 {
        let block = 1024u64 << le32(b, 24);
        let (compat, incompat) = (le32(b, 92), le32(b, 96));
        let blocks = le32(b, 4) | if incompat & 0x80 != 0 { le32(b, 0x150) << 32 } else { 0 };
        let kind = if incompat & 0x40 != 0 || incompat & 0x200 != 0 {
            "ext4"
        } else if compat & 0x4 != 0 {
            "ext3"
        } else {
            "ext2"
        };
        return Some((at - 1024, blocks * block, kind, "Linux filesystem"));
    }
    // HFS+ / HFSX: the volume header 1 KiB in.
    if at >= 1024 && (&b[..2] == b"H+" || &b[..2] == b"HX") && matches!(be32(b, 0) & 0xFFFF, 4 | 5) {
        let size = be32(b, 40) * be32(b, 44);
        return Some((at - 1024, size, "HFS+", "Apple HFS+"));
    }
    // APFS: the container superblock at the start.
    if &b[32..36] == b"NXSB" {
        let size = le32(b, 36) * le64(b, 40);
        return Some((at, size, "APFS", "Apple APFS"));
    }
    if &b[..4] == b"XFSB" {
        let size = be32(b, 4) * be64(b, 8);
        return Some((at, size, "XFS", "Linux filesystem"));
    }
    // Btrfs: its superblock 64 KiB in.
    if at >= 65536 && &b[64..72] == b"_BHRfS_M" {
        return Some((at - 65536, le64(b, 0x70), "Btrfs", "Linux filesystem"));
    }
    if &b[..6] == b"LUKS\xba\xbe" {
        return Some((at, 0, "LUKS", "Linux filesystem"));
    }
    // Linux swap: "SWAPSPACE2" ends its first 4 KiB page (its last sector
    // is 3584 bytes in); the page count is 1 KiB in.
    if at >= 3584 && &b[502..512] == b"SWAPSPACE2" {
        return Some((at - 3584, 0, "Linux swap", "Linux swap"));
    }
    None
}
