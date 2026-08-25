use crate::types::Recipe;

// uutils ships coreutils as one multicall binary. Select exactly the applets
// td's read-only root symlinks into /bin (UUTILS_APPLETS in system-x86-64.rs),
// not a published aggregate: `feat_Tier1`/`unix` drag in ~185 crates we never
// ship -- the checksum tools (sha*sum/md5sum/b2sum/cksum), `factor`, `more`'s
// pager stack, rand, and `stdbuf`, whose crates.io archive lacks src/libstdbuf
// and embeds an empty preload library. The resulting closure is the exact
// union required by the selected applets; notably `expr` carries its pinned
// regex/native support rather than broadening the selection to an aggregate.
//
// Like ripgrep/fd, uutils builds as a `--auto` graph node; the read-only-root
// system image also consumes it. `source_input`
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
            "tty", "dd", "mktemp", "seq", "touch", "mknod", "kill", "readlink",
            "basename", "dirname", "true", "false", "printenv", "link", "unlink",
            "cut", "tr", "expr",
        ])
}
