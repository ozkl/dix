//! Archives made by zip / GNU tar / gzip / xz / bzip2 / zstd, read by the
//! driver; archives written by the driver, read by those tools.

use super::*;
use dix_core::device::host::{FileDevice, open_path};
use dix_core::{AccessMode, DriverRegistry};
use std::path::{Path, PathBuf};
use std::process::Command;

fn tool(name: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/local/bin"].iter().map(|d| Path::new(d).join(name)).find(|p| p.exists())
}

fn have(tools: &[&str]) -> bool {
    let ok = tools.iter().all(|t| tool(t).is_some());
    if !ok {
        eprintln!("{tools:?} not installed: skipping archive test");
    }
    ok
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("dix-arc-{name}-{}", std::process::id()));
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

fn run(dir: &Path, cmd: &str, args: &[&str]) -> Vec<u8> {
    // A UTF-8 locale: unzip otherwise writes non-ASCII names as "#U00fc".
    let out = Command::new(tool(cmd).unwrap()).args(args).current_dir(dir).env("LC_ALL", "C.UTF-8").output().unwrap();
    assert!(out.status.success(), "{cmd} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
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
    Link(String),
}

/// A pseudo-random, poorly compressible buffer.
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

const LONG: &str =
    "a-rather-long-folder-name-that-goes-on/and-another-long-folder-name-inside-it/with-a-file-at-the-end-of-it.txt";

/// The tree the tool archives are made from.
fn make_source(dir: &Path) {
    let w = |rel: &str, data: &[u8]| {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    };
    w("hello.txt", b"hello archive\n");
    w("docs/readme.md", b"# Read me\n");
    w("docs/deep/er/notes.txt", &b"lots of the same text ".repeat(5000));
    w("bin/noise.bin", &noise(3 << 20, 7));
    w("empty.txt", b"");
    w(LONG, b"long path content");
    w("ünïcødé/файл.txt", "unicode ✓".as_bytes());
    std::fs::create_dir_all(dir.join("empty-folder")).unwrap();
    std::os::unix::fs::symlink("docs/readme.md", dir.join("link-to-readme")).unwrap();
}

fn walk_host(dir: &Path, rel: &str, out: &mut BTreeMap<String, Item>) {
    for e in std::fs::read_dir(dir.join(rel.trim_start_matches('/'))).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = format!("{rel}/{name}");
        let ft = e.file_type().unwrap();
        if ft.is_symlink() {
            out.insert(path, Item::Link(std::fs::read_link(e.path()).unwrap().to_string_lossy().into_owned()));
        } else if ft.is_dir() {
            out.insert(path.clone(), Item::Dir);
            walk_host(dir, &path, out);
        } else {
            out.insert(path, Item::File(std::fs::read(e.path()).unwrap()));
        }
    }
}

fn walk_fs(fs: &mut dyn FileSystem, dir: &str, out: &mut BTreeMap<String, Item>) {
    let base = if dir == "/" { String::new() } else { dir.to_owned() };
    for e in fs.read_dir(&p(dir)).unwrap() {
        let path = format!("{base}/{}", e.name);
        match e.metadata.kind {
            EntryKind::Directory => {
                out.insert(path.clone(), Item::Dir);
                walk_fs(fs, &path, out);
            }
            EntryKind::Symlink => {
                out.insert(path, Item::Link(e.metadata.symlink_target.unwrap()));
            }
            _ => {
                out.insert(path.clone(), Item::File(fs.read_to_vec(&p(&path)).unwrap()));
            }
        }
    }
}

fn host_tree(dir: &Path) -> BTreeMap<String, Item> {
    let mut t = BTreeMap::new();
    walk_host(dir, "", &mut t);
    t
}

/// Mounts an image (as the app does: a part of a split set opens them
/// all), retrying briefly while it is locked: tests run in parallel and fork
/// tools, and a child forked while an image is open holds its lock until it
/// execs.
fn open(img: &Path, mode: AccessMode) -> Result<dix_core::Mounted> {
    let mut tries = 0;
    let dev = loop {
        match open_path(img, mode) {
            Err(e) if e.kind() == ErrorKind::InUse && tries < 100 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            r => break r?,
        }
    };
    registry().mount(dev, None, mode)
}

/// Mounts just this one file (not the rest of a split set).
fn open_alone(img: &Path) -> Result<dix_core::Mounted> {
    let mut tries = 0;
    let dev = loop {
        match FileDevice::open(img, AccessMode::ReadOnly) {
            Err(e) if e.kind() == ErrorKind::InUse && tries < 100 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            r => break r?,
        }
    };
    registry().mount(Box::new(dev), None, AccessMode::ReadOnly)
}

fn fs_tree(img: &Path) -> BTreeMap<String, Item> {
    let mut m = open(img, AccessMode::ReadOnly).unwrap();
    assert_eq!(m.driver.info().id, "archive", "{}", img.display());
    let mut t = BTreeMap::new();
    walk_fs(m.fs.as_mut(), "/", &mut t);
    t
}

/// Every archive the tools make reads back as the source folder.
#[test]
fn reads_what_the_tools_make() {
    if !have(&["zip", "tar", "gzip", "xz", "bzip2", "zstd"]) {
        return;
    }
    let s = Scratch::new("read");
    let src = s.0.join("src");
    make_source(&src);
    let want = host_tree(&src);
    let cases: &[(&str, &str, &[&str])] = &[
        ("a.zip", "zip", &["-q", "-r", "-y", "../a.zip", "."]),
        (
            "ustar.tar",
            "tar",
            &[
                "--format=ustar",
                "-cf",
                "../ustar.tar",
                "hello.txt",
                "docs",
                "bin",
                "empty.txt",
                "empty-folder",
                "link-to-readme",
                "ünïcødé",
            ],
        ),
        ("gnu.tar", "tar", &["--format=gnu", "-cf", "../gnu.tar", "."]),
        ("pax.tar", "tar", &["--format=pax", "-cf", "../pax.tar", "."]),
        ("a.tar.gz", "tar", &["-czf", "../a.tar.gz", "."]),
        ("a.tar.xz", "tar", &["-cJf", "../a.tar.xz", "."]),
        ("a.tar.bz2", "tar", &["-cjf", "../a.tar.bz2", "."]),
        ("a.tar.zst", "tar", &["--zstd", "-cf", "../a.tar.zst", "."]),
    ];
    for (name, cmd, args) in cases {
        run(&src, cmd, args);
        let got = fs_tree(&s.0.join(name));
        let mut expected: BTreeMap<String, Item> = want
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    match v {
                        Item::File(d) => Item::File(d.clone()),
                        Item::Dir => Item::Dir,
                        Item::Link(l) => Item::Link(l.clone()),
                    },
                )
            })
            .collect();
        if *name == "ustar.tar" {
            // ustar cannot hold the long path (tar leaves it out).
            expected.retain(|k, _| !k.starts_with("/a-rather"));
        }
        let missing: Vec<_> = expected.keys().filter(|k| !got.contains_key(*k)).collect();
        let extra: Vec<_> = got.keys().filter(|k| !expected.contains_key(*k)).collect();
        assert!(missing.is_empty() && extra.is_empty(), "{name}: missing {missing:?}, extra {extra:?}");
        for (k, v) in &expected {
            assert!(got[k] == *v, "{name}: {k} differs");
        }
    }
}

#[test]
fn single_compressed_files() {
    if !have(&["gzip", "xz", "bzip2", "zstd"]) {
        return;
    }
    let s = Scratch::new("single");
    let data = b"just one file, compressed\n".repeat(1000);
    std::fs::write(s.0.join("notes.txt"), &data).unwrap();
    for (cmd, args, out) in [
        ("gzip", &["-k", "notes.txt"][..], "notes.txt.gz"),
        ("xz", &["-k", "notes.txt"][..], "notes.txt.xz"),
        ("bzip2", &["-k", "notes.txt"][..], "notes.txt.bz2"),
        ("zstd", &["-q", "-k", "notes.txt"][..], "notes.txt.zst"),
    ] {
        run(&s.0, cmd, args);
        // A renamed .gz: its header still knows the name. The others take
        // it from the file name.
        let img = if cmd == "gzip" {
            let renamed = s.0.join("x.gz");
            std::fs::rename(s.0.join(out), &renamed).unwrap();
            renamed
        } else {
            s.0.join(out)
        };
        let tree = fs_tree(&img);
        assert_eq!(tree.len(), 1, "{out}: {:?}", tree.keys().collect::<Vec<_>>());
        assert_eq!(tree.get("/notes.txt"), Some(&Item::File(data.clone())), "{out}");
        let e = open(&img, AccessMode::ReadWrite).err().unwrap();
        assert_eq!(e.kind(), ErrorKind::ReadOnly, "{out}: {e}");
    }
}

/// Changing archives: the tools read the result.
#[test]
fn writes_what_the_tools_read() {
    if !have(&["zip", "unzip", "tar", "zstd"]) {
        return;
    }
    let s = Scratch::new("write");
    let src = s.0.join("src");
    make_source(&src);
    run(&src, "zip", &["-q", "-r", "-y", "../a.zip", "."]);
    run(&src, "tar", &["-cf", "../a.tar", "."]);
    run(&src, "tar", &["-czf", "../a.tar.gz", "."]);
    run(&src, "tar", &["--zstd", "-cf", "../a.tar.zst", "."]);
    run(&src, "tar", &["-cjf", "../a.tar.bz2", "."]);
    run(&src, "tar", &["-cJf", "../a.tar.xz", "."]);
    let seven = have(&["7z"]);
    if seven {
        // Made by 7-Zip with its BCJ filter (all in one block): rewritten
        // here with LZMA2.
        run(&src, "7z", &["a", "-t7z", "-snl", "-bso0", "-bsp0", "-mf=BCJ", "../a.7z", "."]);
    }
    let big = noise(1 << 20, 99);
    for name in ["a.zip", "a.tar", "a.tar.gz", "a.tar.zst", "a.tar.bz2", "a.tar.xz", "a.7z"] {
        if name.ends_with(".7z") && !seven {
            continue;
        }
        let img = s.0.join(name);
        {
            let mut m = open(&img, AccessMode::ReadWrite).unwrap();
            let fs = m.fs.as_mut();
            assert!(fs.capabilities().contains(Capabilities::WRITE));
            fs.write_file(&p("/new.txt"), b"added by the driver").unwrap();
            fs.create_dir(&p("/made")).unwrap();
            fs.write_file(&p("/made/big.bin"), &big).unwrap();
            fs.write_file(&p("/hello.txt"), b"replaced").unwrap();
            fs.rename(&p("/docs"), &p("/made/docs")).unwrap();
            fs.remove_file(&p("/bin/noise.bin")).unwrap();
            fs.remove_dir(&p("/bin")).unwrap();
            fs.unmount().unwrap();
        }
        let mut want = host_tree(&src);
        want.retain(|k, _| !k.starts_with("/bin") && !k.starts_with("/docs"));
        for (k, v) in host_tree(&src.join("docs")) {
            want.insert(format!("/made/docs{k}"), v);
        }
        want.insert("/made/docs".into(), Item::Dir);
        want.insert("/made".into(), Item::Dir);
        want.insert("/made/big.bin".into(), Item::File(big.clone()));
        want.insert("/new.txt".into(), Item::File(b"added by the driver".to_vec()));
        want.insert("/hello.txt".into(), Item::File(b"replaced".to_vec()));
        // Read back by us...
        assert!(fs_tree(&img) == want, "{name}: read back differs");
        // ...and by the tools.
        let out = s.0.join(format!("x-{name}"));
        std::fs::create_dir_all(&out).unwrap();
        let path = img.to_str().unwrap();
        if name.ends_with(".zip") {
            run(&s.0, "unzip", &["-tq", path]);
            run(&out, "unzip", &["-q", path]);
        } else if name.ends_with(".xz") {
            run(&s.0, "xz", &["-t", path]);
            run(&out, "tar", &["-xJf", path]);
        } else if name.ends_with(".7z") {
            run(&s.0, "7z", &["t", "-bso0", "-bsp0", path]);
            run(&out, "7z", &["x", "-snl", "-bso0", "-bsp0", path]);
        } else if name.ends_with(".zst") {
            run(&out, "tar", &["--zstd", "-xf", path]);
        } else {
            run(&out, "tar", &["-xf", path]);
        }
        let got = host_tree(&out);
        assert!(got == want, "{name}: extracted by the tool differs: {:?}", got.keys().collect::<Vec<_>>());
    }
}

/// New archives (the "New image" types), and zip64 for many members.
#[test]
fn new_archives_and_zip64() {
    if !have(&["unzip", "tar", "zstd"]) {
        return;
    }
    let s = Scratch::new("new");
    for v in VARIANTS {
        let img = s.0.join(format!("new.{}", v.id));
        {
            let mut dev = FileDevice::create(&img, 1 << 20).unwrap();
            let opts = FormatOptions { label: Some("made here".into()), variant: Some(v.id.into()) };
            registry().format(&mut dev, "archive", &opts).unwrap();
            assert!(dev.size() < 1 << 20, "{}: shrunk to the archive", v.id);
        }
        {
            let mut m = open(&img, AccessMode::ReadWrite).unwrap();
            assert!(m.fs.read_dir(&p("/")).unwrap().is_empty());
            m.fs.write_file(&p("/one.txt"), b"1").unwrap();
            m.fs.unmount().unwrap();
        }
        if v.id.starts_with("cpio") && !have(&["cpio"]) {
            continue;
        }
        let cat = match v.id {
            "cpio.gz" => "gzip -dc",
            "cpio.zst" => "zstd -dc",
            "cpio.xz" => "xz -dc",
            _ => "cat",
        };
        if v.id == "7z" && !have(&["7z"]) {
            continue;
        }
        let listed = match v.id {
            "zip" => run(&s.0, "unzip", &["-Z1", img.to_str().unwrap()]),
            "7z" => {
                run(&s.0, "7z", &["t", "-bso0", "-bsp0", img.to_str().unwrap()]);
                assert_eq!(run(&s.0, "7z", &["e", "-so", "-bso0", "-bsp0", img.to_str().unwrap(), "one.txt"]), b"1");
                b"one.txt".to_vec()
            }
            id if id.starts_with("cpio") => {
                run(&s.0, "sh", &["-c", &format!("{cat} '{}' | cpio -it --quiet", img.display())])
            }
            "tar.zst" => run(&s.0, "tar", &["--zstd", "-tf", img.to_str().unwrap()]),
            _ => run(&s.0, "tar", &["-tf", img.to_str().unwrap()]),
        };
        assert_eq!(String::from_utf8_lossy(&listed).trim(), "one.txt", "{}", v.id);
    }
    // 70000 members: more than a zip without zip64 can count.
    let img = s.0.join("many.zip");
    {
        let mut dev = FileDevice::create(&img, 0).unwrap();
        registry().format(&mut dev, "archive", &FormatOptions { label: None, variant: Some("zip".into()) }).unwrap();
    }
    {
        let mut m = open(&img, AccessMode::ReadWrite).unwrap();
        for d in 0..70 {
            m.fs.create_dir(&p(&format!("/d{d}"))).unwrap();
            for f in 0..1000 {
                m.fs.write_file(&p(&format!("/d{d}/f{f}")), format!("{d}/{f}").as_bytes()).unwrap();
            }
        }
        m.fs.unmount().unwrap();
    }
    let list = run(&s.0, "unzip", &["-Z1", img.to_str().unwrap()]);
    assert_eq!(String::from_utf8_lossy(&list).lines().count(), 70_070);
    let one = run(&s.0, "unzip", &["-p", img.to_str().unwrap(), "d69/f999"]);
    assert_eq!(one, b"69/999");
    let mut m = open(&img, AccessMode::ReadOnly).unwrap();
    assert_eq!(m.fs.read_dir(&p("/d3")).unwrap().len(), 1000);
}

#[test]
fn dos_times_round_trip() {
    for t in [315532800i64, 1_700_000_000, 4_102_444_798] {
        let (time, date) = dos_time(t);
        assert_eq!(from_dos_time(date, time), Some(t - t % 2));
    }
}

#[test]
fn not_an_archive() {
    let mut dev = dix_core::device::MemoryDevice::new("x", vec![0u8; 4096]);
    assert_eq!(ArchiveDriver.probe(&mut dev).unwrap(), 0);
    let mut dev = dix_core::device::MemoryDevice::new("x", b"just some text".to_vec());
    assert_eq!(ArchiveDriver.probe(&mut dev).unwrap(), 0);
}

/// 7z archives made by 7-Zip with every method we read; the ones we do not
/// read say why.
#[cfg(feature = "archive-7z")]
#[test]
fn reads_7z() {
    if !have(&["7z"]) {
        return;
    }
    let s = Scratch::new("7z");
    let src = s.0.join("src");
    make_source(&src);
    // Real x86 code, for the BCJ filter.
    for exe in ["ls", "cp"] {
        if let Some(p) = tool(exe) {
            std::fs::copy(p, src.join(format!("bin/{exe}"))).unwrap();
        }
    }
    let want = host_tree(&src);
    let cases: &[(&str, &[&str])] = &[
        ("default.7z", &[]),
        ("lzma.7z", &["-m0=LZMA"]),
        ("not-solid.7z", &["-ms=off"]),
        ("plain-header.7z", &["-mhc=off"]),
        ("copy.7z", &["-m0=Copy"]),
        ("deflate.7z", &["-m0=Deflate"]),
        ("bzip2.7z", &["-m0=BZip2"]),
        ("delta.7z", &["-m0=Delta:4", "-m1=LZMA2"]),
        ("bcj.7z", &["-mf=BCJ"]),
    ];
    for (name, extra) in cases {
        let mut args = vec!["a", "-t7z", "-snl", "-bso0", "-bsp0"];
        args.extend_from_slice(extra);
        let out = format!("../{name}");
        args.extend([out.as_str(), "."]);
        run(&src, "7z", &args);
        let got = fs_tree(&s.0.join(name));
        let missing: Vec<_> = want.keys().filter(|k| !got.contains_key(*k)).collect();
        let extra: Vec<_> = got.keys().filter(|k| !want.contains_key(*k)).collect();
        assert!(missing.is_empty() && extra.is_empty(), "{name}: missing {missing:?}, extra {extra:?}");
        for (k, v) in &want {
            assert!(got[k] == *v, "{name}: {k} differs");
        }
    }
    // The BCJ archive really used the filter.
    let listing = run(&s.0, "7z", &["l", "-slt", "bcj.7z"]);
    assert!(String::from_utf8_lossy(&listing).contains("BCJ"), "no BCJ in bcj.7z");

    // What we cannot read is listed, and reading says why.
    let read_error = |name: &str, file: &str| {
        let mut m = open(&s.0.join(name), AccessMode::ReadOnly).unwrap();
        m.fs.read_to_vec(&p(file)).unwrap_err().to_string()
    };
    run(&src, "7z", &["a", "-t7z", "-psecret", "-bso0", "-bsp0", "../secret.7z", "hello.txt"]);
    assert!(read_error("secret.7z", "/hello.txt").contains("encrypted"));
    // Read-write, but nothing is written without the password (it is
    // written encrypted again).
    let mut m = open(&s.0.join("secret.7z"), AccessMode::ReadWrite).unwrap();
    let e = m.fs.write_file(&p("/new.txt"), b"x").and_then(|_| m.fs.sync()).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::PasswordRequired, "{e}");
    drop(m);
    run(&src, "7z", &["a", "-t7z", "-psecret", "-mhe=on", "-bso0", "-bsp0", "../hidden.7z", "hello.txt"]);
    // An encrypted list of files: empty until the password is given.
    let mut m = open(&s.0.join("hidden.7z"), AccessMode::ReadOnly).unwrap();
    assert!(m.fs.read_dir(&p("/")).unwrap().is_empty());
    m.fs.add_password("secret").unwrap();
    assert_eq!(m.fs.read_to_vec(&p("/hello.txt")).unwrap(), b"hello archive\n");
}

/// zip members are read in pieces without unpacking them whole: forward,
/// skipping ahead, going back; damage is caught by the CRC at the end.
#[test]
fn zip_members_stream() {
    if !have(&["zip"]) {
        return;
    }
    let s = Scratch::new("zip-stream");
    let src = s.0.join("src");
    std::fs::create_dir_all(&src).unwrap();
    // Compressible (deflated) and incompressible (stored with -0 below).
    let text: Vec<u8> = (0..3_000_000u32).flat_map(|i| format!("line {i}\n").into_bytes()).take(5_000_000).collect();
    let rand = noise(2_000_000, 5);
    std::fs::write(src.join("text.txt"), &text).unwrap();
    std::fs::write(src.join("rand.bin"), &rand).unwrap();
    run(&src, "zip", &["-q", "../deflated.zip", "text.txt", "rand.bin"]);
    run(&src, "zip", &["-q", "-0", "../stored.zip", "text.txt", "rand.bin"]);
    for name in ["deflated.zip", "stored.zip"] {
        let mut m = open(&s.0.join(name), AccessMode::ReadOnly).unwrap();
        for (file, want) in [("/text.txt", &text), ("/rand.bin", &rand)] {
            let path = p(file);
            // Odd-sized pieces, front to back.
            let mut got = Vec::new();
            let mut buf = vec![0u8; 77_777];
            loop {
                let n = m.fs.read(&path, got.len() as u64, &mut buf).unwrap();
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            assert!(got == *want, "{name}{file}: sequential read differs");
            // Jump ahead, then back.
            for at in [4_000_000usize, 10, 1_999_000, 0] {
                let at = at.min(want.len() - 1);
                let mut b = vec![0u8; 1000];
                let n = m.fs.read(&path, at as u64, &mut b).unwrap();
                assert_eq!(&b[..n], &want[at..(at + 1000).min(want.len())], "{name}{file} at {at}");
            }
            assert!(m.fs.read_to_vec(&path).unwrap() == *want, "{name}{file}: whole read differs");
        }
    }
    // A damaged deflated member: reading to the end reports it.
    let mut bytes = std::fs::read(s.0.join("deflated.zip")).unwrap();
    let at = bytes.windows(8).position(|w| w == b"text.txt").unwrap() + 8 + 2000;
    bytes[at] ^= 0x40;
    std::fs::write(s.0.join("bad.zip"), &bytes).unwrap();
    let mut m = open(&s.0.join("bad.zip"), AccessMode::ReadOnly).unwrap();
    let e = m.fs.read_to_vec(&p("/text.txt")).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Corrupted, "{e}");
}

// ----- cpio -------------------------------------------------------------------

/// Runs a shell pipeline in `dir`.
fn sh(dir: &Path, script: &str) -> Vec<u8> {
    run(dir, "sh", &["-c", script])
}

/// Every cpio format GNU cpio and bsdcpio write, plain and compressed,
/// reads back as the source folder (hard links included).
#[test]
fn reads_cpio_made_by_the_tools() {
    if !have(&["cpio", "bsdcpio", "gzip", "xz", "zstd"]) {
        return;
    }
    let s = Scratch::new("cpio-read");
    let src = s.0.join("src");
    make_source(&src);
    std::fs::hard_link(src.join("docs/readme.md"), src.join("hard-link.md")).unwrap();
    let want = host_tree(&src);
    let cases = [
        ("newc.cpio", "find . | cpio -o -H newc --quiet > ../newc.cpio"),
        ("crc.cpio", "find . | cpio -o -H crc --quiet > ../crc.cpio"),
        ("odc.cpio", "find . | cpio -o -H odc --quiet > ../odc.cpio"),
        ("bin.cpio", "find . | cpio -o -H bin --quiet > ../bin.cpio"),
        ("bsd-newc.cpio", "find . | bsdcpio -o --format newc --quiet > ../bsd-newc.cpio"),
        ("bsd-odc.cpio", "find . | bsdcpio -o --format odc --quiet > ../bsd-odc.cpio"),
        ("a.cpio.gz", "find . | cpio -o -H newc --quiet | gzip > ../a.cpio.gz"),
        ("a.cpio.xz", "find . | cpio -o -H newc --quiet | xz --check=crc32 > ../a.cpio.xz"),
        ("a.cpio.zst", "find . | cpio -o -H newc --quiet | zstd -q > ../a.cpio.zst"),
    ];
    for (name, script) in cases {
        sh(&src, script);
        let got = fs_tree(&s.0.join(name));
        let missing: Vec<_> = want.keys().filter(|k| !got.contains_key(*k)).collect();
        let extra: Vec<_> = got.keys().filter(|k| !want.contains_key(*k)).collect();
        assert!(missing.is_empty() && extra.is_empty(), "{name}: missing {missing:?}, extra {extra:?}");
        for (k, v) in &want {
            assert!(got[k] == *v, "{name}: {k} differs");
        }
    }
    // Permissions and times come through.
    let mut m = open(&s.0.join("newc.cpio"), AccessMode::ReadOnly).unwrap();
    let meta = m.fs.metadata(&p("/hello.txt")).unwrap();
    let host = std::fs::metadata(src.join("hello.txt")).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(meta.mode, Some(host.mode()));
    assert_eq!(meta.modified, Some(host.mtime()));
}

/// Changes written back are read by GNU cpio; device nodes, FIFOs and hard
/// links survive the rewrite.
#[test]
fn writes_cpio_the_tools_read() {
    if !have(&["cpio", "gzip", "mknod", "mkfifo"]) {
        return;
    }
    let s = Scratch::new("cpio-write");
    let src = s.0.join("src");
    make_source(&src);
    // (Taken before the FIFO exists: reading one would block.)
    let source = host_tree(&src);
    std::fs::create_dir(src.join("dev")).unwrap();
    // Device nodes need root: without, only the FIFO.
    let devices = Command::new(tool("mknod").unwrap())
        .args(["dev/console", "c", "5", "1"])
        .current_dir(&src)
        .status()
        .unwrap()
        .success();
    sh(&src, "mkfifo dev/initctl");
    let mut cases = vec![("x.cpio", "cat", "cat"), ("x.cpio.gz", "gzip", "gzip -dc")];
    if have(&["xz"]) {
        // (The kernel reads xz initramfs with CRC32 checks.)
        cases.push(("x.cpio.xz", "xz --check=crc32", "xz -dc"));
    }
    for (name, pack, unpack) in cases {
        sh(&src, &format!("find . | cpio -o -H newc --quiet | {pack} > ../{name}"));
        let img = s.0.join(name);
        {
            let mut m = open(&img, AccessMode::ReadWrite).unwrap();
            m.fs.write_file(&p("/added.txt"), b"added by us").unwrap();
            m.fs.remove_file(&p("/docs/readme.md")).unwrap();
            m.fs.rename(&p("/hello.txt"), &p("/docs/hello.txt")).unwrap();
            let fifo = m.fs.metadata(&p("/dev/initctl")).unwrap();
            assert_eq!((fifo.kind, fifo.mode.map(|m| m & 0o170000)), (EntryKind::Other, Some(0o010000)));
            m.fs.unmount().unwrap();
        }
        if name.ends_with(".xz") {
            // Written with CRC32 checks, as the kernel needs.
            let info = run(&s.0, "xz", &["--robot", "-lv", img.to_str().unwrap()]);
            assert!(String::from_utf8_lossy(&info).contains("CRC32"), "{}", String::from_utf8_lossy(&info));
        }
        let out = s.0.join(format!("out-{name}"));
        std::fs::create_dir(&out).unwrap();
        sh(&out, &format!("{unpack} '{}' | cpio -idm --quiet", img.display()));
        let listing =
            String::from_utf8(sh(&s.0, &format!("{unpack} '{}' | cpio -itv --quiet", img.display()))).unwrap();
        use std::os::unix::fs::FileTypeExt;
        assert!(std::fs::symlink_metadata(out.join("dev/initctl")).unwrap().file_type().is_fifo());
        if devices {
            let console = listing.lines().find(|l| l.ends_with("dev/console")).expect("dev/console listed");
            assert!(console.starts_with('c') && console.contains("5,"), "{console}");
            assert!(std::fs::symlink_metadata(out.join("dev/console")).unwrap().file_type().is_char_device());
        }
        std::fs::remove_dir_all(out.join("dev")).unwrap();
        let mut want: BTreeMap<String, Item> = source
            .iter()
            .map(|(k, v)| {
                let v = match v {
                    Item::File(d) => Item::File(d.clone()),
                    Item::Dir => Item::Dir,
                    Item::Link(l) => Item::Link(l.clone()),
                };
                (k.clone(), v)
            })
            .collect();
        want.remove("/docs/readme.md");
        let hello = want.remove("/hello.txt").unwrap();
        want.insert("/docs/hello.txt".into(), hello);
        want.insert("/added.txt".into(), Item::File(b"added by us".to_vec()));
        assert_eq!(host_tree(&out), want, "{name}");
    }
}

/// An initramfs with early microcode: an uncompressed cpio, then a
/// compressed one. Both are read; writing is refused.
#[test]
fn concatenated_initramfs() {
    if !have(&["cpio", "zstd"]) {
        return;
    }
    let s = Scratch::new("cpio-initrd");
    let early = s.0.join("early");
    std::fs::create_dir_all(early.join("kernel/x86/microcode")).unwrap();
    std::fs::write(early.join("kernel/x86/microcode/GenuineIntel.bin"), noise(5000, 3)).unwrap();
    let main = s.0.join("main");
    make_source(&main);
    sh(&early, "find . | cpio -o -H newc --quiet > ../early.cpio");
    sh(&main, "find . | cpio -o -H newc --quiet | zstd -q > ../main.cpio.zst");
    sh(&s.0, "cat early.cpio main.cpio.zst > initrd.img");
    let img = s.0.join("initrd.img");
    let got = fs_tree(&img);
    assert_eq!(got["/kernel/x86/microcode/GenuineIntel.bin"], Item::File(noise(5000, 3)));
    for (k, v) in host_tree(&main) {
        assert!(got[&k] == v, "{k} differs");
    }
    let e = open(&img, AccessMode::ReadWrite).err().expect("read-write refused");
    assert_eq!(e.kind(), ErrorKind::ReadOnly);
}

// ----- RAR ------------------------------------------------------------------------

/// A folder for the RAR tests: the usual source, plus x86 executables
/// (rar uses its E8/E8E9 filters on them), a multi-channel gradient (the
/// delta filter) and a hard link.
fn rar_source(dir: &Path) {
    make_source(dir);
    for b in ["/bin/ls", "/usr/bin/xorriso", "/bin/bash"] {
        if Path::new(b).exists() {
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            std::fs::copy(b, dir.join("bin").join(Path::new(b).file_name().unwrap())).unwrap();
        }
    }
    let gradient: Vec<u8> = (0..600_000u32).map(|i| ((i % 3) * 40 + (i / 3) / 7) as u8).collect();
    std::fs::write(dir.join("gradient.raw"), gradient).unwrap();
    std::fs::hard_link(dir.join("hello.txt"), dir.join("docs/hard-link.txt")).unwrap();
}

#[test]
fn reads_rar_made_by_rar() {
    if !have(&["rar", "unrar"]) {
        return;
    }
    let s = Scratch::new("rar");
    let src = s.0.join("src");
    rar_source(&src);
    let want = host_tree(&src);
    let cases: &[(&str, &[&str])] = &[
        ("store.rar", &["-m0"]),
        ("fast.rar", &["-m1"]),
        ("normal.rar", &["-m3"]),
        ("best.rar", &["-m5", "-md64m"]),
        ("solid.rar", &["-m5", "-s", "-ow"]),
        ("small-dict.rar", &["-m5", "-s", "-md128k"]),
        ("times.rar", &["-m3", "-tsm4", "-tsc4", "-tsa4"]),
    ];
    for (name, opts) in cases {
        let mut args = vec!["a", "-idq", "-r", "-ol", "-oh"];
        args.extend_from_slice(opts);
        let out = format!("../{name}");
        args.extend_from_slice(&[&out, "."]);
        run(&src, "rar", &args);
        // unrar itself must agree that the archive is sound.
        run(&s.0, "unrar", &["t", "-idq", name]);
        let got = fs_tree(&s.0.join(name));
        let missing: Vec<_> = want.keys().filter(|k| !got.contains_key(*k)).collect();
        let extra: Vec<_> = got.keys().filter(|k| !want.contains_key(*k)).collect();
        assert!(missing.is_empty() && extra.is_empty(), "{name}: missing {missing:?}, extra {extra:?}");
        for (k, v) in &want {
            assert!(got[k] == *v, "{name}: {k} differs");
        }
    }
    // Pieces at odd places, and going back in a solid archive.
    let mut m = open(&s.0.join("solid.rar"), AccessMode::ReadOnly).unwrap();
    assert!(!m.fs.capabilities().is_writable());
    let big = noise(3 << 20, 7);
    let bash = std::fs::read(src.join("bin/bash")).ok();
    for (off, len) in [(3_000_000u64, 100_000usize), (5, 10), (1 << 20, 1 << 20), (0, 1)] {
        let mut buf = vec![0u8; len];
        let n = m.fs.read(&p("/bin/noise.bin"), off, &mut buf).unwrap();
        assert_eq!(&buf[..n], &big[off as usize..off as usize + n]);
        if let Some(b) = &bash {
            let off = off.min(b.len() as u64 - 1);
            let n = m.fs.read(&p("/bin/bash"), off, &mut buf).unwrap();
            assert_eq!(&buf[..n], &b[off as usize..off as usize + n], "bash at {off}");
        }
    }
    // Members in the archive's own order (each continues the solid stream)
    // and then backwards (each starts it again).
    let order = String::from_utf8(
        Command::new(tool("unrar").unwrap()).args(["lb", "solid.rar"]).current_dir(&s.0).output().unwrap().stdout,
    )
    .unwrap();
    let files: Vec<String> =
        order.lines().filter(|l| src.join(l).is_file() && !src.join(l).is_symlink()).map(str::to_owned).collect();
    assert!(files.len() > 5);
    for f in files.iter().chain(files.iter().rev()) {
        let got = m.fs.read_to_vec(&p(&format!("/{f}"))).unwrap();
        assert!(got == std::fs::read(src.join(f)).unwrap(), "{f}");
    }
    let meta = m.fs.metadata(&p("/hello.txt")).unwrap();
    let host = std::fs::metadata(src.join("hello.txt")).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(meta.mode, Some(host.mode()));
    assert_eq!(meta.modified, Some(host.mtime()));
    // (rar -ow stores owner names, not numbers: nothing to compare.)
}

#[test]
fn rar_that_cannot_be_read() {
    if !have(&["rar"]) {
        return;
    }
    let s = Scratch::new("rar-locked");
    let src = s.0.join("src");
    make_source(&src);
    // Encrypted files: listed, not readable.
    run(&src, "rar", &["a", "-idq", "-r", "-psecret", "../enc.rar", "hello.txt", "docs"]);
    let mut m = open(&s.0.join("enc.rar"), AccessMode::ReadOnly).unwrap();
    assert_eq!(m.fs.metadata(&p("/hello.txt")).unwrap().size, 14);
    let e = m.fs.read_to_vec(&p("/hello.txt")).unwrap_err();
    assert!(e.to_string().contains("encrypted"), "{e}");
    drop(m);
    // Encrypted headers: nothing listed without the password.
    run(&src, "rar", &["a", "-idq", "-hpsecret", "../hidden.rar", "hello.txt"]);
    let mut m = open(&s.0.join("hidden.rar"), AccessMode::ReadOnly).unwrap();
    assert!(m.fs.read_dir(&p("/")).unwrap().is_empty());
    assert!(m.fs.take_password_request().unwrap().contains("encrypted"));
    drop(m);
    // One volume on its own: it lists what it holds; files split into the
    // others say so.
    run(&src, "rar", &["a", "-idq", "-m0", "-v1m", "../vol.rar", "bin/noise.bin"]);
    let part = |n: &str| {
        std::fs::read_dir(&s.0)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.file_name().unwrap().to_string_lossy().starts_with(n))
            .unwrap()
    };
    let mut m = open_alone(&part("vol.part1")).unwrap();
    let e = m.fs.read_to_vec(&p("/bin/noise.bin")).unwrap_err();
    assert!(e.to_string().contains("next volume, which is missing"), "{e}");
    drop(m);
    let mut m = open_alone(&part("vol.part2")).unwrap();
    let e = m.fs.read_to_vec(&p("/bin/noise.bin")).unwrap_err();
    assert!(e.to_string().contains("previous volume"), "{e}");
    drop(m);
    // Read-write is refused.
    let e = open(&s.0.join("enc.rar"), AccessMode::ReadWrite).err().unwrap();
    assert_eq!(e.kind(), ErrorKind::ReadOnly);
}

/// RAR 4 (which rar 7 no longer writes): a stored archive made here.
#[test]
fn rar4_stored_files() {
    fn header(kind: u8, flags: u16, body: &[u8]) -> Vec<u8> {
        let mut h = vec![0, 0, kind];
        h.extend_from_slice(&flags.to_le_bytes());
        h.extend_from_slice(&((7 + body.len()) as u16).to_le_bytes());
        h.extend_from_slice(body);
        let crc = (crate::archive::tests::crc(&h[2..]) & 0xFFFF) as u16;
        h[..2].copy_from_slice(&crc.to_le_bytes());
        h
    }
    let file = |name: &str, data: &[u8], method: u8, flags: u16| {
        let mut b = Vec::new();
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.push(3); // Unix
        b.extend_from_slice(&crc(data).to_le_bytes());
        let (time, date) = dix_core::time::to_dos(1_700_000_000);
        b.extend_from_slice(&((date as u32) << 16 | time as u32).to_le_bytes());
        b.push(29);
        b.push(method);
        b.extend_from_slice(&(name.len() as u16).to_le_bytes());
        b.extend_from_slice(&0o100644u32.to_le_bytes());
        b.extend_from_slice(name.as_bytes());
        let mut h = header(0x74, 0x8000 | flags, &b);
        h.extend_from_slice(data);
        h
    };
    let mut a = b"Rar!\x1a\x07\x00".to_vec();
    a.extend(header(0x73, 0, &[0; 6]));
    a.extend(file("hello.txt", b"hello rar4\n", 0x30, 0));
    a.extend(file("docs\\notes.txt", &noise(5000, 1), 0x30, 0));
    a.extend(file("packed.bin", b"compressed bytes", 0x33, 0));
    a.extend(header(0x7B, 0x4000, &[]));
    let s = Scratch::new("rar4");
    let img = s.0.join("old.rar");
    std::fs::write(&img, &a).unwrap();
    let mut m = open(&img, AccessMode::ReadOnly).unwrap();
    assert_eq!(m.fs.read_to_vec(&p("/hello.txt")).unwrap(), b"hello rar4\n");
    assert_eq!(m.fs.read_to_vec(&p("/docs/notes.txt")).unwrap(), noise(5000, 1));
    assert_eq!(m.fs.metadata(&p("/hello.txt")).unwrap().modified, Some(1_700_000_000));
    let e = m.fs.read_to_vec(&p("/packed.bin")).unwrap_err();
    // (Compressed, but not really: damaged data.)
    assert_eq!(e.kind(), ErrorKind::Corrupted, "{e}");
    if have(&["unrar"]) {
        // unrar agrees with the archive made here.
        run(&s.0, "unrar", &["t", "-idq", "old.rar", "hello.txt"]);
    }
    // Volumes (x.rar, x.r00, x.r01): a stored file over all three. A
    // part's header has the whole size; its checksum is of the part's
    // data, the last part's of the whole file.
    let data = noise(30_000, 9);
    let pieces = [&data[..10_000], &data[10_000..25_000], &data[25_000..]];
    for (i, piece) in pieces.iter().enumerate() {
        let last = i == pieces.len() - 1;
        let mut v = b"Rar!\x1a\x07\x00".to_vec();
        v.extend(header(0x73, 0x0001 | if i == 0 { 0x0100 } else { 0 }, &[0; 6]));
        if i == 0 {
            v.extend(file("small.txt", b"in the first volume\n", 0x30, 0));
        }
        let mut f = file("big.bin", piece, 0x30, if i > 0 { 0x01 } else { 0 } | if last { 0 } else { 0x02 });
        // Whole size; checksum.
        f[11..15].copy_from_slice(&(data.len() as u32).to_le_bytes());
        let sum = if last { crc(&data) } else { crc(piece) };
        f[16..20].copy_from_slice(&sum.to_le_bytes());
        let hcrc = (crc(&f[2..f.len() - piece.len()]) & 0xFFFF) as u16;
        f[..2].copy_from_slice(&hcrc.to_le_bytes());
        v.extend(f);
        // End: "another volume follows" but on the last.
        v.extend(header(0x7B, 0x4000 | if last { 0 } else { 0x0001 }, &[]));
        let name = if i == 0 { "vol.rar".to_owned() } else { format!("vol.r{:02}", i - 1) };
        std::fs::write(s.0.join(name), v).unwrap();
    }
    if have(&["unrar"]) {
        run(&s.0, "unrar", &["t", "-idq", "vol.rar"]);
    }
    for part in ["vol.rar", "vol.r00", "vol.r01"] {
        let mut m = open(&s.0.join(part), AccessMode::ReadOnly).unwrap();
        assert_eq!(m.fs.read_to_vec(&p("/big.bin")).unwrap(), data, "{part}");
        assert_eq!(m.fs.read_to_vec(&p("/small.txt")).unwrap(), b"in the first volume\n");
        let mut buf = vec![0u8; 8000];
        assert_eq!(m.fs.read(&p("/big.bin"), 6000, &mut buf).unwrap(), 8000);
        assert_eq!(buf, &data[6000..14_000]);
    }
    let mut m = open_alone(&s.0.join("vol.r00")).unwrap();
    let e = m.fs.read_to_vec(&p("/big.bin")).unwrap_err();
    assert!(e.to_string().contains("previous volume"), "{e}");
}

pub(super) fn crc(b: &[u8]) -> u32 {
    dix_core::partition::crc32(b)
}

// ----- split archives ---------------------------------------------------------------

/// The parts in `dir` whose names start with `prefix`, sorted.
fn parts_of(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
        .collect();
    v.sort();
    v
}

/// Multi-volume RAR, split zip and 7z, and a split tar, opened from one of
/// their parts (as in the app), read back as the source folder.
#[test]
fn reads_split_archives() {
    if !have(&["rar", "unrar", "zip", "7z", "tar", "split"]) {
        return;
    }
    let s = Scratch::new("split");
    let src = s.0.join("src");
    rar_source(&src);
    let want = host_tree(&src);
    let rar = |name: &str, opts: &[&str]| {
        let mut args = vec!["a", "-idq", "-r", "-ol", "-oh"];
        args.extend_from_slice(opts);
        let out = format!("../{name}");
        args.extend_from_slice(&[&out, "."]);
        run(&src, "rar", &args);
    };
    rar("v.rar", &["-m3", "-v400k"]);
    rar("solid.rar", &["-m5", "-s", "-v300k"]);
    rar("store.rar", &["-m0", "-v700k"]);
    // The old naming (x.rar, x.r00, x.r01, …): rar 7 uses it only for RAR 4,
    // which it no longer writes; the names do not change the content.
    rar("old.rar", &["-m3", "-v500k"]);
    for (i, part) in parts_of(&s.0, "old.part").iter().enumerate() {
        let name = if i == 0 { "old.rar".to_owned() } else { format!("old.r{:02}", i - 1) };
        std::fs::rename(part, s.0.join(name)).unwrap();
    }
    run(&src, "zip", &["-q", "-r", "-y", "-s", "400k", "../z.zip", "."]);
    run(&src, "7z", &["a", "-t7z", "-snl", "-bso0", "-bsp0", "-v400k", "../s.7z", "."]);
    run(&src, "tar", &["-cf", "../t.tar", "."]);
    run(&s.0, "split", &["-b", "300k", "-d", "-a", "3", "--numeric-suffixes=1", "t.tar", "t.tar."]);
    std::fs::remove_file(s.0.join("t.tar")).unwrap();
    // Which part is opened: the first, a middle one, the last.
    let cases: Vec<(&str, PathBuf)> = vec![
        ("v.part", parts_of(&s.0, "v.part")[2].clone()),
        ("solid.part", parts_of(&s.0, "solid.part").last().unwrap().clone()),
        ("store.part", parts_of(&s.0, "store.part")[0].clone()),
        ("old.", s.0.join("old.r01")),
        ("z.", s.0.join("z.zip")),
        ("z.", s.0.join("z.z02")),
        ("s.7z.", s.0.join("s.7z.002")),
        ("t.tar.", s.0.join("t.tar.003")),
    ];
    for (prefix, img) in &cases {
        let n = parts_of(&s.0, prefix).len();
        assert!(n >= 3, "{prefix}: {n} parts");
        if prefix.ends_with(".part") || *prefix == "old." {
            let first = if *prefix == "old." { s.0.join("old.rar") } else { parts_of(&s.0, prefix)[0].clone() };
            run(&s.0, "unrar", &["t", "-idq", first.to_str().unwrap()]);
        }
        let mut m = open(img, AccessMode::ReadOnly).unwrap();
        let mut got = BTreeMap::new();
        walk_fs(m.fs.as_mut(), "/", &mut got);
        let name = img.display();
        let missing: Vec<_> = want.keys().filter(|k| !got.contains_key(*k)).collect();
        let extra: Vec<_> = got.keys().filter(|k| !want.contains_key(*k)).collect();
        assert!(missing.is_empty() && extra.is_empty(), "{name}: missing {missing:?}, extra {extra:?}");
        for (k, v) in &want {
            assert!(got[k] == *v, "{name}: {k} differs");
        }
        // Read-only, whatever was asked for.
        assert!(!m.fs.capabilities().is_writable(), "{name}");
    }
    // The last part of a split zip alone says what is missing.
    let e = open_alone(&s.0.join("z.zip")).err().unwrap();
    assert!(e.to_string().contains("other parts"), "{e}");
    // A volume missing after the first: what the first holds whole reads,
    // a file going on into the missing one says so.
    std::fs::remove_file(&parts_of(&s.0, "store.part")[1]).unwrap();
    let mut m = open(&parts_of(&s.0, "store.part")[0], AccessMode::ReadOnly).unwrap();
    let (mut ok, mut cut) = (0, 0);
    for (k, v) in &want {
        if let (Item::File(data), Ok(meta)) = (v, m.fs.metadata(&p(k))) {
            if meta.kind != EntryKind::File {
                continue;
            }
            match m.fs.read_to_vec(&p(k)) {
                Ok(got) => {
                    assert!(got == *data, "{k}");
                    ok += 1;
                }
                Err(e) => {
                    assert!(e.to_string().contains("next volume, which is missing"), "{k}: {e}");
                    cut += 1;
                }
            }
        }
    }
    assert!(ok > 0 && cut == 1, "{ok} read, {cut} cut");
}

// ----- ARJ ------------------------------------------------------------------------

/// A folder for the ARJ tests: nested files, an executable, poorly and
/// very compressible data, and text long enough for many Huffman blocks.
fn arj_source(dir: &Path) {
    let w = |rel: &str, data: &[u8]| {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    };
    w("hello.txt", b"hello arj\n");
    w("empty.txt", b"");
    w("docs/deep/notes.txt", &b"lots of the same text ".repeat(5000));
    w("bin/noise.bin", &noise(700_000, 3));
    // Words picked pseudo-randomly: compressible, but not trivially.
    let words = ["alpha", "beta", "gamma", "delta", "archive", "volume", "header", "the", "of", "and", "\n"];
    let mut x = 12345u32;
    let mut text = Vec::new();
    while text.len() < 1_500_000 {
        x = x.wrapping_mul(1103515245).wrapping_add(12345);
        text.extend_from_slice(words[(x >> 16) as usize % words.len()].as_bytes());
        text.push(b' ');
    }
    w("docs/words.txt", &text);
    if let Some(ls) = tool("ls") {
        std::fs::copy(ls, dir.join("bin/ls")).unwrap();
    }
    std::fs::set_permissions(dir.join("hello.txt"), std::os::unix::fs::PermissionsExt::from_mode(0o640)).unwrap();
}

/// Archives made by arj 3.10 with each method, and in volumes (opened from
/// any of them), read back as the source folder.
#[test]
fn reads_arj_made_by_arj() {
    if !have(&["arj"]) {
        return;
    }
    let s = Scratch::new("arj");
    let src = s.0.join("src");
    arj_source(&src);
    let want = host_tree(&src);
    let arj = |name: &str, opts: &[&str]| {
        let mut args = vec!["a", "-r", "-y"];
        args.extend_from_slice(opts);
        let out = format!("../{name}");
        args.extend_from_slice(&[&out, "."]);
        run(&src, "arj", &args);
        if !opts.iter().any(|o| o.starts_with("-g")) {
            run(&s.0, "arj", &["t", "-y", name]);
        }
    };
    let cases: &[(&str, &[&str])] = &[
        ("m0.arj", &["-m0"]),
        ("m1.arj", &["-m1"]),
        ("m2.arj", &["-m2"]),
        ("m3.arj", &["-m3"]),
        ("m4.arj", &["-m4"]),
        ("vol.arj", &["-m1", "-v300k"]),
        ("vol4.arj", &["-m4", "-v250k"]),
    ];
    for (name, opts) in cases {
        arj(name, opts);
    }
    let mut opened = vec![];
    for (name, _) in cases {
        opened.push(s.0.join(name));
    }
    // Volumes from a middle and the last part too.
    assert!(s.0.join("vol.a03").exists());
    opened.push(s.0.join("vol.a02"));
    let last = parts_of(&s.0, "vol4.").into_iter().filter(|p| p.extension().unwrap() != "arj").max().unwrap();
    opened.push(last);
    for img in &opened {
        let name = img.display();
        let mut m = open(img, AccessMode::ReadOnly).unwrap();
        assert_eq!(m.driver.info().id, "archive", "{name}");
        let mut got = BTreeMap::new();
        walk_fs(m.fs.as_mut(), "/", &mut got);
        // arj stores no empty folders: compare files.
        let files = |t: &BTreeMap<String, Item>| -> Vec<String> {
            t.iter().filter(|(_, v)| matches!(v, Item::File(_))).map(|(k, _)| k.clone()).collect()
        };
        assert_eq!(files(&got), files(&want), "{name}");
        for k in files(&want) {
            assert!(got[&k] == want[&k], "{name}: {k} differs");
        }
        let meta = m.fs.metadata(&p("/hello.txt")).unwrap();
        let host = std::fs::metadata(src.join("hello.txt")).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(meta.mode.map(|m| m & 0o7777), Some(0o640), "{name}");
        assert_eq!(meta.modified, Some(host.mtime()), "{name}");
        assert!(!m.fs.capabilities().is_writable());
    }
    // Pieces out of order of a compressed member.
    let mut m = open(&s.0.join("m1.arj"), AccessMode::ReadOnly).unwrap();
    let words = std::fs::read(src.join("docs/words.txt")).unwrap();
    for (off, len) in [(1_000_000usize, 5000usize), (3, 10), (1_499_000, 4000)] {
        let mut buf = vec![0u8; len];
        let n = m.fs.read(&p("/docs/words.txt"), off as u64, &mut buf).unwrap();
        assert_eq!(&buf[..n], &words[off..(off + len).min(words.len())]);
    }
    drop(m);
    // Garbled (password) files: listed, not readable.
    run(&src, "arj", &["a", "-y", "-gsecret", "../garbled.arj", "hello.txt"]);
    let mut m = open(&s.0.join("garbled.arj"), AccessMode::ReadOnly).unwrap();
    assert_eq!(m.fs.metadata(&p("/hello.txt")).unwrap().size, 10);
    let e = m.fs.read_to_vec(&p("/hello.txt")).unwrap_err();
    assert!(e.to_string().contains("garbled"), "{e}");
    drop(m);
    // A volume alone: a file going on into the next one says so.
    let mut m = open_alone(&s.0.join("vol.arj")).unwrap();
    let mut errors = 0;
    for (k, v) in &want {
        let Item::File(data) = v else { continue };
        if m.fs.metadata(&p(k)).is_err() {
            continue; // in a later volume
        }
        match m.fs.read_to_vec(&p(k)) {
            Ok(got) => assert!(got == *data, "{k}"),
            Err(e) => {
                assert!(e.to_string().contains("next volume, which is missing"), "{k}: {e}");
                errors += 1;
            }
        }
    }
    assert_eq!(errors, 1);
    // Read-write is refused.
    let e = open(&s.0.join("m1.arj"), AccessMode::ReadWrite).err().unwrap();
    assert_eq!(e.kind(), ErrorKind::ReadOnly);
}

// ----- PKZIP methods --------------------------------------------------------------

/// A zip member as given: (name, method, flags, packed bytes, content).
type Member<'a> = (&'a str, u16, u16, Vec<u8>, Vec<u8>);

/// A zip of members as given.
fn zip_of(members: &[Member<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, method, flags, packed, content) in members {
        let need: u16 = match method {
            0..=6 => 10,
            8 | 9 => 21,
            12 => 46,
            _ => 63,
        };
        let crc = crc(content);
        let offset = out.len() as u32;
        let fixed = |v: &mut Vec<u8>| {
            v.extend_from_slice(&need.to_le_bytes());
            v.extend_from_slice(&flags.to_le_bytes());
            v.extend_from_slice(&method.to_le_bytes());
            v.extend_from_slice(&[0, 0, 0x21, 0]); // time, date
            v.extend_from_slice(&crc.to_le_bytes());
            v.extend_from_slice(&(packed.len() as u32).to_le_bytes());
            v.extend_from_slice(&(content.len() as u32).to_le_bytes());
            v.extend_from_slice(&(name.len() as u16).to_le_bytes());
            v.extend_from_slice(&[0, 0]);
        };
        out.extend_from_slice(&0x0403_4B50u32.to_le_bytes());
        fixed(&mut out);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(packed);
        central.extend_from_slice(&0x0201_4B50u32.to_le_bytes());
        central.extend_from_slice(&0x0314u16.to_le_bytes());
        fixed(&mut central);
        central.extend_from_slice(&[0; 6]); // comment, disk, internal attributes
        central.extend_from_slice(&(0o100644u32 << 16).to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_at = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4B50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&(members.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_at.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

/// Bits, least significant first.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, v: u32, bits: u32) {
        self.acc |= ((v as u64) & ((1u64 << bits) - 1)) << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// Shrink (LZW with partial clearing), mirroring what the decoder does:
/// a partial clear whenever the table is full, and every `clear_every`
/// codes as well.
fn shrink(data: &[u8], clear_every: usize) -> Vec<u8> {
    const MAX: usize = 8192;
    const FREE: u16 = u16::MAX;
    let mut w = BitWriter::default();
    let mut bits = 9;
    let mut strings: Vec<Vec<u8>> = (0..MAX).map(|c| if c < 256 { vec![c as u8] } else { Vec::new() }).collect();
    let mut parent = vec![FREE; MAX];
    let mut map: std::collections::HashMap<Vec<u8>, u16> = (0..256).map(|c| (vec![c as u8], c as u16)).collect();
    let mut next = 257;
    let mut pos = 0;
    let mut emitted = 0;
    let mut just_cleared = false;
    while pos < data.len() {
        let mut k = 1;
        while pos + k < data.len() && map.contains_key(&data[pos..pos + k + 1]) {
            k += 1;
        }
        // Right after a clear, UnZip checks a not-yet-defined code against
        // the free code from before the clear: send a shorter string.
        while just_cleared && k > 1 && map[&data[pos..pos + k]] as usize == next {
            k -= 1;
        }
        just_cleared = false;
        let code = map[&data[pos..pos + k]] as usize;
        while code >= 1 << bits {
            w.put(256, bits);
            w.put(1, bits);
            bits += 1;
        }
        w.put(code as u32, bits);
        emitted += 1;
        pos += k;
        let mut is_parent = vec![false; MAX];
        for c in 257..MAX {
            if parent[c] != FREE {
                is_parent[parent[c] as usize] = true;
            }
        }
        // Clear when nearly full (or now and then), but not while the code just
        // sent would be freed: the next new code extends it, and UnZip
        // (like PKZIP) expects that one to stay.
        if (next >= MAX - 256 || emitted % clear_every == 0) && (code < 256 || is_parent[code]) {
            w.put(256, bits);
            w.put(2, bits);
            just_cleared = true;
            for c in 257..MAX {
                if !is_parent[c] && parent[c] != FREE {
                    parent[c] = FREE;
                    map.remove(&strings[c]);
                }
            }
            next = (257..MAX).find(|&c| parent[c] == FREE).unwrap_or(MAX);
        }
        if pos < data.len() && next < MAX {
            let mut s = strings[code].clone();
            s.push(data[pos]);
            // (A string may already have a code, freed and back: keep the
            // newest, as the decoder will.)
            map.insert(s.clone(), next as u16);
            strings[next] = s;
            parent[next] = code as u16;
            next = (next + 1..MAX).find(|&c| parent[c] == FREE).unwrap_or(MAX);
        }
    }
    w.finish()
}

/// Greedy LZ77 matches (hash chains over 3 bytes): (position, length, distance).
fn lz_matches(data: &[u8], window: usize, min: usize, max: usize) -> Vec<(usize, usize, usize)> {
    let mut chains: std::collections::HashMap<[u8; 3], Vec<usize>> = std::collections::HashMap::new();
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let mut best = (0, 0);
        if pos + 3 <= data.len() {
            let key = [data[pos], data[pos + 1], data[pos + 2]];
            if let Some(c) = chains.get(&key) {
                for &q in c.iter().rev().take(32) {
                    if pos - q > window {
                        break;
                    }
                    let mut l = 0;
                    while pos + l < data.len() && l < max && data[q + l] == data[pos + l] {
                        l += 1;
                    }
                    if l > best.0 {
                        best = (l, pos - q);
                    }
                }
            }
        }
        let step = if best.0 >= min { best.0 } else { 1 };
        if best.0 >= min {
            out.push((pos, best.0, best.1));
        }
        for i in pos..pos + step {
            if i + 3 <= data.len() {
                chains.entry([data[i], data[i + 1], data[i + 2]]).or_default().push(i);
            }
        }
        pos += step;
    }
    out
}

/// Reduce with factor `f`: matches as DLE sequences, then follower sets
/// (up to 8 per byte, from the symbol counts).
fn reduce(data: &[u8], f: u32) -> Vec<u8> {
    const DLE: u8 = 144;
    let mask = 0xFFusize >> f;
    let window = ((0xFFusize >> (8 - f)) << 8) + 256;
    let mut syms = Vec::new();
    let matches = lz_matches(data, window, 4, 3 + mask + 255);
    let mut mi = 0;
    let mut pos = 0;
    while pos < data.len() {
        if mi < matches.len() && matches[mi].0 == pos {
            let (_, len, dist) = matches[mi];
            mi += 1;
            let d = dist - 1;
            let field = (len - 3).min(mask);
            syms.push(DLE);
            syms.push((((d >> 8) << (8 - f)) | field) as u8);
            if field == mask {
                syms.push((len - 3 - mask) as u8);
            }
            syms.push(d as u8);
            pos += len;
        } else {
            syms.push(data[pos]);
            if data[pos] == DLE {
                syms.push(0);
            }
            pos += 1;
        }
    }
    // Follower sets: each byte's most frequent followers.
    let mut counts = vec![[0u32; 256]; 256];
    let mut last = 0u8;
    for &s in &syms {
        counts[last as usize][s as usize] += 1;
        last = s;
    }
    let sets: Vec<Vec<u8>> = (0..256)
        .map(|j| {
            let mut v: Vec<u8> = (0..=255u8).filter(|&c| counts[j][c as usize] > 1).collect();
            v.sort_by_key(|&c| std::cmp::Reverse(counts[j][c as usize]));
            v.truncate(8);
            v
        })
        .collect();
    let need = |n: usize| match n {
        0..=2 => 1,
        3..=4 => 2,
        _ => 3,
    };
    let mut w = BitWriter::default();
    for j in (0..256).rev() {
        w.put(sets[j].len() as u32, 6);
        for &c in &sets[j] {
            w.put(c as u32, 8);
        }
    }
    let mut last = 0u8;
    for &s in &syms {
        let set = &sets[last as usize];
        if set.is_empty() {
            w.put(s as u32, 8);
        } else if let Some(i) = set.iter().position(|&c| c == s) {
            w.put(0, 1);
            w.put(i as u32, need(set.len()));
        } else {
            w.put(1, 1);
            w.put(s as u32, 8);
        }
        last = s;
    }
    w.finish()
}

/// Canonical codes for `lens` (as deflate assigns them).
fn canonical(lens: &[u8]) -> Vec<u32> {
    let mut count = [0u32; 17];
    for &l in lens {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut next = [0u32; 17];
    let mut code = 0;
    for l in 1..17 {
        code = (code + count[l - 1]) << 1;
        next[l] = code;
    }
    lens.iter()
        .map(|&l| {
            let c = next[l as usize];
            next[l as usize] += 1;
            c
        })
        .collect()
}

/// Implode with fixed but uneven code lengths; `big`: an 8 KiB window;
/// `literals`: literals are coded too. Returns the data and its flags.
fn implode(data: &[u8], big: bool, literals: bool) -> (Vec<u8>, u16) {
    let mut lit_lens = vec![7u8; 96];
    lit_lens.extend([8u8; 32]);
    lit_lens.extend([10u8; 128]);
    let mut lens = vec![5u8; 16];
    lens.extend([6u8; 16]);
    lens.extend([7u8; 32]);
    let mut out = Vec::new();
    let mut tree = |l: &[u8]| {
        let mut groups: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < l.len() {
            let mut n = 1;
            while i + n < l.len() && l[i + n] == l[i] && n < 16 {
                n += 1;
            }
            groups.push(((n - 1) << 4) as u8 | (l[i] - 1));
            i += n;
        }
        out.push(groups.len() as u8 - 1);
        out.extend(groups);
    };
    if literals {
        tree(&lit_lens);
    }
    tree(&lens);
    tree(&lens);
    let (lit_codes, codes) = (canonical(&lit_lens), canonical(&lens));
    let mut w = BitWriter::default();
    let put_code = |w: &mut BitWriter, code: u32, len: u8| {
        let inv = !code & ((1 << len) - 1);
        for i in (0..len).rev() {
            w.put((inv >> i) & 1, 1);
        }
    };
    let low_bits = if big { 7 } else { 6 };
    let min = if literals { 3 } else { 2 };
    let window = if big { 8192 } else { 4096 };
    let matches = lz_matches(data, window, 3, min + 63 + 255);
    let mut mi = 0;
    let mut pos = 0;
    while pos < data.len() {
        if mi < matches.len() && matches[mi].0 == pos {
            let (_, len, dist) = matches[mi];
            mi += 1;
            w.put(0, 1);
            let d = (dist - 1) as u32;
            w.put(d & ((1 << low_bits) - 1), low_bits);
            let high = (d >> low_bits) as usize;
            put_code(&mut w, codes[high], lens[high]);
            let l = len - min;
            let c = l.min(63);
            put_code(&mut w, codes[c], lens[c]);
            if c == 63 {
                w.put((l - 63) as u32, 8);
            }
            pos += len;
        } else {
            w.put(1, 1);
            let c = data[pos] as usize;
            if literals {
                put_code(&mut w, lit_codes[c], lit_lens[c]);
            } else {
                w.put(c as u32, 8);
            }
            pos += 1;
        }
    }
    out.extend(w.finish());
    (out, (u16::from(big) * 2) | (u16::from(literals) * 4))
}

/// Content for the method tests: text, an executable, runs and noise.
fn pkzip_samples() -> Vec<(&'static str, Vec<u8>)> {
    let words = ["alpha", "beta", "gamma", "delta", "zip", "shrink", "reduce", "implode", "\n", "\u{90}"];
    let mut x = 777u32;
    let mut text = Vec::new();
    while text.len() < 150_000 {
        x = x.wrapping_mul(1103515245).wrapping_add(12345);
        text.extend_from_slice(words[(x >> 16) as usize % words.len()].as_bytes());
        text.push(b' ');
    }
    let mut runs = vec![0u8; 20_000];
    runs.extend(vec![144u8; 3000]);
    runs.extend(b"abcabcabcabc".repeat(500));
    let exe = tool("ls").map(|p| std::fs::read(p).unwrap()).unwrap_or_else(|| noise(50_000, 5));
    vec![("text.txt", text), ("runs.bin", runs), ("exe", exe), ("noise.bin", noise(40_000, 9)), ("empty", vec![])]
}

/// The methods no zip tool here writes, made by the encoders above:
/// Info-ZIP unzip and 7-Zip (which read Shrink and Implode) must accept
/// them, and the driver must read them back.
#[test]
fn reads_old_pkzip_methods() {
    let s = Scratch::new("pkzip");
    let samples = pkzip_samples();
    type Packer<'a> = &'a dyn Fn(&[u8]) -> (u16, u16, Vec<u8>);
    let mut cases: Vec<(String, Vec<Member<'_>>)> = Vec::new();
    let each = |f: Packer<'_>| -> Vec<Member<'_>> {
        samples
            .iter()
            .map(|(n, d)| {
                let (m, fl, p) = f(d);
                (*n, m, fl, p, d.clone())
            })
            .collect()
    };
    cases.push(("shrink.zip".into(), each(&|d| (1, 0, shrink(d, usize::MAX)))));
    cases.push(("shrink-clears.zip".into(), each(&|d| (1, 0, shrink(d, 3000)))));
    for f in 1..=4u32 {
        cases.push((format!("reduce{f}.zip"), each(&|d| (1 + f as u16, 0, reduce(d, f)))));
    }
    for (big, lit) in [(true, true), (false, true), (true, false), (false, false)] {
        cases.push((
            format!("implode-{}k{}.zip", if big { 8 } else { 4 }, if lit { "-lit" } else { "" }),
            each(&|d| {
                let (p, fl) = implode(d, big, lit);
                (6, fl, p)
            }),
        ));
    }
    if have(&["zstd", "xz"]) {
        let pipe = |cmd: &str, args: &[&str], d: &[u8]| {
            use std::io::Write;
            let mut c = Command::new(tool(cmd).unwrap())
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = c.stdin.take().unwrap();
            let d = d.to_vec();
            let t = std::thread::spawn(move || stdin.write_all(&d).unwrap());
            let out = c.wait_with_output().unwrap().stdout;
            t.join().unwrap();
            out
        };
        cases.push(("zstd.zip".into(), each(&|d| (93, 0, pipe("zstd", &["-q", "-c"], d)))));
        cases.push(("xz.zip".into(), each(&|d| (95, 0, pipe("xz", &["-c"], d)))));
    }
    for (name, members) in &cases {
        let img = s.0.join(name);
        std::fs::write(&img, zip_of(members)).unwrap();
        let method = members[0].1;
        // UnZip's partial clear only looks at codes below its free-code
        // pointer (enough for PKZIP, which clears when the table is full):
        // the extra clears are for 7-Zip, which clears every leaf.
        if matches!(method, 1 | 6) && name != "shrink-clears.zip" && have(&["unzip"]) {
            run(&s.0, "unzip", &["-tq", name]);
        }
        if matches!(method, 1 | 6) && have(&["7z"]) {
            run(&s.0, "7z", &["t", "-bso0", "-bsp0", name]);
        }
        let mut m = open(&img, AccessMode::ReadOnly).unwrap();
        for (n, _, _, _, content) in members {
            let got = m.fs.read_to_vec(&p(&format!("/{n}"))).unwrap_or_else(|e| panic!("{name}: {n}: {e}"));
            assert!(got == *content, "{name}: {n} differs");
        }
        // Read pieces too (the member is unpacked once, then kept).
        let mut buf = [0u8; 100];
        let text = &members[0].4;
        assert_eq!(m.fs.read(&p("/text.txt"), 1000, &mut buf).unwrap(), 100);
        assert_eq!(&buf[..], &text[1000..1100]);
    }
    // An archive with these methods can be changed: they are rewritten
    // with deflate.
    let img = s.0.join("implode-8k-lit.zip");
    let mut m = open(&img, AccessMode::ReadWrite).unwrap();
    m.fs.write_file(&p("/new.txt"), b"new").unwrap();
    m.fs.sync().unwrap();
    drop(m);
    if have(&["unzip"]) {
        run(&s.0, "unzip", &["-tq", "implode-8k-lit.zip"]);
    }
    let got = fs_tree(&img);
    for (n, d) in &samples {
        assert!(got[&format!("/{n}")] == Item::File(d.clone()), "{n}");
    }
}

/// Deflate64, bzip2 and LZMA members made by 7-Zip; PPMd says it cannot
/// be read.
#[test]
fn reads_zip_methods_made_by_7zip() {
    if !have(&["7z"]) {
        return;
    }
    let s = Scratch::new("zip-7z");
    let src = s.0.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for (n, d) in pkzip_samples() {
        std::fs::write(src.join(n), d).unwrap();
    }
    let want = host_tree(&src);
    for m in ["Deflate64", "BZip2", "LZMA", "LZMA:eos", "PPMd"] {
        let name = format!("{}.zip", m.replace(':', "-"));
        let mut args = vec!["a".to_owned(), "-tzip".into(), "-bso0".into(), "-bsp0".into()];
        match m.split_once(':') {
            Some((m, "eos")) => {
                args.push(format!("-mm={m}"));
                args.push("-meos=on".into());
            }
            _ => args.push(format!("-mm={m}")),
        }
        args.push(format!("../{name}"));
        args.push(".".into());
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        run(&src, "7z", &a);
        if m == "PPMd" {
            let mut mnt = open(&s.0.join(&name), AccessMode::ReadOnly).unwrap();
            let e = mnt.fs.read_to_vec(&p("/text.txt")).unwrap_err();
            assert!(e.to_string().contains("PPMd"), "{e}");
            continue;
        }
        let got = fs_tree(&s.0.join(&name));
        assert_eq!(got.len(), want.len(), "{name}");
        for (k, v) in &want {
            assert!(got[k] == *v, "{name}: {k} differs");
        }
    }
}

// ----- zip passwords ----------------------------------------------------------------

/// Encrypted zips: ZipCrypto from zip and 7-Zip, WinZip AES-128/192/256
/// from 7-Zip (with deflate, stored and bzip2 inside), two passwords in one
/// archive, wrong passwords, and a rewrite that keeps the encryption.
#[test]
fn reads_encrypted_zips() {
    if !have(&["zip", "unzip", "7z"]) {
        return;
    }
    let s = Scratch::new("zip-pass");
    let src = s.0.join("src");
    make_source(&src);
    let want = host_tree(&src);
    run(&src, "zip", &["-q", "-r", "-y", "-P", "s3cret", "../zc.zip", "."]);
    let mut cases = vec![("zc.zip".to_owned(), "s3cret")];
    for (em, mm) in [
        ("ZipCrypto", "Deflate"),
        ("AES128", "Deflate"),
        ("AES192", "Copy"),
        ("AES256", "Deflate"),
        ("AES256", "BZip2"),
    ] {
        let name = format!("7z-{em}-{mm}.zip");
        let out = format!("../{name}");
        let (em_arg, mm_arg) = (format!("-mem={em}"), format!("-mm={mm}"));
        run(&src, "7z", &["a", "-tzip", "-snl", "-bso0", "-bsp0", "-pbudgie", &em_arg, &mm_arg, &out, "."]);
        cases.push((name, "budgie"));
    }
    for (name, password) in &cases {
        let img = s.0.join(name);
        let mut m = open(&img, AccessMode::ReadOnly).unwrap();
        assert!(m.fs.uses_passwords(), "{name}");
        assert!(m.fs.take_password_request().is_some(), "{name}");
        assert!(m.fs.take_password_request().is_none(), "{name}: asked once");
        // Listed, but not readable yet; that asks for a password again.
        assert_eq!(m.fs.metadata(&p("/hello.txt")).unwrap().size, 14);
        let e = m.fs.read_to_vec(&p("/docs/readme.md")).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::PasswordRequired, "{name}: {e}");
        assert!(m.fs.take_password_request().is_some(), "{name}");
        // A wrong password is refused, the right one kept.
        let e = m.fs.add_password("wrong").unwrap_err();
        assert_eq!(e.kind(), ErrorKind::PasswordRequired, "{name}");
        m.fs.add_password(password).unwrap();
        let mut got = BTreeMap::new();
        walk_fs(m.fs.as_mut(), "/", &mut got);
        assert_eq!(got.len(), want.len(), "{name}");
        for (k, v) in &want {
            assert!(got[k] == *v, "{name}: {k} differs");
        }
        let mut buf = [0u8; 1000];
        let n = m.fs.read(&p("/bin/noise.bin"), 2_000_000, &mut buf).unwrap();
        assert_eq!(&buf[..n], &noise(3 << 20, 7)[2_000_000..2_001_000], "{name}");
    }
    // Two passwords in one archive: each opens its own files.
    run(&src, "zip", &["-q", "-P", "first", "../two.zip", "hello.txt"]);
    run(&src, "zip", &["-q", "-r", "-P", "second", "../two.zip", "docs"]);
    let mut m = open(&s.0.join("two.zip"), AccessMode::ReadOnly).unwrap();
    m.fs.add_password("second").unwrap();
    assert!(m.fs.read_to_vec(&p("/docs/readme.md")).is_ok());
    assert_eq!(m.fs.read_to_vec(&p("/hello.txt")).unwrap_err().kind(), ErrorKind::PasswordRequired);
    m.fs.add_password("first").unwrap();
    assert_eq!(m.fs.read_to_vec(&p("/hello.txt")).unwrap(), b"hello archive\n");
    drop(m);
    // Changing an encrypted archive keeps what was encrypted encrypted
    // (copied as it is); new files are not encrypted.
    for name in ["zc.zip", "7z-AES256-Deflate.zip"] {
        let img = s.0.join(name);
        let mut m = open(&img, AccessMode::ReadWrite).unwrap();
        m.fs.write_file(&p("/added.txt"), b"plain").unwrap();
        m.fs.remove_file(&p("/hello.txt")).unwrap();
        m.fs.sync().unwrap();
        drop(m);
        let pw = if name == "zc.zip" { "s3cret" } else { "budgie" };
        // (UnZip 6 does not read WinZip AES.)
        if name == "zc.zip" {
            run(&s.0, "unzip", &["-tq", "-P", pw, name]);
        }
        run(&s.0, "7z", &["t", "-bso0", "-bsp0", &format!("-p{pw}"), name]);
        let mut m = open(&img, AccessMode::ReadOnly).unwrap();
        assert_eq!(m.fs.read_to_vec(&p("/added.txt")).unwrap(), b"plain");
        assert_eq!(m.fs.read_to_vec(&p("/docs/readme.md")).unwrap_err().kind(), ErrorKind::PasswordRequired);
        m.fs.add_password(pw).unwrap();
        assert!(Item::File(m.fs.read_to_vec(&p("/docs/readme.md")).unwrap()) == want["/docs/readme.md"]);
        assert!(m.fs.metadata(&p("/hello.txt")).is_err());
    }
    // Not encrypted: nothing to ask.
    run(&src, "zip", &["-q", "../plain.zip", "hello.txt"]);
    let mut m = open(&s.0.join("plain.zip"), AccessMode::ReadOnly).unwrap();
    assert!(!m.fs.uses_passwords());
    assert!(m.fs.take_password_request().is_none());
}

/// 7z and RAR 5 archives with a password: encrypted files (solid and not,
/// stored and compressed) and encrypted lists of files, also over volumes.
#[test]
fn reads_encrypted_7z_and_rar() {
    if !have(&["7z", "rar", "unrar"]) {
        return;
    }
    let s = Scratch::new("7z-rar-pass");
    let src = s.0.join("src");
    rar_source(&src);
    let want = host_tree(&src);
    let seven = |name: &str, opts: &[&str]| {
        let mut args = vec!["a", "-t7z", "-snl", "-bso0", "-bsp0", "-pfalcon"];
        args.extend_from_slice(opts);
        let out = format!("../{name}");
        args.extend_from_slice(&[&out, "."]);
        run(&src, "7z", &args);
        s.0.join(name)
    };
    let rar = |name: &str, opts: &[&str]| {
        let mut args = vec!["a", "-idq", "-r", "-ol", "-oh"];
        args.extend_from_slice(opts);
        let out = format!("../{name}");
        args.extend_from_slice(&[&out, "."]);
        run(&src, "rar", &args);
        let parts: Vec<PathBuf> = parts_of(&s.0, &format!("{}.", name.trim_end_matches(".rar")))
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "rar"))
            .collect();
        let first = parts[0].to_string_lossy().into_owned();
        run(&s.0, "unrar", &["t", "-idq", "-pfalcon", &first]);
        // (Volumes: opened from the last part.)
        parts.into_iter().max().unwrap()
    };
    // (path, the list of files is encrypted too)
    let cases = [
        (seven("solid.7z", &[]), false),
        (seven("not-solid.7z", &["-ms=off"]), false),
        (seven("hidden.7z", &["-mhe=on"]), true),
        (rar("normal.rar", &["-pfalcon"]), false),
        (rar("solid.rar", &["-pfalcon", "-s", "-m5"]), false),
        (rar("stored.rar", &["-pfalcon", "-m0"]), false),
        (rar("hidden.rar", &["-hpfalcon"]), true),
        (rar("hidvol.rar", &["-hpfalcon", "-m3", "-v700k"]), true),
    ];
    for (img, hidden) in &cases {
        let name = img.file_name().unwrap().to_string_lossy().into_owned();
        let mut m = open(img, AccessMode::ReadOnly).unwrap();
        assert!(m.fs.uses_passwords(), "{name}");
        assert!(m.fs.take_password_request().is_some(), "{name}");
        if *hidden {
            assert!(m.fs.read_dir(&p("/")).unwrap().is_empty(), "{name}: listed without the password");
        } else {
            assert_eq!(m.fs.metadata(&p("/hello.txt")).unwrap().size, 14, "{name}");
            let e = m.fs.read_to_vec(&p("/docs/readme.md")).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::PasswordRequired, "{name}: {e}");
            assert!(m.fs.take_password_request().is_some(), "{name}");
        }
        let e = m.fs.add_password("parrot").unwrap_err();
        assert_eq!(e.kind(), ErrorKind::PasswordRequired, "{name}: {e}");
        m.fs.add_password("falcon").unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(m.fs.take_password_request().is_none(), "{name}");
        let mut got = BTreeMap::new();
        walk_fs(m.fs.as_mut(), "/", &mut got);
        let missing: Vec<_> = want.keys().filter(|k| !got.contains_key(*k)).collect();
        let extra: Vec<_> = got.keys().filter(|k| !want.contains_key(*k)).collect();
        assert!(missing.is_empty() && extra.is_empty(), "{name}: missing {missing:?}, extra {extra:?}");
        for (k, v) in &want {
            assert!(got[k] == *v, "{name}: {k} differs");
        }
        // Pieces of a big file, out of order.
        let big = noise(3 << 20, 7);
        for (off, len) in [(2_500_000usize, 70_000usize), (17, 33), (1 << 20, 1)] {
            let mut buf = vec![0u8; len];
            let n = m.fs.read(&p("/bin/noise.bin"), off as u64, &mut buf).unwrap();
            assert_eq!(&buf[..n], &big[off..off + len], "{name} at {off}");
        }
    }
}

/// RAR 4 with a password, made here (rar 7 no longer writes RAR 4): files
/// encrypted (AES-128, the RAR 3 key), and encrypted headers (`-hp`),
/// with a short password and one long enough to meet RAR 3's SHA-1 quirk.
/// unrar must accept each, and the driver read them back.
#[test]
fn rar4_with_a_password() {
    use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
    fn header(kind: u8, flags: u16, body: &[u8]) -> Vec<u8> {
        let mut h = vec![0, 0, kind];
        h.extend_from_slice(&flags.to_le_bytes());
        h.extend_from_slice(&((7 + body.len()) as u16).to_le_bytes());
        h.extend_from_slice(body);
        let crc = (crc(&h[2..]) & 0xFFFF) as u16;
        h[..2].copy_from_slice(&crc.to_le_bytes());
        h
    }
    /// AES-128-CBC with the RAR 3 key of `pw` and `salt` (zero padded).
    fn encrypt(pw: &str, salt: &[u8; 8], data: &[u8]) -> Vec<u8> {
        let (key, mut iv) = super::crypto::keys_rar3(pw, salt);
        let cipher = aes::Aes128::new(GenericArray::from_slice(&key));
        let mut out = data.to_vec();
        out.resize(data.len().next_multiple_of(16), 0);
        for block in out.chunks_exact_mut(16) {
            for (b, v) in block.iter_mut().zip(iv) {
                *b ^= v;
            }
            cipher.encrypt_block(GenericArray::from_mut_slice(block));
            iv = block.try_into().unwrap();
        }
        out
    }
    // A file header (and its data) encrypted with `pw` (method: 0x30 stored).
    let file = |name: &str, data: &[u8], method: u8, pw: &str, salt: [u8; 8]| -> (Vec<u8>, Vec<u8>) {
        let packed = encrypt(pw, &salt, data);
        let mut b = Vec::new();
        b.extend_from_slice(&(packed.len() as u32).to_le_bytes());
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.push(3); // Unix
        b.extend_from_slice(&crc(data).to_le_bytes());
        let (time, date) = dix_core::time::to_dos(1_700_000_000);
        b.extend_from_slice(&((date as u32) << 16 | time as u32).to_le_bytes());
        b.push(29);
        b.push(method);
        b.extend_from_slice(&(name.len() as u16).to_le_bytes());
        b.extend_from_slice(&0o100644u32.to_le_bytes());
        b.extend_from_slice(name.as_bytes());
        b.extend_from_slice(&salt);
        (header(0x74, 0x8000 | 0x04 | 0x400, &b), packed)
    };
    let big = noise(70_000, 4);
    let s = Scratch::new("rar4-pass");
    let long = "a much longer password, over 29 characters long";
    for (name, pw, hp) in [
        ("files.rar", "secret", false),
        ("long.rar", long, false),
        ("hidden.rar", "secret", true),
        ("hidden-long.rar", long, true),
    ] {
        let mut a = b"Rar!\x1a\x07\x00".to_vec();
        a.extend(header(0x73, if hp { 0x80 } else { 0 }, &[0; 6]));
        let members = [
            file("hello.txt", b"hello rar4\n", 0x30, pw, [1, 2, 3, 4, 5, 6, 7, 8]),
            file("docs\\big.bin", &big, 0x30, pw, [9; 8]),
        ];
        let end = header(0x7B, 0x4000, &[]);
        // With -hp, every header after the main one: a salt, then AES.
        let mut put = |h: &[u8], data: &[u8], salt: u8| {
            if hp {
                let hs = [salt; 8];
                a.extend_from_slice(&hs);
                a.extend(encrypt(pw, &hs, h));
            } else {
                a.extend_from_slice(h);
            }
            a.extend_from_slice(data);
        };
        for (i, (h, d)) in members.iter().enumerate() {
            put(h, d, 0x40 + i as u8);
        }
        put(&end, &[], 0x50);
        let img = s.0.join(name);
        std::fs::write(&img, &a).unwrap();
        if have(&["unrar"]) {
            run(&s.0, "unrar", &["t", "-idq", &format!("-p{pw}"), name]);
        }
        let mut m = open(&img, AccessMode::ReadOnly).unwrap();
        assert!(m.fs.uses_passwords(), "{name}");
        if hp {
            assert!(m.fs.read_dir(&p("/")).unwrap().is_empty(), "{name}");
        } else {
            let e = m.fs.read_to_vec(&p("/hello.txt")).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::PasswordRequired, "{name}: {e}");
        }
        let e = m.fs.add_password("wrong").unwrap_err();
        assert_eq!(e.kind(), ErrorKind::PasswordRequired, "{name}: {e}");
        m.fs.add_password(pw).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(m.fs.read_to_vec(&p("/hello.txt")).unwrap(), b"hello rar4\n", "{name}");
        assert_eq!(m.fs.read_to_vec(&p("/docs/big.bin")).unwrap(), big, "{name}");
        let mut buf = vec![0u8; 5000];
        assert_eq!(m.fs.read(&p("/docs/big.bin"), 33_333, &mut buf).unwrap(), 5000);
        assert_eq!(buf, &big[33_333..38_333], "{name}");
    }
}

/// PPMd (variant H) as 7-Zip writes it: orders from 2 to 32, and memory
/// small enough that the model fills up and restarts (the allocator must
/// behave exactly as 7-Zip's for that to read back).
#[test]
fn reads_7z_ppmd() {
    if !have(&["7z"]) {
        return;
    }
    let s = Scratch::new("7z-ppmd");
    let src = s.0.join("src");
    // (Text mostly: random bytes are PPMd's slowest case.)
    std::fs::create_dir_all(src.join("bin")).unwrap();
    std::fs::write(src.join("hello.txt"), b"hello ppmd\n").unwrap();
    std::fs::write(src.join("bin/noise.bin"), noise(40_000, 3)).unwrap();
    let words = ["alpha ", "beta ", "gamma ", "delta ", "archive ", "model ", "the ", "\n", "PPMd "];
    let mut x = 99u32;
    let mut text = Vec::new();
    while text.len() < 500_000 {
        x = x.wrapping_mul(1103515245).wrapping_add(12345);
        text.extend_from_slice(words[(x >> 16) as usize % words.len()].as_bytes());
        if x.is_multiple_of(251) {
            text.extend_from_slice(&(x as u64 * 7919).to_string().into_bytes());
        }
    }
    std::fs::write(src.join("words.txt"), &text).unwrap();
    if let Some(ls) = tool("ls") {
        std::fs::copy(ls, src.join("bin/ls")).unwrap();
    }
    let want = host_tree(&src);
    for (name, method) in [
        ("o6.7z", "PPMd"),
        ("o2.7z", "PPMd:o=2:mem=16m"),
        ("o32-small.7z", "PPMd:o=32:mem=1m"),
        ("o8-solid-off.7z", "PPMd:o=8:mem=4m"),
    ] {
        let m = format!("-m0={method}");
        let mut args = vec!["a", "-t7z", "-snl", "-bso0", "-bsp0", &m];
        if name.contains("solid-off") {
            args.push("-ms=off");
        }
        let out = format!("../{name}");
        args.extend_from_slice(&[&out, "."]);
        run(&src, "7z", &args);
        let got = fs_tree(&s.0.join(name));
        assert_eq!(got.len(), want.len(), "{name}");
        for (k, v) in &want {
            assert!(got[k] == *v, "{name}: {k} differs");
        }
    }
}

#[cfg(feature = "archive-rar")]
#[path = "rar4_tests.rs"]
mod rar4;

/// 7z written here: LZMA2 in one solid block, really compressed, and a
/// higher level (the volume's setting) gives a smaller archive.
#[test]
fn writes_7z() {
    if !have(&["7z"]) {
        return;
    }
    let s = Scratch::new("7z-write");
    let img = s.0.join("new.7z");
    {
        let mut dev = FileDevice::create(&img, 0).unwrap();
        registry().format(&mut dev, "archive", &FormatOptions { label: None, variant: Some("7z".into()) }).unwrap();
    }
    let words = ["alpha ", "beta ", "gamma ", "delta ", "archive ", "\n"];
    let mut x = 3u32;
    let mut text = Vec::new();
    while text.len() < 2 << 20 {
        x = x.wrapping_mul(1103515245).wrapping_add(12345);
        text.extend_from_slice(words[(x >> 16) as usize % words.len()].as_bytes());
    }
    let mut sizes = Vec::new();
    for level in ["3", "9"] {
        {
            let mut m = open(&img, AccessMode::ReadWrite).unwrap();
            if level == "3" {
                m.fs.create_dir(&p("/docs")).unwrap();
                m.fs.write_file(&p("/docs/words.txt"), &text).unwrap();
                m.fs.write_file(&p("/noise.bin"), &noise(200_000, 1)).unwrap();
                m.fs.write_file(&p("/empty"), b"").unwrap();
                m.fs.create_dir(&p("/empty-folder")).unwrap();
            } else {
                let set = m.fs.settings();
                assert_eq!(set[0].key, "level");
                assert_eq!(set[0].value, "3");
                m.fs.apply_settings(&[("level".into(), "9".into())]).unwrap();
                assert!(m.fs.apply_settings(&[("level".into(), "10".into())]).is_err());
            }
            m.fs.unmount().unwrap();
        }
        sizes.push(std::fs::metadata(&img).unwrap().len());
        let path = img.to_str().unwrap();
        run(&s.0, "7z", &["t", "-bso0", "-bsp0", path]);
        let info = String::from_utf8_lossy(&run(&s.0, "7z", &["l", "-slt", path])).into_owned();
        assert!(info.contains("Method = LZMA2"), "{info}");
        assert!(info.contains("Solid = +"), "{info}");
        let out = s.0.join(format!("x{level}"));
        std::fs::create_dir_all(&out).unwrap();
        run(&out, "7z", &["x", "-bso0", "-bsp0", path]);
        assert!(std::fs::read(out.join("docs/words.txt")).unwrap() == text, "level {level}");
        assert!(std::fs::read(out.join("empty")).unwrap().is_empty());
        assert!(out.join("empty-folder").is_dir());
    }
    // 2 MB of words and 200 KB of noise.
    assert!(sizes[0] < 900_000, "{sizes:?}");
    assert!(sizes[1] < sizes[0], "{sizes:?}");
}

/// Whether 7-Zip opens `archive` with `password` (testing every file).
fn seven_ok(dir: &Path, archive: &Path, password: &str) -> bool {
    Command::new(tool("7z").unwrap())
        .args(["t", "-bso0", "-bsp0", "-bse0", &format!("-p{password}"), archive.to_str().unwrap()])
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success()
}

/// Whether 7-Zip can list `archive` with `password`.
fn seven_lists(dir: &Path, archive: &Path, password: &str) -> bool {
    let out = Command::new(tool("7z").unwrap())
        .args(["l", "-slt", "-bse0", &format!("-p{password}"), archive.to_str().unwrap()])
        .current_dir(dir)
        .output()
        .unwrap();
    out.status.success() && String::from_utf8_lossy(&out.stdout).contains("Path = hello.txt")
}

/// 7z written with a password: AES as 7-Zip does it, the names too when
/// asked; 7-Zip's own encrypted archives stay encrypted when changed here.
#[test]
fn writes_encrypted_7z() {
    if !have(&["7z"]) {
        return;
    }
    let s = Scratch::new("7z-write-pass");
    let src = s.0.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("hello.txt"), b"hello 7z\n").unwrap();
    std::fs::write(src.join("data.bin"), noise(50_000, 4)).unwrap();
    // A new archive, then encrypted through its settings.
    for names in [false, true] {
        let img = s.0.join(format!("new-{names}.7z"));
        {
            let mut dev = FileDevice::create(&img, 0).unwrap();
            registry().format(&mut dev, "archive", &FormatOptions { label: None, variant: Some("7z".into()) }).unwrap();
        }
        {
            let mut m = open(&img, AccessMode::ReadWrite).unwrap();
            m.fs.write_file(&p("/hello.txt"), b"hello 7z\n").unwrap();
            m.fs.write_file(&p("/data.bin"), &noise(50_000, 4)).unwrap();
            let e = m.fs.apply_settings(&[("encrypt".into(), "true".into())]).unwrap_err();
            assert!(e.to_string().contains("password"), "{e}");
            m.fs.apply_settings(&[
                ("encrypt".into(), "true".into()),
                ("password".into(), "kestrel".into()),
                ("names".into(), names.to_string()),
            ])
            .unwrap();
            m.fs.unmount().unwrap();
        }
        assert!(seven_ok(&s.0, &img, "kestrel"), "names {names}");
        assert!(!seven_ok(&s.0, &img, "wrong"), "names {names}: opens without the password");
        assert_eq!(seven_lists(&s.0, &img, "wrong"), !names, "names {names}");
        let out = s.0.join(format!("x-{names}"));
        std::fs::create_dir_all(&out).unwrap();
        run(&out, "7z", &["x", "-bso0", "-bsp0", "-pkestrel", img.to_str().unwrap()]);
        assert_eq!(std::fs::read(out.join("data.bin")).unwrap(), noise(50_000, 4));
        // Read back here: the password is asked for.
        let mut m = open(&img, AccessMode::ReadOnly).unwrap();
        if names {
            assert!(m.fs.read_dir(&p("/")).unwrap().is_empty());
        } else {
            assert_eq!(m.fs.read_to_vec(&p("/hello.txt")).unwrap_err().kind(), ErrorKind::PasswordRequired);
        }
        m.fs.add_password("kestrel").unwrap();
        assert_eq!(m.fs.read_to_vec(&p("/hello.txt")).unwrap(), b"hello 7z\n");
    }
    // 7-Zip's encrypted archives, changed here: still encrypted.
    for (name, extra) in [("files.7z", None), ("hidden.7z", Some("-mhe=on"))] {
        let mut args = vec!["a", "-t7z", "-bso0", "-bsp0", "-pfalcon"];
        args.extend(extra);
        let out = format!("../{name}");
        args.extend_from_slice(&[&out, "."]);
        run(&src, "7z", &args);
        let img = s.0.join(name);
        {
            let mut m = open(&img, AccessMode::ReadWrite).unwrap();
            // Nothing is written before the password is known.
            let e = m.fs.write_file(&p("/added.txt"), b"added").and_then(|_| m.fs.sync()).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::PasswordRequired, "{name}: {e}");
            m.fs.add_password("falcon").unwrap();
            m.fs.write_file(&p("/added.txt"), b"added").unwrap();
            m.fs.remove_file(&p("/data.bin")).unwrap();
            m.fs.unmount().unwrap();
        }
        assert!(seven_ok(&s.0, &img, "falcon"), "{name}");
        assert!(!seven_ok(&s.0, &img, "wrong"), "{name}: no longer encrypted");
        assert_eq!(seven_lists(&s.0, &img, "wrong"), extra.is_none(), "{name}: names");
        let mut m = open(&img, AccessMode::ReadOnly).unwrap();
        m.fs.add_password("falcon").unwrap();
        assert_eq!(m.fs.read_to_vec(&p("/added.txt")).unwrap(), b"added");
        assert!(m.fs.metadata(&p("/data.bin")).is_err());
    }
    // And the encryption taken off again.
    let img = s.0.join("files.7z");
    {
        let mut m = open(&img, AccessMode::ReadWrite).unwrap();
        m.fs.add_password("falcon").unwrap();
        m.fs.apply_settings(&[("encrypt".into(), "false".into())]).unwrap();
        m.fs.unmount().unwrap();
    }
    assert!(seven_ok(&s.0, &img, "anything"));
}

/// tar compressed with lzma, lzip, lz4 (frames, linked 64 KiB blocks, the
/// legacy format) and compress (16- and 12-bit codes), as the tools make
/// them, and single files compressed with each (lzip in several members).
#[test]
fn more_compressions() {
    if !have(&["tar", "xz", "lzip", "lz4", "compress"]) {
        return;
    }
    let s = Scratch::new("morecomp");
    let src = s.0.join("src");
    make_source(&src);
    let want = host_tree(&src);
    run(&src, "tar", &["-cf", "../a.tar", "."]);
    let sh = |cmd: &str| {
        let out = Command::new("sh").arg("-c").arg(cmd).current_dir(&s.0).output().unwrap();
        assert!(out.status.success(), "{cmd}: {}", String::from_utf8_lossy(&out.stderr));
    };
    sh("xz --format=lzma -c a.tar > a.tar.lzma");
    sh("lzip -c a.tar > a.tar.lz");
    sh("lz4 -q -c a.tar > a.tar.lz4");
    sh("lz4 -q -BD -B4 -c a.tar > linked.tar.lz4");
    sh("lz4 -q -l -c a.tar > legacy.tar.lz4");
    sh("compress -c a.tar > a.tar.Z");
    sh("compress -b 12 -c a.tar > a12.tar.Z");
    for name in ["a.tar.lzma", "a.tar.lz", "a.tar.lz4", "linked.tar.lz4", "legacy.tar.lz4", "a.tar.Z", "a12.tar.Z"] {
        let got = fs_tree(&s.0.join(name));
        assert_eq!(got.len(), want.len(), "{name}");
        for (k, v) in &want {
            assert!(got.get(k) == Some(v), "{name}: {k} differs");
        }
    }
    // Single files.
    let data: Vec<u8> = (0..400_000u32).flat_map(|i| format!("line {} {}\n", i, i * 7 % 13).into_bytes()).collect();
    std::fs::write(s.0.join("notes.txt"), &data).unwrap();
    sh("xz --format=lzma -k -c notes.txt > notes.txt.lzma");
    sh("lzip -k -c --member-size=200KiB notes.txt > notes.txt.lz");
    sh("lz4 -q -c notes.txt > notes.txt.lz4");
    sh("compress -c notes.txt > notes.txt.Z");
    for name in ["notes.txt.lzma", "notes.txt.lz", "notes.txt.lz4", "notes.txt.Z"] {
        let tree = fs_tree(&s.0.join(name));
        assert_eq!(
            tree.get("/notes.txt"),
            Some(&Item::File(data.clone())),
            "{name}: {:?}",
            tree.keys().collect::<Vec<_>>()
        );
    }
    // A damaged lzip member is found by its checksum.
    let mut lz = std::fs::read(s.0.join("notes.txt.lz")).unwrap();
    let n = lz.len();
    lz[n - 30] ^= 0x40;
    assert!(Compression::Lzip.decompress(&lz).is_err());
}

/// Static libraries made by GNU ar (long names in the "//" table), a BSD
/// ar made by hand ("#1/len" names, a symbol table), Debian packages made
/// by dpkg-deb with each compression, RPM packages made by rpmbuild with
/// each payload compressor.
#[test]
fn reads_ar_deb_and_rpm() {
    if !have(&["ar", "dpkg-deb", "rpmbuild"]) {
        return;
    }
    let s = Scratch::new("pkgs");
    let src = s.0.join("src");
    make_source(&src);
    let sh = |dir: &Path, cmd: &str| {
        let out = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).output().unwrap();
        assert!(
            out.status.success(),
            "{cmd}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    // ar
    let lib = s.0.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    let mut members = BTreeMap::new();
    for (name, data) in [
        ("short.o", noise(1001, 1)),
        ("a-member-with-a-long-name.o", noise(5000, 2)),
        ("another-quite-long-member-name.txt", b"text\n".to_vec()),
        ("odd", b"odd".to_vec()),
    ] {
        std::fs::write(lib.join(name), &data).unwrap();
        members.insert(format!("/{name}"), Item::File(data));
    }
    sh(&lib, "ar rc ../lib.a short.o a-member-with-a-long-name.o another-quite-long-member-name.txt odd");
    assert_eq!(fs_tree(&s.0.join("lib.a")), members);
    // BSD: names before the data, and a symbol table left out.
    let mut bsd = b"!<arch>\n".to_vec();
    let mut add = |name: &str, data: &[u8], bsd_name: bool| {
        let (field, body): (String, Vec<u8>) = if bsd_name {
            (format!("#1/{}", name.len()), [name.as_bytes(), data].concat())
        } else {
            (name.to_owned(), data.to_vec())
        };
        bsd.extend(
            format!("{field:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", 1_700_000_000, 501, 20, 100644, body.len()).bytes(),
        );
        bsd.extend(&body);
        if body.len() % 2 == 1 {
            bsd.push(b'\n');
        }
    };
    add("__.SYMDEF SORTED", b"symbols", false);
    add("a-member-with-a-long-name.o", &noise(5000, 2), true);
    add("odd", b"odd", true);
    std::fs::write(s.0.join("bsd.a"), &bsd).unwrap();
    let got = fs_tree(&s.0.join("bsd.a"));
    assert_eq!(got.keys().collect::<Vec<_>>(), vec!["/a-member-with-a-long-name.o", "/odd"]);
    assert_eq!(got["/odd"], Item::File(b"odd".to_vec()));

    // deb: the package's files at the root, the control files in DEBIAN/.
    let pkg = s.0.join("pkg");
    std::fs::create_dir_all(pkg.join("DEBIAN")).unwrap();
    sh(&s.0, "cp -a src pkg/usr && chmod -R go-w pkg");
    std::fs::write(
        pkg.join("DEBIAN/control"),
        "Package: testpkg\nVersion: 1.2-3\nArchitecture: all\nMaintainer: Test <t@example.com>\nDescription: a test\n",
    )
    .unwrap();
    std::fs::write(pkg.join("DEBIAN/postinst"), "#!/bin/sh\nexit 0\n").unwrap();
    sh(&s.0, "chmod 755 pkg/DEBIAN/postinst");
    let want = host_tree(&pkg);
    for z in ["gzip", "xz", "zstd", "none"] {
        sh(&s.0, &format!("dpkg-deb --root-owner-group -Z{z} --build pkg {z}.deb >/dev/null"));
        let got = fs_tree(&s.0.join(format!("{z}.deb")));
        assert!(got == want, "deb {z}: {:?}", got.keys().filter(|k| !want.contains_key(*k)).collect::<Vec<_>>());
    }

    // rpm
    let top = s.0.join("rpm");
    std::fs::create_dir_all(top.join("SPECS")).unwrap();
    std::fs::write(
        top.join("SPECS/t.spec"),
        format!(
            "Name: testpkg\nVersion: 1.2\nRelease: 3\nSummary: t\nLicense: MIT\nBuildArch: noarch\n\
             %description\nt\n%install\nmkdir -p %{{buildroot}}/usr/share/testpkg\n\
             cp -a {}/. %{{buildroot}}/usr/share/testpkg/\n%files\n/usr/share/testpkg\n",
            src.display()
        ),
    )
    .unwrap();
    let mut want = BTreeMap::new();
    want.insert("/usr".to_owned(), Item::Dir);
    want.insert("/usr/share".to_owned(), Item::Dir);
    want.insert("/usr/share/testpkg".to_owned(), Item::Dir);
    for (k, v) in host_tree(&src) {
        want.insert(format!("/usr/share/testpkg{k}"), v);
    }
    for (payload, io) in [("gzip", "w9.gzdio"), ("xz", "w6.xzdio"), ("zstd", "w19.zstdio"), ("bzip2", "w9.bzdio")] {
        sh(
            &top,
            &format!(
                "rpmbuild --quiet --define '_topdir {}' --define '_binary_payload {io}' \
                 --define 'debug_package %{{nil}}' --define '__os_install_post %{{nil}}' -bb SPECS/t.spec",
                top.display()
            ),
        );
        let rpm = top.join("RPMS/noarch/testpkg-1.2-3.noarch.rpm");
        let mut m = open(&rpm, AccessMode::ReadOnly).unwrap();
        assert_eq!(m.fs.info().label.as_deref(), Some("testpkg-1.2-3"), "{payload}");
        let mut got = BTreeMap::new();
        walk_fs(m.fs.as_mut(), "/", &mut got);
        assert!(got == want, "rpm {payload}: {:?}", got.keys().filter(|k| !want.contains_key(*k)).collect::<Vec<_>>());
        std::fs::remove_file(rpm).unwrap();
    }
}

/// Data like a program's: text, noise, and x86 CALLs (E8 and an offset).
fn x86_like(n: usize, seed: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    let mut x = seed | 1;
    while out.len() < n {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        match x % 5 {
            0 => out.extend(b"mov eax, [ebp+8]; call print; "),
            1 => {
                out.push(0xE8);
                out.extend(((x >> 8) as i32 % 50_000).to_le_bytes());
            }
            2 => out.extend((0..(x % 40)).map(|i| (x >> (i % 24)) as u8)),
            _ => out.extend(b"\x55\x89\xE5\x83\xEC\x10"),
        }
    }
    out.truncate(n);
    out
}

/// Cabinets: stored and MSZIP ones made by gcab, LZX ones made by our test
/// encoder (checked by cabextract first) in several windows, block kinds
/// and with x86 CALL translation.
#[test]
fn reads_cabinets() {
    if !have(&["gcab", "cabextract"]) {
        return;
    }
    let s = Scratch::new("cab");
    let src = s.0.join("src");
    make_source(&src);
    let files =
        ["hello.txt", "docs/readme.md", "docs/deep/er/notes.txt", "bin/noise.bin", "empty.txt", "ünïcødé/файл.txt"];
    let want: BTreeMap<String, Item> =
        files.iter().map(|f| (format!("/{f}"), Item::File(std::fs::read(src.join(f)).unwrap()))).collect();
    let only_files = |t: BTreeMap<String, Item>| -> BTreeMap<String, Item> {
        t.into_iter().filter(|(_, v)| !matches!(v, Item::Dir)).collect()
    };
    for (name, zip) in [("store.cab", false), ("mszip.cab", true)] {
        let mut args = vec!["-c"];
        if zip {
            args.push("-z");
        }
        args.push(name);
        args.extend(files);
        let out = Command::new("gcab").args(&args).arg("--").current_dir(&src).output();
        let out = match out {
            Ok(o) if o.status.success() => o,
            _ => Command::new("gcab").args(&args).current_dir(&src).output().unwrap(),
        };
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let got = only_files(fs_tree(&src.join(name)));
        assert!(got == want, "{name}: {:?}", got.keys().collect::<Vec<_>>());
    }
    // LZX.
    let mut members: Vec<(&str, Vec<u8>)> = files.iter().map(|f| (*f, std::fs::read(src.join(f)).unwrap())).collect();
    members.push(("prog.exe", x86_like(300_000, 3)));
    // Matches at many distances (so not the repeated ones), all 8k + 3
    // bytes back: in aligned blocks one aligned symbol is most of them (a
    // short code, unlike three plain bits), and some distances (19, 27)
    // have exactly three extra bits.
    let mut near = noise(512, 5);
    for i in 0..40_000usize {
        let d = 8 * (2 + (i * 5) % 60) + 3;
        let from = near.len() - d;
        let copy: Vec<u8> = near[from..from + 6].to_vec();
        near.extend(copy);
        near.extend(noise(2, i as u32 + 11));
    }
    members.push(("near.bin", near));
    let want: BTreeMap<String, Item> = members.iter().map(|(n, d)| (format!("/{n}"), Item::File(d.clone()))).collect();
    use super::lzx_tests::{Opts, cabinet};
    let total: usize = members.iter().map(|m| m.1.len()).sum();
    for (i, o) in [
        Opts { window_bits: 15, translate: None, block: 100_000, uncompressed: false },
        Opts { window_bits: 16, translate: Some(total as u32), block: 70_000, uncompressed: true },
        Opts { window_bits: 21, translate: Some(12_000_000), block: 1 << 20, uncompressed: false },
        Opts { window_bits: 18, translate: None, block: 32_768, uncompressed: true },
    ]
    .into_iter()
    .enumerate()
    {
        let cab = s.0.join(format!("lzx{i}.cab"));
        std::fs::write(&cab, cabinet(&members, Some(o))).unwrap();
        // libmspack must agree that it is right before we read it.
        let out = Command::new("cabextract").args(["-t", "-q"]).arg(&cab).output().unwrap();
        assert!(out.status.success(), "cabextract on lzx{i}: {}", String::from_utf8_lossy(&out.stderr));
        let got = only_files(fs_tree(&cab));
        assert!(got == want, "lzx{i}: {:?}", got.keys().filter(|k| got[*k] != want[*k]).collect::<Vec<_>>());
    }
}

/// Self-extracting programs: 7-Zip's and RAR's own (Linux) modules, and 7z,
/// RAR, Cabinet and zip archives after a Windows program's code (a zip
/// also with its offsets made absolute by zip -A).
#[test]
fn reads_self_extracting_archives() {
    if !have(&["7z", "rar", "zip", "gcab"]) {
        return;
    }
    let s = Scratch::new("sfx");
    let src = s.0.join("src");
    make_source(&src);
    std::fs::remove_file(src.join("link-to-readme")).unwrap();
    let want = host_tree(&src);
    let sh = |cmd: &str| {
        let out = Command::new("sh").arg("-c").arg(cmd).current_dir(&s.0).env("LC_ALL", "C.UTF-8").output().unwrap();
        assert!(
            out.status.success(),
            "{cmd}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    // A Windows program: "MZ", code, and (by chance) the signatures' bytes.
    let mut stub = b"MZ".to_vec();
    stub.extend(noise(70_000, 9));
    stub.extend(b"7z\xBC\xAF\x27\x1C and Rar!\x1a\x07\x00 and MSCF\0\0\0\0 in the code");
    stub.extend(noise(10_000, 10));
    std::fs::write(s.0.join("stub"), &stub).unwrap();
    sh(
        "cd src && 7z a -bd -bso0 ../a.7z . && 7z a -bd -bso0 -sfx ../7z-sfx.exe . && rar a -r -idq ../a.rar . && zip -q -r -y ../a.zip .",
    );
    let has_rar_sfx = Command::new("sh")
        .arg("-c")
        .arg("cd src && rar a -r -idq -sfx ../rar-sfx.exe .")
        .current_dir(&s.0)
        .env("LC_ALL", "C.UTF-8")
        .status()
        .unwrap()
        .success();
    sh(
        "cat stub a.7z > win-7z.exe && cat stub a.rar > win-rar.exe && cat stub a.zip > win-zip.exe && cp win-zip.exe win-zipA.exe && zip -q -A win-zipA.exe",
    );
    let mut names = vec!["7z-sfx.exe", "win-7z.exe", "win-rar.exe", "win-zip.exe", "win-zipA.exe"];
    if has_rar_sfx {
        names.push("rar-sfx.exe");
    } else {
        eprintln!("rar has no SFX module here: its own self-extractor not tested");
    }
    for name in names {
        let got = fs_tree(&s.0.join(name));
        assert!(got == want, "{name}: {:?}", got.keys().filter(|k| !want.contains_key(*k)).collect::<Vec<_>>());
    }
    // A cabinet after the program.
    let files = ["hello.txt", "bin/noise.bin"];
    sh(&format!("cd src && gcab -c -z ../a.cab {}", files.join(" ")));
    sh("cat stub a.cab > win-cab.exe");
    let got = fs_tree(&s.0.join("win-cab.exe"));
    assert_eq!(got.get("/bin/noise.bin"), want.get("/bin/noise.bin"));
    assert_eq!(got.get("/hello.txt"), want.get("/hello.txt"));
    // A program with no archive is not one.
    assert!(open(&s.0.join("stub"), AccessMode::ReadOnly).map(|m| m.driver.info().id != "archive").unwrap_or(true));
}

/// A XAR archive of the folder `dir` (each file in a different encoding),
/// plus `more` files; with `links`, its symbolic links, a hard link
/// `extra-link` to its first file, and xz (7-Zip's reader takes neither).
fn xar_of(dir: &Path, more: &[(&str, Vec<u8>)], links: bool) -> Vec<u8> {
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }
    struct St {
        heap: Vec<u8>,
        id: usize,
        first: Option<usize>,
        links: bool,
    }
    fn data(st: &mut St, bytes: &[u8], n: usize) -> String {
        // (7-Zip's reader has no xz in XAR: its variant goes without.)
        let (style, packed) = match n % if st.links { 4 } else { 3 } {
            0 => ("application/octet-stream", bytes.to_vec()),
            1 => ("application/x-gzip", miniz_oxide::deflate::compress_to_vec_zlib(bytes, 6)),
            2 => ("application/x-bzip2", Compression::Bzip2.compress(bytes).unwrap()),
            _ => ("application/x-xz", Compression::Xz.compress(bytes).unwrap()),
        };
        let off = st.heap.len();
        st.heap.extend(&packed);
        format!(
            "<data><length>{}</length><offset>{off}</offset><size>{}</size><encoding style=\"{style}\"/></data>",
            packed.len(),
            bytes.len()
        )
    }
    fn walk(st: &mut St, dir: &Path, xml: &mut String) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            st.id += 1;
            let name = esc(&e.file_name().to_string_lossy());
            let common = format!(
                "<name>{name}</name><mode>0644</mode><uid>501</uid><gid>20</gid><mtime>2024-02-29T12:34:56Z</mtime>"
            );
            let t = e.file_type().unwrap();
            if t.is_symlink() && !st.links {
                continue;
            } else if t.is_symlink() {
                let target = std::fs::read_link(e.path()).unwrap();
                xml.push_str(&format!(
                    "<file id=\"{}\">{common}<type>symlink</type><link type=\"file\">{}</link></file>",
                    st.id,
                    esc(&target.to_string_lossy())
                ));
            } else if t.is_dir() {
                xml.push_str(&format!("<file id=\"{}\">{common}<type>directory</type>", st.id));
                walk(st, &e.path(), xml);
                xml.push_str("</file>");
            } else {
                let id = st.id;
                let d = data(st, &std::fs::read(e.path()).unwrap(), id);
                let kind = if st.first.is_none() && st.links {
                    st.first = Some(id);
                    "<type>hardlink</type><link>original</link>"
                } else {
                    "<type>file</type>"
                };
                xml.push_str(&format!("<file id=\"{id}\">{common}{kind}{d}</file>"));
            }
        }
    }
    let mut st = St { heap: Vec::new(), id: 0, first: None, links };
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<xar>\n <toc>\n");
    walk(&mut st, dir, &mut xml);
    for (name, bytes) in more {
        st.id += 1;
        let id = st.id;
        let d = data(&mut st, bytes, id);
        xml.push_str(&format!("<file id=\"{id}\"><name>{name}</name><type>file</type><mode>0644</mode>{d}</file>"));
    }
    if links {
        st.id += 1;
        xml.push_str(&format!(
            "<file id=\"{}\"><name>extra-link</name><type>hardlink</type><link>{}</link></file>",
            st.id,
            st.first.unwrap()
        ));
    }
    xml.push_str(" </toc>\n</xar>\n");
    let toc = miniz_oxide::deflate::compress_to_vec_zlib(xml.as_bytes(), 6);
    let mut out = b"xar!".to_vec();
    out.extend(28u16.to_be_bytes());
    out.extend(1u16.to_be_bytes());
    out.extend((toc.len() as u64).to_be_bytes());
    out.extend((xml.len() as u64).to_be_bytes());
    out.extend(0u32.to_be_bytes()); // no checksum
    out.extend(toc);
    out.extend(st.heap);
    out
}

/// XAR archives (checked by 7-Zip's reader first) and a macOS installer's
/// Payload in them: cpio, gzip-compressed or pbzx (xz chunks).
#[test]
fn reads_xar_and_payloads() {
    if !have(&["7z", "cpio", "xz"]) {
        return;
    }
    let s = Scratch::new("xar");
    let src = s.0.join("src");
    make_source(&src);
    let want = host_tree(&src);
    // The payload: cpio of the folder, as installers have it.
    let out = Command::new("sh")
        .arg("-c")
        .arg("find . | LC_ALL=C sort | cpio -o -H odc --quiet")
        .current_dir(&src)
        .output()
        .unwrap();
    assert!(out.status.success());
    let cpio = out.stdout;
    let mut pbzx = b"pbzx".to_vec();
    pbzx.extend((1u64 << 20).to_be_bytes());
    for chunk in cpio.chunks(1 << 20) {
        let xz = Compression::Xz.compress(chunk).unwrap();
        pbzx.extend((1u64 << 20).to_be_bytes());
        pbzx.extend((xz.len() as u64).to_be_bytes());
        pbzx.extend(xz);
    }
    let payloads = [("Payload", Compression::Gzip.compress(&cpio).unwrap()), ("Payload-pbzx", pbzx.clone())];
    let path = s.0.join("a.pkg");
    std::fs::write(&path, xar_of(&src, &payloads, true)).unwrap();
    // 7-Zip agrees with the test's writer (made without links for it).
    std::fs::write(s.0.join("no-links.pkg"), xar_of(&src, &payloads, false)).unwrap();
    let x = s.0.join("x");
    run(&s.0, "7z", &["x", "-bd", "-bso0", "-ox", "no-links.pkg"]);
    for (k, v) in &want {
        if let Item::File(d) = v {
            assert_eq!(std::fs::read(x.join(k.trim_start_matches('/'))).unwrap(), *d, "7z: {k}");
        }
    }
    // Ours: the folder, the payloads, the hard link.
    let mut got = fs_tree(&path);
    let first = want.iter().find(|(_, v)| matches!(v, Item::File(_))).map(|(_, v)| v);
    assert_eq!(got.remove("/extra-link").as_ref(), first);
    let payloads = [got.remove("/Payload").unwrap(), got.remove("/Payload-pbzx").unwrap()];
    assert!(got == want, "{:?}", got.keys().filter(|k| got.get(*k) != want.get(*k)).collect::<Vec<_>>());
    assert_eq!(payloads[1], Item::File(pbzx));
    // Each payload opens as the folder.
    for p in payloads {
        let Item::File(bytes) = p else { panic!() };
        let dev = dix_core::device::MemoryDevice::new("Payload", bytes);
        let mut m = registry().mount(Box::new(dev), None, AccessMode::ReadOnly).unwrap();
        let mut t = BTreeMap::new();
        walk_fs(m.fs.as_mut(), "/", &mut t);
        assert!(t == want, "payload: {:?}", t.keys().filter(|k| t.get(*k) != want.get(*k)).collect::<Vec<_>>());
    }
}
