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
- [x] **M4 — Typed config front-end compiling to gexps; differential: same store
      paths as the hand-written gexp.** GREEN + verified-red, **signed off
      2026-06-06 (DESIGN §4.3).** New `(system td-typed)`: a validated
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
- [x] **M5 — OCI image artifact: the declaration also lowers to a reproducible
      Docker/OCI image with a deterministic digest.** GREEN + verified-discriminating,
      **signed off 2026-06-06 (§4.3)** — crosses §2.3 "OCI app model".
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

- [x] **M3+ — positive SSH login control (closes the M3 "denied ≠ usable" gap).**
      GREEN + verified-red, **signed off 2026-06-06 (§4.3, security-adjacent).** M3 only
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

- [x] **M6 — manifest-driven, image-swap-only INTERFACE (DESIGN §6).** GREEN +
      verified-red, **signed off 2026-06-06 (§4.3) — extends the OCI layer M5 opened.**
      Makes image package contents a declarative function of a *manifest*: the
      intended way to change what the image contains is to declare a different
      manifest and rebuild the WHOLE image — a wholesale swap, never an in-place
      install. **Scope honesty (triage #4):** M6 proves the *build interface* is
      manifest-driven; it does NOT yet PROVE the *absence* of an imperative
      mutation surface — the built OCI image still ships `guix`/`guix-daemon`, so
      an in-image `guix install` remains physically possible. Removing/disabling
      that surface and asserting it with a negative runtime test is deferred to a
      later milestone (see DESIGN §parking-lot). Landed as three small increments,
      each leaving `make check` green:
      • **M6.1** (`da1ef9e`) — `(system td-typed)` gains a validated `manifest`
        field (a list of `<package>`), wired to the operating-system `packages`
        set. Default = `%base-packages` (the field's own default), so the default
        config stays byte-identical to the frozen oracle: `make diff` (system drv
        `z96c9kjj…`) and `make oci-diff` (OCI drv `8v1bdz2v…`) both still converge,
        unchanged. Validation has teeth (verified): a non-list manifest and a list
        with a non-`<package>` element are rejected at construction.
      • **M6.2** (`541875a`) — `tests/manifest-diff.scm` + `make manifest-diff`:
        self-discriminating differential on the OCI image drv (same shape as M4/M5).
        (a) default manifest converges to the oracle (`8v1bdz2v…`); (b) a manifest
        adding one package (GNU `hello`) lowers to a DIFFERENT OCI image
        (`zmv2j4zr…`) — a new whole-image generation; (c) `hello` is in the swapped
        system's package set and ABSENT from the default's (the manifest, and only
        the manifest, drives contents). VERIFIED-RED: a no-op swap (manifest ==
        default) makes (b)+(c) go `#f` → rung exits 2; reverted.
      • **M6.3** (`5da580d`) — `tests/manifest-image-drv.scm` + `make manifest-check`:
        builds the swapped (default + `hello`) OCI image and `guix build --check`s
        it. VERIFIED reproducible: drv `zmv2j4zr…` → output
        `1v54qv0jn8kl9jf90n7zkvjhkcmysmpz-docker-image.tar.gz`, `--check` rebuild
        showed no divergence. That output store-path IS the swapped generation's
        deterministic digest — a different generation from M5's default image
        (`4x2kvsbd8g…`), recorded here per the parking-lot digest convention.
      Loop wiring: `check: eval diff oci-diff manifest-diff build test oci
      manifest-check` (cheap derivation-level diffs fail fast; the swapped-image
      `--check` is the last/heaviest rung, §1.3). The qcow2 boot rung and the
      frozen `td-system` (+ its M5 default image) are UNTOUCHED — M6 is purely
      additive, like M4/M5 before it.

      ⚠️ **Hermeticity flag (for sign-off, CLAUDE.md §"human-reviewed").** The
      first reference to the swap package `hello` warmed it into the store. On
      this host the daemon's substitute-URL list includes `substitutes.nonguix.org`,
      so every build/diff that touches a not-yet-warm path *queries* nonguix (a
      pre-existing host artifact, NOT introduced by M6, and present for M1–M5 too).
      For `hello`: nonguix served NOTHING — the binary came from official
      `bordeaux.guix.gnu.org`, and `manifest-check` later built `hello` and the
      image LOCALLY from source. Verified: the swapped image drv AND output closures
      reference no nonguix paths (`guix gc --references`). Once warm, every M6 rung
      runs fully offline — the same warm-store/offline property the whole loop
      already rests on (see check.sh + "How to run the loop"). Open question for
      the human: whether the daemon's substitute config should drop nonguix entirely
      so a not-yet-warm path can never even *query* it. Not an M6 regression, but
      surfaced here because M6 is the first milestone to add a package outside the
      base system closure.

      *Explicitly NOT in M6 (later, don't pull early):* FHS-flattened OCI roots
      (DESIGN §6 — the other post-v0 thread; deferred in favour of this one);
      *running* a swapped image (`docker run` = OCI app model, §2.3); multi-package
      / specification-string manifests and manifest files on disk (M6 models the
      manifest as the typed `manifest` field — enough to prove swap semantics);
      generation history / rollback (the VM is ephemeral, §1.5 — "swap" here means
      a distinct reproducible image identity per manifest, not a persistent
      generation list).

- [x] **M7 — image-swap-only BY CONSTRUCTION: remove the imperative `guix install`
      surface (DESIGN §6 parking-lot).** GREEN + verified-red, **signed off
      2026-06-06 (§4.3), and the shipped default flipped to guix-free (see "M7
      promotion" below) — extends the immutability layer M6 opened.** M6 made image CONTENTS
      manifest-driven but explicitly left the imperative mutation surface in place:
      the built OCI image still shipped `guix`/`guix-daemon`, so an in-image
      `guix install` was physically possible. M7 removes it by construction.
      **Feasibility (verified first):** `guix` enters the system closure ONLY via
      `guix-service-type` (from `%base-services`) — NOT via `%base-packages` or
      `operating-system-packages` (probed). Deleting that service yields an image
      with zero guix binaries. Landed as two small increments, each leaving the
      loop green:
      • **M7.1** (`f2492b6`) — `(system td-typed)` gains a validated boolean
        `ship-guix?` field. When #f the compiler deletes `guix-service-type` via
        `modify-services`. Default = #t, so the default config stays byte-identical
        to the frozen oracle: `make diff` (system `z96c9kjj…`), `oci-diff` and
        `manifest-diff` (OCI `8v1bdz2v…`) all still converge, unchanged. `make
        typed-coverage` now proves 12/12 fields wired (ship-guix? #f diverges the
        system drv) + 17/17 invalid values rejected; the schema-derived denominator
        (`record-type-fields`) forced both new rows.
      • **M7.2** (`797efc0`) — `tests/imperative-surface.scm` + `make no-guix`
        (loop rung 11): builds the HARDENED image (default + ship-guix? #f),
        `guix build --check`s it bit-for-bit (drv `67gdky3m…` → output
        `faiarbq5ay0swizck81qbkh39plj1fbb-docker-image.tar.gz`, reproducible — a
        new guix-free generation, distinct from M5's default `4x2kvsbd8g…`), then
        cracks its `layer.tar` and asserts NO `/bin/guix` or `/bin/guix-daemon`
        (0 entries) while the DEFAULT image DOES ship them (4) — self-discriminating
        like manifest-check. VERIFIED-RED: flipping the helper to ship-guix? #t
        makes the "hardened" image the default (4 guix binaries) → the in-hardened
        ==0 assertion fails → rung exits 2; reverted.
      The qcow2 boot rung and the frozen `td-system` (+ its M5 default image) are
      UNTOUCHED — M7 is purely additive, like M4/M5/M6 before it.

      *Honest scope (for sign-off):* M7's claim is ARTIFACT-LEVEL — the guix binary
      is physically ABSENT from the hardened image, which is strictly stronger than
      a docker-run "guix install fails" check (a binary not in the image cannot
      run). Two things are deliberately NOT taken:
      - **Flipping the shipped default to hardened.** `ship-guix?` defaults to #t so
        the *default/shipped* image still ships guix and the §2.5 frozen oracle is
        preserved. Flipping the default to #f re-baselines that oracle (the M4/M5/M6
        digests change) — a spec decision for the human, not the agent. M7 proves
        the construction is available and correct; promoting it is sign-off work.
      - **A literal runtime `guix install` test** (docker-run the image, invoke
        guix, assert it is inert) — that needs the OCI app model (§2.3), still
        deferred. Artifact-absence substitutes for it and is stronger.
      *Explicitly NOT in M7:* FHS-flattened OCI roots (DESIGN §6, the other post-v0
      thread, still future); disabling guix on the bootable qcow2/VM path (M7
      targets the OCI image, where image-swap-only is the model — the VM keeps the
      daemon it needs to be a normal Guix system in v0).

      **M7 review remediation (post-M7 external review — F1 hardened across four
      rounds; F2/F3 fixed).**
      • **F1 (High) — ship-guix? #f was not a real guarantee.** Worked through four
        review rounds, each tightening the guarantee:
        – *Round 1:* the compiler only deleted guix-service-type; ship-guix? #f with
          a manifest LISTING `guix` still shipped it via `packages`. Added a
          constructor rejection by package NAME.
        – *Round 2:* a manifest package that PROPAGATES guix transitively bypassed
          the name check. Extended the constructor to walk each package's transitive
          propagated inputs (`manifest-has-guix?`), and added a verified regression
          row (`typed-coverage` now 19/19 rejected).
        – *Round 3:* review showed any STATIC check is fundamentally incomplete —
          guix still reaches the closure via a NON-propagated runtime reference, or
          via a RENAMED package inheriting guix (name ≠ "guix"). Demoted the static
          check to a fast-fail PRE-FILTER and added a CLOSURE-LEVEL BUILD GATE.
        – *Round 4 (current):* that gate was an OPT-IN docker helper — the public
          `td-config->operating-system` still lowered an UNGATED image, so a caller
          using bare `guix system image` got guix back; "by construction" was still
          false. **Resolution:** EMBED the gate. `td-config->operating-system`, for a
          #f config, prepends `guix-free-marker` (`(system td-hardening)`) — a
          build-time package whose build FAILS if guix is anywhere in the (other)
          packages' closure — to the system's `packages`. Since it lives in the
          package set, EVERY lowering (bare operating-system, qcow2, docker, any
          helper) builds the profile and therefore the marker, so a hardened image is
          guix-free OR it does not build, with no opt-in to skip. `make no-guix` now
          proves this on the BARE public path: the hardened image builds + is
          reproducible (`--check` of the gated artifact, also closing F2-round4 — the
          reproducibility check now targets the actual gated artifact) + tarball has 0
          guix; the #t control has guix; and the bare lowering of an adversarial
          manifest that smuggles guix past the pre-filter via a runtime reference
          FAILS at the embedded marker (verified-red, asserted against the marker's
          own diagnostic). F1 closed: manifest-agnostic, closure-level, no bypass.
      • **F2 (Medium) — no-guix required the SHIPPED image to stay guix-enabled.**
        The rung's positive control was the `$(SYSTEM)` image (asserted to contain
        guix), so promoting the shipped default to hardened would have reddened the
        rung. Fixed: the control is now an explicit `(td-config #:ship-guix? #t)`
        FIXTURE, independent of `$(SYSTEM)` — the rung proves the CONSTRUCTION
        (ship-guix? toggles the surface) regardless of what td ships, so it never
        blocks the promotion. Verified-red on the new structure (flip the hardened
        fixture to #t → 4 binaries → exit 2).
      • **F3 (Low) — DESIGN self-contradiction.** §2.4 said only M5/M6 implemented
        and surface removal was future, contradicting §6's "M7 implemented".
        Reconciled §2.4 to list M7 (artifact-level, default #t, pending sign-off).

## M7 promotion — shipped default flipped to guix-free (signed off 2026-06-06)

Human sign-off (§4.3) on the whole M4–M7 stack, AND the spec decision to **ship the
guix-free distro**: `ship-guix?` now defaults to **#f**. Because the single
`system/td.scm` declaration lowers to BOTH the bootable qcow2/VM and the OCI image, the
WHOLE distro is now guix-free by construction (the user explicitly chose "whole distro
guix-free", not OCI-only). All 11 rungs GREEN.

- **The flip.** `td-config`'s `ship-guix?` default #t → #f (`system/td-typed.scm`).
- **Oracle re-baselined (§2.5).** The frozen hand-written `td-system` (`system/td.scm`)
  was edited to the guix-free system — `(modify-services … (delete guix-service-type))`
  plus `(cons (guix-free-marker %base-packages) %base-packages)` — i.e. byte-for-byte
  what `td-config->operating-system` emits for a `#f` config. So `make diff` /
  `oci-diff` / `manifest-diff` still CONVERGE, now at guix-free digests. Bonus: the
  differential now *enforces* the marker on the oracle (drop it → diff reddens).
- **`typed-coverage` ship-guix? wiring row** flipped #f → #t (the divergent non-default
  value, since the oracle is now #f). 12/12 wired, 19/19 rejected — unchanged otherwise.
- **Real discovery (the VM-guix-free hurdle the old M7 note had deferred):** a guix-free
  Guix `operating-system` breaks inetd sshd — every connection reset at
  `kex_exchange_identification`. Root cause (from `/var/log/secure`): `sshd[…]: fatal:
  /var/empty must be owned by root and not group or world-writable.` `guix-service-type`
  had been creating `/var/empty` as `root:root 0755` as a side effect of its
  `guixbuilder` accounts (whose home is `/var/empty`); deleting it removed that. Fix:
  `guix-free-privsep-service` in `(system td-hardening)` (an `activation-service-type`
  snippet ensuring `/var/empty` root:root 0755), added to the `#f` path of BOTH the
  oracle and the typed compiler so they still converge. Diagnosed by booting the VM and
  dumping the guest's privsep user / `/var/empty` / syslog; confirmed against a guix-ful
  baseline (which passed 5/5) to prove it was the flip, not the environment. (Aside: an
  `init[1]` guile segfault appears early in every boot, INCLUDING the guix-ful baseline —
  pre-existing, harmless, in the initrd; not introduced here.)
- **Re-baselined digests (guix-free), per the parking-lot digest convention:**
  – system drv (oracle): `rxbyhfc70s7qldkcah0a8rf29z9pij6p-system.drv` (was guix-ful
    `z96c9kjj…`); perturbed ssh-port 2222 → `pb06pj1rvca71d7j0lb8ssmisgyllrmm`.
  – default OCI image drv (oracle): `d4fn2m2vf6rhhgvj4cish3023a7kvpp4-docker-image.tar.gz.drv`
    (was `8v1bdz2v…`); perturbed → `z9f9kjb0rp7y3r7adlr265qiizd5ppd4`.
  – default qcow2 output: `rgp5cdjpmjcg5jdzqp85gfc5byv8rhi6-image.qcow2` (reproducible).
  – default docker output: `n3ds4yhw5v49yi53426pc0sbmibc3dl7-docker-image.tar.gz`.
  – swapped (+hello) / no-guix hardened drv: `vkm5wlx6fl5ly3c11qplvall1ryhxd17-…` →
    output `z539zlhhj0r35lqj04zqn62z4xcazbr4-docker-image.tar.gz`.
  – no-guix CONTROL is now the explicit `(td-config #:ship-guix? #t)` fixture, whose OCI
    drv is the OLD guix-ful default `8v1bdz2v68gkbzybbaq4875a5flh2kvp` (4 guix binaries;
    hardened ships 0) — the F2-decoupled control, unaffected by the default flip.
- **Still NOT taken (unchanged):** FHS-flattened OCI roots (DESIGN §6, future); a literal
  docker-run `guix install` runtime check (needs the OCI app model, §2.3 — artifact
  absence is strictly stronger); promoting M5/M6/M7/M3+ from "extend" to numbered ladder
  rungs (DESIGN §2.4 — a separate spec decision, not part of this sign-off).

## Triage remediation (post-M6 external review)

An external review of the M6 work raised 6 findings; all triaged as valid and
fixed, each a small commit with verified-red where applicable. The loop got
materially more honest (it could previously pass while non-hermetic, while not
booting the shipped artifact, and with a rung that could not fail).

> **Note — refined by later review rounds.** This is the round-1 log. Two findings
> below were sharpened by subsequent reviews and now read differently in the live
> code/docs: the offline claim (1) was narrowed to "no substitutes + no remote
> offloading; cold fixed-output source fetches still possible" and every repl rung
> now also sets `#:offload? #f` (see check.sh "THE CONTRACT" + the bullet under
> "How to run the loop"); typed coverage (4) now proves **11/11** record fields
> (8 via drv-divergence + 3 structural), with the denominator introspected from the
> `<td-config>` record rather than a hand-kept count. The entries below are left as
> the historical record of round 1.

1. **(High) Loop was not offline/local-only.** Dropping `--network` never isolated
   the shared HOST daemon (`--share=/var/guix`), which has network + nonguix in its
   substitute URLs. Fixed: `check.sh` exports `GUIX_BUILD_OPTIONS=--no-substitutes`
   and every repl rung sets `(set-build-options store #:use-substitutes? #f)`. Full
   loop verified green with ZERO substitute/download lines. Commit 75e4917.
   *(Later narrowed: the guaranteed-by-construction property is "no substitutes +
   no remote offloading", not full network isolation — `--no-substitutes` does not
   stop a cold fixed-output source fetch by the shared daemon; repl rungs also set
   `#:offload? #f`.) Open for the human:* drop nonguix from the daemon's substitute
   config, and isolate its network, so a cold path cannot even *query*/fetch.
2. **(High) Reproducible qcow2 was never boot-tested.** The marionette `test`
   direct-kernel-booted `%test-os`, bypassing GRUB/partition/disk. Added
   `%test-td-disk-boot` / `make boot-disk`: boots the qcow2 via SeaBIOS→GRUB→kernel.
   Verified boot log + verified-red (wrong kernel ⇒ fail). Residual: image carries
   the marionette backdoor (not byte-exact); byte-exact boot would need a
   serial/ssh harness — documented follow-up. Commit 82a0106.
3. **(High) `eval` was false-green** (STDIN-piped `guix repl` swallows the exit
   code). Moved to `tests/eval.scm` run as `guix repl FILE`. Verified-red. Commit
   2e88b40.
4. **(Medium) Typed coverage only proved ssh-port.** Added
   `tests/typed-coverage.scm` / `make typed-coverage`: per-field WIRING sweep (8/8
   fields diverge the system drv) + VALIDATION sweep (16/16 invalid values
   rejected). Verified-red at the code level (un-wire a field, drop a check).
   bootloader-target/root-fs-type are validation-only by design (documented).
   Commit c9b9cf2. *(Later extended: the three drv-invisible fields
   (bootloader-target, root-mount, root-fs-type) gained a STRUCTURAL wiring sweep,
   so coverage is now 11/11 = 8 drv-divergence + 3 structural; and the denominator
   is introspected from the `<td-config>` record (`record-type-fields`) so a new
   field with no matching row reddens the rung before any sweep runs.)*
5. **(Medium) M6 proved the declaration, not the artifact.** `manifest-diff` (c)
   only checked `operating-system-packages`; reframed honestly, and
   `manifest-check` now cracks the built `layer.tar` and asserts
   `hello/bin/hello` is in the SWAPPED image and absent from the default. Also
   added `tar`+`gzip` to the sandbox toolchain (the first rung to need them).
   Verified-red. Commit 56b5c72.
6. **(Medium) Doc/scope drift.** Reconciled `make check` vs `./check.sh` across
   CLAUDE.md/DESIGN §1.1/Makefile header; marked M6 IMPLEMENTED in DESIGN §6 +
   added a §2.4 ladder note (promoting to numbered rungs is the human's spec
   call). The "M6 entry uncommitted" sub-point was already stale (778512b).
   Commit a703b0e.

The loop is now **11 rungs** (M7 added `no-guix`): `eval diff typed-coverage
oci-diff manifest-diff build test boot-disk oci manifest-check no-guix`. M4/M5/M3+/M6/M7
were **signed off 2026-06-06** (§4.3) and the shipped default flipped to guix-free
(see "M7 promotion" above); these fixes hardened the oracle those milestones were judged
against.

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

**Fix / canonical invocation — now `./check.sh` (DONE, triage #6).** The wrapper
below was the original hand-typed incantation; it is now baked into `check.sh`
(and `make container-check`), so the single command really is `./check.sh`. The
snippet is kept as documentation of *why* each flag is there:

```sh
HOSTGUIX_DIR=$(dirname "$(readlink -f "$(command -v guix)")")
guix shell -C --pure --no-substitutes --no-offload --expose=/gnu/store \
  --share="$HOME/.cache/guix" --share=/var/guix \
  make bash coreutils sed grep findutils tar gzip -- \
  bash -c "export PATH=$HOSTGUIX_DIR:\$PATH; \
           export GUIX_BUILD_OPTIONS='--no-substitutes --no-offload'; make check"
```

- `--expose=/gnu/store` — `-C` otherwise mounts only the profile closure, hiding the
  host guix binary's closure.
- `--share="$HOME/.cache/guix"` — pinned channel checkout (avoids re-fetch).
- `--share=/var/guix` — daemon socket + writable profiles/GC roots for time-machine.
- Putting the host guix (520785e) first on PATH makes the Makefile's `time-machine` a
  no-op that hits the warm store.
- Do **NOT** add `--network`: it pulls substitutes incl. nonguix.org (FSDG + local-only
  violation). The loop must stay offline.
- **No substitutes and no remote offloading by construction (triage #2, narrowed):**
  `--no-substitutes --no-offload` are passed to the OUTER `guix shell` itself (not
  only exported inside it), AND every repl rung sets `(set-build-options store
  #:use-substitutes? #f #:offload? #f)` (guix repl ignores GUIX_BUILD_OPTIONS), so
  even a cold environment build cannot query substitutes or offload to a remote
  builder — every realisation is LOCAL and substitute-free. **This is not a fully
  network-free guarantee:** the shared HOST daemon (`--share=/var/guix`) keeps its
  network, and `--no-substitutes` does not stop a *fixed-output* source derivation
  from fetching on a cold path. That residual is permitted by the hermeticity
  clause (CLAUDE.md prime directive 2: "offline except declared fixed-output
  fetches") and is suppressed in practice by the warm store + pinned-channel guard.
  Isolating the host daemon's network (or a pre-populated source closure) and
  dropping nonguix from its substitute URLs remain defense-in-depth follow-ups, not
  the primary guarantee.

## Loop reminder (CLAUDE.md)

eval → `guix build --check` → marionette test. Short-circuits on first failure. Don't
advance a sub-task until green. Small commits, each stating which test now passes.
`guix style` was tried in M2 and *rejected*: it mangled comments and produced layout
inconsistent with M1's hand-formatted files. Keep the readable hand-formatted 2-space
style that M1 established.
