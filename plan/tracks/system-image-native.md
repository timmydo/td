section: side
status: claimed
title: system-image-native
handle: claude-fable-aca629
date: 2026-06-28
notes: plan/system-image-native.md
summary: move-off-Guile §5 / north-star priority 3 — begin retiring the LAST Guile build lowering: the system-image side (`guix system image -t docker` / `(gnu system image)`, used by the oci/generation-image/registry/place gates). The package build path is already Guile-free (build-recipe rail); this attacks the OCI/disk image construction. Brick 1: a td-native, zero-dep, DETERMINISTIC docker-archive packer in Rust (`builder/src/oci.rs` + `td-builder oci-image`), unit-tested on the check-engine smoke tier. Later bricks lay out a store closure into the rootfs layer, wire a gate (skopeo load + crun run + td-builder check repro), and keep `guix system image` as the REMOVABLE byte-identity oracle (directive 4). Toolchain/kernel BYTES stay guix (retired last — the /td/store tracks).
