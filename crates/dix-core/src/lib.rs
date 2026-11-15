//! # dix-core
//!
//! The UI-independent heart of Disk Image Explorer.
//!
//! The crate is organised in three layers, each defined by a trait:
//!
//! ```text
//!  ┌───────────────────────────── UI (dix-app) ─────────────────────────────┐
//!  │ talks only to `dyn FileSystem` + `DriverRegistry` + `open_source`      │
//!  └────────────────────────────────────────────────────────────────────────┘
//!  ┌────────────── FileSystem (mounted volume, path based API) ─────────────┐
//!  │ produced by a `Driver` (ext4, fat, simplefs, raw, …)                   │
//!  └────────────────────────────────────────────────────────────────────────┘
//!  ┌────────────── BlockDevice (random access bytes) ───────────────────────┐
//!  │ MemoryDevice (browser upload) · FileDevice (image file) · raw disks    │
//!  │ · SliceDevice (partition) · ReadOnlyDevice · AlignedDevice             │
//!  └────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Drivers never know where the bytes come from and the UI never knows how
//! a file system is laid out on disk.

pub mod caps;
pub mod container;
pub mod device;
pub mod driver;
pub mod error;
pub mod fs;
#[cfg(not(target_arch = "wasm32"))]
pub mod hostfs;
pub mod imaging;
pub mod log;
pub mod ops;
pub mod partition;
pub mod path;
pub mod registry;
pub mod time;

pub use caps::{AccessMode, Capabilities};
pub use device::{BlockDevice, DeviceInfo, DeviceKind};
pub use driver::{
    CheckOptions, CheckReport, Driver, DriverInfo, FormatOptions, FormatVariant, MountOptions, ProbeScore,
};
pub use error::{Error, ErrorKind, Result};
pub use fs::{DirEntry, EntryKind, FileSystem, FsInfo, Metadata, Setting, SettingKind};
pub use log::{LogEntry, LogLevel, MountLog};
pub use path::VfsPath;
pub use registry::{DriverRegistry, Mounted};
