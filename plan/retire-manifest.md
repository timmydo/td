# retire-manifest — derive edge-ownership from the recipe graph, drop the manifest

Handle: claude-opus-3267ea — started 2026-06-20. Stacks on #110 (`build-plan --auto`).

## Goal

#110 added `td-builder build-plan --auto`, which derives the build-plan from the recipe
graph. With it, the hand-written `tests/td-chained-edges.txt` manifest is redundant:
both consumers can derive edges from the graph. Retiring it makes the edge-ownership
infra self-maintaining — a new recipe with owned inputs chains + gets credited with no
manual manifest edit.

## Changes

- **Gate 365 (build-plan)** — rewritten to DERIVE its subjects from the recipe graph
  (every owned recipe — has `recipe-*.ts` + `*-no-guix.lock` — with ≥1 owned input edge)
  and build each via `td-builder build-plan --auto`. No manifest. Per subject: structural
  (the subject's `.drv` references its td edge outputs, not guix's), behavioral (runs
  loading td's deps; a library subject's `.so`), repro, oracle. Deps cache across subjects.
- **Gate 367 (build-plan-auto)** — DELETED, subsumed: 365 now uses `--auto` for every
  subject (bash included).
- **Census (`tests/guix-dependence.scm`)** — drops the manifest read (`chained-edges`,
  `td-wired-edges`, `validate-edges!`). edge-owned is derived from the graph: every owned
  recipe is edge-owned (`--auto` wires each owned-input edge, 365 proves it), so
  `edge-owned N / N`; `chained` lists the recipes with owned input edges. N grows with the
  owned set automatically.
- **`tests/td-chained-edges.txt`** — DELETED.

## Result

`edge-owned 25 / 25` (the owned set grew to 25 via #105's which/gperf, both leaves);
`chained: bash gettext-minimal grep nano readline` — the same 5 the manifest enumerated,
now derived. The invariant "every owned recipe builds FROM td inputs" is gate-enforced
(365 reds if any owned-input edge can't chain) and the metric self-maintains.

## Verified-red

- Census: a recipe with an owned input edge whose dep isn't actually owned would not be
  counted — but by construction owned-input-edges filters to owned recipes, so the metric
  tracks the graph; the GATE is the build proof (365 reds if a chain doesn't build, as
  shown when gettext needed ncurses --with-shared, #107).
- Gate 365: break the `--auto` td-recipe-output marking (in td-builder) → a subject builds
  with guix's dep → structural red (the substitution VR from #107/#110 applies; `--auto`'s
  generation is unit-VR'd in #110).
