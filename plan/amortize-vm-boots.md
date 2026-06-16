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

## Verified-red evidence (2026-06-15)

- **Key-login (the genuinely new machinery — key baked into the standalone disk
  image).** With `authorized-keys` emptied (`(list)`), boot-disk shows exactly
  ONE failure — "key-based SSH login succeeds and command output is captured" —
  while the other 6 asserts PASS (6 passes / 1 unexpected failure; gate reds with
  Error 1). So the login assert genuinely verifies authorized login on the GRUB
  boot, and the other asserts discriminate independently. Restored.
- **Relocated marionette-eval assert (password-deny).** With
  `password-authentication? #t` on `%test-os`'s openssh config, boot-disk shows
  exactly ONE failure — "daemon denies password authentication (default-deny)" —
  the other 6 PASS. So a relocated `marionette-eval` ssh-probe assert is
  load-bearing on the GRUB boot too (addresses the review nit). Restored.
- **The relocation is non-vacuous.** The green run's SRFI-64 log shows every
  moved assert with `result-kind: pass` and a REAL actual-value (the running
  ssh-daemon service record, the cgroup2fs type string, etc.) — they execute and
  check real guest state, not vacuously. (They are verbatim relocations of asserts
  already verified-red historically in the direct-kernel test; the new risk was
  whether they RUN on the disk boot + the key availability, both confirmed.)
- **Debug trail:** first run failed the login with `car` on `#f` — the standalone
  disk guest has no shared store, so `copy-file` from the key's /gnu/store path
  threw. Baking the key via an activation service to `/root/td_test_key` still
  failed (`/root` absent at activation); placing it at `/td_test_key` (filesystem
  root) works → login PASS.
- **reset still green:** all 3 ephemerality asserts pass with the test user/key
  now in the shared `%instrumented-disk-os` (immutable system state, inert to the
  dirt/reset assertions), as predicted.

## Measurement

- `./check.sh boot-disk` warm: ~25s (test drv cache hit / ~boot+7 asserts).
- Net: removes the entire `test` gate (one direct-kernel `(virtual-machine os)`
  boot, ~30s historically); boot-disk now runs 7 asserts instead of 2 (a few
  extra seconds for sshd+login) but there is ONE fewer full VM boot per check.
- Full-check wall delta: recorded after the landing full run.
