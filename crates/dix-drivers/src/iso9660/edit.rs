//! Changing an ISO image: the tree is held in memory and the whole image
//! is written anew on [`sync`](dix_core::FileSystem::sync), as with
//! archives (ISO 9660 has no free space to change in place).
//!
//! New file data does not stay in memory: it is appended to the device
//! past the image ("spooled") while the volume is open. Saving writes the
//! new image after everything that is there, reading unchanged files from
//! the old image and new ones from the spool, then moves it to the start
//! and cuts the device to its size. Readers ignore what lies past an
//! image's end, so an image left with spooled data (a crash) still opens.

use super::write::{self, BootSpec, IsoIo, VolumeIds, WNode};
use super::{IsoFs, Node};
use dix_core::{
    BlockDevice, DirEntry, EntryKind, Error, ErrorKind, Metadata, MountLog, Result, Setting, SettingKind, VfsPath,
};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Where a file's bytes are.
#[derive(Debug, Clone)]
pub(super) enum Src {
    Empty,
    /// In the image as it was opened.
    Old(Arc<Node>),
    /// In a UDF-only image as it was opened (read through the UDF driver).
    #[cfg_attr(not(feature = "udf"), allow(dead_code))]
    Udf(String),
    /// Spooled past the image's end.
    Spool {
        at: u64,
        len: u64,
    },
}

#[derive(Debug, Clone)]
pub(super) struct ENode {
    kind: EntryKind,
    size: u64,
    mtime: i64,
    atime: Option<i64>,
    ctime: Option<i64>,
    perm: u32,
    type_bits: u32,
    uid: u32,
    gid: u32,
    link: Option<String>,
    dev: Option<(u32, u32)>,
    src: Src,
}

impl ENode {
    fn new(kind: EntryKind) -> ENode {
        ENode {
            kind,
            size: 0,
            mtime: dix_core::time::unix_now(),
            atime: None,
            ctime: None,
            perm: if kind == EntryKind::Directory { 0o755 } else { 0o644 },
            type_bits: 0,
            uid: 0,
            gid: 0,
            link: None,
            dev: None,
            src: Src::Empty,
        }
    }

    fn meta(&self) -> Metadata {
        let mut m = Metadata::new(self.kind, self.size);
        m.modified = Some(self.mtime);
        m.accessed = self.atime;
        m.created = self.ctime;
        let t = match self.kind {
            EntryKind::Directory => 0o040000,
            EntryKind::Symlink => 0o120000,
            EntryKind::File => 0o100000,
            EntryKind::Other => self.type_bits,
        };
        m.mode = Some(t | self.perm);
        m.uid = Some(self.uid);
        m.gid = Some(self.gid);
        m.symlink_target = self.link.clone();
        m
    }
}

/// The tree being edited.
pub(super) struct Edit {
    nodes: BTreeMap<String, ENode>,
    /// Changes not written yet.
    pub dirty: bool,
    /// The end of what the device holds (image and spool).
    tail: u64,
    label: String,
    boot: Boot,
    /// Also write UDF (kept from a bridge image, or chosen in the settings).
    udf: bool,
    /// Write the ISO 9660 trees (off for a UDF-only image).
    iso: bool,
}

/// How the image boots (its settings).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Boot {
    /// The BIOS boot image's path (empty: none).
    bios: String,
    /// 512-byte sectors the BIOS loads.
    load: u16,
    info_table: bool,
    /// The UEFI boot image's path.
    efi: String,
    hybrid: bool,
    /// Hybrid with a GPT (else an MBR alone).
    gpt: bool,
    /// MBR boot code: a file in the image, or the image's own code.
    mbr_path: String,
    mbr_kept: Option<Vec<u8>>,
}

/// The value meaning "the image's own MBR boot code".
const KEPT: &str = "(the image's own)";

fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

impl Edit {
    fn children<'a>(&'a self, dir: &'a str) -> impl Iterator<Item = (&'a str, &'a ENode)> + 'a {
        let prefix = if dir == "/" { "/".to_owned() } else { format!("{dir}/") };
        let len = prefix.len();
        self.nodes
            .range(prefix.clone()..)
            .take_while(move |(p, _)| p.starts_with(&prefix))
            .filter(move |(p, _)| p.len() > len && !p[len..].contains('/'))
            .map(move |(p, n)| (&p[len..], n))
    }

    fn get(&self, path: &VfsPath) -> Result<&ENode> {
        self.nodes.get(path.as_str()).ok_or_else(|| Error::not_found(path))
    }

    fn parent_dir(&self, path: &VfsPath) -> Result<()> {
        let p = parent(path.as_str());
        match self.nodes.get(p) {
            Some(n) if n.kind == EntryKind::Directory => Ok(()),
            Some(_) => Err(Error::not_a_directory(p)),
            None => Err(Error::not_found(p)),
        }
    }
}

impl IsoFs {
    /// Reads the whole tree for editing.
    pub(super) fn start_editing(&mut self) -> Result<()> {
        let mut nodes = BTreeMap::new();
        let root = self.root.clone();
        let mut r = ENode::new(EntryKind::Directory);
        r.mtime = root.mtime.unwrap_or(r.mtime);
        if let Some(m) = root.rr.mode {
            r.perm = m & 0o7777;
        }
        nodes.insert("/".to_owned(), r);
        let mut stack = vec![("/".to_owned(), root)];
        while let Some((path, dir)) = stack.pop() {
            for e in self.list(&dir)?.iter() {
                if e.node.children.is_some() {
                    continue; // [BOOT]: the boot images, shown, not stored
                }
                let p = if path == "/" { format!("/{}", e.name) } else { format!("{path}/{}", e.name) };
                let n = &e.node;
                let mut en = ENode::new(n.kind);
                en.size = n.size;
                en.mtime = n.mtime.unwrap_or(0);
                en.atime = n.rr.atime;
                en.ctime = n.rr.ctime;
                if let Some(m) = n.rr.mode {
                    en.perm = m & 0o7777;
                    en.type_bits = m & 0o170000;
                }
                en.uid = n.rr.uid.unwrap_or(0);
                en.gid = n.rr.gid.unwrap_or(0);
                en.link = n.rr.link.clone();
                en.dev = n.rr.dev;
                if n.kind == EntryKind::File {
                    en.src = Src::Old(n.clone());
                }
                if n.kind == EntryKind::Directory {
                    stack.push((p.clone(), n.clone()));
                }
                nodes.insert(p, en);
            }
        }
        let tail = self.dev.size().max(self.info.total_bytes.unwrap_or(0));
        // Keep how the image boots: each boot image must be one of its files.
        let mut boot = Boot {
            bios: String::new(),
            load: 4,
            info_table: true,
            efi: String::new(),
            hybrid: !self.hybrid_kinds.is_empty(),
            // New hybrids get a GPT; existing ones keep what they had.
            gpt: self.hybrid_kinds.is_empty() || self.hybrid_kinds.contains(&"GPT"),
            mbr_path: String::new(),
            mbr_kept: None,
        };
        let cannot = |why: String| {
            Error::new(ErrorKind::ReadOnly, format!("this ISO image can only be read: {why}. Open it read-only."))
        };
        for (platform, media, count, rba) in self.boot_entries.clone() {
            let found = nodes
                .iter()
                .find(|(_, n)| matches!(&n.src, Src::Old(o) if o.extents.first().map(|e| e.0) == Some(rba)));
            let Some((path, node)) = found.map(|(p, n)| (p.clone(), n.clone())) else {
                return Err(cannot(format!("a boot image (at block {rba}) is not one of its files")));
            };
            if media != 0 {
                return Err(cannot(format!("{path} boots with floppy or hard disk emulation, which is not kept")));
            }
            match platform {
                0 => {
                    let mut head = [0u8; 16];
                    if let Src::Old(o) = &node.src {
                        self.read_node(o, 0, &mut head)?;
                    }
                    let le = |i: usize| u32::from_le_bytes(head[i..i + 4].try_into().unwrap()) as u64;
                    boot.info_table = le(8) == 16 && le(12) == rba;
                    boot.load = count.max(1);
                    boot.bios = path;
                }
                0xEF => boot.efi = path,
                p => return Err(cannot(format!("it has a boot entry for platform 0x{p:02X}, which is not kept"))),
            }
        }
        if boot.hybrid {
            let mut code = vec![0u8; 432];
            self.dev.read_at(0, &mut code)?;
            if code.iter().any(|&b| b != 0) {
                boot.mbr_kept = Some(code);
            }
        }
        let label = self.info.label.clone().unwrap_or_default();
        let udf = self.has_udf;
        self.edit = Some(Edit { nodes, dirty: false, tail, label, boot, udf, iso: true });
        Ok(())
    }

    fn edit(&mut self) -> &mut Edit {
        self.edit.as_mut().expect("editing")
    }

    pub(super) fn e_read_dir(&mut self, path: &VfsPath) -> Result<Vec<DirEntry>> {
        let ed = self.edit.as_ref().unwrap();
        if ed.get(path)?.kind != EntryKind::Directory {
            return Err(Error::not_a_directory(path));
        }
        Ok(ed.children(path.as_str()).map(|(name, n)| DirEntry { name: name.to_owned(), metadata: n.meta() }).collect())
    }

    pub(super) fn e_metadata(&mut self, path: &VfsPath) -> Result<Metadata> {
        Ok(self.edit.as_ref().unwrap().get(path)?.meta())
    }

    pub(super) fn e_read(&mut self, path: &VfsPath, off: u64, buf: &mut [u8]) -> Result<usize> {
        let n = self.edit.as_ref().unwrap().get(path)?.clone();
        match n.kind {
            EntryKind::Directory => return Err(Error::is_a_directory(path)),
            EntryKind::File => {}
            _ => return Ok(0),
        }
        self.read_src(&n.src, n.size, off, buf)
    }

    fn read_src(&mut self, src: &Src, size: u64, off: u64, buf: &mut [u8]) -> Result<usize> {
        if off >= size {
            return Ok(0);
        }
        let n = buf.len().min((size - off) as usize);
        match src {
            Src::Empty => Ok(0),
            Src::Old(node) => self.read_node(node, off, &mut buf[..n]),
            Src::Udf(path) => {
                let fs = self.udf_src.as_mut().ok_or_else(|| Error::corrupted("UDF source gone"))?;
                fs.read(&VfsPath::parse(path)?, off, &mut buf[..n])
            }
            Src::Spool { at, .. } => {
                self.dev.read_at(at + off, &mut buf[..n])?;
                Ok(n)
            }
        }
    }

    /// Appends `data` past everything on the device.
    fn spool(&mut self, data: &[u8]) -> Result<u64> {
        let at = self.edit().tail;
        if !data.is_empty() {
            self.dev.resize(at + data.len() as u64)?;
            self.dev.write_at(at, data)?;
        }
        self.edit().tail = at + data.len() as u64;
        Ok(at)
    }

    pub(super) fn e_write_file(&mut self, path: &VfsPath, data: &[u8]) -> Result<()> {
        if path.is_root() {
            return Err(Error::is_a_directory(path));
        }
        let ed = self.edit.as_ref().unwrap();
        ed.parent_dir(path)?;
        let old = ed.nodes.get(path.as_str()).cloned();
        if old.as_ref().is_some_and(|n| n.kind == EntryKind::Directory) {
            return Err(Error::is_a_directory(path));
        }
        let at = self.spool(data)?;
        let mut n = ENode::new(EntryKind::File);
        if let Some(o) = old.filter(|o| o.kind == EntryKind::File) {
            (n.perm, n.uid, n.gid) = (o.perm, o.uid, o.gid);
        }
        n.size = data.len() as u64;
        n.src = if data.is_empty() { Src::Empty } else { Src::Spool { at, len: n.size } };
        let ed = self.edit();
        ed.nodes.insert(path.as_str().to_owned(), n);
        ed.dirty = true;
        Ok(())
    }

    pub(super) fn e_write_at(&mut self, path: &VfsPath, off: u64, data: &[u8]) -> Result<()> {
        let n = self.edit.as_ref().unwrap().get(path)?.clone();
        if n.kind != EntryKind::File {
            return Err(Error::is_a_directory(path));
        }
        let tail = self.edit().tail;
        // Appending to the newest spooled file: just grow it.
        let append_to = match n.src {
            Src::Spool { at, len } if at + len == tail && off == len => Some(at),
            Src::Empty if off == 0 => Some(tail),
            _ => None,
        };
        if let Some(at) = append_to {
            self.spool(data)?;
            let ed = self.edit();
            let node = ed.nodes.get_mut(path.as_str()).unwrap();
            node.size = off + data.len() as u64;
            node.src = Src::Spool { at, len: node.size };
            node.mtime = dix_core::time::unix_now();
            ed.dirty = true;
            return Ok(());
        }
        // Anything else: the whole content, changed, spooled again.
        let mut all = vec![0u8; n.size as usize];
        let mut got = 0;
        while got < all.len() {
            let k = self.read_src(&n.src, n.size, got as u64, &mut all[got..])?;
            if k == 0 {
                break;
            }
            got += k;
        }
        let end = off as usize + data.len();
        if all.len() < end {
            all.resize(end, 0);
        }
        all[off as usize..end].copy_from_slice(data);
        self.e_write_file(path, &all)
    }

    pub(super) fn e_create_dir(&mut self, path: &VfsPath) -> Result<()> {
        let ed = self.edit();
        ed.parent_dir(path)?;
        if ed.nodes.contains_key(path.as_str()) {
            return Err(Error::already_exists(path));
        }
        ed.nodes.insert(path.as_str().to_owned(), ENode::new(EntryKind::Directory));
        ed.dirty = true;
        Ok(())
    }

    pub(super) fn e_remove(&mut self, path: &VfsPath, dir: bool) -> Result<()> {
        let ed = self.edit();
        let n = ed.get(path)?;
        if dir != (n.kind == EntryKind::Directory) {
            return Err(if dir { Error::not_a_directory(path) } else { Error::is_a_directory(path) });
        }
        if path.is_root() {
            return Err(Error::invalid_input("the root cannot be removed"));
        }
        if dir && ed.children(path.as_str()).next().is_some() {
            return Err(Error::new(ErrorKind::DirectoryNotEmpty, format!("{path} is not empty")));
        }
        ed.nodes.remove(path.as_str());
        // A boot file that is gone no longer boots anything.
        for p in [&mut ed.boot.bios, &mut ed.boot.efi, &mut ed.boot.mbr_path] {
            if p == path.as_str() {
                p.clear();
            }
        }
        ed.dirty = true;
        Ok(())
    }

    pub(super) fn e_rename(&mut self, from: &VfsPath, to: &VfsPath) -> Result<()> {
        let ed = self.edit();
        ed.get(from)?;
        ed.parent_dir(to)?;
        if from.is_root() || to.starts_with(from) && to != from {
            return Err(Error::invalid_input("a folder cannot move into itself"));
        }
        if ed.nodes.contains_key(to.as_str()) {
            return Err(Error::already_exists(to));
        }
        let prefix = format!("{}/", from.as_str());
        let moved: Vec<String> =
            ed.nodes.keys().filter(|k| *k == from.as_str() || k.starts_with(&prefix)).cloned().collect();
        for p in [&mut ed.boot.bios, &mut ed.boot.efi, &mut ed.boot.mbr_path] {
            if p == from.as_str() || p.starts_with(&prefix) {
                *p = format!("{}{}", to.as_str(), &p[from.as_str().len()..]);
            }
        }
        for k in moved {
            let n = ed.nodes.remove(&k).unwrap();
            let new = format!("{}{}", to.as_str(), &k[from.as_str().len()..]);
            ed.nodes.insert(new, n);
        }
        ed.dirty = true;
        Ok(())
    }

    /// Writes the image anew (see the module documentation).
    pub(super) fn save(&mut self) -> Result<()> {
        let Some(ed) = self.edit.take() else { return Ok(()) };
        let r = self.rewrite(&ed);
        if let Err(e) = r {
            self.edit = Some(ed);
            return Err(e);
        }
        // Read the new image (and edit on).
        let log = MountLog::new();
        let dev = std::mem::replace(&mut self.dev, Box::new(dix_core::device::MemoryDevice::new("gone", Vec::new())));
        #[cfg(feature = "udf")]
        if !ed.iso {
            // UDF only: through the device shared with the UDF reader.
            self.udf_src = None;
            let shared = match self.shared.take() {
                Some(s) => s,
                None => dix_core::device::SharedDevice::new(dev),
            };
            *self = IsoFs::open_udf(shared, &log)?;
            return Ok(());
        }
        let mut fresh = IsoFs::open(dev, &log)?;
        fresh.writable = true;
        fresh.start_editing()?;
        *self = fresh;
        Ok(())
    }

    fn rewrite(&mut self, ed: &Edit) -> Result<()> {
        // The tree as a list, parents before children (node 0: the root).
        let mut nodes: Vec<WNode> = Vec::new();
        let mut srcs: Vec<Src> = Vec::new();
        let mut index: BTreeMap<&str, usize> = BTreeMap::new();
        for (path, n) in &ed.nodes {
            let i = nodes.len();
            index.insert(path.as_str(), i);
            nodes.push(WNode {
                name: path.rsplit('/').next().unwrap_or("").to_owned(),
                kind: n.kind,
                size: if n.kind == EntryKind::File { n.size } else { 0 },
                mtime: n.mtime,
                atime: n.atime,
                ctime: n.ctime,
                perm: n.perm,
                type_bits: n.type_bits,
                uid: n.uid,
                gid: n.gid,
                link: n.link.clone(),
                dev: n.dev,
                // Hard links and unchanged files share their data.
                content_key: match &n.src {
                    Src::Old(o) => o.extents.first().map(|e| e.0 ^ (o.size << 40)),
                    _ => None,
                },
                children: Vec::new(),
            });
            srcs.push(n.src.clone());
            if path != "/" {
                let p = index[parent(path)];
                nodes[p].children.push(i);
            }
        }
        let ids = VolumeIds {
            label: ed.label.clone(),
            publisher: self.ids.0.clone(),
            preparer: "DISK IMAGE EXPLORER".into(),
            application: self.ids.1.clone(),
            iso: ed.iso,
            udf: ed.udf,
        };
        let base = self.dev.size().max(ed.tail);
        let b = &ed.boot;
        let node_of = |p: &str| (!p.is_empty()).then(|| index.get(p).copied()).flatten();
        let mbr_code = match node_of(&b.mbr_path) {
            Some(i) => {
                let mut code = vec![0u8; 432.min(nodes[i].size as usize)];
                self.read_src(&srcs[i], nodes[i].size, 0, &mut code)?;
                Some(code)
            }
            None => b.mbr_kept.clone(),
        };
        let boot = BootSpec {
            bios: node_of(&b.bios).map(|i| (i, b.load, b.info_table)),
            efi: node_of(&b.efi),
            hybrid: b.hybrid && (!b.bios.is_empty() || !b.efi.is_empty()),
            gpt: b.gpt,
            mbr_code,
        };
        let mut io = Saver {
            fs: self,
            base,
            pos: 0,
            buf: Vec::new(),
            srcs: &srcs,
            sizes: nodes.iter().map(|n| n.size).collect(),
        };
        let size = write::write(&nodes, &ids, &boot, &mut io)?;
        io.flush()?;
        // Move the new image to the start (front to back: the copy never
        // overtakes what it reads), then drop the rest.
        let mut chunk = vec![0u8; 1 << 20];
        let mut off = 0u64;
        while off < size {
            let n = (size - off).min((chunk.len()) as u64) as usize;
            self.dev.read_at(base + off, &mut chunk[..n])?;
            self.dev.write_at(off, &chunk[..n])?;
            off += n as u64;
        }
        self.dev.resize(size)?;
        self.dev.flush()
    }
}

/// Writes the new image past `base` and reads file contents.
struct Saver<'a> {
    fs: &'a mut IsoFs,
    base: u64,
    pos: u64,
    buf: Vec<u8>,
    srcs: &'a [Src],
    sizes: Vec<u64>,
}

impl Saver<'_> {
    fn flush(&mut self) -> Result<()> {
        if !self.buf.is_empty() {
            self.fs.dev.write_at(self.base + self.pos, &self.buf)?;
            self.pos += self.buf.len() as u64;
            self.buf.clear();
        }
        Ok(())
    }
}

impl IsoIo for Saver<'_> {
    fn begin(&mut self, size: u64) -> Result<()> {
        self.fs.dev.resize(self.base + size)
    }

    fn put(&mut self, data: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= 1 << 20 {
            self.flush()?;
        }
        Ok(())
    }

    fn read_file(&mut self, node: usize, off: u64, buf: &mut [u8]) -> Result<usize> {
        let src = self.srcs[node].clone();
        self.fs.read_src(&src, self.sizes[node], off, buf)
    }
}

/// An image in memory (new, empty images).
pub(super) struct VecIo(pub Vec<u8>);

impl IsoIo for VecIo {
    fn begin(&mut self, size: u64) -> Result<()> {
        self.0.reserve(size as usize);
        Ok(())
    }

    fn put(&mut self, data: &[u8]) -> Result<()> {
        self.0.extend_from_slice(data);
        Ok(())
    }

    fn read_file(&mut self, _: usize, _: u64, _: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
}

impl IsoFs {
    pub(super) fn e_settings(&self) -> Vec<Setting> {
        let ed = self.edit.as_ref().unwrap();
        let b = &ed.boot;
        let set = |key: &str, label: &str, help: &str, kind, value: String, group: &str| Setting {
            key: key.into(),
            label: label.into(),
            help: help.into(),
            kind,
            value,
            group: group.into(),
        };
        let mbr = if !b.mbr_path.is_empty() {
            b.mbr_path.clone()
        } else if b.mbr_kept.is_some() {
            KEPT.into()
        } else {
            String::new()
        };
        vec![
            set(
                "label",
                "Volume label",
                "Up to 32 characters (16 in Joliet).",
                SettingKind::Text,
                ed.label.clone(),
                "Volume",
            ),
            set(
                "iso9660",
                "ISO 9660",
                "Write the ISO 9660 trees (Rock Ridge and Joliet names). Off: a UDF-only image, as Blu-ray \
                 discs are (no CD/DVD booting).",
                SettingKind::Flag,
                ed.iso.to_string(),
                "Volume",
            ),
            set(
                "udf",
                "UDF",
                "Also write a UDF file system for the same files, as DVD and Blu-ray discs have: Windows, \
                 macOS and Linux read it, and it holds files of 4 GiB and more for every system.",
                SettingKind::Flag,
                ed.udf.to_string(),
                "Volume",
            ),
            set(
                "bios",
                "BIOS boot image",
                "A file in the image the BIOS loads when booting from CD/DVD (no emulation), \
                 e.g. isolinux/isolinux.bin or boot/grub/i386-pc/eltorito.img. Empty: no BIOS boot.",
                SettingKind::File,
                b.bios.clone(),
                "Boot",
            ),
            set(
                "load",
                "BIOS load size",
                "512-byte sectors the BIOS loads (4 for isolinux and GRUB).",
                SettingKind::Number,
                b.load.to_string(),
                "Boot",
            ),
            set(
                "info_table",
                "Boot info table",
                "Patch the boot image with where it lies (isolinux and GRUB need it).",
                SettingKind::Flag,
                b.info_table.to_string(),
                "Boot",
            ),
            set(
                "efi",
                "UEFI boot image",
                "A FAT image file in the image holding EFI/BOOT/BOOTX64.EFI (and others). Empty: no UEFI boot.",
                SettingKind::File,
                b.efi.clone(),
                "Boot",
            ),
            set(
                "hybrid",
                "Hybrid (USB)",
                "Also boot when the image is written to a USB stick or disk: an MBR with the image as \
                 partition 1 and the UEFI image as partition 2.",
                SettingKind::Flag,
                b.hybrid.to_string(),
                "Boot",
            ),
            set(
                "gpt",
                "GPT",
                "With a GPT, as current Linux images: the UEFI image becomes an EFI system partition. \
                 Off: an MBR alone, as isohybrid writes it, for old machines.",
                SettingKind::Flag,
                b.gpt.to_string(),
                "Boot",
            ),
            set(
                "mbr",
                "USB BIOS boot code",
                "For BIOS booting from USB: MBR code that loads the BIOS boot image, such as isolinux's \
                 isohdpfx.bin (a file in the image).",
                SettingKind::File,
                mbr,
                "Boot",
            ),
        ]
    }

    pub(super) fn e_apply_settings(&mut self, values: &[(String, String)]) -> Result<()> {
        let ed = self.edit.as_ref().unwrap();
        let (mut label, mut b, mut udf, mut iso) = (ed.label.clone(), ed.boot.clone(), ed.udf, ed.iso);
        let file = |p: &str, what: &str, min: u64| -> Result<String> {
            if p.is_empty() {
                return Ok(String::new());
            }
            let vp = VfsPath::parse(p).map_err(|_| Error::invalid_input(format!("{what}: {p} is not a path")))?;
            match ed.nodes.get(vp.as_str()) {
                Some(n) if n.kind == EntryKind::File && n.size >= min => Ok(vp.as_str().to_owned()),
                Some(n) if n.kind == EntryKind::File => {
                    Err(Error::invalid_input(format!("{what}: {p} is too small ({} bytes)", n.size)))
                }
                _ => Err(Error::invalid_input(format!("{what}: {p} is not a file in the image"))),
            }
        };
        for (k, v) in values {
            let v = v.trim();
            match k.as_str() {
                "label" => label = v.to_owned(),
                "udf" => udf = v == "true",
                "iso9660" => iso = v == "true",
                "bios" => b.bios = file(v, "BIOS boot image", 1)?,
                "efi" => b.efi = file(v, "UEFI boot image", 512)?,
                "load" => {
                    b.load = v
                        .parse()
                        .ok()
                        .filter(|&n: &u16| n > 0)
                        .ok_or_else(|| Error::invalid_input("BIOS load size: a number from 1 to 65535"))?
                }
                "info_table" => b.info_table = v == "true",
                "hybrid" => b.hybrid = v == "true",
                "gpt" => b.gpt = v == "true",
                "mbr" if v == KEPT => {}
                "mbr" => {
                    b.mbr_path = file(v, "USB BIOS boot code", 432)?;
                    if v.is_empty() {
                        b.mbr_kept = None;
                    }
                }
                _ => return Err(Error::invalid_input(format!("unknown setting {k}"))),
            }
        }
        if b.info_table && !b.bios.is_empty() && ed.nodes[&b.bios].size < 64 {
            return Err(Error::invalid_input("BIOS boot image: too small for a boot info table"));
        }
        if b.hybrid && b.bios.is_empty() && b.efi.is_empty() {
            return Err(Error::invalid_input("Hybrid (USB): choose a BIOS or UEFI boot image first"));
        }
        if !iso && !cfg!(feature = "udf") {
            return Err(Error::invalid_input("UDF-only images need the UDF driver, which this build lacks"));
        }
        if !iso && !udf {
            return Err(Error::invalid_input("ISO 9660, UDF or both: an image needs one"));
        }
        if !iso && (!b.bios.is_empty() || !b.efi.is_empty()) {
            return Err(Error::invalid_input("booting from CD/DVD needs ISO 9660: remove the boot images first"));
        }
        let ed = self.edit();
        if (&label, &b, udf, iso) != (&ed.label, &ed.boot, ed.udf, ed.iso) {
            ed.label = label;
            ed.boot = b;
            ed.udf = udf;
            ed.iso = iso;
            ed.dirty = true;
        }
        Ok(())
    }
}

impl IsoFs {
    /// Why rewriting this bridge image would lose files: something in its
    /// UDF tree (path → kind, size) is not the same in the ISO 9660 tree.
    #[cfg(feature = "udf")]
    pub(super) fn udf_differs(&mut self, udf: &BTreeMap<String, (EntryKind, u64)>) -> Result<Option<String>> {
        let mut nodes = BTreeMap::new();
        let saved = self.edit.take();
        self.start_editing()?;
        if let Some(ed) = self.edit.take() {
            nodes = ed.nodes;
        }
        self.edit = saved;
        for (path, &(kind, size)) in udf {
            let same = match nodes.get(path) {
                Some(n) if n.kind == kind => kind != EntryKind::File || n.size == size,
                _ => false,
            };
            if !same {
                return Ok(Some(format!(
                    "its UDF file system holds {path}, which its ISO 9660 one does not (as on Windows install \
                     images): writing it anew would lose it"
                )));
            }
        }
        Ok(None)
    }
}

impl IsoFs {
    /// Opens a UDF-only image for changing: its tree (read by the UDF
    /// driver) is edited as an ISO's is, and the image is written anew as
    /// UDF only when saved.
    #[cfg(feature = "udf")]
    pub(crate) fn open_udf(shared: dix_core::device::SharedDevice, log: &MountLog) -> Result<IsoFs> {
        let mut src = crate::udf::UdfDriver::open_fs(Box::new(shared.handle()), log)?;
        let info = src.info();
        let mut nodes = BTreeMap::new();
        let root = src.metadata(&VfsPath::root())?;
        let mut r = ENode::new(EntryKind::Directory);
        r.mtime = root.modified.unwrap_or(r.mtime);
        r.perm = root.mode.map_or(0o755, |m| m & 0o7777);
        nodes.insert("/".to_owned(), r);
        let mut stack = vec![VfsPath::root()];
        while let Some(dir) = stack.pop() {
            for e in src.read_dir(&dir)? {
                let p = dir.join(&e.name)?;
                let m = &e.metadata;
                let mut en = ENode::new(m.kind);
                en.size = if m.kind == EntryKind::Directory { 0 } else { m.size };
                en.mtime = m.modified.unwrap_or(0);
                en.atime = m.accessed;
                en.ctime = m.created;
                if let Some(mode) = m.mode {
                    en.perm = mode & 0o7777;
                    en.type_bits = mode & 0o170000;
                }
                en.uid = m.uid.unwrap_or(0);
                en.gid = m.gid.unwrap_or(0);
                en.link = m.symlink_target.clone();
                match m.kind {
                    EntryKind::File if m.size > 0 => en.src = Src::Udf(p.as_str().to_owned()),
                    EntryKind::Directory => stack.push(p.clone()),
                    _ => {}
                }
                nodes.insert(p.as_str().to_owned(), en);
            }
        }
        let tail = shared.size();
        let label = info.label.clone().unwrap_or_default();
        let boot = Boot {
            bios: String::new(),
            load: 4,
            info_table: true,
            efi: String::new(),
            hybrid: false,
            gpt: true,
            mbr_path: String::new(),
            mbr_kept: None,
        };
        let mut fs = IsoFs::empty(Box::new(shared.handle()), info);
        fs.shared = Some(shared);
        fs.udf_src = Some(src);
        fs.has_udf = true;
        fs.writable = true;
        fs.edit = Some(Edit { nodes, dirty: false, tail, label, boot, udf: true, iso: false });
        Ok(fs)
    }
}
