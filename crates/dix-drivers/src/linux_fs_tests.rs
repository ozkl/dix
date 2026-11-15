//! What the XFS and Btrfs tests share: an image made by the file system's
//! own mkfs, filled through a real mount by the Linux kernel when the tests
//! may mount (root, loop devices), then read by our driver and compared,
//! entry by entry, with what the kernel shows of the same image: names,
//! types, sizes, contents, permissions, owners, times, link targets, hard
//! links. Without mounting, the image is filled by mkfs from a folder
//! (`fill_folder`) and compared with that folder.

use dix_core::device::MemoryDevice;
use dix_core::{AccessMode, EntryKind, FileSystem, VfsPath};
use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn have(tools: &[&str]) -> bool {
    tools
        .iter()
        .all(|t| Command::new("sh").arg("-c").arg(format!("command -v {t}")).output().is_ok_and(|o| o.status.success()))
}

pub fn run(cmd: &str, args: &[&str]) {
    let out = Command::new(cmd).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{cmd} {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A folder for a test's images, removed when dropped — also when the test
/// fails (the images are hundreds of MiB).
pub struct Scratch(PathBuf);

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn scratch(name: &str) -> Scratch {
    let d = std::env::temp_dir().join(format!("dix-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Scratch(d)
}

pub fn pattern(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8).collect()
}

/// A new image file of `mib` MiB full of junk, as a used disk is: space a
/// file system has not written (holes, preallocated extents) must not show
/// it.
pub fn junk_image(path: &Path, mib: usize) {
    use std::io::Write;
    let mut f = std::fs::File::create(path).unwrap();
    let chunk = pattern(1 << 20, 0xD1);
    for _ in 0..mib {
        f.write_all(&chunk).unwrap();
    }
}

/// A loop device with the image mounted on `dir`; unmounted when dropped.
pub struct Mount {
    pub dir: PathBuf,
    dev: String,
}

impl Mount {
    /// None when this machine does not let the tests mount.
    /// (`DIX_TEST_NO_MOUNT` set: never, to test the other way.)
    pub fn new(image: &Path, dir: &Path, fs: &str, opts: &str) -> Option<Mount> {
        if std::env::var_os("DIX_TEST_NO_MOUNT").is_some() {
            return None;
        }
        std::fs::create_dir_all(dir).ok()?;
        let out = Command::new("losetup").args(["-f", "--show"]).arg(image).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let dev = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        let mut cmd = Command::new("mount");
        cmd.args(["-t", fs]);
        if !opts.is_empty() {
            cmd.args(["-o", opts]);
        }
        let ok = cmd.arg(&dev).arg(dir).output().is_ok_and(|o| o.status.success());
        if !ok {
            let _ = Command::new("losetup").args(["-d", &dev]).output();
            return None;
        }
        Some(Mount { dir: dir.to_owned(), dev })
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.dir).output();
        let _ = Command::new("losetup").args(["-d", &self.dev]).output();
    }
}

/// Files every test image gets: sizes around block boundaries, Unicode and
/// long names, deep and large folders, links, a FIFO.
pub fn fill_folder(root: &Path) {
    let w = |p: &str, d: &[u8]| {
        let p = root.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, d).unwrap();
    };
    w("empty", b"");
    w("tiny.txt", b"x");
    w("hello.txt", b"Hello, file system!\n");
    w("block.bin", &pattern(4096, 1));
    w("block+1.bin", &pattern(4097, 2));
    w("big.bin", &pattern(3_000_017, 3));
    w("Ünïcödé—日本語.txt", "unicode\n".as_bytes());
    w(&"n".repeat(255), b"long name");
    w("a/b/c/d/e/deep.txt", b"deep");
    for i in 0..5 {
        w(&format!("few/{i}.txt"), format!("{i}").as_bytes());
    }
    for i in 0..40 {
        w(&format!("some/file-{i:03}.dat"), &pattern(i * 7, i as u32));
    }
    for i in 0..3000 {
        w(&format!("many/a-rather-long-file-name-{i:05}.dat"), &pattern(i % 50, i as u32));
    }
    std::os::unix::fs::symlink("hello.txt", root.join("link")).unwrap();
    std::os::unix::fs::symlink(format!("{}/target", "long-component".repeat(25)), root.join("long-link")).unwrap();
    std::os::unix::fs::symlink("../../../hello.txt", root.join("a/b/c/up")).unwrap();
    std::fs::hard_link(root.join("hello.txt"), root.join("a/hard.txt")).unwrap();
    std::fs::set_permissions(root.join("tiny.txt"), std::fs::Permissions::from_mode(0o751)).unwrap();
    let _ = Command::new("mkfifo").arg(root.join("fifo")).status();
}

/// More, through a mount (the kernel writes them): owners, a device node,
/// a file with holes, a fragmented file (holes punched every other 64 KiB:
/// many extents), preallocated space partly written.
pub fn fill_mounted(root: &Path) {
    fill_folder(root);
    let p = root.join("owned.txt");
    std::fs::write(&p, b"someone else's").unwrap();
    std::os::unix::fs::chown(&p, Some(1234), Some(5678)).unwrap();
    let _ = Command::new("mknod").arg(root.join("null")).args(["c", "1", "3"]).status();
    // Holes.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::File::create(root.join("sparse.bin")).unwrap();
        for i in 0..40u64 {
            f.seek(SeekFrom::Start(i * 300_000 + 7)).unwrap();
            f.write_all(&pattern(5000, i as u32)).unwrap();
        }
        f.set_len(40 * 300_000 + 123_456).unwrap();
    }
    // Fragmented.
    let frag = root.join("fragmented.bin");
    std::fs::write(&frag, pattern(16 << 20, 9)).unwrap();
    let _ = Command::new("sync").status();
    for i in 0..128u64 {
        let _ = Command::new("fallocate")
            .args(["--punch-hole", "--offset", &(i * 131072).to_string(), "--length", "65536"])
            .arg(&frag)
            .status();
    }
    // Preallocated, with a little written in the middle.
    let pre = root.join("prealloc.bin");
    let _ = Command::new("fallocate").args(["-l", "2097152"]).arg(&pre).status();
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(&pre).unwrap();
        f.seek(SeekFrom::Start(1_000_000)).unwrap();
        f.write_all(b"written into preallocated space").unwrap();
    }
}

/// A copy of `img` with the `free` byte ranges zeroed: what copying only
/// the used space leaves (the junk in free space gone).
pub fn without_free(img: &Path, free: &[(u64, u64)]) -> PathBuf {
    let mut data = std::fs::read(img).unwrap();
    for &(a, b) in free {
        data[a as usize..b as usize].fill(0);
    }
    let out = img.with_extension("used.img");
    std::fs::write(&out, data).unwrap();
    out
}

/// Reads `img` with our registry; checks the driver chosen.
pub fn mount_ours(img: &Path, id: &str) -> Box<dyn FileSystem> {
    let reg = crate::default_registry();
    let dev = MemoryDevice::new("img", std::fs::read(img).unwrap());
    let m = reg.mount(Box::new(dev), None, AccessMode::ReadOnly).unwrap();
    assert_eq!(m.driver.info().id, id);
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

/// Compares the volume with a host folder (a mount of the same image, or
/// what it was made from), recursively. `inodes`: host (device, inode) to
/// ours, for hard links. Returns the entries compared.
pub fn compare(
    fs: &mut dyn FileSystem,
    host: &Path,
    rel: &str,
    what: &str,
    owners: bool,
    inodes: &mut HashMap<(u64, u64), u64>,
) -> usize {
    let dir = VfsPath::parse(if rel.is_empty() { "/" } else { rel }).unwrap();
    let mut got: Vec<_> = fs.read_dir(&dir).unwrap();
    let mut want: Vec<_> = std::fs::read_dir(host.join(rel.trim_start_matches('/'))).unwrap().flatten().collect();
    got.sort_by(|a, b| a.name.cmp(&b.name));
    want.sort_by_key(|e| e.file_name());
    let names: Vec<_> = got.iter().map(|e| e.name.clone()).collect();
    let wnames: Vec<_> = want.iter().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, wnames, "{what}: {rel}");
    let mut n = 0;
    for (e, h) in got.iter().zip(&want) {
        n += 1;
        let path = format!("{rel}/{}", e.name);
        let hm = std::fs::symlink_metadata(h.path()).unwrap();
        let m = &e.metadata;
        assert_eq!(m.mode, Some(hm.mode()), "{what}: {path} mode");
        assert_eq!(m.modified, Some(hm.mtime()), "{what}: {path} time");
        if owners {
            assert_eq!((m.uid, m.gid), (Some(hm.uid()), Some(hm.gid())), "{what}: {path} owner");
        }
        let ino = m.inode.unwrap();
        assert_eq!(*inodes.entry((hm.dev(), hm.ino())).or_insert(ino), ino, "{what}: {path} hard link");
        let t = hm.file_type();
        if t.is_dir() {
            assert_eq!(m.kind, EntryKind::Directory, "{what}: {path}");
            n += compare(fs, host, &path, what, owners, inodes);
        } else if t.is_symlink() {
            assert_eq!(m.kind, EntryKind::Symlink);
            let target = std::fs::read_link(h.path()).unwrap();
            assert_eq!(m.symlink_target.as_deref(), Some(target.to_str().unwrap()), "{what}: {path}");
        } else if t.is_file() {
            assert_eq!(m.kind, EntryKind::File, "{what}: {path}");
            assert_eq!(m.size, hm.len(), "{what}: {path}");
            let p = VfsPath::parse(&path).unwrap();
            assert!(read_all(fs, &p, m.size) == std::fs::read(h.path()).unwrap(), "{what}: {path} differs");
        } else {
            assert_eq!(m.kind, EntryKind::Other, "{what}: {path}");
        }
    }
    n
}
