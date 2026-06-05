# Design document — <PROJECT_NAME>

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

**Answer:** *(fill in)*

> Everything in this document governs **v0 only** unless explicitly marked as later.

---

## 1. The loop *(answer this section first — nothing else matters until it is settled)*

### 1.1 The single pass/fail command

**Decide:** The one shell command whose exit code (and final line) means "green." The
agent self-corrects by running this and reading output, so it must exist.

**Default:** A `make check` (or `just check`) target that runs, in order: config eval →
`guix build --check` on the target → the marionette system test. Non-zero exit on any
failure.

**Answer:** *(fill in the exact command)*

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

**Answer:** *(fill in)*

### 1.3 Loop-latency budget

**Decide:** Target round-trip time for one write→check cycle, and whether `guix system
vm` meets it. If a cycle is minutes, the agent's exploration collapses.

**Default:** Aim for under ~60s on a warm store for the inner loop. If full VM builds
are too slow, give the agent a prebuilt base image to layer onto, and reserve full
`guix system vm` rebuilds for a less-frequent rung. Treat loop latency as a tracked
metric, not an afterthought.

**Answer:** *(target latency + fast-path decision)*

### 1.4 Agent / container boundary

**Decide:** Does Claude Code run inside or outside the `guix shell -C` container?

**Default:** Agent process runs **outside**; every build/test command it issues enters
a **fresh** `guix shell -C` container. This stops the agent's own environment from
contaminating results and keeps rung 2 (reproducibility) honest.

**Answer:** *(fill in)*

### 1.5 VM state reset

**Decide:** How VM state resets between behavioral tests.

**Default:** Boot from a fresh image per test at v0 (simple, slow). Upgrade path:
QEMU `qcow2` overlay snapshots / CoW reset so a test can run many times cheaply.

**Answer:** *(fill in)*

---

## 2. The target

### 2.1 v0 acceptance test (write it as a literal, not a vibe)

**Decide:** The exact assertion that means "v0 is done."

**Default:** "A single Guile declaration builds reproducibly (`guix build --check`
passes) into a bootable image; the marionette test boots it, asserts
`uname -r` / `cat /proc/version` reports the expected kernel, then the harness resets."

**Answer:** *(fill in the literal assertion)*

### 2.2 Reused vs. built for v0

**Decide:** What you keep from Guix as substrate vs. what you build.

**Default:** Keep `guix-daemon`, `/gnu/store`, and **Guile + gexps as the config
language** for v0. Do **not** introduce the typed front-end yet — changing the language
and building the OS at once is two hard projects fused into one, and it blinds the
agent's cheapest rung. The typed layer is a later milestone that compiles down to
gexps.

**Answer:** *(fill in)*

### 2.3 Explicitly out of scope for v0

**Decide:** Name what the agent must not build yet. Writing the exclusions down is what
stops it boiling the ocean.

**Default (out of scope for v0):** the Rust build daemon; the unified sandbox/portal
broker; composefs/`fs-verity` verified generations; the OCI app model; the typed config
front-end; multi-machine tests; real-hardware/driver work.

**Answer:** *(fill in)*

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

**Answer:** *(fill in / reorder)*

### 2.5 Replacement order and the oracle for each swap

**Decide:** When a Guix component is eventually replaced, what proves the replacement is
correct.

**Default:** The existing Guix component is the **oracle**. For every swap, build the
same thing both ways and diff the store paths (`diffoscope`); require behavioral
equivalence on the full target set *before* extending behavior. Never a big-bang
rewrite.

**Answer:** *(fill in)*

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

**Answer / amendments:** *(fill in; e.g., state-boundary specifics)*

---

## 4. Claude Code wiring

### 4.1 CLAUDE.md

**Decide:** Confirm `CLAUDE.md` (separate file) carries the loop commands (§1), the
invariants (§3), the definition-of-done, repo conventions, and explicit guardrails. Keep
it in sync with this document.

**Answer:** *(fill in any project-specific additions)*

### 4.2 Task decomposition

**Decide:** How work is sliced. Each task = one verifiable increment with its own
acceptance test.

**Default:** Drive from the milestone ladder (§2.4); one milestone at a time; the agent
proposes the next sub-task and its test before writing code.

**Answer:** *(fill in)*

### 4.3 Human checkpoints

**Decide:** What gates on your sign-off vs. what the agent merges on green. Be honest
about the ladder's ceiling.

**Default:** *You* own spec correctness (is the target right?), the
adversarial/security rung, and anything touching real hardware. The agent merges on
green for everything below that ceiling, but opens the milestone for review before
crossing into a new layer.

**Answer:** *(fill in which milestones gate on sign-off)*

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

**Answer:** *(fill in)*

---

## 6. Parking lot / open questions

Anything raised that isn't a v0 decision. Keep it here so it isn't lost and doesn't
expand scope.

- *(add items)*
