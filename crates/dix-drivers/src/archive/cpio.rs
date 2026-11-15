//! cpio: a header per member, then its name and its data.
//!
//! Read: "newc" (`070701`, what Linux initramfs uses), "crc" (`070702`,
//! newc with a checksum of the data), "odc" (`070707`, POSIX portable
//! ASCII) and the old binary format (either byte order). An archive ends
//! with a `TRAILER!!!` member; an initramfs may be several archives one
//! after another (e.g. an uncompressed one with CPU microcode, then a
//! compressed one), which are all read, as the kernel does: later members
//! replace earlier ones. Hard links (members sharing an inode) share the
//! data, which newc stores with one of them only. Device nodes, FIFOs and
//! sockets are kept (with their device numbers) and written back.
//!
//! Written: newc.

use super::codec::Compression;
use super::{Data, DevNode, Node, Raw, Tree, clean_path};
use dix_core::{EntryKind, Error, MountLog, Result};
use std::collections::HashMap;
use std::sync::Arc;

const TRAILER: &str = "TRAILER!!!";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// `070701`, and `070702` with a data checksum.
    Newc,
    Odc,
    /// The old binary format; `true`: big-endian.
    Binary(bool),
}

fn format_of(h: &[u8]) -> Option<Format> {
    match h {
        [b'0', b'7', b'0', b'7', b'0', b'1' | b'2', ..] => Some(Format::Newc),
        [b'0', b'7', b'0', b'7', b'0', b'7', ..] => Some(Format::Odc),
        [0xC7, 0x71, ..] => Some(Format::Binary(false)),
        [0x71, 0xC7, ..] => Some(Format::Binary(true)),
        _ => None,
    }
}

/// One member's header.
struct Header {
    name: String,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    mtime: u64,
    /// (device, inode): members with the same one are hard links.
    inode: (u64, u64),
    rdev: (u32, u32),
    size: u64,
    data_at: u64,
    next: u64,
}

/// Where member bytes come from: the archive's raw bytes, or a segment
/// unpacked into memory.
enum Src<'a, 'b> {
    Raw(&'a mut Raw<'b>),
    Buf(&'a [u8]),
}

impl Src<'_, '_> {
    fn size(&self) -> u64 {
        match self {
            Src::Raw(r) => r.size(),
            Src::Buf(b) => b.len() as u64,
        }
    }

    fn read(&mut self, at: u64, out: &mut [u8]) -> Result<()> {
        match self {
            Src::Raw(r) => r.read_at(at, out),
            Src::Buf(b) => {
                dix_core::device::check_range(at, out.len(), b.len() as u64)?;
                out.copy_from_slice(&b[at as usize..at as usize + out.len()]);
                Ok(())
            }
        }
    }

    fn vec(&mut self, at: u64, len: u64) -> Result<Vec<u8>> {
        let mut v = vec![0u8; len as usize];
        self.read(at, &mut v)?;
        Ok(v)
    }
}

fn hex(b: &[u8]) -> Option<u64> {
    u64::from_str_radix(std::str::from_utf8(b).ok()?, 16).ok()
}

fn oct(b: &[u8]) -> Option<u64> {
    u64::from_str_radix(std::str::from_utf8(b).ok()?.trim_matches(['\0', ' ']), 8).ok()
}

/// Whether `head` starts like a cpio archive (a plausible first header).
pub(super) fn is_cpio(head: &[u8]) -> bool {
    let Some(f) = format_of(head) else { return false };
    let mut src = Src::Buf(head);
    // The name may run past what was read: only the numbers are checked.
    match f {
        Format::Newc => head.len() >= 110 && head[6..110].iter().all(u8::is_ascii_hexdigit),
        Format::Odc => head.len() >= 76 && head[6..76].iter().all(|c| (b'0'..=b'7').contains(c)),
        Format::Binary(_) => head.len() >= 26 && parse_header(&mut src, 0, f).is_ok_and(|h| h.is_some()),
    }
}

fn bad() -> Error {
    Error::corrupted("a damaged cpio header")
}

/// The header at `at` (None: not a header here).
fn parse_header(src: &mut Src<'_, '_>, at: u64, f: Format) -> Result<Option<Header>> {
    let size = src.size();
    let (fixed, align) = match f {
        Format::Newc => (110u64, 4u64),
        Format::Odc => (76, 1),
        Format::Binary(_) => (26, 2),
    };
    if at + fixed > size {
        return Ok(None);
    }
    let h = src.vec(at, fixed)?;
    if format_of(&h) != Some(f) {
        return Ok(None);
    }
    let (mut hd, namesize) = match f {
        Format::Newc => {
            // ino, mode, uid, gid, nlink, mtime, filesize, devmajor,
            // devminor, rdevmajor, rdevminor, namesize, check.
            let n = |i: usize| hex(&h[6 + 8 * i..14 + 8 * i]).ok_or_else(bad);
            let hd = Header {
                name: String::new(),
                inode: (n(7)? << 32 | n(8)?, n(0)?),
                mode: n(1)? as u32,
                uid: n(2)? as u32,
                gid: n(3)? as u32,
                nlink: n(4)? as u32,
                mtime: n(5)?,
                size: n(6)?,
                rdev: (n(9)? as u32, n(10)? as u32),
                data_at: 0,
                next: 0,
            };
            (hd, n(11)?)
        }
        Format::Odc => {
            let o = |a: usize, b: usize| oct(&h[a..b]).ok_or_else(bad);
            let rdev = o(42, 48)?;
            let hd = Header {
                name: String::new(),
                inode: (o(6, 12)?, o(12, 18)?),
                mode: o(18, 24)? as u32,
                uid: o(24, 30)? as u32,
                gid: o(30, 36)? as u32,
                nlink: o(36, 42)? as u32,
                rdev: ((rdev >> 8) as u32 & 0xFF, rdev as u32 & 0xFF),
                mtime: o(48, 59)?,
                size: o(65, 76)?,
                data_at: 0,
                next: 0,
            };
            (hd, o(59, 65)?)
        }
        Format::Binary(be) => {
            let w = |i: usize| {
                let b = [h[2 * i], h[2 * i + 1]];
                (if be { u16::from_be_bytes(b) } else { u16::from_le_bytes(b) }) as u64
            };
            let rdev = w(7);
            let hd = Header {
                name: String::new(),
                inode: (w(1), w(2)),
                mode: w(3) as u32,
                uid: w(4) as u32,
                gid: w(5) as u32,
                nlink: w(6) as u32,
                rdev: ((rdev >> 8) as u32, rdev as u32 & 0xFF),
                mtime: w(8) << 16 | w(9),
                size: w(11) << 16 | w(12),
                data_at: 0,
                next: 0,
            };
            (hd, w(10))
        }
    };
    if namesize == 0 || namesize > 4096 || at + fixed + namesize > size {
        return Err(bad());
    }
    let name = src.vec(at + fixed, namesize)?;
    if name.last() != Some(&0) {
        return Err(bad());
    }
    hd.name = String::from_utf8_lossy(&name[..name.len() - 1]).into_owned();
    hd.data_at = (at + fixed + namesize).next_multiple_of(align);
    hd.next = (hd.data_at + hd.size).next_multiple_of(align);
    Ok(Some(hd))
}

/// Reads every archive in `raw` (an initramfs may hold several, some of
/// them compressed). Returns the members and the number of archives.
pub(super) fn parse(raw: &mut Raw<'_>, log: &MountLog) -> Result<(Tree, usize)> {
    let mut tree = Tree::new();
    let n = archives(&mut Src::Raw(raw), 0, &mut tree, log)?;
    Ok((tree, n))
}

/// The archives in `src` from `at` on; how many there were.
fn archives(src: &mut Src<'_, '_>, mut at: u64, tree: &mut Tree, log: &MountLog) -> Result<usize> {
    let mut count = 0;
    loop {
        let end = segment(src, at, tree, log)?;
        count += 1;
        // What follows the trailer: zero padding, then maybe another one.
        let size = src.size();
        let mut next = end;
        let mut chunk = vec![0u8; 4096];
        loop {
            let n = (size - next).min(4096) as usize;
            if n == 0 {
                return Ok(count);
            }
            src.read(next, &mut chunk[..n])?;
            match chunk[..n].iter().position(|&b| b != 0) {
                Some(i) => {
                    next += i as u64;
                    break;
                }
                None => next += n as u64,
            }
        }
        let mut head = vec![0u8; (size - next).min(512) as usize];
        src.read(next, &mut head)?;
        if format_of(&head).is_some() {
            at = next;
            continue;
        }
        let c = Compression::detect(&head);
        if c == Compression::None {
            log.warn(format!("Unrecognised data after the cpio archive at byte {next} is ignored."));
            return Ok(count);
        }
        log.info(format!("Another archive follows at byte {next}, {}-compressed: unpacking it…", c.name()));
        let packed = src.vec(next, size - next)?;
        let bytes = c.decompress(&packed)?;
        if !is_cpio(&bytes) {
            log.warn(format!("The {}-compressed data at byte {next} is not a cpio archive: ignored.", c.name()));
            return Ok(count);
        }
        return Ok(count + archives(&mut Src::Buf(&bytes), 0, tree, log)?);
    }
}

/// The names of one inode, and its data once found (with its length).
type HardLinks = (Vec<String>, Option<(Data, u64)>);

/// One archive from `at` to its trailer; returns where it ends.
fn segment(src: &mut Src<'_, '_>, mut at: u64, tree: &mut Tree, log: &MountLog) -> Result<u64> {
    let mut head = vec![0u8; 6];
    src.read(at, &mut head)?;
    let f = format_of(&head).ok_or_else(|| Error::corrupted("not a cpio archive"))?;
    let size = src.size();
    let in_memory = matches!(src, Src::Buf(_));
    // Hard links: the paths of each inode, and the data found for it.
    let mut links: HashMap<(u64, u64), HardLinks> = HashMap::new();
    let mut members = 0u64;
    loop {
        let hd = match parse_header(src, at, f) {
            Ok(Some(h)) => h,
            Ok(None) if members > 0 => {
                log.warn(format!("The cpio archive ends without a trailer at byte {at}."));
                break;
            }
            Ok(None) => return Err(Error::corrupted("not a cpio archive")),
            Err(e) if members > 0 => {
                log.warn(format!("{} at byte {at}: the rest of the archive is ignored.", e.message()));
                break;
            }
            Err(e) => return Err(e),
        };
        if hd.name == TRAILER {
            at = hd.next;
            break;
        }
        members += 1;
        at = hd.next;
        let Some(path) = clean_path(&hd.name) else {
            log.warn(format!("Skipped a member with an unusable name: {:?}", hd.name));
            continue;
        };
        let len = hd.size.min(size.saturating_sub(hd.data_at));
        if len < hd.size {
            log.warn(format!("{path} is cut short: the archive ends early."));
        }
        let mut node = Node::new(EntryKind::File);
        node.mtime = Some(hd.mtime as i64);
        node.mode = Some(hd.mode & 0o7777);
        node.uid = Some(hd.uid);
        node.gid = Some(hd.gid);
        match hd.mode & 0o170000 {
            0o040000 => node.kind = EntryKind::Directory,
            0o120000 => {
                let target = String::from_utf8_lossy(&src.vec(hd.data_at, len)?).into_owned();
                node.kind = EntryKind::Symlink;
                node.size = target.len() as u64;
                node.link = Some(target);
            }
            0o100000 | 0 => {
                node.size = len;
                node.data = if len == 0 {
                    Data::Empty
                } else if in_memory {
                    Data::Mem(Arc::new(src.vec(hd.data_at, len)?))
                } else {
                    Data::Range { offset: hd.data_at, len }
                };
                if hd.nlink > 1 {
                    let e = links.entry(hd.inode).or_default();
                    e.0.push(path.clone());
                    if len > 0 {
                        e.1 = Some((node.data.clone(), len));
                    }
                }
            }
            t => {
                node.kind = EntryKind::Other;
                node.dev = Some(DevNode { type_bits: t, major: hd.rdev.0, minor: hd.rdev.1 });
            }
        }
        if node.kind == EntryKind::Directory {
            tree.update_dir(&path, node);
        } else {
            tree.insert(&path, node);
        }
    }
    // newc stores a hard-linked file's data with one of its names only.
    for (paths, data) in links.into_values() {
        let Some((data, len)) = data else { continue };
        for p in paths {
            if let Some(n) = tree.nodes.get_mut(&p)
                && n.size == 0
            {
                n.data = data.clone();
                n.size = len;
            }
        }
    }
    Ok(at)
}

/// One newc header (with its name and padding).
fn newc(out: &mut Vec<u8>, name: &str, mode: u32, node: &Node, nlink: u32, ino: u32, size: u64) -> Result<()> {
    if size > u32::MAX as u64 {
        return Err(Error::invalid_input(format!("{name} is larger than cpio can hold (4 GiB)")));
    }
    let (major, minor) = node.dev.map_or((0, 0), |d| (d.major, d.minor));
    let fields = [
        ino,
        mode,
        node.uid.unwrap_or(0),
        node.gid.unwrap_or(0),
        nlink,
        node.mtime.unwrap_or(0).clamp(0, u32::MAX as i64) as u32,
        size as u32,
        0,
        0,
        major,
        minor,
        name.len() as u32 + 1,
        0,
    ];
    out.extend_from_slice(b"070701");
    for f in fields {
        out.extend_from_slice(format!("{f:08X}").as_bytes());
    }
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.resize(out.len().next_multiple_of(4), 0);
    Ok(())
}

/// Serialises `tree` as newc; `content` gives each file's bytes.
pub(super) fn write(tree: &Tree, content: &mut dyn FnMut(&str, &Node) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut ino = 0u32;
    for (path, node) in tree.iter() {
        ino += 1;
        let name = &path[1..];
        let perm = |default: u32| node.mode.unwrap_or(default) & 0o7777;
        let (mode, nlink, data) = match node.kind {
            EntryKind::Directory => (0o040000 | perm(0o755), 2, Vec::new()),
            EntryKind::Symlink => (0o120000 | perm(0o777), 1, node.link.clone().unwrap_or_default().into_bytes()),
            EntryKind::File => (0o100000 | perm(0o644), 1, content(path, node)?),
            EntryKind::Other => match node.dev {
                Some(d) => (d.type_bits | perm(0o644), 1, Vec::new()),
                None => continue,
            },
        };
        newc(&mut out, name, mode, node, nlink, ino, data.len() as u64)?;
        out.extend_from_slice(&data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    newc(&mut out, TRAILER, 0, &Node::new(EntryKind::File), 1, 0, 0)?;
    // As cpio does: up to a whole 512-byte block.
    out.resize(out.len().next_multiple_of(512), 0);
    Ok(out)
}
