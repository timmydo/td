# Design document — td

A functional Linux distribution, built incrementally by an AI coding agent (Claude
Code) on top of an existing Guix system, growing inside a fast, machine-checkable
verification loop.

This document is the settled contract the agents work against. It pins the three
things an agent can't decide for itself — **the loop it runs to check its work**,
**the target it's aiming at**, and **the scope it may work without sign-off** (the
§7.1 roadmap). Everything else the agents may propose and iterate on. Section numbers
are stable anchors; `CLAUDE.md` and `PLAN.md` reference them, so keep them.

It states the north star and the standing decisions — **not the history**: how we got
here lives in `HISTORY.md` and git, and milestone narration does not belong in this
file. To stay DRY, each volatile fact lives in exactly one place and everything else
points at it: the rung list in the `Makefile`'s `check:` line, claim status in
`PLAN.md`, completed milestones in `HISTORY.md`. `CLAUDE.md` mirrors only the stable
contract (§1, §3, §7.2–7.3). If two statements disagree anyway, **the later-dated
decision governs**; reconcile the older text on sight.

---

## 0. North star and scope

**North star:** a content-addressed, reproducible, immutable distro where the store
path doubles as integrity root and OCI digest, with one Rust sandbox stack spanning
build and run, a typed config front-end, and atomic verified generations.

**Scope at any time = the approved roadmap (§7.1).** The climbed ladder (the v0
closed loop through M10.2) is recorded in `HISTORY.md`.

---

## 1. The loop *(this section comes first — nothing else matters until it's settled)*

### 1.1 The single pass/fail command

`./check.sh` is the one command that means green or red. It sets up the hermetic,
offline sandbox (a fresh `guix shell -C --pure`, store and daemon-socket exposure, a
guard that host guix matches the `channels.scm` pin, substitutes disabled) and runs
`make check` inside it. `make check` runs the rung ladder, short-circuiting on the
first failure; **the `Makefile`'s `CHEAP_RUNGS`/`HEAVY_RUNGS` pools (expanded by its
`check:` target) are the authoritative rung list** — documents point here instead of
restating it. Broad shape: config eval →
differentials → `guix build --check` → behavioral/marionette tests. Plain
`make check` is only correct when you're already inside that sandbox.

### 1.2 Rung classes

- Hermetic build/dev env: `guix shell -C --pure` (no host leakage).
- Reproducibility oracle: `guix build --check` (and `--rounds=2` where cheap). A
  non-reproducible output is a failing test.
- Boot + behavioral: marionette `(gnu tests)` system tests that boot the image and
  drive the guest from Guile.

Fuzzing/adversarial and real-hardware rungs are deferred (off-roadmap).

### 1.3 Loop-latency budget

Target under ~60s per write→check cycle on a warm store. To stay there, layer changes
onto a prebuilt base image and reserve full `guix system vm` rebuilds for a
less-frequent rung. Loop latency is a tracked metric, not an afterthought.

### 1.4 Agent / container boundary

The Claude Code agent runs **outside** the container. Every build/test command it
issues enters a **fresh** `guix shell -C --pure` container, so the agent's own
environment can't contaminate results and the reproducibility rung stays honest.

### 1.5 VM state reset

The harness is **fully ephemeral per test**: boot from a fresh image, wipe all
writable state on reset. That is test isolation, not a ban on persistence *within* a
test (§2.6, §3). The CoW reset (QEMU `qcow2` overlay) is in place and asserted by
the `reset` rung (landed 2026-06-10; measurements in `plan/loop-latency.md`).

---

## 2. The target

### 2.1 Base acceptance test

The base of the ladder, still a live rung: `system/td.scm` builds reproducibly
(`guix build --check` passes) into a bootable image; `tests/boot.scm` boots it and
asserts that `uname -r` in the guest equals the kernel version pinned by the
declaration; then the harness resets the ephemeral VM.

### 2.2 Reused vs. built

Reused: `guix-daemon`, `/gnu/store`, and Guile + gexps as the lowering target. Built
on top: the typed config front-end (`(system td-typed)`) compiles down to gexps, with
the hand-written `system/td.scm` kept FROZEN as the differential oracle (§2.5).
Replacing a reused Guix component is off-roadmap until it earns a §7.1 entry, and
then only under the §2.5 differential discipline.

### 2.3 Scope rule *(redefined 2026-06-10)*

**In scope = on the approved roadmap (§7.1). Everything else is out of scope — STOP
and ask; never expand scope on your own.** Naming the boundary is what stops an agent
boiling the ocean.

Named as staying out (off-roadmap today): the unified sandbox/portal broker,
multi-machine tests, real-hardware/driver work.

### 2.4 Milestone ladder

One **mainline** milestone at a time; each is its own passing, reproducible, committed
acceptance test. An agent does far better climbing a ladder of green bars than holding
a monolith in context. (Parallel side-tracks alongside the mainline are governed by
§7.)

The live ladder — mainline M10.3 → M11 → M12 plus the side-tracks — is §7.1, with
per-track detail under `plan/`. Climbed rungs (v0/M1 through M10.2) are recorded in
`HISTORY.md`.

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
new generation, full stop. (Staged: M10.2 stages each generation's root *content* as
a tarball; M10.3 turns those into per-generation **ext4 root images** and boots
them — read-only by convention: the mount stays rw until M11's sealing, which is
what lets the M10.3 test show an undeclared write lingering in that generation's
root; the tmpfs-root assembly lands together with M11's sealing — a root the boot
path writes to cannot be sealed. ext4 here is only the container format for a
read-only image — and M11's dm-verity data device — not a traditional read-write
filesystem; `td-state` remains the only one of those on the disk.)

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

### 2.7 Generation identity *(settled 2026-06-10 — pinned ahead of M12)*

A generation's identity is the **content digest of its image**. The integer N
(`gen-N`, `td-root-gen-N`) is a placer-local **install ordinal** — it names the
slot a generation occupies on one machine, never the generation itself. Two
machines' "generation 3" need not be the same OS; two placements of the same
digest are the same OS wherever they sit. M12 signs and verifies digests, not
ordinals: the signature is over the image's manifest digest, and "verify before
placement" means digest first, slot second. Pinned now, while M10 is warm, so
M12 never has to retrofit identity into M10's artifacts.

**Digest definition, staged with the artifact.** The end state is the OCI image
manifest digest — the registry-addressable `sha256:…` of the image manifest —
once the generation image has a canonical OCI layout (the `oci-load` track's
territory). Until then the image is a reproducible docker-archive tarball and
the digest is the sha256 of that artifact. Moving between the two is a
representation change to record (a `DIGESTS.md` re-baseline), not a change of
convention: identity = digest of the distributed artifact, in its canonical
form.

**Where the digest lives — the self-reference rule.** An image cannot carry its
own digest (the digest is computed over content that would include the file),
so the identity record splits:

- **Embedded** `boot/td-identity` (inside the image, M10.2) carries what the
  image is *for*: `generation=N`, `root-label=…` — build inputs that bind the
  image to the slot it was built for; the placer rejects a slot mismatch.
- **Placed** `boot/td/gen-N/td-identity` (written by the placer) additionally
  carries what the image *is*: `image-digest=sha256:…`, computed by the placer
  over the artifact it actually unpacked. This line is M12's anchor — verify
  the signature over the digest, hash the pulled artifact, compare, and only
  then place; the placed record then states the verified identity. Adding the
  line is a one-line placer increment, landable under M10.3 or at M12's start.

**Known consequence, accepted.** Today the image is specialized to its slot —
the per-generation root label is baked into the declaration and initrd
(M10.1's crux) — so identical OS content placed in two slots yields two
digests. That is fine: identity attaches to the artifact; slot binding stays a
separate, local concern enforced by the embedded record. Decoupling image from
slot (one image placeable in any slot, root chosen at place time) is a possible
later refinement, off-roadmap, and not needed by M12.

**M12 pre-decisions** (vehicle and policy settled on paper now — the M8 podman
lesson is that vehicle choice can sink a milestone):

- **Signing vehicle: detached signature over the digest; no sigstore.**
  cosign and the sigstore world are Go-heavy and network-assuming — likely the
  next podman. A plain detached ed25519 signature (signify/minisign-style, or
  guile-gcrypt's ed25519 — already a build dependency of
  `system/td-generation.scm`) over the manifest digest fits the offline loop.
  The track file states the probe criterion up front, M8-style — offline-
  buildable from the pinned channel, sane derivation count — before any
  vehicle is adopted.
- **Registry: a static layout, not a product.** The OCI distribution API is
  HTTP over a content-addressed layout; inside the loop, a static `oci-layout`
  directory plus a trivial local server (or plain file transport) *is* the
  registry. This satisfies the §7.1 acceptance wording "pushed to and pulled
  from a registry (local/offline inside the loop)"; no registry product enters
  the loop.
- **Downgrade policy: anti-rollback is out of scope.** The placer rejects
  exactly three things: unsigned, bad signature, digest mismatch. An *old but
  validly signed* generation is re-placeable by design — manual rollback is
  the model (M10), so freshness/epoch enforcement is explicitly not a goal.
  Written down so it isn't invented ad hoc mid-track.
- **Named open question for the track file:** key distribution — how the
  target gets the verifying public key (placer flag, well-known path on
  `td-state`, baked into the placer build). Decide in `plan/m12.md` when the
  track starts; it does not block the convention above.

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
  generation swap (the §2.6 state model). Never stash mutable state outside the
  declared boundary to make something work.
- **Definition of done.** A passing test, reproducible, committed as a small
  increment. If "done" is undefined, the agent declares victory early.

Mirrored in `CLAUDE.md`.

---

## 4. Claude Code wiring

### 4.1 CLAUDE.md

`CLAUDE.md` carries the loop command (§1.1), the four invariants (§3), the
definition-of-done, the parallel-work rules (§7.2–7.4), the repo layout, the
free-software posture (§5 — relaxed to non-goal 2026-06-11), and the state
boundary. Keep §1, §3, and §7.2–7.3 of this document in sync with it.

### 4.2 Task decomposition

Drive from the §7.1 roadmap: one agent drives the next mainline milestone; other
agents take side-tracks in parallel. Every agent states its sub-task and names or
writes its test before writing implementation.

### 4.3 Human checkpoint *(simplified 2026-06-13)*

This is a one-maintainer project, so the process is one gate: **the PR is the
proposal, and the human's PR approval is the sign-off.** Every landing already routes
through a branch-protected PR with a mandatory review of the *diff* (§7.2), and that
single review approves everything — new work, scope, `channels.scm` bumps, oracle
re-baselines, and changes that loosen or restructure an existing rung. You do **not**
need a written proposal, a roadmap entry, or any pre-approval before building: build
the smallest correct increment on a branch and open the PR.

The one thing never to do *silently*: remove, loosen, skip, or restructure away an
existing rung or assertion (in `check.sh`, the `Makefile`, or `tests/`) and slip it
past review. Call it out plainly in the PR so the human approves it knowingly. Adding
or strengthening rungs is always free. The correctness directives in CLAUDE.md
(reproducibility, hermeticity, differential-before-replace, the state boundary) are
not bureaucracy and still hold.

(The retired per-milestone and roadmap-addition sign-off gates and their dates:
`HISTORY.md`.)

---

## 5. Guix-specific decisions

Standing posture decisions; naming them prevents surprises.

- **Spec language — moving off Guile is now a goal** *(decided 2026-06-12,
  human — §4.3 gate-1 roadmap addition; supersedes the former "embrace
  Guile").* The destination is a general-purpose, popular surface language —
  **TypeScript** — for package/system specs, evaluated hermetically and
  lowering to drvs. Guile/gexps are no longer the destination, only the
  **migration lowering target and differential oracle**: a TS spec is correct
  iff it lowers to a store path NAR-hash-equal to the frozen `system/td.scm`
  (§2.5) — exactly the discipline that already guards `td-typed`. The Guile
  oracle is retired LAST, after surface and corpus are off it, because it is
  the equivalence check protecting the migration. Phase 1 is the `ts-frontend`
  track (§7.1): TS→JS via swc, evaluated by an embedded **boa** engine
  (pure-Rust, in-process inside td-builder's sandbox), ambient I/O removed and
  clock/randomness neutered, with lowering builtins (corpus lookup, store-path
  dependency capture) as Rust native functions. Evaluator rationale (boa vs
  javy) and the hermetic-eval design: `plan/ts-frontend.md`.
- **Rust toolchain** *(decided 2026-06-11)*. Rust is the approved vehicle for
  td-builder (§7.1). Building the toolchain from source is a **non-goal** right
  now: the host store may be warmed with substitutes for the pinned channel's
  Rust closure. The loop itself stays offline/no-substitutes as ever — warm
  store in, nothing fetched inside.
- **Package collection — corpus + runtime independence is now a goal**
  *(re-decided 2026-06-12, human — the roadmap addition the prior posture
  invited).* The prior posture (Guix as a pinned corpus input, re-derivation a
  non-goal) is superseded: td will own its own package/system specs and depend
  on **no general-purpose Linux distro** (Guix, Nix, Debian, …) at corpus or
  runtime level. Independence is source-level on *upstream projects*, not from
  scratch: td writes its own *recipes* pulling upstream *source* (kernel.org,
  GNU, …) — replacing the distro's packaging, not the software. Two bounds keep
  this from boiling the ocean and stay **non-goals**:
  - **General-purpose comprehensiveness.** The corpus is td's *target closure*
    — an appliance/image OS, Yocto/Buildroot scale (hundreds of packages), NOT
    a Nixpkgs-scale general distro.
  - **Full-source bootstrap.** The seed/first toolchain stays **external** —
    pulled as a pinned fixed-output input (an upstream binary, or even a Guix
    bootstrap seed, is fine); stage0/Mes-style re-derivation remains out (human,
    2026-06-12). "No distro dependency" governs what td *builds and runs*, not
    where the first byte came from.

  Phase 1 (`ts-frontend`, §7.1) replaces the spec *language* and keeps reading
  the pinned corpus underneath; corpus replacement is Phase 2, separately gated
  (§6).
- **Free-software posture** *(relaxed to non-goal 2026-06-11, human)*. Strict
  FSDG purity is a **non-goal**. The pinned channel remains the default source
  (and happens to be FSDG-clean today), but nonfree inputs — firmware, blobs,
  crates, the `nonguix` channel — may be adopted when hardware or a track needs
  them, declared and pinned like any other input; the former STOP-and-ask rule
  for nonfree code is retired. Not implied and unchanged: the loop's
  hermeticity (offline, substitutes disabled inside, channel-pinned) — that is
  a reproducibility rule, not a free-software rule.
- **Substitutes / build-farm trust.** *(relaxed 2026-06-11)* Local-build purism
  is not a current goal: the host store may be warmed from the official
  substitute servers for pinned-channel closures (the Rust toolchain is the
  motivating case). Inside the loop, substitutes stay disabled and builds stay
  offline — that rung is untouched. Revisit trust-agnostic substitution
  (decentralized build attestation) much later.

---

## 6. Parking lot / open questions

Things raised that aren't current decisions — kept here so they aren't lost and don't
expand scope. An item leaves this list by graduating to the §7.1 roadmap (with human
approval) or by being resolved (record in `HISTORY.md`); it is then deleted here, not
annotated.

- Trust-agnostic substitution / decentralized build attestation (§5).
- **td-check** (follow-on to td-builder): own the reproducibility oracle —
  td-builder's rebuild-and-compare replaces `guix build --check` (semantics
  already pinned by the rootless track: compare against the recorded NAR hash,
  refuse invalid outputs). Replacing that rung restructures the loop, so §4.3
  gate 2 applies when this graduates. (The separable WHEN-it-runs question —
  verdict memoization — was approved 2026-06-12 as the check-memo track,
  §7.1; td-check inherits that policy and its constraints unchanged.)
- **Loop tooling convergence** (follow-on): td-builder's sandbox replaces
  `guix shell -C` in `check.sh` — the north star's "one sandbox stack spanning
  build and run" made literal. Restructures the loop, so §4.3 gate 2 applies
  when it graduates.
- **composefs** (re-parked from M11): reconsider if/when cross-generation dedup earns
  its place — it would replace, not extend, the per-generation-image design, and is
  not in the pinned Guix.
- **Decoupling image from slot** (§2.7): one image placeable in any slot, root chosen
  at place time; not needed by M12.
- **M12 key distribution** (§2.7): how the target gets the verifying public key —
  decide in `plan/m12.md` when the track starts.

---

## 7. Roadmap and parallel work *(added 2026-06-10)*

This section exists so multiple agents can work long-running tasks concurrently and
validate their own work, with the human's gate being the per-PR review (§4.3).

### 7.1 Roadmap *(descriptive, not a gate)*

A running list of in-flight and planned work, kept for coordination — **not** a
prerequisite. You may build something that isn't listed here; the PR review is the
approval (§4.3). Status lives in `PLAN.md`; per-track working state in
`plan/<track>.md`. Add or refine entries freely as work evolves.

**Mainline** (serial — each builds on the last; one agent drives it at a time):

- **M10.3 — manual rollback + declared persistence.** From a disk carrying two
  placed generations, the marionette test boots generation N, asserts its identity
  (root label / system), reboots selecting generation N−1 from the GRUB menu, and
  asserts the older identity. Persistence is asserted per the §2.6 state model, in
  both directions: a declared `td-state` allowlist path written under generation N
  persists into the N−1 boot; an undeclared write does not follow the swap. (§2.6,
  settled 2026-06-10, governs over older "placed state persists" wording.) Detail:
  `plan/m10.md`.
- **M11 — verified generations.** A generation's root carries build-time integrity
  metadata; booting an intact generation succeeds while a corrupted root fails closed
  (verified-red by corrupting bytes). Mechanism *(settled 2026-06-10)*: **dm-verity
  over the per-generation root image**, ChromeOS-style — `veritysetup format` (fixed
  salt) emits a hash tree + root hash at build; the hash rides the kernel cmdline in
  the GRUB menuentry, derived from the image whose digest M12's detached signature
  covers (§2.7) — transitively signed content, placed by a placer that verified it.
  M10.3's ext4 image becomes the verity data device unchanged. fs-verity alone cannot
  verify a root (per-file only — no directory structure; the needed enumerator is
  composefs); composefs is re-parked for if/when cross-generation dedup earns its
  place — it would replace, not extend, the per-generation-image design, and is not
  in the pinned Guix. Verification boundary, stated honestly: at boot, only the root
  partition is verified; the contents of `/boot` — kernel, initrd, and the cmdline
  carrying the hash — are trusted as placed, not re-verified. (Per-generation roots
  are labeled partitions assembled from the placer's root.img, not files under
  `/boot`.) M12 adds placement-time authenticity (only
  signed images get placed); boot-time verification of `/boot` (a signed boot chain)
  is off-roadmap. Integrity ≠ authenticity: signatures are M12.
- **M12 — signed distribution.** A generation image is pushed to and pulled from a
  registry (local/offline inside the loop), its signature verified before placement;
  the placer rejects unsigned or tampered images (verified-red). Identity convention
  and pre-decisions (digest = identity, signing vehicle, registry shape, downgrade
  policy): §2.7.

**Side-tracks** (parallel-safe; mostly disjoint from mainline files; any number may
run concurrently):

- **rootless-builder** — build the target with a rootless user-namespace builder and
  prove daemon-vs-rootless store-path equality (the prime-directive-4 differential;
  the daemon is the oracle). Deferred from M10.1. `plan/rootless-builder.md`.
- **offline-isolation** — CLOSED 2026-06-11, half delivered / half rescoped (human
  sign-off per §4.3). Delivered: a deliberate undeclared fetch (non-fixed-output
  network access) demonstrably fails, asserted every loop (the `offline` rung,
  verified-red). Rescoped: isolating the daemon's network and dropping nonguix from
  its substitute set is deferred to the era when td runs its OWN builder daemon
  (rootless-builder and successors) — the shared host daemon is the owner's machine
  state, needed for the host's own (nonguix) maintenance, and is not td's to
  isolate. The ready-to-resume assertions, evidence, and netns design are archived
  in `plan/offline-isolation.md`. Standing follow-up from M6.
- **oci-load** — verify the generation image loads in a foreign OCI runtime without
  breaking the offline loop (podman already rejected at M8; probe cheap vehicles or
  prove spec conformance structurally). Deferred from M10.1. `plan/oci-load.md`.
- **loop-latency** — qcow2 overlay / CoW VM reset (§1.5) and other cycle-time wins;
  measured improvement with the loop green and per-test ephemerality intact.
  `plan/loop-latency.md`.
- **fhs-app-images** — FHS-style root layout for *app* images (the base stays
  minimal per M9); an FHS app image builds reproducibly and runs on the base host
  rung. `plan/fhs-app-images.md`.
- **td-builder** *(approved 2026-06-11 — the first Guix-component replacement,
  under the §2.5 discipline)* — td's own builder: a Rust binary that executes a
  `.drv` in a user-namespace sandbox and registers the output. Acceptance: the
  daemon-vs-td-builder store differential, run as a self-discriminating rung —
  the same drvs (a trivial gexp drv, an environment-sensitive divergence probe,
  and the system image drv) built both ways yield NAR-hash-equal outputs at
  identical store paths, with `guix-daemon` as the oracle (prime directive 4);
  verified-red by a deliberate builder defect the rung catches. The
  rootless-builder harness (DB snapshot, staged store, validity guards,
  isolation probe) is the rung's skeleton. Vehicle and toolchain posture: §5.
  Follow-on swaps (td-check, evaluator-as-library, loop convergence) are parked
  in §6 until they earn their own entries. `plan/td-builder.md`.
- **ci-gate** *(approved 2026-06-11; re-decided to PR form later that day)* — a
  GitHub Actions runner (hosted, fed by the CI store image —
  `ci/build-ci-image.sh` snapshots the warm build closure, the job imports it)
  executes the **unmodified** `./check.sh` for every PR into branch-protected
  main and posts the verdict as a check; once the image is published, that
  check is required to merge alongside the mandatory human review (§7.2).
  Acceptance: a green candidate PR shows a passing `check` run and merges
  (rebase/squash) onto protected main; a deliberately red candidate (broken
  assertion on a branch — the verified-red) shows a failing `check` run and
  branch protection blocks its merge. CI only: distribution/CD automation
  waits for M12 and a future entry. The hosted-runner design sidesteps the
  runner-host question (t5700g stays untouched — standing immutable-infra
  rule); image mechanics and constraints: `plan/ci-gate.md`.
- **check-memo** *(approved 2026-06-12 — this entry is the §4.3 gate-2
  sign-off: it loosens when an existing assertion runs)* — verdict
  memoization for the `guix build --check` reproducibility legs: skip the
  rebuild-and-compare when a recorded verdict shows the SAME drv hash already
  rebuilt bit-identically in the same environment and the verdict is fresh;
  any miss (changed drv, expired TTL, foreign environment, force-full) runs
  the real `--check` unchanged. Acceptance: the unchanged-tree `./check.sh`
  floor drops measurably with all rungs green, a force-full knob runs the
  original full ladder, and four verified-reds hold (changed drv always
  rebuilds; expired verdict rebuilds; foreign verdict rejected; injected
  nondeterminism on a miss still reds). The binding constraints — drv-hash
  keying, host-local uncommitted verdicts (CI reuse re-opens gate 2),
  bounded TTL, force-full on oracle re-baselines — and the accepted
  detection trade (environment-dependent outputs on unchanged drvs, e.g.
  the 2026-06-12 hosted-runner readdir-order case) are pinned in
  `plan/check-memo.md`; changing any of them re-opens gate 2.

- **ci-image-pipeline** *(approved 2026-06-12)* — a GitHub workflow builds AND
  pushes the CI store image; no human-run commands. Bootstrap a hosted runner
  exactly as the `check` job does (substitutes allowed — image PREP may fetch,
  §5 "warm store in"; the loop stays offline), run the in-repo generator,
  push a CANDIDATE tag with the workflow's `GITHUB_TOKEN` to the REPO
  namespace (`ghcr.io/timmydo/td-ci` — retiring the bot-namespace workaround),
  then a second job pulls the candidate, runs the full unmodified
  `./check.sh` against it, and only on green retags to `:<pin>`.
  Acceptance: a channel-bump (or rung-addition) PR plus one workflow run
  yields a published `:<pin>` image that a green `check` run consumed, with
  no command run on a user machine. Design notes: `plan/ci-gate.md`
  ("pipeline-built CI store image").
  **Policy (human, 2026-06-12), binding repo-wide:** ALL generated artifacts
  are produced on pipelines, never on a user's machine; any exception must be
  documented with explicit human sign-off. (Documented exception under this
  policy: CI store images v1–v3 were dev-box-built and hand-pushed during
  ci-gate bring-up — signed off 2026-06-12, retired by this entry.)
- **ts-frontend** *(approved 2026-06-12 — §4.3 gate-1 roadmap addition; the
  first step of the §5 move-off-Guile goal)* — Phase 1 of the spec-language
  migration: a **TypeScript** surface for td's system/package specs, evaluated
  hermetically and lowering to drvs, with the frozen Guile oracle unchanged as
  the differential (§2.5). Pipeline: TS→JS via swc, evaluated by an embedded
  **boa** engine (pure-Rust, in-process, run inside td-builder's existing
  user-namespace sandbox); the global is stripped to a curated set (no
  `fetch`/`fs`/`process`, `Date` removed, `Math.random` denied) so eval is
  deterministic and offline by construction; lowering builtins — corpus package
  lookup and `storeRef` (the gexp `#$`-style single-source dependency capture:
  store path + input edge in one Rust fn) — are boa native functions.
  Acceptance: a TS spec for the v0 system lowers to a system derivation
  NAR-hash-equal to `system/td.scm` (the same convergence `tests/typed-diff.scm`
  proves for `td-typed`), run as a self-discriminating rung; a perturbed TS spec
  diverges (verified-red); and a spec that attempts I/O (network/fs/clock/
  randomness) is rejected by the hermetic evaluator (verified-red by a probe
  spec that must fail). Scope is the spec *language* only — corpus replacement
  is Phase 2 (§6), and this track keeps reading the pinned corpus underneath.
  The curated-global design and the swc/`tsc` build steps: `plan/ts-frontend.md`.
- **corpus-independence** *(approved 2026-06-13 — §4.3 gate-1 roadmap addition,
  graduated from §6; Phase 2 of the §5 move-off-Guile goal, follow-on to
  `ts-frontend`)* — replace the pinned Guix corpus with td's OWN recipes for the
  target closure, pulling upstream source directly, so td depends on no
  general-purpose distro at corpus level. Bounded by the §5 non-goals
  (appliance-scoped, no full-source bootstrap, seed external) and §2.5/prime-
  directive-4 (the Guix corpus is the oracle; the migration is proven by
  differential, never asserted). Axis note: this is the CORPUS axis (where the
  package definition comes from), distinct from `ts-frontend`'s SURFACE axis (what
  language a spec is written in) — and the two compose: a recipe is AUTHORED in the
  TypeScript surface and lowered through the still-present Guile/gexp layer (the
  sanctioned lowering target, retired LAST), with the toolchain + build-system also
  Guix's (retired last). What changes is provenance: the recipe is reconstructed
  from upstream coordinates (source URL + hash + build system), NOT looked up in
  `(gnu packages …)`. Acceptance (the POC increment): a recipe for one leaf package
  (GNU `hello`) authored in TypeScript (`tests/ts/recipe-hello.ts`) — transpiled by
  tsc, evaluated by the boa evaluator (which gains `recipe`/`fetchSource` capture
  globals), and lowered by a GENERIC Guile recipe bridge (`system/td-recipe.scm`,
  importing no `(gnu packages …)`) — lowers to a derivation NAR-hash-equal
  (store-path-equal) to the same package built from the pinned Guix corpus, run as
  a self-discriminating rung: the TS recipe CONVERGES on the corpus oracle while a
  perturbed TS recipe DIVERGES (verified-red, never vacuous), and the BUILT artifact
  is reproducible (`guix build --check`) with its output NAR hash equal to the
  corpus oracle's. **Own-builder increment (DONE 2026-06-13):** the "behaviorally
  equal where a recipe legitimately differs" case — the same TS recipe lowered
  through `system/td-build` (a raw `derivation` whose BUILDER is the td-builder Rust
  binary's `autotools-build` mode, builder/src/build.rs) instead of
  gnu-build-system, so gnu-build-system AND build-time Guile are removed from the
  build (guix still constructs the .drv — scope fixed by the human 2026-06-13:
  replace gnu-build-system, keep guix for .drv construction; the toolchain stays
  Guix's, retired last). The own-builder output has a distinct store path, so the
  `td-build` rung proves equivalence BEHAVIORALLY (byte-identical program output to
  the corpus hello) + STRUCTURALLY (the derivation's builder is `td-builder`, not
  `guile`) + reproducibly (`--check`). Remaining follow-ons: broadening the recipe
  set toward the full target closure (more build systems, packages with inputs), and
  de-Guiling the `.drv` construction itself (the §6 "evaluator as a library", a
  separate charter). Working state + verified-red log: `plan/corpus-independence.md`.
- **evaluator-as-library** *(approved 2026-06-13 — §4.3 gate-1 roadmap addition,
  graduated from §6; the §5 move-off-Guile goal, follow-on to corpus-independence's
  own-builder increment)* — remove Guile from the `.drv` CONSTRUCTION itself. Today
  `system/td-build.scm` calls Guile's `derivation` to compute the output path,
  serialize the ATerm, and write the `.drv`; this moves that construction into the
  td-builder Rust binary (the §6 "drive gexp→drv lowering from td code so the `guix`
  CLI exits the loop"). The differential is the one §6 named — **identical `.drv`
  both ways**: td-builder emits a `.drv` byte-identical (same store path AND same
  bytes) to the one guix's `derivation` produces for the same spec, with guix as the
  oracle (§2.5 / prime directive 4), run as a self-discriminating rung (a perturbed
  emitter diverges; verified-red). Vehicle: **Rust** (human, 2026-06-13), reusing the
  td-builder crate's ATerm parser + SHA-256. Scope boundary: input RESOLUTION (which
  toolchain/source store paths are inputs) stays Guix's for now — the toolchain is
  retired last (§5); what moves to Rust is the `.drv` construction (ATerm serialize +
  `nix-base32`/`make-store-path` + the recursive `hashDerivationModulo` for output
  paths). Target subject: the `td-build` hello derivation. NOT this entry: replacing
  the reproducibility oracle (td-check) or `guix shell -C` (loop convergence) — both
  remain §6 gate-2 items. **DONE 2026-06-13:** the `drv-emit` rung — td-builder
  re-constructs the `td-build` hello `.drv` byte-identical (store path + content) to
  guix's, validated over hundreds of real store drvs; a perturbed recipe is a distinct
  drv it also matches; verified-red ×2. Working state + verified-red log:
  `plan/evaluator-as-library.md`.
- **td-drv-build** *(approved 2026-06-13 — §4.3 gate-1 roadmap addition; the capstone
  of the §5 move-off-Guile arc, follow-on to evaluator-as-library + the own Rust
  builder + td-builder)* — the end-to-end td-driven build: for the `td-build` hello
  subject, td-builder EMITS the `.drv` (#22) AND EXECUTES it in its own user-namespace
  sandbox (the td-builder S3/S4 build path), producing an output NAR-equal to the
  daemon's build of the same recipe. So construct AND execute are td's Rust — the
  derivation's builder is `td-builder autotools-build` (#21) run by `td-builder build`,
  with NO Guile in either; the daemon is ONLY the differential oracle (prime directive
  4). Acceptance: a rung that (a) has td-builder write the emitted `.drv` (byte-
  identical to guix's), (b) builds it in the td-builder sandbox, and (c) asserts the
  registered output — store path, NAR hash, size, deriver — equals the daemon's
  recorded facts; self-discriminating + verified-red (an emit defect breaks byte-
  identity; an executor defect breaks the NAR differential). Scope boundary, stated
  honestly: input RESOLUTION (which toolchain/source paths are inputs) and the input
  CLOSURE computation stay Guix's, and the daemon still BUILDS the inputs — only the
  TOP derivation (hello) is td-constructed + td-executed; the toolchain is retired
  last (§5). Reuses the td-builder S3/S4 harness. Working state + verified-red log:
  `plan/td-drv-build.md`.
- **td-drv-add** *(approved 2026-06-13 — §4.3 gate-1; the §5 move-off-Guile arc,
  follow-on to evaluator-as-library + td-drv-build)* — wire td's constructed `.drv`
  INTO the loop: td-builder constructs the `.drv` (#22) and REGISTERS it in the store
  itself via the daemon's `addTextToStore` RPC — a minimal Rust worker-protocol
  client (`builder/src/daemon.rs`, transcribed from `(guix store)`/`(guix
  serialization)` at the pin) — so the `.drv` enters the store with NO guile
  `(derivation …)`/`add-text-to-store`. The daemon (C++) stays the store/build
  backend; the GUILE client is what's removed. Acceptance: a rung where (a) `drv-add`
  registers the hello `.drv` and the daemon returns td's OWN computed path (== guix's,
  by content addressing), (b) `store-add` of a uniquely-named object proves the daemon
  actually WRITES td's bytes at a NOVEL path (not idempotent reuse — this is the leg
  that causally proves td's registration, since the skeleton `.drv` is guile-lowered
  and thus already present), and (c) `guix build` of the td-registered `.drv` builds it
  to a working hello (NAR-equality follows from the shared content-addressed path);
  verified-red. Scope: input RESOLUTION (the skeleton) stays Guix's; the daemon is the
  backend.
  Working state + verified-red log: `plan/td-drv-add.md`.

### 7.2 Landing protocol — merge on green, via PR *(PR gate added 2026-06-11)*

Each agent works one claimed track in its **own git worktree/branch** — never
directly on a shared checkout of main. Main is branch-protected: no direct
pushes; every landing is a pull request gated on required CI checks and one
human approval (`.github/BRANCH-PROTECTION.md` is the setup/operations note).
To land:

1. fetch and rebase onto latest `origin/main`;
2. run the **full** `./check.sh` — it must be green;
3. push the branch and mark its PR ready for review; CI re-runs the gate
   (`lint` + the full `./check.sh` on a hosted runner via the CI store image);
4. on green CI and human approval, rebase- or squash-merge (merge commits are
   disabled — history stays linear, as under the old fast-forward rule);
5. if main moved meanwhile, go to 1.

The human approval replaces the old "no human merge step; review-after on
main", and this PR protocol supersedes the same-day no-PR amendment (the human
re-decided later on 2026-06-11: PRs with mandatory review, not status-gated
fast-forwards). The runner's `./check.sh` check joins the required checks once
the ci-gate runner is live (until then `lint` is required and step 2 is the
only full-loop gate); it counts toward the §7.3 two-concurrent-checks ceiling
only if the runner shares a host with dev checks — on its own host, stagger
landings as a courtesy. "Validated" still means green against the main
actually merged into — opening a ready PR with a locally-red or un-run full
`./check.sh` is a contract violation (CI verifies the agent's run; it does
not replace it).

Claims: one agent per track, recorded on the track's status line in `PLAN.md`
as the first commit of the track branch, published by opening the PR as a
draft — claim status is `PLAN.md` on main plus the open PRs' claim edits;
generation mechanics live in `CLAUDE.md` "Parallel work".

### 7.3 Exclusive landings

Changes touching the shared spine — `system/td.scm` (the frozen oracle), `check.sh`,
`Makefile`, `channels.scm`, `DIGESTS.md` — collide with every other agent. Land them
as small standalone PRs, announced in your track file; everyone else rebases.
Oracle re-baselines (which rewrite `DIGESTS.md`) and channel-pin bumps are the
canonical cases. These are coordination rules, not sign-off gates — but remember
§4.3(2): *weakening* anything in the spine still needs the human.

Resource note: each full check already runs its heavy rungs two at a time (`-j2`);
two concurrent full checks therefore mean up to four VMs/builds — observed fine on
this host during the M10.3/loop-latency overlap, but treat that as the ceiling:
don't add a third check or raise `-j`. Stagger landings if loaded.

### 7.4 Files

`PLAN.md` — status index only, one line per track; keep edits tiny so rebases are
trivial. `plan/<track>.md` — per-track working state, single writer (the claiming
agent). `HISTORY.md` — completed-milestone record. `DIGESTS.md` — reproducibility
record (changes only on re-baseline, exclusive landing).
