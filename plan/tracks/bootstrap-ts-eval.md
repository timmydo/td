section: side
status: claimed
title: bootstrap-ts-eval
handle: claude-fable-300f35
date: 2026-06-19
notes: plan/bootstrap-ts-eval.md
summary: move-off-Guile §5 "build the seed tools with td" — follow-on to [[bootstrap-td-builder]] (td-builder fully off guix as a build tool). td-ts-eval (td's own boa-based JS evaluator, ts-eval/, boa_engine 0.20, 128 crate deps) is still `guix build -e (system td-ts) td-ts-eval`-produced. Brick 4: build td-ts-eval from source via `td-builder build-recipe` (buildSystem rust) by stage0 with its 128 boa crates vendored (fixed-output static.crates.io fetches, Cargo.lock-pinned — same machinery as rust-uutils/russh), proven by a new gate; the guix-built td-ts-eval is the SEED that evaluates its own recipe + the behavioral ORACLE (cargo→stage0→td-ts-eval, own-then-diverge). Brick 4b (follow-up): swap the package gates' ts-emit onto the td-built td-ts-eval, removing the seed-tool invocation. node + tsc (the JS runtime) are the hard retired-late seed.
