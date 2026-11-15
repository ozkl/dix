//! A GPT disk with FAT32, ext4, NTFS, Linux swap and exFAT made by their
//! own tools: with the whole table wiped, every file system is found at
//! its start with its size (and label); with one partition deleted, only
//! it is found, in the free space. Skipped without the tools.

use super::partitions::{Lost, find};
use dix_core::device::MemoryDevice;
use std::process::Command;

fn have(tools: &[&str]) -> bool {
    tools
        .iter()
        .all(|t| Command::new("sh").arg("-c").arg(format!("command -v {t}")).output().is_ok_and(|o| o.status.success()))
}

fn run(cmd: &str, args: &[&str]) {
    let out = Command::new(cmd).args(args).output().unwrap();
    assert!(out.status.success(), "{cmd} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

const MIB: u64 = 1 << 20;

#[test]
fn finds_partitions_whose_table_is_gone() {
    if !have(&["sgdisk", "mkfs.fat", "mke2fs", "mkntfs", "mkswap", "mkfs.exfat"]) {
        eprintln!("sgdisk / mkfs tools missing: skipped");
        return;
    }
    let dir = std::env::temp_dir().join(format!("dix-lost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // (file system, MiB, how it is made; kind and label expected)
    let parts: [(&str, u64, &str, Option<&str>); 5] = [
        ("fat32", 40, "FAT32", Some("PHOTOS")),
        ("ext4", 32, "ext4", Some("linuxroot")),
        ("ntfs", 40, "NTFS", Some("Windows")),
        ("swap", 16, "Linux swap", None),
        ("exfat", 24, "exFAT", Some("CARD")),
    ];
    let disk_path = dir.join("disk.img");
    let mut disk = vec![0u8; 200 << 20];
    let mut at = MIB;
    let mut want = Vec::new();
    for (fs, mib, kind, label) in parts {
        let img = dir.join(format!("{fs}.img"));
        std::fs::File::create(&img).unwrap().set_len(mib * MIB).unwrap();
        let p = img.to_str().unwrap();
        match fs {
            "fat32" => run("mkfs.fat", &["-F", "32", "-n", "PHOTOS", p]),
            "ext4" => run("mke2fs", &["-q", "-F", "-t", "ext4", "-L", "linuxroot", p]),
            "ntfs" => run("mkntfs", &["-F", "-f", "-q", "-L", "Windows", p]),
            "swap" => run("mkswap", &[p]),
            _ => run("mkfs.exfat", &["-L", "CARD", p]),
        }
        let data = std::fs::read(&img).unwrap();
        disk[at as usize..(at + mib * MIB) as usize].copy_from_slice(&data);
        want.push((at, mib * MIB, kind, label));
        at += mib * MIB + MIB;
    }
    std::fs::write(&disk_path, &disk).unwrap();
    // The table they were in.
    let mut args = vec!["-o".to_owned()];
    for (i, (start, size, _, _)) in want.iter().enumerate() {
        args.push(format!("-n{}:{}:{}", i + 1, start / 512, (start + size) / 512 - 1));
    }
    args.push(disk_path.to_str().unwrap().to_owned());
    run("sgdisk", &args.iter().map(String::as_str).collect::<Vec<_>>());
    let check = |got: &[Lost], expect: &[(u64, u64, &str, Option<&str>)], what: &str| {
        let got: Vec<_> = got.iter().map(|l| (l.start, l.size, l.kind, l.label.as_deref())).collect();
        assert_eq!(got, expect, "{what}");
    };

    // One partition deleted: only it, in the free space.
    let deleted = dir.join("deleted.img");
    std::fs::copy(&disk_path, &deleted).unwrap();
    run("sgdisk", &["-d", "2", deleted.to_str().unwrap()]);
    let bytes = std::fs::read(&deleted).unwrap();
    let mut dev = MemoryDevice::new("d", bytes.clone());
    let layout = dix_core::partition::edit::Layout::read(&mut dev).unwrap().unwrap();
    let free: Vec<(u64, u64)> = layout.free().iter().map(|f| (f.first * 512, (f.last + 1) * 512)).collect();
    let found = find(Box::new(MemoryDevice::new("d", bytes)), &free, &mut |_, _| true).unwrap();
    check(&found, &want[1..2], "one deleted");
    assert!(found[0].opens);

    // The whole table wiped (both GPT copies and the MBR).
    let wiped = dir.join("wiped.img");
    std::fs::copy(&disk_path, &wiped).unwrap();
    run("sgdisk", &["--zap-all", wiped.to_str().unwrap()]);
    let bytes = std::fs::read(&wiped).unwrap();
    let size = bytes.len() as u64;
    let mut calls = 0;
    let found = find(Box::new(MemoryDevice::new("w", bytes)), &[(0, size)], &mut |_, _| {
        calls += 1;
        true
    })
    .unwrap();
    check(&found, &want, "table wiped");
    assert!(calls > 0);
    // Those with a driver here open; swap does not need to.
    for l in &found {
        assert_eq!(l.opens, l.kind != "Linux swap", "{}", l.kind);
    }
    let _ = std::fs::remove_dir_all(dir);
}
