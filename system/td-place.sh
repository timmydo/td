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
#   1. cracks the bootc OCI image and locates the layer carrying /boot;
#   2. extracts that generation's kernel + initrd into its own per-generation root
#      under <boot>/td/gen-N/, and records the generation's root LABEL alongside
#      (so the menu can be regenerated purely from on-disk state — idempotent);
#   3. prunes the placed generations down to the newest --keep (removing older
#      per-generation roots AND, by regeneration below, their menu entries);
#   4. regenerates a marker-delimited "managed block" of GRUB menuentries — one per
#      kept generation, each `linux`/`initrd` pointing at THAT generation's placed
#      kernel/initrd and selecting THAT generation's root (root=LABEL=td-root-gen-N).
#      Everything OUTSIDE the markers (the user's grub.cfg preamble) is preserved.
#
# Re-running for the same generation is idempotent (the per-generation root is
# replaced, the managed block fully regenerated from on-disk state). Rollback
# (M10.3) is then just booting an older, still-present menu entry.
#
# NOTE (scope, M10.2): the menuentry path is written GRUB-root-relative
# (/td/gen-N/...), and root selection is by the generation's filesystem label
# (the real mechanism is that gen-N's initrd, placed here, mounts gen-N's root —
# proven in M10.1). Wiring the GRUB `search`/`set root` for the target's actual
# partition layout, the `set default`, and the gnu.system/gnu.load closure path is
# M10.3's concern (the real boot+rollback test). This tool's contract is: place
# /boot per-generation, write a correct per-generation menu, and prune.
set -eu

# Markers delimiting the block this tool owns in grub.cfg. Everything between them
# is regenerated on every run; everything else is the user's and is preserved.
BEGIN_MARK='# >>> td generations (managed by td-place) >>>'
END_MARK='# <<< td generations (managed by td-place) <<<'

img=; gen=; root_label=; boot_dir=; grub_cfg=; keep=
while [ $# -gt 0 ]; do
  case "$1" in
    --image)      img=$2;        shift 2 ;;
    --generation) gen=$2;        shift 2 ;;
    --root-label) root_label=$2; shift 2 ;;
    --boot-dir)   boot_dir=$2;   shift 2 ;;
    --grub-cfg)   grub_cfg=$2;   shift 2 ;;
    --keep)       keep=$2;       shift 2 ;;
    *) echo "td-place: unknown argument: $1" >&2; exit 2 ;;
  esac
done
for pair in image:img generation:gen root-label:root_label \
            boot-dir:boot_dir grub-cfg:grub_cfg keep:keep; do
  name=${pair%%:*}; var=${pair#*:}
  eval "val=\${$var}"
  [ -n "$val" ] || { echo "td-place: missing required --$name" >&2; exit 2; }
done
case "$gen" in *[!0-9]*|'') echo "td-place: --generation must be a positive integer: $gen" >&2; exit 2 ;; esac
case "$keep" in *[!0-9]*|'') echo "td-place: --keep must be a positive integer: $keep" >&2; exit 2 ;; esac

# --- 1. Crack the bootc OCI image; find the layer that carries /boot. ----------
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
tar xzf "$img" -C "$work"

boot_layer=
for lt in "$work"/*/layer.tar; do
  [ -f "$lt" ] || continue
  if tar tf "$lt" | grep -Eq '^(\./)?boot/bzImage$'; then
    boot_layer=$lt
    break
  fi
done
[ -n "$boot_layer" ] || {
  echo "td-place: no layer carrying /boot/bzImage in $img — not a bootc generation image" >&2
  exit 1
}

# --- 2. Extract kernel+initrd into this generation's own per-generation root. ---
gen_dir="$boot_dir/td/gen-$gen"
rm -rf "$gen_dir"
mkdir -p "$gen_dir"
# --strip-components=1 drops the leading boot/ so files land directly in gen_dir.
tar xf "$boot_layer" -C "$gen_dir" --strip-components=1 \
    boot/bzImage boot/initrd.cpio.gz
[ -f "$gen_dir/bzImage" ] && [ -f "$gen_dir/initrd.cpio.gz" ] || {
  echo "td-place: boot layer did not yield bzImage + initrd.cpio.gz" >&2
  exit 1
}
# Record this generation's root label so the menu block can be regenerated purely
# from on-disk state (each generation root is self-describing).
printf '%s\n' "$root_label" > "$gen_dir/root-label"

# --- 3. Prune: keep only the newest --keep generations. ------------------------
# Enumerate placed generations (numeric), keep the newest $keep, remove the rest.
present=$(ls "$boot_dir/td" 2>/dev/null \
            | sed -n 's/^gen-\([0-9][0-9]*\)$/\1/p' | sort -n)
kept=$(printf '%s\n' "$present" | sort -n | tail -n "$keep")
for g in $present; do
  keep_this=no
  for k in $kept; do
    [ "$g" = "$k" ] && { keep_this=yes; break; }
  done
  [ "$keep_this" = yes ] || rm -rf "$boot_dir/td/gen-$g"
done

# --- 4. Regenerate the managed GRUB block from the kept generations. -----------
# Strip any existing managed block (idempotent), preserving the user's preamble,
# then append a freshly generated block — newest generation first.
mkdir -p "$(dirname "$grub_cfg")"
tmp=$(mktemp)
if [ -f "$grub_cfg" ]; then
  sed "\|^${BEGIN_MARK}\$|,\|^${END_MARK}\$|d" "$grub_cfg" > "$tmp"
fi
{
  echo "$BEGIN_MARK"
  for g in $(printf '%s\n' "$kept" | sort -rn); do
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

echo "td-place: placed generation $gen (root=$root_label); kept generations: $(printf '%s' "$kept" | tr '\n' ' ')"
