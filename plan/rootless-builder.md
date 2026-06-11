# Track: rootless-builder (side-track)

**Claim status:** see `PLAN.md` (the single source of truth for claims).
**Origin:** deferred from M10.1 (the generation image still builds via the daemon).
**Scope authority:** DESIGN §7.1.

## Goal

Build the target with a rootless user-namespace builder instead of the root
`guix-daemon`, and prove equivalence per prime directive 4: the existing daemon is
the oracle.

## Acceptance

A daemon-vs-rootless **store-path differential**: the same declaration built both
ways yields identical store paths (diff with `diffoscope` on mismatch), run as a
self-discriminating rung. Verified-red required (show a perturbation that makes the
paths diverge, or a deliberately broken rootless build that the rung catches).

## Constraints

- The loop stays offline (no substitutes, no offload) and hermetic.
- This track touches `check.sh`/`Makefile` to add its rung — adding a rung is free,
  but those are shared-spine files: land as a small standalone commit (DESIGN §7.3).

## Working state

(claiming agent: notes here)
