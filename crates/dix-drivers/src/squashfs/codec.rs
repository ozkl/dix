//! The compressors SquashFS uses: gzip (zlib streams), lzma (the old
//! "LZMA alone" format), LZO (LZO1X), xz (with its BCJ filters), LZ4 (the
//! block format) and zstd. LZ4 and LZO are decoded here; the others by the
//! crates the archive driver also uses.

use dix_core::{Error, Result};
use std::io::Read;

/// Compressor ids in the superblock.
pub const GZIP: u16 = 1;
pub const LZMA: u16 = 2;
pub const LZO: u16 = 3;
pub const XZ: u16 = 4;
pub const LZ4: u16 = 5;
pub const ZSTD: u16 = 6;

pub fn name(id: u16) -> &'static str {
    match id {
        GZIP => "gzip",
        LZMA => "lzma",
        LZO => "lzo",
        XZ => "xz",
        LZ4 => "lz4",
        ZSTD => "zstd",
        _ => "unknown",
    }
}

fn bad(what: &str, e: impl std::fmt::Display) -> Error {
    Error::corrupted(format!("SquashFS: {what} data: {e}"))
}

/// Decompresses one block, which unpacks to at most `max` bytes.
pub fn decompress(id: u16, src: &[u8], max: usize) -> Result<Vec<u8>> {
    let mut out = match id {
        GZIP => miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(src, max)
            .map_err(|e| bad("gzip", format!("{:?}", e.status)))?,
        LZMA => {
            let mut out = Vec::new();
            lzma_rs::lzma_decompress(&mut std::io::BufReader::new(src), &mut out).map_err(|e| bad("lzma", e))?;
            out
        }
        XZ => {
            let mut out = Vec::new();
            let mut r = lzma_rust2::XzReader::new(src, true);
            read_limited(&mut r, &mut out, max).map_err(|e| bad("xz", e))?;
            out
        }
        ZSTD => {
            let mut out = Vec::new();
            let mut s = src;
            let mut r = ruzstd::decoding::StreamingDecoder::new(&mut s).map_err(|e| bad("zstd", e))?;
            read_limited(&mut r, &mut out, max).map_err(|e| bad("zstd", e))?;
            out
        }
        LZ4 => lz4_block(src, max)?,
        LZO => lzo1x(src, max)?,
        other => return Err(Error::unsupported(format!("SquashFS compressor {other}"))),
    };
    if out.len() > max {
        out.truncate(max);
    }
    Ok(out)
}

fn read_limited(r: &mut dyn Read, out: &mut Vec<u8>, max: usize) -> std::io::Result<()> {
    let mut chunk = vec![0u8; 64 << 10];
    loop {
        let n = r.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        out.extend_from_slice(&chunk[..n]);
        if out.len() > max {
            return Err(std::io::Error::other("more data than a block holds"));
        }
    }
}

pub use crate::lz4::block as lz4_block;

pub use crate::lzo::lzo1x;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lz4_literals_and_overlapping_match() {
        // "abc", a match of 9 at distance 3, then the closing sequence (no
        // literals) → "abcabcabcabc".
        let src = [0x35, b'a', b'b', b'c', 3, 0, 0x00];
        assert_eq!(lz4_block(&src, 100).unwrap(), b"abcabcabcabc");
    }
}
