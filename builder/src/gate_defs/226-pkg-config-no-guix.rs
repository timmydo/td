//! pkg-config-no-guix — td builds pkg-config with its OWN builder, guix/Guile off
//! PATH, and the tool RESOLVES a .pc file (issue #297). pkg-config had an authored
//! recipe but was the ONE authored-but-unproven package: its bundled glib 2.x fails
//! GCC 15's default -std=gnu23 (goption.c uses `bool`/`true`/`false` as identifiers,
//! now C23 keywords). Pinning CFLAGS=-std=gnu17 in the recipe (the same lever
//! bash/less use) clears the wall, so pkg-config now builds td-natively and this gate
//! proves the FEATURE — not "it built": it resolves a .pc file (--modversion / --cflags
//! / --libs) through the real entry point (tests/pkg-config-check.sh).
//!
//! Structure mirrors corpus-no-guix (the build-recipes prelude builds pkg-config into
//! the shared cache; this gate cache-hits + memo-skips and asserts behavior):
//!   • STRUCTURAL — built with guix/Guile SCRUBBED FROM PATH, every input resolved from
//!     the pinned tests/pkg-config-no-guix.lock (no specification->package), the .drv
//!     assembled by td (no guix (derivation …)) and realized (no guix-daemon);
//!   • DURABLE behavioral — the td-built pkg-config parses a foo.pc: --modversion=1.2.3,
//!     --cflags expands ${prefix} and keeps -DFOO_ENABLED, --libs emits -lfoo, and a
//!     missing module FAILS (self-discriminating resolver);
//!   • DURABLE reproducibility — td's own double-build (cached_check), no guix --check;
//!   • DURABLE self-discrimination — the pkg-config-perturbed twin flips
//!     --with→--without-internal-glib, a load-bearing configureFlags delta, so it
//!     assembles a DISTINCT .drv (the build is recipe-driven, not vacuous).
//! No guix byte-identity oracle (the removable leg). The toolchain + lock are the
//! guix-built seed (§5, retired last). Landing this removes pkg-config from
//! tests/guix-dependence.scm's not-yet-td-built exclusion — the census owned-recipe
//! count rises honestly (19→20).

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "pkg-config-no-guix",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &["pkg-config"],
        inputs: &[],
        store: StoreMode::Shared,
        // Realizes the guix-built seed ($TD_GUIX build) — fails on a pin-drifted host,
        // so tagged non-blocking like its seed-realizing siblings (220/222/359).
        non_blocking: true,
        script: r##"
echo ">> pkg-config-no-guix: pkg-config builds via td-builder build-recipe (no guix/Guile in the path), RESOLVES a .pc file, reproducible; self-discriminated by pkg-config-perturbed"
set -euo pipefail; \
spec="${TD_GATE_SPECS:?the runner exports this gate's declared specs}"; \
case "$spec" in *' '*) echo "FAIL: this gate's script handles exactly ONE spec, got '$spec' — extend the script before extending specs" >&2; exit 1 ;; esac; \
lock="$PWD/tests/$spec-no-guix.lock"; \
test -s "$lock" || { echo "ERROR: no lock $lock" >&2; exit 1; }; \
cu=`grep -- '-coreutils-' "$lock" | sed 's/^[^ ]* //' | head -1`; \
test -n "$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
if ls "$cu/bin" | grep -qE '^(guix|guile)$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; CU="$cu"; CACHE="$PWD/.td-build-cache/pkg"; mkdir -p "$CACHE"; \
case "$TD_RECIPE_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_RECIPE_EVAL is not td's own build ($TD_RECIPE_EVAL)" >&2; exit 1 ;; esac; \
grep ' /gnu/store/' "$lock" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null || { echo "ERROR: could not realize the seed for $spec (regenerate locks on a channel bump)" >&2; exit 1; }; \
cached_build "$spec" "$lock" || exit 1; \
if [ -n "$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — drv unchanged, reused td's prior output (no rebuild): $out"; else echo "  [STRUCTURAL] built with guix/Guile off PATH: $out"; fi; \
test -x "$ns/bin/pkg-config" || { echo "FAIL: no td-built pkg-config at $ns/bin/pkg-config" >&2; exit 1; }; \
LD_LIBRARY_PATH="$ns/lib" sh tests/pkg-config-check.sh "$ns/bin/pkg-config"; \
cached_check "$spec" || exit 1; \
rdrv=`grep -hoE '/gnu/store/[a-z0-9]+-'"$spec"'-[^ ]+\.drv' "$sd/err" "$sd/bout" 2>/dev/null | head -1`; \
test -n "$rdrv" || { echo "FAIL: could not read the real $spec .drv store path (self-discrimination leg)" >&2; exit 1; }; \
pdir="$sd/perturbed"; rm -rf "$pdir"; mkdir -p "$pdir/b" "$pdir/tmp"; \
sh tests/recipe-emit.sh $spec-perturbed > "$pdir/recipe.json" || { echo "FAIL: emit $spec-perturbed" >&2; exit 1; }; \
: "${TB:?}"; \
env -i HOME="$pdir" TMPDIR="$pdir/tmp" PATH="$CU/bin" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  "$TB" build-recipe "$pdir/recipe.json" "$lock" "$pdir/b" /gnu/store > "$pdir/out" 2>&1 || true; \
pdrv=`grep -hoE '/gnu/store/[a-z0-9]+-'"$spec"'-[^ ]+\.drv' "$pdir/out" 2>/dev/null | head -1`; \
test -n "$pdrv" || { echo "FAIL: perturbed $spec recipe did not assemble a .drv (self-discrimination leg)" >&2; tail -5 "$pdir/out" >&2; exit 1; }; \
test "$pdrv" != "$rdrv" || { echo "FAIL: perturbed $spec recipe assembled the SAME .drv ($rdrv) — configureFlags not load-bearing (self-discrimination vacuous)" >&2; exit 1; }; \
echo "  [DURABLE self-discrimination] perturbed $spec recipe -> distinct .drv (real $rdrv vs perturbed $pdrv); the recipe's configureFlags are load-bearing"; \
rm -rf "$pdir"; \
cached_clean; \
echo "PASS: pkg-config-no-guix — pkg-config builds via td-builder build-recipe (every input from a pinned lock, .drv assembled+realized by td, guix/Guile SCRUBBED FROM PATH), RESOLVES a .pc file (durable behavioral), is reproducible by td's own double-build (durable), and is self-discriminated by pkg-config-perturbed's load-bearing configureFlags (durable). CFLAGS=-std=gnu17 clears the bundled-glib C23 wall. Toolchain + lock are the guix-built seed (§5, retired last)."
"##,
    }
}
