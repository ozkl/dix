//! Capabilities and access modes.
//!
//! A [`Driver`](crate::Driver) advertises the *maximum* set of capabilities it
//! implements. A mounted [`FileSystem`](crate::FileSystem) reports the
//! *effective* set, which is reduced when the user opens the image read-only
//! (or the medium itself is read-only). The UI enables/disables actions purely
//! from the effective set.

use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign, Not};

/// How the user asked to open an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AccessMode {
    #[default]
    ReadOnly,
    ReadWrite,
}

impl AccessMode {
    pub fn is_writable(self) -> bool {
        matches!(self, AccessMode::ReadWrite)
    }

    pub fn label(self) -> &'static str {
        match self {
            AccessMode::ReadOnly => "Read-only",
            AccessMode::ReadWrite => "Read-write",
        }
    }
}

/// A set of capability flags (a tiny hand-written `bitflags`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Capabilities(u32);

macro_rules! capability_flags {
    ($($(#[$doc:meta])* $name:ident = $bit:expr, $label:expr;)*) => {
        impl Capabilities {
            $($(#[$doc])* pub const $name: Self = Self(1 << $bit);)*

            const ALL_FLAGS: &'static [(Self, &'static str)] = &[$((Self::$name, $label)),*];
        }
    };
}

capability_flags! {
    /// List directories, stat and read files.
    READ = 0, "read";
    /// Create files and overwrite their content.
    WRITE = 1, "write";
    /// Create directories.
    CREATE_DIR = 2, "create directories";
    /// Delete files and (empty) directories.
    REMOVE = 3, "delete";
    /// Rename / move entries inside the volume.
    RENAME = 4, "rename";
    /// The file system has symbolic links (reported in metadata).
    SYMLINKS = 5, "symlinks";
    /// The file system stores unix permissions / owners.
    PERMISSIONS = 6, "permissions";
    /// The file system stores modification times.
    TIMESTAMPS = 7, "timestamps";
    /// The driver can create a fresh, empty file system on a device.
    FORMAT = 8, "format";
    /// The driver can check a file system for consistency (like `fsck -n`).
    CHECK = 9, "check";
}

impl Capabilities {
    /// Every capability that modifies the medium.
    pub const MUTATING: Self =
        Self(Self::WRITE.0 | Self::CREATE_DIR.0 | Self::REMOVE.0 | Self::RENAME.0 | Self::FORMAT.0);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Can anything at all be modified?
    pub fn is_writable(self) -> bool {
        self.intersects(Self::MUTATING)
    }

    /// The capabilities left when the volume is used with `mode`.
    pub fn for_mode(self, mode: AccessMode) -> Self {
        match mode {
            AccessMode::ReadOnly => self.difference(Self::MUTATING),
            AccessMode::ReadWrite => self,
        }
    }

    /// Human readable names of the flags that are set.
    pub fn names(self) -> impl Iterator<Item = &'static str> {
        Self::ALL_FLAGS.iter().filter(move |(f, _)| self.contains(*f)).map(|(_, n)| *n)
    }
}

impl BitOr for Capabilities {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl BitOrAssign for Capabilities {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for Capabilities {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl Not for Capabilities {
    type Output = Self;
    fn not(self) -> Self {
        Self(!self.0)
    }
}

impl fmt::Debug for Capabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.names()).finish()
    }
}

impl fmt::Display for Capabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<_> = self.names().collect();
        if names.is_empty() {
            f.write_str("none")
        } else {
            f.write_str(&names.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_strips_mutating_flags() {
        let c = Capabilities::READ | Capabilities::WRITE | Capabilities::TIMESTAMPS;
        let ro = c.for_mode(AccessMode::ReadOnly);
        assert!(ro.contains(Capabilities::READ | Capabilities::TIMESTAMPS));
        assert!(!ro.is_writable());
        assert!(c.for_mode(AccessMode::ReadWrite).is_writable());
    }
}
