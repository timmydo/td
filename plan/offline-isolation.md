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

(claiming agent: notes here)
