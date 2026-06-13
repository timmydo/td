#!/bin/sh
# ci/import-store.sh — import a CI store image (OCI layout produced by
# ci/build-ci-image.sh) into the local /gnu/store, so the unmodified
# ./check.sh can run its offline loop here.
#
#   ci/import-store.sh OCI_DIR
#
# Requires: a running guix-daemon, `jq`, and sudo rights (one call, to
# authorize the image's signing key in /etc/guix/acl). Prints the pinned
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

echo ">> verify: the pinned guix profile closure is valid" >&2
guix gc --requisites "$channel_out" > /dev/null
echo ">> import complete" >&2
echo "$channel_out"
