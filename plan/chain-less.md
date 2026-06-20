# chain-less — a NEW owned recipe that auto-chains onto td's ncurses

Handle: claude-opus-3267ea — started 2026-06-20. Stacks on #113 (manifest retired:
gate 365 + the census derive edge-ownership from the recipe graph).

## Goal

#113 made the edge-ownership infra self-maintaining: a new recipe with owned input
edges chains (gate 365 `build-plan --auto`) and gets credited (census) with NO manifest
edit. This track *exercises* that end-to-end by landing one new package — GNU **less** —
whose only declared input is the already-owned **ncurses**. Nothing in gate 365 or the
census is touched: less is picked up automatically.

## Scope note (honest)

The original pitch was a two-owned-edge package. On inspection at the pin:
- guix's `less` 608 depends on **ncurses only** (it does not enable PCRE2), so less is a
  single clean ncurses edge — not ncurses+pcre2.
- The genuine two-owned-edge candidates wall on native tooling: `bc` 1.08.2 needs
  `autoreconf`/`ed`/`flex` (native-inputs absent from the toolchain seed); `rlwrap` needs
  `autoreconf` + `libptytty` (non-owned). Neither is a clean `./configure && make` build.

So less (ncurses, `native=()`, `args=()`) is the clean, guix-faithful increment. The
deliverable's value is the *self-maintaining proof*, not the edge count.

## Changes

- **`tests/ts/recipe-less.ts`** — new owned recipe: GNU less 608, `buildSystem: "gnu"`,
  `inputs: ["ncurses"]`, source `mirror://gnu/less/less-608.tar.gz`.
- **`tests/less-no-guix.lock`** — the curated td build env (the same 15 toolchain seed
  lines as nano/grep at this pin) + the `ncurses` seed (hash-named — nano proves `--auto`
  re-keys that form) + `less-source` (the RAW tarball fixed-output; guix's hurd-only patch
  is irrelevant on Linux).
- **`tests/guix-dependence.expected`** — re-baselined: owned-recipes 25→26; corpus-union
  25/26; edge-owned 26/26; `chained` gains `less`. (Derived, not hand-edited — regenerated
  with `TD_DEPENDENCE_WRITE=1`.)
- Gate 365 + the census: UNCHANGED — less is auto-derived as a subject / auto-credited.
- Gate 365 gains a `less)` behavioral arm (`less --version` loading td's ncurses) — a new
  assertion, the one structural edit, called out here.

## Result

`edge-owned 26 / 26`; `chained: bash gettext-minimal grep less nano readline`. A brand-new
recipe chained onto td's ncurses with zero edits to the chaining infra — the end-to-end
validation that #113's edge-ownership derivation is self-maintaining.

## Verified-red

- (to fill) Gate 365 less arm: break the `less)` behavioral check / perturb the lock's
  ncurses edge → less builds guix's ncurses → structural red.
- Census: less must appear in `chained` and owned-recipes must read 26 — drop recipe-less.ts
  → census drifts from the snapshot → red.
