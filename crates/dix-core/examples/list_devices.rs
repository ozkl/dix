//! Prints the host's disks and partitions as the open dialog lists them:
//!
//! ```bash
//! cargo run -p dix-core --example list_devices
//! ```

fn main() {
    for d in dix_core::device::host::list_devices() {
        let indent = if d.parent.is_some() { "  " } else { "" };
        let size = d.size.map(|s| format!("{:.1} GB", s as f64 / 1e9)).unwrap_or_default();
        println!("{indent}{:<20} {:<50} {size}", d.path, d.description);
    }
}
