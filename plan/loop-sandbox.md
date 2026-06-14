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
- **DONE 2026-06-14 (Step 2: the check.sh swap — human go-ahead "let's do the second
  step … make the ships load bearing and meaningful").** `check.sh` now runs the loop
  inside td's OWN sandbox (`td-builder host-sandbox --expose-cwd -- make …`) BY DEFAULT
  instead of `guix shell -C`. `guix shell` (no `-C`) still PROVISIONS the toolchain
  profile (`--search-paths`); td replaces the CONTAINER, not guix's profile machinery.
  `TD_LOOP_GUIX_SHELL=1` keeps the original `guix shell -C` path as the oracle/fallback.
  Changes: `host_shell` switched to the IDENTITY uid map (matches `guix shell -C`'s
  non-root uid; tmpfs ownership via the new `uid=/gid=` mount data — `sys::mount` gained
  a `data` arg) and preserves `GUIX_BUILD_OPTIONS`/`GUIX_ENVIRONMENT`/`USER`/`LOGNAME`;
  the `loop-rung` rung prefers `$USER`. **Empirical result (Step 2 as first landed):
  most of the loop runs under td's sandbox** — all the VM rungs
  (`test`/`boot-disk`/`place`/`build`), `run` (crun), the OCI rungs, every `td-*` rung
  — full `./check.sh` green (0 FAIL). (Caveat surfaced in review, fixed by the
  critique-resolution entry below: two of those rungs — `loop-sandbox`/`loop-rung` —
  are EQUIVALENCE differentials whose oracle is `guix shell -C`; running them nested
  under td's sandbox silently degraded the differential to td-vs-nested-td. They are
  now in the `guix shell -C` carve-out.)
  **`rootless` is the one carve-out:** it builds in its OWN unprivileged userns and
  snapshots the LIVE store DB; nesting that inside td's sandbox (another unprivileged
  userns) double-nests and the sqlite WAL snapshot cannot coordinate with the host
  daemon from a nested non-root client (the `-shm` wal-index needs write access the
  nested client lacks; forcing the db dir RO then breaks the active-WAL case). So
  `check.sh` runs `rootless` in its native `guix shell -C` (NOT skipped — full
  assertions, a failure fails the whole check) via the new `check-sandbox` Makefile
  target (= `check` minus `rootless`); the canonical `check` *target* is unchanged —
  though ./check.sh's DEFAULT invocation no longer runs `make check` as ONE graph (it
  runs the carve-out in a separate container), a real test-topology change the closing
  section surfaces for knowing approval.
  **CI fallback:** the hosted runner's container restricts the namespace/mount ops td's
  sandbox needs — the outer `host-sandbox` fails there with "Operation not permitted"
  (the runner permits guix's own `guix shell -C` mechanism but not td's raw
  `pivot_root`+bind/tmpfs mounts + nested uid-map). So under `CI`/`GITHUB_ACTIONS`
  check.sh runs the proven `guix shell -C` path; td's sandbox stays the LOCAL default
  (the load-bearing entry agents run). Making td's sandbox run on the restricted runner
  is a follow-up (diagnose the runner's specific seccomp/userns restriction). Caught by
  the #33 CI `check` job, which then went green on the fallback.
- **DONE 2026-06-14 (critique resolution — claude-fable-dce88e).** A review of the
  landed Step-2 swap surfaced two real CODE regressions and several overclaims; this
  increment fixes the regressions and reconciles the claims (full reconciliation in
  "Goal vs. achieved" below). NO rung is dropped, loosened, or skipped — the canonical
  `check` target still lists every rung (directive 3); this only repartitions which
  container each runs in for the default ./check.sh path.
  - **Oracle integrity (a differential had silently gone vacuous).** Step 2 ran
    `check-sandbox` — which then still INCLUDED `loop-sandbox`/`loop-rung` — *inside*
    td's sandbox. But those two rungs are EQUIVALENCE differentials whose oracle is
    `guix shell -C` (directive 4): each runs a command in td's sandbox and compares it
    to the SAME command in the AMBIENT container. With the ambient container now td's
    sandbox, the differential degenerated to td-vs-nested-td and no longer proved
    equivalence to `guix shell -C` at all (only CI, forced onto `guix shell -C`, kept
    the real oracle). FIX: both rungs JOIN the `guix shell -C` carve-out (the new
    `check-guix-shell` Makefile target); `check-sandbox` is now `check` minus
    `SANDBOX_CARVEOUT` (= `rootless loop-sandbox loop-rung`). check.sh routes any direct
    invocation of these rungs to `guix shell -C` too (the `case` guard).
  - **Cheap-first restored.** Step 2 ran `rootless` as a SERIAL PREFIX *before* any
    rung, so a cheap structural red no longer aborted before that heavy build —
    contradicting the cheap-first fail-fast contract. ./check.sh now runs phase 1
    (`check-sandbox`, cheap-first) under td's sandbox FIRST, then phase 2 (the carve-out)
    under `guix shell -C`.

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

## Goal vs. achieved (north-star reconciliation, parity, and known gaps)

The Goal above frames the north star as ONE Rust sandbox stack **equivalent to** and
**replacing** `guix shell -C`. Stated plainly, what this track achieved is narrower —
a reader should not have to reconstruct it from the progress log:

**1. Replaced as the LOCAL default, for the td-sandbox-compatible rungs only.**
./check.sh runs most of the loop (every rung except the carve-out) under td's sandbox
locally. `guix shell -C` is NOT replaced — it stays load-bearing for:
(a) the carve-out rungs `rootless` (cannot nest) and `loop-sandbox`/`loop-rung` (their
differential oracle), on every local run; (b) **100% of CI**; (c) the
`TD_LOOP_GUIX_SHELL=1` fallback. So `guix shell -C` is now the minority container
locally and the *sole* container in CI — load-bearing, not retired.

**2. CI never exercises the new default — the single biggest weakness.** check.sh
forces `TD_LOOP_GUIX_SHELL=1` under CI, so the gate runs the old `guix shell -C` path
top-to-bottom. The host-sandbox MECHANISM is covered (the loop-sandbox/loop-rung rungs
exercise it nested, even in CI), but the OUTER swap wiring — toolchain provisioning,
env passing, the carve-out split, the two-phase orchestration — is invisible to the
gate. A regression there is GREEN in CI and only caught by an agent's local run. Given
the project creed ("no credit for code, only a passing reproducible test"), the
load-bearing local default currently rests on local runs ALONE. This is the **top
follow-up**, not a minor one: it needs a capable/self-hosted runner, or a diagnosis of
the runner's specific seccomp/userns restriction that makes `pivot_root` + bind/tmpfs
mounts + nested uid-map fail with "Operation not permitted".

**3. "Equivalent" was an overclaim — parity matrix.** td's `host_shell`
(`builder/src/sandbox.rs`) and `guix shell -C` are NOT yet hermetically equivalent:

| property        | guix shell -C              | td host-sandbox             | parity |
| ---             | ---                        | ---                         | :---:  |
| user namespace  | yes (identity uid map)     | yes (identity uid map)      | ✓ |
| mount namespace | yes                        | yes                         | ✓ |
| net namespace   | own, loopback-only         | own, loopback-only          | ✓ |
| IPC / UTS ns    | yes                        | yes (NEWIPC \| NEWUTS)      | ✓ |
| **PID namespace** | yes (the command is PID 1) | **NO — shares host PID ns** | ✗ |
| **/proc**       | private (own PID view)     | **host /proc, rbind rw**    | ✗ |
| **/dev**        | minimal / private          | **host /dev, rbind rw**     | ✗ |
| store + socket  | exposed                    | whole /gnu/store (ro) + socket | ✓ |
| host filesystem | hidden (shared cwd only)   | hidden (cwd only)           | ✓ |

A full green loop proves **behavioral sufficiency for the loop's current commands**
(the daemon does the actual building in its own sandbox; the loop steps are
orchestration that does not depend on PID isolation or a private /proc), NOT hermetic
equivalence. The PID-ns + private-/proc + private-/dev gaps are a tracked follow-up;
until they close, the `loop-sandbox`/`loop-rung` equivalence differentials — now run
against the REAL `guix shell -C` oracle (see the critique-resolution entry) — are the
standing guard that the surfaces the loop actually uses stay equal both ways.

**4. The default ./check.sh is a real test-topology change.** The Makefile `check`
*target* is unchanged and still lists every rung (directive 3 satisfied in substance —
no rung dropped or weakened). But the default ./check.sh invocation no longer runs
`make check` as one dependency graph in one container: it runs `check-sandbox` under
td's sandbox, then the carve-out under `guix shell -C` in a separate process. Which
container each rung runs in — and that `rootless`/`loop-sandbox`/`loop-rung` now run in
a separate serial phase — is the restructuring directive 3 says to surface for knowing
approval. It is surfaced here and in the PR, not slipped past review.

**5. Latency cost of the carve-out.** loop-latency cut the full check ~525s→275s by
overlapping heavy rungs two-at-a-time. The carve-out is a serial phase in a SECOND
container, no longer overlapped in the -j2 pool, so its wall time (rootless ~36s solo
per `plan/rootless-builder.md`, plus loop-sandbox/loop-rung paired under -j2) adds to
the critical path, and a second container is spun up. Running the carve-out
concurrently with phase 1 would restore the overlap but push a SINGLE check above the
-j2 / two-heavy ceiling (DESIGN §7.3: two concurrent checks already saturate at four
VMs), so it is sequential on purpose. Measured on this host (warm, all 40 rungs, both
green): the two-phase default ran **240s** vs **206s** for the single-container
`TD_LOOP_GUIX_SHELL=1` layout (every rung in one -j2 pool) — a **+34s (~16%)
serialization cost**, the price of running the carve-out as a separate sequential phase.
(A COLD first run is much longer — ~510s here — but that is dominated by the `--check`
double-builds before the verdict memo warms, NOT the carve-out; the warm-vs-warm delta
is the honest figure.)
