# tests/cache-lib.sh — the shared content-addressed build-recipe cache wrapper used
# by the package-manager build gates (recipe-checks, rust-build).
# `td-builder build-recipe` assembles a
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

# provision_stage0 — the HOST prelude for the loop-container provider (td-builder
# check + check-rung; workstream E #294): ensure the pinned stage0
# toolchain seed (tests/td-builder-rust.lock) is PRESENT — resolving a missing
# path through td's OWN signed substitute store (tools/resolve-seed.sh; #311 — no
# guix process, FAIL-CLOSED with no guix fallback; the common all-present path
# fetches nothing) — then place/load the stage0 via load_stage0. Guix-free
# consumers (the check-harness tier, the in-sandbox gates) call load_stage0
# directly: seed realization is the host caller's half (see the stage0-builder.sh
# header). resolve-seed fails closed on a lock that yields no seed paths —
# otherwise provision-rust.sh would silently fall through to its rustup/system-cc
# path on a warm host (a provenance switch, not a warm).
provision_stage0() {
  # The stage0 td-builder compiles from builder/ source with the ENVIRONMENT's rust +
  # cc (tools/provision-{rust,cc}.sh — TD_RUST_HOME / rust on PATH / rustup), so there is
  # no guix-built toolchain seed to realize here. load_stage0 does the whole job.
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

# cached_build SPEC LOCK  — emit the recipe, td-ASSEMBLE its .drv, and SUBMIT it to the ONE
# shared build daemon (the machine-wide budget limiter, started by the `td-builder check`
# prelude; the daemon caps concurrent builds across ALL agents, so N checks never
# oversubscribe/OOM).
# Sets sd, out, ns, hit. Returns non-zero (with a FAIL message) on a real failure.
cached_build() {
  _spec="$1"; _lock="$2"
  sd="$CACHE/$_spec"; mkdir -p "$sd/b" "$sd/tmp"
  : "${TD_RECIPE_EVAL:?load_recipe_eval must run before cached_build (TD_RECIPE_EVAL unset)}"
  : "${TD_DAEMON_SOCKET:?the shared build daemon must be running (TD_DAEMON_SOCKET unset) — the \`td-builder check\` host prelude starts it}"
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
  # (2) SUBMIT to the shared daemon, carrying the SEED STORE DIR (content-scanned for the
  # input closure — #267 retired the /var/guix/db read) + the per-request builder override
  # (BP BS BD) so the daemon stages THIS worktree's inputs + stage0 builder. Reply:
  # OK <canon> <host> <hit|built>.
  _resp=`"$TB" daemon-request "$TD_DAEMON_SOCKET" "$_drvf /gnu/store $TD_BUILDER_PATH $TD_BUILDER_STORE $TD_BUILDER_DB"` \
    || { echo "FAIL: $_spec daemon build failed ($_resp) — see the daemon log" >&2; return 1; }
  case "$_resp" in "OK "*) : ;; *) echo "FAIL: $_spec daemon build not OK: $_resp" >&2; return 1 ;; esac
  # shellcheck disable=SC2086 -- split the OK <canon> <host> <hit|built> reply into fields
  set -- $_resp
  out="$2"; ns="$3"
  test -n "$out" && [ -n "$ns" ] || { echo "FAIL: $_spec daemon reply malformed: $_resp" >&2; return 1; }
  if [ "${4:-}" = hit ]; then hit=1; else hit=; fi
}

# cached_check SPEC — prove reproducibility, memoized: skip the daemon repro rebuild only when
# a prior check verified THIS EXACT output (the sentinel records the verified output path, so
# a cross-worktree daemon-store HIT for a different drv cannot reuse a stale verdict — the
# shared store made a bare present/absent sentinel unsafe). Otherwise submit a CHECK to the
# SAME shared daemon (so the repro rebuild ALSO counts against the machine-wide budget instead
# of re-oversubscribing) and record the verified output. The CHECK verb reuses the build the
# daemon already realized as the first of the two independent builds and rebuilds only once
# more, so a check costs ONE build, not two. `out` is set by the preceding cached_build in the
# same shell.
cached_check() {
  _spec="$1"
  : "${TD_DAEMON_SOCKET:?the shared build daemon must be running (TD_DAEMON_SOCKET unset)}"
  if [ -n "$hit" ] && [ -n "${out:-}" ] && [ "`cat "$sd/b/verified-reproducible" 2>/dev/null`" = "$out" ]; then
    echo "  [DURABLE repro] CACHED: $_spec output $out previously verified reproducible — daemon repro rebuild skipped (verdict memoized)"
    return 0
  fi
  _drvf=`ls "$sd/b/"*.drv 2>/dev/null | head -1`
  test -n "$_drvf" || { echo "FAIL: no assembled .drv for $_spec" >&2; return 1; }
  _resp=`"$TB" daemon-request "$TD_DAEMON_SOCKET" "CHECK $_drvf /gnu/store $TD_BUILDER_PATH $TD_BUILDER_STORE $TD_BUILDER_DB"` \
    || { echo "FAIL: $_spec NOT reproducible (daemon CHECK: $_resp) — see the daemon log" >&2; return 1; }
  case "$_resp" in "OK "*) : ;; *) echo "FAIL: $_spec NOT reproducible: $_resp" >&2; return 1 ;; esac
  printf '%s\n' "${out:-verified}" > "$sd/b/verified-reproducible"
  echo "  [DURABLE repro] the td build daemon agrees $_spec is reproducible (a fresh rebuild is NAR-equal to the build it already realized)"
}

# cached_clean — drop this spec's transient files, KEEP $sd/b (the cache).
cached_clean() {
  rm -rf "$sd/chk" "$sd/tmp" "$sd/t" "$sd/t.c" "$sd/lk" "$sd/bout" "$sd/err" "$sd/chkerr" "$sd/recipe.json"
  mkdir -p "$sd/tmp"
}
