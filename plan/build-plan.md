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

`sandbox::build` stages each closure item from its literal `/gnu/store/<base>`
path. A td-built dep's files live under a scratch `newstore/`, not `/gnu/store`.
Add a **td-store dir** the staging prefers: if `<tdstore>/<base>` exists, bind
from there (td's build); else from the literal path (guix seed). build-plan
populates `<tdstore>` with each step's output.

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
3. td-store staging in sandbox::build (thread an optional alt store root).
4. `build-plan` subcommand wiring 2+3 + name→path substitution.
5. Gate `build-plan` (pcre2 → grep) + plan/typed-lock fixtures. VERIFY-RED
   (drop the substitution → guix's pcre2 reappears in grep.drv → gate reds).
6. Land per §7.2.

## Verified-red log

(to fill as sub-tasks land)

## Status

Claimed; design above. Implementation starting at sub-task 1.
