//! Containers written by the linux-apfs-rw driver (see
//! `scripts/testdata/make-apfs.sh`), checked by apfsck, read here and
//! compared with what Linux saw in them: every entry's type, size, mode,
//! owner, time, link target and content, and the compressed files' content
//! as the Linux driver decompresses it.

use super::*;
use dix_core::device::MemoryDevice;
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use unicode_normalization::UnicodeNormalization;

fn fixture(name: &str) -> Option<Vec<u8>> {
    let path = format!("{}/testdata/apfs/{name}.img.xz", env!("CARGO_MANIFEST_DIR"));
    match Command::new("xz").arg("-dc").arg(&path).output() {
        Ok(out) if out.status.success() => Some(out.stdout),
        _ => {
            eprintln!("xz not installed: skipping");
            None
        }
    }
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

/// Entries, MD5s, and the MD5s of compressed files as Linux decompresses
/// them; paths composed to NFC, as they are shown.
#[allow(clippy::type_complexity)]
fn manifest(name: &str) -> (BTreeMap<String, Entry>, BTreeMap<String, String>, BTreeMap<String, String>) {
    let path = format!("{}/testdata/apfs/{name}.manifest", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(path).unwrap();
    let (mut entries, mut sums, mut unpacked) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    for line in text.lines() {
        let f: Vec<&str> = line.split('|').collect();
        let p = |s: &str| s.trim_start_matches('.').nfc().collect::<String>();
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
            "C" => {
                unpacked.insert(p(f[2]), f[1].to_string());
            }
            _ => {}
        }
    }
    (entries, sums, unpacked)
}

fn mount(img: Vec<u8>) -> Box<dyn FileSystem> {
    let reg = crate::default_registry();
    let m = reg.mount(Box::new(MemoryDevice::new("apfs", img)), None, AccessMode::ReadOnly).unwrap();
    assert_eq!(m.driver.info().id, "apfs");
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
    let (entries, sums, unpacked) = manifest(name);
    let mut same: BTreeMap<u64, Option<u64>> = BTreeMap::new();
    for (path, e) in &entries {
        let vp = VfsPath::parse(if path.is_empty() { "/" } else { path }).unwrap();
        let m = fs.metadata(&vp).unwrap_or_else(|err| panic!("{name}: {path}: {err}"));
        let kind = match m.kind {
            EntryKind::Directory => "directory",
            EntryKind::File => "regular file",
            EntryKind::Symlink => "symbolic link",
            EntryKind::Other => "other",
        };
        assert_eq!(kind, e.kind, "{name}: {path}");
        // Compressed files look empty to Linux's stat; symbolic links have
        // their target's length here.
        if e.kind == "regular file" && !unpacked.contains_key(path) {
            assert_eq!(m.size, e.size, "{name}: {path}: size");
        }
        if e.kind != "symbolic link" {
            assert_eq!(m.mode.unwrap() & 0o7777, e.mode, "{name}: {path}: mode");
        }
        assert_eq!(m.uid, Some(e.uid), "{name}: {path}: owner");
        assert_eq!(m.gid, Some(e.gid), "{name}: {path}: group");
        assert_eq!(m.modified, Some(e.mtime), "{name}: {path}: modified");
        if e.kind == "symbolic link" {
            assert_eq!(m.symlink_target.as_deref(), Some(e.target.as_str()), "{name}: {path}");
        }
        if e.kind == "regular file" {
            let first = same.entry(e.inode).or_insert(m.inode);
            assert_eq!(*first, m.inode, "{name}: {path}: a hard link to another file");
        }
        if e.kind == "directory" {
            let ours: BTreeSet<String> = fs.read_dir(&vp).unwrap().into_iter().map(|d| d.name).collect();
            let prefix = if path.is_empty() { "/".to_string() } else { format!("{path}/") };
            let theirs: BTreeSet<String> = entries
                .keys()
                .filter_map(|p| p.strip_prefix(&prefix))
                .filter(|rest| !rest.is_empty() && !rest.contains('/'))
                .map(|s| s.to_string())
                .collect();
            assert_eq!(ours, theirs, "{name}: {path}: entries");
        }
    }
    for (path, sum) in &sums {
        if unpacked.contains_key(path) {
            continue;
        }
        let data = fs.read_to_vec(&VfsPath::parse(path).unwrap()).unwrap();
        assert_eq!(&md5(&data), sum, "{name}: {path}: content");
    }
    // Compressed files: what the Linux driver decompressed (it shows the
    // stored kind, type 1, as empty: that one is checked by the tests).
    let empty = "d41d8cd98f00b204e9800998ecf8427e";
    for (path, sum) in unpacked.iter().filter(|(_, s)| *s != empty) {
        let data = fs.read_to_vec(&VfsPath::parse(path).unwrap()).unwrap();
        assert_eq!(&md5(&data), sum, "{name}: {path}: decompressed content");
    }
}

#[test]
fn apfs_volume_as_linux_wrote_it() {
    let Some(img) = fixture("apfs") else { return };
    let mut fs = mount(img);
    let info = fs.info();
    assert_eq!(info.fs_name, "APFS");
    assert_eq!(info.label.as_deref(), Some("Test APFS"));
    assert!(info.free_bytes.is_some_and(|f| f > 0 && f < info.total_bytes.unwrap()), "{info:?}");
    compare(fs.as_mut(), "apfs");
    let p = |s: &str| VfsPath::parse(s).unwrap();
    // Case-insensitive.
    assert_eq!(fs.read_to_vec(&p("/DOCS/Hello.TXT")).unwrap(), b"hello apfs\n");
    // A name stored decomposed, shown and found composed (or not).
    assert_eq!(fs.read_to_vec(&p("/café-nfd.txt")).unwrap(), b"nfd\n");
    assert_eq!(fs.read_to_vec(&p("/cafe\u{301}-nfd.txt")).unwrap(), b"nfd\n");
    // A sparse file: zeros, then its tail.
    let sparse = fs.read_to_vec(&p("/sparse.bin")).unwrap();
    assert_eq!(sparse.len(), 5_000_006);
    assert!(sparse[..5_000_000].iter().all(|&b| b == 0));
    assert_eq!(&sparse[5_000_001..], b"tail\n");
    // The stored compressed file (an attribute long enough to be a stream).
    let expect = fs.read_to_vec(&p("/compressed/expect-stored")).unwrap();
    assert_eq!(fs.metadata(&p("/compressed/stored.txt")).unwrap().size, 10000);
    assert!(fs.read_to_vec(&p("/compressed/stored.txt")).unwrap() == expect);
    assert!(fs.read_dir(&p("/docs/hello.txt")).is_err());
    assert!(fs.metadata(&p("/nope")).is_err());
}

/// The files grown in turns really are in many extents.
#[test]
fn fragmented_files() {
    let Some(img) = fixture("apfs") else { return };
    let c = Container::open(Box::new(MemoryDevice::new("apfs", img)), &dix_core::MountLog::new()).unwrap();
    let mut fs = ApfsFs { c };
    let (vol, ino) = fs.resolve(&VfsPath::parse("/frag-a.bin").unwrap()).unwrap().unwrap();
    let n = fs.c.extents(vol, ino.private_id).unwrap().len();
    assert!(n > 8, "{n} extents");
}

#[test]
fn apfs_case_sensitive() {
    let Some(img) = fixture("apfs-cs") else { return };
    let mut fs = mount(img);
    compare(fs.as_mut(), "apfs-cs");
    let p = |s: &str| VfsPath::parse(s).unwrap();
    assert_eq!(fs.read_to_vec(&p("/Name.txt")).unwrap(), b"upper\n");
    assert_eq!(fs.read_to_vec(&p("/name.txt")).unwrap(), b"lower\n");
    assert!(fs.metadata(&p("/NAME.txt")).is_err());
    assert!(fs.info().details.iter().any(|(_, v)| v.contains("case-sensitive")));
}

/// An LZFSE-compressed file whose chunks are in the resource fork.
#[test]
fn apfs_resource_fork_compression() {
    let Some(img) = fixture("apfs-rsrc") else { return };
    let mut fs = mount(img);
    compare(fs.as_mut(), "apfs-rsrc");
    let p = |s: &str| VfsPath::parse(s).unwrap();
    assert_eq!(fs.metadata(&p("/compressed/rlzfse.txt")).unwrap().size, 150_000);
    // Reads that start inside a chunk and cross into the next.
    let all = fs.read_to_vec(&p("/compressed/expect-chunks")).unwrap();
    let mut b = vec![0u8; 10_000];
    assert_eq!(fs.read(&p("/compressed/rlzfse.txt"), 60_000, &mut b).unwrap(), 10_000);
    assert_eq!(&b[..], &all[60_000..70_000]);
}

#[test]
fn checksums() {
    // Fletcher-64 as APFS defines it: an object with its checksum is valid,
    // one changed byte is not.
    let mut b = vec![0u8; 4096];
    b[8] = 1;
    b[100] = 0x55;
    let c = fletcher64(&b[8..]);
    b[..8].copy_from_slice(&c.to_le_bytes());
    assert!(valid(&b));
    b[200] ^= 1;
    assert!(!valid(&b));
}

#[test]
fn not_apfs() {
    let mut dev = MemoryDevice::new("x", vec![0u8; 1 << 20]);
    assert_eq!(ApfsDriver.probe(&mut dev).unwrap(), 0);
}
