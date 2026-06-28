# Track: loop-runtime (side-track) — check.sh / loop latency, round 2

Successor to `loop-latency` (DONE 2026-06-10, full check 525s→275s, +reset gate).
The corpus has grown from ~18 rungs to **101 heavy gates** since then, so the
hand-run numbers in `plan/loop-latency.md` and the `mk/gates/<NNN>` LPT order are
stale. This track re-attacks loop runtime with DATA.

Handle: claude-fable-2fe799. Spine files (`check.sh`, `Makefile`) = exclusive
landings, small standalone PRs (CLAUDE.md §7.3).

## Sub-tasks

- [x] **L1 — Per-gate wall-clock instrumentation (FIRST; it prioritizes the rest).** DONE.
- [ ] L2 — Re-measure & re-sort heavy gates (LPT). BLOCKED on data: needs a full
      `./check.sh` run's `latest.txt` (101 heavy gates). L1 now produces it; a cold
      full run is ~10h and the host is busy, so re-sort from the next daily-suite
      artifact (or a warm-machine run) — NOT from the stale hand numbers.
- [x] L3 — Parallelize + bound the host warm prelude in check.sh. DONE.
- [ ] L4 — Kill the duplicate qcow2 build in `boot-disk-native`. ANALYZED, deferred:
      `%native-disk-image` (tests/boot.scm) is `%test-os` = the shipped `td-system`
      + a `tester` user + a `td-test-privkey` activation service + authorized-keys,
      a DISTINCT operating-system → a distinct full qcow2 image-assembly derivation.
      Killing it = re-architecting test-image provisioning (boot the SHIPPED qcow2
      under a qemu CoW overlay and inject the test user/key OFFLINE via qemu-nbd/
      libguestfs, or via a fw_cfg/config-drive the guest reads at boot). Needs real
      VM validation + a warm cache + VM budget — a focused standalone session, not
      safe to land from a busy/cold host. SYSTEM_GATES tier (not default `check`).
- [ ] L5 — Extend `--check` verdict memo reuse. PROPOSE-ONLY: per check-memo
      constraint 2, enabling verdict reuse in CI/the daily runner RE-OPENS gate 2
      and requires EXPLICIT human sign-off recorded in plan/check-memo.md FIRST
      (it loosens when a required reproducibility assertion runs — directive 3).
      Cannot self-authorize. Note: the heavy `--check` legs are no longer a per-PR
      CI gate anyway (CI = lint + check-fast); the realistic target is the
      persistent DAILY full-suite runner, keyed by (runner image, imported store
      image digest, channel pin). Deliverable = a scoped proposal for the human.
- [ ] L6 — (theme 2 / S3) default-fetch the rust corpus, not just the toolchain.
      OUT OF SCOPE for this track until theme 2 lands.

## L3 — DONE

check.sh: every host warm runs under `timeout` (TD_WARM_TIMEOUT, default 600s,
graceful fallback when coreutils `timeout` is absent), and the ~10 INDEPENDENT
cargo-proxy warms fan out in batches of TD_WARM_JOBS (default 4). The two
shared-feed warms (bootstrap-sources, td-fetch-crates) stay serial-first (shared
td-feed daemon + store). warm-tsgo keeps its FATAL — a timeout there fails fast,
which is the point (the named fast-tier hang risk). Validated: check-fast green
(exercises the warm-tsgo timeout wrapper); fan-out logic verified in isolation
(concurrent batches; a 5s hang KILLED at a 2s timeout; multi-arg specs
word-split); verified-red — with the timeout disabled the 5s hang completes.

## Landing status

L1 (Makefile + tools) and L3 (check.sh) are both LOOP-SPINE changes →
exclusive landing. `tools/affected-checks.sh --committed-only` ESCALATES (Makefile
spine) → a full `./check.sh` is required before the PR is marked ready. check-fast
(the CI-required gate) is GREEN in the real sandbox with both changes; the full
run is pending a warm machine / free VM budget (the daily suite backstops). PR
opened as a DRAFT until the full run is recorded.

## L1 — DONE (implemented + validated)

Mechanism: each TIMED gate target (`$(CHEAP_GATES) $(HEAVY_GATES) $(SYSTEM_GATES)
$(ENGINE_GATES) build-recipes`) gets a target-specific `.SHELLFLAGS` override
pointing make at `tools/gate-time.sh`; SHELL stays plain bash. make then runs
each recipe line as `bash tools/gate-time.sh <gate> -c '<recipe>'`. A non-default
`.SHELLFLAGS` also disables make's direct-exec fast path, so EVERY recipe line is
wrapped. The wrapper logs `<gate>\tSTART|END\t<epoch-ns>` to a per-run log under
`.td-build-cache/gate-timing/`; `tools/gate-timing-report.sh` reduces each gate's
min(START)/max(END) to a wall-clock span (correct across multi-line recipes and
parallel `-j` gates), sorted longest-first, tagged cheap/heavy, written to
`latest.txt`. `check`/`check-system` print it at end-of-run; `make
gate-timing-report` re-prints the newest run (e.g. after a red gate).

Design choices:
- `SHELL=bash` + `.SHELLFLAGS` override (NOT `SHELL=<script>`): the loop sandbox
  does not guarantee an absolute `/bin/sh`, so the wrapper must be invoked via
  `bash <file>`, not a shebang. Scoped to gate targets only — `check`/helper/
  report targets keep the default shell (verified: an `untimed` target is not
  wrapped).
- Integer nanosecond timestamps (`date +%s%N`) so all reduction is integer
  arithmetic — the sandbox has no awk (and no diffutils).
- FAIL-SAFE: every log step is best-effort; the wrapper always runs the real
  recipe under bash and returns its true exit code (this wraps the loop spine).
- Opt-out: `TD_GATE_TIMING=0` disables the `.SHELLFLAGS` override entirely; the
  override also only engages when `tools/gate-time.sh` exists (`$(wildcard)`).
- Dependency posture (no NEW dependency): `gate-time.sh`'s own body is POSIX; it
  runs each recipe via the SAME `bash` the Makefile's `SHELL := bash` already used
  (recipes are bash — `set -euo pipefail`), so it adds nothing. `gate-timing-report.sh`
  is pure POSIX sh (no bash, no awk — a sort-by-gate-then-time single pass instead
  of associative arrays; newest-log via command substitution, not process
  substitution), invoked via `sh`. bash is required only where the gate recipes
  already require it.

Verified-red / safety evidence (2026-06-28):
- Exit-code transparency: `gate-time.sh failgate -c 'exit 7'` returns 7 (and
  still logs START+END — timing survives a red gate); `-c 'true'` returns 0. The
  catastrophic failure mode (wrapper swallowing a gate's failure) is excluded.
- Non-vacuity: `make plan-index TD_GATE_TIMING=0` writes NO log; with timing on,
  `make plan-index` logs events and the report shows `plan-index cheap 0.072`.
- Mechanism scoping + parallel correctness verified in throwaway Makefiles
  (untimed target not wrapped; min/max spans match injected sleeps under `-j2`).

Files: `tools/gate-time.sh`, `tools/gate-timing-report.sh`, `Makefile`
(timing vars + `.SHELLFLAGS` wiring + report in `check`/`check-system` +
`gate-timing-report` target). Artifact dir is under the gitignored
`.td-build-cache/`.

Next: run a full `./check.sh` (and `./check.sh check-system`) on the dev machine
to capture real per-gate numbers, then L2 renumbers the heavy fragments from the
`latest.txt` table.
