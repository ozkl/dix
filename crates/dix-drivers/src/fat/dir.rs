//! Directory entries: 32-byte short (8.3) entries, preceded by long file
//! name (LFN) entries holding the real name in UTF-16, 13 units each, last
//! part first, tied to their short entry by a checksum of its 8.3 name.
//!
//! Short names use code page 437 (the usual OEM code page); names that fit
//! 8.3 exactly in one case get no LFN (the case kept in the "NT" flags, as
//! Windows and Linux do), others get a `BASIS~N.EXT` short name.

pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_VOLUME: u8 = 0x08;
pub const ATTR_DIR: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
pub const ATTR_LFN: u8 = 0x0F;

/// Case flags (byte 12): base / extension stored in lower case.
const NT_LOWER_BASE: u8 = 0x08;
const NT_LOWER_EXT: u8 = 0x10;

/// Code page 437, bytes 0x80..=0xFF.
const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û',
    'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡',
    '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─',
    '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█',
    '▄', '▌', '▐', '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', '≡', '±', '≥',
    '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', ' ',
];

fn cp437(b: u8) -> char {
    if b < 0x80 { b as char } else { CP437_HIGH[(b - 0x80) as usize] }
}

pub fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

pub fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// The LFN checksum of an 8.3 name.
pub fn checksum(short: &[u8; 11]) -> u8 {
    short.iter().fold(0u8, |s, &c| ((s & 1) << 7).wrapping_add(s >> 1).wrapping_add(c))
}

/// The 8.3 name shown to people ("README.TXT", or "readme.txt" with the
/// case flags).
pub fn short_display(e: &[u8]) -> String {
    let mut raw: [u8; 11] = e[..11].try_into().unwrap();
    if raw[0] == 0x05 {
        raw[0] = 0xE5;
    }
    let case = |s: &[u8], lower: bool| -> String {
        let t: String = s.iter().map(|&b| cp437(b)).collect::<String>().trim_end().to_owned();
        if lower { t.to_lowercase() } else { t }
    };
    let base = case(&raw[..8], e[12] & NT_LOWER_BASE != 0);
    let ext = case(&raw[8..11], e[12] & NT_LOWER_EXT != 0);
    if ext.is_empty() { base } else { format!("{base}.{ext}") }
}

/// One directory entry as found: its name and where its slots are.
#[derive(Debug, Clone)]
pub struct Item {
    pub name: String,
    /// The 32-byte short entry.
    pub raw: [u8; 32],
    /// Device offsets of the short entry and of its LFN entries.
    pub offset: u64,
    pub lfn_offsets: Vec<u64>,
}

impl Item {
    pub fn attr(&self) -> u8 {
        self.raw[11]
    }

    pub fn is_dir(&self) -> bool {
        self.attr() & ATTR_DIR != 0
    }

    pub fn cluster(&self) -> u32 {
        (le16(&self.raw, 20) as u32) << 16 | le16(&self.raw, 26) as u32
    }

    pub fn size(&self) -> u32 {
        le32(&self.raw, 28)
    }

    pub fn short(&self) -> [u8; 11] {
        self.raw[..11].try_into().unwrap()
    }
}

/// Long-name parts being collected: (count, checksum, UTF-16 units, slot
/// offsets, the next part expected).
type LfnRun = (u8, u8, Vec<u16>, Vec<u64>, u8);

/// Reads a directory's slots (`(device offset, entry)` in order) into its
/// items. Deleted entries, `.`/`..` and the volume label are left out;
/// LFN entries whose checksum does not match are ignored (an old long
/// name, left behind by a system that only knows 8.3).
pub fn parse(slots: impl Iterator<Item = (u64, [u8; 32])>) -> (Vec<Item>, Option<String>) {
    let mut items = Vec::new();
    let mut label = None;
    // LFN parts being collected: (count, checksum, units, offsets, next ord).
    let mut lfn: Option<LfnRun> = None;
    for (off, e) in slots {
        if e[0] == 0x00 {
            break;
        }
        if e[0] == 0xE5 {
            lfn = None;
            continue;
        }
        if e[11] & 0x3F == ATTR_LFN {
            let ord = e[0] & 0x1F;
            if e[0] & 0x40 != 0 {
                lfn = (ord >= 1).then(|| (ord, e[13], vec![0xFFFF; ord as usize * 13], Vec::new(), ord));
            }
            match &mut lfn {
                Some((_, sum, units, offs, next)) if *next == ord && e[13] == *sum && ord >= 1 => {
                    let base = (ord as usize - 1) * 13;
                    let parts = (1..11).step_by(2).chain((14..26).step_by(2)).chain((28..32).step_by(2));
                    for (k, o) in parts.enumerate() {
                        units[base + k] = le16(&e, o);
                    }
                    offs.push(off);
                    *next = ord - 1;
                }
                _ => lfn = None,
            }
            continue;
        }
        let pending = lfn.take();
        if e[11] & ATTR_VOLUME != 0 && e[11] & ATTR_DIR == 0 {
            if label.is_none() {
                // 11 characters, not 8.3.
                let text: String = e[..11].iter().map(|&b| cp437(b)).collect();
                label = Some(text.trim_end().to_owned()).filter(|l| !l.is_empty());
            }
            continue;
        }
        if &e[..11] == b".          " || &e[..11] == b"..         " {
            continue;
        }
        let short: [u8; 11] = e[..11].try_into().unwrap();
        let (name, lfn_offsets) = match pending {
            Some((_, sum, units, offs, 0)) if sum == checksum(&short) => {
                let end = units.iter().position(|&u| u == 0 || u == 0xFFFF).unwrap_or(units.len());
                (String::from_utf16_lossy(&units[..end]), offs)
            }
            _ => (short_display(&e), Vec::new()),
        };
        if name.is_empty() {
            continue;
        }
        items.push(Item { name, raw: e, offset: off, lfn_offsets });
    }
    (items, label)
}

/// Characters allowed in short names besides A-Z and 0-9.
fn short_char(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || "!#$%&'()-@^_`{}~".contains(c)
}

/// An exact 8.3 form of `name`, if it has one: (name, case flags).
fn exact_short(name: &str) -> Option<([u8; 11], u8)> {
    let (base, ext) = match name.rfind('.') {
        Some(0) => return None,
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || ext.contains('.') {
        return None;
    }
    // One case per part (kept in the flags); mixed case needs a long name.
    let part = |s: &str| -> Option<(String, bool)> {
        let lower = s.chars().any(|c| c.is_ascii_lowercase());
        let upper = s.chars().any(|c| c.is_ascii_uppercase());
        if lower && upper {
            return None;
        }
        let up = s.to_ascii_uppercase();
        up.chars().all(short_char).then_some((up, lower))
    };
    let (b, bl) = part(base)?;
    let (x, xl) = part(ext)?;
    let mut out = [b' '; 11];
    out[..b.len()].copy_from_slice(b.as_bytes());
    out[8..8 + x.len()].copy_from_slice(x.as_bytes());
    let flags = if bl { NT_LOWER_BASE } else { 0 } | if xl { NT_LOWER_EXT } else { 0 };
    Some((out, flags))
}

/// The short name and case flags for `name`, and whether it needs LFN
/// entries; `taken` says whether an 8.3 name is used in the directory.
pub fn short_for(name: &str, taken: &dyn Fn(&[u8; 11]) -> bool) -> ([u8; 11], u8, bool) {
    if let Some((s, flags)) = exact_short(name)
        && !taken(&s)
    {
        return (s, flags, false);
    }
    // A basis name: upper case, no spaces, no leading dots, other
    // characters as '_', 8 + 3 at most; then "~N" until unique.
    let clean = |s: &str| -> String {
        s.chars()
            .filter(|&c| c != ' ' && c != '.')
            .map(|c| {
                let u = c.to_ascii_uppercase();
                if u.is_ascii() && short_char(u) { u } else { '_' }
            })
            .collect()
    };
    let trimmed = name.trim_start_matches('.');
    let (base, ext) = match trimmed.rfind('.') {
        Some(i) => (clean(&trimmed[..i]), clean(&trimmed[i + 1..])),
        None => (clean(trimmed), String::new()),
    };
    let base = if base.is_empty() { "_".to_owned() } else { base };
    let ext: String = ext.chars().take(3).collect();
    for n in 1..=999_999u32 {
        let tail = format!("~{n}");
        let keep = 8 - tail.len();
        let b: String = base.chars().take(keep).collect();
        let mut out = [b' '; 11];
        let full = format!("{b}{tail}");
        out[..full.len()].copy_from_slice(full.as_bytes());
        out[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
        if !taken(&out) {
            return (out, 0, true);
        }
    }
    // A directory with a million similar names: give up on uniqueness.
    ([b'_'; 11], 0, true)
}

/// The LFN entries for `name` (in on-disk order: last part first).
pub fn lfn_entries(name: &str, short: &[u8; 11]) -> Vec<[u8; 32]> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let count = units.len().div_ceil(13);
    let sum = checksum(short);
    let mut out = Vec::new();
    for ord in (1..=count).rev() {
        let mut e = [0u8; 32];
        e[0] = ord as u8 | if ord == count { 0x40 } else { 0 };
        e[11] = ATTR_LFN;
        e[13] = sum;
        let places = (1..11).step_by(2).chain((14..26).step_by(2)).chain((28..32).step_by(2));
        for (k, o) in places.enumerate() {
            let i = (ord - 1) * 13 + k;
            let u = match i.cmp(&units.len()) {
                std::cmp::Ordering::Less => units[i],
                std::cmp::Ordering::Equal => 0x0000,
                std::cmp::Ordering::Greater => 0xFFFF,
            };
            e[o..o + 2].copy_from_slice(&u.to_le_bytes());
        }
        out.push(e);
    }
    out
}

/// A short entry.
pub fn short_entry(short: &[u8; 11], flags: u8, attr: u8, cluster: u32, size: u32, now: i64) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(short);
    if e[0] == 0xE5 {
        e[0] = 0x05;
    }
    e[11] = attr;
    e[12] = flags;
    let (time, date) = dix_core::time::to_dos(now);
    e[14..16].copy_from_slice(&time.to_le_bytes()); // created
    e[16..18].copy_from_slice(&date.to_le_bytes());
    e[18..20].copy_from_slice(&date.to_le_bytes()); // accessed
    e[22..24].copy_from_slice(&time.to_le_bytes()); // modified
    e[24..26].copy_from_slice(&date.to_le_bytes());
    set_cluster(&mut e, cluster);
    e[28..32].copy_from_slice(&size.to_le_bytes());
    e
}

pub fn set_cluster(e: &mut [u8; 32], cluster: u32) {
    e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
}

pub fn set_modified(e: &mut [u8; 32], now: i64) {
    let (time, date) = dix_core::time::to_dos(now);
    e[22..24].copy_from_slice(&time.to_le_bytes());
    e[24..26].copy_from_slice(&date.to_le_bytes());
    e[18..20].copy_from_slice(&date.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names() {
        let none = |_: &[u8; 11]| false;
        assert_eq!(short_for("README.TXT", &none), (*b"README  TXT", 0, false));
        assert_eq!(short_for("readme.txt", &none), (*b"README  TXT", NT_LOWER_BASE | NT_LOWER_EXT, false));
        assert_eq!(short_for("ReadMe.txt", &none).0, *b"README~1TXT");
        assert!(short_for("ReadMe.txt", &none).2);
        assert_eq!(short_for("a long file name.html", &none).0, *b"ALONGF~1HTM");
        assert_eq!(short_for(".bashrc", &none).0, *b"BASHRC~1   ");
        assert_eq!(short_for("ünïcødé.txt", &none).0, *b"_N_C_D~1TXT");
        let taken = |s: &[u8; 11]| s == b"ALONGF~1HTM" || s == b"ALONGF~2HTM";
        assert_eq!(short_for("a long file name.html", &taken).0, *b"ALONGF~3HTM");
    }

    #[test]
    fn long_names_round_trip() {
        let name = "A rather long name, with ünïcødé – and 30+ characters.txt";
        let (short, flags, lfn) = short_for(name, &|_| false);
        assert!(lfn);
        let mut slots = Vec::new();
        for (i, e) in lfn_entries(name, &short).into_iter().enumerate() {
            slots.push((i as u64 * 32, e));
        }
        let n = slots.len() as u64;
        slots.push((n * 32, short_entry(&short, flags, ATTR_ARCHIVE, 5, 7, 1_700_000_000)));
        let (items, _) = parse(slots.into_iter());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, name);
        assert_eq!(items[0].lfn_offsets.len() as u64, n);
        assert_eq!((items[0].cluster(), items[0].size()), (5, 7));
        // A stale long name (checksum of another short name) is ignored.
        let mut slots: Vec<(u64, [u8; 32])> = lfn_entries(name, b"OTHER   TXT").into_iter().map(|e| (0, e)).collect();
        slots.push((64, short_entry(&short, 0, ATTR_ARCHIVE, 5, 7, 0)));
        let (items, _) = parse(slots.into_iter());
        assert_eq!(items[0].name, short_display(&items[0].raw));
    }
}
