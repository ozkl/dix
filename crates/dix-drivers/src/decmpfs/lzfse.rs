//! LZFSE, Apple's LZ77 + finite state entropy (FSE) coder.
//!
//! A stream of blocks, each starting with a 4-byte magic:
//!
//! * `bvx-`: stored bytes; `bvxn`: an LZVN block (see [`super::lzvn`]);
//! * `bvx1` / `bvx2`: an LZFSE block, its frequency tables plain (v1) or
//!   packed in variable-length codes (v2). The block holds the literals,
//!   four interleaved FSE streams, and the (literal count, match length,
//!   distance) triples, three FSE value streams with extra bits. Both
//!   bit streams are read backwards, from their end;
//! * `bvx$`: the end.

use dix_core::{Error, Result};

fn bad() -> Error {
    Error::corrupted("LZFSE data is damaged")
}

const L_SYMBOLS: usize = 20;
const M_SYMBOLS: usize = 20;
const D_SYMBOLS: usize = 64;
const LITERAL_SYMBOLS: usize = 256;
const L_STATES: usize = 64;
const M_STATES: usize = 64;
const D_STATES: usize = 256;
const LITERAL_STATES: usize = 1024;

const L_EXTRA: [u8; 20] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 3, 5, 8];
const L_BASE: [i32; 20] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 20, 28, 60];
const M_EXTRA: [u8; 20] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 5, 8, 11];
const M_BASE: [i32; 20] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 24, 56, 312];

/// Distances: groups of four codes with 0, 1, …, 15 extra bits.
fn d_tables() -> ([u8; 64], [i32; 64]) {
    let (mut extra, mut base) = ([0u8; 64], [0i32; 64]);
    let mut start = 0i32;
    for g in 0..16 {
        for k in 0..4 {
            extra[g * 4 + k] = g as u8;
            base[g * 4 + k] = start + ((k as i32) << g);
        }
        start += 4 << g;
    }
    (extra, base)
}

/// The backward bit stream: bits are taken from the end of the data.
struct BitIn<'a> {
    data: &'a [u8],
    /// Bytes before this are not read yet.
    pos: usize,
    accum: u64,
    bits: u32,
}

impl<'a> BitIn<'a> {
    /// `data` ends where the stream ends; `n` (−7…0) says how many bits of
    /// the last byte are not used.
    fn new(data: &'a [u8], n: i32) -> Result<Self> {
        let (take, bits) = if n != 0 { (8, 64 + n) } else { (7, 56) };
        if !(56..64).contains(&bits) {
            return Err(bad());
        }
        let mut s = Self { data, pos: data.len(), accum: 0, bits: 0 };
        s.accum = s.take(take);
        s.bits = bits as u32;
        if s.accum >> bits != 0 {
            return Err(bad());
        }
        Ok(s)
    }

    /// The `n` bytes before `pos`, little-endian. Like Apple's decoder,
    /// which tops up from whatever lies before the stream (bits a valid
    /// stream never uses), bytes before the data read as zeros.
    fn take(&mut self, n: usize) -> u64 {
        let have = n.min(self.pos);
        let mut b = [0u8; 8];
        b[n - have..n].copy_from_slice(&self.data[self.pos - have..self.pos]);
        self.pos -= have;
        u64::from_le_bytes(b)
    }

    /// Refills to at least 56 bits.
    fn flush(&mut self) -> Result<()> {
        let nbits = (63 - self.bits) & !7;
        if nbits == 0 {
            return Ok(());
        }
        let v = self.take((nbits / 8) as usize);
        self.accum = (self.accum << nbits) | v;
        self.bits += nbits;
        Ok(())
    }

    fn pull(&mut self, n: u32) -> Result<u64> {
        if n > self.bits {
            return Err(bad());
        }
        self.bits -= n;
        let v = self.accum >> self.bits;
        self.accum &= if self.bits == 0 { 0 } else { (1u64 << self.bits) - 1 };
        Ok(v)
    }
}

#[derive(Clone, Copy, Default)]
struct Entry {
    k: u8,
    symbol: u8,
    delta: i16,
}

fn literal_table(freq: &[u16], states: usize) -> Result<Vec<Entry>> {
    let mut t = Vec::with_capacity(states);
    let clz_n = (states as u32).leading_zeros() as i32;
    let mut sum = 0usize;
    for (sym, &f) in freq.iter().enumerate() {
        let f = f as i32;
        if f == 0 {
            continue;
        }
        sum += f as usize;
        if sum > states {
            return Err(bad());
        }
        let k = (f as u32).leading_zeros() as i32 - clz_n;
        let j0 = ((2 * states as i32) >> k) - f;
        for j in 0..f {
            t.push(if j < j0 {
                Entry { k: k as u8, symbol: sym as u8, delta: (((f + j) << k) - states as i32) as i16 }
            } else {
                Entry { k: (k - 1) as u8, symbol: sym as u8, delta: ((j - j0) << (k - 1)) as i16 }
            });
        }
    }
    t.resize(states, Entry::default());
    Ok(t)
}

#[derive(Clone, Copy, Default)]
struct ValueEntry {
    total: u8,
    vbits: u8,
    delta: i16,
    base: i32,
}

fn value_table(freq: &[u16], states: usize, extra: &[u8], base: &[i32]) -> Result<Vec<ValueEntry>> {
    let mut t = Vec::with_capacity(states);
    let clz_n = (states as u32).leading_zeros() as i32;
    let mut sum = 0usize;
    for (sym, &f) in freq.iter().enumerate() {
        let f = f as i32;
        if f == 0 {
            continue;
        }
        sum += f as usize;
        if sum > states {
            return Err(bad());
        }
        let k = (f as u32).leading_zeros() as i32 - clz_n;
        let j0 = ((2 * states as i32) >> k) - f;
        for j in 0..f {
            let (bits, delta) = if j < j0 { (k, ((f + j) << k) - states as i32) } else { (k - 1, (j - j0) << (k - 1)) };
            t.push(ValueEntry {
                total: bits as u8 + extra[sym],
                vbits: extra[sym],
                delta: delta as i16,
                base: base[sym],
            });
        }
    }
    t.resize(states, ValueEntry::default());
    Ok(t)
}

fn value(state: &mut usize, t: &[ValueEntry], input: &mut BitIn) -> Result<i32> {
    let e = *t.get(*state).ok_or_else(bad)?;
    let v = input.pull(e.total as u32)? as u32;
    *state = (e.delta as i32 + (v >> e.vbits) as i32) as usize;
    Ok(e.base + (v & ((1u32 << e.vbits) - 1)) as i32)
}

/// Variable-length frequency codes of a v2 header.
fn freq_value(bits: u32) -> (u16, u32) {
    const NBITS: [u8; 32] =
        [2, 3, 2, 5, 2, 3, 2, 8, 2, 3, 2, 5, 2, 3, 2, 14, 2, 3, 2, 5, 2, 3, 2, 8, 2, 3, 2, 5, 2, 3, 2, 14];
    const VALUE: [i8; 32] =
        [0, 2, 1, 4, 0, 3, 1, -1, 0, 2, 1, 5, 0, 3, 1, -1, 0, 2, 1, 6, 0, 3, 1, -1, 0, 2, 1, 7, 0, 3, 1, -1];
    let b = (bits & 31) as usize;
    match NBITS[b] {
        8 => (8 + ((bits >> 4) & 0xF) as u16, 8),
        14 => (24 + ((bits >> 4) & 0x3FF) as u16, 14),
        n => (VALUE[b] as u16, n as u32),
    }
}

struct Block {
    n_raw: usize,
    n_literals: usize,
    n_matches: usize,
    n_lit_payload: usize,
    n_lmd_payload: usize,
    literal_bits: i32,
    literal_state: [u16; 4],
    lmd_bits: i32,
    l_state: u16,
    m_state: u16,
    d_state: u16,
    freq: Vec<u16>,
    /// Where the payload starts, from the block's start.
    header: usize,
}

fn le32(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(b.get(at..at + 4).ok_or_else(bad)?.try_into().unwrap()))
}
fn le64(b: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(b.get(at..at + 8).ok_or_else(bad)?.try_into().unwrap()))
}
fn le16(b: &[u8], at: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(b.get(at..at + 2).ok_or_else(bad)?.try_into().unwrap()))
}

/// A v1 header: the counts, then the 360 frequencies as plain 16-bit
/// numbers (770 bytes, 772 with the C struct's padding).
fn header_v1(b: &[u8]) -> Result<Block> {
    Ok(Block {
        n_raw: le32(b, 4)? as usize,
        n_literals: le32(b, 12)? as usize,
        n_matches: le32(b, 16)? as usize,
        n_lit_payload: le32(b, 20)? as usize,
        n_lmd_payload: le32(b, 24)? as usize,
        literal_bits: le32(b, 28)? as i32,
        literal_state: [le16(b, 32)?, le16(b, 34)?, le16(b, 36)?, le16(b, 38)?],
        lmd_bits: le32(b, 40)? as i32,
        l_state: le16(b, 44)?,
        m_state: le16(b, 46)?,
        d_state: le16(b, 48)?,
        freq: (0..360).map(|i| le16(b, 50 + i * 2)).collect::<Result<_>>()?,
        header: 772,
    })
}

fn header_v2(b: &[u8]) -> Result<Block> {
    let f = |v: u64, off: u32, n: u32| (v >> off) & ((1u64 << n) - 1);
    let (v0, v1, v2) = (le64(b, 8)?, le64(b, 16)?, le64(b, 24)?);
    let header = f(v2, 0, 32) as usize;
    if header < 32 || header > b.len() {
        return Err(bad());
    }
    let mut freq = vec![0u16; 360];
    if header > 32 {
        let src = &b[32..header];
        let (mut accum, mut nbits, mut at) = (0u32, 0u32, 0usize);
        for slot in freq.iter_mut() {
            while nbits < 14 && at < src.len() {
                accum |= (src[at] as u32) << nbits;
                nbits += 8;
                at += 1;
            }
            let (v, n) = freq_value(accum);
            if n > nbits {
                return Err(bad());
            }
            accum >>= n;
            nbits -= n;
            *slot = v;
        }
    }
    Ok(Block {
        n_raw: le32(b, 4)? as usize,
        n_literals: f(v0, 0, 20) as usize,
        n_lit_payload: f(v0, 20, 20) as usize,
        n_matches: f(v0, 40, 20) as usize,
        literal_bits: f(v0, 60, 3) as i32 - 7,
        literal_state: [f(v1, 0, 10) as u16, f(v1, 10, 10) as u16, f(v1, 20, 10) as u16, f(v1, 30, 10) as u16],
        n_lmd_payload: f(v1, 40, 20) as usize,
        lmd_bits: f(v1, 60, 3) as i32 - 7,
        l_state: f(v2, 32, 10) as u16,
        m_state: f(v2, 42, 10) as u16,
        d_state: f(v2, 52, 10) as u16,
        freq,
        header,
    })
}

fn decode_block(blk: &Block, payload: &[u8], out: &mut Vec<u8>, limit: usize, start: usize) -> Result<()> {
    if blk.n_literals > 40_000 + 64 || blk.n_matches > 10_000 || payload.len() < blk.n_lit_payload + blk.n_lmd_payload {
        return Err(bad());
    }
    let (l_freq, rest) = blk.freq.split_at(L_SYMBOLS);
    let (m_freq, rest) = rest.split_at(M_SYMBOLS);
    let (d_freq, lit_freq) = rest.split_at(D_SYMBOLS);
    debug_assert_eq!(lit_freq.len(), LITERAL_SYMBOLS);

    // The literals.
    let lit_table = literal_table(lit_freq, LITERAL_STATES)?;
    let mut literals = vec![0u8; blk.n_literals.div_ceil(4) * 4];
    let mut input = BitIn::new(&payload[..blk.n_lit_payload], blk.literal_bits)?;
    let mut st = blk.literal_state.map(|s| s as usize);
    for chunk in literals.chunks_mut(4) {
        input.flush()?;
        for (k, b) in chunk.iter_mut().enumerate() {
            let e = *lit_table.get(st[k]).ok_or_else(bad)?;
            *b = e.symbol;
            st[k] = (e.delta as i32 + input.pull(e.k as u32)? as i32) as usize;
        }
    }

    // The matches.
    let (d_extra, d_base) = d_tables();
    let lt = value_table(l_freq, L_STATES, &L_EXTRA, &L_BASE)?;
    let mt = value_table(m_freq, M_STATES, &M_EXTRA, &M_BASE)?;
    let dt = value_table(d_freq, D_STATES, &d_extra, &d_base)?;
    let lmd = &payload[blk.n_lit_payload..blk.n_lit_payload + blk.n_lmd_payload];
    let mut input = BitIn::new(lmd, blk.lmd_bits)?;
    let (mut ls, mut ms, mut ds) = (blk.l_state as usize, blk.m_state as usize, blk.d_state as usize);
    let mut lit = 0usize;
    let mut d: i64 = -1;
    let block_start = out.len();
    for _ in 0..blk.n_matches {
        input.flush()?;
        let l = value(&mut ls, &lt, &mut input)? as usize;
        let m = value(&mut ms, &mt, &mut input)? as usize;
        let nd = value(&mut ds, &dt, &mut input)?;
        if nd != 0 {
            d = nd as i64;
        }
        if lit + l > blk.n_literals || out.len() - start + l + m > limit {
            return Err(bad());
        }
        out.extend_from_slice(&literals[lit..lit + l]);
        lit += l;
        if m > 0 {
            if d <= 0 || d as usize > out.len() - start {
                return Err(bad());
            }
            let from = out.len() - d as usize;
            for k in 0..m {
                let b = out[from + k];
                out.push(b);
            }
        }
    }
    if out.len() - block_start != blk.n_raw {
        return Err(bad());
    }
    Ok(())
}

/// Decodes a whole LZFSE stream into `out` (appended), at most `limit` bytes.
pub fn decode(src: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<()> {
    let start = out.len();
    let mut at = 0usize;
    loop {
        let b = &src[at.min(src.len())..];
        match b.get(..4) {
            Some(b"bvx$") => return Ok(()),
            Some(b"bvx-") => {
                let n = le32(b, 4)? as usize;
                let data = b.get(8..8 + n).ok_or_else(bad)?;
                if out.len() - start + n > limit {
                    return Err(bad());
                }
                out.extend_from_slice(data);
                at += 8 + n;
            }
            Some(b"bvxn") => {
                let (n_raw, n_payload) = (le32(b, 4)? as usize, le32(b, 8)? as usize);
                let payload = b.get(12..12 + n_payload).ok_or_else(bad)?;
                let before = out.len();
                super::lzvn::decode(payload, out, limit - (out.len() - start))?;
                if out.len() - before != n_raw {
                    return Err(bad());
                }
                at += 12 + n_payload;
            }
            Some(b"bvx1") | Some(b"bvx2") => {
                let blk = if b[3] == b'1' { header_v1(b)? } else { header_v2(b)? };
                let payload = b.get(blk.header..).ok_or_else(bad)?;
                decode_block(&blk, payload, out, limit, start)?;
                at += blk.header + blk.n_lit_payload + blk.n_lmd_payload;
            }
            // A stream may also just end.
            None if at >= src.len() && at > 0 => return Ok(()),
            _ => return Err(bad()),
        }
    }
}
