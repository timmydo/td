# Design document — td

A functional Linux distribution, built incrementally by an AI coding agent (Claude
Code) on top of an existing Guix system, growing inside a fast, machine-checkable
verification loop.

This document is the settled contract the agents work against. It pins the three
things an agent can't decide for itself — **the loop it runs to check its work**,
**the target it's aiming at**, and **the scope it may work without sign-off** (the
§7.1 roadmap). Everything else the agents may propose and iterate on. Section numbers
are stable anchors; `CLAUDE.md` references them, so keep them.

It states the north star and the standing decisions — **not the history**: how we got
here lives in `HISTORY.md` and git, and milestone narration does not belong in this
file. To stay DRY, each volatile fact lives in exactly one place and everything else
points at it: the gate list in the `mk/gates/*.mk` fragments, claim status in the open
draft PRs, completed milestones in `HISTORY.md`. `CLAUDE.md` mirrors only the stable
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
offline sandbox — **td's own `td-builder host-sandbox --expose-cwd`, the sole loop
container** (no `guix shell -C` fallback, no toggle; §7.1): the whole `/gnu/store` (ro)
+ the daemon socket, a private PID namespace + `/proc`, its own loopback-only netns,
host guix + the toolchain on PATH, a guard that host guix matches the `channels.scm`
pin, substitutes disabled — and runs `make check` inside it. `make check` runs the gate
ladder, short-circuiting on the first failure; **the drop-in fragments under
`mk/gates/*.mk` (each self-registering into the `CHEAP_GATES`/`HEAVY_GATES` pools the
`check:` target expands) are the authoritative gate list** — documents point here
instead of restating it, and a new gate is a new fragment file, not an edit to a shared
list. Broad shape: config eval → differentials → `guix build --check` →
package-manager behavioral/oracle tests (built tools run, link tests, the
per-package guix differential). The whole-OS boot tier — marionette `(gnu tests)`
system tests and `guix system image` builds — is **parked out of the default
`check` into the on-demand `check-system`** while td's focus is the user-space
package manager (a human-directed scope call: Makefile `check-system`, DESIGN §4.3);
`check-fast` is the cheap + typed-front-end subset CI runs. Plain `make check` is
only correct when you're already inside that sandbox.

### 1.2 Rung classes

- Hermetic build/dev env: td's own `td-builder host-sandbox` (no host leakage; §7.1).
- Reproducibility oracle: `guix build --check` (and `--rounds=2` where cheap). A
  non-reproducible output is a failing test.
- Boot + behavioral: marionette `(gnu tests)` system tests that boot the image and
  drive the guest from Guile — run in the on-demand `check-system` tier, parked out
  of the default `check` (§1.1).

Fuzzing/adversarial and real-hardware rungs are deferred (off-roadmap).

### 1.3 Loop-latency budget

Target under ~60s per write→check cycle on a warm store. To stay there, layer changes
onto a prebuilt base image and reserve full `guix system vm` rebuilds for a
less-frequent rung. Loop latency is a tracked metric, not an afterthought.

### 1.4 Agent / container boundary

The Claude Code agent runs **outside** the container. Every build/test command it
issues enters a **fresh** container — td's own `td-builder host-sandbox` (the SOLE loop
container; §7.1) — so the agent's own environment can't contaminate results and the
reproducibility rung stays honest. Every rung runs there, including `rootless` (its
nested unprivileged builder nests cleanly given td's PID-namespace parity).

### 1.5 VM state reset

The harness is **fully ephemeral per test**: boot from a fresh image, wipe all
writable state on reset. That is test isolation, not a ban on persistence *within* a
test (§2.6, §3). The CoW reset (QEMU `qcow2` overlay) is in place and asserted by
the `reset` rung (landed 2026-06-10).

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
  The PR states the probe criterion up front, M8-style — offline-
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
- **Named open question (carry it in the PR):** key distribution — how the
  target gets the verifying public key (placer flag, well-known path on
  `td-state`, baked into the placer build). Decide when the
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
  javy) and the hermetic-eval design.
- **Rust toolchain** *(decided 2026-06-11; seed tarball 2026-06-20)*. Rust is the
  approved vehicle for td-builder (§7.1). Building the rustc/gcc toolchain *from
  source* is not required — but it is part of the **seed** that the North Star
  freezes into the pinned binary **seed tarball** (so it stops being a live guix
  dependency). Until that lands, the host store is warmed with the pinned
  channel's Rust closure; either way the loop stays offline/no-substitutes — warm
  store (eventually: the seed tarball) in, nothing fetched inside.
- **Package collection — corpus + runtime independence is now a goal**
  *(re-decided 2026-06-12, human — the roadmap addition the prior posture
  invited).* The prior posture (Guix as a pinned corpus input, re-derivation a
  non-goal) is superseded: td will own its own package/system specs and depend
  on **no general-purpose Linux distro** (Guix, Nix, Debian, …) at corpus or
  runtime level. Independence is source-level on *upstream projects*, not from
  scratch: td writes its own *recipes* pulling upstream *source* (kernel.org,
  GNU, …) — replacing the distro's packaging, not the software. Two bounds keep
  this from boiling the ocean:
  - **General-purpose comprehensiveness** *(still a non-goal)*. The corpus is td's *target closure*
    — an appliance/image OS, Yocto/Buildroot scale (hundreds of packages), NOT
    a Nixpkgs-scale general distro.
  - **Seed toolchain — a FULL-SOURCE BOOTSTRAP at `/td/store`, no guix bytes ever**
    *(re-decided 2026-06-21, human — supersedes the 2026-06-20 "frozen guix-captured
    seed tarball").* The north star is **no guix process AND no guix-built bytes**.
    A guix-captured seed — *even a static one* — fails the "no bytes" half: a static
    `bash` still embeds `/gnu/store` strings (measured: 11, incl. glibc
    locale/gconv/zoneinfo + a bare `/gnu/store`) and its provenance is guix. Removing
    those by a `/gnu/store→/td/store` byte rewrite (relocation) leaves guix-*built*
    bytes, just relabeled. So a guix seed is rejected as the foundation. Instead td's
    toolchain is **built from source at `/td/store`** from a tiny auditable seed — the
    well-trodden bootstrappable-builds chain (stage0-posix `hex0` → `M2`/`mes` →
    `mescc-tools` → `tinycc` → `gcc` → `glibc` → binutils/coreutils/bash/make/…),
    every stage configured `--prefix=/td/store`. No `/gnu/store`, no guix process, no
    guix bytes — in strings or in provenance. This is the FOUNDATION, built **first**,
    before the user-PM/corpus layering rests on it. It is a *port* of an existing,
    reproducible bootstrap (guix's own Full-Source Bootstrap, live-bootstrap), not
    research.
    The build engine it targets — `td-builder build` staging inputs + `NIX_STORE` at
    the active `store::store_dir()` (`/td/store`), so a `/td/store` build is *native*,
    re-hashed and rewrite-free — already exists (user-pm Phase 1/3, `TD_STORE_DIR`).
    guix may still appear ONLY as a removable differential oracle (build the same
    source both ways, diff the trees — directive 4), never as a build input.
    The detailed provenance model this rests on — *no guix process* vs *no guix
    bytes* as two independent properties, what contributes bytes to an artifact
    (source + compiler + libc) vs the build-driver tools (which leave none), the
    operational (`grep /gnu/store`) vs provenance tests, and the per-artifact
    status of the daily-suite captured set (busybox/make guix-byte-free via
    td-fetch + the `/td/store` toolchain, dynamic vs the shared `/td/store` glibc;
    td-builder on upstream rust) — is detailed in §5.1 below.

  Phase 1 (`ts-frontend`, §7.1) replaces the spec *language* and keeps reading
  the pinned corpus underneath; corpus replacement is Phase 2, separately gated
  (§6).
- **New seeds are td-placed fetches, not guix packages** *(move-off-Guile
  enforcement)*. An external seed/tool is a pinned fixed-output FETCH the loop
  realizes + td PLACES (`store-add-recursive`), never a guix `(build-system …)`
  package built via `guix build -e '(@ (system M) pkg)'` / `specification->package`.
  The `guix-surface` gate (`tests/guix-surface.sh`, snapshot
  `tests/guix-surface.expected`) ratchets that "guix-as-packager" surface one-way —
  it may only shrink; growing it is a regression needing a deliberate snapshot edit
  + sign-off (CLAUDE.md directive 7). The existing seed packages (`td-builder`,
  `td-ts-eval`, `td-typescript`) are the baseline, retired by their own tracks.
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

### 5.1 Provenance model — what "off guix" means for td artifacts

The North Star ("remove guix entirely — no guix *process* AND no guix *bytes*")
is two separate claims, and the distinction is subtle and was repeatedly
re-derived; this subsection is the reference.

**Two independent properties.** An artifact can satisfy one without the other:

1. **No guix process** — nothing on the running/target machine invokes `guix` (no
   `guix build`, no `guix-daemon`, no guix on `PATH`). About the *machine*, not the
   bytes.
2. **No guix bytes** — no byte in the artifact originated from guix: not guix-built
   machine code, not a `/gnu/store` string, not a guix-compiled library statically
   linked in. About *provenance*, traced through the build.

The daily-suite-on-a-guix-less-VM goal needs **(1) on the VM** unconditionally.
**(2)** is the stronger, "retire guix last" goal, pursued per-artifact where feasible.

**What contributes bytes to an artifact (and what does not).** When you build a
binary, only some inputs leave bytes in the result:

- **Source code** → compiled in. Provenance = where the source came from.
- **The compiler** (`gcc`/`rustc`) → its code generation *is* the binary's machine
  code; its bundled runtime (`libgcc`, rust `std`) is linked in.
- **The C library** → for a *static* link, `libc.a`/`libm.a` bytes are copied in; for
  a dynamic link, only an interpreter path + `NEEDED` names (the `/gnu/store` strings
  that betray guix provenance).
- **Build-driver tools** (`make`, `sed`, `coreutils`, `bash` running
  `configure`/recipes) → **leave no bytes in the output.** They orchestrate the build;
  their own provenance is irrelevant to the artifact's. You can drive a guix-byte-free
  build with guix's own `make`/`sed` and the *output* is still guix-byte-free.

The practical consequence: to make an artifact guix-byte-free you control its
**source, compiler, and libc** — not the tools that drive the build.

**Two tests.**

- **Operational (cheap, necessary):** `grep -c /gnu/store <artifact>` is `0`. Catches
  the obvious leak (dynamic interpreters, baked store paths). A guix-built static
  `bash` fails this — it embeds 11 `/gnu/store` strings (measured).
- **Provenance (strong, sufficient):** every byte traces to {upstream source} ∪ {td's
  own from-source toolchain}. Established by *how it was built*, not by grepping: built
  from a td-fetch-pinned source with the `/td/store` toolchain ⇒ guix-byte-free by
  construction.

**The `/td/store` toolchain is what makes C artifacts guix-byte-free.** td's
from-source bootstrap (hex0 → mes → tcc → gcc-mesboot → gcc 14.3.0 → glibc 2.41,
binutils 2.44; x86_64 cross) produces a C toolchain at `/td/store` whose own
provenance is a tiny auditable seed — no guix bytes. Compiling C with `/td/store` gcc
+ linking `/td/store` glibc is therefore guix-byte-free at the source. (The
`bootstrap-*-store-native` recipes build it; there is no persistent `/td/store`
artifact today — it is rebuilt from the seed, which is why a guix-byte-free capture
must stand one up.)

**Provenance of the captured set (daily-suite harness).** The harness binds three
binaries — dynamically linked against the shared `/td/store` glibc 2.41 (interp +
RUNPATH → `/td/store`, no `/gnu/store`) — into a sandbox that mounts `/td/store`
(`host-sandbox --store-from /td/store --no-daemon`), `/gnu/store` absent. Same model
as the `rust-store-native` gate, which already runs `rustc` dynamically from
`/td/store`.

| binary | language | source | compiler | libc (dynamic, from `/td/store`) | guix bytes? |
|--------|----------|--------|----------|------|-------------|
| busybox | C | upstream (td-fetch, sha-pinned) | `/td/store` gcc | `/td/store` glibc 2.41 (shared) | **none** |
| make | C | upstream (td-fetch, sha-pinned) | `/td/store` gcc | `/td/store` glibc 2.41 (shared) | **none** |
| td-builder | Rust | in-tree `builder/` | upstream rustc relinked to `/td/store` (`elf.rs`) | `/td/store` glibc 2.41 (shared) | none of guix; rust = upstream |

- **busybox / make** reach full provenance-purity: upstream source + td's own
  toolchain.
- **td-builder** is Rust. The rust toolchain is **upstream** (the released rust
  binaries — *not* guix-compiled rust; any `rustup` toolchain builds it). The
  `rust-store-native` track td-fetches the upstream tarball (sha-pinned, no guix) and
  relinks it to `/td/store` with td's own ELF interp rewriter (`builder/src/elf.rs`,
  no patchelf), then runs `rustc`/`cargo` from `/td/store` in an own-root with
  `/gnu/store` absent. Caveats: (a) `tests/td-builder-rust.lock` still points
  td-builder's *build* at the guix-placed rust — switching it to the `/td/store`
  relinked rust is `rust-store-native` rung 3 (*compiling* with it; *running* it is
  already proven); (b) the rust bytes are an upstream *binary*, not from-source —
  from-source rust provenance is out of scope for now, but it is upstream, not guix.
- The **build-driver** tools that run the `configure` + recipes may be guix's (they
  leave no bytes in the output).

**Why guix appears in the capture today (and the retarget).** The current
`tools/build-static-*.sh` use guix on the *capture host* for convenience (`guix build
-S <pkg>` for sources, the guix `gcc-toolchain` + `glibc:static` to compile) — so the
*output* carries guix glibc bytes — fine for "no guix process on the VM", **not** "no
guix bytes". The retarget removes guix from the capture: **sources** → `td-fetch`
upstream tarballs pinned in `seed/sources/*.lock`; **compiler + libc** → the
`/td/store` gcc + the shared `/td/store` glibc 2.41 (dynamic link). guix then touches
the capture only if you *choose* it as a build-driver; nothing it provides ends up in
the shipped binaries.

---

## 6. Parking lot / open questions

Things raised that aren't current decisions — kept here so they aren't lost and don't
expand scope. An item leaves this list by graduating to the §7.1 roadmap (with human
approval) or by being resolved (record in `HISTORY.md`); it is then deleted here, not
annotated.

- Trust-agnostic substitution / decentralized build attestation (§5).
- **td-check** — GRADUATED to §7.1 (approved 2026-06-13, §4.3 gate-2); see the
  active entry below. Own the reproducibility oracle — td-builder's
  rebuild-and-compare alongside `guix build --check` (semantics already pinned by
  the rootless track: compare against the recorded NAR hash, refuse invalid
  outputs). (The separable WHEN-it-runs question — verdict memoization — was
  approved 2026-06-12 as the check-memo track, §7.1; td-check inherits that policy
  and its constraints unchanged.)
- **Loop tooling convergence / loop-sandbox** — GRADUATED to §7.1 (approved
  2026-06-13, §4.3 gate-2); see the active entry below. td-builder's sandbox
  replaces `guix shell -C` in `check.sh` — the north star's "one sandbox stack
  spanning build and run" made literal. Additive equivalence first; the wholesale
  `check.sh` swap is a later increment.
- **composefs** (re-parked from M11): reconsider if/when cross-generation dedup earns
  its place — it would replace, not extend, the per-generation-image design, and is
  not in the pinned Guix.
- **Decoupling image from slot** (§2.7): one image placeable in any slot, root chosen
  at place time; not needed by M12.
- **M12 key distribution** (§2.7): how the target gets the verifying public key —
  decide when the track starts.
- **Build admission scheduler — "kubernetes-lite"** *(wanted next; human 2026-06-28)*.
  The per-build resource cap landed (`build-resource-caps`: opt-in `TD_BUILD_MEM_MAX`
  → a `setrlimit(RLIMIT_DATA)` backstop + a delegated-cgroup `memory.max` RSS cap on
  each build's sandbox child, scoped like the `nice` knob #212) contains ONE runaway
  build so it can't OOM the host. The wanted follow-on is a real scheduler: per-build
  memory *requests* + admission/bin-packing across ALL in-flight builds, so the loop
  never over-commits the host in the first place. That needs what doesn't exist today
  — a single arbiter every build flows through (the build daemon is serial AND not the
  only path; `make -j2` and concurrent agent checks spawn builds directly) plus a
  per-derivation resource-request field (guix `.drv`s carry none). A larger
  architectural change; parked here so the direction isn't lost.

---

## 7. Roadmap and parallel work *(added 2026-06-10)*

This section exists so multiple agents can work long-running tasks concurrently and
validate their own work, with the human's gate being the per-PR review (§4.3).

### 7.1 Roadmap *(descriptive, not a gate)*

A running list of in-flight and planned work, kept for coordination — **not** a
prerequisite. You may build something that isn't listed here; the PR review is the
approval (§4.3). The live status of in-flight work is the open draft PRs
(`gh pr list`). Add or refine entries freely as work evolves.

**Mainline** (serial — each builds on the last; one agent drives it at a time):

- **M10.3 — manual rollback + declared persistence.** From a disk carrying two
  placed generations, the marionette test boots generation N, asserts its identity
  (root label / system), reboots selecting generation N−1 from the GRUB menu, and
  asserts the older identity. Persistence is asserted per the §2.6 state model, in
  both directions: a declared `td-state` allowlist path written under generation N
  persists into the N−1 boot; an undeclared write does not follow the swap. (§2.6,
  settled 2026-06-10, governs over older "placed state persists" wording.)
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
  the daemon is the oracle). Deferred from M10.1.
- **offline-isolation** — CLOSED 2026-06-11, half delivered / half rescoped (human
  sign-off per §4.3). Delivered: a deliberate undeclared fetch (non-fixed-output
  network access) demonstrably fails, asserted every loop (the `offline` rung,
  verified-red). Rescoped: isolating the daemon's network and dropping nonguix from
  its substitute set is deferred to the era when td runs its OWN builder daemon
  (rootless-builder and successors) — the shared host daemon is the owner's machine
  state, needed for the host's own (nonguix) maintenance, and is not td's to
  isolate. The ready-to-resume assertions, evidence, and netns design are archived. Standing follow-up from M6.
- **oci-load** — verify the generation image loads in a foreign OCI runtime without
  breaking the offline loop (podman already rejected at M8; probe cheap vehicles or
  prove spec conformance structurally). Deferred from M10.1.
- **loop-latency** — qcow2 overlay / CoW VM reset (§1.5) and other cycle-time wins;
  measured improvement with the loop green and per-test ephemerality intact.
- **fhs-app-images** — FHS-style root layout for *app* images (the base stays
  minimal per M9); an FHS app image builds reproducibly and runs on the base host
  rung.
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
  in §6 until they earn their own entries.
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
  rule).
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
  the 2026-06-12 hosted-runner readdir-order case) are settled; changing any of them re-opens gate 2.

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
  no command run on a user machine.
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
  The curated-global design and the swc/`tsc` build steps.
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
  separate charter).
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
  drv it also matches; verified-red ×2.
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
  last (§5). Reuses the td-builder S3/S4 harness.
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
- **td-drv-assemble** *(approved 2026-06-13 — §4.3 gate-1; the §5 move-off-Guile arc,
  follow-on to td-drv-add)* — remove the LAST guile `(derivation …)` from the build
  path. Guile RESOLVES the inputs (toolchain + source → store paths — input resolution,
  retired last §5) and emits a raw line-based SPEC (`system/td-build.scm`
  `write-td-build-spec`: name/system/builder/arg/input-drv/env, no output paths, no
  `(derivation …)`); td-builder `drv-assemble` does the ASSEMBLY `(derivation …)` did —
  add the `out` output + env var, SORT env by key and inputs by path (the daemon's
  canonical order), compute the output path (#22 construct_drv), serialize — and
  registers it via the daemon (#27). Acceptance: a rung where td's assembled+registered
  `.drv` is byte-identical to the same recipe lowered through guix's `(derivation …)`
  (the oracle, equal store path ⇒ equal bytes) and `guix build` builds it to a working
  hello; verified-red. So nothing guile CONSTRUCTS the build derivation anymore — only
  input resolution stays Guix's.
- **td-check** *(approved 2026-06-13 — §4.3 **gate-2**, human go-ahead "then the gate-2
  items (td-check oracle, loop sandbox)"; graduated from the backlog stub above)* — td
  OWNS the reproducibility oracle. `td-builder check DRV CLOSURE SCRATCH` executes the
  `.drv` TWICE in two independent user-namespace sandbox runs (reusing the td-drv-build
  executor) and compares the per-output NAR hashes (reusing the S2 NAR serializer +
  SHA-256) — td's own `guix build --check`, with no daemon and no `guix build --check`
  in the verdict. This is the OBSERVE step of gate 2 done honestly: it does NOT remove
  `guix build --check` from any existing rung (directive 3); it ADDS the `td-check` rung
  proving td's verdict EQUALS guix's on the same `.drv` — td's reproducible NAR hash ==
  the daemon's recorded hash AND `guix build --check` agrees (directive 4, the
  differential a later replacement needs). Scope: input resolution + the closure
  (`guix gc -R`) + the daemon building the INPUTS stay Guix's; only the TOP derivation's
  reproducibility is td's double-build (toolchain retired last, §5).
- **loop-sandbox** *(approved 2026-06-13 — §4.3 **gate-2**, human go-ahead "then the
  gate-2 items (td-check oracle, loop sandbox)"; graduated from the backlog stub above)*
  — td's OWN sandbox is the **SOLE** loop container: `check.sh` runs the whole loop
  inside `td-builder host-sandbox --expose-cwd`, with NO `guix shell -C` fallback and NO
  toggle (human direction 2026-06-14: "make td the default, without a dependency on guix
  or a way to change it back"). td's `host_shell` pivots into a fresh root exposing the
  WHOLE `/gnu/store` (ro) + the daemon socket `/var/guix` + `/dev` + the worktree + the
  guix cache, host-guix + the toolchain on PATH, running as **PID 1 of its own PID
  namespace with a private `/proc`** in its own loopback-only network namespace — full
  `guix shell -C` parity (user/mount/pid/net/ipc/uts ns). That parity is what lets EVERY
  rung run nested in td's sandbox, including `rootless` (its nested unprivileged userns
  builder, which the old shared-`/proc` sandbox could not host) and the loop self-tests.
  `guix shell` (no `-C`) still provisions the toolchain profile; td replaces the
  container. The `loop-sandbox`/`loop-rung` rungs are now INTRINSIC self-tests of td's
  sandbox (store ro + daemon socket + guix, host isolation, PID-1/private-`/proc`,
  loopback-only netns; and the `--expose-cwd` full env runs a real `eval` rung) — no
  `guix shell -C` oracle (equivalence was proven over #30–#33; going forward td is
  self-described, and the build rungs still differential-check against the guix daemon
  oracle). CI runs the unmodified td-sandbox `./check.sh` (the §7.1 ci-gate "fix the
  host, never adapt the loop" policy). Done: #30 (exposure + isolation), #31 (net
  parity), #32/#33 (the swap), then the PID-namespace keystone + carve-out/toggle removal
  (td is the sole sandbox).
- **td-store-db** *(approved 2026-06-14 — "what's next" → "Replace the guix-daemon")* —
  begin replacing the **guix-daemon**, the last big reused Guix component on the build
  side (§2.2/§2.5). td-builder already constructs (#22) / executes (#25) / registers via
  the daemon RPC (#27) / `--check`s its own derivations — build execution is td's. What
  is still ONLY the daemon's is the store-DB **authority**: the `ValidPaths`/`Refs`/
  `DerivationOutputs` rows that make a path valid. Increment 1 (the `store-register`
  rung): `td-builder store-register` WRITES the store SQLite DB ITSELF — a zero-dep
  SQLite FILE-FORMAT writer in Rust (`builder/src/store_db.rs`: the file header, table
  b-tree leaf pages, the record/serial-type varint encoding; unit-tested), the real
  replacement of the daemon's libsqlite (no sqlite3 engine writing it). Increment 2
  registers `hello`'s FULL closure (`guix gc -R`): td writes a store DB — passing
  `PRAGMA integrity_check` — whose registration `sqlite3` reads back byte-identical to
  the daemon's for EVERY closure path (hash + narSize), the full inter-path Refs
  relation, and the artifact's deriver + drv→output. `registrationTime` + the non-
  artifact per-path derivers (the daemon's input-resolution) excluded; verified-red ×2.
  Increment 3 — **td READS its own store** (the "own the store, then diverge" pivot,
  human 2026-06-14): `td-builder store-query` answers store queries (`info` =
  path/hash/narSize; `references` = the Refs relation) by parsing td's own DB with a
  zero-dep SQLite *reader* (`builder/src/store_db_read.rs`) — NO sqlite3 engine and NO
  daemon in td's store-query path. The differential now reads td's DB three ways and
  asserts they agree: TD'S OWN READER == sqlite3 on the same bytes (the parser oracle)
  == the daemon's record (the content oracle). td thus WRITES and READS its store DB
  itself; libsqlite/the daemon are correctness oracles, not the format authority.
  Boundary: the host daemon stays immutable infra (immutable read only); td operates its
  OWN store DB, daemon = oracle (directive 4). With write+read owned, byte-identity to
  the daemon's schema becomes OPTIONAL — the differential stays a correctness check, not
  a compatibility cage. Increment 4 — **td PLACES a path into its own store** (the
  daemon's `addToStore`, write side, flat/text case): `td-builder store-add-text`
  computes the addTextToStore path (`make_text_path`), WRITES the content into a td-owned
  store dir as a canonical `0444` store file, and registers it in a td DB — no daemon in
  the write path. The `store-add` rung's differential uses the daemon's OWN store file as
  oracle (a freshly-added path is in the daemon's WAL, invisible to an immutable
  `db.sqlite` read; the on-disk file is the WAL-free, stronger oracle): td's store path,
  store bytes (by NAR hash), and registration (read back by td's own reader) all match
  the daemon's. Increment 5 — **td computes GC reachability** (the daemon's THIRD role):
  `td-builder store-closure DB ROOT` walks the `Refs` graph from ROOT with td's own reader
  (GC's mark/liveness phase, no daemon); the `store-gc` rung shows td's reachable set from
  hello's output over its OWN scanned Refs equals `guix gc -R` exactly (the destructive
  sweep is not done — boundary-safe). Increment 6 — **recursive addToStore** (the general
  write side): `td-builder store-add-recursive` computes the content-addressed `source`
  path from a tree's recursive NAR sha256, CANONICALLY restores the tree
  (`copy_canonical`: structure + contents + the file exec bit + symlinks — the NAR-relevant
  properties), and registers it; the `store-add-tree` rung shows td's restored tree is
  byte-identical (by NAR hash) to the daemon's own interned `td-builder` source tree and
  td's path matches the daemon's. Increment 7 — **td verifies store integrity**
  (`guix gc --verify --check-contents`): `td-builder store-verify DB STORE-ROOT` re-NAR-hashes
  each registered path and flags any whose content no longer matches its recorded `hash`;
  the `store-verify` rung proves td.db records the daemon's hashes then verifies hello's
  closure in the real `/gnu/store` against them (the daemon differential), and DETECTS a
  one-byte corruption in a td-owned probe. Increment 8 — **the destructive GC sweep** (the
  other half of GC): `td-builder store-gc-sweep STORE-DIR DB ROOT` deletes every registered
  content path not reachable from ROOT from a td-owned store and rewrites the DB to the live
  set; the `store-gc-sweep` rung copies hello's closure into a td store, sweeps with
  ROOT=glibc, and shows the surviving entries + the rewritten DB hold exactly `guix gc -R
  glibc` (the host `/gnu/store` never touched). Increment 9 — **addToStore WITH references**:
  `td-builder store-add-referenced` computes the content-addressed path with the references
  folded into the type (`make_text_path` / makeType), writes the content, and registers the
  path with its `Refs`; the `store-add-referenced` rung shows that for hello's `.drv` and its
  references, td reproduces the daemon's path (drop a ref and it diverges), a byte-identical
  `.drv`, and exactly the daemon's recorded references. Increment 10 (capstone) — **a td
  store backend for a build output**: `td-builder store-add-output` PLACES a built output's
  tree into a td-owned store at its output path and fully registers it (hash + narSize +
  deriver + references + drv→output); the `store-backend` gate shows td's store HOLDS hello's
  output (NAR-identical to the daemon's) and SERVES it — `store-query` (registration +
  references) and `store-verify` (integrity re-hashed against the placed files) all match the
  daemon, with no daemon in any store operation. td now owns the full store backend: write/read
  the DB, add (flat/recursive/referenced), GC (mark + sweep), verify, and back a build output
  end to end — daemon as oracle throughout, never the authority. Next (held by the human until
  the store stack is reconciled): diverge the on-disk format — the differential becomes a
  correctness check on td's chosen format, not a guix-compat constraint.

### 7.2 Landing protocol — merge on green, via PR *(PR gate added 2026-06-11)*

Each agent works in its **own git worktree/branch** — never
directly on a shared checkout of main. Main is branch-protected: no direct
pushes; every landing is a pull request gated on required CI checks and one
human approval (`.github/BRANCH-PROTECTION.md` is the setup/operations note).
To land (**optimistic merge** — main is non-strict since 2026-06-19):

1. validate against your **own base**: run the loop green — `./check.sh`, or
   `td-builder affected-checks --committed-only --run` (which waives the full loop
   or escalates to it per the diff);
2. push the branch and mark its PR ready; CI runs `lint` + `check-fast` (the
   fast tier `./check.sh check-fast` on a hosted runner via the small
   `td-ci-fast` store image — since #26 CI runs the fast tier ONLY; the full
   loop stays the dev-machine gate in step 1 plus the ci-image pipeline's
   `validate` job);
3. on green CI and one human approval, **squash-merge** (the only merge mode
   enabled — merge and rebase merges are off, history stays linear). The squash
   commit's body is composed from the branch's commit messages, not the PR
   description (`squash_merge_commit_message = COMMIT_MESSAGES`), so the durable
   `git log` record is your commit messages — the PR body is review context only.

Main is **non-strict** (`strict_required_status_checks_policy: false`): a PR
merges on its **own** green checks; **main moving under you no longer forces a
rebase-onto-tip + re-run**. Dropping that requirement is the velocity change
(human 2026-06-19) — and it supersedes the old step "if main moved, go to 1".
Rebase only when GitHub reports a real git conflict, or to sequence an exclusive
landing (§7.3) — not merely because main advanced. "Validated" means green
against your base — a ready PR on a locally-red or un-run loop is still a
contract violation (CI verifies the agent's run; it does not replace it).

The price is that `green(A) + green(B) ≠ green(A∪B)`: two independently-green
PRs can combine into a red main. That is **accepted and healed after the fact,
not prevented**, and healing is an **agent duty, not an automated workflow**
(human 2026-06-19): whenever an agent fetches main — to start or to land — it
checks main's latest `check-fast`, and if red runs `ci/revert-suspect.sh
--open-pr` to open a revert PR for the suspect squash commit (main's HEAD).
Squash makes the suspect a single, atomically-revertable commit (the
merge-strategy reason to keep squash); the script's loop guard refuses to revert
a revert. An agent opens the revert PR with its own bot credentials, so it
triggers the required checks and needs no machine PAT or ruleset bypass. The net
is **the fast tier only** — a heavy-only break (boot/VM/marionette/
reproducibility, invisible to `check-fast`) is not caught; it surfaces on the
next manual full `./check.sh` and is fixed forward. Closing that gap by re-running the full loop in CI per-merge is not
feasible (cold hosted runners can't rebuild td's closure; the ci-image is keyed
by channel pin, not main commit), so a **dev-box DAILY full-loop heal is that
heavy net** (human 2026-06-21, formerly "deferred"). The fast check is cheap and
does not meaningfully count toward the §7.3 two-concurrent-checks ceiling; the
full loop that does runs on the dev machine.

**The full `./check.sh` is no longer a per-PR blocking gate for build-engine
changes** *(human 2026-06-21 — "I don't want to block PRs to main on running the
whole suite")*. A `builder/src/*` diff is the spine of every recipe-building gate,
so it used to force the whole corpus locally — the dominant agility cost. It now
validates on the **`check-engine` smoke tier** (`make check-engine`: a TRUE ~2-min
smoke — cheap structural gates + `cargo-test` (compile the engine + its unit tests),
and NOTHING that builds a package from source — `lint` runs in CI), and
`affected-checks` waives the full loop for it. The end-to-end build coverage
(`bootstrap-build`/`build-plan`/`td-check`/corpus/repro) stays in the full `check`,
run by the daily backstop. The full heavy **and** system suite instead runs **once daily** on fresh
main via `ci/daily-full-suite.sh`, driven by a scheduled agent that, on a
regression, **opens a fix-or-revert PR (no auto-merge — a human merges)** and
records the last all-green commit (`.td-last-green`, the seed of a future "stable"
marker). The accepted trade: a corpus/system regression the smoke misses lands and
is healed within a day, not blocked per-PR. The rarer spine files (`channels.scm`,
`check.sh`, `Makefile`, `system/td.scm`, `DIGESTS.md`) still run the full loop
before landing (§7.3); only the frequent engine case is decoupled here.

The human approval (here since 2026-06-11) replaced the original "no human merge
step; review-after on main".

Claims: open a **draft PR** early — that draft, titled for the workstream, IS the
claim, and the open-PR list (`gh pr list`) is the record of who is working on what.
There is no separate claim file and no generated status index; scan the open PRs
before starting so two agents don't pick the same work. Mechanics live in `CLAUDE.md`
"Parallel work".

### 7.3 Exclusive landings

Changes touching the shared spine — `system/td.scm` (the frozen oracle), `check.sh`,
`Makefile`, `channels.scm`, `DIGESTS.md` — collide with every other agent. Land them
as small standalone PRs, announced in the PR description; everyone else rebases.
Oracle re-baselines (which rewrite `DIGESTS.md`) and channel-pin bumps are the
canonical cases. These are coordination rules, not sign-off gates — but remember
§4.3(2): *weakening* anything in the spine still needs the human.

Resource note: each full check already runs its heavy rungs two at a time (`-j2`);
two concurrent full checks therefore mean up to four VMs/builds — observed fine on
this host during the M10.3/loop-latency overlap, but treat that as the ceiling:
don't add a third check or raise `-j`. Stagger landings if loaded.

### 7.4 Files

Work tracking has no dedicated files: claims are the open draft PRs, and work notes +
verified-red evidence live in commit messages + the PR body (the squash merge preserves
the commit messages in `git log`; there is no status-index or claim file). `HISTORY.md`
— completed-milestone record. `DIGESTS.md` — reproducibility record (changes only on
re-baseline, exclusive landing).
