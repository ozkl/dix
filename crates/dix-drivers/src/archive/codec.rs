//! Compression formats around archives (and single compressed files).
//!
//! | format | detection          | read                 | write                          |
//! |--------|--------------------|----------------------|--------------------------------|
//! | gzip   | `1F 8B`            | `miniz_oxide`        | `miniz_oxide` (level 6)        |
//! | zstd   | `28 B5 2F FD`      | `ruzstd`             | `ruzstd` ("fastest" level)     |
//! | bzip2  | `BZh` + level      | `bzip2` (libbz2-rs)  | `bzip2` (level 6)              |
//! | xz     | `FD 37 7A 58 5A 00`| `lzma-rs`            | no (lzma-rs does not compress) |
//!
//! Everything works on whole buffers: a compressed archive is unpacked into
//! memory when it is opened.

use dix_core::{Error, ErrorKind, Result};
use std::io::{Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Zstd,
    Xz,
    Bzip2,
    /// The legacy `.lzma` format.
    Lzma,
    /// lzip (`.lz`).
    Lzip,
    /// LZ4 frames (`.lz4`).
    Lz4,
    /// Unix `compress` (`.Z`).
    Compress,
    /// Apple's pbzx (macOS installer payloads).
    Pbzx,
    /// A zlib stream (inside other formats; never detected on its own).
    Zlib,
}

/// Unpacked data larger than this is refused (it is held in memory).
const MAX_UNPACKED: u64 = 16 << 30;

impl Compression {
    pub fn detect(head: &[u8]) -> Compression {
        if head.starts_with(&[0x1F, 0x8B]) {
            Compression::Gzip
        } else if head.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
            Compression::Zstd
        } else if head.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0]) {
            Compression::Xz
        } else if head.len() >= 4 && head.starts_with(b"BZh") && (b'1'..=b'9').contains(&head[3]) {
            Compression::Bzip2
        } else if head.starts_with(b"LZIP") {
            Compression::Lzip
        } else if head.starts_with(&[0x04, 0x22, 0x4D, 0x18]) || head.starts_with(&[0x02, 0x21, 0x4C, 0x18]) {
            Compression::Lz4
        } else if head.starts_with(&[0x1F, 0x9D]) {
            Compression::Compress
        } else if head.starts_with(b"pbzx") {
            Compression::Pbzx
        } else if super::streams::looks_like_lzma(head) {
            Compression::Lzma
        } else {
            Compression::None
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Compression::None => "",
            Compression::Gzip => "gzip",
            Compression::Zstd => "zstd",
            Compression::Xz => "xz",
            Compression::Bzip2 => "bzip2",
            Compression::Lzma => "lzma",
            Compression::Lzip => "lzip",
            Compression::Lz4 => "lz4",
            Compression::Compress => "compress",
            Compression::Pbzx => "pbzx",
            Compression::Zlib => "zlib",
        }
    }

    /// The usual file name extension (".gz").
    pub fn extension(self) -> &'static str {
        match self {
            Compression::None => "",
            Compression::Gzip => ".gz",
            Compression::Zstd => ".zst",
            Compression::Xz => ".xz",
            Compression::Bzip2 => ".bz2",
            Compression::Lzma => ".lzma",
            Compression::Lzip => ".lz",
            Compression::Lz4 => ".lz4",
            Compression::Compress => ".Z",
            Compression::Pbzx => ".pbzx",
            Compression::Zlib => ".zz",
        }
    }

    /// We can compress in this format (so the archive can be written).
    pub fn can_write(self) -> bool {
        match self {
            Compression::None | Compression::Gzip => true,
            Compression::Zstd => cfg!(feature = "archive-zstd"),
            Compression::Bzip2 => cfg!(feature = "archive-bzip2"),
            Compression::Xz => cfg!(feature = "archive-xz"),
            Compression::Lzma
            | Compression::Lzip
            | Compression::Lz4
            | Compression::Compress
            | Compression::Pbzx
            | Compression::Zlib => false,
        }
    }

    #[allow(unused_variables)]
    pub fn decompress(self, data: &[u8]) -> Result<Vec<u8>> {
        let bad = |e: &dyn std::fmt::Display| Error::corrupted(format!("{} data: {e}", self.name()));
        match self {
            Compression::None => Ok(data.to_vec()),
            Compression::Gzip => gunzip(data),
            #[cfg(not(feature = "archive-zstd"))]
            Compression::Zstd => Err(left_out("zstd")),
            #[cfg(not(feature = "archive-xz"))]
            Compression::Xz => Err(left_out("xz")),
            #[cfg(not(feature = "archive-bzip2"))]
            Compression::Bzip2 => Err(left_out("bzip2")),
            #[cfg(feature = "archive-zstd")]
            Compression::Zstd => {
                let mut out = Vec::new();
                let mut src = data;
                // A file may hold several frames (one after another).
                while !src.is_empty() {
                    let mut dec = ruzstd::decoding::StreamingDecoder::new(&mut src).map_err(|e| bad(&e))?;
                    read_all(&mut dec, &mut out).map_err(|e| bad(&e))?;
                }
                Ok(out)
            }
            #[cfg(feature = "archive-xz")]
            Compression::Xz if xz_sha256(data) => {
                let mut out = Vec::new();
                read_all(&mut lzma_rust2::XzReader::new(data, true), &mut out).map_err(|e| bad(&e))?;
                Ok(out)
            }
            #[cfg(feature = "archive-xz")]
            Compression::Xz => {
                let mut out = Vec::new();
                lzma_rs::xz_decompress(&mut std::io::BufReader::new(data), &mut out).map_err(|e| bad(&e))?;
                Ok(out)
            }
            #[cfg(feature = "archive-bzip2")]
            Compression::Bzip2 => {
                let mut out = Vec::new();
                let mut dec = bzip2::read::MultiBzDecoder::new(data);
                read_all(&mut dec, &mut out).map_err(|e| bad(&e))?;
                Ok(out)
            }
            Compression::Zlib => miniz_oxide::inflate::decompress_to_vec_zlib(data).map_err(|e| bad(&e)),
            _ => {
                let mut out = Limited(Vec::new());
                self.unpack(&mut &data[..], &mut out)?;
                Ok(out.0)
            }
        }
    }

    /// Unpacks a stream into `out` as it is read, without holding it in
    /// memory (a compressed disk image written straight onto a disk).
    /// Errors from `out` come back as they are.
    #[allow(unused_variables)]
    pub fn unpack(self, input: &mut dyn Read, out: &mut dyn Write) -> Result<()> {
        let bad = |e: &dyn std::fmt::Display| Error::corrupted(format!("{} data: {e}", self.name()));
        // Errors of the sink (a disk) are not the data's fault.
        let io = |e: std::io::Error| -> Error {
            if e.kind() == std::io::ErrorKind::Other { Error::new(ErrorKind::Io, e.to_string()) } else { bad(&e) }
        };
        let mut input = std::io::BufReader::with_capacity(1 << 20, input);
        match self {
            Compression::None => {
                std::io::copy(&mut input, out).map_err(io)?;
                Ok(())
            }
            Compression::Gzip => gunzip_stream(&mut input, out),
            #[cfg(not(feature = "archive-zstd"))]
            Compression::Zstd => Err(left_out("zstd")),
            #[cfg(not(feature = "archive-xz"))]
            Compression::Xz => Err(left_out("xz")),
            #[cfg(not(feature = "archive-bzip2"))]
            Compression::Bzip2 => Err(left_out("bzip2")),
            #[cfg(feature = "archive-zstd")]
            Compression::Zstd => {
                use std::io::BufRead;
                // Frame after frame.
                while !input.fill_buf().map_err(|e| bad(&e))?.is_empty() {
                    let mut dec = ruzstd::decoding::StreamingDecoder::new(&mut input).map_err(|e| bad(&e))?;
                    std::io::copy(&mut dec, out).map_err(io)?;
                }
                Ok(())
            }
            #[cfg(feature = "archive-xz")]
            Compression::Xz if xz_sha256(std::io::BufRead::fill_buf(&mut input).map_err(|e| bad(&e))?) => {
                std::io::copy(&mut lzma_rust2::XzReader::new(input, true), out).map_err(io)?;
                Ok(())
            }
            #[cfg(feature = "archive-xz")]
            Compression::Xz => {
                let mut sink = ErrorKeeper { out, error: None };
                let r = lzma_rs::xz_decompress(&mut input, &mut sink);
                if let Some(e) = sink.error {
                    return Err(io(e));
                }
                r.map_err(|e| bad(&e))
            }
            #[cfg(feature = "archive-bzip2")]
            Compression::Bzip2 => {
                let mut dec = bzip2::read::MultiBzDecoder::new(input);
                std::io::copy(&mut dec, out).map_err(io)?;
                Ok(())
            }
            Compression::Lz4 => super::streams::lz4(&mut input, out),
            Compression::Compress => super::streams::compress(&mut input, out),
            Compression::Pbzx => super::streams::pbzx(&mut input, out),
            #[cfg(feature = "archive-xz")]
            Compression::Lzip => super::streams::lzip(&mut input, out),
            #[cfg(feature = "archive-xz")]
            Compression::Lzma => super::streams::lzma(&mut input, out),
            #[cfg(not(feature = "archive-xz"))]
            Compression::Lzip | Compression::Lzma => Err(left_out(self.name())),
            Compression::Zlib => {
                let mut data = Vec::new();
                input.read_to_end(&mut data).map_err(|e| bad(&e))?;
                let bytes = miniz_oxide::inflate::decompress_to_vec_zlib(&data).map_err(|e| bad(&e))?;
                out.write_all(&bytes).map_err(io)
            }
        }
    }

    pub fn compress(self, data: &[u8]) -> Result<Vec<u8>> {
        self.compress_at(data, 3)
    }

    /// Compresses at `level` (1–9; used by xz, whose higher levels are
    /// much slower).
    pub fn compress_at(self, data: &[u8], level: u32) -> Result<Vec<u8>> {
        self.compress_with(data, level, false)
    }

    /// As `compress_at`; `crc32`: xz with a CRC32 check instead of CRC64
    /// (what the Linux kernel reads for an initramfs).
    #[allow(unused_variables)]
    pub fn compress_with(self, data: &[u8], level: u32, crc32: bool) -> Result<Vec<u8>> {
        match self {
            Compression::None => Ok(data.to_vec()),
            Compression::Gzip => Ok(gzip(data)),
            #[cfg(not(feature = "archive-zstd"))]
            Compression::Zstd => Err(left_out("zstd")),
            #[cfg(not(feature = "archive-bzip2"))]
            Compression::Bzip2 => Err(left_out("bzip2")),
            #[cfg(feature = "archive-zstd")]
            Compression::Zstd => {
                Ok(ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest))
            }
            #[cfg(feature = "archive-bzip2")]
            Compression::Bzip2 => {
                use std::io::Write;
                let mut enc = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(6));
                enc.write_all(data).map_err(|e| Error::new(ErrorKind::Io, e.to_string()))?;
                enc.finish().map_err(|e| Error::new(ErrorKind::Io, e.to_string()))
            }
            #[cfg(feature = "archive-xz")]
            Compression::Xz => {
                use std::io::Write;
                // An xz stream as the xz tool writes it (CRC64 check), with
                // a dictionary no larger than the data needs.
                let mut opts = lzma_rust2::XzOptions::with_preset(level.clamp(1, 9));
                let dict = (data.len() as u64).clamp(1 << 16, opts.lzma_options.dict_size as u64) as u32;
                opts.lzma_options.dict_size = dict;
                if crc32 {
                    opts.set_check_sum_type(lzma_rust2::CheckType::Crc32);
                }
                let fail = |e: &dyn std::fmt::Display| Error::new(ErrorKind::Io, format!("xz compression: {e}"));
                let mut w = lzma_rust2::XzWriter::new(Vec::new(), opts).map_err(|e| fail(&format!("{e:?}")))?;
                w.write_all(data).map_err(|e| fail(&e))?;
                w.finish().map_err(|e| fail(&format!("{e:?}")))
            }
            #[cfg(not(feature = "archive-xz"))]
            Compression::Xz => Err(left_out("xz")),
            c => Err(Error::unsupported(format!("writing {} data", c.name()))),
        }
    }
}

/// Whether an xz stream checks its blocks with SHA-256 (as rpm writes
/// them), which lzma-rs cannot: lzma-rust2 reads those.
#[cfg(feature = "archive-xz")]
fn xz_sha256(head: &[u8]) -> bool {
    head.len() > 7 && head.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0]) && head[7] & 0x0F == 0x0A
}

/// A sink that refuses more than [`MAX_UNPACKED`] bytes (held in memory).
struct Limited(Vec<u8>);

impl Write for Limited {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.0.len() as u64 + data.len() as u64 > MAX_UNPACKED {
            return Err(std::io::Error::other("too large to unpack in memory"));
        }
        self.0.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The error for a codec that was not built in.
#[allow(dead_code)]
pub fn left_out(what: &str) -> Error {
    Error::unsupported(format!("{what} support was left out of this build"))
}

/// `read_to_end` with the size limit.
#[allow(dead_code)]
fn read_all(r: &mut dyn Read, out: &mut Vec<u8>) -> std::io::Result<()> {
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let n = r.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        out.extend_from_slice(&chunk[..n]);
        if out.len() as u64 > MAX_UNPACKED {
            return Err(std::io::Error::other("too large to unpack in memory"));
        }
    }
}

/// Raw deflate data from the start of `input`: the bytes and how much of
/// `input` the stream took. `size_hint`: the expected output size.
pub fn inflate(input: &[u8], size_hint: usize) -> Result<(Vec<u8>, usize)> {
    use miniz_oxide::inflate::TINFLStatus;
    use miniz_oxide::inflate::core::{DecompressorOxide, decompress, inflate_flags};
    let mut state = Box::<DecompressorOxide>::default();
    let mut out = vec![0u8; size_hint.clamp(64, 1 << 30).max(input.len().saturating_mul(2).min(1 << 26))];
    let (mut in_pos, mut out_pos) = (0, 0);
    loop {
        let flags = inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        let (status, used, made) = decompress(&mut state, &input[in_pos..], &mut out, out_pos, flags);
        in_pos += used;
        out_pos += made;
        match status {
            TINFLStatus::Done => {
                out.truncate(out_pos);
                return Ok((out, in_pos));
            }
            TINFLStatus::HasMoreOutput => {
                if out.len() as u64 >= MAX_UNPACKED {
                    return Err(Error::new(ErrorKind::NoSpace, "too large to unpack in memory"));
                }
                out.resize(out.len() * 2, 0);
            }
            TINFLStatus::NeedsMoreInput => return Err(Error::corrupted("deflate data ends early")),
            s => return Err(Error::corrupted(format!("damaged deflate data ({s:?})"))),
        }
    }
}

/// Raw deflate.
pub fn deflate(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec(data, 6)
}

/// gzip: header, deflate, CRC32 and size. Several members are joined.
/// Keeps the sink's own error (lzma-rs reports every error as its own).
#[cfg(feature = "archive-xz")]
struct ErrorKeeper<'a> {
    out: &'a mut dyn Write,
    error: Option<std::io::Error>,
}

#[cfg(feature = "archive-xz")]
impl Write for ErrorKeeper<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.out.write(data).inspect_err(|e| self.error = Some(std::io::Error::new(e.kind(), e.to_string())))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// gzip, member after member, as the data comes.
fn gunzip_stream(input: &mut dyn std::io::BufRead, out: &mut dyn Write) -> Result<()> {
    use miniz_oxide::inflate::stream::{InflateState, inflate};
    use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};
    let bad = |what: &str| Error::corrupted(format!("gzip data: {what}"));
    let read_err = |e: std::io::Error| bad(&e.to_string());
    let byte = |input: &mut dyn std::io::BufRead| -> Result<u8> {
        let mut b = [0u8; 1];
        input.read_exact(&mut b).map_err(|_| bad("it ends early"))?;
        Ok(b[0])
    };
    let mut members = 0;
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let head = input.fill_buf().map_err(read_err)?;
        if head.is_empty() || (members > 0 && head[0] == 0) {
            break; // the end (or padding zeros after the last member)
        }
        let mut h = [0u8; 10];
        input.read_exact(&mut h).map_err(|_| bad("it ends early"))?;
        if h[..2] != [0x1F, 0x8B] {
            if members > 0 {
                break;
            }
            return Err(bad("not gzip data"));
        }
        if h[2] != 8 {
            return Err(Error::unsupported("a gzip method other than deflate"));
        }
        let flags = h[3];
        if flags & 0x04 != 0 {
            let xlen = byte(input)? as usize | (byte(input)? as usize) << 8;
            for _ in 0..xlen {
                byte(input)?;
            }
        }
        for bit in [0x08, 0x10] {
            if flags & bit != 0 {
                while byte(input)? != 0 {}
            }
        }
        if flags & 0x02 != 0 {
            byte(input)?;
            byte(input)?;
        }
        let mut state = InflateState::new_boxed(DataFormat::Raw);
        let (mut crc, mut len) = (0u32, 0u32);
        loop {
            let data = input.fill_buf().map_err(read_err)?;
            let ended = data.is_empty();
            let r = inflate(&mut state, data, &mut chunk, MZFlush::None);
            input.consume(r.bytes_consumed);
            let made = &chunk[..r.bytes_written];
            crc = dix_core::partition::crc32_continue(crc, made);
            len = len.wrapping_add(made.len() as u32);
            out.write_all(made).map_err(|e| Error::new(ErrorKind::Io, e.to_string()))?;
            match r.status {
                Ok(MZStatus::StreamEnd) => break,
                Ok(_) => {}
                Err(MZError::Buf) if ended => return Err(bad("it ends early")),
                Err(MZError::Buf) => {}
                Err(e) => return Err(bad(&format!("damaged ({e:?})"))),
            }
        }
        let mut t = [0u8; 8];
        input.read_exact(&mut t).map_err(|_| bad("it ends early"))?;
        if u32::from_le_bytes(t[..4].try_into().unwrap()) != crc
            || u32::from_le_bytes(t[4..].try_into().unwrap()) != len
        {
            return Err(bad("checksum mismatch"));
        }
        members += 1;
    }
    if members == 0 {
        return Err(bad("not gzip data"));
    }
    Ok(())
}

fn gunzip(mut data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut members = 0;
    while data.len() >= 18 && data[..2] == [0x1F, 0x8B] {
        members += 1;
        if data[2] != 8 {
            return Err(Error::unsupported("a gzip method other than deflate"));
        }
        let flags = data[3];
        let mut at = 10;
        if flags & 0x04 != 0 {
            let xlen = u16::from_le_bytes([data[at], data[at + 1]]) as usize;
            at += 2 + xlen;
        }
        for bit in [0x08, 0x10] {
            // File name, comment: NUL-terminated.
            if flags & bit != 0 {
                at += data.get(at..).and_then(|d| d.iter().position(|&b| b == 0)).ok_or_else(short)? + 1;
            }
        }
        if flags & 0x02 != 0 {
            at += 2;
        }
        let body = data.get(at..).ok_or_else(short)?;
        let (bytes, used) = inflate(body, body.len() * 4)?;
        let trailer = body.get(used..used + 8).ok_or_else(short)?;
        let crc = u32::from_le_bytes(trailer[..4].try_into().unwrap());
        if crc != dix_core::partition::crc32(&bytes) {
            return Err(Error::corrupted("gzip data: checksum mismatch"));
        }
        out.extend_from_slice(&bytes);
        data = &body[used + 8..];
        // Padding zeros after the last member (tape blocks) are fine.
        if data.iter().all(|&b| b == 0) {
            break;
        }
    }
    if members == 0 {
        return Err(Error::corrupted("not gzip data"));
    }
    Ok(out)
}

fn short() -> Error {
    Error::corrupted("gzip data ends early")
}

/// The file name stored in a gzip header (`gzip` records the original name).
pub fn gzip_name(data: &[u8]) -> Option<String> {
    if data.len() < 10 || data[..2] != [0x1F, 0x8B] || data[3] & 0x08 == 0 {
        return None;
    }
    let mut at = 10;
    if data[3] & 0x04 != 0 {
        at += 2 + u16::from_le_bytes([*data.get(10)?, *data.get(11)?]) as usize;
    }
    let end = at + data.get(at..)?.iter().position(|&b| b == 0)?;
    // ISO 8859-1, as the format says.
    Some(data[at..end].iter().map(|&b| b as char).collect())
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1F, 0x8B, 8, 0, 0, 0, 0, 0, 0, 3]; // no name, no mtime, Unix
    out.extend_from_slice(&deflate(data));
    out.extend_from_slice(&dix_core::partition::crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

#[cfg(all(test, feature = "archive-zstd"))]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_detection() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8 ^ (i / 1000) as u8).collect();
        for c in [Compression::Gzip, Compression::Zstd] {
            let packed = c.compress(&data).unwrap();
            assert_eq!(Compression::detect(&packed), c);
            assert!(packed.len() < data.len());
            assert_eq!(c.decompress(&packed).unwrap(), data, "{c:?}");
        }
        // Two gzip members one after the other are one stream.
        let mut two = Compression::Gzip.compress(b"hello ").unwrap();
        two.extend(Compression::Gzip.compress(b"world").unwrap());
        assert_eq!(Compression::Gzip.decompress(&two).unwrap(), b"hello world");
        // Damage is caught by the CRC.
        let mut bad = Compression::Gzip.compress(&data).unwrap();
        let n = bad.len();
        bad[n - 6] ^= 1;
        assert_eq!(Compression::Gzip.decompress(&bad).unwrap_err().kind(), ErrorKind::Corrupted);
        // xz: written here, read by lzma-rs (CRC64, and CRC32 for initramfs).
        #[cfg(feature = "archive-xz")]
        for crc32 in [false, true] {
            let packed = Compression::Xz.compress_with(&data, 3, crc32).unwrap();
            assert_eq!(Compression::detect(&packed), Compression::Xz);
            assert!(packed.len() < data.len() / 4);
            assert_eq!(Compression::Xz.decompress(&packed).unwrap(), data);
            // The stream flags' check type: 1 CRC32, 4 CRC64.
            assert_eq!(packed[7], if crc32 { 1 } else { 4 });
        }
    }
}
