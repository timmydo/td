# rust-recipe-surface — working notes

Handle: claude-fable-544148 · started 2026-06-28

## Goal

Replace the boa/TypeScript package-declaration surface (`ts-eval/` boa crate +
`tests/ts/recipe-*.ts` + `td-spec.d.ts`) with **packages declared in Rust**.
The TS surface was itself only "Phase 1 of move-off-Guile" (per the
`td-spec.d.ts` header) — moving it to Rust is the stronger version of the same
move: `rustc` subsumes the `tsc` type-check gate, and a whole JS engine (boa)
plus its crate closure leaves the loop.

## PR1 scope (this branch) — the recipe surface in Rust

- `recipes/` crate, **dependency-free / offline-buildable** (the `builder/`
  discipline — no serde; hand-rolled JSON):
  - `types.rs` — the full recipe vocabulary as typed Rust (mirrors
    `td-spec.d.ts`): `Recipe`, `Source`, `Phase`, `Stmt`, `Clause`,
    `Replacement`, `RefPart`, `FileArg`, `Substitution`, enum `BuildSystem`.
    Each has `to_json(&self) -> Json`. rustc enforces the shape.
  - `json.rs` — a tiny `Json` value enum + recursive-descent parser +
    canonical (sorted-key, compact) writer. Canonicalization lets `verify`
    compare boa's JSON to the Rust recipe's JSON regardless of key order /
    whitespace, with no external JSON lib.
  - `registry.rs` — `lookup(name)` / `names()` over the migrated recipes.
  - `bin/td-recipe-eval.rs` — `emit NAME` (print recipe JSON), `list`,
    `verify NAME BOA.json` (canon-compare; nonzero on mismatch).
- New gate `mk/gates/207-recipe-rs.mk` + `tests/recipe-rs.sh`:
  - **durable**: every migrated recipe `emit`s valid JSON that round-trips
    (canon(emit) is stable) and carries the required fields (name/version/
    buildSystem); coverage = each Rust recipe name has a `recipe-NAME.ts` twin.
  - **migration oracle (REMOVABLE)**: per recipe, boa's JSON (via `ts-emit`)
    canon-equals the Rust recipe's JSON. Labeled so retiring boa deletes these
    lines, not the gate.
  - **durable self-discrimination**: `verify` of a recipe against a DIFFERENT
    recipe's JSON FAILS — the always-on negative control.
- boa (`td-ts-eval`) stays as the oracle. **No consumer cutover** — the corpus
  still builds from boa JSON. `td-recipe-eval` is loop test scaffolding (built
  with cargo in-sandbox, like the `cargo-test` gate), not a store artifact, so
  prime-directive-1 repro applies once the corpus consumes it (PR2), where the
  existing corpus NAR-equality already covers it.

## Verified-red ladder

- [ ] crate builds offline (no network, no non-vendored deps)
- [ ] `emit hello` matches boa's `ts-emit recipe-hello.ts` (canon) — break the
      version, watch `verify` go red
- [ ] gettext-minimal (phases vocabulary) matches boa — the hardest recipe
- [ ] self-discrimination: `verify hello <gzip.json>` fails
- [ ] full `recipe-rs` gate green over all migrated recipes

## Follow-ups → plan/rust-migration.md
