//! Editing the partition table of a disk or disk image (native only).
//!
//! A [`DiskEditor`] holds the table as read and the table as edited; every
//! change is made in memory and listed for the user. [`DiskEditor::apply`]
//! then writes the new table and formats the partitions asked for, in one
//! go, on a background thread. Before that, [`Workspace::disk_blockers`]
//! says what would make writing unsafe: volumes of the disk open in the
//! app, or partitions the operating system has mounted.

#[cfg(not(target_arch = "wasm32"))]
use super::Workspace;
use dix_core::partition::edit::{type_for_file_system, Free, Layout, TypeCode};
use dix_core::partition::Scheme;
#[cfg(not(target_arch = "wasm32"))]
use dix_core::AccessMode;
use dix_core::{BlockDevice, DriverRegistry, Error, FormatOptions, Result};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// A file system to create in a partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatPlan {
    pub driver: String,
    pub variant: Option<String>,
    pub label: String,
    /// Display name ("ext4", "FAT32").
    pub name: String,
}

/// A file system a driver can create (for the format choices).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Formatter {
    pub driver: String,
    pub variant: Option<String>,
    pub name: String,
    pub min_size: u64,
}

/// What the drivers of `registry` can create in a partition (not archives).
pub fn formatters(registry: &DriverRegistry) -> Vec<Formatter> {
    let mut out = Vec::new();
    for d in registry.formatters() {
        let info = d.info();
        let variants = d.format_variants();
        if variants.is_empty() {
            out.push(Formatter {
                driver: info.id.into(),
                variant: None,
                name: info.name.into(),
                min_size: d.min_format_size(),
            });
        }
        for v in variants.iter().filter(|v| v.sized) {
            out.push(Formatter {
                driver: info.id.into(),
                variant: Some(v.id.into()),
                name: v.name.into(),
                min_size: v.min_size,
            });
        }
    }
    out
}

#[derive(Clone)]
pub struct DiskEditor {
    pub path: PathBuf,
    pub size: u64,
    /// The device's logical sector size.
    pub sector_size: u64,
    pub original: Option<Layout>,
    pub layout: Option<Layout>,
    /// File systems found in the partitions as read, by first sector.
    pub found: HashMap<u64, String>,
    /// File systems to create once the table is written, by first sector.
    pub formats: BTreeMap<u64, FormatPlan>,
    /// The changes made, in words, in order.
    pub changes: Vec<String>,
}

impl DiskEditor {
    /// Reads the disk or image at `path` (read-only; the app reads through
    /// [`Workspace::host_reader`]).
    #[cfg(all(test, unix, feature = "ext4", feature = "fat"))]
    pub fn open(registry: &DriverRegistry, path: &Path) -> Result<DiskEditor> {
        Self::read(registry, path, dix_core::device::host::open_disk(registry, path, AccessMode::ReadOnly)?)
    }

    /// Reads the table through `dev` (the disk or image at `path`).
    pub fn read(registry: &DriverRegistry, path: &Path, mut dev: Box<dyn BlockDevice>) -> Result<DiskEditor> {
        let size = dev.size();
        let sector_size = (dev.info().sector_size as u64).clamp(512, 4096);
        let original = Layout::read(dev.as_mut())?;
        let mut found = HashMap::new();
        if let Some(scan) = dix_core::partition::scan(registry, dev.as_mut())? {
            let ss = scan.table.sector_size as u64;
            for (p, d) in scan.table.partitions.iter().zip(scan.drivers) {
                if let Some(d) = d {
                    found.insert(p.start / ss, d);
                }
            }
        }
        Ok(DiskEditor {
            path: path.to_owned(),
            size,
            sector_size,
            layout: original.clone(),
            original,
            found,
            formats: BTreeMap::new(),
            changes: Vec::new(),
        })
    }

    pub fn changed(&self) -> bool {
        self.layout != self.original || !self.formats.is_empty()
    }

    pub fn table_changed(&self) -> bool {
        self.layout != self.original
    }

    /// Forgets every change.
    pub fn reset(&mut self) {
        self.layout = self.original.clone();
        self.formats.clear();
        self.changes.clear();
    }

    fn layout_mut(&mut self) -> Result<&mut Layout> {
        self.layout.as_mut().ok_or_else(|| Error::invalid_input("the disk has no partition table"))
    }

    /// The file system shown for a partition: the one to be created, else
    /// the one found (unless the partition is new).
    pub fn file_system(&self, first: u64) -> Option<String> {
        if let Some(f) = self.formats.get(&first) {
            return Some(format!("{} (new)", f.name));
        }
        let new = self.layout.as_ref()?.slots.iter().any(|s| s.first == first && s.new);
        (!new).then(|| self.found.get(&first).cloned()).flatten()
    }

    /// Replaces the partition table with an empty one.
    pub fn new_table(&mut self, scheme: Scheme) -> Result<()> {
        self.layout = Some(Layout::new(scheme, self.size, self.sector_size)?);
        self.formats.clear();
        self.changes.push(format!("Create a new, empty {} partition table", scheme.label()));
        Ok(())
    }

    /// Creates a partition of `bytes` at the start of `free`, of `type_code`
    /// (or, with a format and no type, the type that suits it).
    pub fn add(
        &mut self,
        free: Free,
        bytes: u64,
        type_code: Option<TypeCode>,
        name: &str,
        format: Option<FormatPlan>,
        formatters: &[Formatter],
    ) -> Result<u32> {
        let layout = self.layout.as_ref().ok_or_else(|| Error::invalid_input("the disk has no partition table"))?;
        let ss = layout.sector_size;
        let sectors = bytes.div_ceil(ss).clamp(1, free.sectors());
        let last = free.first + sectors - 1;
        let scheme = layout.scheme;
        let type_code = match (type_code, &format) {
            (Some(t), _) => t,
            (None, Some(f)) => type_for_file_system(f.variant.as_deref().unwrap_or(&f.driver), scheme)
                .unwrap_or_else(|| dix_core::partition::edit::KNOWN_TYPES[0].code(scheme).unwrap()),
            (None, None) => dix_core::partition::edit::KNOWN_TYPES[0].code(scheme).unwrap(),
        };
        if let Some(f) = &format {
            check_fits(f, sectors * ss, formatters)?;
        }
        let number = self.layout_mut()?.add(free.first, last, type_code, name)?;
        let mut what =
            format!("Create partition {number}: {}, {}", type_code.name(), crate::ui::format_size(sectors * ss));
        if let Some(f) = format {
            what.push_str(&format!(", formatted {}", f.name));
            self.formats.insert(free.first, f);
        }
        self.changes.push(what);
        Ok(number)
    }

    /// Adds a partition where a lost file system was found (see
    /// `dix_drivers::recover::partitions`): exactly its sectors, the type
    /// that suits it, its label as the GPT name.
    pub fn restore(&mut self, lost: &dix_drivers::recover::partitions::Lost) -> Result<u32> {
        let layout = self.layout.as_ref().ok_or_else(|| {
            Error::invalid_input("the disk has no partition table: make one first (New table), then restore")
        })?;
        let (ss, scheme) = (layout.sector_size, layout.scheme);
        if !lost.start.is_multiple_of(ss) {
            return Err(Error::invalid_input(format!("it does not start on a sector boundary ({ss}-byte sectors)")));
        }
        let first = lost.start / ss;
        let last = (lost.start + lost.size).div_ceil(ss) - 1;
        let type_code = dix_core::partition::edit::KNOWN_TYPES
            .iter()
            .find(|t| t.name == lost.type_name)
            .and_then(|t| t.code(scheme))
            .unwrap_or_else(|| dix_core::partition::edit::KNOWN_TYPES[0].code(scheme).unwrap());
        let name = lost.label.clone().unwrap_or_default();
        let number = self.layout_mut()?.add(first, last, type_code, &name)?;
        // It is not new: its file system is shown, not formatted.
        if let Some(slot) = self.layout.as_mut().and_then(|l| l.slots.iter_mut().find(|s| s.first == first)) {
            slot.new = false;
        }
        self.found.insert(first, lost.kind.to_owned());
        let label = lost.label.as_deref().map(|l| format!(" “{l}”")).unwrap_or_default();
        self.changes.push(format!(
            "Restore partition {number}: {}{label}, {} at {}",
            lost.kind,
            crate::ui::format_size(lost.size),
            crate::ui::format_size(lost.start)
        ));
        Ok(number)
    }

    pub fn delete(&mut self, number: u32) -> Result<()> {
        let layout = self.layout_mut()?;
        let s = layout.get(number).cloned().ok_or_else(|| Error::not_found(format!("partition {number}")))?;
        let logical: Vec<u64> = if s.is_extended() {
            layout.slots.iter().filter(|s| s.is_logical()).map(|s| s.first).collect()
        } else {
            vec![]
        };
        layout.delete(number)?;
        self.formats.remove(&s.first);
        for f in logical {
            self.formats.remove(&f);
        }
        self.changes.push(if s.is_extended() {
            format!("Delete the extended partition {number} with its logical partitions")
        } else {
            format!("Delete partition {number} ({})", s.type_code.name())
        });
        Ok(())
    }

    pub fn set_type(&mut self, number: u32, t: TypeCode) -> Result<()> {
        self.layout_mut()?.set_type(number, t)?;
        self.changes.push(format!("Partition {number}: type {}", t.name()));
        Ok(())
    }

    pub fn set_name(&mut self, number: u32, name: &str) -> Result<()> {
        self.layout_mut()?.set_name(number, name)?;
        self.changes.push(format!("Partition {number}: name “{name}”"));
        Ok(())
    }

    pub fn set_bootable(&mut self, number: u32, on: bool) -> Result<()> {
        self.layout_mut()?.set_bootable(number, on)?;
        let what = if on { "bootable" } else { "not bootable" };
        self.changes.push(format!("Partition {number}: {what}"));
        Ok(())
    }

    /// Creates a file system in a partition (erasing what it holds).
    pub fn format(&mut self, number: u32, plan: FormatPlan, formatters: &[Formatter]) -> Result<()> {
        let layout = self.layout.as_ref().ok_or_else(|| Error::invalid_input("the disk has no partition table"))?;
        let s = layout.get(number).ok_or_else(|| Error::not_found(format!("partition {number}")))?;
        if s.is_extended() {
            return Err(Error::invalid_input("an extended partition only holds other partitions"));
        }
        check_fits(&plan, s.sectors() * layout.sector_size, formatters)?;
        self.changes.push(format!("Format partition {number} as {}", plan.name));
        self.formats.insert(s.first, plan);
        Ok(())
    }

    /// Writes the new table and creates the file systems. Runs off the UI
    /// thread; returns notes for the user (e.g. the OS did not reload the
    /// table). Check [`Workspace::disk_blockers`] first.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn apply(&self, registry: &DriverRegistry) -> Result<Vec<String>> {
        let mut dev = dix_core::device::host::open_disk(registry, &self.path, AccessMode::ReadWrite)?;
        self.apply_to(registry, dev.as_mut())?;
        drop(dev);
        let path = self.path.to_string_lossy();
        Ok(dix_core::device::host::reread_partition_table(&path).into_iter().collect())
    }

    /// Writes the new table and creates the file systems on `dev` (the
    /// disk, or an image in memory), then flushes it.
    pub fn apply_to(&self, registry: &DriverRegistry, dev: &mut dyn BlockDevice) -> Result<()> {
        let layout = self.layout.as_ref().ok_or_else(|| Error::invalid_input("the disk has no partition table"))?;
        layout.validate()?;
        if self.table_changed() {
            layout.write(&mut *dev)?;
        }
        let ss = layout.sector_size;
        for (first, plan) in &self.formats {
            let s = layout.slots.iter().find(|s| s.first == *first).expect("formats follow the layout");
            let name = format!("partition {}", s.number);
            let mut part = dix_core::device::SliceDevice::new(&mut *dev, first * ss, s.sectors() * ss, name)?;
            let label = Some(plan.label.trim().to_owned()).filter(|l| !l.is_empty());
            let opts = FormatOptions { label, variant: plan.variant.clone() };
            registry
                .format(&mut part, &plan.driver, &opts)
                .map_err(|e| e.with_context(format!("formatting partition {}", s.number)))?;
        }
        dev.flush()
    }

    /// [`apply_to`](Self::apply_to) an image in memory (an upload): the
    /// image's new bytes. VM disks and DMGs are written through their
    /// format.
    pub fn apply_to_bytes(&self, registry: &DriverRegistry, name: &str, bytes: Vec<u8>) -> Result<Vec<u8>> {
        let mem = dix_core::device::MemoryDevice::new(name, bytes);
        let buffer = mem.buffer();
        let mut dev = registry.open_disk(Box::new(mem), None)?;
        self.apply_to(registry, dev.as_mut())?;
        drop(dev);
        Ok(buffer.snapshot())
    }
}

fn check_fits(plan: &FormatPlan, bytes: u64, formatters: &[Formatter]) -> Result<()> {
    let min =
        formatters.iter().find(|f| f.driver == plan.driver && f.variant == plan.variant).map_or(0, |f| f.min_size);
    if bytes < min {
        return Err(Error::invalid_input(format!("{} needs at least {}", plan.name, crate::ui::format_size(min))));
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
impl Workspace {
    /// Why the partition table of `path` must not be rewritten now: volumes
    /// of it open here, partitions the OS uses. Empty: go ahead.
    pub fn disk_blockers(&self, path: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
        let target = canon(path);
        for v in self.volumes.values() {
            if v.buffer.is_none() && canon(Path::new(&v.source.location)) == target {
                out.push(format!("{} is open here: eject it first", v.name));
            }
        }
        if self.mounting.iter().any(|m| m.buffer.is_none() && canon(Path::new(&m.info.name)) == target) {
            out.push("a volume of this disk is being opened".into());
        }
        for u in dix_core::device::host::os_uses(&path.to_string_lossy()) {
            out.push(format!("{u}: the system uses it (unmount it first)"));
        }
        out
    }
}

/// Writes a partition table with one partition over the whole of `dev`,
/// formatted by `driver` (new disk images). Returns the partition.
pub fn partitioned_image(
    registry: &DriverRegistry,
    dev: &mut dyn BlockDevice,
    scheme: Scheme,
    driver: &str,
    opts: &FormatOptions,
) -> Result<dix_core::partition::Partition> {
    let mut layout = Layout::new(scheme, dev.size(), 512)?;
    let free = layout.free()[0];
    let kind = opts.variant.as_deref().unwrap_or(driver);
    let t = type_for_file_system(kind, scheme)
        .unwrap_or_else(|| dix_core::partition::edit::KNOWN_TYPES[0].code(scheme).unwrap());
    layout.add(free.first, free.last, t, "")?;
    layout.write(dev)?;
    let p = layout.partitions().remove(0);
    let mut part = dix_core::device::SliceDevice::new(&mut *dev, p.start, p.size, "partition 1")?;
    registry.format(&mut part, driver, opts)?;
    dev.flush()?;
    Ok(p)
}

#[cfg(all(test, unix, feature = "ext4", feature = "fat"))]
mod tests {
    use super::*;
    use dix_core::partition::edit::KNOWN_TYPES;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let d = std::env::temp_dir().join(format!("dix-disk-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tool(name: &str) -> Option<PathBuf> {
        ["/usr/sbin", "/sbin", "/usr/bin", "/bin"].iter().map(|d| Path::new(d).join(name)).find(|p| p.exists())
    }

    /// Runs a checker on a copy of `len` bytes at `start` of `img`.
    fn fsck(img: &Path, start: u64, len: u64, cmd: &str, args: &[&str]) {
        let Some(t) = tool(cmd) else { return };
        let part = img.with_extension("part");
        let mut data = vec![0u8; len as usize];
        use std::os::unix::fs::FileExt;
        std::fs::File::open(img).unwrap().read_exact_at(&mut data, start).unwrap();
        std::fs::write(&part, data).unwrap();
        let out = std::process::Command::new(t).args(args).arg(&part).output().unwrap();
        assert!(
            out.status.success(),
            "{cmd}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::remove_file(part).unwrap();
    }

    /// Retries while another test's forked checker still holds the image's
    /// lock (inherited for a moment).
    fn retry<T>(mut f: impl FnMut() -> Result<T>) -> T {
        for _ in 0..100 {
            match f() {
                Err(e) if e.kind() == dix_core::ErrorKind::InUse => {
                    std::thread::sleep(std::time::Duration::from_millis(50))
                }
                r => return r.unwrap(),
            }
        }
        f().unwrap()
    }

    fn plan(fmts: &[Formatter], variant: &str, label: &str) -> FormatPlan {
        let f = fmts.iter().find(|f| f.variant.as_deref() == Some(variant)).unwrap();
        FormatPlan { driver: f.driver.clone(), variant: f.variant.clone(), label: label.into(), name: f.name.clone() }
    }

    #[test]
    fn partition_format_and_open() {
        let s = Scratch::new("apply");
        let img = s.0.join("disk.img");
        std::fs::File::create(&img).unwrap().set_len(128 << 20).unwrap();
        let mut ws = Workspace::new(dix_drivers::default_registry());
        let fmts = formatters(&ws.registry);
        let mut e = retry(|| DiskEditor::open(&ws.registry, &img));
        assert!(e.layout.is_none() && !e.changed());
        e.new_table(Scheme::Gpt).unwrap();
        let free = e.layout.as_ref().unwrap().free()[0];
        assert_eq!(e.add(free, 40 << 20, None, "root", Some(plan(&fmts, "ext4", "ROOT")), &fmts).unwrap(), 1);
        let free = e.layout.as_ref().unwrap().free()[0];
        e.add(free, u64::MAX, None, "data", Some(plan(&fmts, "fat32", "DATA")), &fmts).unwrap();
        // FAT32 needs more room than this: refused before anything is written.
        let tiny = e.layout.as_ref().unwrap().free();
        assert!(tiny.is_empty());
        assert_eq!(e.changes.len(), 3, "{:?}", e.changes);
        assert!(ws.disk_blockers(&img).is_empty());
        let notes = retry(|| e.apply(&ws.registry));
        assert!(notes.is_empty(), "{notes:?}");

        // Read back: the table, and the file systems in it.
        let e2 = retry(|| DiskEditor::open(&ws.registry, &img));
        let l = e2.layout.as_ref().unwrap();
        assert_eq!(l.slots.len(), 2);
        assert_eq!(l.slots[0].type_code, KNOWN_TYPES[0].code(Scheme::Gpt).unwrap());
        assert_eq!(l.slots[1].type_code.name(), "Microsoft basic data");
        assert_eq!(e2.file_system(l.slots[0].first).as_deref(), Some("ext2/3/4"));
        assert_eq!(e2.file_system(l.slots[1].first).as_deref(), Some("FAT / exFAT"));
        let ss = l.sector_size;
        let (a, b) = (&l.slots[0], &l.slots[1]);
        fsck(&img, a.first * ss, a.sectors() * ss, "e2fsck", &["-fn"]);
        fsck(&img, b.first * ss, b.sectors() * ss, "fsck.fat", &["-n"]);
        if let Some(t) = tool("sfdisk") {
            let out = std::process::Command::new(t).arg("--verify").arg(&img).output().unwrap();
            assert!(String::from_utf8_lossy(&out.stdout).contains("No errors detected"));
        }

        // Open partition 2 in the workspace: the disk is then in use.
        let p = ws.scan_host(&img).unwrap().unwrap().table.partitions.remove(1);
        let dev = retry(|| ws.open_host(&img, AccessMode::ReadWrite));
        let id = ws.mount_partition_for_test(dev, p, "disk.img");
        assert_eq!(ws.volumes[&id].info().label.as_deref(), Some("DATA"));
        let blockers = ws.disk_blockers(&img);
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        ws.close_volume(id).unwrap();
        assert!(ws.disk_blockers(&img).is_empty());
    }

    #[test]
    fn mbr_logical_partitions_through_the_editor() {
        let s = Scratch::new("mbr");
        let img = s.0.join("disk.img");
        std::fs::File::create(&img).unwrap().set_len(64 << 20).unwrap();
        let reg = dix_drivers::default_registry();
        let fmts = formatters(&reg);
        let mut e = retry(|| DiskEditor::open(&reg, &img));
        e.new_table(Scheme::Mbr).unwrap();
        let f = e.layout.as_ref().unwrap().free()[0];
        e.add(f, u64::MAX, Some(TypeCode::Mbr(0x0F)), "", None, &fmts).unwrap();
        let f = e.layout.as_ref().unwrap().free().into_iter().find(|f| f.logical).unwrap();
        assert_eq!(e.add(f, 20 << 20, None, "", Some(plan(&fmts, "fat16", "")), &fmts).unwrap(), 5);
        let f = e.layout.as_ref().unwrap().free().into_iter().find(|f| f.logical).unwrap();
        assert_eq!(e.add(f, u64::MAX, None, "", Some(plan(&fmts, "ext2", "")), &fmts).unwrap(), 6);
        assert!(e.set_bootable(1, true).is_err(), "an extended partition cannot be active");
        retry(|| e.apply(&reg));
        let e2 = retry(|| DiskEditor::open(&reg, &img));
        let l = e2.layout.unwrap();
        let types: Vec<_> = l.slots.iter().map(|s| (s.number, s.type_code)).collect();
        assert_eq!(types, [(1, TypeCode::Mbr(0x0F)), (5, TypeCode::Mbr(0x0E)), (6, TypeCode::Mbr(0x83))]);
        assert_eq!(e2.found.len(), 2);
        // Deleting the extended partition takes the logical ones with it.
        let mut e3 = retry(|| DiskEditor::open(&reg, &img));
        e3.delete(1).unwrap();
        assert!(e3.layout.as_ref().unwrap().slots.is_empty());
        assert!(e3.changes[0].contains("extended"));
    }

    #[test]
    fn new_disk_images_with_a_table() {
        let s = Scratch::new("new");
        let mut ws = Workspace::new(dix_drivers::default_registry());
        for (scheme, variant) in [(Scheme::Gpt, "ext4"), (Scheme::Mbr, "fat32")] {
            let path = s.0.join(format!("{variant}.img"));
            let driver = if variant == "ext4" { "ext4" } else { "fat" };
            let opts = FormatOptions { label: Some("NEW".into()), variant: Some(variant.into()) };
            let id = retry(|| ws.create_file_image(&path, 64 << 20, driver, opts.clone(), Some(scheme)));
            let v = &ws.volumes[&id];
            assert_eq!(v.name, format!("{variant}.img #1"));
            assert_eq!(v.source.partition, Some(1));
            let t = ws.scan_host(&path).unwrap().unwrap();
            assert_eq!((t.table.scheme, t.table.partitions.len()), (scheme, 1));
            assert_eq!(t.drivers[0].as_deref(), Some(v.driver.name));
            ws.close_volume(id).unwrap();
            // In memory too.
            let opts = FormatOptions { label: None, variant: Some(variant.into()) };
            let id = ws.create_memory_image("m.img".into(), 40 << 20, driver, opts, Some(scheme)).unwrap();
            assert_eq!(ws.volumes[&id].source.partition, Some(1));
            assert_eq!(ws.volumes[&id].buffer.as_ref().unwrap().len(), 40 << 20);
            ws.force_close_volume(id);
        }
    }

    /// The partition editor on images in memory (uploads, as in the
    /// browser): a blank disk and a qcow2 get a GPT with ext4 and FAT32
    /// partitions; the new bytes read back with both mounted.
    /// A disk whose GPT was wiped: the search finds both file systems,
    /// restoring them into a new table brings them back, files and labels
    /// intact (restored partitions are not wiped or formatted).
    #[test]
    fn restores_lost_partitions() {
        use dix_core::device::MemoryDevice;
        use dix_core::VfsPath;
        let ws = Workspace::new(dix_drivers::default_registry());
        let fmts = formatters(&ws.registry);
        let mut d = crate::ui::disk::DiskDialog::open_memory(&ws, "disk.img".into(), vec![0u8; 128 << 20]);
        let e = d.editor.as_mut().unwrap();
        e.new_table(Scheme::Gpt).unwrap();
        let free = e.layout.as_ref().unwrap().free()[0];
        e.add(free, 40 << 20, None, "root", Some(plan(&fmts, "ext4", "ROOT")), &fmts).unwrap();
        let free = e.layout.as_ref().unwrap().free()[0];
        e.add(free, 50 << 20, None, "data", Some(plan(&fmts, "fat32", "DATA")), &fmts).unwrap();
        let (n, data) = d.memory.take().unwrap();
        let mut data = d.editor.as_ref().unwrap().apply_to_bytes(&ws.registry, &n, data).unwrap();
        // A file in each.
        {
            let disk = ws.registry.open_disk(Box::new(MemoryDevice::new("d", data.clone())), None).unwrap();
            let mem = disk;
            let shared = dix_core::device::SharedDevice::new(mem);
            for (part, file) in [(1, "/in-ext4.txt"), (2, "/IN-FAT.TXT")] {
                let p = dix_core::partition::open_partition(Box::new(shared.handle()), part).unwrap();
                let mut m = ws.registry.mount(p, None, AccessMode::ReadWrite).unwrap();
                m.fs.write_file(&VfsPath::parse(file).unwrap(), file.as_bytes()).unwrap();
                m.fs.unmount().unwrap();
            }
            let mut all = vec![0u8; data.len()];
            shared.handle().read_at(0, &mut all).unwrap();
            data = all;
        }
        // The table wiped: both GPT copies (and the protective MBR).
        let end = data.len();
        data[..34 * 512].fill(0);
        data[end - 33 * 512..].fill(0);
        let mut d = crate::ui::disk::DiskDialog::open_memory(&ws, "disk.img".into(), data.clone());
        assert!(d.editor.as_ref().unwrap().layout.is_none(), "no table any more");
        let dev = d.device(&ws).unwrap();
        let lost = dix_drivers::recover::partitions::find(dev, &[(0, data.len() as u64)], &mut |_, _| true).unwrap();
        let kinds: Vec<_> = lost.iter().map(|l| (l.kind, l.label.as_deref())).collect();
        assert_eq!(kinds, [("ext4", Some("ROOT")), ("FAT32", Some("DATA"))]);
        let e = d.editor.as_mut().unwrap();
        e.new_table(Scheme::Gpt).unwrap();
        for l in &lost {
            e.restore(l).unwrap();
        }
        assert!(e.changes.iter().any(|c| c.starts_with("Restore partition 1: ext4")), "{:?}", e.changes);
        let (n, bytes) = d.memory.take().unwrap();
        let fixed = d.editor.as_ref().unwrap().apply_to_bytes(&ws.registry, &n, bytes).unwrap();
        let disk = dix_core::device::SharedDevice::new(
            ws.registry.open_disk(Box::new(MemoryDevice::new("d", fixed)), None).unwrap(),
        );
        for (part, file, label) in [(1, "/in-ext4.txt", "ROOT"), (2, "/IN-FAT.TXT", "DATA")] {
            let p = dix_core::partition::open_partition(Box::new(disk.handle()), part).unwrap();
            let mut m = ws.registry.mount(p, None, AccessMode::ReadOnly).unwrap();
            assert_eq!(m.fs.read_to_vec(&VfsPath::parse(file).unwrap()).unwrap(), file.as_bytes(), "{file}");
            assert_eq!(m.fs.info().label.as_deref(), Some(label));
        }
    }

    #[test]
    fn partitions_of_images_in_memory() {
        let ws = Workspace::new(dix_drivers::default_registry());
        let fmts = formatters(&ws.registry);
        let qcow2 = {
            let f = ws.registry.formats().iter().find(|f| f.name() == "qcow2").unwrap().clone();
            let mut m = dix_core::device::MemoryDevice::new("d.qcow2", Vec::new());
            f.create(&mut m, 128 << 20, "d.qcow2").unwrap();
            m.buffer().snapshot()
        };
        for (name, bytes) in [("blank.img", vec![0u8; 128 << 20]), ("d.qcow2", qcow2)] {
            let mut d = crate::ui::disk::DiskDialog::open_memory(&ws, name.into(), bytes);
            let e = d.editor.as_mut().unwrap();
            assert!(e.layout.is_none());
            e.new_table(Scheme::Gpt).unwrap();
            let free = e.layout.as_ref().unwrap().free()[0];
            e.add(free, 40 << 20, None, "root", Some(plan(&fmts, "ext4", "ROOT")), &fmts).unwrap();
            let free = e.layout.as_ref().unwrap().free()[0];
            e.add(free, 70 << 20, None, "data", Some(plan(&fmts, "fat32", "DATA")), &fmts).unwrap();
            let (n, data) = d.memory.take().unwrap();
            let data = d.editor.as_ref().unwrap().apply_to_bytes(&ws.registry, &n, data).unwrap();
            if name.ends_with("qcow2") {
                assert!(data.starts_with(b"QFI\xfb") && data.len() < 40 << 20, "still a (small) qcow2");
            }
            d.memory = Some((n, data.clone()));
            let again = d.read(&ws).unwrap();
            assert_eq!(again.layout.as_ref().unwrap().partitions().len(), 2, "{name}");
            let disk = ws.registry.open_disk(Box::new(dix_core::device::MemoryDevice::new(name, data)), None).unwrap();
            let disk = dix_core::device::SharedDevice::new(disk);
            for (part, fs) in [(1, "ext4"), (2, "FAT32")] {
                let p = dix_core::partition::open_partition(Box::new(disk.handle()), part).unwrap();
                let m = ws.registry.mount(p, None, AccessMode::ReadOnly).unwrap();
                assert_eq!(m.fs.info().fs_name, fs, "{name} #{part}");
            }
        }
    }
}
