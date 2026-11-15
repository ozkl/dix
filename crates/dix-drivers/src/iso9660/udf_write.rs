//! The UDF side of a bridge image (as `genisoimage -udf` makes it): UDF
//! 1.02, read by Windows, macOS and Linux, describing the same files as the
//! ISO 9660 trees and pointing to the same data.
//!
//! Layout, in 2048-byte sectors: after the ISO 9660 descriptors, the volume
//! recognition sequence (`BEA01`, `NSR02`, `TEA01`); the main and reserve
//! volume descriptor sequences at 32 and 48 (primary, implementation use,
//! partition, logical volume, unallocated space, terminating descriptors);
//! the logical volume integrity descriptor at 64; the anchor at 256 (and in
//! the last sector, unless a backup GPT is there). The partition starts at
//! 257 and runs to the end of the file data: the file set descriptor, a
//! file entry per member of the tree, then directories too large to be
//! embedded in their file entry; the ISO 9660 path tables, directories and
//! file data follow, all inside the partition.

use super::write::WNode;
use dix_core::{EntryKind, Error, Result};
use std::collections::{HashMap, HashSet};

const SECTOR: usize = 2048;
/// Where the partition starts.
pub(super) const PARTITION: u64 = 257;
const MVDS: u64 = 32;
const RVDS: u64 = 48;
const LVID: u64 = 64;
/// Room for allocation descriptors or embedded data in a file entry.
const FE_ROOM: usize = SECTOR - 176;
/// The longest extent an allocation descriptor holds (30 bits, whole sectors).
const MAX_AD: u64 = (1 << 30) - SECTOR as u64;
/// UDF revision 1.02.
const REV: u16 = 0x0102;

/// Where each node's file entry and directory data go (logical blocks of
/// the partition).
pub(super) struct Plan {
    /// Blocks used before the ISO 9660 structures (from the partition start).
    pub blocks: u64,
    /// Each directory's file identifiers (without addresses filled in yet:
    /// they depend only on the plan, so they are made once).
    streams: HashMap<usize, Vec<u8>>,
    /// Directory data or link data outside its file entry: first block.
    outside: HashMap<usize, u32>,
    /// Symbolic link data.
    links: HashMap<usize, Vec<u8>>,
}

fn fe_block(node: usize) -> u32 {
    1 + node as u32
}

impl Plan {
    pub(super) fn new(nodes: &[WNode]) -> Result<Plan> {
        let mut parent = vec![0usize; nodes.len()];
        for (i, n) in nodes.iter().enumerate() {
            for &c in &n.children {
                parent[c] = i;
            }
        }
        let mut streams = HashMap::new();
        let mut links = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            if n.kind == EntryKind::Directory {
                streams.insert(i, dir_stream(nodes, i, parent[i]));
            }
            if n.kind == EntryKind::Symlink {
                links.insert(i, link_data(n.link.as_deref().unwrap_or("")));
            }
        }
        // The file set descriptor, the file entries, then what does not fit.
        let mut next = 1 + nodes.len() as u32;
        let mut outside = HashMap::new();
        for i in 0..nodes.len() {
            let len = streams.get(&i).or(links.get(&i)).map_or(0, |d| d.len());
            if len > FE_ROOM {
                outside.insert(i, next);
                next += len.div_ceil(SECTOR) as u32;
            }
        }
        Ok(Plan { blocks: next as u64, streams, outside, links })
    }
}

// ----- encoding --------------------------------------------------------------

/// CRC-ITU-T (CCITT, polynomial 0x1021, initial 0), as descriptor tags use.
fn crc(data: &[u8]) -> u16 {
    let mut c: u16 = 0;
    for &b in data {
        c ^= (b as u16) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x1021 } else { c << 1 };
        }
    }
    c
}

/// Fills in a descriptor tag: identifier, version 2, CRC over the rest,
/// location, checksum.
fn seal(d: &mut [u8], ident: u16, location: u32) {
    d[0..2].copy_from_slice(&ident.to_le_bytes());
    d[2..4].copy_from_slice(&2u16.to_le_bytes());
    d[5] = 0;
    d[6..8].copy_from_slice(&0u16.to_le_bytes());
    let len = d.len() - 16;
    let c = crc(&d[16..]);
    d[8..10].copy_from_slice(&c.to_le_bytes());
    d[10..12].copy_from_slice(&(len as u16).to_le_bytes());
    d[12..16].copy_from_slice(&location.to_le_bytes());
    d[4] = 0;
    d[4] = d[..16].iter().fold(0u8, |s, &x| s.wrapping_add(x));
}

/// OSTA compressed Unicode: 8 bits per character if they all fit, else
/// UTF-16BE; at most `max` bytes (whole characters).
fn cs0(s: &str, max: usize) -> Vec<u8> {
    if s.is_empty() {
        return Vec::new();
    }
    if s.chars().all(|c| (c as u32) < 0x100) {
        let mut out = vec![8u8];
        out.extend(s.chars().take(max - 1).map(|c| c as u8));
        return out;
    }
    let mut out = vec![16u8];
    for c in s.chars() {
        let mut units = [0u16; 2];
        let units = c.encode_utf16(&mut units);
        if out.len() + 2 * units.len() > max {
            break;
        }
        for u in units.iter() {
            out.extend(u.to_be_bytes());
        }
    }
    out
}

/// A fixed-size `dstring` field.
fn dstring(s: &str, len: usize) -> Vec<u8> {
    let mut out = cs0(s, len - 1);
    let used = out.len();
    out.resize(len, 0);
    out[len - 1] = used as u8;
    out
}

/// The OSTA compressed Unicode character set specification.
fn charspec() -> [u8; 64] {
    let mut c = [0u8; 64];
    c[1..24].copy_from_slice(b"OSTA Compressed Unicode");
    c
}

/// An entity identifier with a suffix.
fn regid(ident: &[u8], suffix: &[u8]) -> [u8; 32] {
    let mut r = [0u8; 32];
    r[1..1 + ident.len()].copy_from_slice(ident);
    r[24..24 + suffix.len()].copy_from_slice(suffix);
    r
}

fn udf_suffix() -> [u8; 2] {
    REV.to_le_bytes()
}

fn implementation() -> [u8; 32] {
    regid(b"*Disk Image Explorer", &[])
}

/// A timestamp (UTC).
fn timestamp(t: i64) -> [u8; 12] {
    let (y, mo, d, h, mi, s) = dix_core::time::civil_from_unix(t);
    let mut b = [0u8; 12];
    b[0..2].copy_from_slice(&0x1000u16.to_le_bytes()); // type 1, UTC
    b[2..4].copy_from_slice(&(y.clamp(1, 9999) as u16).to_le_bytes());
    b[4..9].copy_from_slice(&[mo as u8, d as u8, h as u8, mi as u8, s as u8]);
    b
}

fn long_ad(len: u32, block: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&len.to_le_bytes());
    b[4..8].copy_from_slice(&block.to_le_bytes());
    b
}

/// UDF permissions from Unix ones (the owner may also change attributes
/// and delete when it may write).
fn permissions(perm: u32) -> u32 {
    let mut p = 0;
    for (shift, at) in [(6, 10), (3, 5), (0, 0)] {
        let bits = (perm >> shift) & 7;
        p |= (bits & 1) << at; // execute
        p |= ((bits >> 1) & 1) << (at + 1); // write
        p |= ((bits >> 2) & 1) << (at + 2); // read
        if shift == 6 && bits & 2 != 0 {
            p |= 3 << (at + 3);
        }
    }
    p
}

/// A symbolic link's path components (ECMA-167 4/14.16).
fn link_data(target: &str) -> Vec<u8> {
    let mut out = Vec::new();
    if target.starts_with('/') {
        out.extend([2, 0, 0, 0]);
    }
    for part in target.split('/').filter(|p| !p.is_empty()) {
        match part {
            "." => out.extend([4, 0, 0, 0]),
            ".." => out.extend([3, 0, 0, 0]),
            name => {
                let n = cs0(name, 255);
                out.extend([5, n.len() as u8, 0, 0]);
                out.extend(n);
            }
        }
    }
    out
}

/// A file identifier descriptor (location filled in when written).
fn fid(chars: u8, name: &[u8], icb: u32) -> Vec<u8> {
    let len = (38 + name.len()).next_multiple_of(4);
    let mut d = vec![0u8; len];
    d[16..18].copy_from_slice(&1u16.to_le_bytes());
    d[18] = chars;
    d[19] = name.len() as u8;
    d[20..36].copy_from_slice(&long_ad(SECTOR as u32, icb));
    d[38..38 + name.len()].copy_from_slice(name);
    d
}

/// A directory's file identifiers: its parent, then its children with
/// names unique once shortened to what UDF holds.
fn dir_stream(nodes: &[WNode], dir: usize, parent: usize) -> Vec<u8> {
    let mut out = fid(0x0A, &[], fe_block(parent));
    let mut taken = HashSet::new();
    for &c in &nodes[dir].children {
        let n = &nodes[c];
        let mut name = cs0(&n.name, 255);
        let mut k = 1;
        while !taken.insert(name.clone()) {
            let suffix = format!("~{k}");
            let keep: String = n.name.chars().take(100).collect();
            name = cs0(&format!("{keep}{suffix}"), 255);
            k += 1;
        }
        let chars = if n.kind == EntryKind::Directory { 2 } else { 0 };
        out.extend(fid(chars, &name, fe_block(c)));
    }
    out
}

/// Fills in the tag of each file identifier in a directory's data, which
/// starts at block `first` (all in that block when embedded).
fn seal_stream(data: &mut [u8], first: u32, embedded: bool) {
    let mut pos = 0;
    while pos + 38 <= data.len() {
        let len = (38 + data[pos + 19] as usize + u16::from_le_bytes([data[pos + 36], data[pos + 37]]) as usize)
            .next_multiple_of(4);
        let block = if embedded { first } else { first + (pos / SECTOR) as u32 };
        seal(&mut data[pos..pos + len], 257, block);
        pos += len;
    }
}

// ----- the file entries ----------------------------------------------------------

fn file_entry(nodes: &[WNode], i: usize, plan: &Plan, extents: &HashMap<usize, Vec<(u64, u64)>>) -> Result<Vec<u8>> {
    let n = &nodes[i];
    let (ftype, size, data) = match n.kind {
        EntryKind::Directory => (4u8, plan.streams[&i].len() as u64, Some(&plan.streams[&i])),
        EntryKind::File => (5, n.size, None),
        EntryKind::Symlink => (12, plan.links[&i].len() as u64, Some(&plan.links[&i])),
        EntryKind::Other => (
            match n.type_bits {
                0o060000 => 6,
                0o020000 => 7,
                0o010000 => 9,
                0o140000 => 10,
                _ => 5,
            },
            0,
            None,
        ),
    };
    // Allocation descriptors (short), or the data embedded.
    let (flags, ads, recorded): (u16, Vec<u8>, u64) = match (data, plan.outside.get(&i)) {
        (Some(d), None) => {
            let mut d = d.clone();
            if n.kind == EntryKind::Directory {
                seal_stream(&mut d, fe_block(i), true);
            }
            (3, d, 0)
        }
        (Some(d), Some(&at)) => {
            let blocks = d.len().div_ceil(SECTOR) as u64;
            let mut ad = (d.len() as u32).to_le_bytes().to_vec();
            ad.extend(at.to_le_bytes());
            (0, ad, blocks)
        }
        (None, _) => {
            let mut ads = Vec::new();
            let mut recorded = 0;
            for &(lba, len) in extents.get(&i).map(|e| e.as_slice()).unwrap_or(&[]) {
                if len == 0 {
                    continue;
                }
                let mut off = 0;
                while off < len {
                    let part = (len - off).min(MAX_AD);
                    let block = lba + off / SECTOR as u64 - PARTITION;
                    ads.extend((part as u32).to_le_bytes());
                    ads.extend((block as u32).to_le_bytes());
                    recorded += part.div_ceil(SECTOR as u64);
                    off += part;
                }
            }
            if ads.len() > FE_ROOM {
                return Err(Error::invalid_input(format!("{} is too large for UDF", n.name)));
            }
            (0, ads, recorded)
        }
    };
    let mut fe = vec![0u8; 176 + ads.len()];
    // ICB tag: strategy 4, one entry.
    fe[16 + 4..16 + 6].copy_from_slice(&4u16.to_le_bytes());
    fe[16 + 8..16 + 10].copy_from_slice(&1u16.to_le_bytes());
    fe[16 + 11] = ftype;
    fe[16 + 18..16 + 20].copy_from_slice(&flags.to_le_bytes());
    fe[36..40].copy_from_slice(&n.uid.to_le_bytes());
    fe[40..44].copy_from_slice(&n.gid.to_le_bytes());
    fe[44..48].copy_from_slice(&permissions(n.perm).to_le_bytes());
    let links = if n.kind == EntryKind::Directory {
        1 + n.children.iter().filter(|&&c| nodes[c].kind == EntryKind::Directory).count() as u16
    } else {
        1
    };
    fe[48..50].copy_from_slice(&links.to_le_bytes());
    fe[56..64].copy_from_slice(&size.to_le_bytes());
    fe[64..72].copy_from_slice(&recorded.to_le_bytes());
    fe[72..84].copy_from_slice(&timestamp(n.atime.unwrap_or(n.mtime)));
    fe[84..96].copy_from_slice(&timestamp(n.mtime));
    fe[96..108].copy_from_slice(&timestamp(n.ctime.unwrap_or(n.mtime)));
    fe[108..112].copy_from_slice(&1u32.to_le_bytes());
    fe[128..160].copy_from_slice(&implementation());
    let unique = if i == 0 { 0 } else { 15 + i as u64 };
    fe[160..168].copy_from_slice(&unique.to_le_bytes());
    fe[172..176].copy_from_slice(&(ads.len() as u32).to_le_bytes());
    fe[176..].copy_from_slice(&ads);
    seal(&mut fe, 261, fe_block(i));
    fe.resize(SECTOR, 0);
    Ok(fe)
}

/// The sectors from `from` (just after the ISO 9660 descriptors) to the end
/// of the UDF region: recognition sequence, descriptor sequences, integrity,
/// anchor and partition contents. `part_len`: the partition's length in
/// sectors.
pub(super) fn head(
    nodes: &[WNode],
    plan: &Plan,
    extents: &HashMap<usize, Vec<(u64, u64)>>,
    label: &str,
    from: u64,
    part_len: u64,
) -> Result<Vec<u8>> {
    let end = PARTITION + plan.blocks;
    let mut out = vec![0u8; ((end - from) as usize) * SECTOR];
    for (i, id) in [b"BEA01", b"NSR02", b"TEA01"].iter().enumerate() {
        let s = sector(&mut out, from, from + i as u64);
        s[1..6].copy_from_slice(*id);
        s[6] = 1;
    }
    let now = dix_core::time::unix_now();
    let label = if label.is_empty() { "CDROM" } else { label };
    let set_id = format!("{:08X}{:08X}", now as u32, (now as u64).wrapping_mul(2_654_435_761) as u32);
    let files = nodes.iter().filter(|n| n.kind != EntryKind::Directory).count() as u32;
    let dirs = nodes.iter().filter(|n| n.kind == EntryKind::Directory).count() as u32;

    // The volume descriptor sequences.
    for base in [MVDS, RVDS] {
        let mut pvd = vec![0u8; 512];
        pvd[24..56].copy_from_slice(&dstring(label, 32));
        pvd[56..58].copy_from_slice(&1u16.to_le_bytes());
        pvd[58..60].copy_from_slice(&1u16.to_le_bytes());
        pvd[60..62].copy_from_slice(&2u16.to_le_bytes());
        pvd[62..64].copy_from_slice(&2u16.to_le_bytes());
        pvd[64..68].copy_from_slice(&1u32.to_le_bytes());
        pvd[68..72].copy_from_slice(&1u32.to_le_bytes());
        pvd[72..200].copy_from_slice(&dstring(&set_id, 128));
        pvd[200..264].copy_from_slice(&charspec());
        pvd[264..328].copy_from_slice(&charspec());
        pvd[344..376].copy_from_slice(&implementation());
        pvd[376..388].copy_from_slice(&timestamp(now));
        pvd[388..420].copy_from_slice(&implementation());
        seal(&mut pvd, 1, base as u32);

        let mut iuvd = vec![0u8; 512];
        iuvd[16..20].copy_from_slice(&1u32.to_le_bytes());
        iuvd[20..52].copy_from_slice(&regid(b"*UDF LV Info", &udf_suffix()));
        iuvd[52..116].copy_from_slice(&charspec());
        iuvd[116..244].copy_from_slice(&dstring(label, 128));
        iuvd[352..384].copy_from_slice(&implementation());
        seal(&mut iuvd, 4, base as u32 + 1);

        let mut pd = vec![0u8; 512];
        pd[16..20].copy_from_slice(&2u32.to_le_bytes());
        pd[20..22].copy_from_slice(&1u16.to_le_bytes());
        pd[24..56].copy_from_slice(&regid(b"+NSR02", &[]));
        pd[184..188].copy_from_slice(&1u32.to_le_bytes()); // read-only
        pd[188..192].copy_from_slice(&(PARTITION as u32).to_le_bytes());
        pd[192..196].copy_from_slice(&(part_len as u32).to_le_bytes());
        pd[196..228].copy_from_slice(&implementation());
        seal(&mut pd, 5, base as u32 + 2);

        let mut lvd = vec![0u8; 446];
        lvd[16..20].copy_from_slice(&3u32.to_le_bytes());
        lvd[20..84].copy_from_slice(&charspec());
        lvd[84..212].copy_from_slice(&dstring(label, 128));
        lvd[212..216].copy_from_slice(&(SECTOR as u32).to_le_bytes());
        lvd[216..248].copy_from_slice(&regid(b"*OSTA UDF Compliant", &udf_suffix()));
        lvd[248..264].copy_from_slice(&long_ad(SECTOR as u32, 0));
        lvd[264..268].copy_from_slice(&6u32.to_le_bytes());
        lvd[268..272].copy_from_slice(&1u32.to_le_bytes());
        lvd[272..304].copy_from_slice(&implementation());
        lvd[432..436].copy_from_slice(&(2 * SECTOR as u32).to_le_bytes());
        lvd[436..440].copy_from_slice(&(LVID as u32).to_le_bytes());
        lvd[440..446].copy_from_slice(&[1, 6, 1, 0, 0, 0]);
        seal(&mut lvd, 6, base as u32 + 3);

        let mut usd = vec![0u8; 24];
        usd[16..20].copy_from_slice(&4u32.to_le_bytes());
        seal(&mut usd, 7, base as u32 + 4);

        let mut td = vec![0u8; 512];
        seal(&mut td, 8, base as u32 + 5);

        for (k, d) in [pvd, iuvd, pd, lvd, usd, td].into_iter().enumerate() {
            sector(&mut out, from, base + k as u64)[..d.len()].copy_from_slice(&d);
        }
    }

    // The integrity descriptor: closed, nothing free (read-only media).
    let mut lvid = vec![0u8; 134];
    lvid[16..28].copy_from_slice(&timestamp(now));
    lvid[28..32].copy_from_slice(&1u32.to_le_bytes());
    lvid[40..48].copy_from_slice(&(16 + nodes.len() as u64).to_le_bytes());
    lvid[72..76].copy_from_slice(&1u32.to_le_bytes());
    lvid[76..80].copy_from_slice(&46u32.to_le_bytes());
    lvid[84..88].copy_from_slice(&(part_len as u32).to_le_bytes());
    lvid[88..120].copy_from_slice(&implementation());
    lvid[120..124].copy_from_slice(&files.to_le_bytes());
    lvid[124..128].copy_from_slice(&dirs.to_le_bytes());
    for at in [128, 130, 132] {
        lvid[at..at + 2].copy_from_slice(&REV.to_le_bytes());
    }
    seal(&mut lvid, 9, LVID as u32);
    sector(&mut out, from, LVID)[..lvid.len()].copy_from_slice(&lvid);
    let mut td = vec![0u8; 512];
    seal(&mut td, 8, LVID as u32 + 1);
    sector(&mut out, from, LVID + 1)[..512].copy_from_slice(&td);

    sector(&mut out, from, 256)[..512].copy_from_slice(&anchor(256));

    // The partition: the file set descriptor, file entries, outside data.
    let mut fsd = vec![0u8; 512];
    fsd[16..28].copy_from_slice(&timestamp(now));
    fsd[28..30].copy_from_slice(&3u16.to_le_bytes());
    fsd[30..32].copy_from_slice(&3u16.to_le_bytes());
    fsd[32..36].copy_from_slice(&1u32.to_le_bytes());
    fsd[36..40].copy_from_slice(&1u32.to_le_bytes());
    fsd[48..112].copy_from_slice(&charspec());
    fsd[112..240].copy_from_slice(&dstring(label, 128));
    fsd[240..304].copy_from_slice(&charspec());
    fsd[304..336].copy_from_slice(&dstring(label, 32));
    fsd[400..416].copy_from_slice(&long_ad(SECTOR as u32, fe_block(0)));
    fsd[416..448].copy_from_slice(&regid(b"*OSTA UDF Compliant", &udf_suffix()));
    seal(&mut fsd, 256, 0);
    sector(&mut out, from, PARTITION)[..512].copy_from_slice(&fsd);
    let mut later = Vec::new();
    for i in 0..nodes.len() {
        let fe = file_entry(nodes, i, plan, extents)?;
        sector(&mut out, from, PARTITION + fe_block(i) as u64).copy_from_slice(&fe);
        if let Some(&at) = plan.outside.get(&i) {
            let mut d = plan.streams.get(&i).or(plan.links.get(&i)).unwrap().clone();
            if nodes[i].kind == EntryKind::Directory {
                seal_stream(&mut d, at, false);
            }
            later.push((((PARTITION + at as u64 - from) as usize) * SECTOR, d));
        }
    }
    for (start, d) in later {
        out[start..start + d.len()].copy_from_slice(&d);
    }
    Ok(out)
}

/// Sector `n` of `out`, which starts at sector `from`.
fn sector(out: &mut [u8], from: u64, n: u64) -> &mut [u8] {
    let at = ((n - from) as usize) * SECTOR;
    &mut out[at..at + SECTOR]
}

/// The anchor volume descriptor pointer, for sector `at`.
pub(super) fn anchor(at: u64) -> Vec<u8> {
    let mut a = vec![0u8; 512];
    a[16..20].copy_from_slice(&(16 * SECTOR as u32).to_le_bytes());
    a[20..24].copy_from_slice(&(MVDS as u32).to_le_bytes());
    a[24..28].copy_from_slice(&(16 * SECTOR as u32).to_le_bytes());
    a[28..32].copy_from_slice(&(RVDS as u32).to_le_bytes());
    seal(&mut a, 2, at as u32);
    a
}
