//! oci-native (system-image-native track; move-off-Guile §5 / north-star priority 3).
//! The td-NATIVE replacement for `guix system image -t docker`: instead of lowering an
//! OCI image through guix's `(gnu system image)` Guile, `td-builder oci-image-closure`
//! computes a store path's closure (builder/src/store_db_read — NO guix process) and packs
//! it into a docker-archive itself (builder/src/oci.rs — a deterministic, zero-dep ustar +
//! manifest/config writer). The packed SUBJECT is td's OWN build: the corpus hello out of
//! the shared daemon cache (`td-builder build-recipe`, warmed by the build-recipes
//! prelude — NO `guix build hello`), so the image tier packs td bytes end to end,
//! matching gate 122 (issue #299). TD_STORE points oci-image-closure at the daemon cache
//! so the td-built tree packs at its canonical /gnu/store name next to the guix-seed
//! deps physically there. This gate proves that constructed image is REAL and WORKS,
//! entirely with DURABLE assertions (no guix byte-identity oracle — td's construction is
//! proven by behavior + its own reproducibility):
//! • skopeo (a foreign OCI implementation) `copy docker-archive:` loads it and yields a
//! sha256 manifest digest — the bytes are a valid OCI image;
//! • crun RUNS it rootless with NO host store bound (the image carries its own closure)
//! and the entrypoint emits "Hello, world!" — the td-built userspace executes;
//! • INTRINSIC reproducibility: packing the same closure twice is byte-identical (prime
//! directive 1, proven by td itself — not `guix build --check`);
//! • self-discrimination: a bogus exec in the same image fails (the green discriminates).
//! Scope: the IMAGE CONSTRUCTION and the packed PACKAGE are td's here. The TOOLCHAIN
//! bytes in the closure (glibc & co from the pinned lock) stay the guix seed (retired
//! last — the /td/store source-bootstrap), and skopeo/crun are guix-realized HARNESS
//! tools, not packed subjects.
//!
//! The gate uses its OWN cache root (.td-build-cache/oci-native), not the shared
//! .td-build-cache/pkg: corpus-no-guix runs concurrently (Heavy vs System pool) and
//! cached_build's per-spec dir is not safe for two gates to share at once (the
//! `rm -f $sd/b/*.drv` reset would yank the other's drv mid-daemon-request). The daemon
//! itself dedupes the build — the prelude-warmed hello is a HIT either way.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "oci-native",
        pools: &[Pool::System],
        needs: &[],
        // cached_build needs the build-recipes prelude: the stage0 placement, the
        // td-recipe-eval sentinel, and the warm daemon-cache hello it cache-hits.
        build_gate: true,
        specs: &["hello"],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> oci-native: td-builder builds a working OCI image from the TD-BUILT hello closure (no guix system image, no guix build hello); skopeo loads it, crun runs it, reproducible"
set -euo pipefail; \
cu=`grep -- '-coreutils-' "$PWD/tests/hello-no-guix.lock" | sed 's/^[^ ]* //' | head -1`; \
test -n "$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
CU="$cu"; CACHE="$PWD/.td-build-cache/oci-native"; mkdir -p "$CACHE"; \
lock="$PWD/tests/hello-no-guix.lock"; \
grep ' /gnu/store/' "$lock" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null \
  || { echo "ERROR: could not realize the toolchain seed for hello (regenerate locks on a channel bump)" >&2; exit 1; }; \
echo ">> the packed SUBJECT is td's OWN build: hello via td-builder build-recipe from the shared daemon cache (no guix build hello)"; \
cached_build hello "$lock" || exit 1; \
hello="$out"; hello_store=`dirname "$ns"`; \
test -n "$hello" -a -x "$ns/bin/hello" || { echo "ERROR: no td-built hello at $ns/bin/hello" >&2; exit 1; }; \
cached_clean; \
skopeo=`$TD_GUIX build skopeo`/bin/skopeo; \
crun=`$TD_GUIX build crun`/bin/crun; \
scratch="$PWD/.oci-native-scratch"; chmod -R u+w "$scratch" 2>/dev/null || true; rm -rf "$scratch"; mkdir -p "$scratch"; \
trap 'chmod -R u+w "$scratch" 2>/dev/null || true; rm -rf "$scratch"' EXIT; \
printf '{"repoTag":"td-hello:latest","env":["PATH=/bin"],"entrypoint":["%s/bin/hello"]}' "$hello" > "$scratch/config.json"; \
echo ">> td-builder oci-image-closure (td CONTENT-SCANS /gnu/store + the daemon cache via TD_STORE, packs the td-built hello's closure — no /var/guix/db, no guix system image)"; \
TD_STORE="$hello_store" "$tb" oci-image-closure /gnu/store "$scratch/config.json" "$scratch/img1.tar" "$hello" \
  || { echo "FAIL: td-builder oci-image-closure failed" >&2; exit 1; }; \
echo ">> INTRINSIC reproducibility: pack the same closure again, assert byte-identical"; \
TD_STORE="$hello_store" "$tb" oci-image-closure /gnu/store "$scratch/config.json" "$scratch/img2.tar" "$hello" \
  || { echo "FAIL: second oci-image-closure failed" >&2; exit 1; }; \
h1=`sha256sum < "$scratch/img1.tar" | cut -d" " -f1`; \
h2=`sha256sum < "$scratch/img2.tar" | cut -d" " -f1`; \
test -n "$h1" -a "$h1" = "$h2" \
  || { echo "FAIL: the td-native image is NOT reproducible (sha256 $h1 != $h2)" >&2; exit 1; }; \
echo "   ok: byte-identical across two packs — sha256 $h1 (td's own reproducibility, no guix oracle)"; \
echo ">> behavioral: skopeo loads it + crun runs the td-built hello"; \
SKOPEO="$skopeo" CRUN="$crun" sh tests/oci-native-check.sh "$scratch/img1.tar" "$hello/bin/hello" "Hello, world!"; \
rm -rf "$scratch"
"##,
    }
}
