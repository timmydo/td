# bootstrap-ts-eval — build td's seed TOOLS with td (move-off-Guile §5)

Follow-on to [[bootstrap-td-builder]] (DONE: the guix-built td-builder is no longer the
build tool in any gate). The package gates still resolve three guix-built SEED TOOLS for
TS-recipe evaluation (`ts-emit`): `node`, `td-typescript` (tsc), and **`td-ts-eval`**.
Of these, td-ts-eval is td's OWN pure-Rust boa evaluator (`ts-eval/`,
`boa_engine = "0.20"`), so it can be built by stage0 via `build-recipe` exactly like the
rust gates — node + tsc are the hard JS-runtime seed (retired-late, like the toolchain §5).

Invocation-count framing: [[td-move-off-guile-remove-invocations]]. Removing
`guix build -e (system td-ts) td-ts-eval` from the package gates is the next category-1
seed-tool win.

## Brick 4: td builds td-ts-eval from source via build-recipe + stage0

**Goal.** A new gate builds td-ts-eval (boa + 128 crate deps) with the td-bootstrapped
stage0 via `td-builder build-recipe` (buildSystem rust), runs it (evaluates a probe TS
spec), and is reproducible — mirroring `rust-build`'s self-host of td-builder. The
guix-built td-ts-eval is the SEED that evaluates td-ts-eval's OWN recipe (the
bootstrap circularity, resolved by the seed) + the behavioral ORACLE.

**The vendored lock (the crux).** boa pulls 128 crates (ts-eval/Cargo.lock). Each is a
fixed-output `static.crates.io` fetch keyed by its Cargo.lock sha256 — the SAME shape as
`tests/cat-uutils.lock` (139) / `tests/td-russh-demo.lock` (188). Generator: a guile
script realizes each crate via `(origin (method url-fetch) …)` (hex sha256 →
nix-base32) and prints `<name>-<version>.crate <store-path>`. Probe CONFIRMED:
`autocfg-1.5.1.crate → /gnu/store/35qjdx…-autocfg-1.5.1.crate`. Generation is a one-time
NETWORK prep (outside the offline loop — the §5 "warm store in"); the lock is then a
checked-in pin. `tests/td-ts-eval.lock` = the toolchain seed (rust/cargo/gcc-toolchain/
coreutils/bash/…) + the 128 `*.crate` lines. The source (`ts-eval/`) is interned at gate
time by `tests/intern-src.sh` (store-add-recursive), as rust-build does.

**The recipe.** `tests/ts/recipe-td-ts-eval.ts` — `buildSystem: "rust"`,
`bins: ["td-ts-eval"]`, source key `td-ts-eval-source`.

**The gate** (`mk/gates/350-rust-ts-eval.mk`, BUILD_GATE, after build-recipes):
- resolve node/tsc + the SEED td-ts-eval (`guix build (system td-ts) td-ts-eval`) for
  ts-emit; bootstrap+load stage0 (cache-lib load_stage0); intern ts-eval/ source.
- `build-recipe` td-ts-eval with stage0 + TD_VENDOR_CRATES (128) + the override, guix/
  Guile off PATH.
- [DURABLE structural] the .drv builder is the stage0 path; the .drv carries
  TD_VENDOR_CRATES.
- [DURABLE behavioral] the td-built td-ts-eval EVALUATES a probe TS spec to the expected
  JSON (it works as the evaluator — boa runs).
- [DURABLE repro] td-builder check double-build.
- [MIGRATION ORACLE] the td-built td-ts-eval evaluates the probe identically to the
  guix-built td-ts-eval, at a DISTINCT store path (own, then diverge).

**Bootstrap circularity (honest).** td-ts-eval's recipe is evaluated by ts-emit, which
needs A td-ts-eval — the guix-built SEED. So `guix build (system td-ts) td-ts-eval`
REMAINS in this gate as the seed+oracle (like guix-tb in rust-build). Removing it from
the OTHER gates' ts-emit (using the td-built td-ts-eval) is Brick 4b.

### Sub-task ladder

1. [x] Generator → `tests/td-ts-eval.lock` (toolchain seed + 128 boa crates); realized
       all 128 (guile url-fetch realizer, hex→nix-base32; one-time network prep).
2. [x] `tests/ts/recipe-td-ts-eval.ts` (buildSystem rust, bins td-ts-eval).
3. [x] `mk/gates/350-rust-ts-eval.mk` — build via stage0, evaluate a probe, repro,
       oracle, structural.
4. [x] `./check.sh rust-ts-eval` GREEN; verified-red.
5. [ ] Affected/landing check; PR.

### Status / evidence

- `./check.sh rust-ts-eval`: GREEN. td built td-ts-eval (boa, 128 vendored crates) with
  stage0; the td-built td-ts-eval EVALUATES a TS spec (the hello recipe → the expected
  JSON — boa runs), IDENTICALLY to the guix-built td-ts-eval, reproducible (double-build),
  at a DISTINCT path (`fwyf5h9…` is guix's). drv builder == stage0 (cargo→stage0→td-ts-eval).
- Census fix: `td-ts-eval` added to `guix-dependence.scm` self-host-specs (a seed tool
  with no `(gnu packages)` oracle, like td-builder/cat/td-russh-demo); census output
  UNCHANGED (23 owned recipes, matches .expected — no re-baseline). Flagged in the PR.
- **Verified-red (behavioral/oracle is load-bearing):** pointed the behavioral run at the
  stage0 td-builder (not an evaluator) → its output (`td-builder 0.1.0 ok`) DIVERGED from
  guix's JSON → `DISAGREE` FAIL (exit 2). The leg genuinely requires the td-built
  td-ts-eval to evaluate correctly. Reverted. (The structural/override leg's causal red
  is gate 365 / Brick 3b.)
- Bootstrap circularity honest: guix-built td-ts-eval is the SEED (evaluates the recipe)
  + oracle; Brick 4b swaps the OTHER gates' ts-emit onto the td-built evaluator.

## Brick 4b (claude-fable-300f35): the gnu gates evaluate with td's own td-ts-eval

**Goal.** The gnu-recipe build path (build-recipes phase + corpus/toolchain/corpus-deps
gates) evaluates its recipes with the td-BUILT td-ts-eval, not the guix-built one —
removing `guix build (system td-ts) td-ts-eval` from those gates. The td-built evaluator
produces byte-identical JSON to guix's (same source, rust-ts-eval oracle), so NO build
output changes — the swap changes WHO evaluates, not WHAT'S built. node + tsc stay guix
(ts-emit's transpile step); 4b removes only the evaluate-step invocation.

**The prelude is fixed, not a per-loop cost.** td-ts-eval is built ONCE via the shared
`tests/ts-eval-tool.sh` (the rust-ts-eval gate's build logic, extracted) into a
content-addressed cache; warm reruns REFERENCE the cached binary instantly (like guix
references its substituted one). The cold build is one more td-built artifact, same model
as the corpus/toolchain — and SHARED: build-recipes builds it once, the rust-ts-eval gate
then cache-hits it (no double-build).

**Design.**
- `tests/ts-eval-tool.sh` — extract the rust-ts-eval build (intern ts-eval/, ts-emit
  recipe-td-ts-eval.ts with the SEED, build-recipe with stage0, memoized); print the
  td-built binary path + write a sentinel.
- `mk/gates/350-rust-ts-eval.mk` — refactor to call ts-eval-tool.sh for the build, then
  run its asserts (behavioral/repro/oracle/structural) on the result.
- `tests/cache-lib.sh load_ts_eval` — read the sentinel → export TD_TS_EVAL=<td-built>.
- `Makefile build-recipes` (EXCLUSIVE) — resolve the SEED + node/tsc + load_stage0, build
  td-ts-eval via ts-eval-tool.sh (the prelude, cached), export TD_TS_EVAL=<td-built>.
- gnu gates — drop `ev=guix build (system td-ts) td-ts-eval`; `load_ts_eval` provides
  TD_TS_EVAL (reads the sentinel build-recipes wrote). Add a DURABLE structural leg: the
  gate's TD_TS_EVAL is the td-built path (under .td-build-cache), NOT guix's /gnu/store.

**Acceptance.** corpus/toolchain/corpus-deps green with TD_TS_EVAL = the td-built
evaluator (structural leg); outputs unchanged (cache-hit); census unchanged. Verified-red:
force load_ts_eval to the guix seed → the structural leg fires. The
`guix build (system td-ts) td-ts-eval` invocation is gone from the gnu gates (only the
build-recipes prelude resolves the seed, once, to BUILD td-ts-eval).

### Status / evidence (4b)

- (in progress)
