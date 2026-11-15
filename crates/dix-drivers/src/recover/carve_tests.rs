//! A memory card that was formatted: photos and videos made by Pillow,
//! ImageMagick and ffmpeg (and a HEIC built by hand) are copied onto a
//! FAT32 volume with random noise, the volume is formatted again, and the
//! scan must find exactly those files, byte for byte — no thumbnail or
//! preview inside them listed again, nothing made up from the noise.
//! Skipped without the tools.

use super::carve;
use dix_core::{FileSystem, VfsPath};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn have(tools: &[&str]) -> bool {
    tools
        .iter()
        .all(|t| Command::new("sh").arg("-c").arg(format!("command -v {t}")).output().is_ok_and(|o| o.status.success()))
}

fn run(cmd: &str, args: &[&str]) {
    let out = Command::new(cmd).args(args).env("MTOOLS_SKIP_CHECK", "1").output().unwrap();
    assert!(out.status.success(), "{cmd} {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

const MAKE_MEDIA: &str = r#"
import io, os, struct, sys
from PIL import Image
d = sys.argv[1]
def noise(w, h, seed):
    import random
    random.seed(seed)
    return Image.frombytes('RGB', (w, h), bytes(random.getrandbits(8) for _ in range(w * h * 3)))
# A JPEG with EXIF dates and a whole JPEG (a thumbnail) inside an APP2 segment.
img = noise(640, 480, 1)
exif = Image.Exif()
exif[0x0132] = '2024:07:14 18:30:05'
exif.get_ifd(0x8769)[0x9003] = '2024:07:14 18:30:05'
thumb = io.BytesIO(); noise(64, 48, 2).save(thumb, 'JPEG')
main = io.BytesIO(); img.save(main, 'JPEG', exif=exif.tobytes(), quality=90)
b = main.getvalue(); t = thumb.getvalue()
app2 = b'\xff\xe2' + struct.pack('>H', len(t) + 2) + t
open(os.path.join(d, 'photo.jpg'), 'wb').write(b[:2] + app2 + b[2:])
# A preview starting on a sector boundary (byte 512) inside another JPEG:
# found once, as part of it, not again on its own.
pad = 512 - 2 - 4
app3 = b'\xff\xe3' + struct.pack('>H', 2 + pad + len(t)) + b'\0' * pad + t
other = io.BytesIO(); noise(500, 400, 12).save(other, 'JPEG', quality=80)
o = other.getvalue()
assert (o[:2] + app3)[512:515] == b'\xff\xd8\xff'
open(os.path.join(d, 'with_preview.jpg'), 'wb').write(o[:2] + app3 + o[2:])
noise(800, 600, 3).save(os.path.join(d, 'progressive.jpg'), 'JPEG', progressive=True, quality=85)
noise(300, 200, 4).save(os.path.join(d, 'picture.png'))
frames = [noise(120, 90, s).convert('P') for s in range(5, 9)]
frames[0].save(os.path.join(d, 'anim.gif'), save_all=True, append_images=frames[1:], duration=100, loop=0)
noise(256, 256, 9).save(os.path.join(d, 'scan.tif'))
noise(320, 240, 10).save(os.path.join(d, 'camera.nef'), 'TIFF', tiffinfo={271: 'NIKON CORPORATION', 272: 'NIKON D750'})
try:
    noise(200, 150, 11).save(os.path.join(d, 'image.webp'), 'WEBP', quality=80)
except Exception:
    pass
# A HEIC: ftyp, meta (with an item info box), mdat.
def box(t, payload): return struct.pack('>I', 8 + len(payload)) + t + payload
ftyp = box(b'ftyp', b'heic' + b'\0\0\0\0' + b'mif1heic')
meta = box(b'meta', b'\0\0\0\0' + box(b'hdlr', b'\0' * 8 + b'pict' + b'\0' * 13) + box(b'iinf', b'\0' * 6))
mdat = box(b'mdat', os.urandom(150000))
open(os.path.join(d, 'phone.heic'), 'wb').write(ftyp + meta + mdat)
open(os.path.join(d, 'noise.bin'), 'wb').write(os.urandom(8 << 20))
"#;

#[test]
fn finds_photos_on_a_formatted_card() {
    if !have(&["python3", "ffmpeg", "mkfs.fat", "mcopy"])
        || !Command::new("python3").args(["-c", "import PIL"]).output().is_ok_and(|o| o.status.success())
    {
        eprintln!("python3 + Pillow, ffmpeg, dosfstools, mtools needed: skipped");
        return;
    }
    let dir = std::env::temp_dir().join(format!("dix-carve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let media = dir.join("media");
    std::fs::create_dir_all(&media).unwrap();
    run("python3", &["-c", MAKE_MEDIA, media.to_str().unwrap()]);
    for (name, extra) in [("clip.mp4", &["-movflags", "+faststart"][..]), ("clip.mov", &[][..])] {
        let mut args = vec!["-loglevel", "error", "-f", "lavfi", "-i", "testsrc=duration=1:size=320x240:rate=10"];
        args.extend(extra);
        let out = media.join(name);
        args.extend(["-y", out.to_str().unwrap()]);
        run("ffmpeg", &args);
    }
    let files: Vec<PathBuf> = {
        let mut v: Vec<PathBuf> = std::fs::read_dir(&media).unwrap().flatten().map(|e| e.path()).collect();
        v.sort();
        v
    };
    // The card: FAT32 with everything on it, then formatted again.
    let card = dir.join("card.img");
    let mut bytes = Vec::new();
    {
        run("mkfs.fat", &["-C", "-F", "32", card.to_str().unwrap(), &(64 * 1024).to_string()]);
        let mut args = vec!["-i".to_owned(), card.to_str().unwrap().to_owned()];
        args.extend(files.iter().map(|p| p.to_str().unwrap().to_owned()));
        args.push("::/".into());
        run("mcopy", &args.iter().map(String::as_str).collect::<Vec<_>>());
        run("mkfs.fat", &["-F", "32", card.to_str().unwrap()]);
        bytes.extend(std::fs::read(&card).unwrap());
    }
    let want: BTreeMap<Vec<u8>, String> = files
        .iter()
        .filter(|p| p.extension().is_none_or(|e| e != "bin"))
        .map(|p| (std::fs::read(p).unwrap(), p.file_name().unwrap().to_string_lossy().into_owned()))
        .collect();
    let mut calls = 0;
    let mut fs = carve::scan(Box::new(dix_core::device::MemoryDevice::new("card", bytes)), &mut |_, _| {
        calls += 1;
        true
    })
    .unwrap();
    assert!(calls > 0);
    let mut got: BTreeMap<Vec<u8>, String> = BTreeMap::new();
    for folder in fs.read_dir(&VfsPath::root()).unwrap() {
        let f = format!("/{}", folder.name);
        for e in fs.read_dir(&VfsPath::parse(&f).unwrap()).unwrap() {
            let p = format!("{f}/{}", e.name);
            assert_eq!(e.metadata.note.as_deref(), Some("found by content"));
            let data = fs.read_to_vec(&VfsPath::parse(&p).unwrap()).unwrap();
            assert!(got.insert(data, p.clone()).is_none(), "{p} found twice");
        }
    }
    for (data, name) in &want {
        let found = got.get(data).unwrap_or_else(|| {
            panic!("{name} ({} bytes) not found; found: {:?}", data.len(), got.values().collect::<Vec<_>>())
        });
        let ext = Path::new(name).extension().unwrap().to_string_lossy().into_owned();
        assert!(found.ends_with(&format!(".{ext}")), "{name} found as {found}");
    }
    assert_eq!(got.len(), want.len(), "extra files found: {:?}", got.values().collect::<Vec<_>>());
    // The photo's date, from EXIF, names it.
    assert!(got.values().any(|p| p.starts_with("/JPEG/2024-07-14 18.30.05")), "{:?}", got.values().collect::<Vec<_>>());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn exif_times() {
    assert_eq!(super::carve::parse_exif_time_for_test(b"2024:07:14 18:30:05"), Some(1_720_981_805));
    assert_eq!(super::carve::parse_exif_time_for_test(b"0000:00:00 00:00:00"), None);
}
