//! Checking (and safely repairing) a file system in the background, with a
//! log the user can watch.

use super::volume::VolumeId;
use dix_core::device::SharedBuffer;
use dix_core::partition::PartitionChoice;
use dix_core::{AccessMode, BlockDevice, CheckOptions, CheckReport, DriverRegistry, LogEntry, MountLog, Result};
use std::sync::{Arc, Mutex};

/// What to open again after a check or repair (the image was ejected for it).
#[derive(Clone)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub enum Reopen {
    /// An image on the host.
    Path { path: std::path::PathBuf, partition: PartitionChoice, mode: AccessMode, driver: String },
    /// An in-memory image: its bytes are still in `buffer`.
    Memory {
        name: String,
        buffer: SharedBuffer,
        partition: PartitionChoice,
        mode: AccessMode,
        driver: String,
        modified: bool,
    },
}

/// What "Repair" would work on, offered with a check's report.
#[derive(Clone, Debug)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub enum RepairTarget {
    /// An image on the host (open or not), or one of its partitions.
    Path { path: std::path::PathBuf, partition: PartitionChoice },
    /// An open in-memory image.
    Volume(VolumeId),
}

/// A finished check.
pub struct CheckOutcome {
    pub name: String,
    pub result: Result<CheckReport>,
    pub log: Vec<LogEntry>,
    pub reopen: Option<Reopen>,
    /// Where a repair could be made (for the report's Repair button).
    pub target: Option<RepairTarget>,
    /// This was a repair run.
    pub repaired: bool,
}

pub struct CheckTask {
    pub name: String,
    pub log: MountLog,
    pub repair: bool,
    pub(super) reopen: Option<Reopen>,
    pub(super) target: Option<RepairTarget>,
    result: Arc<Mutex<Option<Result<CheckReport>>>>,
}

impl CheckTask {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        registry: DriverRegistry,
        dev: Box<dyn BlockDevice>,
        name: String,
        driver: Option<String>,
        repair: bool,
        reopen: Option<Reopen>,
        target: Option<RepairTarget>,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let log = MountLog::new();
        let result = Arc::new(Mutex::new(None));
        let run = {
            let (result, log) = (result.clone(), log.clone());
            move || {
                let r = registry.check(dev, driver.as_deref(), &CheckOptions { log, repair });
                *result.lock().unwrap_or_else(|p| p.into_inner()) = Some(r);
                notify();
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::Builder::new().name("check".into()).spawn(run).expect("failed to spawn the check thread");
        #[cfg(target_arch = "wasm32")]
        run();
        Self { name, log, repair, reopen, target, result }
    }

    pub fn take_result(&self) -> Option<Result<CheckReport>> {
        self.result.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
}
