# Track: amortize-vm-boots (side-track)

**Handle:** claude-fable-840226 · **Claimed:** 2026-06-15

## Goal

Remove one full marionette VM boot from the loop by consolidating the `test`
gate's behavioral assertions onto the realistic GRUB `boot-disk` gate, then
deleting the synthetic direct-kernel `test` gate.

Rationale (human steer 2026-06-15): the `test` gate boots via
`(virtual-machine os)` — direct-kernel `-kernel`/`-initrd`, NOT how td ships —
and carries the behavioral asserts; `boot-disk` boots the qcow2 through
firmware→GRUB (the real path) but only asserts uname. Moving the behaviors onto
`boot-disk` verifies them on the SHIPPING boot path (a coverage improvement in
spirit) and lets the direct-kernel boot go away → one fewer VM boot per check.

## Scope / blast radius

- `tests/boot.scm`:
  - base `%instrumented-disk-os` on `%test-os` (td-system + the test SSH user +
    authorized key) instead of bare `td-system`, so the disk boot can run the
    key-based SSH login assert. `%test-os` already exists (was the direct-kernel
    test's base). The SHIPPED `td-system`/qcow2 oracle is untouched.
  - move asserts #2–#6 (sshd unit up, port listens, password-auth denied,
    key-based login succeeds, container-host cgroup2+crun) into
    `run-td-disk-boot-test`; assert #1 (kernel/uname) already lives there.
  - delete `run-td-boot-test`, `%test-td-boot`; trim the `#:export` + comments.
  - bump the disk-boot VM `-m 512`→`1024` for headroom (sshd + ssh client).
- `mk/gates/155-test.mk`: delete (the `test` gate). Gate removal — surfaced in
  the PR (directive 3). The Makefile derives the pool from the fragments, so a
  delete is the whole edit (no shared-list line).
- `ci/system-test-drvs.scm`: drop `%test-td-boot` from the full-image lowering
  list (it no longer exists). NOT an exclusive-spine file.
- `mk/gates/170-boot-disk.mk`: update the doc to reflect the expanded asserts.

`%instrumented-disk-os` is SHARED with `reset` (reset derives a non-volatile
variant). Adding a test user is benign for ephemerality (it's immutable system
state, not guest-mutable dirt) — verify reset still green.

No oracle/DIGESTS impact: only test-only instrumented images change; the shipped
system + its differentials are untouched.

## Sub-task ladder

1. [ ] base %instrumented-disk-os on %test-os; move asserts #2–6 into the
   disk-boot test; bump -m; delete run-td-boot-test/%test-td-boot + exports.
2. [ ] delete mk/gates/155-test.mk; drop %test-td-boot from ci/system-test-drvs.scm;
   update 170-boot-disk.mk doc.
3. [ ] `./check.sh boot-disk` green; SRFI-64 log shows all 6 asserts on the GRUB boot.
4. [ ] verified-red: break the key-based-login assert (riskiest — new disk-image
   user dependency) and a marionette-eval assert; watch boot-disk red; revert.
5. [ ] `./check.sh reset` green (shared image still boots).
6. [ ] sub-agent diff review; full `./check.sh` green; land per §7.2.

## Verified-red evidence

(to fill in)

## Measurement

(record boot-disk before/after wall time + full-check delta)
