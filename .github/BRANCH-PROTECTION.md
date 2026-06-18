# Main-branch protection & CI setup

As of 2026-06-11 changes land on main **only via pull request** with a green
CI gate and one approving review (DESIGN §7.2). This file is the one-time
setup guide for the GitHub side; the day-to-day landing protocol lives in
DESIGN §7.2 and CLAUDE.md "Parallel work".

## What enforces what

- `.github/workflows/ci.yml` — two status checks per PR:
  - `lint` (GitHub-hosted): cheap structural checks only. It has no Guix and
    cannot run the loop; it never substitutes for `./check.sh`.
  - `check-fast` (hosted): `./check.sh check-fast` — the FAST tier (cheap +
    derivation-level gates + the tsc type-check, Makefile `FAST_GATES`), run by
    importing the small `td-ci-fast` store image. Since #26 the per-PR runner
    runs the fast tier ONLY; the full hermetic loop is the dev-machine gate
    (DESIGN §7.2 step 2) plus the ci-image pipeline's `validate` job (full
    `./check.sh` against the `td-ci` image, `ci-image.yml`) — NOT a per-PR check.
    So branch protection requires `check-fast`, never a `check` context (there
    is no such job).
- `.github/setup-branch-protection.sh` — applies the `protect-main` ruleset:
  PRs only, 1 approving review, required status checks, linear history, no
  force pushes or deletion.

## One-time setup (repo admin)

1. **Plan check.** This repo is private; GitHub enforces rulesets on private
   repos only under GitHub Pro or a paid org plan. On a free plan the ruleset
   saves but does not enforce — upgrade, move the repo into a paid org, or
   make it public first.
2. **Create a machine account** for the agents and grant it write access to
   the repo. This is not optional with mandatory reviews: a PR author cannot
   approve their own PR, so if agents push and open PRs as your account,
   every PR deadlocks. Agents authenticate as the machine account (its SSH
   key for pushes; `gh auth login` as it on the dev box for opening PRs:
   `guix shell gh -- gh auth login`); your account reviews and approves.
3. **Apply the gate** (lint + check-fast required) with an admin
   `gh auth`:

       ./.github/setup-branch-protection.sh

   If branch protection was already configured by hand in the UI, this
   codifies it as the `protect-main` ruleset; remove or align the manual rule
   afterwards so there is one source of truth (Settings → Branches /
   Settings → Rules).
4. **Push the CI store image** (the real gate's fuel). The ci-image pipeline's
   `validate` job (and, with the fast subset, the per-PR `check-fast` job) runs
   on GitHub-HOSTED runners by importing a snapshot of the warm build closure
   the ladder needs — built on a dev box whose guix matches the pin (this
   sidesteps self-hosted runners entirely, and with them the t5700g
   immutable-infra exclusion). On the dev box:

       PUSH=1 ci/build-ci-image.sh /path/with/50G/free

   (needs a gh login holding `write:packages`; pushes
   `ghcr.io/timmydo-bot/td-ci:<pin>` and `:latest`). After the FIRST push, make
   the package public once — GHCR UI ("td-ci" package → settings →
   visibility) — so the workflow pulls it anonymously.
5. **The runner check is mandatory.** As of 2026-06-18 the `td-ci-fast` image
   is published and `check-fast` has been green on recent PRs, so
   `setup-branch-protection.sh` requires it by default (step 3) — there is no
   longer a separate opt-in flag. Only make `check-fast` required while the
   image exists and the job is passing: requiring a check that cannot pass
   blocks every PR. (This requires the `check-fast` context — not `check`,
   which #26 removed as a per-PR job.)

## CI store image (how the hosted runner runs guix)

`./check.sh` needs a host guix at the pinned commit and a warm /gnu/store —
a fresh hosted runner has neither, and with substitutes disabled it would
build the world. The sanctioned move is DESIGN §5's "warm store in, nothing
fetched inside": `ci/build-ci-image.sh` snapshots the exact build closure of
every rung (enumerated from the rungs' own lowering scripts —
`ci/lower-check-drvs.sh`), signs it with the dev box daemon's key, and ships
it as OCI layers; the workflow imports it (`ci/import-store.sh`) and runs the
loop unmodified, offline. The loop is never adapted to CI (ci-gate track
constraint) — the image fixes the HOST.

- **Image tag = channels.scm pin.** The workflow derives the tag from the
  PR's channels.scm, so a channel-bump PR is red until whoever bumps it runs
  `PUSH=1 ci/build-ci-image.sh` from the new pin (part of the bump's
  exclusive-landing duty; the failing pull names the missing tag).
- **New rungs:** `ci/lower-check-drvs.sh` fails loudly when the Makefile rung
  pools change, so a rung-adding PR also updates the enumeration and pushes a
  refreshed image (same tag — push overwrites).
- **Run budget:** image pull+import ≈ 15–25 min, the ladder ≈ 15–30 min on
  the 4-vCPU hosted runner (vs ~6 min on the dev box) — well inside the job's
  240-min timeout.

## The review deadlock (why the machine account is mandatory)

GitHub does not let a PR author approve their own PR. If agents push branches
and open PRs as the same account that reviews (timmydo), a required review can
never be satisfied and every PR deadlocks. Hence step 2: agents act as a
machine account with write access; the human account reviews and approves.
(Mandatory reviews are the decided design — human, 2026-06-11; dropping the
requirement would need that decision revisited, not just a script edit.)

## Day-to-day landing (replaces direct pushes)

1. Rebase your track branch onto latest `origin/main`.
2. Run the full `./check.sh` locally — must be green (CI verifies, it does
   not replace your own run; "fix forward in CI" wastes the runner).
3. Push the branch, open the PR (`gh pr create`), wait for `lint` + `check-fast`.
4. Human review + approval, then rebase- or squash-merge. Merge commits are
   disabled (linear history, matching the old fast-forward convention).
