//! ISO images made by xorriso and genisoimage, read by the driver and
//! compared with the folders they were made from.

use dix_core::device::host::FileDevice;
use dix_core::{AccessMode, EntryKind, FileSystem, VfsPath};
use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

fn tool(name: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin", "/usr/local/bin"]
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.exists())
}

fn have(tools: &[&str]) -> bool {
    let ok = tools.iter().all(|t| tool(t).is_some());
    if !ok {
        eprintln!("{tools:?} not installed: skipping ISO test");
    }
    ok
}

fn run(dir: &Path, cmd: &str, args: &[&str]) {
    let out = Command::new(tool(cmd).unwrap()).args(args).current_dir(dir).env("LC_ALL", "C.UTF-8").output().unwrap();
    assert!(out.status.success(), "{cmd} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("dix-iso-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum Item {
    Dir,
    File(Vec<u8>),
    Link(String),
}

fn write(dir: &Path, rel: &str, data: &[u8]) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

/// Plain files and folders (what any ISO flavour can hold).
fn plain_source(dir: &Path) {
    write(dir, "hello.txt", b"hello ISO\n");
    write(dir, "docs/readme.md", b"# Read me\n");
    write(dir, "docs/big.bin", &noise(3 << 20, 1));
    write(dir, "empty.txt", b"");
}

/// Everything Rock Ridge can carry.
fn full_source(dir: &Path) {
    plain_source(dir);
    write(dir, "Ünïcödé – ✓ 日本語.txt", "unicode\n".as_bytes());
    write(dir, "a file name that is much longer than the sixty-four characters Joliet allows.txt", b"long");
    write(dir, "1/2/3/4/5/6/7/8/9/10/deep.txt", b"deep down");
    std::os::unix::fs::symlink("hello.txt", dir.join("link-to-hello")).unwrap();
    std::os::unix::fs::symlink("../docs", dir.join("1/up-and-over")).unwrap();
    std::fs::hard_link(dir.join("hello.txt"), dir.join("docs/hard.txt")).unwrap();
    std::fs::set_permissions(dir.join("docs/readme.md"), std::fs::Permissions::from_mode(0o640)).unwrap();
}

fn host_tree(dir: &Path, rel: &str, out: &mut BTreeMap<String, Item>) {
    for e in std::fs::read_dir(dir.join(rel.trim_start_matches('/'))).unwrap().flatten() {
        let path = format!("{rel}/{}", e.file_name().to_string_lossy());
        let t = e.file_type().unwrap();
        if t.is_symlink() {
            out.insert(path, Item::Link(std::fs::read_link(e.path()).unwrap().to_string_lossy().into_owned()));
        } else if t.is_dir() {
            out.insert(path.clone(), Item::Dir);
            host_tree(dir, &path, out);
        } else {
            out.insert(path, Item::File(std::fs::read(e.path()).unwrap()));
        }
    }
}

fn fs_tree(fs: &mut dyn FileSystem, dir: &str, out: &mut BTreeMap<String, Item>) {
    let base = if dir == "/" { String::new() } else { dir.to_owned() };
    for e in fs.read_dir(&VfsPath::parse(dir).unwrap()).unwrap() {
        let path = format!("{base}/{}", e.name);
        if path == "/[BOOT]" {
            continue;
        }
        match e.metadata.kind {
            EntryKind::Directory => {
                out.insert(path.clone(), Item::Dir);
                fs_tree(fs, &path, out);
            }
            EntryKind::Symlink => {
                out.insert(path, Item::Link(e.metadata.symlink_target.unwrap()));
            }
            _ => {
                let data = fs.read_to_vec(&VfsPath::parse(&path).unwrap()).unwrap();
                assert_eq!(data.len() as u64, e.metadata.size, "{path}");
                out.insert(path, Item::File(data));
            }
        }
    }
}

/// Opens an image, retrying while a child process another test forks
/// still holds its lock (for a moment, until it execs).
fn open_retry(img: &Path, mode: AccessMode) -> FileDevice {
    for _ in 0..100 {
        match FileDevice::open(img, mode) {
            Err(e) if e.kind() == dix_core::ErrorKind::InUse => {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
            r => return r.unwrap(),
        }
    }
    FileDevice::open(img, mode).unwrap()
}

fn mount(img: &Path) -> dix_core::Mounted {
    let dev = open_retry(img, AccessMode::ReadOnly);
    let m = crate::default_registry().mount(Box::new(dev), None, AccessMode::ReadOnly).unwrap();
    assert_eq!(m.driver.info().id, "iso9660", "{}", img.display());
    m
}

fn tree_of(img: &Path) -> BTreeMap<String, Item> {
    let mut m = mount(img);
    let mut t = BTreeMap::new();
    fs_tree(m.fs.as_mut(), "/", &mut t);
    t
}

fn detail(fs: &dyn FileSystem, name: &str) -> String {
    fs.info().details.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap_or_default()
}

fn assert_same(name: &str, got: &BTreeMap<String, Item>, want: &BTreeMap<String, Item>) {
    let missing: Vec<_> = want.keys().filter(|k| !got.contains_key(*k)).collect();
    let extra: Vec<_> = got.keys().filter(|k| !want.contains_key(*k)).collect();
    assert!(missing.is_empty() && extra.is_empty(), "{name}: missing {missing:?}, extra {extra:?}");
    for (k, v) in want {
        assert!(got[k] == *v, "{name}: {k} differs");
    }
}

/// Rock Ridge (from xorriso and genisoimage, the latter relocating the
/// deep folders): names, links, permissions, times, all content.
#[test]
fn rock_ridge() {
    if !have(&["xorriso", "genisoimage"]) {
        return;
    }
    let s = Scratch::new("rr");
    let src = s.0.join("src");
    full_source(&src);
    let mut want = BTreeMap::new();
    host_tree(&src, "", &mut want);
    run(
        &s.0,
        "xorriso",
        &["-as", "mkisofs", "-quiet", "-R", "-J", "-joliet-long", "-V", "RR TEST", "-o", "x.iso", "src"],
    );
    run(&s.0, "genisoimage", &["-quiet", "-R", "-J", "-input-charset", "utf-8", "-o", "g.iso", "src"]);
    for name in ["x.iso", "g.iso"] {
        let img = s.0.join(name);
        let got = tree_of(&img);
        // genisoimage leaves its relocation folder visible, as Linux does.
        let mut got = got;
        got.remove("/rr_moved");
        assert_same(name, &got, &want);
        let mut m = mount(&img);
        let meta = m.fs.metadata(&VfsPath::parse("/docs/readme.md").unwrap()).unwrap();
        let host = std::fs::metadata(src.join("docs/readme.md")).unwrap();
        assert_eq!(meta.mode, Some(host.mode()), "{name}");
        assert_eq!(meta.modified, Some(host.mtime()), "{name}");
        assert_eq!(meta.uid, Some(host.uid()));
        assert!(detail(m.fs.as_ref(), "Extensions").contains("Rock Ridge"), "{name}");
        assert!(detail(m.fs.as_ref(), "Extensions").contains("Joliet"), "{name}");
        assert!(detail(m.fs.as_ref(), "Names from").starts_with("Rock Ridge"));
        assert_eq!(detail(m.fs.as_ref(), "Bootable"), "No");
    }
    let m = mount(&s.0.join("x.iso"));
    assert_eq!(m.fs.info().label.as_deref(), Some("RR TEST"));
    assert!(detail(m.fs.as_ref(), "Data preparer").contains("XORRISO"));
    assert!(!detail(m.fs.as_ref(), "Created").is_empty());
}

/// Joliet without Rock Ridge: Unicode names; and plain ISO 9660 names.
#[test]
fn joliet_and_plain_names() {
    if !have(&["genisoimage"]) {
        return;
    }
    let s = Scratch::new("joliet");
    let src = s.0.join("src");
    plain_source(&src);
    write(&src, "Ünïcödé – ✓.txt", "unicode\n".as_bytes());
    let mut want = BTreeMap::new();
    host_tree(&src, "", &mut want);
    run(
        &s.0,
        "genisoimage",
        &["-quiet", "-J", "-joliet-long", "-input-charset", "utf-8", "-V", "JOLIET", "-o", "j.iso", "src"],
    );
    let got = tree_of(&s.0.join("j.iso"));
    assert_same("j.iso", &got, &want);
    let m = mount(&s.0.join("j.iso"));
    assert!(detail(m.fs.as_ref(), "Names from").starts_with("Joliet"));

    std::fs::remove_file(src.join("Ünïcödé – ✓.txt")).unwrap();
    run(&s.0, "genisoimage", &["-quiet", "-o", "p.iso", "src"]);
    let mut m = mount(&s.0.join("p.iso"));
    let names: Vec<String> = m.fs.read_dir(&VfsPath::root()).unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.contains(&"HELLO.TXT".to_owned()) && names.contains(&"DOCS".to_owned()), "{names:?}");
    // Looked up without regard to case.
    assert_eq!(m.fs.read_to_vec(&VfsPath::parse("/docs/readme.md").unwrap()).unwrap(), b"# Read me\n");
    assert_eq!(m.fs.read_to_vec(&VfsPath::parse("/DOCS/BIG.BIN").unwrap()).unwrap(), noise(3 << 20, 1));
    assert_eq!(detail(m.fs.as_ref(), "Extensions"), "none (plain ISO 9660)");
}

/// El Torito (BIOS and UEFI images) and an isohybrid MBR.
#[test]
fn bootable_images() {
    if !have(&["xorriso"]) {
        return;
    }
    let s = Scratch::new("boot");
    let src = s.0.join("src");
    plain_source(&src);
    write(&src, "boot/bios.bin", &noise(2048, 5));
    write(&src, "boot/efi.img", &noise(1_474_560, 6));
    run(
        &s.0,
        "xorriso",
        &[
            "-as",
            "mkisofs",
            "-quiet",
            "-R",
            "-J",
            "-b",
            "boot/bios.bin",
            "-no-emul-boot",
            "-boot-load-size",
            "4",
            "-eltorito-alt-boot",
            "-e",
            "boot/efi.img",
            "-no-emul-boot",
            "--protective-msdos-label",
            "-o",
            "b.iso",
            "src",
        ],
    );
    let mut m = mount(&s.0.join("b.iso"));
    let boot = detail(m.fs.as_ref(), "Bootable");
    assert!(boot.starts_with("Yes") && boot.contains("BIOS") && boot.contains("UEFI"), "{boot}");
    assert!(detail(m.fs.as_ref(), "Extensions").contains("El Torito"));
    assert!(detail(m.fs.as_ref(), "Hybrid image").contains("MBR"));
    let mut images: Vec<String> =
        m.fs.read_dir(&VfsPath::parse("/[BOOT]").unwrap()).unwrap().into_iter().map(|e| e.name).collect();
    images.sort();
    assert_eq!(images, ["1-BIOS-NoEmul.img", "2-UEFI-NoEmul.img"]);
    assert_eq!(m.fs.read_to_vec(&VfsPath::parse("/[BOOT]/1-BIOS-NoEmul.img").unwrap()).unwrap(), noise(2048, 5));
    assert_eq!(m.fs.read_to_vec(&VfsPath::parse("/[BOOT]/2-UEFI-NoEmul.img").unwrap()).unwrap(), noise(1_474_560, 6));
    let got = tree_of(&s.0.join("b.iso"));
    assert_eq!(got["/boot/efi.img"], Item::File(noise(1_474_560, 6)));
}

/// zisofs-compressed files (xorriso's filter) read back exactly.
#[test]
fn zisofs() {
    if !have(&["xorriso"]) {
        return;
    }
    let s = Scratch::new("zisofs");
    let src = s.0.join("src");
    let text: Vec<u8> = (0..700_000u32).map(|i| b"compress me, please! "[(i % 21) as usize]).collect();
    let mut mixed = noise(70_000, 2);
    mixed.extend(vec![0u8; 200_000]);
    mixed.extend(noise(9_999, 3));
    write(&src, "text.txt", &text);
    write(&src, "mixed.bin", &mixed);
    write(&src, "tiny.txt", b"x");
    run(&s.0, "xorriso", &["-outdev", "z.iso", "-map", "src", "/", "-set_filter_r", "--zisofs", "/", "--"]);
    let mut m = mount(&s.0.join("z.iso"));
    assert_eq!(m.fs.read_to_vec(&VfsPath::parse("/text.txt").unwrap()).unwrap(), text);
    assert_eq!(m.fs.read_to_vec(&VfsPath::parse("/mixed.bin").unwrap()).unwrap(), mixed);
    assert_eq!(m.fs.read_to_vec(&VfsPath::parse("/tiny.txt").unwrap()).unwrap(), b"x");
    // Pieces across block boundaries.
    let mut buf = vec![0u8; 70_000];
    let n = m.fs.read(&VfsPath::parse("/mixed.bin").unwrap(), 30_000, &mut buf).unwrap();
    assert_eq!(&buf[..n], &mixed[30_000..30_000 + n]);
    let iso_len = std::fs::metadata(s.0.join("z.iso")).unwrap().len();
    assert!(iso_len < 600_000, "the files were not compressed ({iso_len} bytes)");
}

/// A UDF bridge image: the ISO 9660 side is read and UDF is reported.
#[test]
fn udf_is_reported() {
    if !have(&["genisoimage"]) {
        return;
    }
    let s = Scratch::new("udf");
    let src = s.0.join("src");
    plain_source(&src);
    run(&s.0, "genisoimage", &["-quiet", "-udf", "-R", "-J", "-o", "u.iso", "src"]);
    let m = mount(&s.0.join("u.iso"));
    assert!(detail(m.fs.as_ref(), "UDF").starts_with("present"));
    let mut want = BTreeMap::new();
    host_tree(&src, "", &mut want);
    assert_same("u.iso", &tree_of(&s.0.join("u.iso")), &want);
}

// ----- writing ------------------------------------------------------------------

fn output(dir: &Path, cmd: &str, args: &[&str]) -> String {
    let out = Command::new(tool(cmd).unwrap()).args(args).current_dir(dir).env("LC_ALL", "C.UTF-8").output().unwrap();
    assert!(out.status.success(), "{cmd} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

/// Copies a host folder into a volume.
fn copy_in(fs: &mut dyn FileSystem, host: &Path, rel: &str) {
    let mut entries: Vec<_> = std::fs::read_dir(host.join(rel.trim_start_matches('/'))).unwrap().flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = format!("{rel}/{}", e.file_name().to_string_lossy());
        let vp = VfsPath::parse(&path).unwrap();
        let t = e.file_type().unwrap();
        if t.is_symlink() {
            continue; // (no API to make links: added by the tools only)
        } else if t.is_dir() {
            fs.create_dir(&vp).unwrap();
            copy_in(fs, host, &path);
        } else {
            // In pieces, as the app copies large files.
            let data = std::fs::read(e.path()).unwrap();
            fs.create_file(&vp).unwrap();
            for (i, c) in data.chunks(1 << 20).enumerate() {
                fs.write_at(&vp, (i << 20) as u64, c).unwrap();
            }
        }
    }
}

fn open_rw(img: &Path) -> dix_core::Mounted {
    let dev = open_retry(img, AccessMode::ReadWrite);
    crate::default_registry().mount(Box::new(dev), Some("iso9660"), AccessMode::ReadWrite).unwrap()
}

/// What the tools extract, as a tree.
fn extracted(s: &Scratch, img: &Path, how: &str) -> BTreeMap<String, Item> {
    let out = s.0.join(format!("out-{how}"));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    let i = img.to_str().unwrap();
    let o = out.to_str().unwrap();
    match how {
        "xorriso" => drop(output(&s.0, "xorriso", &["-osirrox", "on", "-indev", i, "-extract", "/", o])),
        "bsdtar" => drop(output(&s.0, "bsdtar", &["-xf", i, "-C", o])),
        "7z" => drop(output(&s.0, "7z", &["x", "-y", &format!("-o{o}"), i])),
        _ => unreachable!(),
    }
    let mut t = BTreeMap::new();
    host_tree(&out, "", &mut t);
    t
}

#[test]
fn makes_isos_the_tools_read() {
    if !have(&["xorriso", "bsdtar", "7z", "isoinfo"]) {
        return;
    }
    let s = Scratch::new("make");
    let src = s.0.join("src");
    full_source(&src);
    let img = s.0.join("new.iso");
    {
        let mut dev = FileDevice::create(&img, 0).unwrap();
        let opts = dix_core::FormatOptions { label: Some("Made Here".into()), variant: Some("iso".into()) };
        crate::default_registry().format(&mut dev, "iso9660", &opts).unwrap();
    }
    // An empty image is fine for the tools.
    output(&s.0, "isoinfo", &["-d", "-i", img.to_str().unwrap()]);
    {
        let mut m = open_rw(&img);
        assert!(m.fs.read_dir(&VfsPath::root()).unwrap().is_empty());
        copy_in(m.fs.as_mut(), &src, "");
        m.fs.unmount().unwrap();
    }
    let mut want = BTreeMap::new();
    host_tree(&src, "", &mut want);
    want.retain(|_, v| !matches!(v, Item::Link(_)));
    // Rock Ridge, read by xorriso, libarchive and us.
    assert_same("xorriso", &extracted(&s, &img, "xorriso"), &want);
    assert_same("bsdtar", &extracted(&s, &img, "bsdtar"), &want);
    assert_same("ours", &tree_of(&img), &want);
    // Joliet (7-Zip reads it rather than Rock Ridge): long Unicode names.
    assert_same("7z", &extracted(&s, &img, "7z"), &want);
    // The plain ISO 9660 tree and the path tables.
    let plain = output(&s.0, "isoinfo", &["-l", "-i", img.to_str().unwrap()]);
    assert!(plain.contains("HELLO.TXT;1"), "{plain}");
    let report = output(&s.0, "xorriso", &["-indev", img.to_str().unwrap(), "-find", "/", "-type", "f"]);
    assert!(!report.contains("SORRY") && !report.contains("FAILURE"), "{report}");
    let m = mount(&img);
    assert_eq!(m.fs.info().label.as_deref(), Some("Made Here"));
    assert!(detail(m.fs.as_ref(), "Extensions").contains("Rock Ridge"));
    assert!(detail(m.fs.as_ref(), "Extensions").contains("Joliet"));
    // The image holds the files and little more.
    let content: u64 = want.values().map(|v| if let Item::File(d) = v { d.len() as u64 } else { 0 }).sum();
    let len = std::fs::metadata(&img).unwrap().len();
    assert!(len < content + (1 << 20), "{len} bytes for {content} bytes of files");
}

#[test]
fn changes_an_iso_made_by_the_tools() {
    if !have(&["genisoimage", "xorriso"]) {
        return;
    }
    let s = Scratch::new("change");
    let src = s.0.join("src");
    full_source(&src);
    run(&s.0, "genisoimage", &["-quiet", "-R", "-J", "-input-charset", "utf-8", "-o", "c.iso", "src"]);
    let img = s.0.join("c.iso");
    let p = |s: &str| VfsPath::parse(s).unwrap();
    {
        let mut m = open_rw(&img);
        let fs = m.fs.as_mut();
        fs.write_file(&p("/hello.txt"), b"changed\n").unwrap();
        fs.remove_file(&p("/empty.txt")).unwrap();
        fs.rename(&p("/docs"), &p("/papers")).unwrap();
        fs.create_dir(&p("/new")).unwrap();
        let big = noise(5 << 20, 9);
        fs.create_file(&p("/new/big.bin")).unwrap();
        for (i, c) in big.chunks(1 << 20).enumerate() {
            fs.write_at(&p("/new/big.bin"), (i << 20) as u64, c).unwrap();
        }
        // A write in the middle of an existing file.
        fs.write_at(&p("/papers/readme.md"), 2, b"XX").unwrap();
        fs.sync().unwrap();
        // Still usable after the rewrite, and changed again.
        fs.write_file(&p("/new/second.txt"), b"2").unwrap();
        m.fs.unmount().unwrap();
    }
    let mut want = BTreeMap::new();
    host_tree(&src, "", &mut want);
    let rename: Vec<String> = want.keys().filter(|k| k.starts_with("/docs")).cloned().collect();
    for k in rename {
        let v = want.remove(&k).unwrap();
        want.insert(k.replacen("/docs", "/papers", 1), v);
    }
    want.insert("/hello.txt".into(), Item::File(b"changed\n".to_vec()));
    want.remove("/empty.txt");
    want.insert("/new".into(), Item::Dir);
    want.insert("/new/big.bin".into(), Item::File(noise(5 << 20, 9)));
    want.insert("/new/second.txt".into(), Item::File(b"2".to_vec()));
    want.insert("/papers/readme.md".into(), Item::File(b"# XXad me\n".to_vec()));
    // The relocated deep folders come back in place; links are kept.
    let mut got = tree_of(&img);
    got.remove("/rr_moved");
    assert_same("ours", &got, &want);
    let mut x = extracted(&s, &img, "xorriso");
    x.remove("/rr_moved");
    assert_same("xorriso", &x, &want);
    // Permissions survive, and the hard link still shares its data.
    let mut m = mount(&img);
    let meta = m.fs.metadata(&p("/papers/readme.md")).unwrap();
    assert_eq!(meta.mode.map(|m| m & 0o777), Some(0o640));
    let (a, b) =
        (m.fs.metadata(&p("/papers/hard.txt")).unwrap(), m.fs.metadata(&p("/1/2/3/4/5/6/7/8/9/10/deep.txt")).unwrap());
    assert_eq!(a.size, b"hello ISO\n".len() as u64);
    assert_ne!(a.inode, b.inode);
    // No spooled data left behind.
    let len = std::fs::metadata(&img).unwrap().len();
    assert!(len < (5 << 20) + (3 << 20) + (1 << 20), "{len}");
}

#[test]
fn floppy_emulation_stays_read_only() {
    if !have(&["xorriso"]) {
        return;
    }
    let s = Scratch::new("boot-ro");
    let src = s.0.join("src");
    plain_source(&src);
    write(&src, "floppy.img", &noise(1_474_560, 1));
    run(&s.0, "xorriso", &["-as", "mkisofs", "-quiet", "-R", "-b", "floppy.img", "-o", "b.iso", "src"]);
    let dev = FileDevice::open(s.0.join("b.iso"), AccessMode::ReadWrite).unwrap();
    let e = crate::default_registry().mount(Box::new(dev), Some("iso9660"), AccessMode::ReadWrite).err().unwrap();
    assert_eq!(e.kind(), dix_core::ErrorKind::ReadOnly);
    assert!(e.to_string().contains("emulation"), "{e}");
}

// ----- booting ------------------------------------------------------------------

const ISOLINUX: &str = "/usr/lib/ISOLINUX/isolinux.bin";
const ISOHDPFX: &str = "/usr/lib/ISOLINUX/isohdpfx.bin";
const LDLINUX: &str = "/usr/lib/syslinux/modules/bios/ldlinux.c32";
const OVMF_CODE: &str = "/usr/share/OVMF/OVMF_CODE_4M.fd";
const OVMF_VARS: &str = "/usr/share/OVMF/OVMF_VARS_4M.fd";

/// A folder with isolinux (BIOS) and a GRUB UEFI image that both say who
/// booted them on the serial port. None when the tools are missing.
fn boot_source(s: &Scratch) -> Option<PathBuf> {
    if !have(&["grub-mkstandalone", "mkfs.fat", "mmd", "mcopy"])
        || ![ISOLINUX, ISOHDPFX, LDLINUX].iter().all(|p| Path::new(p).exists())
    {
        eprintln!("isolinux / GRUB / mtools missing: skipping boot test");
        return None;
    }
    let src = s.0.join("bootsrc");
    std::fs::create_dir_all(src.join("isolinux")).unwrap();
    std::fs::create_dir_all(src.join("efi")).unwrap();
    for f in [ISOLINUX, ISOHDPFX, LDLINUX] {
        std::fs::copy(f, src.join("isolinux").join(Path::new(f).file_name().unwrap())).unwrap();
    }
    std::fs::write(src.join("isolinux/isolinux.cfg"), "SERIAL 0 115200\nPROMPT 0\n").unwrap();
    write(&src, "README.txt", b"a bootable test image\n");
    std::fs::write(
        s.0.join("grub.cfg"),
        "serial --unit=0 --speed=115200\nterminal_output serial console\necho UEFI-BOOT-OK\nsleep 600\n",
    )
    .unwrap();
    run(
        &s.0,
        "grub-mkstandalone",
        &["-O", "x86_64-efi", "-o", "BOOTX64.EFI", "--modules=serial echo sleep", "boot/grub/grub.cfg=grub.cfg"],
    );
    let img = src.join("efi/efi.img");
    run(&s.0, "mkfs.fat", &["-C", img.to_str().unwrap(), "8192"]);
    let i = img.to_str().unwrap();
    run(&s.0, "mmd", &["-i", i, "::/EFI", "::/EFI/BOOT"]);
    run(&s.0, "mcopy", &["-i", i, "BOOTX64.EFI", "::/EFI/BOOT/"]);
    Some(src)
}

/// Boots `img` in QEMU (BIOS or UEFI, as a CD or as a disk) and waits for
/// `marker` on the serial port. None when QEMU / OVMF are missing.
fn boots(s: &Scratch, img: &Path, uefi: bool, as_cd: bool, marker: &str) -> Option<bool> {
    if tool("qemu-system-x86_64").is_none() || (uefi && !Path::new(OVMF_CODE).exists()) {
        eprintln!("QEMU / OVMF missing: not booting");
        return None;
    }
    let serial = s.0.join(format!("serial-{uefi}-{as_cd}.txt"));
    let vars = s.0.join("vars.fd");
    let mut args: Vec<String> =
        ["-m", "256", "-display", "none", "-no-reboot", "-net", "none"].map(String::from).to_vec();
    args.extend(["-serial".into(), format!("file:{}", serial.display())]);
    if uefi {
        std::fs::copy(OVMF_VARS, &vars).unwrap();
        args.extend(["-drive".into(), format!("if=pflash,format=raw,readonly=on,file={OVMF_CODE}")]);
        args.extend(["-drive".into(), format!("if=pflash,format=raw,file={}", vars.display())]);
    }
    if as_cd {
        args.extend(["-drive".into(), format!("file={},media=cdrom,readonly=on,if=ide", img.display())]);
    } else {
        args.extend(["-snapshot".into(), "-drive".into(), format!("file={},format=raw,if=ide", img.display())]);
    }
    let mut child = Command::new(tool("qemu-system-x86_64").unwrap())
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut ok = false;
    for _ in 0..240 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if std::fs::read(&serial).is_ok_and(|b| String::from_utf8_lossy(&b).contains(marker)) {
            ok = true;
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Some(ok)
}

fn boot_settings(fs: &mut dyn FileSystem, hybrid: bool, gpt: bool) {
    let set = |k: &str, v: &str| (k.to_owned(), v.to_owned());
    fs.apply_settings(&[
        set("label", "BOOT TEST"),
        set("bios", "/isolinux/isolinux.bin"),
        set("load", "4"),
        set("info_table", "true"),
        set("efi", "/efi/efi.img"),
        set("hybrid", &hybrid.to_string()),
        set("gpt", &gpt.to_string()),
        set("mbr", if hybrid { "/isolinux/isohdpfx.bin" } else { "" }),
    ])
    .unwrap();
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// Checks the El Torito entries (as xorriso reports them), the boot info
/// table, the hybrid MBR, and that it boots four ways.
fn check_bootable(s: &Scratch, img: &Path, gpt: bool) {
    let report = output(&s.0, "xorriso", &["-indev", img.to_str().unwrap(), "-report_el_torito", "plain"]);
    assert!(report.contains("/isolinux/isolinux.bin") && report.contains("/efi/efi.img"), "{report}");
    let line =
        |p: &str| report.lines().find(|l| l.starts_with("El Torito boot img") && l.contains(p)).unwrap().to_owned();
    let (bios, efi) = (line("BIOS"), line("UEFI"));
    assert!(bios.split_whitespace().any(|w| w == "4"), "{bios}");
    assert!(efi.split_whitespace().any(|w| w == "16384"), "{efi}");
    // The boot info table, against the file's own bytes.
    let mut m = mount(img);
    let path = VfsPath::parse("/isolinux/isolinux.bin").unwrap();
    let patched = m.fs.read_to_vec(&path).unwrap();
    let lba = m.fs.metadata(&path).unwrap().inode.unwrap();
    let orig = std::fs::read(ISOLINUX).unwrap();
    let sum = orig[64..].chunks(4).fold(0u32, |s, c| {
        let mut w = [0u8; 4];
        w[..c.len()].copy_from_slice(c);
        s.wrapping_add(u32::from_le_bytes(w))
    });
    assert_eq!((le32(&patched, 8), le32(&patched, 12) as u64), (16, lba));
    assert_eq!((le32(&patched, 16) as usize, le32(&patched, 20)), (orig.len(), sum));
    assert_eq!(&patched[64..], &orig[64..]);
    let boot = detail(m.fs.as_ref(), "Bootable");
    assert!(boot.contains("BIOS") && boot.contains("UEFI"), "{boot}");
    assert!(detail(m.fs.as_ref(), "Hybrid image").contains("MBR"));
    drop(m);
    let len = std::fs::metadata(img).unwrap().len();
    assert_eq!(len % (1 << 20), 0, "a hybrid image is whole MiB");
    let head = std::fs::read(img).unwrap();
    // The MBR keeps the BIOS boot code, with the boot image's address.
    assert_eq!(le32(&head, 432) as u64, lba * 4);
    assert_eq!(&head[..400], &std::fs::read(ISOHDPFX).unwrap()[..400]);
    let dump = output(&s.0, "sfdisk", &["-d", img.to_str().unwrap()]);
    let efi_at = mount(img).fs.metadata(&VfsPath::parse("/efi/efi.img").unwrap()).unwrap().inode.unwrap() * 4;
    if gpt {
        // A protective MBR (active, for picky BIOSes) and a valid GPT.
        assert_eq!((head[446], head[450]), (0x80, 0xEE));
        assert!(dump.contains("label: gpt"), "{dump}");
        let line =
            |name: &str| dump.lines().find(|l| l.contains(name)).unwrap_or_else(|| panic!("{name}: {dump}")).to_owned();
        let iso = line("name=\"ISO9660\"");
        assert!(
            iso.contains("start=          64") && iso.contains("type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"),
            "{iso}"
        );
        let esp = line("name=\"EFI boot\"");
        assert!(esp.contains(&format!("start={efi_at:>12}")), "{esp}");
        assert!(
            esp.contains("size=       16384") && esp.contains("type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
            "{esp}"
        );
        let v = output(&s.0, "sgdisk", &["-v", img.to_str().unwrap()]);
        assert!(v.contains("No problems found"), "{v}");
        let v = output(&s.0, "sfdisk", &["--verify", img.to_str().unwrap()]);
        assert!(v.contains("No errors detected"), "{v}");
        let m = mount(img);
        assert!(detail(m.fs.as_ref(), "Hybrid image").contains("GPT"));
    } else {
        // isohybrid's layout: partition 1 over the image, 2 over the UEFI image.
        assert!(dump.contains(&format!("start=           0, size={:>12}, type=17, bootable", len / 512)), "{dump}");
        assert!(dump.contains(&format!("start={efi_at:>12}, size=       16384, type=ef")), "{dump}");
    }
    for (uefi, cd, marker) in [
        (false, true, "ISOLINUX"),
        (false, false, "ISOLINUX"),
        (true, true, "UEFI-BOOT-OK"),
        (true, false, "UEFI-BOOT-OK"),
    ] {
        if let Some(ok) = boots(s, img, uefi, cd, marker) {
            assert!(
                ok,
                "{} did not boot by {} as a {}",
                img.display(),
                if uefi { "UEFI" } else { "BIOS" },
                if cd { "CD" } else { "disk" }
            );
        }
    }
}

#[test]
fn makes_bootable_isos() {
    if !have(&["xorriso", "sfdisk"]) {
        return;
    }
    let s = Scratch::new("mkboot");
    let Some(src) = boot_source(&s) else { return };
    let img = s.0.join("boot.iso");
    {
        let mut dev = FileDevice::create(&img, 0).unwrap();
        let opts = dix_core::FormatOptions { label: None, variant: Some("iso".into()) };
        crate::default_registry().format(&mut dev, "iso9660", &opts).unwrap();
    }
    {
        let mut m = open_rw(&img);
        copy_in(m.fs.as_mut(), &src, "");
        // Settings are checked.
        let bad = [("bios".to_owned(), "/nope.bin".to_owned())];
        assert!(m.fs.apply_settings(&bad).is_err());
        let bad = [("hybrid".to_owned(), "true".to_owned())];
        assert!(m.fs.apply_settings(&bad).is_err(), "hybrid without a boot image");
        // New hybrids get a GPT unless asked not to.
        assert!(m.fs.settings().iter().any(|s| s.key == "gpt" && s.value == "true"));
        m.fs.unmount().unwrap();
    }
    for gpt in [true, false] {
        let copy = s.0.join(format!("boot-{gpt}.iso"));
        std::fs::copy(&img, &copy).unwrap();
        let mut m = open_rw(&copy);
        boot_settings(m.fs.as_mut(), true, gpt);
        m.fs.unmount().unwrap();
        drop(m);
        let m = mount(&copy);
        assert_eq!(m.fs.info().label.as_deref(), Some("BOOT TEST"));
        drop(m);
        check_bootable(&s, &copy, gpt);
    }
}

/// An isohybrid image made by xorriso, changed: it still boots.
#[test]
fn changing_a_bootable_iso_keeps_it_bootable() {
    if !have(&["xorriso", "sfdisk"]) {
        return;
    }
    let s = Scratch::new("keepboot");
    if boot_source(&s).is_none() {
        return;
    }
    // MBR-only (isohybrid) and with xorriso's GPT.
    for gpt in [false, true] {
        let name = format!("x-{gpt}.iso");
        let mut args = vec![
            "-as",
            "mkisofs",
            "-quiet",
            "-R",
            "-J",
            "-V",
            "BOOT TEST",
            "-b",
            "isolinux/isolinux.bin",
            "-no-emul-boot",
            "-boot-load-size",
            "4",
            "-boot-info-table",
            "-isohybrid-mbr",
            ISOHDPFX,
            "-eltorito-alt-boot",
            "-e",
            "efi/efi.img",
            "-no-emul-boot",
            "-o",
            &name,
            "bootsrc",
        ];
        if gpt {
            args.insert(args.len() - 3, "-isohybrid-gpt-basdat");
        }
        run(&s.0, "xorriso", &args);
        change_and_check(&s, &s.0.join(&name), gpt);
    }
}

fn change_and_check(s: &Scratch, img: &Path, gpt: bool) {
    let img = img.to_owned();
    {
        let mut m = open_rw(&img);
        let settings: BTreeMap<String, String> = m.fs.settings().into_iter().map(|s| (s.key, s.value)).collect();
        assert_eq!(settings["bios"], "/isolinux/isolinux.bin");
        assert_eq!(settings["efi"], "/efi/efi.img");
        assert_eq!((settings["load"].as_str(), settings["info_table"].as_str()), ("4", "true"));
        assert_eq!(settings["hybrid"], "true");
        assert_eq!(settings["gpt"], gpt.to_string());
        assert!(!settings["mbr"].is_empty());
        m.fs.write_file(&VfsPath::parse("/added.txt").unwrap(), b"added after").unwrap();
        m.fs.unmount().unwrap();
    }
    let mut m = mount(&img);
    assert_eq!(m.fs.read_to_vec(&VfsPath::parse("/added.txt").unwrap()).unwrap(), b"added after");
    drop(m);
    check_bootable(s, &img, gpt);
}

// ----- UDF bridge images ------------------------------------------------------------

/// The UDF side of an image, as a tree.
#[cfg(feature = "udf")]
fn udf_tree_of(img: &Path) -> BTreeMap<String, Item> {
    let dev = open_retry(img, AccessMode::ReadOnly);
    let mut m = crate::default_registry().mount(Box::new(dev), Some("udf"), AccessMode::ReadOnly).unwrap();
    let mut t = BTreeMap::new();
    fs_tree(m.fs.as_mut(), "/", &mut t);
    t
}

/// udfinfo's key=value lines.
#[cfg(feature = "udf")]
fn udfinfo(s: &Scratch, img: &Path) -> BTreeMap<String, String> {
    output(&s.0, "udfinfo", &[img.to_str().unwrap()])
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// 7-Zip's extraction of the UDF side.
#[cfg(feature = "udf")]
fn extracted_udf(s: &Scratch, img: &Path) -> BTreeMap<String, Item> {
    let out = s.0.join("out-udf");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    output(&s.0, "7z", &["x", "-y", "-tudf", &format!("-o{}", out.display()), img.to_str().unwrap()]);
    let mut t = BTreeMap::new();
    host_tree(&out, "", &mut t);
    t
}

/// *New image → ISO 9660 + UDF*: both sides hold the files (read by us,
/// 7-Zip and udfinfo), stay in step when the image changes, and UDF can
/// be turned off.
#[test]
#[cfg(feature = "udf")]
fn makes_udf_bridge_images() {
    if !have(&["7z", "udfinfo", "xorriso"]) {
        return;
    }
    let s = Scratch::new("udf-make");
    let src = s.0.join("src");
    full_source(&src);
    // A folder too large to fit in its file entry, with names UDF shortens.
    for i in 0..120 {
        write(&src, &format!("many/entry number {i:03} with a longish name.dat"), &noise(i * 50, i as u32));
    }
    let img = s.0.join("bridge.iso");
    {
        let mut dev = FileDevice::create(&img, 0).unwrap();
        let opts = dix_core::FormatOptions { label: Some("Bridge".into()), variant: Some("iso-udf".into()) };
        crate::default_registry().format(&mut dev, "iso9660", &opts).unwrap();
    }
    let info = udfinfo(&s, &img);
    assert_eq!(info["label"], "Bridge");
    assert_eq!(info["udfrev"], "1.02");
    {
        let mut m = open_rw(&img);
        copy_in(m.fs.as_mut(), &src, "");
        m.fs.unmount().unwrap();
    }
    let mut want = BTreeMap::new();
    host_tree(&src, "", &mut want);
    want.retain(|_, v| !matches!(v, Item::Link(_)));
    assert_same("ISO 9660 side", &tree_of(&img), &want);
    assert_same("UDF side (ours)", &udf_tree_of(&img), &want);
    assert_same("UDF side (7-Zip)", &extracted_udf(&s, &img), &want);
    let report = output(&s.0, "xorriso", &["-indev", img.to_str().unwrap(), "-find", "/", "-type", "f"]);
    assert!(!report.contains("SORRY") && !report.contains("FAILURE"), "{report}");
    let info = udfinfo(&s, &img);
    let files = want.values().filter(|v| matches!(v, Item::File(_))).count();
    let dirs = want.values().filter(|v| matches!(v, Item::Dir)).count() + 1;
    assert_eq!((info["numfiles"].parse::<usize>().unwrap(), info["numdirs"].parse::<usize>().unwrap()), (files, dirs));
    assert_eq!(info["integrity"], "closed");

    // Changed: both sides follow.
    {
        let mut m = open_rw(&img);
        m.fs.write_file(&VfsPath::parse("/hello.txt").unwrap(), b"changed").unwrap();
        m.fs.remove_file(&VfsPath::parse("/docs/big.bin").unwrap()).unwrap();
        m.fs.create_dir(&VfsPath::parse("/new folder").unwrap()).unwrap();
        m.fs.unmount().unwrap();
    }
    want.insert("/hello.txt".into(), Item::File(b"changed".to_vec()));
    want.remove("/docs/big.bin");
    want.insert("/new folder".into(), Item::Dir);
    assert_same("ISO 9660 side, changed", &tree_of(&img), &want);
    assert_same("UDF side, changed", &extracted_udf(&s, &img), &want);

    // UDF off: a plain ISO again.
    {
        let mut m = open_rw(&img);
        m.fs.apply_settings(&[("udf".into(), "false".into())]).unwrap();
        m.fs.unmount().unwrap();
    }
    let mut dev = open_retry(&img, AccessMode::ReadOnly);
    assert_eq!(dix_core::Driver::probe(&crate::udf::UdfDriver, &mut dev).unwrap(), 0);
    assert_same("UDF off", &tree_of(&img), &want);
}

/// A bridge image changes only when its UDF side holds nothing the ISO
/// 9660 side lacks: genisoimage's can (and its UDF side then follows), one
/// with a file only in UDF (as Windows install images) cannot.
#[test]
#[cfg(feature = "udf")]
fn changes_bridge_images_only_when_both_sides_agree() {
    if !have(&["genisoimage", "7z"]) {
        return;
    }
    let s = Scratch::new("udf-change");
    let src = s.0.join("src");
    plain_source(&src);
    run(&s.0, "genisoimage", &["-quiet", "-input-charset", "utf-8", "-udf", "-R", "-J", "-o", "g.iso", "src"]);
    let img = s.0.join("g.iso");
    {
        let mut m = open_rw(&img);
        m.fs.write_file(&VfsPath::parse("/added.txt").unwrap(), b"added").unwrap();
        m.fs.unmount().unwrap();
    }
    let mut want = BTreeMap::new();
    host_tree(&src, "", &mut want);
    want.insert("/added.txt".into(), Item::File(b"added".to_vec()));
    assert_same("genisoimage, changed (UDF)", &extracted_udf(&s, &img), &want);
    assert_same("genisoimage, changed (ISO)", &tree_of(&img), &want);

    let ok = Command::new("python3").args(["-c", "import pycdlib"]).output().is_ok_and(|o| o.status.success());
    if !ok {
        return;
    }
    let script = r#"
import io, sys, pycdlib
iso = pycdlib.PyCdlib()
iso.new(udf='2.60', joliet=3)
iso.add_fp(io.BytesIO(b'readme\n'), 7, '/README.TXT;1', joliet_path='/README.TXT', udf_path='/README.TXT')
iso.add_fp(io.BytesIO(b'install\n'), 8, udf_path='/install.wim')
iso.write(sys.argv[1])
"#;
    let win = s.0.join("win.iso");
    let out = Command::new("python3").args(["-c", script]).arg(&win).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let dev = open_retry(&win, AccessMode::ReadWrite);
    let e = crate::default_registry().mount(Box::new(dev), Some("iso9660"), AccessMode::ReadWrite).err().unwrap();
    assert!(e.to_string().contains("/install.wim"), "{e}");
}
