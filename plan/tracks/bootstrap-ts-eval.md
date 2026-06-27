section: side
status: done
title: bootstrap-ts-eval
handle: claude-fable-300f35
date: 2026-06-20
notes: plan/bootstrap-ts-eval.md
summary: move-off-Guile §5 "build the seed tools with td" — follow-on to [[bootstrap-td-builder]] (td-builder fully off guix as a build tool). td-ts-eval (td's own boa-based JS evaluator, ts-eval/, boa_engine 0.20, 128 crate deps) is still `guix build -e (system td-ts) td-ts-eval`-produced. Brick 4 (DONE #102): td builds td-ts-eval from source via `td-builder build-recipe` (buildSystem rust) by stage0 with its 128 boa crates vendored; new rust-ts-eval gate (evaluates a spec == guix's, reproducible, distinct path); guix td-ts-eval kept as seed+oracle. Brick 4b (DONE #106): the gnu-recipe build path (build-recipes prelude + corpus/toolchain/corpus-deps gates) evaluates its recipes with the td-BUILT td-ts-eval (shared `tests/ts-eval-tool.sh`, content-addressed cached — no per-loop prelude; load_ts_eval), removing `guix build (system td-ts) td-ts-eval` from those gates; outputs unchanged. Brick 4c (in progress): route the rust gates' ts-emit (rust-build/-vendor/-uutils/-russh) onto the td-built td-ts-eval too (load_ts_eval, no Makefile edit — the prelude already wrote the sentinel); rust-ts-eval keeps the seed (it BUILDS td-ts-eval + uses it as oracle). node + tsc (the JS-runtime transpile seed) stay guix, retired-late.
