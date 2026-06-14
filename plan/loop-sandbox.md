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

## Implementation progress

- **DONE 2026-06-13 (#30).** `td-builder host-sandbox` (pivot_root dev-shell) + the
  `loop-sandbox` rung GREEN inside the real `guix shell -C` (nested userns): `guix
  build -d hello` lowers to the SAME `.drv` (`zx4bn6wq…`) inside td's sandbox as under
  `guix shell -C`, and the host worktree is invisible while `/gnu/store` + the daemon
  socket stay exposed. Mechanism findings while building: the rootless uid map must be
  `0 → host_uid` (identity-mapping the host uid left userns-root-owned tmpfs dirs
  unwritable); `/dev` must be exposed (tools need `/dev/null`); coreutils are NOT on
  the sandbox PATH (only the guix bin dir), so probes use bash builtins. `sys.rs` gained
  `pivot_root`/`umount2` + `MS_RDONLY`/`MS_REMOUNT`/`MNT_DETACH`.
- **DONE 2026-06-14 (net-namespace parity).** `host_shell` now unshares `CLONE_NEWNET`
  and brings loopback up (`sys::bring_loopback_up` via `SIOCSIFFLAGS` — new socket/
  ioctl/close syscalls), matching `guix shell -C`'s offline posture; the daemon stays
  reachable across the netns (Unix socket on the bound `/var/guix`). The `loop-sandbox`
  rung gained a net-parity leg: td's sandbox `/proc/self/ns/net` inode DIFFERS from the
  rung's (a fresh netns even nested inside `guix shell -C`), loopback-only, and the
  exposure equivalence holds across it. Finding: `bring_loopback_up` needs an OWNED
  netns (CAP_NET_ADMIN) — without `NEWNET` it `EPERM`s on the host's `lo`, so the two
  are coupled. Remaining follow-up: the wholesale `check.sh` swap.
- **DONE 2026-06-14 (Step 1: the full-rung differential).** `host-sandbox` gained
  `--expose-cwd` — the FULL loop env: the worktree/cwd bound (rw, like `guix shell -C`'s
  shared cwd), `/sys/fs/cgroup` (ro) + the guix cache (`~/.cache/guix`), the caller's
  PATH (the toolchain — all `/gnu/store`), `TD_CHECK_*` + `USER`/`LOGNAME` preserved,
  chdir into the cwd. `host_shell` gained `workdir` + `extra_env` params. New `loop-rung`
  rung: the `eval` rung's exact command (`$(GUIX) repl $(LOAD) tests/eval.scm` — loads
  every system/test module, prints `eval ok`) produces BYTE-IDENTICAL combined output
  inside td's `--expose-cwd` sandbox as directly under `guix shell -C`. Findings:
  `USER`/`LOGNAME` must be preserved (else `guix time-machine` hits the root-owned
  `/var/guix/profiles/default` and `EPERM`s instead of the per-user profile); HOME needs
  no tmpfs in this mode (the cwd/cache binds create it on the writable root tmpfs).
  Step 2 (the actual `check.sh` edit) is NOT in this increment (human: "Step 1 only for
  now", 2026-06-14).

## Verified-red log

**R1 the daemon-socket exposure is load-bearing** (2026-06-13). Dropped the `/var/guix`
bind from the `host-sandbox` exposure set, rebuilt, ran the equivalence command:

    guix build: error: failed to connect to `/var/guix/daemon-socket/socket':
    No such file or directory   (exit 1)

so `td-builder host-sandbox -- guix build -d hello` produced an error instead of the
`.drv` path ⇒ `tdout != oracle` ⇒ the rung's exposure-equivalence leg goes red. Proves
the equivalence is genuine (td's sandbox really must expose the daemon socket; it is not
a vacuous pass). Reverted the bind; rung green again.

**R2 the net-namespace isolation is load-bearing** (2026-06-14). Dropped `CLONE_NEWNET`
(and, since lo-up then `EPERM`s on the host netns, the `bring_loopback_up` call) from
`host_shell`, rebuilt, ran the net-parity probe: td's sandbox `/proc/self/ns/net` came
back EQUAL to the rung's (`net:[4026531833]` == parent) ⇒ `test "$td_ns" != "$parent_ns"`
fails ⇒ the net-parity leg goes red ("did not enter its OWN netns"). Proves `NEWNET`
genuinely puts td's sandbox in a fresh isolated netns (not a vacuous pass), and that
lo-up is coupled to owning that netns. Reverted; rung green again.

**R3 the worktree exposure is load-bearing** (2026-06-14, `loop-rung`). Dropped the cwd
(worktree) bind from `--expose-cwd`, rebuilt, ran the eval differential: the sandbox's
workdir (the cwd) then does not exist inside, so `chdir` fails before exec —
`td-builder host-sandbox: spawning guix in host-sandbox: No such file or directory` —
so td's output is the error, not `eval ok`, ⇒ `td != oracle` ⇒ `loop-rung` red. Proves
the full-env worktree exposure is load-bearing (the rung genuinely runs IN the exposed
worktree, not a vacuous pass). Reverted; rung green again.
