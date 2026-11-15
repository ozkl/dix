//! FAT volumes made by `mkfs.fat` and filled by `mcopy`, read and written
//! by the driver; `fsck.fat -n` and `mcopy` judge the result.

use super::*;
use dix_core::device::host::FileDevice;
use dix_core::{AccessMode, DriverRegistry};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tool(name: &str) -> Option<PathBuf> {
    ["/usr/sbin", "/sbin", "/usr/bin", "/bin"].iter().map(|d| Path::new(d).join(name)).find(|p| p.exists())
}

fn have() -> bool {
    let ok = ["mkfs.fat", "fsck.fat", "mcopy", "mmd"].iter().all(|t| tool(t).is_some());
    if !ok {
        eprintln!("dosfstools / mtools not installed: skipping FAT test");
    }
    ok
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("dix-fat-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(dir: &Path, cmd: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(tool(cmd).unwrap())
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C.UTF-8")
        .env("MTOOLS_SKIP_CHECK", "1")
        .output()
        .unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
}

fn ok(dir: &Path, cmd: &str, args: &[&str]) -> String {
    let (good, out) = run(dir, cmd, args);
    assert!(good, "{cmd} {args:?}: {out}");
    out
}

fn registry() -> DriverRegistry {
    crate::default_registry()
}

fn p(s: &str) -> VfsPath {
    VfsPath::parse(s).unwrap()
}

#[derive(Debug, PartialEq, Eq)]
enum Item {
    Dir,
    File(Vec<u8>),
}

fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn make_source(dir: &Path, big: usize) {
    let w = |rel: &str, data: &[u8]| {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    };
    w("README.TXT", b"upper case 8.3\n");
    w("lower.txt", b"lower case 8.3\n");
    w("MixedCase.Txt", b"needs a long name\n");
    w("A rather long file name with spaces.markdown", b"long\n");
    w("ünïcødé – names/файл.txt", "unicode ✓".as_bytes());
    w("docs/deep/er/notes.txt", &b"lots of the same text ".repeat(3000));
    w("bin/noise.bin", &noise(big, 7));
    w("empty.txt", b"");
    for i in 0..300 {
        w(&format!("many/file number {i:03}.txt"), format!("{i}").as_bytes());
    }
    std::fs::create_dir_all(dir.join("empty folder")).unwrap();
}

fn walk_host(dir: &Path, rel: &str, out: &mut BTreeMap<String, Item>) {
    for e in std::fs::read_dir(dir.join(rel.trim_start_matches('/'))).unwrap().flatten() {
        let path = format!("{rel}/{}", e.file_name().to_string_lossy());
        if e.file_type().unwrap().is_dir() {
            out.insert(path.clone(), Item::Dir);
            walk_host(dir, &path, out);
        } else {
            out.insert(path, Item::File(std::fs::read(e.path()).unwrap()));
        }
    }
}

fn host_tree(dir: &Path) -> BTreeMap<String, Item> {
    let mut t = BTreeMap::new();
    walk_host(dir, "", &mut t);
    t
}

fn walk_fs(fs: &mut dyn FileSystem, dir: &str, out: &mut BTreeMap<String, Item>) {
    let base = if dir == "/" { String::new() } else { dir.to_owned() };
    for e in fs.read_dir(&p(dir)).unwrap() {
        let path = format!("{base}/{}", e.name);
        if e.metadata.is_dir() {
            out.insert(path.clone(), Item::Dir);
            walk_fs(fs, &path, out);
        } else {
            out.insert(path.clone(), Item::File(fs.read_to_vec(&p(&path)).unwrap()));
        }
    }
}

/// Opens an image, retrying briefly while a forked child of another test
/// still holds its lock.
fn open_retry(img: &Path, mode: AccessMode) -> FileDevice {
    for _ in 0..100 {
        match FileDevice::open(img, mode) {
            Err(e) if e.kind() == ErrorKind::InUse => std::thread::sleep(std::time::Duration::from_millis(50)),
            r => return r.unwrap(),
        }
    }
    FileDevice::open(img, mode).unwrap()
}

fn open(img: &Path, mode: AccessMode) -> dix_core::Mounted {
    let mut tries = 0;
    let dev = loop {
        match FileDevice::open(img, mode) {
            Err(e) if e.kind() == ErrorKind::InUse && tries < 100 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            r => break r.unwrap(),
        }
    };
    let m = registry().mount(Box::new(dev), None, mode).unwrap();
    assert_eq!(m.driver.info().id, "fat");
    m
}

fn fs_tree(img: &Path) -> BTreeMap<String, Item> {
    let mut m = open(img, AccessMode::ReadOnly);
    let mut t = BTreeMap::new();
    walk_fs(m.fs.as_mut(), "/", &mut t);
    t
}

fn same(name: &str, got: &BTreeMap<String, Item>, want: &BTreeMap<String, Item>) {
    let missing: Vec<_> = want.keys().filter(|k| !got.contains_key(*k)).collect();
    let extra: Vec<_> = got.keys().filter(|k| !want.contains_key(*k)).collect();
    assert!(missing.is_empty() && extra.is_empty(), "{name}: missing {missing:?}, extra {extra:?}");
    for (k, v) in want {
        assert!(got[k] == *v, "{name}: {k} differs");
    }
}

/// A FAT image of `kind` (12, 16, 32) filled from `src` by mcopy.
fn tool_image(dir: &Path, kind: u32, size_kib: u64, src: &Path) -> PathBuf {
    let img = dir.join(format!("fat{kind}.img"));
    let _ = std::fs::remove_file(&img);
    let (f, size) = (format!("{kind}"), size_kib.to_string());
    ok(dir, "mkfs.fat", &["-C", "-F", &f, "-n", "TOOLS", img.to_str().unwrap(), &size]);
    let entries: Vec<String> =
        std::fs::read_dir(src).unwrap().flatten().map(|e| e.path().to_string_lossy().into_owned()).collect();
    let mut args = vec!["-s", "-i", img.to_str().unwrap()];
    args.extend(entries.iter().map(String::as_str));
    args.push("::/");
    ok(dir, "mcopy", &args);
    img
}

fn fsck(img: &Path) -> (bool, String) {
    run(img.parent().unwrap(), "fsck.fat", &["-n", img.to_str().unwrap()])
}

#[test]
fn reads_what_mtools_writes() {
    if !have() {
        return;
    }
    let s = Scratch::new("read");
    let src = s.0.join("src");
    make_source(&src, 3 << 20);
    let want = host_tree(&src);
    for (kind, kib) in [(12, 8 * 1024), (16, 32 * 1024), (32, 64 * 1024)] {
        let img = tool_image(&s.0, kind, kib, &src);
        let m = open(&img, AccessMode::ReadOnly);
        assert_eq!(m.fs.info().fs_name, format!("FAT{kind}"));
        assert_eq!(m.fs.info().label.as_deref(), Some("TOOLS"));
        drop(m);
        same(&format!("FAT{kind}"), &fs_tree(&img), &want);
    }
}

#[test]
fn writes_that_fsck_and_mtools_accept() {
    if !have() {
        return;
    }
    let s = Scratch::new("write");
    let src = s.0.join("src");
    make_source(&src, 1 << 20);
    for (kind, kib) in [(12, 8 * 1024), (16, 32 * 1024), (32, 64 * 1024)] {
        let img = tool_image(&s.0, kind, kib, &src);
        let big = noise(2 << 20, 99);
        {
            let mut m = open(&img, AccessMode::ReadWrite);
            let fs = m.fs.as_mut();
            fs.write_file(&p("/new.txt"), b"added by the driver").unwrap();
            fs.write_file(&p("/A new file with a Long Name.dat"), b"lfn").unwrap();
            fs.create_dir(&p("/made")).unwrap();
            fs.create_dir(&p("/made/inner folder")).unwrap();
            // A large file in pieces, with a gap that must read as zeroes.
            fs.create_file(&p("/made/big.bin")).unwrap();
            fs.write_at(&p("/made/big.bin"), 0, &big[..1 << 20]).unwrap();
            fs.write_at(&p("/made/big.bin"), 1 << 20, &big[1 << 20..]).unwrap();
            fs.write_file(&p("/sparse.bin"), b"").unwrap();
            fs.write_at(&p("/sparse.bin"), 100_000, b"end").unwrap();
            fs.write_file(&p("/README.TXT"), b"replaced").unwrap();
            // Rename within a folder, move a folder (its ".." changes).
            fs.rename(&p("/lower.txt"), &p("/Renamed To Long.txt")).unwrap();
            fs.rename(&p("/docs"), &p("/made/docs")).unwrap();
            fs.rename(&p("/MixedCase.Txt"), &p("/mixedcase.txt")).unwrap();
            fs.remove_file(&p("/bin/noise.bin")).unwrap();
            fs.remove_dir(&p("/bin")).unwrap();
            for i in 0..150 {
                fs.remove_file(&p(&format!("/many/file number {i:03}.txt"))).unwrap();
            }
            // More files than the freed slots: the folder grows.
            for i in 0..400 {
                fs.write_file(&p(&format!("/many/another file {i:03}.txt")), b"x").unwrap();
            }
            assert_eq!(fs.remove_dir(&p("/many")).unwrap_err().kind(), ErrorKind::DirectoryNotEmpty);
            fs.unmount().unwrap();
        }
        let (good, report) = fsck(&img);
        assert!(good, "FAT{kind}: fsck.fat after writing:\n{report}");
        // What we expect, extracted by mtools and read back by us.
        let mut want = host_tree(&src);
        want.retain(|k, _| !k.starts_with("/bin") && !k.starts_with("/docs"));
        for (k, v) in host_tree(&src.join("docs")) {
            want.insert(format!("/made/docs{k}"), v);
        }
        want.remove("/lower.txt");
        want.remove("/MixedCase.Txt");
        for i in 0..150 {
            want.remove(&format!("/many/file number {i:03}.txt"));
        }
        for i in 0..400 {
            want.insert(format!("/many/another file {i:03}.txt"), Item::File(b"x".to_vec()));
        }
        let mut sparse = vec![0u8; 100_000];
        sparse.extend_from_slice(b"end");
        for (k, v) in [
            ("/new.txt", Item::File(b"added by the driver".to_vec())),
            ("/A new file with a Long Name.dat", Item::File(b"lfn".to_vec())),
            ("/made", Item::Dir),
            ("/made/inner folder", Item::Dir),
            ("/made/docs", Item::Dir),
            ("/made/big.bin", Item::File(big.clone())),
            ("/sparse.bin", Item::File(sparse)),
            ("/README.TXT", Item::File(b"replaced".to_vec())),
            ("/Renamed To Long.txt", Item::File(b"lower case 8.3\n".to_vec())),
            ("/mixedcase.txt", Item::File(b"needs a long name\n".to_vec())),
        ] {
            want.insert(k.to_owned(), v);
        }
        same(&format!("FAT{kind} read back"), &fs_tree(&img), &want);
        let out = s.0.join(format!("out{kind}"));
        std::fs::create_dir_all(&out).unwrap();
        ok(&s.0, "mcopy", &["-s", "-n", "-i", img.to_str().unwrap(), "::/*", out.to_str().unwrap()]);
        same(&format!("FAT{kind} extracted by mcopy"), &host_tree(&out), &want);
    }
}

#[test]
fn a_full_root_and_a_dirty_volume() {
    if !have() {
        return;
    }
    let s = Scratch::new("limits");
    let img = s.0.join("small.img");
    ok(&s.0, "mkfs.fat", &["-C", "-F", "16", "-r", "16", img.to_str().unwrap(), "32768"]);
    // mkfs.fat rounds the root up (cluster alignment): ask the boot sector.
    let boot = std::fs::read(&img).unwrap();
    let root_entries = u16::from_le_bytes([boot[17], boot[18]]);
    {
        let mut m = open(&img, AccessMode::ReadWrite);
        for i in 0..root_entries {
            m.fs.write_file(&p(&format!("/F{i}")), b"x").unwrap();
        }
        let e = m.fs.write_file(&p("/one too many"), b"x").unwrap_err();
        assert_eq!(e.kind(), ErrorKind::NoSpace, "{e}");
        m.fs.unmount().unwrap();
    }
    assert!(fsck(&img).0);
    // Dropped while in use: fsck sees the dirty bit, the next mount says so.
    {
        let m = open(&img, AccessMode::ReadWrite);
        let mut fs = m.fs;
        fs.write_file(&p("/F1"), b"changed").unwrap();
        drop(fs);
    }
    let (_, report) = fsck(&img);
    assert!(report.contains("Dirty bit"), "{report}");
    let dev = open_retry(&img, AccessMode::ReadOnly);
    let log = dix_core::MountLog::new();
    let opts = MountOptions { mode: AccessMode::ReadOnly, log: log.clone() };
    registry().mount_with(Box::new(dev), None, opts).unwrap();
    assert!(log.entries().iter().any(|e| e.message.contains("not cleanly unmounted")), "{:?}", log.entries());
}

/// Volumes made by our mkfs: fsck.fat is happy, mtools reads what we wrote
/// and we read what mtools wrote.
#[test]
fn our_mkfs() {
    if !have() {
        return;
    }
    let s = Scratch::new("mkfs");
    for (variant, size) in [
        ("fat12", 64u64 << 10),
        ("fat12", 2 << 20),
        ("fat16", 4 << 20),
        ("fat16", 200 << 20),
        ("fat32", 34 << 20),
        ("fat32", 600 << 20),
        ("fat32", 5u64 << 30),
    ] {
        let img = s.0.join(format!("{variant}-{size}.img"));
        {
            let mut dev = FileDevice::create(&img, size).unwrap();
            let opts = dix_core::FormatOptions { label: Some("Made Here".into()), variant: Some(variant.into()) };
            registry().format(&mut dev, "fat", &opts).unwrap();
        }
        let (good, report) = fsck(&img);
        assert!(good, "{variant} {size}: fsck.fat right after mkfs:\n{report}");
        let out = ok(&s.0, "minfo", &["-i", img.to_str().unwrap()]);
        assert!(out.contains("MADE HERE"), "{variant}: {out}");
        {
            let mut m = open(&img, AccessMode::ReadWrite);
            assert_eq!(m.fs.info().fs_name, variant.to_uppercase());
            assert_eq!(m.fs.info().label.as_deref(), Some("MADE HERE"));
            assert!(m.fs.read_dir(&p("/")).unwrap().is_empty());
            m.fs.create_dir(&p("/A Folder")).unwrap();
            m.fs.write_file(&p("/A Folder/hello.txt"), b"hello").unwrap();
            m.fs.unmount().unwrap();
        }
        let (good, report) = fsck(&img);
        assert!(good, "{variant} {size}: fsck.fat after writing:\n{report}");
        let got = ok(&s.0, "mtype", &["-i", img.to_str().unwrap(), "::/A Folder/hello.txt"]);
        assert_eq!(got, "hello", "{variant}");
        // mtools fills a third of it; we read that back.
        let third = noise((size / 3).min(64 << 20) as usize, 3);
        let host = s.0.join("third.bin");
        std::fs::write(&host, &third).unwrap();
        ok(&s.0, "mcopy", &["-i", img.to_str().unwrap(), host.to_str().unwrap(), "::/third.bin"]);
        let mut m = open(&img, AccessMode::ReadOnly);
        assert!(m.fs.read_to_vec(&p("/third.bin")).unwrap() == third, "{variant} {size}");
    }
    // Wrong sizes are refused, not half-made.
    let img = s.0.join("tiny.img");
    // (The registry would grow a resizable image to the minimum first.)
    let mut dev = FileDevice::create(&img, 8 << 20).unwrap();
    let e = super::mkfs::mkfs(&mut dev, FatType::Fat32, "").unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidInput, "{e}");
}

// ----- the checker --------------------------------------------------------------

/// A way to damage an image.
type Damage<'a> = Box<dyn Fn(&Path) + 'a>;

fn our_check(img: &Path, repair: bool) -> (dix_core::CheckReport, Vec<String>) {
    let log = dix_core::MountLog::new();
    let mode = if repair { AccessMode::ReadWrite } else { AccessMode::ReadOnly };
    let dev = open_retry(img, mode);
    let opts = dix_core::CheckOptions { log: log.clone(), repair };
    let r = registry().check(Box::new(dev), None, &opts).unwrap();
    (r, log.entries().into_iter().map(|e| format!("{:?}: {}", e.level, e.message)).collect())
}

/// Writes FAT entry `n` (in every copy, or only copy `only`).
fn poke_fat(img: &Path, n: u32, v: u32, only: Option<u32>) {
    let mut bytes = std::fs::read(img).unwrap();
    let l = bpb::Layout::parse(&bytes[..512]).unwrap();
    for copy in 0..l.num_fats {
        if only.is_some_and(|o| o != copy) {
            continue;
        }
        let base = (l.fat_offset + copy as u64 * l.fat_bytes()) as usize;
        match l.fat_type {
            FatType::Fat16 => bytes[base + n as usize * 2..][..2].copy_from_slice(&(v as u16).to_le_bytes()),
            _ => bytes[base + n as usize * 4..][..4].copy_from_slice(&v.to_le_bytes()),
        }
    }
    std::fs::write(img, &bytes).unwrap();
}

fn first_cluster(img: &Path, path: &str) -> u32 {
    let mut m = open(img, AccessMode::ReadOnly);
    m.fs.metadata(&p(path)).unwrap().inode.unwrap() as u32
}

/// The last cluster of the volume (free in these half-empty images).
fn last_cluster(img: &Path) -> u32 {
    let bytes = std::fs::read(img).unwrap();
    bpb::Layout::parse(&bytes[..512]).unwrap().max_cluster()
}

#[test]
fn checker_agrees_with_fsck_fat() {
    if !have() {
        return;
    }
    let s = Scratch::new("check");
    let src = s.0.join("src");
    make_source(&src, 200_000);
    for (kind, kib) in [(16u32, 32 * 1024u64), (32, 64 * 1024)] {
        let clean = tool_image(&s.0, kind, kib, &src);
        let (r, log) = our_check(&clean, false);
        assert!(r.is_clean() && r.warnings == 0, "FAT{kind} clean: {log:#?}");
        assert!(fsck(&clean).0);
        let eoc = if kind == 16 { 0xFFFF } else { 0x0FFF_FFFF };
        let fresh = |name: &str| {
            let img = s.0.join(format!("{name}{kind}.img"));
            std::fs::copy(&clean, &img).unwrap();
            img
        };
        // Repairable damage: both find it; we repair; both find nothing.
        let hello = first_cluster(&clean, "/README.TXT");
        let notes = first_cluster(&clean, "/docs/deep/er");
        let cases: Vec<(&str, Damage<'_>)> = vec![
            ("lost", Box::new(|img: &Path| poke_fat(img, last_cluster(img), eoc, None))),
            (
                "too-long",
                Box::new(move |img: &Path| {
                    let spare = last_cluster(img);
                    poke_fat(img, hello, spare, None);
                    poke_fat(img, spare, eoc, None);
                }),
            ),
            ("mirror", Box::new(move |img: &Path| poke_fat(img, last_cluster(img) - 1, eoc, Some(1)))),
            (
                "dotdot",
                Box::new(move |img: &Path| {
                    // /docs/deep/er's ".." should be /docs/deep: make it the root.
                    let mut bytes = std::fs::read(img).unwrap();
                    let l = bpb::Layout::parse(&bytes[..512]).unwrap();
                    let at = l.cluster_offset(notes) as usize + 32;
                    bytes[at + 26..at + 28].copy_from_slice(&0u16.to_le_bytes());
                    bytes[at + 20..at + 22].copy_from_slice(&0u16.to_le_bytes());
                    std::fs::write(img, &bytes).unwrap();
                }),
            ),
        ];
        for (name, damage) in &cases {
            let img = fresh(name);
            damage(&img);
            let (good, report) = fsck(&img);
            assert!(!good, "FAT{kind} {name}: fsck.fat found nothing:\n{report}");
            let (r, log) = our_check(&img, false);
            assert!(r.errors > 0 && r.repairable, "FAT{kind} {name}: {log:#?}");
            let (r, log) = our_check(&img, true);
            assert!(r.is_clean() && r.fixed > 0, "FAT{kind} {name} repaired: {log:#?}");
            let (good, report) = fsck(&img);
            assert!(good, "FAT{kind} {name}: fsck.fat after our repair:\n{report}");
            assert_eq!(fs_tree(&img), fs_tree(&clean), "FAT{kind} {name}: the files changed");
        }
        // Damage that needs a decision: found by both, left alone.
        let other = first_cluster(&clean, "/lower.txt");
        let hard: Vec<(&str, Damage<'_>)> = vec![
            ("cross", Box::new(move |img: &Path| poke_fat(img, hello, other, None))),
            (
                "short",
                Box::new(|img: &Path| {
                    let mut bytes = std::fs::read(img).unwrap();
                    let at = bytes.windows(11).position(|w| w == b"README  TXT").unwrap();
                    bytes[at + 28..at + 32].copy_from_slice(&1_000_000u32.to_le_bytes());
                    std::fs::write(img, &bytes).unwrap();
                }),
            ),
        ];
        for (name, damage) in &hard {
            let img = fresh(name);
            damage(&img);
            assert!(!fsck(&img).0, "FAT{kind} {name}: fsck.fat found nothing");
            let (r, log) = our_check(&img, false);
            assert!(r.errors > 0 && !r.repairable, "FAT{kind} {name}: {log:#?}");
            let before = std::fs::read(&img).unwrap();
            our_check(&img, true);
            assert!(std::fs::read(&img).unwrap() == before, "FAT{kind} {name}: repair changed something");
        }
    }
}

/// What the deleted-files view shows: path → (note, content).
fn recovered(img: &Path) -> BTreeMap<String, (Option<String>, Option<Vec<u8>>)> {
    let mut fs = crate::recover::deleted_files(Box::new(open_retry(img, AccessMode::ReadOnly))).unwrap();
    let mut out = BTreeMap::new();
    let mut stack = vec!["/".to_owned()];
    while let Some(d) = stack.pop() {
        for e in fs.read_dir(&p(&d)).unwrap() {
            let path = format!("{}/{}", d.trim_end_matches('/'), e.name);
            if e.metadata.kind == EntryKind::Directory {
                stack.push(path.clone());
                out.insert(path, (e.metadata.note, None));
            } else {
                let data = fs.read_to_vec(&p(&path)).unwrap();
                out.insert(path, (e.metadata.note, Some(data)));
            }
        }
    }
    out
}

/// Files and folders deleted by mtools (FAT12, 16 and 32) are found where
/// they were, with their long names (and the short name's first letter
/// worked out), whole; a deleted folder with its files; a file whose space
/// was written over is reported so; what was not deleted is not shown.
#[test]
fn finds_what_mtools_deleted() {
    if !have() || tool("mdel").is_none() || tool("mdeltree").is_none() {
        return;
    }
    let s = Scratch::new("undelete");
    for (kind, kib) in [(12u32, 2048u64), (16, 32 * 1024), (32, 64 * 1024)] {
        let src = s.0.join(format!("src{kind}"));
        let _ = std::fs::remove_dir_all(&src);
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("README.TXT", b"an 8.3 name\n".to_vec()),
            ("A long file name with spaces.markdown", b"long name\n".to_vec()),
            ("Ünïcödé.txt", "unicode ✓".as_bytes().to_vec()),
            ("big.bin", noise(300_017, kind)),
            ("docs/report.doc", noise(20_000, kind + 1)),
            ("old/notes.txt", b"in a deleted folder".to_vec()),
            ("old/more.bin", noise(9_000, kind + 2)),
        ];
        for (rel, d) in &files {
            let path = src.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, d).unwrap();
        }
        std::fs::write(src.join("keep.txt"), b"not deleted").unwrap();
        std::fs::write(src.join("docs/keep.bin"), noise(5000, 99)).unwrap();
        let img = tool_image(&s.0, kind, kib, &src);
        let i = img.to_str().unwrap();
        for f in [
            "::/README.TXT",
            "::/A long file name with spaces.markdown",
            "::/Ünïcödé.txt",
            "::/big.bin",
            "::/docs/report.doc",
        ] {
            ok(&s.0, "mdel", &["-i", i, f]);
        }
        ok(&s.0, "mdeltree", &["-i", i, "::/old"]);
        let got = recovered(&img);
        // A name that fits 8.3 has no long name: its first letter is lost
        // with the deletion (shown as "_", as other tools do).
        let shown = |rel: &str| -> String {
            rel.split('/')
                .map(|c| match c {
                    "README.TXT" | "big.bin" | "report.doc" | "old" | "notes.txt" | "more.bin" => {
                        format!("_{}", &c[1..])
                    }
                    c => c.to_owned(),
                })
                .collect::<Vec<_>>()
                .join("/")
        };
        for (rel, d) in &files {
            let key = format!("/{}", shown(rel));
            let (note, data) =
                got.get(&key).unwrap_or_else(|| panic!("FAT{kind}: {key} not found in {:?}", got.keys()));
            assert_eq!(note.as_deref(), Some("deleted · intact"), "FAT{kind}: {key}");
            assert!(data.as_deref() == Some(&d[..]), "FAT{kind}: {key}: content differs");
        }
        assert!(!got.contains_key("/keep.txt") && !got.contains_key("/docs/keep.bin"), "FAT{kind}: live files shown");
        assert_eq!(got["/docs"].0, None);
        assert_eq!(got["/_ld"].0.as_deref(), Some("deleted folder"), "FAT{kind}");
        // Written over: a new file filling the free space takes the
        // deleted files' space (FAT32 allocates after its last cluster
        // first, so it must wrap around).
        let filler = s.0.join("filler.bin");
        let free = open(&img, AccessMode::ReadOnly).fs.info().free_bytes.unwrap();
        std::fs::write(&filler, noise(free as usize - 64 * 1024, 5)).unwrap();
        ok(&s.0, "mcopy", &["-i", i, filler.to_str().unwrap(), "::/filler.bin"]);
        let got = recovered(&img);
        let note = got["/_ig.bin"].0.clone().unwrap();
        assert!(note.contains("overwritten"), "FAT{kind}: {note}");
    }
}
