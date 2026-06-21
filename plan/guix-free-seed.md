# guix-free-seed — remove guix entirely (frozen seed-binary tarball)

Handle: claude-fable-db65ca · branch: guix-free-seed

## North star (human, 2026-06-20)

Remove guix **entirely** — no guix *process* and no guix *install* dependency. The
mechanism is a **frozen seed-binary tarball**, NOT a Mes-style full-source bootstrap.
The first toolchain (gcc/glibc/binutils + the few build tools td can't yet self-build)
is captured **once** into a pinned, content-addressed tarball — generated *from* a
guix install but, once captured, td depends on the **tarball**, never on a live guix.
A source re-derivation of the seed is optional/later, never a blocker.

This supersedes the old "guix is the permanent seed, retired last / full-source
bootstrap is a non-goal" framing (DESIGN §5).

## Priority ladder

1. **No `guix` process in user-facing commands / build paths.** ✅ DONE (this PR, step 1):
   `td shell` (PR #119 shipped resolving via `guix build PKG` — rejected by the human as
   "a guix connection") now resolves PKG to a **td-built** output — PKG's recipe → emit
   JSON with td's tsgo + td-ts-eval → `td-builder build-recipe` (content-addressed cache
   ⇒ **build-on-demand, cached**) — composes PATH from the td store path, execs. No
   `guix` process anywhere; the `td-shell` gate proves it by running with guix/Guile
   SCRUBBED FROM PATH. Unknown package → error ("no td recipe for PKG"), NO guix fallback.
   The hello it runs is td's own build at a td store path distinct from guix's. Leaf
   recipes today (hello); chained recipes (bash<-readline<-ncurses, needing
   `build-plan --auto` + runtime LD_LIBRARY_PATH for td deps) are the step-1b follow-on.
   Boundary while step 2 is pending: the *build* still links the guix-built toolchain seed
   from the pinned lock (no guix *process*, but guix-built bytes) — closed by step 2.
2. **Serve the toolchain seed from the tarball, not a host guix.** Capture the seed
   closure (the lock's toolchain inputs: gcc/glibc/binutils/coreutils/… + stage0 inputs)
   into a pinned tarball + a manifest (name → store path). A `td seed` step unpacks it
   into the store (td's own `store-add-recursive`/registration, no daemon). The locks
   and `load_stage0`/cache-lib resolve seed paths from the tarball manifest, so the loop
   builds with **no guix install present**. Regenerate the tarball deliberately (a
   `tools/build-seed-tarball.sh`, run on a guix host) — like a channel bump; commit its
   content hash (DIGESTS-style), never rebuild it in the loop.
3. **Retire the loop's guix oracle/lowering LAST.** `guix build --check` (repro oracle —
   td-builder check already double-builds), `guix repl`/`guix system` (Guile config
   lowering). These are the migration scaffolding; delete the differential legs when the
   oracle goes (per CLAUDE.md "Differential + durable discipline").

## This PR (direction + step 1)

- CLAUDE.md "North star" section (the contract statement).
- DESIGN §5 reframe: "Seed toolchain — a frozen binary tarball, NOT a live guix
  dependency" replaces the "full-source bootstrap is a non-goal" bullet; Rust-toolchain
  bullet points at the seed tarball.
- PLAN.md preamble north-star callout.
- This track claim + roadmap.
- **Step 1 implemented**: `td-builder shell` reworked to build & run td's OWN package
  with no guix process (`run_shell`/`emit_recipe_json` in `builder/src/main.rs`); the
  `td-shell` gate (`tests/td-shell.sh`, BUILD_GATES + HEAVY_GATES) asserts it guix-free.

### Verified-red (step 1)
- VR1 — reintroduced a `guix build` call in `run_shell` → with guix scrubbed from PATH
  the behavioral leg reds (proves the gate catches any guix process). Reverted.
- VR3 — made an unknown package silently fall through instead of erroring → the
  load-bearing leg reds ("SUCCEEDED — must error, no guix fallback"). Reverted.

## Status

- 2026-06-20: direction + step 1 (this PR). `td shell` builds/runs td packages,
  guix-free (leaf recipes; hello green + verified-red).
- Next: step 1b (chained recipes via `build-plan --auto` + runtime dep PATHs), then
  step 2 (the frozen seed tarball — `tools/build-seed-tarball.sh` + `td seed`).
