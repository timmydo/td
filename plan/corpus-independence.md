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
