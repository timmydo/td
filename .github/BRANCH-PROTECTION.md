# Main-branch protection & CI setup

As of 2026-06-11 changes land on main **only via pull request** with a green
CI gate and one approving review (DESIGN §7.2). This file is the one-time
setup guide for the GitHub side; the day-to-day landing protocol lives in
DESIGN §7.2 and CLAUDE.md "Parallel work".

## What enforces what

- `.github/workflows/ci.yml` — two required status checks per PR, both
  GitHub-hosted and **host-native** (no Guix, no store image):
  - `lint`: cheap structural checks only (shell syntax, the heal-revert
    primitive, the no-tabs-in-Scheme convention). It cannot run the loop; it
    never substitutes for `td-builder check`.
  - `cargo-test`: the td-native build ENGINE gate — `cargo test --frozen`
    (= --locked --offline) + `cargo clippy` over the dependency-free `builder`
    crate (drv parse/emit, store-path hashing, NAR framing, the SQLite store-db
    reader, ref-scan, the userns sandbox helpers). The builder crate carries no
    dependencies, so this runs on the runner's stock Rust with no image, no guix,
    no network — the same unit tests the dev-machine `check-engine` gate runs.
    This is the heart of td, so it is a required check.

  The guix-built `td-ci-fast` store image + the `check-fast` sandbox job were
  **retired 2026-07-06** (github issue #415): the fast tier went empty when the
  guix gates were deleted (#409), so `check-fast` was a vacuous no-op importing a
  ~4G image. `cargo-test` is the real per-PR engine gate now. The deep tiers (the
  from-source bootstrap ladder, the corpus, the /td/store harness) are NOT per-PR
  checks — they run on the dev machine via `td-builder check` and nightly via
  `td-builder daily`. Cold hosted runners cannot reliably rebuild td's
  closure, so a from-scratch CI build of the full store is not dependable.
- `ci/revert-suspect.sh` — the optimistic-merge heal primitive. Healing is an
  AGENT DUTY (not an automated workflow): when an agent sees `check-fast`… — see
  "Heal net" below (it now watches `lint` + `cargo-test`).
- `.github/setup-branch-protection.sh` — applies the `protect-main` ruleset:
  PRs only, 1 approving review, required status checks (**NON-strict** since
  2026-06-19 — a PR merges on its own green checks, NOT forced current with main
  first; DESIGN §7.2 optimistic merge), linear history, no force pushes or
  deletion.

## One-time setup (repo admin)

1. **Plan check.** This repo is private; GitHub enforces rulesets on private
   repos only under GitHub Pro or a paid org plan. On a free plan the ruleset
   saves but does not enforce — upgrade, move the repo into a paid org, or
   make it public first.
2. **Create a machine account** for the agents and grant it write access to
   the repo. This is not optional with mandatory reviews: a PR author cannot
   approve their own PR, so if agents push and open PRs as your account,
   every PR deadlocks. Agents authenticate as the machine account (its SSH
   key for pushes; `gh auth login` as it on the dev box for opening PRs);
   your account reviews and approves.
3. **Apply the gate** (lint + cargo-test required) with an admin `gh auth`:

       ./.github/setup-branch-protection.sh

   If branch protection was already configured by hand in the UI, this
   codifies it as the `protect-main` ruleset; remove or align the manual rule
   afterwards so there is one source of truth (Settings → Branches /
   Settings → Rules). **When the required-checks list changes** (as with the
   2026-07-06 drop of `check-fast`), re-run this script — a PR gated on a check
   whose job no longer runs waits forever.
4. **No CI store image to push.** The former guix store-image pipeline
   (`ci/build-ci-image.sh`, `ci-image.yml`) was retired with `check-fast`
   (#415); both CI checks are host-native and need no pre-built store.

## Heal net (optimistic merge — DESIGN §7.2)

Main is non-strict, so two independently-green PRs can squash-merge into a red
main. We accept that and heal after the fact instead of forcing every PR to
rebase-onto-tip + re-run. Healing is an **AGENT DUTY, not an automated workflow**
(human 2026-06-19) — no `HEAL_PAT` secret, no ruleset bypass, nothing to
provision.

1. **The duty.** Whenever an agent fetches main (to start a track or to land),
   it checks main's latest required checks (`lint` + `cargo-test`):

       gh run list --branch main --workflow ci.yml -L 1
       # or: gh api repos/<owner>/td/commits/main/check-runs

   If red, the agent runs `ci/revert-suspect.sh --open-pr` to open a revert PR
   for the suspect squash commit (main's HEAD) before continuing. The agent opens
   it with its own bot credentials, so the revert PR triggers the required checks
   normally and is reviewed/merged like any other PR. The script's loop guard
   refuses to revert a revert (no storms).
2. **Scope.** Only what the hosted checks see is covered — a lint/engine break.
   A heavy-only break (the from-source bootstrap ladder, the corpus, the
   /td/store harness, reproducibility — invisible to `lint`/`cargo-test`) is not
   caught per-PR; it surfaces on the nightly `td-builder daily`, which
   opens a fix-or-revert PR. This is the accepted velocity trade.

## The review deadlock (why the machine account is mandatory)

GitHub does not let a PR author approve their own PR. If agents push branches
and open PRs as the same account that reviews (timmydo), a required review can
never be satisfied and every PR deadlocks. Hence step 2: agents act as a
machine account with write access; the human account reviews and approves.
(Mandatory reviews are the decided design — human, 2026-06-11; dropping the
requirement would need that decision revisited, not just a script edit.)

## Day-to-day landing (replaces direct pushes)

Optimistic merge (non-strict since 2026-06-19): validate against your **own
base**, not the latest tip.

1. Run the loop green locally —
   `td-builder affected-checks --committed-only --run` (bounded to the ~10-min
   per-PR tiers; daily-tier gates are deferred to the daily backstop). Nothing
   escalates to the full loop anymore — the channels.scm pin, the sole
   escalation, was removed 2026-07-06. CI verifies your run; it does not replace
   it.
2. Push the branch, open the PR (`gh pr create`), wait for `lint` + `cargo-test`.
3. Human review + approval, then squash-merge (the only merge mode enabled —
   merge and rebase merges are off, linear history; the squash commit body is
   the branch's commit messages, not the PR description). **Do not rebase-onto-tip
   + re-run just because main moved** — that toil is what non-strict drops.
   Rebase only on a real git conflict, or to sequence an exclusive landing. A
   broken combination is healed by the next agent as a duty (above): before you
   start or land, if main's required checks are red, open the revert first.
