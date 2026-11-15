//! exFAT volumes made by `mkfs.exfat` (or by us) and written through
//! `exfat-fuse` (another implementation), read and written by the driver;
//! `fsck.exfat` and `exfat-fuse` judge the result. Needs root, loop
//! devices and FUSE: skipped where they are missing.

use dix_core::device::host::FileDevice;
use dix_core::{AccessMode, ErrorKind, FileSystem, VfsPath};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tool(name: &str) -> Option<PathBuf> {
    ["/usr/sbin", "/sbin", "/usr/bin", "/bin"].iter().map(|d| Path::new(d).join(name)).find(|p| p.exists())
}

fn run(cmd: &str, args: &[&str]) -> (bool, String) {
    match Command::new(tool(cmd).unwrap_or_else(|| PathBuf::from(cmd))).args(args).output() {
        Ok(out) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
        ),
        Err(e) => (false, e.to_string()),
    }
}

fn ok(cmd: &str, args: &[&str]) -> String {
    let (good, out) = run(cmd, args);
    assert!(good, "{cmd} {args:?}: {out}");
    out
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("dix-exfat-{name}-{}", std::process::id()));
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

/// An image mounted through exfat-fuse on a loop device (unmounted on drop).
struct Fuse {
    loopdev: String,
    dir: PathBuf,
}

/// Loop devices and FUSE are shared: one mount at a time.
static FUSE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl Fuse {
    fn mount(img: &Path, dir: &Path) -> Option<Fuse> {
        std::fs::create_dir_all(dir).ok()?;
        let (good, out) = run("losetup", &["-f", "--show", img.to_str()?]);
        if !good {
            eprintln!("losetup: {out}");
            return None;
        }
        let loopdev = out.trim().to_owned();
        let (good, out) = run("mount.exfat-fuse", &[&loopdev, dir.to_str()?]);
        if !good {
            eprintln!("mount.exfat-fuse: {out}");
            run("losetup", &["-d", &loopdev]);
            return None;
        }
        Some(Fuse { loopdev, dir: dir.to_owned() })
    }
}

impl Drop for Fuse {
    fn drop(&mut self) {
        for _ in 0..50 {
            if run("umount", &[self.dir.to_str().unwrap()]).0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        run("losetup", &["-d", &self.loopdev]);
    }
}

/// Whether exFAT can be mounted through FUSE here (found out once: the
/// tests run in parallel and would trip over each other's probe).
fn have() -> bool {
    static HAVE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAVE.get_or_init(probe)
}

fn probe() -> bool {
    let tools = ["mkfs.exfat", "fsck.exfat", "mount.exfat-fuse", "losetup"].iter().all(|t| tool(t).is_some());
    if !tools {
        eprintln!("exfatprogs / exfat-fuse not installed: skipping exFAT test");
        return false;
    }
    // Can we mount at all (root, loop devices, /dev/fuse)?
    let s = Scratch::new("probe");
    let img = s.0.join("probe.img");
    std::fs::File::create(&img).unwrap().set_len(8 << 20).unwrap();
    ok("mkfs.exfat", &[img.to_str().unwrap()]);
    let _lock = FUSE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let works = Fuse::mount(&img, &s.0.join("mnt")).is_some();
    if !works {
        eprintln!("cannot mount exFAT through FUSE here: skipping exFAT test");
    }
    works
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

fn make_source(dir: &Path) {
    let w = |rel: &str, data: &[u8]| {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    };
    w("hello.txt", b"hello exFAT\n");
    w("A rather long file name, longer than fifteen characters.markdown", b"long\n");
    w("ünïcødé – names/файл.txt", "unicode ✓".as_bytes());
    w("docs/deep/er/notes.txt", &b"lots of the same text ".repeat(3000));
    w("bin/noise.bin", &noise(3 << 20, 7));
    w("empty.txt", b"");
    for i in 0..1500 {
        w(&format!("many/a file with a long name, number {i:04}.txt"), format!("{i}").as_bytes());
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

fn p(s: &str) -> VfsPath {
    VfsPath::parse(s).unwrap()
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

/// Opens an image, retrying briefly while it is locked: tests run in
/// parallel and fork tools, and a child forked while an image is open
/// holds its lock until it execs.
fn open_retry(img: &Path, mode: AccessMode) -> dix_core::Result<FileDevice> {
    for _ in 0..100 {
        match FileDevice::open(img, mode) {
            Err(e) if e.kind() == ErrorKind::InUse => std::thread::sleep(std::time::Duration::from_millis(50)),
            r => return r,
        }
    }
    FileDevice::open(img, mode)
}

fn open(img: &Path, mode: AccessMode) -> dix_core::Mounted {
    let dev = open_retry(img, mode).unwrap();
    let m = crate::default_registry().mount(Box::new(dev), None, mode).unwrap();
    assert_eq!(m.fs.info().fs_name, "exFAT");
    m
}

fn fs_tree(img: &Path) -> BTreeMap<String, Item> {
    let mut m = open(img, AccessMode::ReadOnly);
    let mut t = BTreeMap::new();
    walk_fs(m.fs.as_mut(), "/", &mut t);
    t
}

fn same(name: &str, got: &BTreeMap<String, Item>, want: &BTreeMap<String, Item>) {
    let missing: Vec<_> = want.keys().filter(|k| !got.contains_key(*k)).take(5).collect();
    let extra: Vec<_> = got.keys().filter(|k| !want.contains_key(*k)).take(5).collect();
    assert!(missing.is_empty() && extra.is_empty(), "{name}: missing {missing:?}, extra {extra:?}");
    for (k, v) in want {
        assert!(got[k] == *v, "{name}: {k} differs");
    }
}

fn fsck(img: &Path) -> (bool, String) {
    run("fsck.exfat", &["-n", img.to_str().unwrap()])
}

/// A fresh volume from mkfs.exfat, filled through exfat-fuse.
fn tool_image(s: &Scratch, src: &Path) -> PathBuf {
    let img = s.0.join("tools.img");
    std::fs::File::create(&img).unwrap().set_len(128 << 20).unwrap();
    ok("mkfs.exfat", &["-L", "TOOLS", img.to_str().unwrap()]);
    let _lock = FUSE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let m = Fuse::mount(&img, &s.0.join("mnt")).expect("mount");
    ok("cp", &["-r", &format!("{}/.", src.display()), m.dir.to_str().unwrap()]);
    drop(m);
    img
}

#[test]
fn reads_what_exfat_fuse_writes() {
    if !have() {
        return;
    }
    let s = Scratch::new("read");
    let src = s.0.join("src");
    make_source(&src);
    let img = tool_image(&s, &src);
    assert!(fsck(&img).0);
    let m = open(&img, AccessMode::ReadOnly);
    assert_eq!(m.fs.info().label.as_deref(), Some("TOOLS"));
    drop(m);
    same("exfat-fuse → us", &fs_tree(&img), &host_tree(&src));
}

#[test]
fn writes_that_fsck_and_exfat_fuse_accept() {
    if !have() {
        return;
    }
    let s = Scratch::new("write");
    let src = s.0.join("src");
    make_source(&src);
    let img = tool_image(&s, &src);
    let big = noise(5 << 20, 99);
    {
        let mut m = open(&img, AccessMode::ReadWrite);
        let fs = m.fs.as_mut();
        fs.write_file(&p("/new.txt"), b"added by the driver").unwrap();
        fs.create_dir(&p("/made")).unwrap();
        fs.create_dir(&p("/made/inner folder")).unwrap();
        fs.create_file(&p("/made/big.bin")).unwrap();
        fs.write_at(&p("/made/big.bin"), 0, &big[..1 << 20]).unwrap();
        fs.write_at(&p("/made/big.bin"), 1 << 20, &big[1 << 20..]).unwrap();
        fs.write_file(&p("/sparse.bin"), b"").unwrap();
        fs.write_at(&p("/sparse.bin"), 100_000, b"end").unwrap();
        fs.write_file(&p("/hello.txt"), b"replaced").unwrap();
        fs.rename(&p("/empty.txt"), &p("/Renamed to a much longer name.txt")).unwrap();
        fs.rename(&p("/docs"), &p("/made/docs")).unwrap();
        fs.remove_file(&p("/bin/noise.bin")).unwrap();
        fs.remove_dir(&p("/bin")).unwrap();
        // Fragmentation: interleave two growing files.
        fs.create_file(&p("/made/a.bin")).unwrap();
        fs.create_file(&p("/made/b.bin")).unwrap();
        for k in 0..20u64 {
            fs.write_at(&p("/made/a.bin"), k * 40_000, &[k as u8; 40_000]).unwrap();
            fs.write_at(&p("/made/b.bin"), k * 40_000, &[100 + k as u8; 40_000]).unwrap();
        }
        // The folder grows.
        for i in 0..600 {
            fs.write_file(&p(&format!("/made/another file with a long name {i:04}.txt")), b"x").unwrap();
        }
        assert_eq!(fs.remove_dir(&p("/made")).unwrap_err().kind(), ErrorKind::DirectoryNotEmpty);
        fs.unmount().unwrap();
    }
    let (good, report) = fsck(&img);
    assert!(good, "fsck.exfat after writing:\n{report}");
    let mut want = host_tree(&src);
    want.retain(|k, _| !k.starts_with("/bin") && !k.starts_with("/docs"));
    for (k, v) in host_tree(&src.join("docs")) {
        want.insert(format!("/made/docs{k}"), v);
    }
    want.remove("/empty.txt");
    let mut sparse = vec![0u8; 100_000];
    sparse.extend_from_slice(b"end");
    let a: Vec<u8> = (0..20u8).flat_map(|k| vec![k; 40_000]).collect();
    let b: Vec<u8> = (0..20u8).flat_map(|k| vec![100 + k; 40_000]).collect();
    for (k, v) in [
        ("/new.txt", Item::File(b"added by the driver".to_vec())),
        ("/made", Item::Dir),
        ("/made/inner folder", Item::Dir),
        ("/made/docs", Item::Dir),
        ("/made/big.bin", Item::File(big.clone())),
        ("/made/a.bin", Item::File(a)),
        ("/made/b.bin", Item::File(b)),
        ("/sparse.bin", Item::File(sparse)),
        ("/hello.txt", Item::File(b"replaced".to_vec())),
        ("/Renamed to a much longer name.txt", Item::File(Vec::new())),
    ] {
        want.insert(k.to_owned(), v);
    }
    for i in 0..600 {
        want.insert(format!("/made/another file with a long name {i:04}.txt"), Item::File(b"x".to_vec()));
    }
    same("read back by us", &fs_tree(&img), &want);
    let _lock = FUSE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let m = Fuse::mount(&img, &s.0.join("mnt2")).expect("mount");
    same("read by exfat-fuse", &host_tree(&m.dir), &want);
}

#[test]
fn our_mkfs() {
    if !have() {
        return;
    }
    let s = Scratch::new("mkfs");
    for size in [1u64 << 20, 64 << 20, 600 << 20, 5 << 30] {
        let img = s.0.join(format!("{size}.img"));
        {
            let mut dev = FileDevice::create(&img, size).unwrap();
            let opts = dix_core::FormatOptions { label: Some("Made Here".into()), variant: Some("exfat".into()) };
            crate::default_registry().format(&mut dev, "fat", &opts).unwrap();
        }
        let (good, report) = fsck(&img);
        assert!(good, "{size}: fsck.exfat right after mkfs:\n{report}");
        {
            let mut m = open(&img, AccessMode::ReadWrite);
            assert_eq!(m.fs.info().label.as_deref(), Some("Made Here"));
            assert!(m.fs.read_dir(&p("/")).unwrap().is_empty());
            m.fs.create_dir(&p("/A Folder")).unwrap();
            m.fs.write_file(&p("/A Folder/hello.txt"), b"hello").unwrap();
            m.fs.unmount().unwrap();
        }
        let (good, report) = fsck(&img);
        assert!(good, "{size}: fsck.exfat after writing:\n{report}");
        let third = noise((size / 3).min(32 << 20) as usize, 3);
        {
            let _lock = FUSE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let m = Fuse::mount(&img, &s.0.join("mnt")).expect("mount");
            assert_eq!(std::fs::read(m.dir.join("A Folder/hello.txt")).unwrap(), b"hello");
            std::fs::write(m.dir.join("third.bin"), &third).unwrap();
        }
        let mut m = open(&img, AccessMode::ReadOnly);
        assert!(m.fs.read_to_vec(&p("/third.bin")).unwrap() == third, "{size}");
    }
}

#[test]
fn a_dirty_volume_is_reported() {
    if !have() {
        return;
    }
    let s = Scratch::new("dirty");
    let img = s.0.join("d.img");
    std::fs::File::create(&img).unwrap().set_len(16 << 20).unwrap();
    ok("mkfs.exfat", &[img.to_str().unwrap()]);
    {
        let m = open(&img, AccessMode::ReadWrite);
        let mut fs = m.fs;
        fs.write_file(&p("/x"), b"x").unwrap();
        drop(fs); // not unmounted
    }
    let dev = open_retry(&img, AccessMode::ReadOnly).unwrap();
    let log = dix_core::MountLog::new();
    let opts = dix_core::MountOptions { mode: AccessMode::ReadOnly, log: log.clone() };
    crate::default_registry().mount_with(Box::new(dev), None, opts).unwrap();
    assert!(log.entries().iter().any(|e| e.message.contains("not cleanly unmounted")), "{:?}", log.entries());
}

// ----- the checker --------------------------------------------------------------

/// A way to damage an image.
type Damage<'a> = Box<dyn Fn(&Path) + 'a>;

fn our_check(img: &Path, repair: bool) -> (dix_core::CheckReport, Vec<String>) {
    let log = dix_core::MountLog::new();
    let mode = if repair { AccessMode::ReadWrite } else { AccessMode::ReadOnly };
    let dev = open_retry(img, mode).unwrap();
    let opts = dix_core::CheckOptions { log: log.clone(), repair };
    let r = crate::default_registry().check(Box::new(dev), None, &opts).unwrap();
    (r, log.entries().into_iter().map(|e| format!("{:?}: {}", e.level, e.message)).collect())
}

/// The root's entry sets (with their device offsets) and the bitmap's
/// first byte on disk.
fn root_sets(img: &Path) -> (Vec<super::Set>, u64, super::Boot) {
    let bytes = std::fs::read(img).unwrap();
    let boot = super::Boot::parse(&bytes[..512]).unwrap();
    let mut dev = dix_core::device::MemoryDevice::new("x", bytes.clone());
    let mut fat = crate::fat::table::Fat::exfat(boot.fat_offset, boot.fat_bytes, boot.max_cluster());
    let mut slots = Vec::new();
    for c in fat.chain(&mut dev, boot.root_cluster).unwrap() {
        let at = boot.cluster_offset(c) as usize;
        for i in 0..boot.cluster_size as usize / 32 {
            slots.push(((at + 32 * i) as u64, bytes[at + 32 * i..at + 32 * i + 32].try_into().unwrap()));
        }
    }
    let l = super::parse(&slots, None);
    let bitmap = boot.cluster_offset(l.bitmap.unwrap().0);
    (l.sets, bitmap, boot)
}

fn poke(img: &Path, at: u64, f: impl FnOnce(&mut u8)) {
    let mut bytes = std::fs::read(img).unwrap();
    f(&mut bytes[at as usize]);
    std::fs::write(img, &bytes).unwrap();
}

/// Rewrites a set (after changing it) with a correct checksum.
fn rewrite(img: &Path, mut s: super::Set) {
    s.seal();
    let mut bytes = std::fs::read(img).unwrap();
    for (at, e) in s.offsets.iter().zip(&s.raw) {
        bytes[*at as usize..*at as usize + 32].copy_from_slice(e);
    }
    std::fs::write(img, &bytes).unwrap();
}

#[test]
fn checker_agrees_with_fsck_exfat() {
    if !have() {
        return;
    }
    let s = Scratch::new("check");
    let src = s.0.join("src");
    for (name, data) in
        [("hello.txt", noise(10_000, 1)), ("other.txt", noise(20_000, 2)), ("big.bin", noise(300_000, 3))]
    {
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(name), data).unwrap();
    }
    std::fs::create_dir_all(src.join("folder")).unwrap();
    std::fs::write(src.join("folder/inside.txt"), b"inside").unwrap();
    let clean = tool_image(&s, &src);
    let (r, log) = our_check(&clean, false);
    assert!(r.is_clean() && r.warnings == 0, "clean: {log:#?}");
    assert!(fsck(&clean).0);
    let (sets, bitmap, boot) = root_sets(&clean);
    let set = |name: &str| sets.iter().find(|s| s.name == name).unwrap().clone();
    let bit = |c: u32| (bitmap + (c as u64 - 2) / 8, 1u8 << ((c - 2) % 8));
    let fresh = |name: &str| {
        let img = s.0.join(format!("{name}.img"));
        std::fs::copy(&clean, &img).unwrap();
        img
    };
    let last = boot.max_cluster();
    let hello = set("hello.txt").first();
    // Repairable. (fsck.exfat 1.2 does not report bitmap bits set for free
    // clusters, only leaked space: there only we see it.)
    let cases: Vec<(&str, bool, Damage<'_>)> = vec![
        (
            "lost",
            false,
            Box::new(move |img: &Path| {
                let (at, m) = bit(last);
                poke(img, at, |b| *b |= m);
            }),
        ),
        (
            "unmarked",
            true,
            Box::new(move |img: &Path| {
                let (at, m) = bit(hello);
                poke(img, at, |b| *b &= !m);
            }),
        ),
    ];
    for (name, fsck_sees, damage) in &cases {
        let img = fresh(name);
        damage(&img);
        let (good, report) = fsck(&img);
        assert_eq!(!good, *fsck_sees, "{name}: fsck.exfat:\n{report}");
        let (r, log) = our_check(&img, false);
        assert!(r.errors > 0 && r.repairable, "{name}: {log:#?}");
        let (r, log) = our_check(&img, true);
        assert!(r.is_clean() && r.fixed > 0, "{name} repaired: {log:#?}");
        let (good, report) = fsck(&img);
        assert!(good, "{name}: fsck.exfat after our repair:\n{report}");
        assert!(fs_tree(&img) == fs_tree(&clean), "{name}: the files changed");
    }
    // The backup boot region (fsck.exfat does not compare it; we restore it).
    let img = fresh("backup");
    poke(&img, 12 * 512 + 100, |b| *b ^= 0xFF);
    let (r, log) = our_check(&img, false);
    assert!(r.errors == 0 && r.warnings == 1, "backup: {log:#?}");
    our_check(&img, true);
    assert!(our_check(&img, false).0.warnings == 0);
    // Needs a decision: found by both, left alone.
    let other = set("other.txt");
    let hard: Vec<(&str, Damage<'_>)> = vec![
        (
            "checksum",
            Box::new(move |img: &Path| {
                let s = set("hello.txt");
                poke(img, s.offsets[0] + 12, |b| *b ^= 1);
            }),
        ),
        (
            "cross",
            Box::new(move |img: &Path| {
                let mut s = other.clone();
                s.raw[1][20..24].copy_from_slice(&hello.to_le_bytes());
                rewrite(img, s);
            }),
        ),
    ];
    for (name, damage) in &hard {
        let img = fresh(name);
        damage(&img);
        assert!(!fsck(&img).0, "{name}: fsck.exfat found nothing");
        let (r, log) = our_check(&img, false);
        assert!(r.errors > 0 && !r.repairable, "{name}: {log:#?}");
        let before = std::fs::read(&img).unwrap();
        our_check(&img, true);
        assert!(std::fs::read(&img).unwrap() == before, "{name}: repair changed something");
    }
}

/// Files and folders deleted through exfat-fuse are found where they
/// were, with their names, whole; a deleted folder with its files; what
/// was not deleted is not shown.
#[test]
fn finds_what_exfat_fuse_deleted() {
    if !have() {
        return;
    }
    let s = Scratch::new("undelete");
    let img = s.0.join("u.img");
    std::fs::File::create(&img).unwrap().set_len(64 << 20).unwrap();
    ok("mkfs.exfat", &[img.to_str().unwrap()]);
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("photo.jpg", noise(1_500_017, 1)),
        ("Ünïcödé — 日本語.txt", "unicode".as_bytes().to_vec()),
        ("a name longer than fifteen characters.txt", b"three name entries".to_vec()),
        ("docs/report.pdf", noise(200_000, 2)),
        ("old/notes.txt", b"in a deleted folder".to_vec()),
        ("old/more.bin", noise(90_000, 3)),
    ];
    {
        let _lock = FUSE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let m = Fuse::mount(&img, &s.0.join("mnt")).expect("mount");
        for (rel, d) in &files {
            let p = m.dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, d).unwrap();
        }
        std::fs::write(m.dir.join("keep.txt"), b"not deleted").unwrap();
        std::fs::write(m.dir.join("docs/keep.bin"), noise(5000, 9)).unwrap();
        ok("sync", &[]);
        for rel in ["photo.jpg", "Ünïcödé — 日本語.txt", "a name longer than fifteen characters.txt", "docs/report.pdf"]
        {
            std::fs::remove_file(m.dir.join(rel)).unwrap();
        }
        std::fs::remove_dir_all(m.dir.join("old")).unwrap();
        drop(m);
    }
    let mut fs =
        crate::recover::deleted_files(Box::new(FileDevice::open(&img, AccessMode::ReadOnly).unwrap())).unwrap();
    let mut got: BTreeMap<String, (Option<String>, Option<Vec<u8>>)> = BTreeMap::new();
    let mut stack = vec!["/".to_owned()];
    while let Some(d) = stack.pop() {
        for e in fs.read_dir(&VfsPath::parse(&d).unwrap()).unwrap() {
            let p = format!("{}/{}", d.trim_end_matches('/'), e.name);
            if e.metadata.kind == dix_core::EntryKind::Directory {
                stack.push(p.clone());
                got.insert(p, (e.metadata.note, None));
            } else {
                let data = fs.read_to_vec(&VfsPath::parse(&p).unwrap()).unwrap();
                got.insert(p, (e.metadata.note, Some(data)));
            }
        }
    }
    for (rel, d) in &files {
        let key = format!("/{rel}");
        let (note, data) = got.get(&key).unwrap_or_else(|| panic!("{key} not found in {:?}", got.keys()));
        assert_eq!(note.as_deref(), Some("deleted · intact"), "{key}");
        assert!(data.as_deref() == Some(&d[..]), "{key}: content differs");
    }
    assert!(!got.contains_key("/keep.txt") && !got.contains_key("/docs/keep.bin"), "live files shown");
    assert_eq!(got["/docs"].0, None);
    assert_eq!(got["/old"].0.as_deref(), Some("deleted folder"));
}
