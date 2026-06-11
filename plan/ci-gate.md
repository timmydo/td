# Track: ci-gate (side-track)

**Claim status:** see `PLAN.md` (the single source of truth for claims).
**Origin:** roadmap addition approved by the human 2026-06-11 (self-hosted CI
direction; the CD half — automated signed-image distribution — is deliberately
deferred to a post-M12 entry).
**Scope authority:** DESIGN §7.1.

## Goal

A self-hosted GitHub Actions runner executes the **unmodified** `./check.sh` for
every candidate landing and posts the verdict as a commit status. Once this
track's acceptance test is green, that verdict is the binding landing gate — the
§7.2 amendment (2026-06-11) self-arms.

## Acceptance

- A green candidate branch produces a passing check run, and the same SHA
  fast-forwards onto branch-protected main.
- **Verified-red:** a deliberately red candidate (e.g. a broken assertion on a
  branch) produces a failing check run, and branch protection rejects the
  fast-forward of that SHA.
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

1. Pick and provision the runner host (guix matching the pin; daemon for now).
2. Register the runner; workflow runs `./check.sh` on pushes to candidate
   branches and main; green candidate shows a passing check run.
3. Verified-red: a red branch produces a failing check run.
4. Enable branch protection on main requiring the check; prove a green SHA
   fast-forwards and a red SHA is rejected.
5. Announce here that the gate is armed; agents adopt the gated §7.2 step 3.

## Working state

(claiming agent: notes here)
