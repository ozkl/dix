//! Unix `ar` archives (static libraries `.a`, Debian packages `.deb`) and
//! RPM packages (`.rpm`), read-only.
//!
//! **ar**: "!<arch>\n", then members of a 60-byte header (name, time, owner,
//! group, mode, size, all in text) and their data, padded to even. Long
//! names are in a "//" member (GNU: "/123" points into it) or right before
//! the data (BSD: "#1/20"); the symbol table ("/", "__.SYMDEF") is left out.
//!
//! **deb**: an ar of "debian-binary", "control.tar.*" and "data.tar.*"
//! (gzip, xz, zstd, bzip2 or none). The package's files are shown at the
//! root and its control files in DEBIAN/, as `dpkg-deb -R` lays them out.
//!
//! **rpm**: a 96-byte lead, a signature header (padded to 8), the header
//! (tags: name, version, release, the payload's format and compressor),
//! then the payload: a cpio archive, compressed.

use super::{Compression, Data, Node, Raw, Tree, clean_path, cpio, tar};
use dix_core::{EntryKind, Error, MountLog, Result};

pub(super) const AR_MAGIC: &[u8; 8] = b"!<arch>\n";
pub(super) const RPM_MAGIC: [u8; 4] = [0xED, 0xAB, 0xEE, 0xDB];

fn damaged(what: impl std::fmt::Display) -> Error {
    Error::corrupted(format!("ar archive: {what}"))
}

/// A member: its name, where its data is, and its header's details.
pub(super) struct Member {
    pub name: String,
    pub offset: u64,
    pub size: u64,
    pub mtime: Option<i64>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: Option<u32>,
}

fn field(b: &[u8]) -> &str {
    std::str::from_utf8(b).unwrap_or("").trim()
}

/// Whether an ar archive is a Debian package (its first member).
pub(super) fn is_deb(head: &[u8]) -> bool {
    head.starts_with(AR_MAGIC) && head.len() >= 8 + 16 && field(&head[8..24]).trim_end_matches('/') == "debian-binary"
}

pub(super) fn members(raw: &mut Raw<'_>) -> Result<Vec<Member>> {
    let size = raw.size();
    let mut out = Vec::new();
    let mut names: Vec<u8> = Vec::new();
    let mut pos = 8u64;
    while pos + 60 <= size {
        let mut h = [0u8; 60];
        raw.read_at(pos, &mut h)?;
        if &h[58..60] != b"`\n" {
            return Err(damaged(format!("no member header at {pos}")));
        }
        let mut size_field: u64 = field(&h[48..58]).parse().map_err(|_| damaged("a member's size is not a number"))?;
        let mut offset = pos + 60;
        let next = (offset + size_field).next_multiple_of(2);
        if offset + size_field > size {
            return Err(damaged("a member runs past the end"));
        }
        let raw_name = field(&h[..16]).to_owned();
        let name = if raw_name == "//" {
            names = vec![0u8; size_field as usize];
            raw.read_at(offset, &mut names)?;
            pos = next;
            continue;
        } else if raw_name == "/" || raw_name == "/SYM64/" || raw_name.starts_with("__.SYMDEF") {
            pos = next;
            continue;
        } else if let Some(n) = raw_name.strip_prefix("#1/") {
            // BSD: the name is the start of the data.
            let len: u64 = n.parse().map_err(|_| damaged("a long name's length"))?;
            let mut b = vec![0u8; len.min(size_field) as usize];
            raw.read_at(offset, &mut b)?;
            offset += len;
            size_field = size_field.saturating_sub(len);
            String::from_utf8_lossy(&b).trim_end_matches('\0').to_owned()
        } else if let Some(n) = raw_name.strip_prefix('/').and_then(|n| n.parse::<usize>().ok()) {
            // GNU: an offset into the long name table.
            let rest = names.get(n..).ok_or_else(|| damaged("a long name is outside the table"))?;
            let end = rest.windows(2).position(|w| w == b"/\n").or_else(|| rest.iter().position(|&c| c == b'\n'));
            String::from_utf8_lossy(&rest[..end.unwrap_or(rest.len())]).into_owned()
        } else {
            raw_name.strip_suffix('/').unwrap_or(&raw_name).to_owned()
        };
        let num = |b: &[u8], radix: u32| u64::from_str_radix(field(b), radix).ok();
        out.push(Member {
            name,
            offset,
            size: size_field,
            mtime: num(&h[16..28], 10).map(|t| t as i64),
            uid: num(&h[28..34], 10).map(|v| v as u32),
            gid: num(&h[34..40], 10).map(|v| v as u32),
            mode: num(&h[40..48], 8).map(|m| m as u32 & 0o7777),
        });
        pos = next;
    }
    Ok(out)
}

/// A plain ar archive: its members as files.
pub(super) fn parse(raw: &mut Raw<'_>) -> Result<Tree> {
    let mut tree = Tree::new();
    for m in members(raw)? {
        let Some(path) = clean_path(&m.name).filter(|p| p != "/") else { continue };
        let mut n = Node::new(EntryKind::File);
        n.size = m.size;
        n.data = Data::Range { offset: m.offset, len: m.size };
        (n.mtime, n.uid, n.gid, n.mode) = (m.mtime, m.uid, m.gid, m.mode);
        tree.insert(&path, n);
    }
    Ok(tree)
}

/// The compression a member's name says ("data.tar.xz"), else what its
/// bytes say.
fn compression_of(name: &str, head: &[u8]) -> Compression {
    match Compression::detect(head) {
        Compression::None if name.ends_with(".tar") => Compression::None,
        c => c,
    }
}

/// Moves a tree parsed from a part of a buffer to its place: ranges shifted
/// by `base`, paths under `under` ("" for the root).
fn graft(into: &mut Tree, part: Tree, base: u64, under: &str) {
    for (path, mut node) in part.nodes {
        if let Data::Range { offset, .. } = &mut node.data {
            *offset += base;
        }
        let path = if path == "/" { under.to_owned() } else { format!("{under}{path}") };
        if path.is_empty() || path == "/" {
            into.update_dir("/", node);
        } else if node.kind == EntryKind::Directory {
            into.update_dir(&path, node);
        } else {
            into.insert(&path, node);
        }
    }
}

/// A Debian package: the data at the root, the control files in DEBIAN/.
/// Returns the tree and the unpacked tars (one buffer, data first).
pub(super) fn parse_deb(raw: &mut Raw<'_>, log: &MountLog) -> Result<(Tree, Vec<u8>)> {
    let members = members(raw)?;
    let find = |prefix: &str| members.iter().find(|m| m.name.starts_with(prefix));
    let (Some(control), Some(data)) = (find("control.tar"), find("data.tar")) else {
        return Err(Error::corrupted("a Debian package without control.tar or data.tar"));
    };
    let mut buf = Vec::new();
    let mut parts = Vec::new();
    for (m, under) in [(data, ""), (control, "/DEBIAN")] {
        let mut packed = vec![0u8; m.size as usize];
        raw.read_at(m.offset, &mut packed)?;
        let c = compression_of(&m.name, &packed[..packed.len().min(16)]);
        let bytes = c.decompress(&packed).map_err(|e| e.with_context(&m.name))?;
        parts.push((buf.len() as u64, bytes.len(), under));
        buf.extend(bytes);
    }
    let mut tree = Tree::new();
    for (base, len, under) in parts {
        let slice = &buf[base as usize..base as usize + len];
        let mut sub = Raw { dev: &mut *raw.dev, buf: Some(slice) };
        let part = tar::parse(&mut sub, log)?;
        graft(&mut tree, part, base, under);
    }
    if let Some(v) = find("debian-binary") {
        let mut b = vec![0u8; v.size.min(16) as usize];
        raw.read_at(v.offset, &mut b)?;
        log.info(format!("Debian package, format {}.", String::from_utf8_lossy(&b).trim()));
    }
    Ok((tree, buf))
}

// ----- rpm ---------------------------------------------------------------------------

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

/// A header's tags: (tag, type, the value's bytes onwards).
type Tags = Vec<(u32, u32, Vec<u8>)>;

/// A header structure at `at`: its tags and where it ends.
fn rpm_header(raw: &mut Raw<'_>, at: u64) -> Result<(Tags, u64)> {
    let mut h = [0u8; 16];
    raw.read_at(at, &mut h)?;
    if h[..4] != [0x8E, 0xAD, 0xE8, 0x01] {
        return Err(Error::corrupted("rpm: no header where one should be"));
    }
    let (n, len) = (be32(&h, 8) as usize, be32(&h, 12) as usize);
    if n > 1 << 16 || len > 64 << 20 {
        return Err(Error::corrupted("rpm: a header is impossibly large"));
    }
    let mut b = vec![0u8; n * 16 + len];
    raw.read_at(at + 16, &mut b)?;
    let store = &b[n * 16..];
    let mut tags = Vec::new();
    for i in 0..n {
        let e = &b[i * 16..i * 16 + 16];
        let (tag, ty, off) = (be32(e, 0), be32(e, 4), be32(e, 8) as usize);
        // Strings: up to their NUL.
        let value = store.get(off..).map(|v| v.to_vec()).unwrap_or_default();
        tags.push((tag, ty, value));
    }
    Ok((tags, at + 16 + (n * 16 + len) as u64))
}

fn rpm_string(tags: &[(u32, u32, Vec<u8>)], tag: u32) -> Option<String> {
    let (_, _, v) = tags.iter().find(|t| t.0 == tag)?;
    let end = v.iter().position(|&c| c == 0).unwrap_or(v.len());
    Some(String::from_utf8_lossy(&v[..end]).into_owned())
}

/// An RPM package: its files (from the cpio payload), the unpacked payload,
/// and "name-version-release".
pub(super) fn parse_rpm(raw: &mut Raw<'_>, log: &MountLog) -> Result<(Tree, Vec<u8>, String)> {
    let mut lead = [0u8; 96];
    raw.read_at(0, &mut lead)?;
    if lead[..4] != RPM_MAGIC {
        return Err(Error::corrupted("not an rpm package"));
    }
    let (_, sig_end) = rpm_header(raw, 96)?;
    let (tags, payload) = rpm_header(raw, sig_end.next_multiple_of(8))?;
    let nvr = [1000, 1001, 1002].iter().filter_map(|&t| rpm_string(&tags, t)).collect::<Vec<_>>().join("-");
    let format = rpm_string(&tags, 1124).unwrap_or_else(|| "cpio".into());
    if format != "cpio" {
        return Err(Error::unsupported(format!("rpm payloads in {format} format")));
    }
    let size = raw.size().saturating_sub(payload);
    let mut packed = vec![0u8; size as usize];
    raw.read_at(payload, &mut packed)?;
    let c = Compression::detect(&packed[..packed.len().min(16)]);
    let named = rpm_string(&tags, 1125).unwrap_or_else(|| "gzip".into());
    log.info(format!("RPM package {nvr}, payload compressed with {named}."));
    let bytes = c.decompress(&packed)?;
    let (tree, _) = cpio::parse(&mut Raw { dev: &mut *raw.dev, buf: Some(&bytes) }, log)?;
    Ok((tree, bytes, nvr))
}
