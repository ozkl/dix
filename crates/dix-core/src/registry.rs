//! The set of available drivers and auto-detection.

use crate::caps::{AccessMode, Capabilities};
use crate::container::{DiskFormat, FileOpener, OpenContext};
use crate::device::{BlockDevice, ReadOnlyDevice};
use crate::driver::{Driver, FormatOptions, MountOptions, ProbeScore};
use crate::error::{Error, ErrorKind, Result};
use crate::fs::FileSystem;
use std::sync::Arc;

/// A mounted volume together with the driver that produced it.
pub struct Mounted {
    pub fs: Box<dyn FileSystem>,
    pub driver: Arc<dyn Driver>,
}

#[derive(Default, Clone)]
pub struct DriverRegistry {
    drivers: Vec<Arc<dyn Driver>>,
    formats: Vec<Arc<dyn DiskFormat>>,
}

impl DriverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, driver: impl Driver + 'static) -> &mut Self {
        self.drivers.push(Arc::new(driver));
        self
    }

    /// Adds a virtual disk container format (qcow2, VHD, …).
    pub fn register_format(&mut self, format: impl DiskFormat + 'static) -> &mut Self {
        self.formats.push(Arc::new(format));
        self
    }

    pub fn formats(&self) -> &[Arc<dyn DiskFormat>] {
        &self.formats
    }

    /// `dev` as the virtual disk it holds when it is a container (a VM
    /// disk), else unchanged. `opener` opens the other files a container
    /// may refer to (VMDK extents, the parent of a differencing disk).
    pub fn open_disk(&self, dev: Box<dyn BlockDevice>, opener: Option<FileOpener<'_>>) -> Result<Box<dyn BlockDevice>> {
        OpenContext::new(&self.formats, opener).unwrap(dev)
    }

    pub fn drivers(&self) -> &[Arc<dyn Driver>] {
        &self.drivers
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Driver>> {
        self.drivers.iter().find(|d| d.info().id == id).cloned()
    }

    /// Drivers that can create new images.
    pub fn formatters(&self) -> impl Iterator<Item = &Arc<dyn Driver>> {
        self.drivers.iter().filter(|d| d.info().capabilities.contains(Capabilities::FORMAT))
    }

    /// Probes every driver; returns candidates with a non-zero score, best first.
    pub fn detect(&self, dev: &mut dyn BlockDevice) -> Vec<(Arc<dyn Driver>, ProbeScore)> {
        let mut found: Vec<_> = self
            .drivers
            .iter()
            .filter_map(|d| match d.probe(dev) {
                Ok(score) if score > 0 => Some((d.clone(), score)),
                _ => None,
            })
            .collect();
        found.sort_by_key(|f| std::cmp::Reverse(f.1));
        found
    }

    /// Mounts `dev`, either with the driver `driver_id` or the best match.
    ///
    /// For read-only mounts the device is wrapped in [`ReadOnlyDevice`] so the
    /// image cannot be modified even by a misbehaving driver.
    pub fn mount(&self, dev: Box<dyn BlockDevice>, driver_id: Option<&str>, mode: AccessMode) -> Result<Mounted> {
        self.mount_with(dev, driver_id, MountOptions { mode, ..Default::default() })
    }

    /// Like [`mount`](Self::mount) but with full options (e.g. a log to watch).
    pub fn mount_with(
        &self,
        mut dev: Box<dyn BlockDevice>,
        driver_id: Option<&str>,
        opts: MountOptions,
    ) -> Result<Mounted> {
        let mode = opts.mode;
        let driver = match driver_id {
            Some(id) => {
                self.get(id).ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no driver named '{id}'")))?
            }
            None => self
                .detect(dev.as_mut())
                .into_iter()
                .next()
                .map(|(d, _)| d)
                .ok_or_else(|| Error::new(ErrorKind::UnknownFormat, "no driver recognised this image"))?,
        };
        let info = driver.info();
        if !driver.supports_mode(mode) {
            return Err(Error::new(
                ErrorKind::ReadOnly,
                format!("the {} driver does not support {} access", info.name, mode.label().to_lowercase()),
            ));
        }
        if mode.is_writable() && dev.is_read_only() {
            let why = if dev.parts().is_some() {
                "the parts of a split file can only be read: open it read-only".to_string()
            } else if let Some(what) = dev.container() {
                format!("this disk ({what}) can only be read: open it read-only")
            } else {
                "the device is read-only".to_string()
            };
            return Err(Error::new(ErrorKind::ReadOnly, why));
        }
        if !mode.is_writable() {
            dev = Box::new(ReadOnlyDevice(dev));
        }
        opts.log.info(format!("Mounting with the {} driver ({})", info.name, mode.label().to_lowercase()));
        let fs = driver.mount(dev, &opts)?;
        Ok(Mounted { fs, driver })
    }

    /// Checks the file system on `dev` (detected, or with `driver_id`).
    /// Unless `opts.repair` is set the device is wrapped read-only: a check
    /// never changes the image.
    pub fn check(
        &self,
        mut dev: Box<dyn BlockDevice>,
        driver_id: Option<&str>,
        opts: &crate::driver::CheckOptions,
    ) -> Result<crate::driver::CheckReport> {
        let driver = match driver_id {
            Some(id) => {
                self.get(id).ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no driver named '{id}'")))?
            }
            None => self
                .detect(dev.as_mut())
                .into_iter()
                .next()
                .map(|(d, _)| d)
                .ok_or_else(|| Error::new(ErrorKind::UnknownFormat, "no driver recognised this image"))?,
        };
        if !driver.info().capabilities.contains(Capabilities::CHECK) {
            return Err(Error::unsupported(format!("checking {} file systems", driver.info().name)));
        }
        if opts.repair {
            if dev.is_read_only() {
                return Err(Error::new(ErrorKind::ReadOnly, "the image is read-only: it cannot be repaired"));
            }
            opts.log.info(format!("Checking and repairing with the {} driver", driver.info().name));
            driver.check(dev, opts)
        } else {
            opts.log.info(format!("Checking with the {} driver (read-only)", driver.info().name));
            driver.check(Box::new(ReadOnlyDevice(dev)), opts)
        }
    }

    /// Formats `dev` with driver `driver_id`.
    pub fn format(&self, dev: &mut dyn BlockDevice, driver_id: &str, opts: &FormatOptions) -> Result<()> {
        let driver = self
            .get(driver_id)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no driver named '{driver_id}'")))?;
        let min = opts
            .variant
            .as_deref()
            .and_then(|v| driver.format_variants().iter().find(|f| f.id == v))
            .map_or(driver.min_format_size(), |f| f.min_size);
        if dev.size() < min {
            if dev.info().resizable {
                dev.resize(min)?;
            } else {
                return Err(Error::new(ErrorKind::NoSpace, "device is too small"));
            }
        }
        driver.format(dev, opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemoryDevice;
    use crate::driver::DriverInfo;
    use crate::fs::{DirEntry, FsInfo, Metadata};
    use crate::path::VfsPath;

    /// A read-only driver that accepts everything (tests only).
    struct ReadOnlyDriver;

    impl Driver for ReadOnlyDriver {
        fn info(&self) -> DriverInfo {
            DriverInfo { id: "ro", name: "RO", description: "", capabilities: Capabilities::READ }
        }
        fn probe(&self, _dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
            Ok(1)
        }
        fn mount(&self, _dev: Box<dyn BlockDevice>, _opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
            struct Empty;
            impl FileSystem for Empty {
                fn info(&self) -> FsInfo {
                    FsInfo::default()
                }
                fn capabilities(&self) -> Capabilities {
                    Capabilities::READ
                }
                fn access_mode(&self) -> AccessMode {
                    AccessMode::ReadOnly
                }
                fn read_dir(&mut self, _p: &VfsPath) -> Result<Vec<DirEntry>> {
                    Ok(Vec::new())
                }
                fn metadata(&mut self, _p: &VfsPath) -> Result<Metadata> {
                    Ok(Metadata::new(crate::fs::EntryKind::Directory, 0))
                }
                fn read(&mut self, _p: &VfsPath, _o: u64, _b: &mut [u8]) -> Result<usize> {
                    Ok(0)
                }
            }
            Ok(Box::new(Empty))
        }
    }

    #[test]
    fn read_only_drivers_cannot_be_mounted_read_write() {
        let mut reg = DriverRegistry::new();
        reg.register(ReadOnlyDriver);
        let dev = || Box::new(MemoryDevice::zeroed("x", 512));
        assert!(reg.mount(dev(), None, AccessMode::ReadOnly).is_ok());
        assert_eq!(reg.mount(dev(), None, AccessMode::ReadWrite).err().unwrap().kind(), ErrorKind::ReadOnly);
    }

    #[test]
    fn nothing_registered_means_unknown_format() {
        let reg = DriverRegistry::new();
        let r = reg.mount(Box::new(MemoryDevice::zeroed("x", 512)), None, AccessMode::ReadOnly);
        assert_eq!(r.err().unwrap().kind(), ErrorKind::UnknownFormat);
    }
}
