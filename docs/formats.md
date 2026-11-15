[App overview](../README.md) · [Features](features.md) · [Building](building.md) · [Architecture](architecture.md) · [Format reference](formats.md)

# Format reference

Detailed capabilities, limitations, implementation notes and validation for each
driver and disk operation. Source paths are relative to the repository root
unless a driver-specific context is stated.

- [ext2/3/4](#ext234)
- [Partition tables](#partition-tables)
- [FAT and exFAT](#fat-and-exfat)
- [NTFS](#ntfs)
- [HFS+ and HFS](#hfs-and-hfs)
- [APFS](#apfs)
- [XFS](#xfs)
- [Btrfs](#btrfs)
- [ISO 9660](#iso-9660)
- [UDF](#udf)
- [SquashFS](#squashfs)
- [littlefs](#littlefs)
- [Recovering deleted files](#recovering-deleted-files)
- [Copying disks](#copying-disks)
- [Archives](#archives)
- [Virtual machine disks](#virtual-machine-disks)
- [SimpleFS](#simplefs)

## ext2/3/4

`crates/dix-drivers/src/ext4/` is the ext4 driver of the
[ruxis](https://github.com/ozkl/ruxis) kernel (MIT, see
`ruxis/LICENSE-ruxis`):

* `ruxis/` — the kernel's driver code, unchanged except for its `crate::`
  paths and a few clearly marked helpers at the end of `ruxis/mod.rs`
  (`mark_in_use`, `checkpoint`, `release`, `fs_of`);
* `kernel/` — a small userspace stand-in for the kernel facilities it uses:
  `Disk` over a `BlockDevice`, the buffer and page caches (heap pages instead
  of frames), locks, errno, and `kinfo!` routed to the mount log;
* `mod.rs` — the `Driver`/`FileSystem` adapter (paths → inodes).

Supported: block sizes 1–4 KiB, extents and indirect blocks, `64bit`,
`flex_bg`, `meta_bg`, `metadata_csum`, htree directories, extended
attributes, the jbd2 journal (replayed at mount and shown in the mount log)
and the orphan list. Features it does not write make the mount read-only;
ones it cannot read refuse the mount. A **read-only** mount of a volume that
needs recovery replays the journal into an in-memory overlay: you see the
recovered state, the image is not modified.

**Creating images** (`src/ext4/mkfs.rs`, *New image… → ext2/3/4*, or
`cargo run -p dix-drivers --example mkfs -- ext4 disk.img 64M LABEL [--block-size N] [--meta-bg]`):

| Type | Features |
|------|----------|
| ext2 | rev 1, 256-byte inodes, `filetype`, `sparse_super`, `large_file` |
| ext3 | + jbd2 journal (inode 8), `dir_index`, `ext_attr` |
| ext4 | + `extent`, `huge_file`, `dir_nlink`, `extra_isize`, `metadata_csum` (crc32c), `64bit`, `flex_bg` (16 groups), journal `csum_v3` + `64bit` |

The layout follows ruxis's ext2 image builder (`xtask/src/ext2.rs`); block
size, inode ratio and journal size follow mke2fs's defaults. ext4 packs the
bitmaps and inode tables of each 16 groups together (`flex_bg`), uses
64-byte group descriptors (`64bit`: past 2^32 blocks / 16 TiB) and leaves
every group but the first uninitialised (`INODE_UNINIT`, `BLOCK_UNINIT`
where nothing else lives in it), like mke2fs's lazy init: a 2 TiB ext4 is
made in about a second, writing ~150 MB. ext2/ext3 stay 32-bit with
per-group metadata (up to 16 TiB) and zero every inode table.

`meta_bg` (any type) is turned on by itself when the group descriptor table
would not fit in a group (past 2^30 blocks with 1 KiB blocks, 2^33 with
2 KiB, 2^36 with 4 KiB), or on request (`--meta-bg`, `mkfs::Options`): each
block of descriptors is then kept in the first, second and last group it
describes rather than the whole table after every superblock copy. Not
used: the resize inode.

Tests (`src/ext4/tests.rs`) build images with `mke2fs -d`, write through the
driver and let `e2fsck -fn` and `debugfs` judge the result (ext2, ext3,
ext4), and cover journal replay after a crash, forced eject and clean
checkpoints. Images made by our mkfs are checked by `e2fsck` right away and
after thousands of writes through the driver (hashed directory indexes
included, verified with `debugfs htree`), and their journal is replayed after
a simulated crash. A sparse 40 GiB ext4 checks the flex/uninit layout
(directories spread into uninitialised groups, a file larger than a group);
`meta_bg` images are checked on all three types and by their layout;
`cargo test --release -p dix-drivers -- --ignored over_2_32` makes ext4s
with more than 2^32 blocks (8 TiB with 2 KiB blocks, and 4 TiB with 1 KiB
blocks, which needs `meta_bg`). The tests are skipped when e2fsprogs is not
installed.

**Checking** (`src/ext4/fsck.rs`, *right-click an image → Check file
system…*, or *Open image… → Check* for an image that is not open or does not
mount): an e2fsck-style, read-only check in the same five passes — inodes,
blocks and sizes; directory structure; connectivity; reference counts;
group summaries (bitmaps, free counts, all checksums). It parses the disk
itself, independently of the driver; only a pending journal is replayed with
the driver's code, into memory. An image file is ejected cleanly for the
check and opened again afterwards; an in-memory image is checked from a
snapshot.

**Repairing** (the *Repair* button of a check report) follows `fsck -p`
("preen"): it only acts when *every* problem found is one whose fix is fully
determined — group and superblock counts, block and inode bitmaps (rebuilt
from what is actually in use), inode link counts, descriptor checksums of
sane descriptors — and then also replays a pending journal for real and marks
the volume clean. Anything that needs a decision (duplicate or out-of-range
blocks, broken entries, unattached inodes, inode/superblock checksums, ...)
means nothing is changed at all; use e2fsck for those. A repair is always
followed by a full check, and the tests require e2fsck to find a repaired
image clean and an unrepairable one byte-identical.

The tests corrupt images with `debugfs -w` (link counts, bitmaps in both
directions, cleared and unattached inodes, group counts, sizes, `i_blocks`,
file types, shared blocks, broken entries, checksums) and require both
`e2fsck -fn` and our checker to object; on clean images from e2fsprogs
(`flex_bg`, `64bit`, `resize_inode`, `meta_bg`, `uninit_bg`, 1 KiB blocks,
ext2/3) and from our mkfs + driver both must be silent.

## Partition tables

`dix-core/src/partition/` reads **GPT** — the header at LBA 1 (with
512- or 4096-byte sectors), its CRC32 and that of the entry array, falling
back to the backup header in the last sector when the primary is damaged —
and **MBR**, following the EBR chain of an extended partition for logical
partitions (numbered from 5, as Linux does). A FAT/NTFS/exFAT boot sector,
which also ends in `55 AA`, is not mistaken for an MBR. Partition types get
readable names (Linux filesystem, EFI system, Microsoft basic data, Apple
APFS, …). `partition::slice` turns a partition into a `SliceDevice`, so
drivers mount it like any image.

Tests make disks with `sgdisk` and `sfdisk` (including logical partitions
and a wiped primary GPT header) and compare every start, size, type and name
with what those tools report; an ext4 made by `mke2fs` inside a partition
is mounted and written through the slice, and one made by our mkfs inside a
logical partition passes `e2fsck`.

### Editing partitions

*Open image… → Partitions…* shows a disk or disk image as a bar and a list
of its partitions and free space, and edits it like a small GParted:

* **New table** (GPT or MBR, replacing what is there), **new partition** in
  free space (size, type, GPT name; on MBR logical ones go in an extended
  partition, which it can also create), **delete**, **type**, **name**,
  **bootable** (MBR active flag / GPT legacy BIOS bootable), and
  **format** — a new or existing partition gets a file system from our
  mkfs (ext2/3/4, FAT12/16/32, exFAT), with a label; a new partition gets
  the type that suits its file system unless one is chosen.
* Nothing is written until **Apply**: the changes are listed, confirmed,
  then written in one go on a background thread — new partitions' first
  MiB is zeroed (so an old file system there is not found again), the
  table is written, the file systems created. *Undo all* forgets them.
* **Safety**: writing is refused while a partition of the disk is open in
  the app or used by the operating system — mounted or swap, found through
  `/proc/mounts` and the loop devices backing an image on Linux, `mount` on
  macOS, drive letters on Windows (which also refuses writes into mounted
  volumes). Raw disks need administrator rights. Afterwards the OS is asked
  to reload the table (`BLKRRPART` on Linux, `IOCTL_DISK_UPDATE_PROPERTIES`
  on Windows); if it cannot, the user is told to reconnect the disk.
* Partitions start on 1 MiB boundaries. GPT tables are written with both
  headers, entry arrays and CRCs, keeping the disk GUID and each kept
  partition's GUID and attribute bits; writing to an image that has grown
  moves the backup header to the new end. MBR tables keep the boot code and
  disk signature, write the EBR chain in disk order (logical partitions
  are renumbered from 5 as Linux would) and wipe any old GPT headers.
* **In the browser** the editor works on the uploaded image in memory
  (also inside qcow2, VHD, VHDX, VMDK, VDI and DMG containers): *Apply*
  writes into it, and *Close* goes back to the open dialog listing the new
  partitions. Download the image to keep it.
* *New image…* can also make a **disk image**: a GPT or MBR table with one
  partition holding the new file system.

The library (`partition::edit::Layout`) is independent of the app. Its tests
have `sfdisk --verify` and `sgdisk -v` check every table we write (GPT with
512- and 4096-byte sectors, MBR with logical partitions, switching between
the two) and compare starts, sizes, types, GUIDs, names and attributes with
`sfdisk -d`; tables made by `sgdisk` and `sfdisk` are edited (partitions
deleted, added in the gaps, renamed) without disturbing the others. The
app's tests partition and format images and check the file systems with
`e2fsck` and `fsck.fat`. Resizing and moving partitions are not offered.

### Finding lost partitions

*Find lost partitions* in the partition editor reads the disk's free space
(the whole disk when it has no table) sector by sector, as TestDisk does,
for the start of a file system: a boot sector (NTFS, exFAT, FAT12/16/32),
a superblock at its usual place (ext2/3/4 and HFS+ 1 KiB in, Btrfs 64 KiB
in, the Linux swap signature at the end of its first page), or a header at
the start (APFS, XFS, LUKS). The file system's own fields give its size,
and where there is a driver it must open the volume (which also gives the
label), so a stray signature in a file's data is not taken for a
partition. After a find the scan continues after its end, which is quick
and keeps a file system's backup boot sector or superblock copies from
being found as more partitions. It runs in the background with progress;
*Stop* keeps what was found (`dix_drivers::recover::partitions::find`).

Each find can be **opened** read-only straight away, to look inside or copy
files off before changing anything, or **restored**: a partition is added
at its exact start and size, with the type that suits it and its label as
the GPT name (on a disk with no table, make one first with *New table*).
Restoring is an ordinary change — listed, confirmed, written with *Apply*
— except that the restored partition's first MiB is not zeroed.

The test makes a GPT disk with FAT32, ext4, NTFS, swap and exFAT made by
`mkfs.*`, deletes one partition with `sgdisk -d` (only it is found, in the
gap) and then wipes the table with `sgdisk --zap-all` (all five are found,
with exact starts, sizes and labels). The app's test wipes a disk, finds
its ext4 and FAT32 partitions, restores them into a new table and reads
their files back.

## FAT and exFAT

`dix-drivers/src/fat/` is written for this project (not a wrapper around
`rust-fatfs`, whose last release dates from 2023 and has data bugs fixed
only on its unreleased master; no usable exFAT crate exists). One driver,
"FAT / exFAT", read-write, with mkfs and a checker:

* **FAT12/16/32**: the type from the cluster count (as the specification
  requires), long file names (checksummed; stale ones ignored), 8.3 names
  in code page 437 with the NT case flags, `BASIS~N.EXT` short names for
  new long ones. The FAT is paged in on demand and written to every copy;
  files are written in place. Before the first change the volume is marked
  in use (the boot sector's dirty bit, as Linux and `fsck.fat` use, and
  FAT entry 1's clean bit, as Windows uses); a clean unmount clears them,
  and a volume found dirty is reported when opened. *mkfs* follows
  Microsoft's specification (cluster sizes, FSInfo, backup boot sector).
* **exFAT**: the boot region and its checksum, the allocation bitmap,
  the up-case table (name comparisons and hashes use the volume's own),
  entry sets with their checksums, contiguous files ("NoFatChain") kept
  contiguous while they can grow in place and turned into FAT chains when
  not, the VolumeDirty flag, files over 4 GiB. *mkfs* uses Microsoft's
  recommended cluster sizes.
* **Check** (both): chains (shared, looping, broken, too long or short),
  directory structure, orphaned long names, lost clusters, the FAT copies /
  allocation bitmap, FSInfo, checksums. **Repair** is safe-only, as for ext4:
  lost clusters, over-long chains, `.`/`..`, orphaned long names, FAT
  mirrors, bitmap bits, free counts and dirty flags are fixed; shared or
  broken chains and files larger than their clusters are reported, not
  touched.
* Times are stored as UTC (FAT has no time zone).

Tests (`fat/tests.rs`, `fat/exfat/tests.rs`) use the reference tools:
FAT12/16/32 images filled by `mcopy` read back exactly, and after creating,
replacing, growing, sparse writes, renames, folder moves and mass deletes
`fsck.fat` is clean and `mcopy` extracts the expected tree; exFAT volumes
written through `exfat-fuse` (a second implementation, mounted on a loop
device; skipped where FUSE is unavailable) read back exactly, and after our
writes (fragmented files, a growing folder, …) `fsck.exfat` is clean and
`exfat-fuse` sees the expected tree. Our mkfs (FAT 64 KiB – 5 GiB, exFAT
1 MiB – 5 GiB) passes `fsck.fat` / `fsck.exfat` and works with the other
tools both ways. For each kind of damage the checkers agree with
`fsck.fat` / `fsck.exfat`, our repairs leave both clean with the files
unchanged, and what needs a decision is left alone.

## NTFS

`dix-drivers/src/ntfs/` is a read-only NTFS driver written for this project
(the Linux kernel's `ntfs3` is GPL-2.0 and could not be used here under
MIT/Apache, and the read-only `ntfs` crate has not been released since 2023):

* **MFT records** with their update-sequence fixups (torn writes are
  detected), resident and non-resident attributes, runlists, and files
  spread over several records (`$ATTRIBUTE_LIST`, extents merged in VCN
  order) — the MFT itself included.
* **Directories**: the `$I30` B+ tree (`$INDEX_ROOT` and "INDX" blocks);
  lookups go down the tree comparing names through the volume's `$UpCase`,
  case-insensitively like Windows, preferring an exact match. DOS 8.3
  aliases are not listed twice.
* **File data**: sparse files, data past the initialised size (zeroes),
  **LZNT1-compressed** files (per compression unit, the last one cached);
  **alternate data streams** are listed as hidden `file:stream` entries and
  can be read, previewed and exported like files; **hard links** show in
  each folder that names them.
* **Links**: symbolic links and junctions (reparse points; absolute targets
  are shown as Windows paths, e.g. `C:\Users`), and the Interix and WSL
  links that ntfs-3g and WSL make.
* Encrypted (EFS), WOF-compressed (CompactOS) and deduplicated files are
  listed but cannot be read; reading one says why.
* Label, free space (`$Bitmap`), times from `$STANDARD_INFORMATION`. The
  metadata files (`$MFT`, `$Bitmap`, …) and files marked hidden or system
  show with "Show hidden". A volume marked dirty, or with Windows
  hibernated on it (Fast Startup), is reported when opened.
* **Check** (read-only, like `chkdsk` without `/f`): the boot sector against
  its backup, every MFT record in use (signature, fixups, in-use flag),
  `$MFTMirr`, every run (inside the volume, not shared) against `$Bitmap`,
  every directory index (readable, in order, entries naming in-use records
  with the right sequence number and a matching name), names against index
  entries, and orphaned records. Nothing is repaired: use `chkdsk /f`.

Tests (`ntfs/tests.rs`) make volumes with `mkntfs` (512-byte, 4 KiB and
64 KiB clusters), fill them through `ntfs-3g` (a second implementation,
mounted on a loop device; skipped where FUSE is unavailable) — Unicode
names, a 600-file folder, hard and symbolic links, a stream, a sparse file,
compressed files, a file in 2400 pieces whose runlist needs extension
records, and native symlinks and junctions written as raw reparse data —
and read back exactly what was written. The checker finds no errors on
those volumes and finds each kind of damage (torn record, record marked
free, cluster marked free, mirror mismatch, backup boot sector), and the
rest of a damaged volume stays readable.

## HFS+ and HFS

Mac OS Extended (HFS+, and HFSX, its case-sensitive variant) and the older
Mac OS Standard (HFS), read-only (`crates/dix-drivers/src/hfsplus/`). The
volume header points to the special files; three are B-trees: the
*catalog* (every file and folder under its parent's ID and its name, a
folder's entries following its thread record), the *extents overflow* file
(a fork's extents after the eight its record holds) and the *attributes*
file (extended attributes).

What is read: data forks, resource forks (a hidden `name:rsrc` next to the
file), symbolic links, hard links to files and to folders (Time Machine's),
BSD owners, modes and dates, Finder's invisible flag, HFSX names (exact
case; HFS+ names are matched without case, as macOS does), names composed
to NFC (HFS+ stores them decomposed), HFS+ volumes in an HFS wrapper, and
classic HFS with Mac Roman names. A journal with transactions not yet
written in place is reported, not replayed.

**Files compressed by macOS** (`decmpfs`, `crates/dix-drivers/src/decmpfs/`):
an empty data fork, the `UF_COMPRESSED` flag and a `com.apple.decmpfs`
attribute holding the data (small files) or pointing to 64 KiB chunks in
the resource fork. zlib, LZVN and LZFSE are decoded here (Apple's LZFSE
format written from its specification; checked against Apple's reference
`lzfse` encoder on inputs up to 2 MB); LZBITMAP and iCloud placeholders
(dataless files) are not. APFS compresses files the same way.

The test volumes (`crates/dix-drivers/testdata/hfs/`) were written by
Linux's `hfs` and `hfsplus` drivers in a small VM (`scripts/testdata/vm.sh`,
QEMU with an Ubuntu kernel and busybox; `scripts/testdata/make-hfs.sh`
makes them again) and pass `fsck.hfsplus`. Each comes with a manifest of
what Linux saw — every entry's type, size, mode, owner, time, link target
and MD5 — which the tests compare with: 600 files in one folder (a
catalog several levels deep), files in more than eight extents, hard
links, symbolic links, non-ASCII names, HFSX, a journaled volume in an HFS
wrapper, classic HFS, and files compressed with zlib, LZVN and LZFSE.

## APFS

The Apple File System, read-only (`crates/dix-drivers/src/apfs/`). The
container's newest valid superblock (the checkpoint area is searched for
the highest transaction) leads through the container's object map to the
volumes; each volume's object map leads to the nodes of its file system
tree, whose records — inodes, directory entries, extended attributes,
file extents — are keyed by (object ID, type). Every object's Fletcher-64
checksum is checked.

What is read: files (sparse ones too), folders, symbolic links, hard links,
owners, modes, dates, case-insensitive and case-sensitive volumes (names
found either way, composed or decomposed), extended attributes inline or
as data streams, files compressed by macOS (the same decmpfs as HFS+; in
APFS the resource fork is the `com.apple.ResourceFork` attribute), and
sealed system volumes (macOS 11 and later: physical file system trees,
extents in their own tree). A container with several volumes (a Mac's
system disk: "Macintosh HD", "Data", "Preboot", …) shows each as a folder;
one with a single volume shows its files at the root. The volume details
list each volume's case sensitivity, role and counts. Not read:
FileVault-encrypted volumes (listed, refused), snapshots (only the live
file system is shown), Fusion drives.

The test containers (`crates/dix-drivers/testdata/apfs/`) were written by
the linux-apfs-rw driver in the same VM as the HFS ones
(`scripts/testdata/make-apfs.sh` builds the driver for that kernel and
makes them again) and pass `apfsck`. As with HFS+, each has a manifest of
what Linux saw, compared entry by entry: 600 files in one folder, files in
many extents, a sparse file, hard and symbolic links, composed and
decomposed names, a case-sensitive volume, and files compressed with zlib,
LZVN, LZFSE and stored, inline and in resource forks — whose content the
Linux driver decompressed independently for the manifest. Not covered by
real images: containers with several volumes, sealed and encrypted volumes
(no Linux tool makes them).

## XFS

XFS v4 and v5 (with metadata checksums), read-only
(`crates/dix-drivers/src/xfs/`). An inode number says where the inode is
(allocation group, block, slot), and block numbers in a file's map are
per allocation group too. A file's data fork is held in the inode (short
folders, short link targets), a list of extents, or an extent B+tree;
unwritten (preallocated) extents and holes read as zeros. Folders are read
in every form — short form in the inode, a single directory block, leaf
and node folders (their data blocks, below 32 GiB of the folder's address
space) — with or without file types in entries; long link targets are in
blocks of their own. What is shown: files, folders, symbolic links, hard
links, devices and FIFOs, owners, modes, times (also the 64-bit
"bigtime" ones) and the creation time on v5. Not read: extended
attributes, files on a separate realtime device; the journal is not
replayed. Volume details: format and features (reflink, reverse mapping,
…), allocation groups, inode size, label.

## Btrfs

Btrfs, read-only (`crates/dix-drivers/src/btrfs/`). Everything is in
copy-on-write B-trees addressed by logical bytes: the chunk tree maps them
to the device (its own chunks are in the superblock), the root tree lists
the other trees, among them a file system tree per subvolume. A folder's
entries come from its `DIR_INDEX` items; a file's from its `EXTENT_DATA`
items: inline bytes, a range of a data extent on disk (compressed with
zlib, LZO — Btrfs's own framing of LZO1X segments — or zstd), preallocated
space or holes (stored, or implied with the "no holes" feature).
Subvolumes and snapshots show as the folders they are in; inode numbers
are kept apart per subvolume. Single, DUP and RAID1 data on one device is
read; RAID0/10/5/6 over several devices is not. Checksums are not checked;
a log tree waiting to be replayed (the last fsync'd writes) is not applied
(the open log says so). Volume details: subvolumes, node size, checksum
algorithm, features, label.

Both are tested against the Linux kernel: images made by `mkfs.xfs`
(v5; 1 KiB blocks with 8 KiB folder blocks; 32-bit times with 1 KiB
inodes; v4 without file types) and `mkfs.btrfs` (default; 64 KiB nodes;
explicit holes; xxhash checksums and single metadata), first filled with
junk as a used disk is, are mounted through a loop device and filled by
the kernel — files around block sizes, Unicode and 255-byte names, a
3,000-file folder, links short and long, hard links, a FIFO, a device, a
file owned by someone else, a file with holes, a file with 128 holes
punched (an extent B+tree on XFS), preallocated space partly written, and
on Btrfs folders written with each compression forced, a subvolume and a
snapshot of it that differ afterwards. Our driver's view of each image is
then compared, entry by entry, with the kernel's mount of it: names,
types, sizes, every byte, modes, owners, times, link targets, hard links.
Where the tests may not mount, `mkfs.xfs -p` and `mkfs.btrfs --rootdir`
fill the images from a folder instead (this is how v4 XFS, which current
kernels no longer mount, is read). The lost-partition finder now opens the
XFS and Btrfs volumes it finds, with their labels.

## ISO 9660

`dix-drivers/src/iso9660/` opens CD/DVD images (`.iso`), and makes and
changes them:

* **Rock Ridge** — POSIX names, permissions, owners, times, symbolic links,
  device nodes, continuation areas, deep folders relocated by mkisofs
  (`CL`/`RE`), and **zisofs**-compressed files (`ZF`, read through zlib).
  When present it names the files; otherwise **Joliet** (Unicode names,
  UCS-2 levels 1–3); otherwise the plain ISO 9660 names, without `;1`
  (looked up without regard to case).
* Files of several extents (4 GiB and more), extended attribute records,
  hidden entries.
* **El Torito** boot images (BIOS, UEFI, …; no-emulation, floppy and hard
  disk emulation) appear as files in a `[BOOT]` folder, as 7-Zip shows
  them.
* The volume's **properties** list what it is: its extensions (Rock Ridge
  with its version, Joliet with its level, El Torito, UDF), where the names
  come from, whether and how it boots (each El Torito image), whether it is
  a hybrid image that also boots as a disk (MBR / GPT / Apple partition
  map, as isohybrid makes), and the volume's system, publisher, data
  preparer, application and dates. Drivers can add such rows to any
  volume's properties (`FsInfo::details`), and offer settings
  (`FileSystem::settings` / `apply_settings`, shown in *Settings…*).
* A UDF file system next to the ISO 9660 one (a *bridge* disc) is reported;
  the ISO 9660 side opens by default, and the UDF one (where Windows install
  images keep files over 4 GiB) can be chosen as the driver in the open
  dialog.
* **Creating and changing** (*New image… → ISO 9660*, or opening an ISO
  read-write): like an archive, the tree is kept in memory and the whole
  image written anew when changes are saved (end of a copy, eject, quit),
  as a small mkisofs would: a Rock Ridge tree (RRIP 1.10: names,
  permissions, owners, times, symlinks, device numbers; continuation areas
  for long names and links), a Joliet tree (names up to 103 characters),
  ISO 9660 level-2 names and both pairs of path tables. Unchanged files and
  hard links keep sharing their data; zisofs files are written
  uncompressed. New file data is not held in memory: it is spooled to the
  end of the image file while it is open, and saving writes the new image
  after the old one, then moves it into place and cuts the file to size
  (an interrupted save, as with archives, can damage the image).
* **UDF** (*New image… → ISO 9660 → ISO 9660 + UDF*, or the *UDF* setting):
  the image is also a UDF 1.02 volume, as `genisoimage -udf` makes it, with
  the same files pointing to the same data — Windows, macOS and Linux read
  either side, and UDF holds files of 4 GiB and more for every system.
  After the ISO 9660 descriptors come the UDF recognition sequence, the
  main and reserve descriptor sequences (sectors 32 and 48), the integrity
  descriptor (64) and the anchor (256, and the last sector unless a backup
  GPT is there); the UDF partition starts at 257 with the file set
  descriptor and one file entry per file, folder and link (small folders
  and links embedded in it, larger ones after), followed by the ISO 9660
  structures and the file data. Names longer than UDF holds are shortened
  and kept unique. A bridge image keeps its UDF side when changed — it is
  written anew from the tree — but only if that side holds nothing the ISO
  9660 one lacks: Windows install images, whose content is only in UDF,
  open read-only (the file that would be lost is named).
* **Bootable images**: the volume's *Settings…* (context menu of a
  read-write ISO) choose how it boots — a BIOS boot image (El Torito, no
  emulation: isolinux, GRUB's eltorito.img; load size; the boot info table
  they need is patched in), a UEFI boot image (a FAT image file with
  EFI/BOOT/BOOTX64.EFI), and **hybrid** booting from a USB stick or disk,
  with optional BIOS boot code in the MBR (isolinux's isohdpfx.bin, with
  the boot image's address patched in; the image is padded to whole MiB):
  * with a **GPT** (the default, as current Linux images): a protective
    MBR (marked active, for BIOSes that want it), partition 1 "ISO9660"
    (basic data, from 32 KiB) and partition 2, the UEFI image, as an EFI
    system partition — its data is placed last so that nothing overlaps —
    and the backup GPT in the padding; `sgdisk -v` and `sfdisk --verify`
    find no problems (unlike xorriso's overlapping `-isohybrid-gpt-basdat`
    layout, which UEFI and Linux ignore for want of a protective MBR);
  * or, for old machines, an MBR alone as isohybrid writes it: partition 1
    over the image, partition 2 (type EF) over the UEFI image. Opening a bootable ISO read-write keeps its
  setup (boot images, load size, boot info table, the MBR's boot code) as
  long as its boot images are files in it; floppy- or disk-emulation and
  non-PC entries, and Apple partition maps, keep it read-only. Boot files
  renamed or deleted are followed.

Tests make images with xorriso and genisoimage — Rock Ridge with Joliet
(genisoimage relocating ten-deep folders), Joliet only, plain ISO 9660,
BIOS + UEFI El Torito with a protective MBR, zisofs, a UDF bridge — and read
back exactly the folders they came from, with permissions and times. Images
we make (files written in pieces, Unicode and long names, ten-deep
folders) and images we change (made by genisoimage: files replaced,
deleted, renamed, appended, changed in the middle) are extracted by
xorriso, bsdtar (Rock Ridge) and 7-Zip (Joliet) as exactly the expected
tree, and isoinfo lists the plain ISO 9660 names. Bootable images we make,
and isohybrid images made by xorriso (MBR alone, and with its GPT) that we
change, are checked against xorriso's El Torito report, sfdisk and sgdisk
(MBR, GPT, protective MBR) and the boot info table, and
**booted in QEMU** four ways — BIOS (isolinux) and UEFI (OVMF + GRUB), each
as a CD and as a USB disk.

ISO 9660 + UDF images we make (and change, and turn UDF off in) read back
as exactly the expected tree from both sides here and from the UDF side
by 7-Zip, xorriso finds no errors in them, and udfinfo reports their
label, revision, file and folder counts and a closed volume; a genisoimage bridge image is
changed with its UDF side following, and a pycdlib image with a file only
in UDF is refused.

## UDF

`dix-drivers/src/udf/` reads UDF (ECMA-167 with the OSTA UDF profiles
1.02 to 2.60), the file system of DVDs, Blu-ray discs and packet-written
CD-RW/DVD±RW, read-only:

* The volume recognition sequence (`NSR02`/`NSR03`) and the anchor at block
  256 (or at the end) are found with 512 to 32768-byte blocks; the main
  volume descriptor sequence is read, or the reserve copy if it cannot be.
* All four **partition map** kinds: plain; **sparable** (rewritable media,
  packets remapped through the sparing table); **virtual** (CD-R, DVD-R,
  BD-R written incrementally: blocks go through the VAT found at the end of
  the recorded area, UDF 1.50 and 2.x forms); **metadata** (UDF 2.50+, as on
  Blu-ray: entries and folders live in a metadata file, with its mirror
  used if it cannot be read).
* File entries and extended file entries with short, long and extended
  allocation descriptors, chained allocation extents, sparse extents,
  embedded data and indirect entries; files over 4 GiB; folders of any size;
  symbolic links; hidden entries; owner, permissions and access,
  modification and creation times. Names are OSTA compressed Unicode (8- or
  16-bit). Label, free space (from the integrity descriptor, or after the
  recorded end on write-once media), file and folder counts and the UDF
  revision are shown in the volume's properties. Named streams and extended
  attributes are not shown.
* On a bridge disc (ISO 9660 and UDF) the ISO 9660 driver opens by default;
  a disc with only UDF (Blu-ray, mkudffs) opens as UDF.
* **Making and changing UDF-only images** (*New image… → UDF*, or the *ISO
  9660* setting of an ISO turned off): the ISO 9660 writer (see *ISO 9660*)
  writes the UDF side alone — the recognition sequence at sector 16, then
  the same UDF 1.02 layout. Opened read-write, a UDF-only image is edited
  like an ISO: the tree is kept in memory (unchanged files read through
  this driver), new data is spooled to the file's end, and saving writes
  the image anew. Only finished discs are changed (every partition with
  read-only access, as ours and mastering tools make them): a rewritable
  or write-once volume (mkudffs on a USB stick, packet writing, a CD-R
  with a VAT) would become a finished disc, so it opens read-only, as does
  a bridge disc opened with this driver (the ISO 9660 driver changes both
  sides). Needs the `iso9660` feature.

Tests read UDF 1.02 bridge discs made by genisoimage (a 3 MB file, empty
files, Latin-1 and Unicode names, nested folders, a folder of 150 files) and
a UDF image by pycdlib with symbolic links, comparing every file with what
was put in; and mkudffs volumes in every layout it makes — hard disk
(UDF 1.02 to 2.01; 512, 2048 and 4096-byte blocks), DVD-RAM, sparable CD-RW
and DVD-RW, VAT on CD-R, DVD-R and BD-R up to UDF 2.60 — against udfinfo's
label, revision and free space. A metadata partition is made from a plain
volume by moving its file set and root into a metadata file elsewhere (the
originals zeroed), read through the main file and through its mirror.
A UDF-only image we make is filled (files written in pieces), then
changed (a folder renamed, files deleted and replaced); 7-Zip extracts
exactly the expected tree each time, udfinfo reports its label and counts,
and the ISO 9660 driver finds nothing to read. A rewritable mkudffs
volume and a bridge image opened read-write by this driver stay
read-only.

## SquashFS

`dix-drivers/src/squashfs/` reads SquashFS 4.0, the compressed read-only
file system of Linux live CDs, snaps, AppImages and Android and router
firmware:

* The superblock gives the block size, the compressor and the tables.
  Inodes and folders are in metadata blocks (8 KiB unpacked, each compressed
  alone or stored), addressed by (block offset << 16 | offset); ids and
  fragments through lookup tables. Unpacked metadata and data blocks are
  cached.
* Files: blocks of the block size, each compressed, stored or sparse (size
  0, all zeros), with the tail in a fragment block shared by small files;
  extended inodes for files over 4 GiB and with hard links. Folders (with
  their headers per inode block), symbolic links, character and block
  devices, FIFOs and sockets; permissions, owners, times, inode numbers.
* Every compressor: gzip, lzma, xz (with BCJ filters, as live CDs use),
  zstd through the archive driver's crates; **LZ4** (block format) and
  **LZO1X** decoded here. Extended attributes are not shown.
* **AppImages** open directly: a type 2 AppImage is an ELF launcher with
  the SquashFS appended, starting where the ELF's section header table
  ends (as the ELF header says; for a file marked as an AppImage, "AI" 2,
  the first 4 MiB are also searched for the superblock). Type 1 AppImages
  are ISO 9660 images with the ELF header over their unused first 32 KiB:
  the ISO driver opens them.

Tests pack a folder with mksquashfs — every compressor (xz also with the
x86 filter, LZ4 also high-compression), 4 KiB and 1 MiB blocks, without and
always with fragments, uncompressed tables and data, without sparse
detection, and a device node from a pseudo-file — and compare every entry
with the folder: names (600 in one folder, Unicode, 250 characters), type,
size, content (a sparse file, files of exactly one block, a 3 MB file read
in odd pieces), permissions, owner, time, link targets and hard links.
AppImages are made as appimagetool makes them — a real ELF program marked
"AI" 2 with the SquashFS after it, and with a gap that only the search
finds — and a type 1 one from a genisoimage image; a program alone is not
taken for one.

## littlefs

`dix-drivers/src/littlefs/` reads, changes and creates littlefs v2, the
flash file system of MicroPython (Raspberry Pi Pico and other boards),
Zephyr and Mbed OS:

* **Metadata pairs**: two blocks written in turn, each a revision count and
  a log of commits — tags (32 bits, big-endian, each XORed with the one
  before) with their data, closed by a CRC. The newer block's last valid
  commit counts; replaying the log (creates and deletes shift ids; name,
  struct and attribute tags describe them) gives the pair's entries. A
  folder is a chain of pairs joined by hard tails; the global move state
  (summed over every pair along the soft tails) hides an entry whose move
  was interrupted.
* The superblock (entry 0 of the root pair, blocks 0 and 1) gives the
  block size and count. Files are inline or **CTZ skip lists** (from the
  last block back, block n starting with ctz(n)+1 pointers). Free space is
  what neither the pairs nor the files' blocks use. MicroPython's
  modification times (attribute 1, nanoseconds since 1970) are shown.
* **Flash dumps**: a whole Pico flash (`picotool save -a`) holds the
  firmware first; the file system is looked for on 4 KiB boundaries (in
  dumps up to 256 MiB) and opened where it is, with its offset shown.
* **Changing** (opened read-write; *New image… → littlefs*, 4 KiB blocks):
  the tree is kept in memory and, when saved, the file system is written
  anew in its place with the same geometry — as littlefs compacts: each
  folder's entries sorted by name in pairs filled to half a block (room for
  the device to append), joined by hard tails, every pair on the soft-tail
  list; one commit per pair padded to 512 bytes (aligned for the device's
  later commits), the other block erased. Files are inline up to the
  largest inline file the device itself wrote (its cache holds it; at least
  64 bytes, at most an eighth of a block), else CTZ skip lists. New and
  changed files get MicroPython's time attribute. In a flash dump only the
  file system's blocks are written: the firmware stays as it was, so the
  dump can be flashed back (`picotool load`).

Tests make images with the reference littlefs (littlefs-python) — 256,
512, 4096 (the Pico's) and 16384-byte blocks; inline and multi-block
files; a folder of 120 files split over pairs; deletes, renames across
folders, rewrites and appends that leave old commits and compactions in
the logs; MicroPython times — and compare every file, folder and time with
what littlefs reads; and find one at the end of a 2 MiB flash after noise.
Volumes changed here (files replaced, removed, written in pieces, a folder
renamed, 150 new files, a large one) are read by the reference littlefs as
exactly the expected tree, which then writes on them — appending to our
files, compacting our pairs, renaming and removing — and the result reads
the same here. A change inside a flash dump leaves the firmware bytes
untouched; a volume made by *New image* is mounted and filled by littlefs;
and the same files take no more blocks written here than by littlefs.

## Recovering deleted files

`dix-drivers/src/recover/` turns what a file system still holds of deleted
files into a read-only volume (`Recovered`): the original folders (live
ones only as the way to what was deleted), each found file with its times
and a note on its chance. Nothing is written to the scanned volume; the
app scans on a thread (after writing the volume's pending changes) and
opens the result in a tab with a banner: export what you want to another
disk. Each file system has its scanner:

* **NTFS** (`ntfs/undelete.rs`): every MFT record no longer in use that
  still has a name. Its attributes survive the deletion: the name and
  parent folder (the path is rebuilt through the parent references; a
  parent whose record was reused leaves the file under *Lost files*), the
  times, and the data — resident in the record (small files, whole) or
  runs of clusters, read through the driver's own code (compressed files
  too). `$Bitmap` says which of those clusters are in use again.
* **FAT12/16/32** (`fat/undelete.rs`): directory entries marked deleted
  (0xE5), in live folders and in deleted folders whose first cluster still
  starts with "." and "..". The long name comes back whole — its checksum
  even gives back the short name's lost first letter; a name that fitted
  8.3 exactly had no long name, so its first letter is shown as `_` (as
  other tools do). The FAT chain is freed on deletion, so the file is read
  in one piece from its first cluster; the FAT says which of those
  clusters are in use again.
* **exFAT** (`fat/exfat/undelete.rs`): entry sets whose in-use bit was
  cleared (0x05/0x40/0x41). Most files are stored in one piece (the
  stream's "no FAT chain" flag); others are followed along their FAT chain
  while it still matches their size. The allocation bitmap says which
  clusters are in use again.

**Finding photos and videos by content** (`recover/carve.rs`, as PhotoRec
does) reads every 512-byte sector for the start of a known file, whatever
file system there is or was. A start is only a candidate: the file's own
structure is followed to its end — JPEG segments and the coded data up to
the end-of-image marker (progressive ones too); PNG chunks up to IEND; GIF
blocks up to the trailer; TIFF and camera RAW directories with the strips,
tiles, sub-images and previews they point to (the maker names the RAW
kind); ISO media boxes for HEIC, AVIF, CR3, MP4, MOV and 3GP (a photo must
have its `meta`, a video its `moov` and `mdat`); RIFF for WebP and AVI —
and a candidate whose structure does not hold up is dropped. The scan goes
on after the end of each file found, so thumbnails and previews inside one
are not listed again. Files are taken to be in one piece (as on most
cameras' cards; a fragmented one comes out damaged). A photo's EXIF date
(DateTimeOriginal) becomes its time and its name: `2024-07-14 18.30.05
(f19372).jpg`, the number being the sector it starts at. It runs in the
background with progress; *Stop* keeps what was found.

The scan's test makes a formatted card: photos and videos from Pillow,
ImageMagick and ffmpeg (JPEG with EXIF dates and an embedded thumbnail, a
progressive JPEG, a JPEG whose preview starts on a sector boundary, PNG,
animated GIF, TIFF, a Nikon-style RAW, WebP, MP4, MOV) and a HEIC built by
hand, with 8 MiB of random noise, copied onto FAT32 that is then formatted
again; the scan must find exactly those files, byte for byte, and nothing
else.

Tests delete files with other implementations and compare what is found
with what was written: ntfs-3g (small and large files, Unicode names, a
deleted folder with its files; `ntfsundelete` lists the same names), mtools
on FAT12, FAT16 and FAT32 (long and 8.3 names, a deleted folder, and a file
whose space a new one took, reported overwritten), and exfat-fuse; files
not deleted are never shown. The app's test recovers a file deleted from
an in-memory FAT image into its own volume.

## Copying disks

*Copy disk…* (desktop builds; in the sidebar) copies whole disks, both ways.
A copy runs on its own thread with progress, speed and time left; *Hide*
keeps it going while the rest of the app is used (the status bar shows it),
and quitting asks first while one runs.

**Save disk as image** copies a disk (or a partition, or an image file) into
a raw image or a VM disk (qcow2, VHDX, VHD, VMDK, VDI, DMG); zeros are not
stored. *Only the used space* leaves out what file systems mark free, as
partclone does: clusters free in the FAT (FAT12/16/32), in exFAT's
allocation bitmap, in ext2/3/4's group bitmaps (groups whose bitmap was
never initialised count their metadata — backup superblocks, descriptors,
other groups' bitmaps and inode tables with `flex_bg` — as used), in
NTFS's `$Bitmap`, in each XFS allocation group's free-space B+tree (by
block number), and for Btrfs everything its extent tree does not list —
unused space in chunks (on every copy: DUP metadata is kept twice) and
device space in no chunk; Btrfs's first MiB and superblock copies stay.
Everything else is copied: partition tables, the space between
partitions, other file systems, and a file system whose maps cannot be
trusted (an ext4 journal or a Btrfs log tree waiting to be replayed, Btrfs
data striped over devices; XFS's journal is not looked at, so copy an XFS
volume that was unmounted cleanly). Free space reads as zeros in the copy
(`dix_drivers::imaging::used_space`).

For XFS and Btrfs this is tested on the kernel-filled images of their
driver tests (junk in their free space): the free blocks found for XFS are
as many as its allocation group headers count, and with every free range
zeroed `xfs_repair -n` and `btrfs check --check-data-csum` find nothing
and our driver reads the same files as the kernel did; with the first copy
of each DUP chunk wiped as well, `btrfs check` still passes on the second.

**Rescue mode** is for a failing disk, after GNU ddrescue
(`dix_core::imaging::rescue`): reading in 128 KiB clusters, a failed read
marks its cluster and skips ahead — further each time reads keep failing,
up to 1% of the disk — so the readable rest comes first; a second pass reads
what was skipped; failed clusters are then *trimmed* (read sector by sector
from both ends until a sector fails) and *scraped* (every sector left read
once); unreadable sectors can be retried at the end. They stay zeros in the
image. A disk that stops answering altogether (unplugged) ends the rescue
instead of marking everything bad. A map file next to the image
(`disk.img.map`, saved every 10 seconds) records what was read in
ddrescue's mapfile format: a stopped rescue goes on from it when the same
disk is rescued into the same file again, and ddrescue (or tools that draw
its maps) can use it too. The dialog draws the map as it fills. With *Only
the used space*, only that is read.

**Write image to disk** replaces everything on a disk with an image: raw
images and ISOs, VM disks (read through their container) and compressed
images, unpacked as they are written (gzip, xz, zstd, bzip2 — a Raspberry Pi
`.img.xz` needs no unpacking first). The list shows USB disks and SD cards
(*Show all disks* adds the others); writing is refused when the image is on
that disk, when a volume of it is open in the app, or when the image is
larger. The last step names the disk and what the system has mounted from
it; those volumes are unmounted first (Linux: `udisksctl`, else `umount`;
macOS: `diskutil unmountDisk`), and on Windows they are locked and
dismounted while it is written (`FSCTL_LOCK_VOLUME`,
`FSCTL_DISMOUNT_VOLUME`). macOS writes through `/dev/rdiskN`, the raw node,
which is much faster. An old GPT backup header at the end of a larger disk
is cleared. *Check the disk afterwards* reads it back and compares (Linux
drops the disk's cache first, `BLKFLSBUF`, so the medium is read, not
memory): a failing or fake card (smaller than it says) is caught there.
The system is then asked to read the new partition table.

The rescue's tests use a disk that fails reads in given places: bad sectors
are found exactly, at sector granularity, and everything else is copied
byte for byte; the first pass skips past a 16 MiB bad area after a few
failed reads; a weak sector is lost without retries and recovered with one;
a rescue stopped and continued several times (through its map's text) ends
the same; reads stay within the used space when asked; an unplugged disk
ends the rescue keeping what was read. GNU ddrescue's test mode (bad areas
from a map) checks the map format both ways: ddrescue finishes a rescue we
stopped, and we finish one ddrescue stopped, with the same image and bad
sectors. *Only the used space* is tested on a disk full of junk, partitioned
with `sgdisk` and formatted by `mkfs.fat`, `mke2fs` (1 KiB blocks: six
groups, some uninitialised), `mkntfs` and `mkfs.exfat`, with files in each:
the free blocks found for ext4 are exactly those `dumpe2fs` lists, and in
the copy every file system passes `fsck.fat`, `e2fsck`, `ntfsfix` and
`fsck.exfat` and holds the same files. Writing is tested with images made
by `gzip` (two members), `xz`, `zstd` and `bzip2`, a changed byte found by
the check, and an image too large for its disk.

## Archives

`dix-drivers/src/archive/` opens archives as volumes — browse, preview,
search, copy files in and out (also between an archive and a disk image),
create folders, rename, delete:

| Format | Read | Write |
|--------|------|-------|
| zip (stored, deflate; zip64, Unix modes and symlinks, UTF-8 names) | ✓ | ✓ |
| encrypted zip members (ZipCrypto, WinZip AES), with a password | ✓ | kept encrypted |
| zip members with the other PKZIP methods: Shrink, Reduce, Implode, Deflate64, bzip2, LZMA, zstd, xz | ✓ | rewritten as deflate |
| tar (ustar, GNU long names, pax), `.tar.gz`/`.tgz`, `.tar.zst`, `.tar.bz2` | ✓ | ✓ |
| `.tar.xz` | ✓ | ✓ |
| cpio: newc, crc, odc, old binary (both byte orders); `.cpio.gz`/`.zst`/`.bz2`; initramfs | ✓ | newc |
| `.cpio.xz` | ✓ | ✓ (CRC32 checks, for the kernel) |
| 7z (LZMA, LZMA2, PPMd, Deflate, BZip2, zstd, Copy; BCJ x86 and Delta filters; solid) | ✓ | LZMA2, solid |
| a single compressed file (`.gz`, `.xz`, `.bz2`, `.zst`) | ✓ | – |
| tar, cpio or a single file compressed with lzip (`.lz`), legacy LZMA (`.lzma`), LZ4 (`.lz4`; frames, linked blocks, legacy), Unix compress (`.Z`) or Apple's pbzx | ✓ | – |
| ar (static libraries `.a`; GNU and BSD long names) | ✓ | – |
| Debian packages (`.deb`): the files at the root, control files in `DEBIAN/` | ✓ | – |
| RPM packages (`.rpm`; gzip, xz, zstd, bzip2, lzma payloads) | ✓ | – |
| Microsoft Cabinet (`.cab`; stored, MSZIP, LZX) | ✓ | – |
| XAR: macOS installers (`.pkg`), `.xip` (stored, gzip, bzip2, lzma, xz) | ✓ | – |
| self-extracting programs (Windows `.exe`, Linux ELF) with a zip, 7z, RAR or cabinet inside | ✓ | – |
| RAR 5 (stored and compressed, solid, E8/E8E9/ARM/delta filters) | ✓ | – |
| RAR 4 (RAR 2.9 compression: LZ, PPMd, the six standard filters; solid) | ✓ | – |
| ARJ (methods 0–4, volumes) | ✓ | – |
| multi-volume RAR and ARJ, split zip, split 7z, numbered pieces (`.001`, `.002`, …) | ✓ | – |

**Packages and installers.** A `.deb` is an ar archive of a format
version, `control.tar.*` and `data.tar.*`: the data tar's files are shown
at the root and the control tar's in `DEBIAN/`, as `dpkg-deb -R` lays a
package out. An `.rpm` is a lead, a signature header and a header (its
name, version and release are the volume label), then the payload: a cpio
archive, compressed (rpm writes xz with SHA-256 checks, read with
lzma-rust2). A macOS `.pkg` is a XAR archive: a zlib-compressed XML table
of contents (parsed by a small XML reader here) lists files with where
their bytes are in the heap and how they are encoded; an installer's
`Payload` is cpio, gzip-compressed or in Apple's pbzx (xz chunks), and
opens as an archive itself (*Open as image…*). A **Cabinet**'s folders are
unpacked whole when one of their files is read: MSZIP blocks are deflate
streams that refer back into the block before; LZX (written for this
project from the format's description) has windows of 2^15 to 2^21 bytes,
verbatim, aligned-offset and uncompressed blocks, the three repeated
offsets and x86 CALL translation. Quantum compression and files continued
across a set of cabinets are not read. A **self-extracting program** is
searched (its first 16 MiB) for a 7z, RAR or cabinet header after its
code — each candidate checked (the 7z start header's CRC, RAR's main
header, the cabinet's fields), since the program's own code may hold the
signature's bytes; a zip is found by its end record, its offsets corrected
whether `zip -A` made them absolute or not. NSIS and Inno Setup installers
use formats of their own and are not read.

Tests: `tar --lzma`, `--lzip`, `-Z`, `lz4` (frames, 64 KiB linked blocks,
the legacy format), `compress` with 16- and 12-bit codes (whose padding
after a table reset is checked), lzip in several members and a damaged one;
static libraries from GNU ar (long names) and a BSD ar made by hand (with a
symbol table); packages from `dpkg-deb` with gzip, xz, zstd and no
compression, and from `rpmbuild` with gzip, xz, zstd and bzip2 payloads;
stored and MSZIP cabinets from `gcab`; LZX cabinets from an encoder in the
tests (every block type, run codes and x86 translation; windows of 2^15,
2^16, 2^18, 2^21) that `cabextract -t` checks first; 7-Zip's and RAR's own
self-extractors and 7z, RAR, cabinet and zip archives after a Windows
program whose code contains their signatures; XAR packages from the tests'
writer (checked by 7-Zip where its reader can) with folders, links, every
encoding and installer payloads in gzip and pbzx.

**RAR** archives are read with a decompressor written for this project
(no unRAR code, whose licence covers only making RAR compressors): RAR 5,
the format of every rar/WinRAR since 5.0, at every compression level and
dictionary size, solid or not, with its filters for x86 and ARM code and
for multi-channel data; times, Unix permissions, symbolic links, hard links
and file copies. Members are decompressed as they are read (only the
dictionary window is in memory); in a solid archive reading goes on from
where the stream is, also on to later members (copying a whole solid
archive decodes it once), and starts the solid run again only for an
earlier member. Speed is close to unrar's: about 200–230 MB/s on mixed
binaries and text (unrar: 250) and 650 MB/s on very compressible data
(unrar: 665), measured with `cargo run --release -p dix-drivers --example
rar_bench <archive>`. Multi-volume archives are read
whole (a file split over volumes, solid or not, is read across them);
opened alone, a volume lists what it holds and a file that goes on into
another volume says it is missing.
Tests have unrar test, and our reader extract exactly, archives made by
rar 7 (stored, fastest, normal, best, solid, a 128 KiB dictionary that
wraps, high-precision times, links, x86 executables and multi-channel data
that make rar use its E8E9 and delta filters), read pieces out of order in
a solid archive, every member of a solid archive in order and backwards,
and read a RAR 4 archive made by hand (see below). In tests the reader's input comes in
small chunks so that block, table and filter boundaries land across them. The plain E8 and
the ARM filter are implemented to the format but rar chose neither for any
test data.

**PKZIP methods.** Besides stored and deflate (streamed), zip members
compressed with any other method PKZIP defined are read (whole, when
first read): the PKZIP 1.x methods **Shrink** (LZW with partial clearing),
**Reduce** (follower sets, factors 1–4) and **Implode** (Shannon–Fano
codes, 4 and 8 KiB windows, with and without a literal tree), PKZIP 4's
**Deflate64**, and **bzip2**, **LZMA** (with or without an end marker),
**zstd** and **xz** through the codecs above. PPMd and encrypted members
are listed and say why they cannot be read. A zip with such members can
still be changed: on rewrite they are recompressed with deflate. No tool
here writes Shrink, Reduce or Implode, so the tests make them with small
encoders of their own. Info-ZIP UnZip and 7-Zip must accept the Shrink and
Implode archives, and the driver must read every one back; neither tool
reads Reduce, so it is checked against the format description only. 7-Zip
makes the Deflate64, bzip2, LZMA and PPMd ones; zstd and xz members hold
what the `zstd` and `xz` tools write. (UnZip's partial clear only looks at
codes below its next free code, which is all PKZIP needs as it clears when
the table is full; the decoder here clears every leaf, as APPNOTE and
7-Zip do.)

**Encrypted zips.** Zip members encrypted with a password are read in
both forms in use: the traditional PKWARE cipher ("ZipCrypto", what `zip
-e`/`-P` and most tools write) and WinZip AES (AES-128/192/256, AE-1 and
AE-2, what 7-Zip and WinZip write when asked for AES; the `archive-aes`
feature). Opening such a zip lists everything and asks for the password;
a read that fails for lack of one asks again, and *Enter password…* on the
image (right-click) asks any time. A password is checked against the
encrypted files (the smallest ones, fully: ZipCrypto's one check byte lets
a wrong password through 1 time in 256, so the CRC decides) and kept only
if it opens one; several can be given when files use different
passwords. Passwords stay in memory for as long as the image is open and
are never written anywhere. When an encrypted zip is changed, its
encrypted files are copied as they are (still encrypted, with the same
password); files added are not encrypted. PKWARE's "strong encryption"
is only reported.

**RAR 4 compression** (RAR 2.9, what WinRAR 3.x and 4.x wrote) is read
with a decompressor written for this project:
- LZ blocks: Huffman tables sent as differences to the previous ones, the
  last four distances, repeated matches, short matches, and the "low
  distance" code;
- PPM blocks: PPMd variant H (`archive/ppmd.rs`) with RAR's range coder,
  and the escapes in it for the end of a file, new tables, filters and
  matches;
- solid runs, which keep the window, tables and PPM model from file to
  file;
- the filters. RAR 3 filters are programs for a small VM, but RAR only ever
  wrote six standard ones, and unRAR recognises them by the length and CRC
  of their code and runs native code instead. This does the same: x86
  calls (E8 and E8E9), Itanium branches, delta, RGB images and audio.

RAR 1.5 and 2.0 compression (unpack versions 15, 20 and 26, from the
1990s) are listed and reported. No rar that writes RAR 4 runs here, so the
tests carry a small RAR 2.9 encoder of their own, covering all of the
above. unrar must accept everything it makes, and the reader must read it
back, forwards and backwards. That covers every match kind and table
change, solid runs that keep or replace the tables, all six filters (on
data the encoder transforms first, with filter code made to have the
length and CRC unRAR knows), PPM blocks with escapes and matches, a model
that fills its memory and restarts, a PPM-to-LZ switch inside a file, a
solid PPM run that keeps the model, and compressed files with a password.
Damaged data must give errors, never a crash. The PPMd model is also
checked against 7-Zip: 7z archives made with its PPMd method at orders 2
to 32 (one with 1 MiB of memory, so the model restarts) must read back,
which also gives 7z support for PPMd.

**Encrypted 7z and RAR.** The same password prompt opens 7z archives
encrypted by 7-Zip (AES-256; the key is SHA-256 over the salt and the
password 2^19 times) and RAR 5 archives encrypted by rar/WinRAR (AES-256;
PBKDF2-HMAC-SHA256 with 2^15 rounds, a password check value, and CRCs
"tweaked" with the password), solid or not, stored or compressed, over
volumes too. When the list of files itself is encrypted (`7z -mhe=on`,
`rar -hp`), the archive opens empty and asks; the files appear once the
password is given. 7z has no password check: a wrong password is told
from a right one by the CRCs of what it decrypts to. Keys are derived
once per salt and password (deriving takes a noticeable fraction of a
second). RAR 4 archives with a password (RAR 3.x: AES-128, the key from
2^18 rounds of SHA-1, with the quirk of RAR 3's SHA-1, which wrote back
over input it hashed in place and so changed the key of passwords of 29
characters and more) are read too: encrypted stored files, and encrypted
headers (`-hp`, each header with its own salt), so such an archive can
at least be listed; RAR 4 has no password check, so the CRCs decide (a
compressed file is decompressed with each password). RAR 2's cipher is
not supported.
rar 7 cannot make RAR 4 archives: the tests build them here, with a short
password and a long one, and unrar must accept each. Tests have 7-Zip make solid and non-solid archives and one with
an encrypted header, and rar make normal, solid, stored, `-hp` and `-hp`
multi-volume archives (unrar tests them); each must refuse a wrong
password and read back as the source with the right one. Tests read
archives made by `zip -P` and by 7-Zip (ZipCrypto and AES-128/192/256,
with deflate, stored and bzip2 inside), refuse a wrong password, open a
zip whose files have two different passwords, and change encrypted zips
and have `unzip -t` / `7z t` check them with the password.

**ARJ** archives (read-only) are read with decoders written for this
project: stored files, methods 1–3 (LZ77 with static Huffman codes per
block, a 26 KiB window) and method 4 ("fastest"), Unix permissions and
times from Unix hosts, DOS times from the others. A member is unpacked
whole when first read. Volumes (`x.arj`, `x.a01`, …) open from any part: a
file split between volumes is separately compressed sections, each with
its own checksum. Garbled (password-protected) files and files continued
in a missing volume are listed and say why they cannot be read. Tests make
archives with arj 3.10 (every method, volumes with methods 1 and 4, opened
from the first, a middle and the last part, garbled files), have `arj t`
check them, and compare what the driver reads with the source.

**Split archives and images.** Opening a file that is one part of a set
opens the whole set (`dix_core::device::host::split_parts` finds the parts
by name; `JoinedDevice` reads them one after another as one device, without
copying). The names recognised, in their order:

| Naming | Made by |
|--------|---------|
| `x.part1.rar`, `x.part2.rar`, … (any number width) | rar/WinRAR 5+ volumes |
| `x.rar`, `x.r00`, `x.r01`, … `x.r99`, `x.s00`, … | older RAR volumes |
| `x.z01`, `x.z02`, …, `x.zip` (the `.zip` is last) | `zip -s`, WinZip, 7-Zip |
| `x.arj`, `x.a01`, `x.a02`, … | ARJ volumes |
| `x.001`, `x.002`, … (from `.000` if there) | 7-Zip volumes (`x.7z.001`), `split`, HJSplit |

A set starts at its first part and runs to the first one missing; it must
include the file opened, so a lone `x.001` or a set without its first part
opens as the single file it is. A 7z or a file cut into pieces needs
nothing more: the pieces are the bytes of the whole. RAR volumes each have
their own headers, and a file split over them has a header in each
volume, its data continuing from one to the next; the reader merges them
and decompresses across. A split zip's offsets are within each part
("disk"), so the zip reader takes where each part starts from the joined
device; opened alone, the last part says the other parts are needed. Sets
are read-only (the open dialog only offers read-only), and any part of a
set counts as the same image for "already open". Tests open rar 7 volume
sets (normal, solid, stored; from the first, a middle and the last part;
the old `.rNN` names), `zip -s` and 7-Zip `-v` sets and a `split` tar, and
compare with the source; a RAR 4 volume set made by hand (checked by unrar)
and a split SimpleFS image check the rest. In the browser, uploads are
single files, so split sets open on the desktop only.

**cpio** is read in place like tar (compressed ones are unpacked into
memory). Hard links share their data (newc stores it with one of the
names); device nodes, FIFOs and sockets are listed as special files and
kept, with their device numbers, when the archive is rewritten (as newc,
the format Linux uses for initramfs). An initramfs that is several archives
one after another — typically an uncompressed one with CPU microcode, then
a compressed one — is read whole, later members replacing earlier ones as
the kernel does; it opens read-only, since writing would merge the parts.
Tests read what GNU cpio (newc, crc, odc, bin) and bsdcpio (newc, odc)
write, plain and gzip/xz/zstd-compressed, and have GNU cpio unpack what we
write, plain, gzip and xz (device node and FIFO included; the xz one must
have CRC32 checks).

zip and plain tar are read in place: opening a zip reads only its end
record and central directory (a 500 MB zip opens and lists in ~3 MB of
memory), and a member is *streamed* when read — stored ones straight from
the file, deflated ones inflated only as far as the reads go (~100 KiB of
state whatever its size; the CRC is checked at the end). A compressed tar is
unpacked into memory on mount (gzip/xz/… streams have no index to seek in); a
7z block ("folder", many files in a solid archive) is unpacked when one of
its files is read, and the last one is kept. BCJ2 is recognised and
reported, not read.
**7z is written** too, in the form 7-Zip writes by default: the files' data
in one solid LZMA2 block (compressed with `lzma-rust2`, a port of XZ for
Java), names, times, Unix modes, empty files and folders and symbolic links
in the header, and a CRC for each file. Changing a 7z (or making one with
*New image… → Archive → 7z*) writes it anew in that form, whatever methods
it had: every file is unpacked and compressed again. Because that happens
after each change, the default compression level is 3 (about 20 MB/s; the
size is some 35% above 7-Zip's normal level); *Settings…* on the image sets
1 to 9 (6, 7-Zip's normal, runs at about 1 MB/s here) and writes the
archive again at the new level. (Writing happens after each operation: a
copy of many files is one, and so is each rename or delete.)
**With a password**, a 7z is written as 7-Zip writes it: LZMA2 then
AES-256 (its key from 2^19 rounds of SHA-256, a random IV), and with
*Encrypt file names* the header too (an "encoded header" holding the list
of files, packed and encrypted the same way, with a CRC that tells a
wrong password). *Settings…* has *Encrypt with a password*, a password
field (never shown back) and *Encrypt file names*. An encrypted 7z opened
read-write stays encrypted, with its password and its names hidden if
they were; nothing is written until the password is given (an archive
whose list is still locked refuses changes, so it cannot be overwritten
by an almost empty one). Tests have 7-Zip test and extract new encrypted
archives (names hidden or not), refuse them without the password, and
check that its own encrypted archives, changed here, stay encrypted.
Changes stay in memory and are written as a whole new archive when a copy
finishes, on eject and on quit; unchanged zip members are copied without
recompressing. Rewriting needs an image that can change size (an image file
or an in-memory image), and an interrupted rewrite leaves a damaged
archive: archives have no journal. *New image… → Archive* creates an empty
zip, tar, tar.gz, tar.zst, tar.bz2, cpio, cpio.gz, cpio.zst or 7z (the name follows the type).

Codecs are small pure-Rust crates, so archives also work in the browser:
`miniz_oxide` (deflate/gzip), `ruzstd` (zstd), `bzip2` with its
`libbz2-rs-sys` backend, `lzma-rs` for reading xz and LZMA, and
`lzma-rust2` (a port of XZ for Java) for writing xz and 7z's LZMA2. xz is
written as the `xz` tool writes it, with CRC64 checks, except `.cpio.xz`,
which gets CRC32 checks: an xz initramfs is read by the kernel's xz
decoder, which needs those. As with 7z, the archive is written anew after
each operation, so the level is 3 by default (about 20 MB/s; xz's own
default, 6, runs at about 1 MB/s with this encoder); *Settings…* on the
image sets 1 to 9.

Tests (`archive/tests.rs`) make archives with `zip`, GNU `tar` (ustar, gnu,
pax), `gzip`, `xz`, `bzip2` and `zstd` from a tree with nested and empty
folders, a symlink, a path over 100 characters, non-ASCII names and a
multi-megabyte file, and compare what the driver reads with the tree; they
then change zip/tar/tar.gz/tar.zst/tar.bz2/tar.xz archives with the driver and
check them with `unzip -t` / `xz -t` and by extracting with the tools. A 70 000-member
zip checks zip64. 7z archives made by 7-Zip with each method (solid and not,
compressed and plain headers, Delta + LZMA2, BCJ over real x86 binaries) must
read back as the source tree; encrypted ones must say why they cannot be
read, and open read-only. A 7-Zip archive changed by the driver (files
added, replaced, renamed and removed) and a 7z made from scratch must pass
`7z t`, extract with 7-Zip to the expected tree (symlink included), show
LZMA2 and solid in `7z l`, and get smaller at level 9 than at 3.

## Virtual machine disks

A VM disk is a *container*: a file holding a disk in its own layout. The
`vdisk` module (`crates/dix-drivers/src/vdisk/`) implements
`dix_core::container::DiskFormat` for each format; the registry tries them
on every image opened (`dix_core::device::host::open_disk`, and on
uploads), and a match turns the file into the block device of the disk
inside. Partition tables, drivers, Check and the partition editor then
work on it unchanged; two partitions of one VM disk share one open
container, as they share an image file. Other files a disk refers to —
VMDK extents, the parent of a snapshot — are opened from its folder.

| Format | Read | Write |
|--------|------|-------|
| qcow2 v2/v3 | all clusters: plain, zero, compressed (deflate, zstd); backing files (qcow2, raw, any format here); external data files | new clusters at the end of the file with their refcounts (the refcount table grows and moves when full); compressed clusters are rewritten plain. Not with internal snapshots or external data files |
| VHD | fixed, dynamic, differencing (parent by its relative or absolute locator, or its name) | fixed, dynamic (a new block goes where the footer was) |
| VHDX | fixed, dynamic, differencing (parent locator), 512 and 4096-byte sectors; a log left by a crash is replayed in memory, without changing the file | fixed, dynamic (new blocks at the end, entered in the block table in place, not through the log; not after a log replay) |
| VMDK | monolithic and split (2 GB) sparse and flat, stream-optimized (compressed grains), ESXi flat (`x.vmdk` + `x-flat.vmdk`), `ZERO` extents, snapshots (`parentFileNameHint`) | flat extents; hosted sparse extents (new grains and grain tables appended, both grain directory copies updated) |
| VDI | fixed, dynamic | fixed, dynamic (new blocks appended and counted) |

**Apple disk images** (`vdisk/dmg.rs`): the `koly` trailer at the end of a
UDIF image locates its block tables — in the XML property list
(`resource-fork` → `blkx`), or in a classic resource fork in old images.
Each table covers a run of the disk's sectors (one per partition, the
partition map among them) in chunks stored as zeros, raw, ADC, zlib,
bzip2, LZFSE or LZMA (an xz stream per chunk). Chunks are decompressed when
read, the last few kept. Encrypted, sparse (`.sparseimage`,
`.sparsebundle`) and segmented images are not read.

Writing a DMG rewrites it, as with archives: changed 1 MiB pages are kept
in memory over the image, and when it is saved (sync, eject, or once the
pages pass 256 MiB) the file is written again as a zlib-compressed image
(UDZO) — one block table for the disk, unchanged chunks copied as they
were, changed ones compressed, zeros stored as nothing, checksums filled
in. A crash during that rewrite can damage the image. *New image…* makes
DMGs the same way (with ext4, FAT or exFAT inside: HFS+ and APFS are
read-only here). The tests save written images and have dmg2img and 7-Zip
(which checks the checksums) read them back, and QEMU's DMG reader reads a
new one with ext4 written in it, which `e2fsck` then checks. The disk inside usually
has an **Apple partition map** (APM), which the partition layer now reads
too (the partition editor still makes only GPT and MBR tables).

No Linux tool makes DMGs, so the tests (`vdisk/dmg_tests.rs`) build them
the way hdiutil lays them out — blocks per partition, every chunk kind,
checksums — and first check the builder against two independent decoders,
dmg2img and 7-Zip, before reading them here; LZMA chunks, which neither
knows, are checked against the disk only. The HFS+ test volume in a DMG
with an Apple partition map is found and mounted through it.

A disk that cannot be written opens read-only, and opening it read-write
says why ("this disk (VMDK, streamOptimized) can only be read").
Differencing VDIs name their parent by UUID only and are refused.

The tests (`vdisk/tests.rs`) make every subformat `qemu-img` can write,
read it back here and compare with what `qemu-img` reads; build backing
chains three deep with `qemu-io`; write here at odd offsets across blocks
and check the result with `qemu-img check` (refcounts and leaks for qcow2)
and by reading it with `qemu-img`; mount ext4 read-write inside qcow2, VMDK,
VDI and VHD and run `e2fsck` on what QEMU reads; and replay a VHDX log
entry made by hand. The app's tests open a GPT disk in a qcow2 with both
ext4 partitions read-write at once.

**Converting** (*Convert disk image…* in an image's menu): the whole disk
of an open image — every partition, its container read through — is
written anew as a raw image or a qcow2, VHD, VHDX, VMDK, VDI or DMG
(`container::convert`). The new container is made empty, opened as a disk
and filled in 1 MiB chunks; chunks of zeros are skipped, so it holds only
the data (unallocated blocks in a container, holes in a raw file). A
format that rounds the size down (VHD, to whole cylinders) is asked for
enough to hold the disk. Pending changes of the volume are written first.
Natively it runs in the background into a file chosen by the user, with
progress and Cancel (an unfinished file is removed); in the browser it is
made in memory and downloaded. Tests convert a disk with data in four
places into every format and back, comparing every byte with what
qemu-img reads (our reader for DMG) and checking each file holds little
more than the data; a chain qcow2 → VHDX → VDI → VHD → raw goes through
our own readers; and a partitioned image in memory converted to qcow2
still opens its partition.

## SimpleFS

A deliberately simple test format (`DIXSIMP1` superblock + serialised tree,
checksummed). Use *New image…* to create one, add files, close and reopen it to
verify save/load. It is not meant for real data.
