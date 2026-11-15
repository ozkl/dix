//! The codecs against Apple's reference encoder (the `lzfse` tool, which
//! writes LZVN blocks for small inputs and LZFSE blocks for larger ones),
//! and the decmpfs layouts built here around them.

use super::*;
use std::process::Command;

fn sample(n: usize, seed: u32) -> Vec<u8> {
    // Text with repeats near and far, and some noise.
    let mut x = seed;
    let words = ["alpha ", "beta ", "gamma ", "delta ", "epsilon\n", "zeta ", "eta, ", "theta. "];
    let mut v = Vec::with_capacity(n);
    while v.len() < n {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        if x.is_multiple_of(11) {
            v.push((x >> 8) as u8);
        } else {
            v.extend_from_slice(words[(x % 8) as usize].as_bytes());
        }
    }
    v.truncate(n);
    v
}

fn lzfse_tool(data: &[u8]) -> Option<Vec<u8>> {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("dix-lzfse-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let (i, o) = (dir.join("in"), dir.join("out"));
    std::fs::write(&i, data).ok()?;
    let ok = Command::new("lzfse").arg("-encode").arg("-i").arg(&i).arg("-o").arg(&o).status().ok()?.success();
    let out = std::fs::read(&o).ok();
    let _ = std::fs::remove_dir_all(&dir);
    if ok { out } else { None }
}

#[test]
fn lzfse_and_lzvn_streams_from_the_reference_encoder() {
    if Command::new("lzfse").arg("-h").output().is_err() {
        eprintln!("lzfse not installed: skipping");
        return;
    }
    for (n, seed) in
        [(0usize, 1u32), (1, 2), (100, 3), (3000, 4), (4095, 5), (5000, 6), (65536, 7), (300_000, 8), (2_000_000, 9)]
    {
        let data = sample(n, seed);
        let Some(packed) = lzfse_tool(&data) else {
            assert_eq!(n, 0, "the tool refuses only empty input");
            continue;
        };
        let mut out = Vec::new();
        lzfse::decode(&packed, &mut out, data.len()).unwrap_or_else(|e| panic!("{n} bytes: {e}"));
        assert!(out == data, "{n} bytes: {:?}", &packed[..4.min(packed.len())]);
    }
    // Incompressible data: stored blocks.
    let noise: Vec<u8> = (0..70_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let packed = lzfse_tool(&noise).unwrap();
    let mut out = Vec::new();
    lzfse::decode(&packed, &mut out, noise.len()).unwrap();
    assert!(out == noise);
}

#[test]
fn damaged_streams_are_errors_not_panics() {
    if Command::new("lzfse").arg("-h").output().is_err() {
        return;
    }
    let data = sample(200_000, 77);
    let packed = lzfse_tool(&data).unwrap();
    for cut in [5, 40, packed.len() / 2, packed.len() - 5] {
        let _ = lzfse::decode(&packed[..cut], &mut Vec::new(), data.len());
    }
    let mut x = 1u32;
    for _ in 0..200 {
        let mut p = packed.clone();
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let at = 12 + (x as usize >> 8) % (p.len() - 12);
        p[at] ^= 1 << (x % 8);
        let _ = lzfse::decode(&p, &mut Vec::new(), data.len());
    }
}

/// The LZVN payload of an LZFSE `bvxn` block, as a decmpfs attribute
/// holds it (type 7).
fn lzvn_chunk(data: &[u8]) -> Vec<u8> {
    let p = lzfse_tool(data).unwrap();
    assert_eq!(&p[..4], b"bvxn", "small inputs are LZVN blocks");
    let n = u32::from_le_bytes(p[8..12].try_into().unwrap()) as usize;
    p[12..12 + n].to_vec()
}

#[test]
fn lzvn_in_the_attribute() {
    if Command::new("lzfse").arg("-h").output().is_err() {
        return;
    }
    let small = sample(3500, 21);
    let mut x = header(7, small.len() as u64);
    x.extend(lzvn_chunk(&small));
    assert!(read_all(&Compressed::parse(x).unwrap(), &[]) == small);
    let mut x = header(11, small.len() as u64);
    x.extend(lzfse_tool(&small).unwrap());
    assert!(read_all(&Compressed::parse(x).unwrap(), &[]) == small);
}

fn zlib(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 6)
}

fn header(kind: u32, size: u64) -> Vec<u8> {
    let mut h = b"fpmc".to_vec();
    h.extend_from_slice(&kind.to_le_bytes());
    h.extend_from_slice(&size.to_le_bytes());
    h
}

fn read_all(c: &Compressed, rsrc: &[u8]) -> Vec<u8> {
    let mut cache = ChunkCache::default();
    let mut reader = |at: u64, b: &mut [u8]| -> Result<usize> {
        b.copy_from_slice(&rsrc[at as usize..at as usize + b.len()]);
        Ok(b.len())
    };
    let mut out = vec![0u8; c.size as usize];
    // In odd pieces, across chunks.
    let mut at = 0usize;
    while at < out.len() {
        let n = 10_007.min(out.len() - at);
        let got = c
            .read(&mut reader, rsrc.len() as u64, &mut cache, 1, at as u64, &mut out[at..at + n])
            .unwrap_or_else(|e| panic!("at {at}: {e}"));
        assert_eq!(got, n);
        at += n;
    }
    out
}

#[test]
fn decmpfs_layouts() {
    let small = sample(3000, 11);
    // Type 3: zlib in the attribute; and its stored form.
    let mut x = header(3, small.len() as u64);
    x.extend(zlib(&small));
    assert!(read_all(&Compressed::parse(x).unwrap(), &[]) == small);
    let mut x = header(3, small.len() as u64);
    x.push(0xFF);
    x.extend(&small);
    assert!(read_all(&Compressed::parse(x).unwrap(), &[]) == small);
    // Type 1: stored.
    let mut x = header(1, small.len() as u64);
    x.extend(&small);
    assert!(read_all(&Compressed::parse(x).unwrap(), &[]) == small);

    // Type 4: zlib chunks in a classic resource fork.
    let big = sample(200_000, 12);
    let chunks: Vec<Vec<u8>> = big.chunks(CHUNK as usize).map(zlib).collect();
    let mut table = (chunks.len() as u32).to_le_bytes().to_vec();
    let mut off = 4 + 8 * chunks.len() as u32;
    for c in &chunks {
        table.extend(off.to_le_bytes());
        table.extend((c.len() as u32).to_le_bytes());
        off += c.len() as u32;
    }
    let mut body = table;
    for c in &chunks {
        body.extend(c);
    }
    let mut rsrc = vec![0u8; 0x100];
    rsrc[..4].copy_from_slice(&0x100u32.to_be_bytes());
    rsrc[4..8].copy_from_slice(&(0x100 + 4 + body.len() as u32).to_be_bytes());
    rsrc[8..12].copy_from_slice(&(body.len() as u32 + 4).to_be_bytes());
    rsrc.extend((body.len() as u32).to_be_bytes());
    rsrc.extend(&body);
    rsrc.extend([0u8; 50]); // the resource map, not read
    let c = Compressed::parse(header(4, big.len() as u64)).unwrap();
    assert!(read_all(&c, &rsrc) == big);

    // Types 8 and 12: an offset table, LZVN or LZFSE chunks (one stored).
    if Command::new("lzfse").arg("-h").output().is_ok() {
        for kind in [8u32, 12] {
            let chunks: Vec<Vec<u8>> = big
                .chunks(CHUNK as usize)
                .enumerate()
                .map(|(i, c)| {
                    if i == 1 {
                        let mut s = vec![0x06];
                        s.extend(c);
                        s
                    } else if kind == 8 {
                        // The tool codes only small inputs as LZVN (those
                        // are tested above); a 64 KiB chunk is made here.
                        lzvn_whole(c)
                    } else {
                        lzfse_tool(c).unwrap()
                    }
                })
                .collect();
            let mut rsrc = Vec::new();
            let mut off = 4 * (chunks.len() as u32 + 1);
            for c in &chunks {
                rsrc.extend(off.to_le_bytes());
                off += c.len() as u32;
            }
            rsrc.extend(off.to_le_bytes());
            for c in &chunks {
                rsrc.extend(c);
            }
            let c = Compressed::parse(header(kind, big.len() as u64)).unwrap();
            assert!(read_all(&c, &rsrc) == big, "type {kind}");
        }
    }
}

/// An LZVN stream for any data: literal runs only (valid LZVN, made here),
/// for chunks larger than the reference tool codes as LZVN.
fn lzvn_whole(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for run in data.chunks(271) {
        if run.len() >= 16 {
            out.push(0xE0);
            out.push((run.len() - 16) as u8);
        } else {
            out.push(0xE0 | run.len() as u8);
        }
        out.extend(run);
    }
    out.extend([0x06, 0, 0, 0, 0, 0, 0, 0]);
    out
}
