# Running td's loop in a cloud / web session

`./check.sh` is td's only pass/fail command, but it assumes the host **is** a
Guix System pinned to `channels.scm`. A fresh cloud container (Claude Code on the
web, CI) has no Guix at all. These scripts provision that toolchain so the loop
becomes runnable, **without weakening the loop** — `check.sh` still runs offline
(`--no-substitutes`); we only pre-populate the store while the network is up.

## Pieces

| File | Role |
|------|------|
| `scripts/cloud-guix-setup.sh` | Idempotent, phased provisioner: install Guix → start daemon → pin host guix to `channels.scm` → (optional) warm store. |
| `scripts/cloud-guix-warm.sh` | Heavy phase 4: runs the loop ONCE with substitutes on so `/gnu/store` carries every rung's closure; then `check.sh` finds it all locally. |
| `.claude/hooks/session-start.sh` | SessionStart hook (web only) that runs the provisioner. |
| `.claude/settings.json` | Registers the hook. |

## How check.sh's preconditions are met

`check.sh` needs: (1) a `guix` whose `guix describe` equals the pinned commit,
(2) a running `guix-daemon` under `/var/guix`, (3) a `/gnu/store` warm enough to
build every rung offline, (4) the pinned channel checkout under `~/.cache/guix`,
(5) a non-loopback interface (the offline-isolation control). The provisioner's
phases map 1:1 onto these.

## What was validated here (2026-06-11, Ubuntu 24.04 cloud box)

- `apt install guix` → Guix 1.4.0 daemon + `_guixbuild` users. ✅
- `/gnu/store` created, `guix-daemon` started by hand (no systemd in container). ✅
- `guix build hello` substituted from `bordeaux.guix.gnu.org`. ✅ (network is up
  at setup time; `ci.guix.gnu.org` was unreachable, `bordeaux` works — hence the
  `TD_SUBSTITUTE_URLS` default).
- `guix pull --commit=<pin>` — the slow step; validated as the mechanism to make
  `guix describe` match the pin that `check.sh` hard-guards.

## Known environment limitations (carry into the loop honestly)

- **No `/dev/kvm`** in typical containers → the boot/rollback/reset/disk rungs run
  QEMU under TCG (software emulation): correct but slow. Budget accordingly.
- **`/sys/fs/cgroup` is tmpfs, not cgroup2** → the `run` rung (rootless `crun`)
  may fail its cgroup probe. If so, that is an environment gap, not a td
  regression — do not "fix" it by weakening the rung.
- **Disk** — a fully warm store with VM images approaches the box's free space
  (~30 GB observed). Watch `df`.
- **Time** — `guix pull` to an old commit + a full warm can exceed a SessionStart
  hook's budget. The container state is cached AFTER the hook, so the cost is
  paid once; but for reliability prefer baking the heavy phases into a **custom
  environment image** and letting the hook only start the daemon + verify the pin.

## Recommended split

1. **Custom environment image** (one-time, persisted): run
   `TD_WARM=1 scripts/cloud-guix-setup.sh` to bake guix + the pin + a warm store.
2. **SessionStart hook** (every session, fast): the provisioner finds guix
   already pinned and just (re)starts the daemon.

## Manual run

```sh
# full provision incl. warm (heavy):
TD_WARM=1 scripts/cloud-guix-setup.sh
# then the normal loop:
guix describe          # must print the pinned commit
./check.sh eval        # cheap smoke rung
./check.sh             # full loop
```
