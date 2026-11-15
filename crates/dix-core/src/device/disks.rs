//! The disks, partitions and volumes of the host, with names people
//! recognise: `/dev/sdb1  “DATA” · exfat · SanDisk Ultra (USB)` rather
//! than a bare device path (native builds only).
//!
//! Where the details come from (no privileges needed, no extra crates):
//!
//! * Linux: sysfs (`/sys/block`: vendor, model, removable, the bus from the
//!   device's place in the tree) and udev's database (`/run/udev/data`:
//!   file system type and label, GPT partition names), with
//!   `/dev/disk/by-label` and `by-partlabel` as a fallback.
//! * macOS: `diskutil list -plist` and `diskutil info -plist` (media name,
//!   bus protocol, removable, volume names and file systems).
//! * Windows: `IOCTL_STORAGE_QUERY_PROPERTY` for each physical drive
//!   (vendor, product, bus type), `GetVolumeInformationW` and
//!   `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` for drive letters (label, file
//!   system, which physical drive they are on).

/// What a [`HostDevice`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostDeviceKind {
    /// A whole disk (`/dev/sdb`, `/dev/disk6`, `\\.\PhysicalDrive1`).
    Disk,
    /// A partition of a disk (`/dev/sdb1`, `/dev/disk6s5`).
    Partition,
    /// A drive letter (`\\.\E:`).
    Volume,
}

/// A raw device the user may want to open.
#[derive(Debug, Clone)]
pub struct HostDevice {
    pub path: String,
    pub kind: HostDeviceKind,
    /// A one-line summary of the details below, e.g.
    /// `SanDisk Ultra · USB · removable` for a disk,
    /// `“DATA” · exfat · SanDisk Ultra (USB)` for a partition.
    pub description: String,
    pub size: Option<u64>,
    /// The disk a partition or volume is on.
    pub parent: Option<String>,
    /// Vendor and model (`SanDisk Ultra`).
    pub model: Option<String>,
    /// How it is attached: `USB`, `SATA`, `NVMe`, `SD card`, `Virtual`, …
    pub bus: Option<String>,
    pub removable: bool,
    /// Volume label or partition name.
    pub label: Option<String>,
    /// File system or partition table (`ext4`, `exfat`, `GPT`, …).
    pub content: Option<String>,
}

impl HostDevice {
    fn new(path: String, kind: HostDeviceKind) -> Self {
        Self {
            path,
            kind,
            description: String::new(),
            size: None,
            parent: None,
            model: None,
            bus: None,
            removable: false,
            label: None,
            content: None,
        }
    }

    /// "SanDisk Ultra (USB)", the disk's name as part of another line.
    fn disk_name(&self) -> Option<String> {
        match (&self.model, &self.bus) {
            (Some(m), Some(b)) => Some(format!("{m} ({b})")),
            (Some(m), None) => Some(m.clone()),
            (None, Some(b)) => Some(format!("{b} disk")),
            (None, None) => None,
        }
    }
}

/// Lists disks and partitions of the host (best effort, no privileges needed).
pub fn list_devices() -> Vec<HostDevice> {
    let mut list = sys::list();
    describe(&mut list);
    list
}

/// What the operating system uses on the disk or image at `path`: its
/// mounted partitions and swap ("/dev/sdb1 on /media/data"). A partition
/// table must not be rewritten under them. Images count through the loop
/// devices (Linux) backed by them. Best effort: empty when unknown.
pub fn os_uses(path: &str) -> Vec<String> {
    sys::in_use(path)
}

/// Whether [`unmount`] can release a disk's volumes on this system.
pub const CAN_UNMOUNT: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// Unmounts every volume the operating system mounted from the disk at
/// `path`, keeping the disk attached (unlike ejecting it, which makes it
/// disappear until it is plugged in again). macOS: `diskutil unmountDisk`;
/// Linux: `udisksctl unmount` (no root needed for removable disks), else
/// `umount`. Swap is left alone (an error says so).
pub fn unmount(path: &str) -> std::result::Result<(), String> {
    sys::unmount(path)
}

/// Asks the operating system to read the partition table of `path` again
/// after it was rewritten. Returns a note for the user when that did not
/// (or cannot) happen; image files need nothing.
pub fn reread_partition_table(path: &str) -> Option<String> {
    sys::reread(path)
}

/// Held while an image is written over a whole disk (see
/// [`lock_for_writing`]); dropping it lets the system use the disk again.
pub struct WriteLock {
    #[cfg(windows)]
    _volumes: Vec<std::fs::File>,
}

impl Drop for WriteLock {
    /// Gives the disk back to the system (Windows: closing the volume
    /// handles unlocks the volumes, which then mount again).
    fn drop(&mut self) {}
}

/// Gets a disk ready to be written over as a whole. Windows refuses writes
/// into a mounted volume: the disk's volumes are locked and dismounted
/// (as image writers do there) until the lock is dropped, and an error
/// says which volume is in use. Elsewhere volumes are unmounted first
/// ([`unmount`]) and this does nothing.
pub fn lock_for_writing(path: &str) -> std::result::Result<WriteLock, String> {
    #[cfg(windows)]
    {
        sys::lock_volumes(path).map(|v| WriteLock { _volumes: v })
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(WriteLock {})
    }
}

/// Drops what the system keeps in memory of a disk (Linux: its buffer
/// cache, `BLKFLSBUF`; for an image file, its cached pages), so reading it
/// back reads the medium rather than what was just written. Best effort.
pub fn drop_cache(path: &str) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        unsafe extern "C" {
            fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
            fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
        }
        const BLKFLSBUF: std::ffi::c_ulong = 0x1261;
        const POSIX_FADV_DONTNEED: i32 = 4;
        if let Ok(f) = std::fs::File::open(path) {
            // SAFETY: a valid descriptor; BLKFLSBUF takes no argument.
            unsafe {
                ioctl(f.as_raw_fd(), BLKFLSBUF);
                posix_fadvise(f.as_raw_fd(), 0, 0, POSIX_FADV_DONTNEED);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = path;
}

/// The path to write a whole disk through: on macOS the raw node
/// (`/dev/rdisk6` for `/dev/disk6`), which bypasses the buffer cache and is
/// many times faster; elsewhere the path itself.
pub fn raw_path(path: &str) -> String {
    #[cfg(target_os = "macos")]
    if let Some(rest) = path.strip_prefix("/dev/disk") {
        let raw = format!("/dev/rdisk{rest}");
        if std::path::Path::new(&raw).exists() {
            return raw;
        }
    }
    path.to_owned()
}

/// Fills in [`HostDevice::description`].
fn describe(list: &mut [HostDevice]) {
    let disks: Vec<(String, Option<String>)> =
        list.iter().filter(|d| d.kind == HostDeviceKind::Disk).map(|d| (d.path.clone(), d.disk_name())).collect();
    for d in list.iter_mut() {
        let mut parts = Vec::new();
        match d.kind {
            HostDeviceKind::Disk => {
                match (&d.model, &d.bus) {
                    (Some(m), b) => parts.extend([Some(m.clone()), b.clone()].into_iter().flatten()),
                    (None, Some(b)) => parts.push(format!("{b} disk")),
                    (None, None) => parts.push("Disk".to_owned()),
                }
                if d.removable {
                    parts.push("removable".to_owned());
                }
                parts.extend(d.content.clone());
            }
            HostDeviceKind::Partition | HostDeviceKind::Volume => {
                if let Some(l) = d.label.as_deref().filter(|l| !l.is_empty()) {
                    parts.push(format!("“{l}”"));
                }
                parts.extend(d.content.clone());
                let on = d.parent.as_ref().and_then(|p| disks.iter().find(|(path, _)| path == p));
                match on {
                    Some((_, Some(name))) => parts.push(name.clone()),
                    Some((path, None)) => parts.push(format!("on {path}")),
                    None => parts.extend(d.disk_name()),
                }
                if parts.is_empty() {
                    parts.push(if d.kind == HostDeviceKind::Partition { "Partition" } else { "Volume" }.to_owned());
                }
            }
        }
        d.description = parts.join(" · ");
    }
}

/// Tidies a vendor/model string: trims, drops placeholder vendors.
fn clean(s: &str) -> Option<String> {
    let s = s.trim().trim_matches('\0').trim();
    let generic = s.is_empty()
        || s.starts_with("0x")
        || matches!(s.to_ascii_uppercase().as_str(), "ATA" | "GENERIC" | "USB" | "UNKNOWN");
    (!generic).then(|| s.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// "SanDisk" + "SanDisk Ultra" → "SanDisk Ultra" (no repeated vendor).
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn join_vendor(vendor: Option<String>, model: Option<String>) -> Option<String> {
    match (vendor, model) {
        (Some(v), Some(m)) if m.to_lowercase().starts_with(&v.to_lowercase()) => Some(m),
        (Some(v), Some(m)) => Some(format!("{v} {m}")),
        (v, m) => m.or(v),
    }
}

// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod sys {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;

    fn read(path: impl AsRef<Path>) -> Option<String> {
        let s = std::fs::read_to_string(path).ok()?;
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_owned())
    }

    fn sysfs_size(dir: &str) -> Option<u64> {
        let sectors: u64 = read(format!("{dir}/size"))?.parse().ok()?;
        Some(sectors * 512)
    }

    /// udev's properties for a block device ("E:KEY=value" lines).
    fn udev(dir: &str) -> HashMap<String, String> {
        let Some(dev) = read(format!("{dir}/dev")) else { return HashMap::new() };
        let Ok(text) = std::fs::read_to_string(format!("/run/udev/data/b{dev}")) else { return HashMap::new() };
        text.lines()
            .filter_map(|l| l.strip_prefix("E:"))
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    /// `\x20`-style escapes, as udev and /dev/disk/by-label use them.
    pub(super) fn unescape(s: &str) -> String {
        let mut out = Vec::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' {
                if let Ok(v) = u8::from_str_radix(&s[i + 2..i + 4], 16) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// `/dev/disk/by-<what>`: device name → label.
    fn by(what: &str) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for e in std::fs::read_dir(format!("/dev/disk/by-{what}")).into_iter().flatten().flatten() {
            if let Ok(target) = std::fs::canonicalize(e.path()) {
                if let Some(name) = target.file_name() {
                    out.insert(name.to_string_lossy().into_owned(), unescape(&e.file_name().to_string_lossy()));
                }
            }
        }
        out
    }

    /// How the disk is attached, from where it sits in the device tree.
    fn bus(name: &str, dir: &str, props: &HashMap<String, String>) -> Option<String> {
        let real = std::fs::canonicalize(dir).map(|p| p.display().to_string()).unwrap_or_default();
        Some(
            if real.contains("/usb") || props.get("ID_BUS").is_some_and(|b| b == "usb") {
                "USB"
            } else if name.starts_with("nvme") {
                "NVMe"
            } else if name.starts_with("mmcblk") {
                "SD card"
            } else if name.starts_with("loop") {
                "Loop"
            } else if real.contains("/virtio") || name.starts_with("vd") || name.starts_with("xvd") {
                "Virtual"
            } else if real.contains("/ata") {
                "SATA"
            } else if name.starts_with("sr") {
                "Optical"
            } else {
                return props.get("ID_BUS").map(|b| b.to_uppercase());
            }
            .to_owned(),
        )
    }

    /// Whether `dev` is `disk` or one of its partitions (`/dev/sdb1`,
    /// `/dev/nvme0n1p2`, `/dev/loop0p1`).
    fn on_disk(dev: &str, disk: &str) -> bool {
        dev.strip_prefix(disk).is_some_and(|rest| {
            let digits = rest.strip_prefix('p').unwrap_or(rest);
            rest.is_empty() || !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
        })
    }

    pub fn in_use(path: &str) -> Vec<String> {
        let canon = |p: &str| std::fs::canonicalize(p).map(|p| p.to_string_lossy().into_owned()).unwrap_or(p.into());
        let target = canon(path);
        // The disk itself, or the loop devices an image is attached to.
        let mut disks = vec![target.clone()];
        if let Ok(dir) = std::fs::read_dir("/sys/block") {
            for e in dir.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if read(format!("/sys/block/{name}/loop/backing_file")).is_some_and(|b| canon(b.trim()) == target) {
                    disks.push(format!("/dev/{name}"));
                }
            }
        }
        let mut out = Vec::new();
        for line in read("/proc/mounts").unwrap_or_default().lines() {
            let mut f = line.split_whitespace();
            let (Some(src), Some(at)) = (f.next(), f.next()) else { continue };
            let src = canon(src);
            if disks.iter().any(|d| on_disk(&src, d)) {
                out.push(format!("{src} on {}", at.replace("\\040", " ")));
            }
        }
        for line in read("/proc/swaps").unwrap_or_default().lines().skip(1) {
            let Some(src) = line.split_whitespace().next() else { continue };
            let src = canon(src);
            if disks.iter().any(|d| on_disk(&src, d)) {
                out.push(format!("{src} (swap)"));
            }
        }
        out
    }

    pub fn unmount(path: &str) -> std::result::Result<(), String> {
        for u in in_use(path) {
            if let Some(src) = u.strip_suffix(" (swap)") {
                return Err(format!("{src} is used as swap: turn it off first (swapoff {src})"));
            }
            let src = u.split(" on ").next().unwrap_or(&u).to_owned();
            let run = |cmd: &str, args: &[&str]| {
                std::process::Command::new(cmd).args(args).output().ok().filter(|o| o.status.success()).is_some()
            };
            if !run("udisksctl", &["unmount", "--no-user-interaction", "-b", &src]) && !run("umount", &[&src]) {
                return Err(format!("{src} could not be unmounted (is a program using it?)"));
            }
        }
        Ok(())
    }

    pub fn reread(path: &str) -> Option<String> {
        use std::os::unix::fs::FileTypeExt;
        use std::os::unix::io::AsRawFd;
        if !std::fs::metadata(path).is_ok_and(|m| m.file_type().is_block_device()) {
            return None; // an image file
        }
        unsafe extern "C" {
            fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
        }
        const BLKRRPART: std::ffi::c_ulong = 0x125F;
        let f = std::fs::File::open(path).ok()?;
        // SAFETY: a valid descriptor; BLKRRPART takes no argument.
        let r = unsafe { ioctl(f.as_raw_fd(), BLKRRPART) };
        (r != 0).then(|| {
            format!(
                "Linux could not reload the partition table of {path} ({}); unplug and replug the disk \
                 (or run partprobe) to see the new partitions.",
                std::io::Error::last_os_error()
            )
        })
    }

    pub fn list() -> Vec<HostDevice> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir("/sys/block") else { return out };
        let mut disks: Vec<_> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        disks.sort();
        let (labels, partlabels) = (by("label"), by("partlabel"));
        for disk in disks {
            let base = format!("/sys/block/{disk}");
            // Unattached loop devices, RAM disks: nothing to open.
            let empty = sysfs_size(&base).unwrap_or(0) == 0;
            if empty || disk.starts_with("ram") || disk.starts_with("zram") {
                continue;
            }
            let props = udev(&base);
            let mut d = HostDevice::new(format!("/dev/{disk}"), HostDeviceKind::Disk);
            d.size = sysfs_size(&base);
            let vendor = read(format!("{base}/device/vendor")).and_then(|v| clean(&v));
            let model = read(format!("{base}/device/model"))
                .or_else(|| read(format!("{base}/device/name"))) // SD/MMC
                .and_then(|m| clean(&m))
                .or_else(|| props.get("ID_MODEL").and_then(|m| clean(&m.replace('_', " "))));
            d.model = join_vendor(vendor, model);
            d.bus = bus(&disk, &base, &props);
            if disk.starts_with("loop") {
                // What a loop device shows: its backing file.
                d.model = read(format!("{base}/loop/backing_file")).map(|f| format!("Loop: {f}"));
                d.bus = None;
            }
            d.removable = read(format!("{base}/removable")).as_deref() == Some("1");
            d.content = props.get("ID_PART_TABLE_TYPE").map(|t| match t.as_str() {
                "dos" => "MBR".to_owned(),
                t => t.to_uppercase(),
            });
            out.push(d);

            let mut parts: Vec<_> = std::fs::read_dir(&base)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(&disk))
                .collect();
            parts.sort_by_key(|p| natural_key(p));
            for p in parts {
                let dir = format!("{base}/{p}");
                let props = udev(&dir);
                let mut d = HostDevice::new(format!("/dev/{p}"), HostDeviceKind::Partition);
                d.size = sysfs_size(&dir);
                d.parent = Some(format!("/dev/{disk}"));
                d.label = props
                    .get("ID_FS_LABEL_ENC")
                    .map(|l| unescape(l))
                    .or_else(|| props.get("ID_FS_LABEL").cloned())
                    .or_else(|| labels.get(&p).cloned())
                    .or_else(|| props.get("ID_PART_ENTRY_NAME").map(|l| unescape(l)))
                    .or_else(|| partlabels.get(&p).cloned());
                d.content = props.get("ID_FS_TYPE").cloned().filter(|t| !t.is_empty());
                out.push(d);
            }
        }
        out
    }

    fn natural_key(s: &str) -> (String, u64) {
        let digits = s.len() - s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        (s[..s.len() - digits].to_owned(), s[s.len() - digits..].parse().unwrap_or(0))
    }
}

// ---------------------------------------------------------------------------

/// A parsed property list (what `diskutil -plist` prints).
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Plist {
    Dict(Vec<(String, Plist)>),
    Array(Vec<Plist>),
    String(String),
    Integer(i64),
    Bool(bool),
    Other,
}

#[cfg(any(target_os = "macos", test))]
impl Plist {
    pub(crate) fn get(&self, key: &str) -> Option<&Plist> {
        match self {
            Plist::Dict(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub(crate) fn str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Plist::String(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn int(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Plist::Integer(n) => Some(*n),
            _ => None,
        }
    }

    pub(crate) fn bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            Plist::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub(crate) fn array(&self, key: &str) -> &[Plist] {
        match self.get(key) {
            Some(Plist::Array(a)) => a,
            _ => &[],
        }
    }

    /// Parses an XML property list (just enough of XML for that).
    pub(crate) fn parse(xml: &str) -> Option<Plist> {
        let mut p = PlistParser { s: xml, at: 0 };
        // Skip the prolog, DOCTYPE and <plist ...>.
        loop {
            let tag = p.next_tag()?;
            if tag.starts_with("plist") {
                break;
            }
        }
        p.value()
    }
}

#[cfg(any(target_os = "macos", test))]
struct PlistParser<'a> {
    s: &'a str,
    at: usize,
}

#[cfg(any(target_os = "macos", test))]
impl PlistParser<'_> {
    /// The next tag's inside (`dict`, `/dict`, `true/`), skipping text.
    fn next_tag(&mut self) -> Option<&str> {
        let start = self.s[self.at..].find('<')? + self.at;
        let end = self.s[start..].find('>')? + start;
        self.at = end + 1;
        Some(&self.s[start + 1..end])
    }

    /// Text up to the closing tag `</name>`.
    fn text(&mut self, name: &str) -> Option<String> {
        let close = format!("</{name}>");
        let end = self.s[self.at..].find(&close)? + self.at;
        let raw = &self.s[self.at..end];
        self.at = end + close.len();
        Some(
            raw.replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"")
                .replace("&apos;", "'")
                .replace("&amp;", "&"),
        )
    }

    fn value(&mut self) -> Option<Plist> {
        let tag = self.next_tag()?.trim().to_owned();
        self.value_from(&tag)
    }

    fn value_from(&mut self, tag: &str) -> Option<Plist> {
        Some(match tag {
            "dict" => {
                let mut items = Vec::new();
                loop {
                    let t = self.next_tag()?.trim().to_owned();
                    match t.as_str() {
                        "/dict" => break,
                        "key" => {
                            let k = self.text("key")?;
                            items.push((k, self.value()?));
                        }
                        _ => return None,
                    }
                }
                Plist::Dict(items)
            }
            "dict/" => Plist::Dict(Vec::new()),
            "array" => {
                let mut items = Vec::new();
                loop {
                    let t = self.next_tag()?.trim().to_owned();
                    if t == "/array" {
                        break;
                    }
                    items.push(self.value_from(&t)?);
                }
                Plist::Array(items)
            }
            "array/" => Plist::Array(Vec::new()),
            "string" => Plist::String(self.text("string")?),
            "string/" => Plist::String(String::new()),
            "integer" => Plist::Integer(self.text("integer")?.trim().parse().ok()?),
            "true/" => Plist::Bool(true),
            "false/" => Plist::Bool(false),
            "real" | "date" | "data" => {
                self.text(tag)?;
                Plist::Other
            }
            _ => return None,
        })
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use super::*;

    /// `/dev/disk6`, `/dev/rdisk6s1` → `disk6`, `disk6s1`.
    fn ident(p: &str) -> String {
        let n = p.trim_start_matches("/dev/");
        n.strip_prefix('r').filter(|r| r.starts_with("disk")).unwrap_or(n).to_owned()
    }

    pub fn in_use(path: &str) -> Vec<String> {
        if !path.starts_with("/dev/") {
            return Vec::new(); // an image file
        }
        let disk = ident(path);
        let Ok(out) = std::process::Command::new("/sbin/mount").output() else { return Vec::new() };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let (src, rest) = l.split_once(" on ")?;
                let tail = ident(src).strip_prefix(&disk)?.to_owned();
                (tail.is_empty() || tail.starts_with('s'))
                    .then(|| format!("{src} on {}", rest.split(" (").next().unwrap_or(rest)))
            })
            .collect()
    }

    pub fn unmount(path: &str) -> std::result::Result<(), String> {
        if !path.starts_with("/dev/") {
            return Ok(());
        }
        let disk = format!("/dev/{}", ident(path));
        let out = std::process::Command::new("/usr/sbin/diskutil")
            .args(["unmountDisk", &disk])
            .output()
            .map_err(|e| format!("diskutil: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            let msg = if msg.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_owned() } else { msg };
            Err(format!("macOS could not unmount {disk}: {msg}"))
        }
    }

    pub fn reread(path: &str) -> Option<String> {
        path.starts_with("/dev/")
            .then(|| "If macOS does not show the new partitions, eject and reconnect the disk.".to_owned())
    }

    fn diskutil(args: &[&str]) -> Option<Plist> {
        let out = std::process::Command::new("/usr/sbin/diskutil").args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        Plist::parse(&String::from_utf8_lossy(&out.stdout))
    }

    pub fn list() -> Vec<HostDevice> {
        let Some(list) = diskutil(&["list", "-plist"]) else { return fallback() };
        let mut out = Vec::new();
        for disk in list.array("AllDisksAndPartitions") {
            let Some(id) = disk.str("DeviceIdentifier") else { continue };
            let mut d = HostDevice::new(format!("/dev/{id}"), HostDeviceKind::Disk);
            d.size = disk.int("Size").map(|s| s as u64);
            if let Some(info) = diskutil(&["info", "-plist", id]) {
                let media = info.str("MediaName").and_then(clean);
                let model = info.str("DeviceModel").and_then(clean);
                d.model = media.or(model);
                d.bus = info.str("BusProtocol").map(|b| match b {
                    "PCI-Express" if info.bool("SolidState") == Some(true) => "NVMe".to_owned(),
                    "Disk Image" => "Disk image".to_owned(),
                    b => b.to_owned(),
                });
                d.removable = info.bool("RemovableMedia").or(info.bool("Removable")).unwrap_or(false)
                    || info.bool("Ejectable").unwrap_or(false) && info.bool("Internal") == Some(false);
                if info.str("VirtualOrPhysical") == Some("Virtual") && d.model.is_none() {
                    d.model = Some("Synthesized (APFS container)".to_owned());
                }
            }
            d.content = disk.str("Content").map(|c| match c {
                "GUID_partition_scheme" => "GPT".to_owned(),
                "FDisk_partition_scheme" => "MBR".to_owned(),
                c => c.to_owned(),
            });
            out.push(d);
            // Partitions, and the volumes of an APFS container.
            for p in disk.array("Partitions").iter().chain(disk.array("APFSVolumes")) {
                let Some(pid) = p.str("DeviceIdentifier") else { continue };
                let mut part = HostDevice::new(format!("/dev/{pid}"), HostDeviceKind::Partition);
                part.size = p.int("Size").map(|s| s as u64);
                part.parent = Some(format!("/dev/{id}"));
                part.label = p.str("VolumeName").map(str::to_owned);
                part.content = p.str("Content").map(|c| match c {
                    "Apple_HFS" => "HFS+".to_owned(),
                    "Microsoft Basic Data" => "Basic data".to_owned(),
                    c => c.to_owned(),
                });
                out.push(part);
            }
        }
        out
    }

    /// Without diskutil: just the /dev/diskN(sM) names.
    fn fallback() -> Vec<HostDevice> {
        let mut out: Vec<HostDevice> = std::fs::read_dir("/dev")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let rest = name.strip_prefix("disk")?;
                if !rest.chars().next()?.is_ascii_digit() {
                    return None;
                }
                let kind = if rest.contains('s') { HostDeviceKind::Partition } else { HostDeviceKind::Disk };
                Some(HostDevice::new(format!("/dev/{name}"), kind))
            })
            .collect();
        out.sort_by_key(|d| {
            let nums: Vec<u64> = d.path.split(|c: char| !c.is_ascii_digit()).filter_map(|n| n.parse().ok()).collect();
            nums
        });
        out
    }
}

// ---------------------------------------------------------------------------

#[cfg(windows)]
mod sys {
    use super::*;
    use std::ffi::c_void;
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    #[link(name = "kernel32")]
    extern "system" {
        fn DeviceIoControl(
            device: *mut c_void,
            code: u32,
            in_buf: *const c_void,
            in_size: u32,
            out_buf: *mut c_void,
            out_size: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn GetVolumeInformationW(
            root: *const u16,
            name: *mut u16,
            name_size: u32,
            serial: *mut u32,
            max_component: *mut u32,
            flags: *mut u32,
            fs_name: *mut u16,
            fs_name_size: u32,
        ) -> i32;
        fn GetDriveTypeW(root: *const u16) -> u32;
    }

    const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405C;
    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
    const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_CDROM: u32 = 5;

    /// Runs an ioctl into `out`; the number of bytes returned.
    fn ioctl(f: &std::fs::File, code: u32, input: &[u8], out: &mut [u8]) -> Option<usize> {
        let mut returned = 0u32;
        // SAFETY: valid handle; the buffers are valid for the given sizes.
        let ok = unsafe {
            DeviceIoControl(
                f.as_raw_handle(),
                code,
                input.as_ptr() as *const c_void,
                input.len() as u32,
                out.as_mut_ptr() as *mut c_void,
                out.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(returned as usize)
    }

    fn le32(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }

    fn length(f: &std::fs::File) -> Option<u64> {
        let mut b = [0u8; 8];
        ioctl(f, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut b)?;
        Some(u64::from_le_bytes(b)).filter(|&n| n > 0)
    }

    /// A NUL-terminated string at `offset` of a STORAGE_DEVICE_DESCRIPTOR.
    fn c_string(b: &[u8], offset: usize) -> Option<String> {
        if offset == 0 || offset >= b.len() {
            return None;
        }
        let end = b[offset..].iter().position(|&c| c == 0).map_or(b.len(), |n| offset + n);
        clean(&String::from_utf8_lossy(&b[offset..end]))
    }

    fn bus_name(t: u32) -> Option<&'static str> {
        Some(match t {
            0x01 => "SCSI",
            0x02 | 0x03 => "ATA",
            0x04 => "FireWire",
            0x07 => "USB",
            0x08 => "RAID",
            0x09 => "iSCSI",
            0x0A => "SAS",
            0x0B => "SATA",
            0x0C => "SD card",
            0x0D => "MMC",
            0x0E | 0x0F => "Virtual",
            0x10 => "Storage Spaces",
            0x11 => "NVMe",
            _ => return None,
        })
    }

    /// (vendor + product, bus, removable) of a drive.
    fn describe_drive(f: &std::fs::File) -> (Option<String>, Option<String>, bool) {
        // STORAGE_PROPERTY_QUERY { StorageDeviceProperty, PropertyStandardQuery }.
        let query = [0u8; 12];
        let mut b = vec![0u8; 1024];
        let Some(n) = ioctl(f, IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut b) else { return (None, None, false) };
        if n < 36 {
            return (None, None, false);
        }
        let b = &b[..n];
        let vendor = c_string(b, le32(b, 12) as usize);
        let product = c_string(b, le32(b, 16) as usize);
        (join_vendor(vendor, product), bus_name(le32(b, 28)).map(str::to_owned), b[10] != 0)
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn from_wide(b: &[u16]) -> String {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf16_lossy(&b[..end])
    }

    const IOCTL_DISK_UPDATE_PROPERTIES: u32 = 0x0007_0140;

    fn is_drive(path: &str) -> bool {
        path.to_ascii_lowercase().starts_with(r"\\.\physicaldrive")
    }

    /// Volumes with a drive letter on the disk (Windows refuses writes into
    /// a mounted volume).
    pub fn in_use(path: &str) -> Vec<String> {
        if !is_drive(path) {
            return Vec::new();
        }
        list()
            .into_iter()
            .filter(|d| d.parent.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(path)))
            .map(|d| d.path.trim_start_matches(r"\\.\").to_owned() + " (mounted)")
            .collect()
    }

    /// Locks and dismounts every volume with a drive letter on the disk;
    /// the handles keep them so until they are closed.
    pub fn lock_volumes(path: &str) -> std::result::Result<Vec<std::fs::File>, String> {
        use std::os::windows::fs::OpenOptionsExt;
        const FSCTL_LOCK_VOLUME: u32 = 0x0009_0018;
        const FSCTL_DISMOUNT_VOLUME: u32 = 0x0009_0020;
        const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;
        if !is_drive(path) {
            return Ok(Vec::new());
        }
        let mut held = Vec::new();
        for v in list().into_iter().filter(|d| d.parent.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(path))) {
            let letter = v.path.trim_start_matches(r"\\.\").to_owned();
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(FILE_SHARE_READ_WRITE)
                .open(&v.path)
                .map_err(|e| format!("{letter} could not be opened to dismount it: {e}"))?;
            if ioctl(&f, FSCTL_LOCK_VOLUME, &[], &mut []).is_none() {
                return Err(format!("{letter} is in use: close the programs and windows that use it, then try again"));
            }
            if ioctl(&f, FSCTL_DISMOUNT_VOLUME, &[], &mut []).is_none() {
                return Err(format!("Windows could not dismount {letter}"));
            }
            held.push(f);
        }
        Ok(held)
    }

    pub fn unmount(_: &str) -> std::result::Result<(), String> {
        Err("remove the drive letters of its volumes first (Disk Management), or close what uses them".into())
    }

    pub fn reread(path: &str) -> Option<String> {
        if !is_drive(path) {
            return None;
        }
        let f = OpenOptions::new().read(true).write(true).open(path).ok();
        let ok = f.is_some_and(|f| ioctl(&f, IOCTL_DISK_UPDATE_PROPERTIES, &[], &mut []).is_some());
        (!ok)
            .then(|| "Windows did not reload the partition table: reconnect the disk to see the new partitions.".into())
    }

    pub fn list() -> Vec<HostDevice> {
        let mut out = Vec::new();
        for n in 0..32 {
            let path = format!(r"\\.\PhysicalDrive{n}");
            // access_mode(0): query only, works without administrator rights.
            let Ok(f) = OpenOptions::new().access_mode(0).open(&path) else { continue };
            let mut d = HostDevice::new(path, HostDeviceKind::Disk);
            d.size = length(&f);
            (d.model, d.bus, d.removable) = describe_drive(&f);
            out.push(d);
        }
        for letter in b'A'..=b'Z' {
            let letter = letter as char;
            let root = format!(r"{letter}:\");
            if !Path::new(&root).exists() {
                continue;
            }
            let root_w = wide(&root);
            // SAFETY: NUL-terminated wide string.
            let drive_type = unsafe { GetDriveTypeW(root_w.as_ptr()) };
            if !matches!(drive_type, DRIVE_REMOVABLE | DRIVE_FIXED | DRIVE_CDROM) {
                continue; // network drives, RAM disks
            }
            let path = format!(r"\\.\{letter}:");
            let mut d = HostDevice::new(path.clone(), HostDeviceKind::Volume);
            let (mut name, mut fs) = ([0u16; 261], [0u16; 261]);
            // SAFETY: buffers sized as passed; the other out-pointers may be null.
            let ok = unsafe {
                GetVolumeInformationW(
                    root_w.as_ptr(),
                    name.as_mut_ptr(),
                    name.len() as u32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    fs.as_mut_ptr(),
                    fs.len() as u32,
                )
            };
            if ok != 0 {
                d.label = Some(from_wide(&name)).filter(|l| !l.is_empty()).or(Some(format!("{letter}:")));
                d.content = Some(from_wide(&fs)).filter(|f| !f.is_empty());
            } else {
                d.label = Some(format!("{letter}:"));
            }
            if let Ok(f) = OpenOptions::new().access_mode(0).open(&path) {
                d.size = length(&f);
                // VOLUME_DISK_EXTENTS: count, padding, then DiskNumber of the first extent.
                let mut b = [0u8; 64];
                if ioctl(&f, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut b).is_some() && le32(&b, 0) > 0 {
                    d.parent = Some(format!(r"\\.\PhysicalDrive{}", le32(&b, 8)));
                }
                if d.parent.is_none() {
                    (d.model, d.bus, d.removable) = describe_drive(&f);
                }
            }
            out.push(d);
        }
        out
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod sys {
    use super::*;

    pub fn list() -> Vec<HostDevice> {
        Vec::new()
    }

    pub fn in_use(_: &str) -> Vec<String> {
        Vec::new()
    }

    pub fn reread(_: &str) -> Option<String> {
        None
    }

    pub fn unmount(_: &str) -> std::result::Result<(), String> {
        Err("unmounting is not supported on this system".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_name_the_disk() {
        let mut usb = HostDevice::new("/dev/disk6".into(), HostDeviceKind::Disk);
        usb.model = join_vendor(clean("SanDisk "), clean("SanDisk Ultra"));
        usb.bus = Some("USB".into());
        usb.removable = true;
        let mut p = HostDevice::new("/dev/disk6s5".into(), HostDeviceKind::Partition);
        p.parent = Some("/dev/disk6".into());
        p.label = Some("DATA".into());
        p.content = Some("exfat".into());
        let mut bare = HostDevice::new("/dev/disk7s1".into(), HostDeviceKind::Partition);
        bare.parent = Some("/dev/disk7".into());
        let mut list = vec![usb, p, bare];
        describe(&mut list);
        assert_eq!(list[0].description, "SanDisk Ultra · USB · removable");
        assert_eq!(list[1].description, "“DATA” · exfat · SanDisk Ultra (USB)");
        assert_eq!(list[2].description, "Partition");
        assert_eq!(join_vendor(clean("ATA"), clean("Samsung SSD 870")).as_deref(), Some("Samsung SSD 870"));
        assert_eq!(clean("0x1af4"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn udev_escapes() {
        assert_eq!(sys::unescape(r"MY\x20DISK"), "MY DISK");
    }

    /// Shortened `diskutil list -plist` / `info -plist` output.
    #[test]
    fn parses_diskutil_plists() {
        let list = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AllDisks</key>
	<array>
		<string>disk6</string>
		<string>disk6s1</string>
	</array>
	<key>AllDisksAndPartitions</key>
	<array>
		<dict>
			<key>Content</key>
			<string>GUID_partition_scheme</string>
			<key>DeviceIdentifier</key>
			<string>disk6</string>
			<key>OSInternal</key>
			<false/>
			<key>Partitions</key>
			<array>
				<dict>
					<key>Content</key>
					<string>Microsoft Basic Data</string>
					<key>DeviceIdentifier</key>
					<string>disk6s5</string>
					<key>Size</key>
					<integer>31914983424</integer>
					<key>VolumeName</key>
					<string>Tom &amp; Jerry</string>
					<key>VolumeUUID</key>
					<string/>
				</dict>
			</array>
			<key>Size</key>
			<integer>32015679488</integer>
		</dict>
	</array>
	<key>VolumesFromDisks</key>
	<array/>
</dict>
</plist>"#;
        let p = Plist::parse(list).unwrap();
        let disk = &p.array("AllDisksAndPartitions")[0];
        assert_eq!(disk.str("DeviceIdentifier"), Some("disk6"));
        assert_eq!(disk.int("Size"), Some(32015679488));
        assert_eq!(disk.bool("OSInternal"), Some(false));
        let part = &disk.array("Partitions")[0];
        assert_eq!(part.str("VolumeName"), Some("Tom & Jerry"));
        assert_eq!(part.str("VolumeUUID"), Some(""));
        assert_eq!(p.array("VolumesFromDisks"), &[]);

        let info = "<plist version=\"1.0\"><dict><key>BusProtocol</key><string>USB</string>\
            <key>MediaName</key><string>SanDisk Ultra</string><key>RemovableMedia</key><true/>\
            <key>DiskUUID</key><data>AAEC</data><key>Fraction</key><real>0.5</real></dict></plist>";
        let i = Plist::parse(info).unwrap();
        assert_eq!(
            (i.str("MediaName"), i.str("BusProtocol"), i.bool("RemovableMedia")),
            (Some("SanDisk Ultra"), Some("USB"), Some(true))
        );
    }
}
