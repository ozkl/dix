//! **Archives** as volumes: zip, tar, and tar compressed with gzip, xz,
//! bzip2 or zstd; also a single file compressed with one of those
//! (`notes.txt.gz`, `disk.img.xz`), shown as a volume holding that file.
//!
//! | format                     | read | write |
//! |----------------------------|------|-------|
//! | zip (stored, deflate)      | ✓    | ✓     |
//! | tar                        | ✓    | ✓     |
//! | tar.gz / .tgz              | ✓    | ✓     |
//! | tar.zst, tar.bz2           | ✓    | ✓     |
//! | tar.xz, cpio.xz            | ✓    | ✓     |
//! | single .gz/.xz/.bz2/.zst   | ✓    | –     |
//! | cpio (newc, crc, odc, binary), also compressed | ✓ | newc, like tar |
//! | zip methods Shrink, Reduce, Implode, Deflate64, bzip2, LZMA, zstd, xz | ✓ | as deflate |
//! | 7z, RAR, ARJ                | ✓    | –     |
//! | tar/cpio/single .lz, .lzma, .lz4, .Z, pbzx | ✓ | – |
//! | ar, .deb, .rpm              | ✓    | –     |
//! | .cab (stored, MSZIP, LZX)   | ✓    | –     |
//! | XAR (.pkg, .xip)            | ✓    | –     |
//! | self-extracting .exe/ELF (zip, 7z, RAR, cab inside) | ✓ | – |
//!
//! zip and plain tar are read in place: the member list is read on mount,
//! file data when a file is read. A compressed archive is unpacked into
//! memory on mount.
//!
//! Changes are kept in memory and written as a whole new archive on
//! [`sync`](FileSystem::sync) (the end of each copy/import, eject, quit):
//! unchanged zip members are copied as they are, the rest is compressed.
//! The image must be resizable (an image file or an in-memory image), and a
//! rewrite interrupted half-way leaves a damaged archive: archives have no
//! journal.

mod ar;
mod arj;
mod cab;
mod codec;
mod cpio;
#[cfg(all(feature = "archive-aes", any(feature = "archive-7z", feature = "archive-rar")))]
mod crypto;
mod lzx;
#[cfg(test)]
mod lzx_tests;
mod pkzip;
#[cfg(any(feature = "archive-7z", feature = "archive-rar"))]
mod ppmd;
#[cfg(feature = "archive-rar")]
mod rar;
mod streams;
mod tar;
mod xar;
mod zipcrypt;
// The tool cross-checks need every codec.
#[cfg(feature = "archive-7z")]
mod sevenz;
#[cfg(all(
    test,
    not(target_arch = "wasm32"),
    feature = "archive-zstd",
    feature = "archive-bzip2",
    feature = "archive-xz"
))]
mod tests;
mod zip;

/// The parsed 7z structure (folders), kept for reading file data.
#[cfg(feature = "archive-7z")]
type SevenZArchive = sevenz::Archive;
#[cfg(not(feature = "archive-7z"))]
#[derive(Debug, Clone, Default)]
struct SevenZArchive {
    _none: (),
}

/// The parsed RAR member list (and its decompression stream).
#[cfg(feature = "archive-rar")]
type RarArchive = rar::Archive;
#[cfg(not(feature = "archive-rar"))]
#[derive(Debug, Clone, Default)]
struct RarArchive {
    _none: (),
}

/// The RAR signatures' common start (known even when RAR support is left out).
const RAR_SIGNATURE: &[u8] = b"Rar!\x1a\x07";

/// The LZMA2 level 7z archives are written with unless set otherwise: fast
/// (about 20 MB/s; level 6, 7-Zip's normal, is about 1 MB/s with the
/// encoder here, and the archive is written anew after each change).
const SEVENZ_LEVEL: u32 = 3;

/// The 7z signature (known even when 7z support is left out, to say so).
const SEVENZ_SIGNATURE: [u8; 6] = [b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C];

pub use codec::Compression;

use dix_core::{
    AccessMode, BlockDevice, Capabilities, DirEntry, Driver, DriverInfo, EntryKind, Error, ErrorKind, FileSystem,
    FormatOptions, FormatVariant, FsInfo, Metadata, MountLog, MountOptions, ProbeScore, Result, VfsPath,
};
use std::collections::BTreeMap;
use std::sync::Arc;

const CAPS: Capabilities = Capabilities::READ
    .union(Capabilities::WRITE)
    .union(Capabilities::CREATE_DIR)
    .union(Capabilities::REMOVE)
    .union(Capabilities::RENAME)
    .union(Capabilities::TIMESTAMPS)
    .union(Capabilities::FORMAT);

const fn variant(id: &'static str, description: &'static str, extension: &'static str) -> FormatVariant {
    FormatVariant { id, name: id, description, min_size: 0, extension, sized: false }
}

const VARIANTS: &[FormatVariant] = &[
    variant("zip", "zip archive (deflate)", ".zip"),
    variant("tar", "uncompressed tar archive", ".tar"),
    variant("tar.gz", "tar compressed with gzip", ".tar.gz"),
    variant("cpio", "cpio archive (newc, as Linux initramfs)", ".cpio"),
    variant("cpio.gz", "cpio compressed with gzip", ".cpio.gz"),
    #[cfg(feature = "archive-xz")]
    variant("tar.xz", "tar compressed with xz", ".tar.xz"),
    #[cfg(feature = "archive-xz")]
    variant("cpio.xz", "cpio compressed with xz", ".cpio.xz"),
    #[cfg(feature = "archive-7z")]
    variant("7z", "7z archive (LZMA2, solid)", ".7z"),
    #[cfg(feature = "archive-zstd")]
    variant("tar.zst", "tar compressed with zstd", ".tar.zst"),
    #[cfg(feature = "archive-bzip2")]
    variant("tar.bz2", "tar compressed with bzip2", ".tar.bz2"),
    #[cfg(feature = "archive-zstd")]
    variant("cpio.zst", "cpio compressed with zstd", ".cpio.zst"),
];

/// What kind of archive a volume is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Zip,
    Tar(Compression),
    Cpio(Compression),
    /// One compressed file (not a tar inside).
    Single(Compression),
    SevenZ,
    /// RAR 5 (and RAR 4, stored files), read-only.
    Rar,
    /// ARJ, read-only.
    Arj,
    /// A Unix ar archive (static library), read-only.
    Ar,
    /// A Debian package (ar of control and data tars), read-only.
    Deb,
    /// An RPM package (headers and a cpio payload), read-only.
    Rpm,
    /// A Microsoft Cabinet, read-only.
    Cab,
    /// A XAR archive (macOS .pkg, .xip), read-only.
    Xar,
}

impl Kind {
    pub fn name(self) -> String {
        match self {
            Kind::Zip => "zip".into(),
            Kind::Tar(Compression::None) => "tar".into(),
            Kind::Tar(c) => format!("tar{}", c.extension()),
            Kind::Cpio(Compression::None) => "cpio".into(),
            Kind::Cpio(c) => format!("cpio{}", c.extension()),
            Kind::Single(c) => c.name().into(),
            Kind::SevenZ => "7z".into(),
            Kind::Rar => "rar".into(),
            Kind::Arj => "arj".into(),
            Kind::Ar => "ar".into(),
            Kind::Deb => "deb".into(),
            Kind::Rpm => "rpm".into(),
            Kind::Cab => "cab".into(),
            Kind::Xar => "xar".into(),
        }
    }

    /// Why this kind cannot be changed, if it cannot.
    fn read_only_reason(self) -> Option<String> {
        match self {
            Kind::Zip => None,
            Kind::Tar(c) | Kind::Cpio(c) if c.can_write() => None,
            Kind::Tar(c) | Kind::Cpio(c) => {
                Some(format!("{} archives can only be read: there is no {} compressor here", self.name(), c.name()))
            }
            Kind::Single(_) => Some(format!("a single {}-compressed file can only be read", self.name())),
            #[cfg(feature = "archive-7z")]
            Kind::SevenZ => None,
            #[cfg(not(feature = "archive-7z"))]
            Kind::SevenZ => Some("7z support is not built in".into()),
            Kind::Rar => Some("RAR archives can only be read".into()),
            Kind::Arj => Some("ARJ archives can only be read".into()),
            Kind::Ar | Kind::Deb | Kind::Rpm | Kind::Cab | Kind::Xar => {
                Some(format!("{} archives can only be read", self.name()))
            }
        }
    }
}

/// Where a file's bytes are.
#[derive(Debug, Clone)]
pub(crate) enum Data {
    Empty,
    /// In the archive's raw bytes (the device, or the unpacked buffer).
    Range {
        offset: u64,
        len: u64,
    },
    /// A zip member.
    Zip {
        header: u64,
        csize: u64,
        size: u64,
        method: u16,
        crc: u32,
        encrypted: bool,
        /// The general purpose flags (Implode and LZMA read some).
        flags: u16,
        /// The DOS time and date in its headers (ZipCrypto checks the
        /// password against the time; an encrypted member keeps it).
        dos: (u16, u16),
        /// WinZip AES encryption (method 99).
        aes: Option<zip::ZipAes>,
    },
    /// Written since the archive was opened.
    Mem(Arc<Vec<u8>>),
    /// A RAR member that is compressed (or cannot be read): its index in
    /// the member list.
    #[cfg_attr(not(feature = "archive-rar"), allow(dead_code))]
    Rar {
        index: usize,
    },
    /// An ARJ member that is compressed, split over volumes or cannot be
    /// read: its index in the member list.
    Arj {
        index: usize,
    },
    /// Bytes in the raw data, compressed on their own (a XAR member).
    Packed {
        offset: u64,
        len: u64,
        compression: Compression,
    },
    /// A Cabinet file: part of a folder's unpacked bytes.
    Cab {
        folder: usize,
        offset: u64,
        len: u64,
    },
    /// A 7z file: part of a folder's unpacked output.
    #[cfg_attr(not(feature = "archive-7z"), allow(dead_code))]
    SevenZ {
        folder: usize,
        offset: u64,
        len: u64,
        crc: Option<u32>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub kind: EntryKind,
    pub data: Data,
    pub size: u64,
    pub mtime: Option<i64>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub link: Option<String>,
    /// A device node, FIFO or socket (cpio): kept to be written back.
    pub dev: Option<DevNode>,
}

/// What an [`EntryKind::Other`] member is: its file type bits (`S_IFCHR`,
/// …) and device numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DevNode {
    pub type_bits: u32,
    pub major: u32,
    pub minor: u32,
}

impl Node {
    pub fn new(kind: EntryKind) -> Self {
        Self { kind, data: Data::Empty, size: 0, mtime: None, mode: None, uid: None, gid: None, link: None, dev: None }
    }

    fn meta(&self) -> Metadata {
        let mut m = Metadata::new(self.kind, self.size);
        m.modified = self.mtime;
        m.mode = self.mode.map(|p| {
            let t = match self.kind {
                EntryKind::Directory => 0o040000,
                EntryKind::Symlink => 0o120000,
                _ => self.dev.map_or(0o100000, |d| d.type_bits),
            };
            t | p
        });
        m.uid = self.uid;
        m.gid = self.gid;
        m.symlink_target = self.link.clone();
        m
    }
}

/// The members, by path ("/a/b"); parent directories always exist.
#[derive(Debug, Clone)]
pub(crate) struct Tree {
    nodes: BTreeMap<String, Node>,
}

impl Tree {
    pub fn new() -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert("/".to_owned(), Node::new(EntryKind::Directory));
        Self { nodes }
    }

    fn parent(path: &str) -> Option<&str> {
        let i = path.rfind('/')?;
        Some(if i == 0 { "/" } else { &path[..i] })
    }

    pub fn get(&self, path: &str) -> Option<&Node> {
        self.nodes.get(path)
    }

    /// Adds (or replaces) a member, creating missing parent directories.
    pub fn insert(&mut self, path: &str, node: Node) {
        if path == "/" {
            return self.update_dir(path, node);
        }
        let mut missing = Vec::new();
        let mut p = Self::parent(path);
        while let Some(dir) = p {
            match self.nodes.get(dir) {
                Some(n) if n.kind == EntryKind::Directory => break,
                _ => missing.push(dir.to_owned()),
            }
            p = Self::parent(dir);
        }
        for dir in missing {
            self.nodes.insert(dir, Node::new(EntryKind::Directory));
        }
        self.nodes.insert(path.to_owned(), node);
    }

    /// A directory member: keeps the children of an existing directory.
    pub fn update_dir(&mut self, path: &str, node: Node) {
        match self.nodes.get_mut(path) {
            Some(n) if n.kind == EntryKind::Directory => {
                n.mtime = node.mtime.or(n.mtime);
                n.mode = node.mode.or(n.mode);
                n.uid = node.uid.or(n.uid);
                n.gid = node.gid.or(n.gid);
            }
            _ => self.insert(path, node),
        }
    }

    /// Members in order, parents before their children (without the root).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Node)> {
        self.nodes.iter().filter(|(p, _)| p.as_str() != "/").map(|(p, n)| (p.as_str(), n))
    }

    /// The direct children of `dir`.
    fn children<'a>(&'a self, dir: &'a str) -> impl Iterator<Item = (&'a str, &'a Node)> + 'a {
        let prefix = if dir == "/" { "/".to_owned() } else { format!("{dir}/") };
        let len = prefix.len();
        self.nodes
            .range(prefix.clone()..)
            .take_while(move |(p, _)| p.starts_with(&prefix))
            .filter(move |(p, _)| p.len() > len && !p[len..].contains('/'))
            .map(move |(p, n)| (&p[len..], n))
    }

    /// `dir` and everything below it.
    fn subtree(&self, dir: &str) -> Vec<String> {
        let prefix = format!("{dir}/");
        let mut out = vec![dir.to_owned()];
        out.extend(
            self.nodes.range(prefix.clone()..).take_while(|(p, _)| p.starts_with(&prefix)).map(|(p, _)| p.clone()),
        );
        out
    }

    pub fn counts(&self) -> (usize, usize) {
        let dirs = self.iter().filter(|(_, n)| n.kind == EntryKind::Directory).count();
        (self.nodes.len() - 1 - dirs, dirs)
    }
}

/// A member name as a volume path ("./a//b" → "/a/b"); `None` for names
/// that cannot be shown (`..`, invalid characters).
pub(crate) fn clean_path(name: &str) -> Option<String> {
    let mut parts = Vec::new();
    for c in name.split('/') {
        match c {
            "" | "." => {}
            ".." => return None,
            c => {
                VfsPath::validate_name(c).ok()?;
                parts.push(c);
            }
        }
    }
    Some(format!("/{}", parts.join("/")))
}

/// The archive's raw bytes: the device, or the unpacked buffer.
pub(crate) struct Raw<'a> {
    dev: &'a mut dyn BlockDevice,
    buf: Option<&'a [u8]>,
}

impl Raw<'_> {
    pub fn size(&self) -> u64 {
        match self.buf {
            Some(b) => b.len() as u64,
            None => self.dev.size(),
        }
    }

    /// Where each part of a split archive starts, and the end of the last
    /// (`None` unless the device joins several files).
    pub fn part_starts(&self) -> Option<Vec<u64>> {
        if self.buf.is_some() {
            return None;
        }
        let mut starts = vec![0u64];
        for s in self.dev.parts()? {
            starts.push(starts.last().unwrap() + s);
        }
        Some(starts)
    }

    pub fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<()> {
        match self.buf {
            Some(b) => {
                dix_core::device::check_range(offset, out.len(), b.len() as u64)?;
                out.copy_from_slice(&b[offset as usize..offset as usize + out.len()]);
                Ok(())
            }
            None => self.dev.read_at(offset, out),
        }
    }
}

// ----- times ----------------------------------------------------------------

pub(crate) use dix_core::time::{from_dos as from_dos_time, to_dos as dos_time};

// ----- the driver -------------------------------------------------------------

pub struct ArchiveDriver;

/// What the start of a device says.
fn sniff(dev: &mut dyn BlockDevice) -> Result<Option<Kind>> {
    let size = dev.size();
    let mut head = vec![0u8; size.min(512) as usize];
    dev.read_at(0, &mut head)?;
    if head.starts_with(&SEVENZ_SIGNATURE) {
        return Ok(Some(Kind::SevenZ));
    }
    if head.starts_with(RAR_SIGNATURE) {
        return Ok(Some(Kind::Rar));
    }
    if arj::is_arj(&mut Raw { dev: &mut *dev, buf: None })? {
        return Ok(Some(Kind::Arj));
    }
    if head.starts_with(ar::AR_MAGIC) {
        return Ok(Some(if ar::is_deb(&head) { Kind::Deb } else { Kind::Ar }));
    }
    if head.starts_with(&ar::RPM_MAGIC) {
        return Ok(Some(Kind::Rpm));
    }
    if cab::is_cab(&head) {
        return Ok(Some(Kind::Cab));
    }
    if head.starts_with(xar::MAGIC) {
        return Ok(Some(Kind::Xar));
    }
    let c = Compression::detect(&head);
    if c != Compression::None {
        // Tar or not is only known once unpacked: say tar for now.
        return Ok(Some(Kind::Tar(c)));
    }
    if cpio::is_cpio(&head) {
        return Ok(Some(Kind::Cpio(Compression::None)));
    }
    if tar::is_header(&head) {
        return Ok(Some(Kind::Tar(Compression::None)));
    }
    if zip::looks_like_zip(&mut Raw { dev, buf: None })? {
        return Ok(Some(Kind::Zip));
    }
    Ok(None)
}

/// A self-extracting program (a Windows .exe, a Linux ELF) carrying a 7z,
/// RAR or Cabinet archive after its code: where the archive starts. Each
/// signature found is checked (the 7z start header's CRC, RAR's main
/// header, the cabinet header) — the program's own code may hold the bytes
/// of one. (A zip is found by its end record, wherever it starts.)
fn embedded_archive(dev: &mut dyn BlockDevice) -> Result<Option<u64>> {
    let size = dev.size();
    if size < 4096 {
        return Ok(None);
    }
    let mut head = [0u8; 4];
    dev.read_at(0, &mut head)?;
    if head[..2] != *b"MZ" && head != *b"\x7fELF" {
        return Ok(None);
    }
    const CHUNK: u64 = 1 << 20;
    let limit = size.min(16 << 20);
    let mut buf = vec![0u8; CHUNK as usize + 64];
    let mut at = 64u64;
    while at < limit {
        let n = (size - at).min(CHUNK + 64) as usize;
        dev.read_at(at, &mut buf[..n])?;
        for i in 0..n.saturating_sub(40).min(CHUNK as usize) {
            let b = &buf[i..n];
            let ok = if b.starts_with(&SEVENZ_SIGNATURE) {
                dix_core::partition::crc32(&b[12..32]) == u32::from_le_bytes(b[8..12].try_into().unwrap())
            } else if b.starts_with(b"Rar!\x1a\x07\x00") {
                b[9] == 0x73 // RAR 4: the main header follows
            } else if b.starts_with(b"Rar!\x1a\x07\x01\x00") {
                // RAR 5: the main header's CRC32 over its size and body.
                let (mut len, mut k, mut shift) = (0u64, 12usize, 0);
                while k < 16 && b[k] & 0x80 != 0 {
                    len |= ((b[k] & 0x7F) as u64) << shift;
                    shift += 7;
                    k += 1;
                }
                len |= ((b[k] & 0x7F) as u64) << shift;
                let end = k + 1 + len as usize;
                end <= b.len()
                    && dix_core::partition::crc32(&b[12..end]) == u32::from_le_bytes(b[8..12].try_into().unwrap())
            } else if b.starts_with(cab::MAGIC) {
                cab::is_cab(b) && (u32::from_le_bytes(b[8..12].try_into().unwrap()) as u64) <= size - (at + i as u64)
            } else {
                false
            };
            if ok {
                return Ok(Some(at + i as u64));
            }
        }
        at += CHUNK;
    }
    Ok(None)
}

/// An empty archive of `variant`.
fn empty_archive(variant: &str, comment: Option<&str>) -> Result<Vec<u8>> {
    let empty = Tree::new();
    let mut no_content = |_: &str, _: &Node| -> Result<Vec<u8>> { Ok(Vec::new()) };
    let tar = || tar::write(&empty, &mut |_: &str, _: &Node| Ok(Vec::new()));
    let cpio = || cpio::write(&empty, &mut |_: &str, _: &Node| Ok(Vec::new()));
    match variant {
        "zip" => {
            let mut dummy = dix_core::device::MemoryDevice::new("empty", Vec::new());
            zip::write(&empty, &mut Raw { dev: &mut dummy, buf: None }, &mut no_content, comment)
        }
        "tar" => tar(),
        "tar.gz" => Compression::Gzip.compress(&tar()?),
        "tar.zst" => Compression::Zstd.compress(&tar()?),
        "tar.bz2" => Compression::Bzip2.compress(&tar()?),
        "cpio" => cpio(),
        "cpio.gz" => Compression::Gzip.compress(&cpio()?),
        "tar.xz" => Compression::Xz.compress(&tar()?),
        "cpio.xz" => Compression::Xz.compress_with(&cpio()?, 3, true),
        #[cfg(feature = "archive-7z")]
        "7z" => sevenz::write(&empty, &mut |_: &str, _: &Node| Ok(Vec::new()), SEVENZ_LEVEL, None),
        "cpio.zst" => Compression::Zstd.compress(&cpio()?),
        v => Err(Error::invalid_input(format!("unknown archive type '{v}'"))),
    }
}

/// Puts `bytes` on `dev` as its whole content.
fn store(dev: &mut dyn BlockDevice, bytes: &[u8]) -> Result<()> {
    if dev.size() != bytes.len() as u64 {
        if !dev.info().resizable {
            return Err(Error::new(ErrorKind::NoSpace, "an archive needs a resizable image (an image file)"));
        }
        dev.resize(bytes.len() as u64)?;
    }
    dev.write_at(0, bytes)?;
    dev.flush()
}

impl Driver for ArchiveDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: "archive",
            name: "Archive",
            description: "zip, tar, cpio (also .gz/.bz2/.zst/.xz/.lz/.lzma/.lz4/.Z), 7z, RAR, ARJ, .deb, .rpm, ar, .cab, XAR (.pkg), self-extracting .exe and single compressed files. zip, tar, cpio and 7z can be changed.",
            capabilities: CAPS,
        }
    }

    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
        Ok(match sniff(dev)? {
            None if embedded_archive(dev)?.is_some() => 70,
            Some(
                Kind::Tar(Compression::None)
                | Kind::Cpio(Compression::None)
                | Kind::Zip
                | Kind::SevenZ
                | Kind::Rar
                | Kind::Arj
                | Kind::Ar
                | Kind::Deb
                | Kind::Rpm
                | Kind::Cab
                | Kind::Xar,
            ) => 80,
            Some(_) => 60,
            None => 0,
        })
    }

    fn mount(&self, mut dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
        let log = &opts.log;
        if let Some(parts) = dev.parts() {
            log.info(format!("{} parts of a split file, read as one.", parts.len()));
        }
        if sniff(dev.as_mut())?.is_none()
            && let Some(at) = embedded_archive(dev.as_mut())?
        {
            log.info(format!("A self-extracting program: the archive in it, from byte {at}, is shown."));
            let (len, name) = (dev.size() - at, dev.info().name);
            dev = Box::new(dix_core::device::SliceDevice::new(dev, at, len, name)?);
        }
        let mut kind = sniff(dev.as_mut())?.ok_or_else(|| Error::new(ErrorKind::UnknownFormat, "not an archive"))?;
        let mut unpacked = None;
        if let Kind::Tar(c) = kind
            && c != Compression::None
        {
            let size = dev.size();
            log.info(format!("Unpacking {:.1} MB of {} data into memory…", size as f64 / 1e6, c.name()));
            let packed = dix_core::device::read_all(dev.as_mut())?;
            let bytes = c.decompress(&packed)?;
            // An empty tar is only zero blocks.
            let empty_tar = bytes.len() % 512 == 0 && bytes.len() <= 1 << 20 && bytes.iter().all(|&b| b == 0);
            let head = &bytes[..bytes.len().min(512)];
            if cpio::is_cpio(head) {
                kind = Kind::Cpio(c);
            } else if !empty_tar && !tar::is_header(head) {
                kind = Kind::Single(c);
            }
            unpacked = Some(bytes);
        }
        let mode = if dev.is_read_only() { AccessMode::ReadOnly } else { opts.mode };
        if mode.is_writable() {
            if let Some(why) = kind.read_only_reason() {
                return Err(Error::new(ErrorKind::ReadOnly, format!("{why}. Open it read-only.")));
            }
            if !dev.info().resizable {
                return Err(Error::new(
                    ErrorKind::ReadOnly,
                    "an archive on this device cannot grow or shrink, so it can only be read. Open it read-only.",
                ));
            }
        }
        let name = dev.info().name;
        let mut fs = ArchiveFs {
            dev,
            kind,
            tree: Tree::new(),
            unpacked,
            mode,
            dirty: false,
            cache: None,
            block: None,
            reader: None,
            comment: None,
            name,
            seven: SevenZArchive::default(),
            rar: RarArchive::default(),
            arj: arj::Archive::default(),
            cab: cab::Cabinet::default(),
            archives: 1,
            passwords: Vec::new(),
            password_request: None,
            list_locked: false,
            level: SEVENZ_LEVEL,
            seven_encrypted: false,
            seven_password: None,
            seven_names: false,
        };
        fs.load(log)?;
        if fs.archives > 1 {
            log.info(format!("{} cpio archives one after another (an initramfs).", fs.archives));
            if fs.mode.is_writable() {
                fs.mode = AccessMode::ReadOnly; // (nothing written: not dirty)
                return Err(Error::new(
                    ErrorKind::ReadOnly,
                    "this cpio file is several archives one after another (an initramfs with early microcode, \
                     say); writing would merge them into one, which the kernel may not boot. Open it read-only.",
                ));
            }
        }
        let (files, dirs) = fs.tree.counts();
        log.info(format!("{} archive: {files} files, {dirs} folders", kind.name()));
        let locked = fs.encrypted().len();
        if locked > 0 {
            let what = if locked == 1 { "1 file is".to_owned() } else { format!("{locked} files are") };
            log.info(format!("{what} encrypted: a password is needed to read them."));
            fs.password_request = Some(format!("{what} encrypted."));
        }
        Ok(Box::new(fs))
    }

    fn format(&self, dev: &mut dyn BlockDevice, opts: &FormatOptions) -> Result<()> {
        let variant = opts.variant.as_deref().unwrap_or("zip");
        store(dev, &empty_archive(variant, opts.label.as_deref())?)
    }

    fn min_format_size(&self) -> u64 {
        22
    }

    fn format_variants(&self) -> &'static [FormatVariant] {
        VARIANTS
    }
}

// ----- the volume -------------------------------------------------------------

pub struct ArchiveFs {
    dev: Box<dyn BlockDevice>,
    kind: Kind,
    tree: Tree,
    /// A compressed archive, unpacked.
    unpacked: Option<Vec<u8>>,
    mode: AccessMode,
    /// Changes not written to the device yet.
    dirty: bool,
    /// The last member unpacked whole (reads come in pieces).
    cache: Option<(String, Arc<Vec<u8>>)>,
    /// The last 7z block unpacked (it holds many files).
    block: Option<(usize, Arc<Vec<u8>>)>,
    /// The zip member being read (streamed).
    reader: Option<zip::MemberReader>,
    /// A 7z archive's folders.
    #[cfg_attr(not(feature = "archive-7z"), allow(dead_code))]
    seven: SevenZArchive,
    /// A RAR archive's members.
    #[cfg_attr(not(feature = "archive-rar"), allow(dead_code))]
    rar: RarArchive,
    /// An ARJ archive's members.
    arj: arj::Archive,
    /// A Cabinet's folders.
    cab: cab::Cabinet,
    /// The zip comment (kept when rewriting; shown as the label).
    comment: Option<String>,
    name: String,
    /// cpio: how many archives the file holds one after another.
    archives: usize,
    /// Passwords for encrypted zip members (each tried in turn).
    passwords: Vec<String>,
    /// Ask the user for a password (see `take_password_request`).
    password_request: Option<String>,
    /// The list of files itself is encrypted (7z, RAR): empty until a
    /// password opens it.
    list_locked: bool,
    /// 7z: the LZMA2 level to write with.
    level: u32,
    /// 7z: written encrypted (it was, or the settings say so), with this
    /// password (else the one that opened it), the file names too.
    seven_encrypted: bool,
    seven_password: Option<String>,
    seven_names: bool,
}

impl Drop for ArchiveFs {
    /// Last chance to write changes when dropped without unmounting (after
    /// a forced eject the device is gone and this fails, harmlessly).
    fn drop(&mut self) {
        if self.dirty {
            let _ = self.save();
        }
    }
}

impl ArchiveFs {
    fn raw(&mut self) -> Raw<'_> {
        Raw { dev: self.dev.as_mut(), buf: self.unpacked.as_deref() }
    }

    /// Reads the member list from the raw bytes.
    fn load(&mut self, log: &MountLog) -> Result<()> {
        self.cache = None;
        self.block = None;
        self.reader = None;
        let kind = self.kind;
        self.tree = match kind {
            #[cfg(feature = "archive-7z")]
            Kind::SevenZ => {
                let passwords = self.passwords.clone();
                let parsed = sevenz::parse(&mut self.raw(), log, &passwords);
                let Some((mut tree, archive)) = self.unless_list_locked(parsed, log)? else {
                    // (Written again, it stays encrypted, names and all.)
                    self.seven_encrypted = true;
                    self.seven_names = true;
                    return Ok(());
                };
                self.seven = archive;
                if self.seven.folders.iter().any(|f| f.encrypted()) {
                    self.seven_encrypted = true;
                }
                // A symlink's target is its content.
                let links: Vec<(String, Data)> = tree
                    .iter()
                    .filter(|(_, n)| n.kind == EntryKind::Symlink)
                    .map(|(p, n)| (p.to_owned(), n.data.clone()))
                    .collect();
                for (p, data) in links {
                    let target = self.member(&p, &data).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
                    if let Some(n) = tree.nodes.get_mut(&p) {
                        n.size = target.as_ref().map_or(0, |t| t.len() as u64);
                        n.link = target;
                    }
                }
                tree
            }
            #[cfg(not(feature = "archive-7z"))]
            Kind::SevenZ => return Err(codec::left_out("7z")),
            #[cfg(feature = "archive-rar")]
            Kind::Rar => {
                let passwords = self.passwords.clone();
                let mut raw = Raw { dev: self.dev.as_mut(), buf: self.unpacked.as_deref() };
                let parsed = rar::parse(&mut raw, log, &passwords);
                let Some((tree, arc)) = self.unless_list_locked(parsed, log)? else {
                    return Ok(());
                };
                self.rar = arc;
                tree
            }
            #[cfg(not(feature = "archive-rar"))]
            Kind::Rar => return Err(codec::left_out("RAR")),
            Kind::Arj => {
                let (tree, arc) = arj::parse(&mut self.raw(), log)?;
                self.arj = arc;
                tree
            }
            Kind::Ar => ar::parse(&mut self.raw())?,
            Kind::Xar => xar::parse(&mut self.raw(), log)?,
            Kind::Cab => {
                let (tree, cab) = cab::parse(&mut self.raw(), log)?;
                self.cab = cab;
                tree
            }
            Kind::Deb => {
                let (tree, bytes) = ar::parse_deb(&mut Raw { dev: self.dev.as_mut(), buf: None }, log)?;
                self.unpacked = Some(bytes);
                tree
            }
            Kind::Rpm => {
                let (tree, bytes, nvr) = ar::parse_rpm(&mut Raw { dev: self.dev.as_mut(), buf: None }, log)?;
                self.unpacked = Some(bytes);
                self.comment = Some(nvr).filter(|n| !n.is_empty());
                tree
            }
            Kind::Zip => {
                self.comment = zip::comment(&mut self.raw())?;
                let mut tree = zip::parse(&mut self.raw(), log)?;
                self.zip_links(&mut tree);
                tree
            }
            Kind::Tar(_) => tar::parse(&mut self.raw(), log)?,
            Kind::Cpio(_) => {
                let (tree, n) = cpio::parse(&mut self.raw(), log)?;
                self.archives = n;
                tree
            }
            Kind::Single(c) => {
                let len = self.unpacked.as_ref().map_or(0, |b| b.len() as u64);
                let mut head = vec![0u8; self.dev.size().min(4096) as usize];
                self.dev.read_at(0, &mut head)?;
                let base = self.name.rsplit(['/', '\\']).next().unwrap_or("data").to_owned();
                let name = codec::gzip_name(&head).and_then(|n| clean_path(&n)).unwrap_or_else(|| {
                    let stem = base.strip_suffix(c.extension()).filter(|s| !s.is_empty()).unwrap_or(&base);
                    clean_path(stem).filter(|p| p != "/").unwrap_or_else(|| "/data".to_owned())
                });
                let mut tree = Tree::new();
                let mut node = Node::new(EntryKind::File);
                node.size = len;
                node.data = Data::Range { offset: 0, len };
                tree.insert(&name, node);
                tree
            }
        };
        Ok(())
    }

    /// A file's whole content.
    fn content(raw: &mut Raw<'_>, node: &Node) -> Result<Vec<u8>> {
        match &node.data {
            Data::Empty => Ok(Vec::new()),
            Data::Range { offset, len } => {
                let mut v = vec![0u8; *len as usize];
                raw.read_at(*offset, &mut v)?;
                Ok(v)
            }
            // (Encrypted members are copied as they are, never read here.)
            d @ Data::Zip { .. } => zip::read_member(raw, d, &[]),
            Data::Mem(v) => Ok(v.as_ref().clone()),
            // 7z, RAR and ARJ archives are never rewritten.
            Data::SevenZ { .. } | Data::Rar { .. } | Data::Arj { .. } | Data::Cab { .. } | Data::Packed { .. } => {
                Err(Error::read_only())
            }
        }
    }

    /// A zip member or 7z file whole, through a small cache (reads come in
    /// pieces; a 7z folder holds many files).
    fn member(&mut self, path: &str, data: &Data) -> Result<Arc<Vec<u8>>> {
        match data {
            Data::Zip { .. } => {
                if let Some((p, b)) = &self.cache
                    && p == path
                {
                    return Ok(b.clone());
                }
                let mut raw = Raw { dev: self.dev.as_mut(), buf: self.unpacked.as_deref() };
                let b = match zip::read_member(&mut raw, data, &self.passwords) {
                    Ok(b) => Arc::new(b),
                    Err(e) => return Err(self.asked(e, path)),
                };
                self.cache = Some((path.to_owned(), b.clone()));
                Ok(b)
            }
            #[cfg(feature = "archive-7z")]
            &Data::SevenZ { folder, offset, len, crc } => {
                // The file, checked once; its block kept for the others.
                if let Some((p, b)) = &self.cache
                    && p == path
                {
                    return Ok(b.clone());
                }
                let block = match &self.block {
                    Some((f, b)) if *f == folder => b.clone(),
                    _ => {
                        let mut raw = Raw { dev: self.dev.as_mut(), buf: self.unpacked.as_deref() };
                        let b = match sevenz::folder_bytes(&mut raw, &self.seven, folder, &self.passwords) {
                            Ok(b) => Arc::new(b),
                            Err(e) => return Err(self.asked(e, path)),
                        };
                        self.block = Some((folder, b.clone()));
                        b
                    }
                };
                let range = offset as usize..(offset + len) as usize;
                let bytes = block.get(range).ok_or_else(|| Error::corrupted("7z file outside its block"))?;
                if crc.is_some_and(|c| dix_core::partition::crc32(bytes) != c) {
                    return Err(Error::corrupted(format!("{path}: checksum mismatch")));
                }
                let b = Arc::new(bytes.to_vec());
                self.cache = Some((path.to_owned(), b.clone()));
                Ok(b)
            }
            &Data::Packed { offset, len, compression } => {
                if let Some((p, b)) = &self.cache
                    && p == path
                {
                    return Ok(b.clone());
                }
                let mut packed = vec![0u8; len as usize];
                self.raw().read_at(offset, &mut packed)?;
                let b = Arc::new(compression.decompress(&packed).map_err(|e| e.with_context(path))?);
                self.cache = Some((path.to_owned(), b.clone()));
                Ok(b)
            }
            &Data::Cab { folder, offset, len } => {
                let block = match &self.block {
                    Some((f, b)) if *f == folder => b.clone(),
                    _ => {
                        let mut raw = Raw { dev: self.dev.as_mut(), buf: self.unpacked.as_deref() };
                        let b = Arc::new(cab::folder_bytes(&mut raw, &self.cab, folder)?);
                        self.block = Some((folder, b.clone()));
                        b
                    }
                };
                let bytes = block
                    .get(offset as usize..(offset + len) as usize)
                    .ok_or_else(|| Error::corrupted("a cabinet file is outside its folder"))?;
                Ok(Arc::new(bytes.to_vec()))
            }
            &Data::Arj { index } => {
                if let Some((p, b)) = &self.cache
                    && p == path
                {
                    return Ok(b.clone());
                }
                let mut raw = Raw { dev: self.dev.as_mut(), buf: self.unpacked.as_deref() };
                let b = Arc::new(arj::read_member(&self.arj, &mut raw, index)?);
                self.cache = Some((path.to_owned(), b.clone()));
                Ok(b)
            }
            _ => Err(Error::invalid_input("not a packed member")),
        }
    }

    /// Writes the whole archive anew, then reads it back.
    fn save(&mut self) -> Result<()> {
        let kind = self.kind;
        #[cfg(feature = "archive-7z")]
        if kind == Kind::SevenZ {
            // Every file's bytes (out of their blocks), then one new block.
            let files: Vec<(String, Data)> = self
                .tree
                .iter()
                .filter(|(_, n)| n.kind == EntryKind::File)
                .map(|(p, n)| (p.to_owned(), n.data.clone()))
                .collect();
            let mut bytes = BTreeMap::new();
            for (p, d) in files {
                let b = match &d {
                    Data::Empty => Vec::new(),
                    Data::Mem(v) => v.as_ref().clone(),
                    _ => self.member(&p, &d)?.as_ref().clone(),
                };
                bytes.insert(p, b);
            }
            let password = self.seven_write_password()?;
            let encrypt = password.as_deref().map(|p| sevenz::Encrypt { password: p, names: self.seven_names });
            let out = sevenz::write(
                &self.tree,
                &mut |p: &str, _: &Node| Ok(bytes.remove(p).unwrap_or_default()),
                self.level,
                encrypt,
            )?;
            store(self.dev.as_mut(), &out)?;
            self.dirty = false;
            return self.load(&MountLog::new());
        }
        let comment = self.comment.clone();
        let Self { dev, tree, unpacked, .. } = self;
        let mut raw = Raw { dev: dev.as_mut(), buf: unpacked.as_deref() };
        let bytes = match kind {
            Kind::Zip => {
                // New and changed files first; unchanged members are copied
                // by the writer straight from the raw bytes.
                let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
                for (p, n) in tree.iter() {
                    if n.kind == EntryKind::File
                        && !matches!(n.data, Data::Zip { method: 0 | 8, .. } | Data::Zip { encrypted: true, .. })
                    {
                        files.insert(p.to_owned(), Self::content(&mut raw, n)?);
                    }
                }
                let mut from_map = |p: &str, _: &Node| -> Result<Vec<u8>> { Ok(files.remove(p).unwrap_or_default()) };
                zip::write(tree, &mut raw, &mut from_map, comment.as_deref())?
            }
            Kind::Tar(c) | Kind::Cpio(c) => {
                let mut from_raw = |_: &str, n: &Node| Self::content(&mut raw, n);
                let plain = match kind {
                    Kind::Cpio(_) => cpio::write(tree, &mut from_raw)?,
                    _ => tar::write(tree, &mut from_raw)?,
                };
                // (An xz initramfs needs CRC32 checks: the kernel's decoder.)
                let crc32 = matches!(kind, Kind::Cpio(_));
                let packed =
                    if c == Compression::None { None } else { Some(c.compress_with(&plain, self.level, crc32)?) };
                store(dev.as_mut(), packed.as_deref().unwrap_or(&plain))?;
                if c != Compression::None {
                    *unpacked = Some(plain);
                }
                self.dirty = false;
                return self.load(&MountLog::new());
            }
            Kind::Single(_)
            | Kind::SevenZ
            | Kind::Rar
            | Kind::Arj
            | Kind::Ar
            | Kind::Deb
            | Kind::Rpm
            | Kind::Cab
            | Kind::Xar => {
                return Err(Error::read_only());
            }
        };
        store(dev.as_mut(), &bytes)?;
        self.dirty = false;
        self.load(&MountLog::new())
    }

    fn ensure_writable(&self) -> Result<()> {
        if !self.mode.is_writable() {
            return Err(Error::read_only());
        }
        // Written now, an archive whose list is still locked would lose it.
        if self.list_locked {
            return Err(Error::new(
                ErrorKind::PasswordRequired,
                "the list of files is encrypted: enter the password before changing the archive",
            ));
        }
        Ok(())
    }

    /// The password a 7z is written with (`None`: not encrypted).
    #[cfg(feature = "archive-7z")]
    fn seven_write_password(&self) -> Result<Option<String>> {
        if !self.seven_encrypted {
            return Ok(None);
        }
        match self.seven_password.clone().or_else(|| self.passwords.first().cloned()) {
            Some(p) => Ok(Some(p)),
            None => Err(Error::new(
                ErrorKind::PasswordRequired,
                "this 7z archive is written encrypted: enter its password first",
            )),
        }
    }

    fn node(&self, path: &VfsPath) -> Result<&Node> {
        self.tree.get(path.as_str()).ok_or_else(|| Error::not_found(path))
    }

    fn ensure_parent_dir(&self, path: &VfsPath) -> Result<()> {
        let parent = path.parent().ok_or_else(|| Error::new(ErrorKind::InvalidPath, "the root has no parent"))?;
        match self.tree.get(parent.as_str()) {
            Some(n) if n.kind == EntryKind::Directory => Ok(()),
            Some(_) => Err(Error::not_a_directory(&parent)),
            None => Err(Error::not_found(&parent)),
        }
    }

    fn changed(&mut self) {
        self.dirty = true;
        self.cache = None;
        self.block = None;
        self.reader = None;
    }
}

impl ArchiveFs {
    /// A zip symlink's target is its content (an encrypted one's is known
    /// once a password opens it).
    fn zip_links(&mut self, tree: &mut Tree) {
        let links: Vec<(String, Data)> = tree
            .iter()
            .filter(|(_, n)| n.kind == EntryKind::Symlink && n.link.is_none())
            .map(|(p, n)| (p.to_owned(), n.data.clone()))
            .collect();
        let mut raw = Raw { dev: self.dev.as_mut(), buf: self.unpacked.as_deref() };
        for (p, data) in links {
            let target = zip::read_member(&mut raw, &data, &self.passwords)
                .ok()
                .map(|b| String::from_utf8_lossy(&b).into_owned());
            if let Some(n) = tree.nodes.get_mut(&p) {
                n.size = target.as_ref().map_or(0, |t| t.len() as u64);
                n.link = target;
            }
        }
    }

    /// An error for reading `path`; one for lack of a password asks for it.
    fn asked(&mut self, e: Error, path: &str) -> Error {
        if e.kind() == ErrorKind::PasswordRequired {
            self.password_request = Some(format!("{path}: {e}"));
        }
        e.with_context(path)
    }

    /// The parsed list, or `None` (an empty volume asking for a password)
    /// when the list of files is encrypted and no password given opens it.
    #[cfg(any(feature = "archive-7z", feature = "archive-rar"))]
    fn unless_list_locked<T>(&mut self, parsed: Result<T>, log: &MountLog) -> Result<Option<T>> {
        match parsed {
            Err(e) if e.kind() == ErrorKind::PasswordRequired => {
                if !self.list_locked {
                    log.info("The list of files is encrypted: a password is needed to see it.");
                }
                self.list_locked = true;
                self.password_request = Some("The list of files is encrypted.".into());
                self.tree = Tree::new();
                Ok(None)
            }
            r => {
                self.list_locked = false;
                r.map(Some)
            }
        }
    }

    /// The encrypted files: (path, data), smallest first (for a solid 7z
    /// block, its size).
    fn encrypted(&self) -> Vec<(String, Data)> {
        let mut v: Vec<(String, Data, u64)> = self
            .tree
            .iter()
            .filter(|(_, n)| n.size > 0)
            .filter_map(|(p, n)| {
                let size = match n.data {
                    Data::Zip { encrypted: true, csize, .. } => csize,
                    #[cfg(feature = "archive-7z")]
                    Data::SevenZ { folder, .. } => {
                        let f = self.seven.folders.get(folder).filter(|f| f.encrypted())?;
                        f.size()
                    }
                    #[cfg(feature = "archive-rar")]
                    Data::Rar { index } => self.rar.encrypted_size(index)?,
                    _ => return None,
                };
                Some((p.to_owned(), n.data.clone(), size))
            })
            .collect();
        v.sort_by_key(|e| e.2);
        v.into_iter().map(|(p, d, _)| (p, d)).collect()
    }

    /// Whether `password` opens this encrypted file.
    fn opens(&mut self, data: &Data, password: &str) -> Result<()> {
        let one = [password.to_owned()];
        let mut raw = Raw { dev: self.dev.as_mut(), buf: self.unpacked.as_deref() };
        match data {
            Data::Zip { .. } => zip::read_member(&mut raw, data, &one).map(|_| ()),
            #[cfg(feature = "archive-7z")]
            &Data::SevenZ { folder, .. } => {
                let b = sevenz::folder_bytes(&mut raw, &self.seven, folder, &one)?;
                // Kept: the file asked for is likely in it.
                self.block = Some((folder, Arc::new(b)));
                Ok(())
            }
            #[cfg(feature = "archive-rar")]
            &Data::Rar { index } => rar::check_password(&mut self.rar, &mut raw, index, password),
            _ => Err(Error::invalid_input("not encrypted")),
        }
    }
}

impl FileSystem for ArchiveFs {
    fn settings(&self) -> Vec<dix_core::Setting> {
        let xz = matches!(self.kind, Kind::Tar(Compression::Xz) | Kind::Cpio(Compression::Xz));
        if !(self.kind == Kind::SevenZ || xz) || !self.mode.is_writable() {
            return Vec::new();
        }
        let level = dix_core::Setting {
            key: "level".into(),
            label: "Compression level".into(),
            help: "LZMA2, from 1 (fastest) to 9 (smallest). The archive is written anew after each change, so \
                   high levels make every change slow (6, the usual default of 7-Zip and xz, compresses about \
                   1 MB a second here). Changing it writes the archive again at the new level."
                .into(),
            kind: dix_core::SettingKind::Number,
            value: self.level.to_string(),
            group: if xz { "xz".into() } else { "7z".into() },
        };
        if xz {
            return vec![level];
        }
        vec![
            level,
            dix_core::Setting {
                key: "encrypt".into(),
                label: "Encrypt with a password".into(),
                help: "AES-256, as 7-Zip encrypts. Unticked, the archive is written without encryption.".into(),
                kind: dix_core::SettingKind::Flag,
                value: self.seven_encrypted.to_string(),
                group: "Password".into(),
            },
            dix_core::Setting {
                key: "password".into(),
                label: "Password".into(),
                help: "A new password; left empty, the current one is kept.".into(),
                kind: dix_core::SettingKind::Secret,
                value: String::new(),
                group: "Password".into(),
            },
            dix_core::Setting {
                key: "names".into(),
                label: "Encrypt file names".into(),
                help: "The list of files is encrypted too: nothing can be seen without the password.".into(),
                kind: dix_core::SettingKind::Flag,
                value: self.seven_names.to_string(),
                group: "Password".into(),
            },
        ]
    }

    fn apply_settings(&mut self, values: &[(String, String)]) -> Result<()> {
        self.ensure_writable()?;
        let get = |key: &str| values.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        let flag = |key: &str, now: bool| get(key).map_or(now, |v| v == "true");
        let encrypt = flag("encrypt", self.seven_encrypted);
        let names = flag("names", self.seven_names);
        let new_password = get("password").filter(|p| !p.is_empty()).map(str::to_owned);
        if encrypt && self.kind != Kind::SevenZ {
            return Err(Error::invalid_input("only 7z archives are written with a password"));
        }
        if encrypt && new_password.is_none() && self.seven_password.is_none() && self.passwords.is_empty() {
            return Err(Error::invalid_input("enter a password to encrypt with"));
        }
        if !cfg!(feature = "archive-aes") && encrypt {
            return Err(codec::left_out("AES encryption"));
        }
        if encrypt != self.seven_encrypted || names != self.seven_names || new_password.is_some() {
            self.seven_encrypted = encrypt;
            self.seven_names = names;
            if new_password.is_some() {
                self.seven_password = new_password;
            }
            self.changed();
        }
        for (k, v) in values {
            match k.as_str() {
                "encrypt" | "names" | "password" => {}
                "level" => {
                    let l: u32 =
                        v.trim().parse().map_err(|_| Error::invalid_input("the level is a number from 1 to 9"))?;
                    if !(1..=9).contains(&l) {
                        return Err(Error::invalid_input("the level is a number from 1 to 9"));
                    }
                    if l != self.level {
                        self.level = l;
                        self.changed();
                    }
                }
                _ => return Err(Error::invalid_input(format!("no setting '{k}'"))),
            }
        }
        Ok(())
    }

    fn uses_passwords(&self) -> bool {
        self.list_locked || !self.encrypted().is_empty()
    }

    fn take_password_request(&mut self) -> Option<String> {
        self.password_request.take()
    }

    fn add_password(&mut self, password: &str) -> Result<()> {
        if self.passwords.iter().any(|p| p == password) {
            return Ok(());
        }
        // An encrypted list of files: the password must open it.
        if self.list_locked {
            self.passwords.push(password.to_owned());
            let r = self.load(&MountLog::new());
            if self.list_locked || r.is_err() {
                self.passwords.pop();
                self.list_locked = true;
                self.password_request = None;
                r?;
                return Err(Error::new(ErrorKind::PasswordRequired, "this password does not open the list of files"));
            }
            self.password_request = None;
            return Ok(());
        }
        // Kept if it opens a file (trying the small ones; a wrong ZipCrypto
        // password is caught by the CRC; an empty file proves nothing).
        let members = self.encrypted();
        if members.is_empty() {
            return Err(Error::invalid_input("nothing here is encrypted"));
        }
        let (mut tried, mut other) = (false, None);
        for (_, data) in members.iter().take(64) {
            match self.opens(data, password) {
                Ok(()) => {
                    self.passwords.push(password.to_owned());
                    #[cfg(feature = "archive-rar")]
                    {
                        self.rar.passwords = self.passwords.clone();
                    }
                    self.password_request = None;
                    if !matches!(data, Data::SevenZ { .. }) {
                        self.cache = None;
                    }
                    if self.kind == Kind::Zip {
                        // (Not read again: changes may not be written yet.)
                        let mut tree = std::mem::replace(&mut self.tree, Tree::new());
                        self.zip_links(&mut tree);
                        self.tree = tree;
                    } else {
                        // Read-only kinds: read again, encrypted symlinks
                        // get their targets.
                        self.load(&MountLog::new())?;
                    }
                    return Ok(());
                }
                Err(e) if e.kind() == ErrorKind::PasswordRequired => tried = true,
                Err(e) => other = Some(e), // a file that cannot be read anyway
            }
        }
        match other {
            // Nothing could be tried (AES left out of the build, say).
            Some(e) if !tried => Err(e),
            _ => Err(Error::new(ErrorKind::PasswordRequired, "this password opens none of the encrypted files")),
        }
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            driver_id: "archive".into(),
            fs_name: self.kind.name(),
            label: self.comment.clone(),
            total_bytes: Some(self.tree.iter().map(|(_, n)| n.size).sum()),
            free_bytes: None,
            block_size: None,
            details: Vec::new(),
        }
    }

    fn capabilities(&self) -> Capabilities {
        let writable = self.kind.read_only_reason().is_none();
        let caps = if writable { CAPS.difference(Capabilities::FORMAT) } else { Capabilities::READ };
        caps.for_mode(self.mode)
    }

    fn access_mode(&self) -> AccessMode {
        self.mode
    }

    fn read_dir(&mut self, path: &VfsPath) -> Result<Vec<DirEntry>> {
        if self.node(path)?.kind != EntryKind::Directory {
            return Err(Error::not_a_directory(path));
        }
        Ok(self
            .tree
            .children(path.as_str())
            .map(|(name, n)| DirEntry { name: name.to_owned(), metadata: n.meta() })
            .collect())
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        Ok(self.node(path)?.meta())
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let node = self.node(path)?.clone();
        if node.kind == EntryKind::Directory {
            return Err(Error::is_a_directory(path));
        }
        if node.kind == EntryKind::Symlink {
            let t = node.link.unwrap_or_default().into_bytes();
            let from = (offset as usize).min(t.len());
            let n = buf.len().min(t.len() - from);
            buf[..n].copy_from_slice(&t[from..from + n]);
            return Ok(n);
        }
        if offset >= node.size {
            return Ok(0);
        }
        let n = buf.len().min((node.size - offset) as usize);
        match &node.data {
            Data::Empty => Ok(0),
            Data::Range { offset: at, len } => {
                let n = n.min(len.saturating_sub(offset) as usize);
                self.raw().read_at(at + offset, &mut buf[..n])?;
                Ok(n)
            }
            Data::Mem(v) => {
                buf[..n].copy_from_slice(&v[offset as usize..offset as usize + n]);
                Ok(n)
            }
            d @ Data::Zip { method: 0 | 8, encrypted: false, .. } => {
                // Streamed: no need to unpack the whole member for a piece.
                let mut reader = match self.reader.take() {
                    Some(r) if r.path == path.as_str() => r,
                    _ => zip::MemberReader::new(&mut self.raw(), path.as_str(), d)?,
                };
                let mut raw = Raw { dev: self.dev.as_mut(), buf: self.unpacked.as_deref() };
                let r = reader.read_at(&mut raw, offset, &mut buf[..n]);
                self.reader = Some(reader);
                r
            }
            #[cfg(feature = "archive-rar")]
            &Data::Rar { index } => {
                let mut raw = Raw { dev: self.dev.as_mut(), buf: self.unpacked.as_deref() };
                rar::read(&mut self.rar, &mut raw, index, offset, &mut buf[..n])
                    .map_err(|e| self.asked(e, path.as_str()))
            }
            #[cfg(not(feature = "archive-rar"))]
            Data::Rar { .. } => Err(codec::left_out("RAR")),
            d
            @ (Data::Zip { .. } | Data::SevenZ { .. } | Data::Arj { .. } | Data::Cab { .. } | Data::Packed { .. }) => {
                let bytes = self.member(path.as_str(), d)?;
                let n = n.min(bytes.len().saturating_sub(offset as usize));
                buf[..n].copy_from_slice(&bytes[offset as usize..offset as usize + n]);
                Ok(n)
            }
        }
    }

    fn write_file(&mut self, path: &VfsPath, data: &[u8]) -> Result<()> {
        self.ensure_writable()?;
        self.ensure_parent_dir(path)?;
        let mut node = Node::new(EntryKind::File);
        if let Some(old) = self.tree.get(path.as_str()) {
            if old.kind == EntryKind::Directory {
                return Err(Error::is_a_directory(path));
            }
            node.mode = old.mode;
            node.uid = old.uid;
            node.gid = old.gid;
        }
        node.size = data.len() as u64;
        node.data = Data::Mem(Arc::new(data.to_vec()));
        node.mtime = Some(dix_core::time::unix_now());
        self.tree.insert(path.as_str(), node);
        self.changed();
        Ok(())
    }

    fn write_at(&mut self, path: &VfsPath, offset: u64, data: &[u8]) -> Result<()> {
        self.ensure_writable()?;
        let node = self.node(path)?.clone();
        if node.kind != EntryKind::File {
            return Err(Error::is_a_directory(path));
        }
        // Kept in memory until the archive is saved: bring an existing
        // member in first.
        let mut bytes = match node.data {
            Data::Mem(v) => v,
            _ => Arc::new(Self::content(&mut self.raw(), &node)?),
        };
        // Drop the tree's reference, so the buffer is changed in place.
        if let Some(n) = self.tree.nodes.get_mut(path.as_str()) {
            n.data = Data::Empty;
        }
        let v = Arc::make_mut(&mut bytes);
        let end = offset as usize + data.len();
        if v.len() < end {
            v.resize(end, 0);
        }
        v[offset as usize..end].copy_from_slice(data);
        let n = self.tree.nodes.get_mut(path.as_str()).expect("checked above");
        n.size = v.len() as u64;
        n.data = Data::Mem(bytes);
        n.mtime = Some(dix_core::time::unix_now());
        self.changed();
        Ok(())
    }

    fn create_dir(&mut self, path: &VfsPath) -> Result<()> {
        self.ensure_writable()?;
        self.ensure_parent_dir(path)?;
        if self.tree.get(path.as_str()).is_some() {
            return Err(Error::already_exists(path));
        }
        let mut node = Node::new(EntryKind::Directory);
        node.mtime = Some(dix_core::time::unix_now());
        self.tree.insert(path.as_str(), node);
        self.changed();
        Ok(())
    }

    fn remove_file(&mut self, path: &VfsPath) -> Result<()> {
        self.ensure_writable()?;
        if self.node(path)?.kind == EntryKind::Directory {
            return Err(Error::is_a_directory(path));
        }
        self.tree.nodes.remove(path.as_str());
        self.changed();
        Ok(())
    }

    fn remove_dir(&mut self, path: &VfsPath) -> Result<()> {
        self.ensure_writable()?;
        if path.is_root() {
            return Err(Error::invalid_input("the root cannot be removed"));
        }
        if self.node(path)?.kind != EntryKind::Directory {
            return Err(Error::not_a_directory(path));
        }
        if self.tree.children(path.as_str()).next().is_some() {
            return Err(Error::new(ErrorKind::DirectoryNotEmpty, format!("{path} is not empty")));
        }
        self.tree.nodes.remove(path.as_str());
        self.changed();
        Ok(())
    }

    fn rename(&mut self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        self.ensure_writable()?;
        self.node(from)?;
        if from.is_root() || to.starts_with(from) && to != from {
            return Err(Error::invalid_input("cannot move a folder into itself"));
        }
        if self.tree.get(to.as_str()).is_some() {
            return Err(Error::already_exists(to));
        }
        self.ensure_parent_dir(to)?;
        for old in self.tree.subtree(from.as_str()) {
            let node = self.tree.nodes.remove(&old).expect("listed");
            let new = format!("{}{}", to.as_str(), &old[from.as_str().len()..]);
            self.tree.nodes.insert(new, node);
        }
        self.changed();
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        if self.dirty {
            self.save()?;
        }
        Ok(())
    }
}

/// A build without a codec still recognises the format and says what is
/// missing, rather than "unknown format".
#[cfg(all(test, not(feature = "archive-7z")))]
mod left_out_tests {
    use super::*;

    #[test]
    fn a_7z_archive_without_7z_support() {
        let mut bytes = SEVENZ_SIGNATURE.to_vec();
        bytes.resize(64, 0);
        let dev = dix_core::device::MemoryDevice::new("x.7z", bytes);
        let e = crate::default_registry().mount(Box::new(dev), None, AccessMode::ReadOnly).err().unwrap();
        assert!(e.to_string().contains("7z support was left out of this build"), "{e}");
    }
}
