# plan/td-drv-assemble.md — remove the last guile `(derivation …)` from the build path

Track: **td-drv-assemble** (DESIGN §7.1, approved 2026-06-13 — §4.3 gate-1, human
go-ahead "continue with … the skeleton without (derivation …)"). Claim:
claude-fable-4a2e33, 2026-06-13. Single writer.

## Goal

After td-drv-add (#27), td constructs/registers/executes the `.drv` in Rust, but the
SKELETON it reads is still produced by guile's `(derivation …)`. This removes that:
guile only RESOLVES the inputs (toolchain + source → store paths — input resolution,
retired last §5) and emits a raw SPEC; td-builder ASSEMBLES the `.drv` — the ordering
`(derivation …)` imposes (add `out`, sort env by key, inputs by path) + the output-path
computation (#22 construct_drv) + registration (#27) — byte-identical to guix's
`(derivation …)`. So nothing guile constructs the build derivation anymore.

## How

- `system/td-build.scm`: factored `td-build-components` (the shared input resolution);
  `td-rust-build-derivation` (the `(derivation …)` ORACLE) and `write-td-build-spec`
  (emits the raw spec, no `(derivation …)`) both call it.
- Spec format (line-based, parsed by the zero-dep Rust): `name`, `system`, `builder`,
  `arg`, `input-drv <path> <out,…>`, `input-src <path>`, `env k=v`. NO output paths, NO
  `out` env var.
- `builder/src/store.rs` `assemble_drv`: parse the spec → add the `out` output + its
  env var → SORT env by key, inputs/sources by path (the daemon's canonical order) →
  `construct_drv` (output path + serialize). `drv-assemble SPEC` subcommand registers
  it via the daemon (#27) and prints the path.

## De-risk (2026-06-13) — PASSED before the rung

Emit spec → `drv-assemble` → daemon returned `nh886097…-hello-2.12.2.drv` == guix's
`(derivation …)` path. Byte-identical on the first try (the sort matches guix).

## Differential / honesty

The rung asserts td's assembled+registered path == guix's `(derivation …)` path. The
daemon content-addresses td's SENT bytes, and the oracle is guix's `(derivation …)`
path — so equal paths ⇒ td's assembled CONTENT == guix's `(derivation …)` content
(byte-identical). This is a genuine assembly differential (td's Rust vs guix's
`(derivation …)`), NOT idempotency: td assembles from the raw spec independently. The
guile `(derivation …)` remains only as the differential ORACLE; input resolution stays
Guix's (the toolchain, retired last §5).

## Sub-task ladder

1. Charter + `td-build-components`/`write-td-build-spec` + `assemble_drv`/`drv-assemble`.
   — DONE 2026-06-13.
2. The rung. Verify red: an ordering/construct defect makes td's `.drv` != the oracle.
3. Full `./check.sh` green; PR.

## Implementation progress

- **DONE 2026-06-13.** `td-drv-assemble` rung GREEN in-sandbox: guile emits the 24-line
  spec without `(derivation …)`, td assembles byte-identical to guix's `(derivation …)`
  (`nh886097…`), registers via the daemon, `guix build` runs `Hello, world!`. The
  existing rungs that call `td-rust-build-derivation` are unaffected (the refactor
  keeps its result identical).

## Verified-red log

(filled as each assertion is seen red.)
