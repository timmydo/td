# bootstrap-td-builder — build the seed with td, no guix in CREATION (move-off-Guile §5)

**The real goal (human steer, 2026-06-18):** "get rid of guile and guix" means removing
the `guix`/`guile` INVOCATIONS from the loop, not relocating them (see
[[td-move-off-guile-remove-invocations]]). A full `./check.sh` fires ~178 guix calls;
the seed tools are the root: td-builder, td-ts-eval, node, td-typescript are all
`guix build`-produced from Guile package definitions.

The human chose the foundational path: **build the seed with td** — a stage0 td-builder
that bootstraps the self-hosting chain, so guix never has to PRODUCE the first builder.

## The bootstrap circularity

`rust-build` "self-hosts" td-builder via `build-recipe` — but only because a guix-built
td-builder already exists to RUN build-recipe. The first td-builder comes from
`guix build -e '(@ (system td-builder) td-builder)'`. To remove that, we need a stage0
td-builder produced WITHOUT guix.

Enablers found:
- td-builder is **std-only** (builder/Cargo.lock = 1 package), so a guix-free compile
  needs only `rustc`/`cargo` + a gcc linker — no crate vendoring.
- those toolchain paths are already pinned in `tests/td-builder-rust.lock` (the
  guix-built toolchain seed, retired LAST §5).

## Brick 1 (this track): stage0 + proof gate

- `tools/bootstrap-td-builder.sh OUTDIR` — `cargo build --release --offline --frozen`
  under `env -i` with ONLY pinned store tools on PATH (cargo/rust/gcc/coreutils/bash,
  read from the lock as strings; guard: no guix/guile on PATH). No guix, no Guile, no
  daemon. Validated: builds in ~8s, runs (`td-builder 0.1.0 ok`), bit-reproducible.
- `mk/gates/170-bootstrap.mk` — [STRUCTURAL] guix/Guile-free build; [DURABLE behavioral]
  runs sentinel + nar-hash; [DURABLE self-discrimination] perturbed input → different
  hash; [DURABLE intrinsic-reproducibility] two bootstraps bit-identical; [MIGRATION
  ORACLE, removable] behaviorally == guix-built td-builder, distinct binary (stage0
  sha256 b564773…, guix-tb 5fdc614… — same nar-hash e97a8c… on a probe).

## The obstacle for the NEXT brick

`build-recipe` references td-builder (the builder PROCESS) by **store path**
(`current_exe()` → strip `/bin/td-builder`; the assembled drv's `builder` field), and
the sandbox bind-mounts the builder from its real `/gnu/store` path. So making the
loop's builds USE stage0 needs stage0 IN a store the sandbox can bind from — i.e.,
daemon-free placement of the builder (or teaching the sandbox to bind a td-owned
builder path). Two routes for brick 2:
  (a) td-owned store placement of the builder (pure; the "td owns the store" direction),
  (b) daemon places the cargo-built stage0 (creation guix-free, placement via the
      retire-last daemon — transitional).
Then brick 3: demote the guix-built td-builder to oracle-only across the gates.

## Hermeticity — is `env -i` + `$PATH` enough? (No, and here's why it holds)

`env -i` + a pinned PATH is NOT the guarantee on its own: it scrubs the environment and
forces the PINNED rustc/cargo (no ambient toolchain; no host RUSTFLAGS/CC/CARGO_HOME;
HOME/CARGO_HOME → tmp, so no `~/.cargo/config`), but it does not isolate the filesystem
or the network. The isolation comes from WHERE the gate runs:

- **Inside td's loop sandbox** (`td-builder host-sandbox`): a fresh-tmpfs root exposing
  ONLY `/gnu/store` (ro) + the worktree (+ synthetic /dev, fresh /proc) — there is NO
  host `/usr`/`/lib`/`/bin`/`/etc` to leak from — and its OWN loopback-only netns, so
  the build is offline by construction (`--offline --frozen` is belt-and-suspenders).
- **[DURABLE hygiene] leg** proves the PRODUCT is clean: the stage0 binary's ELF
  interpreter and RUNPATH are entirely under `/gnu/store` (store glibc/gcc-lib), never
  host `/usr`/`/lib`. Verified empirically: interp = `…glibc-2.41/…/ld-linux`, RUNPATH =
  store gcc-lib + glibc only. Verified-red: flipping the expected prefix reds the gate
  (exit 2) — the check would catch a host-libc leak.

**Known residual gap (honest):** the loop sandbox exposes the WHOLE store, not just the
declared toolchain closure, so closure-COMPLETENESS is not STRUCTURALLY enforced the way
td's per-build sandbox (build-recipe stages only the closure) enforces it. Bounded today
by: offline netns + pinned PATH + bit-reproducibility + the hygiene leg + behavioral
equivalence to the hermetically-guix-built builder. The structural fix is to run the
stage0 compile inside a STAGED-closure sandbox (only the declared inputs visible, a
missing dep ⇒ build fails) — the convergence point with brick 2 (stage0 produced by a
sandboxed td build).

## Status / evidence

- Manual validation: guix-free compile ✓ (8s), runs ✓, bit-reproducible across 2 builds
  ✓ (sha256 b564773…), behaviorally == guix-tb (same probe nar-hash e97a8c…) ✓, distinct
  binary ✓ (guix-tb 5fdc614…). Pure: `env -i` + only pinned tools (no host /usr/bin).
- `./check.sh bootstrap`: GREEN — all five legs pass in the loop sandbox.
- Verified-red: a non-reproducible bootstrap (append `$RANDOM` to the output) reds the
  intrinsic-reproducibility leg ("two stage0 builds differ … NOT reproducible", exit 2);
  reverted.
- Full `./check.sh`: GREEN (see commit).

## Brick 2 (claude-fable-300f35): the loop BUILDS with stage0 as the in-store builder

**Goal / acceptance.** A real package (`hello`) builds in the loop where the
**builder-of-record is the td-bootstrapped stage0**, not the `guix build`-produced
td-builder — guix NEVER produced the binary that ran the build. Today `build_recipe`
sets `builder = self_store_path()/bin/td-builder` from `current_exe()` (the running,
guix-built binary), so every td build is run by a guix-produced builder. This brick
lets td place stage0 into its OWN store and assemble+realize a recipe whose `builder`
is that td path.

### The obstacle, restated

`build_recipe` references the builder by store path (the assembled drv's `builder`
field + an `input-src`), and `sandbox::build` bind-mounts every closure item from its
canonical `/gnu/store/<base>`. stage0 lives in a scratch dir, not `/gnu/store`, and is
byte-distinct from guix-tb (so it has NO `/gnu/store` path the daemon ever created).
`store-add-recursive` (gate 285) places a tree content-addressed into a td store dir —
but its no-ref guard REJECTS a tree with external references, and stage0 references
glibc + gcc-lib (its ELF interp + RUNPATH, brick-1 hygiene leg). So the primitive must
*scan and record* those refs.

### Design — a `BuilderOverride`, the exact mirror of `SrcOverride` (#97)

1. **`td-builder store-add-builder NAME TREE STORE-DIR OUT-DB SEED-DB`** (new arm,
   builder/src/main.rs). Like `store-add-recursive` but for a tree WITH references:
   - content-addressed path `C_b = make_store_path("source", recursiveNAR(TREE), NAME)`;
   - canonically restore TREE → `STORE-DIR/<base>` (`copy_canonical`);
   - scan TREE's NAR for references against the **seed closure** (candidates = the
     ValidPaths of SEED-DB, i.e. `/var/guix/db/db.sqlite`) → stage0's refs
     (glibc, gcc-lib + whatever it links); register `C_b` + those refs in OUT-DB
     (reusing the ValidPaths/Refs writer, refs as external rows);
   - print `C_b`. (No daemon, no guix — the seed db is read with td's own reader.)

2. **`BuilderOverride { canonical, on_disk, db }`** threaded through `build_recipe` →
   `realize_drv`, mirroring `SrcOverride`. When set:
   - `build_recipe`: `builder_store = canonical`; `builder = {canonical}/bin/td-builder`;
     the `input-src {builder_store}` line uses it (instead of `self_store_path()`).
   - `realize_drv`: for the builder root `C_b`, read its closure from a **db SET** —
     `C_b` + its direct refs from OUT-DB (td's builder db), and each ref's TRANSITIVE
     closure from the seed db (glibc/gcc-lib live there). The `C_b` entry is encoded
     `C_b\t{on_disk}` so the sandbox binds stage0 from the td store dir; every ref is a
     bare `/gnu/store` path (daemon-resident → bound from there). No multi-db helper
     needed beyond "this root's refs come from OUT-DB, their closures from the seed db"
     — a small targeted spanning, not build-plan's general `closure_multi`.
   - CLI surface: extend `build-recipe` with an OPTIONAL trailing pair after the
     (optional) `SRC-STORE-DIR SRC-DB` — keep arity backward-compatible (every existing
     gate's invocation unchanged). Likely cleaner as a `TD_BUILDER_STORE` / `TD_BUILDER_DB`
     env pair (cf. `TD_STORE`), decided when wiring the gate.

3. **Gate `mk/gates/175-bootstrap-build.mk`** (after `170-bootstrap`), subject `hello`:
   - PREP: `guix build` realizes hello's SEED (toolchain + source from the lock) — the
     retired-last seed, as the other corpus gates do.
   - bootstrap stage0 (tools/bootstrap-td-builder.sh) with guix/Guile OFF PATH; place it
     via `store-add-builder` into a td store dir + builder.db.
   - `build-recipe` hello with the builder override + guix/Guile scrubbed from PATH.
   - [STRUCTURAL] hello's assembled `.drv` `builder` field is the **stage0 td path
     `C_b`**, NOT any `…-td-builder-0.1.0` guix output path; the build ran guix-free.
   - [DURABLE behavioral] the loop RUNS hello from td's own output → `Hello, world!`.
   - [DURABLE intrinsic-reproducibility] `td-builder check` double-build of the
     stage0-built hello agrees (no guix --check).
   - [DURABLE self-discrimination] drop the builder override → build falls back to the
     guix-tb builder and the drv's `builder` is guix's td-builder path (verified-red:
     the structural assert flips).
   - [MIGRATION ORACLE, removable] stage0-built hello is behaviorally == guix's hello
     (same program output) at a DISTINCT output path (own, then diverge — different
     builder path ⇒ different drv ⇒ different out path, identical behavior).

### Sub-task ladder

1. [x] Rust: `store-add-builder NAME TREE STORE-DIR OUT-DB SEED-DB` arm — content-addressed
       tree placement (store-add-recursive) + scanned external refs vs the seed db's
       ValidPaths (store-add-referenced's ref shape). Follows the inline-arm convention of
       the other `store-add-*` ops (tested via the gate, not a unit test).
2. [x] Rust: `BuilderOverride` struct + `realize_drv` builder branch — closure = builder
       db (builder + direct refs) ∪ seed db (transitive), the builder entry encoded
       canonical\ton-disk.
3. [x] Rust: `build_recipe` honours the override (builder field + input-src + closure
       root); opt-in via TD_BUILDER_PATH/STORE/DB env, backward-compatible.
4. [ ] `cargo test` green (the affected-checks fast leg).
5. [x] Gate `mk/gates/365-bootstrap-build.mk` (175 was taken by td-builder);
       `./check.sh bootstrap-build` GREEN — all four legs pass.
6. [x] Verified-red ×2. Record below.
7. [ ] Full landing check (`tools/affected-checks.sh --committed-only --run`); PR ready.

### Status / evidence (brick 2)

- `./check.sh bootstrap-build`: GREEN. Stage0 placed at a td content-addressed path
  (`…-td-builder-0.1.0`) DISTINCT from the guix-built td-builder; hello built with that
  stage0 as `builder`, greets, reproducible (td check double-build), distinct from guix's
  hello (same greeting). The builder is staged from td's own store (canonical\ton-disk).
- **Honesty finding (seed-db span):** the first planned Red #2 — "limit realize's closure
  to the builder db only ⇒ build can't see glibc ⇒ fails" — did NOT go red: the build
  stayed GREEN. hello's OWN toolchain inputs (its lock) already supply glibc/gcc-lib, so
  the builder's ref-span is redundant-but-correct (defense-in-depth) for this subject,
  NOT independently load-bearing. So it is NOT claimed as a verified-red leg; the span is
  kept because a builder's closure should be self-contained regardless of recipe inputs.
- **Verified-red #1 (override is load-bearing):** dropped TD_BUILDER_* from the gate's
  build-recipe call → build-recipe fell back to the guix-built td-builder → `FAIL: hello's
  .drv does not name the stage0 builder …` (exit 2). The STRUCTURAL leg flips. Reverted.
- **Verified-red #2 (td-store staging is load-bearing):** corrupted the builder's on-disk
  staging path (`canonical\t/nonexistent-td-builderstore/…`) → `closure item … (on disk
  /nonexistent-td-builderstore/…): No such file or directory` (exit 2) — proving stage0 is
  genuinely fed into the build FROM td's own store, not /gnu/store. Reverted (builder/src
  perturbation busts the corpus drv-cache while applied; reverted before the landing
  check, all green edits committed first — [[td-full-check-oom-and-exit-masking]],
  [[td-commit-before-red-variants]]).

### Next bricks (after this)

- Brick 3 (in progress): demote the guix-built td-builder to ORACLE-ONLY across the
  package gates (below).
- Reconstruct the toolchain seed itself (retired LAST, §5).

## Brick 3 (claude-fable-300f35): the package gates BUILD with stage0, not guix-tb

**Goal.** The actual package BUILDS — the `build-recipes` parallel phase + the
corpus/toolchain/corpus-deps/rust gates — run on the td-bootstrapped stage0, not the
`guix build -e '(@ (system td-builder) td-builder)'` binary. That removes the
seed-tool guix invocation (category 1, [[td-move-off-guile-remove-invocations]]) from
the hot package-manager gates. The Brick-2 override is proven (gate 365); this brick
generalises it through the shared build path.

**Where the builds actually happen.** `build-recipes` (Makefile) resolves
`tb=guix build -e (system td-builder)`, exports `TB`, and fans out `tests/build-pkg.sh`
per spec; build-pkg.sh → `cache-lib.sh` `cached_build`/`cached_check` run `$TB
build-recipe`/`$TB check`. The gates re-source cache-lib and cache-HIT. So guix-tb is
resolved in TWO places (build-recipes + each gate) and is the binary that does all the
building.

**Design (centralise in cache-lib + a helper; one place sets stage0).**
- `tests/stage0-builder.sh STORE-DIR DB` — bootstrap stage0 (tools/bootstrap-td-builder.sh,
  guix-free) then **stage0 runs its own `store-add-builder`** to place itself into
  STORE-DIR+DB (no guix-tb in the loop). Prints `Cb`. Idempotent: a sentinel
  (`STORE-DIR/../path`) lets a second call reuse the placement.
- `cache-lib.sh load_stage0` — read the placement under `.td-build-cache/stage0`
  (bootstrap+place if absent), set `TB=<stage0 bin>` and `TD_BUILDER_PATH/STORE/DB`.
  `cached_build` passes those three through its `env -i` (so the drv's builder is
  stage0) and ASSERTS the assembled drv's builder == the stage0 `Cb` (a NEW durable
  structural leg across every package gate — verified-red: if the override doesn't
  take, the builder is guix-tb and the assert fires). `cached_check` runs `$TB check`
  (stage0).
- `Makefile` `build-recipes` — bootstrap+place stage0 ONCE before the xargs fan-out
  (single writer, no race) and export the env; also realize the td-builder toolchain
  seed. **Exclusive landing (shared spine).** This is the one piece the Brick-3 option
  did not flag: the real builds live in the Makefile phase, so cutting the invocation
  needs this edit.
- gates (corpus-no-guix, toolchain-no-guix, corpus-deps-no-guix, rust-build/-vendor/
  -uutils/-russh) — drop their `tb=guix build -e (system td-builder)` + `TB=` lines;
  cache-lib's `load_stage0` provides TB now (reads the placement build-recipes wrote).
- guix-tb retained ONLY as oracle where a gate explicitly diffs against it (gate 175
  td-builder, gate 365 bootstrap-build) — unchanged.

**Acceptance.** Every package gate green with stage0 as the builder-of-record
(structural leg in cache-lib); the corpus/toolchain artifacts run + are reproducible
(unchanged durable legs); the census is unchanged (it lowers, doesn't build).
Verified-red: force `load_stage0` to no-op (no override) → the cache-lib structural
leg fires (builder is guix-tb). The `guix build -e (system td-builder)` invocation is
gone from build-recipes + the package gates (a per-loop guix-call-count drop).

### Sub-task ladder

1. [ ] `tests/stage0-builder.sh` (bootstrap + stage0-self store-add-builder; idempotent).
2. [ ] `cache-lib.sh`: `load_stage0`; `cached_build` passes TD_BUILDER_* + asserts the
       drv builder is stage0; `cached_check` uses stage0.
3. [ ] `Makefile` build-recipes: place stage0 once, export env, realize the tb seed
       (exclusive landing — announce).
4. [ ] Drop `tb=guix build (system td-builder)` from the package gates.
5. [ ] `./check.sh corpus-no-guix` green (exercises build-recipes → stage0), then the
       rest; verified-red on the cache-lib structural leg.
6. [ ] Full landing check; PR (note the Makefile exclusive landing).

### Status / evidence (brick 3)

- (in progress)
