//! Several devices read one after another as one: the parts of a split
//! archive (`x.7z.001`, `x.002`, …; `x.z01` … `x.zip`; RAR volumes) or a
//! split disk image.

use super::{BlockDevice, DeviceInfo, DeviceKind};
use crate::error::{Error, Result};

/// The parts of a split file, read as one device (read-only).
pub struct JoinedDevice {
    parts: Vec<Box<dyn BlockDevice>>,
    /// Where each part starts, and the end of the last.
    starts: Vec<u64>,
    name: String,
}

impl JoinedDevice {
    /// Joins `parts` in order; `name` is shown for the whole.
    pub fn new(name: impl Into<String>, parts: Vec<Box<dyn BlockDevice>>) -> Self {
        let mut starts = vec![0u64];
        for p in &parts {
            starts.push(starts.last().unwrap() + p.size());
        }
        Self { parts, starts, name: name.into() }
    }

    /// How many parts there are.
    pub fn count(&self) -> usize {
        self.parts.len()
    }
}

impl BlockDevice for JoinedDevice {
    fn info(&self) -> DeviceInfo {
        let first = self.parts.first().map(|p| p.info());
        DeviceInfo {
            name: self.name.clone(),
            kind: first.as_ref().map_or(DeviceKind::ImageFile, |i| i.kind),
            size: self.size(),
            sector_size: first.map_or(512, |i| i.sector_size),
            read_only: true,
            resizable: false,
        }
    }

    fn size(&self) -> u64 {
        *self.starts.last().unwrap()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        super::check_range(offset, buf.len(), self.size())?;
        let mut done = 0usize;
        // The part holding `offset`.
        let mut i = self.starts.partition_point(|&s| s <= offset).saturating_sub(1);
        while done < buf.len() {
            let at = offset + done as u64;
            while self.starts[i + 1] <= at {
                i += 1;
            }
            let n = (buf.len() - done).min((self.starts[i + 1] - at) as usize);
            self.parts[i].read_at(at - self.starts[i], &mut buf[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    fn write_at(&mut self, _offset: u64, _data: &[u8]) -> Result<()> {
        Err(Error::read_only())
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parts(&self) -> Option<Vec<u64>> {
        Some(self.parts.iter().map(|p| p.size()).collect())
    }
}

/// The file names of the split set that `name` belongs to, in order, if
/// it is part of one (and at least two parts are there): `exists` says
/// whether a sibling file is present. The set is taken from its first
/// part up to the first one missing, and must include `name`.
///
/// | naming                             | order                          |
/// |------------------------------------|--------------------------------|
/// | `x.part1.rar`, `x.part2.rar`, …    | by number (any width)          |
/// | `x.rar`, `x.r00` … `x.r99`, `x.s00` … | RAR 4 style volumes         |
/// | `x.z01`, `x.z02`, …, `x.zip`       | split zip (the `.zip` is last) |
/// | `x.arj`, `x.a01`, `x.a02`, …       | ARJ volumes                    |
/// | `x.001`, `x.002`, … (`x.7z.001`)   | by number, from `000` if there |
pub fn split_set(name: &str, exists: impl Fn(&str) -> bool) -> Option<Vec<String>> {
    let set = |names: Vec<String>| -> Option<Vec<String>> {
        if names.len() >= 2 && names.iter().any(|n| n == name) {
            Some(names)
        } else {
            None
        }
    };
    let lower = name.to_ascii_lowercase();
    let dot = name.rfind('.')?;
    let (stem, ext) = (&name[..dot], &name[dot + 1..]);
    let ext_l = ext.to_ascii_lowercase();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    // x.partN.rar
    if ext_l == "rar" {
        if let Some(i) = lower[..stem.len()].rfind(".part") {
            let num = &stem[i + 5..];
            if digits(num) {
                let (prefix, w) = (&stem[..i + 5], num.len());
                let mut names = Vec::new();
                for n in 1.. {
                    let f = format!("{prefix}{n:0w$}.{ext}");
                    if !exists(&f) && f != name {
                        break;
                    }
                    names.push(f);
                }
                return set(names);
            }
        }
    }
    let upper = ext.starts_with(|c: char| c.is_ascii_uppercase());
    let letter = |c: char| if upper { c.to_ascii_uppercase() } else { c };
    // x.rar, x.r00, …, x.s00, …
    let old_rar = ext_l == "rar" || (ext_l.len() == 3 && matches!(&ext_l[..1], "r" | "s") && digits(&ext_l[1..]));
    if old_rar {
        let first = format!(
            "{stem}.{}",
            if ext_l == "rar" {
                ext
            } else if upper {
                "RAR"
            } else {
                "rar"
            }
        );
        if exists(&first) || first == name {
            let mut names = vec![first];
            'more: for l in ['r', 's', 't', 'u'] {
                for n in 0..100 {
                    let f = format!("{stem}.{}{n:02}", letter(l));
                    if !exists(&f) && f != name {
                        break 'more;
                    }
                    names.push(f);
                }
            }
            if let Some(s) = set(names) {
                return Some(s);
            }
        }
    }
    // x.arj, x.a01, x.a02, …
    let arj_part = ext_l.len() >= 3 && ext_l.starts_with('a') && digits(&ext_l[1..]);
    if ext_l == "arj" || arj_part {
        let first = format!(
            "{stem}.{}",
            if ext_l == "arj" {
                ext
            } else if upper {
                "ARJ"
            } else {
                "arj"
            }
        );
        let mut names = vec![first];
        for n in 1..1000 {
            let f = format!("{stem}.{}{n:02}", letter('a'));
            if !exists(&f) && f != name {
                break;
            }
            names.push(f);
        }
        if exists(&names[0]) || names[0] == name {
            return set(names);
        }
        return None;
    }
    // x.z01, …, x.zip
    let zip_part = ext_l.len() >= 3 && ext_l.starts_with('z') && digits(&ext_l[1..]);
    if ext_l == "zip" || zip_part {
        let mut names = Vec::new();
        for n in 1.. {
            let f = format!("{stem}.{}{n:02}", letter('z'));
            if !exists(&f) && f != name {
                break;
            }
            names.push(f);
        }
        if !names.is_empty() {
            names.push(format!("{stem}.{}", if upper { "ZIP" } else { "zip" }));
            if exists(names.last().unwrap()) || names.last().unwrap() == name {
                return set(names);
            }
        }
        return None;
    }
    // x.001, x.002, …
    if ext.len() >= 3 && digits(ext) {
        let w = ext.len();
        let at = |n: usize| format!("{stem}.{n:0w$}");
        let start = if exists(&at(0)) || at(0) == name { 0 } else { 1 };
        let mut names = Vec::new();
        for n in start.. {
            let f = at(n);
            if !exists(&f) && f != name {
                break;
            }
            names.push(f);
        }
        return set(names);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MemoryDevice;

    #[test]
    fn reads_across_parts() {
        let whole: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        let cuts = [0usize, 3, 3, 400, 999, 1000];
        let parts: Vec<Box<dyn BlockDevice>> = cuts
            .windows(2)
            .map(|w| Box::new(MemoryDevice::new("p", whole[w[0]..w[1]].to_vec())) as Box<dyn BlockDevice>)
            .collect();
        let mut d = JoinedDevice::new("x", parts);
        assert_eq!(d.size(), 1000);
        assert_eq!(d.parts(), Some(vec![3, 0, 397, 599, 1]));
        for (off, len) in [(0, 1000), (2, 5), (3, 1), (399, 2), (998, 2), (500, 0), (1000, 0)] {
            let mut b = vec![0u8; len];
            d.read_at(off as u64, &mut b).unwrap();
            assert_eq!(b, &whole[off..off + len]);
        }
        assert!(d.read_at(999, &mut [0u8; 2]).is_err());
        assert!(d.write_at(0, &[1]).is_err());
    }

    fn sets(files: &[&str], open: &str) -> Option<Vec<String>> {
        split_set(open, |n| files.contains(&n))
    }

    #[test]
    fn finds_split_sets() {
        let v = |a: &[&str]| Some(a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let rar = ["a.part1.rar", "a.part2.rar", "a.part3.rar"];
        assert_eq!(sets(&rar, "a.part2.rar"), v(&rar));
        assert_eq!(sets(&["b.part01.rar", "b.part02.rar"], "b.part01.rar"), v(&["b.part01.rar", "b.part02.rar"]));
        assert_eq!(sets(&["c.part1.rar"], "c.part1.rar"), None);
        // Part 1 missing: no set.
        assert_eq!(sets(&["d.part2.rar", "d.part3.rar"], "d.part2.rar"), None);
        let old = ["e.rar", "e.r00", "e.r01"];
        assert_eq!(sets(&old, "e.r01"), v(&old));
        assert_eq!(sets(&old, "e.rar"), v(&old));
        assert_eq!(sets(&["E.RAR", "E.R00"], "E.R00"), v(&["E.RAR", "E.R00"]));
        assert_eq!(sets(&["f.rar"], "f.rar"), None);
        let zip = ["g.z01", "g.z02", "g.zip"];
        assert_eq!(sets(&zip, "g.zip"), v(&zip));
        assert_eq!(sets(&zip, "g.z01"), v(&zip));
        assert_eq!(sets(&["h.zip"], "h.zip"), None);
        let arj = ["n.arj", "n.a01", "n.a02"];
        assert_eq!(sets(&arj, "n.a02"), v(&arj));
        assert_eq!(sets(&arj, "n.arj"), v(&arj));
        assert_eq!(sets(&["o.arj"], "o.arj"), None);
        assert_eq!(sets(&["p.a01", "p.a02"], "p.a01"), None);
        assert_eq!(sets(&["i.z01"], "i.z01"), None);
        let num = ["j.7z.001", "j.7z.002", "j.7z.003"];
        assert_eq!(sets(&num, "j.7z.003"), v(&num));
        assert_eq!(sets(&["k.000", "k.001"], "k.001"), v(&["k.000", "k.001"]));
        assert_eq!(sets(&["l.001"], "l.001"), None);
        assert_eq!(sets(&["m.img"], "m.img"), None);
        assert_eq!(sets(&["noext"], "noext"), None);
    }
}
