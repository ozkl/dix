//! The boot sector, MFT records and their attributes.
//!
//! Every file is a 1 KiB (usually) record in the Master File Table: a
//! header, then attributes (`$STANDARD_INFORMATION`, `$FILE_NAME`, `$DATA`,
//! `$INDEX_ROOT`, …). Small attribute values live in the record ("resident");
//! large ones are elsewhere, described by a *runlist* of cluster runs. A file
//! whose attributes do not fit in one record has an `$ATTRIBUTE_LIST`
//! pointing at extension records; a large attribute may then be split into
//! extents, each covering a range of virtual clusters (VCNs).
//!
//! Records and index blocks are protected by an "update sequence": the last
//! two bytes of every 512-byte stride are replaced by a sequence number on
//! disk and restored from an array in the header when read; a mismatch
//! means a torn write.

use dix_core::{Error, Result};

pub fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

pub fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

pub fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

pub const AT_STANDARD_INFORMATION: u32 = 0x10;
pub const AT_ATTRIBUTE_LIST: u32 = 0x20;
pub const AT_FILE_NAME: u32 = 0x30;
pub const AT_VOLUME_NAME: u32 = 0x60;
pub const AT_VOLUME_INFORMATION: u32 = 0x70;
pub const AT_DATA: u32 = 0x80;
pub const AT_INDEX_ROOT: u32 = 0x90;
pub const AT_INDEX_ALLOCATION: u32 = 0xA0;
pub const AT_BITMAP: u32 = 0xB0;
pub const AT_REPARSE_POINT: u32 = 0xC0;

pub const FLAG_COMPRESSED: u16 = 0x0001;
pub const FLAG_ENCRYPTED: u16 = 0x4000;

/// The record number in a file reference (the low 48 bits).
pub fn record_of(file_ref: u64) -> u64 {
    file_ref & 0x0000_FFFF_FFFF_FFFF
}

#[derive(Debug, Clone)]
pub struct Boot {
    pub bytes_per_sector: u32,
    pub cluster: u64,
    pub total_sectors: u64,
    pub mft_lcn: u64,
    pub record_size: u32,
}

impl Boot {
    pub fn parse(b: &[u8]) -> Result<Boot> {
        let bad = |what: &str| Error::corrupted(format!("not an NTFS boot sector ({what})"));
        if b.len() < 512 || &b[3..11] != b"NTFS    " {
            return Err(bad("signature"));
        }
        let bps = le16(b, 11) as u32;
        if !bps.is_power_of_two() || !(256..=4096).contains(&bps) {
            return Err(bad("sector size"));
        }
        let spc = match b[13] {
            0 => return Err(bad("cluster size")),
            v if v <= 0x80 => v as u64,
            v => 1u64 << (256 - v as u32).min(31),
        };
        let cluster = bps as u64 * spc;
        // Record and index block sizes: clusters, or 2^-n bytes if negative.
        let size_of = |v: u8| -> Result<u32> {
            let v = v as i8;
            let s = if v > 0 { v as u64 * cluster } else { 1u64 << (-(v as i32)).clamp(0, 31) };
            if !(256..=1 << 20).contains(&s) {
                return Err(bad("record size"));
            }
            Ok(s as u32)
        };
        let boot = Boot {
            bytes_per_sector: bps,
            cluster,
            total_sectors: le64(b, 0x28),
            mft_lcn: le64(b, 0x30),
            record_size: size_of(b[0x40])?,
            // (Index blocks: their size is in each directory's $INDEX_ROOT.)
        };
        if boot.total_sectors == 0 || boot.mft_lcn == 0 || boot.mft_lcn * cluster >= boot.total_sectors * bps as u64 {
            return Err(bad("layout"));
        }
        Ok(boot)
    }

    pub fn total_clusters(&self) -> u64 {
        self.total_sectors * self.bytes_per_sector as u64 / self.cluster
    }
}

/// Undoes the update sequence of a record or index block in place.
pub fn fixup(buf: &mut [u8], magic: &[u8; 4]) -> Result<()> {
    if &buf[..4] != magic {
        let what = String::from_utf8_lossy(magic);
        return Err(Error::corrupted(format!("not a {what} block (bad signature)")));
    }
    let (off, count) = (le16(buf, 4) as usize, le16(buf, 6) as usize);
    if count == 0 || off + 2 * count > buf.len() || (count - 1) * 512 > buf.len() {
        return Err(Error::corrupted("a bad update sequence array"));
    }
    let usn = [buf[off], buf[off + 1]];
    for i in 1..count {
        let end = i * 512 - 2;
        if buf[end..end + 2] != usn {
            return Err(Error::corrupted("a torn write (update sequence mismatch)"));
        }
        buf[end] = buf[off + 2 * i];
        buf[end + 1] = buf[off + 2 * i + 1];
    }
    Ok(())
}

/// A run of clusters: `len` clusters at `lcn` (`None`: sparse, reads as
/// zeroes), starting at virtual cluster `vcn` of the attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub vcn: u64,
    pub lcn: Option<u64>,
    pub len: u64,
}

/// Decodes a runlist: per run, a header byte (low nibble: bytes of the
/// length, high nibble: bytes of the offset), the length, then the LCN as
/// a signed offset from the previous run's (no offset: sparse).
pub fn decode_runs(b: &[u8], start_vcn: u64) -> Result<Vec<Run>> {
    let mut runs = Vec::new();
    let (mut i, mut vcn, mut lcn) = (0usize, start_vcn, 0i64);
    while i < b.len() && b[i] != 0 {
        let (ls, os) = ((b[i] & 0x0F) as usize, (b[i] >> 4) as usize);
        i += 1;
        if ls == 0 || ls > 8 || os > 8 || i + ls + os > b.len() {
            return Err(Error::corrupted("a damaged runlist"));
        }
        let mut len = 0u64;
        for k in 0..ls {
            len |= (b[i + k] as u64) << (8 * k);
        }
        i += ls;
        let run_lcn = if os == 0 {
            None
        } else {
            let mut off = 0i64;
            for k in 0..os {
                off |= (b[i + k] as i64) << (8 * k);
            }
            // Sign-extend.
            let shift = 64 - 8 * os as u32;
            off = (off << shift) >> shift;
            i += os;
            lcn = lcn
                .checked_add(off)
                .filter(|&l| l >= 0)
                .ok_or_else(|| Error::corrupted("a runlist points before the volume"))?;
            Some(lcn as u64)
        };
        runs.push(Run { vcn, lcn: run_lcn, len });
        vcn += len;
    }
    Ok(runs)
}

#[derive(Debug, Clone)]
pub enum Body {
    Resident(Vec<u8>),
    NonResident { start_vcn: u64, runs: Vec<Run>, size: u64, init: u64, unit: u16 },
}

#[derive(Debug, Clone)]
pub struct Attr {
    pub ty: u32,
    pub name: String,
    pub flags: u16,
    pub id: u16,
    pub body: Body,
}

impl Attr {
    pub fn value(&self) -> Option<&[u8]> {
        match &self.body {
            Body::Resident(v) => Some(v),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Record {
    pub number: u64,
    pub seq: u16,
    pub flags: u16,
    /// The base record of an extension record (0 for a base record).
    pub base: u64,
    pub attrs: Vec<Attr>,
}

impl Record {
    pub fn in_use(&self) -> bool {
        self.flags & 1 != 0
    }

    pub fn is_dir(&self) -> bool {
        self.flags & 2 != 0
    }
}

/// Parses a record (already fixed up).
pub fn parse_record(b: &[u8], number: u64) -> Result<Record> {
    let first = le16(b, 20) as usize;
    let used = (le32(b, 24) as usize).min(b.len());
    let mut attrs = Vec::new();
    let mut at = first;
    while at + 8 <= used {
        let ty = le32(b, at);
        if ty == 0xFFFF_FFFF {
            break;
        }
        let len = le32(b, at + 4) as usize;
        if len < 16 || at + len > used {
            return Err(Error::corrupted(format!("record {number}: a damaged attribute")));
        }
        let a = &b[at..at + len];
        let (name_len, name_off) = (a[9] as usize, le16(a, 10) as usize);
        let name = if name_len > 0 && name_off + 2 * name_len <= len {
            let units: Vec<u16> = (0..name_len).map(|k| le16(a, name_off + 2 * k)).collect();
            String::from_utf16_lossy(&units)
        } else {
            String::new()
        };
        let body = if a[8] == 0 {
            let (vlen, voff) = (le32(a, 16) as usize, le16(a, 20) as usize);
            if voff + vlen > len {
                return Err(Error::corrupted(format!("record {number}: a damaged resident attribute")));
            }
            Body::Resident(a[voff..voff + vlen].to_vec())
        } else {
            if len < 64 {
                return Err(Error::corrupted(format!("record {number}: a short non-resident attribute")));
            }
            let start_vcn = le64(a, 16);
            let runs_off = le16(a, 32) as usize;
            let runs = decode_runs(a.get(runs_off..).unwrap_or(&[]), start_vcn)?;
            Body::NonResident { start_vcn, runs, size: le64(a, 48), init: le64(a, 56), unit: le16(a, 34) }
        };
        attrs.push(Attr { ty, name, flags: le16(a, 12), id: le16(a, 14), body });
        at += len;
    }
    Ok(Record { number, seq: le16(b, 16), flags: le16(b, 22), base: record_of(le64(b, 32)), attrs })
}

/// One `$ATTRIBUTE_LIST` entry: (type, record holding it).
pub fn list_entries(v: &[u8]) -> Vec<(u32, u64)> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 26 <= v.len() {
        let len = le16(v, at + 4) as usize;
        if len < 26 {
            break;
        }
        out.push((le32(v, at), record_of(le64(v, at + 16))));
        at += len;
    }
    out
}

/// Joins the extents of each non-resident attribute (same type and name)
/// into one, in VCN order; sizes come from the first extent.
pub fn merge(attrs: Vec<Attr>) -> Vec<Attr> {
    let mut out: Vec<Attr> = Vec::new();
    let mut extents: Vec<Attr> = attrs;
    extents.sort_by_key(|a| match a.body {
        Body::NonResident { start_vcn, .. } => start_vcn,
        _ => 0,
    });
    for a in extents {
        let into = out.iter_mut().find(|o| {
            o.ty == a.ty
                && o.name == a.name
                && matches!(o.body, Body::NonResident { .. })
                && matches!(a.body, Body::NonResident { .. })
        });
        match (into, &a.body) {
            (Some(o), Body::NonResident { runs, .. }) => {
                if let Body::NonResident { runs: r, .. } = &mut o.body {
                    r.extend_from_slice(runs);
                }
            }
            _ => out.push(a),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runlists() {
        // 0x21: 1-byte length, 2-byte offset. Then a sparse run, then a
        // run going backwards.
        let b = [0x21, 0x10, 0x00, 0x01, 0x01, 0x08, 0x11, 0x04, 0xF0, 0x00];
        let r = decode_runs(&b, 0).unwrap();
        assert_eq!(
            r,
            vec![
                Run { vcn: 0, lcn: Some(0x100), len: 0x10 },
                Run { vcn: 0x10, lcn: None, len: 8 },
                Run { vcn: 0x18, lcn: Some(0x100 - 0x10), len: 4 },
            ]
        );
    }
}
