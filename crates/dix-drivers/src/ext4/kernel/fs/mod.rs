//! The VFS types the ext4 driver uses (copied from ruxis's `fs`).

pub mod vfs;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFIFO: u32 = 0o010000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFSOCK: u32 = 0o140000;

/// `struct stat`, as the driver fills it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub _pad0: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_atime_nsec: i64,
    pub st_mtime: i64,
    pub st_mtime_nsec: i64,
    pub st_ctime: i64,
    pub st_ctime_nsec: i64,
    pub _unused: [i64; 3],
}

pub struct DirEntry {
    pub name: String,
    pub ino: u64,
    /// `d_type`.
    pub kind: u8,
    pub pos: Option<u64>,
}

/// Device number as `stat` reports it.
pub const fn makedev(major: u64, minor: u64) -> u64 {
    (major & 0xfff) << 8 | (minor & 0xff) | (minor & !0xff) << 12
}

/// Open-file operations (the explorer never opens files this way).
pub trait FileOps: Send + Sync {}
