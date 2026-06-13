# plan/ts-frontend.md — TypeScript spec surface on a boa evaluator (Phase 1)

Track: **ts-frontend** (DESIGN §7.1, approved 2026-06-12 — §4.3 gate-1).
Claim: claude-fable-87a496, 2026-06-12.
Status: **CHARTERED** — this PR adds the §5 goal + §7.1/§6 entries; implementation
not started. Single writer: the claiming agent.

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

- **swc** (Rust) transpiles TS→JS in the inner loop (type-strip only). Full `tsc`
  type-check is a separate author-time/CI pass, not on every eval.
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

## Why boa over javy (decided with the human 2026-06-12)

td's specs are host-call-heavy (corpus lookup, storeRef capture, closure-gate).
- **boa**: pure-Rust crate, live synchronous Rust↔JS native fns holding the graph —
  native fit for "evaluator-as-library with injected builtins"; one Rust toolchain
  (matches td-builder). Cost: younger/incomplete engine, no built-in CPU/mem
  isolation. Mitigations: we control the spec dialect (stay in boa's supported
  subset — Starlark-style restriction), and the sandbox covers resource limits.
- **javy** (QuickJS-on-wasmtime): mature engine + by-construction capability
  isolation + deterministic fuel/memory limits + OS-agnostic. But host builtins
  become marshaled WASM imports (awkward for our call-heavy lowering), and it adds
  wasmtime + a JS→WASM build step. Revisit javy if specs ever become untrusted or
  OS-agnostic sandbox portability outranks toolchain simplicity.

## Acceptance (DESIGN §7.1)

A self-discriminating differential rung (modeled on `tests/typed-diff.scm`):
1. TS v0 spec lowers to a system derivation NAR-hash-equal to `system/td.scm`.
2. A perturbed TS spec diverges — **verified-red**.
3. A spec attempting I/O (network/fs/clock/randomness) is rejected by the hermetic
   evaluator — **verified-red** by a probe spec that must fail.

## Sub-task ladder (write the test first; verify red before trusting green)

1. swc TS→JS transpile step, pinned and offline-buildable; rung asserts a fixed
   `.ts` → expected `.js`. (Verify red: corrupt the transpile output.)
2. boa eval of a trivial JS expression returning a known value; curated global in
   place. (Verify red: leave `Date` present, assert it is gone.)
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
