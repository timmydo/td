# affected-checks-rs — port tools/affected-checks.sh → `td-builder affected-checks`

Handle: claude-opus-8ea90a — claimed 2026-06-28.

rust-migration plan layer **C1** (`plan/rust-migration.md`, "C. Scripts → Rust"):
`tools/affected-checks.sh` is the biggest single shell file (1284 LOC) and the
highest-leverage single rewrite. Precedent: the `affected-checks-engine` track
(#100) already moved the engine-escalation policy; this ports the whole dispatcher.

## What it is

`affected-checks.sh` maps a branch's changed paths to a right-sized check set and
decides whether the full `./check.sh` is waived or required. It is the local
PR-readiness gate (CLAUDE.md §"Diff-sized local check and waiver"). Surfaces:
`--run`, `--committed-only`, `--base REF`, `--path FILE`, `--self-test`, `--help`.

## Design

`builder/src/affected.rs` = a faithful port, exposed as `td-builder affected-checks`:

- **Pure mapping core** over a `root: &Path`: `map_path` mirrors the shell `case`
  arm-by-arm IN ORDER (first match wins), with a small shell-`case` glob matcher
  (`*` matches across `/`, `|` alternation). The gate-file parsing
  (`target_from_gate_file`, `*_SPECS :=`, `BUILD_GATES +=`),
  `default_check_covers_target`, `map_recipe_spec`, `target_for_build_spec` are
  ported as `mk/gates/*.mk` readers. Insertion-ordered dedup for
  preflights/targets/notes/full_required (the shell `contains_word`).
- **Renderer** reproduces the shell stdout byte-for-byte (headers, Changed paths,
  Selected checks, Waiver, Notes, Dry-run note).
- **CLI** (`--run`/default) shells out to `git` for the diff and to `./check.sh` +
  the preflights for execution, exactly like the shell.

The subcommand operates relative to CWD = repo root (the shell `cd $(dirname $0)/..`);
the lib takes an explicit `root` so tests are CWD-independent.

## Verification (directive 4 — own, then diverge)

- **Durable** (the real guard, runs on EVERY PR via the required `cargo-test` CI job
  + `check-engine` smoke): `run_self_test(root)` ported to native Rust `#[test]`s.
  It reads the real `mk/gates/*.mk` + `tests/` tree and asserts the same mappings /
  branch-mode policy the shell self-test asserts. No Guix, no shell — a property of
  the dispatcher's own policy.
- **Removable migration oracle**: a `#[test]` that, for a corpus of paths (every
  `mk/gates/*.mk`, every `tests/ts/recipe-*.ts`, + the self-test's asserted paths),
  diffs the Rust `--path P` render against `bash tools/affected-checks.sh --path P`
  byte-for-byte. Guarded to skip where bash/the script is unavailable (the loop
  sandbox's `guix shell` may not put bash on PATH); it DOES run in the required CI
  `cargo-test` job (plain ubuntu) on every PR. Deletable the day the shell goes.

## Verified-red

Before trusting green: (1) perturb one `map_path` arm (e.g. drop `add_target
rust-russh`) → the self-test reds on `tests/td-russh-demo.lock`; (2) perturb the
renderer (e.g. change the Waiver wording) → the differential oracle reds. Recorded
below once run.

## Scope / cutover

This PR lands the proven Rust port + tests ONLY — `builder/src/*` (+ this track's
plan files). It does NOT touch `tools/affected-checks.sh`: the shell stays the live
tool AND the differential oracle. The cutover (replace the script body with a thin
shim that execs `td-builder affected-checks`) is a deliberate follow-up — it makes
the dispatcher depend on a built `td-builder` binary (today it needs only
bash+git+sed), an ergonomic spine decision worth its own small PR.

affected-checks classifies this diff: `builder/src/*` → `check-engine` smoke,
full loop WAIVED (DESIGN §7.2). Landing check: `tools/affected-checks.sh
--committed-only --run`.
