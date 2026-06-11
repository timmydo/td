# Main-branch protection & CI setup

As of 2026-06-11 changes land on main **only via pull request** with a green
CI gate and one approving review (DESIGN §7.2). This file is the one-time
setup guide for the GitHub side; the day-to-day landing protocol lives in
DESIGN §7.2 and CLAUDE.md "Parallel work".

## What enforces what

- `.github/workflows/ci.yml` — two status checks per PR:
  - `lint` (GitHub-hosted): cheap structural checks only. It has no Guix and
    cannot run the loop; it never substitutes for `./check.sh`.
  - `check` (self-hosted): the canonical full `./check.sh` on a runner that
    is prepared like the dev box (below).
- `.github/setup-branch-protection.sh` — applies the `protect-main` ruleset:
  PRs only, 1 approving review, required status checks, linear history, no
  force pushes or deletion.

## One-time setup (repo admin)

1. **Plan check.** This repo is private; GitHub enforces rulesets on private
   repos only under GitHub Pro or a paid org plan. On a free plan the ruleset
   saves but does not enforce — upgrade, move the repo into a paid org, or
   make it public first.
2. **Authenticate gh** on any machine: `gh auth login` (needs admin on the
   repo). On the dev box: `guix shell gh -- gh auth login`.
3. **Apply the gate** (lint required, runner check not yet):

       ./.github/setup-branch-protection.sh

4. **Register the self-hosted runner** (the real gate). Requirements are
   exactly check.sh's: a Guix system whose *host* guix is the channels.scm
   pinned commit, a warm /gnu/store, /var/guix daemon socket, /dev/kvm, and
   ~2 cores + ~8 GB free per run (the ladder runs heavy rungs two at a time).
   GitHub-hosted runners cannot meet this. On the dev host the runner can run
   as a plain user process (no system reconfiguration):

       # repo Settings → Actions → Runners → New self-hosted runner,
       # then on the host (inside tmux or similar):
       ./config.sh --url https://github.com/timmydo/td --token <TOKEN> \
                   --labels guix,kvm
       ./run.sh

   check.sh refuses to run if the host guix drifts from the pin, so a stale
   runner fails loudly rather than fetching substitutes (it never goes
   silently online).
5. **Make the full check mandatory** once the runner is online and one PR has
   shown a green `check`:

       ./.github/setup-branch-protection.sh --require-runner-check

   (Doing this before a runner exists blocks every PR on a check that never
   reports.)

## The review deadlock (read before relying on "mandatory reviews")

GitHub does not let a PR author approve their own PR. If agents push branches
as the same account that reviews (timmydo), a required review can never be
satisfied and every PR deadlocks. Pick one:

- **Machine account (recommended):** agents push via a second account's SSH
  key / token with write access; the human account reviews and approves.
- **Reviews optional:** set `required_approving_review_count` to 0 in
  setup-branch-protection.sh — PRs + CI stay mandatory, approval doesn't.

## Day-to-day landing (replaces direct pushes)

1. Rebase your track branch onto latest `origin/main`.
2. Run the full `./check.sh` locally — must be green (CI verifies, it does
   not replace your own run; "fix forward in CI" wastes the runner).
3. Push the branch, open the PR (`gh pr create`), wait for `lint` + `check`.
4. Human review + approval, then rebase- or squash-merge. Merge commits are
   disabled (linear history, matching the old fast-forward convention).
