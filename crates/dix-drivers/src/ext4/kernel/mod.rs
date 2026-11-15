//! A small userspace stand-in for the parts of the ruxis kernel that its
//! ext4 driver uses, so the driver code (in `../ruxis`) builds unchanged
//! apart from its `crate::` paths.
//!
//! * `errno`, `sync`, `time`, `arch`, `cred`, `process`: tiny equivalents;
//! * `block`: [`block::Disk`] on top of a [`dix_core::BlockDevice`] (with an
//!   in-memory overlay for read-only mounts), and ruxis's buffer cache;
//! * `mm`: ruxis's page cache, with heap pages standing in for frames;
//! * `fs`: the VFS types and the `Inode` trait the driver implements.
//!
//! `kinfo!` messages go to the mount log of the mount in progress.

pub mod block;
pub mod errno;
pub mod fs;
pub mod mm;
pub mod sync;

use dix_core::{LogLevel, MountLog};
use std::cell::RefCell;

pub mod time {
    pub fn realtime_ns() -> u64 {
        dix_core::time::unix_now_ns()
    }

    pub fn realtime_secs() -> u64 {
        realtime_ns() / 1_000_000_000
    }
}

pub mod arch {
    pub mod paging {
        pub const PAGE_SIZE: usize = 4096;
    }

    /// Varying bits (inode generation numbers): no need for cryptography.
    /// SplitMix64 over the clock and a counter (works in the browser too).
    pub fn entropy() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut z =
            super::time::realtime_ns().wrapping_add(COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed));
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

pub mod cred {
    /// Files created by the explorer belong to root, as on a freshly made
    /// image (`mkfs` / `debugfs` do the same).
    pub struct Creds {
        pub fsuid: u32,
        pub fsgid: u32,
    }

    pub fn current() -> Creds {
        Creds { fsuid: 0, fsgid: 0 }
    }
}

pub mod process {
    /// The explorer acts with full rights on the image (it may use the
    /// blocks reserved for root).
    pub fn capable(_cap: u32) -> bool {
        true
    }
}

thread_local! {
    static LOG: RefCell<Option<MountLog>> = const { RefCell::new(None) };
}

/// Sends `kinfo!` messages of this thread to `log` while `f` runs.
pub fn with_log<R>(log: &MountLog, f: impl FnOnce() -> R) -> R {
    let old = LOG.with(|l| l.replace(Some(log.clone())));
    let r = f();
    LOG.with(|l| *l.borrow_mut() = old);
    r
}

/// How serious a driver message is, from the fixed parts of its wording
/// (the driver's messages follow Linux's). File and device names are part of
/// messages too, so only these known phrases are matched, never loose words.
fn level_of(message: &str) -> LogLevel {
    const ERRORS: &[&str] = &[
        "EXT4-fs error",
        "invalid superblock checksum",
        "larger than a page",
        "couldn't mount as",
        "Checksum for group",
        "external journals are not supported",
        "inode count doesn't match the groups",
        "unknown checksum algorithm",
        "JBD2: Invalid checksum",
        "JBD2: invalid checksum",
        "JBD2: invalid commit block checksum",
        "JBD2: journal block size differs",
        "JBD2: journal checksum error",
        "JBD2: journal not empty",
        "JBD2: journal too small or bad",
        "JBD2: no valid journal superblock",
        "JBD2: unknown journal features",
        "has holes",
    ];
    const WARNINGS: &[&str] = &[
        "EXT4-fs warning",
        "read-only: features",
        "crash simulated",
        "htree ignored",
        "htree lookup failed",
        "orphan inodes deleted",
        "JBD2: recovery:",
        "blocks to commit: more than one transaction",
    ];
    if ERRORS.iter().any(|p| message.contains(p)) {
        LogLevel::Error
    } else if WARNINGS.iter().any(|p| message.contains(p)) {
        LogLevel::Warning
    } else {
        LogLevel::Info
    }
}

/// Target of `kinfo!`: the current mount log, else nowhere.
pub fn log(message: String) {
    LOG.with(|l| {
        if let Some(log) = &*l.borrow() {
            log.log(level_of(&message), message);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_do_not_change_the_level() {
        let name = "ext4: bad-invalid-error.img: 32768 blocks of 4096 bytes, 32768 inodes, journal";
        assert_eq!(level_of(name), LogLevel::Info);
        assert_eq!(level_of("EXT4-fs error (device x): y"), LogLevel::Error);
        assert_eq!(level_of("JBD2: recovery: 1 transactions (3 blocks) replayed, 0 blocks revoked"), LogLevel::Warning);
        assert_eq!(level_of("JBD2: Invalid checksum recovering data block 5 in transaction 2"), LogLevel::Error);
    }
}
