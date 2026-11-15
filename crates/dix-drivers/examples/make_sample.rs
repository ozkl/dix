//! Creates a small SimpleFS image to play with:
//!
//! ```bash
//! cargo run -p dix-drivers --example make_sample -- sample.img
//! cargo run -p dix-app -- sample.img
//! ```

use dix_core::device::host::FileDevice;
use dix_core::{AccessMode, FormatOptions, VfsPath};

fn main() -> dix_core::Result<()> {
    let out = std::env::args().nth(1).unwrap_or_else(|| "sample.img".into());
    let registry = dix_drivers::default_registry();
    let mut dev = FileDevice::create(&out, 256 * 1024)?;
    registry.format(&mut dev, "simplefs", &FormatOptions { label: Some("SAMPLE".into()), variant: None })?;
    drop(dev);

    let dev = dix_core::device::host::open_path(&out, AccessMode::ReadWrite)?;
    let mut fs = registry.mount(dev, Some("simplefs"), AccessMode::ReadWrite)?.fs;
    let p = |s: &str| VfsPath::parse(s).unwrap();
    for dir in [
        "/Applications",
        "/bin",
        "/Desktop",
        "/Documents",
        "/Downloads",
        "/Music",
        "/Pictures",
        "/Public",
        "/Documents/notes",
    ] {
        fs.create_dir(&p(dir))?;
    }
    fs.write_file(&p("/README.txt"), b"This is a SimpleFS sample image created by Disk Image Explorer.\n")?;
    fs.write_file(&p("/share.py"), b"#!/usr/bin/env python3\nprint('hello from inside an image')\n")?;
    fs.write_file(&p("/.hidden"), b"secret")?;
    fs.write_file(&p("/bin/tool"), &(0u8..=255).cycle().take(5000).collect::<Vec<_>>())?;
    fs.write_file(&p("/Documents/notes/todo.md"), b"# TODO\n\n- write an ext4 driver\n- write a FAT driver\n")?;
    fs.write_file(&p("/Pictures/photo.raw"), &vec![0x42; 21_900])?;
    fs.unmount()?; // clean unmount: otherwise the next mount reports a dirty volume
    println!("wrote {out}");
    Ok(())
}
