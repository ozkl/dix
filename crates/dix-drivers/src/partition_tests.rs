//! Partition tables written by util-linux / gdisk, read by
//! `dix_core::partition`, with ext4 file systems inside the partitions.

use dix_core::device::host::FileDevice;
use dix_core::partition::{Scheme, open_partition, read_partition_table};
use dix_core::{AccessMode, BlockDevice, VfsPath};
use std::path::{Path, PathBuf};
use std::process::Command;

fn tool(name: &str) -> Option<PathBuf> {
    ["/usr/sbin", "/sbin", "/usr/bin", "/bin"].iter().map(|d| Path::new(d).join(name)).find(|p| p.exists())
}

fn have(tools: &[&str]) -> bool {
    let ok = tools.iter().all(|t| tool(t).is_some());
    if !ok {
        eprintln!("{tools:?} not installed: skipping partition test");
    }
    ok
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("dix-part-{name}-{}", std::process::id()));
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

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{cmd:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Opens an image, retrying briefly while it is locked: tests run in
/// parallel and fork e2fsprogs, and a child forked while an image is open
/// shares its file (and flock) until it execs.
fn open_retry(path: &Path, mode: AccessMode) -> dix_core::Result<FileDevice> {
    let mut tries = 0;
    loop {
        match FileDevice::open(path, mode) {
            Err(e) if e.kind() == dix_core::ErrorKind::InUse && tries < 100 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            r => return r,
        }
    }
}

fn blank(path: &Path, size: u64) {
    std::fs::File::create(path).unwrap().set_len(size).unwrap();
}

fn table_of(path: &Path) -> dix_core::partition::PartitionTable {
    let mut dev = open_retry(path, AccessMode::ReadOnly).unwrap();
    read_partition_table(&mut dev).unwrap().expect("a partition table")
}

/// `sgdisk -i N`: (first sector, last sector, type GUID, name).
fn sgdisk_info(img: &Path, n: u32) -> (u64, u64, String, String) {
    let out = run(Command::new(tool("sgdisk").unwrap()).arg(format!("-i{n}")).arg(img));
    let field = |key: &str| out.lines().find_map(|l| l.strip_prefix(key)).unwrap().trim().to_owned();
    let sector = |key: &str| field(key).split_whitespace().next().unwrap().parse().unwrap();
    let type_guid = field("Partition GUID code:").split_whitespace().next().unwrap().to_owned();
    let name = field("Partition name:").trim_matches('\'').to_owned();
    (sector("First sector:"), sector("Last sector:"), type_guid, name)
}

#[test]
fn gpt_from_sgdisk() {
    if !have(&["sgdisk", "mke2fs", "e2fsck"]) {
        return;
    }
    let s = Scratch::new("gpt");
    let img = s.0.join("disk.img");
    blank(&img, 64 << 20);
    run(Command::new(tool("sgdisk").unwrap())
        .args(["-n1:2048:+8M", "-t1:ef00", "-c1:EFI System"])
        .args(["-n2:0:+40M", "-t2:8300", "-c2:root"])
        .args(["-n3:0:0", "-t3:8200", "-c3:swap space"])
        .arg(&img));
    let t = table_of(&img);
    assert_eq!(t.scheme, Scheme::Gpt);
    assert!(t.notes.is_empty(), "{:?}", t.notes);
    assert_eq!(t.partitions.len(), 3);
    for p in &t.partitions {
        let (first, last, type_guid, name) = sgdisk_info(&img, p.number);
        assert_eq!((p.start / 512, (p.start + p.size) / 512 - 1), (first, last), "partition {}", p.number);
        assert_eq!(p.type_id, type_guid);
        assert_eq!(p.name.as_deref().unwrap_or(""), name);
    }
    let names: Vec<&str> = t.partitions.iter().map(|p| p.type_name.as_str()).collect();
    assert_eq!(names, ["EFI system", "Linux filesystem", "Linux swap"]);

    // mke2fs into partition 2 (by offset); our driver mounts it through the slice.
    let root = t.get(2).unwrap().clone();
    run(Command::new(tool("mke2fs").unwrap())
        .args(["-q", "-F", "-t", "ext4", "-L", "INSIDE", "-E"])
        .arg(format!("offset={}", root.start))
        .arg(&img)
        .arg(format!("{}k", root.size / 1024)));
    let registry = crate::default_registry();
    {
        // What the open dialog shows: ext4 found in partition 2, preselected.
        let mut dev = open_retry(&img, AccessMode::ReadOnly).unwrap();
        let scan = dix_core::partition::scan(&registry, &mut dev).unwrap().unwrap();
        assert_eq!(scan.drivers, vec![None, Some("ext2/3/4".to_owned()), None]);
        assert_eq!(scan.default_choice(), Some(2));
        let auto = dix_core::partition::resolve(&registry, &mut dev, Default::default()).unwrap();
        assert_eq!(auto.map(|p| p.number), Some(2));
    }
    {
        let dev = Box::new(open_retry(&img, AccessMode::ReadWrite).unwrap());
        let part = open_partition(dev, 2).unwrap();
        let mut m = registry.mount(part, None, AccessMode::ReadWrite).unwrap();
        assert_eq!(m.fs.info().label.as_deref(), Some("INSIDE"));
        m.fs.write_file(&VfsPath::parse("/hello.txt").unwrap(), b"inside a partition").unwrap();
        m.fs.unmount().unwrap();
    }
    // The partition table and partition 1 are untouched; e2fsck likes partition 2.
    assert_eq!(table_of(&img), t);
    let bytes = std::fs::read(&img).unwrap();
    let p2 = s.0.join("p2.img");
    std::fs::write(&p2, &bytes[root.start as usize..(root.start + root.size) as usize]).unwrap();
    let out = Command::new(tool("e2fsck").unwrap()).arg("-fn").arg(&p2).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let cat = Command::new(tool("debugfs").unwrap()).args(["-R", "cat /hello.txt"]).arg(&p2).output().unwrap();
    assert_eq!(cat.stdout, b"inside a partition");
}

#[test]
fn gpt_backup_is_used_when_the_primary_is_wiped() {
    if !have(&["sgdisk"]) {
        return;
    }
    let s = Scratch::new("gpt-backup");
    let img = s.0.join("disk.img");
    blank(&img, 16 << 20);
    run(Command::new(tool("sgdisk").unwrap()).args(["-n1:0:0", "-t1:0700", "-c1:DATA"]).arg(&img));
    let good = table_of(&img);
    // Zero the primary header (sector 1).
    let mut dev = open_retry(&img, AccessMode::ReadWrite).unwrap();
    dev.write_at(512, &[0u8; 512]).unwrap();
    drop(dev);
    let t = table_of(&img);
    assert_eq!(t.partitions, good.partitions);
    assert_eq!(t.partitions[0].type_name, "Microsoft basic data");
    assert_eq!(t.notes.len(), 1, "{:?}", t.notes);
}

#[test]
fn mbr_from_sfdisk_with_logical_partitions() {
    if !have(&["sfdisk", "e2fsck"]) {
        return;
    }
    let s = Scratch::new("mbr");
    let img = s.0.join("disk.img");
    blank(&img, 64 << 20);
    let script =
        "label: dos\nstart=2048, size=16M, type=c, bootable\nstart=34816, size=20M, type=83\ntype=5\n,8M,83\n,4M,82\n";
    let mut child =
        Command::new(tool("sfdisk").unwrap()).arg("-q").arg(&img).stdin(std::process::Stdio::piped()).spawn().unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());

    let t = table_of(&img);
    assert_eq!(t.scheme, Scheme::Mbr);
    // sfdisk --json: "node": "...N", "start": S, "size": Z, "type": "T".
    let json = run(Command::new(tool("sfdisk").unwrap()).arg("--json").arg(&img));
    let mut theirs = Vec::new();
    for block in json.split("\"node\":").skip(1) {
        let num = |key: &str| -> String {
            let at = block.find(key).unwrap() + key.len();
            block[at..].trim_start_matches([' ', ':', '"']).chars().take_while(|c| c.is_ascii_alphanumeric()).collect()
        };
        let node: String = block.trim_start_matches([' ', '"']).chars().take_while(|c| *c != '"').collect();
        let digits = node.len() - node.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        let number: u32 = node[node.len() - digits..].parse().unwrap();
        let kind = u8::from_str_radix(&num("\"type\""), 16).unwrap();
        theirs.push((number, num("\"start\"").parse::<u64>().unwrap(), num("\"size\"").parse::<u64>().unwrap(), kind));
    }
    let ours: Vec<(u32, u64, u64, u8)> = t
        .partitions
        .iter()
        .map(|p| (p.number, p.start / 512, p.size / 512, u8::from_str_radix(&p.type_id[2..], 16).unwrap()))
        .collect();
    assert_eq!(ours, theirs);
    assert!(t.partitions[0].bootable);

    // Our mkfs inside logical partition 5; e2fsck checks it.
    {
        let dev = Box::new(open_retry(&img, AccessMode::ReadWrite).unwrap());
        let mut part = open_partition(dev, 5).unwrap();
        crate::ext4::mkfs::mkfs(part.as_mut(), crate::ext4::mkfs::Variant::Ext4, "LOGICAL").unwrap();
    }
    let p5 = t.get(5).unwrap();
    let bytes = std::fs::read(&img).unwrap();
    let out_img = s.0.join("p5.img");
    std::fs::write(&out_img, &bytes[p5.start as usize..(p5.start + p5.size) as usize]).unwrap();
    let out = Command::new(tool("e2fsck").unwrap()).arg("-fn").arg(&out_img).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    // ...and the table survived (the EBR right before partition 5 included).
    assert_eq!(table_of(&img), t);
}
