//! A single error type shared by devices, drivers and the UI.

use std::fmt;

/// Broad category of an [`Error`]. The UI uses it to pick messages/icons,
/// drivers use it to signal well-known conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    NotFound,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    DirectoryNotEmpty,
    /// The volume or device is opened read-only (or the medium is read-only).
    ReadOnly,
    /// The driver/device does not implement the requested operation.
    Unsupported,
    NoSpace,
    InvalidPath,
    InvalidInput,
    /// On-disk structures are invalid / corrupted.
    Corrupted,
    /// No driver recognised the image.
    UnknownFormat,
    PermissionDenied,
    /// The image or device is in use (locked by another program or already open).
    InUse,
    /// Access outside the bounds of a device.
    OutOfBounds,
    /// The data is encrypted, and no password given so far opens it (see
    /// [`FileSystem::add_password`](crate::FileSystem::add_password)).
    PasswordRequired,
    Io,
    Other,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::NotFound => "not found",
            ErrorKind::AlreadyExists => "already exists",
            ErrorKind::NotADirectory => "not a directory",
            ErrorKind::IsADirectory => "is a directory",
            ErrorKind::DirectoryNotEmpty => "directory not empty",
            ErrorKind::ReadOnly => "read-only",
            ErrorKind::Unsupported => "unsupported operation",
            ErrorKind::NoSpace => "no space left",
            ErrorKind::InvalidPath => "invalid path",
            ErrorKind::InvalidInput => "invalid input",
            ErrorKind::Corrupted => "corrupted data",
            ErrorKind::UnknownFormat => "unknown format",
            ErrorKind::PermissionDenied => "permission denied",
            ErrorKind::InUse => "in use",
            ErrorKind::OutOfBounds => "out of bounds",
            ErrorKind::PasswordRequired => "password required",
            ErrorKind::Io => "I/O error",
            ErrorKind::Other => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn not_found(what: impl fmt::Display) -> Self {
        Self::new(ErrorKind::NotFound, format!("{what}: not found"))
    }

    pub fn already_exists(what: impl fmt::Display) -> Self {
        Self::new(ErrorKind::AlreadyExists, format!("{what}: already exists"))
    }

    pub fn not_a_directory(what: impl fmt::Display) -> Self {
        Self::new(ErrorKind::NotADirectory, format!("{what}: not a directory"))
    }

    pub fn is_a_directory(what: impl fmt::Display) -> Self {
        Self::new(ErrorKind::IsADirectory, format!("{what}: is a directory"))
    }

    pub fn read_only() -> Self {
        Self::new(ErrorKind::ReadOnly, "the volume is opened read-only")
    }

    pub fn unsupported(what: impl fmt::Display) -> Self {
        Self::new(ErrorKind::Unsupported, format!("{what} is not supported"))
    }

    pub fn corrupted(what: impl fmt::Display) -> Self {
        Self::new(ErrorKind::Corrupted, what.to_string())
    }

    pub fn invalid_input(what: impl fmt::Display) -> Self {
        Self::new(ErrorKind::InvalidInput, what.to_string())
    }

    pub fn with_context(mut self, ctx: impl fmt::Display) -> Self {
        self.message = format!("{ctx}: {}", self.message);
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            f.write_str(self.kind.as_str())
        } else {
            f.write_str(&self.message)
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        let kind = match e.kind() {
            K::NotFound => ErrorKind::NotFound,
            K::PermissionDenied => ErrorKind::PermissionDenied,
            K::AlreadyExists => ErrorKind::AlreadyExists,
            K::UnexpectedEof => ErrorKind::OutOfBounds,
            K::InvalidInput => ErrorKind::InvalidInput,
            K::Unsupported => ErrorKind::Unsupported,
            K::ReadOnlyFilesystem => ErrorKind::ReadOnly,
            K::StorageFull => ErrorKind::NoSpace,
            _ => ErrorKind::Io,
        };
        Error::new(kind, e.to_string())
    }
}
