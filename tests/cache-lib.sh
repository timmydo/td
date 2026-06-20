# tests/cache-lib.sh — the shared content-addressed build-recipe cache wrapper used
# by the package-manager build gates (corpus-no-guix, toolchain-no-guix,
# corpus-deps-no-guix, rust-build). `td-builder build-recipe` assembles a
# DETERMINISTIC .drv, so a PERSISTENT cache (.td-build-cache/<gate>/, gitignored)
# lets td reuse a prior NAR-verified output for an UNCHANGED recipe (build-recipe
# prints CACHE=hit) and skip the build; on a verified hit the reproducibility
# double-build is skipped too (verdict memoized, like check-memo). Only a CHANGED
# recipe (⇒ different drv ⇒ miss) rebuilds. Reproducibility/behavior unweakened: the
# first build still double-builds, every run re-NAR-verifies the cached output, and
# the gate's own behavioral test (run/--version/link) re-runs each time.
#
# Caller presets: CU (coreutils dir for the scrubbed PATH), CACHE (the gate's
# persistent cache root). `load_stage0` provides TB + the TD_BUILDER_* override
# (move-off-Guile §5 brick 3): cache-lib BUILDS with the td-bootstrapped stage0, NOT
# the guix-built td-builder. cached_build/cached_check set/use the shell vars: sd (this
# spec's cache dir), out (store path), ns (newstore output dir), hit (non-empty on a hit).

# load_stage0 — ensure a td-bootstrapped stage0 td-builder is placed (tests/stage0-builder.sh:
# cargo-compiled guix-free, stage0 places ITSELF via its own store-add-builder) and export
# it as the td-builder cache-lib uses to build AND check: TB (the stage0 binary) +
# TD_BUILDER_PATH/STORE/DB (the in-store builder-of-record the assembled drv names). So
# the package gates build with stage0, never `guix build -e '(@ (system td-builder)
# td-builder)'`. The placement is shared under .td-build-cache/stage0 (build-recipes
# populates it once before its fan-out; the gates reuse it). The toolchain SEED stays the
# guix-built pin (§5, retired last) — realized by the caller, read by stage0-builder as
# strings.
load_stage0() {
  _s0base="${TD_STAGE0_BASE:-$(pwd)/.td-build-cache/stage0}"
  _cb=`sh tests/stage0-builder.sh "$_s0base"` || { echo "FAIL: stage0-builder could not place a stage0 td-builder" >&2; return 1; }
  TB="$_s0base/store/`basename "$_cb"`/bin/td-builder"
  TD_BUILDER_PATH="$_cb"
  TD_BUILDER_STORE="$_s0base/store"
  TD_BUILDER_DB="$_s0base/builder.db"
  export TB TD_BUILDER_PATH TD_BUILDER_STORE TD_BUILDER_DB
  test -x "$TB" || { echo "FAIL: stage0 td-builder not executable at $TB" >&2; return 1; }
}

# load_ts_eval — export TD_TS_EVAL = the td-BUILT td-ts-eval, so the gnu gates EVALUATE
# their recipes with td's OWN boa evaluator instead of `guix build (system td-ts)
# td-ts-eval` (move-off-Guile §5 brick 4b). td-ts-eval is built ONCE by the build-recipes
# prelude (tests/ts-eval-tool.sh, content-addressed cached) which writes the sentinel this
# reads. The td-built evaluator produces byte-identical JSON to guix's (rust-ts-eval
# oracle), so outputs are unchanged — only WHO evaluates changes. Only the prelude
# resolves the guix SEED (to build td-ts-eval); the gnu gates no longer touch it.
load_ts_eval() {
  _tse="${TD_TSEVAL_BASE:-$(pwd)/.td-build-cache/rust-ts-eval}/tseval-path"
  test -s "$_tse" || { echo "FAIL: no td-built td-ts-eval sentinel ($_tse) — the build-recipes ts-eval prelude must run first" >&2; return 1; }
  TD_TS_EVAL=`cat "$_tse"`
  export TD_TS_EVAL
  test -x "$TD_TS_EVAL" || { echo "FAIL: td-built td-ts-eval not executable at $TD_TS_EVAL" >&2; return 1; }
}

# cached_build SPEC LOCK  — emit the recipe, build via build-recipe with caching.
# Sets sd, out, ns, hit. Returns non-zero (with a FAIL message) on a real failure.
cached_build() {
  _spec="$1"; _lock="$2"
  sd="$CACHE/$_spec"; mkdir -p "$sd/b" "$sd/tmp"
  sh tests/ts-emit.sh "tests/ts/recipe-$_spec.ts" > "$sd/recipe.json" \
    || { echo "FAIL: ts-emit $_spec" >&2; return 1; }
  test -s "$sd/recipe.json" || { echo "ERROR: ts-emit produced no JSON for $_spec" >&2; return 1; }
  rm -f "$sd/b/"*.drv                       # drop any stale drv; build-recipe rewrites the current one
  # Build with the STAGE0 builder override (brick 3): TD_BUILDER_* (set by load_stage0)
  # ride through `env -i` so the assembled drv's builder is the td-bootstrapped stage0,
  # not the guix-built td-builder. `: "${TB:?...}"` fails loudly if load_stage0 was skipped.
  : "${TB:?load_stage0 must run before cached_build (TB unset)}"
  if env -i HOME="$sd" TMPDIR="$sd/tmp" PATH="$CU/bin" \
       TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
       "$TB" build-recipe \
       "$sd/recipe.json" "$_lock" "$sd/b" /var/guix/db/db.sqlite > "$sd/bout" 2>"$sd/err"; then :; \
  else echo "FAIL: build-recipe $_spec (guix/Guile off PATH):" >&2; tail -20 "$sd/err" >&2; return 1; fi
  out=`sed -n 's/^OUT=out //p' "$sd/bout"`
  test -n "$out" || { echo "FAIL: build-recipe produced no output for $_spec" >&2; cat "$sd/err" >&2; return 1; }
  ns="$sd/b/newstore/`basename "$out"`"
  if grep -qx 'CACHE=hit' "$sd/bout"; then hit=1; else hit=; fi
  # [DURABLE structural, brick 3] the assembled drv's builder is the td-bootstrapped
  # stage0, NOT the guix-built td-builder — proves cache-lib built with stage0. Holds on
  # a cache hit too (the drv hash covers the builder, so a hit IS a stage0-keyed drv).
  _drvf=`ls "$sd/b/"*.drv 2>/dev/null | head -1`
  test -n "$_drvf" || { echo "FAIL: no assembled .drv for $_spec" >&2; return 1; }
  # Non-vacuous: TD_BUILDER_PATH must be the stage0 placement load_stage0 set (a bare
  # grep for an empty value would match any td-builder path).
  test -n "$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder (build used the default builder)" >&2; return 1; }
  grep -qF "$TD_BUILDER_PATH/bin/td-builder" "$_drvf" \
    || { echo "FAIL: $_spec .drv builder is not the stage0 $TD_BUILDER_PATH — built by the wrong td-builder?" >&2; return 1; }
}

# cached_check SPEC — prove reproducibility, memoized: skip the td-builder check
# double-build when the build was a cache HIT and a prior check already verified this
# (unchanged) drv. Otherwise run the real double-build and record the verdict.
cached_check() {
  _spec="$1"
  if [ -n "$hit" ] && [ -f "$sd/b/verified-reproducible" ]; then
    echo "  [DURABLE repro] CACHED: $_spec drv unchanged + previously verified reproducible — td-builder check skipped (verdict memoized)"
    return 0
  fi
  rm -rf "$sd/chk"
  "$TB" check "$sd/b/"*.drv "$sd/b/closure.txt" "$sd/chk" >/dev/null 2>"$sd/chkerr" \
    || { echo "FAIL: $_spec NOT reproducible (td-builder check):" >&2; tail -6 "$sd/chkerr" >&2; return 1; }
  : > "$sd/b/verified-reproducible"
  echo "  [DURABLE repro] td-builder check double-build agrees $_spec is reproducible"
}

# cached_clean — drop this spec's transient files, KEEP $sd/b (the cache).
cached_clean() {
  rm -rf "$sd/chk" "$sd/tmp" "$sd/t" "$sd/t.c" "$sd/lk" "$sd/bout" "$sd/err" "$sd/chkerr" "$sd/recipe.json"
  mkdir -p "$sd/tmp"
}
