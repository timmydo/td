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

1. **No `guix` process in user-facing commands / build paths.** First target:
   `td shell` (PR #119 shipped resolving via `guix build PKG` — rejected by the human
   as "a guix connection"). Rework: `td shell PKG -- CMD` resolves PKG to a **td-built**
   output (PKG's recipe → `td-builder build-recipe`/`build-plan --auto`,
   **build-on-demand, cached** under `.td-build-cache`), composes PATH from the td store
   path, execs — no `guix` process anywhere. Unknown package → error ("no td recipe for
   PKG"), NO guix fallback. Works for the owned corpus (hello, coreutils, bash, grep,
   nano, … — ~25 recipes today). Boundary while step 2 is pending: the *build* still
   links the guix-built toolchain seed from the pinned lock (no guix *process*, but
   guix-built bytes) — closed by step 2.
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

## This PR (direction only)

- CLAUDE.md "North star" section (the contract statement).
- DESIGN §5 reframe: "Seed toolchain — a frozen binary tarball, NOT a live guix
  dependency" replaces the "full-source bootstrap is a non-goal" bullet; Rust-toolchain
  bullet points at the seed tarball.
- PLAN.md preamble north-star callout.
- This track claim + roadmap.

No code/gates yet — the ladder lands as follow-on PRs, smallest first (step 1: `td shell`).

## Status

- 2026-06-20: direction PR (this). Next: implement step 1 (`td shell` on td-built
  packages, build-on-demand cached).
