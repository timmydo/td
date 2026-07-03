//! corpus-no-guix — the reconstructed corpus builds with td's OWN tooling and NO
//! guix/Guile in the build path (DESIGN §7.1 move-off-Guile §5). Consolidates the
//! per-recipe build gates onto `td-builder build-recipe`. For the corpus recipe
//! (hello): td-recipe-eval lowers the Rust recipe -> JSON; `td-builder build-recipe`,
//! run with guix/Guile SCRUBBED FROM PATH, resolves every input from the pinned
//! tests/<n>-no-guix.lock (no specification->package), assembles the .drv itself (no
//! guix (derivation …)) and realizes it (no guix-daemon). The build-toolchain leaves
//! (make/sed/grep/…) and library deps live in their own gates (toolchain-no-guix,
//! corpus-deps-no-guix); the GNU "completeness" corpus (gzip/popt/libatomic-ops/
//! gettext-minimal/nano/which/gperf) was dropped — td is a minimal Rust-focused distro
//! that ships the Rust userland, not the GNU corpus (reduce-guix prune).
//! Per recipe: STRUCTURAL (built with guix/Guile off PATH — the path needs neither);
//! DURABLE behavioral (the artifact runs / ships its lib+header); DURABLE reproducibility
//! (`td-builder check` double-builds the .drv, no guix --check); DURABLE self-discrimination
//! (a perturbed <spec>-perturbed twin — a load-bearing field change — assembles a DISTINCT
//! .drv, so the build is recipe-driven, not vacuous). The removable guix-comparison oracle
//! (distinct store path from guix's build — "own, then diverge") is DROPPED: hello now
//! stands on its own here (td-assembled .drv + td-double-build repro), so per AGENTS.md
//! ("the byte-hash-vs-Guix leg is the removable oracle") the guix leg is retired. The
//! toolchain + locks are the guix-built SEED (§5, retired last).
//! Built up front by the parallel `build-recipes` phase (into the shared cache); this
//! gate then cache-hits + memo-skips and only asserts behavior/oracle.
//! Specs that carry the DURABLE self-discrimination leg below (perturbed recipe ->
//! distinct .drv). Only specs whose `<spec>-perturbed` twin perturbs a load-bearing
//! RECIPE FIELD (e.g. configureFlags) belong here: in the build-recipe path the SOURCE
//! is resolved from the pinned lock, not the recipe, so a twin that only flips a
//! SOURCE-HASH byte would be vacuous HERE. `hello-perturbed` perturbs configureFlags
//! (recipes/src/recipes/hello-perturbed.rs), so it assembles a DISTINCT .drv —
//! load-bearing here.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "corpus-no-guix",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &["hello"],
        store: StoreMode::Shared,
        script: r##"
echo ">> corpus-no-guix: hello builds via td-builder build-recipe (no guix/Guile in the path), runs, reproducible (td-builder check); self-discriminated by hello-perturbed"
set -euo pipefail; \
cu=`grep -- '-coreutils-' "$PWD/tests/hello-no-guix.lock" | sed 's/^[^ ]* //' | head -1`; \
test -n "$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
if ls "$cu/bin" | grep -qE '^(guix|guile)$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; CU="$cu"; CACHE="$PWD/.td-build-cache/pkg"; mkdir -p "$CACHE"; \
case "$TD_RECIPE_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_RECIPE_EVAL is not td's own build ($TD_RECIPE_EVAL)" >&2; exit 1 ;; esac; \
echo "  [DURABLE structural] recipes evaluate with td's OWN Rust td-recipe-eval ($TD_RECIPE_EVAL) — boa retired (rust-recipe-surface)"; \
for spec in $TD_GATE_SPECS; do \
  echo "================ $spec ================"; \
  lock="$PWD/tests/$spec-no-guix.lock"; \
  test -s "$lock" || { echo "ERROR: no lock $lock" >&2; exit 1; }; \
  grep ' /gnu/store/' "$lock" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null || { echo "ERROR: could not realize the seed for $spec (regenerate locks on a channel bump)" >&2; exit 1; }; \
  cached_build "$spec" "$lock" || exit 1; \
  if [ -n "$hit" ]; then echo "  [STRUCTURAL] CACHE HIT — drv unchanged, reused td's prior output (no rebuild): $out"; else echo "  [STRUCTURAL] built with guix/Guile off PATH: $out"; fi; \
  L="$ns/lib"; \
  case "$spec" in \
    hello) test "`LD_LIBRARY_PATH="$L" "$ns/bin/hello"`" = "Hello, world!" || { echo "FAIL: hello did not greet" >&2; exit 1; } ;; \
  esac; \
  echo "  [DURABLE behavioral] $spec runs/ships from td's own store output"; \
  cached_check "$spec" || exit 1; \
  case " hello " in *" $spec "*) selfdisc=1 ;; *) selfdisc= ;; esac; \
  if [ -n "$selfdisc" ]; then \
    rdrv=`grep -hoE '/gnu/store/[a-z0-9]+-'"$spec"'-[^ ]+\.drv' "$sd/err" "$sd/bout" 2>/dev/null | head -1`; \
    test -n "$rdrv" || { echo "FAIL: could not read the real $spec .drv store path (self-discrimination leg)" >&2; exit 1; }; \
    pdir="$sd/perturbed"; rm -rf "$pdir"; mkdir -p "$pdir/b" "$pdir/tmp"; \
    sh tests/recipe-emit.sh $spec-perturbed > "$pdir/recipe.json" || { echo "FAIL: ts-emit $spec-perturbed" >&2; exit 1; }; \
    : "${TB:?}"; \
    env -i HOME="$pdir" TMPDIR="$pdir/tmp" PATH="$CU/bin" \
      TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
      "$TB" build-recipe "$pdir/recipe.json" "$lock" "$pdir/b" /gnu/store > "$pdir/out" 2>&1 || true; \
    pdrv=`grep -hoE '/gnu/store/[a-z0-9]+-'"$spec"'-[^ ]+\.drv' "$pdir/out" 2>/dev/null | head -1`; \
    test -n "$pdrv" || { echo "FAIL: perturbed $spec recipe did not assemble a .drv (self-discrimination leg)" >&2; tail -5 "$pdir/out" >&2; exit 1; }; \
    test "$pdrv" != "$rdrv" || { echo "FAIL: perturbed $spec recipe assembled the SAME .drv ($rdrv) — the recipe's content is not load-bearing in the build (self-discrimination vacuous)" >&2; exit 1; }; \
    echo "  [DURABLE self-discrimination] perturbed $spec recipe -> distinct .drv (real $rdrv vs perturbed $pdrv); the recipe's content is load-bearing"; \
    rm -rf "$pdir"; \
  fi; \
  cached_clean; \
done; \
echo "PASS: the reconstructed corpus (hello) builds via td-builder build-recipe — every input resolved from a pinned lock (no specification->package), the .drv assembled by td (no guix (derivation …)) and realized (no guix-daemon), with guix/Guile SCRUBBED FROM PATH; the artifact runs (durable), is reproducible by td's own double-build (durable), and is self-discriminated by hello-perturbed's load-bearing configureFlags (durable). The removable guix-comparison oracle was dropped — hello stands on its own. The toolchain + locks are the guix-built seed (§5, retired last)."
"##,
    }
}
