//! A folder of the host as a [`FileSystem`] (native builds only).
//!
//! Exporting is copying from a volume into a `HostFs`, importing is copying
//! from one: the app's copy jobs do both, in pieces, so large files never
//! have to fit in memory. Symbolic links and special files are listed as
//! such and skipped by copies.

use crate::caps::{AccessMode, Capabilities};
use crate::error::{Error, ErrorKind, Result};
use crate::fs::{DirEntry, EntryKind, FileSystem, FsInfo, Metadata};
use crate::path::VfsPath;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub struct HostFs {
    root: PathBuf,
    /// The file last read or written, kept open between pieces.
    open: Option<(PathBuf, File, bool)>,
}

impl HostFs {
    /// The folder `root` (it must exist).
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.is_dir() {
            return Err(Error::not_a_directory(root.display()));
        }
        Ok(Self { root, open: None })
    }

    fn host(&self, path: &VfsPath) -> PathBuf {
        let mut p = self.root.clone();
        for c in path.components() {
            p.push(c);
        }
        p
    }

    /// The open file for `path` (reopened if it is another one, or for
    /// writing when it was opened read-only).
    fn file(&mut self, path: &VfsPath, write: bool) -> Result<&mut File> {
        let host = self.host(path);
        let reuse = matches!(&self.open, Some((p, _, w)) if *p == host && (*w || !write));
        if !reuse {
            let f = if write { OpenOptions::new().read(true).write(true).open(&host) } else { File::open(&host) }
                .map_err(|e| io(e, &host))?;
            self.open = Some((host, f, write));
        }
        Ok(&mut self.open.as_mut().expect("just set").1)
    }

    fn forget(&mut self, path: &VfsPath) {
        let host = self.host(path);
        if self.open.as_ref().is_some_and(|(p, _, _)| p.starts_with(&host)) {
            self.open = None;
        }
    }
}

fn io(e: std::io::Error, path: &Path) -> Error {
    let kind = match e.kind() {
        std::io::ErrorKind::NotFound => ErrorKind::NotFound,
        std::io::ErrorKind::AlreadyExists => ErrorKind::AlreadyExists,
        std::io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        std::io::ErrorKind::StorageFull => ErrorKind::NoSpace,
        std::io::ErrorKind::DirectoryNotEmpty => ErrorKind::DirectoryNotEmpty,
        _ => ErrorKind::Io,
    };
    Error::new(kind, format!("{}: {e}", path.display()))
}

/// A host entry's metadata (a symbolic link's target included).
fn meta_at(path: &std::path::Path, m: &std::fs::Metadata) -> Metadata {
    let mut out = meta_of(m);
    if m.file_type().is_symlink() {
        out.symlink_target = std::fs::read_link(path).ok().map(|t| t.to_string_lossy().into_owned());
    }
    out
}

fn meta_of(m: &std::fs::Metadata) -> Metadata {
    let kind = if m.is_dir() {
        EntryKind::Directory
    } else if m.is_file() {
        EntryKind::File
    } else if m.file_type().is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    };
    let mut out = Metadata::new(kind, if m.is_file() { m.len() } else { 0 });
    out.modified = m.modified().ok().map(crate::time::system_time_to_unix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        out.mode = Some(m.mode());
        out.uid = Some(m.uid());
        out.gid = Some(m.gid());
    }
    out
}

impl FileSystem for HostFs {
    fn info(&self) -> FsInfo {
        FsInfo { driver_id: "host".into(), fs_name: "host".into(), ..Default::default() }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::READ
            .union(Capabilities::WRITE)
            .union(Capabilities::CREATE_DIR)
            .union(Capabilities::REMOVE)
            .union(Capabilities::RENAME)
    }

    fn access_mode(&self) -> AccessMode {
        AccessMode::ReadWrite
    }

    fn read_dir(&mut self, path: &VfsPath) -> Result<Vec<DirEntry>> {
        let host = self.host(path);
        let mut out = Vec::new();
        for e in std::fs::read_dir(&host).map_err(|e| io(e, &host))? {
            let e = e.map_err(|e| io(e, &host))?;
            let Ok(name) = e.file_name().into_string() else { continue };
            if VfsPath::validate_name(&name).is_err() {
                continue;
            }
            let m = std::fs::symlink_metadata(e.path()).map_err(|err| io(err, &e.path()))?;
            out.push(DirEntry { name, metadata: meta_at(&e.path(), &m) });
        }
        Ok(out)
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        let host = self.host(path);
        std::fs::symlink_metadata(&host).map(|m| meta_at(&host, &m)).map_err(|e| io(e, &host))
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let host = self.host(path);
        let f = self.file(path, false)?;
        f.seek(SeekFrom::Start(offset)).map_err(|e| io(e, &host))?;
        let mut done = 0;
        while done < buf.len() {
            match f.read(&mut buf[done..]) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io(e, &host)),
            }
        }
        Ok(done)
    }

    fn write_file(&mut self, path: &VfsPath, data: &[u8]) -> Result<()> {
        let host = self.host(path);
        self.forget(path);
        let new = !host.exists();
        std::fs::write(&host, data).map_err(|e| io(e, &host))?;
        if new {
            crate::device::host::give_to_sudo_user(&host);
        }
        Ok(())
    }

    fn create_file(&mut self, path: &VfsPath) -> Result<()> {
        let host = self.host(path);
        self.forget(path);
        let new = !host.exists();
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&host)
            .map_err(|e| io(e, &host))?;
        if new {
            crate::device::host::give_to_sudo_user(&host);
        }
        self.open = Some((host, f, true));
        Ok(())
    }

    fn write_at(&mut self, path: &VfsPath, offset: u64, data: &[u8]) -> Result<()> {
        let host = self.host(path);
        let f = self.file(path, true)?;
        f.seek(SeekFrom::Start(offset)).map_err(|e| io(e, &host))?;
        f.write_all(data).map_err(|e| io(e, &host))
    }

    fn create_dir(&mut self, path: &VfsPath) -> Result<()> {
        let host = self.host(path);
        std::fs::create_dir(&host).map_err(|e| io(e, &host))?;
        crate::device::host::give_to_sudo_user(&host);
        Ok(())
    }

    fn remove_file(&mut self, path: &VfsPath) -> Result<()> {
        let host = self.host(path);
        self.forget(path);
        std::fs::remove_file(&host).map_err(|e| io(e, &host))
    }

    fn remove_dir(&mut self, path: &VfsPath) -> Result<()> {
        let host = self.host(path);
        self.forget(path);
        std::fs::remove_dir(&host).map_err(|e| io(e, &host))
    }

    fn rename(&mut self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        let (a, b) = (self.host(from), self.host(to));
        self.forget(from);
        if b.exists() {
            return Err(Error::already_exists(to));
        }
        std::fs::rename(&a, &b).map_err(|e| io(e, &a))
    }

    fn sync(&mut self) -> Result<()> {
        // Close the file being written: its data goes to the OS.
        self.open = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_folder_as_a_volume() {
        let dir = std::env::temp_dir().join(format!("dix-hostfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut fs = HostFs::new(&dir).unwrap();
        let p = |s: &str| VfsPath::parse(s).unwrap();
        fs.create_dir(&p("/d")).unwrap();
        fs.create_file(&p("/d/f.bin")).unwrap();
        fs.write_at(&p("/d/f.bin"), 0, b"hello ").unwrap();
        fs.write_at(&p("/d/f.bin"), 6, b"world").unwrap();
        let mut b = [0u8; 5];
        assert_eq!(fs.read(&p("/d/f.bin"), 6, &mut b).unwrap(), 5);
        assert_eq!(&b, b"world");
        fs.sync().unwrap();
        assert_eq!(std::fs::read(dir.join("d/f.bin")).unwrap(), b"hello world");
        let names: Vec<String> = fs.read_dir(&p("/d")).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["f.bin"]);
        fs.rename(&p("/d/f.bin"), &p("/g.bin")).unwrap();
        assert_eq!(fs.metadata(&p("/g.bin")).unwrap().size, 11);
        std::fs::remove_dir_all(&dir).ok();
    }
}
