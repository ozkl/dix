//! Recursive search inside a volume, by file name or by file contents.
//!
//! The work is split into small, bounded [`SearchJob::step`]s that take the
//! volume lock only for their own duration. Native builds run the steps on a
//! background thread; the browser has no threads, so the app pumps a few
//! steps per frame instead. Either way the UI stays responsive.

use super::volume::{lock_fs, SharedFs};
use dix_core::{DirEntry, EntryKind, FileSystem, VfsPath};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchMode {
    /// Match file and folder names.
    #[default]
    Name,
    /// Match the bytes inside files.
    Content,
}

impl SearchMode {
    pub fn label(self) -> &'static str {
        match self {
            SearchMode::Name => "File",
            SearchMode::Content => "Content",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            SearchMode::Name => SearchMode::Content,
            SearchMode::Content => SearchMode::Name,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: VfsPath,
    pub entry: DirEntry,
}

/// Work units per step (≈ one directory listing, or 4 KiB of file content).
const STEP_BUDGET: u64 = 256;
const CHUNK: usize = 64 * 1024;

/// The resumable state of one search.
pub struct SearchJob {
    mode: SearchMode,
    /// Lower-cased query (names are compared with Unicode lowercase,
    /// contents with ASCII case folding).
    needle: String,
    dirs: Vec<VfsPath>,
    files: Vec<(VfsPath, DirEntry)>,
    /// File currently being scanned and the next offset.
    current: Option<(VfsPath, DirEntry, u64)>,
    /// Tail of the previous chunk, so matches spanning chunks are found.
    carry: Vec<u8>,
}

impl SearchJob {
    pub fn new(root: VfsPath, mode: SearchMode, query: &str) -> Self {
        Self {
            mode,
            needle: query.to_lowercase(),
            dirs: vec![root],
            files: Vec::new(),
            current: None,
            carry: Vec::new(),
        }
    }

    /// Does a bounded amount of work. Returns `true` when the search is finished.
    pub fn step(&mut self, fs: &mut dyn FileSystem, hits: &mut Vec<SearchHit>, scanned: &mut u64) -> bool {
        let mut budget = STEP_BUDGET;
        while budget > 0 {
            if self.mode == SearchMode::Content {
                if let Some((path, entry, offset)) = self.current.take() {
                    budget = budget.saturating_sub(self.scan_chunk(fs, path, entry, offset, hits));
                    continue;
                }
                if let Some((path, entry)) = self.files.pop() {
                    self.carry.clear();
                    *scanned += 1;
                    self.current = Some((path, entry, 0));
                    continue;
                }
            }
            let Some(dir) = self.dirs.pop() else { return true };
            budget = budget.saturating_sub(1);
            let Ok(mut entries) = fs.read_dir(&dir) else { continue };
            budget = budget.saturating_sub(entries.len() as u64 / 64);
            // Reverse so results come out roughly in alphabetical order.
            entries.sort_by(|a, b| b.name.cmp(&a.name));
            for e in entries {
                let Ok(path) = dir.join(&e.name) else { continue };
                match self.mode {
                    SearchMode::Name => {
                        *scanned += 1;
                        if e.name.to_lowercase().contains(&self.needle) {
                            hits.push(SearchHit { path: path.clone(), entry: e.clone() });
                        }
                    }
                    SearchMode::Content if e.metadata.kind == EntryKind::File && e.metadata.size > 0 => {
                        self.files.push((path.clone(), e.clone()));
                    }
                    SearchMode::Content => {}
                }
                if e.metadata.is_dir() {
                    self.dirs.push(path);
                }
            }
        }
        false
    }

    /// Scans one chunk of a file; returns the work units used.
    fn scan_chunk(
        &mut self,
        fs: &mut dyn FileSystem,
        path: VfsPath,
        entry: DirEntry,
        offset: u64,
        hits: &mut Vec<SearchHit>,
    ) -> u64 {
        let mut buf = vec![0u8; CHUNK];
        let n = match fs.read(&path, offset, &mut buf) {
            Ok(n) => n,
            Err(_) => return 1,
        };
        if n == 0 {
            return 1;
        }
        buf.truncate(n);
        let needle = self.needle.as_bytes();
        let mut hay = std::mem::take(&mut self.carry);
        hay.extend_from_slice(&buf);
        if contains_ascii_ci(&hay, needle) {
            hits.push(SearchHit { path, entry });
        } else {
            let keep = needle.len().saturating_sub(1).min(hay.len());
            self.carry = hay[hay.len() - keep..].to_vec();
            self.current = Some((path, entry, offset + n as u64));
        }
        (n as u64 / 4096).max(1)
    }
}

fn contains_ascii_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    hay.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

fn by_path(a: &SearchHit, b: &SearchHit) -> std::cmp::Ordering {
    super::tab::natural_cmp(a.path.as_str(), b.path.as_str())
}

/// State shared between a running search and the UI.
#[derive(Default)]
struct Shared {
    hits: Mutex<Vec<SearchHit>>,
    scanned: AtomicU64,
    done: AtomicBool,
    cancelled: AtomicBool,
}

/// A running (or finished) search. Dropping it cancels the search.
pub struct Search {
    pub query: String,
    pub mode: SearchMode,
    pub root: VfsPath,
    /// Hits collected so far (moved here from the worker by [`Search::poll`]).
    pub hits: Vec<SearchHit>,
    shared: Arc<Shared>,
    /// Browser only: the job is pumped from the UI thread.
    #[cfg(target_arch = "wasm32")]
    job: Option<(SearchJob, SharedFs)>,
}

impl Search {
    /// Starts searching below `root`. `notify` is called whenever new results
    /// arrive (used to wake the UI).
    pub fn start(
        fs: SharedFs,
        root: VfsPath,
        mode: SearchMode,
        query: String,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let shared = Arc::new(Shared::default());
        let job = SearchJob::new(root.clone(), mode, &query);

        #[cfg(not(target_arch = "wasm32"))]
        {
            let worker = shared.clone();
            let mut job = job;
            std::thread::Builder::new()
                .name("search".into())
                .spawn(move || {
                    let mut batch = Vec::new();
                    let mut scanned = 0u64;
                    loop {
                        if worker.cancelled.load(Ordering::Relaxed) {
                            return;
                        }
                        let done = job.step(lock_fs(&fs).as_mut(), &mut batch, &mut scanned);
                        worker.scanned.store(scanned, Ordering::Relaxed);
                        if !batch.is_empty() {
                            // Sort here, off the UI thread; the UI only merges.
                            batch.sort_by(by_path);
                            worker.hits.lock().unwrap_or_else(|p| p.into_inner()).append(&mut batch);
                            notify();
                        }
                        if done {
                            worker.done.store(true, Ordering::Relaxed);
                            notify();
                            return;
                        }
                    }
                })
                .expect("failed to spawn the search thread");
            Self { query, mode, root, hits: Vec::new(), shared }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = notify;
            Self { query, mode, root, hits: Vec::new(), shared, job: Some((job, fs)) }
        }
    }

    /// Collects new results (in the browser this also does a slice of the
    /// work). Returns true if results were added.
    pub fn poll(&mut self) -> bool {
        let mut new = std::mem::take(&mut *self.shared.hits.lock().unwrap_or_else(|p| p.into_inner()));
        #[cfg(target_arch = "wasm32")]
        if let Some((job, fs)) = &mut self.job {
            let mut scanned = self.shared.scanned.load(Ordering::Relaxed);
            let mut done = false;
            for _ in 0..8 {
                if job.step(lock_fs(fs).as_mut(), &mut new, &mut scanned) {
                    done = true;
                    break;
                }
            }
            self.shared.scanned.store(scanned, Ordering::Relaxed);
            if done {
                self.job = None;
                self.shared.done.store(true, Ordering::Relaxed);
            }
        }
        if new.is_empty() {
            return false;
        }
        // Keep results sorted by path. `new` is a concatenation of batches the
        // worker already sorted, which the (run-detecting) sort handles in
        // about linear time; then merge it into the sorted list.
        new.sort_by(by_path);
        let old = std::mem::take(&mut self.hits);
        self.hits.reserve(old.len() + new.len());
        let (mut a, mut b) = (old.into_iter().peekable(), new.into_iter().peekable());
        loop {
            let take_old = match (a.peek(), b.peek()) {
                (Some(x), Some(y)) => by_path(x, y) != std::cmp::Ordering::Greater,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            self.hits.push(if take_old { a.next() } else { b.next() }.unwrap());
        }
        true
    }

    pub fn is_running(&self) -> bool {
        !self.shared.done.load(Ordering::Relaxed)
    }

    /// Items examined so far (entries for name search, files for content search).
    pub fn scanned(&self) -> u64 {
        self.shared.scanned.load(Ordering::Relaxed)
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.shared.cancelled.store(true, Ordering::Relaxed);
    }
}

#[cfg(all(test, feature = "simplefs"))]
mod tests {
    use super::*;
    use dix_core::device::MemoryDevice;
    use dix_core::{AccessMode, FormatOptions};

    fn image() -> Box<dyn FileSystem> {
        let reg = dix_drivers::default_registry();
        let mut dev = MemoryDevice::zeroed("t", 64 * 1024);
        reg.format(&mut dev, "simplefs", &FormatOptions::default()).unwrap();
        let mut fs = reg.mount(Box::new(dev), None, AccessMode::ReadWrite).unwrap().fs;
        let p = |s: &str| VfsPath::parse(s).unwrap();
        fs.create_dir(&p("/docs")).unwrap();
        fs.create_dir(&p("/docs/deep")).unwrap();
        fs.write_file(&p("/docs/Report.txt"), b"quarterly numbers").unwrap();
        // The match spans the 64 KiB chunk boundary.
        let mut big = vec![b'x'; CHUNK - 3];
        big.extend_from_slice(b"NEEDLE");
        fs.write_file(&p("/docs/deep/big.bin"), &big).unwrap();
        fs.write_file(&p("/notes.md"), b"nothing here").unwrap();
        fs
    }

    fn run(fs: &mut dyn FileSystem, mode: SearchMode, q: &str) -> Vec<String> {
        let mut job = SearchJob::new(VfsPath::root(), mode, q);
        let (mut hits, mut scanned) = (Vec::new(), 0);
        while !job.step(fs, &mut hits, &mut scanned) {}
        let mut v: Vec<String> = hits.into_iter().map(|h| h.path.to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn name_and_content_search() {
        let mut fs = image();
        assert_eq!(run(fs.as_mut(), SearchMode::Name, "REPORT"), vec!["/docs/Report.txt"]);
        assert_eq!(run(fs.as_mut(), SearchMode::Name, "e"), vec!["/docs/Report.txt", "/docs/deep", "/notes.md"]);
        assert_eq!(run(fs.as_mut(), SearchMode::Content, "Quarterly"), vec!["/docs/Report.txt"]);
        assert_eq!(run(fs.as_mut(), SearchMode::Content, "needle"), vec!["/docs/deep/big.bin"]);
        assert!(run(fs.as_mut(), SearchMode::Content, "absent").is_empty());
    }

    #[test]
    fn background_search_delivers_results() {
        let fs: SharedFs = Arc::new(Mutex::new(image()));
        let mut s = Search::start(fs, VfsPath::root(), SearchMode::Content, "numbers".into(), Arc::new(|| {}));
        while s.is_running() {
            std::thread::yield_now();
        }
        s.poll();
        assert_eq!(s.hits.len(), 1);
    }

    #[test]
    fn batches_are_merged_in_order() {
        let fs: SharedFs = Arc::new(Mutex::new(image()));
        let mut s = Search::start(fs, VfsPath::root(), SearchMode::Name, "zzz-none".into(), Arc::new(|| {}));
        while s.is_running() {
            std::thread::yield_now();
        }
        s.poll();
        let hit = |p: &str| SearchHit {
            path: VfsPath::parse(p).unwrap(),
            entry: DirEntry { name: p.into(), metadata: dix_core::Metadata::new(EntryKind::File, 0) },
        };
        for batch in [vec![hit("/b10"), hit("/a")], vec![hit("/b2"), hit("/c")], vec![hit("/aa")]] {
            s.shared.hits.lock().unwrap().extend(batch);
            assert!(s.poll());
        }
        let paths: Vec<_> = s.hits.iter().map(|h| h.path.to_string()).collect();
        assert_eq!(paths, vec!["/a", "/aa", "/b2", "/b10", "/c"]);
    }
}
