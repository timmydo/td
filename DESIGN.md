# Design document — td

A functional Linux distribution, built incrementally by an AI coding agent (Claude
Code) on top of an existing Guix system, growing inside a fast machine-checkable
verification loop.

## How to use this document

Each decision below has three parts:

- **Decide** — the question you must answer.
- **Default** — a recommended answer for a Guix-based v0. If you agree, keep it.
- **Answer** — fill this in. Once every `Answer` is filled, this document is the
  contract Claude Code iterates against. Keep `CLAUDE.md` (the agent's persistent
  instructions) in sync with Sections 1 and 3.

The rule of thumb: this document pins down the two things the agent cannot decide for
itself — **the loop it runs to check its own work** and **the target it is aiming
at** — and explicitly bounds scope. Everything else the agent may propose and iterate
on.

---

## 0. North star and v0 scope

**Decide:** In one paragraph, the eventual vision. In a second paragraph, what v0 is.

**Default:** *Eventual:* a content-addressed, reproducible, immutable distro where the
store path doubles as `fs-verity` root and OCI digest, with one Rust sandbox stack
spanning build and run, a typed config front-end, and atomic verified generations.
*v0:* the smallest vertical slice that closes the full verification loop on top of
stock Guix — one declaration that builds reproducibly into a bootable image, boots in
a VM, and passes one behavioral assertion.

**Answer:** Accept the default. *Eventual:* a content-addressed, reproducible, immutable
distro where the store path doubles as `fs-verity` root and OCI digest, with one Rust
sandbox stack spanning build and run, a typed config front-end, and atomic verified
generations. *v0:* the smallest vertical slice that closes the full loop on stock Guix —
one `system/td.scm` declaration that builds reproducibly into a bootable image, boots in
a VM, and passes one behavioral assertion (kernel version).

> Everything in this document governs **v0 only** unless explicitly marked as later.

---

## 1. The loop *(answer this section first — nothing else matters until it is settled)*

### 1.1 The single pass/fail command

**Decide:** The one shell command whose exit code (and final line) means "green." The
agent self-corrects by running this and reading output, so it must exist.

**Default:** A `make check` (or `just check`) target that runs, in order: config eval →
`guix build --check` on the target → the marionette system test. Non-zero exit on any
failure.

**Answer:** `./check.sh` (canonical) → `make check` (the target it runs inside the
sandbox). `make check` runs, short-circuiting on first failure: config eval → the
typed/OCI/manifest differentials → `guix build --check` on `system/td.scm` → the
`tests/boot.scm` marionette test → the manifest-swap reproducibility/artifact rung.
Exits non-zero on any failure. `./check.sh` wraps it with the hermetic, offline setup
(fresh `guix shell -C --pure`, store/daemon exposure, host-guix-pin integrity guard,
**substitutes disabled**); it is the one command that defines green/red. Plain
`make check` is only correct when already inside that sandbox.

### 1.2 Rungs committed for v0

**Decide:** Which verification rungs ship in v0. Each is a dependency you take on now.

**Default:**
- Hermetic dev/build env: `guix shell -C --pure` (containerized, no host leakage).
- Reproducibility oracle: `guix build --check` (and `--rounds=2` where cheap). Treat
  any non-reproducible output as a failing test.
- Boot + behavioral: one marionette-based system test (the `(gnu tests)` framework)
  that boots a `guix system vm` and drives the guest from Guile.
- *Deferred to later milestones:* fuzzing/adversarial (sandbox escape corpus),
  real-hardware testing.

**Answer:** Accept the default rungs for v0: hermetic `guix shell -C --pure` env,
`guix build --check` (with `--rounds=2` where cheap) as the reproducibility oracle, and
one marionette `(gnu tests)` boot test. Fuzzing/adversarial and real-hardware rungs are
deferred.

### 1.3 Loop-latency budget

**Decide:** Target round-trip time for one write→check cycle, and whether `guix system
vm` meets it. If a cycle is minutes, the agent's exploration collapses.

**Default:** Aim for under ~60s on a warm store for the inner loop. If full VM builds
are too slow, give the agent a prebuilt base image to layer onto, and reserve full
`guix system vm` rebuilds for a less-frequent rung. Treat loop latency as a tracked
metric, not an afterthought.

**Answer:** Target under ~60s per write→check cycle on a warm store. Fast-path: layer
changes onto a **prebuilt base image** and reserve full `guix system vm` rebuilds for a
less-frequent rung. Track loop latency as a metric.

### 1.4 Agent / container boundary

**Decide:** Does Claude Code run inside or outside the `guix shell -C` container?

**Default:** Agent process runs **outside**; every build/test command it issues enters
a **fresh** `guix shell -C` container. This stops the agent's own environment from
contaminating results and keeps rung 2 (reproducibility) honest.

**Answer:** Accept the default. The Claude Code agent process runs **outside**; every
build/test command enters a **fresh** `guix shell -C --pure` container.

### 1.5 VM state reset

**Decide:** How VM state resets between behavioral tests.

**Default:** Boot from a fresh image per test at v0 (simple, slow). Upgrade path:
QEMU `qcow2` overlay snapshots / CoW reset so a test can run many times cheaply.

**Answer:** v0 is **fully ephemeral**: boot from a fresh image per test, nothing persists
across runs, all writable state is wiped on reset. Upgrade path: QEMU `qcow2` overlay /
CoW reset for cheap repeated runs.

---

## 2. The target

### 2.1 v0 acceptance test (write it as a literal, not a vibe)

**Decide:** The exact assertion that means "v0 is done."

**Default:** "A single Guile declaration builds reproducibly (`guix build --check`
passes) into a bootable image; the marionette test boots it, asserts
`uname -r` / `cat /proc/version` reports the expected kernel, then the harness resets."

**Answer (literal):** The `system/td.scm` declaration builds reproducibly
(`guix build --check` passes) into a bootable image; the `tests/boot.scm` marionette test
boots it and asserts that `uname -r` in the guest equals the kernel version pinned by the
declaration, then the harness resets the ephemeral VM. v0 is done when this passes,
reproducibly, and is committed.

### 2.2 Reused vs. built for v0

**Decide:** What you keep from Guix as substrate vs. what you build.

**Default:** Keep `guix-daemon`, `/gnu/store`, and **Guile + gexps as the config
language** for v0. Do **not** introduce the typed front-end yet — changing the language
and building the OS at once is two hard projects fused into one, and it blinds the
agent's cheapest rung. The typed layer is a later milestone that compiles down to
gexps.

**Answer:** Accept the default. Keep `guix-daemon`, `/gnu/store`, and Guile + gexps as
the v0 config language. No typed front-end yet — it is a later milestone that compiles
down to gexps.

### 2.3 Explicitly out of scope for v0

**Decide:** Name what the agent must not build yet. Writing the exclusions down is what
stops it boiling the ocean.

**Default (out of scope for v0):** the Rust build daemon; the unified sandbox/portal
broker; composefs/`fs-verity` verified generations; the OCI app model; the typed config
front-end; multi-machine tests; real-hardware/driver work.

**Answer:** Accept the default out-of-scope list: the Rust build daemon; the unified
sandbox/portal broker; composefs/`fs-verity` verified generations; the OCI app model; the
typed config front-end; multi-machine tests; real-hardware/driver work. Building any of
these in v0 is a scope violation — STOP and ask first.

### 2.4 Milestone ladder

**Decide:** The ordered sequence of small, individually-verifiable milestones. The agent
does far better climbing a ladder of green bars than holding a monolith in context.

**Default (each milestone = one passing acceptance test, reproducible, committed):**
1. Closed loop on a trivial image (Section 2.1).
2. Add a service to the declaration; behavioral test asserts the unit is up and a port
   listens.
3. Default-deny hardening on that service; test asserts a forbidden operation is
   **denied**.
4. Introduce the typed config front-end that compiles to gexps; differential test:
   compiled output yields the same store paths as the hand-written gexp.
5. … (extend toward the north star)

**Answer:** Accept the default ladder, in order:
1. Closed loop on a trivial image (§2.1) — boot + kernel-version assertion.
2. Add a service to the declaration; behavioral test asserts the unit is up and a port
   listens.
3. Default-deny hardening on that service; test asserts a forbidden operation is
   **denied**.
4. Introduce the typed config front-end compiling to gexps; differential test: compiled
   output yields the same store paths as the hand-written gexp.
5. … extend toward the north star.
One milestone at a time; each is its own passing, reproducible, committed acceptance
test.

**Implemented continuation of step 5 (pending human sign-off, §4.3).** The "extend"
slot has so far been realized as three milestones that pull §6 parking-lot threads. All
are on `main`, green, with verified-red differentials, but await sign-off before merge
(they cross into new layers): **M5** — the same declaration also lowers to a
reproducible Docker/OCI image; **M6** — manifest-driven, image-swap-only *build
interface*: image contents are a declarative function of a typed `manifest`, and a
changed manifest is a whole new reproducible image generation; **M7** — image-swap-only
*by construction*: the typed `ship-guix?` field (when #f) deletes `guix-service-type` so
the realized OCI image carries no `guix`/`guix-daemon` binary, removing the imperative
`guix install` surface M6 left in place. (M7's claim is artifact-level — the binary is
physically absent, proven by `make no-guix`; the field defaults to #t so the *shipped*
default still ships guix and the §2.5 frozen oracle is preserved. Flipping that default
to #f re-baselines the oracle and is itself a sign-off decision; a literal docker-run
runtime check needs the OCI app model, §2.3, still deferred. See the §6 parking-lot note.)
See `PLAN.md` for the per-milestone status and digests. Promoting these from "extend"
into numbered ladder rungs is a spec decision for the human reviewer.

### 2.5 Replacement order and the oracle for each swap

**Decide:** When a Guix component is eventually replaced, what proves the replacement is
correct.

**Default:** The existing Guix component is the **oracle**. For every swap, build the
same thing both ways and diff the store paths (`diffoscope`); require behavioral
equivalence on the full target set *before* extending behavior. Never a big-bang
rewrite.

**Answer:** Accept the default. The existing Guix component is the oracle. For every
swap, build the same thing both ways and diff store paths with `diffoscope`; require
behavioral equivalence on the full target set *before* extending behavior. No big-bang
rewrites.

---

## 3. Invariants *(non-negotiable — these head the agent's instructions)*

**Decide:** Confirm or amend each. These are hard gates, not aspirations.

- **Reproducibility.** Every artifact must pass `guix build --check`. A non-reproducible
  build is a failing test, not a warning.
- **Hermeticity.** No undeclared dependencies. Builds run offline except declared
  fixed-output fetches. The agent must never "fix" a build by reaching outside the
  container.
- **State boundary.** Define now what is declared-and-immutable vs. writable. The agent
  must not stash mutable state to make something work.
- **Definition of done (any task).** A passing test, reproducible, committed as a small
  increment. If "done" is undefined, the agent declares victory early.

**Answer / amendments:** Confirm all four invariants. **State boundary (v0):** the VM is
**fully ephemeral** — nothing persists across test runs and all writable state is wiped
on reset. `/gnu/store` and the system declaration are declared-and-immutable; there is no
persistent writable state to protect in v0. The agent must never stash mutable state to
make a test pass. This is mirrored in `CLAUDE.md`.

---

## 4. Claude Code wiring

### 4.1 CLAUDE.md

**Decide:** Confirm `CLAUDE.md` (separate file) carries the loop commands (§1), the
invariants (§3), the definition-of-done, repo conventions, and explicit guardrails. Keep
it in sync with this document.

**Answer:** Confirmed. `CLAUDE.md` carries `make check` (§1.1), the four invariants
(§3), the definition-of-done, the repo layout (`Makefile`, `system/`, `tests/`,
`channels.scm`), the strict-FSDG posture, and the ephemeral state boundary. Keep §1 and
§3 of this document in sync with `CLAUDE.md`.

### 4.2 Task decomposition

**Decide:** How work is sliced. Each task = one verifiable increment with its own
acceptance test.

**Default:** Drive from the milestone ladder (§2.4); one milestone at a time; the agent
proposes the next sub-task and its test before writing code.

**Answer:** Accept the default. Drive from the §2.4 ladder, one milestone at a time. The
agent states the sub-task and names/writes its test before writing implementation.

### 4.3 Human checkpoints

**Decide:** What gates on your sign-off vs. what the agent merges on green. Be honest
about the ladder's ceiling.

**Default:** *You* own spec correctness (is the target right?), the
adversarial/security rung, and anything touching real hardware. The agent merges on
green for everything below that ceiling, but opens the milestone for review before
crossing into a new layer.

**Answer:** Accept the default. The human owns spec correctness, the adversarial/security
rung, and anything touching real hardware. Concretely, **milestone 3 (default-deny
hardening)** and **milestone 4 (typed config front-end)** gate on sign-off before merge;
milestones 1–2 merge on green. The agent opens any new-layer milestone for review before
crossing into it.

---

## 5. Guix-specific decisions

**Decide each; none block v0, but naming them now prevents surprises.**

- **Guile for v0.** *Default:* embrace it; the typed front-end comes later and compiles
  to gexps.
- **Rust coexistence.** *Default:* defer; document how Rust components will eventually
  sit alongside the Guile-based daemon when that milestone arrives.
- **Free-software posture.** *Default:* decide whether the project follows Guix's strict
  nonfree stance. This quietly constrains "can the agent pull in this crate / this
  firmware blob" decisions — settle it before it bites.
- **Substitutes / build-farm trust.** *Default:* local builds only at v0; revisit
  trust-agnostic substitution (decentralized build attestation) much later.

**Answer:**
- **Guile for v0.** Embrace it; typed front-end comes later and compiles to gexps.
- **Rust coexistence.** Deferred. Document later how Rust components sit alongside the
  Guile-based daemon when that milestone arrives.
- **Free-software posture.** **Strict FSDG** — follow Guix's free-software guidelines.
  No nonfree firmware, blobs, or crates; no `nonguix` channel. If a task appears to
  require nonfree code, STOP and ask.
- **Substitutes / build-farm trust.** Local builds only at v0. Revisit trust-agnostic
  substitution much later.

---

## 6. Parking lot / open questions

Anything raised that isn't a v0 decision. Keep it here so it isn't lost and doesn't
expand scope.

- Pin the exact kernel version asserted by `tests/boot.scm` (derived from
  `channels.scm`) — record it once the first build lands.
- Upgrade VM reset from fresh-image-per-test to `qcow2` overlay / CoW snapshots once
  loop latency demands it (§1.5).
- Decide how Rust components will eventually coexist with the Guile daemon (§5).
- Revisit trust-agnostic substitution / decentralized build attestation post-v0 (§5).
- **FHS-like OCI root filesystems (post-v0 direction).** The eventual OCI images
  should present a traditional **FHS layout** (`/usr/bin`, `/lib`, …) for the root
  fs, *unlike* stock Guix's `/gnu/store` symlink-farm layout. M5 starts from Guix's
  native `docker` image (store-based) as the reproducibility oracle; the FHS
  flattening is a later step layered on top, not part of M5.
- **No imperative `guix install` workflow (immutable, manifest-driven).** The model
  is: build a whole image from a declarative manifest and **swap the image
  wholesale**; there is no per-package imperative install. Rationale: `guix install`
  accumulates many package versions under `/gnu/store` that are never cleaned up
  well. Keep the distro image-swap-only — no `guix install`-equivalent surface.
  *Status: the typed image-build INTERFACE is manifest-driven and image-swap-only
  as of M6 (on `main`, pending sign-off §4.3).* The typed config's `manifest` field
  is the lever; `make manifest-diff` proves a changed manifest is a different
  whole-image generation and `make manifest-check` proves that generation is
  reproducible and actually contains the declared package.
  *Surface removal — M7 (on `main`, pending sign-off §4.3):* M6 left the
  imperative mutation surface in place — the built OCI artifact shipped `guix` and
  `guix-daemon`, so an in-image `guix install` was physically possible. **M7 makes
  the image guix-free *by construction*:** the typed config gained a `ship-guix?`
  field that, when `#f`, deletes `guix-service-type` (verified to be the ONLY thing
  pulling the `guix` package into the system closure). `make no-guix` proves this
  at the ARTIFACT level — it builds the hardened image, `--check`s it reproducible,
  and asserts no `/bin/guix` or `/bin/guix-daemon` is in its `layer.tar` (0 entries)
  while the default image still ships them (4). A binary absent from the image
  cannot run, so this artifact-level claim is *stronger* than the "negative runtime
  test" originally envisioned (a literal docker-run `guix install` check needs the
  OCI app model, §2.3, still deferred). **Two things remain (each a spec/sign-off
  call, not yet taken):** (1) `ship-guix?` defaults to `#t`, so the *shipped*
  default still ships guix — flipping the default to `#f` re-baselines the §2.5
  frozen oracle and is the human's spec decision; M7 proved the construction
  additively without taking it. (2) The FHS-flattened root (above) is still future.
