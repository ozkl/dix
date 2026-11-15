//! What a disk copy needs to know about file systems: the space they use.
//!
//! Copying only the used space (as partclone and Clonezilla do) leaves out
//! what file systems mark free — their allocation maps say which clusters
//! or blocks hold something: FAT12/16/32 (the FAT), exFAT (its allocation
//! bitmap), ext2/3/4 (the group block bitmaps), NTFS (`$Bitmap`), XFS (each
//! allocation group's free-space B+tree) and Btrfs (what the extent tree
//! lists, in its chunks; device space in no chunk is free).
//! Everything else is copied: the partition table, the space between
//! partitions, partitions with other file systems, a file system whose
//! maps cannot be read (or trusted: an ext4 journal or a Btrfs log tree
//! waiting to be replayed). Free space reads as zeros in the copy.

use dix_core::device::{SharedDevice, SliceDevice};
use dix_core::imaging::complement;
use dix_core::partition::read_partition_table;
use dix_core::{BlockDevice, Result};

/// One partition (or the whole disk without a table) and what is known of
/// its use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Area {
    /// "Partition 2 · Linux filesystem", or "The disk".
    pub what: String,
    pub start: u64,
    pub size: u64,
    /// The file system whose free space is left out ("FAT32", "NTFS", …).
    pub kind: Option<&'static str>,
    /// Bytes left out.
    pub free: u64,
    /// Why it is copied whole although its file system is known.
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsedSpace {
    /// The byte ranges to copy, sorted.
    pub ranges: Vec<(u64, u64)>,
    /// Their total.
    pub used: u64,
    pub size: u64,
    pub areas: Vec<Area>,
}

/// The space to copy of a disk (or of a single file system).
pub fn used_space(dev: Box<dyn BlockDevice>) -> Result<UsedSpace> {
    let shared = SharedDevice::new(dev);
    let mut h = shared.handle();
    let size = h.size();
    let parts: Vec<(u64, u64, String)> = match read_partition_table(&mut h)? {
        Some(t) => t
            .partitions
            .iter()
            .filter(|p| p.may_hold_a_file_system() && !p.truncated && p.start < size)
            .map(|p| (p.start, p.size.min(size - p.start), format!("Partition {}", p.label())))
            .collect(),
        None => vec![(0, size, "The disk".to_owned())],
    };
    let mut holes = Vec::new();
    let mut areas = Vec::new();
    for (start, len, what) in parts {
        let mut area = Area { what: what.clone(), start, size: len, kind: None, free: 0, note: None };
        let slice = SliceDevice::new(shared.handle(), start, len, what)?;
        match free_in(Box::new(slice)) {
            Ok((kind, free)) => {
                area.kind = kind;
                match free {
                    Some(free) => {
                        for (a, b) in free {
                            let (a, b) = (a.min(len), b.min(len));
                            area.free += b - a;
                            holes.push((start + a, start + b));
                        }
                    }
                    None if kind.is_some() => {
                        area.kind = None;
                        area.note = Some(format!(
                            "{} whose allocation maps cannot be trusted now (a journal or log waiting to be \
                             replayed, or data striped over several devices)",
                            kind.unwrap_or_default()
                        ));
                    }
                    None => {}
                }
            }
            Err(e) => area.note = Some(format!("its allocation map could not be read: {e}")),
        }
        areas.push(area);
    }
    let ranges = complement(&holes, size);
    let used = ranges.iter().map(|r| r.1 - r.0).sum();
    Ok(UsedSpace { ranges, used, size, areas })
}

/// A file system whose free space is known (its name), and its free byte
/// ranges (None: not to be trusted).
type Free = (Option<&'static str>, Option<Vec<(u64, u64)>>);

/// What [`Free`] there is on `dev`.
#[allow(unused_mut, unused_variables)]
fn free_in(mut dev: Box<dyn BlockDevice>) -> Result<Free> {
    let mut b = vec![0u8; dev.size().min(2048) as usize];
    if b.len() < 512 {
        return Ok((None, None));
    }
    dev.read_at(0, &mut b)?;
    #[cfg(feature = "ntfs")]
    if &b[3..11] == b"NTFS    " {
        return Ok((Some("NTFS"), Some(crate::ntfs::free_space(dev)?)));
    }
    #[cfg(feature = "exfat")]
    if &b[3..11] == b"EXFAT   " {
        return Ok((Some("exFAT"), Some(crate::fat::exfat::undelete::free_space(dev)?)));
    }
    #[cfg(feature = "fat")]
    if let Ok(l) = crate::fat::bpb::Layout::parse(&b[..512]) {
        return Ok((Some(l.fat_type.name()), Some(crate::fat::free_space(dev)?)));
    }
    #[cfg(feature = "xfs")]
    if &b[..4] == b"XFSB" {
        return Ok((Some("XFS"), Some(crate::xfs::free_space(dev)?)));
    }
    #[cfg(feature = "btrfs")]
    if dev.size() > 0x10048 {
        let mut m = [0u8; 8];
        dev.read_at(0x10040, &mut m)?;
        if &m == b"_BHRfS_M" {
            return Ok((Some("Btrfs"), crate::btrfs::free_space(dev)?));
        }
    }
    #[cfg(feature = "ext4")]
    if b.len() >= 2048 && u16::from_le_bytes([b[1024 + 56], b[1024 + 57]]) == 0xEF53 {
        return Ok((Some("ext2/3/4"), crate::ext4::fsck::free_space(dev)?));
    }
    Ok((None, None))
}

#[cfg(test)]
#[path = "imaging_tests.rs"]
mod tests;
