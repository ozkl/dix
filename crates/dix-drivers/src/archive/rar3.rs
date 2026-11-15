//! RAR 2.9 decompression ("RAR 3" and "RAR 4" archives: what WinRAR 3.x
//! and 4.x wrote, unpack version 29).
//!
//! * **LZ** blocks: Huffman codes for literals and lengths (299), distance
//!   slots (60), the low 4 bits of long distances (17) and repeated
//!   distances (28); the code lengths are sent as differences to the
//!   previous table. Repeats of the last four distances and of the last
//!   match, short matches of 2.
//! * **PPM** blocks: PPMd variant H (see `ppmd`) with RAR's range coder; an
//!   escape byte introduces end of file, new tables, filters and matches.
//! * **Filters** ("RarVM" programs): RAR only ever wrote six standard ones,
//!   and unRAR recognises them by the length and CRC of their code instead
//!   of running it; so does this: x86 calls (E8, E8E9), Itanium branches,
//!   delta, RGB images and audio.
//!
//! Written for this project from the format as unRAR and libarchive
//! implement it.

use super::super::ppmd::{ByteSource, Model, RarCoder, RarDec};
use super::{BitIn, Entry, Huff, Key, Raw, bad};
use dix_core::{Error, Result};
use std::collections::VecDeque;

const NC: usize = 299;
const DC: usize = 60;
const LDC: usize = 17;
const RC: usize = 28;
const BC: usize = 20;
const TABLE: usize = NC + DC + LDC + RC;

const LDECODE: [u32; 28] =
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224];
const LBITS: [u32; 28] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5];
const SDDECODE: [u32; 8] = [0, 4, 8, 16, 32, 64, 128, 192];
const SDBITS: [u32; 8] = [2, 2, 3, 4, 5, 6, 6, 6];

/// Distance slots: (base, extra bits).
fn dist_slots() -> [(u32, u32); DC] {
    // Slots per number of extra bits: 4 with 0, 2 each for 1..=15, 14
    // with 16, none with 17, 12 with 18.
    let counts = [4, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 14, 0, 12];
    let mut t = [(0u32, 0u32); DC];
    let (mut dist, mut slot) = (0u32, 0usize);
    for (bits, &n) in counts.iter().enumerate() {
        for _ in 0..n {
            t[slot] = (dist, bits as u32);
            dist += 1 << bits;
            slot += 1;
        }
    }
    t
}

/// The standard filters, by the length and CRC of their VM code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Std {
    E8,
    E8E9,
    Itanium,
    Delta,
    Rgb,
    Audio,
}

pub(crate) const STANDARD: [(usize, u32, Std); 6] = [
    (53, 0xAD57_6887, Std::E8),
    (57, 0x3CD7_E57E, Std::E8E9),
    (120, 0x3769_893F, Std::Itanium),
    (29, 0x0E06_077D, Std::Delta),
    (149, 0x1C2C_5DC8, Std::Rgb),
    (216, 0xBC85_E701, Std::Audio),
];

#[derive(Debug, Clone)]
struct FilterDef {
    kind: Option<Std>,
    last_len: u32,
    exec: u32,
}

#[derive(Debug, Clone)]
struct Pending {
    start: u64,
    len: u64,
    kind: Option<Std>,
    regs: [u32; 7],
}

/// The VM's memory size: a filter block is at most this long.
const VM_MEMSIZE: usize = 0x40000;

/// Bits of a filter definition (most significant first; zeros past the end).
struct CodeBits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl CodeBits<'_> {
    fn get(&self) -> u32 {
        let byte = |i: usize| self.b.get(i).copied().unwrap_or(0) as u32;
        let i = self.pos / 8;
        let v = (byte(i) << 16) | (byte(i + 1) << 8) | byte(i + 2);
        (v >> (8 - self.pos % 8)) & 0xFFFF
    }

    fn add(&mut self, n: usize) {
        self.pos += n;
    }

    /// A VM number: 4, 8 (or negative 8), 16 or 32 bits.
    fn number(&mut self) -> u32 {
        let d = self.get();
        match d & 0xC000 {
            0 => {
                self.add(6);
                (d >> 10) & 0xF
            }
            0x4000 => {
                if d & 0x3C00 == 0 {
                    self.add(14);
                    0xFFFF_FF00 | ((d >> 2) & 0xFF)
                } else {
                    self.add(10);
                    (d >> 6) & 0xFF
                }
            }
            0x8000 => {
                self.add(2);
                let v = self.get();
                self.add(16);
                v
            }
            _ => {
                self.add(2);
                let hi = self.get();
                self.add(16);
                let lo = self.get();
                self.add(16);
                (hi << 16) | lo
            }
        }
    }
}

impl ByteSource for BitIn {
    fn next_byte(&mut self) -> u8 {
        let b = self.buf.get(self.addr).copied().unwrap_or(0);
        self.addr += 1;
        b
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Unpack29 {
    win: Vec<u8>,
    wr: u64,
    flushed: u64,
    file_start: u64,
    left: u64,
    crc: Option<u32>,
    crc_now: u32,
    inp: BitIn,
    tables_read: bool,
    old_table: [u8; TABLE],
    ld: Huff,
    dd: Huff,
    ldd: Huff,
    rd: Huff,
    dist: [(u32, u32); DC],
    old_dist: [u64; 4],
    last_len: u64,
    prev_low: u64,
    low_rep: u32,
    ppm_mode: bool,
    ppm: Option<Box<Model>>,
    coder: RarCoder,
    esc: u8,
    defs: Vec<FilterDef>,
    last_filter: usize,
    pending: VecDeque<Pending>,
    /// The member's end mark has been read.
    file_end: bool,
}

/// Room kept free in the window (the longest match and then some).
const MARGIN: u64 = 0x1000;

impl Unpack29 {
    pub fn new(dict: u64) -> Unpack29 {
        let size = dict.clamp(4 << 20, 1 << 30).next_power_of_two() as usize;
        Unpack29 {
            win: vec![0; size],
            wr: 0,
            flushed: 0,
            file_start: 0,
            left: 0,
            crc: None,
            crc_now: 0,
            inp: BitIn::default(),
            tables_read: false,
            old_table: [0; TABLE],
            ld: Huff::default(),
            dd: Huff::default(),
            ldd: Huff::default(),
            rd: Huff::default(),
            dist: dist_slots(),
            old_dist: [0; 4],
            last_len: 0,
            prev_low: 0,
            low_rep: 0,
            ppm_mode: false,
            ppm: None,
            coder: RarCoder::default(),
            esc: 2,
            defs: Vec::new(),
            last_filter: 0,
            pending: VecDeque::new(),
            file_end: true,
        }
    }

    pub fn finished(&self) -> bool {
        self.left == 0
    }

    /// Starts a member: the first of a run, or the next of a solid one
    /// (the window, tables, model and filters go on).
    pub fn begin(&mut self, e: &Entry, raw: &mut Raw<'_>, key: Option<Key>) -> Result<()> {
        let cipher = key.map(|k| (k.aes, k.len, k.iv));
        self.inp = BitIn { len: e.packed(), segs: e.segs.clone(), cipher, ..Default::default() };
        self.pending.clear();
        self.flushed = self.wr;
        self.file_start = self.wr;
        self.left = e.size;
        self.crc = e.crc;
        self.crc_now = 0;
        self.file_end = false;
        if e.size == 0 {
            self.file_end = true;
            return Ok(());
        }
        self.inp.refill(raw)?;
        // New tables unless the last member's end said to keep them; a PPM
        // block is always started anew.
        if !self.tables_read || self.ppm_mode {
            self.read_tables()?;
        }
        Ok(())
    }

    fn read_tables(&mut self) -> Result<()> {
        // Byte aligned.
        if self.inp.bit != 0 {
            let n = 8 - self.inp.bit;
            self.inp.add(n);
        }
        let field = self.inp.get16();
        if field & 0x8000 != 0 {
            return self.ppm_init();
        }
        self.ppm_mode = false;
        self.prev_low = 0;
        self.low_rep = 0;
        if field & 0x4000 == 0 {
            self.old_table = [0; TABLE];
        }
        self.inp.add(2);
        let mut bl = [0u8; BC];
        let mut i = 0;
        while i < BC {
            let len = self.inp.bits(4) as u8;
            if len == 15 {
                let zeros = self.inp.bits(4) as usize;
                if zeros == 0 {
                    bl[i] = 15;
                    i += 1;
                } else {
                    for _ in 0..zeros + 2 {
                        if i < BC {
                            bl[i] = 0;
                            i += 1;
                        }
                    }
                }
            } else {
                bl[i] = len;
                i += 1;
            }
        }
        let bd = Huff::new(&bl, 7);
        let mut table = [0u8; TABLE];
        let mut i = 0;
        while i < TABLE {
            let n = bd.decode(&mut self.inp);
            if n < 16 {
                table[i] = (n as u8 + self.old_table[i]) & 0xF;
                i += 1;
            } else if n < 18 {
                let count = if n == 16 { self.inp.bits(3) + 3 } else { self.inp.bits(7) + 11 };
                if i == 0 {
                    return Err(bad("a RAR 3 table repeats nothing"));
                }
                for _ in 0..count {
                    if i < TABLE {
                        table[i] = table[i - 1];
                        i += 1;
                    }
                }
            } else {
                let count = if n == 18 { self.inp.bits(3) + 3 } else { self.inp.bits(7) + 11 };
                for _ in 0..count {
                    if i < TABLE {
                        table[i] = 0;
                        i += 1;
                    }
                }
            }
        }
        self.tables_read = true;
        self.ld = Huff::new(&table[..NC], 10);
        self.dd = Huff::new(&table[NC..NC + DC], 7);
        self.ldd = Huff::new(&table[NC + DC..NC + DC + LDC], 7);
        self.rd = Huff::new(&table[NC + DC + LDC..], 7);
        self.old_table = table;
        Ok(())
    }

    fn ppm_init(&mut self) -> Result<()> {
        let flags = self.inp.next_byte();
        let reset = flags & 0x20 != 0;
        let mem = if reset { self.inp.next_byte() as u32 } else { 0 };
        if flags & 0x40 != 0 {
            self.esc = self.inp.next_byte();
        }
        self.coder = RarCoder::new(&mut self.inp);
        if reset {
            let mut order = (flags & 0x1F) as u32 + 1;
            if order > 16 {
                order = 16 + (order - 16) * 3;
            }
            if order == 1 {
                return Err(bad("a PPM order of 1"));
            }
            let size = (mem + 1) << 20;
            match &mut self.ppm {
                Some(m) if m_size(m) == size => m.reset(order),
                _ => self.ppm = Some(Box::new(Model::new(size, order))),
            }
        } else if self.ppm.is_none() {
            return Err(bad("a PPM block without a model"));
        }
        self.ppm_mode = true;
        Ok(())
    }

    fn ppm_char(&mut self) -> Result<u8> {
        let m = self.ppm.as_mut().ok_or_else(|| bad("no PPM model"))?;
        let mut d = RarDec { c: &mut self.coder, src: &mut self.inp };
        m.decode(&mut d).ok_or_else(|| bad("damaged PPM data"))
    }

    fn put(&mut self, b: u8) {
        let mask = self.win.len() as u64 - 1;
        self.win[(self.wr & mask) as usize] = b;
        self.wr += 1;
    }

    fn copy(&mut self, len: u64, dist: u64) -> Result<()> {
        if dist == 0 || dist > self.wr || dist > self.win.len() as u64 {
            return Err(bad("a match reaches before the start"));
        }
        let mask = self.win.len() as u64 - 1;
        for _ in 0..len {
            let b = self.win[((self.wr - dist) & mask) as usize];
            self.win[(self.wr & mask) as usize] = b;
            self.wr += 1;
        }
        Ok(())
    }

    fn insert_dist(&mut self, d: u64) {
        self.old_dist = [d, self.old_dist[0], self.old_dist[1], self.old_dist[2]];
    }

    /// Decodes until the window is full or the member's end mark.
    fn decode(&mut self, raw: &mut Raw<'_>) -> Result<()> {
        let stop = self.flushed + self.win.len() as u64 - MARGIN;
        while !self.file_end && self.wr < stop {
            if self.inp.pos() > self.inp.len + 64 {
                // No end mark: the data ends with the member.
                if self.wr - self.file_start >= self.left_total() {
                    self.file_end = true;
                    break;
                }
                return Err(bad("a member's data ends early"));
            }
            self.inp.refill(raw)?;
            if self.ppm_mode {
                self.ppm_step(raw)?;
                continue;
            }
            let sym = self.ld.decode(&mut self.inp) as usize;
            if sym < 256 {
                self.put(sym as u8);
            } else if sym >= 271 {
                let n = sym - 271;
                let mut len = LDECODE[n] as u64 + 3;
                if LBITS[n] > 0 {
                    len += self.inp.bits(LBITS[n]) as u64;
                }
                let slot = (self.dd.decode(&mut self.inp) as usize).min(DC - 1);
                let (base, bits) = self.dist[slot];
                let mut dist = base as u64 + 1;
                if bits > 0 {
                    if slot > 9 {
                        if bits > 4 {
                            dist += ((self.inp.get16() >> (20 - bits)) as u64) << 4;
                            self.inp.add(bits - 4);
                        }
                        if self.low_rep > 0 {
                            self.low_rep -= 1;
                            dist += self.prev_low;
                        } else {
                            let low = self.ldd.decode(&mut self.inp) as u64;
                            if low == 16 {
                                self.low_rep = 15;
                                dist += self.prev_low;
                            } else {
                                dist += low;
                                self.prev_low = low;
                            }
                        }
                    } else {
                        dist += self.inp.bits(bits) as u64;
                    }
                }
                if dist >= 0x2000 {
                    len += 1;
                    if dist >= 0x40000 {
                        len += 1;
                    }
                }
                self.insert_dist(dist);
                self.last_len = len;
                self.copy(len, dist)?;
            } else if sym == 256 {
                // End of a block: a new file, new tables, or both.
                let field = self.inp.get16();
                let (new_table, new_file) = if field & 0x8000 != 0 {
                    self.inp.add(1);
                    (true, false)
                } else {
                    self.inp.add(2);
                    (field & 0x4000 != 0, true)
                };
                self.tables_read = !new_table;
                if new_file {
                    self.file_end = true;
                    break;
                }
                if new_table {
                    self.read_tables()?;
                }
            } else if sym == 257 {
                let first = self.inp.bits(8) as u8;
                let mut len = (first & 7) as usize + 1;
                if len == 7 {
                    len = self.inp.bits(8) as usize + 7;
                } else if len == 8 {
                    len = self.inp.bits(16) as usize;
                }
                let mut code = Vec::with_capacity(len);
                for _ in 0..len {
                    self.inp.refill(raw)?;
                    code.push(self.inp.bits(8) as u8);
                }
                self.add_filter(first, &code)?;
            } else if sym == 258 {
                if self.last_len != 0 {
                    self.copy(self.last_len, self.old_dist[0])?;
                }
            } else if sym < 263 {
                let k = sym - 259;
                let dist = self.old_dist[k];
                for i in (1..=k).rev() {
                    self.old_dist[i] = self.old_dist[i - 1];
                }
                self.old_dist[0] = dist;
                let n = (self.rd.decode(&mut self.inp) as usize).min(27);
                let mut len = LDECODE[n] as u64 + 2;
                if LBITS[n] > 0 {
                    len += self.inp.bits(LBITS[n]) as u64;
                }
                self.last_len = len;
                self.copy(len, dist)?;
            } else {
                let n = sym - 263;
                let mut dist = SDDECODE[n] as u64 + 1;
                if SDBITS[n] > 0 {
                    dist += self.inp.bits(SDBITS[n]) as u64;
                }
                self.insert_dist(dist);
                self.last_len = 2;
                self.copy(2, dist)?;
            }
        }
        Ok(())
    }

    fn left_total(&self) -> u64 {
        // (What the member still needs, from its start.)
        self.flushed - self.file_start + self.left
    }

    fn ppm_step(&mut self, raw: &mut Raw<'_>) -> Result<()> {
        let ch = self.ppm_char()?;
        if ch != self.esc {
            self.put(ch);
            return Ok(());
        }
        match self.ppm_char()? {
            0 => self.read_tables(),
            2 => {
                // The end of the file; the next starts with new tables.
                self.file_end = true;
                self.tables_read = false;
                Ok(())
            }
            3 => {
                let first = self.ppm_char()?;
                let mut len = (first & 7) as usize + 1;
                if len == 7 {
                    len = self.ppm_char()? as usize + 7;
                } else if len == 8 {
                    len = (self.ppm_char()? as usize) << 8;
                    len += self.ppm_char()? as usize;
                }
                let mut code = Vec::with_capacity(len);
                for _ in 0..len {
                    self.inp.refill(raw)?;
                    code.push(self.ppm_char()?);
                }
                self.add_filter(first, &code)
            }
            4 => {
                let mut dist = 0u64;
                for _ in 0..3 {
                    dist = (dist << 8) | self.ppm_char()? as u64;
                }
                let len = self.ppm_char()? as u64;
                self.copy(len + 32, dist + 2)
            }
            5 => {
                let len = self.ppm_char()? as u64;
                self.copy(len + 4, 1)
            }
            _ => {
                self.put(self.esc);
                Ok(())
            }
        }
    }

    /// A filter definition (`first`: its flags byte; `code`: the rest).
    fn add_filter(&mut self, first: u8, code: &[u8]) -> Result<()> {
        let mut b = CodeBits { b: code, pos: 0 };
        let mut pos = if first & 0x80 != 0 {
            let n = b.number() as usize;
            if n == 0 {
                self.defs.clear();
                0
            } else {
                n - 1
            }
        } else {
            self.last_filter
        };
        if pos > self.defs.len() || pos > 1024 {
            return Err(bad("a filter number"));
        }
        self.last_filter = pos;
        let new = pos == self.defs.len();
        if new {
            self.defs.push(FilterDef { kind: None, last_len: 0, exec: 0 });
        } else {
            self.defs[pos].exec += 1;
        }
        let mut start = b.number() as u64;
        if first & 0x40 != 0 {
            start += 258;
        }
        let len = if first & 0x20 != 0 { b.number() } else { self.defs[pos].last_len };
        self.defs[pos].last_len = len;
        let mut regs = [0u32; 7];
        regs[3] = 0x3C000;
        regs[4] = len;
        regs[5] = self.defs[pos].exec;
        if first & 0x10 != 0 {
            let mask = b.get() >> 9;
            b.add(7);
            for (i, r) in regs.iter_mut().enumerate() {
                if mask & (1 << i) != 0 {
                    *r = b.number();
                }
            }
        }
        if new {
            let size = b.number() as usize;
            if size == 0 || size >= 0x10000 {
                return Err(bad("a filter's code size"));
            }
            let vm: Vec<u8> = (0..size)
                .map(|_| {
                    let v = (b.get() >> 8) as u8;
                    b.add(8);
                    v
                })
                .collect();
            let xor = vm[1..].iter().fold(0u8, |a, &x| a ^ x);
            let crc = dix_core::partition::crc32(&vm);
            pos = pos.min(self.defs.len() - 1);
            self.defs[pos].kind =
                (xor == vm[0]).then(|| STANDARD.iter().find(|s| s.0 == size && s.1 == crc).map(|s| s.2)).flatten();
        }
        // (Global data for the program: the standard filters use none.)
        if first & 0x08 != 0 {
            let n = b.number() as usize;
            if n > 0x2000 - 0x40 {
                return Err(bad("a filter's data size"));
            }
            b.add(n * 8);
        }
        let kind = self.defs[pos].kind;
        self.pending.push_back(Pending { start: self.wr + start, len: len as u64, kind, regs });
        Ok(())
    }

    fn emit(&mut self, data: &[u8], out: &mut Vec<u8>) {
        let n = (data.len() as u64).min(self.left) as usize;
        out.extend_from_slice(&data[..n]);
        self.crc_now = dix_core::partition::crc32_continue(self.crc_now, &data[..n]);
        self.left -= n as u64;
    }

    fn window(&self, from: u64, to: u64) -> Vec<u8> {
        let mask = self.win.len() as u64 - 1;
        (from..to).map(|p| self.win[(p & mask) as usize]).collect()
    }

    /// Hands out what is decoded, running filters once their block is in.
    fn flush(&mut self, out: &mut Vec<u8>) -> Result<()> {
        while self.flushed < self.wr {
            match self.pending.front().cloned() {
                Some(f) if f.start < self.flushed => {
                    self.pending.pop_front();
                }
                Some(f) if f.start == self.flushed => {
                    let end = f.start + f.len;
                    if end > self.wr {
                        if !self.file_end {
                            return Ok(()); // the block is not complete yet
                        }
                        self.pending.pop_front();
                        continue;
                    }
                    let mut data = self.window(f.start, end);
                    let at = (f.start - self.file_start) as u32;
                    // Filters on the same block run one after another.
                    loop {
                        let f = self.pending.pop_front().unwrap();
                        let kind = f
                            .kind
                            .ok_or_else(|| Error::unsupported("a RAR 3 filter that is not one of the standard ones"))?;
                        let mut regs = f.regs;
                        regs[6] = at;
                        data = run_filter(kind, &regs, data).ok_or_else(|| bad("a filter's parameters"))?;
                        match self.pending.front() {
                            Some(g) if g.start == f.start && g.len == data.len() as u64 => {}
                            _ => break,
                        }
                    }
                    self.emit(&data, out);
                    self.flushed = end;
                }
                Some(f) => {
                    let to = f.start.min(self.wr);
                    let d = self.window(self.flushed, to);
                    self.emit(&d, out);
                    self.flushed = to;
                }
                None => {
                    let d = self.window(self.flushed, self.wr);
                    self.emit(&d, out);
                    self.flushed = self.wr;
                }
            }
        }
        Ok(())
    }

    /// Adds at least `want` bytes of the member to `out` (fewer at its end).
    pub fn fill(&mut self, raw: &mut Raw<'_>, out: &mut Vec<u8>, want: usize) -> Result<()> {
        let start = out.len();
        while self.left > 0 && out.len() - start < want {
            let before = (self.wr, self.left);
            self.decode(raw)?;
            self.flush(out)?;
            if (self.wr, self.left) == before {
                if self.file_end {
                    return Err(bad("a member's data ends early"));
                }
                return Err(bad("RAR 3 data that does not move on"));
            }
        }
        if self.left == 0 {
            // Read on to the member's end mark (it says whether the next
            // member of a solid run brings new tables).
            while !self.file_end {
                let before = self.wr;
                self.decode(raw)?;
                self.flushed = self.wr;
                self.pending.clear();
                if self.wr == before && !self.file_end {
                    break;
                }
            }
            self.flushed = self.wr;
            if let Some(c) = self.crc
                && c != self.crc_now
            {
                return Err(Error::corrupted("a RAR member's checksum does not match: it is damaged"));
            }
        }
        Ok(())
    }
}

fn m_size(m: &Model) -> u32 {
    m.memory()
}

/// Runs a standard filter on a block (`regs`: R0–R6 of the VM).
pub(crate) fn run_filter(kind: Std, regs: &[u32; 7], mut d: Vec<u8>) -> Option<Vec<u8>> {
    let size = (regs[4] as usize).min(d.len());
    match kind {
        Std::E8 | Std::E8E9 => {
            if !(4..=VM_MEMSIZE).contains(&size) {
                return None;
            }
            const FILE_SIZE: u32 = 0x100_0000;
            let file_offset = regs[6];
            let cmp2 = if kind == Std::E8E9 { 0xE9 } else { 0xE8 };
            let mut pos = 0usize;
            while pos < size - 4 {
                let b = d[pos];
                pos += 1;
                if b == 0xE8 || b == cmp2 {
                    let offset = (pos as u32).wrapping_add(file_offset);
                    let addr = u32::from_le_bytes(d[pos..pos + 4].try_into().unwrap());
                    if addr & 0x8000_0000 != 0 {
                        if addr.wrapping_add(offset) & 0x8000_0000 == 0 {
                            d[pos..pos + 4].copy_from_slice(&addr.wrapping_add(FILE_SIZE).to_le_bytes());
                        }
                    } else if addr.wrapping_sub(FILE_SIZE) & 0x8000_0000 != 0 {
                        d[pos..pos + 4].copy_from_slice(&addr.wrapping_sub(offset).to_le_bytes());
                    }
                    pos += 4;
                }
            }
            d.truncate(size);
            Some(d)
        }
        Std::Itanium => {
            if !(21..=VM_MEMSIZE).contains(&size) {
                return None;
            }
            const MASKS: [u8; 16] = [4, 4, 6, 6, 0, 0, 7, 7, 4, 4, 0, 0, 4, 4, 0, 0];
            let mut file_offset = regs[6] >> 4;
            let mut pos = 0usize;
            while pos < size - 21 {
                let byte = (d[pos] & 0x1F) as i32 - 0x10;
                if byte >= 0 {
                    let mask = MASKS[byte as usize];
                    for i in 0..3 {
                        if mask & (1 << i) != 0 {
                            let start = i * 41 + 5;
                            if itanium_bits(&d[pos..], start + 37, 4) == 5 {
                                let off = itanium_bits(&d[pos..], start + 13, 20);
                                itanium_set(&mut d[pos..], off.wrapping_sub(file_offset) & 0xFFFFF, start + 13, 20);
                            }
                        }
                    }
                }
                pos += 16;
                file_offset = file_offset.wrapping_add(1);
            }
            d.truncate(size);
            Some(d)
        }
        Std::Delta => {
            let channels = regs[0] as usize;
            if size > VM_MEMSIZE / 2 || channels == 0 || channels > 1024 {
                return None;
            }
            let mut out = vec![0u8; size];
            let mut src = 0;
            for ch in 0..channels {
                let mut prev = 0u8;
                let mut p = ch;
                while p < size {
                    prev = prev.wrapping_sub(d[src]);
                    out[p] = prev;
                    src += 1;
                    p += channels;
                }
            }
            Some(out)
        }
        Std::Rgb => {
            let width = regs[0].wrapping_sub(3) as usize;
            let pos_r = regs[1] as usize;
            if !(3..=VM_MEMSIZE / 2).contains(&size) || width > size || pos_r > 2 {
                return None;
            }
            let mut out = vec![0u8; size];
            let mut src = 0;
            for ch in 0..3 {
                let mut prev = 0u32;
                let mut i = ch;
                while i < size {
                    let mut predicted = prev;
                    if i >= width + 3 {
                        let upper = out[i - width] as u32;
                        let upper_left = out[i - width - 3] as u32;
                        predicted = prev.wrapping_add(upper).wrapping_sub(upper_left);
                        let pa = (predicted.wrapping_sub(prev) as i32).unsigned_abs();
                        let pb = (predicted.wrapping_sub(upper) as i32).unsigned_abs();
                        let pc = (predicted.wrapping_sub(upper_left) as i32).unsigned_abs();
                        predicted = if pa <= pb && pa <= pc {
                            prev
                        } else if pb <= pc {
                            upper
                        } else {
                            upper_left
                        };
                    }
                    let v = (predicted.wrapping_sub(d[src] as u32)) as u8;
                    src += 1;
                    out[i] = v;
                    prev = v as u32;
                    i += 3;
                }
            }
            let mut i = pos_r;
            while i + 2 < size {
                let g = out[i + 1];
                out[i] = out[i].wrapping_add(g);
                out[i + 2] = out[i + 2].wrapping_add(g);
                i += 3;
            }
            Some(out)
        }
        Std::Audio => {
            let channels = regs[0] as usize;
            if size > VM_MEMSIZE / 2 || channels == 0 || channels > 128 {
                return None;
            }
            let mut out = vec![0u8; size];
            let mut src = 0;
            for ch in 0..channels {
                let (mut prev_byte, mut prev_delta) = (0u32, 0u32);
                let mut dif = [0u32; 7];
                let (mut d1, mut d2, mut d3): (i32, i32, i32);
                d1 = 0;
                d2 = 0;
                let (mut k1, mut k2, mut k3) = (0i32, 0i32, 0i32);
                let mut i = ch;
                let mut count = 0u32;
                while i < size {
                    d3 = d2;
                    d2 = prev_delta.wrapping_sub(d1 as u32) as i32;
                    d1 = prev_delta as i32;
                    let mut predicted = (8u32.wrapping_mul(prev_byte))
                        .wrapping_add((k1.wrapping_mul(d1)) as u32)
                        .wrapping_add((k2.wrapping_mul(d2)) as u32)
                        .wrapping_add((k3.wrapping_mul(d3)) as u32);
                    predicted = (predicted >> 3) & 0xFF;
                    let cur = d[src] as u32;
                    src += 1;
                    predicted = predicted.wrapping_sub(cur);
                    out[i] = predicted as u8;
                    prev_delta = (predicted.wrapping_sub(prev_byte) as u8 as i8) as i32 as u32;
                    prev_byte = predicted;
                    let dd = ((cur as u8 as i8) as i32) << 3;
                    let abs = |v: i32| v.unsigned_abs();
                    dif[0] = dif[0].wrapping_add(abs(dd));
                    dif[1] = dif[1].wrapping_add(abs(dd.wrapping_sub(d1)));
                    dif[2] = dif[2].wrapping_add(abs(dd.wrapping_add(d1)));
                    dif[3] = dif[3].wrapping_add(abs(dd.wrapping_sub(d2)));
                    dif[4] = dif[4].wrapping_add(abs(dd.wrapping_add(d2)));
                    dif[5] = dif[5].wrapping_add(abs(dd.wrapping_sub(d3)));
                    dif[6] = dif[6].wrapping_add(abs(dd.wrapping_add(d3)));
                    if count & 0x1F == 0 {
                        let (mut min, mut which) = (dif[0], 0);
                        dif[0] = 0;
                        for (j, v) in dif.iter_mut().enumerate().skip(1) {
                            if *v < min {
                                min = *v;
                                which = j;
                            }
                            *v = 0;
                        }
                        match which {
                            1 if k1 >= -16 => k1 -= 1,
                            2 if k1 < 16 => k1 += 1,
                            3 if k2 >= -16 => k2 -= 1,
                            4 if k2 < 16 => k2 += 1,
                            5 if k3 >= -16 => k3 -= 1,
                            6 if k3 < 16 => k3 += 1,
                            _ => {}
                        }
                    }
                    i += channels;
                    count += 1;
                }
            }
            Some(out)
        }
    }
}

fn itanium_bits(d: &[u8], bit: usize, n: u32) -> u32 {
    let a = bit / 8;
    let v = u32::from_le_bytes([d[a], d[a + 1], d[a + 2], d[a + 3]]) >> (bit & 7);
    v & (u32::MAX >> (32 - n))
}

fn itanium_set(d: &mut [u8], v: u32, bit: usize, n: u32) {
    let a = bit / 8;
    let mut and = !((u32::MAX >> (32 - n)) << (bit & 7));
    let mut v = v << (bit & 7);
    for i in 0..4 {
        d[a + i] &= and as u8;
        d[a + i] |= v as u8;
        and = (and >> 8) | 0xFF00_0000;
        v >>= 8;
    }
}
