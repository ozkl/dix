//! Creating FAT12/16/32 volumes (like `mkfs.fat`), following Microsoft's
//! FAT specification: 512-byte sectors, two FATs, the cluster size from the
//! specification's tables (adjusted so the cluster count fits the type),
//! FAT32 with FSInfo and a backup boot sector at sector 6.

use super::bpb::FatType;
use dix_core::{BlockDevice, Error, ErrorKind, Result};

const BPS: u64 = 512;

/// Smallest images (bytes) for each type.
pub const MIN_FAT12: u64 = 64 << 10;
pub const MIN_FAT16: u64 = 4 << 20;
pub const MIN_FAT32: u64 = 34 << 20;

fn bits(t: FatType) -> u64 {
    match t {
        FatType::Fat12 => 12,
        FatType::Fat16 => 16,
        FatType::Fat32 => 32,
    }
}

/// Clusters each type may have.
fn cluster_range(t: FatType) -> std::ops::RangeInclusive<u64> {
    match t {
        FatType::Fat12 => 1..=4084,
        FatType::Fat16 => 4085..=65524,
        FatType::Fat32 => 65525..=0x0FFF_FFF5,
    }
}

/// The type `mkfs` picks for an image of `size` bytes.
pub fn default_type(size: u64) -> FatType {
    if size >= 512 << 20 {
        FatType::Fat32
    } else if size >= MIN_FAT16 {
        FatType::Fat16
    } else {
        FatType::Fat12
    }
}

/// Sectors per cluster from the specification's tables.
fn suggested_spc(t: FatType, sectors: u64) -> u64 {
    match t {
        FatType::Fat12 => 1,
        FatType::Fat16 => match sectors {
            0..=32_680 => 2,
            32_681..=262_144 => 4,
            262_145..=524_288 => 8,
            524_289..=1_048_576 => 16,
            1_048_577..=2_097_152 => 32,
            _ => 64,
        },
        FatType::Fat32 => match sectors {
            0..=532_480 => 1,
            532_481..=16_777_216 => 8,
            16_777_217..=33_554_432 => 16,
            33_554_433..=67_108_864 => 32,
            _ => 64,
        },
    }
}

struct Geometry {
    spc: u64,
    reserved: u64,
    root_entries: u64,
    fat_sectors: u64,
    clusters: u64,
}

/// FAT size and cluster count for a cluster size: grow the FAT until it
/// covers the clusters left.
fn geometry(t: FatType, total: u64, spc: u64) -> Geometry {
    let reserved = if t == FatType::Fat32 { 32 } else { 1 };
    let root_entries: u64 = match t {
        FatType::Fat32 => 0,
        _ if total * BPS <= 2 << 20 => 224,
        _ => 512,
    };
    let root_sectors = (root_entries * 32).div_ceil(BPS);
    let mut fat = 1u64;
    loop {
        let data = total.saturating_sub(reserved + 2 * fat + root_sectors);
        let clusters = data / spc;
        let need = ((clusters + 2) * bits(t)).div_ceil(8).div_ceil(BPS);
        if need <= fat {
            return Geometry { spc, reserved, root_entries, fat_sectors: fat, clusters };
        }
        fat = need;
    }
}

/// An 11-byte volume label (upper case, padded).
fn label_bytes(label: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    let clean: Vec<u8> = label
        .chars()
        .map(|c| c.to_ascii_uppercase())
        .map(|c| if c.is_ascii_alphanumeric() || " !#$%&'()-@^_`{}~".contains(c) { c as u8 } else { b'_' })
        .take(11)
        .collect();
    out[..clean.len()].copy_from_slice(&clean);
    out
}

/// Creates a FAT volume of type `t` on `dev` (all of it).
pub fn mkfs(dev: &mut dyn BlockDevice, t: FatType, label: &str) -> Result<()> {
    let size = dev.size();
    let total = size / BPS;
    if t == FatType::Fat32 && total > u32::MAX as u64 {
        return Err(Error::invalid_input("FAT32 holds at most 2 TiB (with 512-byte sectors): use exFAT"));
    }
    let range = cluster_range(t);
    // The suggested cluster size, then halve / double into range.
    let mut spc = suggested_spc(t, total);
    let mut g = geometry(t, total, spc);
    while g.clusters > *range.end() && spc < 128 {
        spc *= 2;
        g = geometry(t, total, spc);
    }
    while g.clusters < *range.start() && spc > 1 {
        spc /= 2;
        g = geometry(t, total, spc);
    }
    if !range.contains(&g.clusters) {
        let what = if g.clusters < *range.start() { "small" } else { "large" };
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("the image is too {what} for {} ({} MiB)", t.name(), size >> 20),
        ));
    }
    let fat32 = t == FatType::Fat32;
    let label11 = label_bytes(if label.trim().is_empty() { "NO NAME" } else { label });
    let id = (dix_core::time::unix_now_ns() as u32) ^ ((size as u32).rotate_left(13));

    // The boot sector.
    let mut b = [0u8; 512];
    b[..3].copy_from_slice(if fat32 { &[0xEB, 0x58, 0x90] } else { &[0xEB, 0x3C, 0x90] });
    b[3..11].copy_from_slice(b"MSWIN4.1");
    b[11..13].copy_from_slice(&(BPS as u16).to_le_bytes());
    b[13] = g.spc as u8;
    b[14..16].copy_from_slice(&(g.reserved as u16).to_le_bytes());
    b[16] = 2;
    b[17..19].copy_from_slice(&(g.root_entries as u16).to_le_bytes());
    if !fat32 && total < 0x10000 {
        b[19..21].copy_from_slice(&(total as u16).to_le_bytes());
    } else {
        b[32..36].copy_from_slice(&(total as u32).to_le_bytes());
    }
    b[21] = 0xF8;
    b[24..26].copy_from_slice(&32u16.to_le_bytes()); // sectors per track
    b[26..28].copy_from_slice(&64u16.to_le_bytes()); // heads
    let (ext, code) = if fat32 {
        b[36..40].copy_from_slice(&(g.fat_sectors as u32).to_le_bytes());
        b[44..48].copy_from_slice(&2u32.to_le_bytes()); // root cluster
        b[48..50].copy_from_slice(&1u16.to_le_bytes()); // FSInfo
        b[50..52].copy_from_slice(&6u16.to_le_bytes()); // backup boot sector
        b[82..90].copy_from_slice(b"FAT32   ");
        (64, 90)
    } else {
        b[22..24].copy_from_slice(&(g.fat_sectors as u16).to_le_bytes());
        b[54..62].copy_from_slice(if t == FatType::Fat12 { b"FAT12   " } else { b"FAT16   " });
        (36, 62)
    };
    b[ext] = 0x80; // drive number
    b[ext + 2] = 0x29; // extended boot signature
    b[ext + 3..ext + 7].copy_from_slice(&id.to_le_bytes());
    b[ext + 7..ext + 18].copy_from_slice(&label11);
    // Not bootable: "int 18h" (try the next boot device), then halt.
    b[code..code + 4].copy_from_slice(&[0xCD, 0x18, 0xEB, 0xFE]);
    b[510] = 0x55;
    b[511] = 0xAA;

    // Reserved sectors, FATs and the root directory start out as zeroes.
    let root_sectors = (g.root_entries * 32).div_ceil(BPS);
    let fat_area = g.reserved + 2 * g.fat_sectors + root_sectors + if fat32 { g.spc } else { 0 };
    let zeroes = vec![0u8; 1 << 20];
    let mut at = 0;
    while at < fat_area * BPS {
        let n = (fat_area * BPS - at).min(zeroes.len() as u64) as usize;
        dev.write_at(at, &zeroes[..n])?;
        at += n as u64;
    }
    dev.write_at(0, &b)?;
    if fat32 {
        let mut info = [0u8; 512];
        info[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
        info[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
        info[488..492].copy_from_slice(&((g.clusters - 1) as u32).to_le_bytes());
        info[492..496].copy_from_slice(&3u32.to_le_bytes());
        info[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
        dev.write_at(BPS, &info)?;
        dev.write_at(6 * BPS, &b)?;
        dev.write_at(7 * BPS, &info)?;
    }
    // FAT entries 0 and 1 (media; clean, no errors), and the root cluster.
    let first: Vec<u8> = match t {
        FatType::Fat12 => vec![0xF8, 0xFF, 0xFF],
        FatType::Fat16 => vec![0xF8, 0xFF, 0xFF, 0xFF],
        FatType::Fat32 => [0x0FFF_FFF8u32, 0x0FFF_FFFF, 0x0FFF_FFFF].iter().flat_map(|v| v.to_le_bytes()).collect(),
    };
    for i in 0..2 {
        dev.write_at((g.reserved + i * g.fat_sectors) * BPS, &first)?;
    }
    // The label as a root directory entry too.
    if !label.trim().is_empty() {
        let root = (g.reserved + 2 * g.fat_sectors) * BPS;
        let mut e = super::dir::short_entry(&label11, 0, super::dir::ATTR_VOLUME, 0, 0, dix_core::time::unix_now());
        e[12..22].fill(0); // only the modification time is kept
        dev.write_at(root, &e)?;
    }
    dev.flush()
}
