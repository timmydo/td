# Track: td-builder (side-track)

**Claim status:** see `PLAN.md` (the single source of truth for claims).
**Origin:** approved 2026-06-11 (§4.3 gate 1) — the first Guix-component
replacement under the §2.5 discipline. Opens the own-builder era the
offline-isolation closure deferred daemon-side isolation to.
**Scope authority:** DESIGN §7.1.

## Goal

A td-owned builder — a Rust binary — that executes a `.drv` in a
user-namespace sandbox and registers the output, proven behaviorally
equivalent to the pinned `guix-daemon` (prime directive 4: the daemon is the
oracle; never replace without a differential).

## Acceptance (from DESIGN §7.1)

The daemon-vs-td-builder store differential, run as a self-discriminating
rung: the same drvs — a trivial gexp drv, an environment-sensitive divergence
probe, and the system image drv (the `build` rung's qcow2) — built both ways
yield NAR-hash-equal outputs at identical store paths. Verified-red by a
deliberate builder defect the rung catches (e.g. wrong NAR serialization, a
leaky sandbox, a references mis-registration).

**Probe-vs-oracle caveat** (from the rootless track's verified-red A): a
uid_map-reading probe GENUINELY diverges between the root daemon
(`0 0 4294967295`) and any userns builder (`30001 30001 1`) — reusing the
rootless probe verbatim against the root daemon yields a permanent, by-design
red. Either restrict the divergence probe to env details that legitimately
must match the chosen oracle configuration (getuid, /dev set, env scrubbing,
hostname — see open question 4), or use the rootless-configured daemon as the
oracle side (legitimate: the `rootless` rung already proves it store-path
equal to the root daemon, so oracle authority transfers).

## Settled decisions

- **Vehicle: Rust** (human, 2026-06-11). Building the toolchain from source is
  a non-goal right now — the host store may be warmed with substitutes for the
  pinned channel's Rust closure (DESIGN §5). The loop itself stays
  offline/no-substitutes. S1 still verifies the warm toolchain actually
  compiles td-builder *inside* the check.sh sandbox before any rung depends on
  it.
- **Harness reuse.** `tests/rootless.sh`'s mechanics — sqlite `.backup` DB
  snapshot, staged store rbind at `/gnu/store`, validity guards (oracle output
  valid, probe output invalid), uid_map isolation probe, kept-output +
  diffoscope diagnostics — are the rung's skeleton; the rebuild side swaps
  from "pinned daemon, unprivileged" to td-builder.
- **FSDG:** Rust crates must be free and come through the pinned channel; no
  vendored nonfree code.

## Open questions (decide here, in order)

1. **Interface seam: daemon protocol vs CLI.** (a) Speak the daemon's socket
   protocol so the unmodified `guix` client drives td-builder — the strongest
   single-variable differential, same philosophy as the rootless rung; or
   (b) a standalone `td-build DRV` CLI — simpler, but the differential then
   varies builder *and* client path together. Probe (a)'s surface at the pin
   (nix/libstore/worker-protocol.hh) before choosing; the answer may be
   staged (CLI first, protocol later) if the protocol surface is large.
2. **NAR serialization + hashing.** Must be bit-for-bit identical to the
   daemon's; deserves its own early differential (the daemon's recorded
   `info.hash` / `guix archive` as oracle) and its own verified-red, before
   any build is attempted on top of it.
3. **DB registration.** What td-builder writes — ValidPaths / Refs /
   DerivationOutputs rows (schema: nix/libstore/schema.sql at the pin) — vs
   delegating registration to existing tooling. Equality of the recorded NAR
   hash and references set is part of the differential either way.
4. **Sandbox parity.** Which of the daemon's build-environment details are
   hash-visible and must match exactly: chroot layout, build uid/gid, /dev
   set, env scrubbing, fixed-output network allowance, /proc, hostname,
   timestamps. The rootless track's in-build uid_map probe pattern
   generalizes to probing each of these from inside a build.

## Sub-task ladder (draft — refine as probes land)

- [x] **S1 toolchain probe** — the pinned channel's Rust toolchain (warmed on
  the host) compiles a hello-world td-builder inside the check.sh sandbox,
  offline. Records closure size and compile time (loop-latency budget, §1.3).
  DONE 2026-06-11: implemented by claude-fable-49b6d6 (PR #2, no guix there);
  guix-loop validation + review round by claude-fable-a03d13 (see Working
  state — "S1 guix-loop validation").
- [ ] **S2 NAR differential** — td's NAR serializer hashes a store item
  bit-for-bit equal to the daemon's recorded hash; verified-red by a
  perturbation (e.g. ordering or padding defect).
- [ ] **S3 drv parse + trivial build** — parse the ATerm `.drv`, execute the
  trivial probe drv in a userns sandbox, register the output; NAR-hash-equal
  to the oracle, at the same store path.
- [ ] **S4 the rung** — full acceptance differential including the system
  image drv, plumbed into HEAVY_RUNGS (exclusive landing: `Makefile`, maybe
  `check.sh` sandbox packages).
- [ ] **S5 verified-red** — deliberate builder defects (NAR, sandbox,
  references) each red the rung; evidence recorded here.
- [ ] **S6 land** — §7.2 protocol; release the claim in `PLAN.md`.

## Working state

**Claimed:** claude-fable-49b6d6, 2026-06-11. Starting with S1 (toolchain
probe) — the prerequisite every later sub-task depends on. Open question 1
(daemon protocol vs CLI) is NOT yet decided; it does not block S1, which only
needs the crate to compile and run.

### S1 — toolchain probe (implemented by claude-fable-49b6d6; validated below)

What landed in this increment:

- `builder/` — the Rust crate. `src/main.rs` is a hello-world skeleton that
  prints a stable sentinel `td-builder <version> ok`; `Cargo.toml` declares a
  zero-dependency binary crate (offline by construction — nothing to vendor);
  `Cargo.lock` is committed (pinned, deterministic, no resolver network step);
  `.gitignore` keeps `target/` out of the tree.
- `system/td-builder.scm` — the `td-builder` Guix package, built with the
  pinned channel's `cargo-build-system` + rust toolchain. Source is a
  `local-file` of `../builder` that excludes `target/`/`.cargo/`, so a stray
  local `cargo build` cannot perturb the derivation hash. `#:cargo-inputs '()`,
  `#:tests? #f` (the rung supplies the behavioral assertion by RUNNING the
  binary, not an empty unit suite).
- `tests/td-builder-drv.scm` — two-step lower-then-realise driver: prints
  `DRV=…`; the build and honest pass/fail happen in `guix build`.
- `Makefile` — new `td-builder` rung in `HEAVY_RUNGS` (rung count 21→22): lower
  → build offline → `guix build --check` (reproducibility; re-runs the compile)
  → RUN the binary and assert its sentinel → record `guix size` + wall-clock.
- `tests/eval.scm` — loads `(system td-builder)` so the fast `eval` rung catches
  a syntax/binding error in the new module sub-second.

Shared-spine note (DESIGN §7.3 exclusive landing): this touches `Makefile` and
`tests/eval.scm`. Announced here; landing expects others to rebase.

### Verified-red (S1)

The `td-builder` rung has two assertion legs; both were driven red against the
SAME defects the rung is built to catch. Full guix-loop evidence must be
captured on a guix-capable host (the implementing container had no
guix/guix-daemon — only the rung's *crate-level* legs could be exercised there):

- **A — compile leg (syntax defect).** Appended `fn broken( {` to
  `src/main.rs`; `cargo build --release --offline` failed
  (`error: this file contains an unclosed delimiter` / `could not compile`).
  In the rung this is the `guix build "$drv"` step going red. RESTORED → green.
- **B — run leg (wrong sentinel).** Changed the format string `… ok` → `… NOPE`;
  the binary built but printed `td-builder 0.1.0 NOPE`, so the rung's
  `grep -Eq '^td-builder [0-9.]+ ok$'` assertion fails. RESTORED → green
  (`td-builder 0.1.0 ok`).

### S1 guix-loop validation (claude-fable-a03d13, 2026-06-11 — takeover of PR #2)

Everything the section above owed, delivered on a guix host:

- **Store warm-in (DESIGN §5):** cargo-build-system pins rust 1.93 (not the
  channel's default `rust` 1.91) — warmed the exact td-builder closure with
  `guix build -L . -e '(@ (system td-builder) td-builder)'` pinned to the
  OFFICIAL substitute servers only (`--substitute-urls="https://bordeaux.guix.gnu.org
  https://ci.guix.gnu.org"` — never the host daemon's default list, which
  includes nonguix; FSDG posture). Cold-store lowering fails honestly: the
  no-substitutes source-build path dies on an upstream boost-patch hash
  mismatch — the rung's warm-store precondition is real, not cosmetic.
- **GREEN:** `./check.sh td-builder` passes — lower, offline build,
  `guix build --check` bit-for-bit, sentinel run. `eval` green with
  `(system td-builder)` loaded.
- **RED A through the rung** (not just cargo): `fn broken( {` appended →
  `make: *** [Makefile:567: td-builder] Error 1` (build leg). RESTORED.
- **RED B through the rung:** sentinel `ok` → `NOPE` → the defective binary
  compiles AND passes `--check`, and the rung still reds at the run leg
  (`FAIL: the compiled td-builder did not print its sentinel`) — the legs
  discriminate independently. RESTORED.
- **Review round** (sub-agent, full contract review of dd1ea2f): one
  should-fix — the `#:select?` substring match admitted an EMPTY `target/`
  directory into the source nar, so a stray local `cargo build` perturbed the
  drv hash, contrary to the stated guarantee. Fixed to a basename match and
  proven by drv differential: clean tree vs tree-with-`target/` lower to the
  SAME drv (`qlip8v4p…`) under the fix, and to DIFFERENT drvs (`ff7d8071…`)
  under the old predicate — red and green for the fix itself. Nits applied:
  `.gitignore` also excludes `.cargo`; run-leg FAIL message mentions the
  nonzero-exit cause; drv script imports spelled out like its siblings.
- **S2 reminder (review):** the package sets `#:tests? #f` (legitimate at S1 —
  empty suite, the rung runs the binary); when S2 adds real unit tests this
  MUST flip to `#t` or they silently never run.

### Measurement log (§1.3 — from the real loop run, 2026-06-11)

| metric | value | notes |
|--------|-------|-------|
| td-builder closure size | 74.5 MiB | `guix size` in-rung (rust runtime closure) |
| first compile wall-clock | ~1s (crate, warm toolchain) | the `--check` recompile re-runs it every loop |
| `./check.sh td-builder` wall | ~14s | incl. the ~12s serial cheap chain; rung proper ~2s + --check recompile |
| crate-level compile (rustup, not the rung) | ~9.3s | local sanity only; NOT the loop number |

Open questions 1–4 (protocol seam, NAR, DB registration, sandbox parity) remain
to decide before S2/S3.
