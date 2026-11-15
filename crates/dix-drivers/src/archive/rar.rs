//! RAR archives, read-only.
//!
//! **RAR 5** (`Rar!\x1a\x07\x01\x00`, what WinRAR and rar have written by
//! default since 2013) is read fully: stored and compressed files at every
//! level, solid archives, the E8, E8E9, ARM and delta filters, directories,
//! times, Unix permissions and owners, symbolic links, hard links and file
//! copies. The decompressor is written for this project from the
//! documented behaviour of the format (as libarchive and 7-Zip do; the
//! unRAR licence only concerns making RAR compressors).
//!
//! **RAR 4** (`Rar!\x1a\x07\x00`): stored files, and RAR 2.9
//! compression (LZ, PPMd, the standard filters; see `v3`). RAR 1.5 and 2.0
//! compression are listed and reported.
//!
//! **Volumes**: the parts joined one after another (see
//! `dix_core::device::JoinedDevice`) are read as one archive; a file split
//! over volumes has a header in each, its data continuing from one to the
//! next ([`Entry::segs`]).
//!
//! **Passwords** (RAR 5, `archive-aes`): encrypted files (an encryption
//! record: AES-256-CBC, a PBKDF2-HMAC-SHA256 key, a password check value,
//! CRCs tweaked with the password) and encrypted headers (`-hp`: every
//! header after the archive encryption header is a 16-byte IV and AES
//! blocks), decrypted as they are read (see `crypto`).
//!
//! RAR 4 (RAR 3.x) passwords too: AES-128 (see `crypto::keys_rar3`) for
//! stored files, and encrypted headers (each an 8-byte salt and AES
//! blocks); the CRCs tell a right password from a wrong one.
//!
//! RAR 1.5/2.0 compressed files, RAR 2 encryption and files continued in a
//! volume that is missing are listed and say why they cannot be read.
//!
//! A compressed member is decompressed as it is read (only the dictionary
//! window is held in memory, not the member). In a solid archive a member
//! depends on those before it: reading continues from where the stream
//! is, and starts again from the beginning of the solid run only when an
//! earlier member is asked for.

use super::{Data, Node, Raw, Tree, clean_path};

/// RAR 2.9 decompression (RAR 3 and 4 archives).
#[path = "rar3.rs"]
pub(crate) mod v3;
use dix_core::{EntryKind, Error, ErrorKind, MountLog, Result};
use std::collections::VecDeque;
#[cfg(test)]
pub(crate) use v3 as v3_for_tests;

pub(super) const SIG5: &[u8] = b"Rar!\x1a\x07\x01\x00";
pub(super) const SIG4: &[u8] = b"Rar!\x1a\x07\x00";

/// A member with data, as the decompressor needs it.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub name: String,
    /// Where its packed data is: (offset, length) in each volume it is in.
    pub segs: Vec<(u64, u64)>,
    pub size: u64,
    /// 0: stored.
    pub method: u8,
    /// Continues the previous compressed member's state.
    pub solid: bool,
    pub dict: u64,
    /// The RAR 7 extended format (more distance codes).
    pub extended: bool,
    pub crc: Option<u32>,
    /// AES encryption (with a password).
    pub enc: Option<Enc>,
    /// RAR 4 AES encryption: the salt.
    pub enc4: Option<[u8; 8]>,
    /// RAR 4: the version of the compression (29: RAR 2.9; 20, 26: RAR 2;
    /// 15: RAR 1.5); 0 for RAR 5.
    pub unp_ver: u8,
    /// Why it cannot be read (encrypted, a volume missing, RAR 4 compression).
    pub problem: Option<String>,
}

/// A RAR 5 encryption record: AES-256 with a key from the password, the
/// salt and 2^`count` PBKDF2 rounds.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(feature = "archive-aes"), allow(dead_code))]
pub(crate) struct Enc {
    salt: [u8; 16],
    iv: [u8; 16],
    count: u8,
    /// Tells a wrong password (8 bytes derived from it).
    check: Option<[u8; 8]>,
    /// The CRC is "tweaked" with the password.
    mac: bool,
}

/// What decrypting one member takes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Key {
    /// The AES key: 32 bytes (RAR 5) or the first 16 (RAR 4).
    aes: [u8; 32],
    len: usize,
    iv: [u8; 16],
    /// For tweaked CRCs.
    hash: Option<[u8; 32]>,
}

/// An encryption record's body: (salt, iv?, count, check) as the file and
/// archive encryption records hold them (`iv`: files only).
/// (version, flags, rounds, salt, IV, check value)
type EncRecord = (u64, u64, u8, [u8; 16], [u8; 16], Option<[u8; 8]>);

fn enc_record(b: &[u8], at: &mut usize, with_iv: bool) -> Result<EncRecord> {
    let version = vint(b, at)?;
    let flags = vint(b, at)?;
    let count = *b.get(*at).ok_or_else(|| bad("an encryption record"))?;
    *at += 1;
    let take16 = |at: &mut usize| -> Result<[u8; 16]> {
        let v = b.get(*at..*at + 16).ok_or_else(|| bad("an encryption record"))?;
        *at += 16;
        Ok(v.try_into().unwrap())
    };
    let salt = take16(at)?;
    let iv = if with_iv { take16(at)? } else { [0; 16] };
    let check = if flags & 1 != 0 {
        let v = b.get(*at..*at + 8).ok_or_else(|| bad("an encryption record"))?;
        *at += 12; // the check value and 4 bytes of its checksum
        Some(v.try_into().unwrap())
    } else {
        None
    };
    Ok((version, flags, count, salt, iv, check))
}

/// The keys of the first password whose check value matches.
#[cfg(feature = "archive-aes")]
fn find_keys(
    passwords: &[String],
    salt: &[u8; 16],
    count: u8,
    check: Option<[u8; 8]>,
) -> Option<super::crypto::RarKeys> {
    passwords
        .iter()
        .map(|p| super::crypto::keys_rar5(p, salt, count as u32))
        .find(|k| check.is_none_or(|c| c == k.check))
}

fn password_needed(what: &str) -> Error {
    Error::new(ErrorKind::PasswordRequired, what.to_owned())
}

impl Entry {
    fn packed(&self) -> u64 {
        self.segs.iter().map(|s| s.1).sum()
    }
}

/// Reads `buf` from offset `off` of data held in pieces (`segs`).
fn read_segs(raw: &mut Raw<'_>, segs: &[(u64, u64)], mut off: u64, mut buf: &mut [u8]) -> Result<()> {
    for &(at, len) in segs {
        if buf.is_empty() {
            break;
        }
        if off >= len {
            off -= len;
            continue;
        }
        let n = buf.len().min((len - off) as usize);
        raw.read_at(at + off, &mut buf[..n])?;
        buf = &mut buf[n..];
        off = 0;
    }
    if buf.is_empty() { Ok(()) } else { Err(bad("a member's data ends early")) }
}

/// Where the next volume starts, after the end of one at `at` (the parts
/// are joined one after another; a volume may have padding at its end):
/// the next part's start, or the signature found a little further on.
fn next_volume(raw: &mut Raw<'_>, at: u64, sig: &[u8]) -> Result<Option<u64>> {
    let size = raw.size();
    let next = match raw.part_starts() {
        Some(starts) => starts.into_iter().find(|&s| s >= at && s < size),
        None => {
            let mut b = vec![0u8; (size.saturating_sub(at)).min(64 << 10) as usize];
            raw.read_at(at, &mut b)?;
            b.windows(sig.len()).position(|w| w == sig).map(|i| at + i as u64)
        }
    };
    let Some(next) = next else { return Ok(None) };
    let mut b = vec![0u8; sig.len()];
    if next + sig.len() as u64 > size {
        return Ok(None);
    }
    raw.read_at(next, &mut b)?;
    Ok((b == sig).then_some(next + sig.len() as u64))
}

/// Volumes: a member split over several continues in the next one with a
/// header of its own.
#[derive(Default)]
struct Split {
    /// The member whose data goes on in the next volume.
    open: Option<usize>,
}

impl Split {
    /// A file header's part of a member: `Some(index)` when it continues
    /// an earlier one (then it is not a new member).
    fn continues(
        &mut self,
        arc: &mut Archive,
        path: &str,
        before: bool,
        after: bool,
        seg: (u64, u64),
        crc: Option<u32>,
    ) -> Option<usize> {
        if !before {
            return None;
        }
        let i = self.open.take().filter(|&i| arc.entries[i].name == path)?;
        let e = &mut arc.entries[i];
        e.segs.push(seg);
        if after {
            self.open = Some(i);
        } else {
            // The last part's checksum is the whole file's.
            e.crc = crc;
        }
        Some(i)
    }

    /// The end: a member still open goes on in a volume that is not here.
    fn finish(self, arc: &mut Archive, tree: &mut Tree) {
        if let Some(i) = self.open {
            let e = &mut arc.entries[i];
            if e.problem.is_none() {
                e.problem = Some("it continues in the next volume, which is missing".to_owned());
            }
            let path = e.name.clone();
            if let Some(n) = tree.nodes.get_mut(&path) {
                n.data = Data::Rar { index: i };
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Archive {
    pub entries: Vec<Entry>,
    /// The decompression stream in use.
    stream: Option<Stream>,
    /// Passwords for encrypted members.
    pub passwords: Vec<String>,
    /// RAR 4 keys found for members (by index): RAR 4 has no password
    /// check, a key is known right by the member's CRC.
    keys4: std::collections::HashMap<usize, Key>,
}

impl Archive {
    /// The packed size of member `index` if it is encrypted.
    pub fn encrypted_size(&self, index: usize) -> Option<u64> {
        let e = self.entries.get(index)?;
        (e.enc.is_some() || e.enc4.is_some()).then(|| e.packed())
    }

    /// The key of a member for the decompression stream: RAR 5's from its
    /// check value; a compressed RAR 4 member's from a password that opened
    /// it before, or the first (its CRC tells at the end).
    fn stream_key(&self, index: usize) -> Result<Option<Key>> {
        let e = &self.entries[index];
        let Some(salt) = e.enc4 else { return self.key(e) };
        if let Some(k) = self.keys4.get(&index) {
            return Ok(Some(*k));
        }
        let p = self.passwords.first().ok_or_else(|| password_needed("it is encrypted: a password is needed"))?;
        #[cfg(feature = "archive-aes")]
        {
            let (key, iv) = super::crypto::keys_rar3(p, &salt);
            let mut aes = [0u8; 32];
            aes[..16].copy_from_slice(&key);
            Ok(Some(Key { aes, len: 16, iv, hash: None }))
        }
        #[cfg(not(feature = "archive-aes"))]
        {
            let _ = (p, salt);
            Err(super::codec::left_out("AES encryption"))
        }
    }

    /// The key of RAR 4 encrypted member `index`: the password whose key
    /// decrypts it to data with the right CRC (a stored member).
    fn key4(&mut self, raw: &mut Raw<'_>, index: usize) -> Result<Key> {
        if let Some(k) = self.keys4.get(&index) {
            return Ok(*k);
        }
        let e = self.entries.get(index).ok_or_else(|| bad("a member is missing"))?.clone();
        let salt = e.enc4.ok_or_else(|| bad("not encrypted"))?;
        if self.passwords.is_empty() {
            return Err(password_needed("it is encrypted: a password is needed"));
        }
        #[cfg(feature = "archive-aes")]
        for p in &self.passwords {
            let (key, iv) = super::crypto::keys_rar3(p, &salt);
            let mut aes = [0u8; 32];
            aes[..16].copy_from_slice(&key);
            let k = Key { aes, len: 16, iv, hash: None };
            if e.method != 0 {
                // Compressed: decompress it with this password alone; the
                // CRC at the end decides.
                let mut one = Archive { entries: self.entries.clone(), ..Default::default() };
                one.passwords = vec![p.clone()];
                one.keys4.insert(index, k);
                let ok = Stream::start(&one, index, raw).and_then(|mut s| {
                    let mut buf = vec![0u8; 1 << 16];
                    let mut at = 0u64;
                    loop {
                        let n = s.read(raw, at, &mut buf)?;
                        if n == 0 {
                            return Ok(());
                        }
                        at += n as u64;
                    }
                });
                if ok.is_ok() {
                    self.keys4.insert(index, k);
                    return Ok(k);
                }
                continue;
            }
            let mut data = vec![0u8; e.size as usize];
            read_encrypted(raw, &e, &k, 0, &mut data)?;
            if e.crc.is_none_or(|c| dix_core::partition::crc32(&data) == c) {
                self.keys4.insert(index, k);
                return Ok(k);
            }
        }
        #[cfg(not(feature = "archive-aes"))]
        {
            let _ = (raw, salt);
            return Err(super::codec::left_out("AES encryption"));
        }
        #[allow(unreachable_code)]
        Err(password_needed("it is encrypted, and no password given opens it"))
    }

    /// The key for an encrypted member (`None`: not encrypted).
    fn key(&self, e: &Entry) -> Result<Option<Key>> {
        let Some(enc) = e.enc else { return Ok(None) };
        #[cfg(feature = "archive-aes")]
        {
            if self.passwords.is_empty() {
                return Err(password_needed("it is encrypted: a password is needed"));
            }
            let k = find_keys(&self.passwords, &enc.salt, enc.count, enc.check)
                .ok_or_else(|| password_needed("it is encrypted, and no password given opens it"))?;
            Ok(Some(Key { aes: k.key, len: 32, iv: enc.iv, hash: enc.mac.then_some(k.hash_key) }))
        }
        #[cfg(not(feature = "archive-aes"))]
        {
            let _ = enc;
            Err(super::codec::left_out("AES encryption"))
        }
    }
}

/// Whether `password` opens encrypted member `index` (by its check value).
pub(super) fn check_password(arc: &mut Archive, _raw: &mut Raw<'_>, index: usize, password: &str) -> Result<()> {
    let e = arc.entries.get(index).ok_or_else(|| bad("a member is missing"))?.clone();
    let mut one = Archive { entries: arc.entries.clone(), passwords: vec![password.to_owned()], ..Default::default() };
    if e.enc4.is_some() {
        return one.key4(_raw, index).map(|_| ());
    }
    one.key(&e).map(|_| ())
}

fn bad(what: &str) -> Error {
    Error::corrupted(format!("a damaged RAR archive ({what})"))
}

/// A RAR 5 variable-length integer.
fn vint(b: &[u8], at: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let c = *b.get(*at).ok_or_else(|| bad("a header ends early"))?;
        *at += 1;
        v |= ((c & 0x7F) as u64) << (7 * i);
        if c & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(bad("a number is too long"))
}

fn u32le(b: &[u8], at: &mut usize) -> Result<u32> {
    let v = b.get(*at..*at + 4).ok_or_else(|| bad("a header ends early"))?;
    *at += 4;
    Ok(u32::from_le_bytes(v.try_into().unwrap()))
}

/// Reads the member list.
/// Reads the member list (`passwords`: for encrypted headers).
pub(super) fn parse(raw: &mut Raw<'_>, log: &MountLog, passwords: &[String]) -> Result<(Tree, Archive)> {
    let mut head = [0u8; 8];
    raw.read_at(0, &mut head[..raw.size().min(8) as usize])?;
    if head.starts_with(SIG5) {
        parse5(raw, log, passwords)
    } else if head.starts_with(SIG4) {
        parse4(raw, log, passwords)
    } else {
        Err(bad("no signature"))
    }
}

fn windows_time(t: u64) -> i64 {
    (t / 10_000_000) as i64 - 11_644_473_600
}

/// A RAR 5 header at `at`: its bytes from the size on (`q` starts after
/// the size), and where its data starts. With `key`, headers are
/// encrypted: a 16-byte IV, then the header in whole AES blocks.
fn header5(raw: &mut Raw<'_>, at: u64, key: Option<&[u8; 32]>) -> Result<(Vec<u8>, usize, u64)> {
    let size = raw.size();
    let Some(_key) = key else {
        let mut pre = [0u8; 14];
        let n = (size - at).min(14) as usize;
        raw.read_at(at, &mut pre[..n])?;
        let crc = u32::from_le_bytes(pre[..4].try_into().unwrap());
        let mut p = 4;
        let hsize = vint(&pre[..n], &mut p)?;
        if hsize > 2 << 20 || at + p as u64 + hsize > size {
            return Err(bad("a header runs past the end"));
        }
        let mut h = vec![0u8; p - 4 + hsize as usize];
        raw.read_at(at + 4, &mut h)?;
        if dix_core::partition::crc32(&h) != crc {
            return Err(bad("a header checksum does not match"));
        }
        let data_at = at + 4 + h.len() as u64;
        return Ok((h, p - 4, data_at));
    };
    #[cfg(feature = "archive-aes")]
    {
        if at + 32 > size {
            return Err(bad("a header runs past the end"));
        }
        let mut iv = [0u8; 16];
        raw.read_at(at, &mut iv)?;
        let mut first = [0u8; 16];
        raw.read_at(at + 16, &mut first)?;
        super::crypto::aes256_cbc_decrypt(_key, &mut iv.clone(), &mut first)?;
        let mut p = 4;
        let hsize = vint(&first, &mut p)?;
        let total = p as u64 + hsize;
        let padded = total.next_multiple_of(16);
        if hsize > 2 << 20 || at + 16 + padded > size {
            // A wrong key gives a nonsense size.
            return Err(bad("a header runs past the end"));
        }
        let mut plain = vec![0u8; padded as usize];
        raw.read_at(at + 16, &mut plain)?;
        super::crypto::aes256_cbc_decrypt(_key, &mut iv, &mut plain)?;
        let crc = u32::from_le_bytes(plain[..4].try_into().unwrap());
        let h = plain[4..total as usize].to_vec();
        if dix_core::partition::crc32(&h) != crc {
            return Err(bad("a header checksum does not match"));
        }
        Ok((h, p - 4, at + 16 + padded))
    }
    #[cfg(not(feature = "archive-aes"))]
    Err(super::codec::left_out("AES encryption"))
}

fn parse5(raw: &mut Raw<'_>, log: &MountLog, passwords: &[String]) -> Result<(Tree, Archive)> {
    let size = raw.size();
    let mut tree = Tree::new();
    let mut arc = Archive::default();
    let mut at = SIG5.len() as u64;
    // Hard links and copies: resolved once all members are known.
    let mut links: Vec<(String, String)> = Vec::new();
    let mut split = Split::default();
    let mut volumes = 1;
    // The key of encrypted headers (after an archive encryption header).
    let mut hkey: Option<[u8; 32]> = None;
    while at + 7 <= size {
        let (h, mut q, data_at) = header5(raw, at, hkey.as_ref())?;
        let kind = vint(&h, &mut q)?;
        let flags = vint(&h, &mut q)?;
        let extra = if flags & 1 != 0 { vint(&h, &mut q)? } else { 0 };
        let data = if flags & 2 != 0 { vint(&h, &mut q)? } else { 0 };
        let extra_at = h.len().checked_sub(extra as usize).ok_or_else(|| bad("an extra area is too large"))?;
        match kind {
            1 => {}
            2 => {
                let fflags = vint(&h, &mut q)?;
                let unpacked = vint(&h, &mut q)?;
                let attrs = vint(&h, &mut q)?;
                let mut mtime = if fflags & 2 != 0 { Some(u32le(&h, &mut q)? as i64) } else { None };
                let crc = if fflags & 4 != 0 { Some(u32le(&h, &mut q)?) } else { None };
                let ci = vint(&h, &mut q)?;
                let host = vint(&h, &mut q)?;
                let nlen = vint(&h, &mut q)? as usize;
                let name =
                    String::from_utf8_lossy(h.get(q..q + nlen).ok_or_else(|| bad("a name runs past its header"))?)
                        .into_owned();
                let mut node = Node::new(if fflags & 1 != 0 { EntryKind::Directory } else { EntryKind::File });
                let mut problem = None;
                let mut enc = None;
                let mut atime = None;
                let mut redirect: Option<(u64, String)> = None;
                // Extra records.
                let mut e = extra_at;
                while e < h.len() {
                    let rsize = vint(&h, &mut e)? as usize;
                    let rstart = e;
                    let rtype = vint(&h, &mut e)?;
                    let rend = rstart + rsize;
                    if rend > h.len() {
                        break;
                    }
                    let r = &h[..rend];
                    match rtype {
                        1 => {
                            let (version, flags, count, salt, iv, check) = enc_record(r, &mut e, true)?;
                            if version != 0 {
                                problem = Some(format!("it uses an unknown RAR encryption ({version})"));
                            }
                            enc = Some(Enc { salt, iv, count, check, mac: flags & 2 != 0 });
                        }
                        3 => {
                            let tf = vint(r, &mut e)?;
                            let t = |e: &mut usize| -> Result<i64> {
                                if tf & 1 != 0 {
                                    Ok(u32le(r, e)? as i64)
                                } else {
                                    let lo = u32le(r, e)? as u64;
                                    let hi = u32le(r, e)? as u64;
                                    Ok(windows_time(hi << 32 | lo))
                                }
                            };
                            if tf & 2 != 0 {
                                mtime = Some(t(&mut e)?);
                            }
                            if tf & 4 != 0 {
                                t(&mut e)?; // creation
                            }
                            if tf & 8 != 0 {
                                atime = Some(t(&mut e)?);
                            }
                        }
                        5 => {
                            let rt = vint(r, &mut e)?;
                            let _rf = vint(r, &mut e)?;
                            let l = vint(r, &mut e)? as usize;
                            let target = String::from_utf8_lossy(r.get(e..e + l).unwrap_or(&[])).into_owned();
                            redirect = Some((rt, target));
                        }
                        6 => {
                            let of = vint(r, &mut e)?;
                            for bit in [1u64, 2] {
                                if of & bit != 0 {
                                    let l = vint(r, &mut e)? as usize;
                                    e += l;
                                }
                            }
                            if of & 4 != 0 {
                                node.uid = Some(vint(r, &mut e)? as u32);
                            }
                            if of & 8 != 0 {
                                node.gid = Some(vint(r, &mut e)? as u32);
                            }
                        }
                        _ => {}
                    }
                    e = rend;
                }
                let _ = atime;
                if flags & 0x08 != 0 {
                    problem = Some("it starts in the previous volume, which is missing".to_owned());
                }
                let version = ci & 0x3F;
                let method = ((ci >> 7) & 7) as u8;
                if method != 0 && version > 1 && problem.is_none() {
                    problem = Some(format!("it uses an unknown RAR compression version ({version})"));
                }
                let base = 128u64 << 10 << ((ci >> 10) & 0x1F).min(30);
                let dict = if version == 1 { base + ((ci >> 15) & 0x1F) * base / 32 } else { base };
                node.mtime = mtime;
                if host == 1 {
                    node.mode = Some(attrs as u32 & 0o7777);
                    match attrs as u32 & 0o170000 {
                        0o120000 if redirect.is_none() => node.kind = EntryKind::Symlink,
                        0o040000 => node.kind = EntryKind::Directory,
                        _ => {}
                    }
                } else if attrs & 0x10 != 0 {
                    node.kind = EntryKind::Directory;
                }
                let Some(path) = clean_path(&name.replace('\\', "/")) else {
                    log.warn(format!("Skipped a member with an unusable name: {name:?}"));
                    at = data_at + data;
                    continue;
                };
                let after = flags & 0x10 != 0;
                if split.continues(&mut arc, &path, flags & 0x08 != 0, after, (data_at, data), crc).is_some() {
                    at = data_at + data;
                    continue;
                }
                match redirect {
                    Some((1..=3, target)) => {
                        node.kind = EntryKind::Symlink;
                        node.size = target.len() as u64;
                        node.link = Some(target);
                    }
                    Some((4 | 5, target)) => links.push((path.clone(), target)),
                    _ => {}
                }
                if node.kind == EntryKind::File || (node.kind == EntryKind::Symlink && node.link.is_none()) {
                    node.size = unpacked;
                    let index = arc.entries.len();
                    arc.entries.push(Entry {
                        name: path.clone(),
                        segs: vec![(data_at, data)],
                        size: unpacked,
                        method,
                        solid: ci & 0x40 != 0,
                        dict,
                        extended: version == 1,
                        crc,
                        enc,
                        enc4: None,
                        unp_ver: 0,
                        problem,
                    });
                    if after {
                        split.open = Some(index);
                    }
                    node.data = if method == 0 && arc.entries[index].problem.is_none() && !after && enc.is_none() {
                        Data::Range { offset: data_at, len: data.min(unpacked) }
                    } else {
                        Data::Rar { index }
                    };
                }
                if node.kind == EntryKind::Directory {
                    tree.update_dir(&path, node);
                } else {
                    tree.insert(&path, node);
                }
            }
            4 => {
                // The headers after this one are encrypted.
                let (version, _, count, salt, _, check) = enc_record(&h, &mut q, false)?;
                if version != 0 {
                    return Err(Error::unsupported(format!("RAR encryption version {version}")));
                }
                if passwords.is_empty() {
                    return Err(password_needed("the list of files is encrypted: a password is needed"));
                }
                #[cfg(feature = "archive-aes")]
                {
                    let k = find_keys(passwords, &salt, count, check).ok_or_else(|| {
                        password_needed("the list of files is encrypted, and no password given opens it")
                    })?;
                    hkey = Some(k.key);
                }
                #[cfg(not(feature = "archive-aes"))]
                {
                    let _ = (count, salt, check);
                    return Err(super::codec::left_out("AES encryption"));
                }
            }
            5 => {
                // Not the last volume: the next one follows (the parts
                // are joined).
                let more = vint(&h, &mut q)? & 1 != 0;
                let next = if more { next_volume(raw, data_at + data, SIG5)? } else { None };
                let Some(next) = next else {
                    if more {
                        log.warn(format!(
                            "This archive goes on in volume {}, which is missing (the parts must be next to each other, with their names).",
                            volumes + 1
                        ));
                    }
                    break;
                };
                at = next;
                volumes += 1;
                // (Each volume starts with its own encryption header.)
                hkey = None;
                continue;
            }
            _ => {}
        }
        at = data_at + data;
    }
    if volumes > 1 {
        log.info(format!("A RAR archive in {volumes} volumes."));
    }
    split.finish(&mut arc, &mut tree);
    arc.passwords = passwords.to_vec();
    for (path, target) in links {
        let src = clean_path(&target.replace('\\', "/")).and_then(|t| tree.get(&t).cloned());
        if let (Some(src), Some(n)) = (src, tree.nodes.get_mut(&path)) {
            n.data = src.data;
            n.size = src.size;
        }
    }
    resolve_symlink_targets(raw, &mut tree, &mut arc)?;
    Ok((tree, arc))
}

/// Symbolic links stored as data (RAR 4, and RAR 5 from some hosts): their
/// target is their content.
fn resolve_symlink_targets(raw: &mut Raw<'_>, tree: &mut Tree, arc: &mut Archive) -> Result<()> {
    let pending: Vec<(String, Data)> = tree
        .iter()
        .filter(|(_, n)| n.kind == EntryKind::Symlink && n.link.is_none())
        .map(|(p, n)| (p.to_owned(), n.data.clone()))
        .collect();
    for (p, data) in pending {
        let target = match data {
            Data::Range { offset, len } if len < 4096 => {
                let mut b = vec![0u8; len as usize];
                raw.read_at(offset, &mut b)?;
                Some(String::from_utf8_lossy(&b).into_owned())
            }
            Data::Rar { index } if arc.entries[index].size < 4096 => {
                let mut b = vec![0u8; arc.entries[index].size as usize];
                read(arc, raw, index, 0, &mut b).ok().map(|n| String::from_utf8_lossy(&b[..n]).into_owned())
            }
            _ => None,
        };
        if let Some(n) = tree.nodes.get_mut(&p) {
            n.size = target.as_ref().map_or(0, |t| t.len() as u64);
            n.link = target;
            n.data = Data::Empty;
        }
    }
    Ok(())
}

// ----- RAR 4 headers ------------------------------------------------------------

/// A RAR 4 Unicode name: the ASCII form, then a compact encoding of the
/// UTF-16 form.
fn rar4_unicode(name: &[u8], enc: &[u8]) -> String {
    let mut out: Vec<u16> = Vec::new();
    let mut ep = 0;
    let high = enc.first().copied().unwrap_or(0) as u16;
    ep += 1;
    let (mut flags, mut bits) = (0u8, 0u32);
    let max = 1024;
    while ep < enc.len() && out.len() < max {
        if bits == 0 {
            flags = enc[ep];
            ep += 1;
            bits = 8;
        }
        match flags >> 6 {
            0 => {
                let Some(&c) = enc.get(ep) else { break };
                out.push(c as u16);
                ep += 1;
            }
            1 => {
                let Some(&c) = enc.get(ep) else { break };
                out.push(c as u16 + (high << 8));
                ep += 1;
            }
            2 => {
                let (Some(&a), Some(&b)) = (enc.get(ep), enc.get(ep + 1)) else { break };
                out.push(a as u16 + ((b as u16) << 8));
                ep += 2;
            }
            _ => {
                let Some(&l) = enc.get(ep) else { break };
                ep += 1;
                if l & 0x80 != 0 {
                    let Some(&corr) = enc.get(ep) else { break };
                    ep += 1;
                    for _ in 0..(l & 0x7F) + 2 {
                        let c = name.get(out.len()).copied().unwrap_or(0);
                        out.push((c.wrapping_add(corr)) as u16 + (high << 8));
                    }
                } else {
                    for _ in 0..l as usize + 2 {
                        out.push(name.get(out.len()).copied().unwrap_or(0) as u16);
                    }
                }
            }
        }
        flags <<= 2;
        bits -= 2;
    }
    String::from_utf16_lossy(&out)
}

/// A RAR 4 header at `at` and where what follows it starts. With
/// `encrypted`, headers are an 8-byte salt and the header in AES-128
/// blocks; the password is the one whose key gives a header with the
/// right CRC (`good`: the one that worked last, tried first).
fn header4(
    raw: &mut Raw<'_>,
    at: u64,
    encrypted: bool,
    passwords: &[String],
    good: &mut usize,
) -> Result<(Vec<u8>, u64)> {
    let size = raw.size();
    // (Plain end headers are not checked: some writers got their CRC wrong;
    // a decrypted one must match, or a wrong key could pass as the end.)
    let crc_ok = |h: &[u8]| {
        let hcrc = u16::from_le_bytes([h[0], h[1]]);
        (h[2] == 0x7B && !encrypted) || (dix_core::partition::crc32(&h[2..]) & 0xFFFF) as u16 == hcrc
    };
    if !encrypted {
        let mut b = [0u8; 7];
        raw.read_at(at, &mut b)?;
        let hsize = u16::from_le_bytes([b[5], b[6]]) as u64;
        if hsize < 7 || at + hsize > size {
            return Err(bad("a header runs past the end"));
        }
        let mut h = vec![0u8; hsize as usize];
        raw.read_at(at, &mut h)?;
        if !crc_ok(&h) {
            return Err(bad("a header checksum does not match"));
        }
        return Ok((h, at + hsize));
    }
    if passwords.is_empty() {
        return Err(password_needed("the list of files is encrypted: a password is needed"));
    }
    #[cfg(feature = "archive-aes")]
    {
        if at + 8 + 16 > size {
            return Err(bad("a header runs past the end"));
        }
        let mut salt = [0u8; 8];
        raw.read_at(at, &mut salt)?;
        let order = std::iter::once(*good).chain((0..passwords.len()).filter(|&i| i != *good));
        for i in order.filter(|&i| i < passwords.len()) {
            let (key, iv0) = super::crypto::keys_rar3(&passwords[i], &salt);
            let mut first = [0u8; 16];
            raw.read_at(at + 8, &mut first)?;
            super::crypto::aes_cbc_decrypt(&key, &mut iv0.clone(), &mut first)?;
            let hsize = u16::from_le_bytes([first[5], first[6]]) as u64;
            let padded = hsize.next_multiple_of(16);
            if hsize < 7 || at + 8 + padded > size {
                continue; // a wrong key gives a nonsense size
            }
            let mut h = vec![0u8; padded as usize];
            raw.read_at(at + 8, &mut h)?;
            super::crypto::aes_cbc_decrypt(&key, &mut iv0.clone(), &mut h)?;
            h.truncate(hsize as usize);
            if crc_ok(&h) {
                *good = i;
                return Ok((h, at + 8 + padded));
            }
        }
        Err(password_needed("the list of files is encrypted, and no password given opens it"))
    }
    #[cfg(not(feature = "archive-aes"))]
    {
        let _ = good;
        Err(super::codec::left_out("AES encryption"))
    }
}

fn parse4(raw: &mut Raw<'_>, log: &MountLog, passwords: &[String]) -> Result<(Tree, Archive)> {
    let size = raw.size();
    let mut tree = Tree::new();
    let mut arc = Archive { passwords: passwords.to_vec(), ..Default::default() };
    let mut at = SIG4.len() as u64;
    let mut solid_archive = false;
    let mut split = Split::default();
    let mut volumes = 1;
    // Headers after the main header are encrypted (`-hp`).
    let mut encrypted = false;
    let mut good = 0;
    while at + 7 <= size {
        let mut b = [0u8; 7];
        raw.read_at(at, &mut b)?;
        if b == SIG4 {
            // The next volume (old ones have no end header).
            at += 7;
            volumes += 1;
            encrypted = false;
            continue;
        }
        let (h, hend) = header4(raw, at, encrypted, passwords, &mut good)?;
        let (kind, flags) = (h[2], u16::from_le_bytes([h[3], h[4]]));
        let mut add = if flags & 0x8000 != 0 && h.len() >= 11 {
            u32::from_le_bytes(h[7..11].try_into().unwrap()) as u64
        } else {
            0
        };
        match kind {
            0x73 => {
                solid_archive = flags & 0x08 != 0;
                encrypted = flags & 0x80 != 0;
            }
            0x74 if h.len() >= 32 => {
                let le32 = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
                let (mut packed, mut unpacked) = (le32(7) as u64, le32(11) as u64);
                let (host, crc, ftime, unp_ver, method, nsize, attrs) =
                    (h[15], le32(16), le32(20), h[24], h[25], u16::from_le_bytes([h[26], h[27]]) as usize, le32(28));
                let mut p = 32;
                if flags & 0x100 != 0 && h.len() >= 40 {
                    packed |= (le32(32) as u64) << 32;
                    unpacked |= (le32(36) as u64) << 32;
                    p = 40;
                }
                add = packed;
                let raw_name = h.get(p..p + nsize).unwrap_or(&[]);
                let name = if flags & 0x200 != 0 {
                    match raw_name.iter().position(|&c| c == 0) {
                        Some(z) => rar4_unicode(&raw_name[..z], &raw_name[z + 1..]),
                        None => String::from_utf8_lossy(raw_name).into_owned(),
                    }
                } else {
                    raw_name.iter().map(|&c| c as char).collect()
                };
                let dir = flags & 0xE0 == 0xE0;
                let mut node = Node::new(if dir { EntryKind::Directory } else { EntryKind::File });
                node.mtime = dix_core::time::from_dos((ftime >> 16) as u16, ftime as u16);
                if host == 3 {
                    node.mode = Some(attrs & 0o7777);
                    if attrs & 0o170000 == 0o120000 {
                        node.kind = EntryKind::Symlink;
                    }
                } else if attrs & 0x10 != 0 {
                    node.kind = EntryKind::Directory;
                }
                // An encrypted file's salt follows its name.
                let salt: Option<[u8; 8]> = (flags & 0x400 != 0)
                    .then(|| h.get(p + nsize..p + nsize + 8).map(|s| s.try_into().unwrap()))
                    .flatten();
                let mut problem = None;
                let enc4 = (flags & 0x04 != 0).then_some(salt.unwrap_or([0; 8]));
                if flags & 0x04 != 0 && (unp_ver < 29 || salt.is_none()) {
                    problem = Some("it is encrypted with an old RAR 2 cipher, which is not supported".to_owned());
                } else if flags & 0x01 != 0 {
                    problem = Some("it starts in the previous volume, which is missing".to_owned());
                } else if method != 0x30 && !matches!(unp_ver, 29 | 36) {
                    problem =
                        Some(format!("it uses an old RAR compression (version {unp_ver}), which is not supported"));
                }
                let Some(path) = clean_path(&name.replace('\\', "/")) else {
                    log.warn(format!("Skipped a member with an unusable name: {name:?}"));
                    at = hend + add;
                    continue;
                };
                let data_at = hend;
                let after = flags & 0x02 != 0;
                if split.continues(&mut arc, &path, flags & 0x01 != 0, after, (data_at, packed), Some(crc)).is_some() {
                    at = hend + add;
                    continue;
                }
                if node.kind != EntryKind::Directory {
                    node.size = unpacked;
                    let index = arc.entries.len();
                    let stored = problem.is_none();
                    arc.entries.push(Entry {
                        name: path.clone(),
                        segs: vec![(data_at, packed)],
                        size: unpacked,
                        method: if stored && method == 0x30 { 0 } else { 1 },
                        solid: solid_archive || flags & 0x10 != 0,
                        // 64 KiB to 4 MiB.
                        dict: 64 << 10 << ((flags >> 5) & 7).min(6),
                        extended: false,
                        crc: Some(crc),
                        enc: None,
                        enc4,
                        unp_ver,
                        problem,
                    });
                    if after {
                        split.open = Some(index);
                    }
                    node.data = if stored && method == 0x30 && !after && enc4.is_none() {
                        Data::Range { offset: data_at, len: packed.min(unpacked) }
                    } else {
                        Data::Rar { index }
                    };
                    tree.insert(&path, node);
                } else {
                    tree.update_dir(&path, node);
                }
            }
            // The end, unless another volume follows.
            0x7B => {
                let next = if flags & 1 != 0 { next_volume(raw, hend + add, SIG4)? } else { None };
                let Some(next) = next else {
                    if flags & 1 != 0 {
                        log.warn(format!(
                            "This archive goes on in volume {}, which is missing (the parts must be next to each other, with their names).",
                            volumes + 1
                        ));
                    }
                    break;
                };
                at = next;
                volumes += 1;
                encrypted = false;
                continue;
            }
            _ => {}
        }
        at = hend + add;
    }
    if volumes > 1 {
        log.info(format!("A RAR archive in {volumes} volumes."));
    }
    split.finish(&mut arc, &mut tree);
    resolve_symlink_targets(raw, &mut tree, &mut arc)?;
    Ok((tree, arc))
}

// ----- reading ------------------------------------------------------------------

/// Reads member `index` at `off` (decompressing as needed).
pub(super) fn read(arc: &mut Archive, raw: &mut Raw<'_>, index: usize, off: u64, buf: &mut [u8]) -> Result<usize> {
    let e = arc.entries.get(index).ok_or_else(|| bad("a member is missing"))?.clone();
    if let Some(p) = &e.problem {
        return Err(Error::new(ErrorKind::Unsupported, format!("{} cannot be read: {p}", e.name)));
    }
    if off >= e.size || buf.is_empty() {
        return Ok(0);
    }
    let want = buf.len().min((e.size - off) as usize);
    if e.method == 0 {
        let key = if e.enc4.is_some() { Some(arc.key4(raw, index)?) } else { arc.key(&e)? };
        match key {
            None => read_segs(raw, &e.segs, off, &mut buf[..want])?,
            Some(k) => read_encrypted(raw, &e, &k, off, &mut buf[..want])?,
        }
        return Ok(want);
    }
    // Continue the stream if it is on this member, before `off`, or on an
    // earlier member of the same solid run (decoding on to this one);
    // otherwise start again.
    let usable = match &arc.stream {
        Some(s) if s.entry == index => s.pos <= off,
        Some(s) if s.entry < index => (s.entry + 1..=index).all(|k| arc.entries[k].method == 0 || arc.entries[k].solid),
        _ => false,
    };
    if usable && arc.stream.as_ref().is_some_and(|s| s.entry != index) {
        let mut s = arc.stream.take().unwrap();
        s.advance(arc, index, raw)?;
        arc.stream = Some(s);
    } else if !usable {
        arc.stream = Some(Stream::start(arc, index, raw)?);
    }
    let s = arc.stream.as_mut().unwrap();
    s.read(raw, off, &mut buf[..want])
}

/// Reads a stored, encrypted member at `off`: CBC needs only the block
/// before the ones wanted (or the IV) to decrypt them.
fn read_encrypted(raw: &mut Raw<'_>, e: &Entry, k: &Key, off: u64, buf: &mut [u8]) -> Result<()> {
    #[cfg(feature = "archive-aes")]
    {
        let first = off / 16 * 16;
        let end = (off + buf.len() as u64).next_multiple_of(16).min(e.packed());
        let mut iv = k.iv;
        if first >= 16 {
            read_segs(raw, &e.segs, first - 16, &mut iv)?;
        }
        let mut blocks = vec![0u8; (end - first) as usize];
        read_segs(raw, &e.segs, first, &mut blocks)?;
        super::crypto::aes_cbc_decrypt(&k.aes[..k.len], &mut iv, &mut blocks)?;
        let skip = (off - first) as usize;
        buf.copy_from_slice(blocks.get(skip..skip + buf.len()).ok_or_else(|| bad("a member's data ends early"))?);
        Ok(())
    }
    #[cfg(not(feature = "archive-aes"))]
    {
        let _ = (raw, e, k, off, buf);
        Err(super::codec::left_out("AES encryption"))
    }
}

/// A decoder: RAR 5, or RAR 2.9 (RAR 3 and 4 archives).
#[derive(Debug, Clone)]
enum Dec {
    V5(Box<Decoder>),
    V3(Box<v3::Unpack29>),
}

impl Dec {
    fn begin(&mut self, e: &Entry, raw: &mut Raw<'_>, key: Option<Key>) -> Result<()> {
        match self {
            Dec::V5(d) => d.begin(e, raw, key),
            Dec::V3(d) => d.begin(e, raw, key),
        }
    }

    fn finished(&self) -> bool {
        match self {
            Dec::V5(d) => d.finished(),
            Dec::V3(d) => d.finished(),
        }
    }

    fn fill(&mut self, raw: &mut Raw<'_>, out: &mut Vec<u8>, want: usize) -> Result<()> {
        match self {
            Dec::V5(d) => d.fill(raw, out, want),
            Dec::V3(d) => d.fill(raw, out, want),
        }
    }
}

/// The decompression stream of a run of (solid) members.
#[derive(Debug, Clone)]
struct Stream {
    dec: Dec,
    /// The member being produced and the offset of `out[0]` in it.
    entry: usize,
    pos: u64,
    out: Vec<u8>,
}

impl Stream {
    /// Positions a stream at the start of member `index`: from the first
    /// member of its solid run, skipping through those before it.
    fn start(arc: &Archive, index: usize, raw: &mut Raw<'_>) -> Result<Stream> {
        let compressed = |i: usize| arc.entries[i].method != 0;
        let mut first = index;
        if arc.entries[index].solid {
            let mut j = index;
            while j > 0 {
                j -= 1;
                if !compressed(j) {
                    continue;
                }
                first = j;
                if !arc.entries[j].solid {
                    break;
                }
            }
        }
        let run: Vec<usize> = (first..=index).filter(|&i| compressed(i)).collect();
        // The window serves the whole run, members after this one included
        // (the stream may go on to them).
        let mut last = index;
        while last + 1 < arc.entries.len() && (!compressed(last + 1) || arc.entries[last + 1].solid) {
            last += 1;
        }
        let all = || (first..=last).filter(|&i| compressed(i));
        let total: u64 = all().map(|i| arc.entries[i].size).sum();
        let dict = all().map(|i| arc.entries[i].dict).max().unwrap_or(0);
        let extended = all().any(|i| arc.entries[i].extended);
        let mut dec = if arc.entries[index].unp_ver != 0 {
            Dec::V3(Box::new(v3::Unpack29::new(dict)))
        } else {
            Dec::V5(Box::new(Decoder::new(dict.max(4 << 20).min(total.max(1)), total, extended)))
        };
        let mut out = Vec::new();
        for &i in &run[..run.len() - 1] {
            if let Some(p) = &arc.entries[i].problem {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    format!(
                        "{} cannot be read: an earlier member it depends on cannot be read ({p})",
                        arc.entries[index].name
                    ),
                ));
            }
            dec.begin(&arc.entries[i], raw, arc.stream_key(i)?)?;
            while !dec.finished() {
                out.clear();
                dec.fill(raw, &mut out, 1 << 20)?;
            }
        }
        dec.begin(&arc.entries[index], raw, arc.stream_key(index)?)?;
        Ok(Stream { dec, entry: index, pos: 0, out: Vec::new() })
    }

    /// Moves on to a later member of the same solid run: the rest of the
    /// current one and those between are decoded and dropped.
    fn advance(&mut self, arc: &Archive, index: usize, raw: &mut Raw<'_>) -> Result<()> {
        let mut out = Vec::new();
        while !self.dec.finished() {
            out.clear();
            self.dec.fill(raw, &mut out, 1 << 20)?;
        }
        for k in self.entry + 1..index {
            let e = &arc.entries[k];
            if e.method == 0 {
                continue;
            }
            if let Some(p) = &e.problem {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    format!(
                        "{} cannot be read: an earlier member it depends on cannot be read ({p})",
                        arc.entries[index].name
                    ),
                ));
            }
            self.dec.begin(e, raw, arc.stream_key(k)?)?;
            while !self.dec.finished() {
                out.clear();
                self.dec.fill(raw, &mut out, 1 << 20)?;
            }
        }
        self.dec.begin(&arc.entries[index], raw, arc.stream_key(index)?)?;
        self.entry = index;
        self.pos = 0;
        self.out.clear();
        Ok(())
    }

    fn read(&mut self, raw: &mut Raw<'_>, off: u64, buf: &mut [u8]) -> Result<usize> {
        // Skip (decompressing) to `off`, then fill.
        loop {
            let end = self.pos + self.out.len() as u64;
            if off < end {
                let from = (off - self.pos) as usize;
                let n = buf.len().min(self.out.len() - from);
                buf[..n].copy_from_slice(&self.out[from..from + n]);
                return Ok(n);
            }
            if self.dec.finished() {
                return Ok(0);
            }
            self.pos = end;
            self.out.clear();
            self.dec.fill(raw, &mut self.out, (1 << 20).max(buf.len()))?;
        }
    }
}

// ----- RAR 5 decompression -----------------------------------------------------

const NC: usize = 306;
const DC: usize = 64;
const DCX: usize = 80;
const LDC: usize = 16;
const RC: usize = 44;
const BC: usize = 20;
const MAX_FILTER: u64 = 0x40_0000;
/// The longest match (with room to spare).
const MAX_MATCH: u64 = 0x1200;

/// Bytes kept read ahead of the bit position: more than a block header and
/// its Huffman tables can take (so they need no refill on the way).
const LOOK_AHEAD: usize = 4096;
/// How much packed data to read at a time (small in tests, so that reads
/// cross refill boundaries often).
const CHUNK: usize = if cfg!(test) { 1500 } else { 1 << 20 };

/// Compressed bits, read from the archive in pieces (most significant bit
/// first, as RAR packs them).
#[derive(Debug, Clone, Default)]
struct BitIn {
    buf: Vec<u8>,
    /// Stream offset of `buf[0]`.
    base: u64,
    addr: usize,
    bit: u32,
    /// The packed data: where its pieces are in the archive, and its length.
    segs: Vec<(u64, u64)>,
    len: u64,
    /// Packed bytes taken into `buf` so far (the rest of `buf` is zeros).
    taken: u64,
    /// Encrypted: the key (and how much of it: 16 or 32 bytes) and the IV
    /// for what comes next (AES-CBC).
    cipher: Option<([u8; 32], usize, [u8; 16])>,
}

impl BitIn {
    fn pos(&self) -> u64 {
        self.base + self.addr as u64
    }

    /// Keeps `LOOK_AHEAD` bytes ahead of the read position (zeros past the end).
    #[inline]
    fn refill(&mut self, raw: &mut Raw<'_>) -> Result<()> {
        if self.addr + LOOK_AHEAD <= self.buf.len() {
            return Ok(());
        }
        self.refill_slow(raw)
    }

    #[cold]
    fn refill_slow(&mut self, raw: &mut Raw<'_>) -> Result<()> {
        let drop = self.addr.min(self.buf.len());
        self.buf.drain(..drop);
        self.base += drop as u64;
        self.addr -= drop;
        // Real data first (replacing the zeros that padded the end).
        // (Past the end of the data the position can be beyond what was
        // taken: then everything here is padding.)
        let real = self.taken.saturating_sub(self.base) as usize;
        self.buf.truncate(real);
        // Enough real data for the look-ahead (at least a chunk); zeros
        // only past the end of the packed data.
        let need = self.addr + LOOK_AHEAD + 8;
        let mut want = CHUNK.max(need.saturating_sub(real));
        if self.cipher.is_some() {
            want = want.next_multiple_of(16); // whole AES blocks
        }
        let n = (self.len - self.taken).min(want as u64) as usize;
        self.buf.resize(real + n, 0);
        read_segs(raw, &self.segs, self.taken, &mut self.buf[real..])?;
        #[cfg(feature = "archive-aes")]
        if let Some((key, len, iv)) = &mut self.cipher {
            let whole = n / 16 * 16;
            super::crypto::aes_cbc_decrypt(&key[..*len], iv, &mut self.buf[real..real + whole])?;
        }
        self.taken += n as u64;
        if self.buf.len() < need {
            self.buf.resize(need, 0);
        }
        Ok(())
    }

    /// The next 64 bits of the buffer from the byte position.
    #[inline]
    fn word(&self) -> u64 {
        u64::from_be_bytes(self.buf[self.addr..self.addr + 8].try_into().unwrap())
    }

    /// The next 16 bits.
    #[inline]
    fn get16(&self) -> u32 {
        ((self.word() << self.bit) >> 48) as u32
    }

    /// The next 32 bits.
    #[inline]
    fn get32(&self) -> u32 {
        ((self.word() << self.bit) >> 32) as u32
    }

    #[inline]
    fn add(&mut self, n: u32) {
        let b = self.bit + n;
        self.addr += (b >> 3) as usize;
        self.bit = b & 7;
    }

    #[inline]
    fn bits(&mut self, n: u32) -> u32 {
        let v = self.get16() >> (16 - n);
        self.add(n);
        v
    }
}

/// A canonical Huffman decoding table, with a lookup table for the short
/// codes (most of them).
#[derive(Debug, Clone, Default)]
struct Huff {
    /// Left-aligned (16-bit) upper limits of each code length.
    limit: [u32; 16],
    pos: [u32; 16],
    num: Vec<u16>,
    /// Codes of up to `quick_bits` bits: (symbol, length) by their bits.
    quick_bits: u32,
    quick: Vec<(u16, u8)>,
}

impl Huff {
    fn new(lengths: &[u8], quick_bits: u32) -> Huff {
        let mut count = [0u32; 16];
        for &l in lengths {
            count[(l & 15) as usize] += 1;
        }
        count[0] = 0;
        let mut h = Huff { num: vec![0; lengths.len()], quick_bits, ..Default::default() };
        let mut upper = 0u32;
        for i in 1..16 {
            upper += count[i];
            h.limit[i] = upper << (16 - i);
            upper *= 2;
            h.pos[i] = h.pos[i - 1] + count[i - 1];
        }
        let mut next = h.pos;
        for (sym, &l) in lengths.iter().enumerate() {
            let l = (l & 15) as usize;
            if l != 0 {
                h.num[next[l] as usize] = sym as u16;
                next[l] += 1;
            }
        }
        h.quick = (0..1u32 << quick_bits)
            .map(|v| {
                let (sym, len) = h.slow(v << (16 - quick_bits));
                if len <= quick_bits { (sym as u16, len as u8) } else { (0, 0) }
            })
            .collect();
        h
    }

    /// (symbol, code length) for the 16 bits `field`.
    fn slow(&self, field: u32) -> (u32, u32) {
        let field = field & 0xFFFE;
        let mut bits = 15;
        for i in 1..15 {
            if field < self.limit[i] {
                bits = i;
                break;
            }
        }
        let d = (field - self.limit[bits - 1]) >> (16 - bits);
        let p = (self.pos[bits] + d) as usize;
        (self.num.get(p).copied().unwrap_or(0) as u32, bits as u32)
    }

    #[inline(always)]
    fn decode(&self, inp: &mut BitIn) -> u32 {
        let field = inp.get16();
        let (sym, len) = self.quick[(field >> (16 - self.quick_bits)) as usize];
        if len != 0 {
            inp.add(len as u32);
            return sym as u32;
        }
        let (sym, len) = self.slow(field);
        inp.add(len);
        sym
    }
}

#[derive(Debug, Clone, Default)]
struct Tables {
    ld: Huff,
    dd: Huff,
    ldd: Huff,
    rd: Huff,
}

#[derive(Debug, Clone, Default)]
struct Block {
    start: u64,
    size: u64,
    bit_size: u32,
    last: bool,
    table: bool,
}

#[derive(Debug, Clone)]
struct Filter {
    start: u64,
    len: u64,
    kind: u32,
    channels: usize,
}

#[derive(Debug, Clone)]
struct Decoder {
    win: Vec<u8>,
    /// Bytes produced and bytes handed out, from the stream's start.
    wr: u64,
    flushed: u64,
    /// The window can hold the whole stream (no wrapping to watch for).
    whole: bool,
    old: [u64; 4],
    last_len: u64,
    tables: Box<Tables>,
    /// Tables have been read (at least once).
    have_tables: bool,
    extended: bool,
    filters: VecDeque<Filter>,
    inp: BitIn,
    block: Block,
    /// The member being produced: where its output starts, what is left
    /// of it, its expected CRC and the CRC so far.
    file_start: u64,
    left: u64,
    crc: Option<u32>,
    crc_now: u32,
    /// The CRC is tweaked with this key (encrypted members).
    crc_key: Option<[u8; 32]>,
    /// Its last block has been decoded.
    done: bool,
}

impl Decoder {
    fn new(win: u64, total: u64, extended: bool) -> Decoder {
        Decoder {
            win: vec![0; win.max(1).next_power_of_two() as usize],
            wr: 0,
            flushed: 0,
            whole: win >= total,
            old: [u64::MAX; 4],
            last_len: 0,
            tables: Box::default(),
            have_tables: false,
            extended,
            filters: VecDeque::new(),
            inp: BitIn::default(),
            block: Block::default(),
            file_start: 0,
            left: 0,
            crc: None,
            crc_now: 0,
            crc_key: None,
            done: true,
        }
    }

    fn finished(&self) -> bool {
        self.left == 0
    }

    /// Starts decoding a member (its packed data).
    fn begin(&mut self, e: &Entry, raw: &mut Raw<'_>, key: Option<Key>) -> Result<()> {
        let cipher = key.map(|k| (k.aes, k.len, k.iv));
        self.inp = BitIn { len: e.packed(), segs: e.segs.clone(), cipher, ..Default::default() };
        self.crc_key = key.and_then(|k| k.hash);
        self.filters.clear();
        self.file_start = self.wr;
        self.left = e.size;
        self.crc = e.crc;
        self.crc_now = 0;
        self.done = false;
        if e.size == 0 {
            return Ok(());
        }
        self.inp.refill(raw)?;
        self.read_block_header()?;
        self.read_tables()?;
        if !self.have_tables {
            return Err(bad("no Huffman tables"));
        }
        Ok(())
    }

    fn read_block_header(&mut self) -> Result<()> {
        let inp = &mut self.inp;
        inp.add((8 - inp.bit) & 7);
        let flags = inp.bits(8);
        let count = ((flags >> 3) & 3) + 1;
        if count == 4 {
            return Err(bad("a block header"));
        }
        let check = inp.bits(8);
        let mut size = 0u32;
        for i in 0..count {
            size |= inp.bits(8) << (i * 8);
        }
        let sum = (0x5A ^ flags ^ size ^ (size >> 8) ^ (size >> 16)) & 0xFF;
        if sum != check {
            return Err(bad("a block header checksum"));
        }
        self.block = Block {
            start: inp.pos(),
            size: size as u64,
            bit_size: (flags & 7) + 1,
            last: flags & 0x40 != 0,
            table: flags & 0x80 != 0,
        };
        Ok(())
    }

    fn read_tables(&mut self) -> Result<()> {
        if !self.block.table {
            return Ok(());
        }
        let inp = &mut self.inp;
        let mut bl = [0u8; BC];
        let mut i = 0;
        while i < BC {
            let l = inp.bits(4) as u8;
            if l == 15 {
                let zeros = inp.bits(4) as usize;
                if zeros == 0 {
                    bl[i] = 15;
                    i += 1;
                } else {
                    for _ in 0..zeros + 2 {
                        if i < BC {
                            bl[i] = 0;
                            i += 1;
                        }
                    }
                }
            } else {
                bl[i] = l;
                i += 1;
            }
        }
        let bd = Huff::new(&bl, 7);
        let dc = if self.extended { DCX } else { DC };
        let size = NC + dc + LDC + RC;
        let mut t = vec![0u8; size];
        let mut i = 0;
        while i < size {
            let n = bd.decode(inp);
            if n < 16 {
                t[i] = n as u8;
                i += 1;
            } else if n < 18 {
                let count = if n == 16 { inp.bits(3) + 3 } else { inp.bits(7) + 11 };
                if i == 0 {
                    return Err(bad("a Huffman table"));
                }
                for _ in 0..count {
                    if i < size {
                        t[i] = t[i - 1];
                        i += 1;
                    }
                }
            } else {
                let count = if n == 18 { inp.bits(3) + 3 } else { inp.bits(7) + 11 };
                for _ in 0..count {
                    if i < size {
                        t[i] = 0;
                        i += 1;
                    }
                }
            }
        }
        self.have_tables = true;
        *self.tables = Tables {
            ld: Huff::new(&t[..NC], 10),
            dd: Huff::new(&t[NC..NC + dc], 7),
            ldd: Huff::new(&t[NC + dc..NC + dc + LDC], 7),
            rd: Huff::new(&t[NC + dc + LDC..], 7),
        };
        Ok(())
    }

    fn slot_to_length(inp: &mut BitIn, slot: u32) -> u64 {
        let (lbits, mut len) = if slot < 8 {
            (0, 2 + slot as u64)
        } else {
            let lb = slot / 4 - 1;
            (lb, 2 + (((4 | (slot & 3)) as u64) << lb))
        };
        if lbits > 0 {
            len += inp.bits(lbits) as u64;
        }
        len
    }

    #[inline]
    fn put(&mut self, b: u8) {
        let mask = self.win.len() - 1;
        self.win[self.wr as usize & mask] = b;
        self.wr += 1;
    }

    #[inline]
    fn copy(&mut self, len: u64, dist: u64) -> Result<()> {
        let n = self.win.len();
        if dist == 0 || dist > self.wr || dist > n as u64 {
            return Err(bad("a match reaches before the start"));
        }
        let mask = n - 1;
        let (len, dist) = (len as usize, dist as usize);
        let dst = self.wr as usize & mask;
        let src = (self.wr as usize).wrapping_sub(dist) & mask;
        if dist >= len && src + len <= n && dst + len <= n {
            // Apart and unwrapped: one move.
            self.win.copy_within(src..src + len, dst);
        } else if dist == 1 && dst + len <= n {
            let b = self.win[src];
            self.win[dst..dst + len].fill(b);
        } else if src < dst && dst + len <= n {
            // Overlapping, unwrapped: the pattern repeats every `dist`
            // bytes, so each copy can take what the last one made.
            let mut done = 0;
            while done < len {
                let chunk = (len - done).min(dst + done - src);
                self.win.copy_within(src..src + chunk, dst + done);
                done += chunk;
            }
        } else {
            let (mut s, mut d) = (src, dst);
            for _ in 0..len {
                self.win[d] = self.win[s];
                s = (s + 1) & mask;
                d = (d + 1) & mask;
            }
        }
        self.wr += len as u64;
        Ok(())
    }

    /// Whether the current block (and so the member, if it is the last) is used up.
    fn block_used(&self) -> bool {
        let end = self.block.start + self.block.size;
        let p = self.inp.pos();
        end == 0 || p + 1 > end || (p + 1 == end && self.inp.bit >= self.block.bit_size)
    }

    /// Decodes until the window is (nearly) full of unread output or the
    /// member's data ends.
    /// Where the slow checks are due: refilling the input, the end of the
    /// block (a buffer index; past it, `block_used` decides).
    fn border(&self) -> usize {
        let look = self.inp.buf.len().saturating_sub(LOOK_AHEAD);
        let end = (self.block.start + self.block.size).saturating_sub(1).saturating_sub(self.inp.base);
        look.min(end.min(usize::MAX as u64) as usize)
    }

    fn decode(&mut self, raw: &mut Raw<'_>) -> Result<()> {
        let cap = self.win.len() as u64;
        // Stop before overwriting what has not been handed out.
        let stop = if self.whole { u64::MAX } else { (self.flushed + cap).saturating_sub(MAX_MATCH) };
        let mut border = 0;
        while !self.done && self.wr < stop {
            if self.inp.addr >= border {
                self.inp.refill(raw)?;
                while self.block_used() {
                    if self.block.last {
                        self.done = true;
                        return Ok(());
                    }
                    self.read_block_header()?;
                    self.read_tables()?;
                    self.inp.refill(raw)?;
                }
                if !self.have_tables {
                    return Err(bad("no Huffman tables"));
                }
                border = self.border();
            }
            let slot = self.tables.ld.decode(&mut self.inp);
            if slot < 256 {
                self.put(slot as u8);
            } else if slot >= 262 {
                let mut len = Self::slot_to_length(&mut self.inp, slot - 262);
                let dslot = self.tables.dd.decode(&mut self.inp) as u64;
                let mut dist = 1u64;
                if dslot < 4 {
                    dist += dslot;
                } else {
                    let dbits = (dslot / 2 - 1) as u32;
                    dist += (2 | (dslot & 1)) << dbits;
                    if dbits >= 4 {
                        if dbits > 4 {
                            // Up to 32 bits at a time.
                            let hi = dbits - 4;
                            let v = if hi > 32 {
                                let a = self.inp.get32() as u64;
                                self.inp.add(32);
                                let b = (self.inp.get32() >> (64 - hi)) as u64;
                                self.inp.add(hi - 32);
                                a << (hi - 32) | b
                            } else {
                                let v = (self.inp.get32() as u64) >> (32 - hi);
                                self.inp.add(hi);
                                v
                            };
                            dist += v << 4;
                        }
                        dist += self.tables.ldd.decode(&mut self.inp) as u64;
                    } else {
                        dist += self.inp.bits(dbits) as u64;
                    }
                }
                if dist > 0x100 {
                    len += 1;
                    if dist > 0x2000 {
                        len += 1;
                        if dist > 0x40000 {
                            len += 1;
                        }
                    }
                }
                self.old = [dist, self.old[0], self.old[1], self.old[2]];
                self.last_len = len;
                self.copy(len, dist)?;
            } else if slot == 256 {
                let mut data = || {
                    let n = self.inp.bits(2) + 1;
                    let mut v = 0u64;
                    for i in 0..n {
                        v |= (self.inp.bits(8) as u64) << (i * 8);
                    }
                    v
                };
                let start = data();
                let len = data();
                let kind = self.inp.bits(3);
                let channels = if kind == 0 { self.inp.bits(5) as usize + 1 } else { 0 };
                if kind > 3 || len > MAX_FILTER {
                    return Err(bad("a filter"));
                }
                if len > 0 {
                    self.filters.push_back(Filter { start: self.wr + start, len, kind, channels });
                }
            } else if slot == 257 {
                if self.last_len != 0 {
                    let (l, d) = (self.last_len, self.old[0]);
                    self.copy(l, d)?;
                }
            } else {
                let i = (slot - 258) as usize;
                let dist = self.old[i];
                for k in (1..=i).rev() {
                    self.old[k] = self.old[k - 1];
                }
                self.old[0] = dist;
                let lslot = self.tables.rd.decode(&mut self.inp);
                let len = Self::slot_to_length(&mut self.inp, lslot);
                self.last_len = len;
                self.copy(len, dist)?;
            }
        }
        Ok(())
    }

    /// Copies window bytes [from, to) into `out` (counting them for the CRC).
    fn emit(&mut self, from: u64, to: u64, out: &mut Vec<u8>, filtered: Option<&[u8]>) {
        let n = (to - from).min(self.left) as usize;
        let start = out.len();
        match filtered {
            Some(d) => out.extend_from_slice(&d[..n]),
            None => {
                let mask = self.win.len() as u64 - 1;
                let mut p = from;
                while out.len() < start + n {
                    let i = (p & mask) as usize;
                    let k = (self.win.len() - i).min(start + n - out.len());
                    out.extend_from_slice(&self.win[i..i + k]);
                    p += k as u64;
                }
            }
        }
        self.crc_now = dix_core::partition::crc32_continue(self.crc_now, &out[start..]);
        self.left -= n as u64;
    }

    /// Hands out what is decoded, applying filters once their block is complete.
    fn flush(&mut self, out: &mut Vec<u8>) {
        while self.flushed < self.wr && self.left > 0 {
            match self.filters.front().cloned() {
                Some(f) if f.start < self.flushed => {
                    self.filters.pop_front();
                }
                Some(f) if f.start == self.flushed => {
                    if f.start + f.len > self.wr && !self.done {
                        return; // wait for the whole block
                    }
                    let end = (f.start + f.len).min(self.wr);
                    // The block, out of the window (in two pieces if it wraps).
                    let mut data = Vec::with_capacity((end - f.start) as usize);
                    let mask = self.win.len() as u64 - 1;
                    let (a, n) = ((f.start & mask) as usize, (end - f.start) as usize);
                    let first = n.min(self.win.len() - a);
                    data.extend_from_slice(&self.win[a..a + first]);
                    data.extend_from_slice(&self.win[..n - first]);
                    let d = apply_filter(&f, data, (f.start - self.file_start) as u32);
                    self.emit(f.start, end, out, Some(&d));
                    self.flushed = end;
                    self.filters.pop_front();
                }
                Some(f) => {
                    let to = f.start.min(self.wr);
                    self.emit(self.flushed, to, out, None);
                    self.flushed = to;
                }
                None => {
                    let to = self.wr;
                    self.emit(self.flushed, to, out, None);
                    self.flushed = to;
                }
            }
        }
    }

    /// Adds at least `want` bytes of the member to `out` (fewer at its end).
    fn fill(&mut self, raw: &mut Raw<'_>, out: &mut Vec<u8>, want: usize) -> Result<()> {
        let start = out.len();
        while self.left > 0 && out.len() - start < want {
            self.decode(raw)?;
            let before = (out.len(), self.left);
            self.flush(out);
            if self.done && self.left > 0 && (out.len(), self.left) == before {
                return Err(bad("a member's data ends early"));
            }
        }
        if self.left == 0 {
            // Whatever the member decoded past its size is not part of it.
            self.flushed = self.wr;
            if let Some(c) = self.crc
                && c != tweaked(self.crc_key.as_ref(), self.crc_now)
            {
                return Err(Error::corrupted("a RAR member's checksum does not match: it is damaged"));
            }
        }
        Ok(())
    }
}

/// A CRC as stored: tweaked with the password's checksum key when there
/// is one.
fn tweaked(key: Option<&[u8; 32]>, crc: u32) -> u32 {
    #[cfg(feature = "archive-aes")]
    if let Some(k) = key {
        return super::crypto::rar5_crc(k, crc);
    }
    let _ = key;
    crc
}

/// Undoes a filter on a block of output (`pos`: where it is in its member).
fn apply_filter(f: &Filter, mut d: Vec<u8>, pos: u32) -> Vec<u8> {
    match f.kind {
        0 => {
            let mut out = vec![0u8; d.len()];
            let mut src = 0;
            for ch in 0..f.channels {
                let mut prev = 0u8;
                let mut p = ch;
                while p < d.len() {
                    prev = prev.wrapping_sub(d[src]);
                    out[p] = prev;
                    src += 1;
                    p += f.channels;
                }
            }
            out
        }
        1 | 2 => {
            const SIZE: u32 = 0x100_0000;
            let n = d.len();
            let mut i = 0usize;
            while i + 4 < n {
                let c = d[i];
                i += 1;
                if c == 0xE8 || (f.kind == 2 && c == 0xE9) {
                    let off = (i as u32).wrapping_add(pos) % SIZE;
                    let addr = u32::from_le_bytes(d[i..i + 4].try_into().unwrap());
                    if addr & 0x8000_0000 != 0 {
                        if addr.wrapping_add(off) & 0x8000_0000 == 0 {
                            d[i..i + 4].copy_from_slice(&addr.wrapping_add(SIZE).to_le_bytes());
                        }
                    } else if addr.wrapping_sub(SIZE) & 0x8000_0000 != 0 {
                        d[i..i + 4].copy_from_slice(&addr.wrapping_sub(off).to_le_bytes());
                    }
                    i += 4;
                }
            }
            d
        }
        _ => {
            let mut i = 0usize;
            while i + 3 < d.len() {
                if d[i + 3] == 0xEB {
                    let mut off = d[i] as u32 | (d[i + 1] as u32) << 8 | (d[i + 2] as u32) << 16;
                    off = off.wrapping_sub((pos.wrapping_add(i as u32)) / 4);
                    d[i] = off as u8;
                    d[i + 1] = (off >> 8) as u8;
                    d[i + 2] = (off >> 16) as u8;
                }
                i += 4;
            }
            d
        }
    }
}
