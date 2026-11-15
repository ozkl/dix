//! LZX decompression, as Microsoft Cabinet files use it (window of 2^15 to
//! 2^21 bytes).
//!
//! The input is 16-bit little-endian words read most significant bit
//! first. The output comes in 32 KiB frames; after each frame the input is
//! aligned to a word again. A stream starts with one bit (x86 call
//! translation on) and, if set, the 32-bit size translation assumes. Then
//! blocks: a type (verbatim, aligned offset, uncompressed) and an output
//! size; compressed blocks carry Huffman code lengths for the main tree
//! (literals, then length/position slot pairs), the length tree and, for
//! aligned blocks, the aligned-offset tree, each coded against the lengths
//! before through a pretree. Matches use the three most recent offsets
//! (R0, R1, R2) or a new one from a position slot and extra bits.
//! Afterwards, in each frame, E8 bytes (x86 CALL) are followed by absolute
//! addresses that are turned back into relative ones.

use dix_core::{Error, Result};

fn bad(what: impl std::fmt::Display) -> Error {
    Error::corrupted(format!("LZX data: {what}"))
}

const FRAME: usize = 32768;
const MAIN_LITERALS: usize = 256;
const LENGTHS: usize = 249;
const PRETREE: usize = 20;
const ALIGNED: usize = 8;
const MIN_MATCH: usize = 2;

/// Bits read most significant first from 16-bit little-endian words.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize, // in bits
}

impl Bits<'_> {
    fn word(&self, i: usize) -> u64 {
        match self.data.get(i * 2..i * 2 + 2) {
            Some(w) => u16::from_le_bytes([w[0], w[1]]) as u64,
            None => 0,
        }
    }

    /// The next 32 bits (without using them).
    fn peek(&self) -> u32 {
        let w = self.pos / 16;
        let v = (self.word(w) << 32) | (self.word(w + 1) << 16) | self.word(w + 2);
        ((v << (self.pos % 16)) >> 16) as u32
    }

    fn read(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        if self.pos + n as usize > self.data.len() * 8 + 32 {
            return Err(bad("it ends early"));
        }
        let v = self.peek() >> (32 - n);
        self.pos += n as usize;
        Ok(v)
    }

    /// To the next word, if not at one.
    fn align(&mut self) {
        self.pos = self.pos.next_multiple_of(16);
    }
}

/// A canonical Huffman code (MSB first), from code lengths.
struct Tree {
    /// For each length: the first code, how many codes, where its symbols
    /// start in `syms`.
    first: [u32; 17],
    count: [u32; 17],
    start: [u32; 17],
    syms: Vec<u16>,
}

impl Tree {
    fn new(lens: &[u8]) -> Result<Tree> {
        let mut count = [0u32; 17];
        for &l in lens {
            if l > 16 {
                return Err(bad("a code is too long"));
            }
            count[l as usize] += 1;
        }
        count[0] = 0;
        let (mut first, mut start) = ([0u32; 17], [0u32; 17]);
        let (mut code, mut at) = (0u32, 0u32);
        for len in 1..=16 {
            code = (code + count[len - 1]) << 1;
            first[len] = code;
            start[len] = at;
            at += count[len];
        }
        // Too many codes for their lengths.
        if (1..=16).any(|l| first[l] + count[l] > 1 << l) {
            return Err(bad("an over-full code"));
        }
        let mut syms = vec![0u16; at as usize];
        let mut fill = start;
        for (s, &l) in lens.iter().enumerate() {
            if l > 0 {
                syms[fill[l as usize] as usize] = s as u16;
                fill[l as usize] += 1;
            }
        }
        Ok(Tree { first, count, start, syms })
    }

    fn decode(&self, b: &mut Bits<'_>) -> Result<usize> {
        let p = b.peek();
        for len in 1..=16u32 {
            let code = p >> (32 - len);
            let i = code.wrapping_sub(self.first[len as usize]);
            if i < self.count[len as usize] {
                b.pos += len as usize;
                return Ok(self.syms[(self.start[len as usize] + i) as usize] as usize);
            }
        }
        Err(bad("an invalid code"))
    }
}

/// Code lengths `lens[from..to]`, coded against what they were, through a
/// pretree of 20 four-bit lengths read first.
fn read_lengths(b: &mut Bits<'_>, lens: &mut [u8], from: usize, to: usize) -> Result<()> {
    let mut pre = [0u8; PRETREE];
    for p in pre.iter_mut() {
        *p = b.read(4)? as u8;
    }
    let pre = Tree::new(&pre)?;
    let mut x = from;
    while x < to {
        let z = pre.decode(b)?;
        match z {
            17 => {
                let n = b.read(4)? as usize + 4;
                lens.get_mut(x..x + n).ok_or_else(|| bad("too many code lengths"))?.fill(0);
                x += n;
            }
            18 => {
                let n = b.read(5)? as usize + 20;
                lens.get_mut(x..x + n).ok_or_else(|| bad("too many code lengths"))?.fill(0);
                x += n;
            }
            19 => {
                let n = b.read(1)? as usize + 4;
                let z = pre.decode(b)?;
                let run = lens.get_mut(x..x + n).ok_or_else(|| bad("too many code lengths"))?;
                let v = ((run[0] as usize + 17 - z) % 17) as u8;
                run.fill(v);
                x += n;
            }
            z => {
                lens[x] = ((lens[x] as usize + 17 - z) % 17) as u8;
                x += 1;
            }
        }
    }
    Ok(())
}

/// The position slots of a window of 2^`bits` bytes.
fn position_slots(bits: u32) -> usize {
    match bits {
        15 => 30,
        16 => 32,
        17 => 34,
        18 => 36,
        19 => 38,
        20 => 42,
        _ => 50,
    }
}

/// Decompresses one LZX stream (a Cabinet folder's data blocks, put one
/// after another) into `size` bytes, with a window of 2^`window_bits`.
pub fn decompress(input: &[u8], window_bits: u32, size: usize) -> Result<Vec<u8>> {
    if !(15..=21).contains(&window_bits) {
        return Err(bad(format!("a window of 2^{window_bits} bytes")));
    }
    let slots = position_slots(window_bits);
    let mut extra = vec![0u32; slots];
    let mut base = vec![0u32; slots];
    for i in 0..slots {
        extra[i] = if i < 4 { 0 } else { ((i as u32 - 2) / 2).min(17) };
        if i > 0 {
            base[i] = base[i - 1] + (1 << extra[i - 1]);
        }
    }
    let mut b = Bits { data: input, pos: 0 };
    let translate = b.read(1)? == 1;
    let translation_size = if translate { (b.read(16)? << 16) | b.read(16)? } else { 0 };
    let mut out: Vec<u8> = Vec::with_capacity(size);
    let mut main_lens = vec![0u8; MAIN_LITERALS + slots * 8];
    let mut length_lens = vec![0u8; LENGTHS];
    let mut r = [1usize; 3];
    let mut next_frame = FRAME;
    while out.len() < size {
        // A block.
        let kind = b.read(3)?;
        let block = ((b.read(16)? << 8) | b.read(8)?) as usize;
        let block_end = out.len() + block;
        let mut aligned_tree = None;
        match kind {
            1 | 2 => {
                if kind == 2 {
                    let mut lens = [0u8; ALIGNED];
                    for l in lens.iter_mut() {
                        *l = b.read(3)? as u8;
                    }
                    aligned_tree = Some(Tree::new(&lens)?);
                }
                read_lengths(&mut b, &mut main_lens, 0, MAIN_LITERALS)?;
                read_lengths(&mut b, &mut main_lens, MAIN_LITERALS, MAIN_LITERALS + slots * 8)?;
                read_lengths(&mut b, &mut length_lens, 0, LENGTHS)?;
                let main = Tree::new(&main_lens)?;
                let lengths = if length_lens.iter().any(|&l| l > 0) { Some(Tree::new(&length_lens)?) } else { None };
                while out.len() < block_end && out.len() < size {
                    let m = main.decode(&mut b)?;
                    if m < MAIN_LITERALS {
                        out.push(m as u8);
                    } else {
                        let m = m - MAIN_LITERALS;
                        let mut len = m & 7;
                        if len == 7 {
                            len += lengths
                                .as_ref()
                                .ok_or_else(|| bad("a long match without a length tree"))?
                                .decode(&mut b)?;
                        }
                        len += MIN_MATCH;
                        let slot = m >> 3;
                        let offset = match slot {
                            0 => r[0],
                            1 => {
                                r.swap(0, 1);
                                r[0]
                            }
                            2 => {
                                r.swap(0, 2);
                                r[0]
                            }
                            s => {
                                let e = extra[s];
                                let mut o = base[s] as usize - 2;
                                match &aligned_tree {
                                    Some(t) if e >= 3 => {
                                        o += (b.read(e - 3)? as usize) << 3;
                                        o += t.decode(&mut b)?;
                                    }
                                    _ => o += b.read(e)? as usize,
                                }
                                r[2] = r[1];
                                r[1] = r[0];
                                r[0] = o;
                                o
                            }
                        };
                        if offset == 0 || offset > out.len() {
                            return Err(bad("a match before the start"));
                        }
                        let from = out.len() - offset;
                        for k in 0..len {
                            let c = out[from + k];
                            out.push(c);
                        }
                    }
                    if out.len() >= next_frame {
                        b.align();
                        next_frame += FRAME;
                    }
                }
            }
            3 => {
                // Padding to a word (a whole word if aligned), R0..R2,
                // then the bytes as they are (and a pad byte if odd).
                b.pos = (b.pos / 16 + 1) * 16;
                let mut at = b.pos / 8;
                for v in r.iter_mut() {
                    let w = input.get(at..at + 4).ok_or_else(|| bad("an uncompressed block is cut"))?;
                    *v = u32::from_le_bytes(w.try_into().unwrap()) as usize;
                    at += 4;
                }
                let n = block.min(size - out.len());
                let raw = input.get(at..at + n).ok_or_else(|| bad("an uncompressed block is cut"))?;
                out.extend_from_slice(raw);
                at += block + (block & 1);
                b.pos = at * 8;
                while out.len() >= next_frame {
                    next_frame += FRAME;
                }
            }
            k => return Err(bad(format!("unknown block type {k}"))),
        }
    }
    out.truncate(size);
    if translate {
        untranslate(&mut out, translation_size as i64);
    }
    Ok(out)
}

/// Undoes the x86 CALL translation in each 32 KiB frame (in the first
/// 32768 frames, of more than 10 bytes, up to its last 10).
fn untranslate(out: &mut [u8], file_size: i64) {
    for (f, frame) in out.chunks_mut(FRAME).enumerate().take(32768) {
        if frame.len() <= 10 {
            continue;
        }
        let end = frame.len() - 10;
        let mut i = 0;
        while i < end {
            if frame[i] != 0xE8 {
                i += 1;
                continue;
            }
            let cur = (f * FRAME + i) as i64;
            let abs = i32::from_le_bytes(frame[i + 1..i + 5].try_into().unwrap()) as i64;
            if abs >= -cur && abs < file_size {
                let rel = if abs >= 0 { abs - cur } else { abs + file_size };
                frame[i + 1..i + 5].copy_from_slice(&(rel as i32).to_le_bytes());
            }
            i += 5;
        }
    }
}
