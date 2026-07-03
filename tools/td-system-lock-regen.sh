#!/bin/sh
# tools/td-system-lock-regen.sh — regenerate tests/td-system.lock, the pinned
# lowering of the FROZEN td system (system/td.scm td-system). Run HOST-SIDE
# (outside the loop sandbox — the capture needs the pinned guix) on a channel
# bump or a td-system/td-hardening re-baseline; the landing is exclusive, like
# DIGESTS.md.
#
# Two halves, then an ATOMIC swap (a failed regen can never truncate or
# half-write the previous good lock — everything lands in a temp file first):
#   1. CAPTURE (guix, demoted to regen time): tests/td-system-lock.scm lowers
#      td-system exactly as `guix system build` does, REALIZES it, and prints
#      the lock header + the pinned root/deriver + the input-sha256-* pins.
#   2. CLOSURE PIN (td): `td-builder store-closure-scan` computes the runtime
#      closure from the pinned root — the SAME scan the consuming gates run
#      (td_system_closure, tests/td-system-lib.sh) — and its count + sha256 are
#      appended, so a gate-time scan that diverges (cold store, grown candidate
#      set) reds against this pin instead of packing different bytes per host.
set -eu

lock="tests/td-system.lock"
test -f channels.scm -a -f system/td.scm || {
  echo "td-system-lock-regen: run from the repo root" >&2; exit 1; }

# Resolve a td-builder (AGENTS.md order): $TD_BUILDER, the release build, PATH,
# else build it once.
tb="${TD_BUILDER:-}"
test -n "$tb" -a -x "${tb:-/nonexistent}" || tb="builder/target/release/td-builder"
test -x "$tb" || tb="$(command -v td-builder || true)"
test -n "$tb" -a -x "${tb:-/nonexistent}" || {
  echo "td-system-lock-regen: no td-builder; building it (cargo build --release)" >&2
  cargo build --release --manifest-path builder/Cargo.toml >&2
  tb="builder/target/release/td-builder"
}

tmp=$(mktemp "$lock.XXXXXX")
cls=$(mktemp "$lock.closure.XXXXXX")
trap 'rm -f "$tmp" "$cls"' EXIT INT TERM

guix time-machine -C channels.scm -- repl -L . tests/td-system-lock.scm > "$tmp"

root=$("$tb" resolve "$tmp" td-system)
"$tb" store-closure-scan /gnu/store "$root" > "$cls"
n=$(grep -c . "$cls")
h=$(sha256sum < "$cls" | cut -d' ' -f1)
printf 'closure-count %s\nclosure-sha256 %s\n' "$n" "$h" >> "$tmp"

chmod 644 "$tmp"   # mktemp creates 0600; the lock is a world-readable tracked file
mv "$tmp" "$lock"
trap - EXIT INT TERM
rm -f "$cls"
echo "td-system-lock-regen: wrote $lock (root $root, $n closure paths, closure sha256 $h)"
