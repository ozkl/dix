//! tar: 512-byte headers, each followed by the member's data padded to 512.
//!
//! Read: v7, ustar (with the 155-byte name prefix), GNU long names/links
//! (`L`/`K`), pax extended headers (`x`: `path`, `linkpath`, `size`,
//! `mtime`, `uid`, `gid`), base-256 numbers for big sizes. Written: ustar,
//! with a pax header when a name, link or size does not fit.

use super::{Data, Node, Raw, Tree, clean_path};
use dix_core::{EntryKind, Error, MountLog, Result};

const BLOCK: u64 = 512;

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// An octal (or GNU base-256) number field.
fn number(b: &[u8]) -> Option<u64> {
    if b.first().is_some_and(|&c| c & 0x80 != 0) {
        // Base-256: big-endian, the top bit of the first byte is the flag.
        let mut v: u64 = (b[0] & 0x3F) as u64;
        for &c in &b[1..] {
            v = v.checked_mul(256)?.checked_add(c as u64)?;
        }
        return Some(v);
    }
    let s = cstr(b);
    let s = s.trim_matches(|c: char| c == ' ' || c == '\0');
    if s.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(s, 8).ok()
}

/// A header's checksum: its bytes, with the checksum field as spaces.
fn checksum(h: &[u8]) -> (u64, i64) {
    let mut unsigned = 0u64;
    let mut signed = 0i64;
    for (i, &b) in h.iter().enumerate() {
        let b = if (148..156).contains(&i) { b' ' } else { b };
        unsigned += b as u64;
        signed += b as i8 as i64;
    }
    (unsigned, signed)
}

/// Whether `h` is a valid tar header (checksum matches).
pub(super) fn is_header(h: &[u8]) -> bool {
    if h.len() < 512 || h.iter().all(|&b| b == 0) {
        return false;
    }
    let Some(stored) = number(&h[148..156]) else { return false };
    let (u, s) = checksum(&h[..512]);
    stored == u || stored as i64 == s
}

/// pax records: "<len> key=value\n".
fn pax(data: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let Some(sp) = data[at..].iter().position(|&b| b == b' ') else { break };
        let Some(len) = std::str::from_utf8(&data[at..at + sp]).ok().and_then(|s| s.parse::<usize>().ok()) else {
            break;
        };
        if len == 0 || at + len > data.len() {
            break;
        }
        let rec = &data[at + sp + 1..at + len];
        let rec = rec.strip_suffix(b"\n").unwrap_or(rec);
        if let Some(eq) = rec.iter().position(|&b| b == b'=') {
            out.push((
                String::from_utf8_lossy(&rec[..eq]).into_owned(),
                String::from_utf8_lossy(&rec[eq + 1..]).into_owned(),
            ));
        }
        at += len;
    }
    out
}

pub(super) fn parse(raw: &mut Raw<'_>, log: &MountLog) -> Result<Tree> {
    let mut tree = Tree::new();
    let size = raw.size();
    let mut at = 0u64;
    let (mut long_name, mut long_link): (Option<String>, Option<String>) = (None, None);
    let mut ext: Vec<(String, String)> = Vec::new();
    let mut members = 0u64;
    let mut h = [0u8; 512];
    while at + BLOCK <= size {
        raw.read_at(at, &mut h)?;
        if h.iter().all(|&b| b == 0) {
            break; // end of archive
        }
        if !is_header(&h) {
            if members == 0 {
                return Err(Error::corrupted("not a tar archive (bad header checksum)"));
            }
            log.warn(format!("A damaged tar header at byte {at}: the rest of the archive is ignored."));
            break;
        }
        let kind = h[156];
        let mut len = number(&h[124..136]).ok_or_else(|| Error::corrupted("a bad size in a tar header"))?;
        let data_at = at + BLOCK;
        let get = |raw: &mut Raw<'_>, len: u64| -> Result<Vec<u8>> {
            if len > 16 << 20 || data_at + len > size {
                return Err(Error::corrupted("a damaged tar extension header"));
            }
            let mut v = vec![0u8; len as usize];
            raw.read_at(data_at, &mut v)?;
            Ok(v)
        };
        // Extension headers describe the next member.
        match kind {
            b'L' => long_name = Some(cstr(&get(raw, len)?)),
            b'K' => long_link = Some(cstr(&get(raw, len)?)),
            b'x' => ext = pax(&get(raw, len)?),
            b'g' => {} // global pax header: nothing we need
            _ => {
                members += 1;
                let mut name = cstr(&h[0..100]);
                if &h[257..262] == b"ustar" {
                    let prefix = cstr(&h[345..500]);
                    if !prefix.is_empty() {
                        name = format!("{prefix}/{name}");
                    }
                }
                let mut link = cstr(&h[157..257]);
                let mut mtime = number(&h[136..148]).map(|t| t as i64);
                let (mut uid, mut gid) = (number(&h[108..116]), number(&h[116..124]));
                if let Some(n) = long_name.take() {
                    name = n;
                }
                if let Some(l) = long_link.take() {
                    link = l;
                }
                for (k, v) in ext.drain(..) {
                    match k.as_str() {
                        "path" => name = v,
                        "linkpath" => link = v,
                        "size" => len = v.parse().unwrap_or(len),
                        "mtime" => mtime = v.split('.').next().and_then(|s| s.parse().ok()).or(mtime),
                        "uid" => uid = v.parse().ok().or(uid),
                        "gid" => gid = v.parse().ok().or(gid),
                        _ => {}
                    }
                }
                let Some(path) = clean_path(&name) else {
                    log.warn(format!("Skipped a member with an unusable name: {name:?}"));
                    at = data_at + len.div_ceil(BLOCK) * BLOCK;
                    continue;
                };
                let mode = number(&h[100..108]).map(|m| m as u32 & 0o7777);
                let mut node = Node::new(EntryKind::File);
                node.mtime = mtime;
                node.mode = mode;
                node.uid = uid.map(|v| v as u32);
                node.gid = gid.map(|v| v as u32);
                match kind {
                    b'0' | 0 | b'7' => {
                        if data_at + len > size {
                            log.warn(format!("{path} is cut short: the archive ends early."));
                        }
                        node.size = len;
                        node.data = Data::Range { offset: data_at, len: len.min(size.saturating_sub(data_at)) };
                    }
                    b'5' => node.kind = EntryKind::Directory,
                    b'2' => {
                        node.kind = EntryKind::Symlink;
                        node.size = link.len() as u64;
                        node.link = Some(link);
                    }
                    b'1' => {
                        // A hard link: the same content as an earlier member.
                        match clean_path(&link).and_then(|l| tree.get(&l).cloned()) {
                            Some(target) if target.kind == EntryKind::File => {
                                node.size = target.size;
                                node.data = target.data;
                            }
                            _ => log.warn(format!("{path}: a hard link to a missing file ({link})")),
                        }
                    }
                    b'S' => {
                        log.warn(format!("{path}: GNU sparse files are not supported; shown as empty."));
                    }
                    _ => node.kind = EntryKind::Other,
                }
                // Data of directories etc. is skipped below like any other.
                if node.kind == EntryKind::Directory && tree.get(&path).is_some_and(|n| n.kind == EntryKind::Directory)
                {
                    tree.update_dir(&path, node);
                } else {
                    tree.insert(&path, node);
                }
            }
        }
        at = data_at + len.div_ceil(BLOCK) * BLOCK;
    }
    Ok(tree)
}

fn octal(b: &mut [u8], v: u64) {
    let s = format!("{:0w$o}", v, w = b.len() - 1);
    b[..s.len()].copy_from_slice(s.as_bytes());
    b[b.len() - 1] = 0;
}

fn fits(v: u64, field: usize) -> bool {
    v < 1u64 << (3 * (field as u32 - 1))
}

fn header(name: &str, kind: u8, node: &Node, size: u64, link: &str) -> [u8; 512] {
    let mut h = [0u8; 512];
    let nb = name.as_bytes();
    h[..nb.len().min(100)].copy_from_slice(&nb[..nb.len().min(100)]);
    let default_mode = if kind == b'5' { 0o755 } else { 0o644 };
    octal(&mut h[100..108], (node.mode.unwrap_or(default_mode) & 0o7777) as u64);
    octal(&mut h[108..116], node.uid.unwrap_or(0) as u64 & 0o7777777);
    octal(&mut h[116..124], node.gid.unwrap_or(0) as u64 & 0o7777777);
    octal(&mut h[124..136], if fits(size, 12) { size } else { 0 });
    octal(&mut h[136..148], node.mtime.unwrap_or(0).clamp(0, 0o77777777777) as u64);
    h[156] = kind;
    let lb = link.as_bytes();
    h[157..157 + lb.len().min(100)].copy_from_slice(&lb[..lb.len().min(100)]);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let (sum, _) = checksum(&h);
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    h
}

fn pad(out: &mut Vec<u8>) {
    let n = out.len().next_multiple_of(BLOCK as usize);
    out.resize(n, 0);
}

/// Serialises `tree`; `content` gives each file's bytes.
pub(super) fn write(tree: &Tree, content: &mut dyn FnMut(&str, &Node) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if tree.iter().next().is_none() {
        // An empty archive is all zeros, which nothing recognises as tar:
        // give it the "./" entry `tar -cf x.tar .` writes.
        let mut root = Node::new(EntryKind::Directory);
        root.mtime = Some(dix_core::time::unix_now());
        out.extend_from_slice(&header("./", b'5', &root, 0, ""));
    }
    for (path, node) in tree.iter() {
        let rel = &path[1..];
        let (kind, data, link) = match node.kind {
            EntryKind::Directory => (b'5', Vec::new(), String::new()),
            EntryKind::Symlink => (b'2', Vec::new(), node.link.clone().unwrap_or_default()),
            EntryKind::File => (b'0', content(path, node)?, String::new()),
            EntryKind::Other => continue,
        };
        let name = if kind == b'5' { format!("{rel}/") } else { rel.to_owned() };
        let size = data.len() as u64;
        // pax for what ustar cannot hold.
        let mut records = Vec::new();
        if name.len() > 100 || !name.is_ascii() {
            records.push(("path", name.clone()));
        }
        if link.len() > 100 || !link.is_ascii() {
            records.push(("linkpath", link.clone()));
        }
        if !fits(size, 12) {
            records.push(("size", size.to_string()));
        }
        if !records.is_empty() {
            let mut body = Vec::new();
            for (k, v) in records {
                // The length counts itself: find the fixed point.
                let rest = format!(" {k}={v}\n");
                let mut len = rest.len() + 1;
                while format!("{len}{rest}").len() != len {
                    len = format!("{len}{rest}").len();
                }
                body.extend_from_slice(format!("{len}{rest}").as_bytes());
            }
            let short: String = name.chars().filter(|c| c.is_ascii()).take(90).collect();
            let mut x = Node::new(EntryKind::File);
            x.mtime = node.mtime;
            out.extend_from_slice(&header(&format!("PaxHeaders/{short}"), b'x', &x, body.len() as u64, ""));
            out.extend_from_slice(&body);
            pad(&mut out);
        }
        out.extend_from_slice(&header(&name, kind, node, size, &link));
        out.extend_from_slice(&data);
        pad(&mut out);
    }
    // End: two zero blocks, then up to a whole 10 KiB record (as tar does).
    out.resize(out.len() + 1024, 0);
    let n = out.len().next_multiple_of(10240);
    out.resize(n, 0);
    Ok(out)
}
