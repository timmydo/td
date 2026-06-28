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

1. [ ] sys.rs: `prlimit64` → `set_rlimit`/`get_rlimit`; `mmap_anon` (for the
   behavioral test). Unit test: rlimit set/get round-trips.
2. [ ] sys.rs: behavioral — forked child with a small RLIMIT_DATA fails a large
   anon mmap; an uncapped child succeeds (the cap is load-bearing). VERIFY RED.
3. [ ] sandbox.rs: `parse_mem_max` + unit tests; cgroup leaf setup helper.
4. [ ] sandbox.rs: wire rlimit + cgroup join into `build`'s pre_exec; cleanup.
5. [ ] DESIGN.md: forward note on the global admission scheduler.
6. [ ] `./check.sh check-engine` green (cheap gates + cargo-test).

## Verified-red evidence

(fill in as each assertion is seen red)
