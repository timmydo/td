# Design document — td

A functional Linux distribution, built incrementally by an AI coding agent (Claude
Code) on top of an existing Guix system, growing inside a fast, machine-checkable
verification loop.

This document is the settled contract the agents work against. It pins the three
things an agent can't decide for itself — **the loop it runs to check its work**,
**the target it's aiming at**, and **the scope it may work without sign-off** (the
§7.1 roadmap). Everything else the agents may propose and iterate on. Section numbers
are stable anchors; `CLAUDE.md` and `PLAN.md` reference them, so keep them. Keep
`CLAUDE.md` in sync with §1, §3, and §7.2–7.3.

---

## 0. North star and v0 scope

**Eventual:** a content-addressed, reproducible, immutable distro where the store path
doubles as `fs-verity` root and OCI digest, with one Rust sandbox stack spanning build
and run, a typed config front-end, and atomic verified generations.

**v0:** the smallest vertical slice that closes the full verification loop on top of
stock Guix — one `system/td.scm` declaration that builds reproducibly into a bootable
image, boots in a VM, and passes one behavioral assertion (kernel version).

---

## 1. The loop *(this section comes first — nothing else matters until it's settled)*

### 1.1 The single pass/fail command

`./check.sh` is the one command that means green or red. It sets up the hermetic,
offline sandbox (a fresh `guix shell -C --pure`, store and daemon-socket exposure, a
guard that host guix matches the `channels.scm` pin, substitutes disabled) and runs
`make check` inside it. `make check` runs, short-circuiting on the first failure:
config eval → the typed/OCI/manifest differentials → `guix build --check` on
`system/td.scm` → the `tests/boot.scm` marionette test → the manifest-swap
reproducibility rung. Plain `make check` is only correct when you're already inside
that sandbox.

### 1.2 Rungs committed for v0

- Hermetic build/dev env: `guix shell -C --pure` (no host leakage).
- Reproducibility oracle: `guix build --check` (and `--rounds=2` where cheap). A
  non-reproducible output is a failing test.
- Boot + behavioral: one marionette `(gnu tests)` system test that boots a
  `guix system vm` and drives the guest from Guile.

Fuzzing/adversarial and real-hardware rungs are deferred to later milestones.

### 1.3 Loop-latency budget

Target under ~60s per write→check cycle on a warm store. To stay there, layer changes
onto a prebuilt base image and reserve full `guix system vm` rebuilds for a
less-frequent rung. Loop latency is a tracked metric, not an afterthought.

### 1.4 Agent / container boundary

The Claude Code agent runs **outside** the container. Every build/test command it
issues enters a **fresh** `guix shell -C --pure` container, so the agent's own
environment can't contaminate results and the reproducibility rung stays honest.

### 1.5 VM state reset

v0 is **fully ephemeral**: boot from a fresh image per test, nothing persists across
runs, all writable state is wiped on reset. Upgrade path, when loop latency demands
it: QEMU `qcow2` overlay / CoW reset for cheap repeated runs.

---

## 2. The target

### 2.1 v0 acceptance test

`system/td.scm` builds reproducibly (`guix build --check` passes) into a bootable
image; `tests/boot.scm` boots it and asserts that `uname -r` in the guest equals the
kernel version pinned by the declaration; then the harness resets the ephemeral VM. v0
is done when this passes, reproducibly, and is committed.

### 2.2 Reused vs. built for v0

Keep `guix-daemon`, `/gnu/store`, and Guile + gexps as the v0 config language. No
typed front-end yet — changing the config language and building the OS at once is two
hard projects fused into one, and it blinds the agent's cheapest rung. The typed layer
is a later milestone that compiles down to gexps.

### 2.3 Scope rule *(redefined 2026-06-10)*

**In scope = on the approved roadmap (§7.1). Everything else is out of scope — STOP
and ask; never expand scope on your own.** Naming the boundary is what stops an agent
boiling the ocean.

(History: the original v0 exclusion list did this job; every crossing was gated on
§4.3 sign-off, as M4–M10.2 were. Of that list, the typed front-end, the OCI app
model, and verified generations have since been crossed deliberately or sit on the
roadmap; the Rust build daemon, the unified sandbox/portal broker, multi-machine
tests, and real-hardware/driver work remain off-roadmap and therefore out of scope.)

### 2.4 Milestone ladder

One **mainline** milestone at a time; each is its own passing, reproducible, committed
acceptance test. An agent does far better climbing a ladder of green bars than holding
a monolith in context. (Parallel side-tracks alongside the mainline are governed by
§7.)

1. Closed loop on a trivial image (§2.1) — boot + kernel-version assertion.
2. Add a service to the declaration; test asserts the unit is up and a port listens.
3. Default-deny hardening on that service; test asserts a forbidden operation is denied.
4. Typed config front-end that compiles to gexps; differential test: compiled output
   yields the same store paths as the hand-written gexp.
5. … extend toward the north star.

**Where step 5 has gone so far** (all on `main`, green, signed off under §4.3):

- **M5** (2026-06-06) — the same declaration also lowers to a reproducible Docker/OCI
  image.
- **M6** (2026-06-06) — manifest-driven, image-swap-only build interface: the image's
  swappable package payload is a function of a typed `manifest` (effective packages =
  fixed base capabilities + manifest payload + enforcement markers), and a changed
  manifest is a whole new reproducible image generation.
- **M7** (2026-06-06) — guix-free by construction: the typed `ship-guix?` field, when
  `#f`, deletes `guix-service-type` and embeds a closure-level `guix-free-marker`, so
  the realized image carries no `guix`/`guix-daemon` and there's no imperative `guix
  install` surface. With this sign-off the shipped default flipped to guix-free
  (`ship-guix?` defaults to `#f`), so the whole distro — bootable qcow2/VM and OCI
  image both — is guix-free. (Detail in §6.)
- **M8** (2026-06-07) — run the shipped OCI image as a real rootless OCI container via
  crun (podman was rejected as a network-fetching, offline-loop-breaking build), with
  a positive run and a negative control. Crosses the §2.3 "OCI app model" line.
- **M9** (2026-06-07) — the booted base is an OCI container *host*: it ships crun and
  mounts cgroup2, and runs a separate Guix-built OCI app image on the base honoring
  its entrypoint. (FHS-flattening the base was dropped — in a "minimal base, apps in
  containers" design, FHS is a property of the app images, not the base.)

M10.1–M10.2 (bootc-style generation image + guix-free placer) are done — record in
`HISTORY.md`. The forward plan from M10.3 on is the §7.1 roadmap, with per-track
detail under `plan/`.

### 2.5 Replacement order and the oracle for each swap

When a Guix component is eventually replaced, the existing Guix component is the
**oracle**. For every swap, build the same thing both ways and diff the store paths
with `diffoscope`; require behavioral equivalence on the full target set before
extending behavior. Never a big-bang rewrite.

### 2.6 State model *(settled 2026-06-10)*

What may persist on a td machine across generation swaps, and where. Decided with
the human 2026-06-10 — that decision is the §4.3 spec review for the state-model
parts of M10.3/M11. Informed by the production track record: shared-stateful,
read-only-root designs (ChromeOS, Android, Talos) have held up for a decade-plus;
every mutable-`/etc` mechanism (ostree's 3-way merge, MicroOS's overlay) is its
ecosystem's standing regret.

A td disk carries exactly three kinds of content:

- **Generation images** — read-only OS content (store + system closure +
  kernel/initrd), placed per generation, swapped wholesale, pruned by `--keep`.
  Never written after placement; M11 seals them, turning read-only from convention
  into a kernel-enforced property.
- **`td-state`** — the ONE writable filesystem (label `td-state`); the only
  traditional read-write filesystem on the disk. It survives every swap and is never
  touched by the placer or prune.
- **`/boot`** — placer-owned (M10.2): written only at place time, read-only at
  runtime by convention.

**The root is assembled, not stored.** Target shape: at boot, `/` is tmpfs; the
generation's image is mounted read-only (providing `/gnu/store` and the system
profile); activation materializes `/etc`, `/run`, `/tmp` from the declaration.
`/etc` is never persistent and never merged — configuration changes by building a
new generation, full stop. (Staged: M10.3 still boots the per-generation ext4 roots
M10.2 places; the tmpfs-root assembly lands together with M11's sealing — a root
the boot path writes to cannot be sealed.)

**Persistence is default-deny and declared.** The typed config carries a
persistent-paths allowlist; each entry is bind-mounted from `td-state` at boot, and
nothing else survives a swap. Two tiers, by backing directory:

- **precious** (`td-state/state/…`) — machine identity and anything backup-worthy.
  SSH host keys are the first entry, relocated explicitly via service configuration
  (e.g. `HostKey` under `/var/lib/ssh`), not by mount magic.
- **disposable** (`td-state/cache/…`) — persistent but re-derivable: logs,
  container images.

`/home` is persistent by definition: it lives on `td-state` (`td-state/home`), the
partition's user-visible face — not a separate filesystem.

**Machine identity ≠ OS identity.** Rollback swaps the OS, never the machine: a
rollback must not change SSH host keys.

**Backup / provision contract.** Rebuilding a machine = the typed config (hosted in
git) + restored `td-state/state` (and `home`). Nothing outside `td-state` is worth
backing up, by construction.

**Enforcement is staged.** M10.3: convention — the allowlist mounts exist, and the
rollback test asserts both directions (a declared path written under generation N
persists into the N−1 boot; an undeclared write does not follow the swap — it
merely lingers inside that generation's root until pruned). M11: kernel-enforced —
sealed read-only image + tmpfs root make an undeclared write fail closed (EROFS).

**Oracle scope.** The state model is part of the generation model: the typed
compiler emits the `td-state` mount and allowlist only when `generation` is set, so
`generation #f` still converges to the untouched frozen oracle and the M4/M5/M6
differentials hold with no re-baseline.

---

## 3. Invariants *(non-negotiable — these head the agent's instructions)*

- **Reproducibility.** Every artifact must pass `guix build --check`. A
  non-reproducible build is a failing test, not a warning.
- **Hermeticity.** No undeclared dependencies. Builds run offline except declared
  fixed-output fetches. Never "fix" a build by reaching outside the container.
- **State boundary.** The VM is ephemeral per test — fresh state per run, wiped on
  reset; that is *test isolation*, not a ban on persistence within a test.
  `/gnu/store` and the declaration are immutable. What may persist on a machine is
  default-deny and declared: only allowlisted paths on `td-state` survive a
  generation swap (the §2.6 state model; 2026-06-10 — supersedes the v0 wording
  "there is no persistent writable state"). Never stash mutable state outside the
  declared boundary to make something work.
- **Definition of done.** A passing test, reproducible, committed as a small
  increment. If "done" is undefined, the agent declares victory early.

Mirrored in `CLAUDE.md`.

---

## 4. Claude Code wiring

### 4.1 CLAUDE.md

`CLAUDE.md` carries the loop command (§1.1), the four invariants (§3), the
definition-of-done, the parallel-work rules (§7.2–7.4), the repo layout, the
strict-FSDG posture, and the state boundary. Keep §1, §3, and §7.2–7.3 of this
document in sync with it.

### 4.2 Task decomposition

Drive from the §7.1 roadmap: one agent drives the next mainline milestone; other
agents take side-tracks in parallel. Every agent states its sub-task and names or
writes its test before writing implementation.

### 4.3 Human checkpoints *(streamlined 2026-06-10)*

Exactly two things require the human; everything else on the roadmap — including
security-flavored milestones, `channels.scm` bumps, and oracle re-baselines — merges
on green via the §7.2 landing protocol:

1. **Roadmap additions.** New tracks or milestones enter §7.1 only with human
   approval. Approving an entry's acceptance test *is* the spec-correctness review,
   so it happens once, up front, instead of per-milestone.
2. **Weakening the loop.** Any change that removes, loosens, skips, or restructures
   away an existing rung or assertion in `check.sh`, the `Makefile`, or `tests/`
   requires explicit human sign-off, regardless of justification. Adding or
   strengthening rungs and assertions is always free.

(History: M1–M2 merged on green; M3–M10.2 were gated per-milestone and signed off —
dates in `HISTORY.md`. That per-milestone gate is retired in favor of the
pre-approved roadmap.)

---

## 5. Guix-specific decisions

None block v0, but naming them prevents surprises.

- **Guile for v0.** Embrace it; the typed front-end comes later and compiles to gexps.
- **Rust coexistence.** Deferred. Document later how Rust components sit alongside the
  Guile-based daemon when that milestone arrives.
- **Free-software posture.** Strict FSDG — follow Guix's free-software guidelines. No
  nonfree firmware, blobs, or crates; no `nonguix` channel. If a task appears to need
  nonfree code, STOP and ask.
- **Substitutes / build-farm trust.** Local builds only at v0. Revisit trust-agnostic
  substitution (decentralized build attestation) much later.

---

## 6. Parking lot / open questions

Things raised that aren't current decisions — kept here so they aren't lost and don't
expand scope. (2026-06-10: several graduated to §7.1 roadmap side-tracks — the qcow2
overlay/CoW reset is `loop-latency`, FHS-like OCI roots is `fhs-app-images`, and the
offline-posture follow-up is `offline-isolation`. Their entries below stand as the
original context.)

- Pin the exact kernel version `tests/boot.scm` asserts (derived from `channels.scm`);
  record it once the first build lands.
- Upgrade VM reset from fresh-image-per-test to `qcow2` overlay / CoW snapshots once
  loop latency demands it (§1.5).
- How Rust components will eventually coexist with the Guile daemon (§5).
- Trust-agnostic substitution / decentralized build attestation, post-v0 (§5).
- **FHS-like OCI root filesystems (post-v0).** Eventual OCI images should present a
  traditional FHS layout (`/usr/bin`, `/lib`, …) instead of Guix's `/gnu/store`
  symlink farm. M5 starts from Guix's native store-based `docker` image as the
  reproducibility oracle; FHS flattening is a later step on top.
- **Guix-free enforcement (how M7 actually holds).** The image-swap-only model has no
  per-package `guix install`; you build a whole image and swap it wholesale. Making
  that guix-free is enforced in two layers, because deleting `guix-service-type` alone
  isn't sufficient (a manifest package can still drag guix into the closure directly,
  via a propagated input, a runtime reference, or a renamed package, and no static
  name check catches all of those):
  1. an **embedded build gate** — `td-config->operating-system` prepends the
     `guix-free-marker` (`(system td-hardening)`), a build-time package that fails if
     any `/bin/guix` is in the closure of the manifest packages. It builds on every
     lowering, so manifest-injected guix fails the build. It's manifest-scoped, so it
     can't see service-injected guix.
  2. a **whole-system gate** — `guix-free-system-gate` builds a derivation over the
     entire folded system closure and fails if any `/bin/guix` is anywhere in it,
     catching service-injected guix. It can't be embedded (it would reference the
     system containing it), so `make no-guix` applies it over the shipped `td-system`.

  `make no-guix` proves both on the bare public lowering: the hardened image builds,
  the artifact is reproducible, no `/bin/guix` in its `layer.tar` (the `#t` control
  still ships it), an adversarial manifest smuggling guix past the pre-filter fails at
  the embedded marker, and a service-injection fixture fails at the whole-system gate.
  An absent binary can't run, which is stronger than a negative runtime test.
  Re-baselining the shipped default to guix-free surfaced one real dependency
  `guix-service-type` had provided as a side effect — sshd's privsep dir `/var/empty`
  (root:root 0755, via the build-user accounts); the guix-free system restores it with
  `guix-free-privsep-service`, proven by the boot rung (key-based SSH still logs in).

---

## 7. Roadmap and parallel work *(added 2026-06-10)*

This section replaces the per-milestone sign-off gate. It exists so multiple agents
can work long-running tasks concurrently and validate their own work, with the human
out of the loop except for the two §4.3 checkpoints.

### 7.1 The approved roadmap

Approved by the human 2026-06-10 — that approval is the §4.3 spec review for every
entry below. Agents implement these without further sign-off. Status lives in
`PLAN.md`; per-track working state in `plan/<track>.md`. Adding an entry requires
human approval; *refining* an entry's design inside its track file does not, so long
as the acceptance test stated here is met or strengthened.

**Mainline** (serial — each builds on the last; one agent drives it at a time):

- **M10.3 — manual rollback.** From a disk carrying two placed generations, the
  marionette test boots generation N, asserts its identity (root label / system),
  reboots selecting generation N−1 from the GRUB menu, and asserts the older
  identity; placed state persists across the reboot. Detail: `plan/m10.md`.
- **M11 — verified generations.** A generation's root carries build-time integrity
  metadata (fs-verity / dm-verity / composefs — mechanism chosen in the track file);
  booting an intact generation succeeds while a corrupted root fails closed
  (verified-red by corrupting bytes). Integrity ≠ authenticity: signatures are M12.
- **M12 — signed distribution.** A generation image is pushed to and pulled from a
  registry (local/offline inside the loop), its signature verified before placement;
  the placer rejects unsigned or tampered images (verified-red).

**Side-tracks** (parallel-safe; mostly disjoint from mainline files; any number may
run concurrently):

- **rootless-builder** — build the target with a rootless user-namespace builder and
  prove daemon-vs-rootless store-path equality (the prime-directive-4 differential;
  the daemon is the oracle). Deferred from M10.1. `plan/rootless-builder.md`.
- **offline-isolation** — drop nonguix from the daemon's substitute URLs and isolate
  the daemon's network; loop stays green isolated, and a deliberate undeclared fetch
  fails. Standing follow-up from M6. `plan/offline-isolation.md`.
- **oci-load** — verify the generation image loads in a foreign OCI runtime without
  breaking the offline loop (podman already rejected at M8; probe cheap vehicles or
  prove spec conformance structurally). Deferred from M10.1. `plan/oci-load.md`.
- **loop-latency** — qcow2 overlay / CoW VM reset (§1.5) and other cycle-time wins;
  measured improvement with the loop green and per-test ephemerality intact.
  `plan/loop-latency.md`.
- **fhs-app-images** — FHS-style root layout for *app* images (the base stays
  minimal per M9); an FHS app image builds reproducibly and runs on the base host
  rung. `plan/fhs-app-images.md`.

### 7.2 Landing protocol — merge on green

Each agent works one claimed track in its **own git worktree/branch** — never
directly on a shared checkout of main. To land:

1. fetch and rebase onto latest `origin/main`;
2. run the **full** `./check.sh` — it must be green;
3. fast-forward main to the branch and push;
4. if main moved while checking, go to 1.

No PRs and no human merge step; the human reviews asynchronously on main and may
revert. "Validated" means green against the main actually landed on — landing
without a green full check is a contract violation. Claims: one agent per track,
recorded on the track's status line in `PLAN.md` (a tiny standalone commit to main).

### 7.3 Exclusive landings

Changes touching the shared spine — `system/td.scm` (the frozen oracle), `check.sh`,
`Makefile`, `channels.scm`, `DIGESTS.md` — collide with every other agent. Land them
as small standalone commits, announced in your track file; everyone else rebases.
Oracle re-baselines (which rewrite `DIGESTS.md`) and channel-pin bumps are the
canonical cases. These are coordination rules, not sign-off gates — but remember
§4.3(2): *weakening* anything in the spine still needs the human.

Resource note: every full check boots QEMU VMs; two concurrent checks are fine on
this host, more may thrash. Stagger landings if loaded.

### 7.4 Files

`PLAN.md` — status index only, one line per track; keep edits tiny so rebases are
trivial. `plan/<track>.md` — per-track working state, single writer (the claiming
agent). `HISTORY.md` — completed-milestone record. `DIGESTS.md` — reproducibility
record (changes only on re-baseline, exclusive landing).
