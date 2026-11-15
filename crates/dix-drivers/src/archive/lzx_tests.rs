//! An LZX encoder and a Cabinet writer, for tests only: no tool here makes
//! LZX cabinets. What they make is first checked by cabextract (libmspack)
//! and then read by our decoder. The encoder uses everything the format
//! has: verbatim, aligned-offset and uncompressed blocks, the three
//! repeated offsets, all three run codes of the code-length pretree, and
//! x86 CALL translation.

const FRAME: usize = 32768;

/// Options for the stream.
#[derive(Clone, Copy)]
pub(super) struct Opts {
    pub window_bits: u32,
    /// The size given for x86 CALL translation (None: off).
    pub translate: Option<u32>,
    /// Output bytes per block; block kinds cycle verbatim, aligned,
    /// uncompressed (or without uncompressed ones).
    pub block: usize,
    pub uncompressed: bool,
}

struct W {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl W {
    fn put(&mut self, v: u32, bits: u32) {
        for i in (0..bits).rev() {
            self.acc = (self.acc << 1) | ((v >> i) & 1);
            self.n += 1;
            if self.n == 16 {
                self.out.extend((self.acc as u16).to_le_bytes());
                self.acc = 0;
                self.n = 0;
            }
        }
    }
    fn align(&mut self) {
        if self.n > 0 {
            self.put(0, 16 - self.n);
        }
    }
}

fn slots(bits: u32) -> usize {
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

/// Code lengths for `freq`, at most `max` bits, a complete code (two
/// symbols at least are given a code).
fn lengths(freq: &[u32], max: u32) -> Vec<u8> {
    let mut f: Vec<u64> = freq.iter().map(|&x| x as u64).collect();
    let used = f.iter().filter(|&&x| x > 0).count();
    for x in f.iter_mut().take(2 - used.min(2)) {
        *x = (*x).max(1);
    }
    if f.iter().filter(|&&x| x > 0).count() < 2 {
        f[1] = 1;
    }
    // Huffman by repeated merging.
    let mut nodes: Vec<(u64, Vec<usize>)> =
        f.iter().enumerate().filter(|e| *e.1 > 0).map(|(i, &x)| (x, vec![i])).collect();
    let mut len = vec![0u32; f.len()];
    while nodes.len() > 1 {
        nodes.sort_by_key(|n| std::cmp::Reverse(n.0));
        let (a, b) = (nodes.pop().unwrap(), nodes.pop().unwrap());
        for &s in a.1.iter().chain(&b.1) {
            len[s] += 1;
        }
        nodes.push((a.0 + b.0, [a.1, b.1].concat()));
    }
    // At most `max` bits, then complete again (Kraft sum exactly 1).
    let unit = |l: u32| 1u64 << (max - l);
    for l in len.iter_mut().filter(|l| **l > max) {
        *l = max;
    }
    let mut kraft: u64 = len.iter().filter(|&&l| l > 0).map(|&l| unit(l)).sum();
    while kraft > 1 << max {
        let i = (0..len.len()).filter(|&i| len[i] > 0 && len[i] < max).max_by_key(|&i| len[i]).unwrap();
        kraft -= unit(len[i]) - unit(len[i] + 1);
        len[i] += 1;
    }
    while kraft < 1 << max {
        let i =
            (0..len.len()).filter(|&i| len[i] > 1 && kraft + unit(len[i]) <= 1 << max).max_by_key(|&i| len[i]).unwrap();
        kraft += unit(len[i]);
        len[i] -= 1;
    }
    len.into_iter().map(|l| l as u8).collect()
}

/// Canonical codes for lengths (MSB first).
fn codes(lens: &[u8]) -> Vec<u32> {
    let mut count = [0u32; 18];
    for &l in lens {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut next = [0u32; 18];
    let mut code = 0;
    for l in 1..18 {
        code = (code + count[l - 1]) << 1;
        next[l] = code;
    }
    lens.iter()
        .map(|&l| {
            if l == 0 {
                0
            } else {
                let c = next[l as usize];
                next[l as usize] += 1;
                c
            }
        })
        .collect()
}

/// Writes `new[from..to]` against `prev` through a pretree, using the run
/// codes 17 and 18 (zeros) and 19 (four or five the same).
fn write_lengths(w: &mut W, prev: &[u8], new: &[u8], from: usize, to: usize) {
    // (symbol, extra bits value, extra bits count)
    let mut seq: Vec<(usize, u32, u32)> = Vec::new();
    let delta = |x: usize, v: u8| ((prev[x] as usize + 17 - v as usize) % 17, 0, 0);
    let mut x = from;
    while x < to {
        let zeros = new[x..to].iter().take_while(|&&v| v == 0).count();
        if zeros >= 20 {
            let n = zeros.min(51);
            seq.push((18, (n - 20) as u32, 5));
            x += n;
        } else if zeros >= 4 {
            seq.push((17, (zeros - 4) as u32, 4));
            x += zeros;
        } else {
            let same = new[x..to].iter().take_while(|&&v| v == new[x]).count().min(5);
            if same >= 4 && new[x] != 0 && prev[x..x + same].iter().all(|&p| p == prev[x]) {
                seq.push((19, (same - 4) as u32, 1));
                seq.push(delta(x, new[x]));
                x += same;
            } else {
                seq.push(delta(x, new[x]));
                x += 1;
            }
        }
    }
    let mut freq = [0u32; 20];
    for s in &seq {
        freq[s.0] += 1;
    }
    let pl = lengths(&freq, 15);
    let pc = codes(&pl);
    for &l in &pl {
        w.put(l as u32, 4);
    }
    for (s, v, n) in seq {
        w.put(pc[s], pl[s] as u32);
        w.put(v, n);
    }
}

enum Tok {
    Lit(u8),
    Match { len: usize, slot: usize, extra: u32, bits: u32 },
}

/// LZX of `data`: the compressed bytes of each 32 KiB frame (one Cabinet
/// data block each).
pub(super) fn compress(data: &[u8], o: Opts) -> Vec<Vec<u8>> {
    let nslots = slots(o.window_bits);
    let mut extra = vec![0u32; nslots];
    let mut base = vec![0u32; nslots];
    for i in 0..nslots {
        extra[i] = if i < 4 { 0 } else { ((i as u32 - 2) / 2).min(17) };
        if i > 0 {
            base[i] = base[i - 1] + (1 << extra[i - 1]);
        }
    }
    // x86 CALL translation first (what the decoder undoes).
    let mut input = data.to_vec();
    if let Some(size) = o.translate {
        for (f, frame) in input.chunks_mut(FRAME).enumerate() {
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
                let rel = i32::from_le_bytes(frame[i + 1..i + 5].try_into().unwrap()) as i64;
                if rel >= -cur && rel < size as i64 {
                    let abs = if rel + cur < size as i64 { rel + cur } else { rel - size as i64 };
                    frame[i + 1..i + 5].copy_from_slice(&(abs as i32).to_le_bytes());
                }
                i += 5;
            }
        }
    }
    let mut w = W { out: Vec::new(), acc: 0, n: 0 };
    w.put(o.translate.is_some() as u32, 1);
    if let Some(size) = o.translate {
        w.put(size >> 16, 16);
        w.put(size & 0xFFFF, 16);
    }
    let window = (1usize << o.window_bits) - 3;
    let mut r = [1usize; 3];
    let (mut main_prev, mut len_prev) = (vec![0u8; 256 + nslots * 8], vec![0u8; 249]);
    let mut frame_ends = Vec::new(); // byte positions where frames end
    let mut pos = 0usize;
    let mut head: std::collections::HashMap<[u8; 3], Vec<usize>> = Default::default();
    let mut kind_i = 0;
    while pos < input.len() {
        let end = (pos + o.block).min(input.len());
        let kinds: &[u32] = if o.uncompressed { &[1, 2, 3] } else { &[1, 2] };
        let kind = kinds[kind_i % kinds.len()];
        kind_i += 1;
        let size = end - pos;
        w.put(kind, 3);
        w.put((size >> 8) as u32, 16);
        w.put((size & 0xFF) as u32, 8);
        if kind == 3 {
            if w.n > 0 {
                w.align();
            } else {
                w.put(0, 16);
            }
            for v in r {
                w.out.extend((v as u32).to_le_bytes());
            }
            let start = w.out.len();
            w.out.extend_from_slice(&input[pos..end]);
            let mut f = (pos / FRAME + 1) * FRAME;
            while f <= end {
                frame_ends.push(start + (f - pos));
                f += FRAME;
            }
            if size % 2 == 1 {
                w.out.push(0);
            }
            for p in pos..end {
                if p + 3 <= input.len() {
                    head.entry(input[p..p + 3].try_into().unwrap()).or_default().push(p);
                }
            }
            pos = end;
            continue;
        }
        let aligned = kind == 2;
        // Tokens (greedy, never across a frame end).
        let mut toks = Vec::new();
        while pos < end {
            let frame_end = ((pos / FRAME + 1) * FRAME).min(end);
            let max = (frame_end - pos).min(257);
            let mut best = (0usize, 0usize);
            if max >= 2 && pos + 3 <= input.len() {
                let key: [u8; 3] = input[pos..pos + 3].try_into().unwrap();
                // The repeated offsets first, then the chain.
                let cands =
                    r.iter().copied().chain(head.get(&key).into_iter().flatten().rev().take(48).map(|&p| pos - p));
                for off in cands {
                    if off == 0 || off > pos || off > window {
                        continue;
                    }
                    let l = (0..max).take_while(|&k| input[pos + k] == input[pos - off + k]).count();
                    if l > best.0 {
                        best = (l, off);
                    }
                }
            }
            let take = if best.0 >= 3 { best.0 } else { 1 };
            if take == 1 {
                toks.push(Tok::Lit(input[pos]));
            } else {
                let off = best.1;
                let (slot, extra_v) = if off == r[0] {
                    (0, 0)
                } else if off == r[1] {
                    r.swap(0, 1);
                    (1, 0)
                } else if off == r[2] {
                    r.swap(0, 2);
                    (2, 0)
                } else {
                    let v = off as u32 + 2;
                    let s = (3..nslots).rev().find(|&s| base[s] <= v).unwrap();
                    r = [off, r[0], r[1]];
                    (s, v - base[s])
                };
                toks.push(Tok::Match { len: take, slot, extra: extra_v, bits: extra[slot] });
            }
            for p in pos..pos + take {
                if p + 3 <= input.len() {
                    head.entry(input[p..p + 3].try_into().unwrap()).or_default().push(p);
                }
            }
            pos += take;
        }
        // The trees.
        let (mut mf, mut lf, mut af) = (vec![0u32; 256 + nslots * 8], vec![0u32; 249], [0u32; 8]);
        for t in &toks {
            match *t {
                Tok::Lit(b) => mf[b as usize] += 1,
                Tok::Match { len, slot, extra, bits } => {
                    let l = len - 2;
                    mf[256 + slot * 8 + l.min(7)] += 1;
                    if l >= 7 {
                        lf[l - 7] += 1;
                    }
                    if aligned && bits >= 3 {
                        af[(extra & 7) as usize] += 1;
                    }
                }
            }
        }
        let (ml, ll) = (lengths(&mf, 16), lengths(&lf, 16));
        let al = lengths(&af, 7);
        if aligned {
            for &l in &al {
                w.put(l as u32, 3);
            }
        }
        write_lengths(&mut w, &main_prev, &ml, 0, 256);
        write_lengths(&mut w, &main_prev, &ml, 256, ml.len());
        write_lengths(&mut w, &len_prev, &ll, 0, 249);
        let (mc, lc, ac) = (codes(&ml), codes(&ll), codes(&al));
        let mut out_pos =
            end - toks.iter().map(|t| if let Tok::Match { len, .. } = t { *len } else { 1 }).sum::<usize>();
        for t in toks {
            match t {
                Tok::Lit(b) => {
                    w.put(mc[b as usize], ml[b as usize] as u32);
                    out_pos += 1;
                }
                Tok::Match { len, slot, extra, bits } => {
                    let l = len - 2;
                    let sym = 256 + slot * 8 + l.min(7);
                    w.put(mc[sym], ml[sym] as u32);
                    if l >= 7 {
                        w.put(lc[l - 7], ll[l - 7] as u32);
                    }
                    if aligned && bits >= 3 {
                        w.put(extra >> 3, bits - 3);
                        w.put(ac[(extra & 7) as usize], al[(extra & 7) as usize] as u32);
                    } else {
                        w.put(extra, bits);
                    }
                    out_pos += len;
                }
            }
            if out_pos.is_multiple_of(FRAME) {
                w.align();
                frame_ends.push(w.out.len());
            }
        }
        main_prev = ml;
        len_prev = ll;
    }
    w.align();
    if frame_ends.last() != Some(&w.out.len()) {
        frame_ends.push(w.out.len());
    }
    let mut chunks = Vec::new();
    let mut from = 0;
    for e in frame_ends {
        chunks.push(w.out[from..e].to_vec());
        from = e;
    }
    chunks
}

/// A Cabinet holding `files` in one folder: stored (`None`) or LZX.
pub(super) fn cabinet(files: &[(&str, Vec<u8>)], lzx: Option<Opts>) -> Vec<u8> {
    let stream: Vec<u8> = files.iter().flat_map(|f| f.1.iter().copied()).collect();
    let (method, chunks): (u16, Vec<(Vec<u8>, usize)>) = match lzx {
        Some(o) => {
            let c = compress(&stream, o);
            let sizes = stream.chunks(FRAME).map(|c| c.len());
            (3 | ((o.window_bits as u16) << 8), c.into_iter().zip(sizes).collect())
        }
        None => (0, stream.chunks(FRAME).map(|c| (c.to_vec(), c.len())).collect()),
    };
    let mut entries = Vec::new();
    let mut offset = 0u32;
    for (name, data) in files {
        let utf8 = !name.is_ascii();
        entries.extend((data.len() as u32).to_le_bytes());
        entries.extend(offset.to_le_bytes());
        entries.extend(0u16.to_le_bytes());
        entries.extend(0x5A21u16.to_le_bytes()); // 2025-01-01
        entries.extend(0x6000u16.to_le_bytes()); // 12:00
        entries.extend((0x20u16 | if utf8 { 0x80 } else { 0 }).to_le_bytes());
        entries.extend(name.replace('/', "\\").bytes());
        entries.push(0);
        offset += data.len() as u32;
    }
    let files_at = 36 + 8;
    let data_at = files_at + entries.len();
    let mut blocks = Vec::new();
    for (c, n) in &chunks {
        blocks.extend(0u32.to_le_bytes()); // no checksum
        blocks.extend((c.len() as u16).to_le_bytes());
        blocks.extend((*n as u16).to_le_bytes());
        blocks.extend(c);
    }
    let total = data_at + blocks.len();
    let mut out = b"MSCF".to_vec();
    out.extend(0u32.to_le_bytes());
    out.extend((total as u32).to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend((files_at as u32).to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend([3, 1]);
    out.extend(1u16.to_le_bytes());
    out.extend((files.len() as u16).to_le_bytes());
    out.extend([0u8; 6]);
    out.extend((data_at as u32).to_le_bytes());
    out.extend((chunks.len() as u16).to_le_bytes());
    out.extend(method.to_le_bytes());
    out.extend(entries);
    out.extend(blocks);
    out
}
