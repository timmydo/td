// recipe-td-ts-eval.ts — td's recipe for td-ts-eval ITSELF (the boa-based JS
// evaluator, ts-eval/), authored in TypeScript: a from-source Rust build of a SEED
// TOOL (move-off-Guile §5; follow-on to td-builder's self-host). `buildSystem:
// "rust"` selects td-builder's cargo phase runner (build::run_rust); `bins` is what
// it installs into `$out/bin`.
//
// There is no `source` here: the crate is the in-tree ts-eval/ tree, supplied at
// build time by the lock's `td-ts-eval-source` entry (the gate interns the CURRENT
// tree with td's own recursive addToStore), not an upstream fetch. The dependency
// closure (boa + 128 crates) is supplied as vendored `.crate` fetches in the lock
// (TD_VENDOR_CRATES). `td-builder build-recipe` resolves every input from the pinned
// lock (no specification->package), ASSEMBLES the .drv in Rust (no guix (derivation
// …)) and REALIZES it daemon-free (no guix-daemon) — built by the td-bootstrapped
// stage0, so nothing in td-ts-eval's build path is guix/Guile. The rustc/cargo/gcc
// seed in the lock is the guix-built toolchain, retired LAST (§5).
recipe({
  name: "td-ts-eval",
  version: "0.1.0",
  buildSystem: "rust",
  bins: ["td-ts-eval"],
});
