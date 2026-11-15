//! Checking (and safely repairing) a FAT volume, like `fsck.fat`.
//!
//! What is checked: the boot sector, that the FAT copies agree, every
//! directory (its `.`/`..`, duplicate names, long-name entries left without
//! their short entry), every chain (in range, not broken, not shared by two
//! files, not looping, as long as the file's size needs), clusters marked
//! used that nothing reaches ("lost"), and FSInfo's free count.
//!
//! Repairs are made only when every error found is one whose fix is fully
//! determined: lost clusters are freed, chains longer than their file are
//! cut, `.`/`..` get the right clusters, orphaned long names are removed,
//! the FAT copies are made equal to the first, the free count is
//! recounted and the dirty bits are cleared. A cross-linked or broken chain,
//! or a file larger than its chain, needs a decision: nothing is changed.

use super::bpb::{FatType, Layout};
use super::dir::{self, Item};
use super::table::Fat;
use dix_core::{BlockDevice, CheckReport, MountLog, Result};

/// A problem found, with the fix if there is a safe one.
enum Fix {
    /// Free these clusters.
    Lost(Vec<u32>),
    /// Cut a file's chain to `keep` clusters (its entry at `entry`).
    Truncate { entry: u64, first: u32, keep: u32 },
    /// Write `cluster` into the `.` or `..` entry at `slot`.
    Dot { slot: u64, cluster: u32 },
    /// Mark orphaned long-name entries deleted.
    Orphans(Vec<u64>),
    /// Make every FAT copy equal to the first.
    Mirror,
    /// FSInfo's free count.
    FreeCount,
    /// Clear the dirty bits.
    Clean,
}

#[derive(Default)]
struct Findings {
    errors: Vec<String>,
    warnings: Vec<String>,
    /// Errors without a safe fix.
    hard: usize,
    fixes: Vec<Fix>,
    files: u64,
    dirs: u64,
    used: u64,
}

impl Findings {
    fn error(&mut self, log: &MountLog, msg: String, fix: Option<Fix>) {
        log.error(&msg);
        self.errors.push(msg);
        match fix {
            Some(f) => self.fixes.push(f),
            None => self.hard += 1,
        }
    }

    fn warn(&mut self, log: &MountLog, msg: String, fix: Option<Fix>) {
        log.warn(&msg);
        self.warnings.push(msg);
        self.fixes.extend(fix);
    }
}

/// A directory still to check: (path, its clusters' bytes, its cluster,
/// its parent's cluster; 0 for the root).
type Pending = (String, Vec<(u64, Vec<u8>)>, u32, u32);

struct Bits(Vec<u64>);

impl Bits {
    fn new(n: usize) -> Self {
        Bits(vec![0; n.div_ceil(64)])
    }
    fn get(&self, i: u32) -> bool {
        self.0[i as usize / 64] & (1 << (i % 64)) != 0
    }
    fn set(&mut self, i: u32) {
        self.0[i as usize / 64] |= 1 << (i % 64);
    }
}

struct Scan<'a> {
    dev: &'a mut dyn BlockDevice,
    l: Layout,
    fat: Fat,
    used: Bits,
    f: Findings,
    log: &'a MountLog,
}

impl Scan<'_> {
    /// Follows a chain, marking its clusters; its clusters (as far as it
    /// is sound) and whether it is.
    fn chain(&mut self, start: u32, what: &str) -> Result<(Vec<u32>, bool)> {
        let mut out = Vec::new();
        let mut c = start;
        loop {
            if c < 2 || c > self.l.max_cluster() {
                let msg = format!("{what}: its chain points outside the volume (cluster {c})");
                self.f.error(self.log, msg, None);
                return Ok((out, false));
            }
            if self.used.get(c) {
                let msg = format!("{what}: cluster {c} also belongs to another file (cross-linked, or a loop)");
                self.f.error(self.log, msg, None);
                return Ok((out, false));
            }
            self.used.set(c);
            out.push(c);
            let next = self.fat.get(self.dev, c)?;
            if self.fat.is_eoc(next) {
                return Ok((out, true));
            }
            if next == 0 || self.fat.is_bad(next) {
                let msg = format!("{what}: its chain is broken after cluster {c}");
                self.f.error(self.log, msg, None);
                return Ok((out, false));
            }
            c = next;
        }
    }

    fn read_clusters(&mut self, clusters: &[u32]) -> Result<Vec<(u64, Vec<u8>)>> {
        let cs = self.l.cluster_size as usize;
        let mut out = Vec::new();
        for &c in clusters {
            let at = self.l.cluster_offset(c);
            let mut v = vec![0u8; cs];
            self.dev.read_at(at, &mut v)?;
            out.push((at, v));
        }
        Ok(out)
    }

    /// One directory: its entries, `.`/`..`, long names; queues children.
    fn directory(
        &mut self,
        path: &str,
        chunks: Vec<(u64, Vec<u8>)>,
        me: u32,
        parent: u32,
        queue: &mut Vec<Pending>,
    ) -> Result<()> {
        let slots: Vec<(u64, [u8; 32])> = chunks
            .iter()
            .flat_map(|(at, v)| {
                v.chunks_exact(32).enumerate().map(move |(i, e)| (at + i as u64 * 32, e.try_into().unwrap()))
            })
            .collect();
        // `.` and `..` (not in the root).
        if me != 0 {
            for (k, (want_name, want)) in [(b".          ", me), (b"..         ", parent)].iter().enumerate() {
                match slots.get(k) {
                    Some((at, e)) if &e[..11] == *want_name => {
                        let c = (dir::le16(e, 20) as u32) << 16 | dir::le16(e, 26) as u32;
                        if c != *want {
                            let msg = format!(
                                "{path}: its '{}' entry points at cluster {c}, not {want}",
                                if k == 0 { "." } else { ".." }
                            );
                            self.f.error(self.log, msg, Some(Fix::Dot { slot: *at, cluster: *want }));
                        }
                    }
                    _ => {
                        let msg = format!("{path}: its '{}' entry is missing", if k == 0 { "." } else { ".." });
                        self.f.error(self.log, msg, None);
                    }
                }
            }
        }
        // Long-name entries without their short entry.
        let mut run: Vec<(u64, u8)> = Vec::new();
        let mut orphans = Vec::new();
        for (at, e) in &slots {
            if e[0] == 0 {
                break;
            }
            if e[0] == 0xE5 {
                orphans.extend(run.drain(..).map(|r| r.0));
                continue;
            }
            if e[11] & 0x3F == dir::ATTR_LFN {
                if e[0] & 0x40 != 0 && !run.is_empty() {
                    orphans.extend(run.drain(..).map(|r| r.0));
                }
                run.push((*at, e[13]));
                continue;
            }
            let short: [u8; 11] = e[..11].try_into().unwrap();
            if run.iter().any(|r| r.1 != dir::checksum(&short)) {
                orphans.extend(run.iter().map(|r| r.0));
            }
            run.clear();
        }
        orphans.extend(run.drain(..).map(|r| r.0));
        if !orphans.is_empty() {
            let msg = format!("{path}: {} long-name entries without their file", orphans.len());
            self.f.warn(self.log, msg, Some(Fix::Orphans(orphans)));
        }
        let (items, _) = dir::parse(slots.into_iter());
        let mut seen = std::collections::HashSet::new();
        for item in items {
            let name = format!("{}/{}", path.trim_end_matches('/'), item.name);
            if !seen.insert(item.name.to_lowercase()) {
                self.f.error(self.log, format!("{name}: the name appears twice"), None);
            }
            if item.is_dir() {
                self.f.dirs += 1;
                if item.cluster() == 0 {
                    self.f.error(self.log, format!("{name}: a folder without clusters"), None);
                    continue;
                }
                let (clusters, sound) = self.chain(item.cluster(), &name)?;
                if sound {
                    let chunks = self.read_clusters(&clusters)?;
                    queue.push((name, chunks, item.cluster(), me));
                }
            } else {
                self.f.files += 1;
                self.file(&name, &item)?;
            }
        }
        Ok(())
    }

    fn file(&mut self, name: &str, item: &Item) -> Result<()> {
        let need = (item.size() as u64).div_ceil(self.l.cluster_size as u64);
        let first = item.cluster();
        if first == 0 {
            if need > 0 {
                self.f.error(self.log, format!("{name}: {} bytes but no clusters", item.size()), None);
            }
            return Ok(());
        }
        let (clusters, sound) = self.chain(first, name)?;
        if !sound {
            return Ok(());
        }
        let have = clusters.len() as u64;
        if have < need {
            let msg = format!("{name}: {} bytes, but its chain holds only {have} clusters", item.size());
            self.f.error(self.log, msg, None);
        } else if have > need {
            let msg = format!("{name}: its chain has {have} clusters, {need} needed for {} bytes", item.size());
            let fix = Fix::Truncate { entry: item.offset, first, keep: need as u32 };
            self.f.error(self.log, msg, Some(fix));
        }
        Ok(())
    }
}

fn scan(dev: &mut dyn BlockDevice, log: &MountLog) -> Result<(Findings, Layout)> {
    let mut boot = [0u8; 512];
    dev.read_at(0, &mut boot)?;
    let l = Layout::parse(&boot)?;
    log.info(format!(
        "{}: {} clusters of {} bytes, {} FATs",
        l.fat_type.name(),
        l.clusters,
        l.cluster_size,
        l.num_fats
    ));
    let mut s =
        Scan { fat: Fat::new(&l), used: Bits::new(l.max_cluster() as usize + 1), f: Findings::default(), log, dev, l };
    if boot[510..512] != [0x55, 0xAA] {
        s.f.warn(log, "The boot sector has no 55 AA signature".into(), None);
    }
    // The FAT copies.
    log.info("Comparing the FAT copies");
    if s.l.active_fat.is_none() && s.l.num_fats > 1 {
        let copies = s.l.fat_copies();
        let bytes = s.l.fat_bytes();
        let mut differ = false;
        let mut at = 0;
        while at < bytes && !differ {
            let n = (bytes - at).min(1 << 20) as usize;
            let mut first = vec![0u8; n];
            s.dev.read_at(copies[0] + at, &mut first)?;
            for &c in &copies[1..] {
                let mut other = vec![0u8; n];
                s.dev.read_at(c + at, &mut other)?;
                differ |= other != first;
            }
            at += n as u64;
        }
        if differ {
            s.f.error(log, "The FAT copies differ".into(), Some(Fix::Mirror));
        }
    }
    let dirty = boot[s.l.state_offset] & 1 != 0 || s.fat.clean_bit(s.dev)? == Some(false);
    if dirty {
        s.f.warn(log, "The volume was not cleanly unmounted (dirty bit set)".into(), Some(Fix::Clean));
    }
    // Directories, from the root.
    log.info("Checking directories and cluster chains");
    let root_chunks = if s.l.fat_type == FatType::Fat32 {
        let (clusters, sound) = s.chain(s.l.root_cluster, "/")?;
        if !sound {
            return Ok((s.f, s.l));
        }
        s.read_clusters(&clusters)?
    } else {
        let mut v = vec![0u8; s.l.root_bytes as usize];
        s.dev.read_at(s.l.root_offset, &mut v)?;
        vec![(s.l.root_offset, v)]
    };
    let mut queue = vec![("/".to_owned(), root_chunks, 0u32, 0u32)];
    while let Some((path, chunks, me, parent)) = queue.pop() {
        s.directory(&path, chunks, me, parent, &mut queue)?;
    }
    // Lost clusters and the free count.
    log.info("Looking for lost clusters");
    let (mut lost, mut free) = (Vec::new(), 0u32);
    for c in 2..=s.l.max_cluster() {
        let v = s.fat.get(s.dev, c)?;
        if v == 0 {
            free += 1;
        } else if !s.used.get(c) && !s.fat.is_bad(v) {
            lost.push(c);
        }
    }
    s.f.used = (s.l.clusters - free) as u64;
    if !lost.is_empty() {
        let msg = format!("{} lost clusters (marked used, but in no file)", lost.len());
        s.f.error(log, msg, Some(Fix::Lost(lost)));
    }
    if let Some(sector) = s.l.fs_info {
        let mut info = [0u8; 512];
        s.dev.read_at(sector as u64 * s.l.bytes_per_sector as u64, &mut info)?;
        let stored = dir::le32(&info, 488);
        if dir::le32(&info, 0) == 0x4161_5252 && stored != u32::MAX && stored != free {
            let msg = format!("FSInfo says {stored} free clusters, there are {free}");
            s.f.warn(log, msg, Some(Fix::FreeCount));
        }
    }
    Ok((s.f, s.l))
}

fn apply(dev: &mut dyn BlockDevice, l: &Layout, fixes: Vec<Fix>, log: &MountLog) -> Result<u64> {
    let mut fat = Fat::new(l);
    let mut fixed = 0;
    let mut mirror = false;
    for fix in fixes {
        fixed += 1;
        match fix {
            Fix::Lost(clusters) => {
                for c in &clusters {
                    fat.set(dev, *c, 0)?;
                }
                log.info(format!("Freed {} lost clusters", clusters.len()));
            }
            Fix::Truncate { entry, first, keep } => {
                let chain = fat.chain(dev, first)?;
                if keep == 0 {
                    fat.free_chain(dev, first)?;
                    let mut e = [0u8; 32];
                    dev.read_at(entry, &mut e)?;
                    dir::set_cluster(&mut e, 0);
                    dev.write_at(entry, &e)?;
                } else {
                    fat.truncate_after(dev, chain[keep as usize - 1])?;
                }
                log.info(format!("Cut a chain to {keep} clusters"));
            }
            Fix::Dot { slot, cluster } => {
                let mut e = [0u8; 32];
                dev.read_at(slot, &mut e)?;
                dir::set_cluster(&mut e, cluster);
                dev.write_at(slot, &e)?;
                log.info("Fixed a '.' or '..' entry");
            }
            Fix::Orphans(offsets) => {
                for at in &offsets {
                    dev.write_at(*at, &[0xE5])?;
                }
                log.info(format!("Removed {} orphaned long-name entries", offsets.len()));
            }
            Fix::Mirror => mirror = true,
            Fix::FreeCount | Fix::Clean => {}
        }
    }
    fat.flush(dev)?;
    if mirror {
        // FAT #1 (now with the fixes above) over the others.
        let copies = l.fat_copies();
        let bytes = l.fat_bytes();
        let mut at = 0;
        while at < bytes {
            let n = (bytes - at).min(1 << 20) as usize;
            let mut v = vec![0u8; n];
            dev.read_at(copies[0] + at, &mut v)?;
            for &c in &copies[1..] {
                dev.write_at(c + at, &v)?;
            }
            at += n as u64;
        }
        log.info("Copied the first FAT over the others");
    }
    // Always: the free count, and clean.
    let mut fat = Fat::new(l);
    let free = fat.count_free(dev)?;
    if let Some(sector) = l.fs_info {
        let at = sector as u64 * l.bytes_per_sector as u64;
        let mut info = [0u8; 512];
        dev.read_at(at, &mut info)?;
        if dir::le32(&info, 0) == 0x4161_5252 {
            info[488..492].copy_from_slice(&free.to_le_bytes());
            dev.write_at(at, &info)?;
        }
    }
    let mut b = [0u8; 1];
    dev.read_at(l.state_offset as u64, &mut b)?;
    dev.write_at(l.state_offset as u64, &[b[0] & !1])?;
    fat.set_clean_bit(dev, true)?;
    fat.flush(dev)?;
    dev.flush()?;
    Ok(fixed)
}

pub fn check(mut dev: Box<dyn BlockDevice>, log: &MountLog, repair: bool) -> Result<CheckReport> {
    let (mut f, l) = scan(dev.as_mut(), log)?;
    let mut fixed = 0;
    if repair && !f.fixes.is_empty() {
        if f.hard > 0 {
            log.warn("Some problems need a decision (see above): nothing was changed.");
        } else {
            log.info("Repairing");
            fixed = apply(dev.as_mut(), &l, std::mem::take(&mut f.fixes), log)?;
            log.info("Checking again");
            f = scan(dev.as_mut(), log)?.0;
        }
    }
    let summary = format!("{} files, {} folders, {}/{} clusters used", f.files, f.dirs, f.used, l.clusters);
    log.info(&summary);
    Ok(CheckReport {
        errors: f.errors.len() as u64,
        warnings: f.warnings.len() as u64,
        summary,
        fixed,
        repairable: f.hard == 0 && !f.fixes.is_empty(),
    })
}
