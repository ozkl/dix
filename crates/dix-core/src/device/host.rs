//! Access to the host machine's storage (native builds only).
//!
//! * regular image files (`disk.img`, `rootfs.ext4`, …)
//! * raw block devices / partitions (`/dev/sdb5`, `/dev/disk4s1`,
//!   `\\.\PhysicalDrive1`, `\\.\E:`)
//!
//! [`open_path`] figures out which one a path is and returns a ready to use
//! [`BlockDevice`]. Raw devices usually need administrator/root rights.
//!
//! ## Locking
//!
//! An opened image is locked for as long as the device lives:
//!
//! * read-write → exclusive lock: nobody else can open it;
//! * read-only → shared lock: others may read it, but nobody can write it.
//!
//! The lock is `File::try_lock`/`try_lock_shared` (flock on Unix,
//! `LockFileEx` on Windows). On Unix such locks are advisory: they stop
//! cooperating programs (and a second copy of this app), not e.g. `dd`. On
//! Windows the file is additionally opened with a share mode that denies
//! other writers, which the OS enforces. On Linux, block devices opened
//! read-write use `O_EXCL`, so the kernel refuses if the device is mounted or
//! held exclusively by another program.

pub use super::disks::{
    drop_cache, list_devices, lock_for_writing, os_uses, raw_path, reread_partition_table, unmount, HostDevice,
    HostDeviceKind, WriteLock, CAN_UNMOUNT,
};
use super::joined::split_set;
use super::{AlignedDevice, BlockDevice, DeviceInfo, DeviceKind, JoinedDevice};
use crate::caps::AccessMode;
use crate::error::{Error, ErrorKind, Result};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Alignment used for raw devices (works for 512e and 4Kn disks).
pub const RAW_ALIGNMENT: u64 = 4096;

/// A host file (or device node) accessed with positioned I/O.
pub struct FileDevice {
    file: File,
    path: PathBuf,
    size: u64,
    read_only: bool,
    kind: DeviceKind,
}

impl FileDevice {
    /// Opens a regular image file.
    pub fn open(path: impl AsRef<Path>, mode: AccessMode) -> Result<Self> {
        let path = path.as_ref();
        let file = open_file(path, mode, false)?;
        let size = file.metadata()?.len();
        Ok(Self { file, path: path.to_owned(), size, read_only: !mode.is_writable(), kind: DeviceKind::ImageFile })
    }

    /// Creates (or truncates) an image file of `size` bytes.
    pub fn create(path: impl AsRef<Path>, size: u64) -> Result<Self> {
        let path = path.as_ref();
        let new = !path.exists();
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true);
        share_mode(&mut opts, AccessMode::ReadWrite);
        let file = opts.open(path).map_err(|e| map_open_error(e, path, AccessMode::ReadWrite))?;
        // Lock before truncating: never clobber an image someone has open.
        lock(&file, AccessMode::ReadWrite, path)?;
        file.set_len(0)?;
        file.set_len(size)?;
        if new {
            give_to_sudo_user(path);
        }
        Ok(Self { file, path: path.to_owned(), size, read_only: false, kind: DeviceKind::ImageFile })
    }

    /// Opens a raw disk / partition. Prefer [`open_path`], which also adds
    /// the alignment layer raw devices need.
    pub fn open_raw(path: impl AsRef<Path>, mode: AccessMode) -> Result<Self> {
        let path = path.as_ref();
        let mut file = open_file(path, mode, true)?;
        let size = sys::device_size(&mut file)?;
        Ok(Self { file, path: path.to_owned(), size, read_only: !mode.is_writable(), kind: DeviceKind::RawDisk })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn open_file(path: &Path, mode: AccessMode, raw: bool) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(mode.is_writable());
    share_mode(&mut opts, mode);
    if raw && mode.is_writable() {
        exclusive_device(&mut opts);
    }
    let file = opts.open(path).map_err(|e| map_open_error(e, path, mode))?;
    lock(&file, mode, path)?;
    Ok(file)
}

fn in_use(path: &Path) -> Error {
    Error::new(
        ErrorKind::InUse,
        format!("{}: the image is in use (open in another program, or already open)", path.display()),
    )
}

fn map_open_error(e: std::io::Error, path: &Path, mode: AccessMode) -> Error {
    // Windows sharing violation / Linux O_EXCL on a busy block device.
    let busy = e.raw_os_error().is_some_and(|c| (cfg!(windows) && c == 32) || (cfg!(target_os = "linux") && c == 16));
    if busy {
        in_use(path)
    } else if e.kind() == std::io::ErrorKind::PermissionDenied && mode.is_writable() {
        let meta = std::fs::metadata(path).ok();
        #[cfg(unix)]
        let owner = meta.as_ref().map(std::os::unix::fs::MetadataExt::uid);
        #[cfg(not(unix))]
        let owner: Option<u32> = None;
        let file = meta.as_ref().is_some_and(|m| m.is_file());
        Error::new(ErrorKind::PermissionDenied, cannot_write(path, file, owner))
    } else {
        Error::from(e).with_context(path.display())
    }
}

/// When the app runs with sudo, files and folders it creates would belong
/// to root, and the user could not change them afterwards without sudo.
/// This gives something just created (`path`) to the user who ran sudo
/// (SUDO_UID / SUDO_GID). Call it only for what the app itself created.
/// Without sudo, or elsewhere than Unix, it does nothing.
pub fn give_to_sudo_user(path: &Path) {
    #[cfg(unix)]
    {
        let id = |v: &str| std::env::var(v).ok().and_then(|s| s.parse::<u32>().ok());
        let (Some(uid), Some(gid)) = (id("SUDO_UID"), id("SUDO_GID")) else { return };
        give_to(path, uid, gid);
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Gives `path` to `uid`:`gid` if root owns it (and `uid` is not root).
#[cfg(unix)]
fn give_to(path: &Path, uid: u32, gid: u32) {
    use std::os::unix::fs::MetadataExt;
    if uid == 0 {
        return;
    }
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.uid() == 0) {
        let _ = std::os::unix::fs::lchown(path, Some(uid), Some(gid));
    }
}

/// Why a file or disk cannot be opened read-write, and what to do.
fn cannot_write(path: &Path, file: bool, owner: Option<u32>) -> String {
    let p = path.display();
    if !file {
        return format!(
            "{p}: writing to a disk needs administrator rights (run the app with sudo, or as administrator), \
             or open it read-only"
        );
    }
    if owner == Some(0) {
        return format!(
            "{p} belongs to root (it was probably made while the app ran with sudo), so it can only be read: \
             open it read-only, or make it yours with: sudo chown \"$(whoami)\" \"{p}\""
        );
    }
    format!("{p}: you may not write to this file (its owner or permissions): open it read-only, or change them")
}

/// Exclusive lock for read-write, shared for read-only. File systems that do
/// not support locking (some network shares) are accepted unlocked.
fn lock(file: &File, mode: AccessMode, path: &Path) -> Result<()> {
    let r = if mode.is_writable() { file.try_lock() } else { file.try_lock_shared() };
    match r {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(in_use(path)),
        Err(std::fs::TryLockError::Error(e)) if e.kind() == std::io::ErrorKind::Unsupported => Ok(()),
        Err(std::fs::TryLockError::Error(e)) => Err(Error::from(e).with_context(path.display())),
    }
}

/// Windows: let others read but never write while we have the image open.
fn share_mode(opts: &mut OpenOptions, _mode: AccessMode) {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        opts.share_mode(FILE_SHARE_READ);
    }
    #[cfg(not(windows))]
    let _ = opts;
}

/// Linux: O_EXCL on a block device = exclusive open; fails with EBUSY if the
/// device is mounted or opened exclusively elsewhere.
fn exclusive_device(opts: &mut OpenOptions) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(any(target_arch = "mips", target_arch = "mips64"))]
        const O_EXCL: i32 = 0x400;
        #[cfg(any(target_arch = "sparc", target_arch = "sparc64"))]
        const O_EXCL: i32 = 0x800;
        #[cfg(not(any(target_arch = "mips", target_arch = "mips64", target_arch = "sparc", target_arch = "sparc64")))]
        const O_EXCL: i32 = 0o200;
        opts.custom_flags(O_EXCL);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = opts;
}

impl BlockDevice for FileDevice {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            name: self.path.display().to_string(),
            kind: self.kind,
            size: self.size,
            sector_size: if self.kind == DeviceKind::RawDisk { RAW_ALIGNMENT as u32 } else { 512 },
            read_only: self.read_only,
            resizable: self.kind == DeviceKind::ImageFile && !self.read_only,
        }
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        super::check_range(offset, buf.len(), self.size)?;
        sys::read_exact_at(&mut self.file, buf, offset).map_err(Error::from)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if self.read_only {
            return Err(Error::read_only());
        }
        super::check_range(offset, data.len(), self.size)?;
        sys::write_all_at(&mut self.file, data, offset).map_err(Error::from)
    }

    fn resize(&mut self, new_size: u64) -> Result<()> {
        if self.read_only {
            return Err(Error::read_only());
        }
        if self.kind != DeviceKind::ImageFile {
            return Err(Error::unsupported("resizing a raw device"));
        }
        self.file.set_len(new_size)?;
        self.size = new_size;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        if self.kind == DeviceKind::RawDisk {
            sys::sync_device(&mut self.file)?;
        } else {
            self.file.sync_data()?;
        }
        Ok(())
    }
}

/// Is `path` a raw device rather than a regular file?
pub fn is_raw_device_path(path: &Path) -> bool {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        s.starts_with(r"\\.\") || s.starts_with(r"\\?\GLOBALROOT")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path)
            .map(|m| m.file_type().is_block_device() || m.file_type().is_char_device())
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        false
    }
}

/// Opens an image file or raw device at `path`.
pub fn open_path(path: impl AsRef<Path>, mode: AccessMode) -> Result<Box<dyn BlockDevice>> {
    let path = path.as_ref();
    if is_raw_device_path(path) {
        let dev = FileDevice::open_raw(path, mode).map_err(|e| {
            if e.kind() == ErrorKind::PermissionDenied {
                Error::new(
                    ErrorKind::PermissionDenied,
                    format!("{e} (raw devices usually require administrator/root rights)"),
                )
            } else {
                e
            }
        })?;
        Ok(Box::new(AlignedDevice::new(dev, RAW_ALIGNMENT)))
    } else {
        let meta = std::fs::metadata(path).map_err(|e| Error::from(e).with_context(path.display()))?;
        if meta.is_dir() {
            return Err(Error::is_a_directory(path.display()));
        }
        if let Some(parts) = split_parts(path) {
            // One part of a split archive or image: all of them, read-only.
            let devs = parts
                .iter()
                .map(|p| FileDevice::open(p, AccessMode::ReadOnly).map(|d| Box::new(d) as Box<dyn BlockDevice>))
                .collect::<Result<Vec<_>>>()?;
            return Ok(Box::new(JoinedDevice::new(path.display().to_string(), devs)));
        }
        Ok(Box::new(FileDevice::open(path, mode)?))
    }
}

/// Opens an image like [`open_path`] and, when it is a virtual machine disk
/// (qcow2, VHD, VMDK, … — see [`crate::container`]), the disk inside it.
/// Files the disk refers to (VMDK extents, the parent of a snapshot) are
/// looked for relative to its folder.
pub fn open_disk(
    registry: &crate::DriverRegistry,
    path: impl AsRef<Path>,
    mode: AccessMode,
) -> Result<Box<dyn BlockDevice>> {
    let path = path.as_ref();
    let dev = open_path(path, mode)?;
    let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
    let opener = move |name: &str, writable: bool| -> Result<Box<dyn BlockDevice>> {
        let mode = if writable { mode } else { AccessMode::ReadOnly };
        Ok(Box::new(FileDevice::open(dir.join(name), mode)?) as Box<dyn BlockDevice>)
    };
    registry.open_disk(dev, Some(&opener))
}

/// The files of the split set (a multi-part archive or a split image) that
/// the file at `path` belongs to, in order (see [`split_set`]).
pub fn split_parts(path: &Path) -> Option<Vec<PathBuf>> {
    let name = path.file_name()?.to_str()?;
    let dir = path.parent().unwrap_or(Path::new(""));
    let names = split_set(name, |n| dir.join(n).is_file())?;
    Some(names.into_iter().map(|n| dir.join(n)).collect())
}

#[cfg(any(unix, not(windows)))]
fn seek_size(file: &mut File) -> Result<u64> {
    Ok(file.seek(SeekFrom::End(0))?)
}

// ---------------------------------------------------------------------------
// Platform specific bits. No external crates: the few OS calls we need are
// declared by hand.
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod sys {
    use super::*;
    use std::os::unix::fs::FileExt;

    /// Writes a disk's buffered data out. Not `sync_data`: on macOS that is
    /// `F_FULLFSYNC`, which only files support (a disk answers "inappropriate
    /// ioctl for device"); `fsync` works on disks everywhere. macOS is then
    /// asked to flush the drive's own cache too, if it can.
    pub fn sync_device(f: &mut File) -> Result<()> {
        use std::os::unix::io::AsRawFd;
        unsafe extern "C" {
            fn fsync(fd: i32) -> i32;
        }
        // SAFETY: a valid descriptor.
        if unsafe { fsync(f.as_raw_fd()) } != 0 {
            let e = std::io::Error::last_os_error();
            // Devices that cannot be synced (ENOTTY, EINVAL, ENOTSUP).
            if !matches!(e.raw_os_error(), Some(25 | 22 | 45 | 95)) {
                return Err(e.into());
            }
        }
        #[cfg(target_os = "macos")]
        {
            unsafe extern "C" {
                fn ioctl(fd: i32, request: u64, ...) -> i32;
            }
            const DKIOCSYNCHRONIZECACHE: u64 = 0x2000_6416; // _IO('d', 22)
                                                            // SAFETY: a valid descriptor; the request takes no argument.
            let _ = unsafe { ioctl(f.as_raw_fd(), DKIOCSYNCHRONIZECACHE) };
        }
        Ok(())
    }

    pub fn read_exact_at(f: &mut File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
        f.read_exact_at(buf, off)
    }

    pub fn write_all_at(f: &mut File, buf: &[u8], off: u64) -> std::io::Result<()> {
        f.write_all_at(buf, off)
    }

    pub fn device_size(file: &mut File) -> Result<u64> {
        let size = seek_size(file)?;
        if size > 0 {
            return Ok(size);
        }
        #[cfg(target_os = "macos")]
        {
            if let Some(s) = macos_ioctl_size(file) {
                return Ok(s);
            }
        }
        Err(Error::new(ErrorKind::Io, "could not determine the size of the device"))
    }

    #[cfg(target_os = "macos")]
    fn macos_ioctl_size(file: &File) -> Option<u64> {
        use std::os::fd::AsRawFd;
        extern "C" {
            fn ioctl(fd: i32, request: u64, ...) -> i32;
        }
        const DKIOCGETBLOCKSIZE: u64 = 0x4004_6418;
        const DKIOCGETBLOCKCOUNT: u64 = 0x4008_6419;
        let mut block_size: u32 = 0;
        let mut block_count: u64 = 0;
        // SAFETY: valid fd, out-pointers to properly sized integers.
        unsafe {
            if ioctl(file.as_raw_fd(), DKIOCGETBLOCKSIZE, &mut block_size as *mut u32) != 0 {
                return None;
            }
            if ioctl(file.as_raw_fd(), DKIOCGETBLOCKCOUNT, &mut block_count as *mut u64) != 0 {
                return None;
            }
        }
        Some(block_size as u64 * block_count)
    }
}

#[cfg(windows)]
mod sys {
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::fs::FileExt;
    use std::os::windows::io::AsRawHandle;

    #[link(name = "kernel32")]
    extern "system" {
        fn DeviceIoControl(
            device: *mut c_void,
            code: u32,
            in_buf: *const c_void,
            in_size: u32,
            out_buf: *mut c_void,
            out_size: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }

    const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405C;

    /// FlushFileBuffers works on disks too.
    pub fn sync_device(f: &mut File) -> Result<()> {
        f.sync_data()?;
        Ok(())
    }

    pub fn read_exact_at(f: &mut File, mut buf: &mut [u8], mut off: u64) -> std::io::Result<()> {
        while !buf.is_empty() {
            match f.seek_read(buf, off) {
                Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    buf = &mut buf[n..];
                    off += n as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn write_all_at(f: &mut File, mut buf: &[u8], mut off: u64) -> std::io::Result<()> {
        while !buf.is_empty() {
            match f.seek_write(buf, off) {
                Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    buf = &buf[n..];
                    off += n as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn ioctl_length(file: &File) -> Option<u64> {
        let mut len: i64 = 0;
        let mut returned = 0u32;
        // SAFETY: valid handle; output buffer is a properly sized i64
        // (GET_LENGTH_INFORMATION is a single LARGE_INTEGER).
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                IOCTL_DISK_GET_LENGTH_INFO,
                std::ptr::null(),
                0,
                &mut len as *mut i64 as *mut c_void,
                std::mem::size_of::<i64>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        (ok != 0 && len > 0).then_some(len as u64)
    }

    pub fn device_size(file: &mut File) -> Result<u64> {
        if let Some(len) = ioctl_length(file) {
            return Ok(len);
        }
        let size = file.seek(SeekFrom::End(0))?;
        if size > 0 {
            Ok(size)
        } else {
            Err(Error::new(ErrorKind::Io, "could not determine the size of the device"))
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    use super::*;
    use std::io::{Read, Write};

    pub fn read_exact_at(f: &mut File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
        f.seek(SeekFrom::Start(off))?;
        f.read_exact(buf)
    }

    pub fn write_all_at(f: &mut File, buf: &[u8], off: u64) -> std::io::Result<()> {
        f.seek(SeekFrom::Start(off))?;
        f.write_all(buf)
    }

    pub fn device_size(file: &mut File) -> Result<u64> {
        seek_size(file)
    }

    pub fn sync_device(f: &mut File) -> Result<()> {
        f.sync_data()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_locked_while_open() {
        let dir = std::env::temp_dir().join(format!("dix-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("locked.img");
        drop(FileDevice::create(&p, 4096).unwrap());
        let kind = |r: Result<Box<dyn BlockDevice>>| r.err().map(|e| e.kind());

        // Read-write is exclusive.
        let rw = open_path(&p, AccessMode::ReadWrite).unwrap();
        assert_eq!(kind(open_path(&p, AccessMode::ReadWrite)), Some(ErrorKind::InUse));
        assert_eq!(kind(open_path(&p, AccessMode::ReadOnly)), Some(ErrorKind::InUse));
        assert_eq!(FileDevice::create(&p, 1).err().map(|e| e.kind()), Some(ErrorKind::InUse));
        drop(rw);

        // Read-only is shared between readers, but blocks writers.
        let ro1 = open_path(&p, AccessMode::ReadOnly).unwrap();
        let ro2 = open_path(&p, AccessMode::ReadOnly).unwrap();
        assert_eq!(kind(open_path(&p, AccessMode::ReadWrite)), Some(ErrorKind::InUse));
        drop((ro1, ro2));

        // The lock goes away with the device; the refused create did not truncate.
        let rw = open_path(&p, AccessMode::ReadWrite).unwrap();
        assert_eq!(rw.size(), 4096);
        drop(rw);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_device_round_trip() {
        // A folder of its own: other tests remove theirs while this runs.
        let dir = std::env::temp_dir().join(format!("dix-test-round-trip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("dev.img");
        {
            let mut d = FileDevice::create(&p, 1024).unwrap();
            d.write_at(100, b"xyz").unwrap();
            d.resize(2048).unwrap();
            d.flush().unwrap();
        }
        // A process another test starts at that moment holds a copy of the
        // writer's descriptor (and so its lock) until it runs its program:
        // try again for a moment.
        let mut d = (0..50)
            .find_map(|_| match open_path(&p, AccessMode::ReadOnly) {
                Err(e) if e.kind() == ErrorKind::InUse => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    None
                }
                r => Some(r),
            })
            .unwrap()
            .unwrap();
        assert_eq!(d.size(), 2048);
        let mut b = [0u8; 3];
        d.read_at(100, &mut b).unwrap();
        assert_eq!(&b, b"xyz");
        assert_eq!(d.write_at(0, b"a").unwrap_err().kind(), ErrorKind::ReadOnly);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn says_why_a_file_cannot_be_written() {
        let p = Path::new("/Users/me/disk.qcow2");
        let root = cannot_write(p, true, Some(0));
        assert!(root.contains("belongs to root") && root.contains("sudo chown"), "{root}");
        let other = cannot_write(p, true, Some(501));
        assert!(other.contains("open it read-only") && !other.contains("root"), "{other}");
        assert!(cannot_write(Path::new("/dev/disk4"), false, Some(0)).contains("administrator"));
    }

    /// Run as root (as with sudo): what was created goes to the user.
    #[cfg(unix)]
    #[test]
    fn created_files_go_to_the_sudo_user() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("dix-sudo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("made.img");
        std::fs::write(&f, b"x").unwrap();
        if std::fs::metadata(&f).unwrap().uid() != 0 {
            eprintln!("not running as root: skipped");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        give_to(&f, 4321, 4322);
        let m = std::fs::metadata(&f).unwrap();
        assert_eq!((m.uid(), m.gid()), (4321, 4322));
        // Not root's: left alone.
        give_to(&f, 999, 999);
        assert_eq!(std::fs::metadata(&f).unwrap().uid(), 4321);
        // Root itself as the user: nothing to do.
        let g = dir.join("other");
        std::fs::write(&g, b"y").unwrap();
        give_to(&g, 0, 0);
        assert_eq!(std::fs::metadata(&g).unwrap().uid(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
