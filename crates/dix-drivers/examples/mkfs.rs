//! Creates an empty ext2/ext3/ext4 image with the built-in mkfs:
//!
//! ```bash
//! cargo run -p dix-drivers --example mkfs -- ext4 disk.img 64M [LABEL] [--block-size 4096] [--meta-bg]
//! ```

use dix_core::device::host::FileDevice;
use dix_drivers::ext4::mkfs;

fn main() -> dix_core::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut opts = mkfs::Options::default();
    if let Some(i) = args.iter().position(|a| a == "--meta-bg") {
        args.remove(i);
        opts.meta_bg = Some(true);
    }
    if let Some(i) = args.iter().position(|a| a == "--block-size") {
        args.remove(i);
        opts.block_size = Some(args.remove(i).parse().expect("block size"));
    }
    let (variant, path, size) = match args.as_slice() {
        [v, p, s, ..] => (v.as_str(), p.as_str(), s.as_str()),
        _ => {
            eprintln!(
                "usage: mkfs <ext2|ext3|ext4> <image> <size, e.g. 64M or 2G> [label] [--block-size N] [--meta-bg]"
            );
            std::process::exit(2);
        }
    };
    let variant = mkfs::Variant::parse(variant).expect("ext2, ext3 or ext4");
    let (num, unit) = size.split_at(size.trim_end_matches(|c: char| c.is_ascii_alphabetic()).len());
    let mult: u64 = match unit.to_ascii_uppercase().as_str() {
        "" => 1,
        "K" => 1 << 10,
        "M" => 1 << 20,
        "G" => 1 << 30,
        "T" => 1 << 40,
        _ => panic!("unknown size unit {unit}"),
    };
    let bytes = num.parse::<u64>().expect("size") * mult;
    let mut dev = FileDevice::create(path, bytes)?;
    mkfs::mkfs_with(&mut dev, variant, args.get(3).map_or("", String::as_str), &opts)?;
    println!("created {variant:?} on {path} ({bytes} bytes)");
    Ok(())
}
