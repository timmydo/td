# plan/td-drv-add.md — wire td's `.drv` into the loop (daemon addTextToStore)

Track: **td-drv-add** (DESIGN §7.1, approved 2026-06-13 — §4.3 gate-1, human go-ahead
"start D"). Claim: claude-fable-4a2e33, 2026-06-13. Single writer.

## Goal

evaluator-as-library (#22) construct + verifies the `.drv`; td-drv-build (#25)
executes it. Both still get the `.drv` INTO the store via guile's `(derivation …)`.
This removes that: td-builder REGISTERS its constructed `.drv` itself, via the
guix-daemon worker-protocol `addTextToStore` RPC — a minimal Rust client
(`builder/src/daemon.rs`). No guile `(derivation …)`/`add-text-to-store`.

The daemon (C++) stays the store/build backend (retired later, own-builder-daemon
era). What's removed from the `.drv` path is the GUILE client. Input RESOLUTION (the
skeleton) stays Guix's, toolchain retired last (§5).

## Protocol (transcribed from `(guix store)`/`(guix serialization)` at the pin)

- ints: 8-byte little-endian; strings: int length + bytes + zero-pad to 8 (= the NAR
  string framing td already has).
- handshake: write magic1 `0x6e697863`, read magic2 `0x6478696f`, read daemon
  version (check major == `0x1`), write client version `0x163`, then (minor≥14)
  cpu-affinity=0, (minor≥11) reserve-space=0, then drain process-stderr to STDERR_LAST.
- `add-text-to-store` = op 8: write op, name (string), text (bytevector), references
  (string-list); drain stderr; read the result store path.
- process-stderr tags: NEXT (log), LAST (done), ERROR (msg+status); READ/WRITE not
  expected here.

## De-risk (2026-06-13) — PASSED before the rung

On the host daemon: `drv-add hello.drv` → the daemon returned td's own computed path
(== guix's). `store-add <unique> <file>` → the daemon WROTE a novel path, content
matched. `guix build` the registered `.drv` → `Hello, world!`.

## Rung (`td-drv-add`)

`drv-emit` (td constructs byte-identical, #22) → `drv-add` (daemon returns td's
computed path == guix's) → `store-add` a uniquely-named object (NOVEL write: the path
didn't exist, the daemon wrote td's bytes, read-back matches) → `guix build` the
td-registered `.drv` (output NAR-equal to the daemon's recorded hash). Heavy.

## Sub-task ladder

1. Charter + `daemon.rs` + `drv-add`/`store-add`. — DONE 2026-06-13.
2. The rung. Verify red: a protocol defect (wrong op/framing) reds drv-add; a construct
   defect reds drv-emit.
3. Full `./check.sh` green; PR.

## Implementation progress

(filled as it lands.)

## Verified-red log

(filled as each assertion is seen red.)
