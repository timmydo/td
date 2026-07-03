//! rust-ripgrep — td builds `ripgrep` (rg 14.1.1) with its WHOLE crate closure (source + 57
//! deps) provisioned GUIX-FREE through td's OWN cargo-proxy: cargo resolved + fetched it
//! through `td-feed cargo-proxy` (td-feed warm crate, host PREP), the proxy verifying
//! each `.crate` sha256 == the crates.io sparse-index cksum (the UPSTREAM pin, NOT a guix
//! artifact); source + deps are interned by td's OWN store-add-recursive and build-recipe
//! vendors from them (TD_VENDOR_DIR). No guix oracle (human 2026-06-23): content-address
//! (the Cargo.lock pin == the index cksum) is the oracle. The shared build+assert lives in
//! tests/crate-free-build.sh; this gate adds the package-specific behavioral leg. The rust/gcc
//! toolchain seed stays guix-built (retired last).
//! 
//! [DURABLE supply-chain] every vendored crate's sha256 ∈ ripgrep's shipped Cargo.lock.
//! [DURABLE structural] the .drv sets TD_VENDOR_DIR + references NO /gnu/store crate path.
//! [DURABLE behavioral] the td-built `rg` finds a pattern line in a tree (not an unrelated file).
//! [DURABLE repro] td-builder check double-build agrees the 57-crate build is reproducible.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "rust-ripgrep",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        script: r##"
echo ">> rust-ripgrep: td builds 'ripgrep' (rg 14.1.1, 57 deps) GUIX-FREE via the cargo-proxy (interned vendor tree, TD_VENDOR_DIR); rg greps a needle; reproducible; no guix build / no /gnu/store crate / no oracle"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; \
export GUIX="$TD_GUIX" ROOT="$PWD"; \
nsout=`sh tests/crate-free-build.sh ripgrep ripgrep-14.1.1 tests/ripgrep.lock ripgrep-source ripgrep` || exit 1; \
eval "$nsout"; ns="$NS"; \
test -x "$ns/bin/rg" || { echo "FAIL: no rg binary at $ns/bin/rg" >&2; exit 1; }; \
tree="$PWD/.td-build-cache/ripgrep-crate-free/tree"; rm -rf "$tree"; mkdir -p "$tree/sub"; printf 'alpha line\nthe needle is here\nbeta line\n' > "$tree/sub/hay.txt"; printf 'nothing to see\n' > "$tree/other.txt"; \
found=`"$ns/bin/rg" needle "$tree"`; \
echo "$found" | grep -q 'needle' || { echo "FAIL: td-built rg did not find the 'needle' line (got: $found)" >&2; exit 1; }; \
echo "$found" | grep -q 'other.txt' && { echo "FAIL: td-built rg matched the unrelated file (over-match)" >&2; exit 1; }; \
echo "  [DURABLE behavioral] the td-built 'rg' (guix-free crates) found the 'needle' line (and not the unrelated file) — it works as ripgrep"; \
rm -rf "$tree"; \
echo "PASS: rust-ripgrep — ripgrep (rg 14.1.1) built with its 57-crate closure provisioned GUIX-FREE via td's cargo-proxy (Cargo.lock-pinned, sha == crates.io cksum, no guix build / no /gnu/store FOD), source+vendor interned by store-add-recursive, built via TD_VENDOR_DIR with guix off PATH; rg greps a needle; reproducible. NO oracle (content-address = the upstream pin). Toolchain seed retired last."
"##,
    }
}
