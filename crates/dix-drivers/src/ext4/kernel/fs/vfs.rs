//! The `Inode` trait the ext4 driver implements (ruxis's VFS, trimmed to
//! what the driver uses).

use super::{DirEntry, FileOps, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG, Stat};
use crate::ext4::kernel::errno::{Errno, KResult};
use std::any::Any;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Regular,
    Directory,
    Symlink,
    CharDevice,
    BlockDevice,
    Fifo,
    Socket,
}

impl FileType {
    pub fn from_mode(mode: u32) -> Self {
        match mode & S_IFMT {
            S_IFDIR => Self::Directory,
            S_IFLNK => Self::Symlink,
            S_IFCHR => Self::CharDevice,
            super::S_IFBLK => Self::BlockDevice,
            S_IFIFO => Self::Fifo,
            super::S_IFSOCK => Self::Socket,
            _ => Self::Regular,
        }
    }

    pub fn mode(self) -> u32 {
        match self {
            Self::Regular => S_IFREG,
            Self::Directory => S_IFDIR,
            Self::Symlink => S_IFLNK,
            Self::CharDevice => S_IFCHR,
            Self::BlockDevice => super::S_IFBLK,
            Self::Fifo => S_IFIFO,
            Self::Socket => super::S_IFSOCK,
        }
    }
}

/// `struct statfs`.
#[derive(Clone, Copy, Debug, Default)]
pub struct StatFs {
    pub f_type: i64,
    pub f_bsize: i64,
    pub f_blocks: u64,
    pub f_bfree: u64,
    pub f_bavail: u64,
    pub f_files: u64,
    pub f_ffree: u64,
    pub f_fsid: [i32; 2],
    pub f_namelen: i64,
    pub f_frsize: i64,
    pub f_flags: i64,
    pub f_spare: [i64; 4],
}

pub trait Inode: Send + Sync {
    fn stat(&self) -> Stat;

    fn as_any(&self) -> &dyn Any;

    fn file_type(&self) -> FileType {
        FileType::from_mode(self.stat().st_mode)
    }

    fn statfs(&self) -> StatFs {
        StatFs::default()
    }

    fn lookup(&self, _name: &str) -> KResult<Arc<dyn Inode>> {
        Err(Errno::ENOTDIR)
    }

    fn readdir(&self) -> KResult<Vec<DirEntry>> {
        Err(Errno::ENOTDIR)
    }

    fn create(&self, _name: &str, _kind: FileType, _mode: u32) -> KResult<Arc<dyn Inode>> {
        Err(Errno::EROFS)
    }

    fn symlink(&self, _name: &str, _target: &str) -> KResult<()> {
        Err(Errno::EROFS)
    }

    fn link(&self, _name: &str, _inode: &Arc<dyn Inode>) -> KResult<()> {
        Err(Errno::EROFS)
    }

    fn unlink(&self, _name: &str, _dir: bool) -> KResult<()> {
        Err(Errno::EROFS)
    }

    fn rename(&self, _old: &str, _new_dir: &Arc<dyn Inode>, _new: &str) -> KResult<()> {
        Err(Errno::EROFS)
    }

    fn readlink(&self) -> KResult<String> {
        Err(Errno::EINVAL)
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> KResult<usize> {
        Err(Errno::EINVAL)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> KResult<usize> {
        Err(Errno::EINVAL)
    }

    fn truncate(&self, _len: u64) -> KResult<()> {
        Err(Errno::EINVAL)
    }

    fn chmod(&self, _mode: u32) -> KResult<()> {
        Err(Errno::EPERM)
    }

    fn chown(&self, _uid: Option<u32>, _gid: Option<u32>) -> KResult<()> {
        Err(Errno::EPERM)
    }

    fn set_times(&self, _atime: Option<i64>, _mtime: Option<i64>) -> KResult<()> {
        Ok(())
    }

    fn get_page(&self, _index: u64, _shared_write: bool) -> KResult<crate::ext4::kernel::mm::PhysAddr> {
        Err(Errno::ENODEV)
    }

    fn xattrs(&self) -> KResult<Vec<(String, Vec<u8>)>> {
        Err(Errno::EOPNOTSUPP)
    }

    fn set_xattrs(&self, _all: Vec<(String, Vec<u8>)>) -> KResult<()> {
        Err(Errno::EOPNOTSUPP)
    }

    fn device(&self) -> u64 {
        0
    }

    fn can_mmap(&self) -> bool {
        false
    }

    fn sync(&self) -> KResult<()> {
        Ok(())
    }

    fn sync_fs(&self) -> KResult<()> {
        Ok(())
    }

    fn drop_caches(&self) -> KResult<()> {
        Ok(())
    }

    fn shutdown(&self) -> KResult<()> {
        Ok(())
    }

    fn unmount(&self) -> KResult<()> {
        Ok(())
    }

    fn open(&self, _flags: u32) -> KResult<Option<Arc<dyn FileOps>>> {
        Ok(None)
    }
}
