#!/bin/sh
# tests/build-recipes.sh — the build phase (DESIGN §7.1 move-off-Guile §5). Extracted
# VERBATIM from the Makefile's `build-recipes` recipe when the gate runner
# (`td-builder gate-run`, builder/src/gates.rs) replaced make as the loop scheduler.
#
# Separates "build everything" from "the checks". The guix-seeded corpus has
# retired, so this is now a corpus-free PRELUDE: it places the stage0 td-builder
# and builds td-recipe-eval (which the build_gate store primitives reuse),
# asserts TD_BUILD_SPECS is empty, and does no per-spec pre-build. Package build
# gates build their subjects in-gate on td's mes-rooted /td/store toolchain.
#
# Called by the runner with cwd = repo root and:
#   TD_BUILD_SPECS  the specs to pre-build. The runner SCOPES this to the selected
#                   gates' spec lists (gates.rs scope_build_recipes, per-PR budget
#                   2026-07-04); the full `check` passes the whole BUILD_SPECS pool.
#                   MAY BE EMPTY (a spec-less selection, e.g. a lone store-DB gate):
#                   the prelude below still runs — build gates fail-fast without it
#                   (cache-lib load_recipe_eval reads the sentinel this writes) —
#                   and only the per-spec pre-build is skipped.
set -euo pipefail

: "${TD_BUILD_SPECS?the gate runner passes TD_BUILD_SPECS (empty = prelude only)}"
nspecs=$(set -- $TD_BUILD_SPECS; echo $#)

echo ">> build-recipes: the build_gate PRELUDE — stage0 td-builder (env rust) + td-recipe-eval, GUIX-FREE"
# The guix-seeded corpus retired — every package now builds on td's mes-rooted
# /td/store toolchain via the store-native gates, not guix's gcc-toolchain.
# So build-recipes is a corpus-free prelude: it places the stage0
# td-builder (compiled from builder/ source with the ENVIRONMENT's rust — no guix seed) and
# builds td-recipe-eval, which the build_gate store primitives reuse. There is no per-spec
# corpus pre-build (TD_BUILD_SPECS is empty) and no `guix build`.
export CACHE="$PWD/.td-build-cache/pkg" TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; mkdir -p "$CACHE"
. tests/cache-lib.sh; load_stage0
echo ">> builds run on the td-bootstrapped stage0 td-builder ($TD_BUILDER_PATH) — compiled from source with the environment's rust, no guix-built td-builder"
# Preserve recipe-eval-tool's exit code: 69 (no toolchain in the jail) degrades
# this prelude to Unprovisioned/tolerated in gate-run rather than RED (#469).
sh tests/recipe-eval-tool.sh "$PWD/.td-build-cache/recipe-eval" >/dev/null \
  || { rc=$?; echo "ERROR: could not build td's Rust recipe evaluator (recipes/ crate)" >&2; exit $rc; }
load_recipe_eval
echo ">> recipes EVALUATE with td's OWN Rust td-recipe-eval ($TD_RECIPE_EVAL)"
test "$nspecs" -eq 0 || { echo "ERROR: build-recipes got specs ($TD_BUILD_SPECS) but the guix-seeded corpus retired — no spec-carrying gate should remain" >&2; exit 1; }
echo "PASS: build-recipes — guix-free prelude: stage0 td-builder placed (env rust) + td-recipe-eval built; the store primitives build their subjects in-gate."
