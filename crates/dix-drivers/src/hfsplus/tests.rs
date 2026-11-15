//! Volumes written by Linux's hfsplus driver (see
//! `scripts/testdata/make-hfs.sh`), read here and compared with what Linux
//! saw in them: every entry's type, size, mode, owner, time, link target
//! and content.

use super::*;
use dix_core::device::MemoryDevice;
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/testdata/hfs/{name}.img.gz", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("gzip").arg("-dc").arg(&path).output().expect("gzip");
    assert!(out.status.success(), "{path}");
    out.stdout
}

struct Entry {
    kind: String,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: i64,
    inode: u64,
    target: String,
}

/// Path (without "./") → entry; path → md5.
fn manifest(name: &str) -> (BTreeMap<String, Entry>, BTreeMap<String, String>) {
    let path = format!("{}/testdata/hfs/{name}.manifest", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(path).unwrap();
    let (mut entries, mut sums) = (BTreeMap::new(), BTreeMap::new());
    for line in text.lines() {
        let f: Vec<&str> = line.split('|').collect();
        let p = |s: &str| s.trim_start_matches('.').to_string();
        match f[0] {
            "E" => {
                entries.insert(
                    p(f[9]),
                    Entry {
                        kind: f[1].replace("regular empty file", "regular file"),
                        size: f[2].parse().unwrap(),
                        mode: u32::from_str_radix(f[3], 8).unwrap(),
                        uid: f[4].parse().unwrap(),
                        gid: f[5].parse().unwrap(),
                        mtime: f[6].parse().unwrap(),
                        inode: f[8].parse().unwrap(),
                        target: f[10].into(),
                    },
                );
            }
            "M" => {
                sums.insert(p(f[2]), f[1].to_string());
            }
            _ => {}
        }
    }
    (entries, sums)
}

fn mount(img: Vec<u8>) -> Box<dyn FileSystem> {
    let reg = crate::default_registry();
    let m = reg.mount(Box::new(MemoryDevice::new("hfs", img)), None, AccessMode::ReadOnly).unwrap();
    assert_eq!(m.driver.info().id, "hfsplus");
    m.fs
}

fn md5(data: &[u8]) -> String {
    use std::io::Write;
    let mut c = Command::new("md5sum")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(data).unwrap();
    let out = c.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap().to_string()
}

/// Compares everything Linux listed with what is read here.
fn compare(fs: &mut dyn FileSystem, name: &str) {
    let (entries, sums) = manifest(name);
    // (busybox prints a "?" for some characters: such names are checked by
    // the tests themselves.)
    // Files compressed by hand (see make-hfs.sh) look empty to Linux; they
    // are checked by the tests themselves.
    let shown = |p: &str| {
        !p.contains('?') && !["zlib", "stored", "lzvn", "lzfse"].iter().any(|k| p == format!("/compressed/{k}.txt"))
    };
    // Names Linux shows as one inode (hard links) are one file here too.
    let mut same: BTreeMap<u64, Option<u64>> = BTreeMap::new();
    for (path, e) in entries.iter().filter(|(p, _)| shown(p)) {
        let vp = VfsPath::parse(if path.is_empty() { "/" } else { path }).unwrap();
        let m = fs.metadata(&vp).unwrap_or_else(|err| panic!("{name}: {path}: {err}"));
        let kind = match m.kind {
            EntryKind::Directory => "directory",
            EntryKind::File => "regular file",
            EntryKind::Symlink => "symbolic link",
            EntryKind::Other => "other",
        };
        assert_eq!(kind, e.kind, "{name}: {path}");
        if e.kind != "directory" {
            assert_eq!(m.size, e.size, "{name}: {path}: size");
        }
        if e.mode != 0 && e.kind != "symbolic link" {
            assert_eq!(m.mode.unwrap() & 0o7777, e.mode, "{name}: {path}: mode");
            // Classic HFS has no owners (Linux shows its mount's).
            if m.uid.is_some() {
                assert_eq!(m.uid, Some(e.uid), "{name}: {path}: owner");
                assert_eq!(m.gid, Some(e.gid), "{name}: {path}: group");
            }
        }
        assert_eq!(m.modified, Some(e.mtime), "{name}: {path}: modified");
        if e.kind == "regular file" {
            let first = same.entry(e.inode).or_insert(m.inode);
            assert_eq!(*first, m.inode, "{name}: {path}: a hard link to another file");
        }
        if e.kind == "symbolic link" {
            assert_eq!(m.symlink_target.as_deref(), Some(e.target.as_str()), "{name}: {path}");
        }
        if e.kind == "directory" {
            // The same names, nothing more (resource forks aside).
            let ours: BTreeSet<String> = fs
                .read_dir(&vp)
                .unwrap()
                .into_iter()
                .map(|d| d.name)
                .filter(|n| !n.ends_with(":rsrc") && !n.starts_with(".journal"))
                .collect();
            let prefix = if path.is_empty() { "/".to_string() } else { format!("{path}/") };
            let theirs: BTreeSet<String> = entries
                .keys()
                .filter_map(|p| p.strip_prefix(&prefix))
                .filter(|rest| !rest.is_empty() && !rest.contains('/'))
                .map(|s| s.to_string())
                .filter(|n| !n.starts_with(".journal"))
                .collect();
            let ours_shown: BTreeSet<String> = ours.iter().filter(|n| shown(n)).cloned().collect();
            let theirs_shown: BTreeSet<String> = theirs.iter().filter(|n| shown(n)).cloned().collect();
            assert_eq!(ours_shown, theirs_shown, "{name}: {path}: entries");
            assert_eq!(ours.len(), theirs.len(), "{name}: {path}: entries");
        }
    }
    for (path, sum) in sums.iter().filter(|(p, _)| shown(p)) {
        let data = fs.read_to_vec(&VfsPath::parse(path).unwrap()).unwrap();
        assert_eq!(&md5(&data), sum, "{name}: {path}: content");
    }
}

#[test]
fn hfs_plus_volume_as_linux_wrote_it() {
    let img = fixture("hfsplus");
    let mut fs = mount(img.clone());
    let info = fs.info();
    assert_eq!(info.fs_name, "HFS+");
    assert_eq!(info.label.as_deref(), Some("Test HFS+"));
    compare(fs.as_mut(), "hfsplus");
    // Names only checked here.
    let root: Vec<String> = fs.read_dir(&VfsPath::root()).unwrap().into_iter().map(|d| d.name).collect();
    // (Linux's hfsplus stores characters outside the BMP as "?".)
    assert!(root.contains(&"emoji ????.txt".to_string()), "{root:?}");
    assert!(root.contains(&"a:b.txt".to_string()));
    assert!(!root.iter().any(|n| n.contains("Private")), "{root:?}");
    assert_eq!(fs.read_to_vec(&VfsPath::parse("/emoji ????.txt").unwrap()).unwrap(), b"party\n");
    // Decomposed on disk, composed here; found either way.
    assert_eq!(fs.read_to_vec(&VfsPath::parse("/cafe\u{301}.txt").unwrap()).unwrap(), "café\n".as_bytes());
    // Case-insensitive, like macOS.
    assert_eq!(fs.read_to_vec(&VfsPath::parse("/DOCS/Hello.TXT").unwrap()).unwrap(), b"hello hfs+\n");
    // Hard links: one file.
    let a = fs.metadata(&VfsPath::parse("/docs/hello.txt").unwrap()).unwrap();
    let b = fs.metadata(&VfsPath::parse("/links/hard-hello").unwrap()).unwrap();
    assert_eq!(a.inode, b.inode);
    // The fragmented files really have their extents in the overflow file.
    let mut v = Volume::open(Box::new(MemoryDevice::new("hfs", img)), &dix_core::MountLog::new()).unwrap();
    let (rec, _) = v.resolve(&VfsPath::parse("/frag-a.bin").unwrap()).unwrap();
    assert_eq!(rec.data.extents.len(), 8);
    assert!(v.data_fork(&rec).unwrap().extents.len() > 8);
    assert!(fs.read_dir(&VfsPath::parse("/docs/hello.txt").unwrap()).is_err());
    assert!(fs.metadata(&VfsPath::parse("/nope").unwrap()).is_err());
}

/// Files compressed the way macOS does (a decmpfs attribute, the
/// UF_COMPRESSED flag): zlib, stored, LZVN and LZFSE.
#[test]
fn files_compressed_by_macos() {
    let mut fs = mount(fixture("hfsplus"));
    let p = |s: &str| VfsPath::parse(s).unwrap();
    let expect = fs.read_to_vec(&p("/compressed/expect.txt")).unwrap();
    assert_eq!(expect.len(), 3000);
    for k in ["zlib", "stored", "lzvn"] {
        let path = p(&format!("/compressed/{k}.txt"));
        assert_eq!(fs.metadata(&path).unwrap().size, 3000, "{k}");
        assert!(fs.read_to_vec(&path).unwrap() == expect, "{k}");
        // A piece from the middle.
        let mut b = [0u8; 100];
        assert_eq!(fs.read(&path, 1234, &mut b).unwrap(), 100);
        assert_eq!(&b[..], &expect[1234..1334], "{k}");
    }
    let expect = fs.read_to_vec(&p("/compressed/expect-lzfse.txt")).unwrap();
    assert_eq!(fs.metadata(&p("/compressed/lzfse.txt")).unwrap().size, 20000);
    assert!(fs.read_to_vec(&p("/compressed/lzfse.txt")).unwrap() == expect);
}

/// Classic HFS, as Linux's hfs driver writes it: Mac Roman names.
#[test]
fn classic_hfs() {
    let mut fs = mount(fixture("hfs"));
    let info = fs.info();
    assert_eq!(info.fs_name, "HFS");
    // (mkfs.hfsplus -h names every HFS volume "untitled".)
    assert_eq!(info.label.as_deref(), Some("untitled"));
    compare(fs.as_mut(), "hfs");
    assert_eq!(fs.read_to_vec(&VfsPath::parse("/Système/café.txt").unwrap()).unwrap(), b"bonjour\n");
}

#[test]
fn hfsx_is_case_sensitive() {
    let mut fs = mount(fixture("hfsx"));
    let info = fs.info();
    assert_eq!(info.fs_name, "HFSX");
    assert_eq!(info.block_size, Some(512));
    compare(fs.as_mut(), "hfsx");
    assert_eq!(fs.read_to_vec(&VfsPath::parse("/Name.txt").unwrap()).unwrap(), b"upper\n");
    assert_eq!(fs.read_to_vec(&VfsPath::parse("/name.txt").unwrap()).unwrap(), b"lower\n");
    assert!(fs.metadata(&VfsPath::parse("/NAME.txt").unwrap()).is_err());
}

#[test]
fn journaled_volume_in_an_hfs_wrapper() {
    let mut fs = mount(fixture("wrapped"));
    let info = fs.info();
    assert_eq!(info.label.as_deref(), Some("Wrapped"));
    assert!(info.details.iter().any(|(k, _)| k == "HFS wrapper"));
    assert!(info.details.iter().any(|(k, v)| k == "Journaled" && v == "yes"));
    compare(fs.as_mut(), "wrapped");
    let root = fs.read_dir(&VfsPath::root()).unwrap();
    assert!(root.iter().find(|d| d.name == ".journal").unwrap().metadata.hidden);
}

#[test]
fn not_hfs() {
    let reg = crate::default_registry();
    let d = HfsDriver;
    let mut dev = MemoryDevice::new("x", vec![0u8; 1 << 20]);
    assert_eq!(d.probe(&mut dev).unwrap(), 0);
    let _ = reg;
}
