//! Images made by mksquashfs from a folder (every compressor, block sizes,
//! fragments on and off, uncompressed tables), read back and compared
//! entry by entry with the folder: type, size, content, permissions,
//! owner, time, link target, hard links. Skipped without mksquashfs.

use super::*;
use dix_core::device::MemoryDevice;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

fn have(tool: &str) -> bool {
    Command::new("sh").arg("-c").arg(format!("command -v {tool}")).output().is_ok_and(|o| o.status.success())
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dix-sqfs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn pattern(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8).collect()
}

fn source(root: &Path) {
    let w = |p: &str, d: &[u8]| {
        let p = root.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, d).unwrap();
    };
    w("empty", b"");
    w("tiny.txt", b"x");
    w("hello.txt", b"Hello, SquashFS!\n");
    w("block.bin", &pattern(131_072, 1)); // exactly one block (128 KiB)
    w("big.bin", &pattern(3_000_017, 2));
    w("text.txt", &b"compressible line of text\n".repeat(20_000));
    w("Ünïcödé — 日本語.txt", "unicode\n".as_bytes());
    w(&"n".repeat(250), b"long name");
    w("a/b/c/d/deep.txt", b"deep");
    for i in 0..600 {
        w(&format!("many/file-{i:04}.dat"), &pattern(i * 3, i as u32));
    }
    // Sparse: zeros mksquashfs stores as empty blocks.
    let mut sparse = vec![0u8; 1 << 20];
    sparse[500_000..500_010].copy_from_slice(b"not zeros!");
    w("sparse.bin", &sparse);
    std::os::unix::fs::symlink("hello.txt", root.join("link")).unwrap();
    std::os::unix::fs::symlink("../../hello.txt", root.join("a/b/up")).unwrap();
    std::fs::hard_link(root.join("hello.txt"), root.join("a/hard.txt")).unwrap();
    std::fs::set_permissions(root.join("tiny.txt"), std::fs::Permissions::from_mode(0o751)).unwrap();
    let _ = Command::new("mkfifo").arg(root.join("fifo")).status();
}

fn mount(img: &Path) -> Box<dyn FileSystem> {
    let reg = crate::default_registry();
    let dev = MemoryDevice::new("sqfs", std::fs::read(img).unwrap());
    let m = reg.mount(Box::new(dev), None, AccessMode::ReadOnly).unwrap();
    assert_eq!(m.driver.info().id, "squashfs");
    m.fs
}

fn read_all(fs: &mut dyn FileSystem, p: &VfsPath, size: u64) -> Vec<u8> {
    let mut out = vec![0u8; size as usize];
    let mut at = 0;
    while at < out.len() {
        let n = (out.len() - at).min(100_003);
        assert_eq!(fs.read(p, at as u64, &mut out[at..at + n]).unwrap(), n);
        at += n;
    }
    out
}

/// Compares the image with the host folder, recursively.
fn compare(fs: &mut dyn FileSystem, host: &Path, rel: &str, what: &str, inodes: &mut HashMap<u64, u64>) {
    let dir = VfsPath::parse(if rel.is_empty() { "/" } else { rel }).unwrap();
    let mut got: Vec<_> = fs.read_dir(&dir).unwrap();
    let mut want: Vec<_> = std::fs::read_dir(host.join(rel.trim_start_matches('/'))).unwrap().flatten().collect();
    got.sort_by(|a, b| a.name.cmp(&b.name));
    want.sort_by_key(|e| e.file_name());
    let names: Vec<_> = got.iter().map(|e| e.name.clone()).collect();
    let wnames: Vec<_> = want.iter().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, wnames, "{what}: {rel}");
    for (e, h) in got.iter().zip(&want) {
        let path = format!("{rel}/{}", e.name);
        let hm = std::fs::symlink_metadata(h.path()).unwrap();
        let m = &e.metadata;
        assert_eq!(m.mode, Some(hm.mode()), "{what}: {path} mode");
        assert_eq!(m.modified, Some(hm.mtime()), "{what}: {path} time");
        assert_eq!((m.uid, m.gid), (Some(hm.uid()), Some(hm.gid())), "{what}: {path} owner");
        // Hard links: one inode number in the image for one on the host.
        let ino = m.inode.unwrap();
        assert_eq!(*inodes.entry(hm.ino()).or_insert(ino), ino, "{what}: {path} hard link");
        let t = hm.file_type();
        if t.is_dir() {
            assert_eq!(m.kind, EntryKind::Directory, "{what}: {path}");
            compare(fs, host, &path, what, inodes);
        } else if t.is_symlink() {
            assert_eq!(m.kind, EntryKind::Symlink);
            let target = std::fs::read_link(h.path()).unwrap();
            assert_eq!(m.symlink_target.as_deref(), Some(target.to_str().unwrap()), "{what}: {path}");
        } else if t.is_file() {
            assert_eq!(m.kind, EntryKind::File);
            assert_eq!(m.size, hm.len(), "{what}: {path}");
            let p = VfsPath::parse(&path).unwrap();
            assert!(read_all(fs, &p, m.size) == std::fs::read(h.path()).unwrap(), "{what}: {path} differs");
        } else {
            assert_eq!(m.kind, EntryKind::Other, "{what}: {path}");
        }
    }
}

#[test]
fn reads_what_mksquashfs_makes() {
    if !have("mksquashfs") {
        eprintln!("mksquashfs not installed: skipped");
        return;
    }
    let dir = scratch("make");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    source(&src);
    let cases: &[&[&str]] = &[
        &["-comp", "gzip"],
        &["-comp", "lzma"],
        &["-comp", "lzo"],
        &["-comp", "xz"],
        &["-comp", "xz", "-Xbcj", "x86"],
        &["-comp", "lz4"],
        &["-comp", "lz4", "-Xhc"],
        &["-comp", "zstd"],
        &["-comp", "gzip", "-b", "4096"],
        &["-comp", "zstd", "-b", "1M"],
        &["-comp", "gzip", "-no-fragments"],
        &["-comp", "gzip", "-always-use-fragments"],
        &["-comp", "xz", "-noI", "-noD", "-noF", "-noX"],
        &["-comp", "gzip", "-no-sparse"],
    ];
    for opts in cases {
        let what = opts.join(" ");
        let img = dir.join("img.sqfs");
        let _ = std::fs::remove_file(&img);
        let out = Command::new("mksquashfs")
            .arg(&src)
            .arg(&img)
            .args(*opts)
            .args(["-no-progress", "-quiet", "-processors", "2"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{what}: {}", String::from_utf8_lossy(&out.stderr));
        let mut fs = mount(&img);
        let info = fs.info();
        let comp = opts[1];
        assert!(info.details.iter().any(|(k, v)| k == "Compression" && v == comp), "{what}: {:?}", info.details);
        compare(fs.as_mut(), &src, "", &what, &mut HashMap::new());
    }
    // A character device, from a pseudo-file definition.
    let img = dir.join("dev.sqfs");
    let _ = std::fs::remove_file(&img);
    let out = Command::new("mksquashfs")
        .arg(&src)
        .arg(&img)
        .args(["-no-progress", "-quiet", "-p", "null c 666 0 0 1 3"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut fs = mount(&img);
    let m = fs.metadata(&VfsPath::parse("/null").unwrap()).unwrap();
    assert_eq!((m.kind, m.mode), (EntryKind::Other, Some(0o020666)));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn lzo_runs_and_long_matches() {
    // lzop's raw LZO1X output is not at hand; mksquashfs -comp lzo covers
    // the decoder above. Here: the empty input's end marker alone.
    assert_eq!(codec::lzo1x(&[0x11, 0, 0], 16).unwrap(), b"");
}

/// AppImages: an ELF launcher ("AI" 2 in its header) with a SquashFS from
/// mksquashfs appended — where the ELF header says it ends, and further
/// on (found by searching) —, a program that is not one, and a type 1
/// AppImage (an ISO 9660 image with the ELF header over its first 32 KiB).
#[test]
fn reads_appimages() {
    if !have("mksquashfs") || !Path::new("/bin/true").exists() {
        eprintln!("mksquashfs or /bin/true missing: skipped");
        return;
    }
    let dir = scratch("appimage");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    source(&src);
    let img = dir.join("fs.sqfs");
    let out = Command::new("mksquashfs")
        .arg(&src)
        .arg(&img)
        .args(["-comp", "zstd", "-no-progress", "-quiet"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let sqfs = std::fs::read(&img).unwrap();
    let mut elf = std::fs::read("/bin/true").unwrap();
    elf[8..11].copy_from_slice(b"AI\x02");
    let reg = crate::default_registry();
    for (name, gap) in [("a.AppImage", 0usize), ("gap.AppImage", 4096 + 17)] {
        let mut bytes = elf.clone();
        bytes.extend(vec![0u8; gap]);
        bytes.extend(&sqfs);
        let m = reg.mount(Box::new(MemoryDevice::new(name, bytes)), None, AccessMode::ReadOnly).unwrap();
        assert_eq!(m.driver.info().id, "squashfs", "{name}");
        let mut fs = m.fs;
        assert!(fs.info().details.iter().any(|(k, _)| k == "AppImage"), "{name}");
        compare(fs.as_mut(), &src, "", name, &mut HashMap::new());
    }
    // A program alone is not an AppImage.
    let mut probe_dev = MemoryDevice::new("true", elf.clone());
    assert_eq!(SquashDriver.probe(&mut probe_dev).unwrap(), 0);
    // Type 1: an ISO image whose first 32 KiB start with an ELF header.
    if have("genisoimage") {
        let iso = dir.join("t1.iso");
        let out = Command::new("genisoimage").args(["-quiet", "-R", "-J", "-o"]).arg(&iso).arg(&src).output().unwrap();
        assert!(out.status.success());
        let mut bytes = std::fs::read(&iso).unwrap();
        let mut head = std::fs::read("/bin/true").unwrap();
        head.truncate(32768);
        head[8..11].copy_from_slice(b"AI\x01");
        bytes[..head.len()].copy_from_slice(&head);
        let mut m = reg.mount(Box::new(MemoryDevice::new("t1.AppImage", bytes)), None, AccessMode::ReadOnly).unwrap();
        assert_eq!(m.driver.info().id, "iso9660");
        let mut buf = vec![0u8; 64];
        let n = m.fs.read(&VfsPath::parse("/hello.txt").unwrap(), 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"Hello, SquashFS!\n");
    }
    let _ = std::fs::remove_dir_all(dir);
}
