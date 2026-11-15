//! Btrfs made by mkfs.btrfs in several variants (default, 64 KiB nodes,
//! explicit hole items, xxhash checksums), filled by the kernel through a
//! mount — files compressed with zlib, LZO and zstd (inline and in
//! extents), a subvolume and a snapshot of it that differ afterwards,
//! holes, preallocated space, large folders — and compared with the
//! kernel's view of the image. Without mounting, filled by mkfs from a
//! folder (`--rootdir`). Skipped without mkfs.btrfs.

use crate::linux_fs_tests::*;
use std::collections::HashMap;
use std::path::Path;

/// Files written with each compression (forced, so even small ones are).
fn fill_compressed(root: &Path, algo: &str) {
    let d = root.join(algo);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("text.txt"), b"compressible line of text\n".repeat(40_000)).unwrap();
    std::fs::write(d.join("small.txt"), b"tiny and compressible ".repeat(40)).unwrap();
    std::fs::write(d.join("random.bin"), pattern(700_001, 77)).unwrap();
    let mut mixed = b"aaaa".repeat(100_000);
    mixed.extend(pattern(300_000, 5));
    mixed.extend(b"bbbb".repeat(100_000));
    std::fs::write(d.join("mixed.bin"), mixed).unwrap();
}

#[test]
fn reads_what_the_kernel_writes() {
    if !have(&["mkfs.btrfs"]) {
        eprintln!("mkfs.btrfs not installed: skipped");
        return;
    }
    let dir = scratch("btrfs");
    let cases: &[&[&str]] = &[&[], &["-n", "65536"], &["-O", "^no-holes"], &["--csum", "xxhash", "-m", "single"]];
    for opts in cases {
        let what = format!("mkfs.btrfs {}", opts.join(" "));
        let img = dir.join("btrfs.img");
        junk_image(&img, 400);
        let p = img.to_str().unwrap();
        let mnt = dir.join("mnt");
        let mut args = vec!["-q", "-f", "-K", "-L", "testvol"];
        args.extend_from_slice(opts);
        args.push(p);
        run("mkfs.btrfs", &args);
        let filled = Mount::new(&img, &mnt, "btrfs", "").map(|m| {
            fill_mounted(&m.dir);
            for algo in ["zlib", "lzo", "zstd"] {
                run("mount", &["-o", &format!("remount,compress-force={algo}"), m.dir.to_str().unwrap()]);
                fill_compressed(&m.dir, algo);
            }
            let sub = m.dir.join("sub");
            run("btrfs", &["-q", "subvolume", "create", sub.to_str().unwrap()]);
            fill_compressed(&sub, "zstd");
            std::fs::write(sub.join("changes.txt"), b"before the snapshot").unwrap();
            run("btrfs", &["-q", "subvolume", "snapshot", sub.to_str().unwrap(), m.dir.join("snap").to_str().unwrap()]);
            std::fs::write(sub.join("changes.txt"), b"after the snapshot, longer").unwrap();
            std::fs::remove_file(sub.join("zstd/random.bin")).unwrap();
            drop(m);
        });
        let n = match filled {
            Some(()) => {
                let mut fs = mount_ours(&img, "btrfs");
                let info = fs.info();
                assert_eq!(info.label.as_deref(), Some("testvol"));
                assert!(info.details.iter().any(|(k, v)| k == "Subvolumes" && v == "2"), "{:?}", info.details);
                let m = Mount::new(&img, &mnt, "btrfs", "ro").expect("mounted before");
                let n = compare(fs.as_mut(), &m.dir, "", &what, true, &mut HashMap::new());
                // Only the used space: with the free space zeroed, btrfs
                // check (every data block's checksum too) finds nothing, and
                // the files are the same.
                let free =
                    super::free_space(Box::new(dix_core::device::MemoryDevice::new("b", std::fs::read(&img).unwrap())))
                        .unwrap()
                        .expect("trusted");
                let free_bytes: u64 = free.iter().map(|r| r.1 - r.0).sum();
                assert!(free_bytes > (150 << 20), "{what}: {free_bytes} free");
                // What Copy disk… uses: the same, for the whole image.
                let used = crate::imaging::used_space(Box::new(dix_core::device::MemoryDevice::new(
                    "u",
                    std::fs::read(&img).unwrap(),
                )))
                .unwrap();
                assert_eq!((used.areas[0].kind, used.areas[0].free), (Some("Btrfs"), free_bytes), "{what}");
                let copy = without_free(&img, &free);
                run("btrfs", &["check", "--readonly", "--check-data-csum", copy.to_str().unwrap()]);
                // Both copies of DUP metadata are kept: with the first copy of
                // each such chunk wiped too, btrfs check reads the second.
                let dump = std::process::Command::new("btrfs")
                    .args(["inspect-internal", "dump-tree", "-t", "chunk"])
                    .arg(&img)
                    .output()
                    .unwrap();
                let dump = String::from_utf8_lossy(&dump.stdout).into_owned();
                let mut wiped = 0;
                let mut data = std::fs::read(&copy).unwrap();
                for item in dump.split("CHUNK_ITEM").skip(1) {
                    let words: Vec<&str> = item.split_whitespace().collect();
                    let after = |w: &str| words.iter().position(|x| *x == w).map(|i| words[i + 1]);
                    if !after("type").is_some_and(|t| t.contains("DUP")) {
                        continue;
                    }
                    let len: usize = after("length").unwrap().parse().unwrap();
                    let first: usize = after("offset").unwrap().parse().unwrap();
                    data[first..first + len].fill(0);
                    wiped += 1;
                }
                if wiped > 0 {
                    let one = img.with_extension("one-copy.img");
                    std::fs::write(&one, data).unwrap();
                    run("btrfs", &["check", "--readonly", one.to_str().unwrap()]);
                }
                let mut fs = mount_ours(&copy, "btrfs");
                compare(fs.as_mut(), &m.dir, "", &format!("{what} (used space)"), true, &mut HashMap::new());
                n
            }
            None => {
                let src = dir.join("src");
                let _ = std::fs::remove_dir_all(&src);
                std::fs::create_dir_all(&src).unwrap();
                fill_folder(&src);
                fill_compressed(&src, "plain");
                junk_image(&img, 400);
                let mut args = vec!["-q", "-f", "-K", "--rootdir", src.to_str().unwrap().to_owned().leak()];
                args.extend_from_slice(opts);
                args.push(p);
                run("mkfs.btrfs", &args);
                let mut fs = mount_ours(&img, "btrfs");
                compare(fs.as_mut(), &src, "", &what, true, &mut HashMap::new())
            }
        };
        assert!(n > 3000, "{what}: {n} entries");
    }
}
