//! File system drivers.

use crate::caps::{AccessMode, Capabilities};
use crate::device::BlockDevice;
use crate::error::{Error, Result};
use crate::fs::FileSystem;

/// How sure a driver is that a device contains its file system.
/// `0` = definitely not, `100` = magic numbers and checksums match.
pub type ProbeScore = u8;

#[derive(Debug, Clone)]
pub struct DriverInfo {
    /// Stable identifier, e.g. `"ext4"`.
    pub id: &'static str,
    /// Display name, e.g. `"ext2/3/4"`.
    pub name: &'static str,
    pub description: &'static str,
    /// Maximum capabilities the driver implements. The UI uses this to decide
    /// whether "Open read-write" is offered at all.
    pub capabilities: Capabilities,
}

#[derive(Clone, Default)]
pub struct MountOptions {
    pub mode: AccessMode,
    /// Where to report what happens during mounting: dirty-volume checks,
    /// journal replay, repairs. The app shows it live.
    pub log: crate::log::MountLog,
}

#[derive(Debug, Clone, Default)]
pub struct FormatOptions {
    pub label: Option<String>,
    /// Which flavour to create, for drivers with several
    /// ([`Driver::format_variants`], e.g. "ext2", "ext3", "ext4"); `None`:
    /// the driver's default (the first one).
    pub variant: Option<String>,
}

/// Options of a consistency check.
#[derive(Clone, Default)]
pub struct CheckOptions {
    /// Progress and every problem found are reported here, live.
    pub log: crate::log::MountLog,
    /// Repair what can be repaired safely (like `fsck -p`): only when every
    /// problem found is of a kind whose fix is fully determined (counts,
    /// bitmaps, link counts). Otherwise nothing is changed.
    pub repair: bool,
}

/// The outcome of a check (the details are in the log).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Inconsistencies: the file system needs repair.
    pub errors: u64,
    /// Things worth knowing that are not damage (e.g. a pending orphan list).
    pub warnings: u64,
    /// One line, e.g. "4106/32768 inodes, 11136/32768 blocks".
    pub summary: String,
    /// Problems repaired (with `repair`); `errors` is what remains.
    pub fixed: u64,
    /// Every remaining error can be repaired safely (offer "Repair").
    pub repairable: bool,
}

impl CheckReport {
    pub fn is_clean(&self) -> bool {
        self.errors == 0
    }
}

/// A flavour a driver can create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatVariant {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    /// Smallest device this variant fits on.
    pub min_size: u64,
    /// The file name extension a new image of this kind gets (".img",
    /// ".zip").
    pub extension: &'static str,
    /// Whether the image has a size to choose (false: it grows and shrinks
    /// with what it holds, like an archive).
    pub sized: bool,
}

/// A file system implementation.
///
/// Drivers are stateless factories: they recognise ([`probe`](Self::probe)),
/// open ([`mount`](Self::mount)) and optionally create
/// ([`format`](Self::format)) file systems on a [`BlockDevice`].
pub trait Driver: Send + Sync {
    fn info(&self) -> DriverInfo;

    /// Inspects the device and returns a confidence score. Must not modify
    /// the device.
    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore>;

    /// Mounts the file system. The returned volume owns the device.
    ///
    /// Crash recovery belongs here. A read-write mount of a volume that was
    /// not cleanly unmounted should recover it (e.g. replay the journal) and
    /// describe that in `opts.log`. A read-only mount must not write; it
    /// may recover in memory only, or refuse, and say which in the log.
    /// Read-write mounts should mark the volume as in use ("dirty") on disk
    /// so an unclean shutdown is detected next time.
    fn mount(&self, dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>>;

    /// Writes an empty file system onto the device.
    fn format(&self, _dev: &mut dyn BlockDevice, _opts: &FormatOptions) -> Result<()> {
        Err(Error::unsupported("formatting"))
    }

    /// Smallest device size `format` accepts.
    fn min_format_size(&self) -> u64 {
        0
    }

    /// Checks the file system on `dev` for consistency. Without
    /// `opts.repair` the device is read-only and nothing changes (`fsck -n`);
    /// with it, safe repairs are made (`fsck -p`) and the result is checked
    /// again.
    fn check(&self, _dev: Box<dyn BlockDevice>, _opts: &CheckOptions) -> Result<CheckReport> {
        Err(Error::unsupported("checking this file system"))
    }

    /// The flavours `format` can create (empty: just one).
    fn format_variants(&self) -> &'static [FormatVariant] {
        &[]
    }

    /// Whether the driver can be mounted in `mode` at all.
    fn supports_mode(&self, mode: AccessMode) -> bool {
        match mode {
            AccessMode::ReadOnly => self.info().capabilities.contains(Capabilities::READ),
            AccessMode::ReadWrite => self.info().capabilities.is_writable(),
        }
    }
}
