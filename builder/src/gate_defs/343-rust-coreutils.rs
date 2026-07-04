//! rust-coreutils — td builds uutils-coreutils (the Rust coreutils, crate `coreutils` 0.9.0,
//! the ONE multicall `coreutils` binary) with its WHOLE crate closure (source + 507 deps)
//! provisioned GUIX-FREE through td's OWN cargo-proxy: cargo resolved + fetched it through `td-
//! feed cargo-proxy` (td-feed warm crate coreutils 0.9.0 uutils, host PREP), the proxy
//! verifying each `.crate` sha256 == the crates.io index cksum; source + deps interned by
//! store-add-recursive, vendored via TD_VENDOR_DIR. No guix oracle: content-address
//! (Cargo.lock pin == index cksum) is the oracle. Shared build+assert in tests/crate-free-
//! build.sh. The rust/gcc toolchain seed stays guix-built (retired last).
//! 
//! [DURABLE supply-chain] every vendored crate's sha256 ∈ coreutils's shipped Cargo.lock.
//! [DURABLE structural] the .drv sets TD_VENDOR_DIR + references NO /gnu/store crate path.
//! [DURABLE behavioral] the ONE multicall `coreutils` binary dispatches mkdir/cp/cat/ls/mv/rm.
//! [DURABLE repro] td-builder check double-build agrees the 507-crate build is reproducible.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "rust-coreutils",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        // Typed artifact input (#353): the scrubbed-PATH coreutils the shared
        // crate-free-build.sh harness consumes — resolved by the runner from
        // this gate's lock.
        inputs: &[ArtifactInput {
            name: "coreutils",
            kind: InputKind::LockEntry { lock: "tests/uutils-coreutils.lock", stem: "coreutils" },
        }],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> rust-coreutils: td builds uutils-coreutils (coreutils 0.9.0, 507 deps) GUIX-FREE via the cargo-proxy (interned vendor tree, TD_VENDOR_DIR); the multicall binary dispatches util subcommands; reproducible; no guix build / no /gnu/store crate / no oracle"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; \
export GUIX="$TD_GUIX" ROOT="$PWD"; \
nsout=`sh tests/crate-free-build.sh uutils coreutils-0.9.0 tests/uutils-coreutils.lock uutils-source uutils` || exit 1; \
eval "$nsout"; ns="$NS"; \
bin="$ns/bin/coreutils"; \
test -x "$bin" || { echo "FAIL: no coreutils multicall binary at $bin" >&2; exit 1; }; \
w="$PWD/.td-build-cache/uutils-crate-free/work"; rm -rf "$w"; mkdir -p "$w"; \
"$bin" mkdir "$w/sub" || { echo "FAIL: multicall mkdir" >&2; exit 1; }; \
test -d "$w/sub" || { echo "FAIL: coreutils mkdir did not create the dir" >&2; exit 1; }; \
printf 'hello from td-built coreutils\nline two\n' > "$w/f.txt"; \
"$bin" cp "$w/f.txt" "$w/sub/g.txt" || { echo "FAIL: multicall cp" >&2; exit 1; }; \
got=`"$bin" cat "$w/sub/g.txt"`; \
test "$got" = "$(printf 'hello from td-built coreutils\nline two')" || { echo "FAIL: coreutils cat did not round-trip the copied file (got: $got)" >&2; exit 1; }; \
"$bin" ls "$w/sub" | grep -qx 'g.txt' || { echo "FAIL: coreutils ls did not list the copied file" >&2; exit 1; }; \
"$bin" mv "$w/sub/g.txt" "$w/sub/h.txt" || { echo "FAIL: multicall mv" >&2; exit 1; }; \
test -e "$w/sub/h.txt" -a ! -e "$w/sub/g.txt" || { echo "FAIL: coreutils mv did not move the file" >&2; exit 1; }; \
"$bin" rm "$w/sub/h.txt" || { echo "FAIL: multicall rm" >&2; exit 1; }; \
test ! -e "$w/sub/h.txt" || { echo "FAIL: coreutils rm did not remove the file" >&2; exit 1; }; \
rm -rf "$w"; \
echo "  [DURABLE behavioral] the ONE td-built coreutils multicall binary (guix-free crates) dispatches mkdir/cp/cat/ls/mv/rm — it works as coreutils"; \
echo "PASS: rust-coreutils — uutils-coreutils (coreutils 0.9.0) built with its 507-crate closure provisioned GUIX-FREE via td's cargo-proxy (Cargo.lock-pinned, sha == crates.io cksum, no guix build / no /gnu/store FOD), source+vendor interned by store-add-recursive, built via TD_VENDOR_DIR with guix off PATH; the multicall binary dispatches util subcommands; reproducible. NO oracle (content-address = the upstream pin). Toolchain seed retired last."
"##,
    }
}
