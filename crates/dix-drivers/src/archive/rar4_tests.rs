//! RAR 4 (RAR 2.9 compression) archives made here, by a small encoder of
//! the format, since no rar that writes RAR 4 runs here: unrar must accept
//! each (it tests the encoder against the real decoder), and the driver must
//! read them back (the decoder here against the same data).

use super::*;
use crate::archive::ppmd::{Model, RarEncoder};
use crate::archive::rar::v3_for_tests::{STANDARD, Std, run_filter};

/// Bits, most significant first.
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn put(&mut self, v: u32, bits: u32) {
        if bits == 0 {
            return;
        }
        self.acc = (self.acc << bits) | (v as u64 & ((1u64 << bits) - 1));
        self.n += bits;
        while self.n >= 8 {
            self.out.push((self.acc >> (self.n - 8)) as u8);
            self.n -= 8;
        }
    }
    fn align(&mut self) {
        if !self.n.is_multiple_of(8) {
            self.put(0, 8 - self.n % 8);
        }
    }
    fn bytes(&mut self, b: &[u8]) {
        self.align();
        self.out.extend_from_slice(b);
    }
    fn finish(mut self) -> Vec<u8> {
        self.align();
        // (Padding: the decoder may look a little ahead.)
        self.out
    }
}

/// Huffman code lengths of at most 15 bits for `freq` (every symbol gets
/// a code: a table kept for the next file must cover what it needs).
fn lengths(freq: &[u32]) -> Vec<u8> {
    let mut f: Vec<u64> = freq.iter().map(|&x| x as u64 + 1).collect();
    loop {
        // Nodes: leaves, then merged pairs (weight, children).
        let n = f.len();
        let mut weight: Vec<u64> = f.clone();
        let mut kids: Vec<(usize, usize)> = vec![(usize::MAX, usize::MAX); n];
        let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<(u64, usize)>> =
            (0..n).map(|i| std::cmp::Reverse((f[i], i))).collect();
        while heap.len() > 1 {
            let std::cmp::Reverse((a, x)) = heap.pop().unwrap();
            let std::cmp::Reverse((b, y)) = heap.pop().unwrap();
            weight.push(a + b);
            kids.push((x, y));
            heap.push(std::cmp::Reverse((a + b, weight.len() - 1)));
        }
        let mut len = vec![0u8; n];
        let mut stack = vec![(weight.len() - 1, 0u32)];
        let mut max = 0;
        while let Some((node, d)) = stack.pop() {
            if node < n {
                len[node] = d.max(1) as u8;
                max = max.max(d);
            } else {
                stack.push((kids[node].0, d + 1));
                stack.push((kids[node].1, d + 1));
            }
        }
        if max <= 15 {
            return len;
        }
        for x in f.iter_mut() {
            *x = (*x).div_ceil(2);
        }
    }
}

/// Canonical codes (most significant bit first) for `len`.
fn codes(len: &[u8]) -> Vec<u32> {
    let mut count = [0u32; 17];
    for &l in len {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut next = [0u32; 17];
    let mut c = 0;
    for l in 1..17 {
        c = (c + count[l - 1]) << 1;
        next[l] = c;
    }
    len.iter()
        .map(|&l| {
            let v = next[l as usize];
            next[l as usize] += 1;
            v
        })
        .collect()
}

const NC: usize = 299;
const DC: usize = 60;
const LDC: usize = 17;
const RC: usize = 28;
const TABLE: usize = NC + DC + LDC + RC;
const LDECODE: [u32; 28] =
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224];
const LBITS: [u32; 28] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5];
const SDDECODE: [u32; 8] = [0, 4, 8, 16, 32, 64, 128, 192];
const SDBITS: [u32; 8] = [2, 2, 3, 4, 5, 6, 6, 6];

fn dist_slots() -> Vec<(u32, u32)> {
    let counts = [4, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 14, 0, 12];
    let mut t = Vec::new();
    let mut d = 0u32;
    for (bits, &n) in counts.iter().enumerate() {
        for _ in 0..n {
            t.push((d, bits as u32));
            d += 1 << bits;
        }
    }
    t
}

fn length_code(base: u32) -> Option<(usize, u32)> {
    (0..28).rev().find(|&n| base >= LDECODE[n] && base - LDECODE[n] < 1 << LBITS[n]).map(|n| (n, base - LDECODE[n]))
}

/// One symbol of an LZ block and its extra bits (written after its code).
#[derive(Clone)]
enum Sym {
    /// A main-table symbol, then extra bits (value, count).
    Main(usize, Vec<(u32, u32)>),
    /// A distance slot, then extra bits, then maybe a low-distance symbol.
    Dist(usize, Vec<(u32, u32)>, Option<usize>),
    /// A repeat-length symbol and its extra bits.
    Rep(usize, u32, u32),
    /// Raw bits (the end-of-block flags, a filter's bytes).
    Raw(u32, u32),
}

/// The LZ state the decoder keeps (to mirror it).
#[derive(Default, Clone)]
struct LzState {
    old_dist: [u64; 4],
    last_len: u64,
    old_table: Vec<u8>,
}

/// LZ-codes `data[start..]` into symbols (greedy matches over a hash
/// chain, also into `data[..start]`, the history), using every kind of
/// match the format has, mirroring the decoder's state in `st`.
fn lz(data: &[u8], start: usize, st: &mut LzState, out: &mut Vec<Sym>) {
    let slots = dist_slots();
    let mut chains: std::collections::HashMap<[u8; 3], Vec<usize>> = std::collections::HashMap::new();
    for i in start.saturating_sub(1 << 20)..start {
        if i + 3 <= data.len() {
            chains.entry([data[i], data[i + 1], data[i + 2]]).or_default().push(i);
        }
    }
    let mut count = 0usize;
    let mut p = start;
    while p < data.len() {
        let mut best = (0usize, 0usize);
        let try_dist = |d: usize, best: &mut (usize, usize)| {
            if d == 0 || d > p || d > (4 << 20) - 0x2000 {
                return;
            }
            let mut l = 0;
            while p + l < data.len() && l < 255 && data[p + l] == data[p + l - d] {
                l += 1;
            }
            if l > best.0 {
                *best = (l, d);
            }
        };
        for &d in &st.old_dist {
            try_dist(d as usize, &mut best);
        }
        if p + 3 <= data.len()
            && let Some(c) = chains.get(&[data[p], data[p + 1], data[p + 2]])
        {
            for &q in c.iter().rev().take(16) {
                try_dist(p - q, &mut best);
            }
        }
        let (len, dist) = best;
        let (l64, d64) = (len as u64, dist as u64);
        let mut used = 0;
        count += 1;
        if len >= 2 {
            if d64 == st.old_dist[0] && l64 == st.last_len && count.is_multiple_of(3) {
                out.push(Sym::Main(258, vec![]));
                used = len;
            } else if let Some(k) = st.old_dist.iter().position(|&o| o == d64)
                && let Some((n, extra)) = length_code(len as u32 - 2)
            {
                out.push(Sym::Main(259 + k, vec![]));
                out.push(Sym::Rep(n, extra, LBITS[n]));
                for i in (1..=k).rev() {
                    st.old_dist[i] = st.old_dist[i - 1];
                }
                st.old_dist[0] = d64;
                st.last_len = l64;
                used = len;
            } else if len == 2 || (len < 5 && dist <= 256 && count.is_multiple_of(2)) {
                if dist <= 256 {
                    let v = dist as u32 - 1;
                    let n = (0..8).rev().find(|&n| v >= SDDECODE[n]).unwrap();
                    out.push(Sym::Main(263 + n, vec![(v - SDDECODE[n], SDBITS[n])]));
                    st.old_dist = [d64, st.old_dist[0], st.old_dist[1], st.old_dist[2]];
                    st.last_len = 2;
                    used = 2;
                }
            } else {
                let bonus = (dist >= 0x2000) as u32 + (dist >= 0x40000) as u32;
                if len as u32 >= 3 + bonus
                    && let Some((n, extra)) = length_code(len as u32 - 3 - bonus)
                {
                    let v = dist as u32 - 1;
                    let slot = (0..DC).rev().find(|&s| v >= slots[s].0).unwrap();
                    let (base, bits) = slots[slot];
                    let x = v - base;
                    let mut ex = vec![];
                    let mut low = None;
                    if bits > 0 {
                        if slot > 9 {
                            if bits > 4 {
                                ex.push((x >> 4, bits - 4));
                            }
                            low = Some((x & 15) as usize);
                        } else {
                            ex.push((x, bits));
                        }
                    }
                    out.push(Sym::Main(271 + n, vec![(extra, LBITS[n])]));
                    out.push(Sym::Dist(slot, ex, low));
                    st.old_dist = [d64, st.old_dist[0], st.old_dist[1], st.old_dist[2]];
                    st.last_len = len as u64;
                    used = len;
                }
            }
        }
        let step = if used > 0 { used } else { 1 };
        if used == 0 {
            out.push(Sym::Main(data[p] as usize, vec![]));
        }
        for i in p..p + step {
            if i + 3 <= data.len() {
                chains.entry([data[i], data[i + 1], data[i + 2]]).or_default().push(i);
            }
        }
        p += step;
    }
}

/// Writes tables for `syms` and then them (an LZ block); `keep`: the
/// lengths are differences to the last table.
fn lz_block(b: &mut Bits, syms: &[Sym], st: &mut LzState, keep: bool) {
    let mut freq = vec![0u32; TABLE];
    for s in syms {
        match s {
            Sym::Main(n, _) => freq[*n] += 1,
            Sym::Dist(slot, _, low) => {
                freq[NC + slot] += 1;
                if let Some(l) = low {
                    freq[NC + DC + l] += 1;
                }
            }
            Sym::Rep(n, _, _) => freq[NC + DC + LDC + n] += 1,
            Sym::Raw(..) => {}
        }
    }
    let mut len = Vec::with_capacity(TABLE);
    len.extend(lengths(&freq[..NC]));
    len.extend(lengths(&freq[NC..NC + DC]));
    len.extend(lengths(&freq[NC + DC..NC + DC + LDC]));
    len.extend(lengths(&freq[NC + DC + LDC..]));
    // The table: as differences, runs of zeros (18/19) and repeats (16/17).
    if !keep || st.old_table.len() != TABLE {
        st.old_table = vec![0; TABLE];
    }
    let mut tsyms: Vec<(usize, u32, u32)> = Vec::new();
    let mut i = 0;
    while i < TABLE {
        let mut run = 1;
        while i + run < TABLE && len[i + run] == len[i] && run < 138 {
            run += 1;
        }
        if len[i] == 0 && run >= 3 {
            if run <= 10 {
                tsyms.push((18, (run - 3) as u32, 3));
            } else {
                tsyms.push((19, (run - 11) as u32, 7));
            }
            i += run;
        } else if i > 0 && len[i] == len[i - 1] && run >= 3 {
            if run <= 10 {
                tsyms.push((16, (run - 3) as u32, 3));
            } else {
                tsyms.push((17, (run - 11) as u32, 7));
            }
            i += run;
        } else {
            tsyms.push((((len[i] as i32 - st.old_table[i] as i32) & 0xF) as usize, 0, 0));
            i += 1;
        }
    }
    let mut bf = [0u32; 20];
    for t in &tsyms {
        bf[t.0] += 1;
    }
    let bl = lengths(&bf);
    let bc = codes(&bl);
    b.align();
    b.put(0, 1); // LZ
    b.put(keep as u32, 1);
    let mut i = 0;
    while i < 20 {
        if bl[i] == 0 {
            let mut z = 1;
            while i + z < 20 && bl[i + z] == 0 && z < 17 {
                z += 1;
            }
            if z >= 3 {
                b.put(15, 4);
                b.put((z - 2) as u32, 4);
                i += z;
                continue;
            }
        }
        if bl[i] == 15 {
            b.put(15, 4);
            b.put(0, 4);
        } else {
            b.put(bl[i] as u32, 4);
        }
        i += 1;
    }
    for &(s, v, n) in &tsyms {
        b.put(bc[s], bl[s] as u32);
        b.put(v, n);
    }
    st.old_table = len.clone();
    let ranges = [(0, NC), (NC, NC + DC), (NC + DC, NC + DC + LDC), (NC + DC + LDC, TABLE)];
    let cs: Vec<Vec<u32>> = ranges.iter().map(|&(a, z)| codes(&len[a..z])).collect();
    for s in syms {
        match s {
            Sym::Main(n, ex) => {
                b.put(cs[0][*n], len[*n] as u32);
                for &(v, k) in ex {
                    b.put(v, k);
                }
            }
            Sym::Dist(slot, ex, low) => {
                b.put(cs[1][*slot], len[NC + slot] as u32);
                for &(v, k) in ex {
                    b.put(v, k);
                }
                if let Some(l) = low {
                    b.put(cs[2][*l], len[NC + DC + l] as u32);
                }
            }
            Sym::Rep(n, v, k) => {
                b.put(cs[3][*n], len[NC + DC + LDC + n] as u32);
                b.put(*v, *k);
            }
            Sym::Raw(v, k) => b.put(*v, *k),
        }
    }
}

/// A VM number, as filter definitions write them.
fn vm_number(b: &mut Bits, v: u32) {
    if v < 16 {
        b.put(0, 2);
        b.put(v, 4);
    } else if v < 256 {
        b.put(1, 2);
        b.put(v, 8);
    } else if v < 0x10000 {
        b.put(2, 2);
        b.put(v, 16);
    } else {
        b.put(3, 2);
        b.put(v >> 16, 16);
        b.put(v & 0xFFFF, 16);
    }
}

/// VM code of the length and CRC unRAR knows a standard filter by (its
/// first byte is the XOR of the rest): CRC-32 is affine, so 40 free bits
/// are solved for.
fn forge(kind: Std) -> Vec<u8> {
    let (size, crc, _) = *STANDARD.iter().find(|s| s.2 == kind).unwrap();
    let mut code: Vec<u8> = (0..size).map(|i| (i as u8).wrapping_mul(37).wrapping_add(11)).collect();
    let fix = |c: &mut Vec<u8>| {
        c[0] = c[1..].iter().fold(0, |a, &x| a ^ x);
    };
    let eval = |c: &[u8]| {
        let mut c = c.to_vec();
        fix(&mut c);
        crc32(&c)
    };
    // Columns: what flipping each of the 40 bits (bytes 1..6) does.
    let base = eval(&code);
    let mut cols: Vec<(u32, u64)> = (0..40)
        .map(|bit| {
            let mut c = code.clone();
            c[1 + bit / 8] ^= 1 << (bit % 8);
            (eval(&c) ^ base, 1u64 << bit)
        })
        .collect();
    // Gaussian elimination over GF(2).
    let mut target = base ^ crc;
    let mut chosen = 0u64;
    let mut rows: Vec<(u32, u64)> = Vec::new();
    for (mut v, mut m) in cols.drain(..) {
        for &(rv, rm) in &rows {
            if v ^ rv < v {
                v ^= rv;
                m ^= rm;
            }
        }
        if v != 0 {
            rows.push((v, m));
            rows.sort_by_key(|r| std::cmp::Reverse(r.0));
        }
    }
    for &(rv, rm) in &rows {
        if target ^ rv < target {
            target ^= rv;
            chosen ^= rm;
        }
    }
    assert_eq!(target, 0, "no solution");
    for bit in 0..40 {
        if chosen & (1 << bit) != 0 {
            code[1 + bit / 8] ^= 1 << (bit % 8);
        }
    }
    fix(&mut code);
    assert_eq!(crc32(&code), crc);
    code
}

fn crc32(b: &[u8]) -> u32 {
    super::crc(b)
}

/// A filter definition at the current position (symbol 257 and its bytes).
fn filter_def(syms: &mut Vec<Sym>, num: Option<u32>, len: u32, regs: &[(usize, u32)], code: Option<&[u8]>) {
    let mut first = 0x20u8; // explicit length
    if num.is_some() {
        first |= 0x80;
    }
    if !regs.is_empty() {
        first |= 0x10;
    }
    let mut d = Bits::default();
    if let Some(n) = num {
        vm_number(&mut d, n);
    }
    vm_number(&mut d, 0); // block start: here
    vm_number(&mut d, len);
    if !regs.is_empty() {
        let mask = regs.iter().fold(0u32, |m, &(i, _)| m | 1 << i);
        d.put(mask, 7);
        for &(_, v) in regs {
            vm_number(&mut d, v);
        }
    }
    if let Some(c) = code {
        vm_number(&mut d, c.len() as u32);
        for &x in c {
            d.put(x as u32, 8);
        }
    }
    let body = d.finish();
    let n = body.len();
    let lenbits = if n <= 6 {
        (n - 1) as u8
    } else if n <= 262 {
        6
    } else {
        7
    };
    syms.push(Sym::Main(257, vec![]));
    syms.push(Sym::Raw((first | lenbits) as u32, 8));
    if lenbits == 6 {
        syms.push(Sym::Raw((n - 7) as u32, 8));
    } else if lenbits == 7 {
        syms.push(Sym::Raw(n as u32, 16));
    }
    for &x in &body {
        syms.push(Sym::Raw(x as u32, 8));
    }
}

/// The end of a block: a new file (`new_table`: the next brings tables) or,
/// in the same file, new tables.
fn end_block(syms: &mut Vec<Sym>, new_file: bool, new_table: bool) {
    syms.push(Sym::Main(256, vec![]));
    if new_file {
        syms.push(Sym::Raw(new_table as u32, 2));
    } else {
        syms.push(Sym::Raw(1, 1));
    }
}

/// A RAR 4 archive of (name, packed data, size, crc) members.
fn rar4(members: &[(String, Vec<u8>, usize, u32)], solid: bool) -> Vec<u8> {
    rar4_with(members, solid, None)
}

/// As `rar4`, the data encrypted with `password` (AES-128, RAR 3's key).
fn rar4_with(members: &[(String, Vec<u8>, usize, u32)], solid: bool, password: Option<&str>) -> Vec<u8> {
    fn header(kind: u8, flags: u16, body: &[u8]) -> Vec<u8> {
        let mut h = vec![0, 0, kind];
        h.extend_from_slice(&flags.to_le_bytes());
        h.extend_from_slice(&((7 + body.len()) as u16).to_le_bytes());
        h.extend_from_slice(body);
        let crc = (crc32(&h[2..]) & 0xFFFF) as u16;
        h[..2].copy_from_slice(&crc.to_le_bytes());
        h
    }
    let mut a = b"Rar!\x1a\x07\x00".to_vec();
    a.extend(header(0x73, if solid { 0x08 } else { 0 }, &[0; 6]));
    for (i, (name, packed, size, crc)) in members.iter().enumerate() {
        let salt = [i as u8 + 1, 2, 3, 4, 5, 6, 7, 8];
        let packed = match password {
            Some(pw) => {
                use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
                let (key, mut iv) = crate::archive::crypto::keys_rar3(pw, &salt);
                let cipher = aes::Aes128::new(GenericArray::from_slice(&key));
                let mut out = packed.clone();
                out.resize(out.len().next_multiple_of(16), 0);
                for block in out.chunks_exact_mut(16) {
                    for (b, v) in block.iter_mut().zip(iv) {
                        *b ^= v;
                    }
                    cipher.encrypt_block(GenericArray::from_mut_slice(block));
                    iv = block.try_into().unwrap();
                }
                out
            }
            None => packed.clone(),
        };
        let mut b = Vec::new();
        b.extend_from_slice(&(packed.len() as u32).to_le_bytes());
        b.extend_from_slice(&(*size as u32).to_le_bytes());
        b.push(3);
        b.extend_from_slice(&crc.to_le_bytes());
        let (time, date) = dix_core::time::to_dos(1_700_000_000);
        b.extend_from_slice(&((date as u32) << 16 | time as u32).to_le_bytes());
        b.push(29);
        b.push(0x33);
        b.extend_from_slice(&(name.len() as u16).to_le_bytes());
        b.extend_from_slice(&0o100644u32.to_le_bytes());
        b.extend_from_slice(name.as_bytes());
        // A 4 MiB dictionary; solid after the first.
        let mut flags = 0x8000 | (6 << 5) | if solid && i > 0 { 0x10 } else { 0 };
        if password.is_some() {
            b.extend_from_slice(&salt);
            flags |= 0x04 | 0x400;
        }
        a.extend(header(0x74, flags, &b));
        a.extend_from_slice(&packed);
    }
    a.extend(header(0x7B, 0x4000, &[]));
    a
}

/// Test data: text, an executable, noise, runs.
fn samples() -> Vec<(String, Vec<u8>)> {
    let words = ["alpha ", "beta ", "gamma ", "delta ", "archive ", "solid ", "the ", "\n", "RAR "];
    let mut x = 41u32;
    let mut text = Vec::new();
    while text.len() < 400_000 {
        x = x.wrapping_mul(1103515245).wrapping_add(12345);
        text.extend_from_slice(words[(x >> 16) as usize % words.len()].as_bytes());
    }
    let mut runs = vec![0u8; 70_000];
    runs.extend(b"abcabcabc".repeat(3000));
    runs.extend(noise(9000, 2));
    runs.extend(runs.clone()); // a far repeat (long distances)
    let exe = tool("ls").map(|p| std::fs::read(p).unwrap()).unwrap_or_else(|| noise(60_000, 6));
    vec![
        ("text.txt".into(), text),
        ("runs.bin".into(), runs),
        ("exe".into(), exe),
        ("noise.bin".into(), noise(30_000, 8)),
        ("small.txt".into(), b"hello rar 2.9\n".to_vec()),
    ]
}

/// Compresses files (LZ), with new tables every 20 000 symbols; solid:
/// the history and decoder state go on, and every other file keeps the
/// tables of the one before (its end says so).
fn compress_lz(files: &[(String, Vec<u8>)], solid: bool) -> Vec<(String, Vec<u8>, usize, u32)> {
    let mut st = LzState::default();
    let mut history: Vec<u8> = Vec::new();
    let mut out = Vec::new();
    for (fi, (name, data)) in files.iter().enumerate() {
        if !solid {
            st = LzState::default();
            history.clear();
        }
        let mut whole = history.clone();
        whole.extend_from_slice(data);
        let mut all = Vec::new();
        lz(&whole, history.len(), &mut st, &mut all);
        let mut b = Bits::default();
        let continuing = solid && fi % 2 == 1;
        // Blocks of about 20 000 symbols, cut only before a main symbol
        // (a match's distance belongs with its length).
        let mut chunks: Vec<&[Sym]> = Vec::new();
        let mut from = 0;
        for i in 0..=all.len() {
            let cut = i == all.len() || (i - from >= 20_000 && matches!(all[i], Sym::Main(..)));
            if cut && i > from {
                chunks.push(&all[from..i]);
                from = i;
            }
        }
        for (ci, chunk) in chunks.iter().enumerate() {
            let mut syms = chunk.to_vec();
            let last = ci + 1 == chunks.len();
            end_block(&mut syms, last, !(last && solid && fi % 2 == 0));
            if ci == 0 && continuing {
                // The previous file's tables go on: symbols only.
                let t = st.old_table.clone();
                write_syms(&mut b, &syms, &t);
            } else {
                lz_block(&mut b, &syms, &mut st, ci > 0 || (solid && fi > 0));
            }
        }
        out.push((name.clone(), b.finish(), data.len(), crc32(data)));
        history.extend_from_slice(data);
    }
    out
}

/// Writes symbols with the tables `len` (no table header).
fn write_syms(b: &mut Bits, syms: &[Sym], len: &[u8]) {
    let ranges = [(0, NC), (NC, NC + DC), (NC + DC, NC + DC + LDC), (NC + DC + LDC, TABLE)];
    let cs: Vec<Vec<u32>> = ranges.iter().map(|&(a, z)| codes(&len[a..z])).collect();
    for s in syms {
        match s {
            Sym::Main(n, ex) => {
                assert!(len[*n] > 0, "symbol {n} has no code in the kept tables");
                b.put(cs[0][*n], len[*n] as u32);
                for &(v, k) in ex {
                    b.put(v, k);
                }
            }
            Sym::Dist(slot, ex, low) => {
                assert!(len[NC + slot] > 0);
                b.put(cs[1][*slot], len[NC + slot] as u32);
                for &(v, k) in ex {
                    b.put(v, k);
                }
                if let Some(l) = low {
                    b.put(cs[2][*l], len[NC + DC + l] as u32);
                }
            }
            Sym::Rep(n, v, k) => {
                assert!(len[NC + DC + LDC + n] > 0);
                b.put(cs[3][*n], len[NC + DC + LDC + n] as u32);
                b.put(*v, *k);
            }
            Sym::Raw(v, k) => b.put(*v, *k),
        }
    }
}

fn check(s: &Scratch, name: &str, members: &[(String, Vec<u8>, usize, u32)], solid: bool, want: &[(String, Vec<u8>)]) {
    let img = s.0.join(name);
    std::fs::write(&img, rar4(members, solid)).unwrap();
    if have(&["unrar"]) {
        run(&s.0, "unrar", &["t", "-idq", name]);
    }
    let mut m = open(&img, AccessMode::ReadOnly).unwrap();
    for (n, d) in want {
        let got = m.fs.read_to_vec(&p(&format!("/{n}"))).unwrap_or_else(|e| panic!("{name}: {n}: {e}"));
        assert!(got == *d, "{name}: {n} differs");
    }
    // And backwards (a solid run starts again).
    for (n, d) in want.iter().rev() {
        let got = m.fs.read_to_vec(&p(&format!("/{n}"))).unwrap();
        assert!(got == *d, "{name}: {n} differs (backwards)");
    }
}

#[test]
fn rar4_lz() {
    let s = Scratch::new("rar4-lz");
    let files = samples();
    check(&s, "lz.rar", &compress_lz(&files, false), false, &files);
    check(&s, "lz-solid.rar", &compress_lz(&files, true), true, &files);
}

/// Each standard filter on data made for it: the encoder applies the
/// opposite transform, writes a filter definition (code with the length
/// and CRC unRAR knows), then the transformed bytes.
#[test]
fn rar4_filters() {
    let s = Scratch::new("rar4-filters");
    let exe = tool("ls").map(|p| std::fs::read(p).unwrap()).unwrap_or_else(|| noise(60_000, 6));
    let block = exe.len().min(0x30000);
    // (name, kind, registers, data, block start in the file)
    let mut audio = Vec::new();
    for i in 0..40_000u32 {
        let v = ((i as f64 / 20.0).sin() * 90.0 + (i as f64 / 7.0).cos() * 20.0) as i32;
        audio.push(v as u8);
    }
    let mut image = Vec::new();
    for y in 0..120u32 {
        for x in 0..100u32 {
            image.extend_from_slice(&[(x * 2) as u8, (y * 2 + x) as u8, (x ^ y) as u8]);
        }
    }
    let mut itanium = noise(32_000, 77);
    for (i, c) in itanium.chunks_mut(16).enumerate() {
        c[0] = 0x10 + (i % 16) as u8; // templates the filter looks at
    }
    let mut delta = Vec::new();
    for i in 0..30_000u32 {
        delta.extend_from_slice(&[(i / 3) as u8, (i / 5) as u8, (i * 3) as u8, 7]);
    }
    type Case<'a> = (&'a str, Std, Vec<(usize, u32)>, Vec<u8>, usize);
    let cases: Vec<Case> = vec![
        ("e8", Std::E8, vec![], exe[..block].to_vec(), 0),
        ("e8e9", Std::E8E9, vec![], exe[..block].to_vec(), 1000),
        ("itanium", Std::Itanium, vec![], itanium, 0),
        ("delta", Std::Delta, vec![(0, 4)], delta, 0),
        ("rgb", Std::Rgb, vec![(0, 300 + 3), (1, 0)], image, 0),
        ("audio", Std::Audio, vec![(0, 2)], audio, 0),
    ];
    for (name, kind, regs, data, at) in cases {
        // The file: some plain bytes, then the filtered block.
        let mut file = noise(at, 3);
        file.extend_from_slice(&data);
        let mut r = [0u32; 7];
        r[4] = data.len() as u32;
        r[6] = at as u32;
        for &(i, v) in &regs {
            r[i] = v;
        }
        let stored = forward(kind, &r, &data);
        assert!(run_filter(kind, &r, stored.clone()).unwrap() == data, "{name}: the forward transform");
        let mut syms = Vec::new();
        let mut st = LzState::default();
        let mut plain = file[..at].to_vec();
        lz(&plain, 0, &mut st, &mut syms);
        filter_def(&mut syms, Some(1), data.len() as u32, &regs, Some(&forge(kind)));
        plain.extend_from_slice(&stored);
        lz(&plain, at, &mut st, &mut syms);
        end_block(&mut syms, true, true);
        let mut b = Bits::default();
        let mut st2 = LzState::default();
        lz_block(&mut b, &syms, &mut st2, false);
        let members = vec![(name.to_string(), b.finish(), file.len(), crc32(&file))];
        check(&s, &format!("{name}.rar"), &members, false, &[(name.to_string(), file)]);
    }
}

/// The opposite of a standard filter: what to store so that it gives `d`.
fn forward(kind: Std, r: &[u32; 7], d: &[u8]) -> Vec<u8> {
    let size = d.len();
    match kind {
        Std::E8 | Std::E8E9 => {
            let mut s = d.to_vec();
            let cmp2 = if kind == Std::E8E9 { 0xE9 } else { 0xE8 };
            let mut pos = 0;
            while pos + 4 < size {
                let b = s[pos];
                pos += 1;
                if b == 0xE8 || b == cmp2 {
                    let want = u32::from_le_bytes(d[pos..pos + 4].try_into().unwrap());
                    let offset = (pos as u32).wrapping_add(r[6]);
                    let dec = |x: u32| {
                        if x & 0x8000_0000 != 0 {
                            if x.wrapping_add(offset) & 0x8000_0000 == 0 { x.wrapping_add(0x100_0000) } else { x }
                        } else if x.wrapping_sub(0x100_0000) & 0x8000_0000 != 0 {
                            x.wrapping_sub(offset)
                        } else {
                            x
                        }
                    };
                    let c = [want.wrapping_add(offset), want.wrapping_sub(0x100_0000), want]
                        .into_iter()
                        .find(|&c| dec(c) == want)
                        .expect("an address the filter cannot give");
                    s[pos..pos + 4].copy_from_slice(&c.to_le_bytes());
                    pos += 4;
                }
            }
            s
        }
        Std::Itanium => {
            // Adding is the opposite of the filter's subtracting.
            let mut s = d.to_vec();
            let mut fo = r[6] >> 4;
            let masks = [4u8, 4, 6, 6, 0, 0, 7, 7, 4, 4, 0, 0, 4, 4, 0, 0];
            let mut pos = 0;
            while pos + 21 < size {
                let byte = (s[pos] & 0x1F) as i32 - 0x10;
                if byte >= 0 {
                    for i in 0..3 {
                        if masks[byte as usize] & (1 << i) != 0 {
                            let st = i * 41 + 5;
                            let get = |b: &[u8], bit: usize, n: u32| {
                                let a = bit / 8;
                                (u32::from_le_bytes([b[a], b[a + 1], b[a + 2], b[a + 3]]) >> (bit & 7))
                                    & (u32::MAX >> (32 - n))
                            };
                            if get(&s[pos..], st + 37, 4) == 5 {
                                let off = get(&s[pos..], st + 13, 20);
                                let v = off.wrapping_add(fo) & 0xFFFFF;
                                let bit = st + 13;
                                let a = pos + bit / 8;
                                let mut and = !((u32::MAX >> 12) << (bit & 7));
                                let mut val = v << (bit & 7);
                                for k in 0..4 {
                                    s[a + k] &= and as u8;
                                    s[a + k] |= val as u8;
                                    and = (and >> 8) | 0xFF00_0000;
                                    val >>= 8;
                                }
                            }
                        }
                    }
                }
                pos += 16;
                fo = fo.wrapping_add(1);
            }
            s
        }
        Std::Delta => {
            let ch = r[0] as usize;
            let mut s = Vec::with_capacity(size);
            for c in 0..ch {
                let mut prev = 0u8;
                let mut p = c;
                while p < size {
                    s.push(prev.wrapping_sub(d[p]));
                    prev = d[p];
                    p += ch;
                }
            }
            s
        }
        Std::Rgb => {
            let width = r[0] as usize - 3;
            let pos_r = r[1] as usize;
            // Undo the green adjustment, then predict as the decoder does.
            let mut u = d.to_vec();
            let mut i = pos_r;
            while i + 2 < size {
                let g = u[i + 1];
                u[i] = u[i].wrapping_sub(g);
                u[i + 2] = u[i + 2].wrapping_sub(g);
                i += 3;
            }
            let mut s = Vec::with_capacity(size);
            for c in 0..3 {
                let mut prev = 0u32;
                let mut i = c;
                while i < size {
                    let mut pr = prev;
                    if i >= width + 3 {
                        let up = u[i - width] as u32;
                        let ul = u[i - width - 3] as u32;
                        pr = prev.wrapping_add(up).wrapping_sub(ul);
                        let pa = (pr.wrapping_sub(prev) as i32).unsigned_abs();
                        let pb = (pr.wrapping_sub(up) as i32).unsigned_abs();
                        let pc = (pr.wrapping_sub(ul) as i32).unsigned_abs();
                        pr = if pa <= pb && pa <= pc {
                            prev
                        } else if pb <= pc {
                            up
                        } else {
                            ul
                        };
                    }
                    s.push((pr as u8).wrapping_sub(u[i]));
                    prev = u[i] as u32;
                    i += 3;
                }
            }
            s
        }
        Std::Audio => {
            // Run the decoder's predictor; store what makes it give `d`.
            let ch = r[0] as usize;
            let mut s = Vec::with_capacity(size);
            for c in 0..ch {
                let (mut prev_byte, mut prev_delta) = (0u32, 0u32);
                let mut dif = [0u32; 7];
                let (mut d1, mut d2) = (0i32, 0i32);
                let mut d3: i32;
                let (mut k1, mut k2, mut k3) = (0i32, 0i32, 0i32);
                let mut i = c;
                let mut count = 0u32;
                while i < size {
                    d3 = d2;
                    d2 = prev_delta.wrapping_sub(d1 as u32) as i32;
                    d1 = prev_delta as i32;
                    let mut pr = (8u32.wrapping_mul(prev_byte))
                        .wrapping_add(k1.wrapping_mul(d1) as u32)
                        .wrapping_add(k2.wrapping_mul(d2) as u32)
                        .wrapping_add(k3.wrapping_mul(d3) as u32);
                    pr = (pr >> 3) & 0xFF;
                    let cur = pr.wrapping_sub(d[i] as u32) & 0xFF;
                    s.push(cur as u8);
                    let predicted = pr.wrapping_sub(cur);
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
                    i += ch;
                    count += 1;
                }
            }
            s
        }
    }
}

/// PPM blocks: text in PPM (with escapes and its two kinds of match), a
/// switch to LZ and back within a file, and a solid run that keeps the
/// model (a PPM block without reset).
#[test]
fn rar4_ppm() {
    let s = Scratch::new("rar4-ppm");
    let files = samples();
    let text = &files[0].1;
    let esc = 2u8;
    // One PPM-coded file: `order`, memory `mb` + 1 MiB.
    let ppm_file = |data: &[u8], order: u32, mb: u8, reset: bool, model: &mut Option<Model>, switch: bool| {
        let mut b = Bits::default();
        let ord_bits = if order > 16 { (order - 16) / 3 + 16 - 1 } else { order - 1 };
        let flags = 0x80 | if reset { 0x20 } else { 0 } | 0x40 | ord_bits as u8;
        let mut head = vec![flags];
        if reset {
            head.push(mb);
            *model = Some(Model::new((mb as u32 + 1) << 20, order));
        }
        head.push(esc);
        b.bytes(&head);
        let m = model.as_mut().unwrap();
        let mut enc = RarEncoder::new();
        let half = if switch { data.len() / 2 } else { data.len() };
        let mut i = 0;
        while i < half {
            let c = data[i];
            // A match now and then: esc 4 (distance, length) and esc 5 (runs).
            if i > 1000 && i % 997 == 0 {
                let (dist, len) = (300usize, 40usize);
                if i + len <= half && data[i..i + len] == data[i - dist..i - dist + len] {
                    m.encode(&mut enc, Some(esc));
                    m.encode(&mut enc, Some(4));
                    let dv = (dist - 2) as u32;
                    for x in [(dv >> 16) as u8, (dv >> 8) as u8, dv as u8, (len - 32) as u8] {
                        m.encode(&mut enc, Some(x));
                    }
                    i += len;
                    continue;
                }
            }
            if c == esc {
                m.encode(&mut enc, Some(esc));
                m.encode(&mut enc, Some(1));
            } else {
                m.encode(&mut enc, Some(c));
            }
            i += 1;
        }
        if switch {
            // To LZ for the rest: esc 0, then LZ tables.
            m.encode(&mut enc, Some(esc));
            m.encode(&mut enc, Some(0));
            b.bytes(&enc.finish());
            let mut syms = Vec::new();
            let mut st = LzState::default();
            lz(data, half, &mut st, &mut syms);
            end_block(&mut syms, true, true);
            lz_block(&mut b, &syms, &mut st, false);
        } else {
            m.encode(&mut enc, Some(esc));
            m.encode(&mut enc, Some(2));
            b.bytes(&enc.finish());
        }
        b.finish()
    };
    let mut model = None;
    let one = vec![("text.txt".to_string(), ppm_file(text, 6, 15, true, &mut model, false), text.len(), crc32(text))];
    check(&s, "ppm.rar", &one, false, &files[..1]);
    let mut model = None;
    // Order 22 (above 16 only every third order exists) in 1 MiB: the model
    // fills and restarts; half the file is LZ.
    let small = vec![("text.txt".to_string(), ppm_file(text, 22, 0, true, &mut model, true), text.len(), crc32(text))];
    check(&s, "ppm-small-mem.rar", &small, false, &files[..1]);
    // A solid run in PPM: the second file goes on with the model.
    let mut model = None;
    let a = ppm_file(&text[..100_000], 8, 4, true, &mut model, false);
    let b = ppm_file(&text[100_000..], 8, 4, false, &mut model, false);
    let parts = vec![("a.txt".to_string(), text[..100_000].to_vec()), ("b.txt".to_string(), text[100_000..].to_vec())];
    let members = vec![
        ("a.txt".to_string(), a, 100_000, crc32(&text[..100_000])),
        ("b.txt".to_string(), b, text.len() - 100_000, crc32(&text[100_000..])),
    ];
    check(&s, "ppm-solid.rar", &members, true, &parts);
}

/// Compressed files with a password (AES-128): RAR 4 has no password
/// check, so the driver decompresses with each password and the CRC
/// decides; solid too.
#[test]
fn rar4_compressed_with_a_password() {
    let s = Scratch::new("rar4-lz-pass");
    let files: Vec<(String, Vec<u8>)> = samples().into_iter().take(3).collect();
    for solid in [false, true] {
        let name = if solid { "solid-pass.rar" } else { "pass.rar" };
        let img = s.0.join(name);
        std::fs::write(&img, rar4_with(&compress_lz(&files, solid), solid, Some("secret"))).unwrap();
        if have(&["unrar"]) {
            run(&s.0, "unrar", &["t", "-idq", "-psecret", name]);
        }
        let mut m = open(&img, AccessMode::ReadOnly).unwrap();
        let e = m.fs.read_to_vec(&p(&format!("/{}", files[0].0))).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::PasswordRequired, "{name}: {e}");
        assert_eq!(m.fs.add_password("wrong").unwrap_err().kind(), ErrorKind::PasswordRequired, "{name}");
        m.fs.add_password("secret").unwrap_or_else(|e| panic!("{name}: {e}"));
        for (n, d) in files.iter().rev() {
            assert!(m.fs.read_to_vec(&p(&format!("/{n}"))).unwrap() == *d, "{name}: {n}");
        }
    }
}

/// Damaged data (bytes changed here and there) gives errors or wrong
/// output, never a crash or a hang.
#[test]
fn rar4_damage_is_an_error() {
    let s = Scratch::new("rar4-damage");
    let files: Vec<(String, Vec<u8>)> = samples().into_iter().skip(1).take(2).collect();
    let good = compress_lz(&files, false);
    let mut x = 7u32;
    for round in 0..150 {
        let mut members = good.clone();
        for m in members.iter_mut() {
            for _ in 0..1 + round % 4 {
                x = x.wrapping_mul(1103515245).wrapping_add(12345);
                let i = (x >> 8) as usize % m.1.len();
                m.1[i] ^= (x >> 24) as u8 | 1;
            }
        }
        let img = s.0.join("damaged.rar");
        std::fs::write(&img, rar4(&members, false)).unwrap();
        let mut m = open(&img, AccessMode::ReadOnly).unwrap();
        for (n, _) in &files {
            let _ = m.fs.read_to_vec(&p(&format!("/{n}")));
        }
    }
}
