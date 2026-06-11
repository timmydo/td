#!/usr/bin/env bash
# rootless rung driver (see the Makefile's `rootless` rung for the contract).
#
# Outer phase (no args beyond the four paths): runs inside the check.sh
# sandbox. Snapshots the host store DB with sqlite's backup API (consistent
# under sqlite locking even while the host daemon writes — a plain cp races),
# then re-enters itself under `unshare -m -U -r` for the inner phase.
#
# Inner phase (--inner): builds a writable view of the store at the SAME path
# (/gnu/store — required for store-path equality), starts the pinned
# guix-daemon UNPRIVILEGED (no --build-users-group, so every chroot build gets
# CLONE_NEWUSER — the rootless user-namespace builder), and runs:
#   (1) validity guard — the oracle output must be valid in the snapshot.
#       bmCheck itself refuses invalid outputs ("build it normally before
#       using --check", nix/libstore/build.cc), so this cannot silently
#       false-green; the guard makes the precondition explicit and the
#       diagnostic actionable;
#   (2) isolation probe — a build whose output records /proc/self/uid_map;
#       an identity map means the build did NOT run in a user namespace;
#   (3) the differential — `guix build --check` of the target image drv: the
#       rootless daemon rebuilds it and compares the rebuild's NAR hash
#       against the oracle hash the ROOT daemon recorded when it built the
#       artifact (info.hash in the snapshot DB — bmCheck in
#       nix/libstore/build.cc; verified: tampering the on-disk staged copy
#       does NOT fool it, the anchor is the root daemon's recorded hash),
#       plus an explicit output-path string equality assert. That is the
#       prime-directive-4 differential with the root daemon as oracle.
#
# Store mechanics: every needed closure item (paths.txt, computed by the
# recipe via `guix gc -R`) is bind-mounted item-by-item into a staged
# directory which is then rbind-mounted OVER /gnu/store. Overlayfs cannot be
# used here: inside `guix shell -C` the profile's store items are individual
# bind mounts under /gnu/store, the nested userns marks them MNT_LOCKED, and
# overlay refuses such a lowerdir (EINVAL). Writes (the --check rebuild, the
# probe output) land in the scratch directory; the bound inputs stay
# write-protected by their real inode permissions (host-root-owned). /var/guix
# is covered with a tmpfs inside the namespace, so the host daemon is
# unreachable by construction — the inner client can only talk to the
# rootless daemon.
set -euo pipefail

if [ "${1-}" != "--inner" ]; then
  scratch=$1; img_drv=$2; img_out=$3; probe_drv=$4; probe_out=$5

  echo ">> rootless: snapshot the host store DB (sqlite backup API)"
  mkdir -p "$scratch/state/db" "$scratch/newstore" "$scratch/log" "$scratch/tmp"
  sqlite3 /var/guix/db/db.sqlite \
    ".timeout 30000" ".backup '$scratch/state/db/db.sqlite'"
  cp /var/guix/db/schema "$scratch/state/db/schema"

  echo ">> rootless: enter the nested user namespace"
  exec unshare -m -U -r "$BASH" "$0" --inner \
       "$scratch" "$img_drv" "$img_out" "$probe_drv" "$probe_out"
fi

shift
scratch=$1; img_drv=$2; img_out=$3; probe_drv=$4; probe_out=$5

mount --make-rprivate /

echo ">> rootless: bind the input closures into the staged store"
while IFS= read -r p; do
  t="$scratch/newstore/${p#/gnu/store/}"
  if [ -d "$p" ]; then mkdir -p "$t"; else : > "$t"; fi
  mount --bind "$p" "$t"
done < "$scratch/paths.txt"
mount --rbind "$scratch/newstore" /gnu/store
mount -t tmpfs tmpfs /var/guix   # the host daemon is now unreachable

export GUIX_STATE_DIRECTORY="$scratch/state"
export GUIX_LOG_DIRECTORY="$scratch/log"
export TMPDIR="$scratch/tmp"     # rebuild happens on disk, not the sandbox tmpfs

echo ">> rootless: start the UNPRIVILEGED guix-daemon (userns builds)"
guix-daemon --no-substitutes --no-offload --disable-deduplication \
  --listen="$scratch/daemon.sock" &
daemon_pid=$!
trap 'kill $daemon_pid 2>/dev/null; wait $daemon_pid 2>/dev/null || true' EXIT
for i in $(seq 1 100); do
  [ -S "$scratch/daemon.sock" ] && break
  sleep 0.1
done
[ -S "$scratch/daemon.sock" ] || {
  echo "FAIL: the rootless guix-daemon never created its socket" >&2; exit 1; }
export GUIX_DAEMON_SOCKET="unix://$scratch/daemon.sock"

echo ">> rootless: validity guard — the oracle artifact must be in the snapshot"
guix gc --references "$img_out" > /dev/null 2>&1 || {
  echo "FAIL: the root-daemon-built image output is NOT valid in the DB" >&2
  echo "      snapshot, so there is no recorded oracle hash to compare the" >&2
  echo "      rootless rebuild against. The recipe must oracle-build before" >&2
  echo "      the snapshot is taken." >&2
  exit 1
}
if guix gc --references "$probe_out" > /dev/null 2>&1; then
  echo "FAIL: the isolation probe's output is already VALID in the host" >&2
  echo "      store, so the rootless daemon would not build it and the" >&2
  echo "      isolation assertion would read another daemon's map. Run" >&2
  echo "      'guix gc -D $probe_out' on the host and re-run." >&2
  exit 1
fi

echo ">> rootless: isolation probe — the build must run in a user namespace"
probe_built=$(guix build --no-substitutes "$probe_drv")
echo "   uid_map seen by the rootless build sandbox:"
sed 's/^/     /' "$probe_built/uid_map"
[ -s "$probe_built/uid_map" ] || {
  echo "FAIL: the isolation probe recorded an empty uid_map" >&2; exit 1; }
# A FRESH per-build user namespace at this pin maps the build user as a single
# non-zero entry ("30001 30001 1"). Both failure shapes start with uid 0:
# the identity map "0 0 4294967295" (no userns at all — a root daemon's plain
# chroot) and an INHERITED outer map "0 <uid> 1" (e.g. --disable-chroot: the
# build just runs in the caller's namespace). Rejecting any first-entry uid 0
# catches both; a multi-line map is not the per-build shape either.
map_lines=$(wc -l < "$probe_built/uid_map")
read -r map_first _ < "$probe_built/uid_map"
if [ "$map_lines" -ne 1 ] || [ "$map_first" = "0" ]; then
  echo "FAIL: the rootless build's uid_map is not a fresh per-build user" >&2
  echo "      namespace mapping (expected a single non-zero build-user entry" >&2
  echo "      like '30001 30001 1'; an identity map means no user namespace," >&2
  echo "      a '0 <uid> 1' map means the build inherited the caller's" >&2
  echo "      namespace, e.g. a chroot-less build)." >&2
  exit 1
fi

echo ">> rootless: differential — rootless rebuild vs the root daemon's artifact"
if ! checked=$(guix build --no-substitutes --check --keep-failed "$img_drv" \
                 | tail -n 1); then
  base=$(basename "$img_out")
  echo "FAIL: the rootless rebuild DIFFERS from the root daemon's artifact." >&2
  echo "      oracle (root daemon):     $img_out" >&2
  echo "      rootless rebuild (kept):  $scratch/newstore/$base-check" >&2
  echo "      Diagnose OUTSIDE the offline loop (diffoscope is a cold Python" >&2
  echo "      closure the sandbox cannot build offline):" >&2
  echo "        guix shell diffoscope -- diffoscope \\" >&2
  echo "          $img_out $scratch/newstore/$base-check" >&2
  exit 1
fi
test "$checked" = "$img_out" || {
  echo "FAIL: store-path mismatch: rootless client reports '$checked'," >&2
  echo "      the root daemon built '$img_out'" >&2
  exit 1
}

echo "PASS: the rootless user-namespace builder reproduced the target image"
echo "      (NAR hash equal to the root daemon's recorded oracle hash) at the"
echo "      same store path ($img_out);"
echo "      its builds run in a user namespace (fresh non-zero uid_map)."
