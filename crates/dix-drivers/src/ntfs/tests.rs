//! NTFS volumes made by `mkntfs` and filled through `ntfs-3g` (another
//! implementation), read by the driver and compared with what was
//! written. Needs root, loop devices and FUSE: skipped where they are
//! missing.

use super::record::*;
use super::{NtfsDriver, Volume, check};
use dix_core::device::host::FileDevice;
use dix_core::{AccessMode, Driver, EntryKind, ErrorKind, FileSystem, MountLog, VfsPath};
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
        let d = std::env::temp_dir().join(format!("dix-ntfs-{name}-{}", std::process::id()));
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

/// An image mounted through ntfs-3g on a loop device (unmounted on drop).
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
        let (good, out) = run("ntfs-3g", &["-o", "streams_interface=windows,compression", &loopdev, dir.to_str()?]);
        if !good {
            eprintln!("ntfs-3g: {out}");
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

/// Whether NTFS can be mounted through ntfs-3g here (found out once).
fn have() -> bool {
    static HAVE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAVE.get_or_init(|| {
        if !["mkntfs", "ntfs-3g", "losetup", "setfattr"].iter().all(|t| tool(t).is_some()) {
            eprintln!("ntfs-3g / attr not installed: skipping NTFS test");
            return false;
        }
        let s = Scratch::new("probe");
        let img = mkntfs(&s.0, 8 << 20, None);
        let _lock = FUSE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let works = Fuse::mount(&img, &s.0.join("mnt")).is_some();
        if !works {
            eprintln!("cannot mount NTFS through ntfs-3g here: skipping NTFS test");
        }
        works
    })
}

fn mkntfs(dir: &Path, size: u64, cluster: Option<u32>) -> PathBuf {
    let img = dir.join("ntfs.img");
    std::fs::File::create(&img).unwrap().set_len(size).unwrap();
    let c = cluster.map(|c| c.to_string());
    let mut args = vec!["-F", "-f", "-q", "-L", "WINDISK"];
    if let Some(c) = &c {
        args.extend(["-c", c]);
    }
    args.push(img.to_str().unwrap());
    ok("mkntfs", &args);
    img
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

#[derive(Debug, PartialEq, Eq)]
enum Item {
    Dir,
    File(Vec<u8>),
    Link(String),
}

/// Fills a mounted volume (and records what it wrote): plain, empty,
/// Unicode-named and large files, a large folder, hard and symbolic
/// links, streams, a sparse file, compressed files and (with
/// `fragment`) a file in over a thousand pieces.
fn fill(m: &Path, fragment: bool) {
    let w = |rel: &str, data: &[u8]| {
        let p = m.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, data).unwrap();
    };
    w("hello.txt", b"hello NTFS\n");
    w("empty", b"");
    w("Ünïcödé – ✓ 日本語.txt", "unicode\n".as_bytes());
    w("deep/a/b/c/d/leaf.bin", &noise(5000, 1));
    w("big.bin", &noise(3 << 20, 2));
    for i in 0..600 {
        w(&format!("many/file number {i:04} with a long name.txt"), format!("{i}\n").as_bytes());
    }
    // Hard and symbolic links.
    std::fs::hard_link(m.join("hello.txt"), m.join("deep/hard.txt")).unwrap();
    std::os::unix::fs::symlink("hello.txt", m.join("to-hello")).unwrap();
    std::os::unix::fs::symlink("deep/a", m.join("to-dir")).unwrap();
    // A stream (ntfs-3g's streams_interface=windows).
    w("hello.txt:secret", b"a hidden stream\n");
    // Sparse: 16 MiB with a little data in the middle.
    let f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).open(m.join("sparse.bin")).unwrap();
    f.set_len(16 << 20).unwrap();
    use std::os::unix::fs::FileExt;
    f.write_all_at(b"in the middle", 7_000_000).unwrap();
    drop(f);
    // Compressed: files made in a folder with the compressed attribute.
    std::fs::create_dir(m.join("packed")).unwrap();
    ok("setfattr", &["-n", "system.ntfs_attrib_be", "-v", "0x00000800", m.join("packed").to_str().unwrap()]);
    let text: Vec<u8> = (0..400_000u32).map(|i| b"compress me, please! "[(i % 21) as usize]).collect();
    w("packed/text.txt", &text);
    w("packed/random.bin", &noise(150_000, 3));
    let mut mixed = noise(70_000, 4);
    mixed.extend(std::iter::repeat_n(b'z', 200_000));
    mixed.extend(noise(9_000, 5));
    w("packed/mixed.bin", &mixed);
    if fragment {
        // Data and holes taking turns: every piece is a run of its own, too
        // many for one record (the runlist continues in extension records).
        let f = std::fs::File::create(m.join("fragmented.bin")).unwrap();
        for i in 0..1200u64 {
            f.write_all_at(&noise(4096, i as u32), i * 12288).unwrap();
        }
    }
}

/// A reparse point buffer: a symbolic link (with `flags`) or a junction.
fn reparse_buf(tag: u32, sub: &str, print: &str, flags: Option<u32>) -> Vec<u8> {
    let u = |s: &str| s.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>();
    let (s, p) = (u(sub), u(print));
    let mut body = Vec::new();
    for v in [0, s.len(), s.len(), p.len()] {
        body.extend((v as u16).to_le_bytes());
    }
    if let Some(f) = flags {
        body.extend(f.to_le_bytes());
    }
    body.extend(s);
    body.extend(p);
    let mut out = tag.to_le_bytes().to_vec();
    out.extend((body.len() as u16).to_le_bytes());
    out.extend([0, 0]);
    out.extend(body);
    out
}

/// Windows' own links, written as raw reparse data through ntfs-3g:
/// a relative symbolic link, an absolute one and a junction.
fn add_reparse_points(m: &Path) {
    let d = m.join("reparse");
    std::fs::create_dir(&d).unwrap();
    let set = |name: &str, dir: bool, buf: Vec<u8>| {
        let p = d.join(name);
        if dir {
            std::fs::create_dir(&p).unwrap();
        } else {
            std::fs::write(&p, b"").unwrap();
        }
        let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
        ok("setfattr", &["-n", "system.ntfs_reparse_data", "-v", &format!("0x{hex}"), p.to_str().unwrap()]);
    };
    set("relative", false, reparse_buf(super::TAG_SYMLINK, "..\\hello.txt", "..\\hello.txt", Some(1)));
    set("absolute", false, reparse_buf(super::TAG_SYMLINK, "\\??\\C:\\Users", "C:\\Users", Some(0)));
    set("junction", true, reparse_buf(super::TAG_JUNCTION, "\\??\\C:\\Windows", "", None));
}

fn host_tree(dir: &Path, rel: &str, out: &mut BTreeMap<String, Item>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().into_string().unwrap();
        let p = format!("{rel}/{name}");
        let t = std::fs::symlink_metadata(e.path()).unwrap().file_type();
        if t.is_symlink() {
            out.insert(p, Item::Link(std::fs::read_link(e.path()).unwrap().to_str().unwrap().into()));
        } else if t.is_dir() {
            out.insert(p.clone(), Item::Dir);
            host_tree(&e.path(), &p, out);
        } else {
            out.insert(p, Item::File(std::fs::read(e.path()).unwrap()));
        }
    }
}

fn fs_tree(fs: &mut dyn FileSystem, path: &str, out: &mut BTreeMap<String, Item>) {
    let vp = VfsPath::parse(path).unwrap();
    for e in fs.read_dir(&vp).unwrap() {
        // System files and streams (name:stream) are not part of the tree.
        if e.name.starts_with('$') || e.name.contains(':') {
            continue;
        }
        let p = if path == "/" { format!("/{}", e.name) } else { format!("{path}/{}", e.name) };
        let item = match e.metadata.kind {
            EntryKind::Directory => {
                out.insert(p.clone(), Item::Dir);
                fs_tree(fs, &p, out);
                continue;
            }
            EntryKind::Symlink => Item::Link(e.metadata.symlink_target.clone().unwrap()),
            _ => {
                let data = fs.read_to_vec(&VfsPath::parse(&p).unwrap()).unwrap();
                assert_eq!(data.len() as u64, e.metadata.size, "{p}");
                Item::File(data)
            }
        };
        out.insert(p, item);
    }
}

fn open_retry(img: &Path) -> FileDevice {
    for _ in 0..100 {
        match FileDevice::open(img, AccessMode::ReadOnly) {
            Err(e) if e.kind() == ErrorKind::InUse => std::thread::sleep(std::time::Duration::from_millis(50)),
            r => return r.unwrap(),
        }
    }
    FileDevice::open(img, AccessMode::ReadOnly).unwrap()
}

fn check_image(img: &Path) -> (dix_core::CheckReport, Vec<String>) {
    let log = MountLog::new();
    let r = check::check(Box::new(open_retry(img)), &log, false).unwrap();
    (r, log.entries().into_iter().map(|e| e.message).collect())
}

/// Makes a volume, fills it through ntfs-3g and returns the image and
/// what it holds.
fn build(s: &Scratch, size: u64, cluster: Option<u32>, fragment: bool) -> Option<(PathBuf, BTreeMap<String, Item>)> {
    if !have() {
        return None;
    }
    let img = mkntfs(&s.0, size, cluster);
    let mut want = BTreeMap::new();
    {
        let _lock = FUSE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mnt = s.0.join("mnt");
        let fuse = Fuse::mount(&img, &mnt).expect("ntfs-3g mount");
        fill(&mnt, fragment);
        host_tree(&mnt, "", &mut want);
        if fragment {
            add_reparse_points(&mnt);
        }
        drop(fuse);
    }
    Some((img, want))
}

fn mount(img: &Path) -> dix_core::Mounted {
    let m = crate::default_registry().mount(Box::new(open_retry(img)), None, AccessMode::ReadOnly).unwrap();
    assert_eq!(m.fs.info().driver_id, "ntfs");
    m
}

fn reads_back(cluster: Option<u32>, fragment: bool) {
    let s = Scratch::new(&format!("read-{cluster:?}"));
    let Some((img, want)) = build(&s, 96 << 20, cluster, fragment) else { return };
    let mut m = mount(&img);
    let fs = m.fs.as_mut();
    assert_eq!(fs.info().label.as_deref(), Some("WINDISK"));
    assert!(!fs.capabilities().is_writable());
    let mut got = BTreeMap::new();
    fs_tree(fs, "/", &mut got);
    if fragment {
        let links: Vec<_> = got.iter().filter(|(k, _)| k.starts_with("/reparse/")).collect();
        let link = |t: &str| Item::Link(t.into());
        assert_eq!(
            links,
            [
                (&"/reparse/absolute".to_string(), &link("C:\\Users")),
                (&"/reparse/junction".to_string(), &link("C:\\Windows")),
                (&"/reparse/relative".to_string(), &link("../hello.txt")),
            ]
        );
        got.retain(|k, _| !k.starts_with("/reparse"));
    }
    assert_eq!(got.keys().collect::<Vec<_>>(), want.keys().collect::<Vec<_>>());
    for (k, v) in &want {
        assert!(got[k] == *v, "{k} differs");
    }
    // Streams, case-insensitive lookup, hard links, metadata.
    let p = |s: &str| VfsPath::parse(s).unwrap();
    assert_eq!(fs.read_to_vec(&p("/hello.txt:secret")).unwrap(), b"a hidden stream\n");
    let listed = fs.read_dir(&VfsPath::root()).unwrap();
    let stream = listed.iter().find(|e| e.name == "hello.txt:secret").expect("the stream is listed");
    assert!(stream.metadata.hidden);
    assert!(listed.iter().find(|e| e.name == "$MFT").unwrap().metadata.hidden);
    assert!(!listed.iter().find(|e| e.name == "to-hello").unwrap().metadata.hidden);
    assert_eq!(fs.read_to_vec(&p("/HELLO.TXT")).unwrap(), b"hello NTFS\n");
    let (a, b) = (fs.metadata(&p("/hello.txt")).unwrap(), fs.metadata(&p("/deep/hard.txt")).unwrap());
    assert_eq!(a.inode, b.inode);
    let host_mtime = a.modified.unwrap();
    assert!((host_mtime - dix_core::time::unix_now()).abs() < 3600, "modified time {host_mtime}");
    assert!(fs.read_dir(&p("/hello.txt")).is_err());
    assert!(fs.metadata(&p("/nope")).is_err());
    // Reads at odd offsets and sizes (across runs and compression units).
    let big = fs.read_to_vec(&p("/packed/mixed.bin")).unwrap();
    for (off, len) in [(0, 1), (4095, 2), (65530, 20), (69_999, 70_000), (278_000, 5000)] {
        let mut buf = vec![0u8; len];
        let n = fs.read(&p("/packed/mixed.bin"), off, &mut buf).unwrap();
        let end = (off as usize + len).min(big.len());
        assert_eq!(&buf[..n], &big[off as usize..end], "at {off}");
    }
    drop(m);

    // Make sure the interesting structures are really there.
    let mut v = Volume::open(Box::new(open_retry(&img))).unwrap();
    let cs = v.cs();
    let mut file = |path: &str| {
        let (n, _) = v.resolve(&p(path)).unwrap();
        v.file(n).unwrap()
    };
    let text = file("/packed/text.txt");
    let data = text.attr(AT_DATA, "").unwrap();
    if cs <= 4096 {
        assert!(data.flags & FLAG_COMPRESSED != 0, "text.txt is not compressed");
        let Body::NonResident { runs, .. } = &data.body else { panic!() };
        let stored: u64 = runs.iter().filter(|r| r.lcn.is_some()).map(|r| r.len).sum();
        assert!(stored * cs < 400_000 / 4, "text.txt takes {stored} clusters");
    }
    let sparse = file("/sparse.bin");
    let Body::NonResident { runs, .. } = &sparse.attr(AT_DATA, "").unwrap().body else { panic!() };
    assert!(runs.iter().any(|r| r.lcn.is_none()), "sparse.bin has no holes");
    let many = file("/many");
    assert!(many.attr(AT_INDEX_ALLOCATION, "$I30").is_some(), "the big folder fits its root");
    if fragment {
        let (n, _) = v.resolve(&p("/fragmented.bin")).unwrap();
        let rec = v.record(n).unwrap();
        let f = v.file(n).unwrap();
        let runs = match &f.attr(AT_DATA, "").unwrap().body {
            Body::NonResident { runs, .. } => runs.len(),
            _ => 0,
        };
        assert!(
            rec.attrs.iter().any(|a| a.ty == AT_ATTRIBUTE_LIST),
            "fragmented.bin has no attribute list ({runs} runs)"
        );
    }
    drop(v);

    let (r, log) = check_image(&img);
    assert_eq!(r.errors, 0, "{log:#?}");
}

#[test]
fn reads_what_ntfs3g_wrote() {
    reads_back(None, true);
}

#[test]
fn small_clusters() {
    reads_back(Some(512), false);
}

#[test]
fn large_clusters() {
    reads_back(Some(65536), false);
}

#[test]
fn probe_and_read_only() {
    let s = Scratch::new("probe-ro");
    if !have() {
        return;
    }
    let img = mkntfs(&s.0, 16 << 20, None);
    let mut dev = open_retry(&img);
    assert_eq!(NtfsDriver.probe(&mut dev).unwrap(), 95);
    // FAT must not claim it.
    #[cfg(feature = "fat")]
    assert_eq!(crate::fat::FatDriver.probe(&mut dev).unwrap(), 0);
    assert!(!NtfsDriver.supports_mode(AccessMode::ReadWrite));
    let log = MountLog::new();
    let opts = dix_core::MountOptions { mode: AccessMode::ReadWrite, log: log.clone() };
    let mut fs = NtfsDriver.mount(Box::new(dev), &opts).unwrap();
    assert_eq!(fs.access_mode(), AccessMode::ReadOnly);
    assert!(fs.write_file(&VfsPath::parse("/x").unwrap(), b"x").is_err());
    let (r, log) = check_image(&img);
    assert_eq!(r.errors, 0, "{log:#?}");
}

/// Writes `data` at `at` in the image.
fn poke(img: &Path, at: u64, data: &[u8]) {
    use std::os::unix::fs::FileExt;
    std::fs::OpenOptions::new().write(true).open(img).unwrap().write_all_at(data, at).unwrap();
}

fn peek(img: &Path, at: u64, n: usize) -> Vec<u8> {
    use std::os::unix::fs::FileExt;
    let mut b = vec![0u8; n];
    std::fs::File::open(img).unwrap().read_exact_at(&mut b, at).unwrap();
    b
}

#[test]
fn check_finds_damage() {
    let s = Scratch::new("damage");
    let Some((clean, _)) = build(&s, 48 << 20, None, false) else { return };
    // Where things are.
    let mut v = Volume::open(Box::new(open_retry(&clean))).unwrap();
    let cs = v.cs();
    let rs = v.boot.record_size as u64;
    assert!(v.mft[0].len * cs > 64 * rs);
    let mft_at = v.mft[0].lcn.unwrap() * cs;
    let (hello, _) = v.resolve(&VfsPath::parse("/hello.txt").unwrap()).unwrap();
    let (big, _) = v.resolve(&VfsPath::parse("/big.bin").unwrap()).unwrap();
    let big_lcn = match &v.file(big).unwrap().attr(AT_DATA, "").unwrap().body {
        Body::NonResident { runs, .. } => runs[0].lcn.unwrap(),
        _ => panic!(),
    };
    let bitmap_lcn = match &v.file(6).unwrap().attr(AT_DATA, "").unwrap().body {
        Body::NonResident { runs, .. } => runs[0].lcn.unwrap(),
        _ => panic!(),
    };
    let mirr_lcn = match &v.file(1).unwrap().attr(AT_DATA, "").unwrap().body {
        Body::NonResident { runs, .. } => runs[0].lcn.unwrap(),
        _ => panic!(),
    };
    let total_sectors = v.boot.total_sectors;
    drop(v);

    type Damage<'a> = Box<dyn Fn(&Path) + 'a>;
    let cases: Vec<(&str, Damage, &str)> = vec![
        ("torn record", Box::new(|img: &Path| poke(img, mft_at + hello * rs + 510, &[0xEE, 0xEE])), "damaged"),
        (
            "record marked free",
            Box::new(|img: &Path| {
                let at = mft_at + hello * rs + 22;
                let f = peek(img, at, 1)[0];
                poke(img, at, &[f & !1]);
            }),
            "not in use",
        ),
        (
            "cluster marked free",
            Box::new(|img: &Path| {
                let at = bitmap_lcn * cs + big_lcn / 8;
                let b = peek(img, at, 1)[0];
                poke(img, at, &[b & !(1 << (big_lcn % 8))]);
            }),
            "marked free",
        ),
        ("mirror differs", Box::new(|img: &Path| poke(img, mirr_lcn * cs + 2 * rs + 100, &[0x5A])), "$MFTMirr"),
        ("backup boot sector", Box::new(|img: &Path| poke(img, total_sectors * 512 + 3, b"XXXX")), "backup boot"),
    ];
    for (what, damage, expect) in cases {
        let img = s.0.join("damaged.img");
        std::fs::copy(&clean, &img).unwrap();
        damage(&img);
        let (r, log) = check_image(&img);
        assert!(r.errors > 0, "{what}: not found");
        assert!(log.iter().any(|l| l.contains(expect)), "{what}: {log:#?}");
        // The rest stays readable.
        let mut m = mount(&img);
        m.fs.read_dir(&VfsPath::root()).unwrap();
        assert!(m.fs.read_to_vec(&VfsPath::parse("/big.bin").unwrap()).is_ok());
    }
}

/// What the deleted-files view shows: path → (note, content).
fn recovered(img: &Path) -> BTreeMap<String, (Option<String>, Option<Vec<u8>>)> {
    let mut fs = crate::recover::deleted_files(Box::new(open_retry(img))).unwrap();
    let mut out = BTreeMap::new();
    let mut stack = vec!["/".to_owned()];
    while let Some(d) = stack.pop() {
        for e in fs.read_dir(&VfsPath::parse(&d).unwrap()).unwrap() {
            let p = format!("{}/{}", d.trim_end_matches('/'), e.name);
            if e.metadata.kind == EntryKind::Directory {
                stack.push(p.clone());
                out.insert(p, (e.metadata.note, None));
            } else {
                let data = fs.read_to_vec(&VfsPath::parse(&p).unwrap()).unwrap();
                out.insert(p, (e.metadata.note, Some(data)));
            }
        }
    }
    out
}

/// Files and folders deleted through ntfs-3g are found where they were,
/// whole: small (resident) and large ones, Unicode names, a deleted folder
/// with its contents; what was not deleted is not shown. ntfsundelete
/// finds the same names.
#[test]
fn finds_what_was_deleted() {
    if !have() {
        return;
    }
    let s = Scratch::new("undelete");
    let img = mkntfs(&s.0, 64 << 20, None);
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("small.txt", b"a small file, resident in its MFT record\n".to_vec()),
        ("big.bin", noise(3_000_017, 1)),
        ("Ünïcödé — 日本語.txt", "unicode".as_bytes().to_vec()),
        ("docs/report.pdf", noise(200_000, 2)),
        ("docs/sub/deep.txt", b"deep in a deleted folder".to_vec()),
        ("docs/sub/more.bin", noise(70_000, 3)),
    ];
    {
        let _lock = FUSE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mnt = s.0.join("mnt");
        let fuse = Fuse::mount(&img, &mnt).expect("ntfs-3g mount");
        std::fs::create_dir_all(mnt.join("docs/sub")).unwrap();
        for (p, d) in &files {
            std::fs::write(mnt.join(p), d).unwrap();
        }
        std::fs::write(mnt.join("keep.txt"), b"not deleted").unwrap();
        std::fs::write(mnt.join("docs/keep.bin"), noise(10_000, 9)).unwrap();
        ok("sync", &[]);
        for p in ["small.txt", "big.bin", "Ünïcödé — 日本語.txt", "docs/report.pdf"] {
            std::fs::remove_file(mnt.join(p)).unwrap();
        }
        std::fs::remove_dir_all(mnt.join("docs/sub")).unwrap();
        drop(fuse);
    }
    let got = recovered(&img);
    for (p, d) in &files {
        let key = format!("/{p}");
        let (note, data) = got.get(&key).unwrap_or_else(|| panic!("{key} not found in {:?}", got.keys()));
        assert_eq!(note.as_deref(), Some("deleted · intact"), "{key}");
        assert!(data.as_deref() == Some(&d[..]), "{key}: content differs");
    }
    assert!(!got.contains_key("/keep.txt") && !got.contains_key("/docs/keep.bin"), "live files are not shown");
    assert_eq!(got["/docs"].0, None, "a live folder leading to deleted files");
    assert_eq!(got["/docs/sub"].0.as_deref(), Some("deleted folder"));
    // ntfsundelete (another implementation) sees the same deleted files.
    if tool("ntfsundelete").is_some() {
        let (_, out) = run("ntfsundelete", &["--scan", "--force", img.to_str().unwrap()]);
        for name in ["small.txt", "big.bin", "report.pdf", "deep.txt", "more.bin"] {
            assert!(out.contains(name), "ntfsundelete does not list {name}: {out}");
        }
    }
}
