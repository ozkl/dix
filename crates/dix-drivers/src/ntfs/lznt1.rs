//! LZNT1, the compression of NTFS compressed files.
//!
//! A compressed file is stored in *compression units* (16 clusters). A unit
//! whose clusters are all allocated holds plain data; one that is wholly
//! sparse is zeroes; otherwise its allocated clusters hold LZNT1 data,
//! which is a series of chunks, each decompressing to 4 KiB:
//!
//! * a 16-bit header: bits 0–11 are the chunk's stored size minus 3 (with
//!   the header: size − 1 of the data after the header), bit 15 says
//!   whether it is compressed; a zero header ends the unit;
//! * an uncompressed chunk is 4096 plain bytes;
//! * a compressed chunk is groups of a flag byte and eight tokens: a
//!   literal byte (flag bit 0) or a 16-bit back-reference (bit 1) whose
//!   split between offset and length bits depends on how far into the
//!   chunk's output it is.

use dix_core::{Error, Result};

pub const CHUNK: usize = 4096;

/// Decompresses one compression unit into `out` (its full size; what the
/// data does not cover is zero).
pub fn decompress(src: &[u8], out: &mut [u8]) -> Result<()> {
    let bad = || Error::corrupted("damaged compressed data (LZNT1)");
    out.fill(0);
    let (mut i, mut o) = (0usize, 0usize);
    while i + 2 <= src.len() && o < out.len() {
        let h = u16::from_le_bytes([src[i], src[i + 1]]);
        if h == 0 {
            break;
        }
        let len = (h & 0x0FFF) as usize + 1;
        let data = src.get(i + 2..i + 2 + len).ok_or_else(bad)?;
        i += 2 + len;
        let end = (o + CHUNK).min(out.len());
        if h & 0x8000 == 0 {
            // Stored: (up to) a whole chunk of plain bytes.
            let n = data.len().min(end - o);
            out[o..o + n].copy_from_slice(&data[..n]);
        } else {
            chunk(data, &mut out[o..end]).ok_or_else(bad)?;
        }
        o = end;
    }
    Ok(())
}

/// One compressed chunk into `out` (at most 4 KiB).
fn chunk(src: &[u8], out: &mut [u8]) -> Option<()> {
    let (mut i, mut o) = (0usize, 0usize);
    while i < src.len() {
        let flags = src[i];
        i += 1;
        for bit in 0..8 {
            if i >= src.len() {
                return Some(());
            }
            if flags >> bit & 1 == 0 {
                *out.get_mut(o)? = src[i];
                i += 1;
                o += 1;
            } else {
                let t = u16::from_le_bytes([src[i], *src.get(i + 1)?]) as usize;
                i += 2;
                // The further into the chunk, the more bits for the offset.
                let mut len_bits = 12u32;
                let mut p = o.checked_sub(1)?;
                while p >= 0x10 {
                    len_bits -= 1;
                    p >>= 1;
                }
                let back = (t >> len_bits) + 1;
                let len = (t & ((1 << len_bits) - 1)) + 3;
                let from = o.checked_sub(back)?;
                if o + len > out.len() {
                    return None;
                }
                // Byte by byte: a reference may overlap what it produces.
                for k in 0..len {
                    out[o + k] = out[from + k];
                }
                o += len;
            }
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_and_overlapping_copy() {
        // "abc" then a back-reference of 1 byte, 9 long.
        // Token at output position 3: 12 length bits, offset 1 => 0x0006.
        let body = [0b0000_1000u8, b'a', b'b', b'c', 0x06, 0x00];
        let mut src = vec![];
        src.extend_from_slice(&((0x8000 | (body.len() as u16 - 1) | 0x3000).to_le_bytes()));
        src.extend_from_slice(&body);
        let mut out = vec![0xFFu8; 8192];
        decompress(&src, &mut out).unwrap();
        assert_eq!(&out[..12], b"abcccccccccc");
        assert!(out[12..].iter().all(|&b| b == 0));
    }
}
