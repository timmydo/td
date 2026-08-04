use crate::types::{Recipe, Step};

// td-sh — target-built static POSIX /bin/sh (busybox-`sh` replacement).
//
// This recipe compiles the td-sh CRATE's binary source (`src/main.rs` plus its
// sibling modules) into a statically-linked target ELF. The sources are embedded
// via `include_str!` so the lintable/testable crate and the shipped binary are
// ONE source of truth and cannot drift; the path escapes the
// `recipes/src/recipes/*.rs` catalog glob, so it is not itself a recipe module.
//
// STATUS: the interpreter passes the seed Oils conformance corpus (resolved to
// the dash/ash goldens; see td-sh/tests/conformance.rs) — a lexer/parser, word
// expansion (parameter/command/arithmetic substitution, field splitting,
// pathname expansion), control flow, functions and the core builtins. The crate
// root `#![deny(unsafe_code)]`s and `sys.rs` holds the one scoped exception --
// `umask(2)`, the syscall `std` exposes no API for. td-sh is NOT yet referenced by
// system-x86-64: the shipped `/bin/sh` stays busybox until td-sh also clears the
// busybox `ash_test` parity gate and the bulk Oils import. Only then is the image
// symlink flipped.
//
// Why mesboot-style (rustc invoked directly) rather than `Recipe::rust`, and why
// static: identical to td-kexec. The busybox `sh` it replaces is boot-critical and
// runs in stage-1 init BEFORE the dynamic uutils glibc closure is reachable, so
// the replacement must be a static ET_EXEC with an EMPTY runtime closure (no
// PT_INTERP, no DT_NEEDED) — which the cargo target-Rust path cannot produce (it
// only knows the dynamic /td/store link). `+crt-static` pulls libc.a/libm.a and
// `relocation-model=static` yields a classic ET_EXEC with no PT_INTERP. The linker
// is td's native gcc with `-B` at glibc's crt objects and binutils' as/ld.
//
// The actual static link needs the full target toolchain (no target rustc in
// the loop sandbox); the sibling td-sh-test carries that build+assert check.
//
// The crate root (`main.rs`) declares each sibling module with `mod NAME;`, so a
// single `rustc src/main.rs` pulls them all in — but only if every module file is
// present next to it in {src}. Keep MODULES in sync with `main.rs`'s `mod` lines.
//
// Every source below is written out with a WriteFile, which the ladder
// `no_bootstrap_step_invokes_host_find_or_xargs` guard scans as a command surface.
// So the embedded `.rs` must not contain the literal tokens `find`/`xargs` (use a
// plain loop / `bytes().position` over `Iterator::find`/`str::find`) — they would
// trip the host-tool-tier guard even though rustc never interprets the file as a
// shell script. Same constraint td-kexec/td-netd document.
const MAIN_RS: &str = include_str!("../../../td-sh/src/main.rs");

// (module basename, source text). rustc resolves `mod NAME;` to `{src}/NAME.rs`.
const MODULES: &[(&str, &str)] = &[
    ("arith", include_str!("../../../td-sh/src/arith.rs")),
    ("ast", include_str!("../../../td-sh/src/ast.rs")),
    ("builtin", include_str!("../../../td-sh/src/builtin.rs")),
    ("exec", include_str!("../../../td-sh/src/exec.rs")),
    ("expand", include_str!("../../../td-sh/src/expand.rs")),
    ("lexer", include_str!("../../../td-sh/src/lexer.rs")),
    ("parser", include_str!("../../../td-sh/src/parser.rs")),
    ("pattern", include_str!("../../../td-sh/src/pattern.rs")),
    ("process", include_str!("../../../td-sh/src/process.rs")),
    ("random", include_str!("../../../td-sh/src/random.rs")),
    ("sys", include_str!("../../../td-sh/src/sys.rs")),
];

#[cfg(test)]
mod tests {
    use super::{MAIN_RS, MODULES};

    /// A module `main.rs` declares but MODULES omits is never written into
    /// {src}, so `rustc src/main.rs` fails with E0583 -- inside the sandbox,
    /// twenty minutes into a recipe check. The two lists are one fact written
    /// twice. td-init and td-util already assert this; td-sh did not.
    #[test]
    fn modules_match_the_mod_lines_in_main_rs() {
        let mut declared: Vec<&str> = MAIN_RS
            .lines()
            .map(str::trim)
            .filter_map(|l| l.strip_prefix("mod ").and_then(|r| r.strip_suffix(';')))
            .collect();
        declared.sort_unstable();
        // An inline `mod tests { ... }` is a brace form, so the `;` suffix above
        // already excludes it; assert that rather than trusting it.
        assert!(
            !declared.contains(&"tests"),
            "`mod tests` is inline, not a file, and must not be embedded"
        );
        let mut embedded: Vec<&str> = MODULES.iter().map(|(n, _)| *n).collect();
        embedded.sort_unstable();
        assert_eq!(
            declared, embedded,
            "MODULES and main.rs's `mod` lines disagree -- a name in one and not \
             the other is an E0583 only the recipe check would catch"
        );
        assert!(!declared.is_empty(), "no `mod` lines parsed out of main.rs");
    }
}

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
                "{out}/bin/td-sh",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-sh".into()],
        exec: true,
    });
    // Fail closed on any interpreter/needed/rpath: the shipped shell must be a
    // self-contained static ELF with an empty runtime closure (boot-critical sh).
    steps.push(Step::assert_static(&["{out}/bin/td-sh"]));

    Recipe::mesboot("td-sh", "0.1")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(steps)
}
