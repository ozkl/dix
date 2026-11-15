//! Images made by the reference littlefs (through littlefs-python): small
//! and large blocks, inline and CTZ files, folders split over several
//! metadata pairs, files renamed, deleted and rewritten (old commits and
//! compactions in the logs), MicroPython's modification times; and one
//! inside a flash dump, after "firmware". Read back and compared with what
//! littlefs says is in them. Skipped without littlefs-python.

use super::*;
use dix_core::device::MemoryDevice;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

fn have_littlefs() -> bool {
    let ok = Command::new("python3").args(["-c", "import littlefs"]).output().is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("littlefs-python not installed: skipped");
    }
    ok
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dix-lfs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Makes an image with littlefs-python: `ops` is Python run with `fs`
/// mounted. Returns the image and its tree as littlefs reads it: path →
/// (content or None for a folder, mtime attribute in ns).
fn make(dir: &std::path::Path, block_size: u32, block_count: u32, ops: &str) -> (Vec<u8>, Tree) {
    let script = format!(
        r#"
import sys
from littlefs import LittleFS
fs = LittleFS(block_size={block_size}, block_count={block_count}, name_max=255)
def pat(n, seed):
    return bytes(((i * 2654435761 + seed) >> 13) & 255 for i in range(n))
{ops}
out = open(sys.argv[2], 'w')
def walk(d):
    for name in fs.listdir(d):
        p = (d.rstrip('/') + '/' + name)
        st = fs.stat(p)
        try:
            t = int.from_bytes(fs.getattr(p, 1), 'little')
        except Exception:
            t = -1
        if st.type == 2:
            out.write('D\t%s\t%d\n' % (p, t))
            walk(p)
        else:
            with fs.open(p, 'rb') as f:
                out.write('F\t%s\t%d\t%s\n' % (p, t, f.read().hex()))
walk('/')
out.close()
open(sys.argv[1], 'wb').write(fs.context.buffer)
"#
    );
    let img = dir.join("img.lfs");
    let list = dir.join("tree.txt");
    let out = Command::new("python3").args(["-c", &script]).arg(&img).arg(&list).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut tree = Tree::new();
    for line in std::fs::read_to_string(&list).unwrap().lines() {
        let f: Vec<&str> = line.splitn(4, '\t').collect();
        let t: i64 = f[2].parse().unwrap();
        let data = (f[0] == "F").then(|| {
            let h = f.get(3).copied().unwrap_or("");
            (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect()
        });
        tree.insert(f[1].to_owned(), (data, (t >= 0).then_some(t)));
    }
    (std::fs::read(&img).unwrap(), tree)
}

type Tree = BTreeMap<String, (Option<Vec<u8>>, Option<i64>)>;

fn ours(fs: &mut dyn FileSystem, dir: &str, out: &mut Tree) {
    for e in fs.read_dir(&VfsPath::parse(dir).unwrap()).unwrap() {
        let p = format!("{}/{}", dir.trim_end_matches('/'), e.name);
        let t = e.metadata.modified.map(|s| s * 1_000_000_000);
        if e.metadata.kind == EntryKind::Directory {
            out.insert(p.clone(), (None, t));
            ours(fs, &p, out);
        } else {
            let vp = VfsPath::parse(&p).unwrap();
            let mut data = vec![0u8; e.metadata.size as usize];
            // In odd pieces, across blocks.
            let mut at = 0;
            while at < data.len() {
                let n = (data.len() - at).min(777);
                assert_eq!(fs.read(&vp, at as u64, &mut data[at..at + n]).unwrap(), n);
                at += n;
            }
            out.insert(p, (Some(data), t));
        }
    }
}

fn mount(img: Vec<u8>) -> (Box<dyn FileSystem>, u8) {
    let mut dev = MemoryDevice::new("lfs", img.clone());
    let score = LittleFsDriver.probe(&mut dev).unwrap();
    let m = crate::default_registry().mount(
        Box::new(MemoryDevice::new("lfs", img)),
        Some("littlefs"),
        AccessMode::ReadOnly,
    );
    (m.unwrap().fs, score)
}

fn check(img: Vec<u8>, want: &Tree, what: &str) -> Box<dyn FileSystem> {
    let (mut fs, _) = mount(img);
    let mut got = Tree::new();
    ours(fs.as_mut(), "/", &mut got);
    // Times: whole seconds here.
    let want: Tree =
        want.iter().map(|(k, (d, t))| (k.clone(), (d.clone(), t.map(|t| t / 1_000_000_000 * 1_000_000_000)))).collect();
    let missing: Vec<_> = want.keys().filter(|k| !got.contains_key(*k)).collect();
    let extra: Vec<_> = got.keys().filter(|k| !want.contains_key(*k)).collect();
    assert!(missing.is_empty() && extra.is_empty(), "{what}: missing {missing:?}, extra {extra:?}");
    for (k, v) in &want {
        assert!(got[k] == *v, "{what}: {k} differs");
    }
    fs
}

const OPS: &str = r#"
fs.makedirs('/lib/pkg/sub')
with fs.open('/main.py', 'wb') as f: f.write(b'print("hello from the pico")\n')
with fs.open('/empty', 'wb') as f: pass
with fs.open('/boot.py', 'wb') as f: f.write(pat(300, 1))
with fs.open('/lib/big.bin', 'wb') as f: f.write(pat(200_000, 2))
with fs.open('/lib/pkg/mid.bin', 'wb') as f: f.write(pat(5_000, 3))
with fs.open('/lib/pkg/sub/Ünïcödé — 日本語.txt', 'wb') as f: f.write('unicode'.encode())
for i in range(120):
    with fs.open('/lib/many-%03d.txt' % i, 'wb') as f: f.write(pat(i * 11, i))
# Churn: renames, deletes, rewrites, appends (old commits, compactions).
for i in range(0, 120, 3):
    fs.remove('/lib/many-%03d.txt' % i)
for i in [i for i in range(1, 120, 7) if i % 3]:
    fs.rename('/lib/many-%03d.txt' % i, '/lib/pkg/moved-%03d.txt' % i)
with fs.open('/boot.py', 'wb') as f: f.write(pat(700, 9))
with fs.open('/lib/pkg/mid.bin', 'ab') as f: f.write(pat(9_000, 4))
fs.rename('/lib/pkg/sub', '/lib/sub-renamed')
fs.setattr('/main.py', 1, (1_700_000_123_456_789_000).to_bytes(8, 'little'))
fs.setattr('/lib', 1, (1_600_000_000_000_000_000).to_bytes(8, 'little'))
"#;

#[test]
fn reads_what_littlefs_writes() {
    if !have_littlefs() {
        return;
    }
    let dir = scratch("read");
    // (block size, count): the Pico's 4 KiB flash blocks; small blocks
    // (many pairs, splits, deep skip lists); large ones.
    for (bs, count) in [(4096, 352), (512, 1024), (256, 2048), (16384, 64)] {
        let (img, want) = make(&dir, bs, count, OPS);
        let what = format!("{bs}×{count}");
        let fs = check(img.clone(), &want, &what);
        let info = fs.info();
        assert_eq!(info.block_size, Some(bs), "{what}");
        assert_eq!(info.total_bytes, Some(bs as u64 * count as u64), "{what}");
        assert!(
            info.free_bytes.is_some_and(|f| f > 0 && f < bs as u64 * count as u64),
            "{what}: {:?}",
            info.free_bytes
        );
        assert_eq!(mount(img).1, 95);
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// MicroPython's layout on a Pico: the file system at the end of a 2 MiB
/// flash, after the firmware (here noise), found by searching.
#[test]
fn finds_littlefs_in_a_flash_dump() {
    if !have_littlefs() {
        return;
    }
    let dir = scratch("dump");
    let (img, want) = make(&dir, 4096, 352, OPS);
    let mut flash: Vec<u8> = (0..(2u32 << 20)).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
    let at = flash.len() - img.len();
    flash[at..].copy_from_slice(&img);
    let (fs, score) = mount(flash.clone());
    assert_eq!(score, 40, "found inside, behind drivers recognising the start");
    assert!(fs.info().details.iter().any(|(k, v)| k == "Offset in the image" && *v == format!("{at:#x}")));
    drop(fs);
    check(flash, &want, "flash dump");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn crc_matches_littlefs() {
    // lfs_crc(0xffffffff, "123456789") (no final inversion).
    assert_eq!(crc32(0xffff_ffff, b"123456789"), !0xCBF4_3926);
}

/// What the reference littlefs reads from `img` (after running `ops` on
/// it, if any): its tree and the image as it left it.
fn reference(dir: &std::path::Path, img: &[u8], bs: u32, count: u32, ops: &str) -> (Vec<u8>, Tree) {
    let input = dir.join("in.lfs");
    std::fs::write(&input, img).unwrap();
    let ops = format!(
        "fs = LittleFS(block_size={bs}, block_count={count}, mount=False, name_max=255)\n\
         fs.context.buffer[:] = open({input:?}, 'rb').read()\nfs.mount()\n{ops}"
    );
    // `make` formats a new volume first; this one is replaced by the image.
    make(dir, bs, count, &ops)
}

fn open_rw(img: Vec<u8>) -> (dix_core::device::SharedBuffer, Box<dyn FileSystem>) {
    let dev = MemoryDevice::new("lfs", img);
    let buf = dev.buffer();
    let m = crate::default_registry().mount(Box::new(dev), Some("littlefs"), AccessMode::ReadWrite).unwrap();
    assert_eq!(m.fs.access_mode(), AccessMode::ReadWrite);
    (buf, m.fs)
}

fn put(fs: &mut dyn FileSystem, path: &str, data: &[u8]) {
    let p = VfsPath::parse(path).unwrap();
    fs.create_file(&p).unwrap();
    for (i, c) in data.chunks(10_000).enumerate() {
        fs.write_at(&p, (i * 10_000) as u64, c).unwrap();
    }
}

fn pattern(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8).collect()
}

/// Changed here, read by the reference littlefs (exactly the expected
/// tree), then written to by it (its commits appended to our pairs, our
/// pairs compacted, our files extended), and read here again.
#[test]
fn changes_that_littlefs_reads_and_writes_on() {
    if !have_littlefs() {
        return;
    }
    let dir = scratch("write");
    for (bs, count) in [(4096u32, 352u32), (512, 1024), (256, 4096)] {
        let what = format!("{bs}×{count}");
        let (img, mut want) = make(&dir, bs, count, OPS);
        let (buf, mut fs) = open_rw(img);
        let w = fs.as_mut();
        w.write_file(&VfsPath::parse("/main.py").unwrap(), b"print('changed')\n").unwrap();
        w.remove_file(&VfsPath::parse("/boot.py").unwrap()).unwrap();
        w.rename(&VfsPath::parse("/lib/pkg").unwrap(), &VfsPath::parse("/pkg2").unwrap()).unwrap();
        w.create_dir(&VfsPath::parse("/data").unwrap()).unwrap();
        // A large file, sized to the volume.
        let log = (bs as usize * count as usize) / 12;
        put(w, "/data/log.bin", &pattern(log, 5));
        put(w, "/data/tiny", b"t");
        for i in 0..150 {
            put(w, &format!("/data/f{i:03}"), &pattern(i * 13 % 300, i as u32));
        }
        w.unmount().unwrap();
        drop(fs);
        // The expected tree.
        let re = |p: &str| p.replacen("/lib/pkg", "/pkg2", 1);
        want = want.into_iter().filter(|(k, _)| k != "/boot.py").map(|(k, v)| (re(&k), v)).collect();
        want.insert("/main.py".into(), (Some(b"print('changed')\n".to_vec()), None));
        want.insert("/data".into(), (None, None));
        want.insert("/data/log.bin".into(), (Some(pattern(log, 5)), None));
        want.insert("/data/tiny".into(), (Some(b"t".to_vec()), None));
        for i in 0..150 {
            want.insert(format!("/data/f{i:03}"), (Some(pattern(i * 13 % 300, i as u32)), None));
        }
        let same = |got: &Tree, label: &str| {
            let gk: Vec<_> = got.keys().collect();
            let wk: Vec<_> = want.keys().collect();
            assert_eq!(gk, wk, "{what} {label}");
            for (k, (d, _)) in &want {
                assert!(got[k].0 == *d, "{what} {label}: {k} differs");
            }
        };
        let ours_img = buf.snapshot();
        let (_, theirs) = reference(&dir, &ours_img, bs, count, "");
        same(&theirs, "read by littlefs");
        // New files have MicroPython's time attribute.
        assert!(theirs["/data/tiny"].1.is_some_and(|t| t > 1_700_000_000_000_000_000), "{what}");
        // littlefs writes on: appends, new files, a rename, removals.
        let more = r#"
with fs.open('/data/log.bin', 'ab') as f: f.write(pat(20_000, 6))
with fs.open('/main.py', 'wb') as f: f.write(b'by littlefs\n')
for i in range(40):
    with fs.open('/data/g%02d' % i, 'wb') as f: f.write(pat(i * 37, i))
fs.rename('/data/f007', '/f007-moved')
for i in range(0, 150, 5):
    fs.remove('/data/f%03d' % i)
"#;
        let (after, theirs) = reference(&dir, &ours_img, bs, count, more);
        let (mut fs, _) = mount(after);
        let mut got = Tree::new();
        ours(fs.as_mut(), "/", &mut got);
        let strip = |t: &Tree| t.iter().map(|(k, (d, _))| (k.clone(), d.clone())).collect::<BTreeMap<_, _>>();
        assert!(strip(&got) == strip(&theirs), "{what}: after littlefs wrote on");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Changing the file system inside a flash dump leaves the rest (the
/// firmware) as it was.
#[test]
fn changes_inside_a_flash_dump() {
    if !have_littlefs() {
        return;
    }
    let dir = scratch("dump-write");
    let (img, _) = make(&dir, 4096, 352, OPS);
    let mut flash: Vec<u8> = (0..(2u32 << 20)).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
    let at = flash.len() - img.len();
    flash[at..].copy_from_slice(&img);
    let (buf, mut fs) = open_rw(flash.clone());
    fs.write_file(&VfsPath::parse("/main.py").unwrap(), b"import machine\n").unwrap();
    fs.unmount().unwrap();
    drop(fs);
    let after = buf.snapshot();
    assert_eq!(after.len(), flash.len());
    assert!(after[..at] == flash[..at], "the firmware is untouched");
    let (_, theirs) = reference(&dir, &after[at..], 4096, 352, "");
    assert_eq!(theirs["/main.py"].0.as_deref(), Some(&b"import machine\n"[..]));
    assert!(theirs.contains_key("/lib/big.bin"));
    let _ = std::fs::remove_dir_all(dir);
}

/// *New image → littlefs*: an empty volume the reference littlefs mounts
/// and fills.
#[test]
fn formats_what_littlefs_mounts() {
    if !have_littlefs() {
        return;
    }
    let dir = scratch("format");
    let mut dev = MemoryDevice::new("new", vec![0u8; 1 << 20]);
    crate::default_registry().format(&mut dev, "littlefs", &dix_core::FormatOptions::default()).unwrap();
    let img = dev.buffer().snapshot();
    let ops = "with fs.open('/hello.txt', 'wb') as f: f.write(b'hi')\nfs.mkdir('/d')\n";
    let (after, theirs) = reference(&dir, &img, 4096, 256, ops);
    assert_eq!(theirs.len(), 2);
    let (mut fs, _) = mount(after);
    assert_eq!(fs.read_to_vec(&VfsPath::parse("/hello.txt").unwrap()).unwrap(), b"hi");
    let _ = std::fs::remove_dir_all(dir);
}

/// The same files take about as many blocks written here as by littlefs.
#[test]
fn rewrites_compactly() {
    if !have_littlefs() {
        return;
    }
    let dir = scratch("compact");
    for (bs, count) in [(4096u32, 352u32), (512, 1024), (256, 4096)] {
        let (img, _) = make(&dir, bs, count, OPS);
        let used = |img: Vec<u8>| {
            let (mut fs, _) = mount(img);
            fs.read_dir(&VfsPath::root()).unwrap();
            let i = fs.info();
            (i.total_bytes.unwrap() - i.free_bytes.unwrap()) / bs as u64
        };
        let theirs = used(img.clone());
        let (buf, mut fs) = open_rw(img);
        // A change, so it is written anew.
        fs.write_file(&VfsPath::parse("/main.py").unwrap(), b"x").unwrap();
        fs.unmount().unwrap();
        drop(fs);
        let ours = used(buf.snapshot());
        eprintln!("{bs}x{count}: littlefs {theirs} blocks, ours {ours}");
        assert!(ours <= theirs + theirs / 5 + 4, "{bs}×{count}: {ours} blocks against littlefs's {theirs}");
    }
    let _ = std::fs::remove_dir_all(dir);
}
