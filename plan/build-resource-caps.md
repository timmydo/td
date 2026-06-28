# build-resource-caps — working notes

Handle: claude-fable-2d249c. Branch: worktree-build-resource-caps.

## Goal

Per-build memory/resource caps so a single runaway build cannot OOM the host.
Opt-in, layered, reproducibility-safe (like the `nice` change #212): a cap can
only make a build *fail*, never change its output bytes.

## Architecture finding (why per-build, not a scheduler — yet)

- Every `td-builder build`/`realize` is an **independent OS process**. Loop
  concurrency comes from `make -j2` in the gate ladder and from multiple agents
  running checks (~4 concurrent builds). None flows through one arbiter.
- `build_daemon.rs` realizes drvs over a Unix socket, but its accept loop is
  **serial** (`for conn in listener.incoming()`) and it is **not** the only
  build path. It is persistence + a socket front-end, not a scheduler.
- No queue / semaphore / admission / memory-request bookkeeping exists.

So true k8s-lite scheduling (per-build memory *requests* + admission across all
in-flight builds) is a larger architectural change (single build path + a
per-drv request field + a capacity budget). This track ships the **per-build
cap** and records the scheduler as the wanted next step in DESIGN.

## Design

- `TD_BUILD_MEM_MAX` (bytes, suffixes K/M/G; unset/0/garbage ⇒ no cap, default
  OFF). Parsed like `TD_BUILD_NICE`.
- Applied in `sandbox.rs::build`'s `pre_exec`, scoped to the single derivation
  build (NOT the outer host-sandbox loop container).
- **rlimit backstop (always):** `setrlimit(RLIMIT_DATA, cap)` on the build
  child before it forks the PID-1 builder — inherited across fork+exec. Works
  rootless and in CI. RLIMIT_DATA (≥ Linux 4.7 counts private-anon mmap + heap)
  blocks the bulk of compiler memory without counting the huge *virtual*
  reservations Go/Rust make (so it false-trips far less than RLIMIT_AS).
- **cgroup RSS cap (best-effort):** when `TD_BUILD_CGROUP` names a writable,
  delegated cgroup2 dir, create a leaf, write `memory.max`, and join the build
  child to it *before* `unshare(NEWUSER)` (after that we lose host-cgroupfs
  perms). On any failure (RO cgroupfs in the loop sandbox, EBUSY, EPERM) warn
  once and fall through to the rlimit backstop. td does not try to create a
  cgroup from nothing in a rootless ns — it uses a delegated one, like a kubelet
  hands a container its cgroup.

## Sub-task ladder

1. [x] sys.rs: `prlimit64` → `set_rlimit`/`get_rlimit`; `mmap_anon` (via a new
   `syscall6`). Unit test `set_rlimit_data_round_trips`. (commit 1)
2. [x] sys.rs: behavioral `rlimit_data_caps_anon_mmap` — forked child capped at
   32 MiB RLIMIT_DATA fails a 256 MiB anon mmap; uncapped child succeeds.
   VERIFIED RED (below). (commit 1)
3. [x] sandbox.rs: `parse_mem_max` + unit tests; `setup_build_cgroup` helper. (commit 2)
4. [x] sandbox.rs: wire rlimit + cgroup join into `build`'s pre_exec; leaf
   cleanup after status. (commit 2)
5. [x] DESIGN.md §6: forward note on the global admission scheduler. (commit 2)
6. [ ] `./check.sh check-engine` green (cheap gates + cargo-test). affected-checks
   waives the full loop for this diff.

## Verified-red evidence

- **`rlimit_data_caps_anon_mmap` (the cap is load-bearing).** First seen red by
  a *real bug it caught*: the uncapped child also failed to map 256 MiB because
  `mmap_anon` used `syscall5`, leaving the offset register garbage → x86_64
  `sys_mmap` EINVALs a non-page-aligned offset even for an anon map. Fixed with
  `syscall6` (offset 0). Then verified red *for the cap specifically*: neutering
  the `set_rlimit` call made the "capped" child map 256 MiB successfully
  (`left: 0, right: 1` — "a child capped at 32 MiB RLIMIT_DATA must fail to map
  268435456 bytes"); restoring the call returns it to green.
- `parse_mem_max_*` and `set_rlimit_data_round_trips` are explicit-value
  assertions over pure/syscall mappings (a stub fails the Some/None cases).

## Scope boundary (stated honestly)

The cap is per-build (one runaway build can't OOM the host). It is NOT a global
scheduler — there is no admission/bin-packing across concurrent builds, because
no single arbiter sees all in-flight builds today (the build daemon is serial +
not the only path; `make -j2` and concurrent agent checks spawn builds directly).
That follow-on is parked in DESIGN §6 per the human's note.
