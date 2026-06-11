# Track: offline-isolation (side-track)

**Claim status:** see `PLAN.md` (the single source of truth for claims).
**Origin:** standing follow-up first surfaced in M6 (see HISTORY.md "Offline posture").
**Scope authority:** DESIGN §7.1.

## Goal

Close the gap between the loop's guarantee (*no substitutes + no offload*) and full
network isolation: drop nonguix from the host daemon's substitute URLs and isolate
the daemon's network so a cold path can't even query.

## Acceptance

The full loop stays green with the daemon network-isolated, and a deliberate
undeclared fetch (non-fixed-output network access) demonstrably fails
(verified-red). Declared fixed-output source fetches remain the only permitted
network path, per the hermeticity clause.

## Constraints

- Don't break the warm-store property — the pinned-channel guard plus warm store is
  what keeps the loop fast; isolation must not force cold rebuilds.
- Daemon configuration is host state outside the repo; document precisely what was
  changed and how `check.sh` asserts it (an unasserted host setting will drift).

## Working state

Agent: claude-fable-cebe98 (claim is in PLAN.md). Worktree:
`.claude/worktrees/offline-isolation`.

**Exclusive-landing announcement:** this track touches the shared spine —
`check.sh` (host-posture guards) and `Makefile` (new `offline` rung). Will land
those as small standalone commits per DESIGN §7.2; expect rebases.

### Host survey (2026-06-11)

- Host is a Guix System (shepherd, no systemd). `guix-daemon` is PID-1's child,
  runs as root, cmdline:
  `--build-users-group guixbuild --max-silent-time 3600 --timeout 86400
  --log-compression gzip --discover=no --substitute-urls
  https://substitutes.nonguix.org https://bordeaux.guix.gnu.org
  https://ci.guix.gnu.org`
  → nonguix IS in the daemon's substitute set (the M6 finding, still true), and
  the daemon lives in the host netns.
- This agent is uid 1001, no passwordless sudo → the host change itself (drop
  nonguix + netns-isolate the daemon) needs the human; everything else is
  repo-side and proceeds first.

### Design

Two probe derivations, one mechanism (read `/proc/net/dev`, expect only `lo`;
then a TCP connect to 192.0.2.1:9 must raise) — the netns is what the daemon
does or does not unshare, so interface visibility IS the property:

1. **sandbox probe** (non-fixed-output): runs in the build sandbox's private
   netns. Asserts the undeclared-fetch path fails. Green on any correctly
   sandboxed daemon — landable now.
2. **daemon probe** (fixed-output): FO builders share the DAEMON's netns (the
   one network-permitted path, and the same netns `guix substitute` runs in).
   Asserts only `lo` there too — i.e. the daemon itself is network-isolated, a
   cold path cannot even query. RED until the host change (that red is the
   verified-red for the shared builder assertions).

Both are rebuilt with `guix build --check` every loop so the assertions
re-execute per-check (and the probes stay reproducible, prime directive 1).

`check.sh` host-side guards (host netns, where the container can't look):
- control: host `/proc/net/dev` must show a non-lo interface — same mechanism
  the probes use, proving it discriminates (a probe that always says "lo only"
  would die here).
- drift guard: `/proc/<daemon-pid>/cmdline` must NOT name nonguix as a
  substitute server (the unasserted-host-setting drift the track warns about).
  RED until the host change.

### Sub-task ladder

- [x] S1 `offline` rung: sandbox probe (build + --check) + check.sh host
      control. Verified-red: (a) the daemon probe failing today is the same
      builder code seeing network; (b) plumbing red (invert expected ifaces).

### S1 verified-red evidence (2026-06-11)

Green run first: `./check.sh offline` →
`sandbox probe: netns interfaces: ("lo")`,
`sandbox probe: egress attempt failed as required: Network is unreachable`,
then `--check` re-ran the builder with the same output (PASS, exit 0).

(a) **Network-visible red** — the fixed-output twin (DRV_DAEMON,
`a64qwx62y989z8zdmh28z40qsjyp50wl-td-offline-daemon-probe.drv`), the IDENTICAL
builder body run where network IS present (the daemon's netns, today), built
manually with `guix build --no-substitutes --no-offload`:

    daemon probe: netns interfaces: ("lo" "tap0")
    FAIL: the daemon netns sees non-loopback interfaces ("lo" "tap0") — an
    undeclared fetch could reach the network from a path that must be isolated.
    builder for `…-td-offline-daemon-probe.drv' failed with exit code 1
    (guix build exit status 1)

So the shared assertions detect a network-visible netns and fail the build —
the sandbox probe's green is not vacuous. (Also documents the daemon's netns
contents today: `tap0` — the host's uplink.)

(b) **Plumbing red** — temporarily inverted the expected interface list
(`'("lo")` → `'("lo0")`) and ran `./check.sh offline`:

    FAIL: the sandbox netns sees non-loopback interfaces ("lo") — …
    builder for `…v5viivszpzrddqfrql1ag87whdv5ia65-td-offline-sandbox-probe.drv'
    failed with exit code 1
    make: *** [Makefile:587: offline] Error 1   (check.sh exit 2)

So a builder-level failure propagates guix build → make → check.sh red.
Reverted; rung green again (exit 0).
- [ ] S2 daemon probe + nonguix drift guard wired into the rung/check.sh
      (red until host change; stays on branch).
- [ ] S3 host change (HUMAN): drop nonguix from substitute-urls + netns-wrap
      the daemon; exact snippet to be provided. Loop must stay green (warm
      store untouched — netns does not invalidate the store).
- [ ] S4 full ./check.sh green; land (exclusive); release claim in PLAN.md.
