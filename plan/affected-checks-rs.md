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

## Verified-red (recorded 2026-06-28)

- **A (mapping)**: dropped `require_full` from the catch-all arm → BOTH
  `self_test_passes_against_repo` AND `matches_shell_oracle_byte_for_byte` FAILED;
  the diff showed SHELL "full ./check.sh would be required" + bullet vs perturbed
  RUST "would be waived" for `new/unmapped.file` / `totally/unmapped/path.xyz`.
  Restored → green.
- **B (renderer)**: changed `Changed paths:` → `Changed paths::` (one char) →
  `matches_shell_oracle_byte_for_byte` FAILED on **227** corpus paths while
  `self_test_passes_against_repo` stayed GREEN — demonstrating the differential
  oracle catches byte-level formatting the substring self-test cannot. Restored →
  green.

Green: `cargo test --frozen` 89 passed (4 affected legs incl. the live-shell
differential, which RAN — not skipped). Binary smoke vs the shell oracle:
`--self-test` PASS; `--path`, bare branch dry-run, and `--committed-only` all
byte-IDENTICAL; error/`--help` exit codes (2/2/2/0) match.

## Cutover — DELETE the shell, in this PR (human, 2026-06-28)

The human chose to **delete `tools/affected-checks.sh` entirely** in this PR
(not a thin shim) and rewire callers/docs to invoke `td-builder affected-checks`
directly; `td-builder` is resolved **prebuilt → cargo build** ($TD_BUILDER →
`builder/target/{release,debug}` → PATH → `cargo build`), keeping the dispatcher
guix-free (North-Star aligned).

Consequences handled:
- The 1284-line shell file is removed (`git rm`).
- The only programmatic caller — the `affected-self-test` preflight — now runs the
  self-test **in-process** (this binary IS the dispatcher; no re-resolution). Its
  rendered command string becomes `td-builder affected-checks --self-test`.
- Docs rewired: `CLAUDE.md` (§"Diff-sized local check and waiver" + the conventions
  entry + the landing step), `DESIGN.md`, `.github/BRANCH-PROTECTION.md`. Historical
  `plan/*.md` notes are left as-is (they record past work).
- The removable shell **differential** test is deleted with its oracle (directive 4
  — the byte-identity leg is the migration oracle, retired with the cutover). It is
  replaced by `renders_exact_output_for_static_paths` — a frozen full-render
  byte-equality `#[test]` over paths whose mapping is repo-file-INDEPENDENT (so it is
  deterministic and runs even in the builder-only package sandbox). The dynamic
  mapping stays guarded by `self_test_passes_against_repo`.
- `repo_tree_present` markers changed to `mk/gates` + `check.sh` (the deleted script
  can no longer be the presence marker, else the self-test would skip vacuously).

affected-checks classifies this diff (the deletion + engine + docs): `builder/src/*`
→ `check-engine` smoke, full loop WAIVED (DESIGN §7.2). Landing check:
`td-builder affected-checks --committed-only --run`.
