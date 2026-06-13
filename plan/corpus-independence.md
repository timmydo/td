# plan/corpus-independence.md — td's own recipes vs the Guix corpus (Phase 2)

Track: **corpus-independence** (DESIGN §7.1, approved 2026-06-13 — §4.3 gate-1,
graduated from §6 on the human go-ahead "do Phase 2 corpus independence … working
POC", 2026-06-13).
Claim: claude-fable-4a2e33, 2026-06-13.
Single writer: the claiming agent.

## Goal (Phase 2 of the §5 move-off-Guile goal)

Replace the *corpus* — where a package's definition comes from. Today every td
artifact reads the pinned Guix corpus (`(gnu packages …)`). Phase 2 reconstructs
recipes from upstream coordinates (source URL + hash + build expression) in td's
own module, proving each td-authored recipe NAR-hash-equal to the Guix corpus's
build of the same package — Guix as the oracle (§2.5 / prime directive 4), retired
LAST.

### Two orthogonal axes (don't conflate them)

- **Surface axis** (`ts-frontend`, Phase 1, DONE): what *language* the spec is
  written in (Guile → TypeScript). Lowers through Guile/gexps.
- **Corpus axis** (this track, Phase 2): where the *package definition* comes from
  (Guix corpus → td's own recipes). Authored, for the POC, in a td Guile module and
  lowered through the still-present Guile/gexp layer — the sanctioned lowering
  target, retired last. The surface language is orthogonal: authoring recipes in the
  TS surface needs the deferred `pkg`/`storeRef` builtins (ts-frontend sub-task 4)
  and is a follow-on increment, not a precondition.

What stays Guix's (retired last, by design — §5/§6):
- the **toolchain** (gcc/glibc/binutils/make …) — the seed is external (§5 non-goal:
  no full-source bootstrap);
- the **build-system** machinery (`gnu-build-system` lowering).
What changes: **provenance**. The recipe is td's, reconstructed from upstream
coordinates, NOT looked up in `(gnu packages …)`.

## Why GNU hello is the POC package

- Maximally trivial in the pinned channel (commit in `channels.scm`): no inputs, no
  native-inputs, no `arguments`, plain `gnu-build-system` (probed 2026-06-13). So a
  from-scratch td recipe (own `origin`, own metadata, `gnu-build-system`) lowers to
  the **identical** derivation as the corpus `hello`
  (`/gnu/store/zx4bn6wqcpvhylrp3nvnmnbqx4n1bh83-hello-2.12.2.drv`, MATCH=#t probed) —
  the convergence the §6 differential demands, at the derivation level.
- Its output (`p3b2p9wn…-hello-2.12.2`) is already warm in the loop's store (the
  `manifest-check` rung swaps hello into an image), so the `corpus` rung builds +
  `--check`s offline.
- NAR-hash equality is necessarily via the SAME store path: hello bakes its own
  `$out` (LOCALEDIR) into the binary, so a different-path build would differ in
  bytes. Convergence ⇒ same drv ⇒ same path ⇒ identical NAR. The differential's
  discriminating power is therefore the **perturbation** leg, not the (tautological-
  once-converged) same-path NAR compare. This is the honest shape; recorded so a
  later reviewer doesn't mistake the same-path NAR equality for the load-bearing
  assertion.

## Acceptance (DESIGN §7.1)

A self-discriminating differential (modeled on `tests/typed-diff.scm`):
1. The td recipe lowers store-path-equal (NAR-hash-equal) to the corpus `hello`.
2. The recipe is a genuinely distinct object: `(not (eq? td-hello hello))`.
3. A perturbed recipe DIVERGES — **verified-red** (never vacuous).
4. The BUILT artifact is reproducible (`guix build --check`) and its output NAR hash
   equals the corpus oracle's.

## Rungs

- `corpus-diff` (CHEAP, derivation-level, runs with the other diffs — fail fast):
  acceptance 1–3. `tests/corpus-diff.scm`.
- `corpus` (HEAVY): acceptance 4 — build `td-hello`, `--check` it (verdict-memoized,
  per check-memo), assert its output path + NAR hash == the corpus oracle's.

## Exclusive-landing note

This track edits the shared spine: DESIGN.md §6/§7.1 (graduation) + PLAN.md (claim) +
`Makefile` (two new rungs) + `tests/eval.scm` (load the new module). Announced here;
others rebase. No other tracks are in flight (no open PRs at claim time), so collision
risk is nil. Landed as one track PR with the charter as the first commit.

## Sub-task ladder (write the test first; verify red before trusting green)

1. Charter: graduate §6→§7.1, claim in PLAN, this file. — DONE 2026-06-13.
2. `system/td-corpus.scm` + `tests/corpus-diff.scm` + `corpus-diff` rung +
   `tests/eval.scm` load. Convergence + distinctness + divergence, derivation-level.
   Verify red: (a) perturb the recipe source hash → convergence leg reds; (b) make the
   perturbed variant identical → divergence leg reds (vacuity guard).
3. `corpus` rung: build + `--check` + NAR-hash-equal to the oracle.
4. Full `./check.sh` green; PR.

## Verified-red log

(filled as each assertion is seen red on a COPY in the job tmp — the "commit before red
variants" gotcha: commit the green first, perturb copies, restore.)
