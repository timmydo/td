use crate::types::Recipe;

// td-sshd — a source-built russh SSH daemon, shipped in the system-x86-64 image.
//
// FIRST local-source rust recipe (#469 local-source provenance): instead of a
// fetched `.crate`, `local_source` names an IN-TREE directory (tests/sshd) that
// the runner copies into the seed store as this recipe's `sshd-source` seed,
// pinned by the compiled seed-digest table — the source bytes are the committed
// tree, not a downloaded archive.
//
// Like uutils it builds as a `--auto` graph node so the read-only-root image can
// consume it: `native_inputs` name the build platform (rust-toolchain for
// cargo/rustc, gcc/binutils/glibc-`self` for the native link env, busybox for
// run_rust's cp/chmod/tar). `cargo_lock` points at the IN-TREE Cargo.lock — the
// SAME lock run_rust builds `--frozen` against — so the `--auto` vendor gate and
// the build share one checksum-pinned closure with no second copy to drift.
//
// Unlike uutils, defaults are KEPT: russh's default features pull its crypto
// backend, whose aws-lc-sys C crate is compiled by td's own GCC/binutils via
// run_rust's CC/AR env (AGENTS.md sanctions C/asm crates built with td tools) and
// links against the declared glibc runtime closure — never a host compiler.
pub fn recipe() -> Recipe {
    Recipe::rust("sshd", "0.1.0")
        .local_source("tests/sshd")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_lock("tests/sshd/Cargo.lock")
        .bins(&["sshd"])
}
