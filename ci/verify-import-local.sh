#!/bin/sh
# ci/verify-import-local.sh — dev-box pre-flight for a freshly built CI store
# image, WITHOUT touching this host's store, daemon database, or
# /etc/guix/acl (the dev host's daemon is not td's to reconfigure).
#
#   ci/verify-import-local.sh OCI_DIR
#
# What it proves, in order:
#   1. verified-red: while the image's signing key is UNAUTHORIZED, the
#      import is rejected AT signature verification — i.e. signature
#      enforcement is in force (an unsigned or tampered stream cannot
#      import). Note this is NOT foreign-image rejection: import-store.sh
#      authorizes whatever key the image itself carries, so WHICH images are
#      trusted is decided by who can push to the GHCR package, not here;
#   2. the full 41G-class stream imports cleanly through ci/import-store.sh
#      (manifest order, chunk reassembly, nar grammar, per-item signatures,
#      meta + channel-cache handling);
#   3. meta/CHANNEL_OUT names a runnable guix whose `guix describe` reports
#      the channels.scm pin.
#
# What it deliberately does NOT prove: placement into a truly EMPTY store.
# On the dev box every image path already exists in /gnu/store (the image
# was exported from here), and inside a user namespace those host-root-owned
# trees cannot be deleted for rewrite, so the throwaway daemon's database is
# SEEDED with a copy of the host's: the daemon still verifies every
# signature and restores every nar, then skips final placement of
# already-valid paths. The genuinely pristine-placement case is exactly what
# the CI runner exercises on every run (empty /gnu/store by construction —
# see .github/workflows/ci.yml).
#
# How: a user namespace (fake root) where
#   - /gnu/store is an OVERLAY (lower = host store read-only, upper =
#     scratch) so the host guix/daemon binaries stay runnable while the
#     import's temporary writes land in the scratch upper layer;
#   - /var/guix is a bind holding the seeded database copy; /etc/guix is an
#     EMPTY bind: no key is authorized until ci/import-store.sh authorizes
#     the image's own.
# A throwaway guix-daemon (--disable-chroot; no builds happen, only imports)
# serves the import.
set -eu

command -v jq >/dev/null 2>&1 \
  || { echo "jq is required (e.g. run via: guix shell jq -- sh $0 OCI_DIR)" >&2; exit 1; }

oci=${1:?usage: ci/verify-import-local.sh OCI_DIR}
oci=$(cd "$oci" && pwd)
cd "$(dirname "$0")/.."
repo=$PWD

# Disk-backed scratch, NOT /tmp: the import restores multi-GiB nars through
# the overlay upper, which would balloon a tmpfs /tmp.
mkdir -p "$HOME/.cache"
work=$(mktemp -d "$HOME/.cache/td-verify-XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/upper" "$work/ovl-work" "$work/var/db" "$work/etc" "$work/home"

# Seed the throwaway daemon's database from the host's (see header). Copied,
# never shared: the host database is opened read-only here and only the copy
# is ever written.
cp /var/guix/db/db.sqlite "$work/var/db/db.sqlite"

hostguix_dir=$(dirname "$(readlink -f "$(command -v guix)")")

unshare -rm --propagation=private sh -eu -c '
  work=$1; oci=$2; repo=$3; hostbin=$4
  mount -t overlay overlay \
    -o "lowerdir=/gnu/store,upperdir=$work/upper,workdir=$work/ovl-work" \
    /gnu/store
  mount --bind "$work/var" /var/guix
  mount --bind "$work/etc" /etc/guix
  mkdir -p /var/guix/daemon-socket
  # No builds happen here (imports only), so no build-users group is needed;
  # different daemon versions spell that differently — try bare first, fall
  # back to an explicit group if the bare form dies. Redirect to a file, not
  # the inherited pipe: a backgrounded daemon holding the callers stdout
  # keeps pipe readers alive long after this script exits.
  "$hostbin/guix-daemon" --disable-chroot >"$work/daemon.log" 2>&1 &
  daemon=$!
  trap "kill $daemon 2>/dev/null || true" EXIT
  for i in $(seq 1 50); do
    [ -S /var/guix/daemon-socket/socket ] && break; sleep 0.2
  done
  if ! kill -0 "$daemon" 2>/dev/null; then
    "$hostbin/guix-daemon" --disable-chroot --build-users-group="$(id -gn)" \
      >>"$work/daemon.log" 2>&1 &
    daemon=$!
    trap "kill $daemon 2>/dev/null || true" EXIT
    for i in $(seq 1 50); do
      [ -S /var/guix/daemon-socket/socket ] && break; sleep 0.2
    done
  fi
  [ -S /var/guix/daemon-socket/socket ] \
    || { echo "FATAL: throwaway daemon never came up:" >&2
         tail -20 "$work/daemon.log" >&2; exit 1; }
  export HOME="$work/home" PATH="$hostbin:$PATH"

  echo ">> verified-red: import MUST be rejected while the key is unauthorized"
  manifest="$oci/blobs/sha256/$(jq -r ".manifests[0].digest" "$oci/index.json" | cut -d: -f2)"
  first_chunk=$(jq -r ".layers[1].digest" "$manifest" | cut -d: -f2)
  red_log=$(mktemp)
  if gzip -dc "$oci/blobs/sha256/$first_chunk" \
       | tar -xOf - --wildcards "chunk-*" \
       | guix archive --import >/dev/null 2>"$red_log"; then
    echo "FAIL: an UNAUTHORIZED import was accepted — signature checking is not in force (or is skipped for already-valid paths, which would be just as disqualifying)" >&2
    exit 1
  fi
  if ! grep -qi "unauthorized" "$red_log"; then
    echo "FAIL: the unauthorized import failed, but NOT at signature verification (unexpected error) — cannot credit the rejection:" >&2
    tail -5 "$red_log" >&2
    exit 1
  fi
  rm -f "$red_log"
  echo "   ok: unauthorized import rejected at signature verification"

  echo ">> real import via ci/import-store.sh (authorize + full stream)"
  # No pipeline here: a pipe to tail would mask an import-store.sh failure.
  "$repo/ci/import-store.sh" "$oci" > "$work/import.out"
  channel_out=$(tail -n1 "$work/import.out")

  echo ">> run the guix named by meta/CHANNEL_OUT"
  pin_described=$("$channel_out/bin/guix" describe -f recutils 2>/dev/null \
                    | sed -n "s/^commit: *//p" | head -n1)
  pin_image=$(sed -n "s/.*(commit *\"\([0-9a-f]\{40\}\)\").*/\1/p" "$repo/channels.scm" | head -n1)
  echo "   image pin:     $pin_image"
  echo "   described pin: $pin_described"
  test -n "$pin_described" && test "$pin_described" = "$pin_image" \
    || { echo "FAIL: imported guix does not describe the pinned commit" >&2; exit 1; }
  echo "PASS: image verified (unauthorized import rejected; full stream imports; CHANNEL_OUT guix reports the pin — the CI runner is the pristine-placement test)"
' verify "$work" "$oci" "$repo" "$hostguix_dir"
