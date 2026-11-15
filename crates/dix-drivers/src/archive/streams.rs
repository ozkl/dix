//! Decoders for the less common compressions, reading a stream and writing
//! what they unpack as they go: LZ4 frames (and the legacy LZ4 format),
//! Unix `compress` (`.Z`, LZW), lzip (`.lz`), legacy LZMA (`.lzma`) and
//! Apple's pbzx (xz chunks; macOS installer payloads).

use dix_core::{Error, ErrorKind, Result};
use std::io::{BufRead, Read, Write};

fn bad(what: &str, e: impl std::fmt::Display) -> Error {
    Error::corrupted(format!("{what} data: {e}"))
}

/// An error of the sink (a disk, a full buffer) as it is; of the input as
/// damaged data.
fn sink(e: std::io::Error) -> Error {
    Error::new(ErrorKind::Io, e.to_string())
}

fn read_exact(input: &mut dyn BufRead, buf: &mut [u8], what: &str) -> Result<()> {
    input.read_exact(buf).map_err(|_| bad(what, "it ends early"))
}

/// Whether the input has nothing more.
fn at_end(input: &mut dyn BufRead) -> Result<bool> {
    Ok(input.fill_buf().map_err(|e| bad("input", e))?.is_empty())
}

// ----- LZ4 ---------------------------------------------------------------------

const LZ4_MAGIC: u32 = 0x184D_2204;
const LZ4_LEGACY: u32 = 0x184C_2102;

/// LZ4 frames one after another (skippable frames skipped), or the legacy
/// format (8 MiB blocks, each on its own).
pub fn lz4(input: &mut dyn BufRead, out: &mut dyn Write) -> Result<()> {
    let mut first = true;
    while !at_end(input)? {
        let mut m = [0u8; 4];
        read_exact(input, &mut m, "lz4")?;
        let magic = u32::from_le_bytes(m);
        if magic & 0xFFFF_FFF0 == 0x184D_2A50 {
            let mut len = [0u8; 4];
            read_exact(input, &mut len, "lz4")?;
            std::io::copy(&mut input.take(u32::from_le_bytes(len) as u64), &mut std::io::sink())
                .map_err(|e| bad("lz4", e))?;
        } else if magic == LZ4_LEGACY {
            lz4_legacy(input, out)?;
        } else if magic == LZ4_MAGIC {
            lz4_frame(input, out)?;
        } else if first {
            return Err(bad("lz4", "not an LZ4 frame"));
        } else {
            break; // trailing bytes
        }
        first = false;
    }
    Ok(())
}

fn lz4_frame(input: &mut dyn BufRead, out: &mut dyn Write) -> Result<()> {
    let mut d = [0u8; 2];
    read_exact(input, &mut d, "lz4")?;
    let (flg, bd) = (d[0], d[1]);
    if flg >> 6 != 1 {
        return Err(bad("lz4", "unknown frame version"));
    }
    let linked = flg & 0x20 == 0;
    let block_sums = flg & 0x10 != 0;
    let mut skip = 1 + if flg & 0x08 != 0 { 8 } else { 0 } + if flg & 0x01 != 0 { 4 } else { 0 };
    let max = match (bd >> 4) & 7 {
        4 => 64 << 10,
        5 => 256 << 10,
        6 => 1 << 20,
        7 => 4 << 20,
        _ => return Err(bad("lz4", "unknown block size")),
    };
    let mut tmp = [0u8; 13];
    while skip > 0 {
        let n = skip.min(tmp.len());
        read_exact(input, &mut tmp[..n], "lz4")?;
        skip -= n;
    }
    // Linked blocks reach back up to 64 KiB into the ones before.
    let mut window: Vec<u8> = Vec::new();
    let mut packed = Vec::new();
    loop {
        let mut s = [0u8; 4];
        read_exact(input, &mut s, "lz4")?;
        let size = u32::from_le_bytes(s);
        if size == 0 {
            break;
        }
        let stored = size & 0x8000_0000 != 0;
        let len = (size & 0x7FFF_FFFF) as usize;
        if len > max {
            return Err(bad("lz4", "a block is larger than the frame allows"));
        }
        packed.resize(len, 0);
        read_exact(input, &mut packed, "lz4")?;
        if block_sums {
            read_exact(input, &mut s, "lz4")?;
        }
        let keep = if linked { window.len().min(64 << 10) } else { 0 };
        window.drain(..window.len() - keep);
        let start = window.len();
        if stored {
            window.extend_from_slice(&packed);
        } else {
            crate::lz4::block_into(&packed, &mut window, max)?;
        }
        out.write_all(&window[start..]).map_err(sink)?;
    }
    if flg & 0x04 != 0 {
        let mut s = [0u8; 4];
        read_exact(input, &mut s, "lz4")?;
    }
    Ok(())
}

fn lz4_legacy(input: &mut dyn BufRead, out: &mut dyn Write) -> Result<()> {
    let mut packed = Vec::new();
    loop {
        if at_end(input)? {
            return Ok(());
        }
        // A block's size, or the next frame's magic number.
        let head = input.fill_buf().map_err(|e| bad("lz4", e))?;
        if head.len() >= 4 {
            let v = u32::from_le_bytes(head[..4].try_into().unwrap());
            if v == LZ4_MAGIC || v == LZ4_LEGACY || v & 0xFFFF_FFF0 == 0x184D_2A50 {
                return Ok(());
            }
        }
        let mut s = [0u8; 4];
        read_exact(input, &mut s, "lz4")?;
        let len = u32::from_le_bytes(s) as usize;
        if len > 8 << 20 {
            return Err(bad("lz4", "a legacy block is too large"));
        }
        packed.resize(len, 0);
        read_exact(input, &mut packed, "lz4")?;
        out.write_all(&crate::lz4::block(&packed, 8 << 20)?).map_err(sink)?;
    }
}

// ----- compress (.Z) -------------------------------------------------------------

/// Unix `compress`: LZW with codes of 9 up to 16 bits, least significant
/// bit first. The writer puts codes out in groups of eight; when the code
/// width changes (or the table is cleared) the rest of a group is padding.
pub fn compress(input: &mut dyn BufRead, out: &mut dyn Write) -> Result<()> {
    let mut h = [0u8; 3];
    read_exact(input, &mut h, "compress")?;
    if h[..2] != [0x1F, 0x9D] {
        return Err(bad("compress", "not compressed with compress"));
    }
    let max_bits = (h[2] & 0x1F) as u32;
    let block_mode = h[2] & 0x80 != 0;
    if !(9..=16).contains(&max_bits) {
        return Err(bad("compress", format!("{max_bits}-bit codes")));
    }
    let mut data = Vec::new();
    input.read_to_end(&mut data).map_err(|e| bad("compress", e))?;
    const CLEAR: usize = 256;
    let first = if block_mode { 257 } else { 256 };
    // The table: each code's previous code and last byte.
    let mut prefix = vec![0u32; 1 << 16];
    let mut suffix = vec![0u8; 1 << 16];
    for (i, s) in suffix.iter_mut().enumerate().take(256) {
        *s = i as u8;
    }
    let mut bits = 9u32;
    let mut next = first;
    let mut pos = 0usize; // in bits
    let mut group_start = 0usize; // bit position where the current width began
    let mut old: Option<usize> = None;
    let mut first_byte = 0u8;
    let mut stack = Vec::with_capacity(1 << 16);
    let mut buf = Vec::with_capacity(1 << 16);
    let total = data.len() * 8;
    // Skips to the end of the current group of eight codes.
    let realign = |pos: usize, start: usize, bits: u32| -> usize {
        let group = 8 * bits as usize;
        let used = pos - start;
        start + used.div_ceil(group) * group
    };
    loop {
        if next > (1usize << bits) - 1 && bits < max_bits {
            pos = realign(pos, group_start, bits);
            bits += 1;
            group_start = pos;
        }
        if pos + bits as usize > total {
            break;
        }
        let byte = pos / 8;
        let mut word = 0u32;
        for k in 0..3 {
            word |= (*data.get(byte + k).unwrap_or(&0) as u32) << (8 * k);
        }
        let code = ((word >> (pos % 8)) & ((1 << bits) - 1)) as usize;
        pos += bits as usize;
        if code == CLEAR && block_mode {
            pos = realign(pos, group_start, bits);
            bits = 9;
            group_start = pos;
            next = first - 1; // the next code is not added
            old = None;
            continue;
        }
        let Some(prev) = old else {
            if code > 255 {
                return Err(bad("compress", "a code before the table has it"));
            }
            first_byte = code as u8;
            buf.push(first_byte);
            old = Some(code);
            if next < first {
                next = first;
            }
            continue;
        };
        let mut c = code;
        stack.clear();
        if code >= next {
            if code > next {
                return Err(bad("compress", "a code past the table's end"));
            }
            stack.push(first_byte); // the KwKwK case
            c = prev;
        }
        while c >= 256 {
            stack.push(suffix[c]);
            c = prefix[c] as usize;
        }
        first_byte = suffix[c];
        stack.push(first_byte);
        buf.extend(stack.iter().rev());
        if next < 1 << max_bits {
            prefix[next] = prev as u32;
            suffix[next] = first_byte;
            next += 1;
        }
        old = Some(code);
        if buf.len() >= 1 << 16 {
            out.write_all(&buf).map_err(sink)?;
            buf.clear();
        }
    }
    out.write_all(&buf).map_err(sink)
}

// ----- lzip / lzma ------------------------------------------------------------------

/// Counts and checks what an LZMA stream writes (lzip's trailer).
#[cfg(feature = "archive-xz")]
struct Checked<'a> {
    out: &'a mut dyn Write,
    crc: u32,
    len: u64,
    error: Option<std::io::Error>,
}

#[cfg(feature = "archive-xz")]
impl Write for Checked<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        match self.out.write(data) {
            Ok(n) => {
                self.crc = dix_core::partition::crc32_continue(self.crc, &data[..n]);
                self.len += n as u64;
                Ok(n)
            }
            Err(e) => {
                self.error = Some(std::io::Error::new(e.kind(), e.to_string()));
                Err(e)
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// lzip: members of "LZIP", a version, the dictionary size, an LZMA stream
/// (lc 3, lp 0, pb 2, with an end marker), and a trailer with the CRC32
/// and size of what it unpacks to, and the member's own size — by which
/// the members are found from the end, as lzip does (the decoder must see
/// exactly one stream).
#[cfg(feature = "archive-xz")]
pub fn lzip(input: &mut dyn BufRead, out: &mut dyn Write) -> Result<()> {
    let mut data = Vec::new();
    input.read_to_end(&mut data).map_err(|e| bad("lzip", e))?;
    if !data.starts_with(b"LZIP") {
        return Err(bad("lzip", "not lzip data"));
    }
    // Trailing bytes after the last member are allowed (as lzip does).
    let mut end = data.len();
    let mut members = Vec::new();
    while end > 0 {
        if end < 26 {
            return Err(bad("lzip", "a member is cut"));
        }
        let size = u64::from_le_bytes(data[end - 8..end].try_into().unwrap()) as usize;
        if size < 26 || size > end || &data[end - size..end - size + 4] != b"LZIP" {
            if members.is_empty() && end == data.len() {
                // Trailing garbage: look for the last member's end.
                match (26..end).rev().find(|&e| {
                    let s = u64::from_le_bytes(data[e - 8..e].try_into().unwrap()) as usize;
                    s >= 26 && s <= e && &data[e - s..e - s + 4] == b"LZIP"
                }) {
                    Some(e) => {
                        end = e;
                        continue;
                    }
                    None => return Err(bad("lzip", "the member sizes do not add up")),
                }
            }
            return Err(bad("lzip", "the member sizes do not add up"));
        }
        members.push((end - size, end));
        end -= size;
    }
    for &(from, to) in members.iter().rev() {
        let m = &data[from..to];
        if m[4] != 1 {
            return Err(bad("lzip", format!("version {}", m[4])));
        }
        let base = 1u32 << (m[5] & 0x1F);
        let dict = base - (base / 16) * (m[5] >> 5) as u32;
        let mut stream = vec![0x5D];
        stream.extend(dict.to_le_bytes());
        stream.extend_from_slice(&m[6..m.len() - 20]);
        let mut checked = Checked { out: &mut *out, crc: 0, len: 0, error: None };
        let opts = lzma_rs::decompress::Options {
            unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(None),
            ..Default::default()
        };
        let r = lzma_rs::lzma_decompress_with_options(&mut &stream[..], &mut checked, &opts);
        if let Some(e) = checked.error.take() {
            return Err(sink(e));
        }
        r.map_err(|e| bad("lzip", e))?;
        let t = &m[m.len() - 20..];
        if u32::from_le_bytes(t[..4].try_into().unwrap()) != checked.crc
            || u64::from_le_bytes(t[4..12].try_into().unwrap()) != checked.len
        {
            return Err(bad("lzip", "checksum mismatch"));
        }
    }
    Ok(())
}

/// The legacy `.lzma` format: properties, dictionary size, size (or all
/// ones: unknown, an end marker), then the LZMA stream.
#[cfg(feature = "archive-xz")]
pub fn lzma(input: &mut dyn BufRead, out: &mut dyn Write) -> Result<()> {
    let mut checked = Checked { out, crc: 0, len: 0, error: None };
    let r = lzma_rs::lzma_decompress(&mut &mut *input, &mut checked);
    if let Some(e) = checked.error.take() {
        return Err(sink(e));
    }
    r.map_err(|e| bad("lzma", e))
}

/// Whether `head` looks like the start of a legacy `.lzma` file (it has no
/// magic number: the usual properties, a sensible dictionary, a size).
pub fn looks_like_lzma(head: &[u8]) -> bool {
    if head.len() < 13 || head[0] != 0x5D {
        return false;
    }
    let dict = u32::from_le_bytes(head[1..5].try_into().unwrap());
    let size = u64::from_le_bytes(head[5..13].try_into().unwrap());
    let dict_ok = (12..=30).any(|b| dict == 1 << b || dict == (1 << b) + (1 << (b - 1)));
    dict_ok && (size == u64::MAX || size < 1 << 40)
}

// ----- pbzx -------------------------------------------------------------------------

/// Apple's pbzx: "pbzx", a chunk size, then chunks (flags, length, an xz
/// stream — or the bytes as they are when they are not one).
pub fn pbzx(input: &mut dyn BufRead, out: &mut dyn Write) -> Result<()> {
    let mut h = [0u8; 12];
    read_exact(input, &mut h, "pbzx")?;
    if &h[..4] != b"pbzx" {
        return Err(bad("pbzx", "not pbzx data"));
    }
    let mut chunk = Vec::new();
    while !at_end(input)? {
        let mut c = [0u8; 16];
        read_exact(input, &mut c, "pbzx")?;
        let len = u64::from_be_bytes(c[8..].try_into().unwrap());
        if len > 1 << 30 {
            return Err(bad("pbzx", "a chunk is too large"));
        }
        chunk.resize(len as usize, 0);
        read_exact(input, &mut chunk, "pbzx")?;
        if chunk.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0]) {
            super::Compression::Xz.unpack(&mut &chunk[..], out)?;
        } else {
            out.write_all(&chunk).map_err(sink)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lz4_linked_blocks_and_skippable_frames() {
        // A skippable frame, then a frame of two linked blocks: the second
        // is a match into the first.
        let mut f = vec![0x50, 0x2A, 0x4D, 0x18, 3, 0, 0, 0, 1, 2, 3];
        f.extend([0x04, 0x22, 0x4D, 0x18, 0x40, 0x40, 0xC0]);
        // Block 1: literals "abcd".
        let b1 = [0x40, b'a', b'b', b'c', b'd'];
        f.extend((b1.len() as u32).to_le_bytes());
        f.extend(b1);
        // Block 2: no literals, a match of 8 at distance 4, then the
        // closing literal block "!".
        let b2 = [0x04, 4, 0, 0x10, b'!'];
        f.extend((b2.len() as u32).to_le_bytes());
        f.extend(b2);
        f.extend([0, 0, 0, 0]);
        let mut out = Vec::new();
        lz4(&mut &f[..], &mut out).unwrap();
        assert_eq!(out, b"abcdabcdabcd!");
    }

    #[test]
    fn pbzx_stored_chunks() {
        let mut f = b"pbzx".to_vec();
        f.extend(16u64.to_be_bytes());
        for part in [&b"hello "[..], b"world"] {
            f.extend(0u64.to_be_bytes());
            f.extend((part.len() as u64).to_be_bytes());
            f.extend(part);
        }
        let mut out = Vec::new();
        pbzx(&mut &f[..], &mut out).unwrap();
        assert_eq!(out, b"hello world");
    }
}
