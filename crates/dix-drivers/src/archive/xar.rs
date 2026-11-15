//! XAR archives — macOS installer packages (`.pkg`) and `.xip` files —
//! read-only.
//!
//! A header ("xar!", its size, the table of contents' packed and unpacked
//! sizes), the table of contents (XML, zlib-compressed), then the heap.
//! The XML lists files, nested in their folders: name, type (file,
//! directory, symlink, hardlink), mode, owner, times, and for files where
//! their bytes are in the heap (offset, archived length, size) and how
//! they are encoded (stored, gzip — a zlib stream —, bzip2, lzma, xz).
//! An installer's `Payload` is a cpio archive, gzip-compressed or (newer
//! ones) pbzx: it opens as an archive of its own ("Open as image…").

use super::{Compression, Data, Node, Raw, Tree, clean_path};
use dix_core::{EntryKind, Error, MountLog, Result};
use std::collections::HashMap;

pub(super) const MAGIC: &[u8; 4] = b"xar!";

fn damaged(what: impl std::fmt::Display) -> Error {
    Error::corrupted(format!("xar archive: {what}"))
}

/// An XML element (only what a table of contents uses).
#[derive(Debug, Default)]
struct Element {
    name: String,
    attrs: Vec<(String, String)>,
    text: String,
    children: Vec<Element>,
}

impl Element {
    fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }
    fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim())
    }
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|a| a.0 == name).map(|a| a.1.as_str())
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let Some(end) = rest[i..].find(';') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let ent = &rest[i + 1..i + end];
        let c = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => out.push(c),
            None => out.push_str(&rest[i..i + end + 1]),
        }
        rest = &rest[i + end + 1..];
    }
    out.push_str(rest);
    out
}

/// A small XML parser: elements, attributes, text, entities; declarations,
/// comments and processing instructions skipped, CDATA kept as text.
fn parse_xml(s: &str) -> Result<Element> {
    let mut stack: Vec<Element> = vec![Element::default()];
    let mut at = 0;
    let b = s.as_bytes();
    while at < b.len() {
        if b[at] != b'<' {
            let end = s[at..].find('<').map_or(s.len(), |i| at + i);
            stack.last_mut().unwrap().text.push_str(&unescape(&s[at..end]));
            at = end;
            continue;
        }
        let rest = &s[at..];
        if rest.starts_with("<!--") {
            at += rest.find("-->").ok_or_else(|| damaged("an unclosed comment"))? + 3;
        } else if rest.starts_with("<![CDATA[") {
            let end = rest.find("]]>").ok_or_else(|| damaged("an unclosed CDATA section"))?;
            stack.last_mut().unwrap().text.push_str(&rest[9..end]);
            at += end + 3;
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            at += rest.find('>').ok_or_else(|| damaged("an unclosed declaration"))? + 1;
        } else if rest.starts_with("</") {
            let end = rest.find('>').ok_or_else(|| damaged("an unclosed tag"))?;
            let e = stack.pop().ok_or_else(|| damaged("a closing tag too many"))?;
            if e.name != rest[2..end].trim() {
                return Err(damaged(format!("<{}> closed by </{}>", e.name, rest[2..end].trim())));
            }
            stack.last_mut().ok_or_else(|| damaged("a closing tag too many"))?.children.push(e);
            at += end + 1;
        } else {
            // A start tag: its name, attributes, maybe self-closing.
            let mut i = 1;
            let mut quote = None;
            while i < rest.len() {
                let c = rest.as_bytes()[i];
                match quote {
                    Some(q) if c == q => quote = None,
                    Some(_) => {}
                    None if c == b'"' || c == b'\'' => quote = Some(c),
                    None if c == b'>' => break,
                    None => {}
                }
                i += 1;
            }
            if i >= rest.len() {
                return Err(damaged("an unclosed tag"));
            }
            let inner = &rest[1..i];
            let closed = inner.ends_with('/');
            let inner = inner.trim_end_matches('/');
            let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
            let mut e = Element { name: inner[..name_end].to_owned(), ..Default::default() };
            let mut a = inner[name_end..].trim();
            while let Some(eq) = a.find('=') {
                let key = a[..eq].trim().to_owned();
                let v = a[eq + 1..].trim_start();
                let q = v.chars().next().ok_or_else(|| damaged("an attribute without a value"))?;
                let end = v[1..].find(q).ok_or_else(|| damaged("an unclosed attribute"))?;
                e.attrs.push((key, unescape(&v[1..1 + end])));
                a = v[end + 2..].trim_start();
            }
            at += i + 1;
            if closed {
                stack.last_mut().unwrap().children.push(e);
            } else {
                stack.push(e);
            }
        }
    }
    let mut root = stack.pop().ok_or_else(|| damaged("no document"))?;
    if !stack.is_empty() {
        return Err(damaged("unclosed elements"));
    }
    root.children.pop().ok_or_else(|| damaged("an empty document"))
}

/// "2023-04-05T06:07:08Z" (or with a fraction) as Unix seconds.
fn iso_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se)
}

fn encoding(style: &str) -> Result<Compression> {
    Ok(match style {
        "application/octet-stream" => Compression::None,
        // (xar's "gzip" is a zlib stream.)
        "application/x-gzip" => Compression::Zlib,
        "application/x-bzip2" => Compression::Bzip2,
        "application/x-lzma" => Compression::Lzma,
        "application/x-xz" => Compression::Xz,
        s => return Err(Error::unsupported(format!("xar data encoded as {s}"))),
    })
}

pub(super) fn parse(raw: &mut Raw<'_>, log: &MountLog) -> Result<Tree> {
    let mut h = [0u8; 28];
    raw.read_at(0, &mut h)?;
    if &h[..4] != MAGIC {
        return Err(damaged("no header"));
    }
    let header = u16::from_be_bytes([h[4], h[5]]) as u64;
    let packed = u64::from_be_bytes(h[8..16].try_into().unwrap());
    if packed > 64 << 20 {
        return Err(damaged("the table of contents is impossibly large"));
    }
    let mut toc = vec![0u8; packed as usize];
    raw.read_at(header, &mut toc)?;
    let xml = Compression::Zlib.decompress(&toc)?;
    let doc = parse_xml(&String::from_utf8_lossy(&xml))?;
    let toc = doc.child("toc").ok_or_else(|| damaged("no <toc>"))?;
    let heap = header + packed;
    let mut tree = Tree::new();
    // Hard links point at the file that has the data, by its id.
    let mut by_id: HashMap<String, Node> = HashMap::new();
    let mut links = Vec::new();
    for f in toc.children.iter().filter(|c| c.name == "file") {
        add(f, "", heap, &mut tree, &mut by_id, &mut links)?;
    }
    for (path, id) in links {
        match by_id.get(&id) {
            Some(n) => tree.insert(&path, n.clone()),
            None => log.warn(format!("{path}: a hard link to a file that is not there")),
        }
    }
    Ok(tree)
}

fn add(
    f: &Element,
    dir: &str,
    heap: u64,
    tree: &mut Tree,
    by_id: &mut HashMap<String, Node>,
    links: &mut Vec<(String, String)>,
) -> Result<()> {
    let name = f.text_of("name").unwrap_or("");
    let Some(path) = clean_path(&format!("{dir}/{name}")).filter(|p| p != "/" && !name.is_empty()) else {
        return Ok(());
    };
    let ty = f.text_of("type").unwrap_or("file");
    let kind = match ty {
        "directory" => EntryKind::Directory,
        "symlink" => EntryKind::Symlink,
        "file" | "hardlink" => EntryKind::File,
        _ => EntryKind::Other,
    };
    let mut n = Node::new(kind);
    n.mtime = f.text_of("mtime").and_then(iso_time);
    n.mode = f.text_of("mode").and_then(|m| u32::from_str_radix(m, 8).ok()).map(|m| m & 0o7777);
    n.uid = f.text_of("uid").and_then(|v| v.parse().ok());
    n.gid = f.text_of("gid").and_then(|v| v.parse().ok());
    if kind == EntryKind::Symlink {
        let target = f.text_of("link").unwrap_or("").to_owned();
        n.size = target.len() as u64;
        n.link = Some(target);
    }
    // A hard link other than the original: the original's data.
    if ty == "hardlink"
        && let Some(l) = f.text_of("link").filter(|l| *l != "original")
    {
        links.push((path, l.to_owned()));
        return Ok(());
    }
    if let Some(d) = f.child("data") {
        let num = |k: &str| d.text_of(k).and_then(|v| v.parse::<u64>().ok());
        let (offset, len, size) = (num("offset").unwrap_or(0), num("length").unwrap_or(0), num("size").unwrap_or(0));
        let enc = d.child("encoding").and_then(|e| e.attr("style")).unwrap_or("application/octet-stream");
        let compression = encoding(enc)?;
        n.size = size;
        n.data = if compression == Compression::None {
            Data::Range { offset: heap + offset, len }
        } else {
            Data::Packed { offset: heap + offset, len, compression }
        };
    }
    if let Some(id) = f.attr("id") {
        by_id.insert(id.to_owned(), n.clone());
    }
    if kind == EntryKind::Directory {
        tree.update_dir(&path, n);
    } else {
        tree.insert(&path, n);
    }
    for c in f.children.iter().filter(|c| c.name == "file") {
        add(c, &path, heap, tree, by_id, links)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_and_times() {
        let doc = parse_xml(
            r#"<?xml version="1.0"?><!-- c --><xar><toc a='1' b="x &amp; y"><name>A&lt;B &#233;</name><e/><![CDATA[raw <x>]]></toc></xar>"#,
        )
        .unwrap();
        let toc = doc.child("toc").unwrap();
        assert_eq!(toc.attr("b"), Some("x & y"));
        assert_eq!(toc.text_of("name"), Some("A<B é"));
        assert_eq!(toc.children.len(), 2);
        assert_eq!(toc.text, "raw <x>");
        assert_eq!(iso_time("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_time("2024-02-29T12:34:56Z"), Some(1_709_210_096));
        assert!(parse_xml("<a><b></a>").is_err());
    }
}
