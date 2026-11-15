//! Files compressed by macOS (HFS+ and APFS "decmpfs" compression, as
//! `ditto --hfsCompression`, `afsctool` and the system itself make them).
//!
//! Such a file has an empty data fork, the owner flag `UF_COMPRESSED`, and
//! an extended attribute `com.apple.decmpfs`: a 16-byte header (`fpmc`, the
//! compression type, the uncompressed size), followed — for small files —
//! by the compressed data itself. Larger files keep their data in the
//! resource fork, cut into 64 KiB chunks each compressed on its own:
//!
//! | type | where | how |
//! |------|-------|-----|
//! | 1, 9 | attribute | stored |
//! | 3 / 4 | attribute / resource fork | zlib |
//! | 7 / 8 | attribute / resource fork | LZVN ([`lzvn`]) |
//! | 10 | resource fork | stored chunks |
//! | 11 / 12 | attribute / resource fork | LZFSE ([`lzfse`]) |
//!
//! A chunk that did not compress is stored with a one-byte marker
//! (`0xFF` for zlib, `0x06` for LZVN and LZFSE).

pub mod lzfse;
pub mod lzvn;
#[cfg(test)]
mod tests;

use dix_core::{Error, Result};

const CHUNK: u64 = 64 * 1024;

fn bad(what: &str) -> Error {
    Error::corrupted(format!("a compressed file: {what}"))
}

/// A compressed file's decmpfs attribute.
pub struct Compressed {
    /// The uncompressed size.
    pub size: u64,
    kind: u32,
    /// The data after the header (types kept in the attribute).
    inline: Vec<u8>,
}

/// The last decompressed chunk of a file, and its resource fork's chunk table.
#[derive(Default)]
pub struct ChunkCache {
    chunk: Option<((u64, u64), Vec<u8>)>,
    table: Option<(u64, Vec<(u64, u64)>)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Codec {
    Stored,
    Zlib,
    Lzvn,
    Lzfse,
}

impl Compressed {
    pub fn parse(x: Vec<u8>) -> Result<Self> {
        if x.len() < 16 || &x[..4] != b"fpmc" {
            return Err(bad("its attribute is not a decmpfs header"));
        }
        let kind = u32::from_le_bytes(x[4..8].try_into().unwrap());
        let size = u64::from_le_bytes(x[8..16].try_into().unwrap());
        Ok(Self { size, kind, inline: x[16..].to_vec() })
    }

    /// The data is in the resource fork (else in the attribute).
    pub fn in_resource_fork(&self) -> bool {
        matches!(self.kind, 4 | 8 | 10 | 12 | 14)
    }

    fn codec(&self) -> Result<Codec> {
        Ok(match self.kind {
            1 | 9 | 10 => Codec::Stored,
            3 | 4 => Codec::Zlib,
            7 | 8 => Codec::Lzvn,
            11 | 12 => Codec::Lzfse,
            5 | 6 => return Err(Error::unsupported("dataless files (kept in iCloud, not on the disk)")),
            13 | 14 => return Err(Error::unsupported("LZBITMAP-compressed files")),
            k => return Err(Error::unsupported(format!("compression type {k}"))),
        })
    }

    /// Reads the uncompressed file. `rsrc` reads the resource fork (of
    /// `rsrc_size` bytes); `key` identifies the file in `cache`.
    pub fn read(
        &self,
        rsrc: &mut dyn FnMut(u64, &mut [u8]) -> Result<usize>,
        rsrc_size: u64,
        cache: &mut ChunkCache,
        key: u64,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize> {
        if offset >= self.size {
            return Ok(0);
        }
        let codec = self.codec()?;
        let want = buf.len().min((self.size - offset) as usize);
        if !self.in_resource_fork() {
            if cache.chunk.as_ref().is_none_or(|(k, _)| *k != (key, u64::MAX)) {
                let data = unpack(codec, &self.inline, self.size as usize)?;
                cache.chunk = Some(((key, u64::MAX), data));
            }
            let data = &cache.chunk.as_ref().unwrap().1;
            if data.len() < self.size as usize {
                return Err(bad("it is shorter than its size"));
            }
            buf[..want].copy_from_slice(&data[offset as usize..offset as usize + want]);
            return Ok(want);
        }
        if cache.table.as_ref().is_none_or(|(k, _)| *k != key) {
            let t = self.table(rsrc, rsrc_size)?;
            cache.table = Some((key, t));
        }
        let mut done = 0usize;
        while done < want {
            let at = offset + done as u64;
            let index = at / CHUNK;
            if cache.chunk.as_ref().is_none_or(|(k, _)| *k != (key, index)) {
                let table = &cache.table.as_ref().unwrap().1;
                let &(start, len) = table.get(index as usize).ok_or_else(|| bad("a chunk is missing"))?;
                if len > 2 * CHUNK + 1024 || start + len > rsrc_size {
                    return Err(bad("a chunk is out of range"));
                }
                let mut packed = vec![0u8; len as usize];
                if rsrc(start, &mut packed)? < packed.len() {
                    return Err(bad("the resource fork is cut short"));
                }
                let expect = (self.size - index * CHUNK).min(CHUNK) as usize;
                let data = unpack(codec, &packed, expect)?;
                if data.len() < expect {
                    return Err(bad("a chunk is shorter than it should be"));
                }
                cache.chunk = Some(((key, index), data));
            }
            let data = &cache.chunk.as_ref().unwrap().1;
            let inner = (at % CHUNK) as usize;
            let n = (data.len() - inner).min(want - done);
            buf[done..done + n].copy_from_slice(&data[inner..inner + n]);
            done += n;
        }
        Ok(want)
    }

    /// Where each chunk is in the resource fork: (offset, length).
    fn table(&self, rsrc: &mut dyn FnMut(u64, &mut [u8]) -> Result<usize>, size: u64) -> Result<Vec<(u64, u64)>> {
        let chunks = self.size.div_ceil(CHUNK) as usize;
        let mut read = |at: u64, len: usize| -> Result<Vec<u8>> {
            if at + len as u64 > size {
                return Err(bad("its chunk table is out of range"));
            }
            let mut b = vec![0u8; len];
            rsrc(at, &mut b)?;
            Ok(b)
        };
        let le = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as u64;
        if self.kind == 4 {
            // A classic resource fork: a header, then the 'cmpf' resource:
            // its length (big-endian), a count and (offset, length) pairs.
            let h = read(0, 16)?;
            let data = u32::from_be_bytes(h[..4].try_into().unwrap()) as u64 + 4;
            let n = le(&read(data, 4)?, 0) as usize;
            if n != chunks {
                return Err(bad("its chunk table does not match its size"));
            }
            let t = read(data + 4, n * 8)?;
            Ok((0..n).map(|i| (data + le(&t, i * 8), le(&t, i * 8 + 4))).collect())
        } else {
            // Offsets of each chunk and of the end, from the fork's start.
            let t = read(0, (chunks + 1) * 4)?;
            if le(&t, 0) != (chunks as u64 + 1) * 4 {
                return Err(bad("its chunk table does not match its size"));
            }
            (0..chunks)
                .map(|i| {
                    let (a, b) = (le(&t, i * 4), le(&t, i * 4 + 4));
                    if b < a { Err(bad("its chunk table is out of order")) } else { Ok((a, b - a)) }
                })
                .collect()
        }
    }
}

/// One chunk (or a whole small file), uncompressed, `expect` bytes long.
fn unpack(codec: Codec, b: &[u8], expect: usize) -> Result<Vec<u8>> {
    let stored = |skip: usize| Ok(b[skip.min(b.len())..].to_vec());
    match codec {
        Codec::Stored => stored(0),
        Codec::Zlib if b.first().is_some_and(|&x| x & 0x0F == 0x0F) => stored(1),
        Codec::Zlib => miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(b, expect)
            .map_err(|_| bad("its zlib data is damaged")),
        Codec::Lzvn | Codec::Lzfse if b.first() == Some(&0x06) => stored(1),
        Codec::Lzfse if !b.starts_with(b"bvx") && b.first() == Some(&0xFF) => stored(1),
        Codec::Lzvn => {
            let mut out = Vec::with_capacity(expect);
            lzvn::decode(b, &mut out, expect)?;
            Ok(out)
        }
        Codec::Lzfse => {
            let mut out = Vec::with_capacity(expect);
            lzfse::decode(b, &mut out, expect)?;
            Ok(out)
        }
    }
}
