section: side
status: claimed
handle: claude-fable-544148
date: 2026-06-28
title: rust-recipe-surface
pr: 224
notes: plan/rust-recipe-surface.md
summary: Replace the boa/TypeScript package surface with packages declared in RUST (the user-asked "td in rust" direction; the natural next step of the §5 move-off-Guile goal, since the TS surface was itself only Phase 1). PR1 introduces the dependency-free `td-recipe` crate — the recipe vocabulary as TYPED Rust structs (rustc enforces the shape that `tsc` enforced via td-spec.d.ts) plus a hand-rolled JSON value/parser/canonical-writer and a `td-recipe-eval` binary (emit/list/verify). A new `recipe-rs` gate proves each Rust recipe is equivalent to its boa-evaluated `.ts` twin (the removable migration oracle) alongside durable structural + self-discrimination legs; boa stays as the oracle, no consumer cutover yet. Follow-ups spec'd in plan/rust-migration.md: system-spec migration, consumer cutover (cache-lib.sh/ts-diff), boa deletion, then gates->Rust (Makefile/mk), scripts->Rust (*.sh/check.sh), and finally dropping the Guile lowering (the retire-last axis). Links [[td-move-off-guile-author-in-ts]] (this supersedes "author in TS" with "author in Rust"), [[td-source-bootstrap-decision]].
