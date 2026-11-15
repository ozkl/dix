#!/usr/bin/env bash
# Makes the HFS / HFS+ test images in crates/dix-drivers/testdata/hfs/:
# empty volumes from hfsprogs' mkfs.hfsplus, filled by Linux's hfs and
# hfsplus drivers in a VM (see vm.sh), each with a manifest of what Linux
# sees in it (stat and md5 of every entry), which the tests compare with.
#
# Files compressed the way macOS does it (decmpfs) are made by hand: an
# empty file gets a com.apple.decmpfs attribute (set in the VM by a small
# static helper), then its catalog record gets the UF_COMPRESSED flag
# (patched here; Linux cannot set it). The same content is kept in a plain
# file next to each, for the tests to compare. Needs gcc (static libc),
# python3 and Apple's lzfse tool.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$(cd "$here/../.." && pwd)/crates/dix-drivers/testdata/hfs"
mkdir -p "$out"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Payload: compressible, but every block different.
mkdir -p "$work/payload"
python3 - "$work/payload" <<'PY'
import sys, os
d = sys.argv[1]
def text(n, seed):
    out, i = [], 0
    while sum(map(len, out)) < n:
        out.append(f"{seed}:{i:08d} the quick brown fox jumps over the lazy dog\n".encode())
        i += 1
    return b"".join(out)[:n]
open(os.path.join(d, "big.bin"), "wb").write(text(3_000_000, "big"))
open(os.path.join(d, "chunk-a"), "wb").write(text(16384, "a"))
open(os.path.join(d, "chunk-b"), "wb").write(text(16384, "b"))
open(os.path.join(d, "rsrc"), "wb").write(text(5000, "resource fork"))
# decmpfs attributes: the 16-byte header, then the data.
import struct, subprocess, zlib
small = text(3000, "compressed")
def attr(kind, data):
    return b"fpmc" + struct.pack("<IQ", kind, len(small)) + data
open(os.path.join(d, "plain.txt"), "wb").write(small)
open(os.path.join(d, "x-zlib"), "wb").write(attr(3, zlib.compress(small, 9)))
open(os.path.join(d, "x-stored"), "wb").write(attr(1, small))
src = os.path.join(d, "plain.txt")
lz = subprocess.run(["lzfse", "-encode", "-i", src], capture_output=True, check=True).stdout
assert lz[:4] == b"bvxn", "3000 bytes are an LZVN block"
n = struct.unpack("<I", lz[8:12])[0]
open(os.path.join(d, "x-lzvn"), "wb").write(attr(7, lz[12:12 + n]))
bigger = text(20000, "lzfse")
lzf = subprocess.run(["lzfse", "-encode"], input=bigger, capture_output=True, check=True).stdout
assert lzf[:4] == b"bvx2"
open(os.path.join(d, "plain-lzfse.txt"), "wb").write(bigger)
open(os.path.join(d, "x-lzfse"), "wb").write(b"fpmc" + struct.pack("<IQ", 11, len(bigger)) + lzf)
PY
gcc -static -O2 -o "$work/payload/setx" -x c - <<'C'
#include <sys/xattr.h>
#include <stdio.h>
/* setx <file> <attribute> <value file> */
int main(int c, char **v) {
  static char b[65536];
  FILE *f = c == 4 ? fopen(v[3], "rb") : 0;
  if (!f) return 2;
  size_t n = fread(b, 1, sizeof b, f);
  if (setxattr(v[1], v[2], b, n, 0)) { perror("setxattr"); return 1; }
  return 0;
}
C

# What Linux sees: one line per entry, then md5 sums of the files.
cat > "$work/manifest.sh" <<'M'
cd /mnt
find . | sort | while read -r f; do
    if [ -L "$f" ]; then t="$(readlink "$f")"; else t=""; fi
    echo "E|$(stat -c '%F|%s|%a|%u|%g|%Y|%h|%i' "$f")|$f|$t"
done
find . -type f | sort | while read -r f; do echo "M|$(md5sum < "$f" | cut -d' ' -f1)|$f"; done
cd /
M

fill() { # the common content of an HFS+ volume at /mnt
cat <<'J'
cd /mnt
mkdir -p docs/deep/er/still many links
echo "hello hfs+" > docs/hello.txt
echo "deep" > docs/deep/er/still/bottom.txt
cp /payload/big.bin docs/big.bin
: > empty.txt
echo "café" > "café.txt"
echo "nihongo" > "日本語.txt"
echo "party" > "emoji 🎉.txt"
echo "colon" > "a:b.txt"
i=0; while [ $i -lt 600 ]; do echo "file number $i" > "many/file-$i.txt"; i=$((i+1)); done
# Two files grown in turns: many extents each (more than the 8 in the
# catalog record, so the rest go to the extents overflow file).
i=0; while [ $i -lt 60 ]; do
    cat /payload/chunk-a >> frag-a.bin; sync
    cat /payload/chunk-b >> frag-b.bin; sync
    i=$((i+1))
done
ln -s ../docs/hello.txt links/to-hello
ln -s /nowhere/at/all links/dangling
ln -s docs links/dirlink-rel-wrong
ln docs/hello.txt links/hard-hello
ln docs/hello.txt hard-too
chmod 0750 docs/deep
chmod 0604 docs/hello.txt
chown 501:20 docs/big.bin
touch -d "2001-02-03 04:05:06" docs/deep/er/still/bottom.txt
mkdir compressed
for k in zlib stored lzvn lzfse; do
    : > compressed/$k.txt
    /payload/setx compressed/$k.txt osx.com.apple.decmpfs /payload/x-$k
done
cp /payload/plain.txt compressed/expect.txt
cp /payload/plain-lzfse.txt compressed/expect-lzfse.txt
cd /
J
}

# 1. HFS+, 4 KiB blocks.
img="$work/hfsplus.img"; truncate -s 24M "$img"; mkfs.hfsplus -v "Test HFS+" "$img" >/dev/null
# 2. HFSX (case-sensitive), 512-byte blocks.
x="$work/hfsx.img"; truncate -s 16M "$x"; mkfs.hfsplus -s -b 512 -v "Case Sensitive" "$x" >/dev/null
# 4. Classic HFS (Mac OS Standard).
c="$work/hfs.img"; truncate -s 8M "$c"; mkfs.hfsplus -h "$c" >/dev/null
# 3. Journaled, in an HFS wrapper.
j="$work/wrapped.img"; truncate -s 16M "$j"; mkfs.hfsplus -w -J -v "Wrapped" "$j" >/dev/null
{
    echo 'mount -t hfsplus /dev/vda /mnt'
    fill
    echo 'echo "== manifest hfsplus"'; cat "$work/manifest.sh"
    echo 'umount /mnt'
    echo 'mount -t hfsplus /dev/vdb /mnt; cd /mnt'
    echo 'echo upper > Name.txt; echo lower > name.txt; mkdir Dir dir; echo x > Dir/x; echo y > dir/y'
    echo 'cp /payload/big.bin big.bin; cd /'
    echo 'echo "== manifest hfsx"'; cat "$work/manifest.sh"
    echo 'umount /mnt'
    echo 'mount -t hfsplus -o force /dev/vdc /mnt; cd /mnt'
    echo 'echo "inside a wrapper" > wrapped.txt; mkdir sub; cp /payload/chunk-a sub/a; cd /'
    echo 'echo "== manifest wrapped"'; cat "$work/manifest.sh"
    echo 'umount /mnt'
    echo 'mount -t hfs -o codepage=macroman,iocharset=utf8 /dev/vdd /mnt; cd /mnt'
    echo 'mkdir -p "Système/Dossier"; echo "bonjour" > "Système/café.txt"; echo deep > "Système/Dossier/x"'
    echo 'cp /payload/big.bin big.bin; cp /payload/chunk-a a; touch -d "1999-12-31 23:59:58" a; cd /'
    echo 'echo "== manifest hfs"'; cat "$work/manifest.sh"
    echo 'umount /mnt'
} > "$work/job.sh"
"$here/vm.sh" "$work/job.sh" "$work/payload" "$img" "$x" "$j" "$c" > "$work/out.txt" || { tail -20 "$work/out.txt" >&2; exit 1; }
# The compressed files: set UF_COMPRESSED in their catalog records.
python3 - "$img" <<'PY'
import struct, sys
img = bytearray(open(sys.argv[1], "rb").read())
vh = img[1024:1536]
block = struct.unpack(">I", vh[40:44])[0]
ext = [struct.unpack(">II", vh[272 + 16 + i * 8:272 + 24 + i * 8]) for i in range(8)]
cat = b"".join(img[s * block:(s + n) * block] for s, n in ext if n)
node_size = struct.unpack(">H", cat[14 + 18:14 + 20])[0]
want = {f"{k}.txt" for k in ("zlib", "stored", "lzvn", "lzfse")}
done = set()
for n in range(len(cat) // node_size):
    node = cat[n * node_size:(n + 1) * node_size]
    if node[8] != 0xFF:  # leaf nodes (kind -1)
        continue
    for i in range(struct.unpack(">H", node[10:12])[0]):
        at = struct.unpack(">H", node[node_size - 2 * (i + 1):node_size - 2 * i])[0]
        klen = struct.unpack(">H", node[at:at + 2])[0]
        nlen = struct.unpack(">H", node[at + 6:at + 8])[0]
        name = node[at + 8:at + 8 + 2 * nlen].decode("utf-16-be")
        rec = at + 2 + klen
        if name in want and struct.unpack(">H", node[rec:rec + 2])[0] == 2:
            flag = n * node_size + rec + 41
            # Back into the image, through the catalog's extents.
            off = flag
            for s, cnt in ext:
                if off < cnt * block:
                    img[s * block + off] |= 0x20
                    break
                off -= cnt * block
            done.add(name)
assert done == want, done
open(sys.argv[1], "wb").write(img)
PY
for name in hfsplus hfsx wrapped hfs; do
    sed -n "/^== manifest $name\$/,/^== manifest /p" "$work/out.txt" | grep -E '^(E|M)\|' > "$out/$name.manifest"
    if [ "$name" != hfs ]; then
        fsck.hfsplus -fn "$work/$name.img" || { echo "fsck.hfsplus failed on $name" >&2; exit 1; }
    fi
    gzip -9n < "$work/$name.img" > "$out/$name.img.gz"
done
ls -la "$out"
