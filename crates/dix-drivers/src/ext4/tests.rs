//! Tests against real images made by e2fsprogs (`mkfs.ext4 -d`), with
//! `e2fsck` and `debugfs` as independent judges. Skipped when e2fsprogs is
//! not installed.

use super::*;
use dix_core::device::DetachableDevice;
use dix_core::device::host::{FileDevice, open_path};
use dix_core::{DriverRegistry, LogLevel, MountLog};
use std::path::{Path, PathBuf};
use std::process::Command;

fn tool(name: &str) -> Option<PathBuf> {
    ["/usr/sbin", "/sbin", "/usr/bin", "/bin"].iter().map(|d| Path::new(d).join(name)).find(|p| p.exists())
}

fn have_e2fsprogs() -> bool {
    let ok = tool("mke2fs").is_some() && tool("e2fsck").is_some() && tool("debugfs").is_some();
    if !ok {
        eprintln!("e2fsprogs not installed: skipping ext4 test");
    }
    ok
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("dix-ext4-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn big(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

/// An image of `kind` (ext2/ext3/ext4) populated from a small tree.
fn make_image(dir: &Path, kind: &str) -> PathBuf {
    let src = dir.join("src");
    std::fs::create_dir_all(src.join("docs/deep")).unwrap();
    std::fs::write(src.join("hello.txt"), b"hello from the host\n").unwrap();
    std::fs::write(src.join("docs/deep/big.bin"), big(3 * 1024 * 1024 + 123)).unwrap();
    for i in 0..300 {
        std::fs::write(src.join(format!("docs/f{i:03}")), format!("file {i}")).unwrap();
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink("hello.txt", src.join("link")).unwrap();
    let img = dir.join(format!("{kind}.img"));
    let st = Command::new(tool("mke2fs").unwrap())
        .args(["-q", "-F", "-t", kind, "-L", "TESTVOL", "-d"])
        .arg(&src)
        .arg(&img)
        .arg("32M")
        .status()
        .unwrap();
    assert!(st.success(), "mke2fs failed");
    img
}

/// `e2fsck -fn`: exit code 0 means no problems found.
fn fsck_clean(img: &Path) -> (bool, String) {
    let out = Command::new(tool("e2fsck").unwrap()).args(["-fn"]).arg(img).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
}

fn debugfs_cat(img: &Path, path: &str) -> Vec<u8> {
    Command::new(tool("debugfs").unwrap()).args(["-R", &format!("cat {path}")]).arg(img).output().unwrap().stdout
}

fn registry() -> DriverRegistry {
    crate::default_registry()
}

/// Opens an image, retrying briefly while it is locked. Tests run in
/// parallel and some fork e2fsprogs: a child forked while an image is open
/// shares its file (and flock) until it execs, so a lock can outlive our
/// close by a moment. (The app never forks.)
fn open_retry(img: &Path, mode: AccessMode) -> Box<dyn BlockDevice> {
    for _ in 0..200 {
        match open_path(img, mode) {
            Err(e) if e.kind() == ErrorKind::InUse => std::thread::sleep(std::time::Duration::from_millis(10)),
            r => return r.unwrap(),
        }
    }
    panic!("{} stayed locked", img.display())
}

fn file_retry(img: &Path, mode: AccessMode) -> FileDevice {
    for _ in 0..200 {
        match FileDevice::open(img, mode) {
            Err(e) if e.kind() == ErrorKind::InUse => std::thread::sleep(std::time::Duration::from_millis(10)),
            r => return r.unwrap(),
        }
    }
    panic!("{} stayed locked", img.display())
}

fn mount(img: &Path, mode: AccessMode) -> (Box<dyn FileSystem>, MountLog) {
    let log = MountLog::new();
    let dev = open_retry(img, mode);
    let m = registry().mount_with(dev, None, MountOptions { mode, log: log.clone() }).unwrap();
    assert_eq!(m.driver.info().id, "ext4");
    (m.fs, log)
}

fn p(s: &str) -> VfsPath {
    VfsPath::parse(s).unwrap()
}

fn names(fs: &mut dyn FileSystem, dir: &str) -> Vec<String> {
    let mut v: Vec<String> = fs.read_dir(&p(dir)).unwrap().into_iter().map(|e| e.name).collect();
    v.sort();
    v
}

#[test]
fn reads_an_image_made_by_mkfs() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("read");
    let img = make_image(&s.0, "ext4");
    let (mut fs, log) = mount(&img, AccessMode::ReadOnly);
    assert!(log.max_level() < Some(LogLevel::Warning), "{:?}", log.entries());
    assert_eq!(fs.info().fs_name, "ext4");
    assert_eq!(fs.info().label.as_deref(), Some("TESTVOL"));
    let root = names(fs.as_mut(), "/");
    assert!(root.contains(&"hello.txt".to_string()) && root.contains(&"docs".to_string()), "{root:?}");
    assert_eq!(fs.read_to_vec(&p("/hello.txt")).unwrap(), b"hello from the host\n");
    assert_eq!(fs.read_to_vec(&p("/docs/deep/big.bin")).unwrap(), big(3 * 1024 * 1024 + 123));
    assert_eq!(names(fs.as_mut(), "/docs").len(), 301);
    assert_eq!(fs.read_to_vec(&p("/docs/f123")).unwrap(), b"file 123");
    #[cfg(unix)]
    {
        let m = fs.metadata(&p("/link")).unwrap();
        assert_eq!(m.kind, EntryKind::Symlink);
        assert_eq!(m.symlink_target.as_deref(), Some("hello.txt"));
    }
    // Read-only means read-only.
    assert_eq!(fs.create_dir(&p("/x")).unwrap_err().kind(), ErrorKind::ReadOnly);
}

fn write_and_check(kind: &str) {
    let s = Scratch::new(&format!("write-{kind}"));
    let img = make_image(&s.0, kind);
    {
        let (mut fs, _) = mount(&img, AccessMode::ReadWrite);
        fs.create_dir(&p("/new")).unwrap();
        fs.create_dir(&p("/new/sub")).unwrap();
        fs.write_file(&p("/new/sub/a.txt"), b"written by the explorer").unwrap();
        fs.write_file(&p("/new/big.bin"), &big(5 * 1024 * 1024 + 7)).unwrap();
        for i in 0..200 {
            fs.write_file(&p(&format!("/new/many-{i}")), format!("n{i}").as_bytes()).unwrap();
        }
        fs.write_file(&p("/hello.txt"), b"replaced").unwrap(); // overwrite
        fs.rename(&p("/docs/f001"), &p("/new/moved")).unwrap();
        fs.remove_file(&p("/docs/f002")).unwrap();
        fs.remove_file(&p("/docs/deep/big.bin")).unwrap();
        fs.remove_dir(&p("/docs/deep")).unwrap();
        assert_eq!(fs.remove_dir(&p("/docs")).unwrap_err().kind(), ErrorKind::DirectoryNotEmpty);
        assert_eq!(fs.create_dir(&p("/new")).unwrap_err().kind(), ErrorKind::AlreadyExists);
        fs.sync().unwrap();
        fs.unmount().unwrap();
    }
    let (ok, report) = fsck_clean(&img);
    assert!(ok, "e2fsck found problems on {kind}:\n{report}");
    // An independent reader agrees.
    assert_eq!(debugfs_cat(&img, "/new/sub/a.txt"), b"written by the explorer");
    assert_eq!(debugfs_cat(&img, "/new/big.bin"), big(5 * 1024 * 1024 + 7));
    assert_eq!(debugfs_cat(&img, "/hello.txt"), b"replaced");
    assert_eq!(debugfs_cat(&img, "/new/moved"), b"file 1");
    // And so do we, after a fresh mount (which finds it clean).
    let (mut fs, log) = mount(&img, AccessMode::ReadOnly);
    assert!(log.max_level() < Some(LogLevel::Warning), "{:?}", log.entries());
    assert_eq!(names(fs.as_mut(), "/new").len(), 203); // sub, big.bin, moved, 200 × many-*
    assert!(!fs.exists(&p("/docs/deep")));
}

#[test]
fn writes_ext4_that_e2fsck_accepts() {
    if have_e2fsprogs() {
        write_and_check("ext4");
    }
}

#[test]
fn writes_ext3_that_e2fsck_accepts() {
    if have_e2fsprogs() {
        write_and_check("ext3");
    }
}

#[test]
fn writes_ext2_that_e2fsck_accepts() {
    if have_e2fsprogs() {
        write_and_check("ext2");
    }
}

fn sha(img: &Path) -> Vec<u8> {
    std::fs::read(img).unwrap()
}

/// Committed to the journal only (ruxis's crash simulation), then gone:
/// the data exists only in the journal until the next mount replays it.
#[test]
fn journal_replay_after_a_crash() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("crash");
    let img = make_image(&s.0, "ext4");
    {
        let (dev, plug) = DetachableDevice::new(file_retry(&img, AccessMode::ReadWrite));
        let opts = MountOptions { mode: AccessMode::ReadWrite, ..Default::default() };
        let mut fs = Ext4Volume::open(Box::new(dev), &opts).unwrap();
        fs.create_dir(&p("/after-crash")).unwrap();
        fs.write_file(&p("/after-crash/note.txt"), b"only in the journal").unwrap();
        // Log the transaction, don't write it in place, stop.
        ruxis::simulate_crash(&fs.root).unwrap();
        plug.pull();
    }

    // Read-only: replayed in memory; the image is not modified.
    let before = sha(&img);
    {
        let (mut fs, log) = mount(&img, AccessMode::ReadOnly);
        let text: Vec<String> = log.entries().into_iter().map(|e| e.message).collect();
        assert!(text.iter().any(|m| m.contains("not cleanly unmounted")), "{text:#?}");
        assert!(text.iter().any(|m| m.contains("JBD2: recovery")), "{text:#?}");
        assert!(text.iter().any(|m| m.contains("in memory")), "{text:#?}");
        assert_eq!(fs.read_to_vec(&p("/after-crash/note.txt")).unwrap(), b"only in the journal");
    }
    assert!(sha(&img) == before, "a read-only mount modified the image");

    // Read-write: replayed for real; then e2fsck is happy.
    {
        let (mut fs, log) = mount(&img, AccessMode::ReadWrite);
        assert_eq!(log.max_level(), Some(LogLevel::Warning), "{:?}", log.entries());
        assert_eq!(fs.read_to_vec(&p("/after-crash/note.txt")).unwrap(), b"only in the journal");
        fs.unmount().unwrap();
    }
    let (ok, report) = fsck_clean(&img);
    assert!(ok, "e2fsck after replay:\n{report}");
    assert_eq!(debugfs_cat(&img, "/after-crash/note.txt"), b"only in the journal");
}

/// A forced eject at an arbitrary moment (the app's Force Eject).
#[test]
fn forced_eject_is_recovered() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("yank");
    let img = make_image(&s.0, "ext4");
    {
        let (dev, plug) = DetachableDevice::new(file_retry(&img, AccessMode::ReadWrite));
        let mut fs = registry().mount(Box::new(dev), None, AccessMode::ReadWrite).unwrap().fs;
        fs.write_file(&p("/synced.txt"), b"durable").unwrap();
        fs.sync().unwrap();
        for i in 0..50 {
            fs.write_file(&p(&format!("/docs/unsynced-{i}")), &big(20_000)).unwrap();
        }
        plug.pull(); // gone, mid-way; then dropped without unmount
    }
    let (mut fs, log) = mount(&img, AccessMode::ReadWrite);
    assert!(log.entries().iter().any(|e| e.message.contains("not cleanly unmounted")), "{:?}", log.entries());
    assert_eq!(fs.read_to_vec(&p("/synced.txt")).unwrap(), b"durable");
    fs.unmount().unwrap();
    drop(fs);
    let (ok, report) = fsck_clean(&img);
    assert!(ok, "e2fsck after a forced eject:\n{report}");
}

#[test]
fn checkpoint_then_more_changes_still_recover() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("checkpoint");
    let img = make_image(&s.0, "ext4");
    {
        let (dev, plug) = DetachableDevice::new(file_retry(&img, AccessMode::ReadWrite));
        let mut fs = registry().mount(Box::new(dev), None, AccessMode::ReadWrite).unwrap().fs;
        fs.unmount().unwrap(); // clean checkpoint ("Save image")
        fs.write_file(&p("/later.txt"), b"after the checkpoint").unwrap();
        fs.sync().unwrap();
        plug.pull();
    }
    let (_, log) = mount(&img, AccessMode::ReadOnly);
    assert!(log.entries().iter().any(|e| e.message.contains("not cleanly unmounted")), "{:?}", log.entries());
}

// ----- mkfs ---------------------------------------------------------------------

fn mkfs_image(dir: &Path, variant: &str, size: u64) -> PathBuf {
    let img = dir.join(format!("{variant}-{size}.img"));
    let mut dev = FileDevice::create(&img, size).unwrap();
    let opts = dix_core::FormatOptions { label: Some("MADE-HERE".into()), variant: Some(variant.into()) };
    registry().format(&mut dev, "ext4", &opts).unwrap();
    img
}

fn dumpe2fs_features(img: &Path) -> String {
    let out = Command::new(tool("dumpe2fs").unwrap()).arg("-h").arg(img).output().unwrap();
    String::from_utf8_lossy(&out.stdout).lines().find(|l| l.starts_with("Filesystem features")).unwrap_or("").to_owned()
}

fn journal_features(img: &Path) -> String {
    let out = Command::new(tool("dumpe2fs").unwrap()).arg("-h").arg(img).output().unwrap();
    String::from_utf8_lossy(&out.stdout).lines().find(|l| l.starts_with("Journal features")).unwrap_or("").to_owned()
}

/// Physical block of logical block `n` of inode `ino`.
fn bmap(img: &Path, ino: u32, n: u64) -> u64 {
    let out =
        Command::new(tool("debugfs").unwrap()).args(["-R", &format!("bmap <{ino}> {n}")]).arg(img).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().expect("debugfs bmap")
}

fn mkfs_round_trip(variant: &str, size: u64) {
    mkfs_round_trip_with(variant, size, mkfs::Options::default());
}

fn mkfs_round_trip_with(variant: &str, size: u64, opts: mkfs::Options) {
    let s = Scratch::new(&format!("mkfs-{variant}-{size}-{}", opts.meta_bg.is_some()));
    let img = if opts.meta_bg.is_none() && opts.block_size.is_none() {
        mkfs_image(&s.0, variant, size)
    } else {
        let img = s.0.join("made.img");
        let mut dev = FileDevice::create(&img, size).unwrap();
        mkfs::mkfs_with(&mut dev, mkfs::Variant::parse(variant).unwrap(), "MADE-HERE", &opts).unwrap();
        img
    };
    let (ok, report) = fsck_clean(&img);
    assert!(ok, "e2fsck right after mkfs ({variant}, {size}):\n{report}");
    let features = dumpe2fs_features(&img);
    match variant {
        "ext2" => assert!(!features.contains("has_journal"), "{features}"),
        "ext3" => assert!(features.contains("has_journal") && !features.contains("extent"), "{features}"),
        _ => {
            for f in ["extent", "metadata_csum", "64bit", "flex_bg"] {
                assert!(features.contains(f), "{features}");
            }
        }
    }
    assert_eq!(features.contains("meta_bg"), opts.meta_bg == Some(true), "{features}");
    // ext2/ext3 stay 32-bit with their bitmaps in their groups.
    assert_eq!(features.contains("64bit") || features.contains("flex_bg"), variant == "ext4", "{features}");
    let journal = journal_features(&img);
    assert_eq!(journal.contains("journal_checksum_v3"), variant == "ext4", "{variant}: {journal}");
    {
        let (mut fs, log) = mount(&img, AccessMode::ReadWrite);
        assert!(log.max_level() < Some(LogLevel::Warning), "{:?}", log.entries());
        assert_eq!(fs.info().fs_name, variant);
        assert_eq!(fs.info().label.as_deref(), Some("MADE-HERE"));
        assert_eq!(names(fs.as_mut(), "/"), vec!["lost+found"]);
        fs.create_dir(&p("/many")).unwrap();
        // Many entries: an indexed (htree) directory where enabled.
        let n = if size < (64 << 20) { 1500 } else { 3000 };
        for i in 0..n {
            fs.write_file(&p(&format!("/many/entry-with-a-long-name-{i:05}.txt")), format!("{i}").as_bytes()).unwrap();
        }
        fs.write_file(&p("/big.bin"), &big(2 * 1024 * 1024 + 3)).unwrap();
        fs.rename(&p("/many/entry-with-a-long-name-00007.txt"), &p("/seven.txt")).unwrap();
        fs.remove_file(&p("/many/entry-with-a-long-name-00008.txt")).unwrap();
        fs.sync().unwrap();
        fs.unmount().unwrap();
    }
    let (ok, report) = fsck_clean(&img);
    assert!(ok, "e2fsck after writing ({variant}, {size}):\n{report}");
    let (errors, lines) = our_check(&img);
    assert_eq!(errors, 0, "our checker after writing ({variant}, {size}):\n{lines:#?}");
    assert_eq!(debugfs_cat(&img, "/seven.txt"), b"7");
    // With dir_index (ext3/ext4) the big directory got a hashed index.
    let htree = Command::new(tool("debugfs").unwrap()).args(["-R", "htree /many"]).arg(&img).output().unwrap();
    let htree = String::from_utf8_lossy(&htree.stdout);
    assert_eq!(htree.contains("Root node dump"), variant != "ext2", "{variant}: {}", &htree[..htree.len().min(300)]);
    let n = if size < (64 << 20) { 1500 } else { 3000 };
    let last = format!("/many/entry-with-a-long-name-{:05}.txt", n - 1);
    assert_eq!(debugfs_cat(&img, &last), format!("{}", n - 1).as_bytes());
    assert_eq!(debugfs_cat(&img, "/big.bin"), big(2 * 1024 * 1024 + 3));
    let (mut fs, log) = mount(&img, AccessMode::ReadOnly);
    assert!(log.max_level() < Some(LogLevel::Warning), "{:?}", log.entries());
    assert_eq!(names(fs.as_mut(), "/many").len(), n - 2);
}

#[test]
fn mkfs_ext2_small_and_large() {
    if have_e2fsprogs() {
        mkfs_round_trip("ext2", 8 << 20);
        mkfs_round_trip("ext2", 600 << 20);
    }
}

#[test]
fn mkfs_ext3_small_and_large() {
    if have_e2fsprogs() {
        mkfs_round_trip("ext3", 8 << 20);
        mkfs_round_trip("ext3", 600 << 20);
    }
}

#[test]
fn mkfs_ext4_small_and_large() {
    if have_e2fsprogs() {
        mkfs_round_trip("ext4", 8 << 20);
        mkfs_round_trip("ext4", 64 << 20);
        mkfs_round_trip("ext4", 600 << 20);
    }
}

/// A big ext4: many flex groups, most of them uninitialised. The driver
/// writes into groups whose inode tables mkfs never touched (directories
/// spread out, a file bigger than a group), and both checkers agree.
#[test]
fn mkfs_ext4_large_uninit_groups() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("mkfs-ext4-large");
    let size = 40u64 << 30; // sparse: only the metadata is written
    let img = mkfs_image(&s.0, "ext4", size);
    let (ok, report) = fsck_clean(&img);
    assert!(ok, "e2fsck right after mkfs:\n{report}");
    let out = Command::new(tool("dumpe2fs").unwrap()).arg(&img).output().unwrap();
    let dump = String::from_utf8_lossy(&out.stdout);
    assert!(dump.contains("Flex block group size:    16"), "{}", &dump[..2000.min(dump.len())]);
    assert!(dump.matches("INODE_UNINIT").count() > 300, "most groups start uninitialised");
    assert!(dump.matches("BLOCK_UNINIT").count() > 200);
    // Bitmaps of group 17 live in flex group 1's leader, group 16.
    let g16 = dump.find("Group 16:").unwrap();
    let g17 = dump.find("Group 17:").unwrap();
    assert!(dump[g17..].contains("Block bitmap at 524"), "{}", &dump[g16..g16 + 900]);
    let (errors, lines) = our_check(&img);
    assert_eq!(errors, 0, "{lines:#?}");
    {
        let (mut fs, log) = mount(&img, AccessMode::ReadWrite);
        assert!(log.max_level() < Some(LogLevel::Warning), "{:?}", log.entries());
        for d in 0..40 {
            fs.create_dir(&p(&format!("/d{d}"))).unwrap();
            for f in 0..5 {
                fs.write_file(&p(&format!("/d{d}/f{f}")), format!("{d}/{f}").as_bytes()).unwrap();
            }
        }
        // 160 MiB: more than one group (128 MiB) of data.
        let chunk = big(1 << 20);
        let mut data = Vec::with_capacity(160 << 20);
        for _ in 0..160 {
            data.extend_from_slice(&chunk);
        }
        fs.write_file(&p("/d39/huge.bin"), &data).unwrap();
        fs.sync().unwrap();
        fs.unmount().unwrap();
    }
    let (ok, report) = fsck_clean(&img);
    assert!(ok, "e2fsck after writing:\n{report}");
    let (errors, lines) = our_check(&img);
    assert_eq!(errors, 0, "{lines:#?}");
    assert_eq!(debugfs_cat(&img, "/d23/f4"), b"23/4");
    // The directories were spread over the groups (8192 inodes each), into
    // ones that started INODE_UNINIT.
    let out = Command::new(tool("debugfs").unwrap()).args(["-R", "ls -l /"]).arg(&img).output().unwrap();
    let groups: std::collections::BTreeSet<u64> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next()?.parse::<u64>().ok())
        .map(|ino| (ino - 1) / 8192)
        .collect();
    assert!(groups.len() > 5, "directories in groups {groups:?}");
    let (mut fs, _) = mount(&img, AccessMode::ReadOnly);
    assert_eq!(fs.read_to_vec(&p("/d39/huge.bin")).unwrap().len(), 160 << 20);
}

/// 64bit for real: more than 2^32 blocks. 2 KiB blocks on a sparse 8 TiB
/// image (a quarter million groups, ~400 MB of descriptor tables after the
/// superblock copies), and 1 KiB blocks on 4 TiB, where the table (32 MiB)
/// is bigger than a group and needs meta_bg. Heavy, so run on request:
/// `cargo test --release -- --ignored over_2_32`.
#[test]
#[ignore]
fn mkfs_ext4_over_2_32_blocks() {
    if !have_e2fsprogs() {
        return;
    }
    over_2_32(2048, 8, false);
    over_2_32(1024, 4, true);
}

fn over_2_32(bs: u64, tib: u64, meta_bg: bool) {
    let s = Scratch::new(&format!("mkfs-2-32-{bs}"));
    let img = s.0.join("huge.img");
    let size = (tib << 40) + (64 << 20);
    {
        // Too many groups without meta_bg.
        let mut dev = FileDevice::create(&img, size).unwrap();
        let opts = mkfs::Options { block_size: Some(1024), meta_bg: Some(false) };
        let r = mkfs::mkfs_with(&mut dev, mkfs::Variant::Ext4, "", &opts);
        assert_eq!(r.unwrap_err().kind(), ErrorKind::InvalidInput);
        let opts = mkfs::Options { block_size: Some(bs), meta_bg: None };
        mkfs::mkfs_with(&mut dev, mkfs::Variant::Ext4, "HUGE", &opts).unwrap();
    }
    assert_eq!(dumpe2fs_features(&img).contains("meta_bg"), meta_bg);
    let out = Command::new(tool("dumpe2fs").unwrap()).arg("-h").arg(&img).output().unwrap();
    let head = String::from_utf8_lossy(&out.stdout);
    let count: u64 = head
        .lines()
        .find_map(|l| l.strip_prefix("Block count:"))
        .and_then(|v| v.trim().parse().ok())
        .expect("block count");
    assert!(count > u32::MAX as u64, "{count}");
    let (ok, report) = fsck_clean(&img);
    assert!(ok, "e2fsck right after mkfs:\n{}", &report[..report.len().min(4000)]);
    {
        let (mut fs, log) = mount(&img, AccessMode::ReadWrite);
        assert!(log.max_level() < Some(LogLevel::Warning), "{:?}", log.entries());
        for d in 0..20 {
            fs.create_dir(&p(&format!("/d{d}"))).unwrap();
            fs.write_file(&p(&format!("/d{d}/f")), &big(100_000 + d)).unwrap();
        }
        fs.unmount().unwrap();
    }
    let (ok, report) = fsck_clean(&img);
    assert!(ok, "e2fsck after writing:\n{}", &report[..report.len().min(4000)]);
    let (errors, lines) = our_check(&img);
    assert_eq!(errors, 0, "{:#?}", &lines[..lines.len().min(50)]);
    assert_eq!(debugfs_cat(&img, "/d19/f"), big(100_019));
}

/// meta_bg on request: descriptor blocks spread over the groups.
#[test]
fn mkfs_meta_bg() {
    if !have_e2fsprogs() {
        return;
    }
    let opts = mkfs::Options { meta_bg: Some(true), ..Default::default() };
    for variant in ["ext2", "ext3", "ext4"] {
        mkfs_round_trip_with(variant, 64 << 20, opts);
    }
    // 1 KiB blocks, 32-byte descriptors: 32 per block, so 8 descriptor
    // blocks in three groups each (the last meta group has two).
    let opts = mkfs::Options { meta_bg: Some(true), block_size: Some(1024) };
    mkfs_round_trip_with("ext3", 2000 << 20, opts);
    let s = Scratch::new("mkfs-meta-bg-layout");
    let img = s.0.join("m.img");
    let mut dev = FileDevice::create(&img, 2000 << 20).unwrap();
    mkfs::mkfs_with(&mut dev, mkfs::Variant::Ext3, "", &opts).unwrap();
    drop(dev);
    let out = Command::new(tool("dumpe2fs").unwrap()).arg(&img).output().unwrap();
    let dump = String::from_utf8_lossy(&out.stdout);
    let at = |g: usize| {
        let i = dump.find(&format!("Group {g}:")).unwrap();
        let j = dump[i..].find("Block bitmap").unwrap();
        dump[i..i + j].contains("Group descriptor at")
    };
    for (g, has) in [(0, true), (1, true), (2, false), (31, true), (32, true), (33, true), (34, false), (63, true)] {
        assert_eq!(at(g), has, "group {g}");
    }
}

#[test]
fn mkfs_minimum_sizes() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("mkfs-min");
    for (variant, size) in
        [("ext2", mkfs::MIN_SIZE), ("ext3", mkfs::MIN_SIZE_JOURNAL), ("ext4", mkfs::MIN_SIZE_JOURNAL)]
    {
        let img = mkfs_image(&s.0, variant, size);
        let (ok, report) = fsck_clean(&img);
        assert!(ok, "{variant} at its minimum size:\n{report}");
    }
    // Too small is refused, not half-made.
    let img = s.0.join("tiny.img");
    let mut dev = FileDevice::create(&img, 64 * 1024).unwrap();
    let r = mkfs::mkfs(&mut dev, mkfs::Variant::Ext2, "");
    assert_eq!(r.unwrap_err().kind(), ErrorKind::NoSpace);
}

/// The journal our mkfs creates works for crash recovery.
#[test]
fn mkfs_journal_replays_after_a_crash() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("mkfs-crash");
    let img = mkfs_image(&s.0, "ext4", 32 << 20);
    {
        let (dev, plug) = DetachableDevice::new(file_retry(&img, AccessMode::ReadWrite));
        let opts = MountOptions { mode: AccessMode::ReadWrite, ..Default::default() };
        let mut fs = Ext4Volume::open(Box::new(dev), &opts).unwrap();
        fs.write_file(&p("/journaled.txt"), b"replayed from our own journal").unwrap();
        ruxis::simulate_crash(&fs.root).unwrap();
        plug.pull();
    }
    let (mut fs, log) = mount(&img, AccessMode::ReadWrite);
    assert!(log.entries().iter().any(|e| e.message.contains("JBD2: recovery")), "{:?}", log.entries());
    assert_eq!(fs.read_to_vec(&p("/journaled.txt")).unwrap(), b"replayed from our own journal");
    fs.unmount().unwrap();
    drop(fs);
    let (ok, report) = fsck_clean(&img);
    assert!(ok, "e2fsck after replay:\n{report}");
}

/// What journal checksums buy: a block damaged inside the journal is
/// detected at replay and not written into the file system.
#[test]
fn journal_checksums_catch_a_damaged_transaction() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("jcsum");
    let img = mkfs_image(&s.0, "ext4", 32 << 20);
    assert!(journal_features(&img).contains("journal_checksum_v3"));
    {
        let (dev, plug) = DetachableDevice::new(file_retry(&img, AccessMode::ReadWrite));
        let opts = MountOptions { mode: AccessMode::ReadWrite, ..Default::default() };
        let mut fs = Ext4Volume::open(Box::new(dev), &opts).unwrap();
        fs.write_file(&p("/x.txt"), b"x").unwrap();
        ruxis::simulate_crash(&fs.root).unwrap();
        plug.pull();
    }
    // The transaction: a descriptor at journal block 1, logged blocks after
    // it. Damage the first logged block (bit rot, a torn write...).
    let block_size = 1024u64;
    let at = bmap(&img, 8, 2) * block_size;
    let mut bytes = std::fs::read(&img).unwrap();
    for b in &mut bytes[at as usize..(at + 16) as usize] {
        *b ^= 0xA5;
    }
    std::fs::write(&img, &bytes).unwrap();

    let (_, log) = mount(&img, AccessMode::ReadOnly);
    let text: Vec<String> = log.entries().into_iter().map(|e| e.message).collect();
    assert!(text.iter().any(|m| m.contains("Invalid checksum recovering")), "{text:#?}");
}

// ----- fsck ---------------------------------------------------------------------

/// Our checker's verdict: (errors, log lines).
fn our_check(img: &Path) -> (u64, Vec<String>) {
    let (report, lines) = our_check_with(img, false);
    (report.errors, lines)
}

fn our_check_with(img: &Path, repair: bool) -> (dix_core::CheckReport, Vec<String>) {
    let log = MountLog::new();
    let mode = if repair { AccessMode::ReadWrite } else { AccessMode::ReadOnly };
    let dev = open_retry(img, mode);
    let opts = dix_core::CheckOptions { log: log.clone(), repair };
    let report = registry().check(dev, None, &opts).unwrap();
    let lines = log.entries().into_iter().map(|e| format!("{:?}: {}", e.level, e.message)).collect();
    (report, lines)
}

/// Images made by e2fsprogs with various features, by our mkfs, and written
/// by the driver: both e2fsck and our checker must call them clean.
#[test]
fn fsck_agrees_on_clean_images() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("fsck-clean");
    let mut images = Vec::new();
    // e2fsprogs, with its defaults (flex_bg, 64bit, resize_inode, ...).
    for (kind, size, extra) in [
        ("ext4", "64M", vec![]),
        ("ext4", "600M", vec![]),
        ("ext4", "32M", vec!["-b", "1024"]),
        ("ext4", "64M", vec!["-O", "^metadata_csum"]),
        ("ext4", "64M", vec!["-O", "^metadata_csum,uninit_bg"]),
        ("ext4", "64M", vec!["-O", "meta_bg,^resize_inode"]),
        ("ext3", "64M", vec![]),
        ("ext2", "64M", vec![]),
    ] {
        let img = s.0.join(format!("e2-{kind}-{size}-{}.img", images.len()));
        let st = Command::new(tool("mke2fs").unwrap())
            .args(["-q", "-F", "-t", kind])
            .args(&extra)
            .arg("-d")
            .arg(make_src(&s.0))
            .arg(&img)
            .arg(size)
            .status()
            .unwrap();
        assert!(st.success());
        images.push(img);
    }
    // Our mkfs, then written through the driver.
    for v in ["ext2", "ext3", "ext4"] {
        let img = mkfs_image(&s.0, v, 32 << 20);
        let (mut fs, _) = mount(&img, AccessMode::ReadWrite);
        fs.create_dir(&p("/d")).unwrap();
        for i in 0..400 {
            fs.write_file(&p(&format!("/d/f{i}")), &big(i * 37)).unwrap();
        }
        fs.remove_file(&p("/d/f3")).unwrap();
        fs.unmount().unwrap();
        drop(fs);
        images.push(img);
    }
    for img in &images {
        let (ok, report) = fsck_clean(img);
        assert!(ok, "e2fsck: {}\n{report}", img.display());
        let (errors, lines) = our_check(img);
        assert_eq!(errors, 0, "our checker on {}:\n{}", img.display(), lines.join("\n"));
    }
}

/// A source tree for `mke2fs -d` (once per scratch dir).
fn make_src(dir: &Path) -> PathBuf {
    let src = dir.join("src");
    if !src.exists() {
        std::fs::create_dir_all(src.join("docs/deep")).unwrap();
        std::fs::write(src.join("hello.txt"), b"hello").unwrap();
        std::fs::write(src.join("docs/deep/big.bin"), big(3 * 1024 * 1024 + 5)).unwrap();
        for i in 0..300 {
            std::fs::write(src.join(format!("docs/f{i:03}")), format!("file {i}")).unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink("hello.txt", src.join("link")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("x".repeat(100), src.join("longlink")).unwrap();
    }
    src
}

fn debugfs_w(img: &Path, cmd: &str) {
    let out = Command::new(tool("debugfs").unwrap()).args(["-w", "-R", cmd]).arg(img).output().unwrap();
    assert!(out.status.success(), "debugfs {cmd}: {}", String::from_utf8_lossy(&out.stderr));
}

fn debugfs_out(img: &Path, cmd: &str) -> String {
    let out = Command::new(tool("debugfs").unwrap()).args(["-R", cmd]).arg(img).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn block_of(img: &Path, path: &str) -> u64 {
    debugfs_out(img, &format!("bmap {path} 0")).trim().parse().expect("bmap")
}

fn free_block(img: &Path) -> u64 {
    let out = debugfs_out(img, "ffb 1 20000");
    out.split_whitespace().last().and_then(|w| w.parse().ok()).expect("ffb")
}

fn fresh(dir: &Path, kind: &str, name: &str) -> PathBuf {
    let img = dir.join(format!("{name}.img"));
    let st = Command::new(tool("mke2fs").unwrap())
        .args(["-q", "-F", "-t", kind, "-d"])
        .arg(make_src(dir))
        .arg(&img)
        .arg("64M")
        .status()
        .unwrap();
    assert!(st.success());
    img
}

/// Damage an image in a known way: e2fsck must complain, and so must we
/// (with a message containing `expect`).
fn both_detect(img: &Path, what: &str, expect: &str) {
    let (ok, report) = fsck_clean(img);
    assert!(!ok, "{what}: e2fsck found nothing?\n{report}");
    let (errors, lines) = our_check(img);
    assert!(errors > 0, "{what}: our checker found nothing\ne2fsck said:\n{report}\nwe said:\n{}", lines.join("\n"));
    assert!(
        lines.iter().any(|l| l.contains(expect)),
        "{what}: expected a message with '{expect}'\nwe said:\n{}\ne2fsck said:\n{report}",
        lines.join("\n")
    );
}

#[test]
fn fsck_detects_what_e2fsck_detects() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("fsck-bad");
    for kind in ["ext4", "ext3"] {
        let case = |name: &str| fresh(&s.0, kind, &format!("{kind}-{name}"));

        let img = case("links");
        debugfs_w(&img, "sif /hello.txt links_count 5");
        both_detect(&img, "wrong link count", "link count is 5, should be 1");

        let img = case("freeb");
        let b = block_of(&img, "/docs/f001");
        debugfs_w(&img, &format!("freeb {b}"));
        both_detect(&img, "used block marked free", "in use but marked free");

        let img = case("setb");
        let b = free_block(&img);
        debugfs_w(&img, &format!("setb {b}"));
        both_detect(&img, "free block marked used", "marked in use but free");

        let img = case("clri");
        debugfs_w(&img, "clri /docs/f002");
        both_detect(&img, "entry to a cleared inode", "points to unused inode");

        let img = case("unlink");
        debugfs_w(&img, "unlink /docs/f004");
        both_detect(&img, "unattached inode", "unattached");

        // Not damage for e2fsck (it notes it and exits 0): a warning for us.
        let img = case("sbfree");
        debugfs_w(&img, "ssv free_blocks_count 12");
        let (ok, report) = fsck_clean(&img);
        assert!(ok && report.contains("Free blocks count wrong"), "{report}");
        let (errors, lines) = our_check(&img);
        assert_eq!(errors, 0, "{}", lines.join("\n"));
        assert!(lines.iter().any(|l| l.starts_with("Warning") && l.contains("free blocks count in the superblock")));

        let img = case("bgfree");
        debugfs_w(&img, "set_bg 0 free_inodes_count 3");
        both_detect(&img, "group free count", "free inodes count is 3");

        let img = case("size");
        debugfs_w(&img, "sif /docs/deep/big.bin size 10");
        both_detect(&img, "size smaller than the data", "size 10 but it has blocks");

        let img = case("iblocks");
        debugfs_w(&img, "sif /docs/deep/big.bin blocks 8");
        both_detect(&img, "wrong i_blocks", "i_blocks is 8");

        let img = case("dirtype");
        debugfs_w(&img, "sif /docs mode 0100644");
        both_detect(&img, "a directory turned into a file", "wrong file type");

        let img = case("dup");
        if kind == "ext3" {
            let b = block_of(&img, "/docs/f006");
            debugfs_w(&img, &format!("sif /docs/f005 block[0] {b}"));
            both_detect(&img, "two files share a block", "claimed by another inode");
        }

        // Raw damage to a directory block: a broken entry length.
        let img = case("rec_len");
        let b = block_of(&img, "/docs");
        let mut bytes = std::fs::read(&img).unwrap();
        let block_size: u64 = debugfs_out(&img, "stats")
            .lines()
            .find(|l| l.starts_with("Block size"))
            .and_then(|l| l.split_whitespace().last()?.parse().ok())
            .unwrap();
        let at = (b * block_size) as usize + 12 + 4; // '..' entry's rec_len
        bytes[at] = 3;
        bytes[at + 1] = 0;
        std::fs::write(&img, &bytes).unwrap();
        both_detect(&img, "broken directory entry", "bad entry");
    }

    // metadata_csum: damaged checksums.
    let img = fresh(&s.0, "ext4", "csum-inode");
    debugfs_w(&img, "sif /hello.txt checksum 0x1234");
    both_detect(&img, "inode checksum", "checksum does not match");
}

#[test]
fn fsck_replays_a_pending_journal_in_memory() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("fsck-journal");
    let img = mkfs_image(&s.0, "ext4", 32 << 20);
    {
        let (dev, plug) = DetachableDevice::new(file_retry(&img, AccessMode::ReadWrite));
        let opts = MountOptions { mode: AccessMode::ReadWrite, ..Default::default() };
        let mut fs = Ext4Volume::open(Box::new(dev), &opts).unwrap();
        for i in 0..20 {
            fs.write_file(&p(&format!("/pending-{i}")), &big(5000)).unwrap();
        }
        ruxis::simulate_crash(&fs.root).unwrap();
        plug.pull();
    }
    let before = std::fs::read(&img).unwrap();
    let (errors, lines) = our_check(&img);
    assert_eq!(errors, 0, "{}", lines.join("\n"));
    assert!(lines.iter().any(|l| l.contains("needs recovery")), "{}", lines.join("\n"));
    assert!(lines.iter().any(|l| l.contains("JBD2: recovery")), "{}", lines.join("\n"));
    assert!(std::fs::read(&img).unwrap() == before, "the check modified the image");
}

// ----- repair -------------------------------------------------------------------

/// A named way to damage an image.
type Damage = Box<dyn Fn(&Path)>;

/// Damage that `fsck -p` repairs on its own: we repair it, and afterwards
/// e2fsck finds nothing (and neither do we).
#[test]
fn safe_problems_are_repaired() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("repair-safe");
    for kind in ["ext4", "ext3", "ext2"] {
        let cases: Vec<(&str, Damage)> = vec![
            ("links", Box::new(|img| debugfs_w(img, "sif /hello.txt links_count 5"))),
            ("dirlinks", Box::new(|img| debugfs_w(img, "sif /docs links_count 9"))),
            ("freeb", Box::new(|img| debugfs_w(img, &format!("freeb {}", block_of(img, "/docs/f001"))))),
            ("setb", Box::new(|img| debugfs_w(img, &format!("setb {}", free_block(img))))),
            ("freei", Box::new(|img| debugfs_w(img, "freei /docs/f010"))),
            ("bgfree", Box::new(|img| debugfs_w(img, "set_bg 0 free_inodes_count 3"))),
            ("bgdirs", Box::new(|img| debugfs_w(img, "set_bg 0 used_dirs_count 1"))),
            ("sbfree", Box::new(|img| debugfs_w(img, "ssv free_blocks_count 12"))),
            (
                "several",
                Box::new(|img| {
                    debugfs_w(img, "sif /hello.txt links_count 2");
                    debugfs_w(img, &format!("freeb {}", block_of(img, "/docs/f003")));
                    debugfs_w(img, "set_bg 0 free_blocks_count 7");
                }),
            ),
        ];
        for (name, damage) in cases {
            let img = fresh(&s.0, kind, &format!("{kind}-{name}"));
            damage(&img);
            let (report, lines) = our_check_with(&img, true);
            assert_eq!(report.errors, 0, "{kind}/{name}: not repaired\n{}", lines.join("\n"));
            assert!(report.fixed > 0, "{kind}/{name}: nothing fixed?\n{}", lines.join("\n"));
            let (ok, e2) = fsck_clean(&img);
            assert!(
                ok && !e2.contains("count wrong"),
                "{kind}/{name}: e2fsck after our repair:\n{e2}\nwe said:\n{}",
                lines.join("\n")
            );
            // Repairing again finds nothing to do.
            let (again, _) = our_check_with(&img, true);
            assert_eq!((again.errors, again.fixed), (0, 0), "{kind}/{name}");
        }
    }
}

/// Damage that needs a decision: nothing is repaired, not one byte changes.
#[test]
fn unsafe_problems_are_left_alone() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("repair-unsafe");
    let cases: Vec<(&str, &str, Damage)> = vec![
        ("ext4", "clri", Box::new(|img| debugfs_w(img, "clri /docs/f002"))),
        ("ext4", "unlink", Box::new(|img| debugfs_w(img, "unlink /docs/f004"))),
        ("ext4", "dirtype", Box::new(|img| debugfs_w(img, "sif /docs mode 0100644"))),
        ("ext4", "csum", Box::new(|img| debugfs_w(img, "sif /hello.txt checksum 0x1234"))),
        ("ext4", "size", Box::new(|img| debugfs_w(img, "sif /docs/deep/big.bin size 10"))),
        (
            "ext3",
            "dup",
            Box::new(|img| {
                let b = block_of(img, "/docs/f006");
                debugfs_w(img, &format!("sif /docs/f005 block[0] {b}"));
            }),
        ),
        // A safe problem next to an unsafe one: still nothing is touched.
        (
            "ext4",
            "mixed",
            Box::new(|img| {
                debugfs_w(img, "sif /hello.txt links_count 5");
                debugfs_w(img, "clri /docs/f002");
            }),
        ),
    ];
    for (kind, name, damage) in cases {
        let img = fresh(&s.0, kind, &format!("{kind}-{name}"));
        damage(&img);
        let before = std::fs::read(&img).unwrap();
        let (report, lines) = our_check_with(&img, true);
        assert!(report.errors > 0 && !report.repairable && report.fixed == 0, "{name}: {}", lines.join("\n"));
        assert!(lines.iter().any(|l| l.contains("Not repaired")), "{name}: {}", lines.join("\n"));
        assert!(std::fs::read(&img).unwrap() == before, "{name}: the image was modified");
    }
}

/// A pending journal is replayed for real before repairing.
#[test]
fn repair_replays_the_journal_first() {
    if !have_e2fsprogs() {
        return;
    }
    let s = Scratch::new("repair-journal");
    let img = mkfs_image(&s.0, "ext4", 32 << 20);
    {
        let (dev, plug) = DetachableDevice::new(file_retry(&img, AccessMode::ReadWrite));
        let opts = MountOptions { mode: AccessMode::ReadWrite, ..Default::default() };
        let mut fs = Ext4Volume::open(Box::new(dev), &opts).unwrap();
        fs.write_file(&p("/kept.txt"), b"from the journal").unwrap();
        ruxis::simulate_crash(&fs.root).unwrap();
        plug.pull();
    }
    let (report, lines) = our_check_with(&img, true);
    assert_eq!(report.errors, 0, "{}", lines.join("\n"));
    assert!(lines.iter().any(|l| l.contains("replaying it onto the image")), "{}", lines.join("\n"));
    let (ok, e2) = fsck_clean(&img);
    assert!(ok, "{e2}");
    assert_eq!(debugfs_cat(&img, "/kept.txt"), b"from the journal");
}
