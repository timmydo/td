use crate::types::{Recipe, Step};

// td-svc — target-built static service supervisor.
//
// This recipe compiles the td-svc CRATE's binary source (`src/main.rs` plus its
// sibling modules) into a statically-linked target ELF. The sources are embedded
// via `include_str!` so the lintable/testable crate and the shipped binary are
// ONE source of truth and cannot drift; the path escapes the
// `recipes/src/recipes/*.rs` catalog glob, so it is not itself a recipe module.
//
// SCOPE: service supervision moved OFF PID 1 — dependency ordering, restart
// backoff, readiness probing, and (in later landings) log capture, an ordered
// shutdown, and Ctrl-Alt-Del. `td-svc/DESIGN.md` is the normative specification.
//
// td-svc is NOT a target-side unsafe exception, and that is the point: it
// `#![forbid(unsafe_code)]`s. Everything it needs is reachable through safe
// `std` — `Command`/`Child` for processes, `CommandExt::process_group` for
// groups, `/proc` reads for liveness and membership, `UnixListener` for control,
// and two `/proc/sys/kernel` writes for Ctrl-Alt-Del. Signalling shells out to
// the uutils `/bin/kill` rather than taking a `kill(2)` surface. DESIGN.md §4
// records the route for each, so a future edit that reaches for `unsafe` has to
// argue against a written answer rather than an absence.
//
// Why mesboot-style (rustc invoked directly) rather than `Recipe::rust`, and why
// static: identical to td-util/td-sh/td-kexec. A supervisor that cannot run when
// the dynamic closure is unreachable is worthless precisely when it is needed —
// it is PID 1's only child on a booted machine — so the binary is a static
// ET_EXEC with an EMPTY runtime closure, which the cargo target-Rust path cannot
// produce. `+crt-static` pulls libc.a/libm.a and `relocation-model=static`
// yields a classic ET_EXEC with no PT_INTERP.
//
// The actual static link needs the full target toolchain (no target rustc in
// the loop sandbox); the sibling td-svc-test carries that build+assert check.
//
// The crate root (`main.rs`) declares each sibling module with `mod NAME;`, so a
// single `rustc src/main.rs` pulls them all in — but only if every module file is
// present next to it in {src}. Keep MODULES in sync with `main.rs`'s `mod` lines.
//
// Every source below is written out with a WriteFile, which the ladder
// `no_bootstrap_step_invokes_host_find_or_xargs` guard scans as a command
// surface. So the embedded `.rs` must not contain the literal host-tool tokens
// that guard rejects — note `Runtime::lookup` and the plain Kahn loop in
// order.rs, both written that way instead of the obvious iterator search, and
// the tests' `match_indices` instead of a bare string search. Same constraint
// td-util/td-sh/td-kexec/td-netd document.
const MAIN_RS: &str = include_str!("../../../td-svc/src/main.rs");

// (module basename, source text). rustc resolves `mod NAME;` to `{src}/NAME.rs`.
const MODULES: &[(&str, &str)] = &[
    ("backoff", include_str!("../../../td-svc/src/backoff.rs")),
    ("order", include_str!("../../../td-svc/src/order.rs")),
    ("procfs", include_str!("../../../td-svc/src/procfs.rs")),
    ("supervise", include_str!("../../../td-svc/src/supervise.rs")),
    ("table", include_str!("../../../td-svc/src/table.rs")),
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
    // separate static libgcc_eh.a. A `-static` rustc link still passes `-lgcc_eh`,
    // so synthesize one from libgcc.a — the same workaround td-util documents.
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
                "-Clink-arg=-L{root}/eh",
                "-Clink-arg=-static-libgcc",
                "--remap-path-prefix",
                "{src}=/td-build",
                "-o",
                "{out}/bin/td-svc",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-svc".into()],
        exec: true,
    });
    // Fail closed on any interpreter/needed/rpath: PID 1's only child must come
    // up before, and independently of, any dynamic closure.
    steps.push(Step::assert_static(&["{out}/bin/td-svc"]));

    Recipe::mesboot("td-svc", "0.1")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(steps)
}
