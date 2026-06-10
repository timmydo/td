#!/bin/sh
# system/td-place.sh — td's guix-free generation PLACER (M10.2).
#
# A "generation" is a bootc-style bootable OCI image (M10.1, system/td-generation.scm):
# td's reproducible userspace image MADE BOOTABLE by a /boot layer carrying that
# generation's kernel + initrd, where the initrd mounts that generation's OWN root
# (the distinct `td-root-gen-N` label, not the shared td-root — M10-design.md P1).
#
# This is the deployment side (M10-design.md step 3, "Place"): a small tool that
# runs ON THE TARGET — which has NO guix. So it is an ordinary POSIX shell script
# using only base tools (tar, gzip, coreutils, sed, grep); it never invokes guix
# and needs no Guile/store. It:
#
#   1. cracks the bootc OCI image and — driven by the OCI manifest, not a blind
#      directory scan — locates the layer carrying /boot AND the userspace layers;
#   2. verifies the image's EMBEDDED identity (boot/td-identity) matches the
#      --generation / --root-label it is being placed as, so a mislabeled image
#      cannot be installed under the wrong generation/root;
#   3. APPLIES the userspace layers into this generation's own root and stages it as
#      <root-store>/td/gen-N/root.tar (the per-generation root CONTENT — so the
#      menu's root=LABEL=td-root-gen-N refers to a root that actually exists), and
#      extracts the kernel + initrd into <boot>/td/gen-N/, recording the root LABEL
#      alongside (so the menu can be regenerated purely from on-disk state);
#   4. prunes the placed generations down to the newest --keep (removing older
#      per-generation roots AND boot dirs AND, by regeneration below, their menu
#      entries);
#   5. regenerates a marker-delimited "managed block" of GRUB menuentries — one per
#      kept generation, each `linux`/`initrd` pointing at THAT generation's placed
#      kernel/initrd and selecting THAT generation's root (root=LABEL=td-root-gen-N).
#      Everything OUTSIDE the markers (the user's grub.cfg preamble) is preserved.
#
# Crash-safety: each generation is extracted + validated in a STAGING directory and
# only swapped into place once complete, so a missing/corrupt image aborts WITHOUT
# destroying the generation already installed there (the old menu entry keeps
# pointing at intact files). Re-running for the same generation is idempotent.
# Rollback (M10.3) is then just booting an older, still-present menu entry.
#
# NOTE (scope, M10.2 — signed-off split): this tool STAGES each generation's root
# CONTENT (root.tar) and writes a root=LABEL=td-root-gen-N menu line, but it does
# NOT yet turn that content into a live, labeled ext4 filesystem (`mke2fs -L`) nor
# wire GRUB's `search`/`set root`/`set default` and the gnu.system/gnu.load closure
# path for the target's real partition layout. Creating the labeled filesystem from
# the staged root.tar and booting it is M10.3 (the real boot+rollback test). This
# tool's contract is: apply userspace layers + stage the per-generation root, place
# /boot per-generation, write a correct per-generation menu, and prune.
set -eu

# Markers delimiting the block this tool owns in grub.cfg. Everything between them
# is regenerated on every run; everything else is the user's and is preserved.
BEGIN_MARK='# >>> td generations (managed by td-place) >>>'
END_MARK='# <<< td generations (managed by td-place) <<<'

img=; gen=; root_label=; boot_dir=; root_store=; grub_cfg=; keep=
while [ $# -gt 0 ]; do
  case "$1" in
    --image)      img=$2;        shift 2 ;;
    --generation) gen=$2;        shift 2 ;;
    --root-label) root_label=$2; shift 2 ;;
    --boot-dir)   boot_dir=$2;   shift 2 ;;
    --root-store) root_store=$2; shift 2 ;;
    --grub-cfg)   grub_cfg=$2;   shift 2 ;;
    --keep)       keep=$2;       shift 2 ;;
    *) echo "td-place: unknown argument: $1" >&2; exit 2 ;;
  esac
done
for pair in image:img generation:gen root-label:root_label \
            boot-dir:boot_dir root-store:root_store grub-cfg:grub_cfg keep:keep; do
  name=${pair%%:*}; var=${pair#*:}
  eval "val=\${$var}"
  [ -n "$val" ] || { echo "td-place: missing required --$name" >&2; exit 2; }
done
case "$gen" in *[!0-9]*|'') echo "td-place: --generation must be a positive integer: $gen" >&2; exit 2 ;; esac
case "$keep" in *[!0-9]*|'') echo "td-place: --keep must be a positive integer: $keep" >&2; exit 2 ;; esac
# --keep 0 would prune EVERY generation (tail -n 0 keeps none) — including the one
# being placed. A placer that deletes everything it just installed is never the
# intent; require at least one kept generation.
[ "$keep" -ge 1 ] || { echo "td-place: --keep must be at least 1 (refusing to prune every generation): $keep" >&2; exit 2; }

work=$(mktemp -d)
boot_stage=; root_stage=
trap 'rm -rf "$work" "$boot_stage" "$root_stage"' EXIT

# Deterministic tar (sorted names, fixed mtime/owner) — so a staged root.tar
# assembled from multiple layers is reproducible (the test `--check`s it). The
# single-layer fast path below copies the layer byte-for-byte instead.
det_tar() { # SRC-DIR OUTFILE
  tar --sort=name --mtime=@1 --owner=0 --group=0 --numeric-owner -cf "$2" -C "$1" .
}

# --- 1. Crack the OCI image; select layers by the MANIFEST, not a blind scan. ---
# A blind `*/layer.tar` scan would also consider ORPHAN layer dirs not referenced
# by manifest.json; only manifest-referenced layers are part of this image.
tar xzf "$img" -C "$work"
[ -f "$work/manifest.json" ] || {
  echo "td-place: $img has no manifest.json — not an OCI image" >&2; exit 1; }

# Ordered list of manifest-referenced layers ("<hex>/layer.tar", one per line).
manifest_layers=$(tr -d '\n' < "$work/manifest.json" \
  | sed -n 's/.*"Layers":\[\([^][]*\)\].*/\1/p' \
  | tr ',' '\n' | sed -n 's/^[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p')
[ -n "$manifest_layers" ] || {
  echo "td-place: manifest.json lists no Layers — malformed image" >&2; exit 1; }

# Among the manifest layers, the boot layer is the one carrying /boot/bzImage.
boot_layer=
for lt in $manifest_layers; do
  [ -f "$work/$lt" ] || {
    echo "td-place: manifest references missing layer $lt — malformed image" >&2; exit 1; }
  if tar tf "$work/$lt" | grep -Eq '^(\./)?boot/bzImage$'; then
    boot_layer=$lt; break
  fi
done
[ -n "$boot_layer" ] || {
  echo "td-place: no manifest-referenced layer carries /boot/bzImage in $img — not a bootc generation image" >&2
  exit 1
}
# The userspace layers are the manifest layers OTHER than the boot layer, in order.
userspace_layers=
for lt in $manifest_layers; do
  [ "$lt" = "$boot_layer" ] && continue
  userspace_layers="$userspace_layers $lt"
done

# --- 2. Verify the image's embedded identity before placing anything. ----------
# The bootc image carries boot/td-identity (generation + root-label). Placing a
# gen-2 image under --generation 1 (or a foreign root label) would produce a menu
# entry that LIES about what it boots; bind the args to the image here.
tar xf "$work/$boot_layer" -C "$work" --strip-components=1 boot/td-identity 2>/dev/null || {
  echo "td-place: bootc image carries no boot/td-identity — cannot verify it is the gen $gen / $root_label image" >&2
  exit 1
}
img_gen=$(sed -n 's/^generation=//p' "$work/td-identity")
img_label=$(sed -n 's/^root-label=//p' "$work/td-identity")
[ "$img_gen" = "$gen" ] || {
  echo "td-place: image identity generation=$img_gen does not match --generation $gen" >&2; exit 1; }
[ "$img_label" = "$root_label" ] || {
  echo "td-place: image identity root-label=$img_label does not match --root-label $root_label" >&2; exit 1; }

# --- 3a. Stage this generation's /boot (kernel + initrd + identity). ------------
# Extract + validate in a sibling staging dir; the live gen dir is untouched until
# the staged copy is complete and good (crash-safe replacement).
mkdir -p "$boot_dir/td"
boot_stage="$boot_dir/td/.gen-$gen.staging.$$"
rm -rf "$boot_stage"; mkdir -p "$boot_stage"
tar xf "$work/$boot_layer" -C "$boot_stage" --strip-components=1 \
    boot/bzImage boot/initrd.cpio.gz boot/td-identity
[ -s "$boot_stage/bzImage" ] && [ -s "$boot_stage/initrd.cpio.gz" ] || {
  echo "td-place: boot layer did not yield a non-empty bzImage + initrd.cpio.gz" >&2
  exit 1
}
# Record this generation's root label so the menu block can be regenerated purely
# from on-disk state (each generation root is self-describing).
printf '%s\n' "$root_label" > "$boot_stage/root-label"

# --- 3b. APPLY the userspace layers into this generation's own root. ------------
# The result is the per-generation root CONTENT, staged as root.tar — so
# root=LABEL=td-root-gen-N refers to a root that actually exists (M10.3 writes it
# onto a labeled filesystem). Single userspace layer (td's case) is copied
# byte-for-byte (the applied rootfs == that layer); multiple layers are applied in
# order with OCI whiteouts, then re-tarred deterministically.
mkdir -p "$root_store/td"
root_stage="$root_store/td/.gen-$gen.staging.$$"
rm -rf "$root_stage"; mkdir -p "$root_stage"
n_userspace=$(printf '%s\n' $userspace_layers | grep -c .)
if [ "$n_userspace" -eq 0 ]; then
  echo "td-place: image has a /boot layer but no userspace layer — no root to place" >&2
  exit 1
elif [ "$n_userspace" -eq 1 ]; then
  # shellcheck disable=SC2086  # intentional word-split: exactly one layer token
  set -- $userspace_layers
  cp "$work/$1" "$root_stage/root.tar"                  # applied rootfs == the lone layer
else
  rootfs="$root_stage/rootfs"; mkdir -p "$rootfs"
  for ul in $userspace_layers; do
    tar xf "$work/$ul" -C "$rootfs"
    if find "$rootfs" -name '.wh..wh..opq' | grep -q .; then
      echo "td-place: opaque-dir OCI whiteouts are not supported (multi-layer image)" >&2
      exit 1
    fi
    find "$rootfs" -name '.wh.*' | while IFS= read -r wh; do
      d=$(dirname "$wh"); b=$(basename "$wh"); rm -rf "$d/${b#.wh.}" "$wh"
    done
  done
  det_tar "$rootfs" "$root_stage/root.tar"
  rm -rf "$rootfs"
fi
[ -s "$root_stage/root.tar" ] || {
  echo "td-place: applied userspace root is empty — refusing to place gen $gen" >&2; exit 1; }

# --- 3c. Atomic swap: only now replace the live generation. ---------------------
rm -rf "$boot_dir/td/gen-$gen"; mv "$boot_stage" "$boot_dir/td/gen-$gen"; boot_stage=
rm -rf "$root_store/td/gen-$gen"; mv "$root_stage" "$root_store/td/gen-$gen"; root_stage=

# --- 4. Prune: keep only the newest --keep generations (boot AND root). ---------
present=$(ls "$boot_dir/td" 2>/dev/null \
            | sed -n 's/^gen-\([0-9][0-9]*\)$/\1/p' | sort -n)
kept=$(printf '%s\n' "$present" | sort -n | tail -n "$keep" | tr '\n' ' ')
for g in $present; do
  case " $kept " in
    *" $g "*) : ;;
    *) rm -rf "$boot_dir/td/gen-$g" "$root_store/td/gen-$g" ;;
  esac
done

# --- 5. Regenerate the managed GRUB block from the kept generations. ------------
# Strip any existing managed block (idempotent), preserving the user's preamble,
# then append a freshly generated block — newest generation first.
mkdir -p "$(dirname "$grub_cfg")"
tmp=$(mktemp)
if [ -f "$grub_cfg" ]; then
  sed "\|^${BEGIN_MARK}\$|,\|^${END_MARK}\$|d" "$grub_cfg" > "$tmp"
fi
{
  echo "$BEGIN_MARK"
  for g in $(printf '%s\n' $kept | sort -rn); do
    gd="$boot_dir/td/gen-$g"
    label=$(cat "$gd/root-label")
    echo "menuentry \"td generation $g (root=$label)\" {"
    echo "  linux /td/gen-$g/bzImage root=LABEL=$label quiet"
    echo "  initrd /td/gen-$g/initrd.cpio.gz"
    echo "}"
  done
  echo "$END_MARK"
} >> "$tmp"
mv "$tmp" "$grub_cfg"

echo "td-place: placed generation $gen (root=$root_label); kept generations: $kept"
