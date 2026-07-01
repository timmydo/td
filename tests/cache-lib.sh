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

# cached_build SPEC LOCK  — emit the recipe, td-ASSEMBLE its .drv, and SUBMIT it to the ONE
# shared build daemon (the machine-wide budget limiter — tools/build-daemon-ensure.sh; the
# daemon caps concurrent builds across ALL agents, so N checks never oversubscribe/OOM).
# Sets sd, out, ns, hit. Returns non-zero (with a FAIL message) on a real failure.
cached_build() {
  _spec="$1"; _lock="$2"
  sd="$CACHE/$_spec"; mkdir -p "$sd/b" "$sd/tmp"
  : "${TD_RECIPE_EVAL:?load_recipe_eval must run before cached_build (TD_RECIPE_EVAL unset)}"
  : "${TD_DAEMON_SOCKET:?the shared build daemon must be running (TD_DAEMON_SOCKET unset) — check.sh starts it in its host prelude}"
  "$TD_RECIPE_EVAL" emit "$_spec" > "$sd/recipe.json" \
    || { echo "FAIL: td-recipe-eval emit $_spec" >&2; return 1; }
  test -s "$sd/recipe.json" || { echo "ERROR: td-recipe-eval produced no JSON for $_spec" >&2; return 1; }
  rm -f "$sd/b/"*.drv                       # drop any stale drv; assemble-recipe rewrites the current one
  : "${TB:?load_stage0 must run before cached_build (TB unset)}"
  # (1) td ASSEMBLES the .drv itself (no guix (derivation …), no Guile) with the STAGE0
  # builder override (brick 3): TD_BUILDER_* ride through `env -i` so the drv's builder is
  # the td-bootstrapped stage0, not the guix-built td-builder (asserted below). No realize
  # here — the daemon does that. The canonical /gnu/store .drv path lands on stderr ($sd/err),
  # which the corpus gate's self-discrimination leg greps.
  if env -i HOME="$sd" TMPDIR="$sd/tmp" PATH="$CU/bin" \
       TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
       "$TB" assemble-recipe \
       "$sd/recipe.json" "$_lock" "$sd/b" > "$sd/bout" 2>"$sd/err"; then :; \
  else echo "FAIL: assemble-recipe $_spec (guix/Guile off PATH):" >&2; tail -20 "$sd/err" >&2; return 1; fi
  _drvf=`sed -n 's/^DRV=//p' "$sd/bout"`
  test -n "$_drvf" && [ -f "$_drvf" ] || { echo "FAIL: assemble-recipe produced no .drv for $_spec" >&2; cat "$sd/bout" "$sd/err" >&2; return 1; }
  # [DURABLE structural, brick 3] the assembled drv's builder is the td-bootstrapped stage0,
  # NOT the guix-built td-builder. Non-vacuous: TD_BUILDER_PATH must be the stage0 placement.
  test -n "$TD_BUILDER_PATH" || { echo "FAIL: TD_BUILDER_PATH unset — load_stage0 did not place a stage0 builder" >&2; return 1; }
  grep -qF "$TD_BUILDER_PATH/bin/td-builder" "$_drvf" \
    || { echo "FAIL: $_spec .drv builder is not the stage0 $TD_BUILDER_PATH — built by the wrong td-builder?" >&2; return 1; }
  # (2) SUBMIT to the shared daemon, carrying the per-request builder override (BP BS BD) so
  # the daemon stages THIS worktree's stage0 as the drv's builder. Reply: OK <canon> <host> <hit|built>.
  _resp=`"$TB" daemon-request "$TD_DAEMON_SOCKET" "$_drvf $TD_BUILDER_PATH $TD_BUILDER_STORE $TD_BUILDER_DB"` \
    || { echo "FAIL: $_spec daemon build failed ($_resp) — see the daemon log" >&2; return 1; }
  case "$_resp" in "OK "*) : ;; *) echo "FAIL: $_spec daemon build not OK: $_resp" >&2; return 1 ;; esac
  # shellcheck disable=SC2086 -- split the OK <canon> <host> <hit|built> reply into fields
  set -- $_resp
  out="$2"; ns="$3"
  test -n "$out" && [ -n "$ns" ] || { echo "FAIL: $_spec daemon reply malformed: $_resp" >&2; return 1; }
  if [ "${4:-}" = hit ]; then hit=1; else hit=; fi
}

# cached_check SPEC — prove reproducibility, memoized: skip the daemon double-build when the
# build was a cache HIT and a prior check already verified this (unchanged) drv. Otherwise
# submit a CHECK to the SAME shared daemon (so the repro rebuilds ALSO count against the
# machine-wide budget instead of re-oversubscribing) and record the verdict.
cached_check() {
  _spec="$1"
  : "${TD_DAEMON_SOCKET:?the shared build daemon must be running (TD_DAEMON_SOCKET unset)}"
  if [ -n "$hit" ] && [ -f "$sd/b/verified-reproducible" ]; then
    echo "  [DURABLE repro] CACHED: $_spec drv unchanged + previously verified reproducible — daemon double-build skipped (verdict memoized)"
    return 0
  fi
  _drvf=`ls "$sd/b/"*.drv 2>/dev/null | head -1`
  test -n "$_drvf" || { echo "FAIL: no assembled .drv for $_spec" >&2; return 1; }
  _resp=`"$TB" daemon-request "$TD_DAEMON_SOCKET" "CHECK $_drvf $TD_BUILDER_PATH $TD_BUILDER_STORE $TD_BUILDER_DB"` \
    || { echo "FAIL: $_spec NOT reproducible (daemon CHECK: $_resp) — see the daemon log" >&2; return 1; }
  case "$_resp" in "OK "*) : ;; *) echo "FAIL: $_spec NOT reproducible: $_resp" >&2; return 1 ;; esac
  : > "$sd/b/verified-reproducible"
  echo "  [DURABLE repro] the td build daemon's double-build agrees $_spec is reproducible"
}

# cached_clean — drop this spec's transient files, KEEP $sd/b (the cache).
cached_clean() {
  rm -rf "$sd/chk" "$sd/tmp" "$sd/t" "$sd/t.c" "$sd/lk" "$sd/bout" "$sd/err" "$sd/chkerr" "$sd/recipe.json"
  mkdir -p "$sd/tmp"
}
