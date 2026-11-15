//! LZ4 blocks (SquashFS, and LZ4 frames in archives).

use dix_core::{Error, Result};

fn bad(e: &str) -> Error {
    Error::corrupted(format!("lz4 data: {e}"))
}

/// An LZ4 block (no frame), at most `max` bytes.
pub fn block(src: &[u8], max: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(max.min(1 << 20));
    block_into(src, &mut out, max)?;
    Ok(out)
}

/// An LZ4 block appended to `out`, whose bytes before are the history its
/// matches may reach back into (linked blocks); at most `max` new bytes.
pub fn block_into(src: &[u8], out: &mut Vec<u8>, max: usize) -> Result<()> {
    let start = out.len();
    let mut i = 0;
    let short = || bad("ends early");
    let len = |i: &mut usize, mut n: usize| -> Result<usize> {
        if n == 15 {
            loop {
                let b = *src.get(*i).ok_or_else(short)?;
                *i += 1;
                n += b as usize;
                if b != 255 {
                    break;
                }
            }
        }
        Ok(n)
    };
    loop {
        let token = *src.get(i).ok_or_else(short)?;
        i += 1;
        let lit = len(&mut i, (token >> 4) as usize)?;
        let lits = src.get(i..i + lit).ok_or_else(short)?;
        if out.len() - start + lit > max {
            return Err(bad("more data than a block holds"));
        }
        out.extend_from_slice(lits);
        i += lit;
        if i == src.len() {
            break;
        }
        let off = u16::from_le_bytes([*src.get(i).ok_or_else(short)?, *src.get(i + 1).ok_or_else(short)?]) as usize;
        i += 2;
        let n = len(&mut i, (token & 15) as usize)? + 4;
        if off == 0 || off > out.len() {
            return Err(bad("a match before the start"));
        }
        if out.len() - start + n > max {
            return Err(bad("more data than a block holds"));
        }
        let from = out.len() - off;
        for k in 0..n {
            let b = out[from + k];
            out.push(b);
        }
    }
    Ok(())
}
