//! Checking (and safely repairing) an exFAT volume, like `fsck.exfat`.
//!
//! Checked: the boot region checksum and its backup, the up-case table's
//! checksum, every entry set (checksum, name hash, sizes), every chain (in
//! range, not broken, not shared, as long as the size needs), and the
//! allocation bitmap against what the files really use.
//!
//! Repaired, when every error found has a determined fix: bitmap bits
//! that disagree with the files are set or cleared, FAT chains longer than
//! their file are cut, the backup boot region is restored from the main
//! one, the dirty flag is cleared. Damaged entry sets, shared or broken
//! chains, or files larger than their clusters are left alone.

use super::{Boot, Set, boot_checksum, le32, parse, upcase};
use crate::fat::table::Fat;
use dix_core::{BlockDevice, CheckReport, MountLog, Result};

enum Fix {
    /// Set (`true`) or clear bitmap bits for these clusters.
    Bitmap(Vec<u32>, bool),
    /// Cut a FAT chain to `keep` clusters.
    Truncate {
        first: u32,
        keep: u32,
    },
    BackupBoot,
    Clean,
}

#[derive(Default)]
struct Findings {
    errors: u64,
    warnings: u64,
    hard: u64,
    fixes: Vec<Fix>,
    files: u64,
    dirs: u64,
}

impl Findings {
    fn error(&mut self, log: &MountLog, msg: String, fix: Option<Fix>) {
        log.error(msg);
        self.errors += 1;
        match fix {
            Some(f) => self.fixes.push(f),
            None => self.hard += 1,
        }
    }

    fn warn(&mut self, log: &MountLog, msg: String, fix: Option<Fix>) {
        log.warn(msg);
        self.warnings += 1;
        self.fixes.extend(fix);
    }
}

struct Scan<'a> {
    dev: &'a mut dyn BlockDevice,
    boot: Boot,
    fat: Fat,
    used: Vec<bool>,
    f: Findings,
    log: &'a MountLog,
    up: Option<upcase::Upcase>,
}

impl Scan<'_> {
    /// Marks a chain's clusters used; its clusters (as far as sound).
    fn chain(&mut self, first: u32, nofat: bool, size: u64, what: &str) -> Result<Option<Vec<u32>>> {
        let cs = self.boot.cluster_size as u64;
        let max = self.boot.max_cluster();
        let mut out = Vec::new();
        if nofat {
            let n = size.div_ceil(cs);
            if first < 2 || first as u64 + n > max as u64 + 1 {
                self.f.error(self.log, format!("{what}: its clusters lie outside the volume"), None);
                return Ok(None);
            }
            for c in first..first + n as u32 {
                if self.used[c as usize] {
                    self.f.error(self.log, format!("{what}: cluster {c} also belongs to another file"), None);
                    return Ok(None);
                }
                self.used[c as usize] = true;
                out.push(c);
            }
            return Ok(Some(out));
        }
        let mut c = first;
        loop {
            if c < 2 || c > max {
                self.f.error(self.log, format!("{what}: its chain points outside the volume ({c})"), None);
                return Ok(None);
            }
            if self.used[c as usize] {
                let msg = format!("{what}: cluster {c} also belongs to another file (cross-linked, or a loop)");
                self.f.error(self.log, msg, None);
                return Ok(None);
            }
            self.used[c as usize] = true;
            out.push(c);
            let next = self.fat.get(self.dev, c)?;
            if self.fat.is_eoc(next) {
                return Ok(Some(out));
            }
            if next < 2 || self.fat.is_bad(next) {
                self.f.error(self.log, format!("{what}: its chain is broken after cluster {c}"), None);
                return Ok(None);
            }
            c = next;
        }
    }

    fn read(&mut self, clusters: &[u32], len: u64) -> Result<Vec<u8>> {
        let cs = self.boot.cluster_size as usize;
        let mut out = Vec::with_capacity(clusters.len() * cs);
        for &c in clusters {
            let mut v = vec![0u8; cs];
            self.dev.read_at(self.boot.cluster_offset(c), &mut v)?;
            out.extend_from_slice(&v);
        }
        out.truncate(len as usize);
        Ok(out)
    }

    fn directory(&mut self, path: &str, clusters: &[u32], queue: &mut Vec<(String, Vec<u32>)>) -> Result<()> {
        let cs = self.boot.cluster_size as u64;
        let mut slots = Vec::new();
        for &c in clusters {
            let at = self.boot.cluster_offset(c);
            let mut v = vec![0u8; cs as usize];
            self.dev.read_at(at, &mut v)?;
            slots.extend(
                v.chunks_exact(32).enumerate().map(|(i, e)| (at + i as u64 * 32, <[u8; 32]>::try_from(e).unwrap())),
            );
        }
        let listing = parse(&slots, self.up.as_ref());
        for (at, why) in &listing.damaged {
            self.f.error(self.log, format!("{path}: a damaged entry set at byte {at}: {why}"), None);
        }
        let mut seen = std::collections::HashSet::new();
        for s in &listing.sets {
            let name = format!("{}/{}", path.trim_end_matches('/'), s.name);
            let key =
                self.up.as_ref().map_or_else(|| s.name.to_uppercase().encode_utf16().collect(), |u| u.upper(&s.name));
            if !seen.insert(key) {
                self.f.error(self.log, format!("{name}: the name appears twice"), None);
            }
            self.set(&name, s, queue)?;
        }
        Ok(())
    }

    fn set(&mut self, name: &str, s: &Set, queue: &mut Vec<(String, Vec<u32>)>) -> Result<()> {
        let cs = self.boot.cluster_size as u64;
        if s.valid() > s.size() {
            let msg = format!("{name}: its valid data length ({}) is past its size ({})", s.valid(), s.size());
            self.f.error(self.log, msg, None);
        }
        if s.is_dir() {
            self.f.dirs += 1;
            if s.first() == 0 || s.size() == 0 || !s.size().is_multiple_of(cs) {
                self.f.error(self.log, format!("{name}: a folder with a bad size ({})", s.size()), None);
                return Ok(());
            }
        } else {
            self.f.files += 1;
        }
        if s.first() == 0 {
            if s.size() > 0 {
                self.f.error(self.log, format!("{name}: {} bytes but no clusters", s.size()), None);
            }
            return Ok(());
        }
        let Some(clusters) = self.chain(s.first(), s.nofat(), s.size(), name)? else { return Ok(()) };
        let need = s.size().div_ceil(cs);
        let have = clusters.len() as u64;
        if have < need {
            self.f.error(self.log, format!("{name}: {} bytes, but only {have} clusters", s.size()), None);
        } else if have > need {
            let msg = format!("{name}: its chain has {have} clusters, {need} needed for {} bytes", s.size());
            let fix = Fix::Truncate { first: s.first(), keep: need as u32 };
            self.f.error(self.log, msg, Some(fix));
            // What is cut is not the file's: do not count it as used.
            for &c in &clusters[need as usize..] {
                self.used[c as usize] = false;
            }
        }
        if s.is_dir() {
            queue.push((name.to_owned(), clusters));
        }
        Ok(())
    }
}

fn scan(dev: &mut dyn BlockDevice, log: &MountLog) -> Result<(Findings, Boot, (u32, u64))> {
    let mut b = [0u8; 512];
    dev.read_at(0, &mut b)?;
    let boot = Boot::parse(&b)?;
    let bps = boot.bytes_per_sector as usize;
    log.info(format!("exFAT: {} clusters of {} bytes", boot.clusters, boot.cluster_size));
    let mut f = Findings::default();
    // The boot region, its checksum and its backup.
    let mut region = vec![0u8; 24 * bps];
    dev.read_at(0, &mut region)?;
    let sum = boot_checksum(&region[..11 * bps]);
    if (0..bps / 4).any(|k| le32(&region, 11 * bps + 4 * k) != sum) {
        f.error(log, "The boot region checksum does not match".into(), None);
    }
    // The backup may differ only in VolumeFlags and PercentInUse.
    let mut main = region[..12 * bps].to_vec();
    let mut backup = region[12 * bps..].to_vec();
    for r in [&mut main, &mut backup] {
        r[106] = 0;
        r[107] = 0;
        r[112] = 0;
    }
    if main != backup {
        f.warn(log, "The backup boot region differs from the main one".into(), Some(Fix::BackupBoot));
    }
    if boot.flags & super::VOLUME_DIRTY != 0 {
        f.warn(log, "The volume was not cleanly unmounted (dirty flag set)".into(), Some(Fix::Clean));
    }
    let fat = Fat::exfat(boot.fat_offset, boot.fat_bytes, boot.max_cluster());
    let mut s = Scan { used: vec![false; boot.max_cluster() as usize + 1], fat, f, log, dev, boot, up: None };
    // The root, and in it the bitmap and the up-case table.
    log.info("Checking directories and cluster chains");
    let root_cluster = s.boot.root_cluster;
    let Some(root) = s.chain(root_cluster, false, 0, "/")? else {
        return Ok((s.f, s.boot, (0, 0)));
    };
    let first_slots = {
        let mut slots = Vec::new();
        let cs = s.boot.cluster_size as u64;
        for &c in &root {
            let at = s.boot.cluster_offset(c);
            let mut v = vec![0u8; cs as usize];
            s.dev.read_at(at, &mut v)?;
            slots.extend(
                v.chunks_exact(32).enumerate().map(|(i, e)| (at + i as u64 * 32, <[u8; 32]>::try_from(e).unwrap())),
            );
        }
        slots
    };
    let meta = parse(&first_slots, None);
    let mut bitmap_at = (0, 0);
    match meta.upcase {
        Some((ck, first, len)) => {
            if let Some(cl) = s.chain(first, false, len, "the up-case table")? {
                let bytes = s.read(&cl, len)?;
                if upcase::checksum(&bytes) != ck {
                    s.f.error(log, "The up-case table checksum does not match".into(), None);
                }
                s.up = Some(upcase::Upcase::parse(&bytes));
            }
        }
        None => s.f.error(log, "The root has no up-case table".into(), None),
    }
    match meta.bitmap {
        Some((first, len)) => {
            s.chain(first, false, len, "the allocation bitmap")?;
            bitmap_at = (first, len);
        }
        None => s.f.error(log, "The root has no allocation bitmap".into(), None),
    }
    let mut queue = vec![("/".to_owned(), root)];
    while let Some((path, clusters)) = queue.pop() {
        s.directory(&path, &clusters, &mut queue)?;
    }
    // The bitmap against what is used.
    if bitmap_at.0 != 0 {
        log.info("Comparing the allocation bitmap with the files");
        let clusters = s.fat.chain(s.dev, bitmap_at.0)?;
        let bitmap = s.read(&clusters, bitmap_at.1)?;
        let (mut unmarked, mut lost) = (Vec::new(), Vec::new());
        for c in 2..=s.boot.max_cluster() {
            let i = (c - 2) as usize;
            let marked = bitmap.get(i / 8).is_some_and(|b| b & (1 << (i % 8)) != 0);
            match (marked, s.used[c as usize]) {
                (false, true) => unmarked.push(c),
                (true, false) => lost.push(c),
                _ => {}
            }
        }
        if !unmarked.is_empty() {
            let msg = format!("{} clusters in use are marked free in the bitmap", unmarked.len());
            s.f.error(log, msg, Some(Fix::Bitmap(unmarked, true)));
        }
        if !lost.is_empty() {
            let msg = format!("{} clusters are marked used in the bitmap but in no file", lost.len());
            s.f.error(log, msg, Some(Fix::Bitmap(lost, false)));
        }
    }
    Ok((s.f, s.boot, bitmap_at))
}

fn apply(
    dev: &mut dyn BlockDevice,
    boot: &Boot,
    bitmap_at: (u32, u64),
    fixes: Vec<Fix>,
    log: &MountLog,
) -> Result<u64> {
    let mut fat = Fat::exfat(boot.fat_offset, boot.fat_bytes, boot.max_cluster());
    let clusters = fat.chain(dev, bitmap_at.0)?;
    let cs = boot.cluster_size as u64;
    let mut fixed = 0;
    for fix in fixes {
        fixed += 1;
        match fix {
            Fix::Bitmap(list, set) => {
                for c in &list {
                    let i = (*c - 2) as u64;
                    let (byte, bit) = (i / 8, i % 8);
                    let at = boot.cluster_offset(clusters[(byte / cs) as usize]) + byte % cs;
                    let mut b = [0u8; 1];
                    dev.read_at(at, &mut b)?;
                    b[0] = if set { b[0] | 1 << bit } else { b[0] & !(1 << bit) };
                    dev.write_at(at, &b)?;
                }
                log.info(format!("{} {} bitmap bits", if set { "Set" } else { "Cleared" }, list.len()));
            }
            Fix::Truncate { first, keep } => {
                let chain = fat.chain(dev, first)?;
                fat.truncate_after(dev, chain[keep as usize - 1])?;
                log.info(format!("Cut a chain to {keep} clusters"));
            }
            Fix::BackupBoot | Fix::Clean => {}
        }
    }
    fat.flush(dev)?;
    // Clean, and the backup boot region equal to the main one.
    let mut f = [0u8; 2];
    dev.read_at(106, &mut f)?;
    let flags = u16::from_le_bytes(f) & !super::VOLUME_DIRTY;
    dev.write_at(106, &flags.to_le_bytes())?;
    let bps = boot.bytes_per_sector as u64;
    let mut region = vec![0u8; 12 * bps as usize];
    dev.read_at(0, &mut region)?;
    dev.write_at(12 * bps, &region)?;
    dev.flush()?;
    Ok(fixed)
}

pub fn check(mut dev: Box<dyn BlockDevice>, log: &MountLog, repair: bool) -> Result<CheckReport> {
    let (mut f, boot, bitmap_at) = scan(dev.as_mut(), log)?;
    let mut fixed = 0;
    if repair && !f.fixes.is_empty() {
        if f.hard > 0 {
            log.warn("Some problems need a decision (see above): nothing was changed.");
        } else {
            log.info("Repairing");
            fixed = apply(dev.as_mut(), &boot, bitmap_at, std::mem::take(&mut f.fixes), log)?;
            log.info("Checking again");
            f = scan(dev.as_mut(), log)?.0;
        }
    }
    let summary = format!("{} files, {} folders, {} clusters", f.files, f.dirs, boot.clusters);
    log.info(&summary);
    Ok(CheckReport {
        errors: f.errors,
        warnings: f.warnings,
        summary,
        fixed,
        repairable: f.hard == 0 && !f.fixes.is_empty(),
    })
}
