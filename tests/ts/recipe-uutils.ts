// recipe-uutils.ts — the FULL uutils coreutils, built from source by td. Extends
// the uu_cat demo (recipe-cat.ts) to the whole multicall binary: the published
// `coreutils` crate 0.9.0 (default-run "coreutils", default feature feat_common_core)
// produces ONE binary that dispatches to all the common utilities (ls/cp/mv/rm/cat/
// mkdir/ln/…). buildSystem "rust" selects td-builder's cargo phase runner; plain
// `cargo build --release` builds the multicall binary (no recipe `features` needed —
// feat_common_core is the crate's default). The 507-crate dependency closure (all
// uu_* members + shared deps) is vendored as `.crate` fetches in the lock
// (TD_VENDOR_CRATES); the source is the upstream `coreutils` crate tarball, keyed
// `uutils-source` (so no fetchSource). build-recipe assembles + realizes the .drv with
// no guix (derivation …) / no guix-daemon; the rustc/cargo/gcc seed is external (§5).
//
// Named "uutils" (NOT "coreutils") so the guix-dependence census does not resolve it
// to GNU coreutils' oracle — it is a from-source Rust tool with no (gnu packages)
// oracle by design, listed in self-host-specs (like `cat`). This is the first step of
// the Rust-focused minimal distro: the userland becomes td-built Rust from source.
recipe({
  name: "uutils",
  version: "0.9.0",
  buildSystem: "rust",
  bins: ["coreutils"],
});
