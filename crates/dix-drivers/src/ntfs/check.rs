//! Checking an NTFS volume (read-only, like `chkdsk` without `/f`).
//!
//! What is checked: the boot sector against its backup at the end of the
//! volume; every MFT record the MFT bitmap marks in use (signature, update
//! sequence, in-use flag); `$MFTMirr` against the first records; every
//! cluster run (inside the volume, not shared by two attributes) against
//! `$Bitmap`; every directory index (readable, in order, entries naming
//! in-use records with the right sequence number and a matching
//! `$FILE_NAME`); that each file has as many index entries as names; and
//! records in use that no directory reaches. Nothing is repaired.

use super::index::Entry;
use super::record::*;
use super::{EXTEND, ROOT, Volume};
use dix_core::{BlockDevice, CheckReport, MountLog, Result};
use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};

struct Findings<'a> {
    log: &'a MountLog,
    errors: u64,
    warnings: u64,
}

impl Findings<'_> {
    fn error(&mut self, msg: String) {
        self.log.error(msg);
        self.errors += 1;
    }

    fn warn(&mut self, msg: String) {
        self.log.warn(msg);
        self.warnings += 1;
    }
}

fn bit(map: &[u8], i: u64) -> bool {
    map.get((i / 8) as usize).is_some_and(|b| b >> (i % 8) & 1 != 0)
}

pub fn check(mut dev: Box<dyn BlockDevice>, log: &MountLog, repair: bool) -> Result<CheckReport> {
    let mut f = Findings { log, errors: 0, warnings: 0 };
    if repair {
        log.info("NTFS volumes are only checked here, not repaired: use chkdsk /f on Windows (or ntfsfix).");
    }
    let mut boot = [0u8; 512];
    dev.read_at(0, &mut boot)?;
    let b = Boot::parse(&boot)?;
    // The backup boot sector: the last sector of the volume (just past the
    // sectors it counts).
    let backup_at = b.total_sectors * b.bytes_per_sector as u64;
    if backup_at + 512 <= dev.size() {
        let mut copy = [0u8; 512];
        dev.read_at(backup_at, &mut copy)?;
        if copy != boot {
            f.error("The backup boot sector differs from the boot sector.".into());
        }
    } else {
        f.warn("The backup boot sector is past the end of the image (is it cut short?).".into());
    }
    let mut v = match Volume::open(dev) {
        Ok(v) => v,
        Err(e) => {
            f.error(format!("The volume cannot be opened: {}", e.message()));
            return Ok(report(f, "the MFT is unreadable".into()));
        }
    };
    let total = b.total_clusters();
    let count = v.record_count();

    // The MFT bitmap: which records are in use.
    let mft = v.file(0)?;
    let mft_bitmap = match mft.attr(AT_BITMAP, "") {
        Some(a) => v.attr_vec(0, a, 1 << 30)?,
        None => {
            f.error("The MFT has no bitmap.".into());
            vec![0xFF; count.div_ceil(8) as usize]
        }
    };

    // Pass 1: every record, and every cluster its attributes use.
    let mut used = vec![0u8; total.div_ceil(8) as usize];
    let mut used_count = 0u64;
    let mut bases = Vec::new();
    let mut seqs: HashMap<u64, u16> = HashMap::new();
    for n in 0..count {
        log.progress(n, count);
        let in_bitmap = bit(&mft_bitmap, n);
        let mut raw = match v.raw_record(n) {
            Ok(r) => r,
            Err(e) => {
                f.error(format!("Record {n} cannot be read: {}", e.message()));
                continue;
            }
        };
        if !in_bitmap {
            continue;
        }
        if let Err(e) = fixup(&mut raw, b"FILE") {
            f.error(format!("Record {n} is marked in use but is damaged: {}", e.message()));
            continue;
        }
        let rec = match parse_record(&raw, n) {
            Ok(r) => r,
            Err(e) => {
                f.error(format!("Record {n}: {}", e.message()));
                continue;
            }
        };
        if !rec.in_use() {
            // mkntfs marks the reserved records 16–23 in use without using them.
            if !(16..24).contains(&n) {
                f.error(format!("Record {n} is marked in use in the MFT bitmap but is free."));
            }
            continue;
        }
        seqs.insert(n, rec.seq);
        if rec.base == 0 {
            bases.push(n);
        }
        for a in &rec.attrs {
            let Body::NonResident { runs, .. } = &a.body else { continue };
            for r in runs {
                let Some(lcn) = r.lcn else { continue };
                if lcn + r.len > total {
                    f.error(format!("Record {n}: a run ({} clusters at {lcn}) is past the end of the volume.", r.len));
                    continue;
                }
                let mut shared = 0u64;
                for c in lcn..lcn + r.len {
                    let (i, m) = ((c / 8) as usize, 1u8 << (c % 8));
                    if used[i] & m != 0 {
                        shared += 1;
                    } else {
                        used[i] |= m;
                        used_count += 1;
                    }
                }
                if shared > 0 {
                    f.error(format!("Record {n}: {shared} clusters near {lcn} are also used by another file."));
                }
            }
        }
    }
    log.clear_progress();

    // $MFTMirr: a copy of the first records.
    if let Ok(mirr) = v.file(1)
        && let Some(a) = mirr.attr(AT_DATA, "")
    {
        let rs = b.record_size as u64;
        let n = (super::attr_size(a) / rs).min(count);
        let mut copy = vec![0u8; rs as usize];
        for i in 0..n {
            let ok = v.attr_read(1, a, i * rs, &mut copy).ok() == Some(copy.len());
            if !ok || v.raw_record(i).ok().as_deref() != Some(&copy[..]) {
                f.error(format!("$MFTMirr does not match record {i} of the MFT."));
            }
        }
    } else {
        f.error("$MFTMirr is missing.".into());
    }

    // The cluster bitmap against what the records use.
    match v.file(6).ok().and_then(|bm| bm.attr(AT_DATA, "").cloned()) {
        Some(a) => {
            let map = v.attr_vec(6, &a, 1 << 32)?;
            let (mut unmarked, mut lost) = (0u64, 0u64);
            for c in 0..total {
                match (bit(&used, c), bit(&map, c)) {
                    (true, false) => unmarked += 1,
                    (false, true) => lost += 1,
                    _ => {}
                }
            }
            if unmarked > 0 {
                f.error(format!("{unmarked} clusters in use are marked free in $Bitmap."));
            }
            if lost > 0 {
                f.error(format!("{lost} clusters are marked in use in $Bitmap but nothing uses them."));
            }
        }
        None => f.error("$Bitmap has no data.".into()),
    }

    // Pass 2: the directory tree.
    let (mut files, mut dirs) = (0u64, 0u64);
    let mut refs: HashMap<u64, u64> = HashMap::new();
    let mut seen_dirs = std::collections::HashSet::from([ROOT]);
    let mut queue = VecDeque::from([ROOT]);
    while let Some(d) = queue.pop_front() {
        dirs += 1;
        let dir = match v.file(d) {
            Ok(x) => x,
            Err(e) => {
                f.error(format!("Directory {d} cannot be read: {}", e.message()));
                continue;
            }
        };
        let entries = match v.dir_entries(&dir) {
            Ok(e) => e,
            Err(e) => {
                f.error(format!("The index of directory {d} is damaged: {}", e.message()));
                continue;
            }
        };
        let mut prev: Option<&Entry> = None;
        for e in &entries {
            if let Some(p) = prev {
                let order = v.upcase.cmp(&p.key.name, &e.key.name).then_with(|| p.key.name.cmp(&e.key.name));
                if order != Ordering::Less {
                    f.error(format!("Directory {d}: the index is out of order at \"{}\".", e.key.name()));
                }
            }
            prev = Some(e);
            let r = e.record();
            if r == d {
                continue; // "." in the root
            }
            if e.key.parent & 0x0000_FFFF_FFFF_FFFF != d {
                f.error(format!("Directory {d}: \"{}\" names another parent.", e.key.name()));
            }
            match seqs.get(&r) {
                None => {
                    f.error(format!("Directory {d}: \"{}\" points at record {r}, which is not in use.", e.key.name()));
                    continue;
                }
                Some(&s) if s != e.seq() && e.seq() != 0 => {
                    f.error(format!("Directory {d}: \"{}\" points at an older use of record {r}.", e.key.name()));
                    continue;
                }
                _ => {}
            }
            *refs.entry(r).or_default() += 1;
            let Ok(file) = v.file(r) else {
                f.error(format!("Directory {d}: \"{}\" (record {r}) cannot be read.", e.key.name()));
                continue;
            };
            if !file.names().any(|n| n.name == e.key.name && n.parent & 0x0000_FFFF_FFFF_FFFF == d) {
                f.error(format!("Directory {d}: \"{}\" is not one of record {r}'s names.", e.key.name()));
            }
            if file.rec.is_dir() {
                if seen_dirs.insert(r) {
                    queue.push_back(r);
                }
            } else if e.key.namespace != 2 {
                files += 1;
            }
        }
    }
    for &n in &bases {
        let Ok(file) = v.file(n) else { continue };
        let names = file.names().count() as u64;
        let found = refs.get(&n).copied().unwrap_or(0);
        if n >= 24 && found == 0 {
            f.error(format!("Record {n} is in use but no directory names it (an orphan)."));
        } else if n != ROOT && found != names && n != EXTEND {
            f.error(format!("Record {n} has {names} names but {found} directory entries."));
        }
    }

    if let Some(info) = v.volume_info()
        && info.dirty
    {
        f.warn("The volume is marked dirty (Windows wants it checked).".into());
    }
    let summary = format!("{files} files, {dirs} directories, {used_count}/{total} clusters used");
    Ok(report(f, summary))
}

fn report(f: Findings, summary: String) -> CheckReport {
    CheckReport { errors: f.errors, warnings: f.warnings, summary, fixed: 0, repairable: false }
}
