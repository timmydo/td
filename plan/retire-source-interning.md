# retire-source-interning — working notes

Handle: claude-fable-510345 — move-off-Guile §5.

## Goal / acceptance

Retire the two PURE tree-interning Guile helpers — `tests/td-builder-source.scm`
and `tests/td-vendor-demo-source.scm` — so the rust-build (gate 330) and
rust-vendor (gate 335) source PREP no longer runs `guix repl … lower-object
%builder-source` (the daemon interns the tree into `/gnu/store` + registers it in
`/var/guix/db/db.sqlite`). td interns the source ITSELF via its own recursive
addToStore (`td-builder store-add-recursive`, the gate-285 primitive), into td's
OWN store dir + td.db, with no daemon in the source path.

`boot.scm` is explicitly OUT of scope: it is config/image-layer lowering (OS defs →
derivation via `guix system image`), the "retired last" toolchain layer, not tree
interning.

## The wrinkle (why it is not a 3-file drop-in)

gate 285's `store-add-recursive` only owns the COMPUTE side: it restores the tree
into a scratch/td-owned store dir + a separate td.db and computes the identical
content-addressed path the daemon would — but it does NOT put the tree at
`/gnu/store/<base>` nor register it in the daemon DB. Meanwhile build-recipe's
CONSUME side hard-depends on the daemon store:
  - `realize_drv` computes the source's input closure from the store-db it is handed
    (gates pass `/var/guix/db/db.sqlite`), so the source must be a ValidPath there;
  - `sandbox::build` binds each closure item from its LITERAL `/gnu/store/<base>`
    location, which only exists if the daemon interned it.
So retiring the daemon interning means teaching build-recipe (and, transitively,
the `check` double-build) to stage a no-reference source from td's OWN store.

## Design

- `td-builder store-add-recursive NAME TREE STORE-DIR DB` → canonical `/gnu/store/…`
  path; restores tree at `STORE-DIR/<base>`; registers in `DB`. (Existing primitive.)
- `build-recipe RECIPE LOCK SCRATCH STORE-DB [SRC-STORE-DIR SRC-DB]` — new OPTIONAL
  trailing pair. When given, the `<name>-source` lock path is treated as a
  td-interned source: its on-disk location is `SRC-STORE-DIR/<base>`, its closure
  comes from `SRC-DB` (no-ref → itself).
- Per-closure-entry on-disk location: a closure entry may be `CANONICAL\tON-DISK`.
  `sandbox::build` binds from ON-DISK onto `newstore/<base from CANONICAL>` (→ shows
  at the canonical path inside the sandbox). No TAB → on-disk == canonical
  (backward compatible — every other gate's closure is unchanged). The encoding
  rides through `closure.txt`, so the separate `check` double-build honours it with
  no new arg.
- `build_and_register`'s reference `candidates` use the CANONICAL half only.
- Source-tree exclusion (`target`, `.cargo`, the `#:select?` guard) is replicated by
  `tests/intern-src.sh` (clean-copy excluding the named basenames, then
  store-add-recursive). `.gitignore` is KEPT (the `#:select?` only dropped
  target/.cargo).

## Sub-task ladder

1. [ ] Rust: `sandbox::build` honours `CANONICAL\tON-DISK` closure entries; unit test.
2. [ ] Rust: `build_and_register` candidates strip the on-disk half.
3. [ ] Rust: `realize_drv` gains optional source-override (closure from td.db,
       TAB-encoded closure.txt entry, on-disk staging).
4. [ ] Rust: `build-recipe` gains optional `SRC-STORE-DIR SRC-DB` (arity 6 or 8).
5. [ ] `tests/intern-src.sh` helper.
6. [ ] Gate 330 (rust-build): swap guix repl → intern-src.sh + build-recipe src args.
7. [ ] Gate 335 (rust-vendor): same.
8. [ ] Gate 345 (rust-russh): same (#94 landed a THIRD identical helper — caught in
       the sweep; retiring it too makes this a complete "retire ALL tree-interning"
       milestone rather than leaving a straggler).
9. [ ] Delete tests/td-builder-source.scm + td-vendor-demo-source.scm +
       td-russh-demo-source.scm. (gate 285 keeps its OWN inline %builder-source oracle;
       boot/image lowering at gates 152/172 is the retired-last config layer.)

## Landing protocol (post-rebase note)

Rebased onto origin/main `ac80c87` (affected-checks waives full loop for classified
diffs — new approval mechanism). Landing step 2 is now
`tools/affected-checks.sh --committed-only --run`, NOT a bare full `./check.sh`. For
this diff it ESCALATES to the full loop (records: "No mapping for tests/intern-src.sh"
— and the builder/src + sandbox.rs changes are broad, touching every sandboxed build),
so `--run` runs the selected checks AND the full `./check.sh`. Record the escalation +
full result in the PR body (PR #97, draft until green).

## Verified-red evidence

Plan: after the full run is GREEN (corpus cache warm), perturb the on-disk staging —
edit tests/intern-src.sh to drop the restored tree (`rm -rf "$store"/*` before the
echo) so the source's on_disk location is absent — clear `.td-build-cache/rust-build`
and re-run `./check.sh rust-build`. Expect RED: `closure item <canonical> (on disk
<srcstore>/<base>): No such file` from sandbox::build — proving the td store dir is
load-bearing (the source is staged from td's OWN store, not /gnu/store). Restore via
`git checkout tests/intern-src.sh` (all green edits are committed — safe). Then re-run
→ GREEN.

DONE — green + two reds:

- GREEN: `affected-checks.sh --committed-only --run` escalated to the full `./check.sh`
  (intern-src.sh unmapped + broad builder change) → EXIT=0. rust-build (self-host),
  rust-vendor (itoa+ryu), rust-uutils (139-crate cat), rust-russh (188-crate SSH
  round-trip `td-russh-ok: ping`, reproducible) all PASS with td interning, guix/Guile
  off PATH. (First attempt OOM-killed mid-ladder on a no-swap box under concurrent
  load; re-ran with TD_BUILD_JOBS=4.)
- RED #1 (gate guard): perturb intern-src.sh to drop the restored tree → gate errors
  `ERROR: td interned no source tree (store-add-recursive)`, EXIT=2. The gate's `-d`
  guard refuses to proceed without a real interned tree.
- RED #2 (staging — the load-bearing one): perturb build_recipe's on_disk to a bogus
  path (gate guard passes, tree present) → `closure item /gnu/store/…-td-builder-src
  (on disk /nonexistent-td-srcstore/…): No such file or directory`, EXIT=2. Proves
  sandbox::build stages the source FROM td's own store dir (on_disk), not /gnu/store —
  so the td store dir is genuinely load-bearing (rules out /gnu/store contamination).
  Reverted; cargo clean.
