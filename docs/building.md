[App overview](../README.md) · [Features](features.md) · [Building](building.md) · [Architecture](architecture.md) · [Format reference](formats.md)

# Building

```bash
cargo run --release -p dix-app             # native
cargo run --release -p dix-app -- disk.img # open an image read-only on start
cargo test                                 # core, drivers and model tests
cargo run -p dix-drivers --example make_sample -- sample.img  # a SimpleFS image to try
cargo run --release -p dix-drivers --example make_large -- large.img 20000  # stress test
```

Linux needs the usual egui/winit system libraries (X11 or Wayland, OpenGL).

File names in any script — Chinese, Japanese, Korean, Arabic, Hebrew, Thai,
Devanagari, … — are drawn with the system's own fonts, found when a
character first needs one (eframe's `system_fonts`; on Linux through
fontconfig, loaded at run time, so it is not needed to build or start).
Without a font for a script, its characters show as boxes; in the browser
only the built-in fonts are used.

Web build (needs [trunk](https://trunkrs.dev)):

```bash
rustup target add wasm32-unknown-unknown
cd crates/dix-app && trunk serve --release   # or: trunk build --release → ../../dist
```

Raw devices need administrator/root rights (e.g. `sudo` on Linux/macOS, "Run
as administrator" on Windows).

### macOS app bundle

On a Mac, `scripts/macos-bundle.sh` builds the app and wraps it in
`target/macos/Disk Image Explorer.app` (the binary, `Info.plist` with the
version from `Cargo.toml`, and the icon), signed ad hoc so Apple silicon runs it:

```bash
scripts/macos-bundle.sh               # this Mac's architecture
scripts/macos-bundle.sh --universal   # Apple silicon + Intel in one binary (lipo)
scripts/macos-bundle.sh --dmg         # also Disk-Image-Explorer-<version>.dmg
scripts/macos-bundle.sh --sign "Developer ID Application: …"   # a real identity
```

`--universal` needs `rustup target add aarch64-apple-darwin x86_64-apple-darwin`;
`--binary PATH` bundles a binary built elsewhere (e.g. in CI) and `--out DIR`
puts the results somewhere else. An ad hoc signature runs on the Mac that made
it; to hand the app to others without Gatekeeper's warning, sign it with a
Developer ID and notarize it.

### Windows executable

On Windows, `build.rs` puts the icon and version information into
`disk-image-explorer.exe` (Explorer's icon, and Properties → Details), with
the Windows SDK's `rc.exe` or MinGW's `windres`; release builds open no
console window. Cross-building from Linux works with MinGW installed:

```bash
rustup target add x86_64-pc-windows-gnu
sudo apt install gcc-mingw-w64-x86-64 binutils-mingw-w64-x86-64
cargo build --release -p dix-app --target x86_64-pc-windows-gnu
```

### The icon

The icon is drawn in `crates/dix-app/assets/icon.svg`: a disc (the image) in an
open folder, with a magnifying glass. `scripts/make-icons.sh` renders it in
Chromium through Playwright and writes the files made from it — `icon-256.png`
(the window icon and the web favicon), `AppIcon.icns` (macOS) and `icon.ico`
(Windows, built into the .exe) — after a change to the SVG.

### Choosing the drivers

Every driver is a Cargo feature, and so is every archive codec (they are
where the dependencies come from). All are on by default; leave some out
with `--no-default-features` and list the ones to keep:

| Feature | What it adds | Dependencies |
|---------|--------------|--------------|
| `ext4` | ext2/3/4 (read-write, journal, mkfs, fsck) | – |
| `fat` | FAT12/16/32 (read-write, long names, mkfs, check) | – |
| `exfat` | exFAT (read-write, mkfs, check), in the FAT driver | – |
| `ntfs` | NTFS (read-only: compressed and sparse files, streams, links; check) | – |
| `hfsplus` | HFS+, HFSX, HFS (read-only; files compressed by macOS: zlib, LZVN, LZFSE) | `miniz_oxide`, `unicode-normalization` |
| `apfs` | APFS (read-only; compressed files as with HFS+) | `miniz_oxide`, `unicode-normalization` |
| `xfs` | XFS v4 and v5 (read-only) | — |
| `btrfs` | Btrfs (read-only; subvolumes, snapshots, zlib/LZO/zstd) | `miniz_oxide`, `ruzstd` |
| `iso9660` | ISO 9660 images (Rock Ridge, Joliet, El Torito, zisofs; create and change) | `miniz_oxide` |
| `littlefs` | littlefs v2 (MicroPython, microcontrollers; found inside flash dumps; read-write, create) | — |
| `squashfs` | SquashFS 4.0 (read-only; gzip, lzma, lzo, xz, lz4, zstd) | `miniz_oxide`, `lzma-rs`, `lzma-rust2`, `ruzstd` |
| `udf` | UDF 1.02–2.60 (plain, sparable, VAT and metadata partitions; UDF-only images made and changed with `iso9660`'s writer) | — |
| `simplefs` | SimpleFS, the tiny test file system | – |
| `vdisk` | VM disks: qcow2, VHD, VHDX, VMDK, VDI; Apple disk images (DMG) | `miniz_oxide` (zstd qcow2 clusters with `archive-zstd`; bzip2, LZMA and LZFSE DMGs with `archive-bzip2`, `archive-xz` and `hfsplus`/`apfs`) |
| `archive` | zip, tar and cpio, plain or `.gz` | `miniz_oxide` |
| `archive-zstd` | `.tar.zst`, `.cpio.zst` (read-write) | `ruzstd` |
| `archive-bzip2` | `.tar.bz2` (read-write) | `bzip2` (pure Rust backend) |
| `archive-xz` | `.tar.xz`, `.cpio.xz` (read-write) | `lzma-rs`, `lzma-rust2` |
| `archive-7z` | 7z (read-write; written with LZMA2) | `lzma-rs`, `lzma-rust2` |
| `archive-rar` | RAR 5 and RAR 4 (read-only) | – |
| `archive-aes` | AES encryption with a password: zip (WinZip AES; ZipCrypto is always in), 7z, RAR 5 | `aes`, `hmac`, `sha1`, `sha2`, `pbkdf2` |

```bash
# disk images only, no archives:
cargo build --release -p dix-app --no-default-features --features ext4
# ext4 and zip/tar, without the extra codecs:
cargo build --release -p dix-app --no-default-features --features ext4,archive
```

`dix-app` forwards the features to `dix-drivers` (which has the same ones).
A build without a driver simply does not offer it; an archive whose codec was
left out is still recognised and says so ("7z support was left out of this
build").

## License

Disk Image Explorer is licensed under the MIT license ([LICENSE](../LICENSE)).
Unless you say otherwise, any contribution you submit for inclusion is
licensed the same way, without further terms.

The ext4 code adapted from [ruxis](https://github.com/ozkl/ruxis) keeps its
MIT license (`crates/dix-drivers/src/ext4/ruxis/LICENSE-ruxis`). Every other
file system, archive and compression reader here was written for this
project from the formats' descriptions, not translated from other
implementations; the tools named in the tests (e2fsck, unrar, cabextract,
the Linux kernel, …) only check the results and are not part of the app.

The crates the app is built with are under permissive licenses (MIT,
Apache-2.0, BSD, Zlib, ISC, BSL-1.0, Unicode-3.0; the built-in fonts under
the SIL Open Font License and the Ubuntu Font Licence; libbzip2's).
[THIRD-PARTY-LICENSES.txt](../THIRD-PARTY-LICENSES.txt) holds all of them,
with the crates each covers. The app shows it (*Licenses* on the welcome
screen, *View → Licenses…*), so every build carries it; the macOS bundle and
the web build also have the files next to the app. After changing
dependencies, write it again (needs `cargo install cargo-about --locked
--features cli`):

```bash
scripts/third-party-licenses.sh
```
