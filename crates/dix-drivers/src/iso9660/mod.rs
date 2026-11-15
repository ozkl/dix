//! ISO 9660 (CD/DVD images, `.iso`), read-only.
//!
//! The volume descriptors start at sector 16: the primary one (the plain
//! ISO 9660 tree: upper-case 8.3-ish names), maybe a Joliet supplementary
//! one (a second tree with UCS-2 names) and an El Torito boot record
//! (pointing at the boot catalog). Directories are runs of records, none
//! crossing a sector; files are contiguous extents (several, for files of
//! 4 GiB and more).
//!
//! Extensions read:
//!
//! * **Rock Ridge** (in each record's System Use area, SUSP): POSIX names
//!   (`NM`), permissions, owners and links (`PX`), symlinks (`SL`), times
//!   (`TF`), device nodes (`PN`), continuation areas (`CE`), relocated deep
//!   directories (`CL`/`RE`) and zisofs-compressed files (`ZF`). When
//!   present it gives the names; otherwise Joliet does; otherwise the plain
//!   ISO names (without `;1`).
//! * **El Torito**: the boot images listed in the boot catalog are shown as
//!   files in a `[BOOT]` folder.
//!
//! Images can also be made and changed (see [`edit`] and [`write`]): the
//! tree is kept in memory and the image written anew, with Rock Ridge and
//! Joliet, when changes are saved. Bootable, hybrid and UDF-bridge images
//! open read-only: a rewrite would lose what makes them boot, or the UDF
//! side.
//!
//! Also reported (in the volume's details): a hybrid MBR/GPT (an image that
//! boots from a USB stick), and a UDF file system next to the ISO 9660 one
//! (not read here: Windows install images keep their content there).

mod edit;
#[cfg(all(test, unix))]
mod tests;
mod udf_write;
mod write;

use dix_core::{
    AccessMode, BlockDevice, Capabilities, DirEntry, Driver, DriverInfo, EntryKind, Error, FileSystem, FsInfo,
    Metadata, MountOptions, ProbeScore, Result, VfsPath,
};
use std::collections::HashMap;
use std::sync::Arc;

const SECTOR: u64 = 2048;

const CAPS: Capabilities =
    Capabilities::READ.union(Capabilities::SYMLINKS).union(Capabilities::PERMISSIONS).union(Capabilities::TIMESTAMPS);

/// With an image that can be rewritten.
const WRITE_CAPS: Capabilities = CAPS
    .union(Capabilities::WRITE)
    .union(Capabilities::CREATE_DIR)
    .union(Capabilities::REMOVE)
    .union(Capabilities::RENAME)
    .union(Capabilities::FORMAT);

const VARIANTS: &[dix_core::FormatVariant] = &[
    dix_core::FormatVariant {
        id: "iso",
        name: "ISO 9660",
        description: "CD/DVD image with Rock Ridge and Joliet names",
        min_size: 0,
        extension: ".iso",
        sized: false,
    },
    dix_core::FormatVariant {
        id: "iso-udf",
        name: "ISO 9660 + UDF",
        description: "CD/DVD/BD image that is also UDF 1.02 (as genisoimage -udf makes): \
                      Windows, macOS and Linux read either side",
        min_size: 0,
        extension: ".iso",
        sized: false,
    },
];

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// A d-characters field, trimmed.
fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim_end_matches([' ', '\0']).to_owned()
}

/// UCS-2 big-endian (Joliet).
fn ucs2(b: &[u8]) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units).trim_end_matches([' ', '\0']).to_owned()
}

/// A directory record's 7-byte time.
fn short_time(b: &[u8]) -> Option<i64> {
    if b.len() < 7 || b[1] == 0 || b[2] == 0 {
        return None;
    }
    let days = dix_core::time::days_from_civil(1900 + b[0] as i64, b[1] as u32, b[2] as u32);
    Some(days * 86400 + b[3] as i64 * 3600 + b[4] as i64 * 60 + b[5] as i64 - b[6] as i8 as i64 * 900)
}

/// A volume descriptor's 17-byte time ("YYYYMMDDHHMMSScc" and an offset).
fn long_time(b: &[u8]) -> Option<i64> {
    if b.len() < 17 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| std::str::from_utf8(&b[r]).ok()?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(4..6)?, n(6..8)?);
    if y == 0 || m == 0 || d == 0 {
        return None;
    }
    let days = dix_core::time::days_from_civil(y, m as u32, d as u32);
    Some(days * 86400 + n(8..10)? * 3600 + n(10..12)? * 60 + n(12..14)? - b[16] as i8 as i64 * 900)
}

/// What the volume descriptors say.
#[derive(Default)]
struct Descriptors {
    pvd: Option<Vec<u8>>,
    /// The Joliet descriptor and its UCS-2 level.
    joliet: Option<(Vec<u8>, u8)>,
    boot_catalog: Option<u32>,
    udf: bool,
}

fn read_descriptors(dev: &mut dyn BlockDevice) -> Result<Descriptors> {
    let mut d = Descriptors::default();
    let mut b = vec![0u8; SECTOR as usize];
    for lba in 16..16 + 64u64 {
        if (lba + 1) * SECTOR > dev.size() {
            break;
        }
        dev.read_at(lba * SECTOR, &mut b)?;
        match &b[1..6] {
            b"CD001" => match b[0] {
                0 if &b[7..30] == b"EL TORITO SPECIFICATION" => d.boot_catalog = Some(le32(&b, 0x47)),
                1 if d.pvd.is_none() => d.pvd = Some(b.clone()),
                2 => {
                    let level = match &b[88..91] {
                        b"%/@" => 1,
                        b"%/C" => 2,
                        b"%/E" => 3,
                        _ => 0,
                    };
                    if level > 0 && d.joliet.is_none() {
                        d.joliet = Some((b.clone(), level));
                    }
                }
                _ => {}
            },
            // UDF's volume recognition sequence follows the ISO descriptors.
            b"BEA01" | b"TEA01" | b"BOOT2" | b"CDW02" => {}
            b"NSR02" | b"NSR03" => d.udf = true,
            _ => break,
        }
    }
    Ok(d)
}

/// What Rock Ridge says about one record.
#[derive(Default, Debug, Clone)]
struct Rr {
    name: Option<String>,
    mode: Option<u32>,
    nlink: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    link: Option<String>,
    mtime: Option<i64>,
    atime: Option<i64>,
    ctime: Option<i64>,
    dev: Option<(u32, u32)>,
    /// A relocated directory's real place (CL).
    child: Option<u32>,
    /// This is a relocated directory: shown where its CL record is.
    relocated: bool,
    /// zisofs: log2 of the block size and the real size.
    zf: Option<(u8, u64)>,
    /// From ER: the extension's identifier (in the root's ".").
    extension: Option<String>,
    /// SP: bytes to skip at the start of each System Use area.
    skip: Option<u8>,
}

#[derive(Debug, Clone)]
struct Node {
    kind: EntryKind,
    /// (first block, bytes) of each extent.
    extents: Vec<(u64, u64)>,
    size: u64,
    rr: Rr,
    mtime: Option<i64>,
    hidden: bool,
    /// The `[BOOT]` folder's files.
    children: Option<Arc<Vec<Entry>>>,
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    node: Arc<Node>,
}

/// Which names a tree has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Names {
    RockRidge,
    Joliet,
    Iso,
}

pub struct IsoDriver;

impl Driver for IsoDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: "iso9660",
            name: "ISO 9660",
            description: "CD/DVD images (.iso): Rock Ridge, Joliet, El Torito boot images, zisofs, UDF bridge; create and change",
            capabilities: WRITE_CAPS,
        }
    }

    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
        if dev.size() < 17 * SECTOR {
            return Ok(0);
        }
        let mut b = [0u8; 8];
        dev.read_at(16 * SECTOR, &mut b)?;
        Ok(if &b[1..6] == b"CD001" && b[6] == 1 { 90 } else { 0 })
    }

    fn mount(&self, dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
        let writable = opts.mode == AccessMode::ReadWrite && !dev.is_read_only();
        let resizable = dev.info().resizable;
        // A bridge image's UDF side: what it holds (it is written anew from
        // the ISO 9660 tree when saved).
        #[cfg(feature = "udf")]
        let (dev, udf_tree) = if writable {
            let shared = dix_core::device::SharedDevice::new(dev);
            let tree = crate::udf::tree(Box::new(shared.handle()));
            (Box::new(shared) as Box<dyn BlockDevice>, tree)
        } else {
            (dev, Ok(None))
        };
        let mut fs = IsoFs::open(dev, &opts.log)?;
        if writable && fs.has_udf {
            #[cfg(feature = "udf")]
            let why = match udf_tree {
                Ok(Some(t)) => fs.udf_differs(&t)?,
                _ => Some("its UDF file system cannot be read".to_owned()),
            };
            #[cfg(not(feature = "udf"))]
            let why = Some("it also holds a UDF file system (this build cannot read UDF)".to_owned());
            if let Some(why) = why {
                fs.rw_blockers.push(why);
            }
        }
        if writable {
            let mut why = fs.rw_blockers.clone();
            if !resizable {
                why.push("it is on a device that cannot grow or shrink".into());
            }
            if !why.is_empty() {
                return Err(Error::new(
                    dix_core::ErrorKind::ReadOnly,
                    format!("this ISO image can only be read: {}. Open it read-only.", why.join("; ")),
                ));
            }
            fs.writable = true;
            fs.start_editing()?;
            opts.log.info("Changes are kept until saved; then the whole image is written anew.");
        }
        Ok(Box::new(fs))
    }

    fn format(&self, dev: &mut dyn BlockDevice, opts: &dix_core::FormatOptions) -> Result<()> {
        let udf = opts.variant.as_deref() == Some("iso-udf");
        format_image(dev, opts.label.as_deref().unwrap_or("CDROM"), true, udf)
    }

    fn format_variants(&self) -> &'static [dix_core::FormatVariant] {
        VARIANTS
    }
}

/// Writes an empty image: ISO 9660, UDF or both.
pub(crate) fn format_image(dev: &mut dyn BlockDevice, label: &str, iso: bool, udf: bool) -> Result<()> {
    let root = write::WNode {
        name: String::new(),
        kind: EntryKind::Directory,
        size: 0,
        mtime: dix_core::time::unix_now(),
        atime: None,
        ctime: None,
        perm: 0o755,
        type_bits: 0,
        uid: 0,
        gid: 0,
        link: None,
        dev: None,
        content_key: None,
        children: Vec::new(),
    };
    let ids = write::VolumeIds {
        label: label.to_owned(),
        publisher: String::new(),
        preparer: "DISK IMAGE EXPLORER".into(),
        application: String::new(),
        iso,
        udf,
    };
    let mut io = edit::VecIo(Vec::new());
    write::write(&[root], &ids, &write::BootSpec::default(), &mut io)?;
    let bytes = io.0;
    if dev.size() != bytes.len() as u64 {
        if !dev.info().resizable {
            return Err(Error::new(dix_core::ErrorKind::NoSpace, "a disc image needs a resizable image file"));
        }
        dev.resize(bytes.len() as u64)?;
    }
    dev.write_at(0, &bytes)?;
    dev.flush()
}

/// Opens a UDF-only image for changing (see [`IsoFs::open_udf`]).
#[cfg(feature = "udf")]
pub(crate) fn open_udf_for_editing(dev: Box<dyn BlockDevice>, log: &dix_core::MountLog) -> Result<Box<dyn FileSystem>> {
    Ok(Box::new(IsoFs::open_udf(dix_core::device::SharedDevice::new(dev), log)?))
}

pub struct IsoFs {
    dev: Box<dyn BlockDevice>,
    block: u64,
    names: Names,
    /// SUSP bytes to skip (from SP).
    skip: usize,
    root: Arc<Node>,
    dirs: HashMap<u64, Arc<Vec<Entry>>>,
    /// The last zisofs block: (file's first block, block number, data).
    zblock: Option<(u64, u64, Arc<Vec<u8>>)>,
    /// The `[BOOT]` folder (El Torito images), shown in the root.
    boot: Option<Arc<Node>>,
    info: FsInfo,
    /// Why a rewrite would lose something (bootable, hybrid, UDF).
    rw_blockers: Vec<String>,
    /// A bridge image (ISO 9660 and UDF).
    has_udf: bool,
    /// A UDF-only image being changed: the UDF driver reading it, and the
    /// device it shares with it.
    udf_src: Option<Box<dyn FileSystem>>,
    #[cfg_attr(not(feature = "udf"), allow(dead_code))]
    shared: Option<dix_core::device::SharedDevice>,
    /// Opened read-write: the tree being edited.
    writable: bool,
    edit: Option<edit::Edit>,
    /// The publisher and application ids (kept on rewrite).
    ids: (String, String),
    /// The boot catalog's entries: (platform, media type, sectors, block).
    boot_entries: Vec<(u8, u8, u16, u64)>,
    /// The partition tables that make it a hybrid image.
    hybrid_kinds: Vec<&'static str>,
}

impl Drop for IsoFs {
    /// Last chance to write changes when dropped without unmounting.
    fn drop(&mut self) {
        if self.edit.as_ref().is_some_and(|e| e.dirty) {
            let _ = self.save();
        }
    }
}

impl IsoFs {
    /// An image with nothing read yet.
    fn empty(dev: Box<dyn BlockDevice>, info: FsInfo) -> IsoFs {
        IsoFs {
            dev,
            block: 2048,
            names: Names::Iso,
            skip: 0,
            root: Arc::new(Node {
                kind: EntryKind::Directory,
                extents: Vec::new(),
                size: 0,
                rr: Rr::default(),
                mtime: None,
                hidden: false,
                children: None,
            }),
            dirs: HashMap::new(),
            zblock: None,
            boot: None,
            info,
            rw_blockers: Vec::new(),
            has_udf: false,
            udf_src: None,
            shared: None,
            writable: false,
            edit: None,
            ids: (String::new(), String::new()),
            boot_entries: Vec::new(),
            hybrid_kinds: Vec::new(),
        }
    }

    fn open(mut dev: Box<dyn BlockDevice>, log: &dix_core::MountLog) -> Result<IsoFs> {
        let d = read_descriptors(dev.as_mut())?;
        let pvd = d.pvd.clone().ok_or_else(|| Error::corrupted("no ISO 9660 primary volume descriptor"))?;
        let block = le16(&pvd, 128) as u64;
        if !matches!(block, 512 | 1024 | 2048) {
            return Err(Error::corrupted(format!("an unusual logical block size ({block})")));
        }
        let root_of = |vd: &[u8]| -> Node {
            let r = &vd[156..190];
            Node {
                kind: EntryKind::Directory,
                extents: vec![(le32(r, 2) as u64 + r[1] as u64, le32(r, 10) as u64)],
                size: 0,
                rr: Rr::default(),
                mtime: short_time(&r[18..25]),
                hidden: false,
                children: None,
            }
        };
        let mut fs = IsoFs::empty(dev, FsInfo::default());
        fs.block = block;
        fs.root = Arc::new(root_of(&pvd));
        fs.ids = (text(&pvd[318..446]), text(&pvd[574..702]));
        // Rock Ridge: the root's "." record starts with SP (or has PX).
        let mut rr_id = None;
        let (root_lba, root_len) = fs.root.extents[0];
        let first = records(&fs.read_extent(root_lba, root_len.min(SECTOR))?).into_iter().next();
        if let Some(first) = first {
            let su = first.su.clone();
            let sp = su.len() >= 7 && &su[0..2] == b"SP" && su[4] == 0xBE && su[5] == 0xEF;
            let mut rr = Rr::default();
            fs.susp(&su, &mut rr, 0)?;
            if sp || rr.mode.is_some() {
                fs.names = Names::RockRidge;
                fs.skip = rr.skip.unwrap_or(0) as usize;
                rr_id = Some(rr.extension.unwrap_or_else(|| "Rock Ridge".into()));
            }
        }
        if fs.names == Names::Iso
            && let Some((svd, _)) = &d.joliet
        {
            fs.names = Names::Joliet;
            fs.root = Arc::new(root_of(svd));
        }
        // El Torito boot images, as a [BOOT] folder.
        let mut boot_desc = Vec::new();
        if let Some(cat) = d.boot_catalog {
            match fs.boot_images(cat as u64) {
                Ok(images) if !images.is_empty() => {
                    boot_desc = images.iter().map(|(_, what, _)| what.clone()).collect();
                    let children: Vec<Entry> =
                        images.into_iter().map(|(name, _, node)| Entry { name, node: Arc::new(node) }).collect();
                    let boot = Node {
                        kind: EntryKind::Directory,
                        extents: Vec::new(),
                        size: 0,
                        rr: Rr::default(),
                        mtime: None,
                        hidden: false,
                        children: Some(Arc::new(children)),
                    };
                    fs.boot = Some(Arc::new(boot));
                }
                Ok(_) => {}
                Err(e) => log.warn(format!("The El Torito boot catalog cannot be read: {}", e.message())),
            }
        }

        // What to tell about the volume.
        let joliet_level = d.joliet.as_ref().map(|(_, l)| *l);
        let mut ext = Vec::new();
        if let Some(id) = &rr_id {
            ext.push(format!("Rock Ridge ({id})"));
        }
        if let Some(l) = joliet_level {
            ext.push(format!("Joliet (UCS-2 level {l})"));
        }
        if !boot_desc.is_empty() {
            ext.push("El Torito".into());
        }
        if d.udf {
            ext.push("UDF".into());
        }
        let label = match &d.joliet {
            Some((svd, _)) if !ucs2(&svd[40..72]).is_empty() => ucs2(&svd[40..72]),
            _ => text(&pvd[40..72]),
        };
        let mut details: Vec<(String, String)> = vec![
            ("Extensions".into(), if ext.is_empty() { "none (plain ISO 9660)".into() } else { ext.join(", ") }),
            (
                "Names from".into(),
                match fs.names {
                    Names::RockRidge => "Rock Ridge (POSIX names, permissions, links)",
                    Names::Joliet => "Joliet (Unicode names)",
                    Names::Iso => "ISO 9660 (short upper-case names)",
                }
                .into(),
            ),
            (
                "Bootable".into(),
                if boot_desc.is_empty() { "No".into() } else { format!("Yes (El Torito): {}", boot_desc.join("; ")) },
            ),
        ];
        let hybrid = hybrid(fs.dev.as_mut())?;
        // Boot images and the MBR are kept when the image is rewritten (see
        // `start_editing`); an Apple partition map is not.
        if hybrid.contains(&"Apple partition map") {
            fs.rw_blockers.push("it has an Apple partition map, which would be lost".into());
        }
        fs.hybrid_kinds = hybrid.clone();
        fs.has_udf = d.udf;
        if !hybrid.is_empty() {
            details.push(("Hybrid image".into(), format!("{} — also boots as a disk (USB stick)", hybrid.join(" + "))));
        }
        if d.udf {
            details.push((
                "UDF".into(),
                "present (the ISO 9660 side is shown; choose the UDF driver for the other)".into(),
            ));
            log.info(
                "This image also holds a UDF file system. If only a notice file shows, the content is in the \
                 UDF part (as in Windows install images): open the image with the UDF driver.",
            );
        }
        for (name, at, len) in [
            ("System", 8, 32),
            ("Volume set", 190, 128),
            ("Publisher", 318, 128),
            ("Data preparer", 446, 128),
            ("Application", 574, 128),
        ] {
            let v = text(&pvd[at..at + len]);
            if !v.is_empty() {
                details.push((name.into(), v));
            }
        }
        for (name, at) in [("Created", 813), ("Modified", 830)] {
            if let Some(t) = long_time(&pvd[at..at + 17]) {
                details.push((name.into(), dix_core::time::format_unix(t)));
            }
        }
        log.info(format!(
            "ISO 9660{}; names from {}",
            if ext.is_empty() { String::new() } else { format!(" with {}", ext.join(", ")) },
            match fs.names {
                Names::RockRidge => "Rock Ridge",
                Names::Joliet => "Joliet",
                Names::Iso => "the ISO 9660 records",
            }
        ));
        fs.info = FsInfo {
            driver_id: "iso9660".into(),
            fs_name: "ISO 9660".into(),
            label: Some(label).filter(|l| !l.is_empty()),
            total_bytes: Some(le32(&pvd, 80) as u64 * block),
            free_bytes: None, // a disc has no free space to speak of
            block_size: Some(block as u32),
            details,
        };
        Ok(fs)
    }

    fn read_extent(&mut self, lba: u64, len: u64) -> Result<Vec<u8>> {
        if len > 64 << 20 {
            return Err(Error::corrupted("an implausibly large directory"));
        }
        let mut b = vec![0u8; len as usize];
        self.dev.read_at(lba * self.block, &mut b)?;
        Ok(b)
    }

    /// Reads System Use entries into `rr` (following continuation areas).
    fn susp(&mut self, su: &[u8], rr: &mut Rr, depth: u32) -> Result<()> {
        let mut at = 0;
        let mut name = String::new();
        let mut name_done = rr.name.is_some();
        let mut sl: Vec<String> = Vec::new();
        let mut sl_glue = false;
        let mut ce = None;
        while at + 4 <= su.len() {
            let (sig, len) = (&su[at..at + 2], su[at + 2] as usize);
            if len < 4 || at + len > su.len() {
                break;
            }
            let e = &su[at..at + len];
            match sig {
                b"SP" if len >= 7 => rr.skip = Some(e[6]),
                b"ST" => break,
                b"CE" if len >= 28 => ce = Some((le32(e, 4) as u64, le32(e, 12) as u64, le32(e, 20) as u64)),
                b"PX" if len >= 36 => {
                    rr.mode = Some(le32(e, 4));
                    rr.nlink = Some(le32(e, 12));
                    rr.uid = Some(le32(e, 20));
                    rr.gid = Some(le32(e, 28));
                }
                b"NM" if len >= 5 && !name_done => {
                    if e[4] & 6 == 0 {
                        name.push_str(&String::from_utf8_lossy(&e[5..]));
                    }
                    if e[4] & 1 == 0 {
                        name_done = true;
                        rr.name = Some(std::mem::take(&mut name)).filter(|n| !n.is_empty());
                    }
                }
                b"SL" if len >= 5 => {
                    let mut i = 5;
                    while i + 2 <= len {
                        let (cf, cl) = (e[i], e[i + 1] as usize);
                        if i + 2 + cl > len {
                            break;
                        }
                        let part = match cf & 0x0E {
                            2 => ".".to_owned(),
                            4 => "..".to_owned(),
                            8 => String::new(), // root: a leading "/"
                            _ => String::from_utf8_lossy(&e[i + 2..i + 2 + cl]).into_owned(),
                        };
                        match sl.last_mut() {
                            Some(last) if sl_glue => last.push_str(&part),
                            _ => sl.push(part),
                        }
                        sl_glue = cf & 1 != 0;
                        i += 2 + cl;
                    }
                    if e[4] & 1 == 0 {
                        let t = sl.join("/");
                        rr.link = Some(if t.is_empty() { "/".into() } else { t });
                    }
                }
                b"TF" if len >= 5 => {
                    let flags = e[4];
                    let size = if flags & 0x80 != 0 { 17 } else { 7 };
                    let mut i = 5;
                    for bit in 0..7 {
                        if flags & (1 << bit) == 0 {
                            continue;
                        }
                        if i + size > len {
                            break;
                        }
                        let t = if size == 17 { long_time(&e[i..i + 17]) } else { short_time(&e[i..i + 7]) };
                        match bit {
                            0 => rr.ctime = rr.ctime.or(t),
                            1 => rr.mtime = t,
                            2 => rr.atime = t,
                            3 => rr.ctime = t,
                            _ => {}
                        }
                        i += size;
                    }
                }
                b"PN" if len >= 20 => {
                    let (high, low) = (le32(e, 4), le32(e, 12));
                    rr.dev = Some(if high != 0 {
                        (high, low)
                    } else {
                        ((low >> 8) & 0xFFF, (low & 0xFF) | ((low >> 12) & 0xFFF00))
                    });
                }
                b"CL" if len >= 12 => rr.child = Some(le32(e, 4)),
                b"RE" => rr.relocated = true,
                b"ZF" if len >= 16 && &e[4..6] == b"pz" => rr.zf = Some((e[7], le32(e, 8) as u64)),
                b"ER" if len >= 8 => {
                    let id_len = e[4] as usize;
                    if 8 + id_len <= len && rr.extension.is_none() {
                        rr.extension = Some(String::from_utf8_lossy(&e[8..8 + id_len]).into_owned());
                    }
                }
                _ => {}
            }
            at += len;
        }
        if let Some((blk, off, len)) = ce
            && depth < 16
            && len > 0
            && len <= 1 << 16
        {
            let mut b = vec![0u8; len as usize];
            self.dev.read_at(blk * self.block + off, &mut b)?;
            self.susp(&b, rr, depth + 1)?;
        }
        Ok(())
    }

    /// The entries of a directory.
    fn list(&mut self, dir: &Node) -> Result<Arc<Vec<Entry>>> {
        if let Some(c) = &dir.children {
            return Ok(c.clone());
        }
        let Some(&(lba, len)) = dir.extents.first() else { return Ok(Arc::new(Vec::new())) };
        if let Some(d) = self.dirs.get(&lba) {
            return Ok(d.clone());
        }
        let bytes = self.read_extent(lba, len)?;
        let mut out: Vec<Entry> = Vec::new();
        // A file of several extents: records with flag 0x80 until the last.
        let mut pending: Option<Node> = None;
        for r in records(&bytes) {
            if r.name == [0] || r.name == [1] {
                continue;
            }
            let mut rr = Rr::default();
            if self.names == Names::RockRidge {
                let su = r.su.get(self.skip..).unwrap_or(&[]).to_vec();
                self.susp(&su, &mut rr, 0)?;
            }
            if rr.relocated {
                continue; // shown where its CL record is
            }
            let name = match self.names {
                Names::RockRidge => rr.name.clone().unwrap_or_else(|| iso_name(&r.name)),
                Names::Joliet => {
                    let n = ucs2(&r.name);
                    let n = n.split(';').next().unwrap_or(&n).to_owned();
                    n.strip_suffix('.').map(str::to_owned).unwrap_or(n)
                }
                Names::Iso => iso_name(&r.name),
            };
            let ext = (r.lba as u64 + r.ear as u64, r.len as u64);
            if let Some(p) = &mut pending {
                p.extents.push(ext);
                if r.flags & 0x80 != 0 {
                    continue;
                }
                let mut node = pending.take().unwrap();
                node.size = node.extents.iter().map(|e| e.1).sum();
                out.push(Entry { name, node: Arc::new(node) });
                continue;
            }
            let mut kind = if r.flags & 2 != 0 { EntryKind::Directory } else { EntryKind::File };
            let mut extents = vec![ext];
            if let Some(child) = rr.child {
                // A relocated directory: its "." record says how long it is.
                kind = EntryKind::Directory;
                let first = self.read_extent(child as u64, SECTOR.min(self.block * 64))?;
                let len = records(&first).first().map_or(SECTOR, |r| r.len as u64);
                extents = vec![(child as u64, len)];
            }
            if let Some(mode) = rr.mode {
                match mode & 0o170000 {
                    0o120000 => kind = EntryKind::Symlink,
                    0o040000 => kind = EntryKind::Directory,
                    0o100000 | 0 => {}
                    _ if kind == EntryKind::File => kind = EntryKind::Other,
                    _ => {}
                }
            }
            if rr.link.is_some() && kind != EntryKind::Directory {
                kind = EntryKind::Symlink;
            }
            let size = match kind {
                EntryKind::File => rr.zf.map_or(ext.1, |(_, s)| s),
                EntryKind::Symlink => rr.link.as_ref().map_or(0, |l| l.len() as u64),
                _ => 0,
            };
            let node =
                Node { kind, extents, size, mtime: rr.mtime.or(r.time), hidden: r.flags & 1 != 0, rr, children: None };
            if r.flags & 0x80 != 0 && kind == EntryKind::File {
                pending = Some(node);
                continue;
            }
            out.push(Entry { name, node: Arc::new(node) });
        }
        if dir.extents == self.root.extents
            && let Some(boot) = &self.boot
            && !out.iter().any(|e| e.name == "[BOOT]")
        {
            out.push(Entry { name: "[BOOT]".into(), node: boot.clone() });
        }
        let out = Arc::new(out);
        if self.dirs.len() > 1024 {
            self.dirs.clear();
        }
        self.dirs.insert(lba, out.clone());
        Ok(out)
    }

    fn lookup(&mut self, path: &VfsPath) -> Result<Arc<Node>> {
        let mut cur = self.root.clone();
        for part in path.components() {
            if cur.kind != EntryKind::Directory {
                return Err(Error::not_a_directory(path));
            }
            let list = self.list(&cur)?;
            let found = list
                .iter()
                .find(|e| e.name == part)
                .or_else(|| list.iter().find(|e| e.name.eq_ignore_ascii_case(part)))
                .ok_or_else(|| Error::not_found(path))?;
            cur = found.node.clone();
        }
        Ok(cur)
    }

    /// The bytes a file's extents hold, from `off`.
    fn read_raw(&mut self, node: &Node, mut off: u64, buf: &mut [u8]) -> Result<usize> {
        let mut done = 0;
        let mut base = 0u64;
        for &(lba, len) in &node.extents {
            if done == buf.len() {
                break;
            }
            if off < base + len {
                let from = off - base;
                let n = (len - from).min((buf.len() - done) as u64) as usize;
                self.dev.read_at(lba * self.block + from, &mut buf[done..done + n])?;
                done += n;
                off += n as u64;
            }
            base += len;
        }
        Ok(done)
    }

    /// One zisofs block, decompressed.
    fn zblock(&mut self, node: &Node, log2: u8, index: u64) -> Result<Arc<Vec<u8>>> {
        let key = node.extents.first().map_or(0, |e| e.0);
        if let Some((k, i, b)) = &self.zblock
            && *k == key
            && *i == index
        {
            return Ok(b.clone());
        }
        let bad = || Error::corrupted("a damaged zisofs file");
        let mut head = [0u8; 16];
        self.read_raw(node, 0, &mut head)?;
        if head[..8] != [0x37, 0xE4, 0x53, 0x96, 0xC9, 0xDB, 0xD6, 0x07] {
            return Err(bad());
        }
        let table = head[12] as u64 * 4;
        let mut ptr = [0u8; 8];
        if self.read_raw(node, table + 4 * index, &mut ptr)? != 8 {
            return Err(bad());
        }
        let (start, end) = (le32(&ptr, 0) as u64, le32(&ptr, 4) as u64);
        let bsize = 1usize << log2.clamp(15, 17);
        let data = if end <= start {
            vec![0u8; bsize]
        } else {
            if end - start > 2 * bsize as u64 {
                return Err(bad());
            }
            let mut packed = vec![0u8; (end - start) as usize];
            if self.read_raw(node, start, &mut packed)? != packed.len() {
                return Err(bad());
            }
            inflate_zlib(&packed, bsize)?
        };
        let data = Arc::new(data);
        self.zblock = Some((key, index, data.clone()));
        Ok(data)
    }

    fn metadata_of(&self, node: &Node) -> Metadata {
        let mut m = Metadata::new(node.kind, node.size);
        m.modified = node.mtime;
        m.accessed = node.rr.atime;
        m.created = node.rr.ctime;
        m.mode = node.rr.mode;
        m.uid = node.rr.uid;
        m.gid = node.rr.gid;
        m.inode = node.extents.first().map(|e| e.0);
        m.symlink_target = node.rr.link.clone();
        m.hidden = node.hidden;
        m
    }
}

/// `inflate` a zlib stream of at most `max` bytes.
fn inflate_zlib(data: &[u8], max: usize) -> Result<Vec<u8>> {
    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, max)
        .map_err(|_| Error::corrupted("a damaged zisofs block"))
}

/// "1.4 MB", "2 KiB".
fn size_text(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / 1e6),
        n if n % 1024 == 0 => format!("{} KiB", n / 1024),
        n => format!("{n} bytes"),
    }
}

/// A plain ISO name: without the version (";1") and a lone trailing dot.
fn iso_name(raw: &[u8]) -> String {
    let n = String::from_utf8_lossy(raw).into_owned();
    let n = n.split(';').next().unwrap_or(&n).to_owned();
    match n.strip_suffix('.') {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => n,
    }
}

/// A directory record.
struct Record {
    lba: u32,
    len: u32,
    /// Extended attribute record length (blocks before the data).
    ear: u8,
    flags: u8,
    time: Option<i64>,
    name: Vec<u8>,
    /// The System Use area.
    su: Vec<u8>,
}

/// The records of a directory's bytes (none crosses a sector boundary;
/// a zero length byte means: go on in the next sector).
fn records(b: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < b.len() {
        let len = b[at] as usize;
        if len == 0 {
            at = (at / SECTOR as usize + 1) * SECTOR as usize;
            continue;
        }
        if len < 34 || at + len > b.len() {
            break;
        }
        let r = &b[at..at + len];
        let nlen = r[32] as usize;
        if 33 + nlen > len {
            break;
        }
        let su_at = 33 + nlen + (nlen + 1) % 2;
        out.push(Record {
            lba: le32(r, 2),
            len: le32(r, 10),
            ear: r[1],
            flags: r[25],
            time: short_time(&r[18..25]),
            name: r[33..33 + nlen].to_vec(),
            su: r.get(su_at..).unwrap_or(&[]).to_vec(),
        });
        at += len;
    }
    out
}

/// "MBR", "GPT", "Apple": partition tables that make the image bootable
/// as a disk too.
fn hybrid(dev: &mut dyn BlockDevice) -> Result<Vec<&'static str>> {
    let mut b = [0u8; 1024];
    dev.read_at(0, &mut b)?;
    let mut out = Vec::new();
    if b[510..512] == [0x55, 0xAA] && (0..4).any(|i| b[446 + 16 * i + 4] != 0) {
        out.push("MBR");
    }
    if &b[512..520] == b"EFI PART" {
        out.push("GPT");
    }
    if &b[0..2] == b"ER" {
        out.push("Apple partition map");
    }
    Ok(out)
}

impl IsoFs {
    /// The boot catalog's images: (file name, description, node).
    fn boot_images(&mut self, cat: u64) -> Result<Vec<(String, String, Node)>> {
        let b = self.read_extent(cat, SECTOR)?;
        if b[0] != 1 || b[30..32] != [0x55, 0xAA] {
            return Err(Error::corrupted("no validation entry"));
        }
        let sum = b[..32].chunks_exact(2).fold(0u16, |s, c| s.wrapping_add(u16::from_le_bytes([c[0], c[1]])));
        if sum != 0 {
            return Err(Error::corrupted("a bad validation entry checksum"));
        }
        let mut out = Vec::new();
        let mut platform = b[1];
        let entry = |e: &[u8], platform: u8, out: &mut Vec<(String, String, Node)>, fs: &mut IsoFs| -> Result<()> {
            let (bootable, media, count, rba) = (e[0] == 0x88, e[1] & 0x0F, le16(e, 6) as u64, le32(e, 8) as u64);
            fs.boot_entries.push((platform, media, count as u16, rba));
            let plat = match platform {
                0 => "BIOS".to_owned(),
                1 => "PowerPC".to_owned(),
                2 => "Mac".to_owned(),
                0xEF => "UEFI".to_owned(),
                p => format!("platform 0x{p:02X}"),
            };
            let (emul, size) = match media {
                1 => ("1.2 MB floppy", 1_228_800),
                2 => ("1.44 MB floppy", 1_474_560),
                3 => ("2.88 MB floppy", 2_949_120),
                4 => {
                    // A hard disk image: as long as its first partition reaches.
                    let mut mbr = [0u8; 512];
                    fs.dev.read_at(rba * fs.block, &mut mbr)?;
                    let end = (0..4)
                        .map(|i| le32(&mbr, 446 + 16 * i + 8) as u64 + le32(&mbr, 446 + 16 * i + 12) as u64)
                        .max();
                    ("hard disk", end.unwrap_or(1).max(1) * 512)
                }
                _ => ("no emulation", count.max(4) * 512),
            };
            let n = out.len() + 1;
            let short = match media {
                0 => "NoEmul",
                4 => "HardDisk",
                _ => "Floppy",
            };
            let mut what = format!("{plat} ({emul}, {})", size_text(size));
            if !bootable {
                what.push_str(", not marked bootable");
            }
            let size = size.min(fs.dev.size().saturating_sub(rba * fs.block));
            out.push((
                format!("{n}-{plat}-{short}.img").replace(' ', "_"),
                what,
                Node {
                    kind: EntryKind::File,
                    extents: vec![(rba, size)],
                    size,
                    rr: Rr::default(),
                    mtime: None,
                    hidden: false,
                    children: None,
                },
            ));
            Ok(())
        };
        entry(&b[32..64], platform, &mut out, self)?;
        let mut at = 64;
        while at + 32 <= b.len() {
            let h = &b[at..at + 32];
            match h[0] {
                0x90 | 0x91 => {
                    platform = h[1];
                    let n = le16(h, 2) as usize;
                    at += 32;
                    for _ in 0..n {
                        if at + 32 > b.len() {
                            break;
                        }
                        let e = b[at..at + 32].to_vec();
                        if e[0] == 0x88 || e[0] == 0 {
                            entry(&e, platform, &mut out, self)?;
                        }
                        at += 32;
                        // Extension entries belong to the one before.
                        while at + 32 <= b.len() && b[at] == 0x44 {
                            at += 32;
                        }
                    }
                    if h[0] == 0x91 {
                        break;
                    }
                }
                _ => break,
            }
        }
        Ok(out)
    }
}

impl FileSystem for IsoFs {
    fn info(&self) -> FsInfo {
        self.info.clone()
    }

    fn capabilities(&self) -> Capabilities {
        if self.writable { WRITE_CAPS.difference(Capabilities::FORMAT) } else { CAPS }
    }

    fn access_mode(&self) -> AccessMode {
        if self.writable { AccessMode::ReadWrite } else { AccessMode::ReadOnly }
    }

    fn read_dir(&mut self, path: &VfsPath) -> Result<Vec<DirEntry>> {
        if self.edit.is_some() {
            return self.e_read_dir(path);
        }
        let dir = self.lookup(path)?;
        if dir.kind != EntryKind::Directory {
            return Err(Error::not_a_directory(path));
        }
        let list = self.list(&dir)?;
        Ok(list.iter().map(|e| DirEntry { name: e.name.clone(), metadata: self.metadata_of(&e.node) }).collect())
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        if self.edit.is_some() {
            return self.e_metadata(path);
        }
        let node = self.lookup(path)?;
        Ok(self.metadata_of(&node))
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if self.edit.is_some() {
            return self.e_read(path, offset, buf);
        }
        let node = self.lookup(path)?;
        match node.kind {
            EntryKind::Directory => return Err(Error::is_a_directory(path)),
            EntryKind::File => {}
            _ => return Ok(0),
        }
        self.read_node(&node, offset, buf)
    }

    fn write_file(&mut self, path: &VfsPath, data: &[u8]) -> Result<()> {
        self.writable()?;
        self.e_write_file(path, data)
    }

    fn write_at(&mut self, path: &VfsPath, offset: u64, data: &[u8]) -> Result<()> {
        self.writable()?;
        self.e_write_at(path, offset, data)
    }

    fn create_dir(&mut self, path: &VfsPath) -> Result<()> {
        self.writable()?;
        self.e_create_dir(path)
    }

    fn remove_file(&mut self, path: &VfsPath) -> Result<()> {
        self.writable()?;
        self.e_remove(path, false)
    }

    fn remove_dir(&mut self, path: &VfsPath) -> Result<()> {
        self.writable()?;
        self.e_remove(path, true)
    }

    fn rename(&mut self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        self.writable()?;
        self.e_rename(from, to)
    }

    fn sync(&mut self) -> Result<()> {
        if self.edit.as_ref().is_some_and(|e| e.dirty) {
            self.save()?;
        }
        Ok(())
    }

    fn release_cache(&mut self, _path: &VfsPath) {
        self.zblock = None;
    }

    fn settings(&self) -> Vec<dix_core::Setting> {
        if self.edit.is_some() { self.e_settings() } else { Vec::new() }
    }

    fn apply_settings(&mut self, values: &[(String, String)]) -> Result<()> {
        self.writable()?;
        self.e_apply_settings(values)
    }
}

impl IsoFs {
    fn writable(&self) -> Result<()> {
        if self.edit.is_some() { Ok(()) } else { Err(Error::read_only()) }
    }

    /// Reads a file of the image as opened (zisofs ones decompressed).
    fn read_node(&mut self, node: &Node, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= node.size {
            return Ok(0);
        }
        let want = buf.len().min((node.size - offset) as usize);
        let Some((log2, _)) = node.rr.zf else { return self.read_raw(node, offset, &mut buf[..want]) };
        let bsize = 1u64 << log2.clamp(15, 17);
        let mut done = 0;
        while done < want {
            let pos = offset + done as u64;
            let block = self.zblock(node, log2, pos / bsize)?;
            let from = (pos % bsize) as usize;
            if from >= block.len() {
                break;
            }
            let n = (block.len() - from).min(want - done);
            buf[done..done + n].copy_from_slice(&block[from..from + n]);
            done += n;
        }
        Ok(done)
    }
}
