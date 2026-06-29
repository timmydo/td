# host-sandbox-stage0 — retire the spine's last guix-as-packager site

**Handle:** claude-opus-7e12d1 · **Claimed:** 2026-06-21 · base: origin/main @ b0994e7 (#133)

## Goal (North-Star rung 1: no guix process in a build path)

`check.sh:190`:
```
tb=$(guix build -L . -e '(@ (system td-builder) td-builder)')/bin/td-builder
```
is the LAST `guix build -e (@ (system M) PKG)` packager invocation on the loop **spine**
— it runs on the host, before the sandbox exists, to produce the td-builder binary that
BECOMES the host-sandbox container. Everything else (the gate tool-use sites) was routed
onto the cargo-built stage0 by [[guix-builder-route]]; the spine site is outside that
track's scope, so it's still guix.

Swap it for `tools/bootstrap-td-builder.sh` (stage0): cargo compiles `builder/` from
source against the pinned toolchain store paths read from `tests/td-builder-rust.lock`
as plain strings — `env -i`, offline, **no guix/guile on PATH** (the script already
asserts this). Same mechanism the gnu+rust gates use via cache-lib `load_stage0`.

## Why this is honest (own, then diverge)

- stage0 is **behaviorally equal** to the guix-built td-builder (the `bootstrap` gate,
  #93, proves it: created guix-free, runs, bit-reproducible double-build, behaviorally
  equal to yet a distinct binary from the guix-built one).
- So the host-sandbox built BY stage0 must run the whole loop identically — the durable
  proof is simply that `./check.sh` stays green with the spine builder swapped.
- The guix-built td-builder survives ONLY where it is a genuine ORACLE (rust-build
  self-host gate 330, bootstrap gate 170, the td-builder package gate 175) — untouched.

## Scope boundary (what this track does NOT do)

- `check.sh:196` `guix shell … --search-paths` (the sandbox toolchain PATH) — that's the
  toolchain SEED, retired by [[seed-tarball]] (serve it from the frozen tarball). Separate
  site, separate track. NOT touched here.
- `check.sh:75` `guix describe` (pin check) — could be replaced by reading the pin from
  channels.scm directly, but it's a verification call, not a build/packager path. Out of
  scope unless trivial to fold in.

## Open question to resolve first

Are the pinned toolchain store paths in `tests/td-builder-rust.lock` guaranteed present
on the host at prelude time (line 190 runs BEFORE the `guix shell` warm on 196)? Today
`guix build -e (system td-builder)` realizes td-builder's closure (incl. the toolchain)
as a side effect. stage0's bootstrap only READS those paths — if a cold host lacks them
the bootstrap fails with "pinned seed not present". Plan: confirm they're in the warmed
channel closure; if a cold host can miss them, add a minimal warm (realize the lock's
toolchain paths — a fixed-input realize like warm-tsgo, NOT a packager `-e`).

## Plan — RUST IN THE SEED (human decision 2026-06-21)

The blocker below (bootstrap needs rust; fast image is rust-free) is resolved by
putting the rust toolchain IN the frozen seed and building td-builder FROM the seed.
The host-sandbox builder is a Rust binary, so "build the engine from a rust-bearing
seed, guix-free" is BOTH "rust in the seed" AND "line-190 off guix".

### Increment 1 — the "rust in the seed" gate (additive; this PR)

Compose the existing PR3 primitives for the Rust engine instead of hello/C:
- `recipe-td-builder.ts` (#84) builds td-builder from source via `build-recipe`
  (buildSystem rust); `tests/td-builder-rust.lock` is its toolchain+source lock.
- `tools/build-seed-tarball.sh ROOTS…` captures a closure → tarball+manifest.
- `td-builder seed-unpack` restores it into a fresh td store + DB, no daemon.
- `build-recipe` with `TD_SEED_STORE`/`TD_SEED_DB` stages inputs from the unpacked
  seed (bound at canonical /gnu/store INSIDE the sandbox, so rust's hardcoded ELF
  interpreters resolve).

New gate (e.g. `mk/gates/378-rust-seed.mk` + `tests/rust-seed.sh`):
1. [ ] Capture roots = the td-builder-rust.lock toolchain paths (rust/cargo/
       gcc-toolchain/coreutils/bash) + the interned builder source; union closure.
2. [ ] `seed-unpack` into a fresh td store.
3. [ ] `build-recipe` td-builder from the seed as its ONLY store DB (guix off PATH,
       /var/guix + live /gnu/store out of the build path).
4. [ ] Legs: [DURABLE behavioral] the seed-built td-builder RUNS (e.g. `--version` /
       a subcommand); [DURABLE repro] `td-builder check` double-build; [DURABLE
       structural] inputs staged from the unpacked seed (closure binds under
       DEST-STORE, none bare-/gnu/store); [REMOVABLE oracle] same store path as the
       guix-seed build. Driver = stage0 (load_stage0), as PR3 used it.
5. [ ] Verified-red: drop a rust path from the captured seed → build fails (seed not
       self-sufficient; no guix fallback). Record evidence.

### Increment 2 — the spine swap (later PR, builds on inc.1)

`check.sh` prelude warms+unpacks the rust-bearing seed (host PREP, like warm-tsgo)
and provisions stage0 + the sandbox toolchain from it; CI fast image ships the seed
tarball (pinned `tests/td-seed.lock`). Retires check.sh:190 AND :196 together. Open:
host-DIRECT rust execution (cargo before any sandbox) needs the toolchain at canonical
paths or relocatable — resolve as part of inc.2. Exclusive landing on check.sh;
sequence with the seed-tarball agent.

## BLOCKER found in analysis (2026-06-21) — fast-image rust dependency

`tools/bootstrap-td-builder.sh` ALWAYS compiles stage0 from `builder/` source, so it
always needs the rust toolchain (rust-1.93.0 / cargo / gcc-toolchain) present + runnable.
The fast CI image **deliberately omits rust**:
- `ci/lower-fast-drvs.sh` enumerates only the check.sh:196 sandbox toolchain
  (`make bash coreutils …`), the channel instance, the tsgo FOD, and the cheap rungs'
  system/OCI drvs. No `(system td-builder)`, no rust.
- Gate comments are explicit: `325-cargo-test.mk` / `230-rust gates` are "Not FAST_GATES:
  … the rust toolchain, which the small td-ci-fast [image lacks]".

check.sh's whole prelude (incl. line 190) runs for `./check.sh check-fast` too. Today
line 190 is a cached-output LOOKUP (`guix build -e` → existing output, no rebuild → no
rust). Compiling stage0 is NEVER a no-op → ALWAYS needs rust → fails offline in the
rust-free fast image. So the swap is not CI-safe as-is.

Resolutions (none is a "small increment"):
1. Ship rust in the fast image — bloats the deliberately-small tier + image-rot/timeout
   risk ([[td-ci-fast-tier-image]]). Likely undesirable.
2. Inject a PREBUILT stage0 binary into the fast image via the pipeline (build-ci-image.sh
   runs on a rust-capable box); check.sh uses it offline, else compiles. No guix, no rust
   in the image — but spans check.sh + ci/build-ci-image.sh + the enumerator + a gate
   (CI-pipeline change, sensitive). The clean end-state, but a medium PR.
3. Descope line 190; take a cleaner guix-removal target (e.g. check.sh:75 `guix describe`
   → read the pin from channels.scm; or a non-spine site).

Reported to the human for a steer (the resolution is a CI-image-policy design decision).

## Verified-red log

### Increment 1 — rust-seed gate (2026-06-21)

- **Green** (`8c2091f`): `./check.sh rust-seed` EXIT=0. Captured + unpacked the rust
  toolchain seed (54 paths / 2.0G); seed-built td-builder staged every input from the
  unpacked seed (none bare /gnu/store), runs + matches stage0, reproducible (td-builder
  check double-build), and lands at the same path as the guix-seed build.
- **Verified-red** (perturbation: after seed-unpack, `rm -rf` the rust-1.93.0 tree from
  the unpacked seed store): `./check.sh rust-seed` EXIT=2 — the build escaped to the live
  `/gnu/store` for the missing rust, and the STRUCTURAL leg caught it:
  `FAIL: an input staged from the live /gnu/store, not the seed: …-rust-1.93.0`.
  Proves the structural "none-bare-/gnu/store" assertion is load-bearing and non-vacuous
  (an incomplete seed cannot silently pass by using the live store). Reverted; tree clean.
  Note: the rust tree was removed from the store but left in seed.db, so the build still
  ran (rust present at the canonical /gnu/store on the dev host) — the structural leg is
  the durable guard. On a true no-/gnu/store host the build would also fail outright.

Increment 1 sub-tasks 1–5 DONE (capture → unpack → build-from-seed; durable
structural/behavioral/repro + removable oracle; verified-red recorded).

### Rebase onto #135/#136/#137 (2026-06-21)

Main landed the warm-seed infra (#135 `tools/warm-seed.sh` + a pinned seed) and gates
378-td-shell-seed / 382-corpus-seed. Adapted (commit e9c6985): renumbered the gate
378→384 (378 now taken); reworked the capture+unpack onto `tools/warm-seed.sh` (the
#135 content-addressed cache rail — no 2GB re-capture per run); mapped tests/rust-seed.sh
→ the rust-seed gate in affected-checks.sh. Same legs/assertions.

- **Re-green** (warm-seed): `./check.sh rust-seed` EXIT=0 — 54-path rust seed warmed
  into `.td-build-cache/seed/<key>/`, all durable legs + removable oracle pass.
- **Re-verified-red** (warm-seed): removed the rust tree from the warm-seed cache entry →
  `./check.sh rust-seed` EXIT=2, structural leg red: "an input staged from the live
  /gnu/store, not the seed: …-rust-1.93.0". Healed the cache (deleted the entry → re-warms
  cleanly). The structural guard is load-bearing under the warm-seed rail too.

Landing: affected-checks WAIVES the full ./check.sh for this diff (rust-seed.sh mapped;
affected-checks.sh → self-test; no spine files). Selected: plan-index --check, bash -n,
affected-checks --self-test, ./check.sh rust-seed.

## Notes

- Exclusive landing: `check.sh` is the shared spine. Announce + sequence with the
  seed-tarball agent (also edits check.sh for the `:196` toolchain seed).
- Full-loop escalation is mandatory for check.sh changes (affected-checks: loop spine).

---

## Increment 2a — harness-seed gate (claude-opus-5354e1, 2026-06-28)

Re-took the (stale) track after the human's path-B steer: make ci/daily-full-suite.sh
runnable on a cloud VM with NO guix installed. inc2a is the keystone PROOF before any
check.sh spine edit — the loop CONTAINER stands up from a seed alone.

**What landed (this PR):**
- `builder/src/main.rs` host-sandbox: two additive flags — `--store-from DIR` (bind an
  unpacked seed store AT /gnu/store instead of the host store) and `--no-daemon` (drop the
  /var/guix bind). Default binds are byte-identical to before (store_from None → same
  /gnu/store bind; no_daemon false → /var/guix bind). Validated on `check-engine` smoke
  for the builder change; the new gate proves the behavior.
- `mk/gates/385-harness-seed.mk` + `tests/harness-seed.sh`: capture the loop toolchain
  (make/bash/coreutils/sed/grep/findutils/tar/gzip/crun/util-linux/sqlite) into a seed via
  the warm-seed rail, then `td-builder host-sandbox --store-from <seed> --no-daemon` and
  run the toolchain inside. guix is only the one-time capture SOURCE (run on a guix host,
  exactly like rust-seed/warm-seed); the consume half touches no guix.

**Closes rust-seed's gap:** rust-seed ran on a guix host where /gnu/store was present, so
it never proved the store resolves when the host store is ABSENT. harness-seed binds ONLY
the seed at /gnu/store and asserts a host-only path (guix itself) is INVISIBLE inside.

**Verified-red log:**
- **Green** standalone (`sh tests/harness-seed.sh`, host) and NESTED (the gate's
  host-sandbox inside check.sh's outer host-sandbox — the real daily-suite path): the
  46-path seed container runs make/tar/sed/grep/gzip/find; SENTINEL-PRESENT,
  HOSTONLY-ABSENT, GUIX-ABSENT, VARGUIX-ABSENT.
- **VR-A (structural discriminator non-vacuous):** re-ran the probe with the HOST
  /gnu/store + daemon bound (default host-sandbox) → `HOSTONLY-PRESENT` and
  `VARGUIX-PRESENT` (flipped). Proves the seed run's `*-ABSENT` lines actually discriminate
  the seed store from the host store, not vacuously pass.
- **VR-B (behavioral self-sufficiency — no host fallback):** warmed a seed with `tar`
  DROPPED but kept `tar` on PATH, then `--store-from` that reduced seed → inner
  `tar --version` failed (RC=7, `TOOL-FAIL tar`). Proves the seed is the ONLY source — a
  missing tool does NOT silently resolve from the host /gnu/store. This run also CAUGHT a
  false-green in the probe itself (`v=$(cmd | head -1)` takes head's exit, always 0,
  masking a missing tool); fixed to `v=$(cmd) && [ -n "$v" ]` + a `TOOL-FAIL` guard in the
  gate, then re-confirmed green.

**Next (inc2b/2c):** check.sh reads the pin from channels.scm (not `guix describe`) and
provisions td-builder + the toolchain from the seed when guix is absent (guix-OPTIONAL, so
the rust-free fast CI image — which HAS guix — is untouched, dissolving the old inc2
blocker); then a guix-free `check-noguix` tier + daily-full-suite wiring (ship a
pre-captured pinned seed so the VM skips the capture). Exclusive landing on check.sh,
sequenced last.

---

## inc2 fast-iteration via td-subst (plan, 2026-06-28) — "loop substitutes too" for x86_64

The userland gate (and rust-x86_64) rebuild the ~18-rung /td/store toolchain from seed every
run (~90 min). The td-subst toolchain mechanism already exists and is LOCK-PARAMETERIZED, so
it works for the x86_64 toolchain — only the x86_64 publishing + consumer wiring is missing.

Mechanism (traced): build td-subst → `td-builder store-add-input-addressed NAME KEY SRC STORE DB`
(KEY = `toolchain-key LOCK`) interns at the lock-keyed /td/store path → `tools/publish-toolchain-subst.sh
LOCK NAME DB STORE OUT_STORE` exports+signs into the persistent store → `tools/resolve-toolchain.sh
LOCK NAME DEST` fetches (sig+StorePath+NarHash) or exits 1 to fall back to from-seed. The
daily suite signs `.td-build-cache/toolchain-subst-export` + copies to `$TD_SUBST_STORE`
(~/.td/subst). Pinned pub key = tests/td-subst.pub. Reference impls: gate 412
(tests/bootstrap-glibc-241-store-native.sh ~L1046-1063, the i686 producer) and
tests/toolchain-subst-default.sh (the consumer + publisher round-trip).

Build-out (each piece = a 90-min from-seed validation):
1. PRODUCER (x86_64): after `run_x86_64_cross` (XGLIBC/XGCC2/XBU/XLIBGCCDIR), for EACH of
   glibc-2.41-x86_64, gcc-14.3.0-x86_64, binutils-2.44-x86_64: `IAKEY=toolchain-key
   tests/td-toolchain-x86_64.lock`; `store-add-input-addressed <name> $IAKEY <component-tree>
   $store $db`; `subst-export` → `.td-build-cache/toolchain-subst-export`. NOTE: the stage0
   store db is single-entry per store-add (gate 412 comment) → export each component
   IMMEDIATELY after interning it, before the next. Components need NOT be byte-reproducible
   (trust = signature + input-addressed name, not repro — that's task 3). First confirm the
   exact install-tree roots XGLIBC/XGCC2/XBU/XLIBGCCDIR point at (x86_64-cross-fns.sh) so the
   interned tree == what `toolchain-path` names + what build_busybox/make consume.
2. CONSUMER: a shared helper (or inline) — before the from-seed build, build td-subst (as
   toolchain-subst-default does) and `resolve-toolchain.sh tests/td-toolchain-x86_64.lock
   <component> <dest>` for the 3 components; ALL HIT → set XGLIBC/XGCC2/XBU/XLIBGCCDIR to the
   fetched trees + SKIP the from-seed cross-up; ANY MISS → from-seed (current code). Wire into
   tests/userland-x86_64-store-native.sh AND tests/rust-x86_64-runtime-store-native.sh.
3. DAILY-SUITE: the publish step already signs+copies `.td-build-cache/toolchain-subst-export`;
   it just needs the x86_64 export present (from step 1 running in the heavy suite). Confirm it
   publishes ALL narinfos in the dir (i686 + x86_64), not one.
4. ONE-TIME populate (for local iteration): run the from-seed build once with a local
   TD_SUBST_PRIVKEY + TD_SUBST_STORE to publish the x86_64 toolchain; thereafter the userland
   gate FETCHES it (~minutes/iteration).

Directive-1: the AUTHORITATIVE from-seed build stays (daily suite = sole from-seed build +
publisher); the resolver is the per-PR/local optimization, fall-back-on-miss (never silent).

### SUPERSEDED by PR #223 (2026-06-28) — defer the toolchain-subst producer/publish

PR #223 (worktree-x64-subst-fetch, x64-toolchain-subst track, plan/x64-toolchain-subst.md)
is building the x86_64 toolchain fetch path: gate 414 interns the real x86_64 glibc at its
lock-keyed path, builds td-subst, publishes + fetches + runs it via resolve-toolchain.sh.
The per-PR BUILD-SKIP (the speedup) is its PR3b follow-up — and #223 found my step-1/2 plan
above is INCOMPLETE: the skip needs the WHOLE-toolchain CLOSURE fetch (gcc + binutils + the
i686 runtime, since the cross gcc is i686 and needs the i686 glibc to compile), not just the
3 x86_64 components, plus daily publishing + check.sh exposing the persistent store.

So this track does NOT build the producer/publish (avoid duplicating/colliding with #223 —
it owns gate 414 / x86_64-cross-fns.sh / bootstrap-x86_64-toolchain-store-native.sh). Instead:
- KEEP the userland layer (gate 420, busybox+make captured set) — the new thing #223 doesn't do.
- It builds the toolchain from seed for now; once #223 + PR3b land the closure-fetch, retarget
  the userland gate's toolchain provisioning to resolve-toolchain.sh (fetch, fall back to seed)
  — a small change then, not the producer build-out planned above.

### GREEN — gate 420 userland-x86_64-store-native (2026-06-28, after #223 merged)

The userland captured set is DONE. After #223 merged, the gate FETCHES the x86_64 toolchain
closure {binutils-2.44, gcc-14.3.0, glibc-2.41} via `x86_64_resolve_closure` (#223's subst
machinery, signed + lock-keyed) and falls back to a from-seed build + `x86_64_build_closure`
(directive 1). On the fetch path each iteration is ~15 min, not ~90 — that unlocked debugging
the busybox/make build inside the loop sandbox. busybox 1.37.0 + GNU make 4.4.1 build from
upstream source (td-fetch, sha-pinned) with that toolchain, DYNAMIC vs /td/store glibc 2.41,
intern at /td/store, and RUN in the store-ns own-root → GNU Make 4.4.1, /gnu/store ABSENT, zero
guix bytes. Verified-red: interp → bogus /td/store path ⇒ behavioral leg reds ("store-ns:
spawning busybox: No such file or directory") — the interp relink is load-bearing.

Sandbox-build lessons (busybox surfaces these as loud failures — the point of choosing it):
- no /bin/sh: configure/Kbuild #!/bin/sh shebangs sed'd to the curated shell; AND glibc's
  popen()/system() hardcode /bin/sh (busybox split-include) — a COMPILED libc call, no shebang
  to sed → symlink /bin/sh in the dev-shell's writable ephemeral tmpfs root (namespace-local).
- build-vs-run interp/rpath: bake the ABSOLUTE build-dir glibc rpath so test binaries run at
  build with NO LD_LIBRARY_PATH (a global LD_LIBRARY_PATH=<x86_64 glibc> poisons the host gawk →
  SIGFPE), then relink interp→/td/store/ld AND rpath→$ORIGIN/../lib at assemble (elf-set-rpath).
- glibc component lacks the Linux UAPI headers (they live in the build sysroot) → -idirafter the
  warm KH_X86_64_TB so glibc's bits/local_lim.h finds <linux/limits.h>.
- scaffolding (no output bytes): awk/m4/bison/... via cross-fns _xbin (sorted/deterministic —
  an ad-hoc gawk pick SIGFPE'd); find/xargs/bzip2 + plain-name binutils (ar/nm/...) for Kbuild;
  CPP=<cross gcc -E> (no /lib/cpp); pin make's autotools mtimes (no automake re-run).

NEXT (inc2c): wire gate 420 into a guix-free check tier + daily-full-suite; the toolchain
fetch already works headless on a populated ~/.td/subst (the daily publishes it).
