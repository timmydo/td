# build-plan — chain td-built outputs into downstream builds

Handle: claude-fable-ca5b4f — claimed 2026-06-18.

## Why

Today every `td-builder build-recipe` resolves its inputs from a per-package
lock whose dependency lines are **guix store paths** (the toolchain + library
seeds). So `corpus-no-guix` proves td builds *grep's own derivation* with no
guix/Guile in the path — but grep still links against **guix's** pcre2
(`tests/grep-no-guix.lock` pins `…-pcre2-10.42`), even though td *can* build
pcre2 itself (`corpus-deps-no-guix`, lever 4). The independence census
(`guix-dependence`) credits a package as owned when its recipe builds; it does
not yet assert the **dependency edges** use td-built outputs. That is the gap:
"td builds pcre2" + "td builds grep" does not yet mean "td builds grep FROM
td's pcre2".

This track closes it: a downstream build that consumes a td-BUILT dependency,
proven by the dep's td output path appearing in the downstream `.drv` while
guix's path does NOT.

## Acceptance test (the durable assertion)

`pcre2 → grep`: build pcre2 with td, substitute its output into grep's inputs,
build grep with td. **grep's assembled `.drv` references td's pcre2 output path,
NOT guix's pcre2 store path.** Durable + self-discriminating: perturb pcre2 and
grep's drv moves; drop the substitution and guix's pcre2 reappears (verified-red).
Plus DURABLE behavioral (grep runs from td's output) and DURABLE reproducibility
(`td-builder check` double-build). MIGRATION ORACLE leg: grep's version matches
guix's at a distinct path.

Subject is `pcre2 → grep`, not the headline `ncurses → nano`: ncurses is not yet
td-built (deferred; adjacent to own-builder-daemon / input-recipes). Both ends of
pcre2 → grep are already td-built (`corpus-deps-no-guix` + `corpus-no-guix`), so
the chaining mechanism is provable now, unblocked. ncurses → nano follows once
ncurses is reconstructed — same machinery.

## Design

Three contained builder changes + a typed lock + a `build-plan` driver.

### 1. Typed lock format (point 3)

Lock lines become `NAME PATH [CLASS]` with CLASS ∈
`seed | source | td-recipe-output | crate`. Backward-compatible: a 2-field line
keeps today's inference (`<name>-source` → source, `*.crate` → crate, else seed),
so every existing lock parses unchanged and no existing gate's shell parsing
breaks (those locks stay 2-field). A `td-recipe-output` entry names a dependency
td builds and substitutes — the lock's recorded PATH is the guix oracle for it.

### 2. Multi-db closure

`realize` computes the build closure from a single store-db. A td-built dep's
record lives in its own `td.db` (write_output_db: the dep + its DIRECT refs),
while the transitive seeds (glibc, gcc-lib …) live only in guix's db. So the
closure of grep's inputs spans both. Add a **path-keyed closure over a SET of
dbs**: merge each db's path→refs map; a path's refs come from whichever db knows
it (td.db for pcre2's direct refs; guix's db for those refs' transitive
closures). No rewrite of guix's huge db (the small-table writer can't reproduce
it); the existing rowid `closure` stays for single-db callers.

### 3. td-store staging

`sandbox::build` stages each closure item from an on-disk path. A td-built dep's
files live under a scratch `newstore/` (copied into a shared td-store), not
`/gnu/store`. Rather than add a new staging override here, REUSE PR #97's
`split_closure_entry`: a closure entry may be `CANONICAL\tON-DISK`, and the sandbox
binds from the on-disk half. `realize_drv` re-keys a closure entry whose tree lives
under the td-store dir (`<tdstore>/<base>`) to that form; bare guix-seed entries and
#97's source-override entries pass through. The on-disk half rides through
`closure.txt`, so a later `td-builder check` stages the dep with no extra state.
(Originally a `TD_STORE` env var read in `sandbox::build`; the #97 rebase superseded
it with the closure-encoded mechanism — one staging path, not two.)

### 4. `build-plan` driver

`td-builder build-plan PLAN GUIX-DB SCRATCH` consumes a topo-ordered plan
(`recipe=<json> lock=<typedlock>` per step). For each step: resolve
`td-recipe-output` lock entries to the already-built output (name→path map from
earlier steps), write the resolved lock, build via the build-recipe machinery
with the multi-db closure ([guix-db] ∪ accumulated td.dbs) and td-store staging,
then copy the output into the td-store + record name→path + its td.db.

## Sub-task ladder

1. Typed lock parsing in build-recipe (+ a unit/structural assertion). VERIFY-RED.
2. Path-keyed multi-db closure in store_db_read (+ unit test). VERIFY-RED.
3. td-store staging via PR #97's `CANONICAL\tON-DISK` closure encoding —
   `realize_drv` tab-encodes a td-built dep's entry; `split_closure_entry` binds it.
4. `build-plan` subcommand wiring 2+3 + name→path substitution.
5. Gate `build-plan` (pcre2 → grep) + plan/typed-lock fixtures. VERIFY-RED
   (drop the substitution → guix's pcre2 reappears in grep.drv → gate reds).
6. Land per §7.2.

## Verified-red log

1. **typed lock** (`lock::infer_class`): perturbed to always return `Seed` →
   `untyped_lines_infer_the_historical_classes` reds (`left: Seed, right: Source`).
2. **multi-db closure** (`closure_multi`): limited the merge to the first db
   (`dbs.iter().take(1)`) → `closure_multi_spans_two_dbs` reds (`root /pcre2 is not
   in any store DB`).
3. **the chaining edge** (gate `build-plan`, pre-rebase TD_STORE mechanism): dropped
   the `td-recipe-output` substitution (chained lock = unchanged `grep-no-guix.lock`,
   pcre2 a guix seed) → gate reds `FAIL: grep's .drv does NOT reference td's pcre2
   (…yamvs0m7…-pcre2)`, `make Error 1`, check.sh exit 2. The perturbed grep built at a
   DIFFERENT path (`wagr12s3…-grep-3.11`) than the green chained grep, confirming the
   pcre2 input edge is load-bearing.
4. **the staging mechanism** (`realize_drv` tab-encoding, RECONCILED post-#97 code):
   disabled the td-store tab-encoding (`if let Some(ts) = Option::<&Path>::None`) so a
   td-built dep's closure entry stays BARE → gate reds: build-plan substitutes td's
   pcre2 into grep, but staging tries to bind it from `/gnu/store/3rf7a9w…-pcre2 (on
   disk /gnu/store/3rf7a9w…-pcre2): No such file or directory`, `make Error 1`,
   check.sh exit 2. Proves the `CANONICAL\tON-DISK` re-keying is load-bearing — without
   it td's pcre2 (not daemon-resident) cannot be staged and the chained build fails.

## Green evidence (gate `build-plan`, pcre2 → grep)

- built: pcre2=`…xzfr9x1f…-pcre2-10.42`  grep=`…0bjjvj1f…-grep-3.11` (post-#93/#94/#97
  rebase seed).
- [DURABLE structural] grep's .drv references td's pcre2 and NOT guix's
  (`…agdqk…-pcre2-10.42`).
- [DURABLE behavioral] td's grep runs 3.11 and a `grep -P` PCRE match works (td's
  pcre2 is loaded).
- [DURABLE repro] `td-builder check` double-build agrees the chained grep is
  reproducible (staging td's pcre2 from the on-disk path closure.txt records).
- [MIGRATION ORACLE] td's grep + pcre2 land at distinct paths from guix's.

## Status

Sub-tasks 1–5 done; all verified-red. Full `./check.sh` green (40 gates, PR #95).

Rebased onto origin/main after #93/#94/#97 landed: PR #97 introduced the
`CANONICAL\tON-DISK` closure encoding (`split_closure_entry`), which is exactly the
staging override sub-task 3 needed — so the `TD_STORE` env approach was dropped and
`realize_drv` now tab-encodes td-built deps instead, reusing #97's one mechanism.
`realize_drv`/`build_recipe` were reconciled to carry BOTH #97's `src_override` and
this track's multi-db `store_dbs` + `td_store`. cargo test 51 passed; re-verified red.

Rebased again after #98 + #100 landed: #98 added a `builder_override` (the
td-bootstrapped stage0 builder, brick 2) with its own closure special-case that read
each builder direct-ref's transitive closure from a singular `db`. Reconciled so
`realize_drv`/`build_recipe` carry ALL THREE overrides — `src_override`,
`builder_override`, `td_store` — and the default + builder-ref transitive lookups both
route through `closure_multi` (no stale singular `db`). #100 (the engine→full-loop
escalation, surfaced BY this track) now correctly forces this diff's landing to the
full `./check.sh`; ran it green (40 gates), incl. the bootstrap gate that exercises the
reconciled `builder_override` path.
