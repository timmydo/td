//! rust-fd — td builds `fd` (the fast find, fd-find 10.2.0) with its WHOLE crate closure
//! (source + 113 deps) provisioned GUIX-FREE through td's OWN cargo-proxy: cargo resolved +
//! fetched it through `td-feed cargo-proxy` (td-feed warm crate fd-find 10.2.0 fd, host
//! PREP — the crates.io name `fd-find` differs from the recipe name `fd`), the proxy verifying
//! each `.crate` sha256 == the crates.io index cksum (UPSTREAM pin, NOT a guix artifact);
//! source + deps interned by store-add-recursive, vendored via TD_VENDOR_DIR. No guix
//! oracle: content-address (Cargo.lock pin == index cksum) is the oracle. Shared build+assert
//! in tests/crate-free-build.sh. The rust/gcc toolchain seed stays guix-built (retired last).
//! 
//! [DURABLE supply-chain] every vendored crate's sha256 ∈ fd's shipped Cargo.lock.
//! [DURABLE structural] the .drv sets TD_VENDOR_DIR + references NO /gnu/store crate path.
//! [DURABLE behavioral] the td-built `fd` recursively FINDS a file in a tree (and only it).
//! [DURABLE repro] td-builder check double-build agrees the 113-crate build is reproducible.

use crate::gates::{ArtifactInput, GateDef, InputKind, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "rust-fd",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        // Typed artifact input (#353): the scrubbed-PATH coreutils the shared
        // crate-free-build.sh harness consumes — resolved by the runner from
        // this gate's lock.
        inputs: &[ArtifactInput {
            name: "coreutils",
            kind: InputKind::LockEntry { lock: "tests/fd.lock", stem: "coreutils" },
        }],
        store: StoreMode::Shared,
        non_blocking: true,
        script: r##"
echo ">> rust-fd: td builds 'fd' (fd-find 10.2.0, 113 deps) GUIX-FREE via the cargo-proxy (interned vendor tree, TD_VENDOR_DIR); fd finds a file; reproducible; no guix build / no /gnu/store crate / no oracle"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; \
export GUIX="$TD_GUIX" ROOT="$PWD"; \
nsout=`sh tests/crate-free-build.sh fd fd-find-10.2.0 tests/fd.lock fd-source fd` || exit 1; \
eval "$nsout"; ns="$NS"; \
test -x "$ns/bin/fd" || { echo "FAIL: no fd binary at $ns/bin/fd" >&2; exit 1; }; \
tree="$PWD/.td-build-cache/fd-crate-free/tree"; rm -rf "$tree"; mkdir -p "$tree/sub"; : > "$tree/foo.txt"; : > "$tree/bar.log"; : > "$tree/sub/needle.txt"; \
found=`"$ns/bin/fd" needle "$tree"`; \
echo "$found" | grep -q 'needle.txt' || { echo "FAIL: td-built fd did not find sub/needle.txt (got: $found)" >&2; exit 1; }; \
echo "$found" | grep -q 'foo.txt' && { echo "FAIL: td-built fd matched an unrelated file (pattern leaked)" >&2; exit 1; }; \
echo "  [DURABLE behavioral] the td-built 'fd' (guix-free crates) recursively FOUND sub/needle.txt (and only it) — it works as fd"; \
rm -rf "$tree"; \
echo "PASS: rust-fd — fd (fd-find 10.2.0) built with its 113-crate closure provisioned GUIX-FREE via td's cargo-proxy (Cargo.lock-pinned, sha == crates.io cksum, no guix build / no /gnu/store FOD), source+vendor interned by store-add-recursive, built via TD_VENDOR_DIR with guix off PATH; fd finds a file; reproducible. NO oracle (content-address = the upstream pin). Toolchain seed retired last."
"##,
    }
}
