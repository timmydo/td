# tests/tsgo.sh — resolve + extract the pinned tsgo tarball (the TypeScript 7 NATIVE
# compiler) and print the dir holding `lib/tsc` (run as `<dir>/lib/tsc` — a statically-
# linked Go binary, NO node). move-off-Guile §5 consumer-swap: the tarball is warmed by
# td's OWN fetcher (tools/warm-tsgo.sh — td-fetch fetch+verify, daemon-stored), so this
# reads its store path from the pin (tests/td-tsgo.lock) with NO
# `guix build -e '(@ (system td-ts) td-tsgo-tarball)'`. The whole npm tarball unpacks to
# `package/`; the native binary loads its lib.*.d.ts from its own lib/, so the front-end
# uses `<print>/lib/tsc` where <print> = "<cache>/<store-basename>/package".
#
# The pinned path must be WARM (host PREP populates it offline-safe — check.sh runs
# warm-tsgo.sh before the loop). Memoized on the tarball's content-addressed store
# basename (changes iff the pin changes), so an unchanged pin extracts once.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
lock="$root/tests/td-tsgo.lock"
tgz=$(sed -n 's/^path //p' "$lock" 2>/dev/null | head -1)
test -n "$tgz" || { echo "tsgo: no path in pin $lock" >&2; exit 1; }
test -s "$tgz" || { echo "tsgo: pinned tarball not warm at $tgz — run tools/warm-tsgo.sh (host PREP)" >&2; exit 1; }
cache="${TD_TSGO_CACHE:-$(pwd)/.td-build-cache/tsgo}"
dir="$cache/$(basename "$tgz")"          # /gnu/store/<hash>-…tgz basename — content-addressed

if [ -x "$dir/package/lib/tsc" ]; then echo "$dir/package"; exit 0; fi
rm -rf "$dir"; mkdir -p "$dir"
tar xzf "$tgz" -C "$dir"
test -x "$dir/package/lib/tsc" || { echo "tsgo: extracted no executable lib/tsc from $tgz" >&2; exit 1; }
echo "$dir/package"
