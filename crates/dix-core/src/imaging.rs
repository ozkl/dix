//! Copying whole disks: a disk into an image file (also from a failing
//! disk, as GNU ddrescue does), and an image onto a disk, checked by
//! reading it back.
//!
//! **Rescue copies** read what can be read first and come back for the
//! rest: unreadable areas are skipped at first (further each time reads
//! keep failing), then read without skipping, then *trimmed* sector by
//! sector from both ends, then *scraped* (every sector left), then retried.
//! A [`Map`] records the state of every byte — in ddrescue's mapfile
//! format, so a copy can be stopped and continued later, here or with
//! ddrescue itself, and viewed with tools made for those maps.

use crate::device::BlockDevice;
use crate::error::{Error, ErrorKind, Result};
use std::io::Write;

/// The state of an area of the disk (ddrescue's symbols in brackets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    /// Not read yet (`?`).
    NonTried,
    /// A read failed somewhere in it; its ends not read one by one yet (`*`).
    NonTrimmed,
    /// Between two unreadable sectors; not read sector by sector yet (`/`).
    NonScraped,
    /// Unreadable sectors (`-`).
    Bad,
    /// Read and copied (`+`).
    Finished,
}

impl Status {
    pub fn symbol(self) -> char {
        match self {
            Status::NonTried => '?',
            Status::NonTrimmed => '*',
            Status::NonScraped => '/',
            Status::Bad => '-',
            Status::Finished => '+',
        }
    }

    pub fn from_symbol(c: char) -> Option<Status> {
        Some(match c {
            '?' => Status::NonTried,
            '*' => Status::NonTrimmed,
            '/' => Status::NonScraped,
            '-' => Status::Bad,
            '+' => Status::Finished,
            _ => return None,
        })
    }
}

/// What a rescue is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Copying,
    Trimming,
    Scraping,
    Retrying,
    Finished,
}

impl Phase {
    fn symbol(self) -> char {
        match self {
            Phase::Copying => '?',
            Phase::Trimming => '*',
            Phase::Scraping => '/',
            Phase::Retrying => '-',
            Phase::Finished => '+',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Phase::Copying => "Copying",
            Phase::Trimming => "Trimming unreadable areas",
            Phase::Scraping => "Scraping unreadable areas",
            Phase::Retrying => "Retrying bad sectors",
            Phase::Finished => "Finished",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    pub pos: u64,
    pub size: u64,
    pub status: Status,
}

/// Bytes in each state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub non_tried: u64,
    pub non_trimmed: u64,
    pub non_scraped: u64,
    pub bad: u64,
    pub finished: u64,
    /// Separate unreadable areas.
    pub bad_areas: u64,
}

/// The state of every byte of a disk being rescued, as runs of one
/// [`Status`] covering `0..size` in order (a ddrescue mapfile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Map {
    size: u64,
    blocks: Vec<Block>,
    /// Where the copy is.
    pub pos: u64,
    pub phase: Phase,
    /// The pass of the phase (1, 2, …).
    pub pass: u32,
}

impl Map {
    /// A disk of `size` bytes, nothing read yet.
    pub fn new(size: u64) -> Map {
        let blocks = if size > 0 { vec![Block { pos: 0, size, status: Status::NonTried }] } else { Vec::new() };
        Map { size, blocks, pos: 0, phase: Phase::Copying, pass: 1 }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// Marks `len` bytes from `pos`.
    pub fn set(&mut self, pos: u64, len: u64, status: Status) {
        let end = pos.saturating_add(len).min(self.size);
        if pos >= end {
            return;
        }
        let i = self.blocks.partition_point(|b| b.pos + b.size <= pos);
        let mut j = i;
        while j < self.blocks.len() && self.blocks[j].pos < end {
            j += 1;
        }
        let (first, last) = (self.blocks[i], self.blocks[j - 1]);
        let mut new = Vec::with_capacity(3);
        if first.pos < pos {
            new.push(Block { pos: first.pos, size: pos - first.pos, status: first.status });
        }
        new.push(Block { pos, size: end - pos, status });
        if last.pos + last.size > end {
            new.push(Block { pos: end, size: last.pos + last.size - end, status: last.status });
        }
        let added = new.len();
        self.blocks.splice(i..j, new);
        // Join runs of the same state around the change.
        let lo = i.saturating_sub(1).max(1);
        let hi = (i + added + 1).min(self.blocks.len());
        for k in (lo..hi).rev() {
            if self.blocks[k - 1].status == self.blocks[k].status {
                self.blocks[k - 1].size += self.blocks[k].size;
                self.blocks.remove(k);
            }
        }
    }

    pub fn status_at(&self, pos: u64) -> Option<Status> {
        let i = self.blocks.partition_point(|b| b.pos + b.size <= pos);
        self.blocks.get(i).map(|b| b.status)
    }

    pub fn totals(&self) -> Totals {
        let mut t = Totals::default();
        for b in &self.blocks {
            match b.status {
                Status::NonTried => t.non_tried += b.size,
                Status::NonTrimmed => t.non_trimmed += b.size,
                Status::NonScraped => t.non_scraped += b.size,
                Status::Bad => {
                    t.bad += b.size;
                    t.bad_areas += 1;
                }
                Status::Finished => t.finished += b.size,
            }
        }
        t
    }

    /// The areas in `status`, within `domain` (sorted byte ranges) if given.
    pub fn regions(&self, status: Status, domain: Option<&[(u64, u64)]>) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        for b in self.blocks.iter().filter(|b| b.status == status) {
            let (a, e) = (b.pos, b.pos + b.size);
            match domain {
                None => out.push((a, e)),
                Some(d) => {
                    let first = d.partition_point(|r| r.1 <= a);
                    for &(s, t) in d[first..].iter().take_while(|r| r.0 < e) {
                        let (s, t) = (s.max(a), t.min(e));
                        if s < t {
                            out.push((s, t));
                        }
                    }
                }
            }
        }
        out
    }

    /// The state of `n` equal parts of the disk, for drawing: the worst
    /// state in each (bad, then not yet trimmed or scraped, then not
    /// tried), else finished.
    pub fn cells(&self, n: usize) -> Vec<Status> {
        let rank = |s: Status| match s {
            Status::Finished => 0,
            Status::NonTried => 1,
            Status::NonScraped | Status::NonTrimmed => 2,
            Status::Bad => 3,
        };
        let mut out = vec![Status::Finished; n];
        if self.size == 0 || n == 0 {
            return out;
        }
        let cell = |pos: u64| ((pos as u128 * n as u128) / self.size as u128) as usize;
        for b in &self.blocks {
            let (first, last) = (cell(b.pos), cell(b.pos + b.size - 1).min(n - 1));
            for c in &mut out[first..=last] {
                if rank(b.status) > rank(*c) {
                    *c = b.status;
                }
            }
        }
        out
    }

    /// Reads a ddrescue mapfile for a disk of `size` bytes. A map that
    /// stops short of the end (a disk whose size ddrescue did not know)
    /// is completed with untried space.
    pub fn parse(text: &str, size: u64) -> Result<Map> {
        let bad = |what: &str| Error::corrupted(format!("map file: {what}"));
        let num = |s: &str| -> Result<u64> {
            let r = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(h) => u64::from_str_radix(h, 16),
                None => s.parse(),
            };
            r.map_err(|_| bad(&format!("“{s}” is not a number")))
        };
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
        let head = lines.next().ok_or_else(|| bad("it is empty"))?;
        let mut f = head.split_whitespace();
        let pos = num(f.next().unwrap_or(""))?;
        let phase = match f.next().and_then(|s| s.chars().next()) {
            Some('*') => Phase::Trimming,
            Some('/') => Phase::Scraping,
            Some('-') => Phase::Retrying,
            Some('+') => Phase::Finished,
            Some(_) => Phase::Copying,
            None => return Err(bad("the first line has no status")),
        };
        let pass = f.next().map(num).transpose()?.unwrap_or(1).max(1) as u32;
        let mut blocks: Vec<Block> = Vec::new();
        let mut end = 0u64;
        for l in lines {
            let mut f = l.split_whitespace();
            let (Some(p), Some(s), Some(st)) = (f.next(), f.next(), f.next()) else {
                return Err(bad(&format!("cannot read the line “{l}”")));
            };
            let (p, s) = (num(p)?, num(s)?);
            let status = st
                .chars()
                .next()
                .and_then(Status::from_symbol)
                .ok_or_else(|| bad(&format!("unknown status “{st}”")))?;
            if p != end {
                return Err(bad("its areas are not in order"));
            }
            if s == 0 {
                continue;
            }
            end = p.checked_add(s).ok_or_else(|| bad("an area is too large"))?;
            match blocks.last_mut() {
                Some(b) if b.status == status => b.size += s,
                _ => blocks.push(Block { pos: p, size: s, status }),
            }
        }
        if end > size {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("the map file is for a disk of {end} bytes, larger than this one ({size} bytes)"),
            ));
        }
        let mut map = Map { size, blocks, pos: pos.min(size), phase, pass };
        if end < size {
            match map.blocks.last_mut() {
                Some(b) if b.status == Status::NonTried => b.size += size - end,
                _ => map.blocks.push(Block { pos: end, size: size - end, status: Status::NonTried }),
            }
        }
        Ok(map)
    }

    /// The map as a ddrescue mapfile.
    pub fn to_text(&self) -> String {
        let mut s = format!(
            "# Mapfile. Created by Disk Image Explorer {}\n# current_pos  current_status  current_pass\n0x{:08X}     {}               {}\n#      pos        size  status\n",
            env!("CARGO_PKG_VERSION"),
            self.pos,
            self.phase.symbol(),
            self.pass
        );
        for b in &self.blocks {
            s.push_str(&format!("0x{:08X}  0x{:08X}  {}\n", b.pos, b.size, b.status.symbol()));
        }
        s
    }
}

/// How a rescue reads.
#[derive(Debug, Clone)]
pub struct RescueOptions {
    /// Bytes read at once while copying (a multiple of the sector size).
    pub cluster: u64,
    /// Passes over the bad sectors at the end.
    pub retries: u32,
    /// Only these byte ranges (sorted, not overlapping): the space a file
    /// system uses. None: the whole disk.
    pub domain: Option<Vec<(u64, u64)>>,
}

impl Default for RescueOptions {
    fn default() -> Self {
        RescueOptions { cluster: 128 << 10, retries: 1, domain: None }
    }
}

/// The first skip after a failed read while copying; it doubles while
/// reads keep failing.
const SKIP_MIN: u64 = 64 << 10;

/// Rescues `src` into `dst` (an image of the same size; areas not read are
/// left as they are, zeros in a new image), going on from where `map`
/// says. `progress(map, read errors)` is called often; false stops (the
/// map then says where to go on from). Returns whether it finished.
pub fn rescue(
    src: &mut dyn BlockDevice,
    dst: &mut dyn BlockDevice,
    map: &mut Map,
    opts: &RescueOptions,
    progress: &mut dyn FnMut(&Map, u64) -> bool,
) -> Result<bool> {
    let size = src.size();
    if map.size() != size {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("the map is for a disk of {} bytes; this one has {size}", map.size()),
        ));
    }
    if dst.size() < size {
        return Err(Error::new(ErrorKind::InvalidInput, "the image is smaller than the disk"));
    }
    let ss = src.info().sector_size as u64;
    let unit = if ss.is_power_of_two() { ss.clamp(512, 4096) } else { 512 };
    let cluster = (opts.cluster / unit).max(1) * unit;
    // The domain on whole sectors.
    let domain = opts.domain.as_ref().map(|d| {
        let mut out: Vec<(u64, u64)> = Vec::new();
        for &(a, b) in d {
            let (a, b) = (a / unit * unit, b.div_ceil(unit).saturating_mul(unit).min(size));
            match out.last_mut() {
                Some(l) if a <= l.1 => l.1 = l.1.max(b),
                _ if a < b => out.push((a, b)),
                _ => {}
            }
        }
        out
    });
    let mut r =
        Rescuer { src, dst, map, unit, buf: vec![0u8; cluster as usize], errors: 0, failing: 0, pending: 0, progress };
    let domain = domain.as_deref();
    let max_skip = (size / 100).clamp(SKIP_MIN, 1 << 30) / unit * unit;
    // Each phase works on what is left in its state, so a copy goes on
    // from any map (also one made with another domain, or by ddrescue).
    // Copying: first skipping past trouble (unless that pass was done),
    // then reading what was skipped.
    let skipping_done = r.map.phase != Phase::Copying || r.map.pass > 1;
    let retried = if r.map.phase == Phase::Retrying { r.map.pass.saturating_sub(1) } else { 0 };
    for pass in 1..=2 {
        if pass == 1 && skipping_done {
            continue;
        }
        r.map.phase = Phase::Copying;
        r.map.pass = pass;
        for (a, b) in r.map.regions(Status::NonTried, domain) {
            if !r.copy(a, b, cluster, if pass == 1 { max_skip } else { 0 })? {
                return Ok(false);
            }
        }
    }
    r.map.phase = Phase::Trimming;
    r.map.pass = 1;
    for (a, b) in r.map.regions(Status::NonTrimmed, domain) {
        if !r.trim(a, b)? {
            return Ok(false);
        }
    }
    r.map.phase = Phase::Scraping;
    for (a, b) in r.map.regions(Status::NonScraped, domain) {
        if !r.sectors(a, b)? {
            return Ok(false);
        }
    }
    for pass in retried + 1..=opts.retries {
        r.map.phase = Phase::Retrying;
        r.map.pass = pass;
        for (a, b) in r.map.regions(Status::Bad, domain) {
            if !r.sectors(a, b)? {
                return Ok(false);
            }
        }
    }
    r.map.phase = Phase::Finished;
    r.map.pass = 1;
    r.map.pos = 0;
    let errors = r.errors;
    r.dst.flush()?;
    Ok((r.progress)(r.map, errors))
}

struct Rescuer<'a> {
    src: &'a mut dyn BlockDevice,
    dst: &'a mut dyn BlockDevice,
    map: &'a mut Map,
    unit: u64,
    buf: Vec<u8>,
    /// Failed reads so far.
    errors: u64,
    /// Failed reads in a row.
    failing: u32,
    /// Bytes handled since progress was last reported.
    pending: u64,
    progress: &'a mut dyn FnMut(&Map, u64) -> bool,
}

impl Rescuer<'_> {
    /// Reads `len` bytes at `pos` into the image and marks them; false
    /// when the read failed.
    fn read(&mut self, pos: u64, len: u64, fail: Status) -> Result<bool> {
        self.map.pos = pos;
        let n = len as usize;
        match self.src.read_at(pos, &mut self.buf[..n]) {
            Ok(()) => {
                if self.buf[..n].iter().any(|&b| b != 0) {
                    self.dst.write_at(pos, &self.buf[..n]).map_err(|e| e.with_context("writing the image"))?;
                }
                self.map.set(pos, len, Status::Finished);
                self.failing = 0;
                Ok(true)
            }
            Err(e) => {
                self.errors += 1;
                self.failing += 1;
                if gone(&e) || (self.failing.is_multiple_of(16) && !self.answers()) {
                    return Err(Error::new(
                        ErrorKind::Io,
                        format!(
                            "the disk stopped answering ({}): unplugged, or failing badly. What was copied is kept; \
                             the copy can be continued later.",
                            e.message()
                        ),
                    ));
                }
                self.map.set(pos, len, fail);
                Ok(false)
            }
        }
    }

    /// Whether the disk still reads somewhere it read before.
    fn answers(&mut self) -> bool {
        let Some(b) = self.map.blocks().iter().find(|b| b.status == Status::Finished).copied() else {
            return true;
        };
        let mut one = vec![0u8; self.unit.min(b.size) as usize];
        self.src.read_at(b.pos, &mut one).is_ok()
    }

    /// Reports progress every so often; false: stop.
    fn tick(&mut self, n: u64) -> bool {
        self.pending += n.max(self.unit * 8);
        if self.pending < 1 << 20 {
            return true;
        }
        self.pending = 0;
        (self.progress)(self.map, self.errors)
    }

    /// Copies `a..b` in clusters; after a failed read skips `skip` bytes
    /// (doubling while reads fail, at most `max_skip`; 0: no skipping).
    fn copy(&mut self, a: u64, b: u64, cluster: u64, max_skip: u64) -> Result<bool> {
        let mut pos = a;
        let mut skip = 0u64;
        while pos < b {
            // Reads on cluster boundaries after the first.
            let n = (cluster - pos % cluster).min(b - pos);
            if self.read(pos, n, Status::NonTrimmed)? {
                skip = 0;
            } else if max_skip > 0 {
                skip = if skip == 0 { SKIP_MIN } else { (skip * 2).min(max_skip) };
                let jump = skip.min(b - (pos + n)) / self.unit * self.unit;
                pos += jump;
            }
            pos += n;
            if !self.tick(n) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Reads a failed cluster's sectors from its start until one fails,
    /// then from its end back until one fails; what is between is left to
    /// scraping.
    fn trim(&mut self, a: u64, b: u64) -> Result<bool> {
        let mut s = a;
        while s < b {
            let n = self.unit.min(b - s);
            let ok = self.read(s, n, Status::Bad)?;
            s += n;
            if !self.tick(n) {
                return Ok(false);
            }
            if !ok {
                break;
            }
        }
        let mut e = b;
        while e > s {
            let n = match e % self.unit {
                0 => self.unit,
                r => r,
            }
            .min(e - s);
            let ok = self.read(e - n, n, Status::Bad)?;
            e -= n;
            if !self.tick(n) {
                return Ok(false);
            }
            if !ok {
                break;
            }
        }
        if e > s {
            self.map.set(s, e - s, Status::NonScraped);
        }
        Ok(true)
    }

    /// Reads every sector of `a..b` once.
    fn sectors(&mut self, a: u64, b: u64) -> Result<bool> {
        let mut s = a;
        while s < b {
            let n = self.unit.min(b - s);
            self.read(s, n, Status::Bad)?;
            s += n;
            if !self.tick(n) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Whether a read error says the disk is not there any more (as opposed to
/// a sector that cannot be read).
fn gone(e: &Error) -> bool {
    let m = e.message();
    let code = m.rsplit_once("(os error ").and_then(|(_, t)| t.split(')').next()).and_then(|c| c.parse::<i32>().ok());
    let codes: &[i32] = if cfg!(windows) {
        // ERROR_NOT_READY, ERROR_NO_SUCH_DEVICE, ERROR_DEVICE_NOT_CONNECTED, ERROR_DEV_NOT_EXIST.
        &[21, 433, 1167, 55]
    } else {
        // ENXIO, ENODEV, ENOMEDIUM (Linux).
        &[6, 19, 123]
    };
    code.is_some_and(|c| codes.contains(&c))
}

/// Byte ranges as one sorted list without overlaps.
pub fn merge_ranges(mut r: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    r.retain(|x| x.0 < x.1);
    r.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(r.len());
    for (a, b) in r {
        match out.last_mut() {
            Some(l) if a <= l.1 => l.1 = l.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// `0..size` without `holes` (sorted or not).
pub fn complement(holes: &[(u64, u64)], size: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut at = 0;
    for (a, b) in merge_ranges(holes.to_vec()) {
        let (a, b) = (a.min(size), b.min(size));
        if a > at {
            out.push((at, a));
        }
        at = at.max(b);
    }
    if at < size {
        out.push((at, size));
    }
    out
}

const STOPPED: &str = "stopped";

/// Whether an error is [`DiskWriter`]'s or [`DiskVerifier`]'s report of
/// being stopped.
pub fn is_stopped(e: &Error) -> bool {
    e.message().ends_with(STOPPED)
}

/// A sink that writes an image onto a disk, from its start: decompressors
/// write into it. Chunks are written as they fill. `report(bytes written)`
/// returns false to stop.
pub struct DiskWriter<'a> {
    dst: &'a mut dyn BlockDevice,
    at: u64,
    buf: Vec<u8>,
    fill: usize,
    report: &'a mut dyn FnMut(u64) -> bool,
}

const SINK_CHUNK: usize = 4 << 20;

impl<'a> DiskWriter<'a> {
    pub fn new(dst: &'a mut dyn BlockDevice, report: &'a mut dyn FnMut(u64) -> bool) -> Self {
        DiskWriter { dst, at: 0, buf: vec![0u8; SINK_CHUNK], fill: 0, report }
    }

    fn put(&mut self) -> std::io::Result<()> {
        if self.fill == 0 {
            return Ok(());
        }
        let size = self.dst.size();
        if self.at + self.fill as u64 > size {
            return Err(std::io::Error::other(format!(
                "the image is larger than the disk ({size} bytes); nothing past its end was written"
            )));
        }
        self.dst.write_at(self.at, &self.buf[..self.fill]).map_err(|e| std::io::Error::other(e.to_string()))?;
        self.at += self.fill as u64;
        self.fill = 0;
        if !(self.report)(self.at) {
            return Err(std::io::Error::other(STOPPED));
        }
        Ok(())
    }

    /// Writes what is left and makes sure it reached the disk. Returns the
    /// image's size.
    pub fn finish(mut self) -> Result<u64> {
        self.put().map_err(Error::from)?;
        self.dst.flush()?;
        Ok(self.at)
    }
}

impl Write for DiskWriter<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let n = data.len().min(self.buf.len() - self.fill);
        self.buf[self.fill..self.fill + n].copy_from_slice(&data[..n]);
        self.fill += n;
        if self.fill == self.buf.len() {
            self.put()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A sink that compares an image with what is on a disk, from its start.
/// The first difference is an error. `report(bytes compared)` returns
/// false to stop.
pub struct DiskVerifier<'a> {
    dst: &'a mut dyn BlockDevice,
    at: u64,
    buf: Vec<u8>,
    disk: Vec<u8>,
    fill: usize,
    report: &'a mut dyn FnMut(u64) -> bool,
}

impl<'a> DiskVerifier<'a> {
    pub fn new(dst: &'a mut dyn BlockDevice, report: &'a mut dyn FnMut(u64) -> bool) -> Self {
        DiskVerifier { dst, at: 0, buf: vec![0u8; SINK_CHUNK], disk: vec![0u8; SINK_CHUNK], fill: 0, report }
    }

    fn compare(&mut self) -> std::io::Result<()> {
        if self.fill == 0 {
            return Ok(());
        }
        let n = self.fill;
        if self.at + n as u64 > self.dst.size() {
            return Err(std::io::Error::other("the image is larger than the disk"));
        }
        self.dst
            .read_at(self.at, &mut self.disk[..n])
            .map_err(|e| std::io::Error::other(format!("reading the disk back: {e}")))?;
        if let Some(i) = (0..n).find(|&i| self.buf[i] != self.disk[i]) {
            return Err(std::io::Error::other(format!(
                "the disk differs from the image at byte {} — the disk may be failing or fake (smaller than it says)",
                self.at + i as u64
            )));
        }
        self.at += n as u64;
        self.fill = 0;
        if !(self.report)(self.at) {
            return Err(std::io::Error::other(STOPPED));
        }
        Ok(())
    }

    /// Compares what is left. Returns the bytes compared.
    pub fn finish(mut self) -> Result<u64> {
        self.compare().map_err(Error::from)?;
        Ok(self.at)
    }
}

impl Write for DiskVerifier<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let n = data.len().min(self.buf.len() - self.fill);
        self.buf[self.fill..self.fill + n].copy_from_slice(&data[..n]);
        self.fill += n;
        if self.fill == self.buf.len() {
            self.compare()?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Feeds a whole device (an image, or a VM disk read through its
/// container) into a sink.
pub fn feed(src: &mut dyn BlockDevice, out: &mut dyn Write) -> Result<()> {
    let size = src.size();
    let mut buf = vec![0u8; SINK_CHUNK];
    let mut at = 0u64;
    while at < size {
        let n = (size - at).min(SINK_CHUNK as u64) as usize;
        src.read_at(at, &mut buf[..n])?;
        out.write_all(&buf[..n]).map_err(Error::from)?;
        at += n as u64;
    }
    Ok(())
}

/// After an image of `image_size` bytes was written to the start of a
/// larger disk: zeros the disk's last MiB (not reaching into the image),
/// so an old GPT backup header there is not found again.
pub fn clear_tail(dst: &mut dyn BlockDevice, image_size: u64) -> Result<()> {
    let size = dst.size();
    if size <= image_size {
        return Ok(());
    }
    let from = image_size.max(size.saturating_sub(1 << 20));
    let zeros = vec![0u8; (size - from) as usize];
    dst.write_at(from, &zeros)?;
    dst.flush()
}

#[cfg(test)]
#[path = "imaging_tests.rs"]
mod tests;
