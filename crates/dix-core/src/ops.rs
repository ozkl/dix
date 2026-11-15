//! Generic, driver independent algorithms built on [`FileSystem`]:
//! recursive delete, recursive copy between volumes, tree walks.

use crate::error::{Error, ErrorKind, Result};
use crate::fs::{EntryKind, FileSystem};
use crate::path::VfsPath;

/// An in-memory copy of a file or directory tree. Used for copy & paste
/// (also between two different images) and for importing host files.
#[derive(Debug, Clone)]
pub enum Bundle {
    File { name: String, data: Vec<u8>, modified: Option<i64> },
    Dir { name: String, children: Vec<Bundle> },
}

impl Bundle {
    pub fn name(&self) -> &str {
        match self {
            Bundle::File { name, .. } | Bundle::Dir { name, .. } => name,
        }
    }

    pub fn with_name(mut self, new: String) -> Self {
        match &mut self {
            Bundle::File { name, .. } | Bundle::Dir { name, .. } => *name = new,
        }
        self
    }

    /// Total bytes of file content.
    pub fn total_size(&self) -> u64 {
        match self {
            Bundle::File { data, .. } => data.len() as u64,
            Bundle::Dir { children, .. } => children.iter().map(Bundle::total_size).sum(),
        }
    }
}

/// Reads `path` (file or directory, recursively) into a [`Bundle`].
/// Symlinks and special files are skipped.
pub fn read_bundle(fs: &mut dyn FileSystem, path: &VfsPath) -> Result<Bundle> {
    let meta = fs.metadata(path)?;
    let name = path.file_name().unwrap_or("root").to_owned();
    match meta.kind {
        EntryKind::Directory => {
            let mut children = Vec::new();
            for e in fs.read_dir(path)? {
                if matches!(e.metadata.kind, EntryKind::File | EntryKind::Directory) {
                    children.push(read_bundle(fs, &path.join(&e.name)?)?);
                }
            }
            Ok(Bundle::Dir { name, children })
        }
        EntryKind::File => Ok(Bundle::File { name, data: fs.read_to_vec(path)?, modified: meta.modified }),
        _ => Err(Error::unsupported(format!("copying special file {path}"))),
    }
}

/// Writes a bundle into directory `dest_dir`. Fails if the target exists.
pub fn write_bundle(fs: &mut dyn FileSystem, dest_dir: &VfsPath, bundle: &Bundle) -> Result<VfsPath> {
    let target = dest_dir.join(bundle.name())?;
    if fs.exists(&target) {
        return Err(Error::already_exists(&target));
    }
    match bundle {
        Bundle::File { data, .. } => fs.write_file(&target, data)?,
        Bundle::Dir { children, .. } => {
            fs.create_dir(&target)?;
            for c in children {
                write_bundle(fs, &target, c)?;
            }
        }
    }
    Ok(target)
}

/// Deletes a file or a directory with everything below it.
pub fn remove_all(fs: &mut dyn FileSystem, path: &VfsPath) -> Result<()> {
    if path.is_root() {
        return Err(Error::new(ErrorKind::InvalidInput, "refusing to delete the root directory"));
    }
    let meta = fs.metadata(path)?;
    if meta.is_dir() {
        for e in fs.read_dir(path)? {
            remove_all(fs, &path.join(&e.name)?)?;
        }
        fs.remove_dir(path)
    } else {
        fs.remove_file(path)
    }
}

/// Picks a name that does not exist in `dir`: `a.txt`, `a (2).txt`, …
pub fn unique_name(fs: &mut dyn FileSystem, dir: &VfsPath, wanted: &str) -> Result<String> {
    if !fs.exists(&dir.join(wanted)?) {
        return Ok(wanted.to_owned());
    }
    let (stem, ext) = match wanted.rfind('.') {
        Some(i) if i > 0 => (&wanted[..i], &wanted[i..]),
        _ => (wanted, ""),
    };
    for n in 2..10_000 {
        let candidate = format!("{stem} ({n}){ext}");
        if !fs.exists(&dir.join(&candidate)?) {
            return Ok(candidate);
        }
    }
    Err(Error::already_exists(wanted))
}

/// Counts files, directories and bytes below `path` (inclusive).
pub fn tree_stats(fs: &mut dyn FileSystem, path: &VfsPath) -> Result<(u64, u64, u64)> {
    let meta = fs.metadata(path)?;
    if !meta.is_dir() {
        return Ok((1, 0, meta.size));
    }
    let (mut files, mut dirs, mut bytes) = (0, 1, 0);
    for e in fs.read_dir(path)? {
        let (f, d, b) = tree_stats(fs, &path.join(&e.name)?)?;
        files += f;
        dirs += d;
        bytes += b;
    }
    Ok((files, dirs, bytes))
}
