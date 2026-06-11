# Track: loop-latency (side-track)

**Status:** DONE — landed 2026-06-10. Acceptance met: see "Results" at the bottom.
(Claim status lives in `PLAN.md`, the single source of truth for claims.)
**Origin:** DESIGN §1.3 (loop latency is a tracked metric) and §1.5 (the named
upgrade path: qcow2 overlay / CoW reset).
**Scope authority:** DESIGN §7.1.

## Goal

Cut write→check cycle time. First candidate: replace fresh-image-per-test VM resets
with QEMU qcow2 overlays (CoW), keeping the guarantee that every test still sees
fresh state.

## Acceptance

Measured wall-clock improvement on the marionette rungs (record before/after numbers
here), with the FULL loop still green and ephemerality intact: a test that dirties
guest state followed by a reset must show the state gone (verified-red: disable the
reset and watch that assertion fail).

## Constraints

- Never trade away test isolation for speed — the state boundary (CLAUDE.md prime
  directive 6) outranks the latency budget.
- Touches `check.sh`/`Makefile`/test harness: shared spine, standalone commits
  (DESIGN §7.3).

## Working state

### Where the VM/reset cost actually lives today (read of the harness, 2026-06-10)

- `%test-td-boot` (rung `test`) uses `(virtual-machine os)`: direct-kernel boot,
  store shared into the guest, volatile root — there is NO per-test image copy to
  optimize; per-run cost is the marionette boot + assertions.
- `%test-td-disk-boot` (rung `boot-disk`) already boots its qcow2 with QEMU
  `-snapshot` — a CoW overlay, run state discarded. The fresh-image cost is NOT the
  reset: it is that the rung builds a SECOND full qcow2 (the marionette-instrumented
  td-system) in addition to the shipped image the `build` rung makes.
- The `--check` rungs (`build oci manifest-check generation-image place no-guix
  container`) rebuild their artifacts from scratch every run BY DESIGN (the
  reproducibility oracle, prime directive 1) — that floor is not addressable by this
  track without weakening the loop (human gate; not pursued).
- Marionette test derivations are content-addressed: on an unchanged tree they are
  cache hits (~0s). The honest write→check metric is therefore the cost AFTER a
  representative one-line change that invalidates the test derivation.

### Sub-task ladder

1. **Baseline (in progress).** Time every rung individually on the warm store
   (run A: unchanged tree = the steady-state floor, dominated by --check rebuilds);
   then invalidate only the marionette test gexp with a no-op builder change and
   time the marionette rungs (run B: the per-change marionette cost the track is
   chartered to cut). Record numbers + host conditions here.
2. Ephemerality assertion: a test that dirties guest state, resets, asserts the
   dirt is gone; verified-red by disabling the reset.
3. Attack the biggest measured cost on the marionette path (candidate from the
   read above: the duplicate full-image build in `boot-disk`, and any
   image-provisioning cost not already CoW), keeping every rung's assertions
   intact or stronger.
4. Re-measure under comparable load; record before/after; land per §7.2 (spine
   files = standalone commits, §7.3).

### Measurement log

- Host conditions for run A: load avg 0.42 at start (1.05 when the script began),
  2.08 at end; two unrelated QEMU VMs up (M10.3 agent presumed) — within the
  DESIGN §7.3 two-concurrent-checks budget, but noted as noise context. Timings to
  ±10s fidelity are what we need.

**Run A — warm store, UNCHANGED tree (the steady-state floor), 2026-06-10, all PASS.**
Per-rung wall-clock via `./check.sh <rung>` (each includes ~2-3s `guix shell`+
time-machine overhead):

| rung | s | | rung | s |
|---|---|---|---|---|
| eval | 2 | | oci | 82 |
| diff | 3 | | manifest-check | 86 |
| typed-coverage | 3 | | generation-image | 173 |
| oci-diff | 3 | | place | 22 |
| manifest-diff | 2 | | no-guix | 89 |
| generation-diff | 3 | | run | 5 |
| build | 13 | | container | 40 |
| test | 4 (cache hit) | | | |
| boot-disk | 4 (cache hit) | | **total** | **~534 (8m54s)** |

Findings from run A:

- The unchanged-tree floor is ~9 min, ~80% of it (430s) in the four OCI-tarball
  `--check` rungs (oci, manifest-check, generation-image, no-guix) — the
  reproducibility oracle re-realises those derivations every run by design.
  NOT addressable by this track without weakening the loop (human gate; not
  pursued).
- Marionette test derivations are cache hits on an unchanged tree (4s), so the
  marionette cost only bites on a change → run B measures that.
- `build` (qcow2 + --check) is only 13s warm: the image derivation rebuild is
  cheap; the early-rung comments ("heavier") predate the warm-store reality.

**Run B — warm store, marionette test builders invalidated by a no-op datum
(bare string literal after each `test-begin` — a comment would NOT change the
gexp), images all cached. 2026-06-10, all PASS:**

| rung | run B | run A (cached) | net marionette cost |
|---|---|---|---|
| test | 35s | 4s | ~31s (VM boot + ssh asserts) |
| boot-disk | 14s | 4s | ~10s (GRUB boot + uname) |
| container | 71s | 40s | ~31s (VM boot + crun asserts) |
| **total** | **120s** | | |

Findings from run B + host probes:

- **There is no per-test image copy to eliminate.** `test`/`container` use
  `(virtual-machine os)` (shared store, volatile root); `boot-disk` boots with
  QEMU `-snapshot` (CoW overlay). The §1.5-named candidate (CoW reset) is
  already structurally in place inside the (gnu tests) framework; the reset is
  already ~free. The win must come from the charter's "other cycle-time wins".
- The marionette cost on a test-edit cycle is the three SERIAL VM boots: 120s
  wall, ~71s if two run concurrently.
- Host: 16 cores, /dev/kvm present (boots are KVM-fast already). guix-daemon
  runs without --max-jobs (per-session default); concurrent client builds can
  overlap, so make-level parallelism is not daemon-serialized (to be verified
  empirically before relying on it).
- Not yet measured: run C (a system-declaration edit, which additionally
  rebuilds the images). Do before/after comparison on that cycle too when the
  speed change exists.

### Design (decided 2026-06-10)

1. **Ephemerality rung (sub-task 2, acceptance-required, strengthening = free).**
   New marionette test using an EXPLICIT qcow2 overlay over the (cached)
   instrumented disk image (same derivation as `boot-disk`):
   boot 1 on overlay A writes dirt + asserts present; boot 2 on the SAME overlay
   A (no reset) asserts dirt STILL present — the committed negative control that
   proves writes persist when the reset is skipped (self-discriminating, M3
   lesson); boot 3 after the reset (fresh overlay B) asserts dirt gone.
   Verified-red evidence: temporarily skip the overlay recreation and watch the
   "gone" assertion fail.
2. **Cycle-time win (sub-task 3): phase-parallel `make check`.** Keep the cheap
   fail-fast rungs (eval → generation-diff) strictly first and serial; run the
   heavy rungs in a bounded-parallel phase (`-j2`, `--output-sync=target` for
   readable logs). NO rung or assertion is removed, loosened, or skipped — the
   same 16+1 rungs must all pass, a failure stops spawning further rungs — so
   this is a §7.3 exclusive-landing spine change, not a §4.3(2) weakening.
   Expected: marionette set 120s → ~75s; unchanged-tree floor ~534s → ~300s.
3. Re-measure (runs A'/B' under comparable load), record before/after, land
   per §7.2.

### Sub-task 2 — ephemerality rung: DONE (green, verified red first)

New rung `reset` (tests/reset.scm, %test-td-reset), wired into `make check`
after `boot-disk`; module loaded by the eval rung.

**Discovery (red-first):** the stock qcow2 image type inherits `<image>`'s
`volatile-root? #t` DEFAULT — every guest write to / lands on an in-RAM
overlay, never the disk. First run failed exactly at the persistence control
("boot 2: dirt persists" → actual-value #f), proving (a) the control is
non-vacuous and (b) the shipped qcow2 discards root writes by default.
**Note for M10.3 (mainline):** placed-generation persistence across reboot
will NOT survive on the image root unless the image sets `volatile-root? #f`
or persists on a separate filesystem (the CLAUDE.md td-state direction).

The rung therefore boots a NON-volatile variant of the instrumented system
(`%persistent-instrumented-image`, one extra image derivation): the strictest
case — a guest that genuinely persists writes, isolated only by the qemu-level
qcow2 overlay (exactly the §1.5 mechanism).

Verified-red evidence:
- 2026-06-10 run 1 (stock volatile image): "boot 2 (overlay A reused, NO
  reset): dirt persists" FAILED — the control catches write-loss/vacuity.
- 2026-06-10 run 3 (reset deliberately skipped, boot 3 reused dirty overlay
  A): "boot 3 (fresh overlay B = the reset): dirt is gone" FAILED while boots
  1–2 passed — the assertion reds precisely when the reset is removed.
- Final committed version: green (3/3), exit 0.

### Sub-task 3 — bounded-parallel check: DONE

Daemon concurrency verified empirically before relying on it: two concurrent
client `guix build --check` realisations of the place/prune trees overlap
(17s serial → 10s concurrent). Change: `check.sh` runs `make -j2
--output-sync=target`; the Makefile chains the six structural rungs serial-
first via order-only deps and lists the heavy rungs LONGEST-FIRST (LPT
packing — the naive order left `container` running alone for its full 71s:
116s → only 94s). Nothing removed/loosened/skipped; a red still stops new
rungs from spawning.

### Results (acceptance)

All on the warm store, same host, 2026-06-10; before/after on the SAME rung
set and invocation shape:

- **Marionette rung set** (test+boot-disk+container, test builders
  invalidated by a no-op datum): **116s serial → 94s** with -j2 in
  command-line order (the full-check LPT order packs container against
  test+boot-disk).
- **Full `./check.sh`, unchanged tree (the steady-state floor): 525s serial
  (-j1) → 275s parallel — green both ways, all 17 rungs.** 275s is at the
  two-slot packing bound (total heavy work ÷ 2 ≈ 260s + serial prefix).
- **Ephemerality intact and now ASSERTED**: the new `reset` rung (see
  sub-task 2) locks the per-test fresh-state guarantee in, verified red by
  disabling the reset.

Residual/possible follow-ons (not pursued; floor is now oracle-dominated):
the four OCI `--check` rungs are ~80% of remaining floor — only a roadmap
change could touch that; -j3 was not attempted (DESIGN §7.3 thrash bound with
a second agent's checks running).
