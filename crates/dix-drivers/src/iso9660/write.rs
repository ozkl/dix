//! Making an ISO 9660 image (a small mkisofs): the primary tree with Rock
//! Ridge (RRIP 1.10: names, permissions, owners, times, symbolic links,
//! device numbers), a Joliet tree (Unicode names, up to 103 characters)
//! and path tables for both.
//!
//! Layout, in sectors of 2048 bytes: 16 empty system-area sectors, the
//! primary and Joliet volume descriptors and the terminator, the four path
//! tables, the primary directories, the Joliet directories, the Rock Ridge
//! continuation areas, then every file's data, contiguous (several
//! extents for files of 4 GiB and more). Files with the same content key
//! (hard links, unchanged files sharing data) share their extents.
//!
//! The image is produced front to back through [`IsoIo`]: nothing is held
//! in memory but the directories.

use dix_core::{EntryKind, Error, Result};
use std::collections::HashMap;

const SECTOR: u64 = 2048;
/// The largest extent: 4 GiB less one sector.
const MAX_EXTENT: u64 = 0xFFFF_F800;

/// One member of the tree to write; `children` index into the same list
/// (entry 0 is the root).
#[derive(Debug, Clone)]
pub(super) struct WNode {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: i64,
    pub atime: Option<i64>,
    pub ctime: Option<i64>,
    /// Permission bits (the type comes from `kind` / `type_bits`).
    pub perm: u32,
    /// For [`EntryKind::Other`]: `S_IFCHR`, `S_IFBLK`, `S_IFIFO`, `S_IFSOCK`.
    pub type_bits: u32,
    pub uid: u32,
    pub gid: u32,
    pub link: Option<String>,
    pub dev: Option<(u32, u32)>,
    /// Files with the same key have the same content (written once).
    pub content_key: Option<u64>,
    pub children: Vec<usize>,
}

/// Where the image goes and where file contents come from.
pub(super) trait IsoIo {
    /// Called once, with the image's size, before anything is written.
    fn begin(&mut self, size: u64) -> Result<()>;
    /// The next bytes of the image.
    fn put(&mut self, data: &[u8]) -> Result<()>;
    /// Reads file `node`'s content at `off` (0: at the end).
    fn read_file(&mut self, node: usize, off: u64, buf: &mut [u8]) -> Result<usize>;
}

/// How the image boots: El Torito entries (BIOS, UEFI) and, for USB
/// sticks and disks, a hybrid MBR.
#[derive(Debug, Clone, Default)]
pub(super) struct BootSpec {
    /// The BIOS boot image (no emulation): node, sectors (of 512 bytes)
    /// to load, and whether to patch its boot info table.
    pub bios: Option<(usize, u16, bool)>,
    /// The UEFI boot image (a FAT image file).
    pub efi: Option<usize>,
    /// Partition the image so it boots as a disk (USB stick): see `gpt`.
    pub hybrid: bool,
    /// Hybrid with a GPT (as current Linux images): a protective MBR, then
    /// partition 1 "ISO9660" and partition 2, the UEFI image, as an EFI
    /// system partition (the UEFI image's data goes last for that), and the
    /// backup GPT in the padding at the end. Otherwise an MBR alone, as
    /// isohybrid writes it: partition 1 over the image, partition 2 (type
    /// EF) over the UEFI image.
    pub gpt: bool,
    /// The MBR's boot code (isolinux's isohdpfx.bin): its first 432 bytes
    /// are used, and the BIOS image's address patched in as isohybrid does.
    pub mbr_code: Option<Vec<u8>>,
}

/// Volume descriptor texts.
pub(super) struct VolumeIds {
    pub label: String,
    pub publisher: String,
    pub preparer: String,
    pub application: String,
    /// Write the ISO 9660 trees (off: a UDF-only image).
    pub iso: bool,
    /// Also write a UDF file system for the same files (a bridge image).
    pub udf: bool,
}

fn both16(v: u16) -> [u8; 4] {
    let (l, b) = (v.to_le_bytes(), v.to_be_bytes());
    [l[0], l[1], b[0], b[1]]
}

fn both32(v: u32) -> [u8; 8] {
    let (l, b) = (v.to_le_bytes(), v.to_be_bytes());
    [l[0], l[1], l[2], l[3], b[0], b[1], b[2], b[3]]
}

/// A directory record's 7-byte time (UTC).
fn short_time(t: i64) -> [u8; 7] {
    let (y, m, d, h, mi, s) = dix_core::time::civil_from_unix(t);
    [(y - 1900).clamp(0, 255) as u8, m as u8, d as u8, h as u8, mi as u8, s as u8, 0]
}

/// A volume descriptor's 17-byte time.
fn long_time(t: Option<i64>) -> [u8; 17] {
    let mut b = [b'0'; 17];
    b[16] = 0;
    if let Some(t) = t {
        let (y, m, d, h, mi, s) = dix_core::time::civil_from_unix(t);
        let text = format!("{:04}{m:02}{d:02}{h:02}{mi:02}{s:02}00", y.clamp(0, 9999));
        b[..16].copy_from_slice(text.as_bytes());
    }
    b
}

/// An a-/d-characters field of `len`, space padded.
fn field(s: &str, len: usize, d_chars: bool) -> Vec<u8> {
    let mut v: Vec<u8> = s
        .to_uppercase()
        .chars()
        .map(|c| {
            let ok = c.is_ascii_uppercase()
                || c.is_ascii_digit()
                || c == '_'
                || (!d_chars && " !\"%&'()*+,-./:;<=>?".contains(c));
            if ok { c as u8 } else { b'_' }
        })
        .take(len)
        .collect();
    v.resize(len, b' ');
    v
}

/// A UCS-2 big-endian field of `len` bytes, space padded.
fn ucs2_field(s: &str, len: usize) -> Vec<u8> {
    let mut v: Vec<u8> = s.encode_utf16().take(len / 2).flat_map(|c| c.to_be_bytes()).collect();
    while v.len() < len {
        v.extend_from_slice(&[0, b' ']);
    }
    v
}

/// A unique ISO 9660 identifier (level 2: up to 30 characters, upper
/// case, one dot, ";1" for files).
fn iso_ident(name: &str, dir: bool, taken: &mut HashMap<Vec<u8>, ()>) -> Vec<u8> {
    let clean = |s: &str| -> String {
        s.to_uppercase().chars().map(|c| if c.is_ascii_uppercase() || c.is_ascii_digit() { c } else { '_' }).collect()
    };
    let (stem, ext) = match name.rfind('.') {
        Some(i) if !dir && i > 0 => (clean(&name[..i]), clean(&name[i + 1..])),
        _ => (clean(name), String::new()),
    };
    let ext: String = ext.chars().take(8).collect();
    let room = if dir { 31 } else { 30 - ext.len() - 1 };
    for n in 0u32.. {
        let tail = if n == 0 { String::new() } else { format!("_{n}") };
        let mut s: String = stem.chars().take(room.saturating_sub(tail.len())).collect();
        s.push_str(&tail);
        if s.is_empty() {
            s.push('_');
        }
        let ident = if dir {
            s
        } else if ext.is_empty() {
            format!("{s}.;1")
        } else {
            format!("{s}.{ext};1")
        };
        let ident = ident.into_bytes();
        if taken.insert(ident.clone(), ()).is_none() {
            return ident;
        }
    }
    unreachable!()
}

/// A Joliet identifier: UCS-2 big-endian, up to 103 characters, ";1" for
/// files, without the characters Joliet forbids.
fn joliet_ident(name: &str, dir: bool, taken: &mut HashMap<Vec<u8>, ()>) -> Vec<u8> {
    let units: Vec<u16> = name
        .chars()
        .map(|c| if "*/:;?\\".contains(c) || (c as u32) < 32 { '_' } else { c })
        .collect::<String>()
        .encode_utf16()
        .collect();
    for n in 0u32.. {
        let tail: Vec<u16> = if n == 0 { Vec::new() } else { format!("_{n}").encode_utf16().collect() };
        let max = 103 - tail.len();
        let mut u: Vec<u16> = if units.len() > max {
            // Keep the extension when shortening.
            let dot = units.iter().rposition(|&c| c == b'.' as u16).filter(|&i| units.len() - i <= 10);
            match dot {
                Some(i) => units[..max - (units.len() - i)].iter().chain(&units[i..]).copied().collect(),
                None => units[..max].to_vec(),
            }
        } else {
            units.clone()
        };
        if !tail.is_empty() {
            let at = u.iter().rposition(|&c| c == b'.' as u16).filter(|&i| i > 0 && !dir).unwrap_or(u.len());
            u.splice(at..at, tail);
        }
        let mut bytes: Vec<u8> = u.iter().flat_map(|c| c.to_be_bytes()).collect();
        if !dir {
            bytes.extend_from_slice(&[0, b';', 0, b'1']);
        }
        if taken.insert(bytes.clone(), ()).is_none() {
            return bytes;
        }
    }
    unreachable!()
}

// ----- Rock Ridge entries --------------------------------------------------

fn entry(sig: &[u8; 2], body: &[u8]) -> Vec<u8> {
    let mut e = vec![sig[0], sig[1], (4 + body.len()) as u8, 1];
    e.extend_from_slice(body);
    e
}

fn rr_entries(n: &WNode, nlink: u32, with_name: bool) -> Vec<Vec<u8>> {
    let type_bits = match n.kind {
        EntryKind::Directory => 0o040000,
        EntryKind::Symlink => 0o120000,
        EntryKind::File => 0o100000,
        EntryKind::Other => n.type_bits,
    };
    let mut px = Vec::new();
    px.extend_from_slice(&both32(type_bits | (n.perm & 0o7777)));
    px.extend_from_slice(&both32(nlink));
    px.extend_from_slice(&both32(n.uid));
    px.extend_from_slice(&both32(n.gid));
    let mut out = vec![entry(b"PX", &px)];
    let mut tf = vec![0x0E];
    tf.extend_from_slice(&short_time(n.mtime));
    tf.extend_from_slice(&short_time(n.atime.unwrap_or(n.mtime)));
    tf.extend_from_slice(&short_time(n.ctime.unwrap_or(n.mtime)));
    out.push(entry(b"TF", &tf));
    if let Some((major, minor)) = n.dev {
        let mut pn = Vec::new();
        pn.extend_from_slice(&both32(major));
        pn.extend_from_slice(&both32(minor));
        out.push(entry(b"PN", &pn));
    }
    if with_name {
        let name = n.name.as_bytes();
        let chunks: Vec<&[u8]> = name.chunks(250).collect();
        for (i, c) in chunks.iter().enumerate() {
            let mut body = vec![if i + 1 < chunks.len() { 1 } else { 0 }];
            body.extend_from_slice(c);
            out.push(entry(b"NM", &body));
        }
    }
    if let Some(link) = &n.link {
        out.extend(sl_entries(link));
    }
    out
}

/// SL entries for a symbolic link's target.
fn sl_entries(target: &str) -> Vec<Vec<u8>> {
    // Component records: (flags, bytes).
    let mut comps: Vec<(u8, Vec<u8>)> = Vec::new();
    if target.starts_with('/') {
        comps.push((8, Vec::new()));
    }
    for part in target.split('/').filter(|p| !p.is_empty()) {
        match part {
            "." => comps.push((2, Vec::new())),
            ".." => comps.push((4, Vec::new())),
            p => {
                let chunks: Vec<&[u8]> = p.as_bytes().chunks(200).collect();
                for (i, c) in chunks.iter().enumerate() {
                    comps.push((if i + 1 < chunks.len() { 1 } else { 0 }, c.to_vec()));
                }
            }
        }
    }
    let mut entries: Vec<Vec<u8>> = Vec::new();
    let mut body: Vec<u8> = vec![0];
    for (flags, bytes) in comps {
        if body.len() + 2 + bytes.len() > 250 {
            body[0] = 1; // continued in the next SL
            entries.push(entry(b"SL", &body));
            body = vec![0];
        }
        body.push(flags);
        body.push(bytes.len() as u8);
        body.extend_from_slice(&bytes);
    }
    entries.push(entry(b"SL", &body));
    entries
}

fn er_entry() -> Vec<u8> {
    let (id, des, src): (&[u8], &[u8], &[u8]) = (
        b"RRIP_1991A",
        b"THE ROCK RIDGE INTERCHANGE PROTOCOL PROVIDES SUPPORT FOR POSIX FILE SYSTEM SEMANTICS",
        b"PLEASE CONTACT DISC PUBLISHER FOR SPECIFICATION SOURCE.",
    );
    let mut body = vec![id.len() as u8, des.len() as u8, src.len() as u8, 1];
    body.extend_from_slice(id);
    body.extend_from_slice(des);
    body.extend_from_slice(src);
    entry(b"ER", &body)
}

// ----- layout ---------------------------------------------------------------

/// What a directory record points at.
#[derive(Clone, Copy)]
enum Target {
    /// "." / "..": a directory.
    Dir(usize),
    /// A file's extent `i`.
    File(usize, usize),
    /// A symbolic link or device: no data.
    Nothing,
}

struct Rec {
    target: Target,
    ident: Vec<u8>,
    flags: u8,
    time: i64,
    /// System Use bytes in the record (without the CE entry).
    inline: Vec<u8>,
    /// Continuation areas (each followed, in the record or the area
    /// before, by a CE entry); their (block, offset) once placed.
    areas: Vec<Vec<u8>>,
    area_at: Vec<(u64, u64)>,
}

impl Rec {
    fn base_len(&self) -> usize {
        33 + self.ident.len() + (self.ident.len() + 1) % 2
    }

    fn len(&self) -> usize {
        let l = self.base_len() + self.inline.len() + if self.areas.is_empty() { 0 } else { 28 };
        l + l % 2
    }
}

/// Splits System Use entries between the record (`room` bytes) and
/// continuation areas.
fn pack(entries: Vec<Vec<u8>>, room: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    let total: usize = entries.iter().map(Vec::len).sum();
    if total <= room {
        return (entries.concat(), Vec::new());
    }
    let mut inline = Vec::new();
    let mut it = entries.into_iter().peekable();
    while let Some(e) = it.peek() {
        if inline.len() + e.len() + 28 > room {
            break;
        }
        inline.extend(it.next().unwrap());
    }
    let mut areas: Vec<Vec<u8>> = vec![Vec::new()];
    for e in it {
        let a = areas.last_mut().unwrap();
        if a.len() + e.len() + 28 > SECTOR as usize {
            areas.push(Vec::new());
        }
        areas.last_mut().unwrap().extend(e);
    }
    (inline, areas)
}

struct DirPlan {
    node: usize,
    parent: usize,
    recs: Vec<Rec>,
    lba: u64,
    sectors: u64,
}

impl DirPlan {
    /// Records never cross a sector boundary.
    fn size_sectors(recs: &[Rec]) -> u64 {
        let mut sectors = 1u64;
        let mut used = 0usize;
        for r in recs {
            if used + r.len() > SECTOR as usize {
                sectors += 1;
                used = 0;
            }
            used += r.len();
        }
        sectors
    }
}

/// Plans one tree (primary with Rock Ridge, or Joliet): directories in
/// breadth-first order (the path table order), each with its records.
fn plan_tree(nodes: &[WNode], joliet: bool, extents: &HashMap<usize, Vec<(u64, u64)>>) -> Vec<DirPlan> {
    let mut dirs = Vec::new();
    let mut queue = std::collections::VecDeque::from([(0usize, 0usize)]);
    while let Some((d, parent_dir)) = queue.pop_front() {
        let node = &nodes[d];
        let mut taken = HashMap::new();
        let mut kids: Vec<(Vec<u8>, usize)> = node
            .children
            .iter()
            .filter(|&&c| !joliet || matches!(nodes[c].kind, EntryKind::Directory | EntryKind::File))
            .map(|&c| {
                let dir = nodes[c].kind == EntryKind::Directory;
                let id = if joliet {
                    joliet_ident(&nodes[c].name, dir, &mut taken)
                } else {
                    iso_ident(&nodes[c].name, dir, &mut taken)
                };
                (id, c)
            })
            .collect();
        kids.sort();
        let subdirs = node.children.iter().filter(|&&c| nodes[c].kind == EntryKind::Directory).count() as u32;
        let mut recs = Vec::new();
        for (ident, target) in [(vec![0u8], Target::Dir(d)), (vec![1u8], Target::Dir(parent_dir))] {
            let mut r = Rec {
                target,
                ident,
                flags: 2,
                time: node.mtime,
                inline: Vec::new(),
                areas: Vec::new(),
                area_at: Vec::new(),
            };
            if !joliet && r.ident == [0] {
                let mut entries = Vec::new();
                if d == 0 {
                    entries.push(entry(b"SP", &[0xBE, 0xEF, 0]));
                }
                entries.extend(rr_entries(node, 2 + subdirs, false));
                if d == 0 {
                    entries.push(er_entry());
                }
                let room = 255 - r.base_len();
                (r.inline, r.areas) = pack(entries, room);
            }
            recs.push(r);
        }
        for (ident, c) in kids {
            let n = &nodes[c];
            let (flags, parts): (u8, Vec<Target>) = match n.kind {
                EntryKind::Directory => {
                    queue.push_back((c, d));
                    (2, vec![Target::Dir(c)])
                }
                EntryKind::File => {
                    let count = extents.get(&c).map_or(1, |e| e.len().max(1));
                    (0, (0..count).map(|i| Target::File(c, i)).collect())
                }
                _ => (0, vec![Target::Nothing]),
            };
            let nlink = if n.kind == EntryKind::Directory {
                2 + n.children.iter().filter(|&&k| nodes[k].kind == EntryKind::Directory).count() as u32
            } else {
                1
            };
            let last = parts.len() - 1;
            for (i, t) in parts.into_iter().enumerate() {
                let mut r = Rec {
                    target: t,
                    ident: ident.clone(),
                    flags: flags | if i < last { 0x80 } else { 0 },
                    time: n.mtime,
                    inline: Vec::new(),
                    areas: Vec::new(),
                    area_at: Vec::new(),
                };
                if !joliet {
                    let room = 255 - r.base_len();
                    (r.inline, r.areas) = pack(rr_entries(n, nlink, true), room);
                }
                recs.push(r);
            }
        }
        let sectors = DirPlan::size_sectors(&recs);
        dirs.push(DirPlan { node: d, parent: parent_dir, recs, lba: 0, sectors });
    }
    dirs
}

/// A path table (L: little-endian numbers, else big-endian).
fn path_table(dirs: &[DirPlan], idents: &[Vec<u8>], little: bool) -> Vec<u8> {
    let number: HashMap<usize, u16> = dirs.iter().enumerate().map(|(i, d)| (d.node, i as u16 + 1)).collect();
    let mut out = Vec::new();
    for (d, ident) in dirs.iter().zip(idents) {
        out.push(ident.len() as u8);
        out.push(0);
        let (lba, parent) = (d.lba as u32, number[&d.parent]);
        if little {
            out.extend_from_slice(&lba.to_le_bytes());
            out.extend_from_slice(&parent.to_le_bytes());
        } else {
            out.extend_from_slice(&lba.to_be_bytes());
            out.extend_from_slice(&parent.to_be_bytes());
        }
        out.extend_from_slice(ident);
        if ident.len() % 2 == 1 {
            out.push(0);
        }
    }
    out
}

/// The identifiers of the directories, as their parents' records name them.
fn dir_idents(dirs: &[DirPlan]) -> Vec<Vec<u8>> {
    let mut by_node: HashMap<usize, Vec<u8>> = HashMap::new();
    for d in dirs {
        for r in &d.recs[2..] {
            if let Target::Dir(c) = r.target {
                by_node.insert(c, r.ident.clone());
            }
        }
    }
    dirs.iter().map(|d| if d.node == 0 { vec![0] } else { by_node[&d.node].clone() }).collect()
}

fn sectors(bytes: u64) -> u64 {
    bytes.div_ceil(SECTOR)
}

/// Writes the image of `nodes` (node 0: the root).
pub(super) fn write(nodes: &[WNode], ids: &VolumeIds, boot: &BootSpec, io: &mut dyn IsoIo) -> Result<u64> {
    if nodes.first().is_none_or(|n| n.kind != EntryKind::Directory) {
        return Err(Error::invalid_input("the root must be a directory"));
    }
    let booting = boot.bios.is_some() || boot.efi.is_some();
    if !ids.iso && !ids.udf {
        return Err(Error::invalid_input("an image needs ISO 9660, UDF or both"));
    }
    if !ids.iso && booting {
        return Err(Error::invalid_input("booting from CD/DVD (El Torito) needs ISO 9660"));
    }
    for &n in boot.bios.iter().map(|(n, _, _)| n).chain(&boot.efi) {
        if nodes.get(n).is_none_or(|x| x.kind != EntryKind::File || x.size == 0 || x.size > MAX_EXTENT) {
            return Err(Error::invalid_input("a boot image must be a file (not empty, under 4 GiB)"));
        }
    }
    if let Some((n, _, true)) = boot.bios
        && nodes[n].size < 64
    {
        return Err(Error::invalid_input("a boot image with a boot info table is at least 64 bytes"));
    }
    // Descriptors: primary, (El Torito boot record), Joliet, terminator;
    // with UDF, its descriptors and the start of its partition next.
    let descriptors_end = if ids.iso { 16 + 3 + booting as u64 } else { 16 };
    let udf = if ids.udf { Some(super::udf_write::Plan::new(nodes)?) } else { None };
    let mut lba = match &udf {
        Some(p) => super::udf_write::PARTITION + p.blocks,
        None => descriptors_end,
    };
    let mut extents: HashMap<usize, Vec<(u64, u64)>> = HashMap::new();
    let mut by_key: HashMap<u64, usize> = HashMap::new();
    let mut data_files: Vec<usize> = Vec::new();
    for (i, n) in nodes.iter().enumerate().filter(|(_, n)| n.kind == EntryKind::File) {
        if let Some(k) = n.content_key
            && let Some(&first) = by_key.get(&k)
        {
            extents.insert(i, Vec::new()); // placed later: the same as `first`
            by_key.entry(k).or_insert(first);
            continue;
        }
        if let Some(k) = n.content_key {
            by_key.insert(k, i);
        }
        data_files.push(i);
        let parts = n.size.div_ceil(MAX_EXTENT).max(1);
        extents.insert(i, vec![(0, 0); parts as usize]);
    }
    // Shared files have as many extents as the one they share with.
    for (i, n) in nodes.iter().enumerate().filter(|(_, n)| n.kind == EntryKind::File) {
        if extents[&i].is_empty() {
            let first = by_key[&n.content_key.unwrap()];
            let len = extents[&first].len();
            extents.insert(i, vec![(0, 0); len]);
        }
    }
    // UDF only: no ISO 9660 trees (and so no path tables or directories).
    let mut primary = if ids.iso { plan_tree(nodes, false, &extents) } else { Vec::new() };
    let mut joliet = if ids.iso { plan_tree(nodes, true, &extents) } else { Vec::new() };
    let (pi, ji) = (dir_idents(&primary), dir_idents(&joliet));
    // Path tables (sizes do not depend on addresses).
    let pt_p = path_table(&primary, &pi, true).len() as u64;
    let pt_j = path_table(&joliet, &ji, true).len() as u64;
    let pt_at = [lba, lba + sectors(pt_p), lba + 2 * sectors(pt_p), lba + 2 * sectors(pt_p) + sectors(pt_j)];
    lba = pt_at[3] + sectors(pt_j);
    // The boot catalog.
    let catalog = lba;
    if booting {
        lba += 1;
    }
    for d in primary.iter_mut().chain(joliet.iter_mut()) {
        d.lba = lba;
        lba += d.sectors;
    }
    // Continuation areas, packed into whole sectors.
    let ce_start = lba;
    let (mut ce_block, mut ce_used) = (lba, 0u64);
    let mut ce_any = false;
    for d in &mut primary {
        for r in &mut d.recs {
            for a in &r.areas {
                let len = a.len() as u64 + 28;
                if ce_used + len > SECTOR {
                    ce_block += 1;
                    ce_used = 0;
                }
                r.area_at.push((ce_block, ce_used));
                ce_used += len;
                ce_any = true;
            }
        }
    }
    lba = if ce_any { ce_block + 1 } else { ce_start };
    let ce_end = lba;
    // A hybrid image's UEFI image goes last: its own partition after the
    // ISO's (GPT partitions must not overlap).
    if boot.hybrid
        && let Some(e) = boot.efi
    {
        let owner = nodes[e].content_key.and_then(|k| by_key.get(&k).copied()).unwrap_or(e);
        if let Some(at) = data_files.iter().position(|&i| i == owner) {
            let i = data_files.remove(at);
            data_files.push(i);
        }
    }
    // File data.
    for &i in &data_files {
        let mut left = nodes[i].size;
        for e in extents.get_mut(&i).unwrap() {
            let len = left.min(MAX_EXTENT);
            *e = (if len == 0 { 0 } else { lba }, len);
            lba += sectors(len);
            left -= len;
        }
    }
    for (i, n) in nodes.iter().enumerate().filter(|(_, n)| n.kind == EntryKind::File) {
        let first = by_key.get(&n.content_key.unwrap_or(u64::MAX)).copied().unwrap_or(i);
        if first != i {
            let e = extents[&first].clone();
            extents.insert(i, e);
        }
    }
    // A hybrid image is a whole number of MiB (as isohybrid makes it).
    // A hybrid image is a whole number of MiB (as isohybrid makes it), with
    // room for the backup GPT (33 sectors of 512 bytes) at its end.
    // With UDF, its second anchor in the last sector (not with a GPT,
    // whose backup is there).
    let data_end = lba;
    let second_anchor = udf.is_some() && !(boot.hybrid && boot.gpt);
    let total = match (boot.hybrid, boot.gpt) {
        (true, true) => (lba + 9).next_multiple_of(512),
        (true, false) => (lba + second_anchor as u64).next_multiple_of(512),
        _ => lba + second_anchor as u64,
    };
    io.begin(total * SECTOR)?;
    let boot_lba = |n: usize| extents[&n][0].0;

    let dir_at: HashMap<usize, (u64, u64)> = primary.iter().map(|d| (d.node, (d.lba, d.sectors * SECTOR))).collect();
    let jdir_at: HashMap<usize, (u64, u64)> = joliet.iter().map(|d| (d.node, (d.lba, d.sectors * SECTOR))).collect();
    let record = |r: &Rec, at: &HashMap<usize, (u64, u64)>| -> Vec<u8> {
        let (ext, size) = match r.target {
            Target::Dir(d) => at[&d],
            Target::File(f, i) => extents[&f][i],
            Target::Nothing => (0, 0),
        };
        let mut b = vec![0u8; r.len()];
        b[0] = r.len() as u8;
        b[2..10].copy_from_slice(&both32(ext as u32));
        b[10..18].copy_from_slice(&both32(size as u32));
        b[18..25].copy_from_slice(&short_time(r.time));
        b[25] = r.flags;
        b[28..32].copy_from_slice(&both16(1));
        b[32] = r.ident.len() as u8;
        b[33..33 + r.ident.len()].copy_from_slice(&r.ident);
        let mut at = r.base_len();
        b[at..at + r.inline.len()].copy_from_slice(&r.inline);
        at += r.inline.len();
        if let (Some(a), Some(&(blk, off))) = (r.areas.first(), r.area_at.first()) {
            b[at..at + 28].copy_from_slice(&ce(blk, off, a.len() as u64 + if r.areas.len() > 1 { 28 } else { 0 }));
        }
        b
    };

    // System area (with the hybrid MBR), descriptors.
    let mut system = vec![0u8; 16 * SECTOR as usize];
    if boot.hybrid {
        system[..512].copy_from_slice(&hybrid_mbr(boot, total, &|n| boot_lba(n), &|n| nodes[n].size));
    }
    let gpt = (boot.hybrid && boot.gpt).then(|| {
        let esp = boot.efi.map(|e| (boot_lba(e) * 4, nodes[e].size.div_ceil(512)));
        Gpt::new(total * 4, lba * 4, esp)
    });
    if let Some(g) = &gpt {
        system[..512].copy_from_slice(&protective_mbr(boot, total, &|n| boot_lba(n)));
        system[512..512 + g.primary.len()].copy_from_slice(&g.primary);
    }
    io.put(&system)?;
    let root_rec = |at: &HashMap<usize, (u64, u64)>| {
        let r = Rec {
            target: Target::Dir(0),
            ident: vec![0],
            flags: 2,
            time: nodes[0].mtime,
            inline: Vec::new(),
            areas: Vec::new(),
            area_at: Vec::new(),
        };
        record(&r, at)
    };
    if ids.iso {
        let now = Some(dix_core::time::unix_now());
        for (kind, tree_at, pt_l, pt_m, pt_len) in
            [(1u8, &dir_at, pt_at[0], pt_at[1], pt_p), (2u8, &jdir_at, pt_at[2], pt_at[3], pt_j)]
        {
            let mut vd = vec![0u8; SECTOR as usize];
            vd[0] = kind;
            vd[1..6].copy_from_slice(b"CD001");
            vd[6] = 1;
            let text = |s: &str, len: usize, d: bool| if kind == 1 { field(s, len, d) } else { ucs2_field(s, len) };
            vd[8..40].copy_from_slice(&text("", 32, false));
            vd[40..72].copy_from_slice(&text(&ids.label, 32, true));
            vd[80..88].copy_from_slice(&both32(total as u32));
            if kind == 2 {
                vd[88..91].copy_from_slice(b"%/E");
            }
            vd[120..124].copy_from_slice(&both16(1));
            vd[124..128].copy_from_slice(&both16(1));
            vd[128..132].copy_from_slice(&both16(SECTOR as u16));
            vd[132..140].copy_from_slice(&both32(pt_len as u32));
            vd[140..144].copy_from_slice(&(pt_l as u32).to_le_bytes());
            vd[148..152].copy_from_slice(&(pt_m as u32).to_be_bytes());
            vd[156..190].copy_from_slice(&root_rec(tree_at));
            vd[190..318].copy_from_slice(&text("", 128, true));
            vd[318..446].copy_from_slice(&text(&ids.publisher, 128, false));
            vd[446..574].copy_from_slice(&text(&ids.preparer, 128, false));
            vd[574..702].copy_from_slice(&text(&ids.application, 128, false));
            // Copyright, abstract, bibliographic file ids (37 bytes each): none.
            vd[702..813].copy_from_slice(&field("", 111, true));
            vd[813..830].copy_from_slice(&long_time(now));
            vd[830..847].copy_from_slice(&long_time(now));
            vd[847..864].copy_from_slice(&long_time(None));
            vd[864..881].copy_from_slice(&long_time(None));
            vd[881] = 1;
            io.put(&vd)?;
            if kind == 1 && booting {
                let mut br = vec![0u8; SECTOR as usize];
                br[1..6].copy_from_slice(b"CD001");
                br[6] = 1;
                br[7..30].copy_from_slice(b"EL TORITO SPECIFICATION");
                br[0x47..0x4B].copy_from_slice(&(catalog as u32).to_le_bytes());
                io.put(&br)?;
            }
        }
        let mut term = vec![0u8; SECTOR as usize];
        term[0] = 255;
        term[1..6].copy_from_slice(b"CD001");
        term[6] = 1;
        io.put(&term)?;
    }
    if let Some(plan) = &udf {
        let part_len = data_end - super::udf_write::PARTITION;
        io.put(&super::udf_write::head(nodes, plan, &extents, &ids.label, descriptors_end, part_len)?)?;
    }

    // Path tables.
    for (dirs, idents) in [(&primary, &pi), (&joliet, &ji)] {
        for little in [true, false] {
            let mut t = path_table(dirs, idents, little);
            t.resize((sectors(t.len() as u64) * SECTOR) as usize, 0);
            io.put(&t)?;
        }
    }
    if booting {
        io.put(&boot_catalog(boot, &|n| boot_lba(n), &|n| nodes[n].size))?;
    }
    // Directories.
    for (dirs, at) in [(&primary, &dir_at), (&joliet, &jdir_at)] {
        for d in dirs.iter() {
            let mut b = Vec::with_capacity((d.sectors * SECTOR) as usize);
            for r in &d.recs {
                let used = b.len() % SECTOR as usize;
                if used + r.len() > SECTOR as usize {
                    b.resize(b.len() + SECTOR as usize - used, 0);
                }
                b.extend(record(r, at));
            }
            b.resize((d.sectors * SECTOR) as usize, 0);
            io.put(&b)?;
        }
    }
    // Continuation areas.
    if ce_any {
        let mut region = vec![0u8; ((ce_end - ce_start) * SECTOR) as usize];
        for d in &primary {
            for r in &d.recs {
                for (i, (a, &(blk, off))) in r.areas.iter().zip(&r.area_at).enumerate() {
                    let at = ((blk - ce_start) * SECTOR + off) as usize;
                    region[at..at + a.len()].copy_from_slice(a);
                    if let (Some(next), Some(&(nb, no))) = (r.areas.get(i + 1), r.area_at.get(i + 1)) {
                        let more = if r.areas.len() > i + 2 { 28 } else { 0 };
                        region[at + a.len()..at + a.len() + 28].copy_from_slice(&ce(nb, no, next.len() as u64 + more));
                    }
                }
            }
        }
        io.put(&region)?;
    }
    // File data.
    let mut buf = vec![0u8; 1 << 20];
    for &i in &data_files {
        let size = nodes[i].size;
        if let Some((n, _, true)) = boot.bios
            && n == i
        {
            // The boot info table: patched into the image's copy.
            let mut data = vec![0u8; size as usize];
            let mut got = 0;
            while got < data.len() {
                let k = io.read_file(i, got as u64, &mut data[got..])?;
                if k == 0 {
                    return Err(Error::corrupted(format!("{} ended early while being written", nodes[i].name)));
                }
                got += k;
            }
            boot_info_table(&mut data, 16, boot_lba(i));
            data.resize((sectors(size) * SECTOR) as usize, 0);
            io.put(&data)?;
            continue;
        }
        let mut off = 0u64;
        while off < size {
            let want = (size - off).min((buf.len()) as u64) as usize;
            let n = io.read_file(i, off, &mut buf[..want])?;
            if n == 0 {
                return Err(Error::corrupted(format!("{} ended early while being written", nodes[i].name)));
            }
            io.put(&buf[..n])?;
            off += n as u64;
        }
        let pad = (sectors(size) * SECTOR - size) as usize;
        io.put(&vec![0u8; pad])?;
    }
    let mut tail = vec![0u8; ((total - lba) * SECTOR) as usize];
    if second_anchor {
        let at = tail.len() - SECTOR as usize;
        tail[at..at + 512].copy_from_slice(&super::udf_write::anchor(total - 1));
    }
    if let Some(g) = &gpt {
        let at = tail.len() - g.backup.len();
        tail[at..].copy_from_slice(&g.backup);
    }
    io.put(&tail)?;
    Ok(total * SECTOR)
}

/// Fills in a boot image's boot info table (bytes 8..64): where the
/// primary volume descriptor and the image are, the image's length and a
/// checksum of its bytes from 64 on. isolinux and GRUB read it.
pub(super) fn boot_info_table(data: &mut [u8], pvd: u64, lba: u64) {
    let sum = data[64..].chunks(4).fold(0u32, |s, c| {
        let mut w = [0u8; 4];
        w[..c.len()].copy_from_slice(c);
        s.wrapping_add(u32::from_le_bytes(w))
    });
    data[8..12].copy_from_slice(&(pvd as u32).to_le_bytes());
    data[12..16].copy_from_slice(&(lba as u32).to_le_bytes());
    let len = data.len() as u32;
    data[16..20].copy_from_slice(&len.to_le_bytes());
    data[20..24].copy_from_slice(&sum.to_le_bytes());
    data[24..64].fill(0);
}

/// The El Torito boot catalog: the validation entry, the default entry
/// (BIOS, or UEFI alone) and a UEFI section.
fn boot_catalog(boot: &BootSpec, lba: &dyn Fn(usize) -> u64, size: &dyn Fn(usize) -> u64) -> Vec<u8> {
    let mut c = vec![0u8; SECTOR as usize];
    let entry = |e: &mut [u8], node: usize, load: u16| {
        e[0] = 0x88; // bootable, no emulation
        e[6..8].copy_from_slice(&load.to_le_bytes());
        e[8..12].copy_from_slice(&(lba(node) as u32).to_le_bytes());
    };
    // Validation entry: header, platform, checksum making the words sum to 0.
    c[0] = 1;
    c[1] = if boot.bios.is_some() { 0 } else { 0xEF };
    c[30] = 0x55;
    c[31] = 0xAA;
    let sum = c[..32].chunks_exact(2).fold(0u16, |s, w| s.wrapping_add(u16::from_le_bytes([w[0], w[1]])));
    c[28..30].copy_from_slice(&0u16.wrapping_sub(sum).to_le_bytes());
    let efi_sectors = |n: usize| size(n).div_ceil(512).min(0xFFFF) as u16;
    match (boot.bios, boot.efi) {
        (Some((b, load, _)), efi) => {
            entry(&mut c[32..64], b, load);
            if let Some(e) = efi {
                // A final section header for UEFI with one entry.
                c[64] = 0x91;
                c[65] = 0xEF;
                c[66..68].copy_from_slice(&1u16.to_le_bytes());
                entry(&mut c[96..128], e, efi_sectors(e));
            }
        }
        (None, Some(e)) => entry(&mut c[32..64], e, efi_sectors(e)),
        (None, None) => {}
    }
    c
}

/// Cylinder/head/sector of a 512-byte sector with 64 heads and 32 sectors
/// per track (isohybrid's geometry).
fn chs(s: u64) -> [u8; 3] {
    let (c, rest) = (s / (64 * 32), s % (64 * 32));
    if c > 1023 {
        return [0xFE, 0xFF, 0xFF];
    }
    [(rest / 32) as u8, ((rest % 32 + 1) as u8) | ((c >> 2) as u8 & 0xC0), c as u8]
}

/// The MBR of a hybrid image, as isohybrid writes it.
fn hybrid_mbr(boot: &BootSpec, total: u64, lba: &dyn Fn(usize) -> u64, size: &dyn Fn(usize) -> u64) -> [u8; 512] {
    let mut m = [0u8; 512];
    if let Some(code) = &boot.mbr_code {
        let n = code.len().min(432);
        m[..n].copy_from_slice(&code[..n]);
        // isohdpfx.bin finds the BIOS boot image here (in 512-byte sectors).
        if let Some((b, _, _)) = boot.bios {
            m[432..436].copy_from_slice(&((lba(b) * 4) as u32).to_le_bytes());
        }
    }
    let id = disk_id();
    m[440..444].copy_from_slice(&id.to_le_bytes());
    let part = |m: &mut [u8; 512], i: usize, active: bool, kind: u8, start: u64, len: u64| {
        let e = &mut m[446 + 16 * i..462 + 16 * i];
        e[0] = if active { 0x80 } else { 0 };
        e[1..4].copy_from_slice(&chs(start));
        e[4] = kind;
        e[5..8].copy_from_slice(&chs(start + len - 1));
        e[8..12].copy_from_slice(&(start as u32).to_le_bytes());
        e[12..16].copy_from_slice(&(len.min(u32::MAX as u64) as u32).to_le_bytes());
    };
    part(&mut m, 0, true, 0x17, 0, total * 4);
    if let Some(e) = boot.efi {
        part(&mut m, 1, false, 0xEF, lba(e) * 4, size(e).div_ceil(512));
    }
    m[510] = 0x55;
    m[511] = 0xAA;
    m
}

/// A protective MBR (one partition of type EE over the disk, marked active
/// for BIOSes that want one) with the hybrid's boot code.
fn protective_mbr(boot: &BootSpec, total: u64, lba: &dyn Fn(usize) -> u64) -> [u8; 512] {
    let mut m = hybrid_mbr(&BootSpec { efi: None, ..boot.clone() }, total, lba, &|_| 0);
    m[446..510].fill(0);
    let e = &mut m[446..462];
    e[0] = 0x80;
    e[1..4].copy_from_slice(&[0, 2, 0]);
    e[4] = 0xEE;
    e[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    e[8..12].copy_from_slice(&1u32.to_le_bytes());
    e[12..16].copy_from_slice(&((total * 4 - 1).min(u32::MAX as u64) as u32).to_le_bytes());
    m
}

/// A hybrid image's GPT: the primary header and entries (for bytes
/// 512.. of the image) and the backup (for its last 33 sectors).
struct Gpt {
    primary: Vec<u8>,
    backup: Vec<u8>,
}

impl Gpt {
    /// `sectors`: the image's size, `iso_end`: where the ISO's data ends,
    /// `esp`: the UEFI image (start, length), all in 512-byte sectors.
    fn new(sectors: u64, iso_end: u64, esp: Option<(u64, u64)>) -> Gpt {
        use dix_core::partition::edit::{parse_guid, random_guid};
        const ENTRIES: usize = 128;
        let mut entries = vec![0u8; ENTRIES * 128];
        let mut put = |i: usize, ty: &str, first: u64, last: u64, name: &str| {
            let e = &mut entries[i * 128..(i + 1) * 128];
            e[..16].copy_from_slice(&parse_guid(ty).unwrap());
            e[16..32].copy_from_slice(&random_guid());
            e[32..40].copy_from_slice(&first.to_le_bytes());
            e[40..48].copy_from_slice(&last.to_le_bytes());
            for (k, c) in name.encode_utf16().enumerate() {
                e[56 + 2 * k..58 + 2 * k].copy_from_slice(&c.to_le_bytes());
            }
        };
        const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
        const ESP: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
        // The ISO from sector 64 (32 KiB: its volume descriptors).
        let iso_last = esp.map_or(iso_end - 1, |(s, _)| s - 1);
        put(0, BASIC_DATA, 64, iso_last, "ISO9660");
        if let Some((s, len)) = esp {
            put(1, ESP, s, s + len - 1, "EFI boot");
        }
        let crc = dix_core::partition::crc32(&entries);
        let disk = dix_core::partition::edit::random_guid();
        let es = (ENTRIES * 128 / 512) as u64;
        let last = sectors - 1;
        let header = |my: u64, alt: u64, at: u64| {
            let mut h = vec![0u8; 512];
            h[..8].copy_from_slice(b"EFI PART");
            h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
            h[12..16].copy_from_slice(&92u32.to_le_bytes());
            h[24..32].copy_from_slice(&my.to_le_bytes());
            h[32..40].copy_from_slice(&alt.to_le_bytes());
            h[40..48].copy_from_slice(&(2 + es).to_le_bytes());
            h[48..56].copy_from_slice(&(last - 1 - es).to_le_bytes());
            h[56..72].copy_from_slice(&disk);
            h[72..80].copy_from_slice(&at.to_le_bytes());
            h[80..84].copy_from_slice(&(ENTRIES as u32).to_le_bytes());
            h[84..88].copy_from_slice(&128u32.to_le_bytes());
            h[88..92].copy_from_slice(&crc.to_le_bytes());
            let c = dix_core::partition::crc32(&h[..92]);
            h[16..20].copy_from_slice(&c.to_le_bytes());
            h
        };
        let mut primary = header(1, last, 2);
        primary.extend_from_slice(&entries);
        let mut backup = entries;
        backup.extend(header(last, 1, last - es));
        Gpt { primary, backup }
    }
}

/// A disk signature for the MBR (from the time and the process).
pub(super) fn disk_id() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(dix_core::time::unix_now_ns());
    h.finish() as u32 | 1
}

/// A CE entry: the next continuation area is `len` bytes at (block, offset).
fn ce(block: u64, offset: u64, len: u64) -> [u8; 28] {
    let mut e = [0u8; 28];
    e[..4].copy_from_slice(&[b'C', b'E', 28, 1]);
    e[4..12].copy_from_slice(&both32(block as u32));
    e[12..20].copy_from_slice(&both32(offset as u32));
    e[20..28].copy_from_slice(&both32(len as u32));
    e
}
