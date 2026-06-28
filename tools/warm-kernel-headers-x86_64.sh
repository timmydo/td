#!/bin/sh
# warm-kernel-headers-x86_64.sh — host-side warm-prep that produces the sanitized Linux UAPI
# headers for the **x86_64** glibc 2.41 cross-build (x86_64-toolchain track) FROM the pinned linux
# source (seed/sources/linux-*.lock) via `make ARCH=x86_64 headers_install`. This is the 64-bit
# sibling of tools/warm-kernel-headers.sh (which emits ARCH=i386 headers for the i686 lane): the
# offline loop sandbox CANNOT run the kernel build, so — like warm-kernel-headers / warm-tsgo — this
# runs on the HOST and the x86_64 cross-glibc rung consumes the produced headers tarball.
#
# Output: .td-build-cache/sources/linux-headers-<ver>-x86_64.tar.gz. BEST-EFFORT: a runner without
# host make/gcc just warns; the heavy x86_64 gate fails loudly if the headers are absent.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
set -- "$root"/seed/sources/linux-*.lock
{ [ "$1" = "$root/seed/sources/linux-*.lock" ] && [ ! -e "$1" ]; } && exit 0   # no linux lock -> nothing to do
lock="$1"
file=$(sed -n 's/^file //p' "$lock" | head -1)
ver=$(printf '%s' "$file" | sed -n 's/^linux-\(.*\)\.tar\..*$/\1/p')   # e.g. 4.14.67
src="$root/.td-build-cache/sources/$file"
out="$root/.td-build-cache/sources/linux-headers-$ver-x86_64.tar.gz"
[ -f "$out" ] && exit 0                                   # already produced
[ -f "$src" ] || { echo ">> warm-kernel-headers-x86_64: linux source not warm ($src) — skipping (PREP best-effort)" >&2; exit 0; }
command -v make >/dev/null 2>&1 && command -v gcc >/dev/null 2>&1 && command -v xz >/dev/null 2>&1 \
  || { echo ">> warm-kernel-headers-x86_64: need host make+gcc+xz to produce headers — skipping (best-effort)" >&2; exit 0; }

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT INT TERM
xz -dc "$src" | tar -xf - -C "$work" --strip-components=1
( cd "$work" && make ARCH=x86_64 INSTALL_HDR_PATH="$work/hdr" headers_install >/dev/null 2>&1 ) \
  || { echo ">> warm-kernel-headers-x86_64: headers_install failed — skipping" >&2; exit 0; }
# headers_install does NOT emit linux/version.h (it is generated); glibc's configure checks it
# (LINUX_VERSION_CODE >= 2.0.10) and otherwise reports "kernel header files TOO OLD!".
maj=${ver%%.*}; rest=${ver#*.}; min=${rest%%.*}; sub=${ver##*.}
code=$(( maj*65536 + min*256 + sub ))
printf '#define LINUX_VERSION_CODE %s\n#define KERNEL_VERSION(a,b,c) (((a) << 16) + ((b) << 8) + (c))\n' "$code" \
  > "$work/hdr/include/linux/version.h"
( cd "$work/hdr/include" && tar -czf "$out.tmp" . ) && mv -f "$out.tmp" "$out"
echo ">> warm-kernel-headers-x86_64: produced $out (LINUX_VERSION_CODE=$code) from the pinned $file" >&2
