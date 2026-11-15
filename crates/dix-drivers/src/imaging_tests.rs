//! Copying only the used space: a disk filled with junk, then partitioned
//! and formatted by the file systems' own tools (FAT32, ext4, NTFS,
//! exFAT), with files in each. The copy leaves out what each marks free —
//! junk there reads as zeros in the copy — and every file system in it
//! passes its own fsck and holds the same files. Skipped without the tools.

#![cfg_attr(not(all(feature = "fat", feature = "exfat", feature = "ext4", feature = "ntfs")), allow(dead_code))]

use super::used_space;
use dix_core::device::MemoryDevice;
use dix_core::{AccessMode, VfsPath};
use std::process::Command;

fn have(tools: &[&str]) -> bool {
    tools
        .iter()
        .all(|t| Command::new("sh").arg("-c").arg(format!("command -v {t}")).output().is_ok_and(|o| o.status.success()))
}

fn run(cmd: &str, args: &[&str]) {
    let out = Command::new(cmd).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{cmd} {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

const MIB: u64 = 1 << 20;

fn junk(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// Files for each file system: (name, contents).
fn files(seed: u32) -> Vec<(String, Vec<u8>)> {
    vec![
        ("small.txt".into(), b"hello from the used space\n".to_vec()),
        ("big.bin".into(), junk(3 * MIB as usize + 777, seed)),
        ("medium.dat".into(), junk(300_000, seed + 1)),
    ]
}

fn read_all(dev: Vec<u8>) -> Vec<(String, Vec<u8>)> {
    let reg = crate::default_registry();
    let mut m = reg.mount(Box::new(MemoryDevice::new("p", dev)), None, AccessMode::ReadOnly).unwrap();
    let mut out = Vec::new();
    let root = VfsPath::root();
    let mut names: Vec<String> = m.fs.read_dir(&root).unwrap().into_iter().map(|e| e.name).collect();
    names.retain(|n| !n.starts_with('$') && n != "lost+found" && n != "System Volume Information");
    names.sort();
    for n in names {
        let data = m.fs.read_to_vec(&root.join(&n).unwrap()).unwrap();
        out.push((n, data));
    }
    out
}

#[cfg(all(feature = "fat", feature = "exfat", feature = "ext4", feature = "ntfs"))]
#[test]
fn copies_only_used_space() {
    if !have(&[
        "sgdisk",
        "mkfs.fat",
        "mcopy",
        "fsck.fat",
        "mke2fs",
        "e2fsck",
        "dumpe2fs",
        "mkntfs",
        "ntfscp",
        "ntfsfix",
        "mkfs.exfat",
        "fsck.exfat",
    ]) {
        eprintln!("sgdisk / mkfs / fsck tools missing: skipped");
        return;
    }
    let dir = std::env::temp_dir().join(format!("dix-used-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let parts: [(&str, u64); 4] = [("fat32", 48), ("ext4", 48), ("ntfs", 48), ("exfat", 32)];
    // Junk everywhere, as on a used disk.
    let mut disk = junk(200 << 20, 99);
    let mut at = MIB;
    let mut placed = Vec::new();
    for (i, (fs, mib)) in parts.iter().enumerate() {
        let img = dir.join(format!("{fs}.img"));
        std::fs::write(&img, junk((mib * MIB) as usize, i as u32 + 7)).unwrap();
        let p = img.to_str().unwrap();
        let src = dir.join(format!("{fs}-files"));
        std::fs::create_dir_all(&src).unwrap();
        for (n, data) in files(i as u32 * 10 + 1) {
            std::fs::write(src.join(&n), data).unwrap();
        }
        match *fs {
            "fat32" => {
                run("mkfs.fat", &["-F", "32", p]);
                for (n, _) in files(1) {
                    run("mcopy", &["-i", p, src.join(&n).to_str().unwrap(), &format!("::/{n}")]);
                }
            }
            "ext4" => run(
                "mke2fs",
                &["-q", "-F", "-t", "ext4", "-b", "1024", "-E", "nodiscard", "-d", src.to_str().unwrap(), p],
            ),
            "ntfs" => {
                run("mkntfs", &["-F", "-f", "-q", p]);
                for (n, _) in files(1) {
                    run("ntfscp", &["-f", p, src.join(&n).to_str().unwrap(), &n]);
                }
            }
            _ => {
                run("mkfs.exfat", &[p]);
                // Written with our own driver (no tool writes exFAT without FUSE).
                let dev = MemoryDevice::new("x", std::fs::read(&img).unwrap());
                let buf = dev.buffer();
                let reg = crate::default_registry();
                let mut m = reg.mount(Box::new(dev), None, AccessMode::ReadWrite).unwrap();
                for (n, data) in files(i as u32 * 10 + 1) {
                    m.fs.write_file(&VfsPath::root().join(&n).unwrap(), &data).unwrap();
                }
                m.fs.unmount().unwrap();
                drop(m);
                std::fs::write(&img, buf.snapshot()).unwrap();
            }
        }
        let data = std::fs::read(&img).unwrap();
        if *fs == "ext4" {
            // Exactly the free blocks e2fsprogs sees (also in groups whose
            // bitmap is not initialised: their backup superblocks stay).
            let out = Command::new("dumpe2fs").arg(p).output().unwrap();
            let mut want = Vec::new();
            for l in String::from_utf8_lossy(&out.stdout).lines() {
                let Some(list) = l.strip_prefix("  Free blocks: ") else { continue };
                for r in list.split(", ").filter(|r| !r.is_empty()) {
                    let (a, b) = r.split_once('-').unwrap_or((r, r));
                    let (a, b): (u64, u64) = (a.parse().unwrap(), b.parse().unwrap());
                    want.push((a * 1024, (b + 1) * 1024));
                }
            }
            assert!(!want.is_empty());
            let got = crate::ext4::fsck::free_space(Box::new(MemoryDevice::new("e", data.clone()))).unwrap().unwrap();
            assert_eq!(dix_core::imaging::merge_ranges(got), dix_core::imaging::merge_ranges(want));
        }
        disk[at as usize..(at + mib * MIB) as usize].copy_from_slice(&data);
        placed.push((at, mib * MIB, i as u32 * 10 + 1));
        at += mib * MIB + MIB;
    }
    let disk_path = dir.join("disk.img");
    std::fs::write(&disk_path, &disk).unwrap();
    let mut args = vec!["-o".to_owned()];
    for (i, (start, size, _)) in placed.iter().enumerate() {
        args.push(format!("-n{}:{}:{}", i + 1, start / 512, (start + size) / 512 - 1));
    }
    args.push(disk_path.to_str().unwrap().to_owned());
    run("sgdisk", &args.iter().map(String::as_str).collect::<Vec<_>>());
    let disk = std::fs::read(&disk_path).unwrap();

    let used = used_space(Box::new(MemoryDevice::new("disk", disk.clone()))).unwrap();
    let kinds: Vec<_> = used.areas.iter().map(|a| a.kind).collect();
    assert_eq!(kinds, vec![Some("FAT32"), Some("ext2/3/4"), Some("NTFS"), Some("exFAT")], "{:?}", used.areas);
    for a in &used.areas {
        assert!(a.free > a.size / 3, "{a:?}");
        assert!(a.free < a.size - 3 * MIB, "{a:?}");
    }
    assert!(used.used < disk.len() as u64 / 2, "{} of {}", used.used, disk.len());

    // The copy.
    let mut src = MemoryDevice::new("disk", disk.clone());
    let out = MemoryDevice::new("copy", Vec::new()).with_resizable(true);
    let buf = out.buffer();
    dix_core::container::convert_ranges(&mut src, Box::new(out), None, "copy", &[], Some(&used.ranges), &mut |_, _| {
        true
    })
    .unwrap();
    let copy = buf.snapshot();
    assert_eq!(copy.len(), disk.len());
    for &(a, b) in &used.ranges {
        assert!(copy[a as usize..b as usize] == disk[a as usize..b as usize]);
    }
    assert!(
        dix_core::imaging::complement(&used.ranges, disk.len() as u64)
            .iter()
            .all(|&(a, b)| copy[a as usize..b as usize].iter().all(|&x| x == 0))
    );

    // Each file system in the copy: its own fsck, and the same files.
    for (i, ((fs, _), (start, size, seed))) in parts.iter().zip(&placed).enumerate() {
        let bytes = copy[*start as usize..(start + size) as usize].to_vec();
        let p = dir.join(format!("copy-{i}.img"));
        std::fs::write(&p, &bytes).unwrap();
        let p = p.to_str().unwrap();
        match *fs {
            "fat32" => run("fsck.fat", &["-n", p]),
            "ext4" => run("e2fsck", &["-fn", p]),
            "ntfs" => run("ntfsfix", &["-n", p]),
            _ => run("fsck.exfat", &["-n", p]),
        }
        let mut want = files(*seed);
        want.sort();
        assert_eq!(read_all(bytes), want, "{fs}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_disk_without_a_table_and_unknown_space() {
    // No partition table, no file system: all of it is copied.
    let dev = MemoryDevice::new("d", vec![7u8; 4 * MIB as usize]);
    let used = used_space(Box::new(dev)).unwrap();
    assert_eq!(used.ranges, vec![(0, 4 * MIB)]);
    assert_eq!(used.areas.len(), 1);
    assert_eq!(used.areas[0].kind, None);
}

/// A compressed image (made by gzip — two members —, xz, zstd and bzip2)
/// unpacked as it streams onto a disk, then checked against the disk.
#[cfg(all(feature = "archive-xz", feature = "archive-zstd", feature = "archive-bzip2"))]
#[test]
fn writes_compressed_images_onto_a_disk() {
    use crate::archive::Compression;
    use dix_core::imaging::{DiskVerifier, DiskWriter};
    if !have(&["gzip", "xz", "zstd", "bzip2"]) {
        eprintln!("compressors missing: skipped");
        return;
    }
    let dir = std::env::temp_dir().join(format!("dix-unpack-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut image = junk(5 * MIB as usize + 12345, 3);
    image[MIB as usize..3 * MIB as usize].fill(0);
    let raw = dir.join("img");
    std::fs::write(&raw, &image).unwrap();
    let pack = |cmd: &str, args: &[&str]| -> Vec<u8> {
        let out = Command::new(cmd).args(args).arg("-c").arg(&raw).output().unwrap();
        assert!(out.status.success());
        out.stdout
    };
    let half = image.len() / 2;
    std::fs::write(dir.join("a"), &image[..half]).unwrap();
    std::fs::write(dir.join("b"), &image[half..]).unwrap();
    let mut gz = Command::new("gzip").arg("-c").arg(dir.join("a")).output().unwrap().stdout;
    gz.extend(Command::new("gzip").arg("-c").arg(dir.join("b")).output().unwrap().stdout);
    let cases = [
        (Compression::Gzip, gz),
        (Compression::Xz, pack("xz", &["-T1"])),
        (Compression::Zstd, pack("zstd", &["-q"])),
        (Compression::Bzip2, pack("bzip2", &[])),
    ];
    for (c, packed) in cases {
        assert_eq!(Compression::detect(&packed), c);
        let mut disk = MemoryDevice::new("disk", vec![0x55; 8 * MIB as usize]);
        let buf = disk.buffer();
        let mut ok = |_| true;
        let mut w = DiskWriter::new(&mut disk, &mut ok);
        c.unpack(&mut &packed[..], &mut w).unwrap();
        assert_eq!(w.finish().unwrap(), image.len() as u64, "{c:?}");
        assert_eq!(&buf.snapshot()[..image.len()], &image[..], "{c:?}");
        let mut v = DiskVerifier::new(&mut disk, &mut ok);
        c.unpack(&mut &packed[..], &mut v).unwrap();
        assert_eq!(v.finish().unwrap(), image.len() as u64);
        // Cut short: an error, not a short image passed as whole.
        let cut = &packed[..packed.len() * 2 / 3];
        let mut sink = Vec::new();
        assert!(c.unpack(&mut &cut[..], &mut sink).is_err(), "{c:?}");
    }
    // A disk too small: the sink's error, as it is.
    let packed = pack("xz", &["-T1"]);
    let mut small = MemoryDevice::zeroed("small", 2 * MIB as usize);
    let mut ok = |_| true;
    let mut w = DiskWriter::new(&mut small, &mut ok);
    let e = Compression::Xz.unpack(&mut &packed[..], &mut w).unwrap_err();
    assert!(e.to_string().contains("larger than the disk"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}
