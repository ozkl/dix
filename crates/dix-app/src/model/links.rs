//! Where a symbolic link leads, within its volume.

use dix_core::{EntryKind, FileSystem, Metadata, VfsPath};
use std::collections::VecDeque;

/// The end of a chain of links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEnd {
    /// It leads here (not a link).
    Found(VfsPath, Metadata),
    /// Nothing is there: the link is broken.
    Missing(String),
    /// The links go round in a circle (or too deep).
    Loop,
    /// A Windows path (`C:\Users`, `\\server\share`): outside the image.
    Outside(String),
}

/// Paths a Windows link may point at, which mean nothing inside an image.
fn windows_path(t: &str) -> bool {
    let b = t.as_bytes();
    t.starts_with('\\') || (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic())
}

/// Follows the link at `link` (whose target is `target`), and every link
/// on the way, as the system using the image would: relative targets from
/// the link's folder, absolute ones from the volume's root.
pub fn resolve_link(fs: &mut dyn FileSystem, link: &VfsPath, target: &str) -> LinkEnd {
    let mut cur: Vec<String> = link.parent().map(|p| p.components().map(str::to_owned).collect()).unwrap_or_default();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut hops = 0;
    let push_target = |cur: &mut Vec<String>, queue: &mut VecDeque<String>, t: &str| {
        if t.starts_with('/') {
            cur.clear();
        }
        for (i, c) in t.split('/').enumerate() {
            queue.insert(i, c.to_owned());
        }
    };
    if windows_path(target) {
        return LinkEnd::Outside(target.to_owned());
    }
    push_target(&mut cur, &mut queue, target);
    let to_path = |cur: &[String]| VfsPath::parse(&format!("/{}", cur.join("/")));
    while let Some(c) = queue.pop_front() {
        match c.as_str() {
            "" | "." => continue,
            ".." => {
                cur.pop();
                continue;
            }
            _ => cur.push(c),
        }
        let missing = |cur: &[String], queue: &VecDeque<String>| {
            let rest: Vec<&str> = queue.iter().map(String::as_str).filter(|c| !c.is_empty()).collect();
            let mut p = format!("/{}", cur.join("/"));
            if !rest.is_empty() {
                p = format!("{}/{}", p.trim_end_matches('/'), rest.join("/"));
            }
            LinkEnd::Missing(p)
        };
        let Ok(path) = to_path(&cur) else { return missing(&cur, &queue) };
        let Ok(m) = fs.metadata(&path) else { return missing(&cur, &queue) };
        if m.kind == EntryKind::Symlink {
            hops += 1;
            if hops > 40 {
                return LinkEnd::Loop;
            }
            let Some(t) = m.symlink_target.clone() else { return missing(&cur, &queue) };
            if windows_path(&t) {
                return LinkEnd::Outside(t);
            }
            cur.pop();
            push_target(&mut cur, &mut queue, &t);
        } else if queue.iter().all(|c| c.is_empty() || c == ".") {
            return LinkEnd::Found(path, m);
        } else if m.kind != EntryKind::Directory && queue.iter().any(|c| !c.is_empty() && c != "." && c != "..") {
            return missing(&cur, &queue);
        }
    }
    match to_path(&cur) {
        Ok(p) => match fs.metadata(&p) {
            Ok(m) => LinkEnd::Found(p, m),
            Err(_) => LinkEnd::Missing(p.to_string()),
        },
        Err(_) => LinkEnd::Missing(format!("/{}", cur.join("/"))),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use dix_core::hostfs::HostFs;
    use std::os::unix::fs::symlink;

    #[test]
    fn follows_links_like_the_system() {
        let d = std::env::temp_dir().join(format!("dix-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("docs/deep")).unwrap();
        std::fs::write(d.join("docs/readme.md"), b"hello").unwrap();
        symlink("docs/readme.md", d.join("rel")).unwrap();
        symlink("/docs/readme.md", d.join("abs")).unwrap();
        symlink("../readme.md", d.join("docs/deep/up")).unwrap();
        symlink("rel", d.join("chain")).unwrap();
        symlink("docs", d.join("dirlink")).unwrap();
        symlink("dirlink/readme.md", d.join("through")).unwrap();
        symlink("nowhere/file", d.join("broken")).unwrap();
        symlink("loop-b", d.join("loop-a")).unwrap();
        symlink("loop-a", d.join("loop-b")).unwrap();
        symlink("C:\\Users\\me", d.join("windows")).unwrap();
        symlink("docs/readme.md/x", d.join("not-a-dir")).unwrap();
        let mut fs = HostFs::new(&d).unwrap();
        let mut end = |link: &str| {
            let p = VfsPath::parse(link).unwrap();
            let t = fs.metadata(&p).unwrap().symlink_target.unwrap();
            match resolve_link(&mut fs, &p, &t) {
                LinkEnd::Found(p, m) => format!("{p} {:?}", m.kind),
                other => format!("{other:?}"),
            }
        };
        assert_eq!(end("/rel"), "/docs/readme.md File");
        assert_eq!(end("/abs"), "/docs/readme.md File");
        assert_eq!(end("/docs/deep/up"), "/docs/readme.md File");
        assert_eq!(end("/chain"), "/docs/readme.md File");
        assert_eq!(end("/dirlink"), "/docs Directory");
        assert_eq!(end("/through"), "/docs/readme.md File");
        assert_eq!(end("/broken"), "Missing(\"/nowhere/file\")");
        assert_eq!(end("/loop-a"), "Loop");
        assert_eq!(end("/windows"), "Outside(\"C:\\\\Users\\\\me\")");
        assert_eq!(end("/not-a-dir"), "Missing(\"/docs/readme.md/x\")");
        let _ = std::fs::remove_dir_all(&d);
    }
}
