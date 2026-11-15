//! LZVN, Apple's fast LZ77 coder (in LZFSE streams and compressed files).
//!
//! A stream of opcodes, each a few literal bytes and/or a match:
//!
//! | opcode | literals | match | distance |
//! |--------|----------|-------|----------|
//! | `LLMMMDDD dddddddd` (sml_d) | L | M + 3 | 11 bits |
//! | `LLMMM110` (pre_d) | L | M + 3 | the previous one |
//! | `LLMMM111 d16` (lrg_d) | L | M + 3 | 16 bits |
//! | `101LLMMM d16` (med_d) | L | (MMM:2 low bits of d16) + 3 | d16 >> 2 |
//! | `1110LLLL` / `11100000 n` | L (1–15) / n + 16 | — | — |
//! | `1111MMMM` / `11110000 n` | — | M (1–15) / n + 16 | the previous one |
//!
//! The literals follow the opcode and its operand; then the match is
//! copied. `0x06` ends the stream; `0x0E` and `0x16` do nothing.

use dix_core::{Error, Result};

fn bad() -> Error {
    Error::corrupted("LZVN data is damaged")
}

/// Decodes `src` into `out` (appended), up to `limit` bytes in all.
pub fn decode(src: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<()> {
    let start = out.len();
    let mut i = 0usize;
    let mut d = 0usize;
    let byte = |i: usize| src.get(i).copied().ok_or_else(bad);
    loop {
        let op = byte(i)?;
        let (lits, mlen, skip): (usize, usize, usize);
        match op {
            0x06 => return Ok(()),
            0x0E | 0x16 => {
                i += 1;
                continue;
            }
            0x70..=0x7F | 0xD0..=0xDF => return Err(bad()),
            0xA0..=0xBF => {
                let w = u16::from_le_bytes([byte(i + 1)?, byte(i + 2)?]) as usize;
                lits = ((op >> 3) & 3) as usize;
                mlen = ((((op & 7) as usize) << 2) | (w & 3)) + 3;
                d = w >> 2;
                skip = 3;
            }
            0xE0 => {
                lits = byte(i + 1)? as usize + 16;
                mlen = 0;
                skip = 2;
            }
            0xE1..=0xEF => {
                lits = (op & 0x0F) as usize;
                mlen = 0;
                skip = 1;
            }
            0xF0 => {
                lits = 0;
                mlen = byte(i + 1)? as usize + 16;
                skip = 2;
            }
            0xF1..=0xFF => {
                lits = 0;
                mlen = (op & 0x0F) as usize;
                skip = 1;
            }
            _ => {
                lits = (op >> 6) as usize;
                mlen = ((op >> 3) & 7) as usize + 3;
                match op & 7 {
                    7 => {
                        d = u16::from_le_bytes([byte(i + 1)?, byte(i + 2)?]) as usize;
                        skip = 3;
                    }
                    6 => {
                        if op < 0x40 {
                            return Err(bad());
                        }
                        skip = 1;
                    }
                    _ => {
                        d = (((op & 7) as usize) << 8) | byte(i + 1)? as usize;
                        skip = 2;
                    }
                }
            }
        }
        i += skip;
        if lits > 0 {
            let l = src.get(i..i + lits).ok_or_else(bad)?;
            if out.len() - start + lits > limit {
                return Err(bad());
            }
            out.extend_from_slice(l);
            i += lits;
        }
        if mlen > 0 {
            if d == 0 || d > out.len() - start || out.len() - start + mlen > limit {
                return Err(bad());
            }
            let from = out.len() - d;
            for k in 0..mlen {
                let b = out[from + k];
                out.push(b);
            }
        }
    }
}
