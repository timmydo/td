# rust-build-vendor — td builds a Rust crate WITH dependencies (Increment 2)

Handle: claude-fable-c018e3 · claimed 2026-06-18 · section: side

## Why (human-directed, 2026-06-18)

Increment 1 ([[rust-build-recipe]] / #84) self-hosts td-builder — a zero-dependency,
std-only crate. The real Rust replacements (uutils, russh) have crate graphs, so the
next capability is **building a crate WITH dependencies offline + reproducibly**. This
proves the cargo-vendor dependency path; Increment 3 (a uutils tool) rides on it.

## Mechanism (de-risked on the host)

- Each dep crate is downloadable from a **stable, content-addressed URL**:
  `https://static.crates.io/crates/<name>/<name>-<ver>.crate`, whose sha256 **equals**
  the `Cargo.lock` checksum. So each dep is a declared fixed-output `url-fetch` input
  (offline contract unchanged — fetched/warmed once, pinned by hash).
- cargo's offline `vendored-sources` only verifies the `.cargo-checksum.json` **`package`**
  field (= the `.crate` sha256), not the per-file map — so the vendor dir can be
  assembled from the `.crate` with a MINIMAL `{"files":{},"package":"<sha>"}`.
- `cargo build --release --offline --frozen` against that vendor dir builds + runs.
  Verified on the host: a binary depending on `itoa` builds offline and prints `42`.

## Design

- **`builder/src/build.rs` `run_rust`**: if `TD_VENDOR_CRATES` (':'-joined `.crate`
  paths) is set, assemble `$build/vendor/<name>-<ver>/` from each (untar + write the
  minimal checksum json), write a `CARGO_HOME/config.toml` with `[source.crates-io]
  replace-with="vendored-sources"` + `[source.vendored-sources] directory=<vendor>`,
  then `cargo build --offline --frozen`. No deps ⇒ unchanged (Inc.1 path).
- **`builder/src/main.rs` `build_recipe`** (rust branch): lock entries whose NAME ends
  `.crate` → `TD_VENDOR_CRATES` (each also an input-src so realize stages it); the rest
  stay TD_INPUTS (toolchain).
- **Demo crate** `tests/vendor-demo/` (authored, minimal): a binary depending on real
  zero-transitive-dep crates (itoa, ryu). Source interned like %builder-source.
- **`tests/td-vendor-demo.lock`**: toolchain seed + the dep `.crate` FOD store paths
  (warmed via guix url-fetch; pinned). Source line appended at gate time.
- **Gate `mk/gates/335-rust-vendor.mk`**: PREP realizes seed + dep FODs + interns source;
  BUILD `build-recipe` with guix/Guile off PATH. Legs: [STRUCTURAL] off-PATH build;
  [DURABLE behavioral] the binary runs + prints expected; [DURABLE repro] td-builder
  check; [self-discrimination] the deps are load-bearing.

## Sub-task ladder
1. [x] de-risk vendor mechanism on host (itoa: fetch .crate, minimal checksum, offline build)
2. [ ] claim + plan-index
3. [ ] demo crate (itoa + ryu) + warm/pin the dep FODs
4. [ ] run_rust vendoring + build_recipe *.crate routing
5. [ ] gate 335-rust-vendor + verified-red
6. [ ] full ./check.sh green; review; ready + auto-merge

## Verified-red evidence
(to fill)
