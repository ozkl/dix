//! The zip compression methods besides deflate, as PKZIP defined them:
//!
//! * 1 **Shrink** (PKZIP 0.9/1.x): LZW, codes of 9 to 13 bits, with
//!   "partial clearing" of the codes no other code builds on;
//! * 2–5 **Reduce** (PKZIP 0.9): "follower sets" (each byte predicts the
//!   next) and then a simple LZ77 on top, factors 1–4;
//! * 6 **Implode** (PKZIP 1.x): LZ77 with a 4 or 8 KiB window and
//!   Shannon–Fano codes for literals, lengths and distances;
//! * 9 **Deflate64** (PKZIP 4): deflate with a 64 KiB window and longer
//!   matches.
//!
//! Each decodes a whole member (old archives hold small files). Written for
//! this project from PKWARE's APPNOTE.TXT.

use dix_core::{Error, Result};

fn bad(method: &str) -> Error {
    Error::corrupted(format!("damaged {method} data in a zip member"))
}

/// Bits, least significant first; zeros past the end.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    have: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0, buf: 0, have: 0 }
    }

    fn need(&mut self, n: u32) {
        while self.have < n {
            let b = self.data.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.buf |= (b as u64) << self.have;
            self.have += 8;
        }
    }

    fn peek(&mut self, n: u32) -> u32 {
        self.need(n);
        (self.buf & ((1u64 << n) - 1)) as u32
    }

    fn skip(&mut self, n: u32) {
        self.need(n);
        self.buf >>= n;
        self.have -= n;
    }

    fn get(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.skip(n);
        v
    }

    /// To the next byte boundary.
    fn align(&mut self) {
        let n = self.have % 8;
        self.skip(n);
    }

    /// Well past the end of the data: damaged.
    fn overrun(&self) -> bool {
        self.pos > self.data.len() + 16
    }
}

/// Copies `len` bytes from `dist` back; before the start of the output
/// there are zeros (the window starts out cleared).
fn copy_back(out: &mut Vec<u8>, dist: usize, len: usize) {
    for _ in 0..len {
        let b = if dist <= out.len() { out[out.len() - dist] } else { 0 };
        out.push(b);
    }
}

// ----- Shrink -------------------------------------------------------------------

/// Method 1.
pub fn unshrink(data: &[u8], size: usize) -> Result<Vec<u8>> {
    const MAX: usize = 1 << 13;
    const FREE: u16 = u16::MAX;
    let err = || bad("Shrink");
    let mut out = Vec::with_capacity(size);
    if size == 0 {
        return Ok(out);
    }
    // Each code's string is a piece of the output: (start, length) (a
    // literal is itself). A code's parent is the code it extends: partial
    // clearing frees the codes that are nobody's parent.
    let mut start = vec![0usize; MAX];
    let mut len = vec![1usize; MAX];
    let mut parent = vec![FREE; MAX];
    let mut b = Bits::new(data);
    let mut bits = 9;
    let mut next = 257usize;
    let first = b.get(bits) as usize;
    if first > 255 {
        return Err(err());
    }
    out.push(first as u8);
    let mut prev = first;
    while out.len() < size {
        let code = b.get(bits) as usize;
        if b.overrun() {
            return Err(err());
        }
        if code == 256 {
            match b.get(bits) {
                1 if bits < 13 => bits += 1,
                2 => {
                    let mut is_parent = vec![false; MAX];
                    for c in 257..MAX {
                        if parent[c] != FREE {
                            is_parent[parent[c] as usize] = true;
                        }
                    }
                    for c in 257..MAX {
                        if !is_parent[c] {
                            parent[c] = FREE;
                        }
                    }
                    next = (257..MAX).find(|&c| parent[c] == FREE).unwrap_or(MAX);
                }
                _ => return Err(err()),
            }
            continue;
        }
        // The previous string was just written, right before `here`: a new
        // code is it and the next byte, there in the output.
        let here = out.len();
        let (new_start, new_len) = (here - len[prev], len[prev] + 1);
        if code < 256 || (code > 256 && parent[code] != FREE) {
            if code < 256 {
                out.push(code as u8);
            } else {
                for k in 0..len[code] {
                    let c = out[start[code] + k];
                    out.push(c);
                }
            }
            if next < MAX {
                (parent[next], start[next], len[next]) = (prev as u16, new_start, new_len);
            }
        } else {
            // Not defined yet (KwKwK): the code being defined now, the
            // previous string and its own first byte.
            if code != next || next >= MAX {
                return Err(err());
            }
            (parent[next], start[next], len[next]) = (prev as u16, new_start, new_len);
            for k in 0..new_len {
                let c = out[new_start + k];
                out.push(c);
            }
        }
        next = (next + 1..MAX).find(|&c| parent[c] == FREE).unwrap_or(MAX);
        prev = code;
    }
    out.truncate(size);
    Ok(out)
}

// ----- Reduce -------------------------------------------------------------------

/// Methods 2–5 (`factor` 1–4).
pub fn unreduce(data: &[u8], size: usize, factor: u32) -> Result<Vec<u8>> {
    const DLE: u8 = 144;
    let mut b = Bits::new(data);
    let mut followers = vec![Vec::new(); 256];
    for j in (0..256).rev() {
        let n = b.get(6) as usize;
        if n > 32 {
            return Err(bad("Reduce"));
        }
        followers[j] = (0..n).map(|_| b.get(8) as u8).collect();
    }
    let need = |n: usize| match n {
        0..=2 => 1,
        3..=4 => 2,
        5..=8 => 3,
        9..=16 => 4,
        _ => 5,
    };
    let mask = 0xFFu32 >> factor;
    let mut out = Vec::with_capacity(size);
    let (mut last, mut state, mut v, mut length) = (0u8, 0, 0u32, 0usize);
    while out.len() < size {
        let set = &followers[last as usize];
        let c = if set.is_empty() || b.get(1) == 1 {
            b.get(8) as u8
        } else {
            *set.get(b.get(need(set.len())) as usize).ok_or_else(|| bad("Reduce"))?
        };
        if b.overrun() {
            return Err(bad("Reduce"));
        }
        last = c;
        match state {
            0 if c == DLE => state = 1,
            0 => out.push(c),
            1 if c == 0 => {
                out.push(DLE);
                state = 0;
            }
            1 => {
                v = c as u32;
                length = (v & mask) as usize;
                state = if length == mask as usize { 2 } else { 3 };
            }
            2 => {
                length += c as usize;
                state = 3;
            }
            _ => {
                let dist = (((v >> (8 - factor)) << 8) + c as u32 + 1) as usize;
                copy_back(&mut out, dist, length + 3);
                state = 0;
            }
        }
    }
    out.truncate(size);
    Ok(out)
}

// ----- Huffman codes, read from the LSB end -----------------------------------

/// A canonical prefix code read a bit at a time from an LSB-first stream
/// (the code's first bit first), optionally with every bit inverted
/// (Implode's Shannon–Fano codes are canonical codes with inverted bits).
struct Code {
    /// By the next `bits` bits: (symbol, length); length 0: no code.
    table: Vec<(u16, u8)>,
    bits: u32,
}

impl Code {
    fn new(lens: &[u8], inverted: bool) -> Option<Code> {
        let max = lens.iter().copied().max().unwrap_or(0) as u32;
        if max == 0 || max > 16 {
            return None;
        }
        let mut count = [0u32; 17];
        for &l in lens {
            count[l as usize] += 1;
        }
        count[0] = 0;
        let mut next = [0u32; 18];
        let mut code = 0;
        for l in 1..=16 {
            code = (code + count[l - 1]) << 1;
            next[l] = code;
        }
        let mut table = vec![(0u16, 0u8); 1 << max];
        for (sym, &l) in lens.iter().enumerate() {
            if l == 0 {
                continue;
            }
            let l = l as u32;
            let mut c = next[l as usize];
            next[l as usize] += 1;
            if c >= 1 << l {
                return None; // over-subscribed
            }
            if inverted {
                c = !c & ((1 << l) - 1);
            }
            // The first bit read is the code's top bit.
            let rev = c.reverse_bits() >> (32 - l);
            let mut i = rev as usize;
            while i < table.len() {
                table[i] = (sym as u16, l as u8);
                i += 1 << l;
            }
        }
        Some(Code { table, bits: max })
    }

    fn decode(&self, b: &mut Bits<'_>) -> Option<u16> {
        let (sym, len) = self.table[b.peek(self.bits) as usize];
        if len == 0 {
            return None;
        }
        b.skip(len as u32);
        Some(sym)
    }
}

// ----- Implode ------------------------------------------------------------------

/// Method 6. `flags`: the member's general purpose flags (bit 1: an 8 KiB
/// window; bit 2: literals are coded too).
pub fn explode(data: &[u8], size: usize, flags: u16) -> Result<Vec<u8>> {
    let err = || bad("Implode");
    let big = flags & 2 != 0;
    let literals = flags & 4 != 0;
    // The code lengths, run-length coded in whole bytes first.
    let mut at = 0;
    let mut tree = |n: usize| -> Result<Code> {
        let count = *data.get(at).ok_or_else(err)? as usize + 1;
        at += 1;
        let mut lens = Vec::with_capacity(n);
        for _ in 0..count {
            let x = *data.get(at).ok_or_else(err)?;
            at += 1;
            for _ in 0..(x >> 4) + 1 {
                lens.push((x & 15) + 1);
            }
        }
        if lens.len() != n {
            return Err(err());
        }
        Code::new(&lens, true).ok_or_else(err)
    };
    let lit = if literals { Some(tree(256)?) } else { None };
    let lens = tree(64)?;
    let dists = tree(64)?;
    let mut b = Bits::new(&data[at..]);
    let low_bits = if big { 7 } else { 6 };
    let min = if literals { 3 } else { 2 };
    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        if b.get(1) == 1 {
            let c = match &lit {
                Some(t) => t.decode(&mut b).ok_or_else(err)? as u8,
                None => b.get(8) as u8,
            };
            out.push(c);
        } else {
            let low = b.get(low_bits) as usize;
            let high = dists.decode(&mut b).ok_or_else(err)? as usize;
            let dist = (high << low_bits | low) + 1;
            let mut len = lens.decode(&mut b).ok_or_else(err)? as usize;
            if len == 63 {
                len += b.get(8) as usize;
            }
            copy_back(&mut out, dist, len + min);
        }
        if b.overrun() {
            return Err(err());
        }
    }
    out.truncate(size);
    Ok(out)
}

// ----- Deflate64 ----------------------------------------------------------------

const LEN_BASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 3];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 16];
const DIST_BASE: [u32; 32] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577, 32769, 49153,
];

/// Method 9: deflate with a 64 KiB window, two more distance codes and
/// length code 285 meaning 3 + 16 bits.
pub fn inflate64(data: &[u8], size: usize) -> Result<Vec<u8>> {
    let err = || bad("Deflate64");
    let mut b = Bits::new(data);
    let mut out = Vec::with_capacity(size);
    loop {
        let last = b.get(1);
        match b.get(2) {
            0 => {
                b.align();
                let n = b.get(16) as usize;
                let n_inv = b.get(16) as usize;
                if n != !n_inv & 0xFFFF {
                    return Err(err());
                }
                for _ in 0..n {
                    out.push(b.get(8) as u8);
                }
            }
            kind @ (1 | 2) => {
                let (lit, dist) = if kind == 1 {
                    let mut l = [8u8; 288];
                    l[144..256].fill(9);
                    l[256..280].fill(7);
                    (Code::new(&l, false).unwrap(), Code::new(&[5u8; 32], false).unwrap())
                } else {
                    let hlit = b.get(5) as usize + 257;
                    let hdist = b.get(5) as usize + 1;
                    let hclen = b.get(4) as usize + 4;
                    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                    let mut cl = [0u8; 19];
                    for &i in &ORDER[..hclen] {
                        cl[i] = b.get(3) as u8;
                    }
                    let cl = Code::new(&cl, false).ok_or_else(err)?;
                    let mut lens = Vec::with_capacity(hlit + hdist);
                    while lens.len() < hlit + hdist {
                        match cl.decode(&mut b).ok_or_else(err)? {
                            s @ 0..=15 => lens.push(s as u8),
                            16 => {
                                let prev = *lens.last().ok_or_else(err)?;
                                for _ in 0..3 + b.get(2) {
                                    lens.push(prev);
                                }
                            }
                            17 => lens.resize(lens.len() + 3 + b.get(3) as usize, 0),
                            _ => lens.resize(lens.len() + 11 + b.get(7) as usize, 0),
                        }
                        if b.overrun() {
                            return Err(err());
                        }
                    }
                    if lens.len() != hlit + hdist {
                        return Err(err());
                    }
                    let lit = Code::new(&lens[..hlit], false).ok_or_else(err)?;
                    // A block of literals only may have no distance codes.
                    let dist = Code::new(&lens[hlit..], false).unwrap_or(Code { table: vec![(0, 0)], bits: 0 });
                    (lit, dist)
                };
                loop {
                    let s = lit.decode(&mut b).ok_or_else(err)? as usize;
                    if b.overrun() {
                        return Err(err());
                    }
                    match s {
                        0..=255 => out.push(s as u8),
                        256 => break,
                        _ => {
                            let i = s - 257;
                            if i >= 29 {
                                return Err(err());
                            }
                            let len = LEN_BASE[i] as usize + b.get(LEN_EXTRA[i] as u32) as usize;
                            let d = dist.decode(&mut b).ok_or_else(err)? as usize;
                            let extra = if d < 4 { 0 } else { (d as u32 - 2) / 2 };
                            let dist = DIST_BASE[d] as usize + b.get(extra) as usize;
                            if dist > out.len() {
                                return Err(err());
                            }
                            copy_back(&mut out, dist, len);
                        }
                    }
                }
            }
            _ => return Err(err()),
        }
        if last == 1 || out.len() > size {
            break;
        }
    }
    if out.len() != size {
        return Err(err());
    }
    Ok(out)
}
