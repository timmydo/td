//! rust-userland-image (system-image-native + rust-userland). SHIP td's OWN build of the
//! Rust userland (procs/fd/ripgrep/sd/eza/bat) through td's OWN image builder. The shipped
//! system no longer carries the guix `(gnu packages rust-apps)` objects for these (removed
//! from the guix operating-system, whose gate tier is retired); this gate
//! proves td ships its OWN build of each tool in a td-NATIVE
//! OCI image (`td-builder oci-image`, NO `guix system image`, NO guix profile). For each
//! tool td: builds it guix-free (tests/crate-free-build.sh — the same path the rust-<tool>
//! gates assert), computes its runtime closure TD-NATIVELY (td-builder elf-interp/elf-rpath
//! + store-closure-scan over /gnu/store — no guix process), lays the td binary + its guix
//! toolchain closure into a rootfs, packs it, and crun-RUNS it from the image (no host store
//! bound) — td's OWN bytes execute. The toolchain LIBS (glibc/libgcc_s) stay guix (retired
//! last by the /td/store source-bootstrap); only the userland TOOLS are td's bytes here.
//! All-durable (behavioral + intrinsic-repro + structural + self-discrimination); NO guix
//! byte-identity oracle. crun/skopeo image gate → SYSTEM tier (like oci-native). It builds
//! each tool itself via crate-free-build (the warm crate vendor set is host-PREP, like the
//! rust-<tool> gates). fd/eza run a REAL job in the image (find / list); the rest assert
//! --version (the binary loads + runs self-contained — their function is the rust-<tool>
//! gates' job).

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "rust-userland-image",
        pools: &[Pool::System],
        needs: &[],
        // The script sources cache-lib and calls load_recipe_eval, so it NEEDS the
        // build-recipes prelude. The retired mk fragment never declared this edge
        // and only passed on worktrees whose cache held a stale recipe-eval
        // sentinel from an earlier full check — the runner's longest-first start
        // order exposed the gap deterministically on a cold worktree (verified
        // red: "no td-recipe-eval sentinel" before this flag; green after).
        build_gate: true,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> rust-userland-image: ship td-BUILT procs/fd/ripgrep/sd/eza/bat in td-NATIVE OCI images (td-builder oci-image, no guix system image); crun runs each from its image; reproducible"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
skopeo=`$TD_GUIX build skopeo`/bin/skopeo; \
crun=`$TD_GUIX build crun`/bin/crun; \
test -x "$skopeo" -a -x "$crun" || { echo "ERROR: could not resolve skopeo/crun" >&2; exit 1; }; \
export GUIX="$TD_GUIX" ROOT="$PWD"; \
ship() { \
  name=$1; cratedir=$2; lock=$3; skey=$4; recipe=$5; bin=$6; expect=$7; shift 7; \
  echo ">> [$bin] build guix-free (crate-free-build) + ship via td-native OCI image"; \
  nsout=`sh tests/crate-free-build.sh "$name" "$cratedir" "$lock" "$skey" "$recipe"` || return 1; \
  eval "$nsout"; \
  test -x "$NS/bin/$bin" || { echo "FAIL: no td-built $bin at $NS/bin/$bin" >&2; return 1; }; \
  TB="$tb" GUIX="$TD_GUIX" SKOPEO="$skopeo" CRUN="$crun" ROOT="$PWD" \
    sh tests/rust-userland-image.sh "$NS" "$OUT" "$bin" "$expect" -- "$@"; \
}; \
ship fd       fd-find-10.2.0  tests/fd.lock      fd-source      fd       fd       ld-linux  --no-ignore ld-linux /gnu/store; \
ship ripgrep  ripgrep-14.1.1  tests/ripgrep.lock ripgrep-source ripgrep  rg       14.1.1    --version; \
ship sd       sd-1.0.0        tests/sd.lock      sd-source      sd       sd       1.0.0     --version; \
ship procs    procs-0.14.10   tests/procs.lock   procs-source   procs    procs    0.14.10   --version; \
ship eza      eza-0.21.6      tests/eza.lock     eza-source     eza      eza      eza-0.21.6  /gnu/store; \
ship bat      bat-0.25.0      tests/bat.lock     bat-source     bat      bat      0.25.0    --version; \
echo "PASS: rust-userland-image — td ships its OWN build of all six Rust userland tools (procs/fd/ripgrep/sd/eza/bat) in td-NATIVE OCI images (td-builder oci-image, NO guix system image); each runs from its image (no host store bound), byte-reproducible, self-discriminating."
"##,
    }
}
