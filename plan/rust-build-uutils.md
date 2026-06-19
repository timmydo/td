# rust-build-uutils — td builds a REAL uutils coreutils tool (Increment 3)

Handle: claude-fable-c018e3 · claimed 2026-06-18 · section: side

## Why (human-directed, 2026-06-18)

The payoff of the rust-build arc: build an actual Rust **coreutils replacement** from
source with td's own builder — no gnu-build-system, no guix cargo-build-system. Inc.1
([[rust-build-recipe]] #84) self-hosts a zero-dep crate; Inc.2 ([[rust-build-vendor]]
#87) added the vendored-dependency path. This rides both to build the uutils `cat`.

## Target: uu_cat 0.9.0 → the `cat` binary

`cargo install uu_cat` builds a `cat` binary (its `[[bin]]` is named `cat`). Its full
dependency closure is **139 crates** (clap + uucore + fluent localization + transitive,
incl. platform/dev entries in the shipped Cargo.lock).

## No builder code change

Inc.1 already unpacks a TARBALL source (run_rust: TD_SRC a tarball → tar xf →
single_subdir), and Inc.2 already vendors `*.crate` lock entries. uu_cat's source IS a
crates-io tarball, so this increment is **pure recipe + lock + gate**:
- `tests/ts/recipe-cat.ts` — buildSystem "rust", bins ["cat"].
- `tests/cat-uutils.lock` — GENERATED from uu_cat's Cargo.lock: the `cat-source` entry
  (uu_cat crate tarball) + 139 `*.crate` deps, all static.crates.io fixed-output fetches
  (sha256 == Cargo.lock checksum), + the 7-crate toolchain seed. Fully static (no
  gate-time source interning — the source is a fetch, not a local tree).
- `mk/gates/340-rust-uutils.mk` — ALL-DURABLE: STRUCTURAL (off-PATH build, .drv carries
  TD_VENDOR_CRATES), DURABLE behavioral (the built `cat` round-trips a file AND a stdin
  pipe — real cat behavior), DURABLE repro (td-builder check over the 139-crate graph).
- `tests/guix-dependence.scm` self-host-specs += "cat" (no corpus oracle).

## De-risk (host, before wiring) — all ✓
- uu_cat 0.9.0 binary is `cat`; vendored offline build succeeds; `cat <file>` works.
- REPRODUCIBLE: built twice at different paths → byte-identical across all 139 crates
  (proc-macros, build scripts, fluent locales all deterministic with SOURCE_DATE_EPOCH
  + remap-path-prefix).
- All 140 FODs (source + 139 deps) warm from static.crates.io, 0 failures.

## Sub-task ladder
1. [x] de-risk: vendor + offline build + run + reproducibility (host)
2. [x] claim + plan-index
3. [x] warm 140 FODs; generate tests/cat-uutils.lock
4. [x] recipe-cat.ts + gate 340-rust-uutils + census exclusion
5. [ ] ./check.sh rust-uutils green + verified-red
6. [ ] full ./check.sh green; review; ready + auto-merge

## Verified-red evidence
(to fill)
