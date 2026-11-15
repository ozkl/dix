//! The checksums of ext4 and its journal: crc32c (Castagnoli), continued
//! from a seed and without the final inversion, as Linux's `ext4_chksum`
//! and `jbd2_chksum` compute it; and the older crc16 of `gdt_csum` group
//! descriptors.

const fn crc32c_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

const fn crc16_table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u16;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xA001 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC32C: [u32; 256] = crc32c_table();
static CRC16: [u16; 256] = crc16_table();

/// crc32c of `data`, continuing from `crc`.
pub fn crc32c(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = CRC32C[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

/// crc16 (the "ANSI" polynomial, reflected) of `data`, continuing from
/// `crc`.
pub fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc = CRC16[((crc ^ b as u16) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}
