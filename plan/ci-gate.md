# Track: ci-gate (side-track)

**Claim status:** see `PLAN.md`.
**Origin:** roadmap addition approved by the human 2026-06-11 (self-hosted CI
direction; the CD half — automated signed-image distribution — is deliberately
deferred to a post-M12 entry). **Re-decided later the same day** (human, in
the GH-CI-CD session): landings go through **PRs with mandatory human review**,
superseding the original no-PR/status-gated-fast-forward form; the human
enabled branch protection on main directly in the GitHub UI that day.
**Scope authority:** DESIGN §7.1.

## Goal

A self-hosted GitHub Actions runner executes the **unmodified** `./check.sh`
for every PR into branch-protected main and posts the verdict as the `check`
check (workflow: `.github/workflows/ci.yml`). Once the runner is live, `check`
joins `lint` as a required check; merging needs green checks + one approving
review (DESIGN §7.2).

## Acceptance

- A green candidate PR shows a passing `check` run and (with approval)
  rebase/squash-merges onto branch-protected main.
- **Verified-red:** a deliberately red candidate (e.g. a broken assertion on a
  branch) shows a failing `check` run, and branch protection blocks its merge.
- The runner runs `./check.sh` as-is: hermetic `guix shell -C --pure`,
  substitutes disabled, channels-pin guard intact.

## Constraints

- **Never adapt the loop to the runner.** If `./check.sh` cannot run unmodified
  on the runner host, fix the host. Any weakening of the script, `Makefile`, or
  `tests/` to fit CI hits the §4.3(2) human gate.
- **Runner host:** selecting and provisioning it is the first sub-task. t5700g
  is excluded — standing immutable-infra rule, reinforced by the
  offline-isolation rescope: its daemon is the owner's machine state, serving
  the host's own maintenance, and is not td's to load or reconfigure. The
  runner host needs guix matching the `channels.scm` pin (`check.sh` guards
  this) and a daemon socket to expose into the sandbox, exactly as the loop
  uses today. The landed `rootless` rung and the td-builder track point at an
  eventually daemonless runner; note it here, don't block on it.
- **Tooling posture:** the runner agent (`actions/runner`) is MIT-licensed free
  software; the control plane (github.com) is proprietary SaaS — accepted by the
  human 2026-06-11 as development infrastructure. (FSDG purity is a non-goal per
  DESIGN §5, relaxed 2026-06-11, so this poses no posture conflict.)
- **Resources:** the runner's check counts toward the §7.3 two-concurrent-checks
  ceiling only if the runner shares a host with dev checks; on its own host,
  stagger landings as a courtesy. The gate serializes landings either way.
- **Out-of-repo state:** branch-protection settings and runner registration live
  outside the repo — document them precisely here when set, or they will drift
  (same discipline as offline-isolation's host-daemon note). The workflow file
  (`.github/workflows/`) is in-repo and part of the deliverable.

## Suggested sub-task ladder

1. Land the workflow (`lint` + self-hosted `check`), the protection setup
   script, and the §7.2 PR-protocol docs. [DONE — see Working state]
2. Human: machine account for agent pushes/PRs + `gh auth login`; run
   `./.github/setup-branch-protection.sh` (lint required).
3. Pick and provision the runner host (guix matching the pin; daemon for now;
   NOT t5700g).
4. Register the runner (labels `guix,kvm`); a green candidate PR shows a
   passing `check` run.
5. Verified-red: a red branch produces a failing `check` run and its PR is
   blocked.
6. Human: `./.github/setup-branch-protection.sh --require-runner-check`;
   announce here that the gate is fully armed.

## Working state

- 2026-06-11 claude-fable-52ceb1: claimed; this work began in the GH-CI-CD
  session before the track entry landed, then was reconciled onto it. Landed
  in this change: `.github/workflows/ci.yml` (hosted `lint` — structural only;
  self-hosted `check` — unmodified `./check.sh`, labels `guix,kvm`, SHA-pinned
  checkout), `.github/setup-branch-protection.sh` (protect-main ruleset: PRs
  only, 1 review, required checks, linear history; `--require-runner-check`
  flips `check` to required), `.github/BRANCH-PROTECTION.md` (setup guide),
  and the §7.2/CLAUDE/PLAN PR-protocol reconciliation. Design fork resolved by
  the human 2026-06-11: PR + mandatory review wins over the no-PR amendment.
  Verified: full `./check.sh` green pre-push; lint steps dry-run green against
  the tree. Open: machine account (PR authors can't self-approve), runner host
  (t5700g excluded), private-repo plan caveat (free plan does not enforce
  protection on private repos — see BRANCH-PROTECTION.md §1).
- The track's verified-red (red branch → failing `check` run → blocked PR)
  needs the live runner; it is step 5 above, not yet run.
