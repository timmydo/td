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
      host store), so the image gains no test-only tools. Committed: cf78c4a.

      ⚠️ While doing M3 I discovered the **behavioral rung had been false-green since
      M1** — see "Loop-integrity fixes" below. M1/M2 assertions only began actually
      running once those were fixed (they now pass for real).
- [~] **M4 — Typed config front-end compiling to gexps; differential: same store
      paths as the hand-written gexp.** GREEN + verified-red, *awaiting human
      sign-off before merge (DESIGN §4.3).* New `(system td-typed)`: a validated
      typed record (`td-config`, a smart constructor that rejects bad
      type/range — port range, fs-type set, booleans — verified rejecting) and a
      compiler `td-config->operating-system` that independently rebuilds the
      system. The hand-written `td-system` stays FROZEN as the oracle (§2.5).
      New `tests/typed-diff.scm` + `make diff` rung is SELF-DISCRIMINATING:
      (a) `%td-default-config` lowers to the SAME `system.drv` as the oracle
      (`z96c9kjj…`), and (b) a perturbed config (ssh-port 2222) lowers to a
      DIFFERENT drv (`l5dpy83m…`). Verified-red: breaking the compiler's default
      made (a) go #f → rung exits 1 (`guix repl FILE` honors the exit code; not
      the STDIN-swallow path). Image derivation unchanged (`a82grxjny…`) — the
      front-end is purely additive. Signed off (§4.3). Commit: 465a6ea
      (bedrock fix: d6a1220).
- [~] **M5 — OCI image artifact: the declaration also lowers to a reproducible
      Docker/OCI image with a deterministic digest.** GREEN + verified-discriminating,
      *awaiting human sign-off before merge (§4.3)* — crosses §2.3 "OCI app model".
      Pulls the north-star "store path doubles as OCI digest" thread (§0): the SAME
      `system/td.scm` that boots as a VM (M1–M4) now also lowers to a Docker/OCI
      image, via `(image-with-os docker-image os)` + `system-image` (exactly what
      `guix system image -t docker` builds). Two new rungs:
      • `tests/oci-diff.scm` (`make oci-diff`) — cheap derivation-level differential,
        self-discriminating like `typed-diff`: (a) the typed `%td-default-config`
        lowers to the SAME OCI image drv as the frozen `td-system` oracle
        (`8v1bdz2v…-docker-image.tar.gz.drv`); (b) a perturbed config (ssh-port 2222)
        lowers to a DIFFERENT drv (`vyz3g46k…`). Verified-red built in.
      • `make oci` — builds the Docker image and `guix build --check`s it bit-for-bit.
        VERIFIED reproducible: output `4x2kvsbd8g795l5dgla01gx4xhbha49g-docker-image.tar.gz`
        (drv `8v1bdz2v68gkbzybbaq4875a5flh2kvp`), `--check` rebuild showed no
        divergence. That output store-path IS the deterministic digest (accept-test
        part d), recorded here per the parking-lot kernel-pin convention.
      Loop wiring: `check: eval diff oci-diff build test oci` (cheap diffs fail fast;
      OCI build's --check mostly re-runs the docker-packing step since the OS closure
      is shared with `build`). The qcow2 boot rung (M1–M3) is unchanged — the OCI
      rung is purely additive, exactly as M4's diff rung was.
      Out of M5 (later): running the image (`docker run` = OCI *app model*); literal
      store-path==digest *equivalence* (needs fs-verity); FHS-flattened roots and the
      manifest-driven image-swap model (DESIGN §6 parking lot). Commit: 66494ca.

      *Acceptance test (literal — write it, don't vibe it):* the pinned guix
      (520785e) exposes the `docker` image type (verified via
      `guix system image --list-image-types`). Feed an `operating-system` to
      `system-image` with that type to get a Docker/OCI image derivation. A new
      build/diff rung (additive, in the `make diff` family — NOT the marionette
      boot) asserts:
      (a) **Reproducible:** `guix build --check` on the OCI image derivation
          passes (bit-identical rebuild). A non-reproducible OCI tarball is a
          FAILING test (prime directive 1) — fix forward or STOP & report; never
          disable `--check`. *Risk:* docker tarballs can embed mtimes; if `--check`
          goes red, that is M5's real work, not a thing to paper over.
      (b) **Front-end equivalence (oracle, §2.5):** the OCI image built from the
          typed `%td-default-config` is the SAME derivation/store path as the one
          built from the FROZEN hand-written `td-system` oracle.
      (c) **Self-discriminating (verified-red, per M3 lesson):** a perturbed config
          (e.g. ssh-port 2222) yields a DIFFERENT OCI image derivation — prove the
          rung distinguishes configs before trusting a green.
      (d) **Digest determinism:** the OCI image output path (and optionally the
          manifest sha256) is a deterministic function of the declaration — record
          the literal digest here once it first builds (cf. parking-lot kernel pin).

      *Explicitly NOT in M5 (later steps, don't pull early):*
      - **Running** the image — no container runtime / `docker run` behavioral
        assertion in the loop. That is the OCI *app model* proper; a later milestone.
      - The literal "store path == OCI digest" *equivalence* — that needs the
        content-addressed store + fs-verity thread (out of scope); M5 claims only
        *determinism* of the digest, not that it IS the store path.
      - Multi-arch / registry push / signing.
      - The marionette boot rung (M1–M3 assertions) is unchanged; the OCI rung is
        purely additive, exactly as M4's diff rung was.

      *Loop-latency flag (§1.3):* a full docker system image may exceed the ~60s
      warm-loop budget; the bit-for-bit `--check` likely belongs on the
      less-frequent rung, with the cheap derivation-level diff (b/c) in the fast
      path. Decide placement when implementing.

- [~] **M3+ — positive SSH login control (closes the M3 "denied ≠ usable" gap).**
      GREEN + verified-red, *awaiting sign-off (§4.3, security-adjacent).* M3 only
      proved password auth is *not advertised*; it never proved a legitimate login
      *works*. New assertion in `tests/boot.scm`: a committed throwaway ed25519
      keypair (`tests/keys/td_test_ed25519{,.pub}`, README marks it test-only) is
      authorized for an unprivileged `tester` account on a TEST-ONLY OS overlay
      (`%test-os` = `(inherit td-system)` + user + `modify-services` authorized-keys).
      The frozen `td-system` and its qcow2/OCI images are UNTOUCHED, so the M4/M5
      differentials and the shipped image carry no test account/key (no backdoor).
      The guest copies the (store-0444) privkey out, `chmod 600`, then logs in as
      non-root over publickey only (root + password both denied per M3), runs a
      command, and asserts exit 0 AND stdout reached us (sentinel `TD_LOGIN_OK` +
      `id -un` == `tester`). Boot rung now: 5 expected passes. VERIFIED-RED:
      authorizing a *different* pubkey → login refused → that one assertion FAILs
      (4 pass / 1 unexpected failure, builder exits 1, rung exits 2), then reverted.
      Commit: aa00716.

## Loop bedrock fix (pre-M4): the "single command" is now real

DESIGN §1.1 promises ONE pass/fail command, but `make check` alone didn't run —
it needed the ~6-line `guix shell -C --expose/--share … host-guix-on-PATH`
incantation (PLAN "How to run the loop"), or it went online and pulled
substitutes from nonguix.org (FSDG + offline violation). Baked that into
**`check.sh`** (+ `make container-check`). It also adds an **integrity guard**:
it refuses to run unless the host guix commit == the `channels.scm` pin, so the
loop can never silently download a different channel instance. `.DEFAULT_GOAL`
is pinned to `check` so the wrapper can't recurse into nested containers.
Canonical command is now just: `./check.sh`.

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
