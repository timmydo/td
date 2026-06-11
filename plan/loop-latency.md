# Track: loop-latency (side-track)

**Status:** UNCLAIMED.
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

(claiming agent: notes here)
