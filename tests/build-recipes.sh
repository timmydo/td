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
#   TD_BUILD_SPECS  the BUILD_SPECS pool (space-separated spec names)
#   TD_GUIX         the pinned guix invocation prefix ("guix time-machine -C channels.scm --")
set -euo pipefail

: "${TD_BUILD_SPECS:?the gate runner passes TD_BUILD_SPECS (the BUILD_SPECS pool)}"
: "${TD_GUIX:?the gate runner passes TD_GUIX (the pinned time-machine prefix)}"
# Two gates may declare the same spec (corpus-no-guix and oci-native both consume
# hello); build each recipe ONCE — a duplicate here would run two concurrent
# build-pkg.sh on the same $CACHE/<spec> dir (their b/*.drv resets race).
TD_BUILD_SPECS=$(printf '%s\n' $TD_BUILD_SPECS | sort -u | tr '\n' ' ')
nspecs=$(set -- $TD_BUILD_SPECS; echo $#)
[ "$nspecs" -gt 0 ] || { echo "ERROR: empty TD_BUILD_SPECS — no package recipes registered" >&2; exit 1; }

echo ">> build-recipes: assemble + submit $nspecs recipes to the shared build daemon (global budget), then reproducibility-check ($TD_BUILD_SPECS)"
: "${TD_DAEMON_SOCKET:?the shared build daemon is not running — the \`td-builder check\` host prelude starts it (ensure_build_daemon)}"
for s in $TD_BUILD_SPECS; do grep ' /gnu/store/' "tests/$s-no-guix.lock"; done \
  | sed 's/^[^ ]* //' | sort -u | xargs $TD_GUIX build >/dev/null \
  || { echo "ERROR: could not realize the build seed (regenerate locks on a channel bump)" >&2; exit 1; }
grep ' /gnu/store/' tests/td-builder-rust.lock | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null \
  || { echo "ERROR: could not realize the stage0 toolchain seed (regenerate tests/td-builder-rust.lock on a channel bump)" >&2; exit 1; }
export CACHE="$PWD/.td-build-cache/pkg" TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; mkdir -p "$CACHE"
. tests/cache-lib.sh; load_stage0
echo ">> builds run on the td-bootstrapped stage0 td-builder ($TD_BUILDER_PATH) — NO guix-built td-builder (move-off-Guile §5 brick 3)"
TD_GUIX="$TD_GUIX" sh tests/recipe-eval-tool.sh "$PWD/.td-build-cache/recipe-eval" >/dev/null \
  || { echo "ERROR: could not build td's Rust recipe evaluator (recipes/ crate)" >&2; exit 1; }
load_recipe_eval
echo ">> recipes EVALUATE with td's OWN Rust td-recipe-eval ($TD_RECIPE_EVAL) — boa retired (rust-recipe-surface)"
echo ">> submitting $nspecs recipes to the shared build daemon ($TD_DAEMON_SOCKET); the daemon's global budget caps concurrency ..."
printf '%s\n' $TD_BUILD_SPECS | xargs -P "$nspecs" -n1 sh tests/build-pkg.sh
echo "PASS: build-recipes — all $nspecs package recipes realized + reproducible via the shared build daemon into .td-build-cache/pkg; the build gates now cache-hit + memo-skip the double-build and only assert behavior/oracle."
