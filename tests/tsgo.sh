# tests/tsgo.sh TGZ — extract the pinned td-tsgo tarball (the TypeScript 7 NATIVE
# compiler) into a td-owned cache dir and print the dir holding `lib/tsc` (run as
# `<dir>/lib/tsc` — a statically-linked Go binary, NO node). move-off-Guile §5: guix is
# only the FETCHER of the pinned fixed-output blob (`td-tsgo-tarball`, an origin —
# exactly like the crate `.crate` fetches); td UNPACKS + provisions it ITSELF, so there
# is NO guix `(build-system …)` package / copy-build-system for the seed. The whole npm
# tarball unpacks to `package/`; the native binary loads its lib.*.d.ts from its own
# lib/, so the front-end uses `<print>/lib/tsc` where <print> = "<cache>/<hash>/package".
#
# Memoized on the tarball's content-addressed store basename (which changes iff the pin —
# URL or sha256 — changes), so an unchanged pin extracts once and is reused instantly.
set -eu

tgz="${1:?usage: tsgo.sh TGZ}"
test -s "$tgz" || { echo "tsgo: no tarball at $tgz" >&2; exit 1; }
cache="${TD_TSGO_CACHE:-$(pwd)/.td-build-cache/tsgo}"
dir="$cache/$(basename "$tgz")"          # /gnu/store/<hash>-…tgz basename — content-addressed

if [ -x "$dir/package/lib/tsc" ]; then echo "$dir/package"; exit 0; fi
rm -rf "$dir"; mkdir -p "$dir"
tar xzf "$tgz" -C "$dir"
test -x "$dir/package/lib/tsc" || { echo "tsgo: extracted no executable lib/tsc from $tgz" >&2; exit 1; }
echo "$dir/package"
