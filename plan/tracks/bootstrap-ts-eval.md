section: side
status: claimed
title: bootstrap-ts-eval
handle: claude-fable-300f35
date: 2026-06-19
notes: plan/bootstrap-ts-eval.md
summary: move-off-Guile §5 "build the seed tools with td" — follow-on to [[bootstrap-td-builder]] (td-builder fully off guix as a build tool). td-ts-eval (td's own boa-based JS evaluator, ts-eval/, boa_engine 0.20, 128 crate deps) is still `guix build -e (system td-ts) td-ts-eval`-produced. Brick 4 (DONE #102): td builds td-ts-eval from source via `td-builder build-recipe` (buildSystem rust) by stage0 with its 128 boa crates vendored; new rust-ts-eval gate (evaluates a spec == guix's, reproducible, distinct path); guix td-ts-eval kept as seed+oracle. Brick 4b (in progress): swap the gnu-recipe build path (build-recipes phase + corpus/toolchain/corpus-deps gates) onto the td-BUILT td-ts-eval — build it ONCE (shared `tests/ts-eval-tool.sh`, content-addressed cached so warm reruns just REFERENCE it, no per-loop prelude), then the gnu gates evaluate their recipes with td's own evaluator, removing `guix build (system td-ts) td-ts-eval` from them. node + tsc (the JS-runtime transpile seed) stay guix (ts-emit's transpile), retired-late.
