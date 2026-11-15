//! Partition tables: finding the slices of a whole-disk image or device.
//!
//! Two schemes are understood:
//!
//! * **GPT** (UEFI): a protective MBR at sector 0, the header at sector 1
//!   (`"EFI PART"`), an array of 128-byte entries (usually 128 of them)
//!   after it, and a backup header in the last sector with its own copy of
//!   the entries before it. Both the header and the entry array carry a
//!   CRC32; when the primary is damaged the backup is used. Disks with
//!   4096-byte sectors put the header at byte 4096 instead of 512.
//! * **APM** (Apple Partition Map, older Macs and disk images): a driver
//!   descriptor (`ER`) in block 0, then one 512-byte entry (`PM`) per
//!   partition from block 1, the map itself among them. Read only: the
//!   partition editor makes GPT and MBR tables.
//! * **MBR** (DOS): four entries at byte 446 of sector 0, signature `55 AA`.
//!   An extended partition holds a chain of EBRs, each describing one
//!   logical partition (numbered from 5, as Linux does) and the next EBR.
//!
//! A FAT, NTFS or exFAT *volume* also ends its first sector in `55 AA`, so
//! sector 0 is only taken as an MBR when it does not look like a volume
//! boot record and its entries make sense for the device.
//!
//! The result is a list of [`Partition`]s (byte ranges); [`open_partition`]
//! turns one into a [`SliceDevice`] that a driver can mount.

pub mod edit;
#[cfg(all(test, unix))]
mod edit_tests;

use crate::device::{BlockDevice, SliceDevice};
use crate::error::{Error, ErrorKind, Result};
use crate::registry::DriverRegistry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Gpt,
    Mbr,
    /// Apple Partition Map (read only).
    Apm,
}

impl Scheme {
    pub fn label(self) -> &'static str {
        match self {
            Scheme::Gpt => "GPT",
            Scheme::Mbr => "MBR",
            Scheme::Apm => "APM",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// 1-based, as the OS numbers it (`sdb2`, `disk6s2`; MBR logical
    /// partitions start at 5).
    pub number: u32,
    /// Byte offset and length on the disk.
    pub start: u64,
    pub size: u64,
    /// Human readable type ("Linux filesystem", "EFI system", …).
    pub type_name: String,
    /// The raw type: a GPT type GUID or an MBR type byte (`0x83`).
    pub type_id: String,
    /// GPT partition name (label), if set.
    pub name: Option<String>,
    /// GPT unique partition GUID.
    pub guid: Option<String>,
    /// MBR active flag / GPT legacy BIOS bootable attribute.
    pub bootable: bool,
    /// The partition extends beyond the end of the device (a truncated
    /// image): it cannot be opened.
    pub truncated: bool,
}

impl Partition {
    /// "2 · Linux filesystem · “root”".
    pub fn label(&self) -> String {
        let mut s = format!("{} · {}", self.number, self.type_name);
        if let Some(n) = self.name.as_deref().filter(|n| !n.is_empty()) {
            s.push_str(&format!(" · “{n}”"));
        }
        s
    }

    /// Whether a file system could live here at all (not an extended
    /// partition container, a GPT protective entry or empty space).
    pub fn may_hold_a_file_system(&self) -> bool {
        !matches!(
            self.type_id.as_str(),
            "0x05" | "0x0F" | "0x85" | "0xEE" | "Apple_partition_map" | "Apple_Free" | "Apple_Void"
        ) && self.type_name != "BIOS boot"
            && self.type_name != "Microsoft reserved"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionTable {
    pub scheme: Scheme,
    /// Logical sector size the table was found with.
    pub sector_size: u32,
    /// GPT disk GUID / MBR disk signature.
    pub disk_id: Option<String>,
    pub partitions: Vec<Partition>,
    /// Things worth telling the user ("the primary GPT header is damaged;
    /// using the backup").
    pub notes: Vec<String>,
}

impl PartitionTable {
    pub fn get(&self, number: u32) -> Option<&Partition> {
        self.partitions.iter().find(|p| p.number == number)
    }
}

/// Reads the partition table of `dev`, if it has one.
pub fn read_partition_table(dev: &mut dyn BlockDevice) -> Result<Option<PartitionTable>> {
    if dev.size() < 1024 {
        return Ok(None);
    }
    for ss in [512u64, 4096] {
        if let Some(t) = read_gpt(dev, ss)? {
            return Ok(Some(t));
        }
    }
    if let Some(t) = read_apm(dev)? {
        return Ok(Some(t));
    }
    read_mbr(dev)
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

/// An Apple Partition Map: `PM` entries from block 1, each giving its
/// partition's first block and length (blocks of the size the driver
/// descriptor in block 0 says, 512 bytes in disk images).
fn read_apm(dev: &mut dyn BlockDevice) -> Result<Option<PartitionTable>> {
    let mut b0 = [0u8; 512];
    dev.read_at(0, &mut b0)?;
    let described = u16::from_be_bytes([b0[2], b0[3]]) as u64;
    let mut sizes = vec![512u64];
    if &b0[..2] == b"ER" && matches!(described, 1024 | 2048 | 4096) {
        sizes.insert(0, described);
    }
    for bs in sizes {
        let mut e = [0u8; 512];
        if bs + 512 > dev.size() {
            continue;
        }
        dev.read_at(bs, &mut e)?;
        if &e[..2] != b"PM" {
            continue;
        }
        let count = (be32(&e, 4) as u64).min(256);
        let mut partitions = Vec::new();
        let mut notes = Vec::new();
        for n in 1..=count {
            if (n + 1) * bs > dev.size() {
                break;
            }
            dev.read_at(n * bs, &mut e)?;
            if &e[..2] != b"PM" {
                notes.push(format!("partition map entry {n} is damaged"));
                continue;
            }
            let text = |b: &[u8]| {
                let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                String::from_utf8_lossy(&b[..end]).into_owned()
            };
            let (name, kind) = (text(&e[16..48]), text(&e[48..80]));
            let (start, size) = (be32(&e, 8) as u64 * bs, be32(&e, 12) as u64 * bs);
            partitions.push(Partition {
                number: n as u32,
                start,
                size,
                type_name: apm_type_name(&kind).to_owned(),
                type_id: kind,
                name: (!name.is_empty()).then_some(name),
                guid: None,
                bootable: false,
                truncated: start + size > dev.size(),
            });
        }
        return Ok(Some(PartitionTable {
            scheme: Scheme::Apm,
            sector_size: bs as u32,
            disk_id: None,
            partitions,
            notes,
        }));
    }
    Ok(None)
}

fn apm_type_name(t: &str) -> &str {
    match t {
        "Apple_partition_map" => "Partition map",
        "Apple_HFS" => "Apple HFS / HFS+",
        "Apple_HFSX" => "Apple HFSX",
        "Apple_APFS" => "Apple APFS",
        "Apple_Free" => "Free space",
        "Apple_Driver" | "Apple_Driver43" | "Apple_Driver_ATA" | "Apple_Driver43_CD" | "Apple_Driver_ATAPI"
        | "Apple_FWDriver" | "Apple_Driver_IOKit" | "Apple_Patches" => "Apple driver",
        "Apple_Boot" | "Apple_Bootstrap" => "Apple boot",
        "Apple_UNIX_SVR2" => "Unix",
        "DOS_FAT_12" | "DOS_FAT_16" | "DOS_FAT_32" | "Windows_FAT_32" => "FAT",
        "Linux" => "Linux",
        _ => t,
    }
}

/// A device for partition `number` of `dev` (read and written through it).
pub fn open_partition(dev: Box<dyn BlockDevice>, number: u32) -> Result<Box<dyn BlockDevice>> {
    let mut dev = dev;
    let table = read_partition_table(dev.as_mut())?
        .ok_or_else(|| Error::new(ErrorKind::Unsupported, "the device has no partition table"))?;
    let p = table.get(number).ok_or_else(|| Error::not_found(format!("partition {number}")))?.clone();
    slice(dev, &p)
}

/// A slice of `dev` for partition `p`.
pub fn slice(dev: Box<dyn BlockDevice>, p: &Partition) -> Result<Box<dyn BlockDevice>> {
    if p.truncated {
        return Err(Error::new(
            ErrorKind::Corrupted,
            format!("partition {} extends beyond the end of the device", p.number),
        ));
    }
    let name = format!("{} (partition {})", dev.info().name, p.number);
    Ok(Box::new(SliceDevice::new(dev, p.start, p.size, name)?))
}

/// The display name of a volume in partition `p` of image `image`:
/// `disk.img #2` or, with a GPT name, `disk.img #2 (root)`.
pub fn volume_name(image: &str, p: &Partition) -> String {
    match p.name.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => format!("{image} #{} ({n})", p.number),
        None => format!("{image} #{}", p.number),
    }
}

/// Which part of a device to mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PartitionChoice {
    /// The whole device if a driver recognises it, else the first
    /// partition one does.
    #[default]
    Auto,
    /// The whole device, partition table or not.
    Whole,
    Number(u32),
}

/// A partition table with what each partition seems to hold.
#[derive(Debug, Clone)]
pub struct PartitionScan {
    pub table: PartitionTable,
    /// The name of the driver that recognises each partition (same order as
    /// `table.partitions`), if any.
    pub drivers: Vec<Option<String>>,
}

impl PartitionScan {
    /// The partition [`PartitionChoice::Auto`] would pick.
    pub fn default_choice(&self) -> Option<u32> {
        let usable = |i: &usize| {
            let p = &self.table.partitions[*i];
            p.may_hold_a_file_system() && !p.truncated
        };
        let all = 0..self.table.partitions.len();
        all.clone()
            .filter(usable)
            .find(|&i| self.drivers[i].is_some())
            .or_else(|| all.clone().find(usable))
            .map(|i| self.table.partitions[i].number)
    }
}

/// Reads the partition table of `dev` and probes every partition.
pub fn scan(registry: &DriverRegistry, dev: &mut dyn BlockDevice) -> Result<Option<PartitionScan>> {
    let Some(table) = read_partition_table(dev)? else { return Ok(None) };
    let mut drivers = Vec::new();
    for p in &table.partitions {
        let found = if p.may_hold_a_file_system() && !p.truncated {
            let mut part = SliceDevice::new(&mut *dev, p.start, p.size, "probe")?;
            registry.detect(&mut part).first().map(|(d, _)| d.info().name.to_owned())
        } else {
            None
        };
        drivers.push(found);
    }
    Ok(Some(PartitionScan { table, drivers }))
}

/// The partition `choice` means on `dev` (None: the whole device).
pub fn resolve(
    registry: &DriverRegistry,
    dev: &mut dyn BlockDevice,
    choice: PartitionChoice,
) -> Result<Option<Partition>> {
    match choice {
        PartitionChoice::Whole => Ok(None),
        PartitionChoice::Number(n) => {
            let table = read_partition_table(dev)?
                .ok_or_else(|| Error::new(ErrorKind::NotFound, "the device has no partition table"))?;
            let p = table.get(n).ok_or_else(|| Error::not_found(format!("partition {n}")))?;
            Ok(Some(p.clone()))
        }
        PartitionChoice::Auto => {
            if !registry.detect(dev).is_empty() {
                return Ok(None);
            }
            let Some(s) = scan(registry, dev)? else { return Ok(None) };
            Ok(s.default_choice().and_then(|n| s.table.get(n).cloned()))
        }
    }
}

// ----- GPT ----------------------------------------------------------------

const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// A GPT header that passed its checks.
struct GptHeader {
    entries_lba: u64,
    count: u32,
    entry_size: u32,
    entries_crc: u32,
    disk_guid: String,
}

fn read_gpt_header(dev: &mut dyn BlockDevice, ss: u64, lba: u64) -> Result<Option<GptHeader>> {
    let Some(off) = lba.checked_mul(ss).filter(|o| o + ss <= dev.size()) else { return Ok(None) };
    let mut h = vec![0u8; ss as usize];
    dev.read_at(off, &mut h)?;
    if &h[..8] != GPT_SIGNATURE {
        return Ok(None);
    }
    let size = le32(&h, 12) as usize;
    if !(92..=ss as usize).contains(&size) {
        return Ok(None);
    }
    let stored = le32(&h, 16);
    h[16..20].fill(0);
    if crc32(&h[..size]) != stored || le64(&h, 24) != lba {
        return Ok(None);
    }
    let (count, entry_size) = (le32(&h, 80), le32(&h, 84));
    if entry_size < 128 || entry_size % 8 != 0 || count == 0 || count as u64 * entry_size as u64 > 4 << 20 {
        return Ok(None);
    }
    Ok(Some(GptHeader {
        entries_lba: le64(&h, 72),
        count,
        entry_size,
        entries_crc: le32(&h, 88),
        disk_guid: guid(&h[56..72]),
    }))
}

fn read_gpt_entries(dev: &mut dyn BlockDevice, ss: u64, h: &GptHeader) -> Result<Option<Vec<u8>>> {
    let len = h.count as u64 * h.entry_size as u64;
    let Some(off) = h.entries_lba.checked_mul(ss).filter(|o| o + len <= dev.size()) else { return Ok(None) };
    let mut e = vec![0u8; len as usize];
    dev.read_at(off, &mut e)?;
    Ok((crc32(&e) == h.entries_crc).then_some(e))
}

fn read_gpt(dev: &mut dyn BlockDevice, ss: u64) -> Result<Option<PartitionTable>> {
    // Room for the protective MBR, the header and some entries at least.
    if dev.size() / ss < 4 {
        return Ok(None);
    }
    let mut notes = Vec::new();
    let last = dev.size() / ss - 1;
    let primary = read_gpt_header(dev, ss, 1)?;
    let mut found = None;
    if let Some(h) = primary {
        match read_gpt_entries(dev, ss, &h)? {
            Some(e) => found = Some((h, e)),
            None => notes.push("the primary GPT partition entries are damaged; using the backup".to_owned()),
        }
    }
    if found.is_none() {
        if let Some(h) = read_gpt_header(dev, ss, last)? {
            if let Some(e) = read_gpt_entries(dev, ss, &h)? {
                if notes.is_empty() {
                    notes.push("the primary GPT header is damaged; using the backup".to_owned());
                }
                found = Some((h, e));
            }
        }
    } else if read_gpt_header(dev, ss, last)?.is_none() {
        // Common for images that were cut short or grown: worth saying, not fatal.
        notes.push("there is no valid backup GPT header at the end of the device".to_owned());
    }
    let Some((h, entries)) = found else { return Ok(None) };
    let mut partitions = Vec::new();
    for (i, e) in entries.chunks_exact(h.entry_size as usize).enumerate() {
        let type_guid = guid(&e[..16]);
        if e[..16].iter().all(|&b| b == 0) {
            continue;
        }
        let (first, last_lba) = (le64(e, 32), le64(e, 40));
        if last_lba < first {
            notes.push(format!("partition {} ends before it starts: ignored", i + 1));
            continue;
        }
        let (start, size) = (first * ss, (last_lba - first + 1) * ss);
        let name: Vec<u16> = (0..36).map(|k| le16(e, 56 + 2 * k)).take_while(|&c| c != 0).collect();
        let name = String::from_utf16_lossy(&name);
        partitions.push(Partition {
            number: i as u32 + 1,
            start,
            size,
            type_name: gpt_type_name(&type_guid).unwrap_or("Unknown type").to_owned(),
            type_id: type_guid,
            name: (!name.is_empty()).then_some(name),
            guid: Some(guid(&e[16..32])),
            bootable: le64(e, 48) & 0b100 != 0,
            truncated: start + size > dev.size(),
        });
    }
    Ok(Some(PartitionTable {
        scheme: Scheme::Gpt,
        sector_size: ss as u32,
        disk_id: Some(h.disk_guid),
        partitions,
        notes,
    }))
}

/// A GUID in its usual text form (the first three fields are little-endian).
fn guid(b: &[u8]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        le32(b, 0),
        le16(b, 4),
        le16(b, 6),
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

fn gpt_type_name(g: &str) -> Option<&'static str> {
    Some(match g {
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" => "EFI system",
        "21686148-6449-6E6F-744E-656564454649" => "BIOS boot",
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux filesystem",
        "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F" => "Linux swap",
        "E6D6D379-F507-44C2-A23C-238F2A3DF928" => "Linux LVM",
        "A19D880F-05FC-4D3B-A006-743F0F84911E" => "Linux RAID",
        "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709" => "Linux root (x86-64)",
        "44479540-F297-41B2-9AF7-D131D5F0458A" => "Linux root (x86)",
        "B921B045-1DF0-41C3-AF44-4C6F280D3FAE" => "Linux root (ARM64)",
        "69DAD710-2CE4-4E3C-B16C-21A1D49ABED3" => "Linux root (ARM)",
        "933AC7E1-2EB4-4F13-B844-0E14E2AEF915" => "Linux /home",
        "3B8F8425-20E0-4F3B-907F-1A25A76F98E8" => "Linux /srv",
        "BC13C2FF-59E6-4262-A352-B275FD6F7172" => "Linux extended boot",
        "CA7D7CCB-63ED-4C53-861C-1742536059CC" => "Linux LUKS",
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" => "Microsoft basic data",
        "E3C9E316-0B5C-4DB8-817D-F92DF00215AE" => "Microsoft reserved",
        "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC" => "Windows recovery",
        "5808C8AA-7E8F-42E0-85D2-E1E90434CFB3" => "Windows LDM metadata",
        "AF9B60A0-1431-4F62-BC68-3311714A69AD" => "Windows LDM data",
        "48465300-0000-11AA-AA11-00306543ECAC" => "Apple HFS+",
        "7C3457EF-0000-11AA-AA11-00306543ECAC" => "Apple APFS",
        "55465300-0000-11AA-AA11-00306543ECAC" => "Apple UFS",
        "426F6F74-0000-11AA-AA11-00306543ECAC" => "Apple boot",
        "52414944-0000-11AA-AA11-00306543ECAC" => "Apple RAID",
        "53746F72-6167-11AA-AA11-00306543ECAC" => "Apple Core Storage",
        "516E7CB4-6ECF-11D6-8FF8-00022D09712B" => "FreeBSD data",
        "516E7CB6-6ECF-11D6-8FF8-00022D09712B" => "FreeBSD UFS",
        "516E7CBA-6ECF-11D6-8FF8-00022D09712B" => "FreeBSD ZFS",
        "516E7CB5-6ECF-11D6-8FF8-00022D09712B" => "FreeBSD swap",
        "6A898CC3-1DD2-11B2-99A6-080020736631" => "ZFS",
        "FE3A2A5D-4F32-41A7-B725-ACCC3285A309" => "ChromeOS kernel",
        "3CB8E202-3B7E-47DD-8A3C-7FF2A13CFCEC" => "ChromeOS root",
        _ => return None,
    })
}

// ----- MBR ----------------------------------------------------------------

fn mbr_type_name(t: u8) -> &'static str {
    match t {
        0x01 => "FAT12",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x0B | 0x0C => "FAT32",
        0x07 => "NTFS / exFAT",
        0x05 | 0x0F | 0x85 => "Extended",
        0x11 | 0x14 | 0x16 | 0x1B | 0x1C | 0x1E => "Hidden FAT",
        0x17 => "Hidden NTFS",
        0x27 => "Windows recovery",
        0x42 => "Windows dynamic",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0x8E => "Linux LVM",
        0xA5 => "FreeBSD",
        0xA6 => "OpenBSD",
        0xA9 => "NetBSD",
        0xA8 => "Apple UFS",
        0xAB => "Apple boot",
        0xAF => "Apple HFS+",
        0xEE => "GPT protective",
        0xEF => "EFI system",
        0xFD => "Linux RAID",
        _ => "Unknown type",
    }
}

fn is_extended(t: u8) -> bool {
    matches!(t, 0x05 | 0x0F | 0x85)
}

/// Sector 0 of a FAT / NTFS / exFAT volume (which also ends in 55 AA).
fn looks_like_volume_boot_record(s: &[u8]) -> bool {
    let jump = s[0] == 0xEB && s[2] == 0x90 || s[0] == 0xE9;
    if !jump {
        return false;
    }
    let oem = &s[3..11];
    if oem == b"NTFS    " || oem == b"EXFAT   " {
        return true;
    }
    // A FAT BIOS parameter block: a sane sector size and "FAT" in the
    // file system type field (FAT12/16 at 0x36, FAT32 at 0x52).
    let bps = le16(s, 11);
    bps.is_power_of_two() && (512..=4096).contains(&bps) && (&s[0x36..0x39] == b"FAT" || &s[0x52..0x55] == b"FAT")
}

struct MbrEntry {
    bootable: bool,
    kind: u8,
    start: u64,
    sectors: u64,
}

fn mbr_entries(s: &[u8]) -> Option<[Option<MbrEntry>; 4]> {
    let mut out: [Option<MbrEntry>; 4] = Default::default();
    for (i, slot) in out.iter_mut().enumerate() {
        let e = &s[446 + 16 * i..446 + 16 * (i + 1)];
        if e[0] != 0 && e[0] != 0x80 {
            return None; // not a partition entry
        }
        let (kind, start, sectors) = (e[4], le32(e, 8) as u64, le32(e, 12) as u64);
        if kind == 0 || sectors == 0 {
            continue;
        }
        *slot = Some(MbrEntry { bootable: e[0] == 0x80, kind, start, sectors });
    }
    Some(out)
}

fn read_mbr(dev: &mut dyn BlockDevice) -> Result<Option<PartitionTable>> {
    let ss = 512u64;
    let mut s = vec![0u8; 512];
    dev.read_at(0, &mut s)?;
    if s[510..512] != [0x55, 0xAA] || looks_like_volume_boot_record(&s) {
        return Ok(None);
    }
    let Some(entries) = mbr_entries(&s) else { return Ok(None) };
    let sectors = dev.size() / ss;
    let primaries: Vec<(usize, &MbrEntry)> =
        entries.iter().enumerate().filter_map(|(i, e)| Some((i, e.as_ref()?))).collect();
    // Something that is really an MBR: at least one entry, each starting on
    // the disk (an entry may run past a truncated image), none at sector 0.
    if primaries.is_empty() || primaries.iter().any(|(_, e)| e.start == 0 || e.start >= sectors) {
        return Ok(None);
    }
    let mut notes = Vec::new();
    let mut partitions = Vec::new();
    let disk_size = dev.size();
    let push = |partitions: &mut Vec<Partition>, number: u32, e: &MbrEntry, base: u64| {
        let start = (base + e.start) * ss;
        let size = e.sectors * ss;
        partitions.push(Partition {
            number,
            start,
            size,
            type_name: mbr_type_name(e.kind).to_owned(),
            type_id: format!("0x{:02X}", e.kind),
            name: None,
            guid: None,
            bootable: e.bootable,
            truncated: start + size > disk_size,
        });
    };
    let mut extended = None;
    for (i, e) in &primaries {
        push(&mut partitions, *i as u32 + 1, e, 0);
        if is_extended(e.kind) && extended.is_none() {
            extended = Some(e.start);
        }
    }
    // Logical partitions: a chain of EBRs inside the extended partition.
    if let Some(ext_start) = extended {
        let mut ebr = ext_start;
        let mut seen = std::collections::BTreeSet::new();
        let mut number = 5;
        while seen.insert(ebr) && seen.len() <= 128 {
            if ebr >= sectors {
                notes.push("the logical partition chain points beyond the device".to_owned());
                break;
            }
            let mut b = vec![0u8; 512];
            dev.read_at(ebr * ss, &mut b)?;
            let Some(es) = (b[510..512] == [0x55, 0xAA]).then(|| mbr_entries(&b)).flatten() else {
                notes.push(format!("a damaged logical partition record at sector {ebr}"));
                break;
            };
            if let Some(e) = &es[0] {
                push(&mut partitions, number, e, ebr);
                number += 1;
            }
            match &es[1] {
                Some(next) if is_extended(next.kind) => ebr = ext_start + next.start,
                _ => break,
            }
        }
    }
    partitions.sort_by_key(|p| p.number);
    let sig = le32(&s, 440);
    Ok(Some(PartitionTable {
        scheme: Scheme::Mbr,
        sector_size: ss as u32,
        disk_id: (sig != 0).then(|| format!("{sig:08X}")),
        partitions,
        notes,
    }))
}

// ----- CRC32 (IEEE 802.3, as GPT uses) --------------------------------------

/// CRC tables for slicing-by-8: `CRC_TABLES[k][b]` is the CRC of byte
/// `b` followed by `k` zero bytes.
const CRC_TABLES: [[u32; 256]; 8] = {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }
    t
};

pub fn crc32(data: &[u8]) -> u32 {
    crc32_continue(0, data)
}

/// The CRC32 of `prefix + data`, given `crc` = the CRC32 of `prefix` (0 for
/// nothing): for data that arrives in pieces.
pub fn crc32_continue(crc: u32, data: &[u8]) -> u32 {
    let t = &CRC_TABLES;
    let mut c = !crc;
    let mut chunks = data.chunks_exact(8);
    for w in &mut chunks {
        let lo = c ^ u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        c = t[7][(lo & 0xFF) as usize]
            ^ t[6][((lo >> 8) & 0xFF) as usize]
            ^ t[5][((lo >> 16) & 0xFF) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][w[4] as usize]
            ^ t[2][w[5] as usize]
            ^ t[1][w[6] as usize]
            ^ t[0][w[7] as usize];
    }
    for &b in chunks.remainder() {
        c = t[0][((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemoryDevice;

    #[test]
    fn crc32_matches_the_standard() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32_continue(crc32(b"1234"), b"56789"), 0xCBF4_3926);
    }

    /// A GPT written by hand: two partitions, primary + backup.
    fn gpt_disk(size: u64, ss: u64) -> Vec<u8> {
        let mut d = vec![0u8; size as usize];
        let last = size / ss - 1;
        let mut entries = vec![0u8; 128 * 128];
        let parts: [(&str, u64, u64, &str); 2] = [
            ("C12A7328-F81F-11D2-BA4B-00A0C93EC93B", 34, 99, "EFI"),
            ("0FC63DAF-8483-4772-8E79-3D69D8477DE4", 100, last - 34, "root"),
        ];
        for (i, (t, first, end, name)) in parts.iter().enumerate() {
            let e = &mut entries[128 * i..128 * (i + 1)];
            e[..16].copy_from_slice(&guid_bytes(t));
            e[16] = i as u8 + 1;
            e[32..40].copy_from_slice(&first.to_le_bytes());
            e[40..48].copy_from_slice(&end.to_le_bytes());
            for (k, c) in name.encode_utf16().enumerate() {
                e[56 + 2 * k..58 + 2 * k].copy_from_slice(&c.to_le_bytes());
            }
        }
        let ecrc = crc32(&entries);
        let header = |my: u64, alt: u64, entries_lba: u64| {
            let mut h = vec![0u8; 92];
            h[..8].copy_from_slice(GPT_SIGNATURE);
            h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
            h[12..16].copy_from_slice(&92u32.to_le_bytes());
            h[24..32].copy_from_slice(&my.to_le_bytes());
            h[32..40].copy_from_slice(&alt.to_le_bytes());
            h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
            h[80..84].copy_from_slice(&128u32.to_le_bytes());
            h[84..88].copy_from_slice(&128u32.to_le_bytes());
            h[88..92].copy_from_slice(&ecrc.to_le_bytes());
            let c = crc32(&h);
            h[16..20].copy_from_slice(&c.to_le_bytes());
            h
        };
        let es = (128 * 128) / ss as usize;
        d[ss as usize..ss as usize + 92].copy_from_slice(&header(1, last, 2));
        d[2 * ss as usize..2 * ss as usize + entries.len()].copy_from_slice(&entries);
        let backup_entries = last - es as u64;
        d[(backup_entries * ss) as usize..(backup_entries * ss) as usize + entries.len()].copy_from_slice(&entries);
        d[(last * ss) as usize..(last * ss) as usize + 92].copy_from_slice(&header(last, 1, backup_entries));
        // Protective MBR.
        d[446 + 4] = 0xEE;
        d[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
        d[446 + 12..446 + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        d[510] = 0x55;
        d[511] = 0xAA;
        d
    }

    fn guid_bytes(s: &str) -> [u8; 16] {
        let hex: String = s.chars().filter(|c| *c != '-').collect();
        let raw: Vec<u8> = (0..16).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect();
        let mut b = [0u8; 16];
        b[..4].copy_from_slice(&[raw[3], raw[2], raw[1], raw[0]]);
        b[4..6].copy_from_slice(&[raw[5], raw[4]]);
        b[6..8].copy_from_slice(&[raw[7], raw[6]]);
        b[8..].copy_from_slice(&raw[8..]);
        b
    }

    fn table(bytes: Vec<u8>) -> Option<PartitionTable> {
        let mut dev = MemoryDevice::new("disk.img", bytes);
        read_partition_table(&mut dev).unwrap()
    }

    #[test]
    fn reads_a_gpt_and_falls_back_to_its_backup() {
        for ss in [512u64, 4096] {
            let disk = gpt_disk(4 << 20, ss);
            let t = table(disk.clone()).unwrap();
            assert_eq!((t.scheme, t.sector_size), (Scheme::Gpt, ss as u32));
            assert!(t.notes.is_empty(), "{:?}", t.notes);
            assert_eq!(t.partitions.len(), 2);
            let p = &t.partitions[1];
            assert_eq!((p.number, p.start, p.type_name.as_str()), (2, 100 * ss, "Linux filesystem"));
            assert_eq!(p.name.as_deref(), Some("root"));
            assert_eq!(p.label(), "2 · Linux filesystem · “root”");
            assert_eq!(t.partitions[0].type_name, "EFI system");

            // Damage the primary header: the backup takes over.
            let mut bad = disk.clone();
            bad[ss as usize + 40] ^= 1;
            let b = table(bad).unwrap();
            assert_eq!(b.partitions, t.partitions);
            assert_eq!(b.notes, vec!["the primary GPT header is damaged; using the backup"]);

            // Damage the primary entries: their CRC catches it.
            let mut bad = disk.clone();
            bad[2 * ss as usize + 128 + 33] ^= 1;
            let b = table(bad).unwrap();
            assert_eq!(b.partitions, t.partitions);
            assert_eq!(b.notes, vec!["the primary GPT partition entries are damaged; using the backup"]);
        }
    }

    #[test]
    fn tiny_devices_have_no_table() {
        for n in [0usize, 100, 1024, 1280, 4095, 4096, 8192, 16383] {
            assert!(table(vec![0x55; n]).is_none(), "{n} bytes");
        }
    }

    #[test]
    fn opens_a_partition_as_a_slice() {
        let mut disk = gpt_disk(4 << 20, 512);
        disk[100 * 512..100 * 512 + 5].copy_from_slice(b"hello");
        let dev = Box::new(MemoryDevice::new("disk.img", disk));
        let mut p = open_partition(dev, 2).unwrap();
        assert_eq!(p.info().name, "disk.img (partition 2)");
        let mut b = [0u8; 5];
        p.read_at(0, &mut b).unwrap();
        assert_eq!(&b, b"hello");
        let last = (4 << 20) / 512 - 1;
        assert_eq!(p.size(), (last - 34 - 100 + 1) * 512);
    }

    fn mbr_entry(d: &mut [u8], at: usize, i: usize, kind: u8, start: u32, sectors: u32) {
        let e = &mut d[at + 446 + 16 * i..at + 446 + 16 * (i + 1)];
        e[4] = kind;
        e[8..12].copy_from_slice(&start.to_le_bytes());
        e[12..16].copy_from_slice(&sectors.to_le_bytes());
        d[at + 510] = 0x55;
        d[at + 511] = 0xAA;
    }

    #[test]
    fn reads_an_mbr_with_logical_partitions() {
        let mut d = vec![0u8; 8 << 20];
        mbr_entry(&mut d, 0, 0, 0x0C, 2048, 2048);
        mbr_entry(&mut d, 0, 1, 0x05, 4096, 8192);
        d[446] = 0x80;
        // Extended at 4096: logical 5 (+2048, 1024 sectors) -> next EBR at +4096.
        mbr_entry(&mut d, 4096 * 512, 0, 0x83, 2048, 1024);
        mbr_entry(&mut d, 4096 * 512, 1, 0x05, 4096, 4096);
        mbr_entry(&mut d, 8192 * 512, 0, 0x82, 2048, 1024);
        let t = table(d).unwrap();
        assert_eq!(t.scheme, Scheme::Mbr);
        let got: Vec<(u32, u64, &str, bool)> =
            t.partitions.iter().map(|p| (p.number, p.start / 512, p.type_name.as_str(), p.bootable)).collect();
        assert_eq!(
            got,
            vec![
                (1, 2048, "FAT32", true),
                (2, 4096, "Extended", false),
                (5, 6144, "Linux", false),
                (6, 10240, "Linux swap", false)
            ]
        );
        assert!(!t.partitions[1].may_hold_a_file_system());
    }

    #[test]
    fn a_fat_volume_or_a_bare_file_system_is_not_a_partition_table() {
        // FAT32 boot sector: jump, OEM, BPB with 512-byte sectors, "FAT32".
        let mut d = vec![0u8; 1 << 20];
        d[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        d[3..11].copy_from_slice(b"mkfs.fat");
        d[11..13].copy_from_slice(&512u16.to_le_bytes());
        d[0x52..0x57].copy_from_slice(b"FAT32");
        // Boot code that happens to look like entries.
        mbr_entry(&mut d, 0, 0, 0x83, 63, 100);
        assert!(table(d).is_none());
        // ext4-like: nothing in sector 0.
        assert!(table(vec![0u8; 1 << 20]).is_none());
        // Entries pointing outside the device.
        let mut d = vec![0u8; 1 << 20];
        mbr_entry(&mut d, 0, 0, 0x83, 1 << 30, 100);
        assert!(table(d).is_none());
    }
}
