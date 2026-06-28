# Track: loop-runtime (side-track) — check.sh / loop latency, round 2

Successor to `loop-latency` (DONE 2026-06-10, full check 525s→275s, +reset gate).
The corpus has grown from ~18 rungs to **101 heavy gates** since then, so the
hand-run numbers in `plan/loop-latency.md` and the `mk/gates/<NNN>` LPT order are
stale. This track re-attacks loop runtime with DATA.

Handle: claude-fable-2fe799. Spine files (`check.sh`, `Makefile`) = exclusive
landings, small standalone PRs (CLAUDE.md §7.3).

## Sub-tasks

- [x] **L1 — Per-gate wall-clock instrumentation (FIRST; it prioritizes the rest).**
- [ ] L2 — Re-measure & re-sort heavy gates (LPT). Needs L1 data from a full run.
- [ ] L3 — Parallelize + bound the host warm prelude in check.sh (fan out the 10
      serial warm-cargo-proxy calls; add per-fetch timeouts — fast-tier hang risk
      flagged in-file).
- [ ] L4 — Kill the duplicate qcow2 build in `boot-disk` (a second instrumented
      image on top of the shipped one — the biggest marionette cost).
- [ ] L5 — Extend `--check` verdict memo reuse (check-memo CI reuse is OFF;
      scope where it is safe to re-open). Loop spine — careful, exclusive.
- [ ] L6 — (theme 2 / S3) default-fetch the rust corpus, not just the toolchain.
      OUT OF SCOPE for this track until theme 2 lands.

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
