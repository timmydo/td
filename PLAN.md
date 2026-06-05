# PLAN.md — working todo / plan (persists between iterations)

Working scratchpad for the td build loop. Keep this in sync as milestones land.
Source of truth for *scope* is `DESIGN.md` §2.4 (the milestone ladder); this file
tracks *where we are* on it.

## Milestone ladder status (DESIGN.md §2.4)

- [x] **M1 — Closed loop on a trivial image** (§2.1). `make check` green end to end:
      eval → `guix build --check` (reproducible qcow2) → marionette boot test asserts
      `uname -r` == declared kernel (6.18.15-gnu). Committed: 5ed0903.
- [x] **M2 — Add a service; assert unit up + port listens.** `make check` green:
      td-system declares `openssh-service-type` (+ `dhcpcd-service-type` to satisfy
      sshd's `'networking`); the marionette test boots once and asserts (a) `uname -r`,
      (b) the `ssh-daemon` shepherd unit is running, (c) the declared sshd port (22,
      derived from `td-ssh-configuration`) is listening. Image still reproducible under
      `guix build --check`. Committed: e02ea83.
- [x] **M3 — Default-deny hardening on sshd; test asserts a forbidden op is DENIED.**
      Hardened `td-ssh-configuration`: `password-authentication? #f` (the honest
      lever — it defaults to `#t`; root-login is already `#f` so it would be a no-op)
      plus `challenge-response-authentication? #f`. The test asks the daemon which
      auth methods it will allow (ssh `-v -o PreferredAuthentications=none`) and
      asserts it offers no password method. Differential VERIFIED: password-auth ON →
      advert `publickey,password` → assertion FAILS (red); OFF → `publickey` only →
      passes. The ssh client is run by absolute /gnu/store path (the VM shares the
      host store), so the image gains no test-only tools. Committed: <fill>.

      ⚠️ While doing M3 I discovered the **behavioral rung had been false-green since
      M1** — see "Loop-integrity fixes" below. M1/M2 assertions only began actually
      running once those were fixed (they now pass for real).
- [ ] M4 — Typed config front-end compiling to gexps; differential test: same store
      paths as hand-written gexp. *(gates on human sign-off — DESIGN §4.3)*
- [ ] M5 — extend toward north star.

## Loop-integrity fixes (M3 — the behavioral rung was lying)

Three compounding defects made `make check`'s behavioral rung pass vacuously. All are
now fixed; the rung honestly goes red (verified). None weaken a test — they make the
oracle real.

1. **`node-test-runner` was unbound** (inherited from M1). The correct binding is
   `system-test-runner`, and it must take `#$output` as its log dir so the test
   derivation actually produces output. The old builder errored on the unbound
   variable before any assertion ran.
2. **`guix repl` reading a script from STDIN always exits 0** — it swallows the
   script's exit code (even `(exit 3)` → 0). The old `test` rung piped the build
   script into `guix repl`, so a FAILED build still exited 0. Fix: lower the monadic
   test value to a `.drv` via repl, then realise it with `guix build "$drv"`, whose
   exit status is honest and which streams the marionette log. (`guix repl FILE` would
   also honor exit codes, but the two-step gives log visibility too.)
3. **Guest forms lacked their imports** — `open-input-pipe`/`read-line`/`read-string`
   were used inside `marionette-eval` without importing `(ice-9 popen)`/`(ice-9
   rdelim)` in the guest, so the forms errored and `marionette-eval` returned `#f`.
   Fixes: get the kernel release from Guile's built-in `(uname)` (no subprocess, no
   PATH dependence) and `use-modules` inside any guest form that shells out.

Lesson for future tests: a green behavioral rung is only meaningful if you have SEEN it
go red. Always run a differential (break the thing, watch the test fail) before
trusting a pass.

## How to run the loop (IMPORTANT — non-obvious, learned in M2)

The naive `guix shell -C --pure -- make check` does NOT work here, for two reasons
discovered in M2:

1. **Empty container** — `-C --pure` with no packages has no `make`/`guix`. Must pass
   the toolchain: `make bash coreutils sed grep findutils` (and a guix).
2. **guix version mismatch** — the `guix` *package* available to `guix shell`
   (`1.5.0-1.deedd48`) is an OLDER commit than the channel we pin (`520785e`). Driving
   the Makefile's `guix time-machine` with deedd48 makes it compute a *different*
   channel-instance derivation for 520785e, miss the warm store cache, and try to
   **download** it from substitute servers (which on this host include nonguix.org).
   That breaks offline/local-only (DESIGN §5) and the FSDG posture.

**Fix / canonical invocation** (offline, local-only, no downloads, reproducible):
use the host's *system* guix — which already IS the pinned commit `520785e` (verify
with `guix describe`) — inside the container, with the full store exposed:

```sh
HOSTGUIX_DIR=$(dirname "$(readlink -f "$(command -v guix)")")
guix shell -C --pure --expose=/gnu/store \
  --share="$HOME/.cache/guix" --share=/var/guix \
  make bash coreutils sed grep findutils -- \
  bash -c "export PATH=$HOSTGUIX_DIR:\$PATH; make check"
```

- `--expose=/gnu/store` — `-C` otherwise mounts only the profile closure, hiding the
  host guix binary's closure.
- `--share="$HOME/.cache/guix"` — pinned channel checkout (avoids re-fetch).
- `--share=/var/guix` — daemon socket + writable profiles/GC roots for time-machine.
- Putting the host guix (520785e) first on PATH makes the Makefile's `time-machine` a
  no-op that hits the warm store → fully offline.
- Do **NOT** add `--network`: it pulls substitutes incl. nonguix.org (FSDG + local-only
  violation). The loop must stay offline.

Candidate cleanup (not yet done; would change the contract — leave for a deliberate
step): bake this invocation into a `make container-check` target or a `check.sh` wrapper
so "the single command" is self-contained. Deferred to avoid silently restructuring the
loop mid-milestone.

## Loop reminder (CLAUDE.md)

eval → `guix build --check` → marionette test. Short-circuits on first failure. Don't
advance a sub-task until green. Small commits, each stating which test now passes.
`guix style` was tried in M2 and *rejected*: it mangled comments and produced layout
inconsistent with M1's hand-formatted files. Keep the readable hand-formatted 2-space
style that M1 established.
