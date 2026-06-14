# PLAN.md — track status index

Scope contract: the approved roadmap, `DESIGN.md` §7.1. This file is ONLY the status
index — one line per track, kept tiny so rebases stay trivial. Per-track working
state: `plan/<track>.md` (single writer — the claiming agent). Completed milestones:
`HISTORY.md`. Reproducibility digests: `DIGESTS.md`. Parallel-work rules: `CLAUDE.md`
"Parallel work" / DESIGN §7.2–7.4.

Claim a track by putting your handle + date on its line as the FIRST commit of your
track branch, published by opening a draft PR (main is branch-protected — no direct
pushes; DESIGN §7.2). Handles are session-unique — generation mechanics in `CLAUDE.md`
"Parallel work". One agent per track; release the claim when you land or stop (close
the PR if abandoning). Claim status = this file on main **plus the open PRs' claim
edits** (track files don't carry it).

## Mainline (serial — one agent drives it at a time)

- [x] **M10.3 manual rollback + declared persistence** — DONE 2026-06-10 (claude-fable); review round DONE 2026-06-10 (claude-fable-9cb426) — `plan/m10.md`
- [x] **M11 verified generations** — DONE 2026-06-11 (claude-fable-7d8371; rollback rung grown to 36 asserts across three boots — sealed tmpfs-root + dm-verity store, corrupted root fails closed) — `plan/m11.md`
- [x] **M12 signed distribution** — DONE 2026-06-12 (claude-fable-c4148a: §7.1 acceptance green — `registry` rung pushes both gen images into a signed static OCI-layout registry (signify over manifest digests, pull-by-digest from the bytes alone) and `verify-place` proves the placer's verified mode places only what verifies, rejecting unsigned/bad-signature/digest-mismatch + a self-stated digest; direct-vs-verified placement differential; placed image-digest= anchor from S1; verified-red ×13 across S1-S4; DIGESTS §2.7 re-baselined) — `plan/m12.md`

## Side-tracks (parallel-safe)

- [x] **rootless-builder** — DONE claude-fable-ca67ec 2026-06-11 (new `rootless` rung: unprivileged userns daemon rebuilds the qcow2 image, NAR-hash-equal to the root daemon's oracle; verified-red A/C) — `plan/rootless-builder.md`
- [x] **offline-isolation** — CLOSED 2026-06-11 claude-fable-cebe98 (undeclared-fetch-fails `offline` rung landed; daemon-side isolation rescoped to the own-builder era, human sign-off — see DESIGN §7.1) — `plan/offline-isolation.md`
- [x] **oci-load** — DONE claude-fable-a03d13 2026-06-11 (new `oci-load` rung: skopeo foreign-loads the plain + gen-1 images into canonical OCI layouts, rejects a corrupted layer; §2.7 manifest-digest identity recorded in DIGESTS.md; verified-red ×4) — `plan/oci-load.md`
- [x] **loop-latency** — DONE claude-fable 2026-06-10 (full check 525s→275s; new `reset` rung) — `plan/loop-latency.md`
- [x] **fhs-app-images** — DONE claude-fable-aed5c2 2026-06-13 (§7.1 acceptance green: the `container` rung gained an FHS-layout app image — hello packed with `#:symlinks '(("/usr/bin/hello" -> "bin/hello"))`, re-packed deterministically, joined to the rung's `--check` set — and a behavioral assertion that crun execs the explicit `/usr/bin/hello` on the booted base (resolves via the in-image symlink, prints output) while the SAME arg fails on the plain store-layout rootfs; verified-red ×2; reuses the container rung's single boot, no new heavy rung) — `plan/fhs-app-images.md`
- [x] **td-builder** — DONE 2026-06-11 (S4 claude-fable-8ebb4e: the §7.1 acceptance differential — td-builder rebuilds the system qcow2 image drv daemon-equal on path/NAR-hash/size/83-refs/deriver, GREEN at S3 sandbox parity, no chroot growth needed; verified-red ×3 at distinct S4 asserts; S1-S3 claude-fable-49b6d6/a03d13/696a4e) — `plan/td-builder.md`
- [x] **ci-gate** — DONE claude-fable-52ceb1 2026-06-12 (hosted-runner gate landed: unmodified ./check.sh fed by the CI store image; 8 live-run iterations fixed build users, host-guix shim, sandbox-tmpfs scratch, du sizing — and exposed the upstream docker readdir-order defect, excluded by sign-off; `check` becomes required when ci-image-pipeline publishes the image and inherits verified-red + --require-runner-check) — `plan/ci-gate.md`
- [x] **check-memo** — DONE claude-fable-580472 2026-06-12 (verdict memoization live on all 11 reproducibility `--check` legs/19 drvs; unchanged-tree floor 440s→145s; permanent `memo` rung asserts the discipline every loop; verified-reds on record; offline/rootless stay direct per the constraint-6 boundary) — `plan/check-memo.md`
- [x] **ci-image-pipeline** — DONE claude-fable-52ceb1 2026-06-13 (workflow builds the CI store image, pushes a candidate via GITHUB_TOKEN to the repo namespace, validates it with the unmodified offline ./check.sh on a fresh runner, retags :<pin>+:latest on main events only; green end to end on PR #14 run 27467579944 — build-image + validate PASS, promote skipped on the PR; 9 live-run iterations fixed build users, host-guix shim, signing key, tmpfs scratch, du sizing, and excluded the import-incompatible outputs — docker-pack fs-order families (sign-off), the rootless probe, and the deriver oracles — so the runner rebuilds them fresh; post-merge human steps: make the ghcr package public on first promote, then --require-runner-check) — design notes in `plan/ci-gate.md`
- [x] **ts-frontend** — DONE claude-fable-3ca5dd 2026-06-13 (Phase 1 of §5 move-off-Guile: TypeScript spec surface lowering to the frozen oracle's drvs; charter landed #15. §7.1 ACCEPTANCE MET (3 rungs): `ts` (pinned tsc type-checks + emits v0 spec, vr×3), `ts-eval` (pure-Rust boa evaluator — vendored offline/hash-pinned, --check reproducible, curated global removes Date/denies Math.random + 5 hermetic I/O-rejection probes, vr×3), `ts-diff` (TS v0 spec → tsc→boa→config→td-config lowers store-path-equal to system/td.scm; perturbation diverges, vr×2). Decisions (human 2026-06-13): boa vendored as a pinned input; tsc does the transpile (swc CLI is a stub). pkg/storeRef deferred — not needed for the scalar v0 system) — `plan/ts-frontend.md`
- [ ] **corpus-independence** — claimed claude-fable-4a2e33 2026-06-13 (Phase 2 of §5
  move-off-Guile, graduated from §6 to §7.1 — human go-ahead 2026-06-13. CORPUS axis:
  td's OWN recipes vs the Guix corpus, Guix as oracle, toolchain/build-system retired
  last — composed with the SURFACE axis so recipes are AUTHORED in TypeScript. POC:
  `tests/ts/recipe-hello.ts` declares GNU hello from upstream coordinates; the boa
  evaluator (new `recipe`/`fetchSource` capture globals) emits it as JSON, lowered by a
  generic Guile bridge `system/td-recipe.scm` (no `(gnu packages …)`); the single
  TS-driven `corpus` rung proves it lowers store-path-equal to the corpus `hello`, a
  perturbed `.ts` diverges, and the built artifact is `--check`-reproducible +
  NAR-hash-equal to the oracle. OWN-BUILDER increment (human direction 2026-06-13):
  `system/td-build.scm` + the td-builder crate's `autotools-build` mode build the
  SAME TS recipe with a td/Rust builder instead of gnu-build-system — the `td-build`
  rung proves it structurally (builder=`td-builder`, not `guile`), reproducibly
  (`--check`), and behaviorally (byte-identical to the corpus hello) at a distinct
  path. PACKAGES-WITH-INPUTS follow-on claimed claude-fable-44df36 2026-06-14
  (the named "broaden the recipe set" step): new `corpus-deps` rung — a recipe
  WITH build inputs (`tests/ts/recipe-nano.ts`: nano declaring gettext-minimal +
  ncurses) lowers store-path-equal to the corpus oracle, inputs resolved by the
  bridge from the corpus (input resolution stays Guix's, retired last); inputs are
  load-bearing (stripping them diverges) and are direct derivation-inputs; build +
  `--check` NAR-hash-equal. Touches the Makefile/td-recipe.scm — small exclusive
  landing, additive) — `plan/corpus-independence.md`
- [ ] **evaluator-as-library** — claimed claude-fable-4a2e33 2026-06-13 (graduated §6→
  §7.1, human go-ahead 2026-06-13. Remove Guile from `.drv` CONSTRUCTION: td-builder
  (Rust) emits a `.drv` byte-identical — store path AND bytes — to guix's `derivation`
  for the `td-build` hello spec; guix is the oracle; reuses the crate's ATerm parser +
  SHA-256, adds the serializer + `nix-base32`/`make-store-path` + `hashDerivationModulo`.
  Input resolution stays Guix's, toolchain retired last. DONE 2026-06-13: the
  `drv-emit` rung — td constructs the td-build hello `.drv` byte-identical to guix's
  (validated over hundreds of real drvs), perturbed recipe is a distinct drv it also
  matches, verified-red ×2) — `plan/evaluator-as-library.md`
- [ ] **td-drv-build** — claimed claude-fable-4a2e33 2026-06-13 (graduated §6→§7.1,
  human go-ahead 2026-06-13. Capstone of the move-off-Guile arc: for the `td-build`
  hello subject, td-builder EMITS the `.drv` (#22) AND EXECUTES it in its own userns
  sandbox (S3/S4), output NAR-equal to the daemon — construct + execute both td's Rust,
  builder=`td-builder autotools-build`, NO guile in either; daemon is the oracle only.
  Input resolution + closure + the daemon building the INPUTS stay Guix's, toolchain
  retired last) — `plan/td-drv-build.md`
- [ ] **td-drv-add** — claimed claude-fable-4a2e33 2026-06-13 (wire td's `.drv` into the
  loop: td-builder constructs the `.drv` (#22) and REGISTERS it via the daemon's
  `addTextToStore` RPC — a Rust worker-protocol client (`builder/src/daemon.rs`) — so it
  enters the store with no guile `(derivation …)`. Rung: `drv-add` (daemon returns td's
  computed path), `store-add` (novel-write proof), `guix build` the registered `.drv`
  daemon-equal. Daemon stays the backend) — `plan/td-drv-add.md`
- [ ] **td-drv-assemble** — claimed claude-fable-4a2e33 2026-06-13 (remove the LAST
  guile `(derivation …)`: guile resolves inputs + emits a raw SPEC
  (`write-td-build-spec`, no `(derivation …)`); td-builder `drv-assemble` does the
  assembly+ordering in Rust (sort env/inputs, add `out`, compute output path, register
  via the daemon) byte-identical to guix's `(derivation …)`. Input resolution stays
  Guix's, toolchain retired last) — `plan/td-drv-assemble.md`
- [ ] **td-check** — claimed claude-fable-4a2e33 2026-06-13 (gate-2: td OWNS the
  reproducibility oracle. `td-builder check DRV CLOSURE SCRATCH` executes the `.drv`
  TWICE in two independent userns sandbox runs and compares the per-output NAR hashes
  — td's own `--check` verdict, no daemon, no `guix build --check`. Rung `td-check`:
  td's double-build agrees (reproducible) AND the differential oracle `guix build
  --check` agrees on the same `.drv`. Input resolution + the daemon building the inputs
  stay Guix's; the verdict is td's) — `plan/td-check.md`
- [ ] **loop-sandbox** — claimed claude-fable-4a2e33 2026-06-13 (gate-2: td's sandbox
  hosts a loop step, toward replacing `guix shell -C`. Additive equivalence FIRST
  (don't touch check.sh yet): `td-builder host-sandbox` is a dev-shell — pivot into a
  fresh root exposing the WHOLE `/gnu/store` (ro) + `/var/guix` (daemon socket) + host
  guix on PATH, host fs otherwise gone. Rung `loop-sandbox`: `guix build -d <target>`
  inside td's sandbox is byte-identical to under `guix shell -C` (exposure), and a
  host-only path is invisible (isolation). Net-isolation parity + the check.sh swap are
  later increments) — `plan/loop-sandbox.md`
- [ ] **td-store-db** — claimed claude-fable-4a2e33 2026-06-14 (begin replacing
  guix-daemon: td owns the store SQLite DB authority. Inc.1/2 — `td-builder
  store-register` WRITES the `ValidPaths`/`Refs`/`DerivationOutputs` for hello's full
  closure as the SQLite FILE FORMAT in pure Rust (`store_db.rs`), differential vs the
  daemon. Inc.3 — `td-builder store-query` READS it back with td's OWN pure-Rust SQLite
  reader (`store_db_read.rs`), no sqlite3/daemon in td's query path ("own the store,
  then diverge"); td's reader == sqlite3 (same bytes) == the daemon. Inc.4 — `td-builder
  store-add-text` PLACES a path into a td-owned store (the daemon's addToStore, write
  side, flat case): td computes the path, WRITES a canonical 0444 store file, registers
  it — byte-identical (NAR) to the daemon's own store file (the WAL-free oracle). Inc.5 —
  `td-builder store-closure` computes GC reachability (the daemon's THIRD role): walks the
  Refs graph from a root with td's own reader (GC mark/liveness, no daemon) == `guix gc
  -R`. Inc.6 — `td-builder store-add-recursive` does the recursive addToStore: computes the
  content-addressed `source` path + CANONICALLY restores a directory TREE (exec bit +
  symlinks) byte-identical (NAR) to the daemon's interned tree. Daemon is the oracle; td
  operates its OWN store DB, host daemon stays immutable infra) — `plan/td-store-db.md`

## The loop (reminder)

One command: `./check.sh`. The `Makefile`'s `CHEAP_RUNGS`/`HEAVY_RUNGS` pools
(expanded by `check:`) are the authoritative rung list (don't restate it here); the
cheap rungs run serial-first, the heavy rungs two at a time (`make -j2`), and a red
still short-circuits. Don't advance a sub-task until green. Small commits, each
stating which test now passes.
