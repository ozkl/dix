use super::search::{Search, SearchHit, SearchMode};
use super::volume::VolumeId;
use dix_core::{DirEntry, FileSystem, VfsPath};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Modified,
    Type,
    Size,
}

impl SortKey {
    pub const ALL: [SortKey; 4] = [SortKey::Name, SortKey::Modified, SortKey::Type, SortKey::Size];

    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::Modified => "Date modified",
            SortKey::Type => "Type",
            SortKey::Size => "Size",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
    pub key: SortKey,
    pub ascending: bool,
    /// List folders before files (whatever the sort key and direction).
    pub folders_first: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self { key: SortKey::Name, ascending: true, folders_first: true }
    }
}

/// The "Type" column text.
pub fn type_label(e: &DirEntry) -> String {
    use dix_core::EntryKind::*;
    match e.metadata.kind {
        Directory => "Folder".into(),
        Symlink => "Symbolic link".into(),
        Other => "Special file".into(),
        File => match e.name.rfind('.') {
            Some(i) if i > 0 && i + 1 < e.name.len() => format!("{} file", e.name[i + 1..].to_uppercase()),
            _ => "File".into(),
        },
    }
}

/// One explorer tab: a location inside a volume plus history and selection.
pub struct Tab {
    pub volume: VolumeId,
    pub path: VfsPath,
    back: Vec<VfsPath>,
    forward: Vec<VfsPath>,
    /// Raw listing of `path`.
    entries: Vec<DirEntry>,
    /// Indices into `entries`, filtered and sorted.
    view: Vec<usize>,
    pub error: Option<String>,
    /// Selected names. A hash set: the list and status bar query it for
    /// every visible entry on every frame.
    pub selection: HashSet<String>,
    /// Last clicked row (index into the view) for shift-selection.
    pub anchor: Option<usize>,
    /// Text in the search box.
    pub search_query: String,
    pub search_mode: SearchMode,
    /// The query/mode changed and a (debounced) search should start.
    pub search_pending: bool,
    /// Active search; while `Some`, the list shows its results.
    pub search: Option<Search>,
    /// Selected row in the search results.
    pub result_sel: Option<usize>,
    /// Indices of the shown search hits (hidden ones filtered), rebuilt only
    /// when hits arrive or the hidden toggle changes.
    hits_view: Vec<usize>,
    pub sort: SortSpec,
    pub show_hidden: bool,
}

impl Tab {
    pub fn new(volume: VolumeId, path: VfsPath) -> Self {
        Self {
            volume,
            path,
            back: Vec::new(),
            forward: Vec::new(),
            entries: Vec::new(),
            view: Vec::new(),
            error: None,
            selection: HashSet::new(),
            anchor: None,
            search_query: String::new(),
            search_mode: SearchMode::Name,
            search_pending: false,
            search: None,
            result_sel: None,
            hits_view: Vec::new(),
            sort: SortSpec::default(),
            show_hidden: false,
        }
    }

    pub fn title(&self, volume_name: &str) -> String {
        self.path.file_name().map(str::to_owned).unwrap_or_else(|| volume_name.to_owned())
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Goes to `path` (recording history). Returns false if it is not a directory.
    pub fn navigate(&mut self, fs: &mut dyn FileSystem, path: VfsPath) {
        if path == self.path {
            self.refresh(fs);
            return;
        }
        self.clear_search();
        let old = std::mem::replace(&mut self.path, path);
        self.back.push(old);
        self.forward.clear();
        self.selection.clear();
        self.anchor = None;
        self.refresh(fs);
    }

    pub fn go_back(&mut self, fs: &mut dyn FileSystem) {
        if let Some(p) = self.back.pop() {
            self.clear_search();
            let cur = std::mem::replace(&mut self.path, p);
            self.forward.push(cur);
            self.selection.clear();
            self.refresh(fs);
        }
    }

    pub fn go_forward(&mut self, fs: &mut dyn FileSystem) {
        if let Some(p) = self.forward.pop() {
            self.clear_search();
            let cur = std::mem::replace(&mut self.path, p);
            self.back.push(cur);
            self.selection.clear();
            self.refresh(fs);
        }
    }

    pub fn go_up(&mut self, fs: &mut dyn FileSystem) {
        if let Some(parent) = self.path.parent() {
            let child = self.path.file_name().map(str::to_owned);
            self.navigate(fs, parent);
            if let Some(c) = child {
                self.selection.insert(c);
            }
        }
    }

    /// Re-reads the directory. If it vanished, walks up to the closest existing parent.
    pub fn refresh(&mut self, fs: &mut dyn FileSystem) {
        loop {
            match fs.read_dir(&self.path) {
                Ok(entries) => {
                    self.entries = entries;
                    self.error = None;
                    break;
                }
                Err(e) => match self.path.parent() {
                    Some(parent) if e.kind() == dix_core::ErrorKind::NotFound => self.path = parent,
                    _ => {
                        self.entries.clear();
                        self.error = Some(e.to_string());
                        break;
                    }
                },
            }
        }
        let names: HashSet<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
        self.selection.retain(|s| names.contains(s.as_str()));
        self.rebuild_view();
    }

    /// Re-applies the hidden flag and sort order.
    pub fn rebuild_view(&mut self) {
        self.rebuild_hits_view();
        let mut view: Vec<usize> =
            (0..self.entries.len()).filter(|&i| self.show_hidden || !super::is_hidden(&self.entries[i])).collect();
        let entries = &self.entries;
        let spec = self.sort;
        let types: Vec<String> =
            if spec.key == SortKey::Type { entries.iter().map(type_label).collect() } else { Vec::new() };
        view.sort_by(|&ia, &ib| {
            let (a, b) = (&entries[ia], &entries[ib]);
            let dirs = if spec.folders_first {
                b.metadata.is_dir().cmp(&a.metadata.is_dir())
            } else {
                std::cmp::Ordering::Equal
            };
            let by_name = || natural_cmp(&a.name, &b.name);
            let ord = match spec.key {
                SortKey::Name => by_name(),
                SortKey::Modified => a.metadata.modified.cmp(&b.metadata.modified).then_with(by_name),
                SortKey::Type => types[ia].cmp(&types[ib]).then_with(by_name),
                SortKey::Size => a.metadata.size.cmp(&b.metadata.size).then_with(by_name),
            };
            dirs.then(if spec.ascending { ord } else { ord.reverse() })
        });
        self.view = view;
    }

    /// Visible entries in display order.
    pub fn visible(&self) -> impl ExactSizeIterator<Item = &DirEntry> + '_ {
        self.view.iter().map(|&i| &self.entries[i])
    }

    pub fn visible_len(&self) -> usize {
        self.view.len()
    }

    pub fn visible_at(&self, idx: usize) -> Option<&DirEntry> {
        self.view.get(idx).map(|&i| &self.entries[i])
    }

    pub fn entry(&self, name: &str) -> Option<&DirEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn total_len(&self) -> usize {
        self.entries.len()
    }

    pub fn selected_entries(&self) -> impl Iterator<Item = &DirEntry> + '_ {
        self.visible().filter(|e| self.selection.contains(&e.name))
    }

    pub fn selected_paths(&self) -> Vec<VfsPath> {
        self.selected_entries().filter_map(|e| self.path.join(&e.name).ok()).collect()
    }

    pub fn set_sort(&mut self, key: SortKey) {
        if self.sort.key == key {
            self.sort.ascending = !self.sort.ascending;
        } else {
            self.sort = SortSpec { key, ascending: true, ..self.sort };
        }
        self.rebuild_view();
    }

    /// Click handling with ctrl/shift semantics. `idx` is a view index.
    pub fn click(&mut self, idx: usize, toggle: bool, range: bool) {
        let Some(name) = self.visible_at(idx).map(|e| e.name.clone()) else { return };
        if range {
            let anchor = self.anchor.unwrap_or(idx);
            let (lo, hi) = (anchor.min(idx), anchor.max(idx));
            if !toggle {
                self.selection.clear();
            }
            let names: Vec<String> = (lo..=hi).filter_map(|i| self.visible_at(i).map(|e| e.name.clone())).collect();
            self.selection.extend(names);
        } else if toggle {
            if !self.selection.remove(&name) {
                self.selection.insert(name);
            }
            self.anchor = Some(idx);
        } else {
            self.selection.clear();
            self.selection.insert(name);
            self.anchor = Some(idx);
        }
    }

    pub fn select_only(&mut self, name: &str) {
        self.selection.clear();
        self.selection.insert(name.to_owned());
        self.anchor = self.view.iter().position(|&i| self.entries[i].name == name);
    }

    pub fn select_all(&mut self) {
        let names: Vec<String> = self.visible().map(|e| e.name.clone()).collect();
        self.selection.extend(names);
    }

    /// Moves the keyboard cursor by `delta` rows.
    pub fn move_cursor(&mut self, delta: isize, extend: bool) {
        let n = self.view.len();
        if n == 0 {
            return;
        }
        let cur =
            self.anchor.or_else(|| self.view.iter().position(|&i| self.selection.contains(&self.entries[i].name)));
        let next = match cur {
            None => 0,
            Some(c) => (c as isize + delta).clamp(0, n as isize - 1) as usize,
        };
        if extend {
            let name = self.entries[self.view[next]].name.clone();
            self.selection.insert(name);
        } else {
            self.selection.clear();
            self.selection.insert(self.entries[self.view[next]].name.clone());
        }
        self.anchor = Some(next);
    }

    pub fn clear_search(&mut self) {
        self.search_query.clear();
        self.search_pending = false;
        self.search = None;
        self.result_sel = None;
        self.hits_view.clear();
    }

    /// Re-filters the search hits (call after new hits or a hidden toggle).
    pub fn rebuild_hits_view(&mut self) {
        let show_hidden = self.show_hidden;
        self.hits_view = match &self.search {
            Some(s) => (0..s.hits.len()).filter(|&i| show_hidden || !super::is_hidden(&s.hits[i].entry)).collect(),
            None => Vec::new(),
        };
    }

    /// Number of search results shown.
    pub fn hit_count(&self) -> usize {
        self.hits_view.len()
    }

    /// The `i`-th shown search result.
    pub fn hit(&self, i: usize) -> Option<&SearchHit> {
        self.search.as_ref()?.hits.get(*self.hits_view.get(i)?)
    }

    /// The entry the preview pane should show: the selected search result or
    /// the single selected entry of the folder.
    pub fn preview_target(&self) -> Option<(VfsPath, DirEntry)> {
        if self.search.is_some() {
            let h = self.hit(self.result_sel?)?;
            return Some((h.path.clone(), h.entry.clone()));
        }
        let e = self.single_selection()?;
        Some((self.path.join(&e.name).ok()?, e.clone()))
    }

    /// The single selected entry, if exactly one is selected.
    pub fn single_selection(&self) -> Option<&DirEntry> {
        if self.selection.len() == 1 {
            self.entry(self.selection.iter().next()?)
        } else {
            None
        }
    }
}

/// Case-insensitive "natural" comparison: `file2` < `file10`.
/// Allocation free: it runs for every comparison when sorting large folders.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < ab.len() && j < bb.len() {
        if ab[i].is_ascii_digit() && bb[j].is_ascii_digit() {
            // Compare digit runs by value: skip leading zeros, then length, then digits.
            let run = |s: &[u8], mut k: usize| {
                while k < s.len() && s[k] == b'0' {
                    k += 1;
                }
                let start = k;
                while k < s.len() && s[k].is_ascii_digit() {
                    k += 1;
                }
                (start, k)
            };
            let ((sa, ea), (sb, eb)) = (run(ab, i), run(bb, j));
            let ord = (ea - sa).cmp(&(eb - sb)).then_with(|| ab[sa..ea].cmp(&bb[sb..eb]));
            if ord != Ordering::Equal {
                return ord;
            }
            i = ea;
            j = eb;
            continue;
        }
        // Digits are ASCII, so i/j are always on char boundaries here.
        let x = a[i..].chars().next().unwrap();
        let y = b[j..].chars().next().unwrap();
        let ord = if x.is_ascii() && y.is_ascii() {
            x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase())
        } else {
            x.to_lowercase().cmp(y.to_lowercase())
        };
        if ord != Ordering::Equal {
            return ord;
        }
        i += x.len_utf8();
        j += y.len_utf8();
    }
    (ab.len() - i).cmp(&(bb.len() - j)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_first_toggle() {
        use dix_core::{EntryKind, Metadata};
        let mut t = Tab::new(VolumeId(1), VfsPath::root());
        let e = |n: &str, k| DirEntry { name: n.into(), metadata: Metadata::new(k, 0) };
        t.entries = vec![e("b-dir", EntryKind::Directory), e("a.txt", EntryKind::File), e("c.txt", EntryKind::File)];
        t.rebuild_view();
        let names = |t: &Tab| t.visible().map(|e| e.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&t), vec!["b-dir", "a.txt", "c.txt"]);
        t.sort.folders_first = false;
        t.rebuild_view();
        assert_eq!(names(&t), vec!["a.txt", "b-dir", "c.txt"]);
        // Descending keeps folders on top when the option is on.
        t.sort = SortSpec { key: SortKey::Name, ascending: false, folders_first: true };
        t.rebuild_view();
        assert_eq!(names(&t), vec!["b-dir", "c.txt", "a.txt"]);
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["file10", "File2", "file1", "a", "file01", "Äpfel", "äb", "file1a", "z9", "z09x"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["a", "file01", "file1", "file1a", "File2", "file10", "z9", "z09x", "äb", "Äpfel"]);
        assert_eq!(natural_cmp("x", "x"), std::cmp::Ordering::Equal);
    }
}
