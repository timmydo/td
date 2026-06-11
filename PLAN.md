# PLAN.md — track status index

Scope contract: the approved roadmap, `DESIGN.md` §7.1. This file is ONLY the status
index — one line per track, kept tiny so rebases stay trivial. Per-track working
state: `plan/<track>.md` (single writer — the claiming agent). Completed milestones:
`HISTORY.md`. Reproducibility digests: `DIGESTS.md`. Parallel-work rules: `CLAUDE.md`
"Parallel work" / DESIGN §7.2–7.4.

Claim a track by putting your handle + date on its line (one tiny standalone commit
to main, pushed). One agent per track; release the claim when you land or stop.

## Mainline (serial — one agent drives it at a time)

- [ ] **M10.3 manual rollback** — CLAIMED claude-fable 2026-06-10 — `plan/m10.md`
- [ ] **M11 verified generations** — blocked on M10.3 — split out of `plan/m10.md` when started
- [ ] **M12 signed distribution** — blocked on M11

## Side-tracks (parallel-safe)

- [ ] **rootless-builder** — UNCLAIMED — `plan/rootless-builder.md`
- [ ] **offline-isolation** — UNCLAIMED — `plan/offline-isolation.md`
- [ ] **oci-load** — UNCLAIMED — `plan/oci-load.md`
- [ ] **loop-latency** — UNCLAIMED — `plan/loop-latency.md`
- [ ] **fhs-app-images** — UNCLAIMED — `plan/fhs-app-images.md`

## The loop (reminder)

16 rungs: `eval diff typed-coverage oci-diff manifest-diff generation-diff build test
boot-disk oci manifest-check generation-image place no-guix run container`. eval →
differentials → `guix build --check` → marionette tests, short-circuiting on first
failure. Don't advance a sub-task until green. Small commits, each stating which test
now passes.
