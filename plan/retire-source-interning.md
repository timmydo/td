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
8. [ ] Delete tests/td-builder-source.scm + td-vendor-demo-source.scm; fix the lock
       comment references.

## Verified-red evidence

(to fill — perturb the on-disk staging / src-db closure and watch 330/335 go red)
