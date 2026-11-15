//! PPMd variant H (Dmitry Shkarin's PPMd, "PPMd7" in 7-Zip): the text
//! compressor of RAR 3/4 archives and of 7z's PPMD method. The model is the
//! same for both; the range coder around it differs (`RarCoder`,
//! `SevenZipCoder`).
//!
//! Written for this project from the published algorithm (Shkarin's PPMd
//! var.H, as unRAR and 7-Zip implement it). The memory allocator follows
//! the original exactly: when memory runs out the model restarts, and the
//! encoder and decoder must agree on when that happens. Memory is one arena
//! of 12-byte units addressed by 32-bit offsets.
//!
//! Layout in the arena (offsets in bytes):
//! * a state (6 bytes): symbol, frequency, successor (u32);
//! * a context (12 bytes): number of states (u16), total frequency (u16),
//!   states (u32), suffix (u32); a context with one state keeps it in
//!   place of the total frequency and states (at offset 2).

// (7z uses one range coder, RAR the other: with only one built in, the
// other's code is unused.)
#![cfg_attr(not(all(feature = "archive-7z", feature = "archive-rar")), allow(dead_code))]

const UNIT: u32 = 12;
const N_INDEXES: usize = 38;
const MAX_FREQ: u32 = 124;
const INT_BITS: u32 = 7;
const PERIOD_BITS: u32 = 7;
const TOT_BITS: u32 = INT_BITS + PERIOD_BITS;
const BIN_SCALE: u32 = 1 << TOT_BITS;
const INTERVAL: u32 = 1 << INT_BITS;
const MAX_O: usize = 64;
const EXP_ESCAPE: [u8; 16] = [25, 14, 9, 7, 5, 5, 4, 4, 4, 3, 3, 3, 2, 2, 2, 2];
const INIT_BIN_ESC: [u16; 8] = [0x3CDD, 0x1F3F, 0x59BF, 0x48F3, 0x64A1, 0x5ABC, 0x6632, 0x6051];

/// A range decoder as the model uses it.
pub(crate) trait RangeDecoder {
    /// The position of the next symbol in a scale of `total`.
    fn threshold(&mut self, total: u32) -> u32;
    /// Takes [low, low + size) of the scale last given to `threshold`.
    fn decode(&mut self, low: u32, size: u32);
    /// A binary context: true when the symbol of probability size0/2^14
    /// comes.
    fn decode_bin(&mut self, size0: u32) -> bool;
}

/// A range encoder (the tests make archives with it).
#[cfg(test)]
pub(crate) trait RangeEncoder {
    fn encode(&mut self, low: u32, size: u32, total: u32);
    fn encode_bin(&mut self, size0: u32, first: bool);
}

/// Where bytes come from for a range decoder.
pub(crate) trait ByteSource {
    fn next_byte(&mut self) -> u8;
}

impl ByteSource for std::slice::Iter<'_, u8> {
    fn next_byte(&mut self) -> u8 {
        self.next().copied().unwrap_or(0)
    }
}

/// RAR's range coder (Subbotin's carry-less coder).
#[derive(Debug, Clone, Default)]
pub(crate) struct RarCoder {
    low: u32,
    code: u32,
    range: u32,
}

const TOP: u32 = 1 << 24;
const BOT: u32 = 1 << 15;

impl RarCoder {
    pub fn new(src: &mut dyn ByteSource) -> RarCoder {
        let mut c = RarCoder { low: 0, code: 0, range: u32::MAX };
        for _ in 0..4 {
            c.code = (c.code << 8) | src.next_byte() as u32;
        }
        c
    }

    fn normalize(&mut self, src: &mut dyn ByteSource) {
        loop {
            if (self.low ^ self.low.wrapping_add(self.range)) >= TOP {
                if self.range >= BOT {
                    break;
                }
                self.range = self.low.wrapping_neg() & (BOT - 1);
            }
            self.code = (self.code << 8) | src.next_byte() as u32;
            self.range <<= 8;
            self.low <<= 8;
        }
    }
}

/// A RAR coder reading from a source (the decoder holds both).
pub(crate) struct RarDec<'a> {
    pub c: &'a mut RarCoder,
    pub src: &'a mut dyn ByteSource,
}

impl RangeDecoder for RarDec<'_> {
    fn threshold(&mut self, total: u32) -> u32 {
        self.c.range /= total.max(1);
        self.c.code.wrapping_sub(self.c.low) / self.c.range.max(1)
    }

    fn decode(&mut self, low: u32, size: u32) {
        self.c.low = self.c.low.wrapping_add(self.c.range.wrapping_mul(low));
        self.c.range = self.c.range.wrapping_mul(size);
        self.c.normalize(self.src);
    }

    fn decode_bin(&mut self, size0: u32) -> bool {
        self.c.range >>= TOT_BITS;
        let count = self.c.code.wrapping_sub(self.c.low) / self.c.range.max(1);
        if count < size0 {
            self.decode(0, size0);
            true
        } else {
            self.decode(size0, BIN_SCALE - size0);
            false
        }
    }
}

/// 7-Zip's range decoder for PPMd (7z's PPMD method).
pub(crate) struct SevenZipCoder<'a> {
    code: u32,
    range: u32,
    src: std::slice::Iter<'a, u8>,
}

impl<'a> SevenZipCoder<'a> {
    pub fn new(data: &'a [u8]) -> Option<SevenZipCoder<'a>> {
        let mut src = data.iter();
        if src.next_byte() != 0 {
            return None;
        }
        let mut code = 0u32;
        for _ in 0..4 {
            code = (code << 8) | src.next_byte() as u32;
        }
        Some(SevenZipCoder { code, range: u32::MAX, src })
    }

    fn normalize(&mut self) {
        while self.range < TOP {
            self.code = (self.code << 8) | self.src.next_byte() as u32;
            self.range <<= 8;
        }
    }
}

impl RangeDecoder for SevenZipCoder<'_> {
    fn threshold(&mut self, total: u32) -> u32 {
        self.range /= total.max(1);
        self.code / self.range.max(1)
    }

    fn decode(&mut self, low: u32, size: u32) {
        self.code = self.code.wrapping_sub(low.wrapping_mul(self.range));
        self.range = self.range.wrapping_mul(size);
        self.normalize();
    }

    fn decode_bin(&mut self, size0: u32) -> bool {
        let bound = (self.range >> TOT_BITS) * size0;
        let first = self.code < bound;
        if first {
            self.range = bound;
        } else {
            self.code -= bound;
            self.range -= bound;
        }
        self.normalize();
        first
    }
}

/// A SEE context (secondary escape estimation).
#[derive(Debug, Clone, Copy, Default)]
struct See {
    summ: u16,
    shift: u8,
    count: u8,
}

impl See {
    fn init(v: u32) -> See {
        See { summ: (v << (PERIOD_BITS - 4)) as u16, shift: (PERIOD_BITS - 4) as u8, count: 4 }
    }

    fn mean(&mut self) -> u32 {
        let r = (self.summ >> self.shift) as u32;
        self.summ = self.summ.wrapping_sub(r as u16);
        r + (r == 0) as u32
    }

    fn update(&mut self) {
        if (self.shift as u32) < PERIOD_BITS {
            self.count = self.count.wrapping_sub(1);
            if self.count == 0 {
                self.summ = self.summ.wrapping_add(self.summ);
                self.count = (3u32 << self.shift) as u8;
                self.shift += 1;
            }
        }
    }
}

/// Which SEE context an escape uses (the dummy one: `None`).
type SeeRef = Option<(usize, usize)>;

/// The PPMd model and its memory.
#[derive(Clone)]
pub(crate) struct Model {
    mem: Vec<u8>,
    size: u32,
    heap_start: u32,
    heap_end: u32,
    text: u32,
    units_start: u32,
    lo_unit: u32,
    hi_unit: u32,
    glue_count: u32,
    free: [u32; N_INDEXES],
    indx2units: [u8; N_INDEXES],
    units2indx: [u8; 128],
    ns2indx: [u8; 256],
    ns2bsindx: [u8; 256],
    hb2flag: [u8; 256],
    see: [[See; 16]; 25],
    dummy_see: See,
    bin_summ: Box<[[u16; 64]; 128]>,
    min_ctx: u32,
    max_ctx: u32,
    found: u32,
    order_fall: u32,
    init_esc: u32,
    prev_success: u32,
    max_order: u32,
    hi_bits_flag: u32,
    run_length: i32,
    init_rl: i32,
    esc_count: u8,
    num_masked: u32,
    char_mask: [u8; 256],
}

impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Model({} bytes, order {})", self.size, self.max_order)
    }
}

impl Model {
    /// A model of `size` bytes of memory and order `max_order` (2–64).
    pub fn new(size: u32, max_order: u32) -> Model {
        let size = size.clamp(1 << 11, 0xFFFF_FFFF - 64);
        let heap_start = 1;
        let heap_end = heap_start + size;
        let mut m = Model {
            mem: vec![0; (heap_end + UNIT * 2) as usize],
            size,
            heap_start,
            heap_end,
            text: 0,
            units_start: 0,
            lo_unit: 0,
            hi_unit: 0,
            glue_count: 0,
            free: [0; N_INDEXES],
            indx2units: [0; N_INDEXES],
            units2indx: [0; 128],
            ns2indx: [0; 256],
            ns2bsindx: [0; 256],
            hb2flag: [0; 256],
            see: [[See::default(); 16]; 25],
            dummy_see: See { summ: 0, shift: PERIOD_BITS as u8, count: 0 },
            bin_summ: Box::new([[0; 64]; 128]),
            min_ctx: 0,
            max_ctx: 0,
            found: 0,
            order_fall: 0,
            init_esc: 0,
            prev_success: 0,
            max_order: max_order.clamp(2, MAX_O as u32),
            hi_bits_flag: 0,
            run_length: 0,
            init_rl: 0,
            esc_count: 1,
            num_masked: 0,
            char_mask: [0; 256],
        };
        let mut k = 1u32;
        for i in 0..N_INDEXES {
            m.indx2units[i] = k as u8;
            k += match i {
                0..=2 => 1,
                3 => 2,
                4..=6 => 2,
                7 => 3,
                8..=10 => 3,
                11 => 4,
                _ => 4,
            };
        }
        let mut i = 0;
        for k in 0..128 {
            if (m.indx2units[i] as usize) < k + 1 {
                i += 1;
            }
            m.units2indx[k] = i as u8;
        }
        m.ns2bsindx[0] = 0;
        m.ns2bsindx[1] = 2;
        m.ns2bsindx[2..11].fill(4);
        m.ns2bsindx[11..].fill(6);
        for i in 0..3 {
            m.ns2indx[i] = i as u8;
        }
        let (mut mm, mut k, mut step) = (3u32, 1u32, 1u32);
        for i in 3..256 {
            m.ns2indx[i] = mm as u8;
            k -= 1;
            if k == 0 {
                step += 1;
                k = step;
                mm += 1;
            }
        }
        m.hb2flag[0x40..].fill(8);
        m.restart();
        m
    }

    // ----- memory --------------------------------------------------------

    fn u8_at(&self, o: u32) -> u32 {
        self.mem.get(o as usize).copied().unwrap_or(0) as u32
    }

    fn set_u8(&mut self, o: u32, v: u32) {
        if let Some(b) = self.mem.get_mut(o as usize) {
            *b = v as u8;
        }
    }

    fn u16_at(&self, o: u32) -> u32 {
        let o = o as usize;
        self.mem.get(o..o + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]) as u32)
    }

    fn set_u16(&mut self, o: u32, v: u32) {
        let o = o as usize;
        if let Some(b) = self.mem.get_mut(o..o + 2) {
            b.copy_from_slice(&(v as u16).to_le_bytes());
        }
    }

    fn u32_at(&self, o: u32) -> u32 {
        let o = o as usize;
        self.mem.get(o..o + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    fn set_u32(&mut self, o: u32, v: u32) {
        let o = o as usize;
        if let Some(b) = self.mem.get_mut(o..o + 4) {
            b.copy_from_slice(&v.to_le_bytes());
        }
    }

    fn copy(&mut self, to: u32, from: u32, n: u32) {
        let (to, from, n) = (to as usize, from as usize, n as usize);
        if from + n <= self.mem.len() && to + n <= self.mem.len() {
            self.mem.copy_within(from..from + n, to);
        }
    }

    // Contexts.
    fn ns(&self, c: u32) -> u32 {
        self.u16_at(c)
    }
    fn set_ns(&mut self, c: u32, v: u32) {
        self.set_u16(c, v)
    }
    fn summ(&self, c: u32) -> u32 {
        self.u16_at(c + 2)
    }
    fn set_summ(&mut self, c: u32, v: u32) {
        self.set_u16(c + 2, v)
    }
    fn stats(&self, c: u32) -> u32 {
        self.u32_at(c + 4)
    }
    fn set_stats(&mut self, c: u32, v: u32) {
        self.set_u32(c + 4, v)
    }
    fn suffix(&self, c: u32) -> u32 {
        self.u32_at(c + 8)
    }
    fn set_suffix(&mut self, c: u32, v: u32) {
        self.set_u32(c + 8, v)
    }
    fn one_state(c: u32) -> u32 {
        c + 2
    }

    // States.
    fn sym(&self, s: u32) -> u32 {
        self.u8_at(s)
    }
    fn set_sym(&mut self, s: u32, v: u32) {
        self.set_u8(s, v)
    }
    fn freq(&self, s: u32) -> u32 {
        self.u8_at(s + 1)
    }
    fn set_freq(&mut self, s: u32, v: u32) {
        self.set_u8(s + 1, v)
    }
    fn succ(&self, s: u32) -> u32 {
        self.u32_at(s + 2)
    }
    fn set_succ(&mut self, s: u32, v: u32) {
        self.set_u32(s + 2, v)
    }
    fn swap_states(&mut self, a: u32, b: u32) {
        for k in 0..6 {
            let (x, y) = (self.u8_at(a + k), self.u8_at(b + k));
            self.set_u8(a + k, y);
            self.set_u8(b + k, x);
        }
    }
    fn state(&self, s: u32) -> [u8; 6] {
        let mut v = [0u8; 6];
        for (k, b) in v.iter_mut().enumerate() {
            *b = self.u8_at(s + k as u32) as u8;
        }
        v
    }
    fn set_state(&mut self, s: u32, v: [u8; 6]) {
        for (k, b) in v.iter().enumerate() {
            self.set_u8(s + k as u32, *b as u32);
        }
    }

    // ----- the allocator (as Shkarin's) ----------------------------------

    fn i2u(&self, i: usize) -> u32 {
        self.indx2units[i] as u32
    }
    fn u2i(&self, nu: u32) -> usize {
        self.units2indx[(nu as usize).clamp(1, 128) - 1] as usize
    }

    fn insert_node(&mut self, p: u32, i: usize) {
        self.set_u32(p, self.free[i]);
        self.free[i] = p;
    }

    fn remove_node(&mut self, i: usize) -> u32 {
        let p = self.free[i];
        self.free[i] = self.u32_at(p);
        p
    }

    fn split_block(&mut self, p: u32, old: usize, new: usize) {
        let mut udiff = self.i2u(old) - self.i2u(new);
        let mut p = p + self.i2u(new) * UNIT;
        let mut i = self.u2i(udiff);
        if self.i2u(i) != udiff {
            i -= 1;
            self.insert_node(p, i);
            let k = self.i2u(i);
            p += k * UNIT;
            udiff -= k;
        }
        let i = self.u2i(udiff);
        self.insert_node(p, i);
    }

    // Blocks being glued: a stamp (0xFFFF: free), units, next, previous.
    fn glue_free_blocks(&mut self) {
        let head = self.heap_end;
        let (next, prev) = (|b: u32| b + 4, |b: u32| b + 8);
        self.set_u32(next(head), head);
        self.set_u32(prev(head), head);
        self.set_u16(head, 0);
        if self.lo_unit != self.hi_unit {
            self.set_u8(self.lo_unit, 0);
        }
        for i in 0..N_INDEXES {
            while self.free[i] != 0 {
                let p = self.remove_node(i);
                // Insert after the head.
                let n = self.u32_at(next(head));
                self.set_u32(prev(p), head);
                self.set_u32(next(p), n);
                self.set_u32(prev(n), p);
                self.set_u32(next(head), p);
                self.set_u16(p, 0xFFFF);
                self.set_u16(p + 2, self.i2u(i));
            }
        }
        let unlink = |m: &mut Model, b: u32| {
            let (n, pr) = (m.u32_at(next(b)), m.u32_at(prev(b)));
            m.set_u32(next(pr), n);
            m.set_u32(prev(n), pr);
        };
        let mut p = self.u32_at(next(head));
        let mut guard = 0u64;
        while p != head && guard < 1 << 32 {
            guard += 1;
            loop {
                let nu = self.u16_at(p + 2);
                let p1 = p + nu * UNIT;
                if self.u16_at(p1) == 0xFFFF && nu + self.u16_at(p1 + 2) < 0x10000 && p1 != head {
                    unlink(self, p1);
                    let n1 = self.u16_at(p1 + 2);
                    self.set_u16(p + 2, nu + n1);
                } else {
                    break;
                }
            }
            p = self.u32_at(next(p));
        }
        loop {
            let mut p = self.u32_at(next(head));
            if p == head || p == 0 {
                break;
            }
            unlink(self, p);
            let mut sz = self.u16_at(p + 2);
            while sz > 128 {
                self.insert_node(p, N_INDEXES - 1);
                sz -= 128;
                p += 128 * UNIT;
            }
            let mut i = self.u2i(sz);
            if self.i2u(i) != sz {
                i -= 1;
                let k = sz - self.i2u(i);
                self.insert_node(p + (sz - k) * UNIT, (k - 1) as usize);
            }
            self.insert_node(p, i);
        }
        self.set_u32(next(head), 0);
        self.set_u32(prev(head), 0);
    }

    fn alloc_units_rare(&mut self, indx: usize) -> u32 {
        if self.glue_count == 0 {
            self.glue_count = 255;
            self.glue_free_blocks();
            if self.free[indx] != 0 {
                return self.remove_node(indx);
            }
        }
        let mut i = indx;
        loop {
            i += 1;
            if i == N_INDEXES {
                self.glue_count -= 1;
                let j = self.i2u(indx) * UNIT;
                if self.units_start - self.text > j {
                    self.units_start -= j;
                    return self.units_start;
                }
                return 0;
            }
            if self.free[i] != 0 {
                break;
            }
        }
        let r = self.remove_node(i);
        self.split_block(r, i, indx);
        r
    }

    fn alloc_units(&mut self, nu: u32) -> u32 {
        let indx = self.u2i(nu);
        if self.free[indx] != 0 {
            return self.remove_node(indx);
        }
        let r = self.lo_unit;
        let n = self.i2u(indx) * UNIT;
        if self.lo_unit + n <= self.hi_unit {
            self.lo_unit += n;
            return r;
        }
        self.alloc_units_rare(indx)
    }

    fn alloc_context(&mut self) -> u32 {
        if self.hi_unit != self.lo_unit {
            self.hi_unit -= UNIT;
            return self.hi_unit;
        }
        if self.free[0] != 0 {
            return self.remove_node(0);
        }
        self.alloc_units_rare(0)
    }

    fn expand_units(&mut self, old: u32, old_nu: u32) -> u32 {
        let (i0, i1) = (self.u2i(old_nu), self.u2i(old_nu + 1));
        if i0 == i1 {
            return old;
        }
        let p = self.alloc_units(old_nu + 1);
        if p != 0 {
            self.copy(p, old, old_nu * UNIT);
            self.insert_node(old, i0);
        }
        p
    }

    fn shrink_units(&mut self, old: u32, old_nu: u32, new_nu: u32) -> u32 {
        let (i0, i1) = (self.u2i(old_nu), self.u2i(new_nu));
        if i0 == i1 {
            return old;
        }
        if self.free[i1] != 0 {
            let p = self.remove_node(i1);
            self.copy(p, old, new_nu * UNIT);
            self.insert_node(old, i0);
            p
        } else {
            self.split_block(old, i0, i1);
            old
        }
    }

    fn free_units(&mut self, p: u32, nu: u32) {
        let i = self.u2i(nu);
        self.insert_node(p, i);
    }

    // ----- the model ------------------------------------------------------

    fn restart(&mut self) {
        self.char_mask = [0; 256];
        // The allocator.
        self.free = [0; N_INDEXES];
        self.text = self.heap_start;
        let size2 = UNIT * (self.size / 8 / UNIT * 7);
        let size1 = self.size - size2;
        self.units_start = self.heap_start + size1;
        self.lo_unit = self.units_start;
        self.hi_unit = self.lo_unit + size2;
        self.glue_count = 0;
        // The order -1 context: every byte once.
        self.init_rl = -(self.max_order.min(12) as i32) - 1;
        let c = self.alloc_context();
        self.min_ctx = c;
        self.max_ctx = c;
        self.set_suffix(c, 0);
        self.order_fall = self.max_order;
        self.set_ns(c, 256);
        self.set_summ(c, 257);
        let stats = self.alloc_units(128);
        self.set_stats(c, stats);
        self.found = stats;
        self.run_length = self.init_rl;
        self.prev_success = 0;
        for i in 0..256 {
            let s = stats + i * 6;
            self.set_sym(s, i);
            self.set_freq(s, 1);
            self.set_succ(s, 0);
        }
        for i in 0..128 {
            for (k, &esc) in INIT_BIN_ESC.iter().enumerate() {
                for m in (0..64).step_by(8) {
                    self.bin_summ[i][k + m] = (BIN_SCALE - esc as u32 / (i as u32 + 2)) as u16;
                }
            }
        }
        for i in 0..25 {
            for k in 0..16 {
                self.see[i][k] = See::init(5 * i as u32 + 10);
            }
        }
    }

    /// The memory it was made with.
    pub fn memory(&self) -> u32 {
        self.size
    }

    /// Starts afresh with a new order (RAR's "reset" PPM block).
    pub fn reset(&mut self, max_order: u32) {
        self.max_order = max_order.clamp(2, MAX_O as u32);
        self.esc_count = 1;
        self.restart();
    }

    fn see_mut(&mut self, r: SeeRef) -> &mut See {
        match r {
            Some((a, b)) => &mut self.see[a][b],
            None => &mut self.dummy_see,
        }
    }

    fn rescale(&mut self) {
        let c = self.min_ctx;
        let old_ns = self.ns(c);
        let stats = self.stats(c);
        // The found state to the front.
        let mut p = self.found;
        while p > stats {
            self.swap_states(p, p - 6);
            p -= 6;
        }
        self.set_freq(stats, self.freq(stats) + 4);
        self.set_summ(c, self.summ(c) + 4);
        let mut esc_freq = self.summ(c) as i64 - self.freq(stats) as i64;
        let adder = (self.order_fall != 0) as u32;
        let f0 = (self.freq(stats) + adder) >> 1;
        self.set_freq(stats, f0);
        let mut summ = f0;
        let mut p = stats;
        for _ in 1..old_ns {
            p += 6;
            esc_freq -= self.freq(p) as i64;
            let f = (self.freq(p) + adder) >> 1;
            self.set_freq(p, f);
            summ += f;
            if f > self.freq(p - 6) {
                let tmp = self.state(p);
                let mut p1 = p;
                loop {
                    let prev = self.state(p1 - 6);
                    self.set_state(p1, prev);
                    p1 -= 6;
                    if p1 == stats || tmp[1] as u32 <= self.freq(p1 - 6) {
                        break;
                    }
                }
                self.set_state(p1, tmp);
            }
        }
        self.set_summ(c, summ);
        let mut ns = old_ns;
        if self.freq(p) == 0 {
            let mut i = 0;
            loop {
                i += 1;
                p -= 6;
                if self.freq(p) != 0 {
                    break;
                }
            }
            esc_freq += i as i64;
            ns -= i;
            self.set_ns(c, ns);
            if ns == 1 {
                let mut tmp = self.state(stats);
                loop {
                    tmp[1] -= tmp[1] >> 1;
                    esc_freq >>= 1;
                    if esc_freq <= 1 {
                        break;
                    }
                }
                self.free_units(stats, old_ns.div_ceil(2));
                self.set_state(Self::one_state(c), tmp);
                self.found = Self::one_state(c);
                return;
            }
        }
        esc_freq -= esc_freq >> 1;
        self.set_summ(c, (self.summ(c) as i64 + esc_freq) as u32);
        let (n0, n1) = (old_ns.div_ceil(2), ns.div_ceil(2));
        if n0 != n1 {
            let s = self.shrink_units(stats, n0, n1);
            self.set_stats(c, s);
        }
        self.found = self.stats(c);
    }

    fn create_successors(&mut self, skip: bool, p1: u32) -> u32 {
        let mut pc = self.min_ctx;
        let up_branch = self.succ(self.found);
        let fsym = self.sym(self.found);
        let mut ps: Vec<u32> = Vec::with_capacity(MAX_O);
        if !skip {
            ps.push(self.found);
            if self.suffix(pc) == 0 {
                return self.make_children(pc, &ps, up_branch);
            }
        }
        let mut p;
        let mut first = p1 != 0;
        loop {
            if first {
                p = p1;
                pc = self.suffix(pc);
                first = false;
            } else {
                pc = self.suffix(pc);
                if self.ns(pc) != 1 {
                    p = self.stats(pc);
                    let mut n = 0;
                    while self.sym(p) != fsym && n < 256 {
                        p += 6;
                        n += 1;
                    }
                } else {
                    p = Self::one_state(pc);
                }
            }
            if self.succ(p) != up_branch {
                pc = self.succ(p);
                break;
            }
            if ps.len() >= MAX_O {
                return 0;
            }
            ps.push(p);
            if self.suffix(pc) == 0 {
                break;
            }
        }
        self.make_children(pc, &ps, up_branch)
    }

    fn make_children(&mut self, mut pc: u32, ps: &[u32], up_branch: u32) -> u32 {
        if ps.is_empty() {
            return pc;
        }
        let up_sym = self.u8_at(up_branch);
        let up_succ = up_branch + 1;
        let up_freq = if self.ns(pc) != 1 {
            if pc <= self.text {
                return 0;
            }
            let mut p = self.stats(pc);
            let mut n = 0;
            while self.sym(p) != up_sym && n < 256 {
                p += 6;
                n += 1;
            }
            let cf = self.freq(p).saturating_sub(1);
            let s0 = self.summ(pc).saturating_sub(self.ns(pc)).saturating_sub(cf);
            1 + if 2 * cf <= s0 { (5 * cf > s0) as u32 } else { (2 * cf + 3 * s0 - 1) / (2 * s0).max(1) }
        } else {
            self.freq(Self::one_state(pc))
        };
        for &s in ps.iter().rev() {
            let c = self.alloc_context();
            if c == 0 {
                return 0;
            }
            self.set_ns(c, 1);
            let o = Self::one_state(c);
            self.set_sym(o, up_sym);
            self.set_freq(o, up_freq);
            self.set_succ(o, up_succ);
            self.set_suffix(c, pc);
            self.set_succ(s, c);
            pc = c;
        }
        pc
    }

    fn update_model(&mut self) {
        let fs = self.state(self.found);
        let (fsym, ffreq, fsucc) = (fs[0] as u32, fs[1] as u32, u32::from_le_bytes(fs[2..6].try_into().unwrap()));
        let mut p = 0u32;
        let pc = self.suffix(self.min_ctx);
        if ffreq < MAX_FREQ / 4 && pc != 0 {
            if self.ns(pc) != 1 {
                p = self.stats(pc);
                if self.sym(p) != fsym {
                    let mut n = 0;
                    while self.sym(p) != fsym && n < 256 {
                        p += 6;
                        n += 1;
                    }
                    if self.freq(p) >= self.freq(p - 6) {
                        self.swap_states(p, p - 6);
                        p -= 6;
                    }
                }
                if self.freq(p) < MAX_FREQ - 9 {
                    self.set_freq(p, self.freq(p) + 2);
                    self.set_summ(pc, self.summ(pc) + 2);
                }
            } else {
                p = Self::one_state(pc);
                if self.freq(p) < 32 {
                    self.set_freq(p, self.freq(p) + 1);
                }
            }
        }
        if self.order_fall == 0 {
            let c = self.create_successors(true, p);
            self.min_ctx = c;
            self.max_ctx = c;
            if c == 0 {
                return self.restart_after_full();
            }
            self.set_succ(self.found, c);
            return;
        }
        self.set_u8(self.text, fsym);
        self.text += 1;
        let mut successor = self.text;
        if self.text >= self.units_start {
            return self.restart_after_full();
        }
        let mut fsucc = fsucc;
        if fsucc != 0 {
            if fsucc <= self.text {
                fsucc = self.create_successors(false, p);
                if fsucc == 0 {
                    return self.restart_after_full();
                }
            }
            self.order_fall -= 1;
            if self.order_fall == 0 {
                successor = fsucc;
                if self.max_ctx != self.min_ctx {
                    self.text -= 1;
                }
            }
        } else {
            self.set_succ(self.found, successor);
            fsucc = self.min_ctx;
        }
        let ns = self.ns(self.min_ctx);
        let s0 = self.summ(self.min_ctx).wrapping_sub(ns).wrapping_sub(ffreq - 1);
        let mut c = self.max_ctx;
        let mut guard = 0;
        while c != self.min_ctx && guard < 1000 {
            guard += 1;
            let ns1 = self.ns(c);
            if ns1 != 1 {
                if ns1 & 1 == 0 {
                    let s = self.expand_units(self.stats(c), ns1 >> 1);
                    if s == 0 {
                        return self.restart_after_full();
                    }
                    self.set_stats(c, s);
                }
                let sf = self.summ(c);
                let add = (2 * ns1 < ns) as u32 + 2 * ((4 * ns1 <= ns) as u32 & (sf <= 8 * ns1) as u32);
                self.set_summ(c, sf + add);
            } else {
                let s = self.alloc_units(1);
                if s == 0 {
                    return self.restart_after_full();
                }
                let one = self.state(Self::one_state(c));
                self.set_state(s, one);
                self.set_stats(c, s);
                let f = self.freq(s);
                self.set_freq(s, if f < MAX_FREQ / 4 - 1 { f + f } else { MAX_FREQ - 4 });
                self.set_summ(c, self.freq(s) + self.init_esc + (ns > 3) as u32);
            }
            let mut cf = 2 * ffreq * (self.summ(c) + 6);
            let sf = s0.wrapping_add(self.summ(c));
            if cf < 6 * sf {
                cf = 1 + (cf > sf) as u32 + (cf >= 4 * sf) as u32;
                self.set_summ(c, self.summ(c) + 3);
            } else {
                cf = 4 + (cf >= 9 * sf) as u32 + (cf >= 12 * sf) as u32 + (cf >= 15 * sf) as u32;
                self.set_summ(c, self.summ(c) + cf);
            }
            let s = self.stats(c) + ns1 * 6;
            self.set_succ(s, successor);
            self.set_sym(s, fsym);
            self.set_freq(s, cf);
            self.set_ns(c, ns1 + 1);
            c = self.suffix(c);
        }
        self.max_ctx = fsucc;
        self.min_ctx = fsucc;
    }

    fn restart_after_full(&mut self) {
        self.restart();
        self.esc_count = 0;
    }

    /// After a symbol: on to the next context, or update the model.
    fn next_context(&mut self) {
        let succ = self.succ(self.found);
        if self.order_fall == 0 && succ > self.text {
            self.min_ctx = succ;
            self.max_ctx = succ;
        } else {
            self.update_model();
            if self.esc_count == 0 {
                self.esc_count = 1;
                self.char_mask = [0; 256];
            }
        }
    }

    fn bin_index(&self) -> (usize, usize) {
        let c = self.min_ctx;
        let rs = Self::one_state(c);
        let suffix_ns = self.ns(self.suffix(c)).max(1);
        let i = (self.freq(rs).clamp(1, 128) - 1) as usize;
        let j = self.prev_success
            + self.ns2bsindx[(suffix_ns - 1).min(255) as usize] as u32
            + self.hi_bits_flag
            + 2 * self.hb2flag[self.sym(rs) as usize] as u32
            + ((self.run_length >> 26) & 0x20) as u32;
        (i, (j as usize).min(63))
    }

    fn make_esc_freq(&mut self, diff: u32) -> (SeeRef, u32) {
        let c = self.min_ctx;
        let ns = self.ns(c);
        if ns != 256 {
            let a = self.ns2indx[(diff.max(1) - 1).min(255) as usize] as usize;
            let b = ((diff as i64) < self.ns(self.suffix(c)) as i64 - ns as i64) as usize
                + 2 * (self.summ(c) < 11 * ns) as usize
                + 4 * (self.num_masked > diff) as usize
                + self.hi_bits_flag as usize;
            let r = Some((a.min(24), b.min(15)));
            let m = self.see_mut(r).mean();
            (r, m)
        } else {
            (None, 1)
        }
    }

    /// Decodes a symbol; `None`: the end mark (or damaged data).
    pub fn decode(&mut self, rc: &mut dyn RangeDecoder) -> Option<u8> {
        let c = self.min_ctx;
        if c <= self.text || c >= self.heap_end {
            return None;
        }
        let ns = self.ns(c);
        if ns != 1 {
            let stats = self.stats(c);
            if stats <= self.text || stats >= self.heap_end {
                return None;
            }
            let scale = self.summ(c);
            let count = rc.threshold(scale);
            if count >= scale {
                return None;
            }
            let mut p = stats;
            let mut hi = self.freq(p);
            if count < hi {
                rc.decode(0, hi);
                self.prev_success = (2 * hi > scale) as u32;
                self.run_length += self.prev_success as i32;
                self.found = p;
                hi += 4;
                self.set_freq(p, hi);
                self.set_summ(c, scale + 4);
                if hi > MAX_FREQ {
                    self.rescale();
                }
                let sym = self.sym(self.found) as u8;
                self.next_context();
                return Some(sym);
            }
            if self.found == 0 {
                return None;
            }
            self.prev_success = 0;
            for _ in 1..ns {
                p += 6;
                let f = self.freq(p);
                hi += f;
                if hi > count {
                    rc.decode(hi - f, f);
                    self.update1(p);
                    let sym = self.sym(self.found) as u8;
                    self.next_context();
                    return Some(sym);
                }
            }
            // An escape: every symbol here is masked.
            self.hi_bits_flag = self.hb2flag[self.sym(self.found) as usize] as u32;
            rc.decode(hi, scale - hi);
            self.num_masked = ns;
            for k in 0..ns {
                let s = self.sym(stats + k * 6) as usize;
                self.char_mask[s] = self.esc_count;
            }
            self.found = 0;
        } else {
            let rs = Self::one_state(c);
            self.hi_bits_flag = self.hb2flag[self.sym(self.found) as usize] as u32;
            let (i, j) = self.bin_index();
            let bs = self.bin_summ[i][j] as u32;
            if rc.decode_bin(bs) {
                self.bin_summ[i][j] = (bs + INTERVAL - ((bs + 32) >> 7)) as u16;
                self.found = rs;
                let f = self.freq(rs);
                self.set_freq(rs, f + (f < 128) as u32);
                self.prev_success = 1;
                self.run_length += 1;
                let sym = self.sym(rs) as u8;
                self.next_context();
                return Some(sym);
            }
            let bs = bs - ((bs + 32) >> 7);
            self.bin_summ[i][j] = bs as u16;
            self.init_esc = EXP_ESCAPE[(bs >> 10) as usize] as u32;
            self.num_masked = 1;
            self.char_mask[self.sym(rs) as usize] = self.esc_count;
            self.prev_success = 0;
            self.found = 0;
        }
        // Escapes: to shorter contexts, with the masked symbols left out.
        loop {
            let mut guard = 0;
            loop {
                self.order_fall += 1;
                let s = self.suffix(self.min_ctx);
                if s == 0 || s <= self.text || s >= self.heap_end || guard > 300 {
                    return None;
                }
                self.min_ctx = s;
                guard += 1;
                if self.ns(s) != self.num_masked {
                    break;
                }
            }
            let c = self.min_ctx;
            let ns = self.ns(c);
            let diff = ns - self.num_masked;
            let (see, esc) = self.make_esc_freq(diff);
            let mut list = Vec::with_capacity(diff as usize);
            let mut p = self.stats(c);
            let mut hi = 0;
            let mut k = 0;
            while list.len() < diff as usize && k < 256 {
                if self.char_mask[self.sym(p) as usize] != self.esc_count {
                    hi += self.freq(p);
                    list.push(p);
                }
                p += 6;
                k += 1;
            }
            let scale = esc + hi;
            let count = rc.threshold(scale);
            if count >= scale {
                return None;
            }
            if count < hi {
                let mut h = 0;
                for &s in &list {
                    let f = self.freq(s);
                    h += f;
                    if h > count {
                        rc.decode(h - f, f);
                        self.see_mut(see).update();
                        self.update2(s);
                        let sym = self.sym(self.found) as u8;
                        self.next_context();
                        return Some(sym);
                    }
                }
                return None;
            }
            rc.decode(hi, scale - hi);
            for &s in &list {
                let sym = self.sym(s) as usize;
                self.char_mask[sym] = self.esc_count;
            }
            let summ = self.see_mut(see).summ;
            self.see_mut(see).summ = summ.wrapping_add(scale as u16);
            self.num_masked = ns;
        }
    }

    fn update1(&mut self, p: u32) {
        let c = self.min_ctx;
        self.found = p;
        self.set_freq(p, self.freq(p) + 4);
        self.set_summ(c, self.summ(c) + 4);
        if self.freq(p) > self.freq(p - 6) {
            self.swap_states(p, p - 6);
            self.found = p - 6;
            if self.freq(p - 6) > MAX_FREQ {
                self.rescale();
            }
        }
    }

    fn update2(&mut self, p: u32) {
        let c = self.min_ctx;
        self.found = p;
        self.set_freq(p, self.freq(p) + 4);
        self.set_summ(c, self.summ(c) + 4);
        if self.freq(p) > MAX_FREQ {
            self.rescale();
        }
        self.esc_count = self.esc_count.wrapping_add(1);
        self.run_length = self.init_rl;
    }

    /// Encodes `symbol` (`None`: the end mark), mirroring `decode`.
    #[cfg(test)]
    pub fn encode(&mut self, rc: &mut dyn RangeEncoder, symbol: Option<u8>) {
        let want = symbol.map_or(u32::MAX, |s| s as u32);
        let c = self.min_ctx;
        let ns = self.ns(c);
        if ns != 1 {
            let stats = self.stats(c);
            let scale = self.summ(c);
            let mut p = stats;
            if self.sym(p) == want {
                let hi = self.freq(p);
                rc.encode(0, hi, scale);
                self.prev_success = (2 * hi > scale) as u32;
                self.run_length += self.prev_success as i32;
                self.found = p;
                self.set_freq(p, hi + 4);
                self.set_summ(c, scale + 4);
                if hi + 4 > MAX_FREQ {
                    self.rescale();
                }
                return self.next_context();
            }
            self.prev_success = 0;
            let mut low = self.freq(p);
            for _ in 1..ns {
                p += 6;
                if self.sym(p) == want {
                    rc.encode(low, self.freq(p), scale);
                    self.update1(p);
                    return self.next_context();
                }
                low += self.freq(p);
            }
            self.hi_bits_flag = self.hb2flag[self.sym(self.found) as usize] as u32;
            rc.encode(low, scale - low, scale);
            self.num_masked = ns;
            for k in 0..ns {
                let s = self.sym(stats + k * 6) as usize;
                self.char_mask[s] = self.esc_count;
            }
            self.found = 0;
        } else {
            let rs = Self::one_state(c);
            self.hi_bits_flag = self.hb2flag[self.sym(self.found) as usize] as u32;
            let (i, j) = self.bin_index();
            let bs = self.bin_summ[i][j] as u32;
            if self.sym(rs) == want {
                rc.encode_bin(bs, true);
                self.bin_summ[i][j] = (bs + INTERVAL - ((bs + 32) >> 7)) as u16;
                self.found = rs;
                let f = self.freq(rs);
                self.set_freq(rs, f + (f < 128) as u32);
                self.prev_success = 1;
                self.run_length += 1;
                return self.next_context();
            }
            rc.encode_bin(bs, false);
            let bs = bs - ((bs + 32) >> 7);
            self.bin_summ[i][j] = bs as u16;
            self.init_esc = EXP_ESCAPE[(bs >> 10) as usize] as u32;
            self.num_masked = 1;
            self.char_mask[self.sym(rs) as usize] = self.esc_count;
            self.prev_success = 0;
            self.found = 0;
        }
        loop {
            loop {
                self.order_fall += 1;
                let s = self.suffix(self.min_ctx);
                if s == 0 {
                    return; // the end mark: an escape from order -1
                }
                self.min_ctx = s;
                if self.ns(s) != self.num_masked {
                    break;
                }
            }
            let c = self.min_ctx;
            let ns = self.ns(c);
            let diff = ns - self.num_masked;
            let (see, esc) = self.make_esc_freq(diff);
            let mut list = Vec::new();
            let mut p = self.stats(c);
            let mut hi = 0;
            while list.len() < diff as usize {
                if self.char_mask[self.sym(p) as usize] != self.esc_count {
                    hi += self.freq(p);
                    list.push(p);
                }
                p += 6;
            }
            let scale = esc + hi;
            let mut low = 0;
            for &s in &list {
                if self.sym(s) == want {
                    rc.encode(low, self.freq(s), scale);
                    self.see_mut(see).update();
                    self.update2(s);
                    return self.next_context();
                }
                low += self.freq(s);
            }
            rc.encode(hi, scale - hi, scale);
            for &s in &list {
                let sym = self.sym(s) as usize;
                self.char_mask[sym] = self.esc_count;
            }
            let summ = self.see_mut(see).summ;
            self.see_mut(see).summ = summ.wrapping_add(scale as u16);
            self.num_masked = ns;
        }
    }
}

/// RAR's range encoder (tests).
#[cfg(test)]
#[derive(Default)]
pub(crate) struct RarEncoder {
    low: u32,
    range: u32,
    pub out: Vec<u8>,
}

#[cfg(test)]
impl RarEncoder {
    pub fn new() -> RarEncoder {
        RarEncoder { low: 0, range: u32::MAX, out: Vec::new() }
    }

    fn normalize(&mut self) {
        loop {
            if (self.low ^ self.low.wrapping_add(self.range)) >= TOP {
                if self.range >= BOT {
                    break;
                }
                self.range = self.low.wrapping_neg() & (BOT - 1);
            }
            self.out.push((self.low >> 24) as u8);
            self.range <<= 8;
            self.low <<= 8;
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        for _ in 0..4 {
            self.out.push((self.low >> 24) as u8);
            self.low <<= 8;
        }
        self.out
    }
}

#[cfg(test)]
impl RangeEncoder for RarEncoder {
    fn encode(&mut self, low: u32, size: u32, total: u32) {
        self.range /= total;
        self.low = self.low.wrapping_add(self.range.wrapping_mul(low));
        self.range = self.range.wrapping_mul(size);
        self.normalize();
    }

    fn encode_bin(&mut self, size0: u32, first: bool) {
        self.range >>= TOT_BITS;
        let (low, size) = if first { (0, size0) } else { (size0, BIN_SCALE - size0) };
        self.low = self.low.wrapping_add(self.range.wrapping_mul(low));
        self.range = self.range.wrapping_mul(size);
        self.normalize();
    }
}

/// 7z's PPMD coder: `props` (order, memory size), `size` bytes out.
pub(crate) fn decode_7z(props: &[u8], input: &[u8], size: usize) -> Option<Vec<u8>> {
    if props.len() < 5 {
        return None;
    }
    let order = props[0] as u32;
    let mem = u32::from_le_bytes(props[1..5].try_into().unwrap());
    if !(2..=64).contains(&order) || !(1 << 11..=0xFFFF_FFFF - 36).contains(&mem) {
        return None;
    }
    let mut model = Model::new(mem, order);
    let mut rc = SevenZipCoder::new(input)?;
    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        out.push(model.decode(&mut rc)?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the RAR coder encodes, it decodes (model and coder together).
    #[test]
    fn rar_coder_round_trip() {
        let mut text = Vec::new();
        let mut x = 5u32;
        while text.len() < 300_000 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            let words = ["the ", "model ", "of ", "order ", "seven ", "\n", "PPMd ", "escape "];
            text.extend_from_slice(words[(x >> 16) as usize % words.len()].as_bytes());
            if x.is_multiple_of(97) {
                text.push((x >> 8) as u8);
            }
        }
        // A small memory too: the model restarts when it is full.
        for (mem, order) in [(16 << 20, 6), (1 << 16, 4), (4 << 20, 16)] {
            let mut enc = RarEncoder::new();
            let mut m = Model::new(mem, order);
            for &b in &text {
                m.encode(&mut enc, Some(b));
            }
            let data = enc.finish();
            let mut m = Model::new(mem, order);
            let mut src = data.iter();
            let mut c = RarCoder::new(&mut src);
            let mut out = Vec::new();
            while out.len() < text.len() {
                let mut d = RarDec { c: &mut c, src: &mut src };
                out.push(m.decode(&mut d).expect("a symbol"));
            }
            assert!(out == text, "mem {mem} order {order}");
        }
    }
}
