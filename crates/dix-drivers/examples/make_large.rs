//! Creates a SimpleFS image with many entries, for performance testing:
//!
//! ```bash
//! cargo run --release -p dix-drivers --example make_large -- large.img 20000
//! ```
//!
//! Layout: `/many` holds N files, `/dirs` holds N/6 sub folders (exercises the
//! folder tree), `/nested` is a 30 levels deep chain.

use dix_core::device::host::FileDevice;
use dix_core::{AccessMode, FormatOptions, VfsPath};

fn main() -> dix_core::Result<()> {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "large.img".into());
    let n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(20_000);

    let registry = dix_drivers::default_registry();
    let mut dev = FileDevice::create(&out, 1024 * 1024)?;
    registry.format(&mut dev, "simplefs", &FormatOptions { label: Some("LARGE".into()), variant: None })?;
    drop(dev);

    // SimpleFS writes the whole image after every change, so build the tree in
    // memory first and write it with a single import.
    use dix_core::ops::Bundle;
    let files = (0..n)
        .map(|i| Bundle::File {
            name: format!("file-{i:05}.{}", ["txt", "log", "bin", "rs", "md"][i % 5]),
            data: format!("content of file {i}\n").repeat(1 + i % 7).into_bytes(),
            modified: None,
        })
        .collect();
    let dirs = (0..n / 6).map(|i| Bundle::Dir { name: format!("dir-{i:05}"), children: Vec::new() }).collect();
    let mut nested = Bundle::Dir { name: "level-30".into(), children: Vec::new() };
    for level in (1..30).rev() {
        nested = Bundle::Dir { name: format!("level-{level:02}"), children: vec![nested] };
    }
    let root = vec![
        Bundle::Dir { name: "many".into(), children: files },
        Bundle::Dir { name: "dirs".into(), children: dirs },
        Bundle::Dir { name: "nested".into(), children: vec![nested] },
    ];

    let dev = dix_core::device::host::open_path(&out, AccessMode::ReadWrite)?;
    let mut fs = registry.mount(dev, Some("simplefs"), AccessMode::ReadWrite)?.fs;
    let t = std::time::Instant::now();
    for b in &root {
        dix_core::ops::write_bundle(fs.as_mut(), &VfsPath::root(), b)?;
    }
    fs.unmount()?; // clean unmount: otherwise the next mount reports a dirty volume
    println!("wrote {out} ({n} files) in {:.1?}", t.elapsed());
    Ok(())
}
