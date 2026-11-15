//! Deleted files on NTFS.
//!
//! Deleting a file clears the in-use flag of its MFT record (and frees its
//! clusters in $Bitmap); the record keeps its attributes — names with the
//! parent folder, times, and the data: resident in the record (small
//! files, recovered whole) or as runs of clusters. Every record not in use
//! that still has a name is looked at; its path is rebuilt from the parent
//! references (a parent whose record has been reused for something else
//! leaves the file under "Lost files"). $Bitmap says which of its clusters
//! are in use again. Compressed files are read through the driver's own
//! decompression.

use super::index::FileName;
use super::record::*;
use super::{Volume, filetime};
use crate::recover::{Chance, Found, LOST, Recovered, Source};
use dix_core::{BlockDevice, Result};
use std::collections::HashMap;

/// Records before this are the volume's own ($MFT … reserved).
const FIRST_USER_RECORD: u64 = 24;
const ROOT: u64 = 5;

struct Data {
    v: Volume,
    /// (record, $DATA attribute) of each found file.
    files: Vec<(u64, Attr)>,
}

impl Source for Data {
    fn read(&mut self, id: usize, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let (rec, a) = &self.files[id];
        let mut done = 0;
        while done < buf.len() {
            let n = self.v.attr_read(*rec, a, offset + done as u64, &mut buf[done..])?;
            if n == 0 {
                break;
            }
            done += n;
        }
        buf[done..].fill(0);
        Ok(buf.len())
    }
}

/// What a record says about itself.
struct Rec {
    in_use: bool,
    dir: bool,
    seq: u16,
    /// Its name (not the DOS 8.3 alias) and parent reference.
    name: Option<(String, u64)>,
    attrs: Vec<Attr>,
}

pub fn scan(dev: Box<dyn BlockDevice>) -> Result<Recovered> {
    let mut v = Volume::open(dev)?;
    // Which clusters are in use now.
    let bitmap = {
        let f = v.file(6)?;
        let a = f.attr(AT_DATA, "").cloned().ok_or_else(|| dix_core::Error::corrupted("$Bitmap has no data"))?;
        v.attr_vec(6, &a, 1 << 32)?
    };
    let used = |lcn: u64| bitmap.get((lcn / 8) as usize).is_none_or(|b| b & (1 << (lcn % 8)) != 0);
    let count = v.record_count();
    let mut recs: HashMap<u64, Rec> = HashMap::new();
    let mut deleted = Vec::new();
    for n in FIRST_USER_RECORD..count {
        let Ok(r) = v.record(n) else { continue };
        if r.base != 0 {
            continue;
        }
        let r = read(&mut v, r);
        if !r.in_use && r.name.is_some() {
            deleted.push(n);
        }
        recs.insert(n, r);
    }
    // Live folders the deleted ones are in, read as needed.
    let mut path_of: HashMap<u64, Option<Vec<String>>> = HashMap::new();
    let mut found = Vec::new();
    let mut files = Vec::new();
    for n in deleted {
        let r = &recs[&n];
        let (name, parent) = r.name.clone().unwrap();
        let mut path = folder_path(&mut v, &mut recs, &mut path_of, parent, 0).unwrap_or_else(|| vec![LOST.to_owned()]);
        path.push(name);
        let r = &recs[&n];
        let (modified, created) = times(&r.attrs);
        if r.dir {
            found.push(Found { path, dir: true, size: 0, modified, created, chance: Chance::Folder, id: 0 });
            continue;
        }
        let Some(a) = r.attrs.iter().find(|a| a.ty == AT_DATA && a.name.is_empty()).cloned() else { continue };
        let size = super::attr_size(&a);
        let chance = match &a.body {
            Body::Resident(_) => Chance::Intact,
            Body::NonResident { runs, .. } => {
                let (mut free, mut reused) = (0u64, 0u64);
                for run in runs {
                    if let Some(lcn) = run.lcn {
                        for c in lcn..lcn + run.len {
                            if used(c) { reused += 1 } else { free += 1 }
                        }
                    }
                }
                if a.flags & FLAG_ENCRYPTED != 0 { Chance::Lost } else { Chance::of(free, reused) }
            }
        };
        found.push(Found { path, dir: false, size, modified, created, chance, id: files.len() });
        files.push((n, a));
    }
    Ok(Recovered::new("NTFS", found, Box::new(Data { v, files })))
}

/// A record's state, name and attributes (extension records merged).
fn read(v: &mut Volume, r: Record) -> Rec {
    let mut attrs = r.attrs.clone();
    if let Some(list) = r.attrs.iter().find(|a| a.ty == AT_ATTRIBUTE_LIST).cloned()
        && let Ok(bytes) = v.attr_vec(r.number, &list, 1 << 20)
    {
        let mut more = Vec::new();
        for (_, ext) in list_entries(&bytes) {
            if ext != r.number
                && let Ok(e) = v.record(ext)
                && e.base == r.number
            {
                more.extend(e.attrs);
            }
        }
        attrs.retain(|a| a.ty != AT_ATTRIBUTE_LIST);
        attrs.extend(more);
        attrs = merge(attrs);
    }
    let name = attrs
        .iter()
        .filter(|a| a.ty == AT_FILE_NAME)
        .filter_map(|a| a.value().and_then(FileName::parse))
        // The real name rather than its 8.3 alias.
        .min_by_key(|f| if f.namespace == 2 { 1 } else { 0 })
        .map(|f| (String::from_utf16_lossy(&f.name), f.parent));
    let dir = r.is_dir() || attrs.iter().any(|a| a.ty == AT_INDEX_ROOT);
    Rec { in_use: r.in_use(), dir, seq: r.seq, name, attrs }
}

/// The path of folder `parent` (a file reference: record and sequence),
/// None when it is not known any more.
fn folder_path(
    v: &mut Volume,
    recs: &mut HashMap<u64, Rec>,
    cache: &mut HashMap<u64, Option<Vec<String>>>,
    parent: u64,
    depth: u32,
) -> Option<Vec<String>> {
    let n = record_of(parent);
    let seq = (parent >> 48) as u16;
    if n == ROOT {
        return Some(Vec::new());
    }
    if depth > 64 {
        return None;
    }
    if let Some(p) = cache.get(&n) {
        return p.clone();
    }
    if let std::collections::hash_map::Entry::Vacant(e) = recs.entry(n) {
        let r = v.record(n).ok()?;
        e.insert(read(v, r));
    }
    let r = &recs[&n];
    // The same folder: its sequence number as referenced (one more once
    // it was deleted itself).
    let same = r.dir && (r.seq == seq || (!r.in_use && r.seq == seq.wrapping_add(1)));
    let result = if same {
        let (name, up) = r.name.clone()?;
        folder_path(v, recs, cache, up, depth + 1).map(|mut p| {
            p.push(name);
            p
        })
    } else {
        None
    };
    cache.insert(n, result.clone());
    result
}

/// Modified and created times from $STANDARD_INFORMATION.
fn times(attrs: &[Attr]) -> (Option<i64>, Option<i64>) {
    match attrs.iter().find(|a| a.ty == AT_STANDARD_INFORMATION).and_then(|a| a.value()) {
        Some(v) if v.len() >= 32 => (filetime(le64(v, 8)), filetime(le64(v, 0))),
        _ => (None, None),
    }
}
