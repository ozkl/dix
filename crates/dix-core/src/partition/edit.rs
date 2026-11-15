//! Editing partition tables: create a table, create and delete partitions,
//! change their type, name and boot flag, and write the result.
//!
//! A [`Layout`] is the whole table in memory, in sectors. It is read from a
//! device ([`Layout::read`]) or made empty ([`Layout::new`]), changed by its
//! methods (each checks that the result is valid) and written in one go by
//! [`Layout::write`]. Nothing touches the device before that, so a caller
//! can keep the original, show what changed and let the user cancel.
//!
//! * **GPT**: a protective MBR (the boot code in sector 0 is kept), the
//!   primary header and entry array after it, a backup of both at the end
//!   of the device, CRC32s. Partition and disk GUIDs and the attribute bits
//!   of existing partitions are kept. Writing to a device that has grown
//!   moves the backup to the new end.
//! * **MBR**: up to four primary partitions, one of which may be an
//!   extended partition holding logical partitions (numbered from 5 in disk
//!   order), each preceded by its EBR; the first EBR is at the start of the
//!   extended partition. Sizes are limited to 2³² sectors (2 TiB).
//!
//! New partitions start on 1 MiB boundaries, and their first MiB is zeroed
//! when written, so that a file system that lived there before is not
//! found again. Writing an MBR over a GPT wipes the GPT headers (readers
//! prefer a GPT).

use super::{crc32, gpt_type_name, guid, le32, le64, looks_like_volume_boot_record, mbr_type_name, Scheme};
use crate::device::BlockDevice;
use crate::error::{Error, ErrorKind, Result};

/// New partitions start on multiples of this.
pub const ALIGN: u64 = 1 << 20;

/// The GPT entry array size written (as everyone does: 128 entries of 128
/// bytes).
const GPT_ENTRIES: u32 = 128;

/// What a partition is, in the table's own terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeCode {
    Gpt([u8; 16]),
    Mbr(u8),
}

impl TypeCode {
    /// The type's name ("Linux filesystem", "FAT32").
    pub fn name(self) -> String {
        match self {
            TypeCode::Gpt(g) => gpt_type_name(&guid(&g)).map_or_else(|| guid(&g), str::to_owned),
            TypeCode::Mbr(b) => mbr_type_name(b).to_owned(),
        }
    }

    /// The raw code: a GUID or `0x83`.
    pub fn code(self) -> String {
        match self {
            TypeCode::Gpt(g) => guid(&g),
            TypeCode::Mbr(b) => format!("0x{b:02X}"),
        }
    }

    pub fn is_extended(self) -> bool {
        matches!(self, TypeCode::Mbr(0x05 | 0x0F | 0x85))
    }
}

/// A partition type to choose from, with its code in each scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownType {
    pub name: &'static str,
    pub gpt: Option<&'static str>,
    pub mbr: Option<u8>,
}

impl KnownType {
    pub fn code(&self, scheme: Scheme) -> Option<TypeCode> {
        match scheme {
            Scheme::Gpt => self.gpt.and_then(parse_guid).map(TypeCode::Gpt),
            Scheme::Mbr => self.mbr.map(TypeCode::Mbr),
            Scheme::Apm => None,
        }
    }
}

/// The types offered when creating a partition (the first fits most uses).
pub const KNOWN_TYPES: &[KnownType] = &[
    KnownType { name: "Linux filesystem", gpt: Some("0FC63DAF-8483-4772-8E79-3D69D8477DE4"), mbr: Some(0x83) },
    KnownType { name: "Windows data (FAT32)", gpt: Some("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"), mbr: Some(0x0C) },
    KnownType {
        name: "Windows data (NTFS / exFAT)",
        gpt: Some("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"),
        mbr: Some(0x07),
    },
    KnownType { name: "Windows data (FAT16)", gpt: Some("EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"), mbr: Some(0x0E) },
    KnownType { name: "EFI system", gpt: Some("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"), mbr: Some(0xEF) },
    KnownType { name: "Linux swap", gpt: Some("0657FD6D-A4AB-43C4-84E5-0933C84B4F4F"), mbr: Some(0x82) },
    KnownType { name: "Linux LVM", gpt: Some("E6D6D379-F507-44C2-A23C-238F2A3DF928"), mbr: Some(0x8E) },
    KnownType { name: "Linux RAID", gpt: Some("A19D880F-05FC-4D3B-A006-743F0F84911E"), mbr: Some(0xFD) },
    KnownType { name: "Linux root (x86-64)", gpt: Some("4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709"), mbr: None },
    KnownType { name: "Linux root (ARM64)", gpt: Some("B921B045-1DF0-41C3-AF44-4C6F280D3FAE"), mbr: None },
    KnownType { name: "Linux /home", gpt: Some("933AC7E1-2EB4-4F13-B844-0E14E2AEF915"), mbr: None },
    KnownType { name: "BIOS boot", gpt: Some("21686148-6449-6E6F-744E-656564454649"), mbr: None },
    KnownType { name: "Microsoft reserved", gpt: Some("E3C9E316-0B5C-4DB8-817D-F92DF00215AE"), mbr: None },
    KnownType { name: "Windows recovery", gpt: Some("DE94BBA4-06D1-4D40-A16A-BFD50179D6AC"), mbr: Some(0x27) },
    KnownType { name: "Apple APFS", gpt: Some("7C3457EF-0000-11AA-AA11-00306543ECAC"), mbr: None },
    KnownType { name: "Apple HFS+", gpt: Some("48465300-0000-11AA-AA11-00306543ECAC"), mbr: Some(0xAF) },
    KnownType { name: "FreeBSD data", gpt: Some("516E7CB4-6ECF-11D6-8FF8-00022D09712B"), mbr: Some(0xA5) },
    KnownType { name: "Extended (holds logical partitions)", gpt: None, mbr: Some(0x0F) },
];

/// The type that suits a file system made by a driver's mkfs (`"ext4"`,
/// `"fat32"`, `"exfat"`; variant ids).
pub fn type_for_file_system(variant: &str, scheme: Scheme) -> Option<TypeCode> {
    let name = match variant {
        v if v.starts_with("ext") => "Linux filesystem",
        "fat32" => "Windows data (FAT32)",
        "fat16" | "fat12" => "Windows data (FAT16)",
        "exfat" | "ntfs" => "Windows data (NTFS / exFAT)",
        _ => return None,
    };
    let mut t = KNOWN_TYPES.iter().find(|t| t.name == name)?.code(scheme)?;
    if variant == "fat12" && scheme == Scheme::Mbr {
        t = TypeCode::Mbr(0x01);
    }
    Some(t)
}

/// A partition being edited. Sectors are of the table's sector size;
/// `last` is inclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// As the OS numbers it (MBR logical partitions from 5).
    pub number: u32,
    pub first: u64,
    pub last: u64,
    pub type_code: TypeCode,
    /// GPT name.
    pub name: String,
    /// GPT unique partition GUID.
    pub guid: [u8; 16],
    /// GPT attribute bits (bit 2: legacy BIOS bootable).
    pub attrs: u64,
    /// MBR active flag.
    pub active: bool,
    /// MBR logical partitions: the sector of the EBR describing it.
    pub ebr: Option<u64>,
    /// Created in this edit: its first MiB is zeroed when written.
    pub new: bool,
}

impl Slot {
    pub fn sectors(&self) -> u64 {
        self.last - self.first + 1
    }

    pub fn is_extended(&self) -> bool {
        self.type_code.is_extended()
    }

    pub fn is_logical(&self) -> bool {
        self.ebr.is_some()
    }

    pub fn bootable(&self) -> bool {
        match self.type_code {
            TypeCode::Gpt(_) => self.attrs & 0b100 != 0,
            TypeCode::Mbr(_) => self.active,
        }
    }
}

/// Unused space where a partition can go: `first..=last`, `first` aligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Free {
    pub first: u64,
    pub last: u64,
    /// Inside the extended partition (a new partition here is logical).
    pub logical: bool,
}

impl Free {
    pub fn sectors(&self) -> u64 {
        self.last - self.first + 1
    }
}

/// A whole partition table in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub scheme: Scheme,
    pub sector_size: u64,
    /// The device's size in sectors.
    pub sectors: u64,
    pub disk_guid: [u8; 16],
    pub disk_signature: u32,
    /// Sorted by number.
    pub slots: Vec<Slot>,
    /// GPT: the number of entries in the array.
    entries: u32,
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, msg.into())
}

impl Layout {
    /// An empty table for a device of `size` bytes.
    pub fn new(scheme: Scheme, size: u64, sector_size: u64) -> Result<Layout> {
        let ss = match scheme {
            Scheme::Mbr => 512,
            Scheme::Gpt => sector_size,
            Scheme::Apm => return Err(Error::unsupported("making Apple partition maps")),
        };
        if !matches!(ss, 512 | 4096) {
            return Err(invalid(format!("unsupported sector size {ss}")));
        }
        if size < 3 * ALIGN {
            return Err(invalid("the device is too small for a partition table with a partition"));
        }
        let l = Layout {
            scheme,
            sector_size: ss,
            sectors: size / ss,
            disk_guid: random_guid(),
            disk_signature: u32::from_le_bytes(random_guid()[..4].try_into().unwrap()),
            slots: Vec::new(),
            entries: GPT_ENTRIES,
        };
        if l.free().iter().all(|f| f.sectors() * ss < ALIGN) {
            return Err(invalid("the device is too small for a partition table with a partition"));
        }
        Ok(l)
    }

    /// The table on `dev`, if it has one.
    pub fn read(dev: &mut dyn BlockDevice) -> Result<Option<Layout>> {
        let Some(t) = super::read_partition_table(dev)? else { return Ok(None) };
        if t.scheme == Scheme::Apm {
            return Err(Error::unsupported("editing Apple partition maps (they can be read)"));
        }
        let ss = t.sector_size as u64;
        let sectors = dev.size() / ss;
        match t.scheme {
            // Layouts are never APM: `new` and `read` refuse it.
            Scheme::Apm => unreachable!("an Apple partition map layout"),
            Scheme::Gpt => {
                let last = sectors - 1;
                let h = match super::read_gpt_header(dev, ss, 1)? {
                    Some(h) if super::read_gpt_entries(dev, ss, &h)?.is_some() => h,
                    _ => super::read_gpt_header(dev, ss, last)?.ok_or_else(|| Error::corrupted("no GPT header"))?,
                };
                let entries =
                    super::read_gpt_entries(dev, ss, &h)?.ok_or_else(|| Error::corrupted("no GPT entries"))?;
                let mut slots = Vec::new();
                for (i, e) in entries.chunks_exact(h.entry_size as usize).enumerate() {
                    if e[..16].iter().all(|&b| b == 0) {
                        continue;
                    }
                    let name: Vec<u16> = (0..36).map(|k| super::le16(e, 56 + 2 * k)).take_while(|&c| c != 0).collect();
                    slots.push(Slot {
                        number: i as u32 + 1,
                        first: le64(e, 32),
                        last: le64(e, 40),
                        type_code: TypeCode::Gpt(e[..16].try_into().unwrap()),
                        name: String::from_utf16_lossy(&name),
                        guid: e[16..32].try_into().unwrap(),
                        attrs: le64(e, 48),
                        active: false,
                        ebr: None,
                        new: false,
                    });
                }
                Ok(Some(Layout {
                    scheme: Scheme::Gpt,
                    sector_size: ss,
                    sectors,
                    disk_guid: parse_guid(t.disk_id.as_deref().unwrap_or_default()).unwrap_or_default(),
                    disk_signature: 0,
                    slots,
                    entries: h.count.max(GPT_ENTRIES),
                }))
            }
            Scheme::Mbr => {
                let mut s = [0u8; 512];
                dev.read_at(0, &mut s)?;
                let entries = super::mbr_entries(&s).ok_or_else(|| Error::corrupted("a damaged MBR"))?;
                let mut slots = Vec::new();
                let mut ext = None;
                for (i, e) in entries.iter().enumerate() {
                    let Some(e) = e else { continue };
                    slots.push(mbr_slot(i as u32 + 1, e, 0, None));
                    if super::is_extended(e.kind) && ext.is_none() {
                        ext = Some(e.start);
                    }
                }
                if let Some(ext_start) = ext {
                    let mut ebr = ext_start;
                    let mut seen = std::collections::BTreeSet::new();
                    let mut number = 5;
                    while seen.insert(ebr) && seen.len() <= 128 && ebr < sectors {
                        let mut b = [0u8; 512];
                        dev.read_at(ebr * 512, &mut b)?;
                        if b[510..512] != [0x55, 0xAA] {
                            break;
                        }
                        let Some(es) = super::mbr_entries(&b) else { break };
                        if let Some(e) = &es[0] {
                            slots.push(mbr_slot(number, e, ebr, Some(ebr)));
                            number += 1;
                        }
                        match &es[1] {
                            Some(next) if super::is_extended(next.kind) => ebr = ext_start + next.start,
                            _ => break,
                        }
                    }
                }
                slots.sort_by_key(|s| s.number);
                Ok(Some(Layout {
                    scheme: Scheme::Mbr,
                    sector_size: 512,
                    sectors,
                    disk_guid: [0; 16],
                    disk_signature: le32(&s, 440),
                    slots,
                    entries: 0,
                }))
            }
        }
    }

    pub fn get(&self, number: u32) -> Option<&Slot> {
        self.slots.iter().find(|s| s.number == number)
    }

    fn get_mut(&mut self, number: u32) -> Result<&mut Slot> {
        self.slots
            .iter_mut()
            .find(|s| s.number == number)
            .ok_or_else(|| Error::not_found(format!("partition {number}")))
    }

    pub fn extended(&self) -> Option<&Slot> {
        self.slots.iter().find(|s| s.is_extended())
    }

    /// Sectors per alignment step.
    pub fn align(&self) -> u64 {
        ALIGN / self.sector_size
    }

    /// Where partitions may lie: `first..=last`.
    pub fn usable(&self) -> (u64, u64) {
        match self.scheme {
            // Layouts are never APM: `new` and `read` refuse it.
            Scheme::Apm => unreachable!("an Apple partition map layout"),
            Scheme::Gpt => {
                let es = (self.entries as u64 * 128).div_ceil(self.sector_size);
                (2 + es, self.sectors.saturating_sub(2 + es))
            }
            Scheme::Mbr => (1, (self.sectors - 1).min(u32::MAX as u64)),
        }
    }

    /// Unused space, in disk order: outside any partition (or, for MBR,
    /// inside the extended partition outside its logical ones), aligned.
    /// Pieces smaller than the alignment step are left out.
    pub fn free(&self) -> Vec<Free> {
        let (lo, hi) = self.usable();
        let top: Vec<(u64, u64)> = self.slots.iter().filter(|s| !s.is_logical()).map(|s| (s.first, s.last)).collect();
        let mut out: Vec<Free> =
            gaps(lo, hi, &top).into_iter().filter_map(|(a, b)| self.aligned(a, b, false)).collect();
        if let Some(ext) = self.extended() {
            // A logical partition needs a sector for its EBR before it (the
            // first one at the start of the extended partition).
            let inner: Vec<(u64, u64)> =
                self.slots.iter().filter(|s| s.is_logical()).map(|s| (s.first - 1, s.last)).collect();
            out.extend(gaps(ext.first, ext.last, &inner).into_iter().filter_map(|(a, b)| self.aligned(a + 1, b, true)));
        }
        out.sort_by_key(|f| f.first);
        out
    }

    fn aligned(&self, a: u64, b: u64, logical: bool) -> Option<Free> {
        let first = a.div_ceil(self.align()) * self.align();
        (first <= b && b - first + 1 >= self.align()).then_some(Free { first, last: b, logical })
    }

    /// Creates a partition on `first..=last` (sectors), which must lie in
    /// free space. On MBR it is logical when inside the extended partition;
    /// a [`TypeCode::is_extended`] type makes the extended partition.
    /// Returns its number.
    pub fn add(&mut self, first: u64, last: u64, type_code: TypeCode, name: &str) -> Result<u32> {
        if first > last {
            return Err(invalid("the partition ends before it starts"));
        }
        match (self.scheme, type_code) {
            (Scheme::Gpt, TypeCode::Gpt(_)) | (Scheme::Mbr, TypeCode::Mbr(_)) => {}
            _ => return Err(invalid("that type does not belong to this kind of partition table")),
        }
        if type_code == TypeCode::Gpt([0; 16]) || type_code == TypeCode::Mbr(0) {
            return Err(invalid("the partition type cannot be empty"));
        }
        let (lo, hi) = self.usable();
        if first < lo || last > hi {
            return Err(invalid("the partition does not fit on the device (or within what the table can address)"));
        }
        let ext = self.extended().map(|e| (e.first, e.last));
        let inside_ext = ext.is_some_and(|(a, b)| first > a && last <= b);
        let logical = self.scheme == Scheme::Mbr && inside_ext;
        if type_code.is_extended() && (ext.is_some()) {
            return Err(invalid("there is already an extended partition"));
        }
        // Must not overlap anything at its level.
        let clash = self.slots.iter().any(|s| {
            let (a, b) = if logical {
                if !s.is_logical() {
                    return false;
                }
                (s.first - 1, s.last)
            } else {
                if s.is_logical() {
                    return false;
                }
                (s.first, s.last)
            };
            let lo = if logical { first - 1 } else { first };
            lo <= b && a <= last
        });
        if clash || (!logical && ext.is_some_and(|(a, b)| first <= b && a <= last)) {
            return Err(invalid("the partition overlaps another one"));
        }
        let number = match self.scheme {
            // Layouts are never APM: `new` and `read` refuse it.
            Scheme::Apm => unreachable!("an Apple partition map layout"),
            Scheme::Gpt => (1..=self.entries)
                .find(|n| self.get(*n).is_none())
                .ok_or_else(|| invalid("the partition table is full"))?,
            Scheme::Mbr if logical => 5, // renumbered below
            Scheme::Mbr => (1..=4).find(|n| self.get(*n).is_none()).ok_or_else(|| {
                invalid("an MBR holds four primary partitions; create logical ones in an extended partition")
            })?,
        };
        if logical && type_code.is_extended() {
            return Err(invalid("an extended partition cannot be inside another"));
        }
        let ebr = logical.then_some(first - 1); // placed by renumber()
        if self.scheme == Scheme::Gpt && name.encode_utf16().count() > 36 {
            // (Checked before anything changes.)
            return Err(invalid("a GPT partition name has at most 36 characters"));
        }
        self.slots.push(Slot {
            number,
            first,
            last,
            type_code,
            name: if self.scheme == Scheme::Gpt { name.to_owned() } else { String::new() },
            guid: if self.scheme == Scheme::Gpt { random_guid() } else { [0; 16] },
            attrs: 0,
            active: false,
            ebr,
            new: !type_code.is_extended(),
        });
        self.renumber();
        Ok(self.slots.iter().find(|s| s.first == first && s.last == last).unwrap().number)
    }

    /// Logical partitions are numbered in disk order from 5 (the EBR chain
    /// is written in that order). The first EBR is at the start of the
    /// extended partition; the others up to one alignment step before their
    /// partition, after the previous one.
    fn renumber(&mut self) {
        let align = self.align();
        let ext_first = self.extended().map(|e| e.first);
        let mut logical: Vec<&mut Slot> = self.slots.iter_mut().filter(|s| s.ebr.is_some()).collect();
        logical.sort_by_key(|s| s.first);
        let mut prev_end = None;
        for (i, s) in logical.into_iter().enumerate() {
            s.number = 5 + i as u32;
            s.ebr = Some(match (i, ext_first, prev_end) {
                (0, Some(f), _) => f,
                (_, _, Some(p)) => s.first.saturating_sub(align).max(p + 1),
                _ => s.ebr.unwrap(),
            });
            prev_end = Some(s.last);
        }
        self.slots.sort_by_key(|s| s.number);
    }

    /// Deletes a partition (an extended one with its logical partitions;
    /// later logical partitions move down a number).
    pub fn delete(&mut self, number: u32) -> Result<()> {
        let s = self.get(number).ok_or_else(|| Error::not_found(format!("partition {number}")))?;
        let extended = s.is_extended();
        self.slots.retain(|s| s.number != number && !(extended && s.is_logical()));
        self.renumber();
        Ok(())
    }

    pub fn set_type(&mut self, number: u32, type_code: TypeCode) -> Result<()> {
        let scheme = self.scheme;
        let has_logical = self.slots.iter().any(|s| s.is_logical());
        let s = self.get_mut(number)?;
        match (scheme, type_code) {
            (Scheme::Gpt, TypeCode::Gpt(g)) if g != [0; 16] => {}
            (Scheme::Mbr, TypeCode::Mbr(b)) if b != 0 => {
                if s.is_extended() != type_code.is_extended() && (s.is_logical() || has_logical || !s.is_extended()) {
                    return Err(invalid("a partition cannot be turned into or out of an extended partition"));
                }
            }
            _ => return Err(invalid("that type does not belong to this kind of partition table")),
        }
        s.type_code = type_code;
        Ok(())
    }

    pub fn set_name(&mut self, number: u32, name: &str) -> Result<()> {
        if self.scheme != Scheme::Gpt {
            return Err(invalid("only GPT partitions have names"));
        }
        if name.encode_utf16().count() > 36 {
            return Err(invalid("a GPT partition name has at most 36 characters"));
        }
        self.get_mut(number)?.name = name.to_owned();
        Ok(())
    }

    /// The boot flag: MBR "active" (one partition at most, a primary one),
    /// GPT "legacy BIOS bootable".
    pub fn set_bootable(&mut self, number: u32, on: bool) -> Result<()> {
        if self.scheme == Scheme::Mbr {
            let s = self.get(number).ok_or_else(|| Error::not_found(format!("partition {number}")))?;
            if s.is_logical() || s.is_extended() {
                return Err(invalid("only a primary partition can be active"));
            }
            for s in &mut self.slots {
                s.active = on && s.number == number;
            }
        } else {
            let s = self.get_mut(number)?;
            s.attrs = if on { s.attrs | 0b100 } else { s.attrs & !0b100 };
        }
        Ok(())
    }

    /// Checks the whole layout (what [`write`](Self::write) would write).
    pub fn validate(&self) -> Result<()> {
        let (lo, hi) = self.usable();
        let top: Vec<&Slot> = self.slots.iter().filter(|s| !s.is_logical()).collect();
        for s in &self.slots {
            if s.first > s.last || (!s.is_logical() && (s.first < lo || s.last > hi)) {
                return Err(Error::corrupted(format!("partition {} lies outside the usable area", s.number)));
            }
        }
        for (i, a) in top.iter().enumerate() {
            if top[i + 1..].iter().any(|b| a.first <= b.last && b.first <= a.last) {
                return Err(Error::corrupted(format!("partition {} overlaps another one", a.number)));
            }
        }
        if self.scheme == Scheme::Mbr {
            if top.len() > 4 || top.iter().filter(|s| s.is_extended()).count() > 1 {
                return Err(Error::corrupted("too many primary or extended partitions"));
            }
            let logical: Vec<&Slot> = self.slots.iter().filter(|s| s.is_logical()).collect();
            if let Some(ext) = self.extended() {
                for (i, l) in logical.iter().enumerate() {
                    let ebr = l.ebr.unwrap();
                    if ebr < ext.first || ebr >= l.first || l.last > ext.last {
                        return Err(Error::corrupted(format!(
                            "logical partition {} lies outside the extended one",
                            l.number
                        )));
                    }
                    if logical[i + 1..].iter().any(|m| ebr <= m.last && m.ebr.unwrap() <= l.last) {
                        return Err(Error::corrupted(format!("partition {} overlaps another one", l.number)));
                    }
                }
            } else if !logical.is_empty() {
                return Err(Error::corrupted("logical partitions without an extended partition"));
            }
        }
        Ok(())
    }

    /// Writes the table to `dev` (the whole device the layout was made for).
    pub fn write(&self, dev: &mut dyn BlockDevice) -> Result<()> {
        self.validate()?;
        if dev.size() / self.sector_size != self.sectors {
            return Err(invalid("the device changed size since the table was read"));
        }
        if dev.is_read_only() {
            return Err(Error::read_only());
        }
        let ss = self.sector_size;
        // Sector 0: keep boot code, unless it is a file system's boot sector.
        let mut mbr = [0u8; 512];
        dev.read_at(0, &mut mbr)?;
        if looks_like_volume_boot_record(&mbr) || mbr[510..512] != [0x55, 0xAA] {
            mbr = [0u8; 512];
        }
        mbr[440..510].fill(0);
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        match self.scheme {
            // Layouts are never APM: `new` and `read` refuse it.
            Scheme::Apm => unreachable!("an Apple partition map layout"),
            Scheme::Gpt => {
                let last = self.sectors - 1;
                let mut entries = vec![0u8; self.entries as usize * 128];
                for s in &self.slots {
                    let e = &mut entries[(s.number as usize - 1) * 128..s.number as usize * 128];
                    let TypeCode::Gpt(t) = s.type_code else { unreachable!() };
                    e[..16].copy_from_slice(&t);
                    e[16..32].copy_from_slice(&s.guid);
                    e[32..40].copy_from_slice(&s.first.to_le_bytes());
                    e[40..48].copy_from_slice(&s.last.to_le_bytes());
                    e[48..56].copy_from_slice(&s.attrs.to_le_bytes());
                    for (k, c) in s.name.encode_utf16().take(36).enumerate() {
                        e[56 + 2 * k..58 + 2 * k].copy_from_slice(&c.to_le_bytes());
                    }
                }
                let es = (entries.len() as u64).div_ceil(ss);
                let (lo, hi) = self.usable();
                let ecrc = crc32(&entries);
                let header = |my: u64, alt: u64, at: u64| {
                    let mut h = vec![0u8; ss as usize];
                    h[..8].copy_from_slice(super::GPT_SIGNATURE);
                    h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
                    h[12..16].copy_from_slice(&92u32.to_le_bytes());
                    h[24..32].copy_from_slice(&my.to_le_bytes());
                    h[32..40].copy_from_slice(&alt.to_le_bytes());
                    h[40..48].copy_from_slice(&lo.to_le_bytes());
                    h[48..56].copy_from_slice(&hi.to_le_bytes());
                    h[56..72].copy_from_slice(&self.disk_guid);
                    h[72..80].copy_from_slice(&at.to_le_bytes());
                    h[80..84].copy_from_slice(&self.entries.to_le_bytes());
                    h[84..88].copy_from_slice(&128u32.to_le_bytes());
                    h[88..92].copy_from_slice(&ecrc.to_le_bytes());
                    let c = crc32(&h[..92]);
                    h[16..20].copy_from_slice(&c.to_le_bytes());
                    h
                };
                // Protective MBR: one 0xEE entry over the whole disk.
                let e = &mut mbr[446..462];
                e.copy_from_slice(&[0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0]);
                e[8..12].copy_from_slice(&1u32.to_le_bytes());
                e[12..16].copy_from_slice(&((self.sectors - 1).min(u32::MAX as u64) as u32).to_le_bytes());
                self.wipe(dev)?;
                dev.write_at(0, &mbr)?;
                dev.write_at(2 * ss, &entries)?;
                dev.write_at((last - es) * ss, &entries)?;
                dev.write_at(last * ss, &header(last, 1, last - es))?;
                dev.write_at(ss, &header(1, last, 2))?;
            }
            Scheme::Mbr => {
                mbr[440..444].copy_from_slice(&self.disk_signature.to_le_bytes());
                for s in self.slots.iter().filter(|s| !s.is_logical()) {
                    let at = 446 + 16 * (s.number as usize - 1);
                    mbr[at..at + 16].copy_from_slice(&mbr_entry(s.active, s.type_code, s.first, s.sectors(), 0));
                }
                self.wipe(dev)?;
                // Old GPT headers would win over the MBR: remove them.
                for gss in [512u64, 4096] {
                    for lba in [1, dev.size() / gss - 1] {
                        let mut b = [0u8; 8];
                        if (lba + 1) * gss <= dev.size() {
                            dev.read_at(lba * gss, &mut b)?;
                            if &b == super::GPT_SIGNATURE {
                                dev.write_at(lba * gss, &vec![0u8; gss as usize])?;
                            }
                        }
                    }
                }
                if let Some(ext) = self.extended() {
                    let logical: Vec<&Slot> = self.slots.iter().filter(|s| s.is_logical()).collect();
                    if logical.is_empty() {
                        let mut b = [0u8; 512];
                        b[510] = 0x55;
                        b[511] = 0xAA;
                        dev.write_at(ext.first * 512, &b)?;
                    }
                    for (i, l) in logical.iter().enumerate() {
                        let ebr = l.ebr.unwrap();
                        let mut b = [0u8; 512];
                        b[446..462].copy_from_slice(&mbr_entry(false, l.type_code, l.first - ebr, l.sectors(), ebr));
                        if let Some(n) = logical.get(i + 1) {
                            let ne = n.ebr.unwrap();
                            b[462..478].copy_from_slice(&mbr_entry(
                                false,
                                TypeCode::Mbr(0x05),
                                ne - ext.first,
                                n.last - ne + 1,
                                ext.first,
                            ));
                        }
                        b[510] = 0x55;
                        b[511] = 0xAA;
                        dev.write_at(ebr * 512, &b)?;
                    }
                }
                dev.write_at(0, &mbr)?;
            }
        }
        dev.flush()
    }

    /// Zeroes the first MiB of each new partition.
    fn wipe(&self, dev: &mut dyn BlockDevice) -> Result<()> {
        let zero = vec![0u8; ALIGN as usize];
        for s in self.slots.iter().filter(|s| s.new) {
            let len = (s.sectors() * self.sector_size).min(ALIGN) as usize;
            dev.write_at(s.first * self.sector_size, &zero[..len])?;
        }
        Ok(())
    }

    /// The partitions as [`super::Partition`]s (what reading the written
    /// table would give).
    pub fn partitions(&self) -> Vec<super::Partition> {
        let ss = self.sector_size;
        self.slots
            .iter()
            .map(|s| super::Partition {
                number: s.number,
                start: s.first * ss,
                size: s.sectors() * ss,
                type_name: s.type_code.name(),
                type_id: s.type_code.code(),
                name: (!s.name.is_empty()).then(|| s.name.clone()),
                guid: (self.scheme == Scheme::Gpt).then(|| guid(&s.guid)),
                bootable: s.bootable(),
                truncated: false,
            })
            .collect()
    }
}

fn mbr_slot(number: u32, e: &super::MbrEntry, base: u64, ebr: Option<u64>) -> Slot {
    let first = base + e.start;
    Slot {
        number,
        first,
        last: first + e.sectors - 1,
        type_code: TypeCode::Mbr(e.kind),
        name: String::new(),
        guid: [0; 16],
        attrs: 0,
        active: e.bootable,
        ebr,
        new: false,
    }
}

/// An MBR entry; `start` relative to its table, `abs` the absolute start
/// (for the CHS fields).
fn mbr_entry(active: bool, t: TypeCode, start: u64, sectors: u64, base: u64) -> [u8; 16] {
    let TypeCode::Mbr(kind) = t else { unreachable!() };
    let abs = base + start;
    let mut e = [0u8; 16];
    e[0] = if active { 0x80 } else { 0 };
    e[1..4].copy_from_slice(&chs(abs));
    e[4] = kind;
    e[5..8].copy_from_slice(&chs(abs + sectors - 1));
    e[8..12].copy_from_slice(&(start as u32).to_le_bytes());
    e[12..16].copy_from_slice(&(sectors as u32).to_le_bytes());
    e
}

/// Cylinder/head/sector of an LBA with 255 heads and 63 sectors per track
/// (or the "too far" marker), as fdisk writes them.
fn chs(lba: u64) -> [u8; 3] {
    let (c, rest) = (lba / (255 * 63), lba % (255 * 63));
    if c > 1023 {
        return [0xFE, 0xFF, 0xFF];
    }
    let (h, s) = (rest / 63, rest % 63 + 1);
    [h as u8, (s as u8) | ((c >> 2) as u8 & 0xC0), c as u8]
}

/// Free gaps in `lo..=hi` between `used` ranges.
fn gaps(lo: u64, hi: u64, used: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut used = used.to_vec();
    used.sort();
    let mut out = Vec::new();
    let mut at = lo;
    for (a, b) in used {
        if a > at && at <= hi {
            out.push((at, (a - 1).min(hi)));
        }
        at = at.max(b.saturating_add(1));
    }
    if at <= hi {
        out.push((at, hi));
    }
    out
}

/// Parses `0FC63DAF-8483-4772-8E79-3D69D8477DE4` into GPT byte order.
pub fn parse_guid(s: &str) -> Option<[u8; 16]> {
    let hex: Vec<u8> = s.bytes().filter(|c| *c != b'-').collect();
    if hex.len() != 32 || s.len() != 36 {
        return None;
    }
    let mut raw = [0u8; 16];
    for (i, r) in raw.iter_mut().enumerate() {
        *r = u8::from_str_radix(std::str::from_utf8(&hex[2 * i..2 * i + 2]).ok()?, 16).ok()?;
    }
    let mut b = raw;
    b[..4].copy_from_slice(&[raw[3], raw[2], raw[1], raw[0]]);
    b[4..6].copy_from_slice(&[raw[5], raw[4]]);
    b[6..8].copy_from_slice(&[raw[7], raw[6]]);
    Some(b)
}

/// A random (version 4) GUID, in GPT byte order. The randomness comes from
/// the standard library's per-process hash keys, the time and a counter.
pub fn random_guid() -> [u8; 16] {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let state = std::collections::hash_map::RandomState::new();
    let mut b = [0u8; 16];
    for (i, half) in b.chunks_exact_mut(8).enumerate() {
        let mut h = state.build_hasher();
        h.write_u64(crate::time::unix_now_ns());
        h.write_u64(n);
        h.write_usize(i);
        h.write_usize(&COUNTER as *const _ as usize);
        half.copy_from_slice(&h.finish().to_le_bytes());
    }
    b[7] = (b[7] & 0x0F) | 0x40; // version 4 (time_hi is little-endian)
    b[8] = (b[8] & 0x3F) | 0x80; // variant 1
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemoryDevice;

    const LINUX: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";

    fn linux(scheme: Scheme) -> TypeCode {
        KNOWN_TYPES[0].code(scheme).unwrap()
    }

    #[test]
    fn guids_round_trip() {
        assert_eq!(guid(&parse_guid(LINUX).unwrap()), LINUX);
        let r = random_guid();
        assert_ne!(r, random_guid());
        assert_eq!(&guid(&r)[14..15], "4");
    }

    #[test]
    fn gpt_create_write_read() {
        let mut dev = MemoryDevice::new("d.img", vec![0u8; 64 << 20]);
        let mut l = Layout::new(Scheme::Gpt, dev.size(), 512).unwrap();
        let f = l.free();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].first, 2048);
        let a = l.add(2048, 2048 + 20479, linux(Scheme::Gpt), "root").unwrap();
        let b = l.add(40960, f[0].last, KNOWN_TYPES[4].code(Scheme::Gpt).unwrap(), "EFI").unwrap();
        assert_eq!((a, b), (1, 2));
        assert!(l.add(30000, 50000, linux(Scheme::Gpt), "").is_err(), "overlap");
        l.set_bootable(1, true).unwrap();
        l.write(&mut dev).unwrap();
        let back = Layout::read(&mut dev).unwrap().unwrap();
        let strip = |l: &Layout| l.slots.iter().map(|s| Slot { new: false, ..s.clone() }).collect::<Vec<_>>();
        assert_eq!(strip(&back), strip(&l));
        assert_eq!(back.disk_guid, l.disk_guid);
        let t = super::super::read_partition_table(&mut dev).unwrap().unwrap();
        assert!(t.notes.is_empty(), "{:?}", t.notes);
        assert_eq!(t.partitions, l.partitions());
    }

    #[test]
    fn mbr_primary_extended_logical() {
        let mut dev = MemoryDevice::new("d.img", vec![0u8; 64 << 20]);
        let mut l = Layout::new(Scheme::Mbr, dev.size(), 512).unwrap();
        l.add(2048, 10239, TypeCode::Mbr(0x0C), "").unwrap();
        let e = l.add(10240, 100_000, TypeCode::Mbr(0x0F), "").unwrap();
        assert_eq!(e, 2);
        let inside: Vec<Free> = l.free().into_iter().filter(|f| f.logical).collect();
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].first, 12288);
        let l2 = l.add(40960, 60000, TypeCode::Mbr(0x82), "").unwrap();
        let l1 = l.add(12288, 20000, TypeCode::Mbr(0x83), "").unwrap();
        // Numbered in disk order.
        assert_eq!((l1, l2), (5, 5));
        assert_eq!(l.get(6).unwrap().first, 40960);
        assert_eq!(l.get(5).unwrap().ebr, Some(10240));
        l.add(110_000u64.div_ceil(2048) * 2048, 120_000, TypeCode::Mbr(0x83), "").unwrap();
        l.add(122_880, 124_000, TypeCode::Mbr(0x83), "").unwrap();
        assert!(l.add(126_976, 130_000, TypeCode::Mbr(0x83), "").is_err(), "a fifth primary");
        l.set_bootable(1, true).unwrap();
        l.write(&mut dev).unwrap();
        let back = Layout::read(&mut dev).unwrap().unwrap();
        let strip = |l: &Layout| l.slots.iter().map(|s| Slot { new: false, ..s.clone() }).collect::<Vec<_>>();
        assert_eq!(strip(&back), strip(&l));
        // Deleting a logical renumbers the next; deleting the extended
        // takes the logical ones with it.
        l.delete(5).unwrap();
        assert_eq!(l.get(5).unwrap().first, 40960);
        assert_eq!(l.get(5).unwrap().ebr, Some(10240));
        l.write(&mut dev).unwrap();
        assert_eq!(strip(&Layout::read(&mut dev).unwrap().unwrap()), strip(&l));
        l.delete(2).unwrap();
        assert!(l.slots.iter().all(|s| s.number <= 4));
    }

    #[test]
    fn mbr_over_gpt_wipes_the_gpt() {
        let mut dev = MemoryDevice::new("d.img", vec![0u8; 16 << 20]);
        let mut g = Layout::new(Scheme::Gpt, dev.size(), 512).unwrap();
        g.add(2048, 4095, linux(Scheme::Gpt), "x").unwrap();
        g.write(&mut dev).unwrap();
        let mut m = Layout::new(Scheme::Mbr, dev.size(), 512).unwrap();
        m.add(2048, 8191, TypeCode::Mbr(0x83), "").unwrap();
        m.write(&mut dev).unwrap();
        let t = super::super::read_partition_table(&mut dev).unwrap().unwrap();
        assert_eq!(t.scheme, Scheme::Mbr);
        assert_eq!(t.partitions.len(), 1);
    }

    #[test]
    fn new_partitions_are_wiped() {
        let mut dev = MemoryDevice::new("d.img", vec![0xAB; 16 << 20]);
        let mut l = Layout::new(Scheme::Gpt, dev.size(), 512).unwrap();
        l.add(2048, 6143, linux(Scheme::Gpt), "").unwrap();
        l.write(&mut dev).unwrap();
        let mut b = vec![0u8; 4096 * 512];
        dev.read_at(2048 * 512, &mut b).unwrap();
        assert!(b[..1 << 20].iter().all(|&x| x == 0));
        assert!(b[1 << 20..].iter().all(|&x| x == 0xAB));
    }
}
