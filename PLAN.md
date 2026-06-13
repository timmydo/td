# PLAN.md — track status index

Scope contract: the approved roadmap, `DESIGN.md` §7.1. This file is ONLY the status
index — one line per track, kept tiny so rebases stay trivial. Per-track working
state: `plan/<track>.md` (single writer — the claiming agent). Completed milestones:
`HISTORY.md`. Reproducibility digests: `DIGESTS.md`. Parallel-work rules: `CLAUDE.md`
"Parallel work" / DESIGN §7.2–7.4.

Claim a track by putting your handle + date on its line as the FIRST commit of your
track branch, published by opening a draft PR (main is branch-protected — no direct
pushes; DESIGN §7.2). Handles are session-unique — generation mechanics in `CLAUDE.md`
"Parallel work". One agent per track; release the claim when you land or stop (close
the PR if abandoning). Claim status = this file on main **plus the open PRs' claim
edits** (track files don't carry it).

## Mainline (serial — one agent drives it at a time)

- [x] **M10.3 manual rollback + declared persistence** — DONE 2026-06-10 (claude-fable); review round DONE 2026-06-10 (claude-fable-9cb426) — `plan/m10.md`
- [x] **M11 verified generations** — DONE 2026-06-11 (claude-fable-7d8371; rollback rung grown to 36 asserts across three boots — sealed tmpfs-root + dm-verity store, corrupted root fails closed) — `plan/m11.md`
- [x] **M12 signed distribution** — DONE 2026-06-12 (claude-fable-c4148a: §7.1 acceptance green — `registry` rung pushes both gen images into a signed static OCI-layout registry (signify over manifest digests, pull-by-digest from the bytes alone) and `verify-place` proves the placer's verified mode places only what verifies, rejecting unsigned/bad-signature/digest-mismatch + a self-stated digest; direct-vs-verified placement differential; placed image-digest= anchor from S1; verified-red ×13 across S1-S4; DIGESTS §2.7 re-baselined) — `plan/m12.md`

## Side-tracks (parallel-safe)

- [x] **rootless-builder** — DONE claude-fable-ca67ec 2026-06-11 (new `rootless` rung: unprivileged userns daemon rebuilds the qcow2 image, NAR-hash-equal to the root daemon's oracle; verified-red A/C) — `plan/rootless-builder.md`
- [x] **offline-isolation** — CLOSED 2026-06-11 claude-fable-cebe98 (undeclared-fetch-fails `offline` rung landed; daemon-side isolation rescoped to the own-builder era, human sign-off — see DESIGN §7.1) — `plan/offline-isolation.md`
- [x] **oci-load** — DONE claude-fable-a03d13 2026-06-11 (new `oci-load` rung: skopeo foreign-loads the plain + gen-1 images into canonical OCI layouts, rejects a corrupted layer; §2.7 manifest-digest identity recorded in DIGESTS.md; verified-red ×4) — `plan/oci-load.md`
- [x] **loop-latency** — DONE claude-fable 2026-06-10 (full check 525s→275s; new `reset` rung) — `plan/loop-latency.md`
- [ ] **fhs-app-images** — UNCLAIMED — `plan/fhs-app-images.md`
- [x] **td-builder** — DONE 2026-06-11 (S4 claude-fable-8ebb4e: the §7.1 acceptance differential — td-builder rebuilds the system qcow2 image drv daemon-equal on path/NAR-hash/size/83-refs/deriver, GREEN at S3 sandbox parity, no chroot growth needed; verified-red ×3 at distinct S4 asserts; S1-S3 claude-fable-49b6d6/a03d13/696a4e) — `plan/td-builder.md`
- [ ] **ci-gate** — claimed claude-fable-52ceb1 2026-06-11 (workflow + protection setup landing; runner pending) — `plan/ci-gate.md`
- [x] **check-memo** — DONE claude-fable-580472 2026-06-12 (verdict memoization live on all 11 reproducibility `--check` legs/19 drvs; unchanged-tree floor 440s→145s; permanent `memo` rung asserts the discipline every loop; verified-reds on record; offline/rootless stay direct per the constraint-6 boundary) — `plan/check-memo.md`

## The loop (reminder)

One command: `./check.sh`. The `Makefile`'s `CHEAP_RUNGS`/`HEAVY_RUNGS` pools
(expanded by `check:`) are the authoritative rung list (don't restate it here); the
cheap rungs run serial-first, the heavy rungs two at a time (`make -j2`), and a red
still short-circuits. Don't advance a sub-task until green. Small commits, each
stating which test now passes.
