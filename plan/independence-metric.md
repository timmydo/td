# independence-metric — measure how much of td depends on guix

Handle: claude-opus-f4c9c8 — claimed 2026-06-15.

## Why

Standing question (human, 2026-06-15): "until td can build all those packages
itself, in reality we're just testing guix, not td." The loop has many
*differential* gates (corpus-\*, td-build, drv-\*, store-\*) that test td's own
code against guix as the oracle — but nothing **quantifies** the ratio. Two
independence axes get conflated:

- **runtime** — is guix in the *shipped product's* closure? Already covered by
  the `no-guix` gate (110): the image ships zero guix binaries.
- **build-time** — were guix-built *tools* used to *produce* it? **Unmeasured.**
  This track adds the missing number.

## What it measures (build-time independence)

A derivation is **td-reproducible** iff td has a recipe proven store-path-equal
to the corpus oracle — i.e. a `tests/ts/recipe-<spec>.ts` exists (every one is
proven by a `corpus-*` gate in the same ladder; in a green loop, recipe ⇒ proof).

For a **target**, take the full *build closure* (the derivation prerequisite
graph — `derivation-prerequisites`, no building) and classify each derivation
td-reproducible vs guix-supplied. Two targets:

- `corpus-union` — union build closure of all owned recipes. The number that
  *moves* as input-recipes lands more recipes.
- `shipped-system` — `(operating-system-derivation td-system)` from
  `system/td.scm`. The product. ~0% today (a few owned packages happen to land
  in its closure, e.g. gzip).

Baseline at pin 520785e3: `corpus-union 6/266 (2.26%)`, `shipped-system
3/1405 (0.21%)`.

## Shape (smallest increment)

- `tests/guix-dependence.scm` — the census; auto-derives the owned set from
  `tests/ts/recipe-*.ts` (minus `*-perturbed.ts`), computes both targets,
  emits a deterministic report, and compares it verbatim to the snapshot.
- `tests/guix-dependence.expected` — the checked-in census snapshot. Drift is a
  deliberate re-baseline (DIGESTS pattern): landing a recipe raises the number
  and the snapshot delta shows it in the PR. Pin-keyed (a channel bump
  re-baselines it like DIGESTS).
- `mk/gates/070-guix-dependence.mk` — cheap gate (<2s; lowers derivations, no
  build; offline). Purely additive — removes/loosens nothing (directive 3).

## Honest scope / follow-ups

- v1 grounds "owned" on the recipe files + asserts each resolves to a real
  corpus package; it does NOT re-lower each TS recipe in-census (that needs the
  TS toolchain → heavy). The proof lives in the sibling `corpus-*` gates. A
  stronger binding (derive owned from gate coverage, or re-lower in-census) is a
  follow-up.
- The denominators are guix's closure shape — that's the point: the gate records
  td's *ownership ratio* and catches drift in it. It does not re-prove
  reproducibility (the corpus gates do).

## Verified-red ladder (SEEN red 2026-06-15)

1. perturb the snapshot count (266→265) → exit 1, FAIL "census drifted".
   Comparison has teeth. Restored → PASS.
2. hide tests/ts/recipe-gzip.ts → recomputed `owned-recipes (5)`,
   `corpus-union 5/265 (1.89%)`, `shipped-system 2/1405 (0.14%)` (gzip dropped
   from both present-lists but still counted in the system TOTAL as a
   guix-supplied dep) → mismatch, exit 1. The metric tracks ownership, not a
   constant. Restored → PASS.

## Baseline (pin 520785e3)

    owned-recipes (6): gzip hello libatomic-ops nano pkg-config popt
    corpus-union: td-reproducible 6 / 266 (2.26%) [all 6]
    shipped-system: td-reproducible 3 / 1405 (0.21%) [gzip nano pkg-config]

## Sub-task ladder

- [x] census script + snapshot + gate; baseline recorded
- [x] verified-red (both rungs) recorded here
- [ ] gate green in the sandbox (./check.sh guix-dependence)
- [x] full ./check.sh green; landed via PR #63 (auto-merge armed, awaiting review)
