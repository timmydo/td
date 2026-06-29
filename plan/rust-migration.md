# Rust migration — replacing sh + make/mk + boa/TS with Rust

The full picture for moving td's loop machinery and declaration surfaces to Rust.
PR1 (the `rust-recipe-surface` track) does the first slice — the package-recipe
surface; everything below is the spec for the follow-ups. The order is
smallest-correct-increment first, and each layer lands as its own track of small
green PRs (CLAUDE.md §"Parallel work").

Three layers exist today:

| Layer | Today | Scale |
|---|---|---|
| Package/system surface | boa crate `ts-eval/` + `tests/ts/*.ts` + `td-spec.d.ts`, evaluated to JSON, lowered by Guile | 53 recipes, 4 specs, 1 boa binary |
| Orchestration | `Makefile` + `mk/gates/*.mk` drop-in fragments | ~130 gate fragments |
| Scripts | `check.sh`, `tests/*.sh`, `tools/*.sh`, `ci/*.sh` | ~90 in the canonical tree |

Framing: the TS surface was itself only "Phase 1 of move-off-Guile" (per
`td-spec.d.ts`). Moving it to Rust is the stronger version of the same move —
`rustc` subsumes the `tsc` gate and a whole JS engine (boa) plus its crate
closure leaves the loop. None of this removes guix by itself; the Guile *lowering*
is the retire-last axis (A2 below), kept until the end per the North Star.

---

## A. The boa/TS surface → Rust  (this track + follow-ups)

### A1 — package recipes in Rust  ✅ PR1 (this branch)

`recipes/` = the dependency-free `td-recipe` crate: `types.rs` mirrors
`td-spec.d.ts` as typed structs/enums, `json.rs` is a hand-rolled JSON
value/parser/canonical-writer, `catalog.rs` declares all 53 recipes, and
`td-recipe-eval` emits/lists/verifies. The `recipe-rs` gate proves the Rust
catalog equivalent to the boa/`.ts` surface (durable coverage + round-trip +
discrimination, plus the removable boa oracle). **boa stays; no consumer cutover.**

### A2 — system specs in Rust  (follow-up)

The other thing boa evaluates: `tests/ts/spec-*.ts` (the `system()` axis,
`SystemSpec` in `td-spec.d.ts`). Mirror `SystemSpec` as a Rust struct, add the 4
specs (`v0`, `perturbed`, `gen1`, `bad-fstype`) to the crate, and extend
`recipe-rs` (or a sibling `spec-rs` gate) with the same durable + boa-oracle legs.
Small: it is one more struct and four data values; `bad-fstype` is the negative
control (an out-of-union `rootFsType` must not even compile in Rust — strictly
stronger than the `tsc` TS2322 leg).

### A3 — consumer cutover  (follow-up, the step that retires boa from the loop)

Flip the JSON producers from boa to the Rust crate. The cutover points are few:
- `tests/cache-lib.sh` `cached_build` — one line: `ts-emit recipe-$spec.ts >
  recipe.json` → `td-recipe-eval emit $spec > recipe.json`.
- `tests/ts-diff.scm` driver (`mk/gates/205-ts-diff.mk`) — feed
  `td-recipe-eval`-emitted spec JSON instead of `ts-emit spec-*.ts`.
- `build-recipes` (`Makefile`) + `tests/ts-eval-tool.sh` — drop the boa
  `td-ts-eval` prelude; recipe JSON now comes from the Rust binary.
The DURABLE proof is unchanged and already strong: the corpus gates
(`corpus-*-no-guix`, `td-check`, repro) build the package from the JSON and assert
NAR-equality to guix + behavior. So cutover means "the corpus now builds from
Rust-emitted JSON and stays green" — the existing corpus differential becomes the
recipe layer's durable guard.
Caveat (prime directive 1): once `td-recipe-eval` is in the build path it is a
built artifact — build it reproducibly (a td recipe / `build-recipe`, like
`td-ts-eval` today, or the existing corpus repro double-build covers its output).

### A4 — delete boa  (follow-up, after A3 is green for a full cycle)

Remove `ts-eval/` (the boa crate), `tests/td-ts-eval.lock` (boa's vendored crate
closure), and — once specs no longer need transpile — `tsgo`/`td-tsgo.lock` and
`tests/ts/*.ts` + `td-spec.d.ts`. Retire the `ts`, `ts-eval`, `ts-diff`,
`tsgo-pin` gates (call out each removed gate in the PR — directive 3 — they are
*replaced* by `recipe-rs`/`spec-rs`, the move is the point). The hermetic-eval
self-tests (`ts-eval-check.sh`: deny `Date`/`Math.random`) become **moot** — Rust
data has no ambient clock/RNG, so the eval-hermeticity property is structural, not
a guarded runtime surface. Net: a JS engine + its crate tree leave the loop; the
`guix-dependence`/`guix-surface` censuses shrink.

---

## B. Makefile + mk/gates → Rust  (a `td-check` gate registry)

The Makefile's job is: assemble the `CHEAP/HEAVY/FAST/SYSTEM/ENGINE` pools,
derive the ordering graph, and schedule heavy gates two-at-a-time. The
must-preserve property is **one file per gate, self-registering, no shared list
line** (so concurrent gate PRs never collide).

Shape: a `td-check` crate where each gate is a value registered into a distributed
slice. Zero-dep options to keep the offline discipline:
- a `build.rs` that globs `gates/*.rs` and generates the registry (no external
  crate, preserves "drop a file"), or
- vendor `linkme`/`inventory` (tiny) if a vendor lock is acceptable.

```rust
// gates/eval.rs
gate! { name: "eval", tier: Cheap, order: 10,
    run: |cx| cx.guix(["repl", "-L", ".", "tests/eval.scm"]).run() }
```

The `$(foreach …)` ordering chain + `-j2` LPT packing become a tier-aware
scheduler (cheap serial-first, heavy ≤2 concurrent — the DESIGN §7.3 ceiling).
`make list-gates` → `td-check --list`. Gates still shell out to `guix` via
`Command` — this is orchestration-in-Rust, NOT guix retirement. Migrate
incrementally: a `td-check` that *reads the existing `mk/gates`* and runs them is
the bridge; port recipes file-by-file; keep `make check` working until parity.
Exclusivity: the core `Makefile` is shared spine (exclusive landing); a new gate
is still just a new file.

## C. Scripts → Rust  (fold into `td-check` / `td-builder`)

Two kinds:
- **Test drivers** (`tests/bootstrap-*.sh`, `tests/rust-*.sh`, …) become Rust
  functions a gate's `run` closure calls, sharing libs with the engine
  (`builder/` already has `sandbox`/`store`/`drv`/`nar`).
- **`check.sh`** — its guards (host-guix==pin, netns-discriminates probe, warm
  prelude) + the final `host-sandbox -- make …` become the `td-check` entry.

The one real wrinkle is **bootstrap**: `check.sh` is shell because shell is always
present and it is what *builds* `td-builder`. If the orchestrator is the Rust
binary you are building, that is circular. Two outs, both already in place: keep a
~20-line shell/`cargo run` outer shim, OR ship `td-check` in the frozen seed
(rust + the engine already live on the seed rail — `tools/warm-seed.sh`). Migrate
leaf scripts first (no spine change); do `check.sh` last, behind the shim.

## A2-final. Drop the Guile lowering  (the North-Star finale, retired LAST)

The deepest layer: today recipe/spec JSON is lowered to a `.drv` by the Guile
bridge (`system td-recipe`, `td-config`). td-builder already has `drv.rs` /
`nar.rs` / `store.rs`, so it can lower a `Recipe` struct **directly** to a
derivation, dropping the bridge. This is the retire-last axis (CLAUDE.md North
Star / DESIGN §5): keep the guix differential (`guix build --check`, the corpus
NAR-equality) as the removable oracle until td's own lowering is proven
byte-equivalent, then delete the Guile lowering and, finally, the oracle.

---

## Suggested order

A1 (done) → A2 (specs) → B (gates registry, bridges to existing `mk/gates`) →
A3 (consumer cutover) → A4 (delete boa) → C (scripts, `check.sh` last) →
A2-final (drop the Guile lowering). A and B/C are independent and can proceed in
parallel tracks; A4 must follow A3; A2-final is last of all.
