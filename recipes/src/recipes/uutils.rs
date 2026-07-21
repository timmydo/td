use crate::types::Recipe;

// uutils ships coreutils as one multicall binary. Select exactly the applets
// td's read-only root symlinks into /bin (UUTILS_APPLETS in system-x86-64.rs),
// not a published aggregate: `feat_Tier1`/`unix` drag in ~185 crates we never
// ship -- the checksum tools (sha*sum/md5sum/b2sum/cksum), `factor`'s bignum,
// `more`'s pager stack, regex, rand, every `cc` C-build edge, and `stdbuf`,
// whose crates.io archive lacks src/libstdbuf and embeds an empty preload
// library. The residual closure is uucore + clap + uucore's baked-in i18n.
//
// Unlike ripgrep/fd (built only via `td shell`), uutils builds as a `--auto`
// graph node so the read-only-root system image can consume it: `source_input`
// wires TD_SRC from the pinned .crate; `native_inputs` name the build platform
// (rust-toolchain for cargo/rustc, gcc/binutils/glibc-`self` for the native link
// env the builder derives, busybox for cp/chmod/tar); `cargo_lock` is the
// committed, checksum-pinned closure the `--auto` vendor gate verifies against.
pub fn recipe() -> Recipe {
    Recipe::rust("uutils", "0.9.0")
        .source_input("uutils-source")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_lock("recipes/locks/uutils/Cargo.lock")
        .bins(&["coreutils"])
        .no_default_features()
        // Keep this list in sync with UUTILS_APPLETS in system-x86-64.rs.
        .features(&[
            "uname", "ls", "cat", "echo", "printf", "pwd", "cp", "mv", "rm",
            "mkdir", "rmdir", "ln", "id", "env", "df", "du", "chmod", "chown",
            "sleep", "sync", "wc", "head", "tail", "sort", "date", "whoami",
            "tty", "dd", "mktemp", "seq", "touch", "mknod",
        ])
}
