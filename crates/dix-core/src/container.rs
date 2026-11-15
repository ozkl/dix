//! Virtual disk containers: files that hold a disk image in their own
//! layout — the disks of virtual machines (qcow2, VHD, VHDX, VMDK, VDI).
//!
//! A [`DiskFormat`] recognises its container and opens it as a
//! [`BlockDevice`] that reads (and, where the format allows it, writes)
//! the *virtual* disk: partitions and file systems inside are then found
//! exactly as on a raw image. The formats themselves live in the drivers
//! crate and are registered with the [`DriverRegistry`].
//!
//! Some containers are made of several files: VMDK extents, and the
//! parent (backing) image of a differencing disk or snapshot. They are
//! opened by name through an opener the caller gives (natively: files in
//! the same folder); without one, only single-file containers open.
//!
//! [`DriverRegistry`]: crate::DriverRegistry

use crate::device::BlockDevice;
use crate::error::{Error, ErrorKind, Result};
use std::sync::Arc;

/// Opens a file that a container refers to, by the name the container
/// gives (relative to the container's folder, or absolute), read-write if
/// `writable`.
pub type FileOpener<'a> = &'a dyn Fn(&str, bool) -> Result<Box<dyn BlockDevice>>;

/// A virtual disk container format.
pub trait DiskFormat: Send + Sync {
    /// Short name ("qcow2", "VHD", …).
    fn name(&self) -> &'static str;

    /// Is `dev` a container of this format?
    fn probe(&self, dev: &mut dyn BlockDevice) -> bool;

    /// The virtual disk inside `dev`. It is writable when `dev` is and the
    /// format (and this variant of it) can be written.
    fn open(&self, dev: Box<dyn BlockDevice>, ctx: &OpenContext<'_>) -> Result<Box<dyn BlockDevice>>;

    /// For formats that can make new disks: the file extension ("qcow2")
    /// and a short description of what [`create`](Self::create) makes.
    fn creates(&self) -> Option<(&'static str, &'static str)> {
        None
    }

    /// Writes an empty container for a disk of `size` bytes (rounded as the
    /// format needs) into `file`, which is empty and can grow. `name` is the
    /// file's name, for formats that refer to it.
    fn create(&self, file: &mut dyn BlockDevice, size: u64, name: &str) -> Result<()> {
        let _ = (file, size, name);
        Err(Error::unsupported(format!("creating {} disks", self.name())))
    }
}

/// What a [`DiskFormat`] may use while opening: the other formats (a
/// parent image can be a container too) and the opener for other files.
pub struct OpenContext<'a> {
    formats: &'a [Arc<dyn DiskFormat>],
    opener: Option<FileOpener<'a>>,
    depth: u32,
}

/// How many parents deep a chain of differencing disks may go.
const MAX_DEPTH: u32 = 32;

impl<'a> OpenContext<'a> {
    pub fn new(formats: &'a [Arc<dyn DiskFormat>], opener: Option<FileOpener<'a>>) -> Self {
        Self { formats, opener, depth: 0 }
    }

    /// Another file of the container (a VMDK extent), as it is on the host.
    pub fn open_file(&self, name: &str, writable: bool) -> Result<Box<dyn BlockDevice>> {
        match self.opener {
            Some(open) => open(name, writable).map_err(|e| e.with_context(format!("'{name}', a part of this disk"))),
            None => Err(Error::new(
                ErrorKind::NotFound,
                format!("this disk is made of several files, and '{name}' cannot be opened from here"),
            )),
        }
    }

    /// The parent (backing) disk of a differencing disk: opened read-only
    /// and, if it is a container itself, unwrapped.
    pub fn open_parent(&self, name: &str) -> Result<Box<dyn BlockDevice>> {
        if self.depth >= MAX_DEPTH {
            return Err(Error::corrupted("the chain of parent disks is too long (or loops)"));
        }
        let dev = match self.opener {
            Some(open) => open(name, false).map_err(|e| e.with_context(format!("the parent disk '{name}'")))?,
            None => {
                return Err(Error::new(
                    ErrorKind::NotFound,
                    format!("this is a differencing disk, and its parent '{name}' cannot be opened from here"),
                ))
            }
        };
        let child = OpenContext { formats: self.formats, opener: self.opener, depth: self.depth + 1 };
        child.unwrap(dev)
    }

    /// `dev` as a virtual disk if a format recognises it, else unchanged.
    pub fn unwrap(&self, mut dev: Box<dyn BlockDevice>) -> Result<Box<dyn BlockDevice>> {
        for f in self.formats {
            if f.probe(dev.as_mut()) {
                return f.open(dev, self).map_err(|e| e.with_context(format!("a {} disk", f.name())));
            }
        }
        Ok(dev)
    }
}

/// How far a conversion has come.
pub trait ConvertProgress {
    /// `done` of `total` bytes copied; false to stop.
    fn report(&mut self, done: u64, total: u64) -> bool;
}

impl<F: FnMut(u64, u64) -> bool> ConvertProgress for F {
    fn report(&mut self, done: u64, total: u64) -> bool {
        self(done, total)
    }
}

/// Copies the disk `src` into a new disk image in `file` (empty, able to
/// grow): a container of `format`, or a raw image without one. `name` is
/// the new file's name (for formats that refer to it), `formats` the
/// registered ones (to open what was made). Chunks of zeros are not
/// written, so the new image holds only the data: unallocated in a
/// container, holes in a raw file. Returns the bytes of data written.
pub fn convert(
    src: &mut dyn BlockDevice,
    file: Box<dyn BlockDevice>,
    format: Option<&dyn DiskFormat>,
    name: &str,
    formats: &[Arc<dyn DiskFormat>],
    progress: &mut dyn ConvertProgress,
) -> Result<u64> {
    convert_ranges(src, file, format, name, formats, None, progress)
}

/// [`convert`], copying only `ranges` of the disk (sorted byte ranges, as
/// the space file systems use); the rest of the new disk reads as zeros.
/// Progress counts the bytes of the ranges. A read error names where it
/// happened.
pub fn convert_ranges(
    src: &mut dyn BlockDevice,
    mut file: Box<dyn BlockDevice>,
    format: Option<&dyn DiskFormat>,
    name: &str,
    formats: &[Arc<dyn DiskFormat>],
    ranges: Option<&[(u64, u64)]>,
    progress: &mut dyn ConvertProgress,
) -> Result<u64> {
    const CHUNK: usize = 1 << 20;
    let size = src.size();
    let mut dst: Box<dyn BlockDevice> = match format {
        Some(f) => {
            let want = size_that_holds(f, size, name, formats)?;
            f.create(file.as_mut(), want, name)?;
            file.flush()?;
            f.open(file, &OpenContext::new(formats, None))?
        }
        None => {
            file.resize(size)?;
            file
        }
    };
    if dst.size() < size {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("the new disk holds {} bytes, less than the {size} to copy", dst.size()),
        ));
    }
    let whole = [(0, size)];
    let ranges = ranges.unwrap_or(&whole);
    let total: u64 = ranges.iter().map(|&(a, b)| b.min(size).saturating_sub(a)).sum();
    let mut buf = vec![0u8; CHUNK];
    let (mut done, mut written) = (0u64, 0u64);
    for &(a, b) in ranges {
        let (mut at, end) = (a, b.min(size));
        while at < end {
            // Chunks on 1 MiB boundaries (so containers get whole clusters).
            let n = (CHUNK as u64 - at % CHUNK as u64).min(end - at) as usize;
            src.read_at(at, &mut buf[..n]).map_err(|e| e.with_context(format!("reading the disk at byte {at}")))?;
            if buf[..n].iter().any(|&b| b != 0) {
                dst.write_at(at, &buf[..n])?;
                written += n as u64;
            }
            at += n as u64;
            done += n as u64;
            if !progress.report(done, total) {
                return Err(Error::new(ErrorKind::Other, "the conversion was stopped"));
            }
        }
    }
    dst.flush()?;
    Ok(written)
}

/// The size to ask `format` for so that the disk it makes holds `size`
/// bytes (some round down, as VHD to whole cylinders). Tried in memory:
/// an empty container is small.
fn size_that_holds(format: &dyn DiskFormat, size: u64, name: &str, formats: &[Arc<dyn DiskFormat>]) -> Result<u64> {
    let mut want = size;
    for _ in 0..8 {
        let mut mem = crate::device::MemoryDevice::new(name, Vec::new());
        format.create(&mut mem, want, name)?;
        let got = format.open(Box::new(mem), &OpenContext::new(formats, None))?.size();
        if got >= size {
            return Ok(want);
        }
        want += (size - got).max(1 << 20);
    }
    Err(Error::new(ErrorKind::InvalidInput, format!("{} cannot hold a disk of {size} bytes", format.name())))
}
