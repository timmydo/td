# PLAN.md — working todo / plan (persists between iterations)

Working scratchpad for the td build loop. Source of truth for *scope* is `DESIGN.md`
§2.4 (the milestone ladder); this file tracks *where we are* on it. Recorded digests
and commit hashes are kept as a reproducibility record (see the bottom of this file).

## Milestone ladder status (DESIGN.md §2.4)

All milestones below are GREEN with a verified-red differential on record (we've seen
the rung actually fail before trusting a pass — the core lesson from M3). M4–M7 and M3+
were signed off 2026-06-06 (§4.3); M8, M9, M9.3 were signed off 2026-06-07.

- [x] **M1 — Closed loop on a trivial image** (§2.1). `make check` green end to end:
  eval → reproducible qcow2 → marionette boot asserts `uname -r` == declared kernel
  (6.18.15-gnu). Commit 5ed0903.
- [x] **M2 — Service up + port listens.** td-system declares `openssh-service-type`
  (+ `dhcpcd-service-type` for sshd's `'networking`); boot test asserts the
  `ssh-daemon` unit is running and port 22 (derived from `td-ssh-configuration`) is
  listening. Commit e02ea83.
- [x] **M3 — Default-deny hardening on sshd.** `password-authentication? #f` (the
  honest lever; root-login already `#f`) + `challenge-response-authentication? #f`.
  Test asks the daemon its auth methods and asserts no password method is offered;
  verified-red by flipping password-auth back on. The ssh client runs by absolute
  store path, so the image gains no test-only tools. Commit cf78c4a. (While doing M3,
  found the behavioral rung had been false-green since M1 — see "Loop-integrity fixes".)
- [x] **M4 — Typed config front-end.** New `(system td-typed)`: a validated `td-config`
  record + smart constructor, and a compiler `td-config->operating-system` that rebuilds
  the system. The hand-written `td-system` stays FROZEN as the oracle (§2.5). `make diff`
  is self-discriminating: the default config lowers to the same `system.drv` as the
  oracle; a perturbed config (ssh-port 2222) lowers to a different one. The image
  derivation is unchanged — the front-end is purely additive. Commit 465a6ea (bedrock
  fix d6a1220).
- [x] **M5 — OCI image artifact.** The same `system/td.scm` that boots as a VM also
  lowers to a reproducible Docker/OCI image (`image-with-os docker-image` + `system-image`
  = `guix system image -t docker`). Two rungs: `oci-diff` (cheap derivation-level
  differential, self-discriminating) and `oci` (`guix build --check` the image
  bit-for-bit). The output store path is the deterministic digest. Crosses the §2.3 OCI
  line. Commit 66494ca. (Out of M5, deferred: running the image; literal store-path==digest
  equivalence, which needs fs-verity; FHS-flattened roots.)
- [x] **M3+ — Positive SSH login control.** M3 proved password auth isn't advertised but
  never that a legitimate login works. A committed throwaway ed25519 key
  (`tests/keys/`, marked test-only) authorizes an unprivileged `tester` on a TEST-ONLY OS
  overlay; the frozen `td-system` and its images are untouched (no backdoor in the shipped
  artifact). The guest logs in over publickey only (root + password denied), runs a
  command, asserts exit 0 + sentinel. Verified-red by authorizing a different key. Commit
  aa00716.
- [x] **M6 — Manifest-driven, image-swap-only interface.** The image's swappable package
  payload is a function of a typed `manifest` field; changing it means declaring a
  different manifest and rebuilding the whole image (a wholesale swap, never an in-place
  install). Effective packages = fixed base capabilities (e.g. crun) + manifest payload +
  enforcement markers; the base capabilities are a manifest-independent invariant.
  Landed as M6.1 (`da1ef9e`, the validated field), M6.2 (`541875a`, the self-discriminating
  `manifest-diff` rung), M6.3 (`5da580d`, `manifest-check` builds the swapped image and
  `--check`s it). Scope honesty: M6 proves the build *interface* is manifest-driven; it
  does NOT yet remove the imperative surface (the image still shipped guix) — that's M7.
  The constructor's name check walks propagated inputs and rejects direct + propagated
  guix/crun, but a renamed clone is provably uncatchable by a name scan and is documented
  as permitted payload (it can't remove an injected capability).
- [x] **M7 — Guix-free by construction.** Removes the imperative `guix install` surface:
  the typed `ship-guix?` field, when `#f`, deletes `guix-service-type` and the image
  carries no guix binary. Because a static name check can't catch every way guix enters a
  closure (propagated input, plain runtime reference, renamed package, or a *service*),
  the guarantee is **two layers** (arrived at over several review rounds): (1) an embedded
  build gate — `guix-free-marker` in `packages`, built on every lowering, fails if guix is
  in the manifest packages' closure (manifest-scoped); (2) a whole-system gate —
  `guix-free-system-gate`, a derivation over the entire system closure that catches
  service-injected guix, applied by `make no-guix` over the shipped `td-system` (can't be
  embedded — it would reference the system containing it). `no-guix` proves both on the
  bare public lowering, with an adversarial-manifest and a service-injection fixture each
  verified-red against the gate's own diagnostic. An absent binary can't run — stronger
  than a negative runtime test. Landed M7.1 (`f2492b6`), M7.2 (`797efc0`). Detail in
  `(system td-hardening)`.
- [x] **M8 — Run the shipped OCI image as a real container.** M5–M7 prove properties of
  the artifact; none ran it. M8 executes the shipped guix-free image as a rootless OCI
  container and asserts its userspace runs (positive sentinel + negative control). Runtime
  chosen by probing: **crun** (18 derivations, offline-buildable) over podman (1238
  derivations + 290 cold fetches — breaks the offline loop). The `run` rung is not a
  derivation (running a container needs a live userns the build sandbox forbids), so like
  `docker run` it executes in the loop shell against the freshly built image
  (`tests/run-image.sh`). Two environment facts: the sandbox grants a single uid, and
  `/sys/fs/cgroup` inside `-C` is plain sysfs, so check.sh exposes the host cgroup2 mount.
  Finding: an unbooted image has `/bin` empty (FHS conveniences are materialized at boot by
  activation), so the rung execs a shell at its store path.
- [x] **M9 — The booted base is an OCI container host.** Supersedes FHS-on-base (in a
  "minimal base, apps in containers" design, FHS belongs to the app images). M9.1: ship
  `crun` in the base and mount cgroup2 at `/sys/fs/cgroup`, edited identically in the
  oracle and the typed compiler via a shared `cgroup2-file-system` (prevents drift); the
  differentials self-rebaselined and the boot test asserts "cgroup2 mounted + crun shipped".
  M9.2 (`container` rung, `tests/container.scm`): boot the base and run a Guix-built OCI app
  image (`docker-image` of GNU hello) with the shipped crun as root, asserting it prints
  `Hello, world!`; the app runs directly off the read-only store rootfs (copying the ~70MB
  closure into the guest overflowed its tmpfs). M9.2 hardening added a second app image with
  a bogus *declared* entrypoint (proving image metadata drives the run) and a structured
  JSON entrypoint parse.
- [x] **M9.3 — Managed cgroups: crun ENFORCES a declared limit.** M9.2 ran crun with
  `--cgroup-manager=disabled`, proving it starts/runs but not that limits take effect.
  M9.3 runs crun with the `cgroupfs` manager, applies `pids.limit = 73` via the OCI config,
  and has the container read its own `/sys/fs/cgroup/pids.max` and print it. Self-
  discriminating by construction: cgroup2's default `pids.max` is `max`, so reading exactly
  `73` can only happen if crun applied the limit. Verified-red by switching back to
  `--cgroup-manager=disabled` (no cgroup created → read fails). No check.sh change (crun is
  in the base, runs as guest root). Commits a339338, 8a72a56, f19dc3d (triage: assert the
  exact `73`, not a substring).

## M10 forward plan — bootc-style generations (GATED)

The north-star "atomic verified generations" thread, **re-scoped 2026-06-08 to the
simplest useful form: manual rollback.** Full design in `M10-design.md`. A generation is
a bootc-style bootable OCI image; you build it with Guix, place it, add a GRUB entry, and
roll back by booting an older entry — essentially Guix's own generation menu. No boot
counters, no health agent, no auto-commit. This crosses DESIGN §2.3 ("verified
generations"), so it is **gated on §4.3 sign-off before any implementation.**

Three decisions that still hold from the scoping discussion:
1. **State boundary: define, don't abandon.** "Nothing persists across test runs" (§3) is
   test-isolation (fresh disk per test), not a ban on persistence *within* a test. M10
   introduces guest-persistent state (the generation list / boot selection) for the first
   time, so the writable-vs-immutable boundary gets defined — but in the manual model it's
   small (a GRUB menu + extracted roots).
2. **Oracle = Guix builds the bundle** (reproducible, `--check`). The deployment side
   (place + menu update) is tested against behavior, not diffed against a Guix component
   it doesn't have.
3. **Integrity ≠ security.** Corruption detection is ordinary functional behavior;
   authenticity, signatures, and anti-rollback are a separate later security milestone.

Sub-ladder (gated):
- **M10.1 — DONE** (signed off 2026-06-09). Built in two slices:
  - *Per-generation root* (`generation-diff` rung). The typed `generation` field derives a
    distinct, bootloader-selectable root label (`<root>-gen-<n>`) per generation, replacing
    the single shared `td-root` (`system/td.scm:57`). generation #f still converges to the
    frozen oracle, so the M4/M5/M6 differentials hold. Without this every entry mounts the
    same filesystem and rollback is a no-op (the P1 crux).
  - *Bootc-style generation image* (`generation-image` rung, `system/td-generation.scm`).
    td's OCI lowering emits userspace only; this APPENDS a /boot layer (kernel + initrd from
    the same OS) to that reproducible image, producing one OCI image carrying a bootable
    kernel+initrd. The initrd is built from this generation's OS, so it mounts that
    generation's distinct root. The rung `--check`s reproducibility of two generations and
    cracks the layers to assert /boot is present (and absent from the plain userspace image).
  - Open follow-on (not blocking M10.1): the image is structurally a valid 2-layer OCI image
    but is not yet verified to LOAD into a foreign runtime (podman/docker) — distribution is
    an M10.2+ concern since we ship our own placer; and the build still uses the daemon, not
    the rootless user-namespace builder (deferred, its own slice — needs a daemon-vs-rootless
    store-path differential per prime directive 4).
- **M10.2 — DONE** (signed off 2026-06-10). Guix-free, offline placer (`place` rung).
  `system/td-place.sh` is a POSIX shell tool that runs ON THE TARGET (no guix): it cracks
  a bootc generation image, extracts that generation's kernel+initrd into its OWN
  per-generation root (`<boot>/td/gen-N/`, recording the gen's root label), prunes the
  placed generations to the newest `--keep`, and regenerates a marker-delimited managed
  block of GRUB menuentries — one per kept generation, each `linux`/`initrd` pointing at
  THAT generation's files and selecting THAT generation's root (`root=LABEL=td-root-gen-N`),
  newest first; the user's grub.cfg preamble (outside the markers) is preserved.
  `system/td-place.scm` runs it inside a derivation whose builder PATH is ONLY base tools
  (NO guix), so a successful build PROVES the placer guix-free BY CONSTRUCTION (guix absent
  from the sandbox — the same "absent → cannot be used" guarantee as `no-guix`), and the
  placed target tree is `guix build --check`-reproducible. Behavioral, not diffed against a
  Guix component it lacks (M10-design.md decision 2): `tests/place-check.scm` cracks two
  trees — PLACE (gens 1,2 keep 10) and PRUNE (gens 1,2,3 keep 2) — and asserts per-gen
  placement, distinct initrds, the per-gen menu + root selection (from the typed compiler,
  no drift), preamble preservation, and that pruned gen 1 leaves no root dir AND no menu
  entry. Verified-red on BOTH crux properties: breaking prune (keep-all) reddens the PRUNE
  scenario; writing a shared `td-root` for every gen reddens the root-selection checks.
  - **M10.2 review round (2026-06-10) — signed off.** Addressed 6 findings. P1: (1) the placer now
    APPLIES the userspace layers and stages each generation's root CONTENT at
    `roots/td/gen-N/root.tar` — so `root=LABEL=td-root-gen-N` refers to a root that exists
    (creating the labeled ext4 from it is deferred to M10.3 — signed-off scope split, Option
    A); (2) each generation is extracted+validated in a staging dir and only ATOMICALLY
    swapped in, so a corrupt image no longer destroys the generation already installed;
    (3) `--keep 0` is rejected (it would prune every generation). P2: (4) the bootc image
    carries `boot/td-identity` (generation + root label) and the placer VERIFIES it against
    `--generation`/`--root-label`, so a mislabeled image can't be placed; (5) layer selection
    is MANIFEST-DRIVEN (orphan layers ignored); (6) `place-check.scm` now parses the managed
    block into PER-MENUENTRY bodies and asserts each generation's directives live in its own
    entry with no foreign ones (a directive swap between entries now reddens). Verified-red:
    standalone fake-image harness flips each guard (keep-0, atomic, identity, manifest) to
    FAIL when mutated out; the per-entry checker flags a swapped block (4 fails) and a missing
    root.tar, and is gen-1/gen-10 boundary-safe.
- **M10.3** — manual-rollback test: boot generation N, then boot N-1 from the menu, assert.
  (Open follow-ons the placer left for M10.3: turn each staged `roots/td/gen-N/root.tar` into
  a live ext4 filesystem LABELED `td-root-gen-N` (`mke2fs -d -L`, reproducible UUID/hash_seed);
  the menuentry path is GRUB-root-relative `/td/gen-N/...` and root selection is by label —
  wiring the GRUB `search`/`set root` for the target partition layout, `set default`, and the
  gnu.system/gnu.load closure path is M10.3's job, as is target-persistent state across boots.)

Deferred: auto-rollback (boot counter + health agent); verified/dedup generations
(ostree/composefs/fs-verity); registry push/signing. The earlier auto-rollback
investigation (one-shot GRUB) was retired — the manual model doesn't need it.

## Key lessons and standing notes

- **Verified-red discipline.** A green behavioral rung is only meaningful once you've SEEN
  it go red. Always break the thing and watch the test fail before trusting a pass. This
  caught the M1–M3 false-green (three compounding defects: `system-test-runner` was
  unbound; `guix repl` reading from STDIN swallows the exit code so a failed build exited
  0; guest forms lacked their `(ice-9 …)` imports so `marionette-eval` silently returned
  `#f`). All fixed; the rung now goes red honestly.
- **Offline posture (precise).** The loop guarantees *no substitutes + no remote
  offloading*, not full network isolation. `check.sh` passes `--no-substitutes
  --no-offload` to the outer `guix shell` and every repl rung also sets
  `#:use-substitutes? #f #:offload? #f` (repl ignores `GUIX_BUILD_OPTIONS`). The shared
  host daemon keeps its network, and `--no-substitutes` doesn't stop a cold fixed-output
  *source* fetch — permitted by the hermeticity clause (offline except declared
  fixed-output fetches) and suppressed in practice by the warm store + pinned-channel
  guard. Open follow-up: drop nonguix from the daemon's substitute URLs and isolate its
  network so a cold path can't even query. (First surfaced in M6, the first milestone to
  add a package outside the base closure; nonguix served nothing for `hello`.)
- **check.sh is the single command.** It bakes in the `guix shell -C` incantation that
  plain `make check` needs (toolchain packages, host guix first on PATH so the Makefile's
  `time-machine` is a warm-store no-op rather than a download, store/cache/daemon
  exposure) plus an integrity guard that refuses to run unless host guix == the
  `channels.scm` pin. Don't add `--network` — it pulls substitutes incl. nonguix (FSDG +
  offline violation).
- **`guix style` is rejected.** Tried in M2; it mangled comments and broke M1's
  hand-formatted layout. Keep the readable hand-formatted 2-space style.

## Recorded digests (reproducibility record)

Per the parking-lot digest convention (DESIGN §6), the shipped artifacts' deterministic
outputs. Current baseline is guix-free (`ship-guix?` defaults to `#f` since the 2026-06-06
sign-off; the single `system/td.scm` lowers to both the qcow2/VM and the OCI image, so the
whole distro is guix-free). The frozen oracle was re-baselined by editing `system/td.scm`
to exactly what `td-config->operating-system` emits for a `#f` config — delete
`guix-service-type`, add `guix-free-marker`, add `guix-free-privsep-service` — so the
differentials still converge, now at guix-free digests, and the differential itself
enforces the marker on the oracle.

- system drv (oracle): `rxbyhfc70s7qldkcah0a8rf29z9pij6p-system.drv`; perturbed
  (ssh-port 2222): `pb06pj1rvca71d7j0lb8ssmisgyllrmm`.
- default OCI image drv (oracle): `d4fn2m2vf6rhhgvj4cish3023a7kvpp4-docker-image.tar.gz.drv`;
  perturbed: `z9f9kjb0rp7y3r7adlr265qiizd5ppd4`.
- default qcow2 output: `rgp5cdjpmjcg5jdzqp85gfc5byv8rhi6-image.qcow2`.
- default docker output: `n3ds4yhw5v49yi53426pc0sbmibc3dl7-docker-image.tar.gz`.
- swapped (+hello) / no-guix hardened drv: `vkm5wlx6fl5ly3c11qplvall1ryhxd17-…` → output
  `z539zlhhj0r35lqj04zqn62z4xcazbr4-docker-image.tar.gz`.
- no-guix control: the explicit `(td-config #:ship-guix? #t)` fixture, OCI drv
  `8v1bdz2v68gkbzybbaq4875a5flh2kvp` (4 guix binaries; hardened ships 0) — decoupled from
  the shipped default so promoting the default never reddens the rung.

The privsep discovery behind the re-baseline: a guix-free system breaks inetd sshd
(`/var/empty must be owned by root and not group or world-writable`) because
`guix-service-type` had created `/var/empty` (root:root 0755) as a side effect of its
build-user accounts. `guix-free-privsep-service` restores it; the boot rung proves
key-based login still works.

## The loop (reminder)

16 rungs: `eval diff typed-coverage oci-diff manifest-diff generation-diff build test
boot-disk oci manifest-check generation-image place no-guix run container`. eval →
differentials → `guix build --check` → marionette tests, short-circuiting on first failure.
Don't advance a sub-task until green. Small commits, each stating which test now passes.
