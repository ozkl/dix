//! Finding deleted files: a read-only volume of what a file system still
//! remembers of the files deleted from it.
//!
//! Each supported file system has a scanner (NTFS: MFT records no longer in
//! use; FAT12/16/32 and exFAT: directory entries marked deleted) that
//! returns [`Found`] items — a path, times, and how likely the data is to
//! be intact — and a [`Source`] to read their data. [`Recovered`] shows
//! them as an ordinary file system: the original folders, holding only
//! what was deleted (and the folders leading to it), never written to.

use dix_core::{
    AccessMode, BlockDevice, Capabilities, DirEntry, EntryKind, Error, FileSystem, FsInfo, Metadata, Result, VfsPath,
};
use std::collections::BTreeMap;

pub mod carve;
#[cfg(all(test, unix))]
mod carve_tests;
pub mod partitions;
#[cfg(all(test, unix))]
mod partitions_tests;

/// Where a deleted file's parent folder is no longer known.
pub const LOST: &str = "Lost files";

/// Reads a found file's data.
pub trait Source: Send {
    fn read(&mut self, id: usize, offset: u64, buf: &mut [u8]) -> Result<usize>;
}

/// A deleted file or folder.
#[derive(Debug, Clone)]
pub struct Found {
    /// Path components from the volume's root.
    pub path: Vec<String>,
    pub dir: bool,
    pub size: u64,
    pub modified: Option<i64>,
    pub created: Option<i64>,
    pub chance: Chance,
    /// For the source.
    pub id: usize,
}

/// How likely a deleted file's data is to be what it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chance {
    /// Nothing has been written over it (as far as can be told).
    Intact,
    /// This share (0–100) of its space is in use again.
    Overwritten(u8),
    /// Where the data was is not known any more.
    Lost,
    /// A folder.
    Folder,
    /// Found by its content (no file system entry).
    Carved,
}

impl Chance {
    fn note(self) -> String {
        match self {
            Chance::Intact => "deleted · intact".into(),
            Chance::Overwritten(p) if p >= 100 => "deleted · overwritten".into(),
            Chance::Overwritten(p) => format!("deleted · {p}% overwritten"),
            Chance::Lost => "deleted · data lost".into(),
            Chance::Folder => "deleted folder".into(),
            Chance::Carved => "found by content".into(),
        }
    }

    /// From the clusters still free and in use again.
    pub fn of(free: u64, used: u64) -> Chance {
        match used {
            0 => Chance::Intact,
            _ => Chance::Overwritten(((used * 100).div_ceil(free + used)).min(100) as u8),
        }
    }
}

struct Node {
    meta: Metadata,
    id: Option<usize>,
}

/// The deleted files of a volume, as a read-only file system.
pub struct Recovered {
    nodes: BTreeMap<String, Node>,
    source: Box<dyn Source>,
    info: FsInfo,
}

impl Recovered {
    /// `fs_name`: the scanned file system's name.
    pub fn new(fs_name: &str, found: Vec<Found>, source: Box<dyn Source>) -> Recovered {
        Recovered::with_title(fs_name, "deleted files", found, source)
    }

    /// `fs_name · what` names the volume ("NTFS · deleted files").
    pub fn with_title(fs_name: &str, what: &str, found: Vec<Found>, source: Box<dyn Source>) -> Recovered {
        let mut nodes = BTreeMap::new();
        let folder =
            |note: Option<String>| Node { meta: Metadata { note, ..Metadata::new(EntryKind::Directory, 0) }, id: None };
        nodes.insert("/".to_owned(), folder(None));
        let (mut files, mut intact, mut bytes) = (0u64, 0u64, 0u64);
        for f in found {
            let clean: Vec<String> = f
                .path
                .iter()
                .map(|c| c.replace(['/', '\0'], "_"))
                .map(|c| if c.is_empty() || c == "." || c == ".." { "_".into() } else { c })
                .collect();
            if clean.is_empty() {
                continue;
            }
            // The folders leading to it (live ones without a note).
            let mut p = String::new();
            for c in &clean[..clean.len() - 1] {
                p.push('/');
                p.push_str(c);
                nodes.entry(p.clone()).or_insert_with(|| folder(None));
            }
            // Two deleted files of the same name: the later ones numbered.
            let parent = p.clone();
            let name = clean.last().unwrap();
            let mut path = format!("{parent}/{name}");
            let mut k = 2;
            while nodes.get(&path).is_some_and(|n| n.id.is_some() || !f.dir) {
                path = numbered(&parent, name, k);
                k += 1;
            }
            if f.dir {
                // A folder may already be there (holding found files).
                let n = nodes.entry(path).or_insert_with(|| folder(None));
                n.meta.note = Some(f.chance.note());
                n.meta.modified = f.modified;
                n.meta.created = f.created;
                continue;
            }
            files += 1;
            bytes += f.size;
            intact += (f.chance == Chance::Intact) as u64;
            let mut m = Metadata::new(EntryKind::File, f.size);
            m.modified = f.modified;
            m.created = f.created;
            m.note = Some(f.chance.note());
            nodes.insert(path, Node { meta: m, id: Some(f.id) });
        }
        let info = FsInfo {
            driver_id: "recovered".into(),
            fs_name: format!("{fs_name} · {what}"),
            label: None,
            // Not a volume with space of its own: its size and free space
            // mean nothing here (the found files' total is in the details).
            total_bytes: None,
            free_bytes: None,
            block_size: None,
            details: vec![
                ("Files found".into(), files.to_string()),
                ("Intact (as far as can be told)".into(), intact.to_string()),
                ("Their size".into(), format!("{bytes} bytes")),
                (
                    "Note".into(),
                    "Read-only. Export what you want to another disk: writing to this one could overwrite the rest."
                        .into(),
                ),
            ],
        };
        Recovered { nodes, source, info }
    }

    fn node(&self, path: &VfsPath) -> Result<&Node> {
        self.nodes.get(path.as_str()).ok_or_else(|| Error::not_found(path))
    }

    /// How many deleted files were found.
    pub fn count(&self) -> usize {
        self.nodes.values().filter(|n| n.id.is_some()).count()
    }
}

/// "name (k).ext".
fn numbered(parent: &str, name: &str, k: usize) -> String {
    match name.rfind('.').filter(|&i| i > 0) {
        Some(i) => format!("{parent}/{} ({k}){}", &name[..i], &name[i..]),
        None => format!("{parent}/{name} ({k})"),
    }
}

impl FileSystem for Recovered {
    fn info(&self) -> FsInfo {
        self.info.clone()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::READ.union(Capabilities::TIMESTAMPS)
    }

    fn access_mode(&self) -> AccessMode {
        AccessMode::ReadOnly
    }

    fn read_dir(&mut self, path: &VfsPath) -> Result<Vec<DirEntry>> {
        if !self.node(path)?.meta.is_dir() {
            return Err(Error::not_a_directory(path));
        }
        let prefix = if path.is_root() { "/".to_owned() } else { format!("{}/", path.as_str()) };
        Ok(self
            .nodes
            .range(prefix.clone()..)
            .take_while(|(p, _)| p.starts_with(&prefix))
            .filter(|(p, _)| p.len() > prefix.len() && !p[prefix.len()..].contains('/'))
            .map(|(p, n)| DirEntry { name: p[prefix.len()..].to_owned(), metadata: n.meta.clone() })
            .collect())
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        Ok(self.node(path)?.meta.clone())
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let n = self.node(path)?;
        let (Some(id), size) = (n.id, n.meta.size) else { return Err(Error::is_a_directory(path)) };
        if offset >= size {
            return Ok(0);
        }
        let want = (size - offset).min(buf.len() as u64) as usize;
        self.source.read(id, offset, &mut buf[..want])
    }
}

/// Data in runs of the device: (device offset, or None for zeros; bytes).
pub struct Runs {
    pub dev: Box<dyn BlockDevice>,
    pub files: Vec<Vec<(Option<u64>, u64)>>,
}

impl Source for Runs {
    fn read(&mut self, id: usize, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let runs = self.files.get(id).ok_or_else(|| Error::not_found("a found file"))?;
        let (mut pos, mut done) = (0u64, 0usize);
        for &(at, len) in runs {
            if done == buf.len() {
                break;
            }
            let here = offset + done as u64;
            if here >= pos + len {
                pos += len;
                continue;
            }
            let within = here - pos;
            let n = (len - within).min((buf.len() - done) as u64) as usize;
            match at {
                Some(a) if a + within + n as u64 <= self.dev.size() => {
                    self.dev.read_at(a + within, &mut buf[done..done + n])?
                }
                _ => buf[done..done + n].fill(0),
            }
            done += n;
            pos += len;
        }
        buf[done..].fill(0);
        Ok(buf.len())
    }
}

/// The deleted files of the file system on `dev` (NTFS, FAT or exFAT).
pub fn deleted_files(mut dev: Box<dyn BlockDevice>) -> Result<Box<dyn FileSystem>> {
    #[cfg(feature = "ntfs")]
    if dix_core::Driver::probe(&crate::ntfs::NtfsDriver, dev.as_mut())? > 0 {
        return Ok(Box::new(crate::ntfs::undelete::scan(dev)?));
    }
    #[cfg(feature = "fat")]
    {
        let mut b = [0u8; 512];
        dev.read_at(0, &mut b)?;
        #[cfg(feature = "exfat")]
        if &b[3..11] == crate::fat::exfat::OEM {
            return Ok(Box::new(crate::fat::exfat::undelete::scan(dev)?));
        }
        if crate::fat::bpb::Layout::parse(&b).is_ok() {
            return Ok(Box::new(crate::fat::undelete::scan(dev)?));
        }
    }
    let _ = &mut dev;
    Err(Error::unsupported("finding deleted files on this file system (NTFS, FAT and exFAT are supported)"))
}

/// Whether [`deleted_files`] can look at volumes of this driver.
pub fn supports(driver_id: &str) -> bool {
    matches!(driver_id, "ntfs" | "fat")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chances_and_names() {
        assert_eq!(Chance::of(10, 0), Chance::Intact);
        assert_eq!(Chance::of(3, 1), Chance::Overwritten(25));
        assert_eq!(numbered("/d", "a.txt", 2), "/d/a (2).txt");
        assert_eq!(numbered("", "README", 3), "/README (3)");
    }
}
