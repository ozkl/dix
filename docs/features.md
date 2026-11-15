[App overview](../README.md) · [Features](features.md) · [Building](building.md) · [Architecture](architecture.md) · [Format reference](formats.md)

# Features

- Explorer UI: tabs, back/forward/up, breadcrumb address bar (click the empty
  part to type a path), recursive search by **file name** or **file contents**
  (toggle button in the search box; runs on a background thread natively and
  in small per-frame slices in the browser, so the UI never blocks), folder tree, details and icon views,
  sorting, hidden files, preview pane (text/hex), properties, status bar.
- File operations: new folder, rename (F2), delete, cut/copy/paste (also
  between two different images), import from the host, export to the host.
- **Open as image…** (context menu of a file in a volume): a file holding an
  image of its own — the SquashFS of a live CD (`casper/filesystem.squashfs`),
  a disk image inside an ISO or archive — is read into memory and opened like
  an uploaded image (partitions, driver, mode), without exporting it first.
- **Convert disk image…** (an image's menu): the whole disk into raw, qcow2,
  VHD, VHDX, VMDK, VDI or DMG, holding only the data (see *Virtual machine
  disks*).
- **Recover deleted files…** (an NTFS, FAT or exFAT image's menu): what the
  file system still remembers of deleted files opens as a read-only volume
  of its own — the original folders, holding only what was deleted, each
  file marked *deleted · intact*, *n% overwritten* or *data lost* — to
  browse, preview and export from (see [Recovering deleted files](formats.md#recovering-deleted-files)).
- **Scan for photos and videos…** (any image's menu, or *Scan for photos…*
  in the open dialog for an image that does not even open): finds photos and
  videos by their content, after formatting or when the file system is
  damaged — JPEG, PNG, GIF, WebP, HEIC/AVIF, camera RAW (CR2, CR3, NEF,
  ARW, DNG, ORF, PEF), TIFF, MP4, MOV, 3GP, AVI — into a read-only volume
  with a folder per type, photos named by the date they were taken (see
  [Recovering deleted files](formats.md#recovering-deleted-files)).
- **Find lost partitions** (*Partitions…* in the open dialog): file systems
  whose partition was deleted, or whose whole table was wiped, are found in
  the free space (NTFS, FAT, exFAT, ext2/3/4, HFS+, APFS, XFS, Btrfs, LUKS,
  swap) with their label and size, to open read-only or put back in the
  table with *Restore* (see [Finding lost partitions](formats.md#finding-lost-partitions)).
- **Copy disk…** (desktop builds, in the sidebar): **save a disk as an
  image** — raw or a VM disk (qcow2, VHDX, …), optionally only the space its
  file systems use — with a **rescue mode for failing disks** that skips
  unreadable places, comes back for them sector by sector and keeps a
  ddrescue-compatible map so the copy can be stopped and continued; and
  **write an image to a disk** (a USB stick, an SD card): raw, ISO, VM disks
  and compressed images (`.img.xz`, `.gz`, `.zst`, `.bz2`, unpacked as they
  are written), checked by reading the disk back (see [Copying disks](formats.md#copying-disks)).
- **Hex viewer** (*View hex* on a file, *View raw hex* on an image): offset,
  hex and text rows in a window of its own (several at once), for files and
  for an image's raw bytes (the whole disk image, or the volume's
  partition; sector starts marked). It scrolls through disks of any size
  (rows are drawn from a top row, not laid out), reads 64 KiB at a time,
  goes to an offset (decimal, `0x…`, or `s<sector>`), finds hex bytes or
  quoted text forward from the selection (64 MiB per click, then on), and
  shows the integers at the selected byte (u8 … u64, both byte orders).
- **Open read-only or read-write.** Even when a driver supports writing the
  user can choose read-only; the device is then wrapped so that nothing can be
  written, whatever the driver does. Buttons are enabled from the volume's
  capabilities, so read-only drivers just work.
- Host access:
  - native: image files are accessed in place with positioned I/O; raw disks
    and partitions (`/dev/sdX`, `/dev/diskN`, `\\.\PhysicalDriveN`, `\\.\E:`)
    are listed in the open dialog **with names you recognise** — vendor and
    model, bus, removable, volume label and file system — and accessed with
    sector-aligned I/O:

    ```text
    /dev/disk6        SanDisk Ultra · USB · removable · GPT      32.0 GB
        /dev/disk6s1  “EFI” · EFI · SanDisk Ultra (USB)          209 MB
        /dev/disk6s2  “DATA” · ExFAT · SanDisk Ultra (USB)       31.8 GB
    ```

    (Linux: sysfs and udev's database; macOS: `diskutil … -plist`; Windows:
    the storage property query and volume information. `cargo run -p
    dix-core --example list_devices` prints the list.)
  - **Partitions** (GPT and MBR, including logical partitions): choose a
    whole-disk image or device and the open dialog lists its partitions —
    number, type, GPT name, size and the file system found in each — with the
    first one a driver recognises preselected. A dropped or command-line
    disk image opens that partition directly. Several partitions of one image
    can be open at once, read-write too: they share one open file (and its
    lock). Check and Repair work per partition.
  - **Partition editing** (*Partitions…* in the open dialog): new GPT/MBR
    tables, create, delete, retype, rename and format partitions, with every
    change listed before it is written and a refusal while the disk is in
    use. When the system has volumes of the disk mounted (a USB stick
    plugged into a Mac), *Unmount and apply* unmounts them first and keeps
    the disk connected (macOS: `diskutil unmountDisk`; Linux: `udisksctl
    unmount`, else `umount`). Don't *Eject* it in the Finder: that
    disconnects the whole disk until it is plugged in again. Writing to a
    raw disk needs administrator rights (`sudo`); image files and folders
    the app creates while running with sudo are given to the user who ran
    it (`SUDO_UID`), so they open without sudo later. *New image…* can make a
    partitioned disk image. In the browser it edits the uploaded image (raw
    or a VM disk) in memory; closing the editor returns to the open dialog
    with the new partitions, and the image can be downloaded afterwards.
  - web: images are uploaded into memory, edited there and downloaded again
    ("Download image"); exported files are downloaded by the browser.
- Drag & drop depends on context: with a writable image open, dropped files
  are copied into the current folder; otherwise the open dialog appears with
  the dropped image filled in, so you choose read-only or read-write. While
  the open dialog is showing, a drop always fills that dialog (replacing what
  it had) instead of opening another one.
- **Eject** (⏏ next to each image) is a *clean unmount*. While a write is
  running it says "Image is busy" (Force Eject… / Cancel); it asks about
  unsaved in-memory images, and keeps the image open if writing the last
  changes fails. Quitting is refused the same way (Quit Anyway / Cancel); in
  the browser a "Leave site?" warning protects unsaved in-memory images.
- **Force Eject…** (right-click an image) is an *unclean unmount*: the image
  is disconnected at once, like pulling out a disk. Always available, after
  a warning (a stronger one while writing), to test crash recovery.
- **Images are locked while open**: read-write exclusively, read-only shared
  (other readers allowed, writers refused). Windows additionally denies other
  writers via the share mode; Linux opens block devices read-write with
  `O_EXCL` (refused while mounted). The app never opens the same image twice.
- Create new images (any driver with the `FORMAT` capability).
- *[Virtual machine disks](formats.md#virtual-machine-disks)* open like raw images: qcow2 (QEMU), VHD and
  VHDX (Hyper-V, Virtual PC, Azure), VMDK (VMware) and VDI (VirtualBox),
  with the partitions and file systems inside. Snapshots and differencing
  disks are read through their parent (found next to them), and most
  formats can be written — see [Virtual machine disks](formats.md#virtual-machine-disks).
  The image's details show the format ("qcow2 v3, 64 KiB clusters").
  *New image…* can make new ones: choose a **Disk format** — qcow2, VHD
  (dynamic), VHDX (dynamic), VMDK (monolithic sparse), VDI (dynamic) or DMG
  — and the file only
  grows by what is written to it (`qemu-img check` accepts them).
- **Apple disk images** (`.dmg`, UDIF) open the same way: raw, zlib,
  bzip2, LZFSE, LZMA and ADC-compressed images, with the Apple partition map
  or GPT inside (an HFS+ or APFS volume opens directly). They can be
  changed and made with *New image…* too: the image is written again,
  zlib-compressed, when it is saved.
- **Split files open from any part**: multi-volume RAR (`x.part1.rar` … or
  `x.rar`, `x.r00` …) and ARJ (`x.arj`, `x.a01` …), split zip (`x.z01` … `x.zip`), split 7z
  (`x.7z.001` …) and any file cut into numbered pieces (`x.img.001` …, as
  made by `split`, HJSplit or 7-Zip) are found next to the part you open and
  read as one, read-only. The open dialog says how many parts it found.
