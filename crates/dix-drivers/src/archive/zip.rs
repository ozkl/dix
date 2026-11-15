//! zip: local headers with the data, a central directory listing every
//! member, and the end-of-central-directory record (EOCD) last.
//!
//! Read: stored and deflated members (streamed), every other PKZIP method
//! but PPMd (Shrink, Reduce, Implode, Deflate64, bzip2, LZMA, zstd, xz; see
//! `pkzip`; read whole), zip64 (sizes, offsets and counts past 4 GiB /
//! 65535), split zips, Unix modes and symbolic links, the extended
//! timestamp, UTF-8 names, data before the archive (self-extractors).
//! Encrypted and PPMd members are listed but cannot be read.
//! Written: deflate (or stored when that is not smaller), zip64 when
//! needed, Unix modes, UTF-8 names; unchanged members are copied as they are.

use super::{Compression, Data, Node, Raw, Tree, clean_path, codec, dos_time, from_dos_time};
use dix_core::{EntryKind, Error, ErrorKind, MountLog, Result};

const LOCAL: u32 = 0x0403_4B50;
const CENTRAL: u32 = 0x0201_4B50;
const EOCD: u32 = 0x0605_4B50;
const EOCD64: u32 = 0x0606_4B50;
const EOCD64_LOCATOR: u32 = 0x0706_4B50;

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// The end of a zip: (EOCD position, the record and its comment).
fn find_eocd(raw: &mut Raw<'_>) -> Result<Option<(u64, Vec<u8>)>> {
    let size = raw.size();
    if size < 22 {
        return Ok(None);
    }
    let tail_len = size.min(22 + 65535);
    let mut tail = vec![0u8; tail_len as usize];
    raw.read_at(size - tail_len, &mut tail)?;
    for i in (0..=tail.len() - 22).rev() {
        if le32(&tail, i) == EOCD && i + 22 + le16(&tail, i + 20) as usize == tail.len() {
            return Ok(Some((size - tail_len + i as u64, tail[i..].to_vec())));
        }
    }
    Ok(None)
}

/// Whether `raw` looks like a zip (for probing).
pub(super) fn looks_like_zip(raw: &mut Raw<'_>) -> Result<bool> {
    let mut sig = [0u8; 4];
    if raw.size() >= 4 {
        raw.read_at(0, &mut sig)?;
        if u32::from_le_bytes(sig) == LOCAL {
            return Ok(true);
        }
    }
    Ok(find_eocd(raw)?.is_some())
}

/// The zip archive comment (shown as the volume label).
pub(super) fn comment(raw: &mut Raw<'_>) -> Result<Option<String>> {
    Ok(find_eocd(raw)?.map(|(_, e)| String::from_utf8_lossy(&e[22..]).into_owned()).filter(|c| !c.is_empty()))
}

pub(super) fn parse(raw: &mut Raw<'_>, log: &MountLog) -> Result<Tree> {
    let (eocd_at, eocd) = find_eocd(raw)?.ok_or_else(|| Error::corrupted("not a zip archive (no end record)"))?;
    let (mut count, mut cd_size, mut cd_offset) =
        (le16(&eocd, 10) as u64, le32(&eocd, 12) as u64, le32(&eocd, 16) as u64);
    // A split zip (x.z01, x.z02, …, x.zip): offsets are within a "disk"
    // (part); the parts are joined in order, so a disk starts where the
    // sizes of those before it add up to.
    let (mut last_disk, mut cd_disk) = (le16(&eocd, 4) as u64, le16(&eocd, 6) as u64);
    let starts = raw.part_starts();
    let mut cd_end = eocd_at;
    // zip64: a locator right before the EOCD points at the zip64 record.
    if eocd_at >= 20 {
        let mut loc = [0u8; 20];
        raw.read_at(eocd_at - 20, &mut loc)?;
        if le32(&loc, 0) == EOCD64_LOCATOR {
            let disk = le32(&loc, 4) as usize;
            let at = le64(&loc, 8) + starts.as_ref().and_then(|s| s.get(disk).copied()).unwrap_or(0);
            let mut r = [0u8; 56];
            if at + 56 <= raw.size() {
                raw.read_at(at, &mut r)?;
                if le32(&r, 0) == EOCD64 {
                    (count, cd_size, cd_offset) = (le64(&r, 32), le64(&r, 40), le64(&r, 48));
                    (last_disk, cd_disk) = (le32(&r, 16) as u64, le32(&r, 20) as u64);
                    cd_end = at;
                }
            }
        }
    }
    let disk_start = |d: u64| -> Result<u64> {
        if last_disk == 0 {
            return Ok(0);
        }
        match &starts {
            Some(s) if s.len() == last_disk as usize + 2 => Ok(s[d.min(last_disk) as usize]),
            Some(s) => Err(Error::corrupted(format!(
                "this split zip has {} parts, but {} were found next to it",
                last_disk + 1,
                s.len() - 1
            ))),
            None => Err(Error::unsupported(format!(
                "this is the last part of a zip split into {} parts (x.z01, x.z02, …, x.zip); \
                 the other parts must be in the same folder, with their names",
                last_disk + 1
            ))),
        }
    };
    // Data in front of the archive (a self-extractor) shifts every offset.
    let shift = if last_disk > 0 {
        0
    } else {
        cd_end.checked_sub(cd_size).and_then(|start| start.checked_sub(cd_offset)).unwrap_or(0)
    };
    let cd_start = cd_offset + shift + disk_start(cd_disk)?;
    if last_disk > 0 {
        log.info(format!("A zip split into {} parts.", last_disk + 1));
    }
    if cd_size > 1 << 31 || cd_start + cd_size > raw.size() {
        return Err(Error::corrupted("the zip central directory is outside the file"));
    }
    let mut cd = vec![0u8; cd_size as usize];
    raw.read_at(cd_start, &mut cd)?;
    let mut tree = Tree::new();
    let mut at = 0usize;
    let mut skipped = 0;
    for _ in 0..count {
        if at + 46 > cd.len() || le32(&cd, at) != CENTRAL {
            log.warn("The zip central directory ends early: some members may be missing.");
            break;
        }
        let e = &cd[at..];
        let (made_by, flags, method) = (le16(e, 4), le16(e, 8), le16(e, 10));
        let (time, date, crc) = (le16(e, 12), le16(e, 14), le32(e, 16));
        let (mut csize, mut size) = (le32(e, 20) as u64, le32(e, 24) as u64);
        let (name_len, extra_len, comment_len) = (le16(e, 28) as usize, le16(e, 30) as usize, le16(e, 32) as usize);
        let external = le32(e, 38);
        let mut local = le32(e, 42) as u64;
        let mut aes = None;
        let mut disk = le16(e, 34) as u64;
        let end = 46 + name_len + extra_len + comment_len;
        if at + end > cd.len() {
            log.warn("A damaged zip central directory entry: the rest is ignored.");
            break;
        }
        let raw_name = &e[46..46 + name_len];
        // Bit 11: UTF-8; otherwise CP437, which is ASCII for most names.
        let name = String::from_utf8_lossy(raw_name).replace('\\', "/");
        let mut mtime = from_dos_time(date, time);
        let mut extra = &e[46 + name_len..46 + name_len + extra_len];
        while extra.len() >= 4 {
            let (id, len) = (le16(extra, 0), le16(extra, 2) as usize);
            let body = &extra[4..(4 + len).min(extra.len())];
            match id {
                0x0001 => {
                    // zip64: the 64-bit values of the fields that are 0xFFFFFFFF, in order.
                    let mut k = 0;
                    for field in [&mut size, &mut csize, &mut local] {
                        if *field == 0xFFFF_FFFF && k + 8 <= body.len() {
                            *field = le64(body, k);
                            k += 8;
                        }
                    }
                    if disk == 0xFFFF && k + 4 <= body.len() {
                        disk = le32(body, k) as u64;
                    }
                }
                0x9901 if body.len() >= 7 => {
                    aes = Some(ZipAes { version: le16(body, 0), strength: body[4], method: le16(body, 5) });
                }
                0x5455 if !body.is_empty() && body[0] & 1 != 0 && body.len() >= 5 => {
                    mtime = Some(le32(body, 1) as i32 as i64);
                }
                _ => {}
            }
            extra = &extra[(4 + len).min(extra.len())..];
        }
        at += end;
        let Some(path) = clean_path(&name) else {
            skipped += 1;
            continue;
        };
        let unix_mode = (made_by >> 8 == 3).then_some(external >> 16).filter(|&m| m != 0);
        let mut node = Node::new(EntryKind::File);
        node.mtime = mtime;
        node.mode = unix_mode.map(|m| m & 0o7777);
        let is_dir = name.ends_with('/') || unix_mode.is_some_and(|m| m & 0o170000 == 0o040000) || external & 0x10 != 0;
        let is_link = unix_mode.is_some_and(|m| m & 0o170000 == 0o120000);
        let header = local + shift + if last_disk > 0 { disk_start(disk)? } else { 0 };
        let encrypted = flags & 1 != 0;
        let dos = (time, date);
        node.data = Data::Zip { header, csize, size, method, crc, encrypted, flags, dos, aes };
        node.size = size;
        if is_dir {
            node.kind = EntryKind::Directory;
            node.data = Data::Empty;
            node.size = 0;
            tree.update_dir(&path, node);
        } else {
            if is_link {
                node.kind = EntryKind::Symlink;
            }
            tree.insert(&path, node);
        }
    }
    if skipped > 0 {
        log.warn(format!("Skipped {skipped} member(s) with unusable names."));
    }
    Ok(tree)
}

/// Where a member's data starts (after its local header).
fn data_start(raw: &mut Raw<'_>, header: u64) -> Result<u64> {
    let mut h = [0u8; 30];
    raw.read_at(header, &mut h)?;
    if le32(&h, 0) != LOCAL {
        return Err(Error::corrupted("a damaged zip member header"));
    }
    Ok(header + 30 + le16(&h, 26) as u64 + le16(&h, 28) as u64)
}

/// A member's compressed bytes, as stored.
fn stored_bytes(raw: &mut Raw<'_>, header: u64, csize: u64) -> Result<Vec<u8>> {
    let start = data_start(raw, header)?;
    if start + csize > raw.size() || csize > 16 << 30 {
        return Err(Error::corrupted("a zip member extends beyond the archive"));
    }
    let mut v = vec![0u8; csize as usize];
    raw.read_at(start, &mut v)?;
    Ok(v)
}

/// WinZip AES: the vendor version (1: AE-1, the CRC is kept; 2: AE-2, it
/// is 0), the key strength (1–3) and the real method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ZipAes {
    pub version: u16,
    pub strength: u8,
    pub method: u16,
}

/// A member's content; `passwords` are tried on an encrypted one.
pub(super) fn read_member(raw: &mut Raw<'_>, data: &Data, passwords: &[String]) -> Result<Vec<u8>> {
    let Data::Zip { header, csize, size, method, crc, encrypted, flags, dos, aes } = *data else {
        return Err(Error::invalid_input("not a zip member"));
    };
    if size > 16 << 30 {
        return Err(Error::corrupted("a zip member is too large"));
    }
    let packed = stored_bytes(raw, header, csize)?;
    if !encrypted {
        let bytes = unpack(&packed, method, size, flags)?;
        if dix_core::partition::crc32(&bytes) != crc {
            return Err(Error::corrupted("zip member checksum mismatch"));
        }
        return Ok(bytes);
    }
    if flags & 0x40 != 0 {
        return Err(Error::unsupported("reading zip members with PKWARE strong encryption"));
    }
    if passwords.is_empty() {
        return Err(Error::new(ErrorKind::PasswordRequired, "this file is encrypted: a password is needed"));
    }
    // ZipCrypto's check byte: the CRC's top byte, or the time's when the
    // sizes and CRC come after the data.
    let check = if flags & 8 != 0 { (dos.0 >> 8) as u8 } else { (crc >> 24) as u8 };
    for password in passwords {
        let plain = match aes {
            Some(a) => super::zipcrypt::winzip_aes(password.as_bytes(), &packed, a.strength)?,
            None => super::zipcrypt::zipcrypto(password.as_bytes(), &packed, check)?,
        };
        let Some(plain) = plain else { continue };
        let real_method = aes.map_or(method, |a| a.method);
        let bytes = match unpack(&plain, real_method, size, flags) {
            Ok(b) => b,
            // A wrong ZipCrypto password passes the check byte 1 time in 256.
            Err(_) if aes.is_none() => continue,
            Err(e) => return Err(e),
        };
        // AE-2 has no CRC (the authentication code checked the data).
        let crc_ok = aes.is_some_and(|a| a.version == 2) || dix_core::partition::crc32(&bytes) == crc;
        if crc_ok {
            return Ok(bytes);
        }
        if aes.is_some() {
            return Err(Error::corrupted("zip member checksum mismatch"));
        }
    }
    Err(Error::new(ErrorKind::PasswordRequired, "this file is encrypted, and no password given opens it"))
}

/// A member's data unpacked with `method`.
fn unpack(packed: &[u8], method: u16, size: u64, flags: u16) -> Result<Vec<u8>> {
    let n = size as usize;
    let bytes = match method {
        0 => packed[..packed.len().min(n)].to_vec(),
        8 => codec::inflate(packed, n)?.0,
        1 => super::pkzip::unshrink(packed, n)?,
        2..=5 => super::pkzip::unreduce(packed, n, method as u32 - 1)?,
        6 => super::pkzip::explode(packed, n, flags)?,
        9 => super::pkzip::inflate64(packed, n)?,
        12 => Compression::Bzip2.decompress(packed)?,
        14 => lzma(packed, size, flags)?,
        93 => Compression::Zstd.decompress(packed)?,
        95 => Compression::Xz.decompress(packed)?,
        98 => return Err(Error::unsupported("reading PPMd-compressed zip members")),
        99 => return Err(Error::corrupted("an AES-encrypted zip member without its AES record")),
        m => return Err(Error::unsupported(format!("zip compression method {m}"))),
    };
    if bytes.len() as u64 != size {
        return Err(Error::corrupted("a zip member's data has the wrong size"));
    }
    Ok(bytes)
}

/// LZMA (method 14): a version, the properties' size and the properties,
/// then raw LZMA data, ended by an end marker if flag bit 1 says so.
#[cfg(any(feature = "archive-xz", feature = "archive-7z"))]
fn lzma(packed: &[u8], size: u64, flags: u16) -> Result<Vec<u8>> {
    if packed.len() < 9 || le16(packed, 2) != 5 {
        return Err(Error::corrupted("damaged LZMA data in a zip member"));
    }
    // lzma-rs wants the classic header: properties, then the size (all
    // ones: "up to the end marker").
    let mut stream = packed[4..9].to_vec();
    let known = if flags & 2 != 0 { u64::MAX } else { size };
    stream.extend_from_slice(&known.to_le_bytes());
    stream.extend_from_slice(&packed[9..]);
    let mut out = Vec::new();
    lzma_rs::lzma_decompress(&mut std::io::BufReader::new(&stream[..]), &mut out)
        .map_err(|e| Error::corrupted(format!("LZMA data in a zip member: {e}")))?;
    Ok(out)
}

#[cfg(not(any(feature = "archive-xz", feature = "archive-7z")))]
fn lzma(_: &[u8], _: u64, _: u16) -> Result<Vec<u8>> {
    Err(codec::left_out("LZMA"))
}

/// What a member looks like in the output.
struct Out {
    name: Vec<u8>,
    method: u16,
    crc: u32,
    csize: u64,
    size: u64,
    offset: u64,
    mtime: Option<i64>,
    external: u32,
    /// An encrypted member copied as it is: its flags (bits 0–3), DOS time
    /// and date, and AES record.
    kept: Option<(u16, (u16, u16), Option<ZipAes>)>,
}

impl Out {
    fn flags(&self) -> u16 {
        0x0800 | self.kept.map_or(0, |k| k.0) // UTF-8 names
    }

    fn dos(&self) -> (u16, u16) {
        self.kept.map_or_else(|| dos_time(self.mtime.unwrap_or(0)), |k| k.1)
    }

    /// The WinZip AES extra field.
    fn aes_record(&self) -> Option<[u8; 11]> {
        let a = self.kept?.2?;
        let mut r = [0u8; 11];
        r[..2].copy_from_slice(&0x9901u16.to_le_bytes());
        r[2..4].copy_from_slice(&7u16.to_le_bytes());
        r[4..6].copy_from_slice(&a.version.to_le_bytes());
        r[6..8].copy_from_slice(b"AE");
        r[8] = a.strength;
        r[9..11].copy_from_slice(&a.method.to_le_bytes());
        Some(r)
    }
}

fn put16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn put64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

const MAX32: u64 = 0xFFFF_FFFF;

/// Serialises `tree`. Unchanged members are copied (`raw`); others come
/// from `content` and are compressed.
pub(super) fn write(
    tree: &Tree,
    raw: &mut Raw<'_>,
    content: &mut dyn FnMut(&str, &Node) -> Result<Vec<u8>>,
    comment: Option<&str>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut members = Vec::new();
    for (path, node) in tree.iter() {
        let rel = &path[1..];
        let (name, mode_type) = match node.kind {
            EntryKind::Directory => (format!("{rel}/"), 0o040000),
            EntryKind::Symlink => (rel.to_owned(), 0o120000),
            EntryKind::File => (rel.to_owned(), 0o100000),
            EntryKind::Other => continue,
        };
        let default_mode = if node.kind == EntryKind::Directory { 0o755 } else { 0o644 };
        let mode = mode_type | node.mode.unwrap_or(default_mode) & 0o7777;
        let external = (mode << 16) | (u32::from(node.kind == EntryKind::Directory) * 0x10);
        // An encrypted member is copied as it is (it stays encrypted):
        // its flags, time (ZipCrypto checks passwords against it) and AES
        // record come along.
        let mut kept = None;
        // The bytes as stored, and how.
        let (method, crc, size, packed) = match (&node.kind, &node.data) {
            (EntryKind::Directory, _) => (0, 0, 0, Vec::new()),
            (_, &Data::Zip { header, csize, size, method, crc, encrypted: true, flags, dos, aes }) => {
                kept = Some((flags & 0x0F, dos, aes));
                (method, crc, size, stored_bytes(raw, header, csize)?)
            }
            (_, Data::Zip { header, csize, size, method, crc, encrypted: false, .. }) if matches!(method, 0 | 8) => {
                (*method, *crc, *size, stored_bytes(raw, *header, *csize)?)
            }
            _ => {
                let data = match node.kind {
                    EntryKind::Symlink => node.link.clone().unwrap_or_default().into_bytes(),
                    _ => content(path, node)?,
                };
                let crc = dix_core::partition::crc32(&data);
                let packed = codec::deflate(&data);
                if packed.len() < data.len() {
                    (8, crc, data.len() as u64, packed)
                } else {
                    (0, crc, data.len() as u64, data)
                }
            }
        };
        let m = Out {
            name: name.into_bytes(),
            method,
            crc,
            csize: packed.len() as u64,
            size,
            offset: out.len() as u64,
            mtime: node.mtime,
            external,
            kept,
        };
        let (time, date) = m.dos();
        let big = m.size >= MAX32 || m.csize >= MAX32;
        let ts = m.mtime.filter(|t| (0..=i32::MAX as i64).contains(t));
        let aes = m.aes_record();
        put32(&mut out, LOCAL);
        put16(
            &mut out,
            if big {
                45
            } else if aes.is_some() {
                51
            } else {
                20
            },
        );
        put16(&mut out, m.flags());
        put16(&mut out, m.method);
        put16(&mut out, time);
        put16(&mut out, date);
        put32(&mut out, m.crc);
        put32(&mut out, if big { MAX32 as u32 } else { m.csize as u32 });
        put32(&mut out, if big { MAX32 as u32 } else { m.size as u32 });
        put16(&mut out, m.name.len() as u16);
        put16(
            &mut out,
            if big { 20 } else { 0 } + if ts.is_some() { 9 } else { 0 } + aes.map_or(0, |a| a.len() as u16),
        );
        out.extend_from_slice(&m.name);
        if big {
            put16(&mut out, 0x0001);
            put16(&mut out, 16);
            put64(&mut out, m.size);
            put64(&mut out, m.csize);
        }
        // The extended timestamp, in both headers like zip does.
        if let Some(t) = ts {
            put16(&mut out, 0x5455);
            put16(&mut out, 5);
            out.push(1);
            put32(&mut out, t as u32);
        }
        if let Some(a) = aes {
            out.extend_from_slice(&a);
        }
        out.extend_from_slice(&packed);
        // A kept member with a data descriptor (flag 3) keeps it.
        if m.flags() & 8 != 0 {
            put32(&mut out, 0x0807_4B50);
            put32(&mut out, m.crc);
            if big {
                put64(&mut out, m.csize);
                put64(&mut out, m.size);
            } else {
                put32(&mut out, m.csize as u32);
                put32(&mut out, m.size as u32);
            }
        }
        members.push(m);
    }
    let cd_start = out.len() as u64;
    for m in &members {
        let mut z64 = Vec::new();
        for v in [m.size, m.csize, m.offset] {
            if v >= MAX32 {
                put64(&mut z64, v);
            }
        }
        // The extended timestamp keeps the exact modification time.
        let ts = m.mtime.filter(|t| (0..=i32::MAX as i64).contains(t));
        put32(&mut out, CENTRAL);
        put16(&mut out, (3 << 8) | 30); // made by: Unix, zip 3.0
        let aes = m.aes_record();
        put16(
            &mut out,
            if !z64.is_empty() {
                45
            } else if aes.is_some() {
                51
            } else {
                20
            },
        );
        put16(&mut out, m.flags());
        put16(&mut out, m.method);
        let (time, date) = m.dos();
        put16(&mut out, time);
        put16(&mut out, date);
        put32(&mut out, m.crc);
        put32(&mut out, m.csize.min(MAX32) as u32);
        put32(&mut out, m.size.min(MAX32) as u32);
        put16(&mut out, m.name.len() as u16);
        let extra_len = if z64.is_empty() { 0 } else { 4 + z64.len() }
            + if ts.is_some() { 9 } else { 0 }
            + aes.map_or(0, |a| a.len());
        put16(&mut out, extra_len as u16);
        put16(&mut out, 0); // comment
        put16(&mut out, 0); // disk
        put16(&mut out, 0); // internal attributes
        put32(&mut out, m.external);
        put32(&mut out, m.offset.min(MAX32) as u32);
        out.extend_from_slice(&m.name);
        if !z64.is_empty() {
            put16(&mut out, 0x0001);
            put16(&mut out, z64.len() as u16);
            out.extend_from_slice(&z64);
        }
        if let Some(t) = ts {
            put16(&mut out, 0x5455);
            put16(&mut out, 5);
            out.push(1);
            put32(&mut out, t as u32);
        }
        if let Some(a) = aes {
            out.extend_from_slice(&a);
        }
    }
    let cd_size = out.len() as u64 - cd_start;
    let count = members.len() as u64;
    if count >= 0xFFFF || cd_start >= MAX32 || cd_size >= MAX32 {
        let at = out.len() as u64;
        put32(&mut out, EOCD64);
        put64(&mut out, 44);
        put16(&mut out, (3 << 8) | 45);
        put16(&mut out, 45);
        put32(&mut out, 0);
        put32(&mut out, 0);
        put64(&mut out, count);
        put64(&mut out, count);
        put64(&mut out, cd_size);
        put64(&mut out, cd_start);
        put32(&mut out, EOCD64_LOCATOR);
        put32(&mut out, 0);
        put64(&mut out, at);
        put32(&mut out, 1);
    }
    let comment = comment.unwrap_or("").as_bytes();
    let comment = &comment[..comment.len().min(0xFFFF)];
    put32(&mut out, EOCD);
    put16(&mut out, 0);
    put16(&mut out, 0);
    put16(&mut out, count.min(0xFFFF) as u16);
    put16(&mut out, count.min(0xFFFF) as u16);
    put32(&mut out, cd_size.min(MAX32) as u32);
    put32(&mut out, cd_start.min(MAX32) as u32);
    put16(&mut out, comment.len() as u16);
    out.extend_from_slice(comment);
    Ok(out)
}

/// Reads one member in pieces without unpacking all of it: a stored
/// member is read where it lies, a deflated one is inflated as far as the
/// reads go (about 100 KiB of state, whatever the member's size).
/// Sequential reads continue where the last one stopped; reading earlier
/// data starts the stream again. The CRC is checked when the end is
/// reached by reading from the start.
pub(super) struct MemberReader {
    pub path: String,
    start: u64,
    csize: u64,
    size: u64,
    crc: u32,
    deflated: bool,
    state: Box<miniz_oxide::inflate::stream::InflateState>,
    /// Compressed bytes read from the archive but not inflated yet.
    input: Vec<u8>,
    input_at: usize,
    /// Compressed bytes taken from the archive so far.
    in_pos: u64,
    /// Uncompressed bytes produced so far.
    out_pos: u64,
    running_crc: u32,
}

impl MemberReader {
    pub fn new(raw: &mut Raw<'_>, path: &str, data: &Data) -> Result<MemberReader> {
        let Data::Zip { header, csize, size, method, crc, encrypted, .. } = *data else {
            return Err(Error::invalid_input("not a zip member"));
        };
        if encrypted {
            return Err(Error::unsupported("reading encrypted zip members"));
        }
        let deflated = match method {
            0 => false,
            8 => true,
            // The other methods are read whole (`read_member`).
            m => return Err(Error::invalid_input(format!("zip method {m} is not streamed"))),
        };
        let start = data_start(raw, header)?;
        if start + csize > raw.size() {
            return Err(Error::corrupted("a zip member extends beyond the archive"));
        }
        Ok(MemberReader {
            path: path.to_owned(),
            start,
            csize,
            size,
            crc,
            deflated,
            state: miniz_oxide::inflate::stream::InflateState::new_boxed(miniz_oxide::DataFormat::Raw),
            input: Vec::new(),
            input_at: 0,
            in_pos: 0,
            out_pos: 0,
            running_crc: 0,
        })
    }

    fn restart(&mut self) {
        self.state = miniz_oxide::inflate::stream::InflateState::new_boxed(miniz_oxide::DataFormat::Raw);
        self.input.clear();
        self.input_at = 0;
        self.in_pos = 0;
        self.out_pos = 0;
        self.running_crc = 0;
    }

    /// Inflates the next bytes into `out`; how many (0 at the end).
    fn next(&mut self, raw: &mut Raw<'_>, out: &mut [u8]) -> Result<usize> {
        use miniz_oxide::{MZError, MZFlush, MZStatus};
        loop {
            if self.input_at == self.input.len() && self.in_pos < self.csize {
                let n = (self.csize - self.in_pos).min(64 << 10) as usize;
                self.input.resize(n, 0);
                raw.read_at(self.start + self.in_pos, &mut self.input)?;
                self.in_pos += n as u64;
                self.input_at = 0;
            }
            let exhausted = self.input_at == self.input.len() && self.in_pos == self.csize;
            let flush = if exhausted { MZFlush::Finish } else { MZFlush::None };
            let r = miniz_oxide::inflate::stream::inflate(&mut self.state, &self.input[self.input_at..], out, flush);
            self.input_at += r.bytes_consumed;
            match r.status {
                Ok(MZStatus::StreamEnd) => return Ok(r.bytes_written),
                // Progress, or (no output yet) more input to come.
                Ok(_) | Err(MZError::Buf) if r.bytes_written > 0 => return Ok(r.bytes_written),
                Ok(_) | Err(MZError::Buf) if !exhausted || r.bytes_consumed > 0 => {}
                Ok(_) | Err(MZError::Buf) => {
                    return Err(Error::corrupted(format!("{}: deflate data ends early", self.path)));
                }
                Err(e) => return Err(Error::corrupted(format!("{}: damaged deflate data ({e:?})", self.path))),
            }
        }
    }

    /// Reads at `offset` into `buf`; how many bytes (0 at the end).
    pub fn read_at(&mut self, raw: &mut Raw<'_>, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= self.size {
            return Ok(0);
        }
        let want = buf.len().min((self.size - offset) as usize);
        if !self.deflated {
            raw.read_at(self.start + offset, &mut buf[..want])?;
            if offset == 0 && want as u64 == self.size && dix_core::partition::crc32(&buf[..want]) != self.crc {
                return Err(Error::corrupted(format!("{}: checksum mismatch", self.path)));
            }
            return Ok(want);
        }
        if offset < self.out_pos {
            self.restart();
        }
        // Skip ahead to `offset`.
        let mut scratch = [0u8; 16 << 10];
        while self.out_pos < offset {
            let n = (offset - self.out_pos).min((scratch.len()) as u64) as usize;
            let got = self.next(raw, &mut scratch[..n])?;
            if got == 0 {
                return Err(Error::corrupted(format!("{}: deflate data ends early", self.path)));
            }
            self.produced(&scratch[..got])?;
        }
        let mut done = 0;
        while done < want {
            let got = self.next(raw, &mut buf[done..want])?;
            if got == 0 {
                return Err(Error::corrupted(format!("{}: deflate data ends early", self.path)));
            }
            let chunk = buf[done..done + got].to_vec();
            self.produced(&chunk)?;
            done += got;
        }
        Ok(done)
    }

    /// Accounts for bytes produced (CRC, position); checks the CRC at the end.
    fn produced(&mut self, bytes: &[u8]) -> Result<()> {
        self.running_crc = dix_core::partition::crc32_continue(self.running_crc, bytes);
        self.out_pos += bytes.len() as u64;
        if self.out_pos == self.size && self.running_crc != self.crc {
            return Err(Error::corrupted(format!("{}: checksum mismatch", self.path)));
        }
        Ok(())
    }
}
