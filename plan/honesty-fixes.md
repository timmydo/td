# plan/honesty-fixes.md — loop-sandbox honesty fixes + PLAN.md management

Track: **honesty-fixes** (claude-opus-117569, 2026-06-14). Addresses an external
review of the loop-sandbox work (8 findings) plus the PLAN.md status drift, in one PR
(human direction: "everything in one PR"). Single writer.

## Findings addressed

| # | Sev | Fix |
|---|-----|-----|
| 1 | High | `host_shell` arms `PR_SET_PDEATHSIG=SIGKILL` at BOTH fork levels (C0 + the PID-1 child) so killing td-builder cascades and the kernel reaps the inner PID namespace — no orphaned build/mounts on CI cancel/timeout. (`sys.rs` `set_pdeathsig`/`getppid`; `sandbox.rs`.) |
| 2 | High | Minimal synthetic `/dev` (tmpfs + the standard char devices bind-mounted + shm + best-effort devpts + fd symlinks) replaces the blanket host `/dev` rbind, which leaked `/dev/kmsg`, `/dev/kvm`, raw disks, `/dev/mem`, input devices, GPUs. (`sandbox.rs`; `/dev` dropped from `main.rs` binds.) |
| 3 | Med | `tests/rootless.sh`: corrected the over-claimed "race-free" comment — the LAST-and-ALONE ordering quiesces the DB only WITHIN one check; a second concurrent check (DESIGN §7.3) can still write it. The integrity_check is the loud-fail safety net; cross-check race-freedom is a documented follow-up. |
| 4 | Med | `Makefile`: the `rootless: \| <other heavy gates>` ordering is now gated on `MAKECMDGOALS` so an explicit `./check.sh rootless` runs alone (single-target contract restored); a full `make check` still orders it last. |
| 5 | Med | CI/branch-protection: `setup-branch-protection.sh`, `BRANCH-PROTECTION.md`, `DESIGN.md` §7.2 now reference the actual `check-fast` job (#26 replaced the per-PR full `check`); applying `--require-runner-check` no longer requires a nonexistent context. |
| 6 | Med | `sandbox.rs`: a failed ro-remount of an `ro_optional` bind (cgroup2 on the azure runner) now DETACHES the bind (fail-closed) instead of leaving the host subtree writable. Docs corrected. |
| 7 | Low | `main.rs`: the host-sandbox scratch dir is removed after the command exits (was one leaked `/tmp/td-host-sandbox-*` per run; swept 132 stale). |
| 8 | Low | PLAN.md drift fixed + a generated/enforced mechanism (below). |

## PLAN.md management (the "better way")

Mirrors the gates-split win: PLAN.md's track table is GENERATED from per-track records
`plan/tracks/<track>.md` by `tools/plan-index.sh`; a cheap `plan-index` gate fails the
loop if the committed PLAN.md drifts from the records. Claiming edits your OWN record
(no shared-line collision); a merged track can't keep reading "claimed" (the gate
catches it). Also restores one-line-per-track (DESIGN §7.4) — the verbose detail stays
in `plan/<track>.md`. CLAUDE.md "Parallel work" + DESIGN §7.2/§7.4 updated.

## Verified-red log

**R1 minimal /dev is load-bearing** (2026-06-14, `sandbox-hardening` leg A). Built the
PRE-FIX td-builder (the blanket host `/dev` rbind) and ran the leg-A probe inside it:
`/dev/kmsg`, `/dev/kvm`, `/dev/mem` were all reachable ⇒ leg A reds ("host device
leak"). With the minimal `/dev`, all three are absent and `/dev/null` is present +
writable ⇒ green. Proves the assertion has teeth (it is not a vacuous pass).

**R2 PR_SET_PDEATHSIG reaping is load-bearing** (2026-06-14, `sandbox-hardening` leg B).
Same pre-fix binary: started a host-sandbox running two store `sleep`s, killed the
top-level td-builder, and 3 inner processes SURVIVED ⇒ leg B reds ("orphaned"). With
the pdeathsig cascade, the inner tree (5 procs: top + C0 + PID-1 bash + 2 sleeps) is
fully reaped to 0 ⇒ green. Proves the reaping is real.

**R3 the plan-index gate catches drift** (2026-06-14). Flipped a record's `status: done`
→ `claimed` WITHOUT regenerating PLAN.md; `tools/plan-index.sh --check` exited non-zero
("PLAN.md is OUT OF SYNC") ⇒ `plan-index` reds. Regenerating (or reverting) greens it.
Proves the gate genuinely enforces record↔table consistency.

**R4 the rootless single-target fix** (2026-06-14, build-graph). `make -np rootless`
shows `rootless: | generation-diff` (cheap chain only) after the fix vs. the full heavy
ladder before — an explicit `./check.sh rootless` no longer drags in every heavy gate;
`make -np check` still orders rootless after all other heavy gates.

## Status

All eight findings fixed; two new gates (`plan-index` cheap, `sandbox-hardening` heavy)
with verified-red on record; full `./check.sh` green (every heavy gate runs under the
new minimal-/dev sandbox).
