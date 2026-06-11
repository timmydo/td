# Track: rootless-builder (side-track)

**Claim status:** see `PLAN.md` (the single source of truth for claims).
**Origin:** deferred from M10.1 (the generation image still builds via the daemon).
**Scope authority:** DESIGN §7.1.

## Goal

Build the target with a rootless user-namespace builder instead of the root
`guix-daemon`, and prove equivalence per prime directive 4: the existing daemon is
the oracle.

## Acceptance

A daemon-vs-rootless **store-path differential**: the same declaration built both
ways yields identical store paths (diff with `diffoscope` on mismatch), run as a
self-discriminating rung. Verified-red required (show a perturbation that makes the
paths diverge, or a deliberately broken rootless build that the rung catches).

## Constraints

- The loop stays offline (no substitutes, no offload) and hermetic.
- This track touches `check.sh`/`Makefile` to add its rung — adding a rung is free,
  but those are shared-spine files: land as a small standalone commit (DESIGN §7.3).

## Working state

Agent: claude-fable-ca67ec (claim 2026-06-11; status lives in PLAN.md).

### Design (settled by S1 probes, 2026-06-11)

The rootless builder is the **pinned guix-daemon itself, run unprivileged** in a
nested user namespace — not a new component. At the pin (520785e), a daemon
without `--build-users-group` adds `CLONE_NEWUSER` to every chroot build
(nix/libstore/build.cc:2734): genuine user-namespace isolation, no
`--disable-chroot` weakening. Both sides of the differential run the SAME
daemon binary (the host system profile's), differing only in privilege +
namespace — a single-variable experiment; the root daemon is the oracle
(prime directive 4).

Mechanics (all inside the check.sh sandbox, offline):

1. **DB snapshot**: `sqlite3 /var/guix/db/db.sqlite ".backup ..."` — consistent
   under sqlite's locking even while the host daemon writes (plain `cp` races).
2. **Writable store at the same path**: stage `$scratch/newstore`, bind each
   store item of the needed closures into it, then `mount --rbind` the staged
   tree over `/gnu/store` inside `unshare -m -U -r`. Store PATH stays
   `/gnu/store` (required for store-path equality); writes land in scratch;
   inputs stay write-protected by real inode permissions (host-root-owned).
   - Why not overlayfs: inside `guix shell -C` every profile item is an
     individual bind under /gnu/store; nested userns marks them MNT_LOCKED and
     overlay refuses such a lowerdir (EINVAL). Verified: overlay over
     /gnu/store works in a host userns (even depth 2) but never inside the
     loop's container. The staged-store approach needs no overlay at all.
3. **Daemon state**: `GUIX_STATE_DIRECTORY=$scratch/state` (snapshot DB),
   `GUIX_LOG_DIRECTORY=$scratch/log` (else it wants /var/log, ro),
   `--listen=$scratch/daemon.sock`; `/var/guix` covered with tmpfs inside the
   ns so the host daemon is unreachable by construction.
4. **Differential** = `guix build --check DRV` against the rootless daemon,
   where DRV's output was built by the root daemon (visible via the bind of
   its closure): same drv ⇒ same store path by construction; `--check` makes
   the daemon rebuild it rootlessly and compare bit-for-bit against the
   oracle's artifact. Divergence reds the rung.
5. **Bind closure** = `guix gc -R` of: the target drv, its output, each direct
   input's output paths (from `derivation-inputs`), the host guix package
   (client + daemon binaries), and `$GUIX_ENVIRONMENT` (sandbox tools survive
   the rbind shadowing). 471 paths / 1s bind loop for the trivial probe.

### Sub-task ladder

- [x] **S1 feasibility probes** — rootless daemon + staged store + `--check`
  green on a trivial gexp drv inside the check.sh-style sandbox. Evidence
  above; throwaway scripts under `.probe/` (not committed).
- [x] **S2 rung** — `tests/rootless.sh` + `tests/rootless-drvs.scm` +
  `rootless` rung in HEAVY_RUNGS: (a) validity guards (oracle output must be
  valid in the snapshot — else `--check` would BUILD instead of COMPARE, a
  false green; probe output must be INVALID — else the assertion would read
  another daemon's map); (b) isolation probe drv reads /proc/self/uid_map
  in-build; assert = exactly one line with a NON-ZERO first uid ("30001 30001
  1" at this pin) — rejects both the identity map (no userns) and an inherited
  "0 uid 1" map (e.g. a chroot-less build), the hole a plain
  not-identity-grep would leave; (c) the differential on the target image drv
  (the `build` rung's qcow2 system image): host-oracle build, rootless
  `--check --keep-failed`, plus an explicit output-path string equality
  assert. On mismatch the rung prints the kept `-check` path and the exact
  diffoscope command to run OUTSIDE the loop (diffoscope is a cold Python
  closure — not buildable offline inside it). GREEN 2026-06-11: image drv
  gklrdcy...-image.qcow2.drv rebuilt rootlessly, bit-for-bit equal at
  /gnu/store/m95hwxsa...-image.qcow2. Measured: 36s solo (incl. sandbox
  setup; bind closure 4536 items ~10s, DB snapshot ~1s, image rebuild ~6s).
  Sub-agent contract review: no violations; its robustness findings (daemon
  shutdown race, sqlite busy timeout, set -u guard) are applied.
- [x] **S3 verified-red** (all via temporary edits on top of the committed
  green S2; each reverted after the red was observed; run 2026-06-11):
  - **(A) cross-builder divergence — RED ✓.** A DISTINCT env-sensitive drv
    (`td-rootless-divergence-probe`, reads uid_map; named differently from
    the green probe so its host-built validity never leaks into later
    snapshots) was built by the ROOT daemon, and the rung's differential was
    temporarily pointed at it. Red exactly at the differential:
    `guix build: error: derivation '...-td-rootless-divergence-probe.drv'
    may not be deterministic: output '...' differs from '...-check'`, the
    rung's FAIL block printed the kept `-check` path + diffoscope command,
    exit 2. Contents: oracle build saw `0 0 4294967295` (root daemon, no
    userns), rootless rebuild saw `30001 30001 1` — a REAL daemon-vs-rootless
    divergence, caught. Host store cleaned with `guix gc -D` after.
  - **(B) on-disk tamper — instructive GREEN, not a hole.** Replacing the
    staged copy of the oracle image with a 4-byte-tampered copy did NOT red
    the rung. Source check (pinned nix/libstore/build.cc, bmCheck in
    registerOutputs): `--check` compares the REBUILD's NAR hash against
    `info.hash` — the hash the ROOT daemon recorded when it built the oracle,
    carried by the DB snapshot — not against on-disk bytes. So the rung's
    green run truthfully asserted rebuild == oracle hash; the anchor is
    tamper-evident BY not being the disk. bmCheck also throws on invalid
    outputs ("build it normally before using --check"), so build-instead-of-
    compare cannot silently green either (the rung's validity guard remains
    as an explicit, actionable precondition). tests/rootless.sh comments and
    PASS/FAIL wording corrected to say "NAR hash vs recorded oracle hash".
  - **(C) isolation — RED ✓.** Daemon temporarily started with
    `--disable-chroot`: the probe recorded `0 1001 1` (the build inherited
    the caller's namespace) and the uid_map-shape assertion redded with its
    diagnostic, exit 2. This red also validates the assert's strengthening:
    the first-draft "not the identity map" grep would have PASSED `0 1001 1`
    — a false green found by review and closed before commit.
- [ ] **S4 land** — exclusive-landing note: touches `check.sh` (adds
  `util-linux sqlite` to the sandbox packages) and `Makefile` (new rung in
  HEAVY_RUNGS). Small standalone commits.

### Open empirical questions

- Full qcow2 image drv rebuild under the rootless daemon (genimage path, no
  KVM expected) — timing + content equality. Probing before writing the rung.
- /etc/passwd "user with UID 0 not found" warning inside the ns — harmless in
  probes; keep an eye on it.
