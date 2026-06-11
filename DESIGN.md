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

Named as staying out (off-roadmap today): the Rust build daemon, the unified
sandbox/portal broker, multi-machine tests, real-hardware/driver work.

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
  `system/td-generation.scm`) over the manifest digest fits FSDG + offline.
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

(The retired per-milestone sign-off gate and its dates: `HISTORY.md`.)

---

## 5. Guix-specific decisions

Standing posture decisions; naming them prevents surprises.

- **Guile.** Embrace it; the typed front-end compiles down to gexps (§2.2).
- **Rust coexistence.** Deferred. Document later how Rust components sit alongside the
  Guile-based daemon when that milestone arrives.
- **Free-software posture.** Strict FSDG — follow Guix's free-software guidelines. No
  nonfree firmware, blobs, or crates; no `nonguix` channel. If a task appears to need
  nonfree code, STOP and ask.
- **Substitutes / build-farm trust.** Local builds only. Revisit trust-agnostic
  substitution (decentralized build attestation) much later.

---

## 6. Parking lot / open questions

Things raised that aren't current decisions — kept here so they aren't lost and don't
expand scope. An item leaves this list by graduating to the §7.1 roadmap (with human
approval) or by being resolved (record in `HISTORY.md`); it is then deleted here, not
annotated.

- How Rust components will eventually coexist with the Guile daemon (§5).
- Trust-agnostic substitution / decentralized build attestation (§5).
- **composefs** (re-parked from M11): reconsider if/when cross-generation dedup earns
  its place — it would replace, not extend, the per-generation-image design, and is
  not in the pinned Guix.
- **Decoupling image from slot** (§2.7): one image placeable in any slot, root chosen
  at place time; not needed by M12.
- **M12 key distribution** (§2.7): how the target gets the verifying public key —
  decide in `plan/m12.md` when the track starts.

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
recorded on the track's status line in `PLAN.md` (a tiny standalone commit to main)
under a session-unique handle — `PLAN.md` is the single source of truth for claim
status; generation mechanics live in `CLAUDE.md` "Parallel work".

### 7.3 Exclusive landings

Changes touching the shared spine — `system/td.scm` (the frozen oracle), `check.sh`,
`Makefile`, `channels.scm`, `DIGESTS.md` — collide with every other agent. Land them
as small standalone commits, announced in your track file; everyone else rebases.
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
