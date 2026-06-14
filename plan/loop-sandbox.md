# plan/loop-sandbox.md — td's sandbox hosts a loop step (replace `guix shell -C`)

Track: **loop-sandbox** (DESIGN §7.1 "Loop tooling convergence", gate-2 — human
go-ahead 2026-06-13: "then the gate-2 items (td-check oracle, loop sandbox)"). Claim:
claude-fable-4a2e33, 2026-06-13. Single writer. Stacked on the td-check branch (#29)
until that lands, then rebased onto main.

## Goal

`check.sh` enters a fresh `guix shell -C --pure` for every loop step (DESIGN §1.4) —
the hermetic container that exposes `/gnu/store` + the daemon socket + host-guix on
PATH while isolating the rest. The north star is ONE Rust sandbox stack spanning build
AND run; today td's sandbox (`builder/src/sandbox.rs`) only does the BUILD side (a
closure-staged build jail). This track grows it to host a loop step, equivalent to
`guix shell -C`.

Per the agreed approach (human, 2026-06-13): **additive equivalence rung FIRST** — do
NOT touch `check.sh`'s real entry yet. Prove td's sandbox can host a representative
loop operation with output byte-identical to running it under `guix shell -C`. The
wholesale `check.sh` swap is a LATER increment once equivalence is proven (gate-2
OBSERVE step, mirroring td-check).

## First increment (smallest honest)

`td-builder host-sandbox -- CMD...` (a DEV-SHELL mode, distinct from the build jail):
- `NEWUSER|NEWNS` (mount namespace), uid/gid mapped so the daemon still sees the real
  host uid (peer-cred trust preserved — the kernel translates the inner map);
- pivot into a fresh root exposing ONLY: `/gnu/store` (rbind, read-only — the WHOLE
  store, not a closure), `/var/guix` (rbind — the daemon socket + GC roots), `/proc`,
  a tmpfs `/tmp`, and the host-guix bin dir on PATH; the host filesystem is otherwise
  GONE (the isolation boundary);
- run CMD, inherit stdio, propagate the exit code.

Rung `loop-sandbox`:
1. **Exposure equivalence** — run `guix build -d <warm target>` (lowers to a `.drv`
   path; needs store + daemon socket + guix) inside td's host-sandbox AND under
   `guix shell -C` (same flags check.sh uses); assert the printed store path is
   byte-identical. Proves td's sandbox exposes the store + socket + guix the same way.
2. **Isolation** — a host-only path (e.g. this worktree's checkout, or `/etc/hostname`)
   is NOT visible inside td's sandbox (proves it is a real container, not a bare
   userns).

Scope (honest, deferred follow-ups, like the build sandbox deferred NEWPID/chroot to
S4): network-namespace + loopback parity (the `guix shell -C` netns) and the actual
`check.sh` entry swap are LATER increments. This rung runs INSIDE check.sh's existing
offline `guix shell -C`, so the outer container still owns the offline posture; this
increment proves only the store/socket/guix exposure + isolation in td's Rust sandbox.

## Differential / honesty

The rung asserts td-sandbox `guix build -d` output == `guix shell -C` `guix build -d`
output (same store path) — a genuine equivalence differential (td's container vs guix's
container), guix the oracle. Nothing in `check.sh` or any existing rung is changed
(directive 3); this ADDS a rung. The wholesale swap waits on this equivalence
(directive 4).

## Sub-task ladder

1. Claim + plan + DESIGN entry. — sub-task A.
2. `sandbox::host_shell` + `host-sandbox` subcommand (dev-shell: pivot_root + store/
   var-guix/proc/tmp binds + PATH). — sub-task B.
3. The `loop-sandbox` rung (exposure equivalence + isolation). Verify red: drop a bind
   (e.g. /var/guix) ⇒ guix can't reach the daemon ⇒ output diverges ⇒ rung red. — C.
4. Full `./check.sh` green; PR. — sub-task D.

## Verified-red log

(filled as each assertion is seen red.)
