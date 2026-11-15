//! VM disks written by `qemu-img` (every subformat it makes), read back
//! here and compared with what `qemu-img` reads; writes made here are
//! checked with `qemu-img check` and read back by `qemu-img`.

use dix_core::device::host::FileDevice;
use dix_core::{AccessMode, BlockDevice, DriverRegistry, VfsPath};
use std::path::{Path, PathBuf};
use std::process::Command;

fn have_qemu() -> bool {
    let ok = Command::new("qemu-img").arg("--version").output().is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("qemu-img not installed: skipping");
    }
    ok
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("dix-vdisk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn registry() -> DriverRegistry {
    crate::default_registry()
}

/// Opens a VM disk the way the app does: the file, then its container.
fn open(reg: &DriverRegistry, path: &Path, mode: AccessMode) -> Box<dyn BlockDevice> {
    let dir = path.parent().unwrap().to_path_buf();
    let opener = move |name: &str, writable: bool| -> dix_core::Result<Box<dyn BlockDevice>> {
        let mode = if writable { AccessMode::ReadWrite } else { AccessMode::ReadOnly };
        Ok(Box::new(FileDevice::open(dir.join(name), mode)?) as Box<dyn BlockDevice>)
    };
    let dev = Box::new(FileDevice::open(path, mode).unwrap());
    reg.open_disk(dev, Some(&opener)).unwrap()
}

fn read_all(dev: &mut dyn BlockDevice) -> Vec<u8> {
    let mut out = vec![0u8; dev.size() as usize];
    // In uneven pieces, to cross block boundaries at odd places.
    let mut at = 0usize;
    let mut step = 70_001usize;
    while at < out.len() {
        let n = step.min(out.len() - at);
        dev.read_at(at as u64, &mut out[at..at + n]).unwrap();
        at += n;
        step = step * 3 % 500_000 + 1;
    }
    out
}

/// What qemu-img reads from `path`.
fn qemu_read(s: &Scratch, path: &Path) -> Vec<u8> {
    let raw = s.path("qemu-view.raw");
    run(Command::new("qemu-img").args(["convert", "-O", "raw"]).arg(path).arg(&raw));
    let v = std::fs::read(&raw).unwrap();
    std::fs::remove_file(raw).unwrap();
    v
}

/// QEMU shows a fixed VHD's footer as the disk's last sector; the footer
/// says the disk ends before it.
fn qemu_fixed_vhd(expected: &mut Vec<u8>, dev: &dyn BlockDevice) {
    if dev.container().is_some_and(|c| c == "VHD, fixed") && expected.len() as u64 == dev.size() + 512 {
        expected.truncate(dev.size() as usize);
    }
}

const SIZE: usize = 24 << 20;

/// A disk with data here and there and zeros between.
fn sample() -> Vec<u8> {
    let mut v = vec![0u8; SIZE];
    let mut x = 0x1234_5678u32;
    for (start, len) in [(0usize, 1 << 20), (5 << 20, 300_000), ((20 << 20) - 1000, 6000), (SIZE - 4096, 4096)] {
        for b in &mut v[start..start + len] {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            // Half of it compressible.
            *b = if x & 0x100 != 0 { (x >> 24) as u8 } else { b'a' + (x % 7) as u8 };
        }
    }
    v
}

fn convert(s: &Scratch, raw: &Path, fmt: &str, opts: &[&str], name: &str) -> PathBuf {
    let out = s.path(name);
    run(Command::new("qemu-img").args(["convert", "-f", "raw", "-O", fmt]).args(opts).arg(raw).arg(&out));
    out
}

const VARIANTS: &[(&str, &[&str], &str)] = &[
    ("qcow2", &[], "qcow2"),
    ("qcow2", &["-c"], "qcow2"),
    ("qcow2", &["-c", "-o", "compression_type=zstd"], "qcow2"),
    ("qcow2", &["-o", "compat=0.10"], "qcow2"),
    ("qcow2", &["-c", "-o", "compat=0.10,cluster_size=4096"], "qcow2"),
    ("qcow2", &["-o", "cluster_size=2M"], "qcow2"),
    ("qcow2", &["-o", "refcount_bits=64"], "qcow2"),
    ("vpc", &[], "VHD"),
    ("vpc", &["-o", "subformat=fixed"], "VHD"),
    ("vpc", &["-o", "force_size=on"], "VHD"),
    ("vhdx", &[], "VHDX"),
    ("vhdx", &["-o", "subformat=fixed"], "VHDX"),
    ("vhdx", &["-o", "block_size=8M,log_size=1M"], "VHDX"),
    ("vhdx", &["-o", "block_state_zero=off"], "VHDX"),
    ("vmdk", &[], "VMDK"),
    ("vmdk", &["-o", "subformat=monolithicFlat"], "VMDK"),
    ("vmdk", &["-o", "subformat=twoGbMaxExtentSparse"], "VMDK"),
    ("vmdk", &["-o", "subformat=twoGbMaxExtentFlat"], "VMDK"),
    ("vmdk", &["-o", "subformat=streamOptimized"], "VMDK"),
    ("vmdk", &["-o", "zeroed_grain=on"], "VMDK"),
    ("vdi", &[], "VDI"),
    ("vdi", &["-o", "static=on"], "VDI"),
];

fn ext(fmt: &str) -> &str {
    match fmt {
        "vpc" => "vhd",
        f => f,
    }
}

#[test]
fn reads_what_qemu_writes() {
    if !have_qemu() {
        return;
    }
    let s = Scratch::new("read");
    let raw = s.path("disk.raw");
    let data = sample();
    std::fs::write(&raw, &data).unwrap();
    let reg = registry();
    for (i, (fmt, opts, label)) in VARIANTS.iter().enumerate() {
        let path = convert(&s, &raw, fmt, opts, &format!("d{i}.{}", ext(fmt)));
        let mut expected = qemu_read(&s, &path);
        let mut dev = open(&reg, &path, AccessMode::ReadOnly);
        let what = dev.container().unwrap_or_default();
        qemu_fixed_vhd(&mut expected, dev.as_ref());
        assert!(what.starts_with(label), "{fmt} {opts:?}: {what}");
        assert_eq!(dev.size(), expected.len() as u64, "{fmt} {opts:?}: size");
        let got = read_all(dev.as_mut());
        assert!(got == expected, "{fmt} {opts:?} ({what}): content differs");
        // qemu pads some formats; the data is all there.
        assert_eq!(&got[..SIZE.min(got.len())], &data[..SIZE.min(got.len())], "{fmt} {opts:?}");
    }
}

#[test]
fn raw_images_are_left_alone() {
    let reg = registry();
    let dev = Box::new(dix_core::device::MemoryDevice::new("x", sample()));
    let dev = reg.open_disk(dev, None).unwrap();
    assert!(dev.container().is_none());
    assert_eq!(dev.size(), SIZE as u64);
}

/// Differencing disks over a base, both made by qemu: what the child does
/// not have comes from the base.
#[test]
fn reads_backing_chains() {
    if !have_qemu() {
        return;
    }
    let s = Scratch::new("chain");
    let raw = s.path("disk.raw");
    std::fs::write(&raw, sample()).unwrap();
    let reg = registry();
    for (fmt, opts) in [("qcow2", &[][..]), ("qcow2", &["-c"][..]), ("vmdk", &[][..])] {
        convert(&s, &raw, fmt, opts, &format!("base.{fmt}"));
        let mid = s.path(&format!("mid.{fmt}"));
        let top = s.path(&format!("top.{fmt}"));
        let _ = std::fs::remove_file(&mid);
        let _ = std::fs::remove_file(&top);
        run(Command::new("qemu-img").args(["create", "-f", fmt, "-F", fmt, "-b"]).arg(format!("base.{fmt}")).arg(&mid));
        run(Command::new("qemu-io")
            .args(["-f", fmt, "-c", "write -P 0xab 1000k 200k", "-c", "write -z 5M 4k"])
            .arg(&mid));
        run(Command::new("qemu-img").args(["create", "-f", fmt, "-F", fmt, "-b"]).arg(format!("mid.{fmt}")).arg(&top));
        run(Command::new("qemu-io")
            .args(["-f", fmt, "-c", "write -P 0xcd 1100k 3k", "-c", "write -P 0x11 23M 64k"])
            .arg(&top));
        let expected = qemu_read(&s, &top);
        let mut dev = open(&reg, &top, AccessMode::ReadOnly);
        assert!(dev.container().unwrap().contains("mid"), "{:?}", dev.container());
        assert!(read_all(dev.as_mut()) == expected, "{fmt} {opts:?}: chain content differs");
    }
    // A raw backing file.
    let top = s.path("over-raw.qcow2");
    run(Command::new("qemu-img").args(["create", "-f", "qcow2", "-F", "raw", "-b", "disk.raw"]).arg(&top));
    run(Command::new("qemu-io").args(["-f", "qcow2", "-c", "write -P 0x5a 3M 100k"]).arg(&top));
    let expected = qemu_read(&s, &top);
    assert!(read_all(open(&reg, &top, AccessMode::ReadOnly).as_mut()) == expected);
}

/// Writes scattered over the disk: across blocks, into blocks already
/// there and into new ones, at odd offsets.
fn scribble(dev: &mut dyn BlockDevice, expected: &mut [u8], seed: u32, count: usize) {
    let mut x = seed;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        x
    };
    let size = expected.len() as u64;
    for k in 0..count {
        let len = match k % 4 {
            0 => 512,
            1 => (next() % 9000) as usize + 1,
            2 => (next() % 300_000) as usize + 1,
            _ => 4096,
        };
        let at = next() as u64 % (size - len as u64);
        let at = if k % 4 == 3 { at / 4096 * 4096 } else { at };
        let fill: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        dev.write_at(at, &fill).unwrap();
        expected[at as usize..at as usize + len].copy_from_slice(&fill);
    }
    dev.flush().unwrap();
}

fn qemu_check(path: &Path, fmt: &str) {
    if matches!(fmt, "qcow2" | "vdi" | "vmdk" | "vhdx") {
        let out = Command::new("qemu-img").args(["check", "-f", fmt]).arg(path).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success() && !text.contains("leak"), "qemu-img check {fmt}: {text}");
    }
}

#[test]
fn writes_that_qemu_reads() {
    if !have_qemu() {
        return;
    }
    let s = Scratch::new("write");
    let raw = s.path("disk.raw");
    std::fs::write(&raw, sample()).unwrap();
    let reg = registry();
    let writable: &[(&str, &[&str])] = &[
        ("qcow2", &[]),
        ("qcow2", &["-c"]),
        ("qcow2", &["-o", "compat=0.10"]),
        ("qcow2", &["-o", "refcount_bits=8"]),
        ("vpc", &[]),
        ("vpc", &["-o", "subformat=fixed"]),
        ("vdi", &[]),
        ("vdi", &["-o", "static=on"]),
        ("vmdk", &[]),
        ("vmdk", &["-o", "subformat=twoGbMaxExtentSparse"]),
        ("vmdk", &["-o", "subformat=monolithicFlat"]),
        ("vhdx", &[]),
        ("vhdx", &["-o", "block_size=8M"]),
    ];
    for (i, (fmt, opts)) in writable.iter().enumerate() {
        let path = convert(&s, &raw, fmt, opts, &format!("w{i}.{}", ext(fmt)));
        let mut expected = qemu_read(&s, &path);
        let full = expected.len();
        {
            let mut dev = open(&reg, &path, AccessMode::ReadWrite);
            qemu_fixed_vhd(&mut expected, dev.as_ref());
            assert!(!dev.is_read_only(), "{fmt} {opts:?}: {:?}", dev.container());
            scribble(dev.as_mut(), &mut expected, 7 + i as u32, 60);
            assert!(read_all(dev.as_mut()) == expected, "{fmt} {opts:?}: reading back own writes");
        }
        qemu_check(&path, fmt);
        let mut back = qemu_read(&s, &path);
        assert_eq!(back.len(), full);
        back.truncate(expected.len());
        assert!(back == expected, "{fmt} {opts:?}: qemu reads something else");
    }
}

/// Small clusters: the refcount table fills up and is moved.
#[test]
fn qcow2_grows_its_refcount_table() {
    if !have_qemu() {
        return;
    }
    let s = Scratch::new("refcounts");
    let path = s.path("small.qcow2");
    run(Command::new("qemu-img").args(["create", "-f", "qcow2", "-o", "cluster_size=512"]).arg(&path).arg("24M"));
    let reg = registry();
    let mut expected = vec![0u8; SIZE];
    {
        let mut dev = open(&reg, &path, AccessMode::ReadWrite);
        let chunk: Vec<u8> = (0..1 << 20).map(|i| (i % 251) as u8).collect();
        for k in 0..20u64 {
            dev.write_at(k << 20, &chunk).unwrap();
            expected[(k as usize) << 20..((k as usize) + 1) << 20].copy_from_slice(&chunk);
        }
        scribble(dev.as_mut(), &mut expected, 99, 40);
    }
    qemu_check(&path, "qcow2");
    assert!(qemu_read(&s, &path) == expected);
}

#[test]
fn differencing_and_compressed_disks_stay_read_only() {
    if !have_qemu() {
        return;
    }
    let s = Scratch::new("ro");
    let raw = s.path("disk.raw");
    std::fs::write(&raw, sample()).unwrap();
    let reg = registry();
    // A stream-optimized VMDK, and a qcow2 with an internal snapshot.
    for (fmt, opts) in [("vmdk", &["-o", "subformat=streamOptimized"][..]), ("qcow2", &[][..])] {
        let path = convert(&s, &raw, fmt, opts, &format!("ro.{fmt}"));
        if fmt == "qcow2" {
            run(Command::new("qemu-img").args(["snapshot", "-c", "snap"]).arg(&path));
        }
        let mut dev = open(&reg, &path, AccessMode::ReadWrite);
        assert!(dev.is_read_only(), "{fmt}");
        assert!(dev.write_at(0, b"x").is_err());
    }
    // Opening one read-write says why it cannot be.
    let path = s.path("ro.vmdk");
    let dev = open(&reg, &path, AccessMode::ReadWrite);
    let rw = reg.drivers().iter().find(|d| d.supports_mode(AccessMode::ReadWrite)).unwrap().info().id;
    let err = reg.mount(dev, Some(rw), AccessMode::ReadWrite).err().unwrap();
    assert!(err.to_string().contains("streamOptimized"), "{err}");
}

/// A whole stack: ext4 inside a qcow2 and a VMDK, mounted read-write; the
/// result is checked by e2fsck on what qemu reads.
#[cfg(feature = "ext4")]
#[test]
fn ext4_inside_vm_disks() {
    if !have_qemu() || Command::new("mkfs.ext4").arg("-V").output().is_err() {
        return;
    }
    let s = Scratch::new("ext4");
    let src = s.path("src");
    std::fs::create_dir_all(src.join("dir")).unwrap();
    std::fs::write(src.join("dir/hello.txt"), b"hello from inside a VM disk\n").unwrap();
    let raw = s.path("fs.raw");
    run(Command::new("mkfs.ext4").args(["-q", "-F", "-d"]).arg(&src).arg(&raw).arg("32M"));
    let reg = registry();
    for fmt in ["qcow2", "vmdk", "vdi", "vpc"] {
        let path = convert(&s, &raw, fmt, &[], &format!("fs.{}", ext(fmt)));
        let dev = open(&reg, &path, AccessMode::ReadWrite);
        let mut m = reg.mount(dev, None, AccessMode::ReadWrite).unwrap();
        assert_eq!(m.driver.info().id, "ext4");
        let p = VfsPath::parse("/dir/hello.txt").unwrap();
        let mut b = vec![0u8; 64];
        let n = m.fs.read(&p, 0, &mut b).unwrap();
        assert_eq!(&b[..n], b"hello from inside a VM disk\n");
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i * 31 % 253) as u8).collect();
        let q = VfsPath::parse("/big.bin").unwrap();
        m.fs.create_file(&q).unwrap();
        m.fs.write_at(&q, 0, &big).unwrap();
        m.fs.sync().unwrap();
        drop(m);
        qemu_check(&path, fmt);
        let out = s.path("check.raw");
        run(Command::new("qemu-img").args(["convert", "-O", "raw"]).arg(&path).arg(&out));
        run(Command::new("e2fsck").args(["-fn"]).arg(&out));
        let dumped = s.path("big.out");
        run(Command::new("debugfs").arg("-R").arg(format!("dump /big.bin {}", dumped.display())).arg(&out));
        assert!(std::fs::read(&dumped).unwrap() == big, "{fmt}: the file written");
    }
}

/// A VHDX whose log still holds a change (as after a crash): a log entry
/// that rewrites the BAT page so that block 1 shows block 0's data. It is
/// replayed in memory; the file is not changed.
#[test]
fn vhdx_log_is_replayed() {
    if !have_qemu() {
        return;
    }
    use super::vhdx::checksum;
    let s = Scratch::new("vhdxlog");
    let path = s.path("l.vhdx");
    run(Command::new("qemu-img").args(["create", "-f", "vhdx", "-o", "block_size=1M"]).arg(&path).arg("64M"));
    run(Command::new("qemu-io").args(["-f", "vhdx", "-c", "write -P 0xab 0 1M"]).arg(&path));
    let mut f = std::fs::read(&path).unwrap();
    let le32 = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    let le64 = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    // The BAT, from the region table.
    let bat_guid = [0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42, 0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08];
    let r = &f[192 * 1024..256 * 1024];
    let bat = (0..le32(r, 8) as usize)
        .map(|i| &r[16 + i * 32..48 + i * 32])
        .find(|e| e[..16] == bat_guid)
        .map(|e| le64(e, 16) as usize)
        .unwrap();
    let mut page = f[bat..bat + 4096].to_vec();
    assert_eq!(le64(&page, 0) & 7, 6, "block 0 is present");
    assert_ne!(le64(&page, 8) & 7, 6, "block 1 is not");
    let e0 = page[..8].to_vec();
    page[8..16].copy_from_slice(&e0);
    // The current header, and its log.
    let h_at = if le64(&f, 65536 + 8) > le64(&f, 131072 + 8) { 65536 } else { 131072 };
    let (log_len, log_at) = (le32(&f, h_at + 68) as usize, le64(&f, h_at + 72) as usize);
    let guid = [7u8; 16];
    let seq = 0x1_0000_0005u64;
    let mut entry = vec![0u8; 8192];
    entry[..4].copy_from_slice(b"loge");
    entry[8..12].copy_from_slice(&8192u32.to_le_bytes());
    entry[16..24].copy_from_slice(&seq.to_le_bytes());
    entry[24..28].copy_from_slice(&1u32.to_le_bytes());
    entry[32..48].copy_from_slice(&guid);
    entry[48..56].copy_from_slice(&(f.len() as u64).to_le_bytes());
    entry[56..64].copy_from_slice(&(f.len() as u64).to_le_bytes());
    let d = &mut entry[64..96];
    d[..4].copy_from_slice(b"desc");
    d[4..8].copy_from_slice(&page[4092..]);
    d[8..16].copy_from_slice(&page[..8]);
    d[16..24].copy_from_slice(&(bat as u64).to_le_bytes());
    d[24..32].copy_from_slice(&seq.to_le_bytes());
    let data = &mut entry[4096..];
    data[..4].copy_from_slice(b"data");
    data[4..8].copy_from_slice(&((seq >> 32) as u32).to_le_bytes());
    data[8..4092].copy_from_slice(&page[8..4092]);
    data[4092..].copy_from_slice(&(seq as u32).to_le_bytes());
    let c = checksum(&entry, 4);
    entry[4..8].copy_from_slice(&c.to_le_bytes());
    assert!(log_len >= 8192);
    f[log_at..log_at + 8192].copy_from_slice(&entry);
    let h = &mut f[h_at..h_at + 4096];
    h[48..64].copy_from_slice(&guid);
    let c = checksum(h, 4);
    h[4..8].copy_from_slice(&c.to_le_bytes());
    std::fs::write(&path, &f).unwrap();

    let reg = registry();
    let mut dev = open(&reg, &path, AccessMode::ReadOnly);
    assert!(dev.container().unwrap().contains("log replayed"), "{:?}", dev.container());
    let mut b = vec![0u8; 2 << 20];
    dev.read_at(0, &mut b).unwrap();
    assert!(b.iter().all(|&x| x == 0xab), "block 1 now shows block 0");
    drop(dev);
    assert!(std::fs::read(&path).unwrap() == f, "the file is left as it was");
}

/// New disks made here (as "New image…" makes them): accepted by
/// qemu-img as they are, then with ext4 written through them.
#[cfg(feature = "ext4")]
#[test]
fn creates_disks_qemu_accepts() {
    if !have_qemu() {
        return;
    }
    let s = Scratch::new("create");
    let reg = registry();
    let size = 100 << 20;
    for f in reg.formats().iter().filter(|f| f.creates().is_some()) {
        let (ext, _) = f.creates().unwrap();
        let path = s.path(&format!("new.{ext}"));
        {
            let mut file = FileDevice::create(&path, 0).unwrap();
            f.create(&mut file, size, &format!("new.{ext}")).unwrap();
            file.flush().unwrap();
        }
        let fmt = match ext {
            "vhd" => "vpc",
            e => e,
        };
        qemu_check(&path, fmt);
        // "virtual size: 100 MiB (104857600 bytes)"
        let info = run(Command::new("qemu-img").args(["info", "-f", fmt]).arg(&path));
        let virt: u64 = info
            .lines()
            .find_map(|l| l.strip_prefix("virtual size:"))
            .and_then(|l| l.split('(').nth(1))
            .and_then(|n| n.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap();
        let dev = open(&reg, &path, AccessMode::ReadWrite);
        assert!(!dev.is_read_only(), "{ext}");
        // VHD rounds down to a whole geometry; QEMU's fixed-VHD quirk aside.
        assert!(dev.size() <= size && dev.size() > size - (2 << 20), "{ext}: {}", dev.size());
        assert_eq!(virt, dev.size(), "{ext}: qemu-img sees the same size");
        let mut dev = dev;
        reg.format(dev.as_mut(), "ext4", &dix_core::FormatOptions::default()).unwrap();
        let mut m = reg.mount(dev, Some("ext4"), AccessMode::ReadWrite).unwrap();
        let big: Vec<u8> = (0..5_000_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let p = VfsPath::parse("/big.bin").unwrap();
        m.fs.create_file(&p).unwrap();
        m.fs.write_at(&p, 0, &big).unwrap();
        m.fs.sync().unwrap();
        drop(m);
        qemu_check(&path, fmt);
        let raw = s.path("check.raw");
        run(Command::new("qemu-img").args(["convert", "-f", fmt, "-O", "raw"]).arg(&path).arg(&raw));
        run(Command::new("e2fsck").args(["-fn"]).arg(&raw));
        let dumped = s.path("big.out");
        run(Command::new("debugfs").arg("-R").arg(format!("dump /big.bin {}", dumped.display())).arg(&raw));
        assert!(std::fs::read(&dumped).unwrap() == big, "{ext}: the file written");
        // Still small: only what was written is in the file.
        assert!(std::fs::metadata(&path).unwrap().len() < 40 << 20, "{ext}");
    }
}

/// Conversion: the sample disk into every format that can be made (and
/// raw), compared with what qemu-img reads (our reader for DMG); then a
/// chain qcow2 → VHDX → VDI → VHD → raw through our readers. Each holds
/// only the data.
#[test]
fn converts_between_formats() {
    if !have_qemu() {
        return;
    }
    let s = Scratch::new("convert");
    let reg = registry();
    let data = sample();
    let src_path = s.path("src.raw");
    std::fs::write(&src_path, &data).unwrap();
    let check = |path: &Path, ext: &str| {
        let mut got =
            if ext == "dmg" { read_all(open(&reg, path, AccessMode::ReadOnly).as_mut()) } else { qemu_read(&s, path) };
        assert!(got.len() >= data.len(), "{ext}: {} bytes", got.len());
        assert!(got[data.len()..].iter().all(|&b| b == 0), "{ext}: beyond the disk");
        got.truncate(data.len());
        assert!(got == data, "{ext}: the data differs");
        let len = std::fs::metadata(path).unwrap().len();
        let on_disk = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(path).unwrap().blocks() * 512
        };
        // Only the data (1.3 MB in four places) and the format's own
        // structures: well under the 24 MiB disk.
        assert!(len.min(on_disk) < data.len() as u64 * 3 / 4, "{ext}: {len} bytes, {on_disk} on disk");
    };
    let creatable: Vec<_> = reg.formats().iter().filter(|f| f.creates().is_some()).cloned().collect();
    let mut src = open(&reg, &src_path, AccessMode::ReadOnly);
    for f in &creatable {
        let ext = f.creates().unwrap().0;
        let path = s.path(&format!("out.{ext}"));
        let file = Box::new(FileDevice::create(&path, 0).unwrap());
        let mut calls = 0;
        let mut progress = |_: u64, _: u64| {
            calls += 1;
            true
        };
        let written = dix_core::container::convert(
            src.as_mut(),
            file,
            Some(f.as_ref()),
            &format!("out.{ext}"),
            reg.formats(),
            &mut progress,
        )
        .unwrap();
        assert!(calls >= 24 && written < data.len() as u64, "{ext}: {calls} reports, {written} written");
        let fmt = if ext == "vhd" { "vpc" } else { ext };
        qemu_check(&path, fmt);
        check(&path, ext);
    }
    // Raw.
    let raw = s.path("out.img");
    let file = Box::new(FileDevice::create(&raw, 0).unwrap());
    dix_core::container::convert(src.as_mut(), file, None, "out.img", reg.formats(), &mut |_, _| true).unwrap();
    assert!(std::fs::read(&raw).unwrap() == data, "raw");
    // A chain through our own readers.
    let mut from = s.path("out.qcow2");
    for ext in ["vhdx", "vdi", "vhd", "img"] {
        let to = s.path(&format!("chain.{ext}"));
        let mut src = open(&reg, &from, AccessMode::ReadOnly);
        let f = creatable.iter().find(|f| f.creates().unwrap().0 == ext);
        let file = Box::new(FileDevice::create(&to, 0).unwrap());
        dix_core::container::convert(src.as_mut(), file, f.map(|f| f.as_ref()), "chain", reg.formats(), &mut |_, _| {
            true
        })
        .unwrap();
        from = to;
    }
    let mut back = std::fs::read(&from).unwrap();
    assert!(back[data.len()..].iter().all(|&b| b == 0));
    back.truncate(data.len());
    assert!(back == data, "the chain");
    // Stopping.
    let file = Box::new(FileDevice::create(s.path("stopped.qcow2"), 0).unwrap());
    let q = creatable.iter().find(|f| f.creates().unwrap().0 == "qcow2").unwrap();
    let e = dix_core::container::convert(src.as_mut(), file, Some(q.as_ref()), "s", reg.formats(), &mut |done, _| {
        done < 4 << 20
    });
    assert!(e.is_err());
}
