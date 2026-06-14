# plan/corpus-independence.md — td's own recipes vs the Guix corpus (Phase 2)

Track: **corpus-independence** (DESIGN §7.1, approved 2026-06-13 — §4.3 gate-1,
graduated from §6 on the human go-ahead "do Phase 2 corpus independence … working
POC", 2026-06-13).
Claim: claude-fable-4a2e33, 2026-06-13.
Single writer: the claiming agent.

## Goal (Phase 2 of the §5 move-off-Guile goal)

Replace the *corpus* — where a package's definition comes from. Today every td
artifact reads the pinned Guix corpus (`(gnu packages …)`). Phase 2 reconstructs
recipes from upstream coordinates (source URL + hash + build system), proving each
td-authored recipe NAR-hash-equal to the Guix corpus's build of the same package —
Guix as the oracle (§2.5 / prime directive 4), retired LAST.

## Direction decision (human, 2026-06-13) — author recipes in TypeScript

The first cut authored the hello recipe as a hand-written Guile module
(`system/td-corpus.scm`). The human flagged this: the §5 goal is to move authoring
*off* Guile onto TypeScript, so a new hand-authored Guile artifact is the wrong
direction. **Resolution: the recipe is AUTHORED in the TypeScript surface** and
lowered by a *generic* Guile bridge. The Guile that remains is the bridge — the
retire-last lowering target — not a hand-written recipe. `system/td-corpus.scm` was
removed; `tests/ts/recipe-hello.ts` + `system/td-recipe.scm` replace it.

### The two axes COMPOSE here

- **Surface axis** (`ts-frontend`, Phase 1): what *language* a spec is written in
  (Guile → TypeScript). Delivered the boa evaluator + tsc front-end.
- **Corpus axis** (this track): where the *package definition* comes from (Guix
  corpus → td's own recipes).

Phase 2 composes them: a recipe is authored in TS (corpus content) and lowered
through the TS front-end + a generic Guile bridge (surface + lowering target). What
stays Guix's, retired LAST (§5: seed external, no full-source bootstrap): the
**toolchain** (gcc/glibc/make …) and the **build-system** (`gnu-build-system`).
What changes is **provenance**: the recipe is reconstructed from upstream
coordinates, NOT looked up in `(gnu packages …)`.

## Pipeline

```
recipe-hello.ts --tsc--> JS --boa(td-ts-eval, recipe()/fetchSource())--> recipe JSON
   --(system td-recipe bridge)--> Guix package --lower--> drv
                                                            |
                              differential vs (gnu packages base) hello  (NAR-hash-equal)
```

- **boa evaluator** (`ts-eval/src/main.rs`): the curated-global prelude gains
  `fetchSource(uri, sha256)` (declares an upstream source as data — does NOT fetch)
  and `recipe(r)` (captures the package declaration); the capture emits the recipe
  as JSON (taking precedence over `system()`). Pure data-capture JS, like the
  existing `system()` — hermetic contract unchanged, no new crates.
- **bridge** (`system/td-recipe.scm`): `json-recipe->package` reconstructs a Guix
  `package` from the JSON (name/version/source/build-system). Generic — it
  interprets recipe DATA, it does not author a recipe; imports no `(gnu packages …)`.

## Why GNU hello is the POC package

- Maximally trivial in the pinned channel: no inputs, no native-inputs, no
  `arguments`, plain `gnu-build-system` (probed 2026-06-13). So a from-scratch
  recipe with the right coordinates lowers to the corpus's exact derivation
  (ungrafted `2nfg943…-hello-2.12.2.drv`; grafted `zx4bn6w…`).
- Its output is already warm in the loop's store (`manifest-check` swaps hello into
  an image), so the build + `--check` runs offline.
- NAR-hash equality is necessarily via the SAME store path: hello bakes its own
  `$out` (LOCALEDIR) into the binary, so a different-path build would differ in
  bytes. Convergence ⇒ same drv ⇒ same path ⇒ identical NAR. The differential's
  discriminating power is therefore the **perturbation** leg, not the (tautological-
  once-converged) same-path NAR compare. Recorded so a later reviewer doesn't
  mistake the same-path NAR equality for the load-bearing assertion.

## Acceptance (DESIGN §7.1)

A self-discriminating differential (modeled on `tests/ts-diff.scm`), the single
TS-driven `corpus` rung:
1. The TS recipe (recipe-hello.ts → tsc → boa → bridge) lowers store-path-equal
   (NAR-hash-equal) to the corpus `hello`.
2. A perturbed TS recipe (recipe-perturbed.ts, one wrong byte in the source hash)
   DIVERGES — **verified-red** (never vacuous).
3. The BUILT artifact is reproducible (`guix build --check`, verdict-memoized) and
   its output NAR hash equals the corpus oracle's.

## Files

- `ts-eval/src/main.rs` — boa prelude gains `recipe`/`fetchSource`; capture emits
  the recipe JSON. (Extends the ts-frontend evaluator — now shared infra.)
- `tests/ts/td-spec.d.ts` — the TS dialect gains `Source`/`BuildSystem`/`Recipe` +
  `fetchSource`/`recipe`.
- `tests/ts/recipe-hello.ts` — the recipe, authored in TypeScript.
- `tests/ts/recipe-perturbed.ts` — the discriminator (wrong source hash).
- `system/td-recipe.scm` — the generic JSON-recipe → package bridge.
- `tests/ts-recipe-diff.scm` — the converge/discriminate differential.
- `tests/ts-recipe-drv.scm` — emits the drvs for the build leg.
- `Makefile` — single heavy `corpus` rung (differential + build + --check + NAR);
  `tests/eval.scm` loads `(system td-recipe)`.

## Exclusive-landing note

Touches the shared spine: DESIGN §6/§7.1, PLAN.md, `Makefile`, `tests/eval.scm`, and
the ts-frontend evaluator `ts-eval/src/main.rs` (now shared infra — ts-frontend is
landed/DONE). Announced here; others rebase. No other tracks in flight at claim time.

## Sub-task ladder (write the test first; verify red before trusting green)

1. Charter: graduate §6→§7.1, claim in PLAN, this file. — DONE 2026-06-13.
2. (superseded) Guile-authored `system/td-corpus.scm` + `corpus-diff`/`corpus` rungs.
   — REPLACED per the direction decision above.
3. TS-authored recipe: evaluator `recipe`/`fetchSource`, dialect, recipe-*.ts,
   `system/td-recipe.scm` bridge, single `corpus` rung. — DONE 2026-06-13.
4. Full `./check.sh` green; PR.

## Implementation progress

- **TS-authored recipe POC: DONE 2026-06-13.** `./check.sh corpus` GREEN in-sandbox:
  recipe-hello.ts emits
  `{"name":"hello","version":"2.12.2","source":{...,"sha256":"1aqq…"},"buildSystem":"gnu"}`;
  the bridge lowers it to `2nfg943…-hello.drv` == the corpus oracle; recipe-perturbed.ts
  diverges (`a5nc0x49…`); build + `--check` (MEMO HIT — same drv as the earlier
  Guile cut) + NAR-equal (`0qhasy0w…`). The boa binary rebuilt with the new prelude
  and the `ts-eval`/`ts-diff` rungs are unaffected (system() path + bare-result
  fallback unchanged).

- **Own Rust builder (replace gnu-build-system): DONE 2026-06-13.** Scope fixed by
  the human 2026-06-13: replace gnu-build-system with a td/Rust builder, KEEP guix
  for `.drv` construction; differential is BEHAVIORAL (own-builder output has a
  distinct store path — hello bakes `$out`). New `autotools-build` mode in the
  td-builder crate (`builder/src/build.rs`) runs the autotools phases in Rust
  (set-paths from `TD_INPUTS` → tar unpack → `./configure --prefix=$out` → `make` →
  `make install`), invoked AS the derivation's builder. `system/td-build.scm`
  constructs that derivation with a raw `derivation` (builder = the td-builder
  binary, NOT guile; inputs = the source + gcc-toolchain + make/bash/tar/… toolchain,
  retired last; env = TD_SRC/TD_INPUTS). New `td-build` heavy rung: drives the SAME
  TS recipe (recipe-hello.ts → tsc → boa → JSON), lowers it through td-build, and
  asserts (a) STRUCTURAL — the builder basename is `td-builder` while the corpus
  oracle's is `guile`; (b) REPRODUCIBLE — `guix build --check` (verdict-memoized);
  (c) BEHAVIORAL — the td-built hello and the corpus hello print byte-identical
  output (`Hello, world!`); (d) the artifact is a DISTINCT store object. GREEN
  in-sandbox (`./check.sh td-build`). `eval` loads `(system td-build)`.
  - Findings: gcc-toolchain-15.2.0 is warm (a different compiler than the corpus's
    gcc-14.3 — fine for a behavioral diff); the only fix beyond the minimal phases
    was `make SHELL=<bash>` (the `po/` install rules launch `/bin/sh`, absent in the
    sandbox); the minimal build is already `--check`-reproducible (SOURCE_DATE_EPOCH=1
    + deterministic install; no strip/compress-documentation needed).

## Verified-red log

The "commit before red variants" gotcha (memory): green committed first; reds run on
manipulated env / copies, real fixtures untouched.

- **converge** — feed the real perturbed emitted JSON (from recipe-perturbed.ts) as
  the candidate ⇒ RED "the TS-authored recipe does NOT reproduce the corpus oracle's
  derivation". (exit 1)
- **discriminate** — feed the correct JSON as BOTH candidate and perturbed ⇒ RED
  "differential is vacuous — a perturbed TS recipe … did NOT change the derivation".
  (exit 1)
- Green control with the real (rj, pj) ⇒ PASS (exit 0).
- The build-leg convergence guard (TD_DRV == ORACLE_DRV) is the same convergence
  property as the differential's converge leg (verified-red above); the `--check`
  reproducibility leg reuses `tests/check-memo.sh` (its nondeterminism/expiry/foreign
  reds are verified-red on the check-memo track).

`td-build` rung (own Rust builder), each driven via `./check.sh td-build`, restored after:
- **R1 behavioral** — a temporary defect in `builder/src/build.rs` corrupts the source
  greeting (Hello→Goodbye) before configure; the build succeeds and runs, but ⇒ RED
  "td-built hello printed 'Goodbye, world!', expected 'Hello, world!'" (exit 2). Proves
  the behavioral differential catches a builder that produces a wrong-behaving binary,
  not just a missing one.
- **R2 structural** — `tests/td-build-drv.scm` perturbed to emit the guile-built corpus
  oracle as the td-build drv ⇒ RED "td-build builder is 'guile', expected the td-builder
  Rust binary" (exit 2). Proves the "gnu-build-system is gone" check discriminates a
  Rust-built derivation from a guile-built one.
- The `--check` reproducibility leg reuses `tests/check-memo.sh` (verified-red on the
  check-memo track).

## Follow-on: packages WITH inputs (claude-fable-44df36, 2026-06-14)

The named "broaden the recipe set" step (DESIGN §7.1 corpus-independence: "more
build systems, packages with inputs"). Where `corpus`/`td-build` prove a LEAF
recipe (hello), this proves a recipe with DEPENDENCIES converges.

- **Subject: GNU nano.** Picked by a cheap convergence probe over candidate
  packages: it has a clean `url-fetch` source (no patches/snippet), EMPTY package
  arguments, and two genuine REGULAR inputs (`gettext-minimal`, `ncurses`) — so a
  plain reconstruction (coordinates + gnu-build-system + resolved inputs) lowers
  store-path-identical to the corpus oracle. Other clean candidates (sed, datamash:
  native-input-only; which: no inputs; less: custom origin → diverges) were
  rejected by the same probe. nano's full build closure was warm except the source
  tarball (`guix build -S nano`, substitutes — setup outside the loop, §5).
- **Surface:** `tests/ts/recipe-nano.ts` declares `inputs: ["gettext-minimal",
  "ncurses"]`. The boa evaluator is UNCHANGED — it `JSON.stringify`s the recipe
  object as authored, so a new field needs no Rust change. The `Recipe` TS dialect
  gains optional `inputs?: readonly string[]`.
- **Bridge:** `system/td-recipe.scm` gains `resolve-inputs` — each declared name →
  `specification->package` (input resolution stays Guix's, retired LAST — §5; the
  new-style labels Guix derives are the package names, matching the corpus oracle's,
  so convergence holds). `inputs` is OPTIONAL: hello (no field) lowers exactly as
  before, so the `corpus` rung is untouched (re-verified: hello-converge #t).
- **Rung `corpus-deps`** (heavy; HEAVY_RUNGS, additive — small exclusive Makefile
  landing). Two legs like `corpus` plus the inputs axis:
  (a) converge nano==oracle; (b) perturbed source diverges; (c) inputs STRIPPED
  diverges (load-bearing); (d) ncurses + gettext-minimal are direct
  derivation-inputs; then build + `--check` (verdict-memoized) NAR-hash-equal to the
  corpus oracle (`1fkfyjw5…`). `./check.sh corpus-deps` green, ~44s.

Verified-red (green committed first per the "commit before red variants" gotcha;
each restored via `git checkout`):
- **R1 convergence load-bearing** — `resolve-inputs` returns `'()` (bridge drops
  inputs) ⇒ RED at (a): candidate `!=` oracle (exit 1). Proves input resolution is
  load-bearing for convergence; doubles as the build-leg's convergence-guard red.
- **R2 deps discriminator** — `strip-inputs` made a no-op (no-inputs == candidate)
  ⇒ RED at (c): "the declared inputs are NOT load-bearing" (exit 1). Proves leg (c)
  is not vacuous.
- **R3 input-edge** — the (d) needle pointed at an absent package (`gettextZZZ`)
  ⇒ RED at (d): "a declared build input is missing … (gettext=#f)" (exit 1). Proves
  leg (d) actually verifies presence.
- The `--check` reproducibility leg reuses `tests/check-memo.sh` (verified-red on
  the check-memo track); NAR-equality follows from store-path equality.

Note (out of scope here): `ci/lower-check-drvs.sh` `KNOWN_RUNGS`/`LOWERING_SCRIPTS`
are already stale on main (missing the whole ts/corpus/td-drv/loop arc) — the CI
`check` job is not yet required (`lint` is), so it does not gate local `./check.sh`.
`corpus-deps` + its `tests/ts-recipe-nano-*.scm` add to that backlog; a single
ci-image refresh should reconcile them all (separate `ci/` concern, flagged in PR).

## Follow-on: OWN BUILDER, packages WITH inputs (claude-fable-44df36, 2026-06-14)

Composes the two axes: the with-inputs recipe (nano, from the corpus-deps PR)
built by td's OWN Rust builder (the `td-build`/autotools-build path) instead of
gnu-build-system — so a package that LINKS real dependencies is built with NO
gnu-build-system and NO build-side Guile. Stacked on the corpus-deps PR (needs the
`inputs?` dialect field + `recipe-nano.ts`).

- **`system/td-build.scm`** (`td-build-components`): resolve the recipe's declared
  `inputs` from the corpus by name (`specification->package` — same input
  resolution as `td-recipe.scm`, stays Guix's, retired LAST §5) and fold their
  outputs into `TD_INPUTS` + the derivation's inputs. **No `build.rs` change
  needed** — the Rust `set-paths` already derives `C_INCLUDE_PATH`/`LIBRARY_PATH`
  from every `TD_INPUTS` entry, so the deps' headers/libs are found. A leaf recipe
  (hello, no `inputs`) lowers identically ⇒ the hello-based `td-drv-*` rungs are
  untouched.
- **Feasibility spike (before the rung):** built nano via `td-rust-build-derivation`
  with `inputs ["ncurses" "gettext-minimal"]` → compiled + linked + RAN, byte-
  identical `--version` to the corpus nano (incl. ncursesw-driven `--enable-utf8`),
  closure references ncurses. Guix's ncurses ships `curses.h` FLAT in `include/`
  (no `ncursesw/` subdir), so the minimal `set-paths` suffices — the header-layout
  risk I flagged in the sketch did not materialise for this subject.
- **Rung `td-build-deps`** (heavy; HEAVY_RUNGS, additive — exclusive Makefile
  landing; `tests/td-build-deps-drv.scm`): STRUCTURAL (builder=`td-builder`, not
  `guile`), INPUT-EDGE (ncurses + gettext are DIRECT inputs of the derivation),
  REPRODUCIBLE (`--check`, verdict-memoized), BEHAVIORAL (byte-identical `--version`
  to corpus nano), distinct store path. `./check.sh td-build-deps` green, ~86s.

Verified-red (green committed first; restored via `git checkout`):
- **R1 inputs load-bearing** — `td-build-components` `dep-names` forced to `'()`
  (ignore declared inputs). Two observations: (a) `tests/td-build-deps-drv.scm`
  reports `TD_HAS_NCURSES=no`/`TD_HAS_GETTEXT=no` ⇒ the rung's input-edge assertion
  reds; (b) the actual td-builder build of nano FAILS (configure/compile can't find
  the deps, exit 1). Proves the declared inputs are load-bearing for the own-builder
  build AND that the input-edge check discriminates.
- **STRUCTURAL** (builder must be `td-builder`) and **BEHAVIORAL** (`--version`
  string equality) reuse the exact checks the `td-build` rung already verified-red
  (R2 structural on that track); a `--version` mismatch is a plain string-compare
  red.
- The `--check` leg reuses `tests/check-memo.sh` (verified-red on the check-memo
  track).
- Same `ci/lower-check-drvs.sh` staleness note applies (`tests/td-build-deps-drv.scm`
  joins the backlog; CI `check` not yet required).
