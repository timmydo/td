#!/usr/bin/env bash
# tests/build-recipes.sh — the build phase (DESIGN §7.1 move-off-Guile §5). Extracted
# VERBATIM from the Makefile's `build-recipes` recipe when the gate runner
# (`td-builder gate-run`, builder/src/gates.rs) replaced make as the loop scheduler.
#
# Separates "build everything" from "the checks": td-ASSEMBLE + SUBMIT every package
# recipe (TD_BUILD_SPECS — the corpus, toolchain leaves and library deps, accumulated
# from the mk/gates/*.mk fragments' `BUILD_SPECS +=` lines) up front to the ONE shared
# build daemon, which realizes + reproducibility-checks them into the shared
# content-addressed store; the package build gates then cache-HIT + memo-skip the
# double-build and only assert behavior + migration-oracle. The daemon
# (started by the `td-builder check` host prelude) is the SINGLE machine-wide
# build limiter (TD_BUILD_JOBS). The xargs -P below is only SUBMIT
# parallelism — submits block on the daemon's budget.
#
# Called by the runner with cwd = repo root and:
#   TD_BUILD_SPECS  the specs to pre-build. The runner SCOPES this to the selected
#                   gates' spec lists (gates.rs scope_build_recipes, per-PR budget
#                   2026-07-04); the full `check` passes the whole BUILD_SPECS pool.
#                   MAY BE EMPTY (a spec-less selection, e.g. a lone store-DB gate):
#                   the prelude below still runs — build gates fail-fast without it
#                   (cache-lib load_recipe_eval reads the sentinel this writes) —
#                   and only the per-spec pre-build is skipped.
#   TD_GUIX         the pinned guix invocation prefix ("guix time-machine -C channels.scm --")
set -euo pipefail

: "${TD_BUILD_SPECS?the gate runner passes TD_BUILD_SPECS (empty = prelude only)}"
: "${TD_GUIX:?the gate runner passes TD_GUIX (the pinned time-machine prefix)}"
nspecs=$(set -- $TD_BUILD_SPECS; echo $#)

echo ">> build-recipes: assemble + submit $nspecs recipes to the shared build daemon (global budget), then reproducibility-check ($TD_BUILD_SPECS)"
: "${TD_DAEMON_SOCKET:?the shared build daemon is not running — the \`td-builder check\` host prelude starts it (ensure_build_daemon)}"
if [ "$nspecs" -gt 0 ]; then
  for s in $TD_BUILD_SPECS; do grep ' /gnu/store/' "tests/$s-no-guix.lock"; done \
    | sed 's/^[^ ]* //' | sort -u | xargs $TD_GUIX build >/dev/null \
    || { echo "ERROR: could not realize the build seed (regenerate locks on a channel bump)" >&2; exit 1; }
fi
grep ' /gnu/store/' tests/td-builder-rust.lock | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null \
  || { echo "ERROR: could not realize the stage0 toolchain seed (regenerate tests/td-builder-rust.lock on a channel bump)" >&2; exit 1; }
export CACHE="$PWD/.td-build-cache/pkg" TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; mkdir -p "$CACHE"
. tests/cache-lib.sh; load_stage0
echo ">> builds run on the td-bootstrapped stage0 td-builder ($TD_BUILDER_PATH) — NO guix-built td-builder (move-off-Guile §5 brick 3)"
TD_GUIX="$TD_GUIX" sh tests/recipe-eval-tool.sh "$PWD/.td-build-cache/recipe-eval" >/dev/null \
  || { echo "ERROR: could not build td's Rust recipe evaluator (recipes/ crate)" >&2; exit 1; }
load_recipe_eval
echo ">> recipes EVALUATE with td's OWN Rust td-recipe-eval ($TD_RECIPE_EVAL) — boa retired (rust-recipe-surface)"
if [ "$nspecs" -gt 0 ]; then
  echo ">> submitting $nspecs recipes to the shared build daemon ($TD_DAEMON_SOCKET); the daemon's global budget caps concurrency ..."
  printf '%s\n' $TD_BUILD_SPECS | xargs -P "$nspecs" -n1 sh tests/build-pkg.sh
  echo "PASS: build-recipes — all $nspecs package recipes realized + reproducible via the shared build daemon into .td-build-cache/pkg; the build gates now cache-hit + memo-skip the double-build and only assert behavior/oracle."
else
  echo "PASS: build-recipes — prelude only (no specs among the selected gates): stage0 seed realized + td-recipe-eval built; nothing pre-built (in-gate builds cover the rest)."
fi
