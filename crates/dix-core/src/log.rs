//! A message channel from drivers to the user, used while mounting and
//! while checking a file system.
//!
//! Mounting can involve real work (checking a dirty volume, replaying a
//! journal) that the user wants to watch. Drivers write to the [`MountLog`]
//! from [`MountOptions`](crate::MountOptions); the application shows the
//! messages live and keeps them with the volume.

use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Info,
    /// Something was wrong but handled (e.g. a journal was replayed).
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub level: LogLevel,
    pub message: String,
}

/// Cloneable, thread-safe log. Readers can poll it while a mount runs.
#[derive(Clone, Default)]
pub struct MountLog {
    entries: Arc<Mutex<Vec<LogEntry>>>,
    progress: Arc<Mutex<Option<(u64, u64)>>>,
}

impl MountLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        e.push(LogEntry { level, message: message.into() });
    }

    pub fn info(&self, message: impl Into<String>) {
        self.log(LogLevel::Info, message);
    }

    pub fn warn(&self, message: impl Into<String>) {
        self.log(LogLevel::Warning, message);
    }

    pub fn error(&self, message: impl Into<String>) {
        self.log(LogLevel::Error, message);
    }

    /// Reports progress of a long step (e.g. transactions replayed of total).
    pub fn progress(&self, done: u64, total: u64) {
        *self.progress.lock().unwrap_or_else(|p| p.into_inner()) = Some((done, total));
    }

    pub fn clear_progress(&self) {
        *self.progress.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    pub fn current_progress(&self) -> Option<(u64, u64)> {
        *self.progress.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn entries(&self) -> Vec<LogEntry> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Highest level logged so far.
    pub fn max_level(&self) -> Option<LogLevel> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner()).iter().map(|e| e.level).max()
    }
}
