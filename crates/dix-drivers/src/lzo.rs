//! LZO1X decompression (SquashFS and Btrfs use it).

use dix_core::{Error, Result};

fn bad(what: &str, e: impl std::fmt::Display) -> Error {
    Error::corrupted(format!("{what} data: {e}"))
}

fn short(what: &str) -> Error {
    bad(what, "ends early")
}

/// LZO1X (as lzo1x_999 makes it and the Linux kernel reads it).
pub fn lzo1x(src: &[u8], max: usize) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(max.min(1 << 20));
    let mut ip = 0usize;
    let byte = |ip: &mut usize| -> Result<usize> {
        let b = *src.get(*ip).ok_or_else(|| short("lzo"))?;
        *ip += 1;
        Ok(b as usize)
    };
    let literals = |out: &mut Vec<u8>, ip: &mut usize, n: usize| -> Result<()> {
        let s = src.get(*ip..*ip + n).ok_or_else(|| short("lzo"))?;
        if out.len() + n > max {
            return Err(bad("lzo", "more data than a block holds"));
        }
        out.extend_from_slice(s);
        *ip += n;
        Ok(())
    };
    let copy = |out: &mut Vec<u8>, dist: usize, n: usize| -> Result<()> {
        if dist == 0 || dist > out.len() {
            return Err(bad("lzo", "a match before the start"));
        }
        if out.len() + n > max {
            return Err(bad("lzo", "more data than a block holds"));
        }
        let from = out.len() - dist;
        for k in 0..n {
            let b = out[from + k];
            out.push(b);
        }
        Ok(())
    };
    // A run length: `t` plus zero bytes (255 each) and a final byte.
    let extend = |ip: &mut usize, base: usize| -> Result<usize> {
        let mut zeros = 0;
        while *src.get(*ip).ok_or_else(|| short("lzo"))? == 0 {
            zeros += 1;
            *ip += 1;
        }
        let last = byte(ip)?;
        Ok(zeros * 255 + base + last)
    };

    let mut state: usize;
    if *src.first().ok_or_else(|| short("lzo"))? > 17 {
        let t = byte(&mut ip)? - 17;
        literals(&mut out, &mut ip, t)?;
        state = if t < 4 { t } else { 4 };
    } else {
        state = 0;
    }
    loop {
        let mut t = byte(&mut ip)?;
        let next;
        if t < 16 {
            if state == 0 {
                if t == 0 {
                    t = extend(&mut ip, 15)?;
                }
                literals(&mut out, &mut ip, t + 3)?;
                state = 4;
                continue;
            } else if state != 4 {
                next = t & 3;
                let dist = 1 + (t >> 2) + (byte(&mut ip)? << 2);
                copy(&mut out, dist, 2)?;
            } else {
                next = t & 3;
                let dist = 1 + 0x800 + (t >> 2) + (byte(&mut ip)? << 2);
                copy(&mut out, dist, 3)?;
            }
        } else if t >= 64 {
            next = t & 3;
            let dist = 1 + ((t >> 2) & 7) + (byte(&mut ip)? << 3);
            copy(&mut out, dist, (t >> 5) + 1)?;
        } else if t >= 32 {
            let mut n = (t & 31) + 2;
            if n == 2 {
                n = extend(&mut ip, 31)? + 2;
            }
            let v = byte(&mut ip)? | (byte(&mut ip)? << 8);
            next = v & 3;
            copy(&mut out, 1 + (v >> 2), n)?;
        } else {
            let high = (t & 8) << 11;
            let mut n = (t & 7) + 2;
            if n == 2 {
                n = extend(&mut ip, 7)? + 2;
            }
            let v = byte(&mut ip)? | (byte(&mut ip)? << 8);
            next = v & 3;
            let dist = high + (v >> 2);
            if dist == 0 {
                break; // the end marker
            }
            copy(&mut out, dist + 0x4000, n)?;
        }
        literals(&mut out, &mut ip, next)?;
        state = next;
    }
    Ok(out)
}
