//! Decompresses every file of a RAR archive and reports the speed:
//! `cargo run --release -p dix-drivers --example rar_bench -- archive.rar`
use dix_core::{AccessMode, FileSystem, VfsPath};
fn walk(fs: &mut dyn FileSystem, dir: &VfsPath, out: &mut Vec<VfsPath>) {
    for e in fs.read_dir(dir).unwrap() {
        let p = dir.join(&e.name).unwrap();
        if e.metadata.is_dir() {
            walk(fs, &p, out)
        } else if e.metadata.kind == dix_core::EntryKind::File {
            out.push(p)
        }
    }
}
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dev = dix_core::device::host::open_path(&a[1], AccessMode::ReadOnly).unwrap();
    let mut m = dix_drivers::default_registry().mount(dev, None, AccessMode::ReadOnly).unwrap();
    let mut files = Vec::new();
    walk(m.fs.as_mut(), &VfsPath::root(), &mut files);
    let t = std::time::Instant::now();
    let mut buf = vec![0u8; 4 << 20];
    let mut total = 0u64;
    let mut sum = 0u64;
    for p in files {
        let ft = std::time::Instant::now();
        let mut off = 0u64;
        loop {
            let n = m.fs.read(&p, off, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            off += n as u64;
            sum = sum.wrapping_add(buf[n - 1] as u64);
        }
        total += off;
        eprintln!("  {p}: {} MB in {:.2}s", off >> 20, ft.elapsed().as_secs_f64());
    }
    let s = t.elapsed().as_secs_f64();
    println!("{} MB in {:.2}s = {:.0} MB/s ({sum})", total >> 20, s, total as f64 / 1e6 / s);
}
