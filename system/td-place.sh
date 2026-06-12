#!/bin/sh
# system/td-place.sh — td's guix-free generation PLACER (M10.2, grown in M10.3).
#
# A "generation" is a bootc-style bootable OCI image (M10.1, system/td-generation.scm):
# td's reproducible userspace MADE BOOTABLE by a /boot layer carrying that
# generation's kernel + initrd, where the initrd mounts that generation's OWN root
# (the distinct `td-root-gen-N` label, not the shared td-root — M10-design.md P1).
#
# This is the deployment side (M10-design.md step 3, "Place"): a small tool that
# runs ON THE TARGET — which has NO guix. So it is an ordinary POSIX shell script
# using only base tools (tar, gzip, coreutils, sed, grep, and — for --mkfs —
# mke2fs); it never invokes guix and needs no Guile/store. It:
#
#   1. obtains the image content. LEGACY mode (--image) cracks a local
#      docker-archive tarball. VERIFIED mode (--registry --digest --pubkey,
#      M12, DESIGN §2.7) pulls from a signed static registry instead and
#      REFUSES to place unless the §2.7 pull contract holds FIRST: a signed
#      identity statement exists for the demanded manifest digest, its
#      detached signify signature verifies with --pubkey, the statement states
#      that digest, the manifest blob re-hashes to it, and every referenced
#      blob re-hashes to its name (verify before placement: digest first,
#      slot second; rejects exactly unsigned, bad signature, digest mismatch —
#      anti-rollback is out of scope by design). Only then are the layers
#      decompressed and handed to the same placement path. Either way the
#      manifest — never a blind directory scan — selects the layer carrying
#      /boot AND the userspace layers;
#   2. verifies the image's EMBEDDED identity (boot/td-identity) matches the
#      --generation / --root-label it is being placed as, so a mislabeled image
#      cannot be installed under the wrong generation/root; the identity also
#      carries system= (the store path the GRUB entry must boot — gnu.system/
#      gnu.load) and root-uuid= (the deterministic filesystem UUID for --mkfs);
#   3. APPLIES the userspace layers into this generation's own root, stages it as
#      <root-store>/td/gen-N/root.tar, VERIFIES the identity's system path is
#      actually IN that root (the menu must point at a root that boots, not just
#      one that exists), and extracts kernel + initrd into <boot>/td/gen-N/,
#      recording root-label/system/root-uuid alongside (so the menu can be
#      regenerated purely from on-disk state); the placed copy of td-identity
#      gains `image-digest=sha256:<hex>` — the §2.7 identity of what was
#      placed (the VERIFIED manifest digest in registry mode, the artifact's
#      sha256 in legacy mode), which the image cannot carry itself;
#   4. with --mkfs, turns the staged root content into a LIVE ext4 filesystem
#      image <root-store>/td/gen-N/root.img, labeled with this generation's root
#      label and the identity's deterministic UUID (mke2fs -d; reproducible — the
#      M10.3 disk test `guix build --check`s a tree containing it). Run as root,
#      or under fakeroot so the filesystem gets root-owned files (that is how
#      Guix's own image builder runs mke2fs). M11: a dm-verity hash tree is then
#      APPENDED to root.img (fixed salt + identity UUID — still reproducible),
#      self-verified, and the root hash + hash offset are recorded next to the
#      generation's boot files (verity-roothash, verity-hashoffset) — the
#      image cannot carry its own root hash (self-reference, DESIGN §2.7);
#   5. prunes the placed generations down to the newest --keep (removing older
#      per-generation roots AND boot dirs AND, by regeneration below, their menu
#      entries);
#   6. regenerates a marker-delimited "managed block" of GRUB config:
#        - `search --no-floppy --label <boot-label> --set=root` when a boot
#          partition label is known (--boot-label, remembered on disk) — so the
#          entries' /td/gen-N/... paths resolve on the right partition;
#        - `set default=td-gen-<newest>` — newest generation boots by default;
#        - the MANUAL-ROLLBACK HOOK: `if [ -s /td/default.cfg ]; then source
#          /td/default.cfg; fi`. Rolling back = writing `set default=td-gen-<M>`
#          into td/default.cfg on the boot partition and rebooting. The placer
#          NEVER writes that file; it only gives it the last word on `default`.
#        - one menuentry per kept generation (`--id td-gen-N`), each loading THAT
#          generation's placed kernel/initrd and selecting THAT generation's root.
#          M11: a generation with verity records (placed with --mkfs) gets
#          `td.roothash=<hash> td.hashoffset=<bytes>` — the initrd opens its
#          labeled partition as the dm-verity device and mounts it read-only at
#          /gnu/store ("/" is a declared tmpfs; no root= at all); an mkfs-less
#          placement keeps the legacy bare-label `root=td-root-gen-N` line.
#          Both forms carry gnu.system=<system> gnu.load=<system>/boot
#          (+ per-generation --extra-kernel-args, recorded on disk).
#      Everything OUTSIDE the markers (the user's grub.cfg preamble) is preserved.
#
# Crash-safety: each generation is extracted + validated in a STAGING directory and
# only swapped into place once complete, so a missing/corrupt image aborts WITHOUT
# destroying the generation already installed there (the old menu entry keeps
# pointing at intact files). Re-running for the same generation is idempotent.
# Rollback (M10.3) is booting an older, still-present menu entry — see the hook.
set -eu

# Markers delimiting the block this tool owns in grub.cfg. Everything between them
# is regenerated on every run; everything else is the user's and is preserved.
BEGIN_MARK='# >>> td generations (managed by td-place) >>>'
END_MARK='# <<< td generations (managed by td-place) <<<'

# M11: the dm-verity salt — FIXED ("td-verity-salt-v0", zero-padded to the
# 32-byte default) so the appended hash tree, and with it root.img, is
# bit-reproducible; per-image distinctness comes from the identity's
# deterministic UUID. Kept in sync with tests/place-check.scm.
VERITY_SALT=74642d7665726974792d73616c742d7630000000000000000000000000000000

img=; gen=; root_label=; boot_dir=; root_store=; grub_cfg=; keep=
boot_label=; extra_kernel_args=; mkfs=no
registry=; digest=; pubkey=
while [ $# -gt 0 ]; do
  case "$1" in
    --image)      img=$2;        shift 2 ;;
    --registry)   registry=$2;   shift 2 ;;
    --digest)     digest=$2;     shift 2 ;;
    --pubkey)     pubkey=$2;     shift 2 ;;
    --generation) gen=$2;        shift 2 ;;
    --root-label) root_label=$2; shift 2 ;;
    --boot-dir)   boot_dir=$2;   shift 2 ;;
    --root-store) root_store=$2; shift 2 ;;
    --grub-cfg)   grub_cfg=$2;   shift 2 ;;
    --keep)       keep=$2;       shift 2 ;;
    --boot-label) boot_label=$2; shift 2 ;;
    --extra-kernel-args) extra_kernel_args=$2; shift 2 ;;
    --mkfs)       mkfs=yes;      shift 1 ;;
    *) echo "td-place: unknown argument: $1" >&2; exit 2 ;;
  esac
done
# Input mode: exactly one of LEGACY (--image) or VERIFIED (--registry +
# --digest + --pubkey, all three) — M12, DESIGN §2.7.
verified=no
if [ -n "$registry" ] || [ -n "$digest" ] || [ -n "$pubkey" ]; then
  [ -n "$registry" ] && [ -n "$digest" ] && [ -n "$pubkey" ] || {
    echo "td-place: verified mode needs ALL of --registry/--digest/--pubkey" >&2; exit 2; }
  [ -z "$img" ] || {
    echo "td-place: --image and --registry are mutually exclusive" >&2; exit 2; }
  verified=yes
else
  [ -n "$img" ] || {
    echo "td-place: missing required --image (or --registry/--digest/--pubkey)" >&2; exit 2; }
fi
# What to call the image source in messages, per mode.
src=${img:-"$registry @ $digest"}
for pair in generation:gen root-label:root_label \
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

# --- 1. Obtain the image content; select layers by the MANIFEST, never a ------
# --- blind scan (orphan layer dirs are not part of the image). ----------------
if [ "$verified" = yes ]; then
  # VERIFIED mode (M12, DESIGN §2.7): enforce the whole pull contract BEFORE
  # unpacking anything. Reject exactly: unsigned, bad signature, digest
  # mismatch. Verification needs only base tools + signify (cf. veritysetup
  # for --mkfs).
  command -v signify >/dev/null 2>&1 || {
    echo "td-place: verified mode requires signify on PATH" >&2; exit 1; }
  case "$digest" in
    sha256:*) : ;;
    *) echo "td-place: --digest must be sha256:<hex>: $digest" >&2; exit 2 ;;
  esac
  dhex=${digest#sha256:}
  case "$dhex" in
    *[!0-9a-f]*|'') echo "td-place: --digest carries non-hex content: $digest" >&2; exit 2 ;;
  esac
  [ "${#dhex}" -eq 64 ] || {
    echo "td-place: --digest must carry 64 hex chars: $digest" >&2; exit 2; }

  # (a) UNSIGNED: a signed identity statement must exist for the demanded digest.
  stmt="$registry/signatures/$dhex.digest"
  [ -f "$stmt" ] || {
    echo "td-place: no identity statement for $digest in $registry — refusing unsigned content" >&2; exit 1; }
  [ -f "$stmt.sig" ] || {
    echo "td-place: no signature for $digest in $registry — refusing unsigned content" >&2; exit 1; }
  # (b) BAD SIGNATURE: the detached signify signature must verify, and the
  # statement must state the demanded digest.
  signify -V -q -p "$pubkey" -m "$stmt" -x "$stmt.sig" || {
    echo "td-place: signature verification failed for $digest — refusing to place" >&2; exit 1; }
  [ "$(cat "$stmt")" = "$digest" ] || {
    echo "td-place: signed statement does not state $digest — digest mismatch; refusing to place" >&2; exit 1; }
  # (c) DIGEST MISMATCH: the manifest blob and EVERY blob it references
  # (config + layers) must re-hash to their digests — content addressing is
  # the byte-identity between what was signed and what is about to be placed.
  mf="$registry/oci/blobs/sha256/$dhex"
  [ -f "$mf" ] || {
    echo "td-place: no manifest blob for $digest — digest mismatch; refusing to place" >&2; exit 1; }
  mh=$(sha256sum "$mf"); mh=${mh%% *}
  [ "$mh" = "$dhex" ] || {
    echo "td-place: manifest blob does not re-hash to $digest — digest mismatch; refusing to place" >&2; exit 1; }
  all_refs=$(tr -d ' \n\t' < "$mf" \
    | grep -o '"digest":"sha256:[0-9a-f]\{64\}"' \
    | sed 's/^.*sha256://; s/"$//')
  [ -n "$all_refs" ] || {
    echo "td-place: manifest for $digest references no blobs — malformed registry" >&2; exit 1; }
  for bh in $all_refs; do
    bf="$registry/oci/blobs/sha256/$bh"
    [ -f "$bf" ] || {
      echo "td-place: referenced blob $bh missing from the registry — digest mismatch; refusing to place" >&2; exit 1; }
    ah=$(sha256sum "$bf"); ah=${ah%% *}
    [ "$ah" = "$bh" ] || {
      echo "td-place: blob $bh does not re-hash to its digest — digest mismatch; refusing to place" >&2; exit 1; }
  done

  # VERIFIED. Stage the ordered LAYERS (the manifest's "layers" array; the
  # config blob is verified above but not unpacked) — the sed/grep parse is
  # safe on this manifest because it was just signature- and hash-verified
  # (skopeo-produced: one top-level "layers" array, no annotations), and
  # layer_refs is textually a subset of the re-hash-verified all_refs, so a
  # parse confusion could only mis-order verified blobs, never admit
  # unverified bytes — into $work as
  # <hex>/layer.tar — decompressed when gzipped (skopeo writes tar+gzip; the
  # magic bytes decide, and gzip itself fails closed on a lying magic) — and
  # hand them to the SAME placement path legacy mode uses below.
  layer_refs=$(tr -d ' \n\t' < "$mf" \
    | sed -n 's/.*"layers":\[\([^][]*\)\].*/\1/p' \
    | grep -o '"digest":"sha256:[0-9a-f]\{64\}"' \
    | sed 's/^.*sha256://; s/"$//')
  [ -n "$layer_refs" ] || {
    echo "td-place: manifest for $digest lists no layers — not a usable image" >&2; exit 1; }
  manifest_layers=
  for bh in $layer_refs; do
    bf="$registry/oci/blobs/sha256/$bh"
    mkdir -p "$work/$bh"
    case "$(od -An -tx1 -N2 "$bf" | tr -d ' ')" in
      1f8b) gzip -dc "$bf" > "$work/$bh/layer.tar" ;;
      *)    cp "$bf" "$work/$bh/layer.tar" ;;
    esac
    manifest_layers="$manifest_layers $bh/layer.tar"
  done
  # The placed identity records the VERIFIED manifest digest (§2.7: identity
  # = digest of the distributed artifact in its canonical form).
  img_digest=$digest
else
  # LEGACY mode: a local docker-archive tarball.
  tar xzf "$img" -C "$work"
  [ -f "$work/manifest.json" ] || {
    echo "td-place: $img has no manifest.json — not an OCI image" >&2; exit 1; }

  # Ordered list of manifest-referenced layers ("<hex>/layer.tar", one per line).
  manifest_layers=$(tr -d '\n' < "$work/manifest.json" \
    | sed -n 's/.*"Layers":\[\([^][]*\)\].*/\1/p' \
    | tr ',' '\n' | sed -n 's/^[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p')
  [ -n "$manifest_layers" ] || {
    echo "td-place: manifest.json lists no Layers — malformed image" >&2; exit 1; }

  # The placed identity records the sha256 of the artifact actually unpacked
  # (§2.7 staged representation for the local-archive path).
  img_digest=$(sha256sum "$img")
  img_digest="sha256:${img_digest%% *}"
  img_hex=${img_digest#sha256:}
  case "$img_hex" in
    *[!0-9a-f]*|'')
      echo "td-place: could not compute the image digest for $img" >&2; exit 1 ;;
  esac
  [ "${#img_hex}" -eq 64 ] || {
    echo "td-place: computed image digest has ${#img_hex} hex chars, expected 64" >&2; exit 1; }
fi

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
  echo "td-place: no manifest-referenced layer carries /boot/bzImage in $src — not a bootc generation image" >&2
  exit 1
}
# The userspace layers are the manifest layers OTHER than the boot layer, in order.
userspace_layers=
for lt in $manifest_layers; do
  [ "$lt" = "$boot_layer" ] && continue
  userspace_layers="$userspace_layers $lt"
done

# --- 2. Verify the image's embedded identity before placing anything. ----------
# The bootc image carries boot/td-identity (generation + root-label + system +
# root-uuid). Placing a gen-2 image under --generation 1 (or a foreign root label)
# would produce a menu entry that LIES about what it boots; bind the args to the
# image here. The system/root-uuid fields are what the menu and --mkfs consume.
tar xf "$work/$boot_layer" -C "$work" --strip-components=1 boot/td-identity 2>/dev/null || {
  echo "td-place: bootc image carries no boot/td-identity — cannot verify it is the gen $gen / $root_label image" >&2
  exit 1
}
img_gen=$(sed -n 's/^generation=//p' "$work/td-identity")
img_label=$(sed -n 's/^root-label=//p' "$work/td-identity")
img_system=$(sed -n 's/^system=//p' "$work/td-identity")
img_uuid=$(sed -n 's/^root-uuid=//p' "$work/td-identity")
[ "$img_gen" = "$gen" ] || {
  echo "td-place: image identity generation=$img_gen does not match --generation $gen" >&2; exit 1; }
[ "$img_label" = "$root_label" ] || {
  echo "td-place: image identity root-label=$img_label does not match --root-label $root_label" >&2; exit 1; }
case "$img_system" in
  /gnu/store/*) : ;;
  *) echo "td-place: image identity carries no usable system= store path ($img_system) — cannot write a bootable menu entry" >&2; exit 1 ;;
esac
[ -n "$img_uuid" ] || {
  echo "td-place: image identity carries no root-uuid= — cannot create the generation's filesystem deterministically" >&2; exit 1; }
# M12 (DESIGN §2.7): an image CANNOT carry its own digest (self-reference) —
# an embedded image-digest= line is necessarily forged and would shadow the
# line the placer appends below; fail closed.
if grep -q '^image-digest=' "$work/td-identity"; then
  echo "td-place: image's embedded identity already carries image-digest= — an image cannot state its own digest (§2.7); refusing to place" >&2
  exit 1
fi

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
# Record this generation's root label, system path and root uuid so the menu
# block can be regenerated (and the filesystem re-created) purely from on-disk
# state — each placed generation is self-describing.
printf '%s\n' "$root_label" > "$boot_stage/root-label"
printf '%s\n' "$img_system" > "$boot_stage/system"
printf '%s\n' "$img_uuid"   > "$boot_stage/root-uuid"
printf '%s\n' "$extra_kernel_args" > "$boot_stage/kernel-args"

# M12 (DESIGN §2.7): the PLACED identity additionally records what the image
# IS — $img_digest, set in step 1 per input mode: the VERIFIED manifest digest
# (registry mode) or the sha256 of the artifact actually unpacked (legacy).
# The image cannot carry its own digest (self-reference), so the placer
# appends it here.
chmod u+w "$boot_stage/td-identity"          # tar kept the image's 0444
printf 'image-digest=%s\n' "$img_digest" >> "$boot_stage/td-identity"
chmod 0444 "$boot_stage/td-identity"

# --- 3b. APPLY the userspace layers into this generation's own root. ------------
# The result is the per-generation root CONTENT, staged as root.tar — so
# root=td-root-gen-N (the bare-label spec Guix's initrd parses) refers to a root that actually exists (--mkfs below
# writes it onto a labeled filesystem). Single userspace layer (td's case) is
# copied byte-for-byte (the applied rootfs == that layer); multiple layers are
# applied in order with OCI whiteouts, then re-tarred deterministically.
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

# The menu entry written below points gnu.system/gnu.load at the identity's
# system path INSIDE this root — verify it is actually there, or the entry would
# select a root that exists but cannot boot.
if ! tar tf "$root_stage/root.tar" | grep -Eq "^(\./)?${img_system#/}/boot$"; then
  echo "td-place: identity system $img_system (/boot) is NOT in the applied userspace root — the menu entry would not boot; refusing to place gen $gen" >&2
  exit 1
fi

# --- 3c. With --mkfs: turn the staged root content into a live, labeled ext4. ---
# The exact invocation Guix's own image builder uses (gnu/build/image.scm
# make-ext-image) — the proven-reproducible path: content-addressed UUID from the
# identity, fixed root ownership, lazy-init off the table for determinism. Sized
# like Guix's estimate-partition-size: content + 25% + 1 MiB floor.
if [ "$mkfs" = yes ]; then
  command -v mke2fs >/dev/null 2>&1 || {
    echo "td-place: --mkfs requires mke2fs on PATH" >&2; exit 1; }
  command -v veritysetup >/dev/null 2>&1 || {
    echo "td-place: --mkfs requires veritysetup on PATH (dm-verity hash tree, M11)" >&2; exit 1; }
  fsdir="$root_stage/fsroot"
  mkdir -p "$fsdir"
  tar xf "$root_stage/root.tar" -C "$fsdir"
  # M11: the image's filesystem root is the applied root's /gnu/store SUBTREE
  # — at boot "/" is a tmpfs assembled by activation (DESIGN §2.6 "the root
  # is assembled, not stored"); the generation image provides exactly the
  # store (which carries the system closure the menu boots), mounted
  # read-only through dm-verity. The OCI artifact (root.tar — what M12
  # signs) still carries the full applied root; only the LIVE image narrows.
  [ -d "$fsdir/gnu/store" ] || {
    echo "td-place: applied root carries no /gnu/store — cannot build the generation's store image" >&2
    exit 1; }
  fsroot="$fsdir/gnu/store"
  # Determinism: the tar gives every entry a fixed mtime, but the imaged top
  # dir's own mtime is fresh — pin it; and mke2fs itself stamps the superblock/journal/
  # root inode with the current time and a RANDOM hash seed unless told
  # otherwise (found by `guix build --check` going red on the placed tree).
  # SOURCE_DATE_EPOCH + E2FSPROGS_FAKE_TIME pin the clock; hash_seed pins the
  # directory-hash seed to the (already deterministic) filesystem UUID.
  touch -d @1 "$fsroot"
  # Settle writeback before sizing: du reports st_blocks, and under ext4
  # delayed allocation a just-extracted tree reports 0/partial blocks until
  # writeback completes — a timing-dependent under-count that turned the
  # hosted CI's cross-host --check red intermittently (the runner rebuilds
  # against dev-built outputs; placed tree live run #5, rollback disk run
  # #3) while the settled-state builds observed so far agree across
  # filesystems (run #4 matched bit-for-bit). syncfs the tree's filesystem
  # (global sync where -f is unsupported) so du always measures the
  # settled state.
  sync -f "$fsroot" 2>/dev/null || sync
  size_kb=$(du -sk "$fsroot" | cut -f1)
  size_kb=$((size_kb + size_kb / 4 + 1024))
  # M11: the ext4 data area must be a whole number of 4096-byte dm-verity
  # data blocks — round up so --data-blocks below is exact and the appended
  # hash area starts on a data-block boundary.
  size_kb=$(( (size_kb + 3) / 4 * 4 ))
  SOURCE_DATE_EPOCH=1 E2FSPROGS_FAKE_TIME=1 \
  mke2fs -t ext4 -d "$fsroot" \
         -L "$root_label" -U "$img_uuid" \
         -E "root_owner=0:0,lazy_itable_init=1,lazy_journal_init=1,hash_seed=$img_uuid" \
         "$root_stage/root.img" "${size_kb}k"
  rm -rf "$fsdir"
  [ -s "$root_stage/root.img" ] || {
    echo "td-place: mke2fs produced no root.img for gen $gen" >&2; exit 1; }

  # M11: append the dm-verity hash tree (ChromeOS style — data and hash share
  # root.img; the hash area starts at the end of the data area). FIXED salt +
  # the identity's deterministic UUID keep root.img bit-reproducible. The
  # resulting root hash cannot live inside the image (it covers the data the
  # file carries — self-reference, DESIGN §2.7), so it is RECORDED next to
  # this generation's boot files. (The menu does not carry it yet — the boot
  # switch is M11 S2; until then the records are placement metadata only.)
  data_bytes=$((size_kb * 1024))
  veritysetup format "$root_stage/root.img" "$root_stage/root.img" \
      --hash-offset="$data_bytes" --data-blocks=$((data_bytes / 4096)) \
      --data-block-size=4096 --hash-block-size=4096 \
      --salt="$VERITY_SALT" --uuid="$img_uuid" > "$work/verity-format.out"
  roothash=$(sed -n 's/^Root hash:[[:space:]]*//p' "$work/verity-format.out")
  case "$roothash" in
    *[!0-9a-f]*|'')
      echo "td-place: veritysetup format yielded no usable root hash for gen $gen" >&2
      exit 1 ;;
  esac
  # Gate: the recorded hash must verify the image as built — a placement
  # whose own integrity metadata does not check out is never placed.
  veritysetup verify "$root_stage/root.img" "$root_stage/root.img" "$roothash" \
      --hash-offset="$data_bytes" >/dev/null || {
    echo "td-place: dm-verity self-verification FAILED for gen $gen — refusing to place" >&2
    exit 1; }
  printf '%s\n' "$roothash"   > "$boot_stage/verity-roothash"
  printf '%s\n' "$data_bytes" > "$boot_stage/verity-hashoffset"
fi

# --- 3d. Atomic swap: only now replace the live generation. ---------------------
rm -rf "$boot_dir/td/gen-$gen"; mv "$boot_stage" "$boot_dir/td/gen-$gen"; boot_stage=
rm -rf "$root_store/td/gen-$gen"; mv "$root_stage" "$root_store/td/gen-$gen"; root_stage=

# Remember the boot partition label for menu regeneration (any placement may
# regenerate the block, with or without the flag).
if [ -n "$boot_label" ]; then
  printf '%s\n' "$boot_label" > "$boot_dir/td/boot-label"
fi

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
# then append a freshly generated block — newest generation first, newest is the
# default, and the manual-rollback hook gives td/default.cfg the last word.
mkdir -p "$(dirname "$grub_cfg")"
newest=$(printf '%s\n' $kept | sort -rn | head -n 1)
tmp=$(mktemp)
if [ -f "$grub_cfg" ]; then
  sed "\|^${BEGIN_MARK}\$|,\|^${END_MARK}\$|d" "$grub_cfg" > "$tmp"
fi
{
  echo "$BEGIN_MARK"
  if [ -s "$boot_dir/td/boot-label" ]; then
    echo "search --no-floppy --label $(cat "$boot_dir/td/boot-label") --set=root"
  fi
  echo "set default=td-gen-$newest"
  echo "if [ -s /td/default.cfg ]; then source /td/default.cfg; fi"
  for g in $(printf '%s\n' $kept | sort -rn); do
    gd="$boot_dir/td/gen-$g"
    label=$(cat "$gd/root-label")
    sys=$(cat "$gd/system")
    extra=$(cat "$gd/kernel-args" 2>/dev/null || true)
    # M11: a generation placed with --mkfs carries verity records — its entry
    # passes the root hash + hash offset instead of root= (the initrd opens
    # the labeled partition as /dev/mapper/td-root, verifies every read, and
    # mounts it at /gnu/store; "/" is a declared tmpfs, so no root= at all).
    # A generation without records (mkfs-less placement) keeps the legacy
    # bare-label root= line.
    if [ -s "$gd/verity-roothash" ] && [ -s "$gd/verity-hashoffset" ]; then
      rootargs="td.roothash=$(cat "$gd/verity-roothash") td.hashoffset=$(cat "$gd/verity-hashoffset")"
    else
      rootargs="root=$label"
    fi
    echo "menuentry \"td generation $g (root=$label)\" --id td-gen-$g {"
    if [ -n "$extra" ]; then
      echo "  linux /td/gen-$g/bzImage $rootargs gnu.system=$sys gnu.load=$sys/boot $extra"
    else
      echo "  linux /td/gen-$g/bzImage $rootargs gnu.system=$sys gnu.load=$sys/boot"
    fi
    echo "  initrd /td/gen-$g/initrd.cpio.gz"
    echo "}"
  done
  echo "$END_MARK"
} >> "$tmp"
mv "$tmp" "$grub_cfg"

echo "td-place: placed generation $gen (root=$root_label, system=$img_system); kept generations: $kept"
