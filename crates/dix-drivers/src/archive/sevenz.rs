//! 7z (read-only).
//!
//! Layout: a 32-byte signature header pointing at the *next header* at the
//! end of the file; the packed streams are in between. The next header is
//! usually itself compressed ("encoded header": a streams description whose
//! one folder unpacks to the real header). The header describes:
//!
//! * pack streams: where the compressed data is;
//! * folders: each a small graph of coders (e.g. BCJ after LZMA) turning
//!   pack streams into one output; with *solid* compression one folder's
//!   output holds many files (substreams), one after another;
//! * files: names, empty / directory flags, times, attributes (Windows bits,
//!   and Unix mode bits in the high half when bit 0x8000 is set).
//!
//! Coders: Copy, LZMA, LZMA2 (lzma-rs), Deflate, BZip2, zstd (the 7-Zip
//! zstd fork), PPMd (see `ppmd`), the BCJ x86 and Delta filters, and
//! AES-256 with a password (`archive-aes`; also an encrypted header, the
//! list of files). Not supported (reported clearly): BCJ2 and other branch
//! filters.
//! A folder is unpacked whole when one of its files is read.

use super::{Data, Node, Raw, Tree, clean_path, codec};
use dix_core::{EntryKind, Error, MountLog, Result};

pub(super) const SIGNATURE: [u8; 6] = [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C];

/// Unpacked folder output larger than this is refused (held in memory).
const MAX_FOLDER: u64 = 16 << 30;

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn bad(what: &str) -> Error {
    Error::corrupted(format!("damaged 7z header ({what})"))
}

/// A cursor over header bytes.
struct R<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> R<'a> {
    fn byte(&mut self) -> Result<u8> {
        let v = *self.b.get(self.at).ok_or_else(|| bad("ends early"))?;
        self.at += 1;
        Ok(v)
    }

    fn bytes(&mut self, n: u64) -> Result<&'a [u8]> {
        let n = usize::try_from(n).map_err(|_| bad("size"))?;
        let s =
            self.b.get(self.at..self.at.checked_add(n).ok_or_else(|| bad("size"))?).ok_or_else(|| bad("ends early"))?;
        self.at += n;
        Ok(s)
    }

    /// 7z's variable-length number: the leading 1 bits of the first byte
    /// say how many bytes follow (little-endian), the rest of it is the top.
    fn number(&mut self) -> Result<u64> {
        let first = self.byte()?;
        let mut mask = 0x80u8;
        let mut value = 0u64;
        for i in 0..8 {
            if first & mask == 0 {
                let high = (first & mask.wrapping_sub(1)) as u64;
                return Ok(value | (high << (8 * i)));
            }
            value |= (self.byte()? as u64) << (8 * i);
            mask >>= 1;
        }
        Ok(value)
    }

    fn count(&mut self) -> Result<usize> {
        let n = self.number()?;
        if n > 1 << 26 {
            return Err(bad("count"));
        }
        Ok(n as usize)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(le32(self.bytes(4)?, 0))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(le64(self.bytes(8)?, 0))
    }

    /// `n` bits, most significant first.
    fn bits(&mut self, n: usize) -> Result<Vec<bool>> {
        let bytes = self.bytes(n.div_ceil(8) as u64)?;
        Ok((0..n).map(|i| bytes[i / 8] & (0x80 >> (i % 8)) != 0).collect())
    }

    /// "All defined" byte, else a bit vector.
    fn defined(&mut self, n: usize) -> Result<Vec<bool>> {
        if self.byte()? != 0 { Ok(vec![true; n]) } else { self.bits(n) }
    }

    fn digests(&mut self, n: usize) -> Result<Vec<Option<u32>>> {
        let defined = self.defined(n)?;
        defined.into_iter().map(|d| if d { self.u32().map(Some) } else { Ok(None) }).collect()
    }
}

#[derive(Debug, Clone)]
struct Coder {
    id: Vec<u8>,
    ins: usize,
    outs: usize,
    props: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Folder {
    coders: Vec<Coder>,
    /// (in stream, out stream) pairs connecting coders.
    binds: Vec<(usize, usize)>,
    /// The in streams fed from pack streams, in pack stream order.
    packed: Vec<usize>,
    /// The size of every out stream.
    sizes: Vec<u64>,
    crc: Option<u32>,
    /// Where this folder's pack streams are (offset, length) in the file.
    packs: Vec<(u64, u64)>,
    /// Its files' (offset, length, CRC): with encryption, the CRCs tell a
    /// right password from a wrong one.
    checks: Vec<(u64, u64, u32)>,
}

const AES: [u8; 4] = [0x06, 0xF1, 0x07, 0x01];

impl Folder {
    /// The out stream nothing else consumes: the folder's output.
    fn main_out(&self) -> Result<usize> {
        (0..self.sizes.len()).find(|o| !self.binds.iter().any(|b| b.1 == *o)).ok_or_else(|| bad("folder"))
    }

    pub fn size(&self) -> u64 {
        self.main_out().ok().and_then(|o| self.sizes.get(o).copied()).unwrap_or(0)
    }

    /// Unpacking it needs a password.
    pub fn encrypted(&self) -> bool {
        self.coders.iter().any(|c| c.id == AES)
    }
}

/// The folders, for reading file data later.
#[derive(Debug, Clone, Default)]
pub(super) struct Archive {
    pub folders: Vec<Folder>,
}

#[derive(Default)]
struct Streams {
    folders: Vec<Folder>,
    /// Per folder: (size, crc) of each substream.
    substreams: Vec<Vec<(u64, Option<u32>)>>,
}

fn folder(r: &mut R<'_>) -> Result<Folder> {
    let mut f = Folder::default();
    for _ in 0..r.count()? {
        let flags = r.byte()?;
        if flags & 0x80 != 0 {
            return Err(Error::unsupported("7z coders with alternative methods"));
        }
        let id = r.bytes((flags & 0x0F) as u64)?.to_vec();
        let (ins, outs) = if flags & 0x10 != 0 { (r.count()?, r.count()?) } else { (1, 1) };
        let props = if flags & 0x20 != 0 {
            let n = r.number()?;
            r.bytes(n)?.to_vec()
        } else {
            Vec::new()
        };
        f.coders.push(Coder { id, ins, outs, props });
    }
    let total_in: usize = f.coders.iter().map(|c| c.ins).sum();
    let total_out: usize = f.coders.iter().map(|c| c.outs).sum();
    if total_out == 0 || total_in == 0 {
        return Err(bad("folder without streams"));
    }
    for _ in 0..total_out - 1 {
        f.binds.push((r.count()?, r.count()?));
    }
    let packed = total_in.checked_sub(f.binds.len()).ok_or_else(|| bad("bind pairs"))?;
    if packed == 1 {
        f.packed.push((0..total_in).find(|i| !f.binds.iter().any(|b| b.0 == *i)).ok_or_else(|| bad("folder"))?);
    } else {
        for _ in 0..packed {
            f.packed.push(r.count()?);
        }
    }
    Ok(f)
}

/// PackInfo, CodersInfo and SubStreamsInfo (`base`: where pack data starts).
fn streams(r: &mut R<'_>, base: u64) -> Result<Streams> {
    let mut s = Streams::default();
    let mut pack_pos = 0u64;
    let mut pack_sizes: Vec<u64> = Vec::new();
    let mut nums: Option<Vec<usize>> = None;
    let mut sub_sizes: Option<Vec<Vec<u64>>> = None;
    let mut sub_crcs: Option<Vec<Option<u32>>> = None;
    loop {
        match r.number()? {
            0 => break,
            0x06 => {
                pack_pos = r.number()?;
                let n = r.count()?;
                loop {
                    match r.number()? {
                        0 => break,
                        0x09 => pack_sizes = (0..n).map(|_| r.number()).collect::<Result<_>>()?,
                        0x0A => {
                            r.digests(n)?;
                        }
                        _ => return Err(bad("pack info")),
                    }
                }
            }
            0x07 => {
                if r.number()? != 0x0B {
                    return Err(bad("coders info"));
                }
                let n = r.count()?;
                if r.byte()? != 0 {
                    return Err(Error::unsupported("7z folders stored elsewhere"));
                }
                s.folders = (0..n).map(|_| folder(r)).collect::<Result<_>>()?;
                if r.number()? != 0x0C {
                    return Err(bad("unpack sizes"));
                }
                for f in &mut s.folders {
                    let outs: usize = f.coders.iter().map(|c| c.outs).sum();
                    f.sizes = (0..outs).map(|_| r.number()).collect::<Result<_>>()?;
                }
                loop {
                    match r.number()? {
                        0 => break,
                        0x0A => {
                            for (f, crc) in s.folders.iter_mut().zip(r.digests(n)?) {
                                f.crc = crc;
                            }
                        }
                        _ => return Err(bad("coders info")),
                    }
                }
            }
            0x08 => loop {
                match r.number()? {
                    0 => break,
                    0x0D => nums = Some((0..s.folders.len()).map(|_| r.count()).collect::<Result<_>>()?),
                    0x09 => {
                        let counts = nums.clone().unwrap_or(vec![1; s.folders.len()]);
                        let mut all = Vec::new();
                        for (f, &k) in s.folders.iter().zip(&counts) {
                            let mut sizes = Vec::new();
                            if k > 0 {
                                for _ in 0..k - 1 {
                                    sizes.push(r.number()?);
                                }
                                let used: u64 = sizes.iter().sum();
                                sizes.push(f.size().checked_sub(used).ok_or_else(|| bad("substream sizes"))?);
                            }
                            all.push(sizes);
                        }
                        sub_sizes = Some(all);
                    }
                    0x0A => {
                        let counts = nums.clone().unwrap_or(vec![1; s.folders.len()]);
                        let unknown: usize = s
                            .folders
                            .iter()
                            .zip(&counts)
                            .map(|(f, &k)| if k == 1 && f.crc.is_some() { 0 } else { k })
                            .sum();
                        sub_crcs = Some(r.digests(unknown)?);
                    }
                    _ => return Err(bad("substreams info")),
                }
            },
            _ => return Err(bad("streams info")),
        }
    }
    // Where each folder's pack streams are.
    let mut at = base + pack_pos;
    let mut next = 0;
    for f in &mut s.folders {
        for _ in 0..f.packed.len() {
            let len = *pack_sizes.get(next).ok_or_else(|| bad("pack streams"))?;
            f.packs.push((at, len));
            at += len;
            next += 1;
        }
    }
    // Substreams: one per folder unless said otherwise.
    let counts = nums.unwrap_or(vec![1; s.folders.len()]);
    let mut crcs = sub_crcs.unwrap_or_default().into_iter();
    for (i, f) in s.folders.iter().enumerate() {
        let sizes = match &sub_sizes {
            Some(all) => all[i].clone(),
            None if counts[i] == 1 => vec![f.size()],
            None => vec![0; counts[i]],
        };
        let known = counts[i] == 1 && f.crc.is_some();
        s.substreams
            .push(sizes.into_iter().map(|size| (size, if known { f.crc } else { crcs.next().flatten() })).collect());
    }
    Ok(s)
}

/// The name of a coder, for messages.
fn method_name(id: &[u8]) -> String {
    match id {
        [0x03, 0x04, 0x01] => "PPMd".into(),
        [0x03, 0x03, 0x01, 0x1B] => "BCJ2".into(),
        [0x06, 0xF1, 0x07, 0x01] => "AES encryption".into(),
        [0x0A] => "ARM64 filter".into(),
        [0x03, 0x03, 0x05, 0x01] => "ARM filter".into(),
        [0x03, 0x03, 0x02, 0x05] => "PowerPC filter".into(),
        [0x03, 0x03, 0x04, 0x01] => "IA-64 filter".into(),
        [0x03, 0x03, 0x07, 0x01] => "ARM Thumb filter".into(),
        [0x03, 0x03, 0x08, 0x05] => "SPARC filter".into(),
        id => format!("method {}", id.iter().map(|b| format!("{b:02X}")).collect::<String>()),
    }
}

/// The x86 branch converter (BCJ), decoding: relative CALL/JMP targets
/// were made absolute to compress better.
fn bcj_x86(buf: &mut [u8]) {
    const ALLOWED: [bool; 8] = [true, true, true, false, true, false, false, false];
    const BIT: [u32; 8] = [0, 1, 2, 2, 3, 3, 3, 3];
    let test = |b: u8| b == 0x00 || b == 0xFF;
    if buf.len() <= 4 {
        return;
    }
    let size = buf.len() - 4;
    let mut prev_pos = usize::MAX;
    let mut prev_mask: u32 = 0;
    let mut i = 0;
    while i < size {
        if buf[i] & 0xFE != 0xE8 {
            i += 1;
            continue;
        }
        let gap = i.wrapping_sub(prev_pos);
        if gap > 3 {
            prev_mask = 0;
        } else {
            prev_mask = (prev_mask << (gap - 1)) & 7;
            if prev_mask != 0 {
                let b = buf[i + 4 - BIT[prev_mask as usize] as usize];
                if !ALLOWED[prev_mask as usize] || test(b) {
                    prev_pos = i;
                    prev_mask = (prev_mask << 1) | 1;
                    i += 1;
                    continue;
                }
            }
        }
        prev_pos = i;
        if test(buf[i + 4]) {
            let mut src = le32(buf, i + 1);
            let mut dest;
            loop {
                dest = src.wrapping_sub((i as u32).wrapping_add(5));
                if prev_mask == 0 {
                    break;
                }
                let j = BIT[prev_mask as usize] * 8;
                let b = (dest >> (24 - j)) as u8;
                if !test(b) {
                    break;
                }
                src = dest ^ ((1u32 << (32 - j)) - 1);
            }
            dest &= 0x01FF_FFFF;
            dest |= 0u32.wrapping_sub(dest & 0x0100_0000);
            buf[i + 1..i + 5].copy_from_slice(&dest.to_le_bytes());
            i += 5;
        } else {
            prev_mask = (prev_mask << 1) | 1;
            i += 1;
        }
    }
}

/// AES-256-CBC with a key from the password (see `crypto`).
#[cfg(feature = "archive-aes")]
fn aes(c: &Coder, mut input: Vec<u8>, password: &str) -> Result<Vec<u8>> {
    let p = &c.props;
    let b0 = *p.first().ok_or_else(|| bad("AES properties"))?;
    let cycles = (b0 & 0x3F) as u32;
    let (mut salt, mut iv) = (Vec::new(), [0u8; 16]);
    if b0 & 0xC0 != 0 {
        let b1 = *p.get(1).ok_or_else(|| bad("AES properties"))?;
        let salt_len = ((b0 >> 7) & 1) as usize + (b1 >> 4) as usize;
        let iv_len = ((b0 >> 6) & 1) as usize + (b1 & 0x0F) as usize;
        salt = p.get(2..2 + salt_len).ok_or_else(|| bad("AES properties"))?.to_vec();
        let v = p.get(2 + salt_len..2 + salt_len + iv_len).ok_or_else(|| bad("AES properties"))?;
        iv[..v.len()].copy_from_slice(v);
    }
    let key = super::crypto::key_7z(password, &salt, cycles);
    let whole = input.len() / 16 * 16;
    super::crypto::aes256_cbc_decrypt(&key, &mut iv, &mut input[..whole])?;
    input.truncate(whole);
    Ok(input)
}

#[cfg(not(feature = "archive-aes"))]
fn aes(_: &Coder, _: Vec<u8>, _: &str) -> Result<Vec<u8>> {
    Err(codec::left_out("AES encryption"))
}

/// Runs one coder (`password`: for AES).
fn decode(c: &Coder, input: Vec<u8>, size: u64, password: Option<&str>) -> Result<Vec<u8>> {
    if size > MAX_FOLDER {
        return Err(Error::new(dix_core::ErrorKind::NoSpace, "a 7z block too large to unpack in memory"));
    }
    let lzma = |e: lzma_rs::error::Error| Error::corrupted(format!("7z LZMA data: {e}"));
    let mut out = match c.id.as_slice() {
        [0x00] => input,
        [0x03, 0x01, 0x01] => {
            // Raw LZMA: give lzma-rs the classic header (properties, size).
            if c.props.len() < 5 {
                return Err(bad("LZMA properties"));
            }
            let mut stream = Vec::with_capacity(13 + input.len());
            stream.extend_from_slice(&c.props[..5]);
            stream.extend_from_slice(&size.to_le_bytes());
            stream.extend_from_slice(&input);
            let mut out = Vec::with_capacity(size as usize);
            lzma_rs::lzma_decompress(&mut std::io::BufReader::new(&stream[..]), &mut out).map_err(lzma)?;
            out
        }
        [0x21] => {
            let mut out = Vec::with_capacity(size as usize);
            lzma_rs::lzma2_decompress(&mut std::io::BufReader::new(&input[..]), &mut out).map_err(lzma)?;
            out
        }
        [0x03, 0x03, 0x01, 0x03] => {
            let mut b = input;
            bcj_x86(&mut b);
            b
        }
        [0x03] => {
            let dist = *c.props.first().unwrap_or(&0) as usize + 1;
            let mut b = input;
            for i in dist..b.len() {
                b[i] = b[i].wrapping_add(b[i - dist]);
            }
            b
        }
        [0x04, 0x01, 0x08] => codec::inflate(&input, size as usize)?.0,
        [0x03, 0x04, 0x01] => super::ppmd::decode_7z(&c.props, &input, size as usize)
            .ok_or_else(|| Error::corrupted("damaged 7z PPMd data"))?,
        [0x04, 0x02, 0x02] => codec::Compression::Bzip2.decompress(&input)?,
        [0x04, 0xF7, 0x11, 0x01] => codec::Compression::Zstd.decompress(&input)?,
        [0x06, 0xF1, 0x07, 0x01] => match password {
            Some(p) => aes(c, input, p)?,
            None => return Err(locked()),
        },
        id => return Err(Error::unsupported(format!("7z {}", method_name(id)))),
    };
    if (out.len() as u64) < size {
        return Err(Error::corrupted("7z data ends early"));
    }
    out.truncate(size as usize);
    Ok(out)
}

fn locked() -> Error {
    Error::new(dix_core::ErrorKind::PasswordRequired, "it is encrypted: a password is needed")
}

/// Unpacks folder `f` (its main output); an encrypted one with each of
/// `passwords` until one gives data whose CRCs match.
fn unpack(raw: &mut Raw<'_>, f: &Folder, passwords: &[String]) -> Result<Vec<u8>> {
    if !f.encrypted() {
        return unpack_with(raw, f, None);
    }
    if passwords.is_empty() {
        return Err(locked());
    }
    for p in passwords {
        match unpack_with(raw, f, Some(p)) {
            Ok(out) => {
                let ok = f.checks.iter().all(|&(at, len, crc)| {
                    out.get(at as usize..(at + len) as usize).is_some_and(|b| dix_core::partition::crc32(b) == crc)
                });
                if ok {
                    return Ok(out);
                }
            }
            // A method that cannot be read whatever the password.
            Err(e) if e.kind() == dix_core::ErrorKind::Unsupported => return Err(e),
            Err(_) => {} // a wrong password gives garbage
        }
    }
    Err(Error::new(dix_core::ErrorKind::PasswordRequired, "it is encrypted, and no password given opens it"))
}

/// Unpacks folder `f` with one password (or none).
fn unpack_with(raw: &mut Raw<'_>, f: &Folder, password: Option<&str>) -> Result<Vec<u8>> {
    fn out_stream(raw: &mut Raw<'_>, f: &Folder, o: usize, depth: usize, pw: Option<&str>) -> Result<Vec<u8>> {
        if depth > 8 {
            return Err(bad("coder graph"));
        }
        // The coder owning out stream `o`, and its first in stream.
        let (mut first_out, mut first_in) = (0, 0);
        let mut owner = None;
        for c in &f.coders {
            if o < first_out + c.outs {
                owner = Some(c);
                break;
            }
            first_out += c.outs;
            first_in += c.ins;
        }
        let c = owner.ok_or_else(|| bad("coder graph"))?;
        if c.outs != 1 || c.ins != 1 {
            return Err(Error::unsupported(format!("7z {}", method_name(&c.id))));
        }
        let input = match f.binds.iter().find(|b| b.0 == first_in) {
            Some(&(_, from)) => out_stream(raw, f, from, depth + 1, pw)?,
            None => {
                let k = f.packed.iter().position(|&i| i == first_in).ok_or_else(|| bad("pack streams"))?;
                let (at, len) = f.packs[k];
                if at + len > raw.size() || len > MAX_FOLDER {
                    return Err(Error::corrupted("7z data extends beyond the file"));
                }
                let mut v = vec![0u8; len as usize];
                raw.read_at(at, &mut v)?;
                v
            }
        };
        decode(c, input, f.sizes[o], pw)
    }
    let out = out_stream(raw, f, f.main_out()?, 0, password)?;
    if let Some(crc) = f.crc
        && dix_core::partition::crc32(&out) != crc
    {
        return Err(Error::corrupted("7z data checksum mismatch"));
    }
    Ok(out)
}

/// The bytes of folder `index`.
pub(super) fn folder_bytes(
    raw: &mut Raw<'_>,
    archive: &Archive,
    index: usize,
    passwords: &[String],
) -> Result<Vec<u8>> {
    unpack(raw, archive.folders.get(index).ok_or_else(|| bad("folder index"))?, passwords)
}

/// Windows FILETIME (100 ns since 1601) as Unix seconds.
fn filetime(t: u64) -> i64 {
    (t / 10_000_000) as i64 - 11_644_473_600
}

/// Reads the file list (`passwords`: for an encrypted one).
pub(super) fn parse(raw: &mut Raw<'_>, log: &MountLog, passwords: &[String]) -> Result<(Tree, Archive)> {
    let mut sig = [0u8; 32];
    if raw.size() < 32 {
        return Err(Error::corrupted("not a 7z archive"));
    }
    raw.read_at(0, &mut sig)?;
    if sig[..6] != SIGNATURE {
        return Err(Error::corrupted("not a 7z archive"));
    }
    if dix_core::partition::crc32(&sig[12..32]) != le32(&sig, 8) {
        return Err(bad("start header checksum"));
    }
    let (offset, size, crc) = (le64(&sig, 12), le64(&sig, 20), le32(&sig, 28));
    if size == 0 {
        return Ok((Tree::new(), Archive::default())); // an empty archive
    }
    let at = 32u64.checked_add(offset).ok_or_else(|| bad("next header"))?;
    if size > 1 << 30 || at + size > raw.size() {
        return Err(Error::corrupted("the 7z header is outside the file (a truncated archive?)"));
    }
    let mut header = vec![0u8; size as usize];
    raw.read_at(at, &mut header)?;
    if dix_core::partition::crc32(&header) != crc {
        return Err(bad("header checksum"));
    }
    // A compressed header: its streams unpack to the real one.
    for _ in 0..4 {
        if header.first() != Some(&0x17) {
            break;
        }
        let mut r = R { b: &header, at: 1 };
        let s = streams(&mut r, 32)?;
        let f = s.folders.first().ok_or_else(|| bad("encoded header"))?;
        header = unpack(raw, f, passwords)?;
    }
    let mut r = R { b: &header, at: 0 };
    if r.number()? != 0x01 {
        return Err(bad("header"));
    }
    let mut main = Streams::default();
    let mut tree = Tree::new();
    loop {
        match r.number()? {
            0 => break,
            0x02 => loop {
                // Archive properties: skipped.
                if r.number()? == 0 {
                    break;
                }
                let n = r.number()?;
                r.bytes(n)?;
            },
            0x03 => {
                streams(&mut r, 32)?;
            }
            0x04 => {
                main = streams(&mut r, 32)?;
                // Each folder's files, for checking a password.
                for (f, subs) in main.folders.iter_mut().zip(&main.substreams) {
                    let mut at = 0;
                    for &(len, crc) in subs {
                        if let Some(crc) = crc {
                            f.checks.push((at, len, crc));
                        }
                        at += len;
                    }
                }
            }
            0x05 => files(&mut r, &main, &mut tree, log)?,
            _ => return Err(bad("header")),
        }
    }
    Ok((tree, Archive { folders: main.folders }))
}

fn files(r: &mut R<'_>, s: &Streams, tree: &mut Tree, log: &MountLog) -> Result<()> {
    let n = r.count()?;
    let mut names = vec![String::new(); n];
    let mut empty_stream = vec![false; n];
    let mut empty_file: Vec<bool> = Vec::new();
    let mut anti: Vec<bool> = Vec::new();
    let mut mtime = vec![None; n];
    let mut attrib = vec![None; n];
    loop {
        let id = r.number()?;
        if id == 0 {
            break;
        }
        let len = r.number()?;
        let mut p = R { b: r.bytes(len)?, at: 0 };
        let empties = empty_stream.iter().filter(|&&e| e).count();
        match id {
            0x0E => empty_stream = p.bits(n)?,
            0x0F => empty_file = p.bits(empties)?,
            0x10 => anti = p.bits(empties)?,
            0x11 => {
                if p.byte()? != 0 {
                    return Err(Error::unsupported("7z names stored elsewhere"));
                }
                let units: Vec<u16> = p.b[1..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                for (name, raw) in names.iter_mut().zip(units.split(|&u| u == 0)) {
                    *name = String::from_utf16_lossy(raw);
                }
            }
            0x14 => {
                let defined = p.defined(n)?;
                if p.byte()? != 0 {
                    return Err(Error::unsupported("7z times stored elsewhere"));
                }
                for (t, d) in mtime.iter_mut().zip(defined) {
                    if d {
                        *t = Some(filetime(p.u64()?));
                    }
                }
            }
            0x15 => {
                let defined = p.defined(n)?;
                if p.byte()? != 0 {
                    return Err(Error::unsupported("7z attributes stored elsewhere"));
                }
                for (a, d) in attrib.iter_mut().zip(defined) {
                    if d {
                        *a = Some(p.u32()?);
                    }
                }
            }
            _ => {} // creation/access times, start positions, padding: not needed
        }
    }
    // Files with data take the substreams in order.
    let mut streams = s.substreams.iter().enumerate().flat_map(|(f, subs)| {
        let mut offset = 0u64;
        subs.iter().map(move |&(size, crc)| {
            let item = (f, offset, size, crc);
            offset += size;
            item
        })
    });
    let (mut empty_index, mut skipped) = (0, 0);
    for i in 0..n {
        let (is_empty_file, is_anti) = if empty_stream[i] {
            let k = empty_index;
            empty_index += 1;
            (empty_file.get(k).copied().unwrap_or(false), anti.get(k).copied().unwrap_or(false))
        } else {
            (false, false)
        };
        let data =
            if empty_stream[i] { None } else { Some(streams.next().ok_or_else(|| bad("more files than data"))?) };
        if is_anti {
            continue;
        }
        let Some(path) = clean_path(&names[i].replace('\\', "/")).filter(|p| p != "/") else {
            skipped += 1;
            continue;
        };
        let a = attrib[i].unwrap_or(0);
        let unix = (a & 0x8000 != 0).then_some(a >> 16).filter(|&m| m != 0);
        let is_dir = (empty_stream[i] && !is_empty_file) || a & 0x10 != 0;
        let mut node = Node::new(if is_dir { EntryKind::Directory } else { EntryKind::File });
        node.mtime = mtime[i];
        node.mode = unix.map(|m| m & 0o7777);
        if let Some((folder, offset, size, crc)) = data {
            node.size = size;
            node.data = Data::SevenZ { folder, offset, len: size, crc };
        }
        if unix.is_some_and(|m| m & 0o170000 == 0o120000) {
            node.kind = EntryKind::Symlink;
        }
        if is_dir {
            tree.update_dir(&path, node);
        } else {
            tree.insert(&path, node);
        }
    }
    if skipped > 0 {
        log.warn(format!("Skipped {skipped} member(s) with unusable names."));
    }
    Ok(())
}

// ----- writing ----------------------------------------------------------------

/// A 7z number: the first byte's leading ones say how many bytes follow.
fn put_number(out: &mut Vec<u8>, v: u64) {
    let (mut first, mut mask, mut i) = (0u8, 0x80u8, 0);
    while i < 8 {
        if v < 1u64 << (7 * (i + 1)) {
            first |= (v >> (8 * i)) as u8;
            break;
        }
        first |= mask;
        mask >>= 1;
        i += 1;
    }
    out.push(first);
    for k in 0..i {
        out.push((v >> (8 * k)) as u8);
    }
}

/// A bit vector, most significant bit first.
fn put_bits(out: &mut Vec<u8>, bits: &[bool]) {
    for chunk in bits.chunks(8) {
        let mut b = 0u8;
        for (i, &x) in chunk.iter().enumerate() {
            if x {
                b |= 0x80 >> i;
            }
        }
        out.push(b);
    }
}

/// A property: its id, size and bytes.
fn put_property(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    put_number(out, body.len() as u64);
    out.extend_from_slice(body);
}

/// The LZMA2 coder's property: the dictionary size, as 2 or 3 times a power
/// of two.
fn lzma2_dict_prop(dict: u32) -> u8 {
    (0..40u8).find(|&p| dict <= (2 | (p as u32 & 1)) << (p / 2 + 11)).unwrap_or(40)
}

/// LZMA2 compression at `level` (1–9), with a dictionary no larger than
/// the data needs.
fn lzma2(data: &[u8], level: u32) -> Result<(Vec<u8>, u8)> {
    use std::io::Write;
    let mut opts = lzma_rust2::Lzma2Options::with_preset(level.clamp(1, 9));
    let dict = (data.len() as u64).clamp(1 << 16, opts.lzma_options.dict_size as u64) as u32;
    opts.lzma_options.dict_size = dict;
    let io = |e: std::io::Error| Error::new(dix_core::ErrorKind::Io, format!("LZMA2 compression: {e}"));
    let mut w = lzma_rust2::Lzma2Writer::new(Vec::new(), opts);
    w.write_all(data).map_err(io)?;
    let out = w.finish().map_err(|e| Error::new(dix_core::ErrorKind::Io, format!("LZMA2 compression: {e:?}")))?;
    Ok((out, lzma2_dict_prop(dict)))
}

/// Writing with a password: AES-256 (7-Zip's key derivation, 2^19
/// rounds); `names`: the header (the list of files) too.
pub(super) struct Encrypt<'a> {
    pub password: &'a str,
    pub names: bool,
}

/// One block: LZMA2, then AES with a password. Returns the packed bytes,
/// the folder's description (coders and how they connect) and the size of
/// each coder's output.
fn pack_block(data: &[u8], level: u32, password: Option<&str>) -> Result<(Vec<u8>, Vec<u8>, Vec<u64>)> {
    let (lz, prop) = lzma2(data, level)?;
    let mut folder = Vec::new();
    let Some(password) = password else {
        put_number(&mut folder, 1);
        folder.extend_from_slice(&[0x21, 0x21]);
        put_number(&mut folder, 1);
        folder.push(prop);
        return Ok((lz, folder, vec![data.len() as u64]));
    };
    #[cfg(feature = "archive-aes")]
    {
        // Coder 0, LZMA2, gives the output; its input is coder 1's (AES)
        // output; AES reads the packed stream.
        const CYCLES: u32 = 19;
        let iv: [u8; 16] = super::crypto::random_bytes();
        let key = super::crypto::key_7z(password, &[], CYCLES);
        let mut packed = lz.clone();
        packed.resize(packed.len().next_multiple_of(16), 0);
        super::crypto::aes256_cbc_encrypt(&key, &iv, &mut packed);
        put_number(&mut folder, 2);
        folder.extend_from_slice(&[0x21, 0x21]);
        put_number(&mut folder, 1);
        folder.push(prop);
        folder.push(0x20 | AES.len() as u8);
        folder.extend_from_slice(&AES);
        let mut props = vec![CYCLES as u8 | 0x40, 0x0F];
        props.extend_from_slice(&iv);
        put_number(&mut folder, props.len() as u64);
        folder.extend_from_slice(&props);
        put_number(&mut folder, 0); // in stream 0 (LZMA2's) ...
        put_number(&mut folder, 1); // ... from out stream 1 (AES's)
        Ok((packed, folder, vec![data.len() as u64, lz.len() as u64]))
    }
    #[cfg(not(feature = "archive-aes"))]
    {
        let _ = (password, lz);
        Err(codec::left_out("AES encryption"))
    }
}

/// Pack info and unpack info for one block (at `pack_pos` after the
/// signature header), with the CRC of its output if given.
fn put_block_info(h: &mut Vec<u8>, pack_pos: u64, packed: u64, folder: &[u8], sizes: &[u64], crc: Option<u32>) {
    h.push(0x06);
    put_number(h, pack_pos);
    put_number(h, 1);
    h.push(0x09);
    put_number(h, packed);
    h.push(0x00);
    h.push(0x07);
    h.push(0x0B);
    put_number(h, 1);
    h.push(0x00);
    h.extend_from_slice(folder);
    h.push(0x0C);
    for &s in sizes {
        put_number(h, s);
    }
    if let Some(c) = crc {
        h.push(0x0A);
        h.push(1);
        h.extend_from_slice(&c.to_le_bytes());
    }
    h.push(0x00);
}

/// Unix seconds as a Windows FILETIME.
fn to_filetime(t: i64) -> u64 {
    ((t + 11_644_473_600).max(0) as u64) * 10_000_000
}

/// Serialises `tree` as a 7z archive: one solid LZMA2 block for all file
/// data, and a plain header (names, times, Unix modes, empty files and
/// folders, symlinks as their target). `content` gives a file's bytes;
/// `level`: LZMA2's 1 (fastest) to 9 (smallest); `encrypt`: a password.
pub(super) fn write(
    tree: &Tree,
    content: &mut dyn FnMut(&str, &Node) -> Result<Vec<u8>>,
    level: u32,
    encrypt: Option<Encrypt<'_>>,
) -> Result<Vec<u8>> {
    let password = encrypt.as_ref().map(|e| e.password);
    struct Item {
        name: String,
        data: Option<Vec<u8>>,
        dir: bool,
        mtime: Option<i64>,
        attrib: u32,
    }
    let mut items = Vec::new();
    for (path, node) in tree.iter() {
        if path == "/" || node.kind == EntryKind::Other {
            continue;
        }
        let (type_bits, default_mode) = match node.kind {
            EntryKind::Directory => (0o040000, 0o755),
            EntryKind::Symlink => (0o120000, 0o777),
            _ => (0o100000, 0o644),
        };
        let mode = type_bits | node.mode.unwrap_or(default_mode) & 0o7777;
        let dir = node.kind == EntryKind::Directory;
        // Windows attributes (directory) and the Unix mode in the high half.
        let attrib = (mode << 16) | 0x8000 | if dir { 0x10 } else { 0x20 };
        let data = match node.kind {
            EntryKind::Directory => None,
            EntryKind::Symlink => Some(node.link.clone().unwrap_or_default().into_bytes()),
            _ => Some(content(path, node)?),
        };
        let data = data.filter(|d| !d.is_empty());
        items.push(Item { name: path[1..].to_owned(), data, dir, mtime: node.mtime, attrib });
    }
    if items.is_empty() {
        // An empty archive: no header at all.
        let mut out = SIGNATURE.to_vec();
        out.extend_from_slice(&[0, 4]);
        let start = [0u8; 20];
        out.extend_from_slice(&dix_core::partition::crc32(&start).to_le_bytes());
        out.extend_from_slice(&start);
        return Ok(out);
    }
    // The data, one after another, compressed as one block.
    let mut unpacked = Vec::new();
    let mut sizes = Vec::new();
    let mut crcs = Vec::new();
    for it in &items {
        if let Some(d) = &it.data {
            unpacked.extend_from_slice(d);
            sizes.push(d.len() as u64);
            crcs.push(dix_core::partition::crc32(d));
        }
    }
    let mut h = vec![0x01]; // header
    let packed = if unpacked.is_empty() {
        Vec::new()
    } else {
        let (packed, folder, out_sizes) = pack_block(&unpacked, level, password)?;
        h.push(0x04); // main streams: one block
        put_block_info(&mut h, 0, packed.len() as u64, &folder, &out_sizes, None);
        h.push(0x08); // substreams: the files in the block, and their CRCs
        h.push(0x0D);
        put_number(&mut h, sizes.len() as u64);
        if sizes.len() > 1 {
            h.push(0x09);
            for &s in &sizes[..sizes.len() - 1] {
                put_number(&mut h, s);
            }
        }
        h.push(0x0A);
        h.push(1);
        for c in &crcs {
            h.extend_from_slice(&c.to_le_bytes());
        }
        h.push(0x00);
        h.push(0x00);
        packed
    };
    h.push(0x05); // files
    put_number(&mut h, items.len() as u64);
    let empty: Vec<bool> = items.iter().map(|it| it.data.is_none()).collect();
    if empty.iter().any(|&e| e) {
        let mut b = Vec::new();
        put_bits(&mut b, &empty);
        put_property(&mut h, 0x0E, &b);
        let files: Vec<bool> = items.iter().filter(|it| it.data.is_none()).map(|it| !it.dir).collect();
        let mut b = Vec::new();
        put_bits(&mut b, &files);
        put_property(&mut h, 0x0F, &b);
    }
    let mut names = vec![0u8];
    for it in &items {
        for u in it.name.encode_utf16() {
            names.extend_from_slice(&u.to_le_bytes());
        }
        names.extend_from_slice(&[0, 0]);
    }
    put_property(&mut h, 0x11, &names);
    let defined: Vec<bool> = items.iter().map(|it| it.mtime.is_some()).collect();
    if defined.iter().any(|&d| d) {
        let mut b = Vec::new();
        if defined.iter().all(|&d| d) {
            b.push(1);
        } else {
            b.push(0);
            put_bits(&mut b, &defined);
        }
        b.push(0); // not external
        for it in &items {
            if let Some(t) = it.mtime {
                b.extend_from_slice(&to_filetime(t).to_le_bytes());
            }
        }
        put_property(&mut h, 0x14, &b);
    }
    let mut b = vec![1, 0];
    for it in &items {
        b.extend_from_slice(&it.attrib.to_le_bytes());
    }
    put_property(&mut h, 0x15, &b);
    h.push(0x00); // end of files
    h.push(0x00); // end of header
    // An encrypted list of files: the header packed like a block, and an
    // "encoded header" saying where it is (its CRC tells a wrong password).
    let mut packed = packed;
    if let Some(e) = encrypt.as_ref().filter(|e| e.names) {
        let (hp, folder, sizes) = pack_block(&h, level, Some(e.password))?;
        let mut eh = vec![0x17];
        put_block_info(
            &mut eh,
            packed.len() as u64,
            hp.len() as u64,
            &folder,
            &sizes,
            Some(dix_core::partition::crc32(&h)),
        );
        eh.push(0x00);
        packed.extend_from_slice(&hp);
        h = eh;
    }
    // The signature header: version 0.4, then where the header is.
    let mut start = Vec::with_capacity(20);
    start.extend_from_slice(&(packed.len() as u64).to_le_bytes());
    start.extend_from_slice(&(h.len() as u64).to_le_bytes());
    start.extend_from_slice(&dix_core::partition::crc32(&h).to_le_bytes());
    let mut out = SIGNATURE.to_vec();
    out.extend_from_slice(&[0, 4]);
    out.extend_from_slice(&dix_core::partition::crc32(&start).to_le_bytes());
    out.extend_from_slice(&start);
    out.extend_from_slice(&packed);
    out.extend_from_slice(&h);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        for (bytes, v) in [
            (&[0x05][..], 5u64),
            (&[0x80, 0x80][..], 0x80),
            (&[0xBF, 0xFF][..], 0x3FFF),
            (&[0xC1, 0x34, 0x12][..], 0x01_1234),
            (&[0xFF, 1, 2, 3, 4, 5, 6, 7, 8][..], 0x0807_0605_0403_0201),
        ] {
            assert_eq!(R { b: bytes, at: 0 }.number().unwrap(), v, "{bytes:02X?}");
        }
    }

    #[test]
    fn written_numbers_read_back() {
        for v in [0u64, 1, 127, 128, 0x3FFF, 0x4000, 1 << 20, 1 << 35, u64::MAX] {
            let mut b = Vec::new();
            put_number(&mut b, v);
            assert_eq!(R { b: &b, at: 0 }.number().unwrap(), v, "{v}");
        }
    }
}
