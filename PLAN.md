# PLAN.md — track status index

Scope contract: the approved roadmap, `DESIGN.md` §7.1. This file is ONLY the status
index — one line per track, kept tiny so rebases stay trivial. Per-track working
state: `plan/<track>.md` (single writer — the claiming agent). Completed milestones:
`HISTORY.md`. Reproducibility digests: `DIGESTS.md`. Parallel-work rules: `CLAUDE.md`
"Parallel work" / DESIGN §7.2–7.4.

Claim a track by putting your handle + date on its line (one tiny standalone commit
to main, pushed). Handles are session-unique — generation mechanics in `CLAUDE.md`
"Parallel work". One agent per track; release the claim when you land or stop. This
file is the **single source of truth for claim status** (track files don't carry it).

## Mainline (serial — one agent drives it at a time)

- [x] **M10.3 manual rollback + declared persistence** — DONE 2026-06-10 (claude-fable); review round CLAIMED claude-fable-9cb426 2026-06-10 — `plan/m10.md`
- [ ] **M11 verified generations** — UNCLAIMED (unblocked) — `plan/m11.md`
- [ ] **M12 signed distribution** — blocked on M11 — `plan/m12.md`

## Side-tracks (parallel-safe)

- [ ] **rootless-builder** — UNCLAIMED — `plan/rootless-builder.md`
- [ ] **offline-isolation** — UNCLAIMED — `plan/offline-isolation.md`
- [ ] **oci-load** — UNCLAIMED — `plan/oci-load.md`
- [x] **loop-latency** — DONE claude-fable 2026-06-10 (full check 525s→275s; new `reset` rung) — `plan/loop-latency.md`
- [ ] **fhs-app-images** — UNCLAIMED — `plan/fhs-app-images.md`

## The loop (reminder)

One command: `./check.sh`. The `check:` line in the `Makefile` is the authoritative
rung list (don't restate it here); the structural rungs run serial-first, the heavy
rungs two at a time (`make -j2`), and a red still short-circuits. Don't advance a
sub-task until green. Small commits, each stating which test now passes.
