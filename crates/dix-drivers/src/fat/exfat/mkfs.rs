//! Creating exFAT volumes (like `mkfs.exfat`): 512-byte sectors, the
//! cluster size Microsoft recommends for the size, one FAT, and at the start
//! of the cluster heap the allocation bitmap, the up-case table and the root
//! directory. The boot region (and its backup) ends in its checksum sector.

use super::{OEM, boot_checksum, upcase};
use dix_core::{BlockDevice, Error, ErrorKind, Result};

const BPS: u64 = 512;

/// Smallest image accepted.
pub const MIN_SIZE: u64 = 1 << 20;

/// Sectors per cluster, as a power of two.
fn spc_shift(size: u64) -> u32 {
    match size {
        0..=0x1000_0000 => 3,             // up to 256 MiB: 4 KiB
        0x1000_0001..=0x8_0000_0000 => 6, // up to 32 GiB: 32 KiB
        _ => 8,                           // 128 KiB
    }
}

pub fn mkfs(dev: &mut dyn BlockDevice, label: &str) -> Result<()> {
    let size = dev.size();
    if size < MIN_SIZE {
        return Err(Error::new(ErrorKind::InvalidInput, "exFAT needs at least 1 MiB"));
    }
    let total = size / BPS;
    let shift = spc_shift(size);
    let spc = 1u64 << shift;
    let cs = spc * BPS;
    // Aligned like mkfs.exfat: the FAT at 1 MiB (small volumes: 16 KiB).
    let fat_offset: u64 = if total >= 1 << 17 { 2048 } else { 32 };
    let (mut fat_len, mut heap, mut clusters) = (1u64, 0u64, 0u64);
    for _ in 0..8 {
        heap = (fat_offset + fat_len).next_multiple_of(spc.max(fat_offset));
        clusters = (total.saturating_sub(heap) / spc).min(0xFFFF_FFF5);
        let need = ((clusters + 2) * 4).div_ceil(BPS);
        if need <= fat_len {
            break;
        }
        fat_len = need;
    }
    if clusters < 8 {
        return Err(Error::new(ErrorKind::InvalidInput, "the image is too small for exFAT"));
    }
    let up = upcase::Upcase::generate();
    let table = up.compress();
    let bitmap_len = clusters.div_ceil(8);
    let n_bitmap = bitmap_len.div_ceil(cs);
    let n_upcase = (table.len() as u64).div_ceil(cs);
    // Clusters: bitmap from 2, then the up-case table, then the root.
    let bitmap_first = 2u32;
    let upcase_first = bitmap_first + n_bitmap as u32;
    let root = upcase_first + n_upcase as u32;
    let used = n_bitmap + n_upcase + 1;
    let off = |c: u32| heap * BPS + (c as u64 - 2) * cs;

    // Zero the boot regions, the FAT and the clusters used.
    let zeroes = vec![0u8; 1 << 20];
    let zero = |dev: &mut dyn BlockDevice, at: u64, len: u64| -> Result<()> {
        let mut done = 0;
        while done < len {
            let n = (len - done).min(zeroes.len() as u64) as usize;
            dev.write_at(at + done, &zeroes[..n])?;
            done += n as u64;
        }
        Ok(())
    };
    zero(dev, 0, (fat_offset + fat_len) * BPS)?;
    zero(dev, off(2), used * cs)?;

    // The FAT: media, end, then the three chains.
    let mut fat: Vec<u8> = Vec::new();
    let mut entry = |v: u32| fat.extend_from_slice(&v.to_le_bytes());
    entry(0xFFFF_FFF8);
    entry(0xFFFF_FFFF);
    for (first, n) in [(bitmap_first, n_bitmap), (upcase_first, n_upcase), (root, 1)] {
        for k in 0..n as u32 {
            entry(if k + 1 == n as u32 { 0xFFFF_FFFF } else { first + k + 1 });
        }
    }
    dev.write_at(fat_offset * BPS, &fat)?;

    // The bitmap: the clusters used above.
    let mut bitmap = vec![0u8; bitmap_len as usize];
    for i in 0..used as usize {
        bitmap[i / 8] |= 1 << (i % 8);
    }
    dev.write_at(off(bitmap_first), &bitmap)?;
    dev.write_at(off(upcase_first), &table)?;

    // The root: label, bitmap, up-case table.
    let mut entries: Vec<[u8; 32]> = Vec::new();
    let units: Vec<u16> = label.trim().encode_utf16().take(11).collect();
    if !units.is_empty() {
        let mut e = [0u8; 32];
        e[0] = 0x83;
        e[1] = units.len() as u8;
        for (k, u) in units.iter().enumerate() {
            e[2 + 2 * k..4 + 2 * k].copy_from_slice(&u.to_le_bytes());
        }
        entries.push(e);
    }
    let mut e = [0u8; 32];
    e[0] = 0x81;
    e[20..24].copy_from_slice(&bitmap_first.to_le_bytes());
    e[24..32].copy_from_slice(&bitmap_len.to_le_bytes());
    entries.push(e);
    let mut e = [0u8; 32];
    e[0] = 0x82;
    e[4..8].copy_from_slice(&upcase::checksum(&table).to_le_bytes());
    e[20..24].copy_from_slice(&upcase_first.to_le_bytes());
    e[24..32].copy_from_slice(&(table.len() as u64).to_le_bytes());
    entries.push(e);
    let root_bytes: Vec<u8> = entries.concat();
    dev.write_at(off(root), &root_bytes)?;

    // The boot region.
    let mut region = vec![0u8; 12 * BPS as usize];
    let b = &mut region[..512];
    b[..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
    b[3..11].copy_from_slice(OEM);
    b[72..80].copy_from_slice(&total.to_le_bytes());
    b[80..84].copy_from_slice(&(fat_offset as u32).to_le_bytes());
    b[84..88].copy_from_slice(&(fat_len as u32).to_le_bytes());
    b[88..92].copy_from_slice(&(heap as u32).to_le_bytes());
    b[92..96].copy_from_slice(&(clusters as u32).to_le_bytes());
    b[96..100].copy_from_slice(&root.to_le_bytes());
    let serial = (dix_core::time::unix_now_ns() as u32) ^ (size as u32).rotate_left(7);
    b[100..104].copy_from_slice(&serial.to_le_bytes());
    b[104..106].copy_from_slice(&0x0100u16.to_le_bytes()); // revision 1.0
    b[108] = 9;
    b[109] = shift as u8;
    b[110] = 1; // FATs
    b[111] = 0x80;
    b[112] = (used * 100 / clusters) as u8;
    b[120..124].copy_from_slice(&[0xCD, 0x18, 0xEB, 0xFE]); // not bootable
    b[510] = 0x55;
    b[511] = 0xAA;
    // Extended boot sectors: their signature only.
    for s in 1..=8 {
        let end = (s + 1) * BPS as usize;
        region[end - 4..end].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    }
    let sum = boot_checksum(&region[..11 * BPS as usize]);
    for k in 0..(BPS as usize / 4) {
        let at = 11 * BPS as usize + 4 * k;
        region[at..at + 4].copy_from_slice(&sum.to_le_bytes());
    }
    dev.write_at(0, &region)?;
    dev.write_at(12 * BPS, &region)?;
    dev.flush()
}
