# plan/ts-frontend.md — TypeScript spec surface on a boa evaluator (Phase 1)

Track: **ts-frontend** (DESIGN §7.1, approved 2026-06-12 — §4.3 gate-1).
Claim: claude-fable-3ca5dd, 2026-06-13 (took over from claude-fable-87a496, who
chartered the track in #15 — now merged — and stopped at "implementation pending";
no implementation PR was open, so the track was re-claimed cleanly from main).
Status: **IMPLEMENTING** — charter landed in #15; sub-task 1 (swc transpile) in
progress. Single writer: the claiming agent.

## Decision log (binding for this track)

- **Evaluator sourcing — boa vendored as a pinned input** *(human, 2026-06-13)*.
  The §7.1-named evaluator **boa is NOT in the pinned channel** (commit
  520785e…): there are no `rust-boa*` crates at all, so the plan's earlier
  "warm store in" assumption (that boa would resolve like `rust-swc`) does not
  hold. The human chose to stay faithful to the charter rather than swap the
  engine: bring `boa_engine` + its full transitive crate tree in as a
  **hash-pinned `cargo vendor` fixed-output** input and build it with
  `cargo-build-system`, pure-Rust and in-process in the Rust builder as
  chartered. Mechanics: Guix permits network for fixed-output derivations
  (output content-addressed), so the vendored-crate fetch happens once via the
  daemon and the loop stays offline by construction (substitutes disabled, only
  the DECLARED fixed-output source fetch — same narrowed contract as
  `tests/typed-diff.scm`). OPEN: the artifact policy (§7.1 ci-image-pipeline,
  "all generated artifacts on pipelines") — a vendored-crate tarball is closer
  to a pinned upstream *source* than a build output, but producing the initial
  pin needs network; resolve when sub-task 2 lands the boa crate (document a
  pipeline path or a signed-off bring-up exception, as ci-gate did for its v1–v3
  images).
- What IS in the pinned channel and builds offline today: `node` 22.14.0,
  `rust-swc` 1.2.129 (ships the `swc` CLI via `swc_cli`; transitive crates come
  from Guix's `cargo-inputs 'rust-swc` registry), and the C JS engines
  `quickjs`/`quickjs-ng`/`duktape`/`mujs`. `typescript`/`tsc` is NOT packaged —
  the sub-task-1 type-check rung will vendor the `typescript` npm package as a
  pinned input run under the packaged `node` (small analog of the boa pin).

## Goal (Phase 1 of the §5 move-off-Guile goal)

Replace the spec *language* — not yet the corpus. A TypeScript spec for td's
system lowers to the same drvs as the frozen Guile oracle, evaluated hermetically.
Guile/gexps remain underneath as the migration lowering target and as the
differential oracle (§2.5); they are retired LAST (after surface and corpus are
off them), because the oracle is the equivalence check that protects the migration.

Out of scope here (later phases / non-goals — DESIGN §5/§6):
- Corpus replacement (own recipes) — Phase 2, parked in §6, separately gated.
- Full-source bootstrap; general-purpose comprehensiveness — standing non-goals.
- Seed/first toolchain stays external (pinned fixed-output input).

## Pipeline

```
spec.ts --swc--> spec.js --boa(curated global)--> config value --lower--> drv
                                                                            |
                                       differential vs system/td.scm (NAR-hash-equal)
```

- **swc** (Rust) strips types TS→JS for *execution* (boa runs JS, not TS). The
  types are not wasted — they earn their keep in a separate **`tsc` type-check
  pass** (author-time + a loop/CI rung), which is where a bad spec is caught
  *before* it ever runs: `rootFsType: "ext3"`, a missing field, the wrong shape.
  This is the standard TS split — `tsc` checks, swc emits — with one consequence
  worth stating: types only buy anything if `tsc` actually runs, so a type-check
  rung is **first-class here, not optional** (erased types with no checker would
  be pure decoration). What `tsc` cannot see — values from dynamic computation,
  cross-field invariants, data parsed at eval time — the lowering builtins still
  validate at runtime (as `td-config` does today). Types and runtime checks are
  complementary, not redundant: compile-time catches shape, runtime catches
  values.
- **boa** (`boa_engine`, pure-Rust) evaluates the JS in-process, inside
  td-builder's existing user-namespace sandbox.
- **Lowering builtins** are boa native functions holding the live build graph:
  - `pkg(name)` — resolve against the (still-pinned) corpus; returns a handle.
  - `storeRef(pkg, subpath)` — the gexp `#$(file-append …)` analogue: records the
    dependency edge AND yields the store path, in one Rust fn (single source —
    cannot desync, unlike a two-place label+path scheme).

## Hermetic eval — determinism is not just isolation

Sandboxing stops I/O; it does NOT stop language-level nondeterminism. boa ships
standard builtins, so the curated global must also remove/neuter:
- **Remove**: `Date` (clock), any `fetch`/`fs`/`process` (boa has no web/Node APIs
  by default, so these are absent unless added — keep them absent).
- **Deny**: `Math.random`, `crypto.getRandomValues` (throw).
- **Pin**: locale/timezone if any `Intl`/formatting is reachable.
- Insertion-order iteration (Map/Set/string keys) is deterministic in JS — fine.
boa is a tree-walking/bytecode interpreter (no JIT) → deterministic by construction
once the above are stripped. Resource isolation (CPU/mem) boa lacks built-in: rely
on td-builder's sandbox (rlimits/seccomp) — the same jail builds run in.

## Acceptance (DESIGN §7.1)

A self-discriminating differential rung (modeled on `tests/typed-diff.scm`):
1. TS v0 spec lowers to a system derivation NAR-hash-equal to `system/td.scm`.
2. A perturbed TS spec diverges — **verified-red**.
3. A spec attempting I/O (network/fs/clock/randomness) is rejected by the hermetic
   evaluator — **verified-red** by a probe spec that must fail.

## Implementation progress (verified-red log)

- **Sub-task 1 — TS spec front-end (`ts` rung): DONE 2026-06-13.** tsc does both
  the type-check and the type-stripping emit (transpiler decision above). New
  files: `system/td-ts.scm` (the pinned `td-typescript` 5.5.4 input — npm tarball
  url-fetch + sha256, copy-build-system, runs under the packaged `node`),
  `tests/ts/td-spec.d.ts` (the v0 dialect — ambient globals, mirroring the future
  boa global), `tests/ts/spec-v0.ts` (well-typed v0 spec), `tests/ts/spec-bad-fstype.ts`
  (the always-on negative control: `rootFsType: "ext3"`), `tests/ts/spec-v0.expected.js`
  (golden emit), `tests/ts-check.sh` (driver). Wired: `Makefile` `ts` rung +
  `HEAVY_RUNGS`; `tests/eval.scm` loads `(system td-ts)`. GREEN in-sandbox
  (`./check.sh ts`, `./check.sh eval`); `guix build --check td-typescript`
  reproduces bit-for-bit.
  - Verified-red ×3 (perturbed COPIES in the job tmp, real fixtures untouched —
    the "commit before red variants" gotcha): (1) appended garbage to the golden →
    transpile leg reds; (2) flipped the GOOD spec to `"ext3"` → type-check-good leg
    reds (TS2322); (3) flipped the BAD control to `"ext4"` → type-check-bad leg reds
    ("tsc ACCEPTED …"). Real-dir control stays green.
  - Open-question resolved: the TS dialect lives in `tests/ts/td-spec.d.ts`
    (ambient globals = the curated boa global). The corpus-handle and
    reuse-vs-add-harness questions belong to sub-tasks 2/4/5, still open.

- **Sub-task 2 — boa evaluator + curated global (`ts-eval` rung): DONE 2026-06-13.**
  The chartered pure-Rust, in-process boa evaluator. New `ts-eval/` crate
  (`boa_engine` 0.20) — `src/main.rs` reads JS on stdin, evaluates a
  CURATED-GLOBAL prelude first (`delete globalThis.Date`; `Math.random` →
  throw), then the user JS, printing the result. Packaging
  (`system/td-ts.scm`): `%ts-eval-vendor` is a **fixed-output** `cargo vendor`
  (network permitted because content-addressed; nss-certs has no bundle so the
  builder concatenates the hashed certs into one for libcurl), hash-pinned
  `07kpr4kf…`; `td-ts-eval` builds **offline** against it with rust 1.93.0.
  `guix build --check td-ts-eval` reproduces bit-for-bit. Rung `ts-eval`
  (`Makefile` + `HEAVY_RUNGS`, `tests/ts-eval-drv.scm` + `tests/ts-eval-check.sh`)
  --checks the binary (verdict-memoized) then asserts: `1+2*3⇒7`; `typeof
  Date⇒undefined`; `Math.random()` denied; `Math.max⇒4` (curation is surgical).
  GREEN in-sandbox (`./check.sh ts-eval`, 14s warm).
  - Vendoring approach: **repo-clean** — only `ts-eval/{Cargo.toml,Cargo.lock,src}`
    are committed (boa pulls ~110 crates / ~53 MB; not committed). The ~50 MB
    vendor lives hash-pinned in the store via the fixed-output derivation. NOTE
    for CI/ci-image-pipeline: `%ts-eval-vendor` must be warm in any store the
    offline loop runs against (image PREP may fetch, §5 "warm store in") — the
    CI store image needs regenerating to include it (follow-up).
  - Verified-red ×2 (host cargo, perturbed prelude): drop `delete …Date` →
    leg (2) reds ("typeof Date = function"); make `Math.random` return 0.42 →
    leg (3) reds ("ALLOWED ⇒ 0.42"). Restored control green.
  - Resolved open question: corpus-handle representation is deferred to sub-task 4
    (no builtins yet); the evaluator boundary is stdin JS → stdout value for now.

## Sub-task ladder (write the test first; verify red before trusting green)

1. ~~swc~~ **tsc** TS→JS transpile + a **`tsc` type-check rung** (pinned, offline)
   — **DONE** (see progress log; tsc does both, swc dropped per the decision log):
   the transpile leg asserts a fixed `.ts` → golden `.js` (verify red: corrupt the
   output); the type-check leg asserts a well-typed spec passes and an
   ill-typed one (`rootFsType: "ext3"`) FAILS `tsc` — **verified-red** —
   so the types are load-bearing, not decoration.
2. boa eval of a trivial JS expression returning a known value; curated global in
   place. (Verify red: leave `Date` present, assert it is gone.) — **DONE**
   (`ts-eval` rung; see progress log).
3. Hermetic-eval rung: a spec touching `Math.random`/fs is rejected. (Verified-red
   per acceptance #3.)
4. `pkg`/`storeRef` builtins; lower a minimal fragment; compare one drv to the
   oracle's.
5. Full v0 system spec → NAR-hash-equal to `system/td.scm` (acceptance #1);
   perturbation diverges (acceptance #2).

## Exclusive-landing note

This chartering PR edits DESIGN.md §5/§6/§7.1 (the settled contract) + PLAN.md —
an exclusive landing. Announced here; others rebase. Implementation lands as its
own non-exclusive track PRs once chartered. The boa/swc crates are new pinned
inputs (declared like any other; §5 substitutes posture unchanged — warm store in,
loop offline). CLAUDE.md is not edited by this PR (its §5 reference is the
free-software posture, unchanged); add a move-off-Guile note there as a follow-up
if the human wants agent-facing posture updated.

## Open questions for implementation

- Corpus handle representation across the boa boundary (JsValue object vs. opaque
  id into a Rust-side table).
- Where the TS spec dialect is documented/restricted (the "supported subset").
- Whether `tests/typed-diff.scm`'s harness is reused or a parallel `ts-diff` rung
  is added (prefer adding, not modifying — strengthening tests is free).
