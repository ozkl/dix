//! UDF volumes made by genisoimage (UDF 1.02 bridge discs with files),
//! pycdlib (UDF 2.60 with symbolic links) and mkudffs (every revision and
//! media layout: plain, sparable, VAT, metadata partition; 512 to
//! 4096-byte blocks), checked against what was put in them and against
//! udfinfo. Tests whose tool is missing are skipped.

use super::*;
use dix_core::device::MemoryDevice;
use std::path::{Path, PathBuf};
use std::process::Command;

fn have(tool: &str) -> bool {
    Command::new("sh").arg("-c").arg(format!("command -v {tool}")).output().is_ok_and(|o| o.status.success())
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dix-udf-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn mount(img: Vec<u8>, driver: Option<&str>) -> (String, Box<dyn FileSystem>) {
    let reg = crate::default_registry();
    let m = reg.mount(Box::new(MemoryDevice::new("udf", img)), driver, AccessMode::ReadOnly).unwrap();
    (m.driver.info().id.to_owned(), m.fs)
}

fn read_file(fs: &mut dyn FileSystem, path: &str) -> Vec<u8> {
    let p = VfsPath::parse(path).unwrap();
    let size = fs.metadata(&p).unwrap().size as usize;
    let mut out = vec![0u8; size];
    // Odd-sized reads, to cross extent and block boundaries.
    let mut at = 0;
    while at < size {
        let n = (size - at).min(70_001);
        assert_eq!(fs.read(&p, at as u64, &mut out[at..at + n]).unwrap(), n);
        at += n;
    }
    out
}

fn pattern(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8).collect()
}

/// Writes a tree; returns (relative path, content or None for a folder).
fn make_tree(root: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    std::fs::create_dir_all(root).unwrap();
    let mut items: Vec<(String, Option<Vec<u8>>)> = vec![
        ("empty.txt".into(), Some(Vec::new())),
        ("hello.txt".into(), Some(b"Hello, UDF!\n".to_vec())),
        ("big.bin".into(), Some(pattern(3_000_017, 7))),
        ("Café crème.txt".into(), Some(b"latin-1 name".to_vec())),
        ("Ünïcödé ñame — 日本語.txt".into(), Some(b"unicode".to_vec())),
        ("dir".into(), None),
        ("dir/sub".into(), None),
        ("dir/sub/deep.txt".into(), Some(pattern(5000, 3))),
        ("many".into(), None),
    ];
    // A directory longer than a block.
    for i in 0..150 {
        items.push((format!("many/file with a fairly long name {i:03}.dat"), Some(pattern(i * 37, i as u32))));
    }
    for (p, data) in &items {
        let path = root.join(p);
        match data {
            None => std::fs::create_dir_all(&path).unwrap(),
            Some(d) => std::fs::write(&path, d).unwrap(),
        }
    }
    items
}

fn check_tree(fs: &mut dyn FileSystem, items: &[(String, Option<Vec<u8>>)]) {
    for (p, data) in items {
        let path = format!("/{p}");
        let m = fs.metadata(&VfsPath::parse(&path).unwrap()).unwrap_or_else(|e| panic!("{path}: {e}"));
        match data {
            None => assert_eq!(m.kind, EntryKind::Directory, "{path}"),
            Some(d) => {
                assert_eq!(m.kind, EntryKind::File, "{path}");
                assert_eq!(m.size, d.len() as u64, "{path}");
                assert!(read_file(fs, &path) == *d, "{path}: content differs");
            }
        }
    }
    let root: Vec<String> = fs.read_dir(&VfsPath::root()).unwrap().into_iter().map(|e| e.name).collect();
    for want in ["empty.txt", "hello.txt", "big.bin", "dir", "many"] {
        assert!(root.iter().any(|n| n == want), "{want} in {root:?}");
    }
    assert_eq!(fs.read_dir(&VfsPath::parse("/many").unwrap()).unwrap().len(), 150);
}

#[test]
fn genisoimage_bridge_disc() {
    if !have("genisoimage") {
        eprintln!("genisoimage not installed: skipped");
        return;
    }
    let dir = scratch("geniso");
    let src = dir.join("src");
    let items = make_tree(&src);
    let iso = dir.join("disc.iso");
    let out = Command::new("genisoimage")
        .args(["-quiet", "-input-charset", "utf-8", "-udf", "-R", "-J", "-joliet-long", "-V", "MY_DISC", "-o"])
        .arg(&iso)
        .arg(&src)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let img = std::fs::read(&iso).unwrap();

    // A bridge disc: ISO 9660 is preferred, UDF can be chosen.
    let mut dev = MemoryDevice::new("disc.iso", img.clone());
    assert_eq!(UdfDriver.probe(&mut dev).unwrap(), 85);
    let (id, _) = mount(img.clone(), None);
    assert_eq!(id, "iso9660");

    let (id, mut fs) = mount(img, Some("udf"));
    assert_eq!(id, "udf");
    let info = fs.info();
    assert_eq!(info.label.as_deref(), Some("MY_DISC"));
    assert_eq!(info.block_size, Some(2048));
    assert!(info.details.iter().any(|(k, v)| k == "Format" && v == "UDF 1.02"), "{:?}", info.details);
    check_tree(fs.as_mut(), &items);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pycdlib_udf_260_with_symlinks() {
    let ok = Command::new("python3").args(["-c", "import pycdlib"]).output().is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("pycdlib not installed: skipped");
        return;
    }
    let dir = scratch("pycdlib");
    let iso = dir.join("udf.iso");
    let big = pattern(1_234_567, 11);
    std::fs::write(dir.join("big.bin"), &big).unwrap();
    let script = r#"
import sys, io, pycdlib
out, big = sys.argv[1], sys.argv[2]
iso = pycdlib.PyCdlib()
iso.new(udf='2.60', vol_ident='PYCD')
data = b'symlinked data\n'
iso.add_fp(io.BytesIO(data), len(data), '/TARGET.TXT;1', udf_path='/target.txt')
iso.add_directory('/DIR1', udf_path='/dir1')
iso.add_file(big, '/DIR1/BIG.BIN;1', udf_path='/dir1/big.bin')
iso.add_symlink(udf_symlink_path='/link', udf_target='target.txt')
iso.add_fp(io.BytesIO(b'u'), 1, '/UNI.TXT;1', udf_path='/Ünïcödé — 日本語.txt')
iso.add_symlink(udf_symlink_path='/dir1/up', udf_target='../target.txt')
iso.write(out)
iso.close()
"#;
    let out = Command::new("python3").args(["-c", script]).arg(&iso).arg(dir.join("big.bin")).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let (_, mut fs) = mount(std::fs::read(&iso).unwrap(), Some("udf"));
    // pycdlib writes UDF 2.60 structures but declares 1.02 (as udfinfo says).
    if have("udfinfo") {
        let rev = format!("UDF {}", udfinfo(&iso)["udfrev"]);
        assert!(fs.info().details.iter().any(|(k, v)| k == "Format" && *v == rev), "{:?}", fs.info().details);
    }
    assert_eq!(read_file(fs.as_mut(), "/target.txt"), b"symlinked data\n");
    assert_eq!(read_file(fs.as_mut(), "/Ünïcödé — 日本語.txt"), b"u");
    assert!(read_file(fs.as_mut(), "/dir1/big.bin") == big);
    let link = fs.metadata(&VfsPath::parse("/link").unwrap()).unwrap();
    assert_eq!(link.kind, EntryKind::Symlink);
    assert_eq!(link.symlink_target.as_deref(), Some("target.txt"));
    let up = fs.metadata(&VfsPath::parse("/dir1/up").unwrap()).unwrap();
    assert_eq!(up.symlink_target.as_deref(), Some("../target.txt"));
    let _ = std::fs::remove_dir_all(dir);
}

/// udfinfo's key=value lines.
fn udfinfo(path: &Path) -> HashMap<String, String> {
    let out = Command::new("udfinfo").arg(path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

#[test]
fn mkudffs_layouts() {
    if !have("mkudffs") || !have("udfinfo") {
        eprintln!("udftools not installed: skipped");
        return;
    }
    let dir = scratch("mkudffs");
    // (media type, revision, block size, size in MiB, partition maps)
    let cases: &[(&str, &str, u32, u64, &str)] = &[
        ("hd", "2.01", 512, 64, "plain"),
        ("hd", "2.01", 2048, 64, "plain"),
        ("hd", "2.01", 4096, 64, "plain"),
        ("hd", "1.02", 2048, 32, "plain"),
        ("hd", "1.50", 512, 32, "plain"),
        ("dvdram", "2.01", 2048, 64, "plain"),
        ("cdrw", "1.50", 2048, 64, "sparable"),
        ("cdrw", "2.01", 2048, 64, "sparable"),
        ("dvdrw", "2.01", 2048, 64, "sparable"),
        ("cdr", "1.50", 2048, 32, "plain, virtual (VAT)"),
        ("cdr", "2.01", 2048, 32, "plain, virtual (VAT)"),
        ("dvdr", "2.01", 2048, 32, "plain, virtual (VAT)"),
        // This mkudffs makes no metadata partitions (see
        // metadata_partition below).
        ("dvdr", "2.50", 2048, 32, "plain, virtual (VAT)"),
        ("cdr", "2.60", 2048, 32, "plain, virtual (VAT)"),
        ("bdr", "2.50", 2048, 64, "plain, virtual (VAT)"),
        ("bdr", "2.60", 2048, 64, "plain, virtual (VAT)"),
    ];
    for &(media, rev, bs, mib, maps) in cases {
        let what = format!("{media} UDF {rev} {bs}");
        let img = dir.join(format!("{media}-{rev}-{bs}.udf"));
        // Write-once media: mkudffs writes only the recorded part (the VAT
        // ends it), given the size in blocks; others fill a sized file.
        let write_once = matches!(media, "cdr" | "dvdr" | "bdr");
        let _ = std::fs::remove_file(&img);
        if !write_once {
            let f = std::fs::File::create(&img).unwrap();
            f.set_len(mib << 20).unwrap();
        }
        let blocks = ((mib << 20) / bs as u64).to_string();
        let out = Command::new("mkudffs")
            .args([
                format!("--media-type={media}"),
                format!("--udfrev={rev}"),
                format!("--blocksize={bs}"),
                "--label=LABEL1".into(),
                "--uid=1000".into(),
                "--gid=100".into(),
            ])
            .arg(&img)
            .args(write_once.then_some(blocks))
            .output()
            .unwrap();
        if !out.status.success() {
            panic!("{what}: mkudffs: {}", String::from_utf8_lossy(&out.stderr));
        }
        let want = udfinfo(&img);
        let data = std::fs::read(&img).unwrap();
        let mut dev = MemoryDevice::new("udf", data.clone());
        assert_eq!(UdfDriver.probe(&mut dev).unwrap(), 95, "{what}");
        let (id, mut fs) = mount(data, None);
        assert_eq!(id, "udf", "{what}");
        let info = fs.info();
        assert_eq!(info.label.as_deref(), Some("LABEL1"), "{what}");
        assert_eq!(info.block_size, Some(bs), "{what}");
        let detail = |k: &str| info.details.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        // mkudffs writes 2.50 structures for 2.60 (udfinfo says 2.50; the
        // logical volume declares 2.60).
        let rev = if rev == "2.60" { "2.60".to_owned() } else { want["udfrev"].clone() };
        assert_eq!(detail("Format"), Some(format!("UDF {rev}")), "{what}");
        assert_eq!(detail("Partition maps").as_deref(), Some(maps), "{what}");
        if let (Some(free), Ok(n)) = (info.free_bytes, want["freeblocks"].parse::<u64>()) {
            assert_eq!(free, n * bs as u64, "{what}: free space");
        }
        // The root: an empty folder owned as asked.
        let root = fs.metadata(&VfsPath::root()).unwrap();
        assert_eq!(root.kind, EntryKind::Directory, "{what}");
        assert_eq!((root.uid, root.gid), (Some(1000), Some(100)), "{what}");
        // UDF 1.50 rewritable media: a hidden system file for the space
        // that cannot be used.
        let entries = fs.read_dir(&VfsPath::root()).unwrap();
        assert!(entries.iter().all(|e| e.name == "Non-Allocatable Space" && e.metadata.hidden), "{what}: {entries:?}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn names_and_times() {
    assert_eq!(cs0(&[8, b'a', b'b']), "ab");
    assert_eq!(cs0(&[8, 0xe9, b't', 0xe9]), "été", "Latin-1");
    assert_eq!(cs0(&[16, 0, b'a', 0x30, 0x42]), "a\u{3042}");
    assert_eq!(cs0(&[16, 0xd8, 0x3d, 0xde, 0x00]), "\u{1f600}");
    let mut f = [0u8; 32];
    f[..4].copy_from_slice(&[8, b'X', b'Y', b'Z']);
    f[31] = 4;
    assert_eq!(dstring(&f), "XYZ");
    // 2020-01-02 03:04:05 at UTC+2 (type 1, +120 minutes).
    let mut t = [0u8; 12];
    t[..2].copy_from_slice(&(0x1000u16 | 120).to_le_bytes());
    t[2..4].copy_from_slice(&2020u16.to_le_bytes());
    t[4..9].copy_from_slice(&[1, 2, 3, 4, 5]);
    assert_eq!(timestamp(&t), Some(1_577_934_245 - 7200));
    // Symbolic link components: root, "a", parent, "b".
    let comps = [2, 0, 0, 0, 5, 2, 0, 0, 8, b'a', 3, 0, 0, 0, 5, 2, 0, 0, 8, b'b'];
    assert_eq!(symlink(&comps), "/a/../b");
}

/// Sets a descriptor tag's checksum.
fn seal(b: &mut [u8]) {
    b[4] = 0;
    b[4] = b[..16].iter().fold(0u8, |s, &x| s.wrapping_add(x));
}

/// A metadata partition (UDF 2.50+, Blu-ray), made from a plain mkudffs
/// volume: the logical volume gets a metadata partition map, the file set
/// descriptor and root directory move into a metadata file stored
/// elsewhere in the partition (the originals are zeroed, so a wrong
/// mapping fails), and the root is reached through it. With the main
/// metadata file unreadable, its mirror is used.
#[test]
fn metadata_partition() {
    if !have("mkudffs") {
        eprintln!("udftools not installed: skipped");
        return;
    }
    let dir = scratch("metadata");
    let path = dir.join("plain.udf");
    std::fs::File::create(&path).unwrap().set_len(32 << 20).unwrap();
    let out = Command::new("mkudffs")
        .args(["--media-type=hd", "--blocksize=2048", "--label=META", "--uid=7", "--gid=8"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let plain = std::fs::read(&path).unwrap();
    let bs = 2048usize;
    let blk = |img: &[u8], n: u64| img[n as usize * bs..(n as usize + 1) * bs].to_vec();

    for broken_main in [false, true] {
        let mut img = plain.clone();
        let anchor = blk(&img, 256);
        let mvds = le32(&anchor, 20) as u64;
        // The descriptors in the main sequence.
        let (mut lvd_at, mut pd) = (0, None);
        for n in mvds..mvds + 16 {
            let b = blk(&img, n);
            match tag(&b) {
                Some(TAG_LVD) => lvd_at = n,
                Some(TAG_PD) => pd = Some(b),
                _ => {}
            }
        }
        let pd = pd.unwrap();
        let (number, start, len) = (le16(&pd, 22), le32(&pd, 188) as u64, le32(&pd, 192) as u64);
        let mut lvd = blk(&img, lvd_at);
        let fsd_lb = le32(&lvd, 252) as u64;
        let fsd = blk(&img, start + fsd_lb);
        let root_lb = le32(&fsd, 404) as u64;
        // The blocks that move: the file set descriptor, the root's entry
        // and (if not embedded) its directory data.
        let k = 64u64;
        assert!(fsd_lb < k && root_lb < k, "{fsd_lb} {root_lb}");
        let moved_to = len - k - 16;
        let (main_at, mirror_at) = (moved_to + k, moved_to + k + 1);

        // Metadata block n = partition block moved_to + n.
        for n in 0..k {
            let src = ((start + n) as usize) * bs;
            let dst = ((start + moved_to + n) as usize) * bs;
            img.copy_within(src..src + bs, dst);
            img[src..src + bs].fill(0);
        }
        // The metadata file and its mirror: file entries with one extent.
        for (at, ftype) in [(main_at, FT_METADATA), (mirror_at, FT_METADATA_MIRROR)] {
            let mut fe = vec![0u8; bs];
            fe[0..2].copy_from_slice(&TAG_FE.to_le_bytes());
            fe[2..4].copy_from_slice(&3u16.to_le_bytes());
            fe[12..16].copy_from_slice(&(at as u32).to_le_bytes());
            fe[16 + 4..16 + 6].copy_from_slice(&4u16.to_le_bytes()); // strategy 4
            fe[16 + 11] = ftype;
            fe[36..48].copy_from_slice(&[0xff; 12]); // uid, gid: none
            fe[56..64].copy_from_slice(&(k * bs as u64).to_le_bytes());
            fe[172..176].copy_from_slice(&8u32.to_le_bytes());
            fe[176..180].copy_from_slice(&((k * bs as u64) as u32).to_le_bytes());
            fe[180..184].copy_from_slice(&(moved_to as u32).to_le_bytes());
            seal(&mut fe);
            let at = ((start + at) as usize) * bs;
            img[at..at + bs].copy_from_slice(&fe);
        }
        // The partition map: metadata file at main_at (or at a zeroed
        // block, so the mirror is used).
        let table_len = le32(&lvd, 264) as usize;
        let mut map = vec![0u8; 64];
        map[0] = 2;
        map[1] = 64;
        map[5..5 + 23].copy_from_slice(b"*UDF Metadata Partition");
        map[28..30].copy_from_slice(&0x0250u16.to_le_bytes());
        map[36..38].copy_from_slice(&1u16.to_le_bytes());
        map[38..40].copy_from_slice(&number.to_le_bytes());
        let main = if broken_main { 0 } else { main_at as u32 };
        map[40..44].copy_from_slice(&main.to_le_bytes());
        map[44..48].copy_from_slice(&(mirror_at as u32).to_le_bytes());
        map[48..52].copy_from_slice(&u32::MAX.to_le_bytes());
        map[52..56].copy_from_slice(&32u32.to_le_bytes());
        map[56..58].copy_from_slice(&1u16.to_le_bytes());
        lvd[440 + table_len..440 + table_len + 64].copy_from_slice(&map);
        lvd[264..268].copy_from_slice(&((table_len + 64) as u32).to_le_bytes());
        lvd[268..272].copy_from_slice(&2u32.to_le_bytes());
        lvd[240..242].copy_from_slice(&0x0250u16.to_le_bytes());
        // The file set descriptor is in the metadata partition (reference 1).
        lvd[256..258].copy_from_slice(&1u16.to_le_bytes());
        seal(&mut lvd);
        let at = lvd_at as usize * bs;
        img[at..at + bs].copy_from_slice(&lvd);
        // ... and so is the root.
        let fsd_at = ((start + moved_to + fsd_lb) as usize) * bs;
        img[fsd_at + 408..fsd_at + 410].copy_from_slice(&1u16.to_le_bytes());
        seal(&mut img[fsd_at..fsd_at + bs]);

        let what = if broken_main { "mirror" } else { "main" };
        let (id, mut fs) = mount(img, None);
        assert_eq!(id, "udf");
        let info = fs.info();
        assert_eq!(info.label.as_deref(), Some("META"), "{what}");
        let detail = |k: &str| info.details.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(detail("Partition maps").as_deref(), Some("plain, metadata"), "{what}");
        assert_eq!(detail("Format").as_deref(), Some("UDF 2.50"), "{what}");
        let root = fs.metadata(&VfsPath::root()).unwrap();
        assert_eq!(root.kind, EntryKind::Directory, "{what}");
        assert_eq!((root.uid, root.gid), (Some(7), Some(8)), "{what}");
        // UDF 1.50 rewritable media: a hidden system file for the space
        // that cannot be used.
        let entries = fs.read_dir(&VfsPath::root()).unwrap();
        assert!(entries.iter().all(|e| e.name == "Non-Allocatable Space" && e.metadata.hidden), "{what}: {entries:?}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(feature = "iso9660")]
/// What 7-Zip extracts from an image's UDF side: path → content (None for
/// a folder).
fn extracted_7z(dir: &Path, img: &Path) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
    let out = dir.join("out-7z");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    let st =
        Command::new("7z").args(["x", "-y", "-tudf"]).arg(format!("-o{}", out.display())).arg(img).output().unwrap();
    assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
    let mut t = std::collections::BTreeMap::new();
    let mut stack = vec![out.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let rel = e.path().strip_prefix(&out).unwrap().to_string_lossy().into_owned();
            if e.file_type().unwrap().is_dir() {
                t.insert(rel, None);
                stack.push(e.path());
            } else {
                t.insert(rel, Some(std::fs::read(e.path()).unwrap()));
            }
        }
    }
    t
}

#[cfg(feature = "iso9660")]
fn want_of(items: &[(String, Option<Vec<u8>>)]) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
    items.iter().cloned().collect()
}

#[cfg(feature = "iso9660")]
fn open_rw(img: &Path) -> dix_core::Mounted {
    let dev = dix_core::device::host::FileDevice::open(img, AccessMode::ReadWrite).unwrap();
    crate::default_registry().mount(Box::new(dev), None, AccessMode::ReadWrite).unwrap()
}

/// *New image → UDF*: a UDF-only image, filled, changed and read back by
/// us, 7-Zip and udfinfo. Rewritable volumes and bridge images open
/// read-only through this driver.
#[test]
#[cfg(feature = "iso9660")]
fn makes_and_changes_udf_only_images() {
    if !have("7z") || !have("udfinfo") || !have("mkudffs") {
        eprintln!("7z or udftools not installed: skipped");
        return;
    }
    let dir = scratch("udf-only");
    let img = dir.join("new.iso");
    {
        let mut dev = dix_core::device::host::FileDevice::create(&img, 0).unwrap();
        let opts = dix_core::FormatOptions { label: Some("Only UDF".into()), variant: Some("udf".into()) };
        crate::default_registry().format(&mut dev, "udf", &opts).unwrap();
    }
    let info = udfinfo(&img);
    assert_eq!(info["label"], "Only UDF");
    let data = std::fs::read(&img).unwrap();
    let mut dev = MemoryDevice::new("udf", data);
    assert_eq!(UdfDriver.probe(&mut dev).unwrap(), 95);
    assert_eq!(crate::iso9660::IsoDriver.probe(&mut dev).unwrap(), 0, "no ISO 9660");

    // Filled (large files written in pieces, as the app copies).
    let src = dir.join("src");
    let items = make_tree(&src);
    {
        let mut m = open_rw(&img);
        assert_eq!(m.driver.info().id, "udf");
        assert_eq!(m.fs.access_mode(), AccessMode::ReadWrite);
        for (p, d) in &items {
            let vp = VfsPath::parse(&format!("/{p}")).unwrap();
            match d {
                None => m.fs.create_dir(&vp).unwrap(),
                Some(d) => {
                    m.fs.create_file(&vp).unwrap();
                    for (i, c) in d.chunks(1 << 20).enumerate() {
                        m.fs.write_at(&vp, (i << 20) as u64, c).unwrap();
                    }
                }
            }
        }
        m.fs.unmount().unwrap();
    }
    let mut want = want_of(&items);
    assert_eq!(extracted_7z(&dir, &img), want, "7-Zip");
    let (_, mut fs) = mount(std::fs::read(&img).unwrap(), None);
    check_tree(fs.as_mut(), &items);
    let info = udfinfo(&img);
    let files = items.iter().filter(|(_, d)| d.is_some()).count();
    let dirs = items.len() - files + 1;
    assert_eq!((info["numfiles"].parse::<usize>().unwrap(), info["numdirs"].parse::<usize>().unwrap()), (files, dirs));
    let mut dev = MemoryDevice::new("udf", std::fs::read(&img).unwrap());
    assert_eq!(crate::iso9660::IsoDriver.probe(&mut dev).unwrap(), 0, "still no ISO 9660");

    // Changed: renamed, deleted, replaced.
    {
        let mut m = open_rw(&img);
        m.fs.rename(&VfsPath::parse("/dir").unwrap(), &VfsPath::parse("/renamed").unwrap()).unwrap();
        m.fs.remove_file(&VfsPath::parse("/big.bin").unwrap()).unwrap();
        m.fs.write_file(&VfsPath::parse("/hello.txt").unwrap(), b"replaced").unwrap();
        m.fs.unmount().unwrap();
    }
    let renamed: Vec<(String, Option<Vec<u8>>)> =
        want.iter().filter(|(k, _)| k.starts_with("dir")).map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in renamed {
        want.remove(&k);
        want.insert(format!("renamed{}", &k[3..]), v);
    }
    want.remove("big.bin");
    want.insert("hello.txt".into(), Some(b"replaced".to_vec()));
    assert_eq!(extracted_7z(&dir, &img), want, "7-Zip, changed");

    // A rewritable volume (mkudffs) and a bridge image: read-only here.
    let hd = dir.join("hd.udf");
    std::fs::File::create(&hd).unwrap().set_len(16 << 20).unwrap();
    assert!(Command::new("mkudffs").arg("--media-type=hd").arg(&hd).output().unwrap().status.success());
    let m = open_rw(&hd);
    assert_eq!(m.fs.access_mode(), AccessMode::ReadOnly);
    let bridge = dir.join("bridge.iso");
    {
        let mut dev = dix_core::device::host::FileDevice::create(&bridge, 0).unwrap();
        let opts = dix_core::FormatOptions { label: None, variant: Some("iso-udf".into()) };
        crate::default_registry().format(&mut dev, "iso9660", &opts).unwrap();
    }
    let dev = dix_core::device::host::FileDevice::open(&bridge, AccessMode::ReadWrite).unwrap();
    let m = crate::default_registry().mount(Box::new(dev), Some("udf"), AccessMode::ReadWrite).unwrap();
    assert_eq!(m.fs.access_mode(), AccessMode::ReadOnly);
    let _ = std::fs::remove_dir_all(dir);
}
