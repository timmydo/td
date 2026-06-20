# design-check-tiers-sync — notes

Honesty fix (directive 3 is about not *silently* weakening gates; this is the
reverse — the gate move already happened and was approved, the DESIGN contract
just never got synced and now mis-describes the default loop).

## What was wrong
- DESIGN §1.1 "Broad shape" ended the default `./check.sh` in
  "behavioral/marionette tests". Since the `build-check-split` track (landed,
  `claude-fable-2715d4`) the whole-OS boot tier moved out of the default `check`
  into the on-demand `check-system` (Makefile:160). So the contract claimed
  coverage the default command no longer runs.
- DESIGN never documented the `check-system` tier at all, though `Makefile`
  and `check-fast` (CI) both depend on it.

## Fix
- §1.1: default `check` = package-manager behavioral/oracle tests; the boot tier
  (marionette + `guix system image`) is parked into on-demand `check-system`;
  `check-fast` is the cheap + typed-front-end CI subset.
- §1.2: note the "Boot + behavioral" rung class runs in `check-system`.

Docs-only — `tools/affected-checks.sh --path DESIGN.md` selects no checks and
waives the full loop.
