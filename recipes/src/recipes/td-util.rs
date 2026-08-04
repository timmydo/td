use crate::types::{Recipe, Step};

// td-util — target-built static multicall for td's diagnostics userland.
//
// This recipe compiles the td-util CRATE's binary source (`src/main.rs` plus its
// sibling modules) into a statically-linked target ELF. The sources are embedded
// via `include_str!` so the lintable/testable crate and the shipped binary are
// ONE source of truth and cannot drift; the path escapes the
// `recipes/src/recipes/*.rs` catalog glob, so it is not itself a recipe module.
//
// SCOPE: the applets that must run where uutils' coreutils cannot. Two kinds.
// `clear`, `which`, `free`, `ps`, `dmesg` and `less` are names uutils does not
// provide at all, and get `/bin` symlinks — `less` not quite: uutils has a pager
// behind a feature flag that compiles a crossterm stack in, which is a dependency
// this image does not take, so busybox was being carried for `more` alone.
// `cat`, `chmod`, `chown`, `ln`, `mkdir`, `printf`, `readlink`, `rm`, `sleep` and
// `test` it DOES provide — dynamically linked, which the pre-pivot initramfs has no
// loader for and which `/etc/rootcheck` must not depend on, since reporting a broken
// runtime closure is its job. Those ten carry no `/bin` name here (uutils owns them
// and the farms are disjoint) and are reached as `td-util <applet>`.
//
// `free`/`ps` read /proc and `dmesg` reads /dev/kmsg O_NONBLOCK, all ordinary file
// I/O. `less` is the one that is not: taking a keystroke without waiting for Enter,
// and asking how many rows a screen has, are `ioctl(2)`, and nothing in safe `std`
// reaches them. That is the SEVENTH target-side unsafe exception UNSAFE.md records —
// ONE syscall, THREE pinned requests (TCGETS/TCSETS/TIOCGWINSZ), confined to
// `sys.rs` with `term.rs` its only caller — so the crate root is `#![deny]` rather
// than `#![forbid]` and `main.rs`'s `mod confinement` tests hold the surface at that.
// The applets that would widen it further (reboot/poweroff/halt, switch_root,
// cttyhack, init, and `sync`, which is td-init's) stay out of scope: each needs a
// syscall this roster does not have, which is a further reviewed amendment.
//
// system-x86-64 packs td-util into the real root AND both initramfs cpios, routes
// /bin/{clear,which,free,ps,dmesg,less} here, and calls the other ten as
// `td-util <applet>` where it used to call `busybox <applet>`. The greeter runs each
// by its /bin path and emits TD_UTIL_RUNTIME_MARKER only if all six exit 0, so
// `td-recipe-eval qemu-boot-system` re-proves the whole
// farm on every boot — including the /proc and /dev/kmsg applets whose legs below
// are skipped when the build sandbox lacks /proc. That oracle is operator-run (qemu
// is absent from the host-free gate sandbox), so it is a release gate, not a
// per-change one. Same staging td-sh follows.
//
// Why mesboot-style (rustc invoked directly) rather than `Recipe::rust`, and why
// static: identical to td-sh/td-kexec. A diagnostics tool that cannot run when
// the dynamic closure is unreachable is worthless precisely when it is needed,
// so the replacement is a static ET_EXEC with an EMPTY runtime closure — which
// the cargo target-Rust path cannot produce (it only knows the dynamic /td/store
// link). `+crt-static` pulls libc.a/libm.a and `relocation-model=static` yields a
// classic ET_EXEC with no PT_INTERP. The linker is td's native gcc with `-B` at
// glibc's crt objects and binutils' as/ld.
//
// The actual static link needs the full target toolchain (no target rustc in
// the loop sandbox); the sibling td-util-test carries that build+assert check.
//
// The crate root (`main.rs`) declares each sibling module with `mod NAME;`, so a
// single `rustc src/main.rs` pulls them all in — but only if every module file is
// present next to it in {src}. `modules_match_the_mod_lines_in_main_rs` holds the
// two in sync: a module named but not embedded is a build that fails only in the
// heavy recipe leg, minutes after a `cargo test` that stayed green.
//
// Every source below is written out with a WriteFile, which the ladder
// `no_bootstrap_step_invokes_host_find_or_xargs` guard scans as a command
// surface. So the embedded `.rs` must not contain the literal host-tool tokens
// that guard rejects (use `bytes().position`/`rposition` and plain loops) — they
// would trip the host-tool-tier guard even though rustc never interprets the
// file as a shell script. Same constraint td-sh/td-kexec/td-netd document.
const MAIN_RS: &str = include_str!("../../../td-util/src/main.rs");

// (module basename, source text). rustc resolves `mod NAME;` to `{src}/NAME.rs`.
const MODULES: &[(&str, &str)] = &[
    ("cat", include_str!("../../../td-util/src/cat.rs")),
    ("dmesg", include_str!("../../../td-util/src/dmesg.rs")),
    ("fileattr", include_str!("../../../td-util/src/fileattr.rs")),
    ("fileops", include_str!("../../../td-util/src/fileops.rs")),
    ("free", include_str!("../../../td-util/src/free.rs")),
    ("less", include_str!("../../../td-util/src/less.rs")),
    ("printf", include_str!("../../../td-util/src/printf.rs")),
    ("procfs", include_str!("../../../td-util/src/procfs.rs")),
    ("ps", include_str!("../../../td-util/src/ps.rs")),
    ("sleep", include_str!("../../../td-util/src/sleep.rs")),
    ("sys", include_str!("../../../td-util/src/sys.rs")),
    ("term", include_str!("../../../td-util/src/term.rs")),
    ("test", include_str!("../../../td-util/src/test.rs")),
    ("which", include_str!("../../../td-util/src/which.rs")),
];

pub fn recipe() -> Recipe {
    // The self-hosted toolchains install under a nested stage/td/store/<pkg>
    // DESTDIR (re the /td/store prefix); rust-toolchain installs flat.
    let rustc = "{in:rust-toolchain}/bin/rustc";
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let gccbin = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin";
    let bbin = "{in:binutils-x86-64-self}/bin";
    let glib = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64/lib";
    // gcc-x86-64-self folds the unwinder objects INTO libgcc.a and never emits a
    // separate static libgcc_eh.a (it built libgcc PIC/shared for rustc's shared
    // driver). A `-static` rustc link still passes `-lgcc_eh` (prebuilt libstd
    // references `_Unwind_*` even under panic=abort), so ld reds "cannot find
    // -lgcc_eh". Synthesize one from libgcc.a (which DOES define `_Unwind_Resume`
    // et al.) into {root}/eh and add it to the link search path — the standard
    // libgcc.a→libgcc_eh.a workaround for a toolchain missing the split EH archive.
    let objcopy = "{in:binutils-x86-64-self}/bin/objcopy";
    let ranlib = "{in:binutils-x86-64-self}/bin/ranlib";
    let libgcc_a = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/lib/gcc/x86_64-pc-linux-gnu/14.3.0/libgcc.a";

    // Bound so they outlive the argv slice; `&String` deref-coerces to `&str`.
    let linker = format!("-Clinker={gcc}");
    let lib_b = format!("-Clink-arg=-B{glib}");
    let bin_b = format!("-Clink-arg=-B{bbin}");
    let path = format!("{bbin}:{gccbin}");

    let mut steps = Vec::new();
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::WriteFile {
        path: "{src}/main.rs".into(),
        content: MAIN_RS.into(),
        exec: false,
    });
    // Every module `main.rs` declares must sit beside it so `rustc src/main.rs`
    // can resolve `mod NAME;` from the filesystem.
    for (name, source) in MODULES {
        steps.push(Step::WriteFile {
            path: format!("{{src}}/{name}.rs"),
            content: (*source).into(),
            exec: false,
        });
    }
    // Synthesize {root}/eh/libgcc_eh.a = libgcc.a (objcopy preserves the members;
    // ranlib writes the archive index ld needs) so `-lgcc_eh` resolves.
    steps.push(Step::MkDir {
        path: "{root}/eh".into(),
    });
    steps.push(Step::run("{root}", &[objcopy, libgcc_a, "{root}/eh/libgcc_eh.a"]).env("PATH", &path));
    steps.push(Step::run("{root}", &[ranlib, "{root}/eh/libgcc_eh.a"]).env("PATH", &path));
    steps.push(
        Step::run(
            "{src}",
            &[
                rustc,
                "--edition",
                "2021",
                "-C",
                "opt-level=s",
                "--target",
                "x86_64-unknown-linux-gnu",
                "-C",
                "target-feature=+crt-static",
                "-C",
                "relocation-model=static",
                // Mirror the crate's [profile.release] (cargo never sees this
                // direct rustc build): abort — not unwind — on panic, and strip.
                "-C",
                "panic=abort",
                "-C",
                "strip=symbols",
                &linker,
                "-L",
                glib,
                &lib_b,
                &bin_b,
                // The synthesized libgcc_eh.a lives here (see above).
                "-Clink-arg=-L{root}/eh",
                "-Clink-arg=-static-libgcc",
                "--remap-path-prefix",
                "{src}=/td-build",
                "-o",
                "{out}/bin/td-util",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-util".into()],
        exec: true,
    });
    // Fail closed on any interpreter/needed/rpath: a diagnostics tool with a
    // runtime closure is useless exactly when the closure is what broke.
    steps.push(Step::assert_static(&["{out}/bin/td-util"]));

    Recipe::mesboot("td-util", "0.1")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::{MAIN_RS, MODULES};

    /// Every `mod NAME;` in main.rs must have its source embedded here.
    ///
    /// The recipe writes `{src}/NAME.rs` per MODULES entry and rustc resolves the
    /// `mod` line against that directory, so a name declared but not embedded is a
    /// compile error inside the sandbox — surfacing only in the heavy recipe leg,
    /// long after a green `cargo test`. The reverse (an embedded module nothing
    /// declares) is dead source shipped into the build, so both directions red.
    #[test]
    fn modules_match_the_mod_lines_in_main_rs() {
        let mut declared: Vec<&str> = MAIN_RS
            .lines()
            .map(str::trim)
            .filter_map(|l| l.strip_prefix("mod ").and_then(|r| r.strip_suffix(';')))
            .collect();
        declared.sort_unstable();
        // `#[cfg(test)] mod tests { ... }` is a brace form, so the `;` suffix above
        // already excludes it; assert that rather than trusting it.
        for inline in ["tests", "confinement"] {
            assert!(
                !declared.contains(&inline),
                "the inline `{inline}` module is not a file and must not be embedded"
            );
        }
        let mut embedded: Vec<&str> = MODULES.iter().map(|(n, _)| *n).collect();
        embedded.sort_unstable();
        assert_eq!(
            declared, embedded,
            "MODULES and main.rs's `mod` lines disagree — a name in one and not the \
             other only reds when the recipe builds"
        );
        assert!(
            !declared.is_empty(),
            "parsing found no `mod` lines at all, so this test would pass vacuously"
        );
    }

    /// The crate root must DENY, not FORBID, the unsafe lint.
    ///
    /// `less` needs `ioctl(2)`, whose one scoped `#[allow]` a `forbid` cannot host:
    /// reverting this line to `forbid` does not red here, it reds inside the sandbox
    /// minutes into the heavy leg. The SIZE of that surface is held by `main.rs`'s
    /// own `mod confinement` tests; this only holds the door open.
    #[test]
    fn the_embedded_crate_root_denies_rather_than_forbids() {
        let lint = concat!("unsafe_", "code");
        assert!(
            MAIN_RS.contains(&format!("{}![deny({lint})]", "#")),
            "the embedded crate root must deny the unsafe lint"
        );
        assert!(
            !MAIN_RS.contains(&format!("{}![forbid({lint})]", "#")),
            "forbid cannot host the scoped allow sys.rs needs"
        );
    }

    /// Every embedded source IS the file that sits beside `main.rs` on disk.
    ///
    /// `MODULES` pins module NAMES against `main.rs`'s `mod` lines; it says nothing
    /// about where each `include_str!` points. That gap matters now that `sys.rs`
    /// and `term.rs` are the crate's unsafe surface: a path aimed at a file outside
    /// `td-util/src/` would ship bytes that `main.rs`'s `mod confinement` — which
    /// walks `CARGO_MANIFEST_DIR/src` — never reads, so the whole confinement would
    /// be asserting about source the build does not use, with every test green.
    #[test]
    fn every_embedded_source_is_the_file_on_disk() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../td-util/src");
        let read = |name: &str| std::fs::read_to_string(src.join(name)).unwrap_or_default();
        let main = read("main.rs");
        assert!(!main.is_empty(), "td-util/src/main.rs must be readable, or this is vacuous");
        assert_eq!(MAIN_RS, main, "the embedded crate root is not td-util/src/main.rs");
        for (name, source) in MODULES {
            let on_disk = read(&format!("{name}.rs"));
            assert!(!on_disk.is_empty(), "td-util/src/{name}.rs must be readable");
            assert_eq!(
                *source, on_disk,
                "the embedded '{name}' is not td-util/src/{name}.rs; the confinement \
                 tests read the file, the build ships these bytes"
            );
        }
    }
}
