# auto-plan — derive the build-plan from the recipe graph

Handle: claude-opus-bc6cbf — claimed 2026-06-19.

## Goal

The build-plan chains (edge-owned 23/23, #107) are driven by a hand-written manifest
(`tests/td-chained-edges.txt`) + a gate that derives each subject's chained lock in
shell. This track moves plan GENERATION into td-builder: `build-plan --auto` reads the
recipe GRAPH (each recipe JSON's declared `inputs`) and derives the topo plan + the
`td-recipe-output` edge markings itself. A recipe's td-built edges then chain
automatically — as the owned set grows, no hand-written plan/manifest line is needed.

## Design

`td-builder build-plan --auto TARGET RECIPE-DIR LOCK-DIR GUIX-DB SCRATCH`:

1. An input is OWNED iff `RECIPE-DIR/<name>.json` AND `LOCK-DIR/<name>-no-guix.lock`
   both exist; otherwise it's an external seed (the toolchain, retired last).
2. Topo-sort TARGET's owned-input closure (post-order DFS over the recipe JSONs'
   `inputs` arrays; cycles error).
3. For each recipe in the order, derive a chained lock from its base lock by re-keying
   each owned-input dep to `D <path> td-recipe-output` (matched bare or hash-named;
   missing dep errors), and emit a `step <recipe.json> <chained.lock>` line.
4. Run the generated plan through the existing `build_plan`.

So `--auto bash` derives `ncurses -> readline -> bash` from `recipe-bash.ts`'s
`inputs: ["readline","ncurses"]` (+ readline's `["ncurses"]`) — the same DAG #107's
manifest enumerates, but derived.

## Scope

This PR: the `--auto` capability + a gate (367) proving it end-to-end on bash (the
deepest DAG). The manifest + the manifest-driven gate (365) + the census stay as-is
(they read the manifest for edge-owned crediting). Retiring the manifest — having 365
and the census derive from the recipe graph too — is a natural follow-up now that
`--auto` exists.

## Verified-red

- Unit: `auto_entry_is_dep` matches bare (`pcre2`) + hash-named (`<hash>-ncurses-…`)
  deps, rejects near-miss (`ncursesw`) + toolchain entries; `auto_chained_lock` marks
  only owned deps (bare-keyed + `td-recipe-output`), passes seeds/source through, and
  errors when a declared owned dep is absent from the lock.
- Gate 367: break the marking → bash builds with guix's ncurses → structural red
  (recorded with the gate's verified-red run).
