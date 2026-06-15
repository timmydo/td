# plan/loop-sandbox.md — td's sandbox hosts a loop step (replace `guix shell -C`)

Track: **loop-sandbox** (DESIGN §7.1 "Loop tooling convergence", gate-2 — human
go-ahead 2026-06-13: "then the gate-2 items (td-check oracle, loop sandbox)"). Claim:
claude-fable-4a2e33, 2026-06-13. Single writer. Stacked on the td-check branch (#29)
until that lands, then rebased onto main.

## Goal

**STATUS (2026-06-14): ACHIEVED and then some — td's sandbox is now the SOLE loop
container; the `guix shell -C` fallback, the `TD_LOOP_GUIX_SHELL` toggle, and the
carve-out are all GONE (see the last "Implementation progress" entry + R7/R8). The
narrative below is the original additive-first plan, kept for history.**

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
- **DONE 2026-06-14 (Step 2: the check.sh swap — human go-ahead "let's do the second
  step … make the ships load bearing and meaningful").** `check.sh` now runs the loop
  inside td's OWN sandbox (`td-builder host-sandbox --expose-cwd -- make …`) BY DEFAULT
  instead of `guix shell -C`. `guix shell` (no `-C`) still PROVISIONS the toolchain
  profile (`--search-paths`); td replaces the CONTAINER, not guix's profile machinery.
  `TD_LOOP_GUIX_SHELL=1` keeps the original `guix shell -C` path as the oracle/fallback.
  Changes: `host_shell` switched to the IDENTITY uid map (matches `guix shell -C`'s
  non-root uid; tmpfs ownership via the new `uid=/gid=` mount data — `sys::mount` gained
  a `data` arg) and preserves `GUIX_BUILD_OPTIONS`/`GUIX_ENVIRONMENT`/`USER`/`LOGNAME`;
  the `loop-rung` rung prefers `$USER`. **Empirical result: the WHOLE loop runs under
  td's sandbox** — all the VM rungs (`test`/`boot-disk`/`place`/`build`), `run` (crun),
  the OCI rungs, every `td-*` rung — 36/37, full `./check.sh` green (38 PASS, 0 FAIL).
  **`rootless` is the one carve-out:** it builds in its OWN unprivileged userns and
  snapshots the LIVE store DB; nesting that inside td's sandbox (another unprivileged
  userns) double-nests and the sqlite WAL snapshot cannot coordinate with the host
  daemon from a nested non-root client (the `-shm` wal-index needs write access the
  nested client lacks; forcing the db dir RO then breaks the active-WAL case). So
  `check.sh` runs `rootless` in its native `guix shell -C` (NOT skipped — full
  assertions, a failure fails the whole check) via the new `check-sandbox` Makefile
  target (= `check` minus `rootless`); the canonical `check` is unchanged.
  **CI fallback:** the hosted runner's container restricts the namespace/mount ops td's
  sandbox needs — the outer `host-sandbox` fails there with "Operation not permitted"
  (the runner permits guix's own `guix shell -C` mechanism but not td's raw
  `pivot_root`+bind/tmpfs mounts + nested uid-map). So under `CI`/`GITHUB_ACTIONS`
  check.sh runs the proven `guix shell -C` path; td's sandbox stays the LOCAL default
  (the load-bearing entry agents run). Making td's sandbox run on the restricted runner
  is a follow-up (diagnose the runner's specific seccomp/userns restriction). Caught by
  the #33 CI `check` job, which then went green on the fallback.
- **DONE 2026-06-14 (td is the SOLE sandbox — human direction "make td the default,
  without a dependency on guix or a way to change it back, and implement the features
  necessary").** Removed the `guix shell -C` fallback, the `TD_LOOP_GUIX_SHELL` toggle,
  and the `check-sandbox` carve-out entirely. `check.sh` now ALWAYS runs
  `td-builder host-sandbox --expose-cwd -- make -j2 check` — every rung in td's sandbox,
  nothing else. The features that made it possible:
  - **PID-namespace keystone (`builder/src/sandbox.rs`/`sys.rs`).** `host_shell` lacked a
    PID namespace and bound the host `/proc`, so nested containers tripped over the
    host's root-owned PID 1 (`/proc/1/setgroups` EPERM) — the real reason `rootless`
    "couldn't nest" (the R6 "fundamental double-nested userns limit" was a misdiagnosis).
    `host_shell` now unshares `CLONE_NEWPID` with `NEWUSER`, forks so the command runs as
    PID 1 of a fresh PID ns, and mounts a private `/proc` reflecting it (host `/proc` bind
    dropped). Full `guix shell -C` parity (user/mount/pid/net/ipc/uts ns); minimal nested
    userns+pidns+mount-proc now works, and so does `rootless`.
  - **`rootless` runs LAST, alone (Makefile ordering).** Its sqlite `.backup` of the LIVE
    store DB (`tests/rootless.sh`) cannot read an ACTIVE WAL as the non-root client (the
    root-owned `-shm` is unwritable). That never bit before because `rootless` always ran
    in a separate phase, never overlapping the `-j2` heavy pool. Gating it order-only on
    every other heavy rung makes make schedule it after they finish → it snapshots a
    QUIESCENT DB. A scheduling constraint within td's sandbox, NOT a guix carve-out.
  - **`loop-sandbox`/`loop-rung` are now INTRINSIC self-tests** (no `guix shell -C`
    oracle — human's choice): they assert td's sandbox surface directly (store ro +
    daemon socket + guix via a real `guix build -d hello`; host isolation;
    PID-1/private-`/proc`; loopback-only netns; and the `--expose-cwd` full env runs a
    real `eval` rung). Equivalence to `guix shell -C` was proven over #30–#33; going
    forward td is self-described and the build rungs still differential-check against the
    guix daemon oracle. This also makes the old #40 oracle-contamination worry moot (no
    oracle to contaminate). **Directive 3:** the equivalence-vs-guix-shell-C differential
    is intentionally retired and replaced by self-tests — surfaced here and in the PR.
  - **CI** runs the unmodified td-sandbox `./check.sh` (the runner already lifts the
    unprivileged-userns restriction it needs; ci-gate "fix the host, never adapt the
    loop" policy).

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
`td-builder host-sandbox: spawning guix in host-sandbox: No such file or directory`,
exit non-zero ⇒ the rung's td-capture `|| FAIL` fires ⇒ `loop-rung` red. Proves the
full-env worktree exposure is load-bearing (the rung genuinely runs IN the exposed
worktree, not a vacuous pass). Reverted; rung green again.

Note: the differential compares the eval command's STDOUT (`eval ok`) + its exit
status, NOT combined stdout+stderr — the Guile auto-compile warnings ("imported module
(gnu) overrides core binding") on stderr are emitted only on a `.go`-cache MISS, so
under the `-j2` parallel loop a concurrent `guix repl` warms the cache between the
oracle and td runs and the warning SET diverges (caught: the first full `./check.sh`
went red on exactly that, while the standalone run was green). stdout is the
deterministic rung signal.

**Step 2 reds (the loop under td's sandbox — each surfaced a missing exposure, watched
red, fixed)** (2026-06-14):
- **R4 `GUIX_ENVIRONMENT` is load-bearing.** First full loop under td's sandbox: the
  `rootless` rung went red — `ERROR: GUIX_ENVIRONMENT is unset` (it binds that profile
  into its staged store). `guix shell -C` exports it; td's sandbox didn't. Fixed by
  computing it from the provisioned toolchain profile and preserving it in `host_shell`.
- **R5 the `loop-rung` `USER` corruption.** Nested under the swap, the rung's
  `user=$(id -un)` ran inside the outer sandbox where uid 0 has no `/etc/passwd` entry,
  so `id -un` printed `0` AND exited non-zero → `user="0\nnobody"` → the inner
  `guix time-machine` tried `mkdir /var/guix/profiles/per-user/0\nnobody` → red. Fixed by
  preferring the preserved `$USER` (`${USER:-…}`).
- **R6 `rootless` cannot nest.** Its sqlite store-DB WAL snapshot fails inside td's
  sandbox: with the `-shm` absent it tries to create it in the root-owned db dir
  ("readonly database"); binding the db dir RO instead breaks the active-WAL case
  ("unable to open database file"). Both seen red. This is a FUNDAMENTAL double-nested
  userns limit, not a missing exposure — resolved by the `check-sandbox` carve-out
  (rootless runs in its native `guix shell -C`, fully, never skipped).
  *(RETRACTED 2026-06-14 — see R7: it was NOT a fundamental limit. The real cause was
  the missing PID namespace / private `/proc`; with the keystone, rootless nests fine,
  and the residual failure was concurrency, not nesting — see R8.)*

**R7 the PID namespace + private `/proc` are load-bearing (the keystone)** (2026-06-14).
BEFORE adding `CLONE_NEWPID` + a private `/proc` to `host_shell`, inside td's sandbox
`/proc/1` was the HOST's root-owned shepherd (the sandbox shared the host PID ns and
bound host `/proc`), so a nested container's `/proc/1/setgroups` write got EPERM —
`guix shell -C` and the `rootless` daemon could not nest. AFTER the keystone, `/proc/1`
is the sandbox command (uid 1001), only the sandbox's own PIDs are visible, and minimal
nested userns+pidns+mount-proc works. The `loop-sandbox` rung's PID-1 assertion
(`/proc/1/comm` == the command, not the shepherd) goes red without it. This RETRACTS
R6's "fundamental double-nested userns limit" — it was the missing PID ns, not a kernel
limit.

**R8 `rootless` cannot snapshot a CONCURRENTLY-written store DB** (2026-06-14). With
`rootless` moved into the `-j2` heavy pool (no longer a separate phase), the first full
`./check.sh` went RED at the store-DB snapshot — `Error: attempt to write a readonly
database` (`tests/rootless.sh` sqlite `.backup`): a concurrently-building rung leaves an
active WAL whose root-owned `-shm` the non-root snapshot cannot write. DIRECTLY OBSERVED.
Fixed by gating `rootless` order-only on every other heavy rung so make runs it LAST,
alone, against a QUIESCENT DB. Proves the constraint is real (the carve-out had masked
it by never overlapping rootless with a build) and that the fix is a scheduling
constraint within td's sandbox, not a guix dependency.
