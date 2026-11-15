//! File system drivers for Disk Image Explorer.
//!
//! Each driver lives in its own module and implements
//! [`dix_core::Driver`]. Add new drivers (ISO 9660, …) here
//! and register them in [`default_registry`].
//!
//! Each driver is a Cargo feature (all on by default): `ext4`, `fat`, `exfat`, `ntfs`, `iso9660`, `udf`, `squashfs`, `littlefs`, `simplefs`,
//! `archive`, and the archive codecs `archive-zstd`, `archive-bzip2`,
//! `archive-xz` and `archive-7z`. A build without a driver simply does not
//! offer it.

extern crate alloc;

#[cfg(feature = "apfs")]
pub mod apfs;
#[cfg(feature = "archive")]
pub mod archive;
#[cfg(feature = "btrfs")]
pub mod btrfs;
#[cfg(any(feature = "hfsplus", feature = "apfs"))]
pub mod decmpfs;
#[cfg(feature = "ext4")]
pub mod ext4;
#[cfg(feature = "fat")]
pub mod fat;
#[cfg(feature = "hfsplus")]
pub mod hfsplus;
pub mod imaging;
#[cfg(feature = "iso9660")]
pub mod iso9660;
#[cfg(all(test, unix, any(feature = "xfs", feature = "btrfs")))]
mod linux_fs_tests;
#[cfg(feature = "littlefs")]
pub mod littlefs;
#[cfg(any(feature = "squashfs", feature = "archive"))]
pub mod lz4;
#[cfg(any(feature = "squashfs", feature = "btrfs"))]
pub mod lzo;
#[cfg(feature = "ntfs")]
pub mod ntfs;
pub mod recover;
#[cfg(feature = "simplefs")]
pub mod simplefs;
#[cfg(feature = "squashfs")]
pub mod squashfs;
#[cfg(feature = "udf")]
pub mod udf;
#[cfg(feature = "vdisk")]
pub mod vdisk;
#[cfg(feature = "xfs")]
pub mod xfs;

#[cfg(all(test, feature = "ext4", not(target_arch = "wasm32")))]
mod partition_tests;

use dix_core::DriverRegistry;

/// A registry with every driver this build includes.
#[allow(unused_mut)]
pub fn default_registry() -> DriverRegistry {
    let mut r = DriverRegistry::new();
    #[cfg(feature = "ext4")]
    r.register(ext4::Ext4Driver);
    #[cfg(feature = "fat")]
    r.register(fat::FatDriver);
    #[cfg(feature = "ntfs")]
    r.register(ntfs::NtfsDriver);
    #[cfg(feature = "hfsplus")]
    r.register(hfsplus::HfsDriver);
    #[cfg(feature = "apfs")]
    r.register(apfs::ApfsDriver);
    #[cfg(feature = "xfs")]
    r.register(xfs::XfsDriver);
    #[cfg(feature = "btrfs")]
    r.register(btrfs::BtrfsDriver);
    #[cfg(feature = "iso9660")]
    r.register(iso9660::IsoDriver);
    #[cfg(feature = "udf")]
    r.register(udf::UdfDriver);
    #[cfg(feature = "squashfs")]
    r.register(squashfs::SquashDriver);
    #[cfg(feature = "littlefs")]
    r.register(littlefs::LittleFsDriver);
    #[cfg(feature = "simplefs")]
    r.register(simplefs::SimpleFsDriver);
    #[cfg(feature = "archive")]
    r.register(archive::ArchiveDriver);
    #[cfg(feature = "vdisk")]
    vdisk::register(&mut r);
    r
}
