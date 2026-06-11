#!/bin/sh
# scripts/cloud-guix-setup.sh — provision a Guix toolchain in a cloud/CI box so
# td's offline verification loop (./check.sh) can run there.
#
# WHY THIS EXISTS
#   ./check.sh is the only pass/fail command, but it assumes the host IS a Guix
#   System pinned to channels.scm: a `guix` binary at the pinned commit, a warm
#   /gnu/store, a running guix-daemon under /var/guix, and the pinned channel
#   checkout under ~/.cache/guix. A fresh cloud container (e.g. Claude Code on
#   the web) has none of that. This script builds those preconditions so the
#   loop is runnable. It does NOT weaken the loop: check.sh still runs offline
#   (--no-substitutes); this only PRE-POPULATES the store while the network is
#   up, the one window substitutes are allowed (a declared setup phase, not a
#   loop fetch).
#
# WHAT check.sh REQUIRES (and where each is satisfied below):
#   1. `guix` on PATH whose `guix describe` == the channels.scm commit   -> phase 3
#   2. a running guix-daemon with its socket under /var/guix             -> phase 2
#   3. /gnu/store populated with every rung's dependency closure         -> phase 4
#   4. the pinned channel checkout under ~/.cache/guix                   -> phase 3
#   5. a non-loopback interface (offline-isolation control)              -> assumed
#      (cloud boxes have eth0; the control fails loud if not)
#
# IDEMPOTENT BY DESIGN: every phase checks for its own completion and skips if
# already done, so re-running (each session start) is cheap once the container
# state is cached. Safe to run as the SessionStart hook's body OR as a one-time
# environment setup script baked into a custom image (the recommended home for
# the heavy phases — see scripts/cloud-guix-setup.README.md).
#
# Run as root (cloud containers are). Prints a phase log to stdout.
set -eu

PIN=$(sed -n 's/.*(commit *"\([0-9a-f]\{40\}\)").*/\1/p' \
        "$(dirname "$0")/../channels.scm" | head -n1)
[ -n "$PIN" ] || { echo "cloud-guix-setup: cannot read pinned commit from channels.scm" >&2; exit 1; }
SUBS="${TD_SUBSTITUTE_URLS:-https://bordeaux.guix.gnu.org}"
CURRENT="$HOME/.config/guix/current"

log() { echo "cloud-guix-setup: $*"; }

# --- phase 1: install the Guix package (daemon + tooling), if absent ----------
# Ubuntu/Debian ships `guix` (1.4.0): it lays down /usr/bin/guix(-daemon), the
# _guixbuild build group + _guixbuilder1..N users, and the service units. We do
# NOT rely on its version — phase 3 pulls the pinned commit on top.
if ! command -v guix-daemon >/dev/null 2>&1; then
  log "phase 1: installing guix via apt"
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq guix
else
  log "phase 1: guix already installed ($(guix --version | head -n1))"
fi

# --- phase 2: store dir + running daemon --------------------------------------
# systemd is usually inactive in these containers, so the package's gnu-store.mount
# / guix-daemon.service never fire. Create the store and start the daemon by hand.
[ -d /gnu/store ] || { log "phase 2: creating /gnu/store"; install -d -m 1775 /gnu/store; }
if ! pgrep -x guix-daemon >/dev/null 2>&1; then
  log "phase 2: starting guix-daemon"
  mkdir -p /var/log/guix
  # --discover=no: no LAN substitute discovery (we point at SUBS explicitly).
  setsid guix-daemon --build-users-group=_guixbuild --discover=no \
    >/var/log/guix/daemon.log 2>&1 &
  # wait for the socket rather than sleeping blindly
  i=0; while [ ! -S /var/guix/daemon-socket/socket ] && [ "$i" -lt 30 ]; do
    i=$((i+1)); sleep 1
  done
  [ -S /var/guix/daemon-socket/socket ] || {
    echo "cloud-guix-setup: daemon socket never appeared; see /var/log/guix/daemon.log" >&2
    tail -20 /var/log/guix/daemon.log >&2 || true; exit 1; }
else
  log "phase 2: guix-daemon already running"
fi

# --- phase 3: pin the host guix to channels.scm -------------------------------
# check.sh FATALs unless `guix describe` == PIN. `guix pull --commit=PIN` builds
# the channel instance at that commit (substitutable from SUBS) and points
# ~/.config/guix/current at it. We then put that profile FIRST on PATH so
# `command -v guix` / `guix describe` resolve to the pinned guix (not /usr/bin).
pinned_now() {
  [ -x "$CURRENT/bin/guix" ] || return 1
  [ "$("$CURRENT/bin/guix" describe -f recutils 2>/dev/null \
        | sed -n 's/^commit: *//p' | head -n1)" = "$PIN" ]
}
if pinned_now; then
  log "phase 3: host guix already pinned to $PIN"
else
  log "phase 3: guix pull --commit=$PIN (this is the slow step; substitutes from $SUBS)"
  guix pull --commit="$PIN" --substitute-urls="$SUBS"
  pinned_now || { echo "cloud-guix-setup: pull completed but guix describe != $PIN" >&2; exit 1; }
fi

# Persist PATH for the session. CLAUDE_ENV_FILE is set when run from a hook; when
# run standalone we also update the login profile so interactive shells inherit it.
ENVLINE="export PATH=\"$CURRENT/bin:\$PATH\"; export GUIX_PROFILE=\"$CURRENT\""
if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
  printf '%s\n' "$ENVLINE" >> "$CLAUDE_ENV_FILE"
fi
grep -qF "$CURRENT/bin" "$HOME/.profile" 2>/dev/null || \
  printf '%s\n' "$ENVLINE" >> "$HOME/.profile"
export PATH="$CURRENT/bin:$PATH"

# --- phase 4: warm the store so the OFFLINE loop can realise every rung --------
# check.sh runs --no-substitutes, so every derivation a rung builds must already
# be local. Pre-build the loop here, WITH substitutes and network, so the cached
# container carries a warm store. Gated behind TD_WARM=1 because it is the
# heavy, long phase (VM-image builds + marionette closures) — best done once in a
# custom image rather than every session. See the README for the trade-off.
if [ "${TD_WARM:-0}" = "1" ]; then
  log "phase 4: warming the store (TD_WARM=1) — building the loop with substitutes"
  # Same sandbox as check.sh, but substitutes ALLOWED (setup window only). The
  # repl rungs force #:use-substitutes? #f internally, so warming the closures
  # they need is done by building the loop's heavy targets directly here.
  ( cd "$(dirname "$0")/.." && TD_ALLOW_SUBSTITUTES=1 ./scripts/cloud-guix-warm.sh ) || {
    echo "cloud-guix-setup: warm phase failed (loop deps not fully cached)" >&2; exit 1; }
else
  log "phase 4: SKIPPED (set TD_WARM=1 to pre-build the offline loop's store)"
fi

log "done. Verify with:  guix describe   then   ./check.sh eval"
