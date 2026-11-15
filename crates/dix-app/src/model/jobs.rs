//! Background write operations (copy, move, import, delete).
//!
//! A job is a stack of small [`Op`]s. Each step runs one op under the volume
//! lock, so the UI keeps working and the user can eject at any moment, even
//! in the middle of a write. Natively the steps run on a thread; in the
//! browser the app runs a few steps per frame.
//!
//! Cancelling stops between two ops and then syncs (a clean stop). A forced
//! eject instead unplugs the device: the next op fails and nothing more is
//! written, which leaves the image exactly as an interrupted write would.

use super::volume::{lock_fs, SharedFs};
use dix_core::ops::Bundle;
use dix_core::{EntryKind, Result, VfsPath};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Which volume an op works on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Where data is read from (copy/move source).
    Src,
    /// Where data is written.
    Dst,
}

#[derive(Debug)]
pub enum Op {
    /// Copy a file or a whole directory (expanded lazily) from Src to Dst.
    Copy {
        src: VfsPath,
        dst: VfsPath,
    },
    /// The next piece of a large file being copied (from `offset` of
    /// `size`); `created`: the copy made the destination file.
    CopyChunk {
        src: VfsPath,
        dst: VfsPath,
        offset: u64,
        size: u64,
        created: bool,
    },
    /// Write data that came from the host.
    Write {
        dst: VfsPath,
        data: Vec<u8>,
    },
    Mkdir(VfsPath),
    /// Delete a file or a directory tree (expanded lazily).
    Remove {
        side: Side,
        path: VfsPath,
    },
    /// Remove a directory whose children were removed already.
    Rmdir {
        side: Side,
        path: VfsPath,
    },
}

/// A job syncs after this many ops or bytes, like a file system's periodic
/// commit: it bounds what an interruption can lose, without syncing so often
/// that drivers which write a lot per sync (SimpleFS rewrites the whole image)
/// slow to a crawl.
const SYNC_EVERY_OPS: u32 = 2048;
const SYNC_EVERY_BYTES: u64 = 64 * 1024 * 1024;

/// Files larger than this are copied in pieces of this size, so a copy
/// never holds more of a file in memory (and can be cancelled mid-file).
const CHUNK: usize = 4 << 20;

pub struct WriteJob {
    dst: SharedFs,
    src: Option<SharedFs>,
    /// LIFO work list.
    stack: Vec<Op>,
    /// Runs after `stack` is empty (e.g. removing the sources of a move).
    then: Vec<Op>,
    since_sync: (u32, u64),
    /// The buffer pieces of large files go through.
    buf: Vec<u8>,
}

impl WriteJob {
    pub fn new(dst: SharedFs, src: Option<SharedFs>, mut ops: Vec<Op>, mut then: Vec<Op>) -> Self {
        ops.reverse();
        then.reverse();
        Self { dst, src, stack: ops, then, since_sync: (0, 0), buf: Vec::new() }
    }

    fn fs(&self, side: Side) -> &SharedFs {
        match side {
            Side::Dst => &self.dst,
            Side::Src => self.src.as_ref().unwrap_or(&self.dst),
        }
    }

    fn sync(&mut self) -> Result<()> {
        self.since_sync = (0, 0);
        lock_fs(&self.dst).sync()?;
        if let Some(src) = &self.src {
            lock_fs(src).sync()?;
        }
        Ok(())
    }

    /// Writes a file; if that fails (no space left, an I/O error...), a file
    /// it created is removed again rather than left behind half-written.
    /// (One that existed before stays: its old contents are gone anyway.)
    fn write_new(&self, dst: &VfsPath, data: &[u8]) -> Result<()> {
        let mut fs = lock_fs(&self.dst);
        let existed = fs.metadata(dst).is_ok();
        let r = fs.write_file(dst, data);
        if r.is_err() && !existed {
            let _ = fs.remove_file(dst);
        }
        r
    }

    /// One piece of a large file: read from the source, written to the
    /// destination; how many bytes.
    fn copy_chunk(&mut self, src: &VfsPath, dst: &VfsPath, offset: u64, size: u64) -> Result<u64> {
        let want = (size - offset).min(CHUNK as u64) as usize;
        let mut buf = std::mem::take(&mut self.buf);
        buf.resize(want, 0);
        let mut got = 0;
        {
            let mut fs = lock_fs(self.fs(Side::Src));
            while got < want {
                let n = fs.read(src, offset + got as u64, &mut buf[got..])?;
                if n == 0 {
                    return Err(dix_core::Error::new(
                        dix_core::ErrorKind::Io,
                        format!("{src} ended early (it changed while being copied)"),
                    ));
                }
                got += n;
            }
            fs.release_cache(src);
        }
        {
            let mut fs = lock_fs(&self.dst);
            fs.write_at(dst, offset, &buf[..got])?;
            fs.release_cache(dst);
        }
        self.buf = buf;
        Ok(got as u64)
    }

    /// [`abandon`](Self::abandon), then sync: a clean stop.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn abandon_and_sync(&mut self) -> Result<()> {
        self.abandon();
        self.sync()
    }

    /// Cancelling in the middle of a large file: remove the part copied, if
    /// the copy made the file.
    pub fn abandon(&mut self) {
        if let Some(Op::CopyChunk { dst, created: true, .. }) = self.stack.last() {
            let _ = lock_fs(&self.dst).remove_file(dst);
        }
    }

    /// After a failed op: make what was done (and cleaned up) durable, as
    /// far as the file systems still allow.
    fn sync_after_failure(&mut self) {
        let _ = self.sync();
    }

    /// Runs one op. Returns Ok(false) when everything is done (and synced).
    pub fn step(&mut self, progress: &Mutex<JobProgress>) -> Result<bool> {
        let op = match self.stack.pop() {
            Some(op) => op,
            None if !self.then.is_empty() => {
                // Make the copy durable before touching the sources.
                self.sync()?;
                self.stack = std::mem::take(&mut self.then);
                return Ok(true);
            }
            None => {
                self.sync()?;
                return Ok(false);
            }
        };
        let mut bytes = 0u64;
        // A large file counts as one item, when its last piece is done.
        let mut item = 1;
        let mut current = match &op {
            Op::Copy { src, .. } => src.to_string(),
            Op::CopyChunk { src, offset, size, .. } => format!("{src} ({}%)", offset * 100 / (*size).max(1)),
            Op::Write { dst, .. } | Op::Mkdir(dst) => dst.to_string(),
            Op::Remove { path, .. } | Op::Rmdir { path, .. } => path.to_string(),
        };
        match op {
            Op::Copy { src, dst } => {
                let meta = lock_fs(self.fs(Side::Src)).metadata(&src)?;
                match meta.kind {
                    EntryKind::Directory => {
                        let children = lock_fs(self.fs(Side::Src)).read_dir(&src)?;
                        lock_fs(&self.dst).create_dir(&dst)?;
                        for c in children.iter().rev() {
                            if matches!(c.metadata.kind, EntryKind::File | EntryKind::Directory) {
                                self.stack.push(Op::Copy { src: src.join(&c.name)?, dst: dst.join(&c.name)? });
                            }
                        }
                    }
                    EntryKind::File if meta.size <= CHUNK as u64 => {
                        let data = lock_fs(self.fs(Side::Src)).read_to_vec(&src)?;
                        bytes = data.len() as u64;
                        self.write_new(&dst, &data)?;
                    }
                    EntryKind::File => {
                        // Large: an empty file now, then one piece per step.
                        let mut fs = lock_fs(&self.dst);
                        let created = fs.metadata(&dst).is_err();
                        fs.create_file(&dst)?;
                        drop(fs);
                        self.stack.push(Op::CopyChunk { src, dst, offset: 0, size: meta.size, created });
                        item = 0;
                    }
                    _ => {} // symlinks / special files are skipped
                }
            }
            Op::CopyChunk { src, dst, offset, size, created } => match self.copy_chunk(&src, &dst, offset, size) {
                Ok(n) => {
                    bytes = n;
                    current = format!("{src} ({}%)", (offset + n) * 100 / size.max(1));
                    if offset + n < size {
                        self.stack.push(Op::CopyChunk { src, dst, offset: offset + n, size, created });
                        item = 0;
                    }
                }
                Err(e) => {
                    if created {
                        let _ = lock_fs(&self.dst).remove_file(&dst);
                    }
                    return Err(e);
                }
            },
            Op::Write { dst, data } => {
                bytes = data.len() as u64;
                self.write_new(&dst, &data)?;
            }
            Op::Mkdir(path) => lock_fs(&self.dst).create_dir(&path)?,
            Op::Remove { side, path } => {
                let fs = self.fs(side).clone();
                let meta = lock_fs(&fs).metadata(&path)?;
                if meta.is_dir() {
                    let children = lock_fs(&fs).read_dir(&path)?;
                    self.stack.push(Op::Rmdir { side, path: path.clone() });
                    for c in children {
                        self.stack.push(Op::Remove { side, path: path.join(&c.name)? });
                    }
                } else {
                    lock_fs(&fs).remove_file(&path)?;
                }
            }
            Op::Rmdir { side, path } => lock_fs(self.fs(side)).remove_dir(&path)?,
        }
        {
            let mut p = progress.lock().unwrap_or_else(|p| p.into_inner());
            p.items += item;
            p.bytes += bytes;
            p.current = current;
        }
        self.since_sync.0 += 1;
        self.since_sync.1 += bytes;
        if self.since_sync.0 >= SYNC_EVERY_OPS || self.since_sync.1 >= SYNC_EVERY_BYTES {
            self.sync()?;
        }
        Ok(true)
    }
}

/// Turns host files/folders into ops (pre-order: folders before contents).
pub fn bundle_ops(dir: &VfsPath, bundle: Bundle, out: &mut Vec<Op>) -> Result<()> {
    match bundle {
        Bundle::File { name, data, .. } => out.push(Op::Write { dst: dir.join(&name)?, data }),
        Bundle::Dir { name, children } => {
            let path = dir.join(&name)?;
            out.push(Op::Mkdir(path.clone()));
            for c in children {
                bundle_ops(&path, c, out)?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct JobProgress {
    pub items: u64,
    pub bytes: u64,
    /// Path being processed.
    pub current: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEnd {
    Done,
    Cancelled,
    Failed(String),
}

#[derive(Default)]
struct Shared {
    progress: Mutex<JobProgress>,
    end: Mutex<Option<JobEnd>>,
    cancel: AtomicBool,
}

/// A running job.
pub struct JobHandle {
    shared: Arc<Shared>,
    #[cfg(target_arch = "wasm32")]
    job: Option<WriteJob>,
}

impl JobHandle {
    pub fn start(job: WriteJob, notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        let shared = Arc::new(Shared::default());
        #[cfg(not(target_arch = "wasm32"))]
        {
            let worker = shared.clone();
            let mut job = job;
            std::thread::Builder::new()
                .name("write-job".into())
                .spawn(move || {
                    let end = run_to_end(&mut job, &worker, &*notify);
                    *worker.end.lock().unwrap_or_else(|p| p.into_inner()) = Some(end);
                    notify();
                })
                .expect("failed to spawn a job thread");
            Self { shared }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = notify;
            Self { shared, job: Some(job) }
        }
    }

    /// Browser: does a slice of the work. Natively a no-op.
    pub fn pump(&mut self) {
        #[cfg(target_arch = "wasm32")]
        if let Some(job) = &mut self.job {
            for _ in 0..32 {
                let r = if self.shared.cancel.load(Ordering::Relaxed) {
                    Err(None)
                } else {
                    job.step(&self.shared.progress).map_err(Some)
                };
                let end = match r {
                    Ok(true) => continue,
                    Ok(false) => JobEnd::Done,
                    Err(None) => match job.abandon_and_sync() {
                        Ok(()) => JobEnd::Cancelled,
                        Err(e) => JobEnd::Failed(e.to_string()),
                    },
                    Err(Some(e)) => {
                        job.sync_after_failure();
                        JobEnd::Failed(e.to_string())
                    }
                };
                *self.shared.end.lock().unwrap_or_else(|p| p.into_inner()) = Some(end);
                self.job = None;
                break;
            }
        }
    }

    /// Asks the job to stop after the current op (then it syncs).
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }

    pub fn progress(&self) -> JobProgress {
        self.shared.progress.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// `Some` once the job has finished.
    pub fn end(&self) -> Option<JobEnd> {
        self.shared.end.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn run_to_end(job: &mut WriteJob, shared: &Shared, notify: &dyn Fn()) -> JobEnd {
    let mut last_notify = std::time::Instant::now();
    loop {
        if shared.cancel.load(Ordering::Relaxed) {
            job.abandon();
            return match job.sync() {
                Ok(()) => JobEnd::Cancelled,
                Err(e) => JobEnd::Failed(e.to_string()),
            };
        }
        match job.step(&shared.progress) {
            Ok(true) => {}
            Ok(false) => return JobEnd::Done,
            Err(e) => {
                job.sync_after_failure();
                return JobEnd::Failed(e.to_string());
            }
        }
        if last_notify.elapsed() > std::time::Duration::from_millis(100) {
            last_notify = std::time::Instant::now();
            notify();
        }
    }
}

#[cfg(all(test, feature = "simplefs", not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use dix_core::{AccessMode, FormatOptions};

    /// Cancelling in the middle of a large file removes the part copied.
    #[test]
    fn a_cancelled_large_copy_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("dix-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("big.bin"), vec![7u8; 3 * CHUNK + 5]).unwrap();
        let reg = dix_drivers::default_registry();
        let mut dev = dix_core::device::MemoryDevice::zeroed("t.img", 1 << 20);
        reg.format(&mut dev, "simplefs", &FormatOptions::default()).unwrap();
        let fs = reg.mount(Box::new(dev), None, AccessMode::ReadWrite).unwrap().fs;
        let dst: SharedFs = Arc::new(Mutex::new(fs));
        let src: SharedFs = Arc::new(Mutex::new(Box::new(dix_core::hostfs::HostFs::new(&dir).unwrap())));
        let p = |s: &str| VfsPath::parse(s).unwrap();
        let ops = vec![Op::Copy { src: p("/big.bin"), dst: p("/big.bin") }];
        let mut job = WriteJob::new(dst.clone(), Some(src), ops, Vec::new());
        let progress = Mutex::new(JobProgress::default());
        // Create the file, then two of four pieces.
        for _ in 0..3 {
            assert!(job.step(&progress).unwrap());
        }
        assert_eq!(lock_fs(&dst).metadata(&p("/big.bin")).unwrap().size, 2 * CHUNK as u64);
        let label = progress.lock().unwrap().current.clone();
        assert!(label.ends_with("(66%)"), "{label}");
        job.abandon();
        job.sync().unwrap();
        assert!(lock_fs(&dst).metadata(&p("/big.bin")).is_err(), "the partial file was removed");
        std::fs::remove_dir_all(&dir).ok();
    }
}
