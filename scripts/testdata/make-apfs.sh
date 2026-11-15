#!/usr/bin/env bash
# Makes the APFS test images in crates/dix-drivers/testdata/apfs/: empty
# containers from apfsprogs' mkapfs, filled by the linux-apfs-rw driver in
# a VM (see vm.sh), checked by apfsck, each with a manifest of what Linux
# sees in it.
#
# The driver is built here for the VM's kernel, from Ubuntu's apfs-dkms
# 0.3.12 (the 0.3.2 in Ubuntu 24.04 cannot write on that kernel). Files
# compressed the way macOS does it get a com.apple.decmpfs attribute (and,
# for chunked data, a com.apple.ResourceFork one) in the VM; the
# UF_COMPRESSED flag is then set here in their inodes (Linux cannot set
# it), with the nodes' checksums updated. Needs gcc (static libc),
# python3, xz, apfsprogs and Apple's lzfse tool.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$(cd "$here/../.." && pwd)/crates/dix-drivers/testdata/apfs"
mkdir -p "$out"
K="${VM_KERNEL:-6.8.0-149-generic}"
cache="${VM_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/dix-test-vm}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The apfs and libcrc32c modules for the VM's kernel.
mod="$cache/apfs-$K.ko"
if [ ! -f "$mod" ]; then
    mkdir -p "$cache/debs" "$cache/build"
    (cd "$cache/debs" && apt-get download "linux-headers-$K" "linux-headers-${K%-generic}" \
        && curl -fsSLO https://archive.ubuntu.com/ubuntu/pool/universe/l/linux-apfs-rw/apfs-dkms_0.3.12-1ubuntu1_all.deb)
    for p in "$cache"/debs/linux-headers-*.deb "$cache"/debs/apfs-dkms_0.3.12*.deb; do dpkg -x "$p" "$cache/build"; done
    src="$(ls -d "$cache"/build/usr/src/linux-apfs-rw-*)"
    (cd "$src" && sh genver.sh && make KERNEL_DIR="$cache/build/usr/src/linux-headers-$K")
    cp "$src/apfs.ko" "$mod"
fi
crc="$cache/libcrc32c-$K.ko"
[ -f "$crc" ] || zstd -q -d -f "$cache/$K/lib/modules/$K/kernel/lib/libcrc32c.ko.zst" -o "$crc"

mkdir -p "$work/payload"
python3 - "$work/payload" <<'PY'
import os, struct, subprocess, sys, zlib
d = sys.argv[1]
def text(n, seed):
    out, i = [], 0
    while sum(map(len, out)) < n:
        out.append(f"{seed}:{i:08d} the quick brown fox jumps over the lazy dog\n".encode())
        i += 1
    return b"".join(out)[:n]
w = lambda name, data: open(os.path.join(d, name), "wb").write(data)
w("big.bin", text(3_000_000, "big"))
w("chunk-a", text(16384, "a"))
w("chunk-b", text(16384, "b"))
def lzfse(data):
    return subprocess.run(["lzfse", "-encode"], input=data, capture_output=True, check=True).stdout
def hdr(kind, size):
    return b"fpmc" + struct.pack("<IQ", kind, size)
small = text(3000, "compressed")
w("expect", small)
w("x-zlib", hdr(3, len(small)) + zlib.compress(small, 9))
lz = lzfse(small)
assert lz[:4] == b"bvxn"
w("x-lzvn", hdr(7, len(small)) + lz[12:12 + struct.unpack("<I", lz[8:12])[0]])
w("x-lzfse", hdr(11, len(small)) + lzfse(small))
# Stored, and long enough for the attribute to be a data stream.
mid = text(10000, "stored")
w("expect-stored", mid)
w("x-stored", hdr(1, len(mid)) + mid)
# Chunked in the resource fork: LZFSE (type 12) and zlib (type 4).
big = text(150_000, "chunks")
w("expect-chunks", big)
chunks = [lzfse(big[i:i + 65536]) for i in range(0, len(big), 65536)]
table, off = b"", 4 * (len(chunks) + 1)
for c in chunks:
    table += struct.pack("<I", off); off += len(c)
table += struct.pack("<I", off)
# (Padded past what an attribute keeps inline: macOS always stores the
# resource fork as a data stream.)
w("r-lzfse", table + b"".join(chunks) + bytes(8192))
w("x-rlzfse", hdr(12, len(big)))
zchunks = [zlib.compress(big[i:i + 65536], 9) for i in range(0, len(big), 65536)]
body = struct.pack("<I", len(zchunks))
off = 4 + 8 * len(zchunks)
for c in zchunks:
    body += struct.pack("<II", off, len(c)); off += len(c)
body += b"".join(zchunks)
rsrc = struct.pack(">IIII", 0x100, 0x100 + 4 + len(body), len(body) + 4, 8192) + bytes(0x100 - 16)
rsrc += struct.pack(">I", len(body)) + body + bytes(8192)
w("r-zlib", rsrc)
w("x-rzlib", hdr(4, len(big)))
PY
gcc -static -O2 -o "$work/payload/setx" -x c - <<'C'
#include <sys/xattr.h>
#include <stdio.h>
/* setx <file> <attribute> <value file> */
int main(int c, char **v) {
  static char b[1 << 20];
  FILE *f = c == 4 ? fopen(v[3], "rb") : 0;
  if (!f) return 2;
  size_t n = fread(b, 1, sizeof b, f);
  if (setxattr(v[1], v[2], b, n, 0)) { perror("setxattr"); return 1; }
  return 0;
}
C

cat > "$work/manifest.sh" <<'M'
cd /mnt
find . | sort | while read -r f; do
    if [ -L "$f" ]; then t="$(readlink "$f")"; else t=""; fi
    echo "E|$(stat -c '%F|%s|%a|%u|%g|%Y|%h|%i' "$f")|$f|$t"
done
find . -type f | sort | while read -r f; do echo "M|$(md5sum < "$f" | cut -d' ' -f1)|$f"; done
cd /
M

# Compressed files: these in the main image; an LZFSE resource fork in an
# image of its own, which apfsck is not run on (it reads every resource
# fork as zlib's layout; the Linux driver, which reads them in the second
# pass below, and macOS use an offset table for LZVN and LZFSE).
echo "zlib lzvn lzfse stored rzlib" > "$work/payload/kinds-apfs"
echo "rlzfse" > "$work/payload/kinds-apfs-rsrc"

main="$work/apfs.img"; truncate -s 128M "$main"; mkapfs -L "Test APFS" "$main" >/dev/null
cs="$work/apfs-cs.img"; truncate -s 128M "$cs"; mkapfs -s -L "Case Sensitive" "$cs" >/dev/null
rs="$work/apfs-rsrc.img"; truncate -s 128M "$rs"; mkapfs -L "Resource Fork" "$rs" >/dev/null
{
cat <<'J'
mount -t apfs -o readwrite /dev/vda /mnt
cd /mnt
mkdir -p docs/deep/er/still many links compressed
echo "hello apfs" > docs/hello.txt
echo "deep" > docs/deep/er/still/bottom.txt
cp /payload/big.bin docs/big.bin
: > empty.txt
echo "café" > "café.txt"
printf 'nfd\n' > "$(printf 'cafe\314\201-nfd.txt')"
echo "nihongo" > "日本語.txt"
echo "colon" > "a:b.txt"
i=0; while [ $i -lt 600 ]; do echo "file number $i" > "many/file-$i.txt"; i=$((i+1)); done
i=0; while [ $i -lt 40 ]; do
    cat /payload/chunk-a >> frag-a.bin; sync
    cat /payload/chunk-b >> frag-b.bin; sync
    i=$((i+1))
done
dd if=/dev/zero of=sparse.bin bs=1 count=1 seek=5000000 2>/dev/null
echo tail >> sparse.bin
ln -s ../docs/hello.txt links/to-hello
ln -s /nowhere/at/all links/dangling
ln docs/hello.txt links/hard-hello
ln docs/hello.txt hard-too
chmod 0750 docs/deep
chmod 0604 docs/hello.txt
chown 501:20 docs/big.bin
touch -d "2001-02-03 04:05:06" docs/deep/er/still/bottom.txt
for k in $(cat /payload/kinds-apfs); do
    : > compressed/$k.txt
    /payload/setx compressed/$k.txt osx.com.apple.decmpfs /payload/x-$k
    if [ -f /payload/r-${k#r} ] && [ "${k#r}" != "$k" ]; then
        /payload/setx compressed/$k.txt osx.com.apple.ResourceFork /payload/r-${k#r}
    fi
done
cp /payload/expect /payload/expect-stored /payload/expect-chunks compressed/
cd /
J
echo 'echo "== manifest apfs"'; cat "$work/manifest.sh"
echo 'umount /mnt'
cat <<'J'
mount -t apfs -o readwrite /dev/vdb /mnt; cd /mnt
echo upper > Name.txt; echo lower > name.txt; mkdir Dir dir; echo x > Dir/x; echo y > dir/y
cp /payload/big.bin big.bin; cd /
J
echo 'echo "== manifest apfs-cs"'; cat "$work/manifest.sh"
echo 'umount /mnt'
cat <<'J'
mount -t apfs -o readwrite /dev/vdc /mnt; mkdir /mnt/compressed; cd /mnt
: > compressed/rlzfse.txt
/payload/setx compressed/rlzfse.txt osx.com.apple.decmpfs /payload/x-rlzfse
/payload/setx compressed/rlzfse.txt osx.com.apple.ResourceFork /payload/r-lzfse
cp /payload/expect-chunks compressed/; cd /
J
echo 'echo "== manifest apfs-rsrc"'; cat "$work/manifest.sh"
echo 'umount /mnt'
} > "$work/job.sh"
VM_EXTRA_MODULES="$crc $mod" "$here/vm.sh" "$work/job.sh" "$work/payload" "$main" "$cs" "$rs" > "$work/out.txt" \
    || { tail -20 "$work/out.txt" >&2; exit 1; }

# Set UF_COMPRESSED in the compressed files' inodes.
for img in "$main" "$rs"; do
KINDS="$(cat "$work/payload/kinds-$(basename "$img" .img)")" python3 - "$img" <<'PY'
import struct, sys
path = sys.argv[1]
img = bytearray(open(path, "rb").read())
le16 = lambda b, o: struct.unpack_from("<H", b, o)[0]
le32 = lambda b, o: struct.unpack_from("<I", b, o)[0]
le64 = lambda b, o: struct.unpack_from("<Q", b, o)[0]
bs = le32(img, 36)
blk = lambda n: img[n * bs:(n + 1) * bs]
def fletcher(data):
    a = b = 0
    for (w,) in struct.iter_unpack("<I", data):
        a = (a + w) % 0xFFFFFFFF; b = (b + a) % 0xFFFFFFFF
    c1 = 0xFFFFFFFF - ((a + b) % 0xFFFFFFFF)
    c2 = 0xFFFFFFFF - ((a + c1) % 0xFFFFFFFF)
    return (c2 << 32) | c1
# The newest superblock in the checkpoint area.
sb = blk(0)
base, count = le64(sb, 112), le32(sb, 104)
for i in range(count):
    b = blk(base + i)
    if b[32:36] == b"NXSB" and le64(b, 0) == fletcher(b[8:]) and le64(b, 16) > le64(sb, 16):
        sb = b
xid = le64(sb, 16)
def entries(n):
    b = blk(n); flags, nk = le16(b, 32), le32(b, 36)
    toc, tlen = 56 + le16(b, 40), le16(b, 42)
    keys, vend = toc + tlen, (bs - 40 if flags & 1 else bs)
    for i in range(nk):
        if flags & 4:
            k, v = le16(b, toc + 4 * i), le16(b, toc + 4 * i + 2); kl = 16; vl = 16 if flags & 2 else 8
        else:
            k, kl, v, vl = struct.unpack_from("<HHHH", b, toc + 8 * i)
        yield flags & 2, b[keys + k:keys + k + kl], vend - v, vl
def omap(root, oid):
    n = root
    while True:
        best = None
        leaf = False
        for leaf, k, vo, vl in entries(n):
            if (le64(k, 0), le64(k, 8)) > (oid, xid): break
            if leaf and le64(k, 0) != oid: continue
            best = blk(n)[vo:vo + vl]
        if leaf: return le64(best, 8)
        n = le64(best, 0)
omap_root = lambda o: le64(blk(o), 48)
vol = blk(omap(omap_root(le64(sb, 160)), le64(sb, 184)))
vomap = omap_root(le64(vol, 128))
import os
names = {f"{k}.txt" for k in os.environ["KINDS"].split()}
ids, inodes, rsrc_ids = set(), {}, set()
def walk(n):
    for leaf, k, vo, vl in entries(n):
        v = blk(n)[vo:vo + vl]
        if not leaf:
            walk(omap(vomap, le64(v, 0))); continue
        h = le64(k, 0); oid, ty = h & ((1 << 60) - 1), h >> 60
        if ty == 9 and k[12:12 + (le32(k, 8) & 0x3FF) - 1].decode() in names:
            ids.add(le64(v, 0))
            if k[12:13] == b"r":
                rsrc_ids.add(le64(v, 0))
        if ty == 3:
            inodes[oid] = (n, vo)
walk(omap(vomap, le64(vol, 136)))
assert len(ids) == len(names), ids
for oid in ids:
    n, vo = inodes[oid]
    at = n * bs + vo + 68
    struct.pack_into("<I", img, at, le32(img, at) | 0x20)  # UF_COMPRESSED
    struct.pack_into("<Q", img, n * bs, fletcher(img[n * bs + 8:(n + 1) * bs]))
for oid in rsrc_ids:  # (the resource fork kinds are named r*)
    n, vo = inodes[oid]
    at = n * bs + vo + 48
    struct.pack_into("<Q", img, at, (le64(img, at) | 0x4000) & ~0x8000)  # HAS_RSRC_FORK, not NO_RSRC_FORK
    struct.pack_into("<Q", img, n * bs, fletcher(img[n * bs + 8:(n + 1) * bs]))
open(path, "wb").write(img)
PY
done

# Second pass: what the Linux driver reads from the compressed files.
{
    for d in a b c; do
        echo "mount -t apfs /dev/vd$d /mnt; cd /mnt"
        echo "if [ -d compressed ]; then for f in compressed/*.txt; do echo \"vd$d C|\$(md5sum < \$f | cut -d' ' -f1)|./\$f\"; done; fi"
        echo 'cd /; umount /mnt'
    done
} > "$work/job2.sh"
VM_EXTRA_MODULES="$crc $mod" "$here/vm.sh" "$work/job2.sh" "" "$main" "$cs" "$rs" > "$work/out2.txt" \
    || { tail -20 "$work/out2.txt" >&2; exit 1; }

for name in apfs apfs-cs apfs-rsrc; do
    sed -n "/^== manifest $name\$/,/^== manifest /p" "$work/out.txt" | grep -E '^(E|M)\|' > "$out/$name.manifest"
    # The compressed files' content as the Linux driver decompresses it.
    d=$(case $name in apfs) echo a;; apfs-cs) echo b;; *) echo c;; esac)
    { grep "^vd$d C|" "$work/out2.txt" || true; } | cut -d' ' -f2- >> "$out/$name.manifest"
    if [ "$name" != apfs-rsrc ]; then
        apfsck "$work/$name.img" || { echo "apfsck failed on $name" >&2; exit 1; }
    fi
    xz -9 < "$work/$name.img" > "$out/$name.img.xz"
done
ls -la "$out"
