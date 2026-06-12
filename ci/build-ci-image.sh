#!/bin/sh
# ci/build-ci-image.sh — build (and optionally push) the CI store image: an
# OCI artifact carrying the warm /gnu/store build closure of the full rung
# ladder, so a GitHub-HOSTED runner can run the unmodified ./check.sh.
#
# Why an image at all: check.sh runs the loop offline with substitutes
# disabled against a warm store (DESIGN §5: "warm store in, nothing fetched
# inside"). A fresh hosted runner has an empty store; warming it from
# substitute servers at PR time is slow and decays as the pin ages. So we
# warm ONCE, here, on a machine that just ran a green check, snapshot exactly
# the closure the ladder needs, and ship it to ghcr.io — the runner imports
# it and the loop runs as offline as it does on a dev box. The loop itself is
# never adapted (ci-gate track constraint): this fixes the HOST, not the loop.
#
# What the image contains (one manifest, gzipped tar layers):
#   layer 0   meta/PIN                 the channels.scm commit this was built from
#             meta/CHANNEL_OUT         store path of the pinned guix profile
#             meta/signing-key.pub     this host daemon's public signing key
#             meta/cache.tar.gz        ~/.cache/guix checkouts+authentication
#                                      (offline time-machine + channel auth)
#   layer 1+  chunk-NNN                guix archive --export stream, split;
#                                      concatenate in order and import
#
# The export stream is SIGNED by this host's daemon; the consumer authorizes
# meta/signing-key.pub before importing (ci/import-store.sh).
#
# Usage:
#   ci/build-ci-image.sh [WORKDIR]      build the OCI layout under WORKDIR
#                                       (default ./.ci-image-work; needs ~50G)
#   PUSH=1 ci/build-ci-image.sh [...]   also push to ghcr.io/timmydo-bot/td-ci
#                                       as :<pin> and :latest (needs a gh
#                                       login with write:packages)
#
# Rebuild + push whenever channels.scm bumps (the workflow pulls the tag
# matching the pin, so a bump PR stays red until the new image is pushed —
# see .github/BRANCH-PROTECTION.md "CI store image").
set -eu

cd "$(dirname "$0")/.."

# --- Same integrity guard as check.sh: the snapshot is only honest if this
# host's guix IS the pin (otherwise we would snapshot some other channel's
# closure and the runner's pin guard would reject it anyway).
pinned=$(sed -n 's/.*(commit *"\([0-9a-f]\{40\}\)").*/\1/p' channels.scm | head -n1)
hostcommit=$(guix describe -f recutils 2>/dev/null | sed -n 's/^commit: *//p' | head -n1)
test -n "$pinned" || { echo "FATAL: no parseable pin in channels.scm" >&2; exit 1; }
if [ "$hostcommit" != "$pinned" ]; then
  echo "FATAL: host guix ($hostcommit) != pinned channel ($pinned)" >&2
  exit 1
fi

work=${1:-.ci-image-work}
mkdir -p "$work"
work=$(cd "$work" && pwd)
oci="$work/oci"
rm -rf "$oci" "$work/stage"
mkdir -p "$oci/blobs/sha256" "$work/stage"

free_kb=$(df -Pk "$work" | awk 'NR==2 {print $4}')
if [ "$free_kb" -lt 68157440 ]; then
  echo "FATAL: $work has <65G free; staging peaks near ~41G of chunks + ~21G of compressed blobs" >&2
  exit 1
fi

echo ">> enumerate: every derivation the rung ladder realises"
# No pipeline: piping into sort would swallow the enumerator's exit status
# and defeat its KNOWN_RUNGS fail-loudly guard.
sh ci/lower-check-drvs.sh > "$work/check-drvs.raw"
sort -u "$work/check-drvs.raw" > "$work/check-drvs.txt"
echo "   $(wc -l < "$work/check-drvs.txt") top-level derivations"

echo ">> closure: drv graph + valid outputs (the warm-store build closure)"
xargs -a "$work/check-drvs.txt" guix gc --requisites \
  | sort -u > "$work/drv-closure.txt"
grep '\.drv$' "$work/drv-closure.txt" > "$work/all-drvs.txt"
DRVLIST="$work/all-drvs.txt" guix repl ci/drv-outputs.scm \
  > "$work/outputs.txt" 2>/dev/null
channel_out=$(guix repl -L . ci/channel-instance-drv.scm 2>/dev/null \
  | sed -n 's/^CHANNEL_OUT=//p')
test -n "$channel_out" || { echo "FATAL: no channel instance output" >&2; exit 1; }
# EXCLUDE the docker-image artifact family's OUTPUTS from the export (their
# drvs and input closures stay, so the runner builds them itself and the
# --check rungs compare runner-vs-runner). Why: the pinned guix's docker
# builder packs tars in READDIR ORDER (guix/docker.scm never passes #:tar to
# tar-base-options, so --sort=name is dropped) — filesystem-dependent bytes
# (btrfs dev box vs ext4 runner), an UPSTREAM defect td cannot patch without
# forking the builder (future work; human-signed accommodation 2026-06-12,
# see plan/ci-gate.md). Everything embedding such a tarball is excluded too:
# generation images (repack it), the registry (embeds gen-image layouts),
# placed trees + rollback disk (extract gen images).
grep -Ev -- '-(docker-image\.tar\.gz|td-generation-image-gen-[0-9]+|td-registry|td-placed-tree(-mkfs)?|td-rollback-disk)$' \
  "$work/outputs.txt" > "$work/outputs-kept.txt"
echo "   excluded $(($(wc -l < "$work/outputs.txt") - $(wc -l < "$work/outputs-kept.txt"))) fs-order-sensitive family outputs (runner rebuilds them)"
sort -u "$work/check-drvs.txt" "$work/outputs-kept.txt" > "$work/roots.txt"
printf '%s\n' "$channel_out" >> "$work/roots.txt"
echo "   $(wc -l < "$work/roots.txt") export roots (channel profile: $channel_out)"

echo ">> export: signed nar stream, split into 2GiB chunks"
mkdir -p "$work/stage/store"
# Through a fifo, not a pipe: `export | split` would mask an export failure
# (no pipefail in POSIX sh) and ship a truncated stream as a "good" image.
# `wait` on the writer recovers its status. xargs -x: if the root list ever
# outgrew one invocation, concatenated export streams would silently end at
# the first stream's end-marker on import — die loudly instead.
rm -f "$work/export.fifo"; mkfifo "$work/export.fifo"
xargs -x -a "$work/roots.txt" guix archive --export -r \
  > "$work/export.fifo" &
export_pid=$!
split -b 2G -d -a 3 "$work/export.fifo" "$work/stage/store/chunk-"
wait "$export_pid" || { echo "FATAL: guix archive --export failed" >&2; exit 1; }
rm -f "$work/export.fifo"
echo "   $(ls "$work/stage/store" | wc -l) chunks, $(du -sh "$work/stage/store" | cut -f1)"

echo ">> meta layer: pin, profile path, signing key, channel cache"
mkdir -p "$work/stage/meta"
printf '%s\n' "$pinned"      > "$work/stage/meta/PIN"
printf '%s\n' "$channel_out" > "$work/stage/meta/CHANNEL_OUT"
cp /etc/guix/signing-key.pub   "$work/stage/meta/signing-key.pub"
tar -C "$HOME/.cache/guix" -czf "$work/stage/meta/cache.tar.gz" \
  $(cd "$HOME/.cache/guix" && ls -d checkouts authentication 2>/dev/null)

# --- OCI layout assembly (no docker on a Guix box; the format is just
# content-addressed JSON + tar.gz blobs, so write it directly).
layer() {  # layer DIR NAME... -> emits "diffid digest size" on stdout
  dir=$1; shift
  tar -C "$dir" --sort=name --owner=0 --group=0 --numeric-owner \
      --mtime='@0' -cf "$work/layer.tar" "$@"
  diffid=$(sha256sum "$work/layer.tar" | cut -d' ' -f1)
  gzip -n -1 "$work/layer.tar"
  digest=$(sha256sum "$work/layer.tar.gz" | cut -d' ' -f1)
  size=$(stat -c %s "$work/layer.tar.gz")
  mv "$work/layer.tar.gz" "$oci/blobs/sha256/$digest"
  echo "$diffid $digest $size"
}

echo ">> assemble OCI layout"
layers="$work/layers.txt"; : > "$layers"
layer "$work/stage" meta >> "$layers"
for c in "$work/stage/store"/chunk-*; do
  layer "$work/stage/store" "$(basename "$c")" >> "$layers"
  rm -f "$c"   # reclaim as we go: chunk + its gzip never coexist twice
done

diff_ids=$(awk '{printf "%s\"sha256:%s\"", sep, $1; sep=","}' sep= "$layers")
printf '{"architecture":"amd64","os":"linux","config":{},"rootfs":{"type":"layers","diff_ids":[%s]}}' \
  "$diff_ids" > "$work/config.json"
cfg_digest=$(sha256sum "$work/config.json" | cut -d' ' -f1)
cfg_size=$(stat -c %s "$work/config.json")
mv "$work/config.json" "$oci/blobs/sha256/$cfg_digest"

layer_descs=$(awk '{printf "%s{\"mediaType\":\"application/vnd.oci.image.layer.v1.tar+gzip\",\"digest\":\"sha256:%s\",\"size\":%s}", sep, $2, $3; sep=","}' sep= "$layers")
printf '{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:%s","size":%s},"layers":[%s]}' \
  "$cfg_digest" "$cfg_size" "$layer_descs" > "$work/manifest.json"
man_digest=$(sha256sum "$work/manifest.json" | cut -d' ' -f1)
man_size=$(stat -c %s "$work/manifest.json")
mv "$work/manifest.json" "$oci/blobs/sha256/$man_digest"

printf '{"imageLayoutVersion":"1.0.0"}' > "$oci/oci-layout"
printf '{"schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:%s","size":%s,"annotations":{"org.opencontainers.image.ref.name":"%s"}}]}' \
  "$man_digest" "$man_size" "$pinned" > "$oci/index.json"
rm -rf "$work/stage"
echo "   OCI layout: $oci ($(du -sh "$oci" | cut -f1))"

if [ "${PUSH:-0}" = "1" ]; then
  echo ">> push: ghcr.io/timmydo-bot/td-ci:{$pinned,latest}"
  skopeo=$(guix build skopeo)/bin/skopeo
  token=$(gh auth token)
  for tag in "$pinned" latest; do
    "$skopeo" copy --insecure-policy \
      --dest-creds "timmydo-bot:$token" \
      "oci:$oci:$pinned" "docker://ghcr.io/timmydo-bot/td-ci:$tag"
  done
  echo "   pushed; make the package PUBLIC once (GHCR UI or API) so the"
  echo "   workflow can pull it anonymously"
else
  echo "   not pushing (set PUSH=1 to push to ghcr.io/timmydo-bot/td-ci)"
fi
