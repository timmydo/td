use crate::types::{Recipe, Step};

// td-util — target-built static multicall for td's diagnostics userland.
//
// This recipe compiles the td-util CRATE's binary source (`src/main.rs` plus its
// sibling modules) into a statically-linked target ELF. The sources are embedded
// via `include_str!` so the lintable/testable crate and the shipped binary are
// ONE source of truth and cannot drift; the path escapes the
// `recipes/src/recipes/*.rs` catalog glob, so it is not itself a recipe module.
//
// SCOPE: the applets that must run where uutils' coreutils cannot, and that need no
// syscall surface beyond safe `std`. Two kinds. `clear`, `which`, `free`, `ps` and
// `dmesg` are names uutils does not provide at all, and get `/bin` symlinks.
// `cat`, `chmod`, `chown`, `ln`, `mkdir`, `printf`, `readlink`, `rm` and `sleep` it
// DOES provide — dynamically linked, which the pre-pivot initramfs has no loader for
// and which `/etc/rootcheck` must not depend on, since reporting a broken runtime
// closure is its job. Those nine carry no `/bin` name here (uutils owns them and the
// farms are disjoint) and are reached as `td-util <applet>`.
// `free`/`ps` read /proc and `dmesg` reads /dev/kmsg O_NONBLOCK, all ordinary
// file I/O, so the crate stays `#![deny(unsafe_code)]` and adds NO target-side
// unsafe exception to AGENTS.md. The applets that would (reboot/poweroff/halt,
// switch_root, cttyhack, init, and `sync`, which is td-init's) are deliberately out
// of scope: each needs a raw syscall, which is a reviewed unsafe-surface amendment,
// not a drive-by.
//
// system-x86-64 packs td-util into the real root AND both initramfs cpios, routes
// /bin/{clear,which,free,ps,dmesg} here, and calls the other nine as `td-util <applet>`
// where it used to call `busybox <applet>`. The greeter runs each by its /bin path and emits TD_UTIL_RUNTIME_MARKER
// only if all five exit 0, so `td-recipe-eval qemu-boot-system` re-proves the whole
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
// present next to it in {src}. Keep MODULES in sync with `main.rs`'s `mod` lines.
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
    ("printf", include_str!("../../../td-util/src/printf.rs")),
    ("procfs", include_str!("../../../td-util/src/procfs.rs")),
    ("ps", include_str!("../../../td-util/src/ps.rs")),
    ("sleep", include_str!("../../../td-util/src/sleep.rs")),
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
