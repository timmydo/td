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

## Build walls hit + fixes (less is legacy C)

1. **patch-shebang on read-only scripts** — less's tarball ships `mkinstalldirs` 0444; td's
   `patch_one_shebang` did `fs::write` → `Permission denied (os error 13)`. Fixed in
   `builder/src/build.rs`: grant owner-write before the rewrite, restore the ORIGINAL mode
   after (reproducibility-safe — $out modes come from `make install`). General fix (any
   read-only build script), with a new unit test.
2. **gcc-15 / C23 vs K&R termcap decls** — `screen.c` declares `char *tgetstr()` K&R-style;
   gcc 15 (td's toolchain seed) defaults to C23 where `()` = `(void)`, conflicting with td's
   ncurses `termcap.h` prototypes. Fixed with `configureFlags: ["CFLAGS=-O2 -std=gnu17"]`
   (pre-C23: `()` = unspecified args). Bounded to less.

## Verified-red

- **build.rs read-only fix** (unit, confirmed): with the write-grant disabled,
  `patch_shebangs_rewrites_a_read_only_script_and_restores_its_mode` fails with the exact
  `patch-shebang .../mkinstalldirs: Permission denied (os error 13)` — the same error less's
  real build hit. Restored → 55/55 cargo tests pass.
- **Gate 365 less behavioral** (confirmed): less's binary `NEEDED libncurses.so.6`; WITH td's
  ncurses on LD path → `less 608 (POSIX regular expressions)`; WITHOUT → loader fails
  (`libncurses.so.6: cannot open`). The `less --version 608` assertion is load-bearing on td's
  ncurses edge.
- **Gate 365 structural** (build proof): less's build was seen RED twice (patch-shebang, then
  screen.c) before the fixes; the `.drv references td's ncurses not guix's` assertion rides on
  `--auto`'s substitution (unit-VR'd #110, gate-VR'd #107).
- **Census**: drop recipe-less.ts → owned-recipes/chained drift from the snapshot → red.
