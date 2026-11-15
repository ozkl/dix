//! Finding photos and videos by their content ("carving", as PhotoRec
//! does): every sector of a disk is looked at for the start of a known
//! file, whatever file system there is or was — after formatting, or when
//! the file system is too damaged to open.
//!
//! A start is only a candidate: the file's own structure is followed to
//! find where it ends (JPEG segments and the entropy-coded data up to its
//! end marker; PNG chunks up to IEND; GIF blocks up to the trailer; TIFF
//! and camera RAW directories with the strips, tiles and previews they
//! point to; ISO media boxes for HEIC, CR3, MP4 and MOV; RIFF for WebP and
//! AVI). A candidate whose structure does not hold up is dropped. The scan
//! goes on after the end of each file found, so the thumbnails and
//! previews inside one are not listed again. Files are taken to be stored
//! in one piece (as on most cameras' cards); a photo's EXIF date becomes
//! its time.

use super::{Chance, Found, Recovered, Runs};
use dix_core::{BlockDevice, Result};

const SECTOR: u64 = 512;
const CHUNK: usize = 4 << 20;

/// What a found file is.
struct Kind {
    folder: &'static str,
    ext: &'static str,
}

/// A file found: where, how long, what, its date.
struct Hit {
    offset: u64,
    len: u64,
    kind: Kind,
    date: Option<i64>,
}

/// Reads the device through a cache of one chunk.
struct Reader<'a> {
    dev: &'a mut dyn BlockDevice,
    at: u64,
    buf: Vec<u8>,
}

impl Reader<'_> {
    fn bytes(&mut self, pos: u64, n: usize) -> Option<&[u8]> {
        let size = self.dev.size();
        if pos.checked_add(n as u64)? > size {
            return None;
        }
        if pos < self.at || pos + n as u64 > self.at + self.buf.len() as u64 {
            let len = ((size - pos) as usize).min(CHUNK.max(n));
            self.buf.resize(len, 0);
            self.dev.read_at(pos, &mut self.buf).ok()?;
            self.at = pos;
        }
        let from = (pos - self.at) as usize;
        Some(&self.buf[from..from + n])
    }

    fn u8(&mut self, pos: u64) -> Option<u8> {
        self.bytes(pos, 1).map(|b| b[0])
    }

    fn be16(&mut self, pos: u64) -> Option<u16> {
        self.bytes(pos, 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }

    fn be32(&mut self, pos: u64) -> Option<u32> {
        self.bytes(pos, 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    }

    fn be64(&mut self, pos: u64) -> Option<u64> {
        self.bytes(pos, 8).map(|b| u64::from_be_bytes(b.try_into().unwrap()))
    }
}

/// Scans `dev` for photos and videos; `progress(done, total)` returns
/// false to stop (what was found so far is kept).
pub fn scan(mut dev: Box<dyn BlockDevice>, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<Recovered> {
    let size = dev.size();
    let mut hits = Vec::new();
    let mut pos = 0u64;
    let mut chunk = vec![0u8; CHUNK];
    'scan: while pos < size {
        let n = ((size - pos) as usize).min(CHUNK);
        dev.read_at(pos, &mut chunk[..n])?;
        let mut s = 0usize;
        while s + 16 <= n {
            if let Some(start) = candidate(&chunk[s..s + 16]) {
                let at = pos + s as u64;
                let mut r = Reader { dev: dev.as_mut(), at: u64::MAX, buf: Vec::new() };
                if let Some(hit) = measure(&mut r, at, start) {
                    // On after it (what is inside is not listed again).
                    let next = (at + hit.len).next_multiple_of(SECTOR);
                    hits.push(hit);
                    if !progress(next.min(size), size) {
                        break 'scan;
                    }
                    pos = next;
                    continue 'scan;
                }
            }
            s += SECTOR as usize;
        }
        pos += n as u64;
        if !progress(pos.min(size), size) {
            break;
        }
    }
    let mut found = Vec::new();
    let mut files = Vec::new();
    for h in hits {
        let sector = h.offset / SECTOR;
        let name = match h.date {
            Some(t) => {
                let (y, mo, d, hh, mi, ss) = dix_core::time::civil_from_unix(t);
                format!("{y:04}-{mo:02}-{d:02} {hh:02}.{mi:02}.{ss:02} (f{sector}).{}", h.kind.ext)
            }
            None => format!("f{sector}.{}", h.kind.ext),
        };
        found.push(Found {
            path: vec![h.kind.folder.to_owned(), name],
            dir: false,
            size: h.len,
            modified: h.date,
            created: None,
            chance: Chance::Carved,
            id: files.len(),
        });
        files.push(vec![(Some(h.offset), h.len)]);
    }
    Ok(Recovered::with_title("Photo scan", "files found by content", found, Box::new(Runs { dev, files })))
}

/// What could start at the beginning of a sector.
#[derive(Clone, Copy)]
enum Start {
    Jpeg,
    Png,
    Gif,
    Tiff,
    Media,
    Riff,
}

fn candidate(h: &[u8]) -> Option<Start> {
    match h {
        [0xFF, 0xD8, 0xFF, m, ..] if (0xC0..=0xFE).contains(m) => Some(Start::Jpeg),
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some(Start::Png),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some(Start::Gif),
        [b'I', b'I', 0x2A, 0x00, ..] | [b'M', b'M', 0x00, 0x2A, ..] => Some(Start::Tiff),
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => Some(Start::Media),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..]
        | [b'R', b'I', b'F', b'F', _, _, _, _, b'A', b'V', b'I', b' ', ..] => Some(Start::Riff),
        _ => None,
    }
}

fn measure(r: &mut Reader, at: u64, start: Start) -> Option<Hit> {
    match start {
        Start::Jpeg => jpeg(r, at),
        Start::Png => png(r, at),
        Start::Gif => gif(r, at),
        Start::Tiff => tiff(r, at),
        Start::Media => media(r, at),
        Start::Riff => riff(r, at),
    }
}

const MAX_PHOTO: u64 = 256 << 20;

/// JPEG: segments up to the start of scan, the coded data up to the next
/// marker, … up to the end-of-image marker.
fn jpeg(r: &mut Reader, at: u64) -> Option<Hit> {
    let mut pos = at + 2;
    let mut date = None;
    let mut scans = 0;
    loop {
        if pos - at > MAX_PHOTO {
            return None;
        }
        if r.u8(pos)? != 0xFF {
            return None;
        }
        // Fill bytes.
        let mut m = r.u8(pos + 1)?;
        while m == 0xFF {
            pos += 1;
            m = r.u8(pos + 1)?;
        }
        match m {
            0xD9 if scans > 0 => {
                let len = pos + 2 - at;
                return Some(Hit { offset: at, len, kind: Kind { folder: "JPEG", ext: "jpg" }, date });
            }
            0xD0..=0xD7 | 0x01 => pos += 2,
            0x00 | 0xD8 | 0xD9 => return None,
            _ => {
                let len = r.be16(pos + 2)? as u64;
                if len < 2 {
                    return None;
                }
                if m == 0xE1 && date.is_none() && r.bytes(pos + 4, 6)? == b"Exif\0\0" {
                    let n = (len as usize).saturating_sub(8).min(1 << 16);
                    let exif = r.bytes(pos + 10, n)?.to_vec();
                    date = exif_date(&exif);
                }
                pos += 2 + len;
                if m == 0xDA {
                    scans += 1;
                    // The coded data: up to an FF that is not stuffing (00)
                    // or a restart marker.
                    loop {
                        if pos - at > MAX_PHOTO {
                            return None;
                        }
                        let left = r.dev.size().saturating_sub(pos).min(MAX_PHOTO - (pos - at));
                        if left < 2 {
                            return None;
                        }
                        let b = r.bytes(pos, 4096.min(left as usize))?.to_vec();
                        let mut found = None;
                        for i in 0..b.len().saturating_sub(1) {
                            if b[i] == 0xFF && b[i + 1] != 0 && !(0xD0..=0xD7).contains(&b[i + 1]) && b[i + 1] != 0xFF {
                                found = Some(i);
                                break;
                            }
                        }
                        match found {
                            Some(i) => {
                                pos += i as u64;
                                break;
                            }
                            None => pos += (b.len() - 1) as u64,
                        }
                    }
                }
            }
        }
    }
}

/// The date a photo was taken, from EXIF (a TIFF structure).
fn exif_date(t: &[u8]) -> Option<i64> {
    let le = match t.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| {
        t.get(o..o + 2).map(|b| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
    };
    let u32_at = |o: usize| {
        t.get(o..o + 4).map(|b| {
            let a: [u8; 4] = b.try_into().unwrap();
            if le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
        })
    };
    let entries = |ifd: usize| -> Vec<(u16, usize)> {
        let n = u16_at(ifd).unwrap_or(0) as usize;
        (0..n.min(256)).filter_map(|k| Some((u16_at(ifd + 2 + 12 * k)?, ifd + 2 + 12 * k))).collect()
    };
    let text = |e: usize| -> Option<i64> {
        let off = u32_at(e + 8)? as usize;
        parse_exif_time(t.get(off..off + 19)?)
    };
    let ifd0 = u32_at(4)? as usize;
    let mut fallback = None;
    for (tag, e) in entries(ifd0) {
        match tag {
            0x0132 => fallback = text(e),
            0x8769 => {
                let sub = u32_at(e + 8)? as usize;
                for (tag, e) in entries(sub) {
                    if tag == 0x9003
                        && let Some(t) = text(e)
                    {
                        return Some(t);
                    }
                }
            }
            _ => {}
        }
    }
    fallback
}

/// "YYYY:MM:DD HH:MM:SS" (local time, taken as is).
fn parse_exif_time(s: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(s).ok()?;
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<u32>().ok();
    let (y, mo, d, h, mi, se) = (n(0, 4)?, n(5, 7)?, n(8, 10)?, n(11, 13)?, n(14, 16)?, n(17, 19)?);
    if !(1990..=2100).contains(&y) || !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60
    {
        return None;
    }
    Some(dix_core::time::days_from_civil(y as i64, mo, d) * 86400 + (h * 3600 + mi * 60 + se) as i64)
}

/// PNG: chunks up to IEND.
fn png(r: &mut Reader, at: u64) -> Option<Hit> {
    let mut pos = at + 8;
    let mut first = true;
    loop {
        let len = r.be32(pos)? as u64;
        let ty = r.bytes(pos + 4, 4)?.to_vec();
        if !ty.iter().all(|c| c.is_ascii_alphabetic()) || (first && &ty != b"IHDR") || len > MAX_PHOTO {
            return None;
        }
        first = false;
        pos += 12 + len;
        if pos - at > MAX_PHOTO {
            return None;
        }
        if &ty == b"IEND" {
            return Some(Hit { offset: at, len: pos - at, kind: Kind { folder: "PNG", ext: "png" }, date: None });
        }
    }
}

/// GIF: header, colour table, then blocks up to the trailer (0x3B).
fn gif(r: &mut Reader, at: u64) -> Option<Hit> {
    let flags = r.u8(at + 10)?;
    let mut pos = at + 13;
    if flags & 0x80 != 0 {
        pos += 3 << ((flags & 7) + 1);
    }
    let sub_blocks = |r: &mut Reader, mut pos: u64| -> Option<u64> {
        loop {
            let n = r.u8(pos)? as u64;
            pos += 1 + n;
            if n == 0 {
                return Some(pos);
            }
        }
    };
    let mut images = 0;
    loop {
        if pos - at > MAX_PHOTO {
            return None;
        }
        match r.u8(pos)? {
            0x3B if images > 0 => {
                return Some(Hit {
                    offset: at,
                    len: pos + 1 - at,
                    kind: Kind { folder: "GIF", ext: "gif" },
                    date: None,
                });
            }
            0x21 => pos = sub_blocks(r, pos + 2)?,
            0x2C => {
                let f = r.u8(pos + 9)?;
                pos += 10;
                if f & 0x80 != 0 {
                    pos += 3 << ((f & 7) + 1);
                }
                let code = r.u8(pos)?;
                if !(2..=12).contains(&code) {
                    return None;
                }
                pos = sub_blocks(r, pos + 1)?;
                images += 1;
            }
            _ => return None,
        }
    }
}

/// TIFF and the camera RAW files built on it: the end of the furthest
/// thing its directories point to.
fn tiff(r: &mut Reader, at: u64) -> Option<Hit> {
    let head = r.bytes(at, 16)?.to_vec();
    let le = &head[..2] == b"II";
    let rd16 = |r: &mut Reader, o: u64| {
        r.bytes(at + o, 2).map(|b| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
    };
    let rd32 = |r: &mut Reader, o: u64| {
        r.bytes(at + o, 4).map(|b| {
            let a: [u8; 4] = b.try_into().unwrap();
            if le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
        })
    };
    let mut end = 8u64;
    let mut make = String::new();
    let mut dng = false;
    let mut width = false;
    let mut date = None;
    let mut todo = vec![rd32(r, 4)? as u64];
    let mut seen = 0;
    while let Some(ifd) = todo.pop() {
        seen += 1;
        if seen > 64 {
            return None;
        }
        if !(8..=MAX_PHOTO).contains(&ifd) {
            continue;
        }
        let n = rd16(r, ifd)? as u64;
        if n == 0 || n > 1000 {
            return None;
        }
        end = end.max(ifd + 2 + 12 * n + 4);
        let mut strips = (Vec::new(), Vec::new());
        let mut jpeg = (None, None);
        for k in 0..n {
            let e = ifd + 2 + 12 * k;
            let (tag, ty, count) = (rd16(r, e)?, rd16(r, e + 2)?, rd32(r, e + 4)? as u64);
            let unit = match ty {
                1 | 2 | 6 | 7 => 1,
                3 | 8 => 2,
                4 | 9 | 11 | 13 => 4,
                5 | 10 | 12 => 8,
                _ => return None,
            };
            let bytes = unit * count;
            let value_at = if bytes <= 4 { e + 8 } else { rd32(r, e + 8)? as u64 };
            if bytes > 4 {
                end = end.max(value_at + bytes);
            }
            let values = |r: &mut Reader| -> Option<Vec<u64>> {
                (0..count.min(1 << 16))
                    .map(|i| {
                        if unit == 2 {
                            rd16(r, value_at + 2 * i).map(u64::from)
                        } else {
                            rd32(r, value_at + 4 * i).map(u64::from)
                        }
                    })
                    .collect()
            };
            match tag {
                0x0100 => width = true,
                0x010F => {
                    let b = r.bytes(at + value_at, count.min(32) as usize)?;
                    make = String::from_utf8_lossy(b).trim_end_matches('\0').to_owned();
                }
                0x0132 if count >= 19 => date = date.or_else(|| parse_exif_time(r.bytes(at + value_at, 19)?)),
                0x0111 | 0x0144 => strips.0 = values(r)?,
                0x0117 | 0x0145 => strips.1 = values(r)?,
                0x0201 => jpeg.0 = values(r)?.first().copied(),
                0x0202 => jpeg.1 = values(r)?.first().copied(),
                0x014A | 0x8769 => todo.extend(values(r)?),
                0xC612 => dng = true,
                _ => {}
            }
        }
        for (o, l) in strips.0.iter().zip(&strips.1) {
            end = end.max(o + l);
        }
        if let (Some(o), Some(l)) = jpeg {
            end = end.max(o + l);
        }
        let next = rd32(r, ifd + 2 + 12 * n)? as u64;
        if next != 0 {
            todo.push(next);
        }
    }
    if !width || end > MAX_PHOTO {
        return None;
    }
    let make = make.to_ascii_uppercase();
    let (folder, ext) = if &head[8..10] == b"CR" {
        ("RAW", "cr2")
    } else if dng {
        ("RAW", "dng")
    } else if make.starts_with("NIKON") {
        ("RAW", "nef")
    } else if make.starts_with("SONY") {
        ("RAW", "arw")
    } else if make.starts_with("OLYMPUS") || make.starts_with("OM DIGITAL") {
        ("RAW", "orf")
    } else if make.starts_with("PENTAX") || make.starts_with("RICOH") {
        ("RAW", "pef")
    } else {
        ("TIFF", "tif")
    };
    Some(Hit { offset: at, len: end, kind: Kind { folder, ext }, date })
}

/// ISO media files (HEIC, AVIF, CR3, MP4, MOV, 3GP): top-level boxes,
/// as long as they are boxes these files have.
fn media(r: &mut Reader, at: u64) -> Option<Hit> {
    let brand = r.bytes(at + 8, 4)?.to_vec();
    let (folder, ext, video) = match &brand[..] {
        b"heic" | b"heix" | b"hevc" | b"heim" | b"heis" | b"mif1" | b"msf1" => ("HEIC", "heic", false),
        b"avif" | b"avis" => ("HEIC", "avif", false),
        b"crx " => ("RAW", "cr3", true),
        b"qt  " => ("Video", "mov", true),
        b"3gp4" | b"3gp5" | b"3gp6" | b"3g2a" => ("Video", "3gp", true),
        b"isom" | b"iso2" | b"iso4" | b"iso5" | b"iso6" | b"mp41" | b"mp42" | b"avc1" | b"M4V " | b"dash" => {
            ("Video", "mp4", true)
        }
        _ => return None,
    };
    const TOP: [&[u8; 4]; 18] = [
        b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"uuid", b"meta", b"pnot", b"PICT", b"pdin", b"moof",
        b"mfra", b"sidx", b"styp", b"prft", b"emsg", b"idat",
    ];
    let mut pos = at;
    let (mut has_index, mut has_data) = (false, false);
    loop {
        let size = r.be32(pos)? as u64;
        let ty = r.bytes(pos + 4, 4).map(|b| b.to_vec());
        let Some(ty) = ty.filter(|t| TOP.iter().any(|k| k[..] == t[..])) else { break };
        let len = match size {
            1 => r.be64(pos + 8)?,
            0 => return None, // "to the end of the file": no end to find
            s => s,
        };
        if len < 8 || (pos == at && &ty != b"ftyp") {
            break;
        }
        match &ty[..] {
            b"moov" | b"meta" | b"moof" => has_index = true,
            b"mdat" | b"idat" => has_data = true,
            _ => {}
        }
        pos += len;
        if pos > r.dev.size() {
            return None;
        }
    }
    // A photo has its items in "meta" (and data in mdat); a video its index
    // in "moov" and data in "mdat".
    if !has_index || (video && !has_data) || pos == at {
        return None;
    }
    Some(Hit { offset: at, len: pos - at, kind: Kind { folder, ext }, date: None })
}

/// RIFF: WebP and AVI (their length is in the header).
fn riff(r: &mut Reader, at: u64) -> Option<Hit> {
    let b = r.bytes(at, 12)?.to_vec();
    let len = u32::from_le_bytes(b[4..8].try_into().unwrap()) as u64 + 8;
    if len < 20 || at + len > r.dev.size() {
        return None;
    }
    let (folder, ext) = if &b[8..12] == b"WEBP" { ("WebP", "webp") } else { ("Video", "avi") };
    Some(Hit { offset: at, len: len + (len & 1), kind: Kind { folder, ext }, date: None })
}

#[cfg(test)]
pub(crate) fn parse_exif_time_for_test(s: &[u8]) -> Option<i64> {
    parse_exif_time(s)
}
