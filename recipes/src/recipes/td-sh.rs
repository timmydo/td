use crate::shared::{split_target_debug, target_rustc};
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
// `umask(2)`, a disposition-only `rt_sigaction(2)`, and an `ioctl(2)` restricted
// to three value-pinned terminal requests: the calls `std` exposes no API for.
// td-sh IS the shipped `/bin/sh` and `/bin/ash`: system-x86-64 packs this output
// into the real root and into BOTH initramfs images, where it replaced the
// busybox multicall outright rather than beside it. Every generated script --
// both `/init`s and every `/etc` script -- is interpreted by this binary.
//
// Why mesboot-style (rustc invoked directly) rather than `Recipe::rust`, and why
// static: identical to td-kexec. The busybox `sh` it replaces is boot-critical and
// runs in stage-1 init BEFORE the dynamic uutils glibc closure is reachable, so
// the replacement must be a static ET_EXEC with an EMPTY runtime closure (no
// PT_INTERP, no DT_NEEDED) — which the cargo target-Rust path cannot produce (it
// only knows the dynamic /td/store link). `+crt-static` pulls libc.a/libm.a and
// `relocation-model=static` yields a classic ET_EXEC with no PT_INTERP. The linker
// is td-link.
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
// A `.rs` body is read only INSIDE its string literals, so `Iterator::find` as an
// identifier is free; what must not appear is a bare `find`/`xargs` in a LITERAL,
// which reads exactly as a command name would. That guard's roster exempts named
// reviewed bodies from even that, and none of td-sh's is on it — td-sh names
// `Command`, which is the thing that roster's tripwire test watches for.
const MAIN_RS: &str = include_str!("../../../td-sh/src/main.rs");

// (module basename, source text). rustc resolves `mod NAME;` to `{src}/NAME.rs`.
const MODULES: &[(&str, &str)] = &[
    ("arith", include_str!("../../../td-sh/src/arith.rs")),
    ("ast", include_str!("../../../td-sh/src/ast.rs")),
    ("builtin", include_str!("../../../td-sh/src/builtin.rs")),
    ("complete", include_str!("../../../td-sh/src/complete.rs")),
    ("exec", include_str!("../../../td-sh/src/exec.rs")),
    ("expand", include_str!("../../../td-sh/src/expand.rs")),
    ("funcs", include_str!("../../../td-sh/src/funcs.rs")),
    ("jobs", include_str!("../../../td-sh/src/jobs.rs")),
    ("lexer", include_str!("../../../td-sh/src/lexer.rs")),
    ("line", include_str!("../../../td-sh/src/line.rs")),
    ("parser", include_str!("../../../td-sh/src/parser.rs")),
    ("pattern", include_str!("../../../td-sh/src/pattern.rs")),
    ("process", include_str!("../../../td-sh/src/process.rs")),
    ("random", include_str!("../../../td-sh/src/random.rs")),
    ("regex", include_str!("../../../td-sh/src/regex.rs")),
    ("sys", include_str!("../../../td-sh/src/sys.rs")),
    ("term", include_str!("../../../td-sh/src/term.rs")),
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
    let rustc = "{in:rust-toolchain}/bin/rustc";

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
    steps.push(
        target_rustc(
            "{src}",
            rustc,
            &[
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
                // direct rustc build): abort — not unwind — on panic. The
                // shared target policy deliberately preserves symbols.
                "-C",
                "panic=abort",
                "-o",
                "{out}/bin/td-sh",
                "{src}/main.rs",
            ],
        )
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-sh".into()],
        exec: true,
    });
    // Fail closed on any interpreter/needed/rpath: the shipped shell must be a
    // self-contained static ELF with an empty runtime closure (boot-critical sh).
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-sh"]));

    Recipe::mesboot("td-sh", "0.1")
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .steps(steps)
}
