//! The boot sector (BIOS parameter block) and the volume layout.
//!
//! ```text
//! | reserved (boot sector, FAT32: FSInfo, backup boot) | FAT #1 | FAT #2 |
//! | root directory (FAT12/16 only, fixed size) | data clusters 2, 3, … |
//! ```
//!
//! The FAT type is decided by the number of data clusters alone, as
//! Microsoft's specification requires: fewer than 4085 is FAT12, fewer than
//! 65525 FAT16, otherwise FAT32.

use dix_core::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatType {
    Fat12,
    Fat16,
    Fat32,
}

impl FatType {
    pub fn name(self) -> &'static str {
        match self {
            FatType::Fat12 => "FAT12",
            FatType::Fat16 => "FAT16",
            FatType::Fat32 => "FAT32",
        }
    }
}

pub(super) fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

pub(super) fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// Where everything is.
#[derive(Debug, Clone)]
pub struct Layout {
    pub fat_type: FatType,
    pub bytes_per_sector: u32,
    pub cluster_size: u32,
    pub reserved_sectors: u32,
    pub num_fats: u32,
    pub fat_sectors: u32,
    /// Root directory entries (FAT12/16).
    pub root_entries: u32,
    pub total_sectors: u64,
    /// Data clusters (numbered 2 ..= clusters + 1).
    pub clusters: u32,
    pub fat_offset: u64,
    /// The FAT12/16 root directory region.
    pub root_offset: u64,
    pub root_bytes: u64,
    pub data_offset: u64,
    /// FAT32: the first cluster of the root directory.
    pub root_cluster: u32,
    /// FAT32: the FSInfo sector.
    pub fs_info: Option<u32>,
    /// FAT32 with mirroring off: the only FAT in use.
    pub active_fat: Option<u32>,
    pub label: Option<String>,
    pub volume_id: u32,
    /// Offset of the boot sector's state byte (bit 0: dirty).
    pub state_offset: usize,
}

impl Layout {
    pub fn parse(b: &[u8]) -> Result<Layout> {
        let bad = |what: &str| Error::corrupted(format!("not a FAT boot sector ({what})"));
        if b.len() < 512 {
            return Err(bad("short"));
        }
        if !(b[0] == 0xEB || b[0] == 0xE9) {
            return Err(bad("no jump instruction"));
        }
        let bps = le16(b, 11) as u32;
        if !bps.is_power_of_two() || !(512..=4096).contains(&bps) {
            return Err(bad("sector size"));
        }
        let spc = b[13] as u32;
        if spc == 0 || !spc.is_power_of_two() {
            return Err(bad("cluster size"));
        }
        let reserved = le16(b, 14) as u32;
        let num_fats = b[16] as u32;
        let root_entries = le16(b, 17) as u32;
        let total16 = le16(b, 19) as u64;
        let media = b[21];
        let fat16 = le16(b, 22) as u32;
        let total32 = le32(b, 32) as u64;
        if reserved == 0 || num_fats == 0 || num_fats > 4 || !(media == 0xF0 || media >= 0xF8) {
            return Err(bad("parameters"));
        }
        let fat_sectors = if fat16 != 0 { fat16 } else { le32(b, 36) };
        let total_sectors = if total16 != 0 { total16 } else { total32 };
        if fat_sectors == 0 || total_sectors == 0 {
            return Err(bad("sizes"));
        }
        let root_sectors = (root_entries * 32).div_ceil(bps);
        let meta = reserved as u64 + num_fats as u64 * fat_sectors as u64 + root_sectors as u64;
        let data_sectors = total_sectors.checked_sub(meta).ok_or_else(|| bad("sizes"))?;
        let clusters = (data_sectors / spc as u64).min(0x0FFF_FFF5) as u32;
        if clusters == 0 {
            return Err(bad("no data clusters"));
        }
        let fat_type = if clusters < 4085 {
            FatType::Fat12
        } else if clusters < 65525 {
            FatType::Fat16
        } else {
            FatType::Fat32
        };
        if fat_type == FatType::Fat32 && root_entries != 0 || fat_type != FatType::Fat32 && root_entries == 0 {
            return Err(bad("root directory"));
        }
        // The FATs must be big enough for every cluster.
        let entry_bits = match fat_type {
            FatType::Fat12 => 12,
            FatType::Fat16 => 16,
            FatType::Fat32 => 32,
        };
        if (fat_sectors as u64 * bps as u64 * 8) < (clusters as u64 + 2) * entry_bits {
            return Err(bad("FAT too small"));
        }
        let fat_offset = reserved as u64 * bps as u64;
        let root_offset = fat_offset + num_fats as u64 * fat_sectors as u64 * bps as u64;
        let root_bytes = root_sectors as u64 * bps as u64;
        let (ext, label_at, id_at, sig_at, state_offset) =
            if fat_type == FatType::Fat32 { (true, 71, 67, 66, 0x41) } else { (false, 43, 39, 38, 0x25) };
        let (mut root_cluster, mut fs_info, mut active_fat) = (0, None, None);
        if ext {
            let flags = le16(b, 40);
            if flags & 0x80 != 0 {
                active_fat = Some((flags & 0x0F) as u32).filter(|&f| f < num_fats);
            }
            root_cluster = le32(b, 44);
            if root_cluster < 2 || root_cluster > clusters + 1 {
                return Err(bad("root cluster"));
            }
            let fsi = le16(b, 48) as u32;
            fs_info = (fsi != 0 && fsi != 0xFFFF && fsi < reserved).then_some(fsi);
        }
        let (label, volume_id) = if b[sig_at] == 0x29 {
            let l = String::from_utf8_lossy(&b[label_at..label_at + 11]).trim_end().to_owned();
            (Some(l).filter(|l| !l.is_empty() && l != "NO NAME"), le32(b, id_at))
        } else {
            (None, 0)
        };
        Ok(Layout {
            fat_type,
            bytes_per_sector: bps,
            cluster_size: bps * spc,
            reserved_sectors: reserved,
            num_fats,
            fat_sectors,
            root_entries,
            total_sectors,
            clusters,
            fat_offset,
            root_offset,
            root_bytes,
            data_offset: root_offset + root_bytes,
            root_cluster,
            fs_info,
            active_fat,
            label,
            volume_id,
            state_offset,
        })
    }

    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        self.data_offset + (cluster as u64 - 2) * self.cluster_size as u64
    }

    pub fn max_cluster(&self) -> u32 {
        self.clusters + 1
    }

    pub fn fat_bytes(&self) -> u64 {
        self.fat_sectors as u64 * self.bytes_per_sector as u64
    }

    /// Where each FAT copy in use starts.
    pub fn fat_copies(&self) -> Vec<u64> {
        match self.active_fat {
            Some(f) => vec![self.fat_offset + f as u64 * self.fat_bytes()],
            None => (0..self.num_fats).map(|i| self.fat_offset + i as u64 * self.fat_bytes()).collect(),
        }
    }
}

/// Whether sector 0 looks like a FAT boot sector (for probing).
pub fn looks_like_fat(b: &[u8]) -> bool {
    Layout::parse(b).is_ok()
}
