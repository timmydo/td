#!/bin/sh
# ci/import-store.sh — import a CI store image (OCI layout produced by
# ci/build-ci-image.sh) into the local /gnu/store, so the unmodified
# ./check.sh can run its offline loop here.
#
#   ci/import-store.sh OCI_DIR
#
# Requires: a running guix-daemon, `jq`, and sudo rights (one call, to
# authorize the image's signing key in /etc/guix/acl). The post-import
# closure-validity check uses td's own store reader, so it also needs a
# prebuilt td-builder ($TD_BUILDER / builder/target/release / a PATH
# td-builder) or a Rust toolchain to build the dependency-free builder crate
# offline (`cargo --frozen`) — never a `guix gc` process. Prints the pinned
# guix profile path (meta/CHANNEL_OUT) on the LAST line of stdout; put
# "<that>/bin" first on PATH before running ./check.sh, exactly as a dev
# box keeps the pinned system guix first.
#
# Layer contract (see ci/build-ci-image.sh): manifest layer 0 is meta/,
# layers 1..N each hold one bare chunk-NNN forming one signed
# `guix archive --export` stream when concatenated in order.
set -eu

oci=${1:?usage: ci/import-store.sh OCI_DIR}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

manifest="$oci/blobs/sha256/$(jq -r '.manifests[0].digest' "$oci/index.json" | cut -d: -f2)"
jq -r '.layers[].digest' "$manifest" | cut -d: -f2 > "$tmp/layers"

# Layer 0: meta.
meta_blob=$(head -n1 "$tmp/layers")
tar -xzf "$oci/blobs/sha256/$meta_blob" -C "$tmp"
pin=$(cat "$tmp/meta/PIN")
channel_out=$(cat "$tmp/meta/CHANNEL_OUT")
echo ">> image pin: $pin" >&2
echo ">> pinned guix profile: $channel_out" >&2

echo ">> authorize the image's signing key" >&2
if [ "$(id -u)" = 0 ]; then
  guix archive --authorize < "$tmp/meta/signing-key.pub"
else
  sudo guix archive --authorize < "$tmp/meta/signing-key.pub"
fi

echo ">> channel cache -> ~/.cache/guix (offline time-machine + channel auth)" >&2
# --no-same-owner: the archive carries the dev box's uid/gid; the cache must
# belong to whoever runs the check here (and unmapped ids EINVAL in a userns).
mkdir -p "$HOME/.cache/guix"
tar -xzf "$tmp/meta/cache.tar.gz" -C "$HOME/.cache/guix" --no-same-owner

echo ">> import store chunks (signed nar stream; streamed, no on-disk copy)" >&2
# Each layer tar holds exactly one store/chunk-NNN; emitting each chunk's
# bytes with `tar -xO` in manifest order reassembles the export stream, so
# nothing is extracted to disk (the runner's disk fits blobs + store, not a
# third copy).
tail -n +2 "$tmp/layers" | while read -r blob; do
  gzip -dc "$oci/blobs/sha256/$blob" | tar -xOf - --wildcards 'chunk-*'
done | guix archive --import >&2

echo ">> verify: the pinned guix profile closure is valid (td-builder store-closure-scan over /gnu/store — td's OWN content-scanner, NO guix gc process, NO /var/guix/db read)" >&2
# Validate the just-imported closure with td's OWN content-scanner instead of a
# `guix gc` process (CLAUDE.md directive 8 — the guix surface only shrinks; this
# retires the LAST `guix gc` site in the CI provisioning path, the one #249
# deferred). #249 left it a guix call because "no td-builder is available" on the
# fresh runner — but the builder crate is dependency-free (builder/Cargo.toml has
# no [dependencies]), so the runner's pre-installed Rust builds it OFFLINE in
# seconds (the same `cargo --frozen` the ci.yml `cargo-test` job relies on).
# Resolve a prebuilt td-builder if one exists ($TD_BUILDER / release binary /
# PATH), else build it once.
repo=$(cd "$(dirname "$0")/.." && pwd)
if [ -n "${TD_BUILDER:-}" ]; then tb=$TD_BUILDER
elif [ -x "$repo/builder/target/release/td-builder" ]; then tb=$repo/builder/target/release/td-builder
elif command -v td-builder >/dev/null 2>&1; then tb=td-builder
else
  echo ">> building td-builder (cargo build --frozen --release; dependency-free, offline)" >&2
  cargo build --frozen --release --manifest-path "$repo/builder/Cargo.toml" >&2
  tb=$repo/builder/target/release/td-builder
fi
# store-closure-scan CONTENT-SCANS the live /gnu/store from $channel_out (the daemon
# placed it during the `guix archive --import` above) — the daemon's scanForReferences,
# proven == `guix gc -R` by the `store-gc` gate (mk/gates/290) — with NO read of guix's
# private /var/guix/db. `guix archive --import` already registers in topological order
# and fails on any unsatisfied reference, so the closure is complete by here; this is
# the independent td-native re-walk. Guard the root explicitly (a MISSING root is the
# one incompleteness a content-scan cannot otherwise see — it would echo the root and
# exit 0); store-closure-scan then fails loudly if any reachable path's bytes are
# unreadable.
test -e "$channel_out" || { echo "FATAL: imported profile $channel_out is missing" >&2; exit 1; }
"$tb" store-closure-scan /gnu/store "$channel_out" > /dev/null
echo ">> import complete" >&2
echo "$channel_out"
