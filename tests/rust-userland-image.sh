#!/bin/sh
# tests/rust-userland-image.sh — assemble + verify a td-NATIVE OCI image whose
# userspace is a td-BUILT Rust userland tool (procs/fd/ripgrep/sd/eza/bat), built
# guix-free by tests/crate-free-build.sh. This SHIPS the td-built binary through
# td's OWN image builder (`td-builder oci-image`, system-image-native) instead of
# the guix `(gnu packages rust-apps)` object the shipped system used to carry (now
# removed from the retired guix operating-system declarations) — so the shipped tool is td's
# OWN bytes, not guix-built ones. The rust/gcc toolchain
# LIBS the binary links (glibc, libgcc_s) stay guix for now (retired last by the
# /td/store source-bootstrap); they are laid into the image at their /gnu/store
# paths so the image is SELF-CONTAINED (crun runs it with no host store bound).
#
# The closure is computed td-natively (no `guix gc`, no guix system image):
#   1. the realized binary tree is scanned for /gnu/store references (the ELF
#      PT_INTERP + DT_RUNPATH via `td-builder elf-interp`/`elf-rpath`, plus any
#      embedded store path), giving the binary's DIRECT runtime deps;
#   2. `td-builder store-closure-scan /gnu/store` expands each to its full closure;
#   3. those guix toolchain trees + the td-built binary tree are laid at their
#      /gnu/store locations into a rootfs and packed with `td-builder oci-image`.
#
# Assertions (all DURABLE — they hold with no guix oracle in the room):
#   [behavioral]  crun runs the td-built tool IN the image (no host store bound) and
#                 it does its job (matches EXPECT) — the td bytes execute.
#   [repro]       packing the same rootfs twice is byte-identical (prime directive 1,
#                 proven by td itself, not `guix build --check`).
#   [structural]  the binary's interpreter + libs resolve from the image's OWN closure
#                 (a self-contained image; the negative control proves it discriminates).
#   [self-discrim] a bogus exec in the same image FAILS.
#
# Usage: rust-userland-image.sh NS OUT BIN EXPECT -- ARG...
#   NS      the realized newstore tree of the binary (crate-free-build's NS=).
#   OUT     its canonical store path (crate-free-build's OUT=, e.g. /gnu/store/<h>-fd).
#   BIN     the binary name under OUT/bin (fd, rg, sd, procs, eza, bat).
#   EXPECT  a substring the tool must emit when run with ARG... in the image.
#   ARG...  the tool's arguments (after `--`); run as `OUT/bin/BIN ARG...`.
# Reads env: TB (td-builder) GUIX(=guix) SKOPEO CRUN ROOT(=pwd).
set -eu

NS=${1:?usage: rust-userland-image.sh NS OUT BIN EXPECT -- ARG...}
OUT=${2:?missing canonical OUT path}
BIN=${3:?missing binary name}
EXPECT=${4:?missing expected substring}
shift 4
[ "${1:-}" = "--" ] && shift || true   # drop the -- separator
# remaining "$@" = the tool's args

: "${TB:?TB unset (load_stage0)}"
GUIX=${GUIX:-guix}
SKOPEO=${SKOPEO:?set SKOPEO to the skopeo binary}
CRUN=${CRUN:?set CRUN to the crun binary}
root=${ROOT:-$(pwd)}

test -x "$NS/bin/$BIN" || { echo "FAIL: no td-built binary at $NS/bin/$BIN" >&2; exit 1; }
test "$(basename "$NS")" = "$(basename "$OUT")" || {
  echo "FAIL: NS basename $(basename "$NS") != OUT basename $(basename "$OUT")" >&2; exit 1; }

work="$root/.td-build-cache/$BIN-userland-image"
chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"; mkdir -p "$work"
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT

# --- (1) direct refs: the /gnu/store store paths the binary needs at run time.
# elf-interp/elf-rpath give the loader + RUNPATH lib dirs; a tree-wide scan catches
# any other embedded store path. Truncate every hit to its 3-component store path.
store_paths() { grep -rhoE '/gnu/store/[0-9a-z]{32}-[a-zA-Z0-9._+-]+' "$@" 2>/dev/null; }
{
  "$TB" elf-interp "$NS/bin/$BIN" 2>/dev/null || true
  "$TB" elf-rpath  "$NS/bin/$BIN" 2>/dev/null | tr ':' '\n' || true
  store_paths "$NS/bin/$BIN"
} | grep -oE '/gnu/store/[0-9a-z]{32}-[a-zA-Z0-9._+-]+' | sort -u > "$work/refs.txt"
# The binary's OWN OUT path is content-addressed at /gnu/store but registered in TD's db,
# NOT /var/guix/db; it is laid into the rootfs directly (step 3 below). Drop it before the
# guix-db closure lookup so that lookup sees only guix toolchain paths (the PT_INTERP +
# DT_RUNPATH libs), which ARE in /var/guix/db. Any OTHER td-interned ref would make
# store-closure fail LOUD ("root not in store DB") — never ship a silently-incomplete image.
grep -vxF "$OUT" "$work/refs.txt" > "$work/direct.txt" || true
test -s "$work/direct.txt" || { echo "FAIL: no guix /gnu/store runtime refs in $NS/bin/$BIN (only its own OUT? a static-store binary?)" >&2; exit 1; }
echo "  [structural] $BIN has $(wc -l < "$work/direct.txt") direct /gnu/store runtime ref(s) (PT_INTERP + DT_RUNPATH + embedded)" >&2

# --- (2) closure: expand each direct ref to its full closure by CONTENT-SCANNING the live
# /gnu/store (td-builder store-closure-scan — no store DB, no /var/guix read, no guix
# process). The toolchain seed must be realized first (caller does).
: > "$work/closure.txt"
while read -r p; do
  "$TB" store-closure-scan /gnu/store "$p" >> "$work/closure.txt" \
    || { echo "FAIL: store-closure of $p failed (toolchain seed not realized?)" >&2; exit 1; }
done < "$work/direct.txt"
sort -u "$work/closure.txt" -o "$work/closure.txt"
nclo=$(wc -l < "$work/closure.txt")
echo "  [structural] td-native closure (td-builder store-closure-scan /gnu/store): $nclo guix toolchain path(s)" >&2

# --- (3) rootfs: lay each closure member at its /gnu/store location, then the td-built
# binary tree at ITS canonical path. -a preserves the exec bits + symlinks.
build_rootfs() {  # $1 = dest rootfs dir
  rfs=$1
  mkdir -p "$rfs/gnu/store"
  while read -r c; do
    b=$(basename "$c")
    test -e "$rfs/gnu/store/$b" || cp -a "$c" "$rfs/gnu/store/$b"
  done < "$work/closure.txt"
  cp -a "$NS" "$rfs/gnu/store/$(basename "$OUT")"
  chmod -R u+w "$rfs"
}
build_rootfs "$work/rootfs"
test -x "$work/rootfs$OUT/bin/$BIN" || { echo "FAIL: td binary absent from assembled rootfs ($OUT/bin/$BIN)" >&2; exit 1; }

cat > "$work/image-config.json" <<EOF
{"repoTag":"td-$BIN:latest","env":["PATH=$OUT/bin:/bin","HOME=/","TERM=dumb"],"entrypoint":["$OUT/bin/$BIN"]}
EOF

# --- pack twice; assert byte-identical (INTRINSIC reproducibility, td's own oracle).
echo ">> td-builder oci-image (no guix system image): pack the rootfs into a docker-archive" >&2
"$TB" oci-image "$work/rootfs" "$work/image-config.json" "$work/img1.tar" >/dev/null \
  || { echo "FAIL: td-builder oci-image failed" >&2; exit 1; }
"$TB" oci-image "$work/rootfs" "$work/image-config.json" "$work/img2.tar" >/dev/null \
  || { echo "FAIL: second oci-image pack failed" >&2; exit 1; }
h1=$(sha256sum < "$work/img1.tar" | cut -d' ' -f1)
h2=$(sha256sum < "$work/img2.tar" | cut -d' ' -f1)
test "$h1" = "$h2" || { echo "FAIL: td-native image NOT byte-reproducible ($h1 != $h2)" >&2; exit 1; }
echo "  [repro] packing the same rootfs twice is byte-identical ($h1) — td's own reproducibility oracle" >&2

# --- skopeo (a foreign OCI impl) accepts the bytes.
echo ">> skopeo copy docker-archive -> oci layout (foreign-tool acceptance)" >&2
"$SKOPEO" --tmpdir "$work" copy --insecure-policy "docker-archive:$work/img1.tar" "oci:$work/layout:td" >/dev/null
digest=$("$SKOPEO" --tmpdir "$work" inspect --format '{{.Digest}}' "oci:$work/layout:td")
case "$digest" in
  sha256:*) echo "  [structural] skopeo loaded the td-native $BIN image (manifest $digest)" >&2 ;;
  *) echo "FAIL: skopeo produced no sha256 manifest digest (got: '$digest')" >&2; exit 1 ;;
esac

# --- unpack the single layer into a runtime bundle rootfs (NO host store bound).
mkdir -p "$work/extract" "$work/runfs"
tar xf "$work/img1.tar" -C "$work/extract"
layer=$(find "$work/extract" -name layer.tar | head -1)
test -n "$layer" || { echo "FAIL: no layer.tar inside the image" >&2; exit 1; }
tar xf "$layer" -C "$work/runfs"
test -x "$work/runfs$OUT/bin/$BIN" || { echo "FAIL: $BIN absent from the unpacked image rootfs" >&2; exit 1; }

HOSTUID=$(id -u); HOSTGID=$(id -g)
gen_config() {  # $1 = JSON array literal for process.args (crun reads <bundle>/config.json)
  cat > "$work/config.json" <<EOF
{
  "ociVersion": "1.0.0",
  "process": {
    "terminal": false, "user": { "uid": 0, "gid": 0 }, "args": $1,
    "env": [ "PATH=$OUT/bin:/bin:/usr/bin", "HOME=/", "TERM=dumb" ],
    "cwd": "/", "noNewPrivileges": true
  },
  "root": { "path": "runfs", "readonly": true },
  "hostname": "td-rust-userland",
  "mounts": [
    { "destination": "/proc", "type": "proc", "source": "proc" },
    { "destination": "/dev", "type": "tmpfs", "source": "tmpfs",
      "options": [ "nosuid", "strictatime", "mode=755", "size=65536k" ] }
  ],
  "linux": {
    "uidMappings": [ { "containerID": 0, "hostID": ${HOSTUID}, "size": 1 } ],
    "gidMappings": [ { "containerID": 0, "hostID": ${HOSTGID}, "size": 1 } ],
    "namespaces": [
      { "type": "pid" }, { "type": "ipc" }, { "type": "uts" },
      { "type": "mount" }, { "type": "user" }, { "type": "network" }
    ]
  }
}
EOF
}
run_ctr() { ( cd "$work" && "$CRUN" --cgroup-manager=disabled --root="$work/state" run "$1" ); }

# JSON-encode the entrypoint + args into a process.args array.
args_json="[ \"$OUT/bin/$BIN\""
for a in "$@"; do args_json="$args_json, \"$a\""; done
args_json="$args_json ]"

# --- (POSITIVE) the td-built tool runs IN the image and does its job.
echo ">> crun run (no host store bound): $BIN $*" >&2
gen_config "$args_json"
if ! out=$(run_ctr "td-$BIN-pos" 2>&1); then
  echo "FAIL: the td-built $BIN did not run in the td-native image (crun exited non-zero)" >&2
  printf '%s\n' "$out" >&2
  exit 1
fi
printf '%s\n' "$out" | grep -qF "$EXPECT" \
  || { echo "FAIL: $BIN ran but did not emit '$EXPECT' (got: $out)" >&2; exit 1; }
echo "  [behavioral] the td-built '$BIN' ran in the td-native image (its OWN bytes, no host store bound) and emitted '$EXPECT' — it works" >&2

# --- (NEGATIVE) a bogus exec MUST fail (the green discriminates).
gen_config '[ "/gnu/store/td-nonexistent-xyz/bin/nope" ]'
if run_ctr "td-$BIN-neg" >/dev/null 2>&1; then
  echo "FAIL: a non-existent binary RAN — the check cannot discriminate" >&2; exit 1
fi
echo "  [self-discrim] a bogus exec in the same image failed (the green discriminates)" >&2

echo "PASS: rust-userland-image — a td-NATIVE OCI image (td-builder oci-image, NO guix system image) ships the td-BUILT '$BIN' (guix-free crates) + its toolchain closure, laid out td-natively (td-builder store-closure-scan /gnu/store); skopeo loads it; crun runs '$BIN' from the image's OWN bytes (no host store bound) doing its job; byte-reproducible; self-discriminating."
