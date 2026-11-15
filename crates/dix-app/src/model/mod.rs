//! Application state and operations, **independent of egui**.
//!
//! The UI layer only reads this state and sends [`crate::ui::Action`]s;
//! everything that touches a [`dix_core::FileSystem`] happens here.

mod check;
// (Partition editing is native only; new partitioned images work everywhere.)
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub mod disk;
#[cfg(not(target_arch = "wasm32"))]
pub mod imaging;
mod jobs;
mod links;
mod mount;
mod search;
mod tab;
mod volume;
mod workspace;

pub use check::{CheckOutcome, CheckTask, Reopen, RepairTarget};
pub use jobs::JobEnd;
pub use links::{resolve_link, LinkEnd};
pub use search::{Search, SearchMode};
pub use tab::{type_label as tab_type_label, SortKey, Tab};
pub use volume::{volume_source, Volume, VolumeId};
#[cfg(not(target_arch = "wasm32"))]
pub use workspace::ExportTarget;
pub use workspace::{ScanProgress, Workspace};

/// Is the entry hidden (dot file or hidden attribute)?
pub fn is_hidden(e: &dix_core::DirEntry) -> bool {
    e.name.starts_with('.') || e.metadata.hidden
}
