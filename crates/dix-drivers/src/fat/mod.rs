//! **FAT12, FAT16 and FAT32** (read-write), with long file names.
//!
//! * [`bpb`]: the boot sector and where everything is;
//! * [`table`]: the file allocation table (cluster chains, allocation);
//! * [`dir`]: directory entries, 8.3 and long names;
//! * this module: the volume ([`FatFs`]) and the driver.
//!
//! Directories and the FAT are read on demand; file data goes straight
//! between the caller and the device, in place (large files are copied in
//! pieces with `write_at`). FAT changes are written back on `sync`.
//!
//! Crash detection: before the first change the volume is marked in use
//! (the boot sector's dirty bit, as Linux and `fsck.fat` use, and on
//! FAT16/32 the "clean shutdown" bit of FAT entry 1, as Windows uses); a
//! clean unmount clears both. A volume found marked in use is reported in
//! the mount log (FAT has no journal: the checker finds what is left).
//!
//! Times are stored as UTC (FAT has no time zone; Windows uses local time).

pub mod bpb;
pub mod check;
pub mod dir;
#[cfg(feature = "exfat")]
pub mod exfat;
pub mod mkfs;
pub mod table;
pub mod undelete;

/// The free space of a FAT12/16/32 volume (clusters whose FAT entry is 0),
/// as byte ranges: what a copy of only the used space leaves out.
pub fn free_space(mut dev: Box<dyn BlockDevice>) -> Result<Vec<(u64, u64)>> {
    let mut b = [0u8; 512];
    dev.read_at(0, &mut b)?;
    let l = bpb::Layout::parse(&b)?;
    let mut fat = table::Fat::new(&l);
    let cs = l.cluster_size as u64;
    let mut out: Vec<(u64, u64)> = Vec::new();
    for c in 2..=l.max_cluster() {
        if fat.get(dev.as_mut(), c)? != 0 {
            continue;
        }
        let at = l.cluster_offset(c);
        match out.last_mut() {
            Some(r) if r.1 == at => r.1 += cs,
            _ => out.push((at, at + cs)),
        }
    }
    Ok(out)
}

use bpb::{FatType, Layout};
use dir::Item;
use dix_core::{
    AccessMode, BlockDevice, Capabilities, DirEntry, Driver, DriverInfo, EntryKind, Error, ErrorKind, FileSystem,
    FormatOptions, FormatVariant, FsInfo, Metadata, MountOptions, ProbeScore, Result, VfsPath,
};
use table::Fat;

const CAPS: Capabilities = Capabilities::READ
    .union(Capabilities::WRITE)
    .union(Capabilities::CREATE_DIR)
    .union(Capabilities::REMOVE)
    .union(Capabilities::RENAME)
    .union(Capabilities::TIMESTAMPS)
    .union(Capabilities::FORMAT)
    .union(Capabilities::CHECK);

const VARIANTS: &[FormatVariant] = &[
    FormatVariant {
        id: "fat32",
        name: "FAT32",
        description: "Memory cards and USB sticks up to 2 TiB, files up to 4 GiB",
        min_size: mkfs::MIN_FAT32,
        extension: ".img",
        sized: true,
    },
    FormatVariant {
        id: "fat16",
        name: "FAT16",
        description: "Small volumes (4 MiB – 2 GiB), old systems",
        min_size: mkfs::MIN_FAT16,
        extension: ".img",
        sized: true,
    },
    FormatVariant {
        id: "fat12",
        name: "FAT12",
        description: "Floppy-sized volumes",
        min_size: mkfs::MIN_FAT12,
        extension: ".img",
        sized: true,
    },
    #[cfg(feature = "exfat")]
    FormatVariant {
        id: "exfat",
        name: "exFAT",
        description: "Large memory cards and USB sticks, files over 4 GiB",
        min_size: 1 << 20,
        extension: ".img",
        sized: true,
    },
];

/// FAT files are limited to 4 GiB - 1.
const MAX_FILE: u64 = u32::MAX as u64;

pub struct FatDriver;

impl Driver for FatDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: "fat",
            name: "FAT / exFAT",
            description: "FAT12, FAT16, FAT32 and exFAT (read-write, long file names)",
            capabilities: CAPS,
        }
    }

    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> {
        if dev.size() < 512 {
            return Ok(0);
        }
        let mut b = [0u8; 512];
        dev.read_at(0, &mut b)?;
        if &b[3..11] == b"EXFAT   " {
            #[cfg(feature = "exfat")]
            return Ok(if exfat::Boot::parse(&b).is_ok() { 95 } else { 0 });
            // Recognised, to say that exFAT support was left out.
            #[cfg(not(feature = "exfat"))]
            return Ok(60);
        }
        Ok(match Layout::parse(&b) {
            Ok(l) => {
                let named = &b[54..57] == b"FAT" || &b[82..87] == b"FAT32";
                if l.total_sectors * l.bytes_per_sector as u64 > dev.size() + l.bytes_per_sector as u64 {
                    40 // a volume larger than its device: a truncated image?
                } else if named {
                    90
                } else {
                    70
                }
            }
            Err(_) => 0,
        })
    }

    fn mount(&self, mut dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> {
        let log = &opts.log;
        let mut boot = [0u8; 512];
        dev.read_at(0, &mut boot)?;
        if &boot[3..11] == b"EXFAT   " {
            #[cfg(feature = "exfat")]
            return exfat::mount(dev, opts);
            #[cfg(not(feature = "exfat"))]
            return Err(Error::unsupported("exFAT support was left out of this build"));
        }
        let layout = Layout::parse(&boot)?;
        let mode = if dev.is_read_only() { AccessMode::ReadOnly } else { opts.mode };
        let end = layout.total_sectors * layout.bytes_per_sector as u64;
        if end > dev.size() {
            log.warn(format!("The volume claims {} bytes but the image has {}: it may be cut short.", end, dev.size()));
        }
        let mut fat = Fat::new(&layout);
        // In use when last seen?
        let boot_dirty = boot[layout.state_offset] & 1 != 0;
        let fat_unclean = fat.clean_bit(dev.as_mut())? == Some(false);
        if boot_dirty || fat_unclean {
            log.warn(
                "The volume was not cleanly unmounted (it was removed or the system stopped while it was in use).",
            );
            log.info("FAT has no journal: check the file system to find what an interrupted write left behind.");
        }
        // Free space: FSInfo's count if it is plausible, else counted.
        if let Some(sector) = layout.fs_info {
            let mut s = [0u8; 512];
            dev.read_at(sector as u64 * layout.bytes_per_sector as u64, &mut s)?;
            if dir::le32(&s, 0) == 0x4161_5252 && dir::le32(&s, 484) == 0x6141_7272 {
                let (free, next) = (dir::le32(&s, 488), dir::le32(&s, 492));
                if free <= layout.clusters && !(boot_dirty || fat_unclean) {
                    fat.free = Some(free);
                }
                if (2..=layout.max_cluster()).contains(&next) {
                    fat.next_free = next;
                }
            }
        }
        if fat.free.is_none() {
            fat.count_free(dev.as_mut())?;
            fat.trim();
        }
        log.info(format!(
            "{}: {} clusters of {} bytes, {} free",
            layout.fat_type.name(),
            layout.clusters,
            layout.cluster_size,
            fat.free.unwrap_or(0)
        ));
        let mut fs = FatFs { dev, layout, fat, mode, in_use: boot_dirty || fat_unclean, chain: None, label: None };
        fs.label = fs.read_label()?;
        Ok(Box::new(fs))
    }

    fn format(&self, dev: &mut dyn BlockDevice, opts: &FormatOptions) -> Result<()> {
        let t = match opts.variant.as_deref() {
            Some("fat12") => FatType::Fat12,
            Some("fat16") => FatType::Fat16,
            Some("fat32") => FatType::Fat32,
            #[cfg(feature = "exfat")]
            Some("exfat") => return exfat::mkfs::mkfs(dev, opts.label.as_deref().unwrap_or("")),
            None => mkfs::default_type(dev.size()),
            Some(v) => return Err(Error::invalid_input(format!("unknown FAT type '{v}'"))),
        };
        mkfs::mkfs(dev, t, opts.label.as_deref().unwrap_or(""))
    }

    fn min_format_size(&self) -> u64 {
        mkfs::MIN_FAT12
    }

    fn check(&self, mut dev: Box<dyn BlockDevice>, opts: &dix_core::CheckOptions) -> Result<dix_core::CheckReport> {
        let mut b = [0u8; 512];
        dev.read_at(0, &mut b)?;
        if &b[3..11] == b"EXFAT   " {
            #[cfg(feature = "exfat")]
            return exfat::check::check(dev, &opts.log, opts.repair);
            #[cfg(not(feature = "exfat"))]
            return Err(Error::unsupported("exFAT support was left out of this build"));
        }
        check::check(dev, &opts.log, opts.repair)
    }

    fn format_variants(&self) -> &'static [FormatVariant] {
        VARIANTS
    }
}

/// A directory: the FAT12/16 root region, or a cluster chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirRef {
    Root,
    Cluster(u32),
}

pub struct FatFs {
    dev: Box<dyn BlockDevice>,
    layout: Layout,
    fat: Fat,
    mode: AccessMode,
    /// Marked in use on disk.
    in_use: bool,
    /// The last file's chain, as runs of consecutive clusters:
    /// (first cluster of the file, [(cluster, count)]).
    chain: Option<(u32, Vec<(u32, u32)>)>,
    label: Option<String>,
}

impl FatFs {
    fn cs(&self) -> u64 {
        self.layout.cluster_size as u64
    }

    fn ensure_writable(&self) -> Result<()> {
        if self.mode.is_writable() { Ok(()) } else { Err(Error::read_only()) }
    }

    /// Before the first change: mark the volume in use on disk.
    fn begin_change(&mut self) -> Result<()> {
        self.ensure_writable()?;
        if !self.in_use {
            self.set_state(false)?;
            self.in_use = true;
        }
        Ok(())
    }

    /// The dirty bits: boot sector (and its FAT32 backup) and FAT entry 1.
    fn set_state(&mut self, clean: bool) -> Result<()> {
        let at = self.layout.state_offset as u64;
        let mut b = [0u8; 1];
        self.dev.read_at(at, &mut b)?;
        b[0] = if clean { b[0] & !1 } else { b[0] | 1 };
        self.dev.write_at(at, &b)?;
        self.fat.set_clean_bit(self.dev.as_mut(), clean)?;
        self.fat.flush(self.dev.as_mut())?;
        self.dev.flush()
    }

    fn root(&self) -> DirRef {
        DirRef::Root
    }

    /// The slots of a directory: (device offset of the chunk, bytes).
    fn chunks(&mut self, d: DirRef) -> Result<Vec<(u64, Vec<u8>)>> {
        let start = match d {
            DirRef::Root if self.layout.fat_type != FatType::Fat32 => {
                let mut v = vec![0u8; self.layout.root_bytes as usize];
                self.dev.read_at(self.layout.root_offset, &mut v)?;
                return Ok(vec![(self.layout.root_offset, v)]);
            }
            DirRef::Root => self.layout.root_cluster,
            DirRef::Cluster(c) => c,
        };
        let mut out = Vec::new();
        for c in self.fat.chain(self.dev.as_mut(), start)? {
            let at = self.layout.cluster_offset(c);
            let mut v = vec![0u8; self.cs() as usize];
            self.dev.read_at(at, &mut v)?;
            out.push((at, v));
        }
        Ok(out)
    }

    fn slots(chunks: &[(u64, Vec<u8>)]) -> impl Iterator<Item = (u64, [u8; 32])> + '_ {
        chunks.iter().flat_map(|(at, v)| {
            v.chunks_exact(32).enumerate().map(move |(i, e)| (at + i as u64 * 32, e.try_into().unwrap()))
        })
    }

    fn list(&mut self, d: DirRef) -> Result<Vec<Item>> {
        let chunks = self.chunks(d)?;
        Ok(dir::parse(Self::slots(&chunks)).0)
    }

    fn read_label(&mut self) -> Result<Option<String>> {
        let chunks = self.chunks(DirRef::Root)?;
        Ok(dir::parse(Self::slots(&chunks)).1.or_else(|| self.layout.label.clone()))
    }

    /// The directory an item is (`..` entries point at 0 for the root).
    fn dir_of(&self, item: &Item) -> DirRef {
        match item.cluster() {
            0 => DirRef::Root,
            c if self.layout.fat_type == FatType::Fat32 && c == self.layout.root_cluster => DirRef::Root,
            c => DirRef::Cluster(c),
        }
    }

    fn find(items: &[Item], name: &str) -> Option<usize> {
        let lower = name.to_lowercase();
        items
            .iter()
            .position(|i| i.name == name)
            .or_else(|| items.iter().position(|i| i.name.to_lowercase() == lower))
            .or_else(|| items.iter().position(|i| dir::short_display(&i.raw).to_lowercase() == lower))
    }

    /// The directory `path` is (it must be one).
    fn resolve_dir(&mut self, path: &VfsPath) -> Result<DirRef> {
        let mut d = self.root();
        for c in path.components() {
            let items = self.list(d)?;
            let i = Self::find(&items, c).ok_or_else(|| Error::not_found(path))?;
            if !items[i].is_dir() {
                return Err(Error::not_a_directory(path));
            }
            d = self.dir_of(&items[i]);
        }
        Ok(d)
    }

    /// The entry of `path` (not the root) and the directory holding it.
    fn lookup(&mut self, path: &VfsPath) -> Result<(DirRef, Item)> {
        let parent = path.parent().ok_or_else(|| Error::invalid_input("the root has no entry"))?;
        let name = path.file_name().unwrap_or_default();
        let d = self.resolve_dir(&parent)?;
        let items = self.list(d)?;
        let i = Self::find(&items, name).ok_or_else(|| Error::not_found(path))?;
        Ok((d, items[i].clone()))
    }

    fn meta(item: &Item) -> Metadata {
        let e = &item.raw;
        let mut m =
            Metadata::new(if item.is_dir() { EntryKind::Directory } else { EntryKind::File }, item.size() as u64);
        if item.is_dir() {
            m.size = 0;
        }
        m.modified = dix_core::time::from_dos(dir::le16(e, 24), dir::le16(e, 22));
        m.created = dix_core::time::from_dos(dir::le16(e, 16), dir::le16(e, 14));
        m.accessed = dix_core::time::from_dos(dir::le16(e, 18), 0);
        m.inode = Some(item.cluster() as u64);
        m.hidden = item.attr() & dir::ATTR_HIDDEN != 0;
        // Read-only maps to permission bits for display.
        let w = if item.attr() & dir::ATTR_READ_ONLY != 0 { 0 } else { 0o200 };
        m.mode = Some(if item.is_dir() { 0o040555 | w } else { 0o100444 | w });
        m
    }

    /// The runs of a file's chain (cached for the last file).
    fn runs(&mut self, first: u32) -> Result<Vec<(u32, u32)>> {
        if let Some((f, r)) = &self.chain
            && *f == first
        {
            return Ok(r.clone());
        }
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for c in self.fat.chain(self.dev.as_mut(), first)? {
            match runs.last_mut() {
                Some((s, n)) if *s + *n == c => *n += 1,
                _ => runs.push((c, 1)),
            }
        }
        self.chain = Some((first, runs.clone()));
        Ok(runs)
    }

    /// Calls `f(device offset, range in the buffer)` for the bytes
    /// `[offset, offset + len)` of a file whose chain is `runs`.
    fn spans(&self, runs: &[(u32, u32)], offset: u64, len: usize) -> Vec<(u64, std::ops::Range<usize>)> {
        let cs = self.cs();
        let mut out = Vec::new();
        let mut file_pos = 0u64;
        let mut done = 0usize;
        for &(start, n) in runs {
            let run_bytes = n as u64 * cs;
            let (lo, hi) = (offset + done as u64, offset + len as u64);
            if lo >= file_pos + run_bytes {
                file_pos += run_bytes;
                continue;
            }
            let within = lo - file_pos;
            let take = (run_bytes - within).min(((hi - lo) as usize) as u64) as usize;
            out.push((self.layout.cluster_offset(start) + within, done..done + take));
            done += take;
            file_pos += run_bytes;
            if done == len {
                break;
            }
        }
        out
    }

    fn write_slot(&mut self, at: u64, e: &[u8; 32]) -> Result<()> {
        self.dev.write_at(at, e)
    }

    /// A zeroed cluster.
    fn zero_cluster(&mut self, c: u32) -> Result<()> {
        let z = vec![0u8; self.cs() as usize];
        self.dev.write_at(self.layout.cluster_offset(c), &z)
    }

    /// Adds an entry named `name` to directory `d` (its short entry made
    /// from `template`: attributes, cluster, size, times).
    fn add_entry(&mut self, d: DirRef, name: &str, template: [u8; 32]) -> Result<Item> {
        if name.encode_utf16().count() > 255 {
            return Err(Error::invalid_input("FAT names are limited to 255 characters"));
        }
        let mut chunks = self.chunks(d)?;
        let items = dir::parse(Self::slots(&chunks)).0;
        if Self::find(&items, name).is_some_and(|i| items[i].name.to_lowercase() == name.to_lowercase()) {
            return Err(Error::already_exists(name));
        }
        let shorts: Vec<[u8; 11]> = items.iter().map(|i| i.short()).collect();
        let (short, flags, needs_lfn) = dir::short_for(name, &|s| shorts.contains(s));
        let mut entries = if needs_lfn { dir::lfn_entries(name, &short) } else { Vec::new() };
        let mut e = template;
        e[..11].copy_from_slice(&short);
        if e[0] == 0xE5 {
            e[0] = 0x05;
        }
        e[12] = flags;
        entries.push(e);
        // A run of free slots (never starting after the end marker).
        let need = entries.len();
        let find = |chunks: &[(u64, Vec<u8>)]| -> Option<Vec<u64>> {
            let mut run: Vec<u64> = Vec::new();
            for (at, slot) in Self::slots(chunks) {
                if slot[0] == 0x00 || slot[0] == 0xE5 {
                    run.push(at);
                    if run.len() == need {
                        return Some(run);
                    }
                } else {
                    run.clear();
                }
            }
            None
        };
        let places = match find(&chunks) {
            Some(p) => p,
            None => {
                // Grow the directory (the FAT12/16 root cannot grow).
                let first = match d {
                    DirRef::Root if self.layout.fat_type != FatType::Fat32 => {
                        return Err(Error::new(ErrorKind::NoSpace, "the root directory is full"));
                    }
                    DirRef::Root => self.layout.root_cluster,
                    DirRef::Cluster(c) => c,
                };
                let last = *self.fat.chain(self.dev.as_mut(), first)?.last().expect("a directory has a cluster");
                let per = (self.cs() / 32) as usize;
                let more = (need.div_ceil(per)) as u32 + 1;
                for c in self.fat.alloc(self.dev.as_mut(), more, Some(last))? {
                    self.zero_cluster(c)?;
                    chunks.push((self.layout.cluster_offset(c), vec![0u8; self.cs() as usize]));
                }
                self.chain = None;
                find(&chunks).ok_or_else(|| Error::new(ErrorKind::NoSpace, "the directory is full"))?
            }
        };
        for (at, e) in places.iter().zip(&entries) {
            self.write_slot(*at, e)?;
        }
        let (items, _) = dir::parse(Self::slots(&self.chunks(d)?));
        items
            .into_iter()
            .find(|i| i.offset == *places.last().unwrap())
            .ok_or_else(|| Error::corrupted("entry vanished"))
    }

    fn delete_entry(&mut self, item: &Item) -> Result<()> {
        for &at in item.lfn_offsets.iter().chain(std::iter::once(&item.offset)) {
            self.dev.write_at(at, &[0xE5])?;
        }
        Ok(())
    }

    fn update_entry(&mut self, item: &Item, f: impl FnOnce(&mut [u8; 32])) -> Result<()> {
        let mut e = [0u8; 32];
        self.dev.read_at(item.offset, &mut e)?;
        f(&mut e);
        self.dev.write_at(item.offset, &e)
    }

    /// Writes FSInfo (FAT32): the free count and where to look next.
    fn write_fs_info(&mut self) -> Result<()> {
        let Some(sector) = self.layout.fs_info else { return Ok(()) };
        let at = sector as u64 * self.layout.bytes_per_sector as u64;
        let mut s = [0u8; 512];
        self.dev.read_at(at, &mut s)?;
        if dir::le32(&s, 0) != 0x4161_5252 {
            return Ok(());
        }
        s[488..492].copy_from_slice(&self.fat.free.unwrap_or(u32::MAX).to_le_bytes());
        s[492..496].copy_from_slice(&self.fat.next_free.to_le_bytes());
        self.dev.write_at(at, &s)
    }
}

impl FileSystem for FatFs {
    fn info(&self) -> FsInfo {
        let cs = self.cs();
        FsInfo {
            driver_id: "fat".into(),
            fs_name: self.layout.fat_type.name().into(),
            label: self.label.clone(),
            total_bytes: Some(self.layout.clusters as u64 * cs),
            free_bytes: self.fat.free.map(|f| f as u64 * cs),
            block_size: Some(cs as u32),
            details: Vec::new(),
        }
    }

    fn capabilities(&self) -> Capabilities {
        CAPS.difference(Capabilities::FORMAT.union(Capabilities::CHECK)).for_mode(self.mode)
    }

    fn access_mode(&self) -> AccessMode {
        self.mode
    }

    fn read_dir(&mut self, path: &VfsPath) -> Result<Vec<DirEntry>> {
        let d = self.resolve_dir(path)?;
        Ok(self.list(d)?.iter().map(|i| DirEntry { name: i.name.clone(), metadata: Self::meta(i) }).collect())
    }

    fn metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        if path.is_root() {
            return Ok(Metadata::new(EntryKind::Directory, 0));
        }
        Ok(Self::meta(&self.lookup(path)?.1))
    }

    fn read(&mut self, path: &VfsPath, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let (_, item) = self.lookup(path)?;
        if item.is_dir() {
            return Err(Error::is_a_directory(path));
        }
        let size = item.size() as u64;
        if offset >= size {
            return Ok(0);
        }
        let n = buf.len().min((size - offset) as usize);
        let runs = self.runs(item.cluster())?;
        let spans = self.spans(&runs, offset, n);
        let got = spans.last().map_or(0, |s| s.1.end);
        if got < n {
            return Err(Error::corrupted(format!("{path}: its cluster chain is shorter than its size")));
        }
        for (at, r) in spans {
            self.dev.read_at(at, &mut buf[r])?;
        }
        Ok(n)
    }

    fn write_file(&mut self, path: &VfsPath, data: &[u8]) -> Result<()> {
        self.create_file(path)?;
        if !data.is_empty() {
            self.write_at(path, 0, data)?;
        }
        Ok(())
    }

    fn create_file(&mut self, path: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let now = dix_core::time::unix_now();
        match self.lookup(path) {
            Ok((_, item)) if item.is_dir() => Err(Error::is_a_directory(path)),
            Ok((_, item)) => {
                if item.cluster() != 0 {
                    self.fat.free_chain(self.dev.as_mut(), item.cluster())?;
                    self.chain = None;
                }
                self.update_entry(&item, |e| {
                    dir::set_cluster(e, 0);
                    e[28..32].copy_from_slice(&0u32.to_le_bytes());
                    dir::set_modified(e, now);
                })
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let parent = path.parent().unwrap_or_default();
                let d = self.resolve_dir(&parent)?;
                let name = path.file_name().unwrap_or_default();
                let e = dir::short_entry(&[b' '; 11], 0, dir::ATTR_ARCHIVE, 0, 0, now);
                self.add_entry(d, name, e).map(|_| ())
            }
            Err(e) => Err(e),
        }
    }

    fn write_at(&mut self, path: &VfsPath, offset: u64, data: &[u8]) -> Result<()> {
        self.begin_change()?;
        let (_, item) = self.lookup(path)?;
        if item.is_dir() {
            return Err(Error::is_a_directory(path));
        }
        let old_size = item.size() as u64;
        let end = offset + data.len() as u64;
        if end > MAX_FILE {
            return Err(Error::new(ErrorKind::NoSpace, "FAT files are limited to 4 GiB"));
        }
        let new_size = old_size.max(end);
        let cs = self.cs();
        let mut first = item.cluster();
        let have = self.runs(first)?.iter().map(|r| r.1 as u64).sum::<u64>();
        let need = new_size.div_ceil(cs);
        if need > have {
            let last = self.runs(first)?.last().map(|&(s, n)| s + n - 1);
            let added = self.fat.alloc(self.dev.as_mut(), (need - have) as u32, last)?;
            if first == 0 {
                first = added[0];
            }
            self.chain = None;
        }
        let runs = self.runs(first)?;
        // Writing past the end: what lies between is zeroes.
        if offset > old_size {
            let zeros = vec![0u8; (offset - old_size).min(1 << 20) as usize];
            let mut at = old_size;
            while at < offset {
                let n = (offset - at).min((zeros.len()) as u64) as usize;
                for (dev_at, r) in self.spans(&runs, at, n) {
                    self.dev.write_at(dev_at, &zeros[r])?;
                }
                at += n as u64;
            }
        }
        for (dev_at, r) in self.spans(&runs, offset, data.len()) {
            self.dev.write_at(dev_at, &data[r])?;
        }
        let now = dix_core::time::unix_now();
        self.update_entry(&item, |e| {
            dir::set_cluster(e, first);
            e[28..32].copy_from_slice(&(new_size as u32).to_le_bytes());
            dir::set_modified(e, now);
            e[11] |= dir::ATTR_ARCHIVE;
        })
    }

    fn create_dir(&mut self, path: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let parent = path.parent().ok_or_else(|| Error::already_exists("/"))?;
        let d = self.resolve_dir(&parent)?;
        let name = path.file_name().unwrap_or_default();
        if Self::find(&self.list(d)?, name).is_some() {
            return Err(Error::already_exists(path));
        }
        let now = dix_core::time::unix_now();
        let c = self.fat.alloc(self.dev.as_mut(), 1, None)?[0];
        self.zero_cluster(c)?;
        let parent_cluster = match d {
            DirRef::Root => 0,
            DirRef::Cluster(p) => p,
        };
        let dot = dir::short_entry(b".          ", 0, dir::ATTR_DIR, c, 0, now);
        let dotdot = dir::short_entry(b"..         ", 0, dir::ATTR_DIR, parent_cluster, 0, now);
        let at = self.layout.cluster_offset(c);
        self.write_slot(at, &dot)?;
        self.write_slot(at + 32, &dotdot)?;
        let e = dir::short_entry(&[b' '; 11], 0, dir::ATTR_DIR, c, 0, now);
        match self.add_entry(d, name, e) {
            Ok(_) => Ok(()),
            Err(err) => {
                let _ = self.fat.free_chain(self.dev.as_mut(), c);
                Err(err)
            }
        }
    }

    fn remove_file(&mut self, path: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let (_, item) = self.lookup(path)?;
        if item.is_dir() {
            return Err(Error::is_a_directory(path));
        }
        self.delete_entry(&item)?;
        if item.cluster() != 0 {
            self.fat.free_chain(self.dev.as_mut(), item.cluster())?;
        }
        self.chain = None;
        Ok(())
    }

    fn remove_dir(&mut self, path: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let (_, item) = self.lookup(path)?;
        if !item.is_dir() {
            return Err(Error::not_a_directory(path));
        }
        let me = self.dir_of(&item);
        if !self.list(me)?.is_empty() {
            return Err(Error::new(ErrorKind::DirectoryNotEmpty, format!("{path} is not empty")));
        }
        self.delete_entry(&item)?;
        if item.cluster() != 0 {
            self.fat.free_chain(self.dev.as_mut(), item.cluster())?;
        }
        Ok(())
    }

    fn rename(&mut self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        self.begin_change()?;
        let (from_dir, item) = self.lookup(from)?;
        if to.starts_with(from) && to != from {
            return Err(Error::invalid_input("cannot move a folder into itself"));
        }
        let to_parent = to.parent().ok_or_else(|| Error::invalid_input("the root"))?;
        let to_dir = self.resolve_dir(&to_parent)?;
        let to_name = to.file_name().unwrap_or_default();
        let same_name = to_dir == from_dir && to_name.to_lowercase() == item.name.to_lowercase();
        if !same_name && Self::find(&self.list(to_dir)?, to_name).is_some() {
            return Err(Error::already_exists(to));
        }
        // Only the case changes: the old entry has to go first.
        if same_name {
            self.delete_entry(&item)?;
            return self.add_entry(to_dir, to_name, item.raw).map(|_| ());
        }
        // New entry first: an interruption leaves two names, not none.
        self.add_entry(to_dir, to_name, item.raw)?;
        self.delete_entry(&item)?;
        if item.is_dir() && to_dir != from_dir {
            // Its ".." now points at the new parent.
            let parent_cluster = match to_dir {
                DirRef::Root => 0,
                DirRef::Cluster(c) => c,
            };
            let at = self.layout.cluster_offset(item.cluster()) + 32;
            let mut e = [0u8; 32];
            self.dev.read_at(at, &mut e)?;
            if &e[..2] == b".." {
                dir::set_cluster(&mut e, parent_cluster);
                self.dev.write_at(at, &e)?;
            }
        }
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        if !self.mode.is_writable() {
            return Ok(());
        }
        self.fat.flush(self.dev.as_mut())?;
        self.write_fs_info()?;
        self.fat.trim();
        self.dev.flush()
    }

    fn unmount(&mut self) -> Result<()> {
        if self.mode.is_writable() && self.in_use {
            self.sync()?;
            self.set_state(true)?;
            self.in_use = false;
        }
        Ok(())
    }
}

impl Drop for FatFs {
    /// Dropped without unmounting: write the FAT changes at least (the
    /// volume stays marked in use; after a forced eject this fails,
    /// harmlessly).
    fn drop(&mut self) {
        if self.mode.is_writable() && self.in_use {
            let _ = self.sync();
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
