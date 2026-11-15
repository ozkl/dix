//! Disk images built here the way hdiutil lays them out — every chunk
//! kind, blocks per partition, an XML property list or an old-style
//! resource fork — first decoded by two independent tools (dmg2img and
//! 7-Zip) to check the builder, then read here.

use super::dmg::encode;
use dix_core::device::MemoryDevice;
use dix_core::{AccessMode, BlockDevice, DriverRegistry, VfsPath};
use std::path::{Path, PathBuf};
use std::process::Command;

const RAW: u32 = 1;
const IGNORE: u32 = 2;
const ADC: u32 = 0x8000_0004;
const ZLIB: u32 = 0x8000_0005;
const BZIP2: u32 = 0x8000_0006;
const LZFSE: u32 = 0x8000_0007;
const LZMA: u32 = 0x8000_0008;

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--help").output().is_ok() || Command::new(tool).output().is_ok()
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("dix-dmg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
    fn path(&self, n: &str) -> PathBuf {
        self.0.join(n)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // DIX_KEEP=1 keeps the images, to look at.
        if std::env::var_os("DIX_KEEP").is_some() {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn compress(kind: u32, data: &[u8], s: &Scratch) -> Vec<u8> {
    use std::io::Write;
    match kind {
        RAW => data.to_vec(),
        ZLIB => miniz_oxide::deflate::compress_to_vec_zlib(data, 6),
        ADC => encode::adc(data),
        BZIP2 => {
            let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(6));
            e.write_all(data).unwrap();
            e.finish().unwrap()
        }
        LZMA => {
            let mut opts = lzma_rust2::XzOptions::with_preset(3);
            opts.set_check_sum_type(lzma_rust2::CheckType::Crc32);
            let mut w = lzma_rust2::XzWriter::new(Vec::new(), opts).unwrap();
            w.write_all(data).unwrap();
            w.finish().unwrap()
        }
        LZFSE => {
            let (i, o) = (s.path("lzfse.in"), s.path("lzfse.out"));
            std::fs::write(&i, data).unwrap();
            assert!(
                Command::new("lzfse").arg("-encode").arg("-i").arg(&i).arg("-o").arg(&o).status().unwrap().success()
            );
            std::fs::read(&o).unwrap()
        }
        _ => unreachable!(),
    }
}

fn b64(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for (i, c) in data.chunks(3).enumerate() {
        if i > 0 && i % 13 == 0 {
            s.push_str("\n\t\t\t\t");
        }
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            s.push(if k <= c.len() { A[(n >> (18 - 6 * k) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

fn crc(data: &[u8]) -> u32 {
    dix_core::partition::crc32(data)
}

/// A UDIF image of `raw`: one block table per (first sector, sectors,
/// name), chunks of `chunk` sectors, stored with `kinds` in turn (all-zero
/// chunks are left out). `xml`: the tables in a property list, else in a
/// resource fork.
fn build(raw: &[u8], blocks: &[(u64, u64, &str)], kinds: &[u32], chunk: u64, xml: bool, s: &Scratch) -> Vec<u8> {
    let mut fork = Vec::new();
    let mut tables = Vec::new();
    let mut k = 0usize;
    for &(first, sectors, _) in blocks {
        let mut chunks = Vec::new();
        let mut at = 0u64;
        while at < sectors {
            let n = chunk.min(sectors - at);
            let data = &raw[((first + at) * 512) as usize..((first + at + n) * 512) as usize];
            let (kind, packed) = if data.iter().all(|&b| b == 0) {
                (if k.is_multiple_of(2) { IGNORE } else { 0 }, Vec::new())
            } else {
                let kind = kinds[k % kinds.len()];
                (kind, compress(kind, data, s))
            };
            k += 1;
            chunks.push((kind, at, n, fork.len() as u64, packed.len() as u64));
            fork.extend_from_slice(&packed);
            at += n;
        }
        let mut m = vec![0u8; 204];
        m[..4].copy_from_slice(b"mish");
        m[4..8].copy_from_slice(&1u32.to_be_bytes());
        m[8..16].copy_from_slice(&first.to_be_bytes());
        m[16..24].copy_from_slice(&sectors.to_be_bytes());
        m[32..36].copy_from_slice(&0x208u32.to_be_bytes());
        m[64..68].copy_from_slice(&2u32.to_be_bytes());
        m[68..72].copy_from_slice(&32u32.to_be_bytes());
        // The block's checksum leaves out its "ignore" chunks (as 7-Zip
        // checks it).
        let kept: Vec<u8> = chunks
            .iter()
            .filter(|c| c.0 != IGNORE)
            .flat_map(|&(_, at, n, _, _)| {
                raw[((first + at) * 512) as usize..((first + at + n) * 512) as usize].to_vec()
            })
            .collect();
        m[72..76].copy_from_slice(&crc(&kept).to_be_bytes());
        m[200..204].copy_from_slice(&(chunks.len() as u32 + 1).to_be_bytes());
        for (kind, at, n, off, len) in chunks.iter().copied().chain([(0xFFFF_FFFF, sectors, 0, fork.len() as u64, 0)]) {
            m.extend_from_slice(&kind.to_be_bytes());
            m.extend_from_slice(&0u32.to_be_bytes());
            m.extend_from_slice(&at.to_be_bytes());
            m.extend_from_slice(&n.to_be_bytes());
            m.extend_from_slice(&off.to_be_bytes());
            m.extend_from_slice(&len.to_be_bytes());
        }
        tables.push(m);
    }
    let data_len = fork.len() as u64;
    let data_crc = crc(&fork);
    let mut out = fork;
    let (mut xml_at, mut xml_len, mut rsrc_at, mut rsrc_len) = (0u64, 0u64, 0u64, 0u64);
    if xml {
        let mut p = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\
             \t<key>resource-fork</key>\n\t<dict>\n\t\t<key>blkx</key>\n\t\t<array>\n",
        );
        for (i, (m, (_, _, name))) in tables.iter().zip(blocks).enumerate() {
            p += &format!(
                "\t\t\t<dict>\n\t\t\t\t<key>Attributes</key>\n\t\t\t\t<string>0x0050</string>\n\
                 \t\t\t\t<key>CFName</key>\n\t\t\t\t<string>{name}</string>\n\
                 \t\t\t\t<key>Data</key>\n\t\t\t\t<data>\n\t\t\t\t{}\n\t\t\t\t</data>\n\
                 \t\t\t\t<key>ID</key>\n\t\t\t\t<string>{}</string>\n\
                 \t\t\t\t<key>Name</key>\n\t\t\t\t<string>{name}</string>\n\t\t\t</dict>\n",
                b64(m),
                i as i64 - 1
            );
        }
        p += "\t\t</array>\n\t</dict>\n</dict>\n</plist>\n";
        xml_at = out.len() as u64;
        xml_len = p.len() as u64;
        out.extend_from_slice(p.as_bytes());
    } else {
        // A classic resource fork: header, data (length-prefixed), map.
        let mut data = Vec::new();
        let mut offs = Vec::new();
        for m in &tables {
            offs.push(data.len() as u32);
            data.extend_from_slice(&(m.len() as u32).to_be_bytes());
            data.extend_from_slice(m);
        }
        let mut map = vec![0u8; 28];
        map[24..26].copy_from_slice(&28u16.to_be_bytes()); // type list
        map.extend_from_slice(&0u16.to_be_bytes()); // one type
        map.extend_from_slice(b"blkx");
        map.extend_from_slice(&(tables.len() as u16 - 1).to_be_bytes());
        map.extend_from_slice(&10u16.to_be_bytes()); // refs after the type list
        for (i, o) in offs.iter().enumerate() {
            map.extend_from_slice(&(i as u16).to_be_bytes());
            map.extend_from_slice(&0xFFFFu16.to_be_bytes());
            map.extend_from_slice(&(o & 0x00FF_FFFF).to_be_bytes());
            map.extend_from_slice(&0u32.to_be_bytes());
        }
        let mut r = vec![0u8; 256];
        r[..4].copy_from_slice(&256u32.to_be_bytes());
        r[4..8].copy_from_slice(&(256 + data.len() as u32).to_be_bytes());
        r[8..12].copy_from_slice(&(data.len() as u32).to_be_bytes());
        r[12..16].copy_from_slice(&(map.len() as u32).to_be_bytes());
        r.extend_from_slice(&data);
        r.extend_from_slice(&map);
        rsrc_at = out.len() as u64;
        rsrc_len = r.len() as u64;
        out.extend_from_slice(&r);
    }
    let mut t = vec![0u8; 512];
    t[..4].copy_from_slice(b"koly");
    t[4..8].copy_from_slice(&4u32.to_be_bytes());
    t[8..12].copy_from_slice(&512u32.to_be_bytes());
    t[12..16].copy_from_slice(&1u32.to_be_bytes());
    t[32..40].copy_from_slice(&data_len.to_be_bytes());
    t[40..48].copy_from_slice(&rsrc_at.to_be_bytes());
    t[48..56].copy_from_slice(&rsrc_len.to_be_bytes());
    t[56..60].copy_from_slice(&1u32.to_be_bytes());
    t[60..64].copy_from_slice(&1u32.to_be_bytes());
    t[64..80].copy_from_slice(&[0x42; 16]);
    t[80..84].copy_from_slice(&2u32.to_be_bytes());
    t[84..88].copy_from_slice(&32u32.to_be_bytes());
    t[88..92].copy_from_slice(&data_crc.to_be_bytes());
    t[216..224].copy_from_slice(&xml_at.to_be_bytes());
    t[224..232].copy_from_slice(&xml_len.to_be_bytes());
    t[352..356].copy_from_slice(&2u32.to_be_bytes());
    t[356..360].copy_from_slice(&32u32.to_be_bytes());
    let sums: Vec<u8> = tables.iter().flat_map(|m| m[72..76].to_vec()).collect();
    t[360..364].copy_from_slice(&crc(&sums).to_be_bytes());
    t[488..492].copy_from_slice(&1u32.to_be_bytes());
    t[492..500].copy_from_slice(&(raw.len() as u64 / 512).to_be_bytes());
    out.extend_from_slice(&t);
    out
}

/// A disk with data here and there, zeros between.
fn sample(sectors: usize) -> Vec<u8> {
    let mut v = vec![0u8; sectors * 512];
    let mut x = 99u32;
    let size = sectors * 512;
    for (start, len) in [(0usize, 700_000usize), (1_500_000, 3_000_000), (size - 70_000, 70_000)] {
        let len = len.min(size.saturating_sub(start));
        for b in &mut v[start..start + len] {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = if x & 0x300 != 0 { b'a' + (x % 5) as u8 } else { (x >> 24) as u8 };
        }
    }
    v
}

fn open(img: Vec<u8>) -> Box<dyn BlockDevice> {
    crate::default_registry().open_disk(Box::new(MemoryDevice::new("x.dmg", img)), None).unwrap()
}

fn read_all(dev: &mut dyn BlockDevice) -> Vec<u8> {
    let mut out = vec![0u8; dev.size() as usize];
    let (mut at, mut step) = (0usize, 99_991usize);
    while at < out.len() {
        let n = step.min(out.len() - at);
        dev.read_at(at as u64, &mut out[at..at + n]).unwrap();
        at += n;
        step = step * 7 % 900_000 + 1;
    }
    out
}

fn dmg2img(dmg: &Path, s: &Scratch) -> Vec<u8> {
    let out = s.path("dmg2img.img");
    let r = Command::new("dmg2img").arg("-i").arg(dmg).arg("-o").arg(&out).output().unwrap();
    assert!(r.status.success(), "dmg2img: {}", String::from_utf8_lossy(&r.stdout));
    std::fs::read(out).unwrap()
}

/// 7-Zip extracts each block table as a file; joined in order they are the
/// disk.
fn seven_zip(dmg: &Path, s: &Scratch) -> Vec<u8> {
    let dir = s.path("7z");
    let _ = std::fs::remove_dir_all(&dir);
    let r = Command::new("7z")
        .arg("x")
        .arg("-tdmg")
        .arg("-y")
        .arg(format!("-o{}", dir.display()))
        .arg(dmg)
        .output()
        .unwrap();
    assert!(r.status.success(), "7z: {}", String::from_utf8_lossy(&r.stdout));
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort_by_key(|p| {
        p.file_name().unwrap().to_string_lossy().split('.').next().unwrap().parse::<u32>().unwrap_or(0)
    });
    files.iter().flat_map(|f| std::fs::read(f).unwrap()).collect()
}

#[test]
fn every_chunk_kind() {
    let s = Scratch::new("kinds");
    let raw = sample(16384);
    let blocks = [(0u64, 64u64, "Driver Descriptor Map (DDM : 0)"), (64, 16384 - 64, "disk image (Apple_HFS : 1)")];
    // Kinds dmg2img decodes; with LZFSE, which 7-Zip decodes too; and
    // with LZMA (ULMO, an xz stream per chunk), which neither tool here
    // knows: that set is checked against the disk only.
    let mut sets = vec![vec![RAW, ZLIB, BZIP2, ADC]];
    if have("lzfse") {
        sets.push(vec![ZLIB, LZFSE, RAW, ADC, BZIP2]);
    }
    sets.push(vec![LZMA, ZLIB, RAW]);
    for (i, kinds) in sets.iter().enumerate() {
        let dmg = build(&raw, &blocks, kinds, 1000, true, &s);
        let path = s.path(&format!("k{i}.dmg"));
        std::fs::write(&path, &dmg).unwrap();
        if i == 0 && have("dmg2img") {
            assert!(dmg2img(&path, &s) == raw, "dmg2img reads the built image as the disk");
        }
        if have("7z") && !kinds.contains(&LZMA) {
            assert!(seven_zip(&path, &s) == raw, "7-Zip reads the built image as the disk ({kinds:x?})");
        }
        let mut dev = open(dmg);
        assert_eq!(dev.size(), raw.len() as u64);
        assert!(dev.container().unwrap().starts_with("Apple disk image"), "{:?}", dev.container());
        assert!(read_all(dev.as_mut()) == raw, "{kinds:x?}");
    }
}

#[test]
fn old_images_with_a_resource_fork() {
    let s = Scratch::new("rsrc");
    let raw = sample(8192);
    let blocks = [(0u64, 8192u64, "whole disk")];
    let dmg = build(&raw, &blocks, &[ZLIB, RAW], 2048, false, &s);
    let mut dev = open(dmg);
    assert!(read_all(dev.as_mut()) == raw);
}

#[test]
fn damaged_images_are_errors_not_panics() {
    let s = Scratch::new("bad");
    let raw = sample(4096);
    let dmg = build(&raw, &[(0, 4096, "x")], &[ZLIB, ADC, BZIP2], 1000, true, &s);
    let reg = crate::default_registry();
    let mut x = 7u32;
    for _ in 0..300 {
        let mut d = dmg.clone();
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let at = (x as usize >> 4) % d.len();
        d[at] ^= 1 << (x % 8);
        if let Ok(mut dev) = reg.open_disk(Box::new(MemoryDevice::new("x", d)), None) {
            let mut b = vec![0u8; 65536];
            for off in (0..dev.size()).step_by(300_000) {
                let n = (dev.size() - off).min(b.len() as u64) as usize;
                let _ = dev.read_at(off, &mut b[..n]);
            }
        }
    }
    let mut enc = vec![0u8; 4096];
    enc[..8].copy_from_slice(b"encrcdsa");
    let err = reg.open_disk(Box::new(MemoryDevice::new("x", enc)), None).err().unwrap();
    assert!(err.to_string().contains("encrypted"), "{err}");
}

/// The HFS+ test volume in a DMG with an Apple partition map, as hdiutil
/// makes them: found through the map and mounted.
#[cfg(feature = "hfsplus")]
#[test]
fn hfs_plus_in_an_apple_partition_map() {
    let s = Scratch::new("apm");
    let gz = format!("{}/testdata/hfs/hfsplus.img.gz", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("gzip").arg("-dc").arg(&gz).output().unwrap();
    let hfs = out.stdout;
    let part_sectors = hfs.len() as u64 / 512;
    let start = 64u64;
    let total = start + part_sectors + 16;
    let mut raw = vec![0u8; (total * 512) as usize];
    // Driver descriptor, then map entries: the map itself, the volume.
    raw[..2].copy_from_slice(b"ER");
    raw[2..4].copy_from_slice(&512u16.to_be_bytes());
    raw[4..8].copy_from_slice(&(total as u32).to_be_bytes());
    let entry = |raw: &mut Vec<u8>, n: usize, first: u64, count: u64, name: &str, kind: &str| {
        let e = &mut raw[n * 512..(n + 1) * 512];
        e[..2].copy_from_slice(b"PM");
        e[4..8].copy_from_slice(&2u32.to_be_bytes());
        e[8..12].copy_from_slice(&(first as u32).to_be_bytes());
        e[12..16].copy_from_slice(&(count as u32).to_be_bytes());
        e[16..16 + name.len()].copy_from_slice(name.as_bytes());
        e[48..48 + kind.len()].copy_from_slice(kind.as_bytes());
    };
    entry(&mut raw, 1, 1, 63, "Apple", "Apple_partition_map");
    entry(&mut raw, 2, start, part_sectors, "disk image", "Apple_HFS");
    raw[(start * 512) as usize..(start * 512) as usize + hfs.len()].copy_from_slice(&hfs);
    let blocks =
        [(0u64, start, "Apple (Apple_partition_map : 1)"), (start, total - start, "disk image (Apple_HFS : 2)")];
    let dmg = build(&raw, &blocks, &[ZLIB], 2048, true, &s);
    let path = s.path("hfs.dmg");
    std::fs::write(&path, &dmg).unwrap();
    if have("dmg2img") {
        assert!(dmg2img(&path, &s) == raw);
    }

    let reg: DriverRegistry = crate::default_registry();
    let mut dev = reg.open_disk(Box::new(MemoryDevice::new("hfs.dmg", dmg)), None).unwrap();
    let table = dix_core::partition::read_partition_table(dev.as_mut()).unwrap().unwrap();
    assert_eq!(table.scheme, dix_core::partition::Scheme::Apm);
    let p = table.partitions.iter().find(|p| p.type_id == "Apple_HFS").unwrap();
    assert_eq!((p.start, p.size, p.name.as_deref()), (start * 512, part_sectors * 512, Some("disk image")));
    // The open dialog's choice: the HFS+ partition.
    let scan = dix_core::partition::scan(&reg, dev.as_mut()).unwrap().unwrap();
    assert_eq!(scan.default_choice(), Some(p.number));
    let part = dix_core::partition::slice(dev, p).unwrap();
    let mut m = reg.mount(part, None, AccessMode::ReadOnly).unwrap();
    assert_eq!(m.driver.info().id, "hfsplus");
    assert_eq!(m.fs.read_to_vec(&VfsPath::parse("/docs/hello.txt").unwrap()).unwrap(), b"hello hfs+\n");
}

/// Writes into an image of every chunk kind, saved (the image rewritten as
/// UDZO): dmg2img and 7-Zip read the new file as the changed disk, and so
/// does this reader, opened again.
#[test]
fn writes_rewrite_the_image() {
    let s = Scratch::new("write");
    let mut raw = sample(16384);
    let blocks = [(0u64, 64u64, "map"), (64, 16384 - 64, "disk image")];
    let mut kinds = vec![RAW, ZLIB, BZIP2, ADC, LZMA];
    if have("lzfse") {
        kinds.push(LZFSE);
    }
    let dmg = build(&raw, &blocks, &kinds, 1000, true, &s);
    let mem = MemoryDevice::new("w.dmg", dmg);
    let buf = mem.buffer();
    let mut dev = crate::default_registry().open_disk(Box::new(mem), None).unwrap();
    assert!(!dev.is_read_only());
    let mut x = 5u32;
    for k in 0..40 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let len = [1usize, 511, 4096, 70_000, 1_500_000][k % 5];
        let at = x as usize % (raw.len() - len);
        let fill: Vec<u8> = (0..len).map(|i| (i as u32 ^ x) as u8).collect();
        dev.write_at(at as u64, &fill).unwrap();
        raw[at..at + len].copy_from_slice(&fill);
        if k == 20 {
            dev.flush().unwrap(); // saved halfway too
        }
    }
    // A stretch of zeros, stored as nothing.
    dev.write_at(3 << 20, &vec![0u8; 2 << 20]).unwrap();
    raw[3 << 20..5 << 20].fill(0);
    assert!(read_all(dev.as_mut()) == raw, "before saving");
    dev.flush().unwrap();
    assert!(read_all(dev.as_mut()) == raw, "after saving");
    drop(dev);
    let bytes = buf.snapshot();
    let path = s.path("w.dmg");
    std::fs::write(&path, &bytes).unwrap();
    if have("dmg2img") {
        assert!(dmg2img(&path, &s) == raw, "dmg2img reads the rewritten image");
    }
    if have("7z") {
        assert!(seven_zip(&path, &s) == raw, "7-Zip reads the rewritten image");
        let t = Command::new("7z").args(["t", "-tdmg"]).arg(&path).output().unwrap();
        assert!(t.status.success(), "7-Zip checks its checksums: {}", String::from_utf8_lossy(&t.stdout));
    }
    let mut again = open(bytes);
    assert!(again.container().unwrap().contains("zlib"));
    assert!(read_all(again.as_mut()) == raw);
}
