//! XFS made by mkfs.xfs in several variants (v5, 1 KiB blocks with 8 KiB
//! folder blocks, 32-bit timestamps, v4 without file types), filled by the
//! kernel through a mount — every folder form (short, block, leaf, node),
//! files with many extents (an extent B+tree), holes, preallocated space,
//! remote link targets — and compared with the kernel's view of the image.
//! Without mounting, filled by mkfs from a prototype file. Skipped without
//! mkfs.xfs.

use crate::linux_fs_tests::*;
use std::collections::HashMap;
use std::path::Path;

/// A mkfs.xfs prototype file for the folder `src` (modes kept).
fn proto(src: &Path, out: &mut String, depth: usize) {
    use std::os::unix::fs::PermissionsExt;
    let mut entries: Vec<_> = std::fs::read_dir(src).unwrap().flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let m = std::fs::symlink_metadata(e.path()).unwrap();
        let perm = m.permissions().mode() & 0o777;
        let pad = " ".repeat(depth);
        let t = m.file_type();
        if t.is_dir() {
            out.push_str(&format!("{pad}{name} d--{perm:03o} 0 0\n"));
            proto(&e.path(), out, depth + 1);
            out.push_str(&format!("{pad}$\n"));
        } else if t.is_symlink() {
            let target = std::fs::read_link(e.path()).unwrap();
            out.push_str(&format!("{pad}{name} l--{perm:03o} 0 0 {}\n", target.display()));
        } else if t.is_file() {
            out.push_str(&format!("{pad}{name} ---{perm:03o} 0 0 {}\n", e.path().display()));
        } else {
            out.push_str(&format!("{pad}{name} p--{perm:03o} 0 0\n"));
        }
    }
}

#[test]
fn reads_what_the_kernel_writes() {
    if !have(&["mkfs.xfs"]) {
        eprintln!("mkfs.xfs not installed: skipped");
        return;
    }
    let dir = scratch("xfs");
    let cases: &[&[&str]] = &[
        &[],
        &["-b", "size=1024", "-n", "size=8192"],
        &["-m", "bigtime=0", "-i", "size=1024"],
        &["-m", "crc=0", "-n", "ftype=0"],
    ];
    for opts in cases {
        let what = format!("mkfs.xfs {}", opts.join(" "));
        let img = dir.join("xfs.img");
        let _ = std::fs::remove_file(&img);
        junk_image(&img, 320);
        let p = img.to_str().unwrap();
        let mut args = vec!["-q", "-f", "-K", "-L", "testvol"];
        args.extend_from_slice(opts);
        args.push(p);
        let out = std::process::Command::new("mkfs.xfs").args(&args).output().unwrap();
        if !out.status.success() {
            // An older or newer mkfs that refuses a variant (v4 is deprecated).
            eprintln!("{what}: {}", String::from_utf8_lossy(&out.stderr));
            continue;
        }
        let mnt = dir.join("mnt");
        let mounted = Mount::new(&img, &mnt, "xfs", "").map(|m| {
            fill_mounted(&m.dir);
            drop(m);
        });
        let n = match mounted {
            Some(()) => {
                let mut fs = mount_ours(&img, "xfs");
                assert_eq!(fs.info().label.as_deref(), Some("testvol"));
                // The kernel's view of the same image, read-only.
                let m = Mount::new(&img, &mnt, "xfs", "ro,norecovery").expect("mounted before");
                let n = compare(fs.as_mut(), &m.dir, "", &what, true, &mut HashMap::new());
                // Only the used space: as many free blocks as the AG headers
                // count; with them zeroed, xfs_repair finds nothing and the
                // files are the same.
                let free =
                    super::free_space(Box::new(dix_core::device::MemoryDevice::new("x", std::fs::read(&img).unwrap())))
                        .unwrap();
                let free_bytes: u64 = free.iter().map(|r| r.1 - r.0).sum();
                let db = |cmds: &[&str]| -> u64 {
                    let mut c = std::process::Command::new("xfs_db");
                    c.arg("-r");
                    for x in cmds {
                        c.args(["-c", x]);
                    }
                    let out = String::from_utf8(c.arg(&img).output().unwrap().stdout).unwrap();
                    out.rsplit('=').next().unwrap().trim().parse().unwrap()
                };
                let (agcount, bs) = (db(&["sb 0", "p agcount"]), db(&["sb 0", "p blocksize"]));
                let counted: u64 = (0..agcount).map(|a| db(&[&format!("agf {a}"), "p freeblks"])).sum();
                assert_eq!(free_bytes, counted * bs, "{what}: free space");
                assert!(free_bytes > (200 << 20), "{what}: {free_bytes} free");
                // What Copy disk… uses: the same, for the whole image.
                let used = crate::imaging::used_space(Box::new(dix_core::device::MemoryDevice::new(
                    "u",
                    std::fs::read(&img).unwrap(),
                )))
                .unwrap();
                assert_eq!((used.areas[0].kind, used.areas[0].free), (Some("XFS"), free_bytes), "{what}");
                let copy = without_free(&img, &free);
                run("xfs_repair", &["-n", "-f", copy.to_str().unwrap()]);
                let mut fs = mount_ours(&copy, "xfs");
                compare(fs.as_mut(), &m.dir, "", &format!("{what} (used space)"), true, &mut HashMap::new());
                n
            }
            None => {
                // Filled by mkfs from a folder instead.
                let src = dir.join("src");
                let _ = std::fs::remove_dir_all(&src);
                std::fs::create_dir_all(&src).unwrap();
                fill_folder(&src);
                let mut text = "/dev/null\n0 0\nd--755 0 0\n".to_owned();
                proto(&src, &mut text, 1);
                text.push_str("$\n");
                std::fs::write(dir.join("proto"), text).unwrap();
                let mut args = vec!["-q", "-f", "-K", "-p", dir.join("proto").to_str().unwrap().to_owned().leak()];
                args.extend_from_slice(opts);
                args.push(p);
                run("mkfs.xfs", &args);
                let mut fs = mount_ours(&img, "xfs");
                compare_loose(fs.as_mut(), &src, "", &what)
            }
        };
        assert!(n > 3000, "{what}: {n} entries");
    }
}

/// Without a mount to compare with: names and contents of the folder mkfs
/// read (owners and times are mkfs's own).
fn compare_loose(fs: &mut dyn dix_core::FileSystem, host: &Path, rel: &str, what: &str) -> usize {
    use dix_core::{EntryKind, VfsPath};
    let dir = VfsPath::parse(if rel.is_empty() { "/" } else { rel }).unwrap();
    let mut got = fs.read_dir(&dir).unwrap();
    got.sort_by(|a, b| a.name.cmp(&b.name));
    let mut want: Vec<_> = std::fs::read_dir(host.join(rel.trim_start_matches('/'))).unwrap().flatten().collect();
    want.sort_by_key(|e| e.file_name());
    assert_eq!(got.len(), want.len(), "{what}: {rel}");
    let mut n = 0;
    for (e, h) in got.iter().zip(&want) {
        n += 1;
        assert_eq!(e.name, h.file_name().to_string_lossy(), "{what}");
        let path = format!("{rel}/{}", e.name);
        let t = h.file_type().unwrap();
        if t.is_dir() {
            n += compare_loose(fs, host, &path, what);
        } else if t.is_file() {
            let p = VfsPath::parse(&path).unwrap();
            let mut buf = vec![0u8; e.metadata.size as usize];
            let _ = fs.read(&p, 0, &mut buf).unwrap();
            assert!(buf == std::fs::read(h.path()).unwrap(), "{what}: {path}");
        } else if t.is_symlink() {
            assert_eq!(e.metadata.kind, EntryKind::Symlink);
        }
    }
    n
}
