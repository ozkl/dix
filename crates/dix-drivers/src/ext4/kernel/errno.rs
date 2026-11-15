//! The kernel's error numbers, and their translation to [`dix_core::Error`].

use dix_core::{Error, ErrorKind};
use std::cell::RefCell;

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Errno {
    EPERM,
    ENOENT,
    EIO,
    ENXIO,
    ENOMEM,
    EFAULT,
    EBUSY,
    EEXIST,
    EXDEV,
    ENOTDIR,
    EISDIR,
    EINVAL,
    EFBIG,
    ENOSPC,
    EROFS,
    EMLINK,
    ENAMETOOLONG,
    ENOTEMPTY,
    EBADMSG,
    EUCLEAN,
    ENODEV,
    EOPNOTSUPP,
}

pub type KResult<T> = core::result::Result<T, Errno>;

thread_local! {
    /// The device error behind the last `EIO` (its message is worth keeping).
    static LAST_IO: RefCell<Option<Error>> = const { RefCell::new(None) };
}

pub(crate) fn io_error(e: Error) -> Errno {
    let errno = match e.kind() {
        ErrorKind::NoSpace | ErrorKind::OutOfBounds => Errno::ENOSPC,
        ErrorKind::ReadOnly => Errno::EROFS,
        _ => Errno::EIO,
    };
    LAST_IO.with(|l| *l.borrow_mut() = Some(e));
    errno
}

impl Errno {
    pub fn describe(self) -> &'static str {
        match self {
            Errno::EPERM => "operation not permitted",
            Errno::ENOENT => "no such file or directory",
            Errno::EIO => "I/O error",
            Errno::ENXIO => "no such device or address",
            Errno::ENOMEM => "out of memory",
            Errno::EFAULT => "bad address",
            Errno::EBUSY => "busy",
            Errno::EEXIST => "file exists",
            Errno::EXDEV => "cross-device link",
            Errno::ENOTDIR => "not a directory",
            Errno::EISDIR => "is a directory",
            Errno::EINVAL => "invalid argument",
            Errno::EFBIG => "file too large",
            Errno::ENOSPC => "no space left on the file system",
            Errno::EROFS => "read-only file system",
            Errno::EMLINK => "too many links",
            Errno::ENAMETOOLONG => "name too long",
            Errno::ENOTEMPTY => "directory not empty",
            Errno::EBADMSG => "bad checksum",
            Errno::EUCLEAN => "file system structure needs cleaning (corrupted)",
            Errno::ENODEV => "no such device",
            Errno::EOPNOTSUPP => "operation not supported",
        }
    }
}

impl From<Errno> for Error {
    fn from(e: Errno) -> Error {
        if e == Errno::EIO
            && let Some(inner) = LAST_IO.with(|l| l.borrow_mut().take())
        {
            return inner;
        }
        let kind = match e {
            Errno::ENOENT | Errno::ENXIO => ErrorKind::NotFound,
            Errno::EEXIST => ErrorKind::AlreadyExists,
            Errno::ENOTDIR => ErrorKind::NotADirectory,
            Errno::EISDIR => ErrorKind::IsADirectory,
            Errno::ENOTEMPTY => ErrorKind::DirectoryNotEmpty,
            Errno::EROFS => ErrorKind::ReadOnly,
            Errno::ENOSPC => ErrorKind::NoSpace,
            Errno::EPERM => ErrorKind::PermissionDenied,
            Errno::EBUSY => ErrorKind::InUse,
            Errno::EBADMSG | Errno::EUCLEAN => ErrorKind::Corrupted,
            Errno::ENAMETOOLONG => ErrorKind::InvalidPath,
            Errno::EINVAL | Errno::EFBIG | Errno::EMLINK | Errno::EXDEV | Errno::EFAULT => ErrorKind::InvalidInput,
            Errno::EIO | Errno::ENOMEM => ErrorKind::Io,
            Errno::ENODEV | Errno::EOPNOTSUPP => ErrorKind::Unsupported,
        };
        Error::new(kind, format!("ext4: {}", e.describe()))
    }
}
