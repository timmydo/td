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
- [ ] **S2 rung** — `tests/rootless-*.{sh,scm}` driver + `rootless` rung in
  HEAVY_RUNGS: (a) isolation probe drv reads /proc/self/uid_map in-build and
  asserts a non-identity single-uid map (proves builds really run in a userns;
  self-discriminating — a daemon falling back to plain chroot reds it);
  (b) the differential on the target image drv (the `build` rung's qcow2
  system image): host-oracle build, rootless `--check`, plus an explicit
  output-path string equality assert. On mismatch: --keep-failed + print both
  paths (diffoscope is a cold Python closure — can't build it offline inside
  the loop; mismatch diagnosis runs it OUTSIDE the loop on the kept
  `-check` dir; the rung prints the exact command).
- [ ] **S3 verified-red** — (A) cross-builder divergence: a DISTINCT
  env-sensitive drv (reads uid_map; named differently from the green probe so
  its host-built validity never leaks into later snapshots) built by the ROOT
  daemon, then rootless `--check` must red "may not be deterministic".
  (B) comparator: tamper the staged copy of the oracle output, `--check` must
  red. (C) isolation assert: point it at a root-daemon-built map → red.
- [ ] **S4 land** — exclusive-landing note: touches `check.sh` (adds
  `util-linux sqlite` to the sandbox packages) and `Makefile` (new rung in
  HEAVY_RUNGS). Small standalone commits.

### Open empirical questions

- Full qcow2 image drv rebuild under the rootless daemon (genimage path, no
  KVM expected) — timing + content equality. Probing before writing the rung.
- /etc/passwd "user with UID 0 not found" warning inside the ns — harmless in
  probes; keep an eye on it.
