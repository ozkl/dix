//! Partition tables we write, judged by `sfdisk` (util-linux) and `sgdisk`
//! (gdisk, a separate GPT implementation); tables they write, edited by
//! us. Skipped where the tools are missing.

use super::edit::*;
use super::Scheme;
use crate::device::host::FileDevice;
use crate::AccessMode;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn tool(name: &str) -> Option<PathBuf> {
    ["/usr/sbin", "/sbin", "/usr/bin", "/bin"].iter().map(|d| Path::new(d).join(name)).find(|p| p.exists())
}

fn have() -> bool {
    let ok = tool("sfdisk").is_some() && tool("sgdisk").is_some();
    if !ok {
        eprintln!("sfdisk / sgdisk not installed: skipping");
    }
    ok
}

fn run(cmd: &str, args: &[&str], input: Option<&str>) -> (bool, String) {
    let mut c = Command::new(tool(cmd).unwrap());
    c.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    if let Some(i) = input {
        use std::io::Write;
        child.stdin.take().unwrap().write_all(i.as_bytes()).unwrap();
    }
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr))
}

fn ok(cmd: &str, args: &[&str], input: Option<&str>) -> String {
    let (good, out) = run(cmd, args, input);
    assert!(good, "{cmd} {args:?}: {out}");
    out
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("dix-partedit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }

    fn image(&self, size: u64) -> PathBuf {
        let p = self.0.join("disk.img");
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(size).unwrap();
        p
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Opens an image, retrying briefly while it is locked: tests run in
/// parallel and fork tools, and a child forked while an image is open
/// holds its lock until it execs.
fn dev(img: &Path) -> FileDevice {
    for _ in 0..100 {
        match FileDevice::open(img, AccessMode::ReadWrite) {
            Err(e) if e.kind() == crate::ErrorKind::InUse => std::thread::sleep(std::time::Duration::from_millis(50)),
            r => return r.unwrap(),
        }
    }
    FileDevice::open(img, AccessMode::ReadWrite).unwrap()
}

/// One line of `sfdisk -d`: number, start, size, type, uuid, name, attrs,
/// bootable.
#[derive(Debug, PartialEq, Eq)]
struct Line {
    number: u32,
    start: u64,
    size: u64,
    ty: String,
    uuid: String,
    name: String,
    attrs: String,
    bootable: bool,
}

fn dump(img: &Path) -> (String, Vec<Line>) {
    let out = ok("sfdisk", &["-d", img.to_str().unwrap()], None);
    let mut label_id = String::new();
    let mut lines = Vec::new();
    for l in out.lines() {
        if let Some(id) = l.strip_prefix("label-id: ") {
            label_id = id.to_uppercase();
        }
        let Some((dev, rest)) = l.split_once(" : ") else { continue };
        let number: u32 = dev.rsplit(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap();
        let mut line = Line {
            number,
            start: 0,
            size: 0,
            ty: String::new(),
            uuid: String::new(),
            name: String::new(),
            attrs: String::new(),
            bootable: false,
        };
        for field in rest.split(", ") {
            let (k, v) = field.split_once('=').unwrap_or((field.trim(), ""));
            let v = v.trim().trim_matches('"');
            match k.trim() {
                "start" => line.start = v.parse().unwrap(),
                "size" => line.size = v.parse().unwrap(),
                "type" => line.ty = v.to_uppercase(),
                "uuid" => line.uuid = v.to_uppercase(),
                "name" => line.name = unescape(v),
                "attrs" => line.attrs = v.into(),
                "bootable" => line.bootable = true,
                _ => {}
            }
        }
        lines.push(line);
    }
    (label_id, lines)
}

/// `sfdisk -d` writes non-ASCII bytes as `\xHH`.
fn unescape(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && b.get(i + 1) == Some(&b'x') && i + 4 <= b.len() {
            out.push(u8::from_str_radix(&s[i + 2..i + 4], 16).unwrap());
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// What `sfdisk -d` should say about a layout.
fn expected(l: &Layout) -> Vec<Line> {
    l.slots
        .iter()
        .map(|s| Line {
            number: s.number,
            start: s.first,
            size: s.sectors(),
            ty: match s.type_code {
                TypeCode::Gpt(_) => s.type_code.code(),
                TypeCode::Mbr(b) => format!("{b:X}"),
            },
            uuid: if l.scheme == Scheme::Gpt { super::guid(&s.guid) } else { String::new() },
            name: s.name.clone(),
            attrs: if s.attrs & 0b100 != 0 { "LegacyBIOSBootable".into() } else { String::new() },
            bootable: s.active,
        })
        .collect()
}

fn verify(img: &Path, scheme: Scheme) {
    let out = ok("sfdisk", &["--verify", img.to_str().unwrap()], None);
    assert!(out.contains("No errors detected"), "sfdisk --verify: {out}");
    if scheme == Scheme::Gpt {
        let out = ok("sgdisk", &["-v", img.to_str().unwrap()], None);
        assert!(out.contains("No problems found"), "sgdisk -v: {out}");
    }
}

fn type_of(name: &str, scheme: Scheme) -> TypeCode {
    KNOWN_TYPES.iter().find(|t| t.name == name).unwrap().code(scheme).unwrap()
}

#[test]
fn our_gpt_passes_sgdisk_and_sfdisk() {
    if !have() {
        return;
    }
    let s = Scratch::new("gpt");
    let img = s.image(256 << 20);
    let mut l = Layout::new(Scheme::Gpt, 256 << 20, 512).unwrap();
    let free = l.free()[0];
    l.add(free.first, free.first + 204_799, type_of("EFI system", Scheme::Gpt), "EFI System").unwrap();
    let f = l.free()[0];
    l.add(f.first, f.first + 99_999, type_of("Linux filesystem", Scheme::Gpt), "root – ünïcode").unwrap();
    let f = l.free()[0];
    l.add(f.first, f.last, type_of("Windows data (FAT32)", Scheme::Gpt), "DATA").unwrap();
    l.set_bootable(2, true).unwrap();
    l.write(&mut dev(&img)).unwrap();
    verify(&img, Scheme::Gpt);
    let (id, lines) = dump(&img);
    assert_eq!(id, super::guid(&l.disk_guid));
    assert_eq!(lines, expected(&l));
    // The free space ends where sgdisk says the usable area ends.
    let out = ok("sgdisk", &["-p", img.to_str().unwrap()], None);
    assert!(out.contains(&format!("last usable sector is {}", l.usable().1)), "{out}");
}

#[test]
fn edit_a_gpt_made_by_sgdisk() {
    if !have() {
        return;
    }
    let s = Scratch::new("sgdisk");
    let img = s.image(64 << 20);
    let p = img.to_str().unwrap();
    ok(
        "sgdisk",
        &[
            "-o",
            "-n",
            "1:2048:+10M",
            "-t",
            "1:ef00",
            "-c",
            "1:EFI",
            "-n",
            "2:0:+20M",
            "-t",
            "2:8300",
            "-c",
            "2:root",
            "-A",
            "2:set:2",
            "-A",
            "2:set:60",
            "-n",
            "3:0:0",
            p,
        ],
        None,
    );
    let (id, before) = dump(&img);
    let mut l = Layout::read(&mut dev(&img)).unwrap().unwrap();
    assert_eq!(expected(&l)[..1], before[..1]);
    // Delete 1, put a swap partition in its place, rename 3.
    l.delete(1).unwrap();
    let f = l.free()[0];
    assert_eq!((f.first, f.last), (2048, 22527));
    assert_eq!(l.add(f.first, f.last, type_of("Linux swap", Scheme::Gpt), "swap").unwrap(), 1);
    l.set_name(3, "data").unwrap();
    l.write(&mut dev(&img)).unwrap();
    verify(&img, Scheme::Gpt);
    let (id2, after) = dump(&img);
    assert_eq!(id2, id, "the disk GUID changed");
    // Partition 2 untouched: same GUID, attributes kept.
    assert_eq!(after[1], before[1]);
    assert_eq!(after[1].attrs, "LegacyBIOSBootable GUID:60");
    assert_eq!((after[2].uuid.as_str(), after[2].name.as_str()), (before[2].uuid.as_str(), "data"));
    assert_eq!((after[0].start, after[0].size, after[0].name.as_str()), (2048, 20480, "swap"));
}

#[test]
fn a_grown_image_gets_its_backup_gpt_moved() {
    if !have() {
        return;
    }
    let s = Scratch::new("grown");
    let img = s.image(32 << 20);
    ok("sgdisk", &["-o", "-n", "1:0:0", img.to_str().unwrap()], None);
    std::fs::OpenOptions::new().write(true).open(&img).unwrap().set_len(64 << 20).unwrap();
    let mut l = Layout::read(&mut dev(&img)).unwrap().unwrap();
    let f = l.free();
    assert_eq!(f.len(), 1, "the new space is free");
    l.add(f[0].first, f[0].last, type_of("Linux filesystem", Scheme::Gpt), "").unwrap();
    l.write(&mut dev(&img)).unwrap();
    verify(&img, Scheme::Gpt);
    assert_eq!(dump(&img).1.len(), 2);
}

#[test]
fn our_mbr_with_logical_partitions_passes_sfdisk() {
    if !have() {
        return;
    }
    let s = Scratch::new("mbr");
    let img = s.image(256 << 20);
    let mut l = Layout::new(Scheme::Mbr, 256 << 20, 512).unwrap();
    let m = 2048; // 1 MiB in sectors
    l.add(m, 21 * m - 1, TypeCode::Mbr(0x0C), "").unwrap();
    l.add(21 * m, 200 * m - 1, TypeCode::Mbr(0x0F), "").unwrap();
    // Logical partitions, added out of order.
    l.add(60 * m, 80 * m - 1, TypeCode::Mbr(0x82), "").unwrap();
    l.add(22 * m, 40 * m - 1, TypeCode::Mbr(0x83), "").unwrap();
    let f = l.free().into_iter().find(|f| f.logical && f.first > 80 * m).unwrap();
    l.add(f.first, f.last, TypeCode::Mbr(0x07), "").unwrap();
    let f = l.free().into_iter().find(|f| !f.logical).unwrap();
    l.add(f.first, f.last, TypeCode::Mbr(0x83), "").unwrap();
    l.set_bootable(1, true).unwrap();
    l.write(&mut dev(&img)).unwrap();
    verify(&img, Scheme::Mbr);
    let (id, lines) = dump(&img);
    assert_eq!(id, format!("0X{:08X}", l.disk_signature));
    assert_eq!(lines, expected(&l));
    assert_eq!(lines.iter().map(|l| l.number).collect::<Vec<_>>(), [1, 2, 3, 5, 6, 7]);
}

#[test]
fn edit_an_mbr_made_by_sfdisk() {
    if !have() {
        return;
    }
    let s = Scratch::new("sfdisk");
    let img = s.image(200 << 20);
    ok(
        "sfdisk",
        &["-q", img.to_str().unwrap()],
        Some(
            "label: dos\n1 : start=2048, size=20480, type=c, bootable\n2 : start=22528, size=204800, type=f\n\
             5 : start=24576, size=40960, type=83\n6 : start=67584, size=40960, type=82\n\
             7 : start=110592, size=40960, type=83\n",
        ),
    );
    let (id, before) = dump(&img);
    let mut l = Layout::read(&mut dev(&img)).unwrap().unwrap();
    assert_eq!(expected(&l), before);
    // Replace the middle logical partition with an LVM one, delete the
    // first: the others move down a number.
    l.delete(6).unwrap();
    let f = l.free().into_iter().find(|f| f.logical && f.first > 24576 + 40960).unwrap();
    assert_eq!(l.add(f.first, 67584 + 40959, TypeCode::Mbr(0x8E), "").unwrap(), 6);
    l.delete(5).unwrap();
    l.write(&mut dev(&img)).unwrap();
    verify(&img, Scheme::Mbr);
    let (id2, after) = dump(&img);
    assert_eq!(id2, id);
    assert_eq!(after, expected(&l));
    let got: Vec<(u32, u64, &str)> = after.iter().map(|l| (l.number, l.start, l.ty.as_str())).collect();
    assert_eq!(got, [(1, 2048, "C"), (2, 22528, "F"), (5, f.first, "8E"), (6, 110592, "83")]);
    assert!(after[0].bootable);
}

#[test]
fn switching_schemes() {
    if !have() {
        return;
    }
    let s = Scratch::new("switch");
    let img = s.image(64 << 20);
    ok("sgdisk", &["-o", "-n", "1:0:+10M", img.to_str().unwrap()], None);
    let mut m = Layout::new(Scheme::Mbr, 64 << 20, 512).unwrap();
    m.add(2048, 40959, TypeCode::Mbr(0x83), "").unwrap();
    m.write(&mut dev(&img)).unwrap();
    verify(&img, Scheme::Mbr);
    let (good, out) = run("sgdisk", &["-v", img.to_str().unwrap()], None);
    assert!(!out.contains("EFI PART") || !good, "{out}");
    assert_eq!(ok("blkid", &["-p", "-o", "value", "-s", "PTTYPE", img.to_str().unwrap()], None).trim(), "dos");
    let mut g = Layout::new(Scheme::Gpt, 64 << 20, 512).unwrap();
    g.add(2048, 40959, type_of("Linux filesystem", Scheme::Gpt), "back").unwrap();
    g.write(&mut dev(&img)).unwrap();
    verify(&img, Scheme::Gpt);
    assert_eq!(ok("blkid", &["-p", "-o", "value", "-s", "PTTYPE", img.to_str().unwrap()], None).trim(), "gpt");
}

#[test]
fn a_gpt_with_4k_sectors() {
    if !have() {
        return;
    }
    let s = Scratch::new("4k");
    let img = s.image(64 << 20);
    let mut l = Layout::new(Scheme::Gpt, 64 << 20, 4096).unwrap();
    let f = l.free()[0];
    assert_eq!(f.first, 256);
    l.add(f.first, f.last, type_of("Linux filesystem", Scheme::Gpt), "big sectors").unwrap();
    let mut d = dev(&img);
    l.write(&mut d).unwrap();
    let back = Layout::read(&mut d).unwrap().unwrap();
    assert_eq!((back.sector_size, back.slots.len()), (4096, 1));
    drop(d);
    // sfdisk learns the sector size from a device: a loop device with
    // 4 KiB sectors (when loop devices are available).
    let (good, out) = run("losetup", &["-f", "--show", "--sector-size", "4096", img.to_str().unwrap()], None);
    if !good {
        eprintln!("no loop device ({out}): 4 KiB sectors checked by us only");
        return;
    }
    let loopdev = out.trim().to_owned();
    let (dump, verify) = (run("sfdisk", &["-d", &loopdev], None), run("sfdisk", &["--verify", &loopdev], None));
    run("losetup", &["-d", &loopdev], None);
    assert!(dump.1.contains("sector-size: 4096") && dump.1.contains("start=         256"), "{}", dump.1);
    assert!(dump.1.contains("name=\"big sectors\""), "{}", dump.1);
    assert!(verify.1.contains("No errors detected"), "{}", verify.1);
}
