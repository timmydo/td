# guix-packager-ratchet — working notes

Handle: claude-fable-a94246 · base: `#113` (ca613a1) · 2026-06-20

## Goal

Make "no NEW guix-as-packager usage" a tracked, enforced invariant (move-off-Guile
§5), so the arc can't silently regress — e.g. a new `(build-system …)` package or a
`guix build -e '(@ (system M) pkg)'` slipping in instead of td placing its own seed.

Principle (from the bootstrap-stage0 / tsgo arc): an external seed is a pinned
FIXED-OUTPUT FETCH the loop realizes + td PLACES (`store-add-recursive`); it must NOT
be a guix `(build-system …)` package built via `guix build -e '(@ (system M) pkg)'`.
That "guix-as-packager" surface is what we forbid growing. Out of scope: removing the
EXISTING packager sites (td-typescript/td-ts-eval/td-builder) — those are retired by
their own tracks (tsgo-migrate #111, bootstrap-*). This track snapshots + ratchets them
so they can only go DOWN.

## What landed

- **`tests/guix-surface.sh`** — static, offline scanner. Scans Makefile, mk/gates/*.mk,
  tests/*.sh (excludes itself + leading-`#` comment lines) for active
  `guix build -e '(@ (system M) NAME)'` invocations; classifies each ref by reading
  `system/M.scm` — a `(package …)` define = PACKAGER, an `(origin …)`/`url-fetch` define
  = an allowed FETCHER seed. Emits the sorted distinct `(file, (system M) name)` PACKAGER
  sites. WRITE mode (`TD_SURFACE_WRITE=1`) re-baselines `tests/guix-surface.expected`;
  COMPARE mode is a one-way ratchet: FAIL if any current site is absent from the snapshot
  (grew), PASS when the set only shrinks (+ a re-baseline nudge). Also prints a compact
  informational census (packager / oracle / lowerer / gc) — only the packager set is
  ratcheted.
- **`tests/guix-surface.expected`** — baseline snapshot: **48 packager sites**
  (td-builder ×27, td-typescript ×14, td-ts-eval ×7 across the gates + Makefile).
  (Started at 47; rebasing onto #112 added `mk/gates/356-build-hermetic.mk`'s
  td-builder site — the gate FLAGGED it as a grow, absorbed into the baseline since
  #112 landed first. It's a step-2 routing target.)
- **`mk/gates/072-guix-surface.mk`** — drop-in cheap gate (registers
  `CHEAP_GATES += guix-surface`; no Makefile edit → parallel-safe). `make list-gates`
  shows it in the cheap pool.
- **CLAUDE.md directive 7** + **DESIGN §5 bullet** — new seeds are td-placed
  fixed-output fetches, never guix `(build-system …)` packages; the `guix-surface` gate
  enforces it; growth needs sign-off + a snapshot edit (directive 3).

## Honesty / scope boundaries

- The package-vs-origin discrimination is load-bearing and STATIC (reads the define's
  constructor in `system/M.scm`) — so a future `guix build -e '(@ (system td-ts)
  td-tsgo-source)'` (an origin = a fetch) is correctly an allowed FETCHER, while
  `td-typescript` (a package) is a PACKAGER. No guix invoked → truly cheap.
- `specification->package` lives in `.scm` (the resolver axis) — out of this gate's
  shell/make scope; it is tracked by the guix-dependence census (070) + the resolve
  gate, not double-counted here.
- Site key = distinct `(file, mod, name)`, not line number → robust to reformatting /
  line moves; still catches a new file packaging a seed or a file packaging a new seed.
- Ratchet is subset-based (current ⊆ baseline), strictly stronger than a bare count so a
  remove+add in one PR can't sneak a new site in at equal count.

## Verified-red (seen fail before trusting the pass)

1. **Growth fails.** Appended `tb=\`$(GUIX) build $(LOAD) -e '(@ (system td-builder)
   td-builder)'\`/bin/td-builder` to `mk/gates/010-eval.mk` (a gate with no packager
   site) → gate RED: `FAIL: guix-as-packager surface GREW … + mk/gates/010-eval.mk
   (system td-builder) td-builder`, exit 1. Restored → green.
2. **Origin discrimination is real (control).** Same invocation form but resolving
   `(@ (system td-builder) %builder-source)` (an `(origin …)`, not a package) → PASS,
   count stays 47 (classified FETCHER, not PACKAGER). Proves the package/origin check is
   load-bearing, not cosmetic — exactly the `td-tsgo-source` vs `td-typescript`
   distinction. Restored → green.
3. **Shrink passes + nudges.** Deleted the td-builder packager line from
   `mk/gates/360-td-offline.mk` → PASS with `ratchet slack: 1 packager site(s) retired
   since baseline — re-baseline …`. Restored → green.

(Reverts done via file backups, not `git checkout` — uncommitted green edits coexist.)

## Landing

- Drop-in gate fragment (not a Makefile edit) → parallel-safe per CLAUDE.md.
- CLAUDE.md / DESIGN §5 edits are doc; §5 bullet is a new line (no conflict expected
  with the open DESIGN-sync PR #109, which touches §1.1/§1.2).
- Interaction with tsgo-migrate (#111) / bootstrap tracks: when they remove
  td-typescript / td-builder packager sites, the surface SHRINKS → this gate still
  PASSES (subset). No forced re-baseline on those PRs.
- Validate: `tools/affected-checks.sh --committed-only --run` (record waiver/escalation
  in the PR) → draft PR → ready + auto-merge.
