# Design document — td

A functional Linux distribution, built incrementally by an AI coding agent (Claude
Code) on top of an existing Guix system, growing inside a fast, machine-checkable
verification loop.

This document is the settled contract Claude Code works against. It pins the two
things the agent can't decide for itself — **the loop it runs to check its work** and
**the target it's aiming at** — and bounds scope. Everything else the agent may
propose and iterate on. Section numbers are stable anchors; `CLAUDE.md` and `PLAN.md`
reference them, so keep them. Everything here governs **v0 only** unless marked as
later. Keep `CLAUDE.md` in sync with §1 and §3.

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

### 2.3 Out of scope for v0

Naming the exclusions is what stops the agent boiling the ocean. Out of scope: the
Rust build daemon; the unified sandbox/portal broker; composefs/`fs-verity` verified
generations; the OCI app model; the typed config front-end; multi-machine tests;
real-hardware/driver work. Building any of these in v0 is a scope violation — STOP and
ask. (Crossing one deliberately later requires §4.3 sign-off, as M5–M9 did.)

### 2.4 Milestone ladder

One milestone at a time; each is its own passing, reproducible, committed acceptance
test. The agent does far better climbing a ladder of green bars than holding a
monolith in context.

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

The M10 forward plan (native generation lifecycle) lives in `PLAN.md` and is gated.

### 2.5 Replacement order and the oracle for each swap

When a Guix component is eventually replaced, the existing Guix component is the
**oracle**. For every swap, build the same thing both ways and diff the store paths
with `diffoscope`; require behavioral equivalence on the full target set before
extending behavior. Never a big-bang rewrite.

---

## 3. Invariants *(non-negotiable — these head the agent's instructions)*

- **Reproducibility.** Every artifact must pass `guix build --check`. A
  non-reproducible build is a failing test, not a warning.
- **Hermeticity.** No undeclared dependencies. Builds run offline except declared
  fixed-output fetches. Never "fix" a build by reaching outside the container.
- **State boundary.** In v0 the VM is fully ephemeral — nothing persists across test
  runs, all writable state is wiped on reset. `/gnu/store` and the declaration are
  immutable; there is no persistent writable state to protect in v0. Never stash
  mutable state to make something work.
- **Definition of done.** A passing test, reproducible, committed as a small
  increment. If "done" is undefined, the agent declares victory early.

Mirrored in `CLAUDE.md`.

---

## 4. Claude Code wiring

### 4.1 CLAUDE.md

`CLAUDE.md` carries the loop command (§1.1), the four invariants (§3), the
definition-of-done, the repo layout (`Makefile`, `system/`, `tests/`, `channels.scm`),
the strict-FSDG posture, and the ephemeral state boundary. Keep §1 and §3 of this
document in sync with it.

### 4.2 Task decomposition

Drive from the §2.4 ladder, one milestone at a time. The agent states the sub-task and
names or writes its test before writing implementation.

### 4.3 Human checkpoints

The human owns spec correctness (is the target right?), the adversarial/security rung,
and anything touching real hardware. The agent merges on green below that ceiling but
opens any new-layer milestone for review before crossing into it. Concretely:
milestones 1–2 merged on green; milestone 3 (default-deny hardening) and milestone 4
(typed front-end) gated on sign-off; M5–M9 were each opened for sign-off as they
crossed new layers.

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

Things raised that aren't v0 decisions — kept here so they aren't lost and don't
expand scope.

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
