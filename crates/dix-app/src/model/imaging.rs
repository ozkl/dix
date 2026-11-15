//! Copying whole disks (native builds): a disk saved as an image — all of
//! it, only the space its file systems use, or rescued from a failing disk
//! — and an image written onto a disk, then read back to check it.
//!
//! Each runs on a thread of its own; the UI reads its [`Progress`] and
//! gets the [`Outcome`] when it ends.

use dix_core::container::DiskFormat;
use dix_core::device::host::{self, FileDevice};
use dix_core::device::SharedDevice;
use dix_core::imaging::{self, Map, RescueOptions, Status};
use dix_core::{AccessMode, BlockDevice, DriverRegistry};
use eframe::egui;
#[cfg(feature = "archive")]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How far a job has come; shared with its thread.
#[derive(Default)]
pub struct Progress {
    /// What it is doing ("Copying", "Verifying", …).
    pub phase: Mutex<String>,
    pub done: AtomicU64,
    pub total: AtomicU64,
    /// What was done before this run (a rescue going on): not counted in
    /// the speed.
    pub base: AtomicU64,
    /// A rescue's map, as of a moment ago.
    pub map: Mutex<Option<Map>>,
    /// Failed reads of a rescue.
    pub errors: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    fn phase(&self, p: impl Into<String>) {
        *self.phase.lock().unwrap() = p.into();
    }
    fn stopped(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// How a job ended.
#[derive(Debug)]
pub struct Outcome {
    pub message: String,
    /// The image made (to offer opening it).
    pub image: Option<PathBuf>,
    /// Stopped before the end, as asked (it can go on later).
    pub partial: bool,
}

pub struct Job {
    pub title: String,
    pub progress: Arc<Progress>,
    pub result: Receiver<Result<Outcome, String>>,
    pub started: Instant,
    /// It writes onto a disk: stopping or quitting leaves the disk half
    /// written.
    pub writes_disk: bool,
    /// Stopping keeps what was done (a rescue goes on later).
    pub resumable: bool,
}

fn job(
    title: String,
    writes_disk: bool,
    resumable: bool,
    ctx: egui::Context,
    run: impl FnOnce(&Progress) -> Result<Outcome, String> + Send + 'static,
) -> Job {
    let progress = Arc::new(Progress::default());
    let (tx, rx) = channel();
    let p = progress.clone();
    std::thread::spawn(move || {
        let r = run(&p);
        let _ = tx.send(r);
        ctx.request_repaint();
    });
    Job { title, progress, result: rx, started: Instant::now(), writes_disk, resumable }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

fn size(n: u64) -> String {
    crate::ui::format_size(n)
}

/// The map file kept next to a rescued image (`disk.img.map`).
pub fn map_path(image: &Path) -> PathBuf {
    PathBuf::from(format!("{}.map", image.display()))
}

/// An earlier rescue into `image` that can be continued: its map.
pub fn earlier_rescue(image: &Path, disk_size: u64) -> Option<Map> {
    let text = std::fs::read_to_string(map_path(image)).ok()?;
    let map = Map::parse(&text, disk_size).ok()?;
    let len = std::fs::metadata(image).ok()?.len();
    (len == disk_size).then_some(map)
}

/// Opens a disk (or image) to copy from, read-only; a VM disk is read
/// through its container.
pub fn open_source(registry: &DriverRegistry, path: &str) -> dix_core::Result<Box<dyn BlockDevice>> {
    host::open_disk(registry, path, AccessMode::ReadOnly)
}

/// The space to copy: the used space when asked (and it can be read),
/// else everything. Notes for the outcome.
fn plan(src: &SharedDevice, used_only: bool, progress: &Progress) -> (Option<Vec<(u64, u64)>>, String) {
    if !used_only {
        return (None, String::new());
    }
    progress.phase("Reading the file systems' allocation maps");
    match dix_drivers::imaging::used_space(Box::new(src.handle())) {
        Ok(u) => {
            let known: Vec<String> =
                u.areas.iter().filter_map(|a| a.kind.map(|k| format!("{k} {}", size(a.free)))).collect();
            let note = if known.is_empty() {
                " (no FAT, exFAT, ext2/3/4 or NTFS file system found: all of it was copied)".to_owned()
            } else {
                format!(" — free space left out: {}", known.join(", "))
            };
            (Some(u.ranges), note)
        }
        Err(e) => (None, format!(" (the file systems could not be read, so all of it was copied: {e})")),
    }
}

/// Saves `src` into a new image at `dst`: raw or a container `format`.
pub fn start_save(
    ctx: egui::Context,
    src: Box<dyn BlockDevice>,
    dst: PathBuf,
    format: Option<Arc<dyn DiskFormat>>,
    formats: Vec<Arc<dyn DiskFormat>>,
    used_only: bool,
) -> Job {
    let title = format!("Saving {}", file_name(&dst));
    job(title, false, false, ctx, move |p| {
        let src = SharedDevice::new(src);
        let disk = src.size();
        let (ranges, note) = plan(&src, used_only, p);
        if p.stopped() {
            return Err("Stopped.".into());
        }
        p.phase("Copying");
        p.total.store(ranges.as_ref().map_or(disk, |r| r.iter().map(|x| x.1 - x.0).sum()), Ordering::Relaxed);
        let name = file_name(&dst);
        let r = (|| -> dix_core::Result<u64> {
            let file = Box::new(FileDevice::create(&dst, 0)?);
            let mut report = |d: u64, _: u64| {
                p.done.store(d, Ordering::Relaxed);
                !p.stopped()
            };
            dix_core::container::convert_ranges(
                &mut src.handle(),
                file,
                format.as_deref(),
                &name,
                &formats,
                ranges.as_deref(),
                &mut report,
            )
        })();
        match r {
            Ok(written) => Ok(Outcome {
                message: format!("Saved {name}: {} of data{note}.", size(written)),
                image: Some(dst),
                partial: false,
            }),
            Err(e) => {
                let _ = std::fs::remove_file(&dst);
                if p.stopped() {
                    return Err("Stopped; the unfinished image was deleted.".into());
                }
                let hint = if e.message().contains("reading the disk at byte") {
                    " The disk has places that cannot be read: a rescue copy gets everything else."
                } else {
                    ""
                };
                Err(format!("{e}.{hint}"))
            }
        }
    })
}

/// Rescues `src` into the raw image `dst`, going on from an earlier rescue
/// if its map is there.
pub fn start_rescue(ctx: egui::Context, src: Box<dyn BlockDevice>, dst: PathBuf, used_only: bool, retries: u32) -> Job {
    let title = format!("Rescuing into {}", file_name(&dst));
    job(title, false, true, ctx, move |p| {
        let src = SharedDevice::new(src);
        let disk = src.size();
        let map_file = map_path(&dst);
        let (mut file, mut map, again) = match earlier_rescue(&dst, disk) {
            Some(map) => {
                let f = FileDevice::open(&dst, AccessMode::ReadWrite).map_err(|e| e.to_string())?;
                (f, map, true)
            }
            None => (FileDevice::create(&dst, disk).map_err(|e| e.to_string())?, Map::new(disk), false),
        };
        let save = |m: &Map| -> Result<(), String> {
            let tmp = PathBuf::from(format!("{}.tmp", map_file.display()));
            std::fs::write(&tmp, m.to_text())
                .and_then(|_| std::fs::rename(&tmp, &map_file))
                .map_err(|e| format!("the map file {} could not be written: {e}", map_file.display()))
        };
        let new_map = !map_file.exists();
        save(&map)?;
        if new_map {
            host::give_to_sudo_user(&map_file);
        }
        let (domain, note) = plan(&src, used_only, p);
        let total = domain.as_ref().map_or(disk, |d| d.iter().map(|x| x.1 - x.0).sum());
        p.total.store(total, Ordering::Relaxed);
        let left = |m: &Map| -> u64 {
            [Status::NonTried, Status::NonTrimmed, Status::NonScraped]
                .iter()
                .flat_map(|&s| m.regions(s, domain.as_deref()))
                .map(|r| r.1 - r.0)
                .sum()
        };
        let before = total.saturating_sub(left(&map));
        p.base.store(before, Ordering::Relaxed);
        p.done.store(before, Ordering::Relaxed);
        let opts = RescueOptions { retries, domain: domain.clone(), ..Default::default() };
        let (mut shown, mut saved) = (Instant::now() - Duration::from_secs(1), Instant::now());
        let mut save_error = None;
        let r = imaging::rescue(&mut src.handle(), &mut file, &mut map, &opts, &mut |m, errors| {
            if shown.elapsed() >= Duration::from_millis(200) {
                shown = Instant::now();
                let what = match m.phase {
                    imaging::Phase::Copying | imaging::Phase::Retrying => {
                        format!("{} (pass {})", m.phase.label(), m.pass)
                    }
                    ph => ph.label().to_owned(),
                };
                p.phase(what);
                p.done.store(total.saturating_sub(left(m)), Ordering::Relaxed);
                p.errors.store(errors, Ordering::Relaxed);
                *p.map.lock().unwrap() = Some(m.clone());
            }
            if saved.elapsed() >= Duration::from_secs(10) {
                saved = Instant::now();
                if let Err(e) = save(m) {
                    save_error = Some(e);
                    return false;
                }
            }
            !p.stopped()
        });
        *p.map.lock().unwrap() = Some(map.clone());
        p.done.store(total.saturating_sub(left(&map)), Ordering::Relaxed);
        save(&map)?;
        if let Some(e) = save_error {
            return Err(e);
        }
        let t = map.totals();
        let name = file_name(&dst);
        match r {
            Ok(true) => {
                let bad = if t.bad == 0 {
                    "nothing unreadable".to_owned()
                } else {
                    format!(
                        "{} unreadable in {} place{} (zeros in the image)",
                        size(t.bad),
                        t.bad_areas,
                        if t.bad_areas == 1 { "" } else { "s" }
                    )
                };
                let from = if again { " (continued from the earlier copy)" } else { "" };
                Ok(Outcome {
                    message: format!(
                        "Rescued into {name}{from}: {} read, {bad}{note}. The map is in {}.",
                        size(t.finished),
                        file_name(&map_file)
                    ),
                    image: Some(dst),
                    partial: false,
                })
            }
            Ok(false) => Ok(Outcome {
                message: format!(
                    "Stopped: {} read so far. To go on from there, rescue this disk into {name} again (its map, {}, \
                     says where).",
                    size(t.finished),
                    file_name(&map_file)
                ),
                image: None,
                partial: true,
            }),
            Err(e) => {
                Err(format!("{e} ({} read; the map {} says what is left)", size(t.finished), file_name(&map_file)))
            }
        }
    })
}

/// What an image to write onto a disk is.
#[derive(Clone, Debug)]
pub struct ImageInfo {
    /// "raw image", "qcow2 disk", "xz-compressed image", …
    pub kind: String,
    /// What will be written, if known before unpacking.
    pub size: Option<u64>,
    /// The file's own size.
    pub file_size: u64,
}

/// A compressed image (written as it is unpacked), if `path` is one.
#[cfg(feature = "archive")]
fn compression(path: &Path) -> Option<dix_drivers::archive::Compression> {
    use dix_drivers::archive::Compression;
    let mut head = [0u8; 8];
    let mut f = std::fs::File::open(path).ok()?;
    let n = f.read(&mut head).ok()?;
    Some(Compression::detect(&head[..n])).filter(|c| *c != Compression::None)
}

pub fn describe_image(registry: &DriverRegistry, path: &Path) -> Result<ImageInfo, String> {
    let file_size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    #[cfg(feature = "archive")]
    if let Some(c) = compression(path) {
        return Ok(ImageInfo { kind: format!("{}-compressed image", c.name()), size: None, file_size });
    }
    let dev = open_source(registry, &path.to_string_lossy()).map_err(|e| e.to_string())?;
    let kind = match dev.container() {
        Some(c) => format!("{c} disk"),
        None => "raw image".to_owned(),
    };
    Ok(ImageInfo { kind, size: Some(dev.size()), file_size })
}

/// Counts what is read (the compressed bytes of an image being unpacked).
#[cfg(feature = "archive")]
struct Counting<'a> {
    inner: std::fs::File,
    progress: &'a Progress,
}

#[cfg(feature = "archive")]
impl Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.progress.done.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// Feeds the image at `path` into a sink: unpacked as it is read if
/// compressed, else read through its container.
fn feed(registry: &DriverRegistry, path: &Path, out: &mut dyn std::io::Write, p: &Progress) -> dix_core::Result<()> {
    p.done.store(0, Ordering::Relaxed);
    #[cfg(feature = "archive")]
    if let Some(c) = compression(path) {
        p.total.store(std::fs::metadata(path)?.len(), Ordering::Relaxed);
        let mut input = Counting { inner: std::fs::File::open(path)?, progress: p };
        return c.unpack(&mut input, out);
    }
    let mut src = open_source(registry, &path.to_string_lossy())?;
    p.total.store(src.size(), Ordering::Relaxed);
    imaging::feed(src.as_mut(), out)
}

/// Writes the image at `image` onto the disk at `disk` (its volumes
/// unmounted first if `unmount`; on Windows they are dismounted while it
/// is written), then reads it back if `verify`.
pub fn start_write(
    ctx: egui::Context,
    registry: DriverRegistry,
    image: PathBuf,
    disk: String,
    verify: bool,
    unmount: bool,
) -> Job {
    let title = format!("Writing {} to {disk}", file_name(&image));
    job(title, true, false, ctx, move |p| {
        let compressed = {
            #[cfg(feature = "archive")]
            {
                compression(&image).is_some()
            }
            #[cfg(not(feature = "archive"))]
            {
                false
            }
        };
        p.phase("Getting the disk ready");
        if unmount {
            host::unmount(&disk)?;
            let left = host::os_uses(&disk);
            if !left.is_empty() {
                return Err(format!("Nothing was written: still in use after unmounting: {}.", left.join("; ")));
            }
        }
        let _lock = host::lock_for_writing(&disk)?;
        let raw = host::raw_path(&disk);
        let mut dst = host::open_path(&raw, AccessMode::ReadWrite)
            .or_else(|e| if raw != disk { host::open_path(&disk, AccessMode::ReadWrite) } else { Err(e) })
            .map_err(|e| e.to_string())?;
        let disk_size = dst.size();
        p.phase("Writing");
        let written = {
            let mut report = |n: u64| {
                if !compressed {
                    p.done.store(n, Ordering::Relaxed);
                }
                !p.stopped()
            };
            let mut w = imaging::DiskWriter::new(dst.as_mut(), &mut report);
            let r = feed(&registry, &image, &mut w, p).and_then(|_| w.finish());
            match r {
                Ok(n) => n,
                Err(e) if imaging::is_stopped(&e) => {
                    return Err("Stopped: the disk is only partly written, and will not work as it is.".into())
                }
                Err(e) => return Err(format!("Writing failed: {e}. The disk is only partly written.")),
            }
        };
        // An old GPT backup at the end of a larger disk would be found again.
        imaging::clear_tail(dst.as_mut(), written).map_err(|e| e.to_string())?;
        let mut note = String::new();
        if verify {
            p.phase("Verifying");
            host::drop_cache(&disk);
            host::drop_cache(&raw);
            let mut report = |n: u64| {
                if !compressed {
                    p.done.store(n, Ordering::Relaxed);
                }
                !p.stopped()
            };
            let mut v = imaging::DiskVerifier::new(dst.as_mut(), &mut report);
            match feed(&registry, &image, &mut v, p).and_then(|_| v.finish()) {
                Ok(_) => note.push_str(" and checked it: it reads back the same"),
                Err(e) if imaging::is_stopped(&e) => note.push_str(" (the check was stopped)"),
                Err(e) => return Err(format!("Written, but the check failed: {e}.")),
            }
        }
        // Only an image with partitions needs the system to read them again.
        let partitioned = dix_core::partition::read_partition_table(dst.as_mut()).ok().flatten().is_some();
        drop(dst);
        drop(_lock);
        let mut message = format!("Wrote {} ({}) to {disk}{note}.", file_name(&image), size(written));
        if written < disk_size {
            message.push_str(&format!(" The {} after it on the disk is unused.", size(disk_size - written)));
        }
        if let Some(n) = host::reread_partition_table(&disk).filter(|_| partitioned) {
            message.push(' ');
            message.push_str(&n);
        }
        Ok(Outcome { message, image: None, partial: false })
    })
}

/// Whether the image file at `image` is on the disk at `disk` (on one of
/// its mounted volumes): writing would destroy it as it is read.
pub fn image_is_on(image: &Path, disk: &str) -> bool {
    let image = std::fs::canonicalize(image).unwrap_or_else(|_| image.to_owned());
    let image = image.to_string_lossy().to_lowercase();
    host::os_uses(disk).iter().any(|u| {
        let at = match u.split_once(" on ") {
            Some((_, at)) => at.to_owned(),
            // Windows: "E: (mounted)".
            None => u.split(' ').next().unwrap_or("").to_owned() + "\\",
        };
        let at = at.to_lowercase();
        let at = at.trim_end_matches(['/', '\\']);
        image.starts_with(&format!("{at}/")) || image.starts_with(&format!("{at}\\")) || at.is_empty()
    })
}

/// Whether two paths are the same file.
pub fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(job: Job) -> Result<Outcome, String> {
        job.result.recv_timeout(Duration::from_secs(60)).expect("the job ended")
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 + i / 4096) as u8).collect()
    }

    #[test]
    fn saves_rescues_and_writes_disks() {
        let dir = std::env::temp_dir().join(format!("dix-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = egui::Context::default();
        let registry = dix_drivers::default_registry();
        let data = pattern(6 << 20);
        let disk = dir.join("disk.img");
        std::fs::write(&disk, &data).unwrap();

        // Saved raw, and as qcow2 (read back through its container).
        let src = open_source(&registry, disk.to_str().unwrap()).unwrap();
        let out = wait(start_save(ctx.clone(), src, dir.join("a.img"), None, Vec::new(), false)).unwrap();
        assert!(out.message.starts_with("Saved a.img"), "{}", out.message);
        assert_eq!(std::fs::read(dir.join("a.img")).unwrap(), data);
        let formats = registry.formats().to_vec();
        let qcow = formats.iter().find(|f| f.name() == "qcow2").cloned();
        let src = open_source(&registry, disk.to_str().unwrap()).unwrap();
        wait(start_save(ctx.clone(), src, dir.join("a.qcow2"), qcow, formats.clone(), true)).unwrap();
        let mut back = open_source(&registry, dir.join("a.qcow2").to_str().unwrap()).unwrap();
        assert_eq!(back.container().as_deref().map(|c| c.starts_with("qcow2")), Some(true));
        assert_eq!(dix_core::device::read_all(back.as_mut()).unwrap(), data);

        // Rescued (nothing bad): the image and its map.
        let src = host::open_path(&disk, AccessMode::ReadOnly).unwrap();
        let out = wait(start_rescue(ctx.clone(), src, dir.join("r.img"), false, 1)).unwrap();
        assert!(out.message.contains("nothing unreadable"), "{}", out.message);
        assert_eq!(std::fs::read(dir.join("r.img")).unwrap(), data);
        let map = earlier_rescue(&dir.join("r.img"), data.len() as u64).unwrap();
        assert_eq!(map.totals().finished, data.len() as u64);

        // Written onto a larger "disk" with old data, and checked.
        let target = dir.join("stick");
        std::fs::write(&target, vec![0xABu8; 9 << 20]).unwrap();
        let t = target.to_str().unwrap().to_owned();
        let out = wait(start_write(ctx.clone(), registry.clone(), disk.clone(), t.clone(), true, false)).unwrap();
        assert!(out.message.contains("checked it"), "{}", out.message);
        let got = std::fs::read(&target).unwrap();
        assert_eq!(&got[..data.len()], &data[..]);
        assert!(got[(8 << 20)..].iter().all(|&b| b == 0));

        // A compressed image, unpacked as it is written.
        if std::process::Command::new("xz").arg("--version").output().is_ok() {
            let st = std::process::Command::new("xz").args(["-k", "-T1"]).arg(&disk).status().unwrap();
            assert!(st.success());
            let xz = dir.join("disk.img.xz");
            let info = describe_image(&registry, &xz).unwrap();
            assert_eq!(info.kind, "xz-compressed image");
            std::fs::write(&target, vec![0x11u8; 9 << 20]).unwrap();
            let out = wait(start_write(ctx.clone(), registry.clone(), xz, t.clone(), true, false)).unwrap();
            assert!(out.message.contains("checked it"), "{}", out.message);
            assert_eq!(&std::fs::read(&target).unwrap()[..data.len()], &data[..]);
        }

        // Too large for the disk.
        std::fs::write(&target, vec![0u8; 1 << 20]).unwrap();
        let e = wait(start_write(ctx, registry, disk, t, false, false)).unwrap_err();
        assert!(e.contains("larger than the disk"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
