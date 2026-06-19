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

1. [ ] Generator → `tests/td-ts-eval.lock` (toolchain seed + 128 boa crates); realize them.
2. [ ] `tests/ts/recipe-td-ts-eval.ts` (buildSystem rust, bins td-ts-eval).
3. [ ] `mk/gates/350-rust-ts-eval.mk` — build via stage0, run (evaluate a probe), repro,
       oracle, structural; verified-red.
4. [ ] `./check.sh rust-ts-eval` green; verified-red.
5. [ ] Full/affected landing check; PR.

### Status / evidence

- (in progress) — crate-fetch mechanism probed green (autocfg).
