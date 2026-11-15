use super::*;
use crate::device::{DeviceInfo, MemoryDevice};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const MIB: u64 = 1 << 20;

/// Byte ranges, and a count of reads outside them.
type Watch = (Vec<(u64, u64)>, Arc<AtomicU64>);

/// A disk with unreadable sectors: `bad` byte ranges always fail, `weak`
/// sectors fail a number of times and then read.
struct Faulty {
    data: Vec<u8>,
    bad: Vec<(u64, u64)>,
    weak: HashMap<u64, u32>,
    reads: Arc<AtomicU64>,
    failed: Arc<AtomicU64>,
    /// Reads outside these are counted here.
    outside: Option<Watch>,
    /// After this many reads every read fails as an unplugged disk does.
    gone_after: Option<u64>,
}

impl Faulty {
    fn new(data: Vec<u8>, bad: Vec<(u64, u64)>) -> Faulty {
        Faulty {
            data,
            bad,
            weak: HashMap::new(),
            reads: Arc::default(),
            failed: Arc::default(),
            outside: None,
            gone_after: None,
        }
    }
}

impl BlockDevice for Faulty {
    fn info(&self) -> DeviceInfo {
        MemoryDevice::new("faulty", Vec::new()).info()
    }
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let n = self.reads.fetch_add(1, Ordering::Relaxed);
        let end = offset + buf.len() as u64;
        if let Some((ok, count)) = &self.outside {
            if !ok.iter().any(|&(a, b)| offset >= a && end <= b) {
                count.fetch_add(1, Ordering::Relaxed);
            }
        }
        if self.gone_after.is_some_and(|g| n >= g) {
            self.failed.fetch_add(1, Ordering::Relaxed);
            return Err(Error::new(ErrorKind::Io, "No such device (os error 19)"));
        }
        let mut fail = self.bad.iter().any(|&(a, b)| offset < b && end > a);
        for s in (offset / 512)..end.div_ceil(512) {
            if let Some(left) = self.weak.get_mut(&(s * 512)) {
                if *left > 0 {
                    *left -= 1;
                    fail = true;
                }
            }
        }
        if fail {
            self.failed.fetch_add(1, Ordering::Relaxed);
            return Err(Error::new(ErrorKind::Io, "Input/output error (os error 5)"));
        }
        buf.copy_from_slice(&self.data[offset as usize..end as usize]);
        Ok(())
    }
    fn write_at(&mut self, _: u64, _: &[u8]) -> Result<()> {
        Err(Error::read_only())
    }
}

fn pattern(size: usize) -> Vec<u8> {
    // Every 512-byte sector different, some all zeros (not written).
    let mut x = 0x1234_5678u32;
    (0..size)
        .map(|i| {
            if (i / 4096) % 7 == 3 {
                return 0;
            }
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn image(size: u64) -> MemoryDevice {
    MemoryDevice::zeroed("image", size as usize)
}

/// What the rescued image must hold: the disk with its bad bytes as zeros.
fn expected(data: &[u8], bad: &[(u64, u64)]) -> Vec<u8> {
    let mut e = data.to_vec();
    for &(a, b) in bad {
        e[a as usize..b as usize].fill(0);
    }
    e
}

fn bad_blocks(map: &Map) -> Vec<(u64, u64)> {
    map.blocks().iter().filter(|b| b.status == Status::Bad).map(|b| (b.pos, b.pos + b.size)).collect()
}

#[test]
fn rescues_around_bad_sectors() {
    let size = 16 * MIB;
    let data = pattern(size as usize);
    let bad = vec![
        (MIB + 3 * 512, MIB + 4 * 512),                // one sector
        (5 * MIB + 2048, 5 * MIB + 2048 + 200 * 1024), // 200 KiB
        (size - 512, size),                            // the last sector
    ];
    let mut src = Faulty::new(data.clone(), bad.clone());
    // Fails twice, then reads: lost without retries.
    src.weak.insert(9 * MIB + 512 * 7, 2);
    let mut dst = image(size);
    let buf = dst.buffer();
    let mut map = Map::new(size);
    let opts = RescueOptions { retries: 0, ..Default::default() };
    let mut calls = 0;
    let done = rescue(&mut src, &mut dst, &mut map, &opts, &mut |_, _| {
        calls += 1;
        true
    })
    .unwrap();
    assert!(done);
    assert!(calls > 10);
    let mut want_bad = bad.clone();
    want_bad.insert(2, (9 * MIB + 512 * 7, 9 * MIB + 512 * 8));
    assert_eq!(bad_blocks(&map), want_bad);
    let t = map.totals();
    assert_eq!(t.bad, want_bad.iter().map(|r| r.1 - r.0).sum::<u64>());
    assert_eq!(t.finished + t.bad, size);
    assert_eq!((t.non_tried, t.non_trimmed, t.non_scraped, t.bad_areas), (0, 0, 0, 4));
    assert_eq!(map.phase, Phase::Finished);
    assert_eq!(buf.snapshot(), expected(&data, &want_bad));

    // A retry pass gets the weak sector.
    let mut src = Faulty::new(data.clone(), bad.clone());
    src.weak.insert(9 * MIB + 512 * 7, 2);
    let mut dst = image(size);
    let buf = dst.buffer();
    let mut map = Map::new(size);
    let opts = RescueOptions { retries: 1, ..Default::default() };
    assert!(rescue(&mut src, &mut dst, &mut map, &opts, &mut |_, _| true).unwrap());
    assert_eq!(bad_blocks(&map), bad);
    assert_eq!(buf.snapshot(), expected(&data, &bad));
}

#[test]
fn skips_past_a_bad_area_first() {
    let size = 64 * MIB;
    let data = pattern(size as usize);
    let bad = vec![(8 * MIB, 24 * MIB)];
    let mut src = Faulty::new(data, bad);
    let failed = src.failed.clone();
    let mut dst = image(size);
    let mut map = Map::new(size);
    // Stop when the first pass is over.
    let mut first_pass: Option<(Map, u64)> = None;
    let r = rescue(&mut src, &mut dst, &mut map, &RescueOptions::default(), &mut |m, _| {
        if m.pass == 2 || m.phase != Phase::Copying {
            first_pass = Some((m.clone(), failed.load(Ordering::Relaxed)));
            return false;
        }
        true
    })
    .unwrap();
    assert!(!r);
    let (m, failures) = first_pass.unwrap();
    // Everything after the bad area was copied in the first pass (from a
    // little past its end: the last skip overshoots, read in pass 2)…
    let &(from, to) = m.regions(Status::Finished, None).last().unwrap();
    assert!(to == size && (24 * MIB..26 * MIB).contains(&from), "{from}..{to}");
    // …after few failed reads: most of the bad area was skipped, untried.
    // (skips grow to at most 1% of the disk: ~25 here, not 128 clusters)
    assert!(failures < 40, "{failures} failed reads");
    assert!(m.totals().non_tried > 8 * MIB, "{:?}", m.totals());
}

#[test]
fn stops_and_goes_on_from_the_map() {
    let size = 12 * MIB;
    let data = pattern(size as usize);
    let bad = vec![(2 * MIB + 4096, 2 * MIB + 8192), (7 * MIB, 7 * MIB + 300 * 1024)];
    let mut dst = image(size);
    let buf = dst.buffer();
    let opts = RescueOptions::default();
    let mut map = Map::new(size);
    // Stop and go on several times, through the map's text each time.
    let mut runs = 0;
    loop {
        runs += 1;
        let mut src = Faulty::new(data.clone(), bad.clone());
        let mut n = 0;
        let done = rescue(&mut src, &mut dst, &mut map, &opts, &mut |_, _| {
            n += 1;
            n < 4
        })
        .unwrap();
        map = Map::parse(&map.to_text(), size).unwrap();
        if done {
            break;
        }
        assert!(runs < 100);
    }
    assert!(runs > 3);
    assert_eq!(bad_blocks(&map), bad);
    assert_eq!(map.totals().finished, size - 4096 - 300 * 1024);
    assert_eq!(buf.snapshot(), expected(&data, &bad));
}

#[test]
fn stays_in_the_domain() {
    let size = 8 * MIB;
    let data = pattern(size as usize);
    let domain = vec![(0, MIB), (3 * MIB + 512, 5 * MIB - 512), (7 * MIB, size)];
    let mut src = Faulty::new(data.clone(), vec![(4 * MIB, 4 * MIB + 512)]);
    let outside = Arc::new(AtomicU64::new(0));
    src.outside = Some((domain.clone(), outside.clone()));
    let mut dst = image(size);
    let buf = dst.buffer();
    let mut map = Map::new(size);
    let opts = RescueOptions { domain: Some(domain.clone()), ..Default::default() };
    assert!(rescue(&mut src, &mut dst, &mut map, &opts, &mut |_, _| true).unwrap());
    assert_eq!(outside.load(Ordering::Relaxed), 0);
    assert_eq!(map.regions(Status::NonTried, None), vec![(MIB, 3 * MIB + 512), (5 * MIB - 512, 7 * MIB)]);
    assert_eq!(bad_blocks(&map), vec![(4 * MIB, 4 * MIB + 512)]);
    let got = buf.snapshot();
    let mut want = vec![0u8; size as usize];
    for &(a, b) in &domain {
        want[a as usize..b as usize].copy_from_slice(&data[a as usize..b as usize]);
    }
    want[4 * MIB as usize..4 * MIB as usize + 512].fill(0);
    assert_eq!(got, want);
}

#[test]
fn notices_an_unplugged_disk() {
    let size = 8 * MIB;
    let mut src = Faulty::new(pattern(size as usize), Vec::new());
    src.gone_after = Some(20);
    let mut dst = image(size);
    let mut map = Map::new(size);
    let e = rescue(&mut src, &mut dst, &mut map, &RescueOptions::default(), &mut |_, _| true).unwrap_err();
    assert!(e.to_string().contains("stopped answering"), "{e}");
    // What was read is kept.
    assert_eq!(map.totals().finished, 20 * 128 * 1024);
}

#[test]
fn map_text() {
    // As GNU ddrescue writes it (with an unfinished end it did not know).
    let text = "# Mapfile. Created by GNU ddrescue version 1.27\n\
                # Command line: ddrescue /dev/sdb sdb.img sdb.map\n\
                # current_pos  current_status  current_pass\n\
                0x00120000     *               1\n\
                #      pos        size  status\n\
                0x00000000  0x00100000  +\n\
                0x00100000  0x00000200  -\n\
                1049088  0x0001FE00  *\n\
                0x00120000  0x00010000  /\n";
    let map = Map::parse(text, 4 * MIB).unwrap();
    assert_eq!(map.phase, Phase::Trimming);
    assert_eq!(map.pos, 0x120000);
    let b: Vec<_> = map.blocks().iter().map(|b| (b.pos, b.size, b.status.symbol())).collect();
    assert_eq!(
        b,
        vec![
            (0, 0x100000, '+'),
            (0x100000, 0x200, '-'),
            (0x100200, 0x1FE00, '*'),
            (0x120000, 0x10000, '/'),
            (0x130000, 4 * MIB - 0x130000, '?'),
        ]
    );
    assert_eq!(Map::parse(&map.to_text(), 4 * MIB).unwrap(), map);
    assert!(Map::parse(text, MIB).is_err());
    assert!(Map::parse("0x0 ?\n0x0 0x10 +\n0x20 0x10 +\n", MIB).is_err());
    // Cells for drawing: the worst state in each.
    let cells = map.cells(4);
    assert_eq!(cells, vec![Status::Finished, Status::Bad, Status::NonTried, Status::NonTried]);
}

#[test]
fn map_set_joins_runs() {
    let mut m = Map::new(100);
    m.set(10, 10, Status::Bad);
    m.set(20, 10, Status::Bad);
    m.set(0, 10, Status::Finished);
    m.set(30, 70, Status::Finished);
    assert_eq!(m.blocks().len(), 3);
    m.set(10, 20, Status::Finished);
    assert_eq!(m.blocks(), &[Block { pos: 0, size: 100, status: Status::Finished }]);
    m.set(99, 5, Status::NonTried);
    assert_eq!(m.blocks().last().unwrap(), &Block { pos: 99, size: 1, status: Status::NonTried });
}

#[test]
fn writes_and_verifies_an_image() {
    let img = pattern(10 * MIB as usize + 1000);
    // A disk with old data on it, larger than the image.
    let mut disk = MemoryDevice::new("disk", vec![0xAB; 16 * MIB as usize]);
    let buf = disk.buffer();
    let mut src = MemoryDevice::new("img", img.clone());
    let mut written = 0;
    let mut report = |n| {
        written = n;
        true
    };
    let mut w = DiskWriter::new(&mut disk, &mut report);
    feed(&mut src, &mut w).unwrap();
    assert_eq!(w.finish().unwrap(), img.len() as u64);
    assert_eq!(written, img.len() as u64);
    clear_tail(&mut disk, img.len() as u64).unwrap();
    let got = buf.snapshot();
    assert_eq!(&got[..img.len()], &img[..]);
    assert!(got[img.len()..15 * MIB as usize].iter().all(|&b| b == 0xAB));
    assert!(got[15 * MIB as usize..].iter().all(|&b| b == 0));

    let mut ok = |_| true;
    let mut v = DiskVerifier::new(&mut disk, &mut ok);
    feed(&mut src, &mut v).unwrap();
    assert_eq!(v.finish().unwrap(), img.len() as u64);

    // A byte that did not arrive.
    disk.write_at(7 * MIB + 5, &[img[(7 * MIB + 5) as usize] ^ 1]).unwrap();
    let mut v = DiskVerifier::new(&mut disk, &mut ok);
    let e = feed(&mut src, &mut v).and_then(|_| v.finish()).unwrap_err();
    assert!(e.to_string().contains(&format!("at byte {}", 7 * MIB + 5)), "{e}");

    // Too large for the disk; stopping.
    let mut small = MemoryDevice::zeroed("small", 4 * MIB as usize);
    let mut w = DiskWriter::new(&mut small, &mut ok);
    let e = feed(&mut src, &mut w).and_then(|_| w.finish()).unwrap_err();
    assert!(e.to_string().contains("larger than the disk"), "{e}");
    let mut stop = |_| false;
    let mut w = DiskWriter::new(&mut disk, &mut stop);
    let e = feed(&mut src, &mut w).unwrap_err();
    assert!(is_stopped(&e), "{e}");
}

#[test]
fn ranges() {
    assert_eq!(merge_ranges(vec![(5, 9), (0, 2), (1, 3), (9, 10), (4, 4)]), vec![(0, 3), (5, 10)]);
    assert_eq!(complement(&[(2, 4), (8, 20)], 10), vec![(0, 2), (4, 8)]);
    assert_eq!(complement(&[], 10), vec![(0, 10)]);
}

#[test]
fn converts_only_ranges() {
    let data = pattern(4 * MIB as usize);
    let mut src = MemoryDevice::new("src", data.clone());
    let out = MemoryDevice::new("out", Vec::new()).with_resizable(true);
    let buf = out.buffer();
    let ranges = [(512, MIB + 100), (3 * MIB, 4 * MIB)];
    let mut seen = (0, 0);
    crate::container::convert_ranges(&mut src, Box::new(out), None, "out", &[], Some(&ranges), &mut |d, t| {
        seen = (d, t);
        true
    })
    .unwrap();
    assert_eq!(seen, (2 * MIB - 412, 2 * MIB - 412));
    let got = buf.snapshot();
    let mut want = vec![0u8; data.len()];
    for &(a, b) in &ranges {
        want[a as usize..b as usize].copy_from_slice(&data[a as usize..b as usize]);
    }
    assert_eq!(got, want);
}

/// GNU ddrescue goes on from our map, and we from its: both ways the
/// copy ends with the same image and the same bad sectors.
#[test]
fn ddrescue_reads_our_maps_and_we_read_its() {
    if std::process::Command::new("ddrescue").arg("--version").output().is_err() {
        eprintln!("ddrescue is not installed: skipped");
        return;
    }
    let dir = std::env::temp_dir().join(format!("dix-ddrescue-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let size = 8 * MIB;
    let data = pattern(size as usize);
    let bad = vec![(MIB + 1024, MIB + 1536), (3 * MIB, 3 * MIB + 70 * 1024), (6 * MIB + 4096, 6 * MIB + 5120)];
    let want = expected(&data, &bad);
    std::fs::write(dir.join("src.img"), &data).unwrap();
    // ddrescue's test mode: the disk's good ('+') and bad areas.
    let mut test = Map::new(size);
    test.set(0, size, Status::Finished);
    for &(a, b) in &bad {
        test.set(a, b - a, Status::Bad);
    }
    std::fs::write(dir.join("test.map"), test.to_text()).unwrap();
    let ddrescue = |args: &[&str]| {
        let out = std::process::Command::new("ddrescue")
            .args(["-q", "-f", "-H", "test.map"])
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };

    // Ours, stopped part way; ddrescue finishes it.
    let mut dst = image(size);
    let buf = dst.buffer();
    let mut map = Map::new(size);
    let mut n = 0;
    let done = rescue(
        &mut Faulty::new(data.clone(), bad.clone()),
        &mut dst,
        &mut map,
        &RescueOptions::default(),
        &mut |_, _| {
            n += 1;
            n < 4
        },
    )
    .unwrap();
    assert!(!done && map.totals().finished > 0 && map.totals().non_tried > 0);
    std::fs::write(dir.join("a.img"), buf.snapshot()).unwrap();
    std::fs::write(dir.join("a.map"), map.to_text()).unwrap();
    ddrescue(&["src.img", "a.img", "a.map"]);
    let theirs = Map::parse(&std::fs::read_to_string(dir.join("a.map")).unwrap(), size).unwrap();
    assert_eq!(bad_blocks(&theirs), bad);
    assert_eq!(theirs.totals().finished, size - bad.iter().map(|r| r.1 - r.0).sum::<u64>());
    assert_eq!(std::fs::read(dir.join("a.img")).unwrap(), want);

    // ddrescue's, stopped after 5 MiB; we finish it.
    std::fs::write(dir.join("b.img"), vec![0u8; size as usize]).unwrap();
    ddrescue(&["-s", "5MiB", "src.img", "b.img", "b.map"]);
    let mut map = Map::parse(&std::fs::read_to_string(dir.join("b.map")).unwrap(), size).unwrap();
    assert!(map.totals().non_tried >= 3 * MIB);
    let mut dst = MemoryDevice::new("b", std::fs::read(dir.join("b.img")).unwrap());
    let buf = dst.buffer();
    assert!(rescue(
        &mut Faulty::new(data.clone(), bad.clone()),
        &mut dst,
        &mut map,
        &RescueOptions::default(),
        &mut |_, _| true
    )
    .unwrap());
    assert_eq!(bad_blocks(&map), bad);
    assert_eq!(buf.snapshot(), want);
    let _ = std::fs::remove_dir_all(&dir);
}
