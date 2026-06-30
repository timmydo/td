# HISTORY.md — completed-milestone record

The permanent record of finished milestones, moved out of the roadmap (2026-06-10) so
the working files stay small. Nothing here is open work: scope is governed by the
roadmap (`DESIGN.md` §7.1), status by the open PRs. Recorded digests live in
`DIGESTS.md`.

All milestones below are GREEN with a verified-red differential on record (we've seen
the rung actually fail before trusting a pass — the core lesson from M3). Sign-offs
under the original per-milestone gate (retired 2026-06-10, DESIGN §4.3): M4–M7 and M3+
on 2026-06-06; M8, M9, M9.3 on 2026-06-07; M10.1 on 2026-06-09; M10.2 and its review
round on 2026-06-10.

## Milestone ladder (DESIGN.md §2.4)

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
  found the behavioral rung had been false-green since M1 — see "Key lessons".)
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
  than a negative runtime test. The `no-guix` proof: hardened image builds, artifact
  reproducible, no `/bin/guix` in its `layer.tar` (the `ship-guix? #t` control still
  ships it). Re-baselining the shipped default to guix-free surfaced one real
  dependency `guix-service-type` had provided as a side effect — sshd's privsep dir
  `/var/empty` (root:root 0755, via the build-user accounts); restored by
  `guix-free-privsep-service`, proven by the boot rung (key-based SSH still logs in).
  Landed M7.1 (`f2492b6`), M7.2 (`797efc0`). Detail in `(system td-hardening)`.
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
  in the base, runs as guest root). Commits a339338, 8a72a56, f19d3d/f19dc3d (triage: assert
  the exact `73`, not a substring).
- [x] **M10.1 — Per-generation root + bootc-style generation image** (signed off
  2026-06-09). Built in two slices:
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
  - Deferred out of M10.1 into roadmap side-tracks: foreign-runtime LOAD verification
    (`oci-load`), the rootless user-namespace builder differential (`rootless-builder`).
- [x] **M10.2 — Guix-free, offline placer** (`place` rung; signed off 2026-06-10).
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

- [x] **M10.3 — manual rollback** (`rollback` rung; landed 2026-06-10, first roadmap
  entry under the §7 merge-on-green protocol — no per-milestone sign-off). The DESIGN
  §7.1 acceptance test, end to end: from a disk carrying two placed generations, boot
  generation 2 (the GRUB default), assert its identity, select generation 1 via the
  menu, reboot the SAME disk, assert the older identity, and prove the placed state
  persisted. What it took:
  - **Bundle (S1).** The generation image's userspace layer now packs the REAL
    (non-containerized) system closure (`initialize-root-partition` +
    `build-docker-image` — the disk-image root as an OCI layer): the stock docker
    lowering containerizes the OS (dummy kernel, services stripped), which is not
    honestly bootable. `boot/td-identity` gained `system=` (what the menu must
    gnu.system=/gnu.load=) and `root-uuid=` (deterministic `operating-system-uuid`);
    the checker proves system= equals the typed compiler's lowered system path AND
    exists inside the userspace layer (bootable, not bootable-looking).
  - **Placer (S2, still guix-free).** Menu entries carry `--id td-gen-N` and
    gnu.system/gnu.load from the identity; the managed block gained
    `search --label <boot-label>`, `set default=td-gen-<newest>`, and the
    MANUAL-ROLLBACK HOOK (`if [ -s /td/default.cfg ]; then source ...; fi` — rollback
    = write one line there and reboot; the placer never writes it). `--mkfs` turns the
    staged root.tar into a live ext4 `root.img` labeled `td-root-gen-N` with the
    identity UUID; `place-check.scm` verifies label/UUID straight from the superblock
    bytes (offsets 1024+0x38/0x68/0x78 — no tools).
  - **Disk (S3).** `system/td-disk.scm` assembles the placed tree into a raw MBR/GRUB
    disk (boot partition = placed `boot/` + GRUB modules, prefix `(hd0,msdos1)/grub`;
    one partition per generation using the placer's root.img verbatim;
    genimage + grub-bios-setup, as Guix's own disk images do).
  - **Test (S4).** One persistent qcow2 overlay booted TWICE through firmware->GRUB;
    per boot, identity is asserted three independent ways (cmdline root=<label>;
    st_dev of / == st_rdev of /dev/disk/by-label/<label>; /run/current-system ==
    that generation's system path). The first in-test guest REBOOT in the loop —
    persistence within a test is legitimate; isolation is the overlay dying with the
    build (CLAUDE.md prime directive 6).
  - **State model (S5; DESIGN §2.6, landed by the human mid-flight and absorbed at
    the pre-landing rebase).** Typed `persistent-paths` allowlist
    ((precious|disposable . path), default = the precious /var/lib/ssh entry, which
    a generation config must keep — cross-field rule); generation systems mount
    td-state (needed-for-boot — the initrd mounts it) and bind each entry from its
    tier directory; activation mints the SSH host key on first boot THROUGH THE
    BACKING PATH (it runs after the initrd's mounts, before shepherd's — bind
    sources are not root-prefixed by the initrd, so the binds are shepherd-mounted,
    and `user-processes`→`file-systems` ordering keeps sshd behind them); sshd
    HostKey relocated via extra-content. All generation-gated: the default config
    still lowers byte-identically to the frozen oracle. The rollback test asserts
    the model both ways across the swap (declared sentinel survives; undeclared
    write does not; /var/lib/ssh is BACKED by td-state on each boot; the gen-2
    host key is byte-identical under gen-1 — machine identity ≠ OS identity).
  - **Two traps found by the loop, fixed:** (1) Guix's initrd parses the WHOLE
    `root=` value as a bare label — the dracut-style `LABEL=` prefix is searched
    literally and never matches (first disk reached GRUB+kernel, then looped
    "waiting for partition"); the placer writes `root=<label>`. (2) bare `mke2fs`
    is NON-deterministic (superblock timestamps + random hash seed) — caught by
    `guix build --check` going red on the placed tree; td's qcow2 rung never exposed
    this because partition images are INPUTS to the drv that `--check` re-runs. Fixed
    with `SOURCE_DATE_EPOCH=1 E2FSPROGS_FAKE_TIME=1` + `hash_seed=<uuid>` + pinned
    top-dir mtimes; both the mkfs tree and the assembled disk now pass `--check`.
  - Verified-red: S1 R1–R3 (tampered identity system=/root-uuid=, userspace layer
    dropped from manifest), S2 R4–R11 (crossed gnu.system between entries, stripped
    search/default/hook lines, flipped superblock label/uuid/magic bytes, missing
    root.img/recorded state), S4 variant A (rollback act skipped → boot 2 is gen-2
    again: all three gen-1 identity asserts + both persistence asserts FAIL) and
    variant B (gen-2's menuentry vandalized in-guest → "menu still lists BOTH" FAILs).

- [x] **M11 — verified generations** (`rollback` rung grown to 36 asserts across
  THREE boots; landed 2026-06-11). The DESIGN §7.1 acceptance: a generation's root
  carries build-time integrity metadata; booting an intact generation succeeds while
  a corrupted root fails closed. Mechanism as settled: **dm-verity over the
  per-generation root image**, ChromeOS-style, plus the §2.6 tmpfs-root assembly
  (a root the boot path writes to cannot be sealed). What it took:
  - **Probes first (S0, the M8 lesson).** Pinned kernel: `CONFIG_DM_VERITY=m`,
    `CONFIG_BLK_DEV_DM=y`, but `CONFIG_DM_INIT` unset — the `dm-mod.create=`
    cmdline path is out; the device opens in userspace from the initrd. Guix's
    `cryptsetup-static` is built `--disable-veritysetup`, so td defines
    `veritysetup-static` ((system td-verity), 17 modest drvs, offline-buildable).
  - **Reproducible hash tree (S1).** The placer's `--mkfs` rounds the ext4 data
    area to whole 4 KiB verity blocks, APPENDS the hash tree (fixed salt
    "td-verity-salt-v0", the identity's deterministic UUID), self-verifies, and
    records `verity-roothash`/`verity-hashoffset` beside the boot files — the image
    cannot carry its own root hash (§2.7 self-reference). place-check (1d) verifies
    the verity superblock byte-level (magic/version/uuid/algorithm/block
    sizes/data-blocks/salt) and that the ext4 area covers the data blocks EXACTLY.
  - **The sealed boot (S2).** Generation systems now declare: `/` = tmpfs (no
    `root=` on the cmdline at all — activation assembles /etc, /run, /tmp); the
    generation image (now the gnu/store SUBTREE as ext4) opened in the initrd's
    pre-mount as `/dev/mapper/td-root` by a td mapped-device kind (static
    veritysetup; `td.roothash=`/`td.hashoffset=` parsed from the cmdline — written
    by the placer's menuentry; the data partition still found by the
    per-generation LABEL, the slot binding, now UNDER the verifying layer) and
    mounted READ-ONLY at /gnu/store, fail closed by construction (no fallback
    path). All generation-gated: the default config still lowers byte-identically
    to the frozen oracle (typed-diff re-proven). The reshaped rollback rung asserts
    per boot: recorded roothash on the cmdline (not the other generation's), no
    root=, store IS the verity device whose only slave is the labeled partition,
    / is tmpfs, every store mount ro down to the ext4 superblock, the rw remount
    REFUSED by the kernel, undeclared store write EROFS — plus all M10.3
    state-model/persistence asserts unchanged.
  - **A false-green caught by verified-red (the discipline paying rent):** the
    naive EROFS probe passed even on a deliberately UNSEALED variant —
    %base-file-systems' `%immutable-store` ro BIND (software convention,
    remountable) sits on /gnu/store of every Guix system. The probe now remounts
    the top bind rw and asserts the SUPERBLOCK rw remount is refused — only the
    read-only dm-verity device enforces that — separating §2.6 "enforcement" from
    "convention" in the test itself.
  - **Fail-closed acceptance (S3).** Boot 3: roll forward to gen-2, corrupt ONE
    sector of the first data block of gen-2's gnu.load boot script in the overlay
    (debugfs-located, guarded inside the verity data area; label and ext4
    superblock kept INTACT so discovery and mount succeed exactly as on a healthy
    system — only verification can catch it), boot: the kernel logs the dm-verity
    corruption signature and the system never assembles (no shepherd — a marker
    boot 2 positively proves on the same serial channel). First attempt corrupted
    the superblock instead and failed closed at LABEL DISCOVERY — a failure an
    unsealed system shares, i.e. not integrity evidence; retargeted.
  - Verified-red: R12–R23 (S1 byte-level tampers), variant E (swapped expected
    roothashes → exactly the two cmdline asserts), menu tampers V1–V3, the
    unsealed variant H (exactly the 12 sealing asserts across both boots),
    generation-diff (d) read-only-drop, variant I (corruption neutralized →
    exactly the fail-closed assert).

## Key lessons (full narrative — condensed normative versions live in CLAUDE.md)

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
  guard. Open follow-up: the `offline-isolation` roadmap side-track. (First surfaced in
  M6, the first milestone to add a package outside the base closure; nonguix served
  nothing for `hello`.)
- **check.sh is the single command.** It bakes in the `guix shell -C` incantation that
  plain `make check` needs (toolchain packages, host guix first on PATH so the Makefile's
  `time-machine` is a warm-store no-op rather than a download, store/cache/daemon
  exposure) plus an integrity guard that refuses to run unless host guix == the
  `channels.scm` pin. Don't add `--network` — it pulls substitutes incl. nonguix (FSDG +
  offline violation).
- **`guix style` is rejected.** Tried in M2; it mangled comments and broke M1's
  hand-formatted layout. Keep the readable hand-formatted 2-space style.
- **Retired investigations.** Auto-rollback via one-shot GRUB (the manual-rollback model
  doesn't need it); FHS-flattening the base (M9: in a "minimal base, apps in containers"
  design, FHS is a property of app images — now the `fhs-app-images` side-track).
