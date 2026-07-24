# tests/cache-lib.sh — shared helpers for the build_gate prelude: place the
# td-bootstrapped stage0 td-builder and load td's Rust recipe evaluator. Sourced
# by tests/build-recipes.sh (and the package build gates) so every build uses the
# stage0 td-builder compiled from builder/ source — never a guix-built td-builder.
#
# Caller presets: CU (coreutils dir for the scrubbed PATH), CACHE (the gate's
# persistent cache root). `load_stage0` provides TB + the TD_BUILDER_* override
# (move-off-Guile §5 brick 3): cache-lib BUILDS with the td-bootstrapped stage0, NOT
# the guix-built td-builder.

# load_stage0 — ensure a td-bootstrapped stage0 td-builder is placed (`td-builder
# stage0-place`, builder/src/stage0.rs: cargo-compiled guix-free, stage0 places ITSELF via
# its own store-add-builder; no ambient host sh, re #469) and export
# it as the td-builder cache-lib uses to build AND check: TB (the stage0 binary) +
# TD_BUILDER_PATH/STORE/DB (the in-store builder-of-record the assembled drv names). So
# the package gates build with stage0, never `guix build -e '(@ (system td-builder)
# td-builder)'`. The placement is shared under .td-build-cache/stage0 (build-recipes
# populates it once before its fan-out; the gates reuse it). The rust+cc toolchain is
# resolved guix-free by `provision-{rust,cc}` (TD_RUST_HOME, or rustc/cargo + system cc
# on PATH, or rustup) — no lock, no guix pin.
load_stage0() {
  _s0base="${TD_STAGE0_BASE:-$(pwd)/.td-build-cache/stage0}"
  _tb_self="${TD_BUILDER_SELF:?load_stage0 requires TD_BUILDER_SELF (gate-run exports it)}"
  _cb=`"$_tb_self" stage0-place "$_s0base"` || { rc=$?; echo "FAIL: td-builder stage0-place could not place a stage0 td-builder" >&2; return $rc; }  # preserve 69 (no toolchain in the jail) so callers degrade to Unprovisioned, not RED (#469)
  TB="$_s0base/store/`basename "$_cb"`/bin/td-builder"
  TD_BUILDER_PATH="$_cb"
  TD_BUILDER_STORE="$_s0base/store"
  TD_BUILDER_DB="$_s0base/builder.db"
  export TB TD_BUILDER_PATH TD_BUILDER_STORE TD_BUILDER_DB
  test -x "$TB" || { echo "FAIL: stage0 td-builder not executable at $TB" >&2; return 1; }
}

# provision_stage0 — the HOST prelude for the loop-container provider (td-builder
# check + check-rung; workstream E #294). The stage0 td-builder compiles from
# builder/ source with the ENVIRONMENT's rust + cc (`td-builder provision-{rust,cc}`,
# builder/src/stage0.rs), so there is no guix-built toolchain seed to realize/fetch
# here — it is just load_stage0. provision-{rust,cc} resolve the toolchain from
# TD_RUST_HOME, rustc/cargo + system cc on PATH, or rustup — never a lock or a guix
# profile. Inside the host-tool-free loop sandbox none of those are reachable, so
# stage0-place exits 69 and the gate degrades to Unprovisioned/tolerated (#469).
provision_stage0() {
  load_stage0
}

# load_recipe_eval — export TD_RECIPE_EVAL = td's OWN recipe/spec evaluator (the
# dependency-free Rust `td-recipe` crate, recipes/), built ONCE by the build-recipes
# prelude (tests/recipe-eval-tool.sh) into .td-build-cache/recipe-eval, whose
# sentinel this reads. This REPLACES the boa td-recipe-eval on the build path
# (rust-recipe-surface): recipes/specs are declared in Rust now, so the loop emits
# their JSON with td-recipe-eval (the SAME JSON boa produced — proven canon-equal by
# recipe-rs) instead of transpiling+evaluating `.ts` through tsgo+boa.
load_recipe_eval() {
  _re="${TD_RECIPE_EVAL_BASE:-$(pwd)/.td-build-cache/recipe-eval}/recipe-eval-path"
  test -s "$_re" || { echo "FAIL: no td-recipe-eval sentinel ($_re) — the build-recipes prelude must run first" >&2; return 1; }
  TD_RECIPE_EVAL=`cat "$_re"`
  export TD_RECIPE_EVAL
  test -x "$TD_RECIPE_EVAL" || { echo "FAIL: td-recipe-eval not executable at $TD_RECIPE_EVAL" >&2; return 1; }
}

# cached_clean — drop this spec's transient files, KEEP $sd/b (the cache).
cached_clean() {
  rm -rf "$sd/chk" "$sd/tmp" "$sd/t" "$sd/t.c" "$sd/lk" "$sd/bout" "$sd/err" "$sd/chkerr" "$sd/recipe.json"
  mkdir -p "$sd/tmp"
}
