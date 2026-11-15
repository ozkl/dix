//! ARJ archives, read-only.
//!
//! An ARJ archive is a main header, then a header and the data of each file
//! (`0x60 0xEA`, size, a fixed part, name and comment, CRC-32, extended
//! headers), ending with an empty header. Methods: 0 stored; 1–3 LZ77 with
//! static Huffman codes per block (a 26 KiB window, the one coder at three
//! levels); 4 "fastest", LZ77 with simple variable-length numbers.
//! Garbled (password) files are listed and say why they cannot be read.
//!
//! **Volumes** (`x.arj`, `x.a01`, `x.a02`, …) are read joined (see
//! `dix_core::device::JoinedDevice`): a file split between volumes is
//! stored as separately compressed sections, one per volume, each with its
//! own size and checksum.
//!
//! The decoders are written for this project from the format as ARJ's
//! author documented it (and as the public-domain UNARJ implements it).

use super::{Data, Node, Raw, Tree, clean_path};
use dix_core::{EntryKind, Error, MountLog, Result};

const ID: [u8; 2] = [0x60, 0xEA];
/// The largest basic header ARJ writes.
const MAX_HEADER: usize = 2600;

/// One piece of a file's data (a file split over volumes has several).
#[derive(Debug, Clone)]
pub(crate) struct Section {
    pub at: u64,
    pub packed: u64,
    pub size: u64,
    pub crc: u32,
    pub method: u8,
}

#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub name: String,
    pub sections: Vec<Section>,
    /// Why it cannot be read (garbled, an unknown method, a volume missing).
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Archive {
    pub entries: Vec<Entry>,
}

fn bad(what: &str) -> Error {
    Error::corrupted(format!("a damaged ARJ archive ({what})"))
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// A header at `at`: its basic part, and where what follows it starts
/// (after the extended headers). `None` for the end-of-archive header.
fn header(raw: &mut Raw<'_>, at: u64) -> Result<Option<(Vec<u8>, u64)>> {
    let size = raw.size();
    let mut id = [0u8; 4];
    if at + 4 > size {
        return Err(bad("it ends early"));
    }
    raw.read_at(at, &mut id)?;
    if id[..2] != ID {
        return Err(bad("a header is missing"));
    }
    let bs = le16(&id, 2) as usize;
    if bs == 0 {
        return Ok(None);
    }
    if !(30..=MAX_HEADER).contains(&bs) || at + 4 + bs as u64 + 4 > size {
        return Err(bad("a header has a wrong size"));
    }
    let mut b = vec![0u8; bs + 4];
    raw.read_at(at + 4, &mut b)?;
    if dix_core::partition::crc32(&b[..bs]) != le32(&b, bs) {
        return Err(bad("a header checksum does not match"));
    }
    b.truncate(bs);
    // Extended headers: skipped.
    let mut p = at + 4 + bs as u64 + 4;
    loop {
        let mut n = [0u8; 2];
        if p + 2 > size {
            return Err(bad("it ends early"));
        }
        raw.read_at(p, &mut n)?;
        p += 2;
        let es = u16::from_le_bytes(n) as u64;
        if es == 0 {
            break;
        }
        p += es + 4;
    }
    Ok(Some((b, p)))
}

/// Where the archive starts: at 0, or after a self-extractor.
fn find_start(raw: &mut Raw<'_>) -> Result<Option<u64>> {
    let mut head = vec![0u8; raw.size().min(256 << 10) as usize];
    raw.read_at(0, &mut head)?;
    let mut i = 0;
    while let Some(k) = head[i..].windows(2).position(|w| w == ID) {
        let at = (i + k) as u64;
        if let Ok(Some((b, _))) = header(raw, at)
            && b[6] == 2
        {
            return Ok(Some(at));
        }
        i += k + 1;
    }
    Ok(None)
}

/// Whether `raw` is an ARJ archive (for probing).
pub(super) fn is_arj(raw: &mut Raw<'_>) -> Result<bool> {
    let mut id = [0u8; 2];
    if raw.size() < 4 {
        return Ok(false);
    }
    raw.read_at(0, &mut id)?;
    // Only at the start: a self-extractor is an executable first.
    Ok(id == ID && matches!(header(raw, 0), Ok(Some((b, _))) if b[6] == 2))
}

/// Reads the member list.
pub(super) fn parse(raw: &mut Raw<'_>, log: &MountLog) -> Result<(Tree, Archive)> {
    let mut at = find_start(raw)?.ok_or_else(|| bad("no main header"))?;
    let mut tree = Tree::new();
    let mut arc = Archive::default();
    let mut open: Option<usize> = None;
    let mut volumes = 1;
    loop {
        // The main header (one per volume).
        let (main, next) = header(raw, at)?.ok_or_else(|| bad("no main header"))?;
        let more_volumes = main[4] & 0x04 != 0;
        at = next;
        while let Some((b, next)) = header(raw, at)? {
            at = next;
            let first = b[0] as usize;
            let (host, flags, method, ftype) = (b[3], b[4], b[5], b[6]);
            let (time, packed, size, crc) = (le32(&b, 8), le32(&b, 12) as u64, le32(&b, 16) as u64, le32(&b, 20));
            let mode = le16(&b, 26);
            if first > b.len() {
                return Err(bad("a header has a wrong size"));
            }
            let mut strings = b[first..].split(|&c| c == 0);
            let raw_name = strings.next().unwrap_or(&[]);
            let name = String::from_utf8_lossy(raw_name).replace('\\', "/");
            let data_at = at;
            at += packed;
            if !matches!(ftype, 0 | 1 | 3) {
                continue; // volume and chapter labels
            }
            let Some(path) = clean_path(&name) else {
                log.warn(format!("Skipped a member with an unusable name: {name:?}"));
                continue;
            };
            let section = Section { at: data_at, packed, size, crc, method };
            // The rest of a file from the previous volume.
            if flags & 0x08 != 0
                && let Some(i) = open.take().filter(|&i| arc.entries[i].name == path)
            {
                arc.entries[i].sections.push(section);
                if flags & 0x04 != 0 {
                    open = Some(i);
                }
                let total: u64 = arc.entries[i].sections.iter().map(|s| s.size).sum();
                if let Some(n) = tree.nodes.get_mut(&path) {
                    n.size = total;
                }
                continue;
            }
            let mut node = Node::new(if ftype == 3 { EntryKind::Directory } else { EntryKind::File });
            // UNIX hosts store Unix times, the others DOS times.
            node.mtime =
                if host == 2 { Some(time as i64) } else { dix_core::time::from_dos((time >> 16) as u16, time as u16) };
            if host == 2 {
                node.mode = Some(mode as u32 & 0o7777);
            }
            if node.kind == EntryKind::Directory {
                tree.update_dir(&path, node);
                continue;
            }
            let problem = if flags & 0x01 != 0 {
                Some("it is garbled (password protected)".to_owned())
            } else if flags & 0x08 != 0 {
                Some("it starts in the previous volume, which is missing".to_owned())
            } else if method > 4 {
                Some(format!("it uses an unknown ARJ method ({method})"))
            } else {
                None
            };
            let index = arc.entries.len();
            node.size = size;
            node.data = if method == 0 && problem.is_none() && flags & 0x04 == 0 {
                Data::Range { offset: data_at, len: packed.min(size) }
            } else {
                Data::Arj { index }
            };
            arc.entries.push(Entry { name: path.clone(), sections: vec![section], problem });
            open = (flags & 0x04 != 0).then_some(index);
            tree.insert(&path, node);
        }
        if !more_volumes {
            break;
        }
        // The next volume: at the next part (or right after this one).
        let end = at + 4;
        let next = match raw.part_starts() {
            Some(starts) => starts.into_iter().find(|&s| s >= end && s < raw.size()),
            None => (end + 4 <= raw.size()).then_some(end),
        };
        match next {
            Some(n) if header(raw, n).is_ok_and(|h| h.is_some_and(|(b, _)| b[6] == 2)) => {
                at = n;
                volumes += 1;
            }
            _ => {
                log.warn(format!(
                    "This archive goes on in volume {}, which is missing (the parts must be next to each other, with their names).",
                    volumes + 1
                ));
                break;
            }
        }
    }
    if let Some(i) = open
        && arc.entries[i].problem.is_none()
    {
        arc.entries[i].problem = Some("it continues in the next volume, which is missing".to_owned());
        let path = arc.entries[i].name.clone();
        if let Some(n) = tree.nodes.get_mut(&path) {
            n.data = Data::Arj { index: i };
        }
    }
    if volumes > 1 {
        log.info(format!("An ARJ archive in {volumes} volumes."));
    }
    Ok((tree, arc))
}

/// A member's whole content.
pub(super) fn read_member(arc: &Archive, raw: &mut Raw<'_>, index: usize) -> Result<Vec<u8>> {
    let e = arc.entries.get(index).ok_or_else(|| bad("a member is missing"))?;
    if let Some(p) = &e.problem {
        return Err(Error::new(dix_core::ErrorKind::Unsupported, format!("{} cannot be read: {p}", e.name)));
    }
    let mut out = Vec::new();
    for s in &e.sections {
        if s.packed > 1 << 32 || s.size > 1 << 32 {
            return Err(bad("a member is too large"));
        }
        let mut packed = vec![0u8; s.packed as usize];
        raw.read_at(s.at, &mut packed)?;
        let start = out.len();
        match s.method {
            0 => out.extend_from_slice(&packed[..packed.len().min(s.size as usize)]),
            1..=3 => decode(&packed, s.size as usize, &mut out)?,
            4 => decode_fastest(&packed, s.size as usize, &mut out)?,
            _ => return Err(bad("an unknown method")),
        }
        if out.len() - start != s.size as usize {
            return Err(bad("a member's data ends early"));
        }
        if dix_core::partition::crc32(&out[start..]) != s.crc {
            return Err(Error::corrupted(format!("{}: checksum mismatch", e.name)));
        }
    }
    Ok(out)
}

// ----- decoding --------------------------------------------------------------

/// Bits, most significant first; zeros past the end.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    /// The next bits, left-aligned.
    buf: u64,
    have: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0, buf: 0, have: 0 }
    }

    fn fill(&mut self) {
        while self.have <= 56 {
            let b = self.data.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.buf |= (b as u64) << (56 - self.have);
            self.have += 8;
        }
    }

    /// The next 16 bits, not taken.
    fn peek16(&mut self) -> u32 {
        if self.have < 16 {
            self.fill();
        }
        (self.buf >> 48) as u32
    }

    fn skip(&mut self, n: u32) {
        if self.have < n {
            self.fill();
        }
        self.buf <<= n;
        self.have -= n;
    }

    fn get(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek16() >> (16 - n);
        self.skip(n);
        v
    }

    /// Well past the end of the data: a damaged stream.
    fn overrun(&self) -> bool {
        self.pos > self.data.len() + 64
    }
}

/// A canonical Huffman code (or one symbol with no bits).
struct Huff {
    /// By the next `bits` bits: (symbol, length).
    table: Vec<(u16, u8)>,
    bits: u32,
}

impl Huff {
    fn constant(sym: u16) -> Self {
        Huff { table: vec![(sym, 0)], bits: 0 }
    }

    fn new(lens: &[u8]) -> Result<Self> {
        let max = lens.iter().copied().max().unwrap_or(0) as u32;
        if max == 0 || max > 16 {
            return Err(bad("a code table"));
        }
        let mut table = vec![(0u16, 0u8); 1 << max];
        let mut code = 0u32;
        let mut filled = 0usize;
        for len in 1..=max {
            for (sym, _) in lens.iter().enumerate().filter(|&(_, &l)| l as u32 == len) {
                let span = 1usize << (max - len);
                let start = (code << (max - len)) as usize;
                if start + span > table.len() {
                    return Err(bad("a code table"));
                }
                table[start..start + span].fill((sym as u16, len as u8));
                filled += span;
                code += 1;
            }
            code <<= 1;
        }
        // ARJ's tables are always complete.
        if filled != table.len() {
            return Err(bad("a code table"));
        }
        Ok(Huff { table, bits: max })
    }

    fn decode(&self, b: &mut Bits<'_>) -> u16 {
        let (sym, len) = self.table[(b.peek16() >> (16 - self.bits)) as usize];
        b.skip(len as u32);
        sym
    }
}

const NC: usize = 510;
const NP: usize = 17;
const NT: usize = 19;

/// Code lengths for the length code or the position code.
fn read_pt_len(b: &mut Bits<'_>, nn: usize, nbit: u32, special: Option<usize>) -> Result<Huff> {
    let n = b.get(nbit) as usize;
    if n == 0 {
        return Ok(Huff::constant(b.get(nbit) as u16));
    }
    if n > nn {
        return Err(bad("a code table"));
    }
    let mut lens = vec![0u8; nn];
    let mut i = 0;
    while i < n {
        let bits = b.peek16();
        let mut c = bits >> 13;
        if c == 7 {
            let mut mask = 1 << 12;
            while bits & mask != 0 {
                mask >>= 1;
                c += 1;
            }
        }
        b.skip(if c < 7 { 3 } else { c - 3 });
        lens[i] = c as u8;
        i += 1;
        if Some(i) == special {
            let zeros = b.get(2) as usize;
            i += zeros;
            if i > nn {
                return Err(bad("a code table"));
            }
        }
    }
    Huff::new(&lens)
}

fn read_c_len(b: &mut Bits<'_>, pt: &Huff) -> Result<Huff> {
    let n = b.get(9) as usize;
    if n == 0 {
        return Ok(Huff::constant(b.get(9) as u16));
    }
    if n > NC {
        return Err(bad("a code table"));
    }
    let mut lens = vec![0u8; NC];
    let mut i = 0;
    while i < n {
        let c = pt.decode(b);
        if c <= 2 {
            let zeros = match c {
                0 => 1,
                1 => b.get(4) as usize + 3,
                _ => b.get(9) as usize + 20,
            };
            i += zeros;
            if i > NC {
                return Err(bad("a code table"));
            }
        } else {
            lens[i] = (c - 2) as u8;
            i += 1;
        }
    }
    Huff::new(&lens)
}

/// Copies a match: `len` bytes from `dist + 1` back.
fn copy(out: &mut Vec<u8>, dist: usize, len: usize) -> Result<()> {
    let from = out.len().checked_sub(dist + 1).ok_or_else(|| bad("a match reaches before the start"))?;
    for k in 0..len {
        let c = out[from + k];
        out.push(c);
    }
    Ok(())
}

/// Methods 1–3.
fn decode(packed: &[u8], size: usize, out: &mut Vec<u8>) -> Result<()> {
    let end = out.len() + size;
    let start = out.len();
    let mut b = Bits::new(packed);
    let mut block = 0u32;
    let (mut c_code, mut p_code) = (Huff::constant(0), Huff::constant(0));
    while out.len() < end {
        if block == 0 {
            block = b.get(16);
            let pt = read_pt_len(&mut b, NT, 5, Some(3))?;
            c_code = read_c_len(&mut b, &pt)?;
            p_code = read_pt_len(&mut b, NP, 5, None)?;
            if b.overrun() {
                return Err(bad("the data ends early"));
            }
        }
        block = block.wrapping_sub(1);
        let c = c_code.decode(&mut b) as usize;
        if c <= 255 {
            out.push(c as u8);
        } else {
            let len = c - 253;
            let p = p_code.decode(&mut b) as u32;
            let dist = if p == 0 { 0 } else { (1usize << (p - 1)) + b.get(p - 1) as usize };
            if out.len() - start < dist + 1 {
                return Err(bad("a match reaches before the start"));
            }
            copy(out, dist, len)?;
        }
        if b.overrun() {
            return Err(bad("the data ends early"));
        }
    }
    out.truncate(end);
    Ok(())
}

/// A number: up to `stop - start` one bits pick a width, then that many bits.
fn fast_number(b: &mut Bits<'_>, start: u32, stop: u32) -> usize {
    let (mut plus, mut pwr) = (0usize, 1usize << start);
    let mut width = start;
    while width < stop {
        if b.get(1) == 0 {
            break;
        }
        plus += pwr;
        pwr <<= 1;
        width += 1;
    }
    plus + b.get(width) as usize
}

/// Method 4.
fn decode_fastest(packed: &[u8], size: usize, out: &mut Vec<u8>) -> Result<()> {
    let end = out.len() + size;
    let start = out.len();
    let mut b = Bits::new(packed);
    while out.len() < end {
        let c = fast_number(&mut b, 0, 7);
        if c == 0 {
            out.push(b.get(8) as u8);
        } else {
            let len = c - 1 + 3;
            let dist = fast_number(&mut b, 9, 13);
            if out.len() - start < dist + 1 {
                return Err(bad("a match reaches before the start"));
            }
            copy(out, dist, len)?;
        }
        if b.overrun() {
            return Err(bad("the data ends early"));
        }
    }
    out.truncate(end);
    Ok(())
}
