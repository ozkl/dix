#!/usr/bin/env bash
# A tiny Linux VM for making test images with file systems this machine's
# kernel cannot write (HFS, HFS+, APFS): an Ubuntu kernel and busybox in an
# initramfs, booted by QEMU (no KVM needed, ~5 s). Used by the make-*.sh
# scripts next to it.
#
#   vm.sh <job.sh> <payload-dir> <disk-image>...
#
# The disks are /dev/vda, /dev/vdb, …; the payload is at /payload; the job
# runs as root and its output is printed. Needs qemu-system-x86_64, zstd,
# cpio and apt-get (for the kernel packages, downloaded once to $VM_CACHE).
set -euo pipefail

K="${VM_KERNEL:-6.8.0-149-generic}"
cache="${VM_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/dix-test-vm}"
root="$cache/$K"

if [ ! -f "$root/boot/vmlinuz-$K" ]; then
    mkdir -p "$cache/debs" "$root"
    (cd "$cache/debs" && apt-get download "linux-image-unsigned-$K" "linux-modules-$K" \
        "linux-modules-extra-$K" busybox-static)
    for p in "$cache"/debs/*.deb; do dpkg -x "$p" "$root"; done
fi

job="$1"; payload="$2"; shift 2
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work"/root/{bin,proc,sys,dev,mnt,mnt2,payload,lib/modules}
cp "$root/usr/bin/busybox" "$work/root/bin/"
mods="$root/lib/modules/$K/kernel"
for m in fs/nls/nls_utf8 fs/nls/mac-roman fs/hfs/hfs fs/hfsplus/hfsplus; do
    zstd -q -d -f "$mods/$m.ko.zst" -o "$work/root/lib/modules/$(basename "$m").ko"
done
# Extra modules built elsewhere (the APFS one), loaded after these.
if [ -n "${VM_EXTRA_MODULES:-}" ]; then
    cp $VM_EXTRA_MODULES "$work/root/lib/modules/"
fi
if [ -n "$payload" ]; then cp -a "$payload"/. "$work/root/payload/"; fi
cp "$job" "$work/root/job.sh"
cat > "$work/root/init" <<'I'
#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc proc /proc; mount -t sysfs sys /sys; mount -t devtmpfs dev /dev
for m in nls_utf8 mac-roman hfs hfsplus; do insmod /lib/modules/$m.ko; done
for pass in 1 2; do for m in /lib/modules/*.ko; do insmod $m 2>/dev/null; done; done
export TZ=UTC
echo "== job"
sh -e /job.sh 2>&1
echo "== job exit $?"
sync
poweroff -f
I
chmod +x "$work/root/init" "$work/root/job.sh"
(cd "$work/root" && find . | cpio -o -H newc 2>/dev/null | gzip -1) > "$work/initrd.gz"
drives=()
for d in "$@"; do drives+=(-drive "file=$d,format=raw,if=virtio"); done
out="$(timeout 900 qemu-system-x86_64 -m 1024 -nographic -no-reboot -kernel "$root/boot/vmlinuz-$K" \
    -initrd "$work/initrd.gz" -append "console=ttyS0 quiet panic=-1" "${drives[@]}" 2>&1 \
    | tr -d '\r' | sed 's/.*== job/== job/' | sed -n '/^== job$/,/^== job exit/p')"
echo "$out" | sed '1d;$d'
echo "$out" | tail -n1 | grep -q '^== job exit 0$'
