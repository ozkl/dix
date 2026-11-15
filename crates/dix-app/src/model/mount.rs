//! Mounting in the background, with a log the user can watch.
//!
//! Mounting may take a while (loading a big image, checking a volume that
//! was not cleanly unmounted, replaying a journal). It runs on a thread
//! natively, and the UI shows the driver's [`MountLog`] live.

use dix_core::device::{DetachableDevice, DevicePlug, SharedBuffer};
use dix_core::partition::{self, Partition, PartitionChoice};
use dix_core::{AccessMode, BlockDevice, DeviceInfo, DriverRegistry, MountLog, MountOptions, Mounted, Result};
use std::sync::{Arc, Mutex};

/// The volume and the partition it is in (None: the whole device).
pub type MountResult = Result<(Mounted, Option<Partition>)>;

/// A mount in progress.
pub struct MountTask {
    /// Display name of the image.
    pub name: String,
    pub log: MountLog,
    pub(super) info: DeviceInfo,
    /// The virtual disk format, for a VM disk.
    pub(super) container: Option<String>,
    pub(super) plug: DevicePlug,
    pub(super) buffer: Option<SharedBuffer>,
    /// Remounting an in-memory image that had unsaved changes.
    pub(super) keep_modified: bool,
    /// The partition asked for.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(super) partition: PartitionChoice,
    result: Arc<Mutex<Option<MountResult>>>,
}

impl MountTask {
    /// Starts mounting. The device is made detachable first, so the volume
    /// can later be force-ejected (see [`DevicePlug`]); a partition is a
    /// slice of the detachable device.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        registry: DriverRegistry,
        dev: Box<dyn BlockDevice>,
        name: String,
        driver: Option<String>,
        mode: AccessMode,
        buffer: Option<SharedBuffer>,
        partition: PartitionChoice,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let info = dev.info();
        let container = dev.container();
        let (dev, plug) = DetachableDevice::new(dev);
        let log = MountLog::new();
        let result = Arc::new(Mutex::new(None));
        let opts = MountOptions { mode, log: log.clone() };
        let run = {
            let result = result.clone();
            move || {
                let r = (|| {
                    let mut dev: Box<dyn BlockDevice> = Box::new(dev);
                    let part = partition::resolve(&registry, dev.as_mut(), partition)?;
                    if let Some(p) = &part {
                        opts.log.info(format!("Partition {}", p.label()));
                        dev = partition::slice(dev, p)?;
                    }
                    Ok((registry.mount_with(dev, driver.as_deref(), opts)?, part))
                })();
                *result.lock().unwrap_or_else(|p| p.into_inner()) = Some(r);
                notify();
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::Builder::new().name("mount".into()).spawn(run).expect("failed to spawn the mount thread");
        // No threads in the browser: mount right away (the log is shown afterwards).
        #[cfg(target_arch = "wasm32")]
        run();
        Self { name, log, info, container, plug, buffer, keep_modified: false, partition, result }
    }

    /// The result, once the mount has finished.
    pub fn take_result(&self) -> Option<MountResult> {
        self.result.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
}
