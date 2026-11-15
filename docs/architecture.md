[App overview](../README.md) · [Features](features.md) · [Building](building.md) · [Architecture](architecture.md) · [Format reference](formats.md)

# Architecture

```text
crates/
  dix-core     interfaces + host storage, no UI, (almost) no dependencies
  dix-drivers  file system drivers (implement dix_core::Driver)
  dix-app      egui application
```

The layers only talk through traits:

```text
 dix-app  ─ ui/        egui drawing → emits Action values
          ─ model/     Workspace, Volume, Tab, clipboard, operations (no egui)
          ─ platform/  host I/O: native (rfd + std::fs) | web (upload/download)
              │
              ▼  dyn FileSystem, DriverRegistry
 dix-core ─ FileSystem   mounted volume: read_dir, metadata, read, write_file,
          │              create_dir, remove_*, rename, sync, capabilities()
          ─ Driver       probe(dev) → score, mount(dev, mode), format(dev)
          ─ BlockDevice  read_at / write_at / size / resize / flush
                 ├── MemoryDevice   (uploads, new images; SharedBuffer for download)
                 ├── FileDevice     (image files, raw disks – native only)
                 ├── AlignedDevice  (sector aligned I/O for raw disks)
                 ├── SliceDevice    (a partition inside a disk image)
                 ├── SharedDevice   (one image behind several partition volumes)
                 └── ReadOnlyDevice (enforces "open read-only")
```

* **`Capabilities`** – `READ`, `WRITE`, `CREATE_DIR`, `REMOVE`, `RENAME`,
  `SYMLINKS`, `PERMISSIONS`, `TIMESTAMPS`, `FORMAT`. A driver advertises its
  maximum set (`DriverInfo::capabilities`); a mounted volume reports the
  effective set (reduced for read-only mounts). The UI never asks "which
  driver is this?", only "what can this volume do?".
* **`AccessMode`** – `ReadOnly` / `ReadWrite`, chosen by the user when opening.
  `DriverRegistry::mount` refuses read-write for read-only drivers and wraps
  the device in `ReadOnlyDevice` for read-only mounts.
* **`VfsPath`** – normalised absolute paths inside a volume, so drivers never
  see `..`, `\\` or empty components.
* **`ops`** – driver independent algorithms (recursive copy via `Bundle`,
  recursive delete, unique names), used for paste and import/export.

### Background writes, eject and crash testing

* Copy/move/import/delete run as **background jobs** (`dix-app/src/model/jobs.rs`):
  small steps, each holding the volume lock briefly, with a periodic sync
  and a clean *Cancel* (stops between steps, then syncs). Progress is shown
  in the status bar.
* **Large files are streamed.** Copies between images, *import* from and
  *export* to the host all copy files over 4 MiB in 4 MiB pieces
  (`FileSystem::create_file` + `write_at`; the host side is a `HostFs`, a
  host folder seen as a file system, so import and export are just copies).
  Drivers drop the cached pages of a file as it goes (`release_cache`), so a
  400 MB copy (host → ext4, ext4 → ext4, ext4 → host, zip → ext4) peaks at
  ~75 MB, most of it ext4 writes waiting for the next sync. A copy can be
  cancelled mid-file; a partly copied file is removed. (SimpleFS and
  archives keep written files in memory until they are saved; in the
  browser, uploads and downloads are in memory anyway.)
* Every mounted device is wrapped in a `DetachableDevice`. **Force Eject**
  (right-click an image, or *Image is busy* → *Force Eject…*, after a
  warning) pulls the plug: all further I/O fails
  (even the driver's flush on drop), so the image is left exactly as an
  interrupted write would leave it. Searches only read and never block eject.
* **Eject** and quit call `FileSystem::unmount()` (clean unmount);
  quitting while writes run asks the same way.
* Mounting runs in the background and drivers report through
  `MountOptions::log` (`MountLog`: messages + progress). The app shows the log
  live ("Opening …"), shows a report if the driver warned or failed, and keeps
  the log in the image's properties.

To test crash recovery of a journaling driver: open an image read-write,
start a large copy, right-click the image → *Force Eject…*, then open the
image again and read the mount log. (Force ejecting an idle read-write image
tests the "not cleanly unmounted" path without interrupted writes.)

Contract for drivers (SimpleFS implements it as a reference):

1. A read-write mount marks the volume *in use* on disk before the first
   write (ext4: `needs_recovery`); `unmount()` marks it clean.
2. `mount()` of a volume still marked in use recovers it (e.g. journal
   replay) and logs what it did; read-only mounts must not write (recover in
   memory or refuse, and say so in the log).
3. `sync()` makes everything durable but keeps the volume marked in use.

### Writing a driver

```rust
pub struct MyFsDriver;

impl dix_core::Driver for MyFsDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo { id: "myfs", name: "MyFS", description: "…",
                     capabilities: Capabilities::READ | Capabilities::TIMESTAMPS }
    }
    fn probe(&self, dev: &mut dyn BlockDevice) -> Result<ProbeScore> { /* check magic */ }
    fn mount(&self, dev: Box<dyn BlockDevice>, opts: &MountOptions) -> Result<Box<dyn FileSystem>> { … }
}
```

A read-only `FileSystem` only implements `info`, `capabilities`,
`access_mode`, `read_dir`, `metadata` and `read`; all mutating methods default
to `Unsupported`. Register the driver in `dix_drivers::default_registry()`.
`crates/dix-drivers/src/simplefs/` is a complete read-write example with
tests (the registry tests in `dix-core` contain a minimal read-only one).

## Roadmap

- Archives: stream large members instead of holding them in memory.
- ext4: the resize inode when creating.

Deliberately out of scope for now: repairs that need a decision (reconnecting
to lost+found, clearing inodes, resolving duplicate blocks). Repair stays
safe-only; e2fsck is the tool for those.
- Run file system work on a background thread with progress for large copies.
