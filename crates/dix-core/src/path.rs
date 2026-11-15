//! Paths *inside* a volume.
//!
//! [`VfsPath`] is always absolute, uses `/` as the separator, contains no `.`
//! or `..` components and no empty components. Drivers can therefore rely on
//! receiving clean paths no matter what the user typed into the address bar.

use crate::error::{Error, ErrorKind, Result};
use std::fmt;

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VfsPath(String);

impl Default for VfsPath {
    fn default() -> Self {
        Self::root()
    }
}

impl VfsPath {
    pub fn root() -> Self {
        VfsPath("/".to_owned())
    }

    /// Parses user input. Accepts `/` and `\` separators, resolves `.` and
    /// `..` (never escaping the root) and ignores repeated separators.
    pub fn parse(input: &str) -> Result<Self> {
        let mut parts: Vec<&str> = Vec::new();
        for comp in input.split(['/', '\\']) {
            match comp {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                name => {
                    Self::validate_name(name)?;
                    parts.push(name);
                }
            }
        }
        Ok(Self::from_parts(&parts))
    }

    fn from_parts(parts: &[&str]) -> Self {
        if parts.is_empty() {
            return Self::root();
        }
        let mut s = String::with_capacity(parts.iter().map(|p| p.len() + 1).sum());
        for p in parts {
            s.push('/');
            s.push_str(p);
        }
        VfsPath(s)
    }

    /// Checks that `name` is usable as a single path component.
    pub fn validate_name(name: &str) -> Result<()> {
        if name.is_empty() || name == "." || name == ".." {
            return Err(Error::new(ErrorKind::InvalidPath, format!("invalid name '{name}'")));
        }
        if name.contains(['/', '\\', '\0']) {
            return Err(Error::new(ErrorKind::InvalidPath, format!("name '{name}' contains a forbidden character")));
        }
        Ok(())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    /// Path components, root excluded.
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &str> + '_ {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    pub fn depth(&self) -> usize {
        self.components().count()
    }

    pub fn file_name(&self) -> Option<&str> {
        if self.is_root() {
            None
        } else {
            self.0.rsplit('/').next()
        }
    }

    pub fn parent(&self) -> Option<VfsPath> {
        if self.is_root() {
            return None;
        }
        let idx = self.0.rfind('/').unwrap_or(0);
        if idx == 0 {
            Some(Self::root())
        } else {
            Some(VfsPath(self.0[..idx].to_owned()))
        }
    }

    /// Appends one component.
    pub fn join(&self, name: &str) -> Result<VfsPath> {
        Self::validate_name(name)?;
        if self.is_root() {
            Ok(VfsPath(format!("/{name}")))
        } else {
            Ok(VfsPath(format!("{}/{name}", self.0)))
        }
    }

    /// Component-wise prefix test (`/a` is an ancestor of `/a/b`, not of `/ab`).
    pub fn starts_with(&self, base: &VfsPath) -> bool {
        base.is_root()
            || self.0 == base.0
            || (self.0.starts_with(&base.0) && self.0.as_bytes().get(base.0.len()) == Some(&b'/'))
    }

    /// All ancestors from the root down to (and including) `self`.
    pub fn ancestors_inclusive(&self) -> Vec<VfsPath> {
        let parts: Vec<&str> = self.components().collect();
        (0..=parts.len()).map(|n| Self::from_parts(&parts[..n])).collect()
    }

    /// Re-roots `self` from `from` onto `to` (`/a/b/c`, `/a` → `/x` = `/x/b/c`).
    pub fn rebase(&self, from: &VfsPath, to: &VfsPath) -> Option<VfsPath> {
        if !self.starts_with(from) {
            return None;
        }
        let rest = if from.is_root() { &self.0[..] } else { &self.0[from.0.len()..] };
        let mut s = if to.is_root() { String::new() } else { to.0.clone() };
        s.push_str(rest);
        if s.is_empty() {
            s.push('/');
        }
        Some(VfsPath(s))
    }
}

impl fmt::Display for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VfsPath({:?})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_normalise() {
        assert_eq!(VfsPath::parse("").unwrap().as_str(), "/");
        assert_eq!(VfsPath::parse("a//b/./c/..").unwrap().as_str(), "/a/b");
        assert_eq!(VfsPath::parse("\\x\\y").unwrap().as_str(), "/x/y");
        assert_eq!(VfsPath::parse("/../..").unwrap().as_str(), "/");
    }

    #[test]
    fn relations() {
        let p = VfsPath::parse("/a/b/c").unwrap();
        assert_eq!(p.file_name(), Some("c"));
        assert_eq!(p.parent().unwrap().as_str(), "/a/b");
        assert_eq!(VfsPath::parse("/a").unwrap().parent().unwrap(), VfsPath::root());
        assert!(p.starts_with(&VfsPath::parse("/a").unwrap()));
        assert!(!VfsPath::parse("/ab").unwrap().starts_with(&VfsPath::parse("/a").unwrap()));
        assert_eq!(p.ancestors_inclusive().len(), 4);
        let r = p.rebase(&VfsPath::parse("/a").unwrap(), &VfsPath::parse("/x").unwrap()).unwrap();
        assert_eq!(r.as_str(), "/x/b/c");
        let r = p.rebase(&VfsPath::root(), &VfsPath::parse("/x").unwrap()).unwrap();
        assert_eq!(r.as_str(), "/x/a/b/c");
        assert!(VfsPath::root().join("a/b").is_err());
    }
}
