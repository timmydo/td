section: side
status: claimed
handle: claude-fable-db65ca
date: 2026-06-20
title: guix-free-seed
notes: plan/guix-free-seed.md
summary: North star (human 2026-06-20) — remove guix ENTIRELY, no guix process AND no guix install dependency. Mechanism is a FROZEN SEED-BINARY TARBALL (gcc/glibc/binutils + the few build tools td can't yet self-build, captured ONCE from guix into a pinned content-addressed tarball; regenerable, not a live dependency) — NOT a Mes-style full-source bootstrap (that's an optional later refinement). Priority ladder: (1) no `guix` process in user-facing commands / build paths — `td shell` resolves a td-BUILT package (recipe -> td-builder build-on-demand, cached), never `guix build`; unknown pkg errors, no guix fallback; (2) serve the toolchain seed from the tarball instead of a host guix (the loop no longer needs a guix install); (3) retire the loop's guix oracle/lowering (`guix build --check` repro oracle, `guix repl`/`system` lowering) LAST. This PR is the DIRECTION (CLAUDE.md "North star", DESIGN §5 reframe, PLAN preamble) + this claim; implementation lands incrementally per the ladder.
