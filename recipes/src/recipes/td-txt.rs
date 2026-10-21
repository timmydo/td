use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

// td-txt — target-built static text userland (busybox `grep`/`sed` replacement).
//
// This recipe compiles the td-txt CRATE's binary source (`src/main.rs` plus its
// sibling modules) into a statically-linked target ELF. The sources are embedded
// via `include_str!` so the lintable/testable crate and the shipped binary are
// ONE source of truth and cannot drift; the path escapes the
// `recipes/src/recipes/*.rs` catalog glob, so it is not itself a recipe module.
//
// STATUS: shipped. `system-x86-64` points `/bin/grep` and `/bin/sed` at this
// multicall through `TD_TXT_APPLETS` (the symlink farm is built there), and no
// busybox serves either any more. That is enforced, not merely true: the image
// build REFUSES a packed `/bin/busybox` or a staged busybox package outright,
// so the multiplexer cannot come back by drift.
//
// The conformance corpus is the vendored GNU grep regex suites, the GNU sed
// testsuite triples, and td-txt's own option-surface cases; all three are
// driven by td-txt/tests/conformance.rs. It is NOT a clean sweep, and the
// phrasing here matters because this comment was wrong once:
// td-txt/spec/expectations.txt carries the cases td-txt still answers
// differently, generated rather than curated, and a listed case that starts
// passing reds the gate until the entry is promoted.
//
// Why mesboot-style (rustc invoked directly) rather than `Recipe::rust`, and why
// static: identical to td-sh/td-util. `grep` and `sed` are reached from scripts
// that run before — or instead of — the dynamic uutils glibc closure, so the
// replacement is a static ET_EXEC with an EMPTY runtime closure (no PT_INTERP, no
// DT_NEEDED), which the cargo target-Rust path cannot produce (it only knows the
// dynamic /td/store link). `+crt-static` pulls libc.a/libm.a and
// `relocation-model=static` yields a classic ET_EXEC with no PT_INTERP. The linker
// is td-link.
//
// The actual static link needs the full target toolchain (no target rustc in
// the loop sandbox); the sibling td-txt-test carries that build+assert check.
//
// The crate root (`main.rs`) declares each sibling module with `mod NAME;`, so a
// single `rustc td-txt/src/main.rs` pulls them all in — but only if every module
// file is present where rustc resolves it. Keep MODULES in sync with `main.rs`'s
// `mod` lines. `regex` is the td-regex crate's one file, which `main.rs` names by
// `#[path = "../../td-regex/src/lib.rs"]`, so the sources are staged in the
// checkout's own layout under {src}, as td-login stages td-authd's file.
//
// Every source below is written out with a WriteFile, which the ladder
// `no_bootstrap_step_invokes_host_find_or_xargs` guard scans as a command surface.
// A `.rs` body is read only INSIDE its string literals, so `Iterator::find` as an
// identifier is free; a bare `find`/`xargs` in a LITERAL is not. `sed.rs` alone is
// on that guard's roster, for the one case nothing can be spelled around — GNU's
// `can't find label' diagnostic, which must match byte for byte. Another module
// needing such a literal is a REVIEWED roster entry there, not an edit here, and
// naming `Command` in any of them reds that guard's tripwire test. Keep the
// premise true. td-sh and td-netd document the constraint as it stands for them.
const MAIN_RS: &str = include_str!("../../../td-txt/src/main.rs");

// (staged path, source text): each module where rustc resolves it from
// `{src}/td-txt/src/main.rs` — `mod NAME;` beside it, `regex` at its `#[path]`.
const MODULES: &[(&str, &str)] = &[
    (
        "{src}/td-txt/src/grep.rs",
        include_str!("../../../td-txt/src/grep.rs"),
    ),
    (
        "{src}/td-regex/src/lib.rs",
        include_str!("../../../td-regex/src/lib.rs"),
    ),
    (
        "{src}/td-txt/src/sed.rs",
        include_str!("../../../td-txt/src/sed.rs"),
    ),
    (
        "{src}/td-txt/src/util.rs",
        include_str!("../../../td-txt/src/util.rs"),
    ),
];

pub fn recipe() -> Recipe {
    let rustc = "{in:rust-toolchain}/bin/rustc";

    let mut steps = Vec::new();
    for directory in ["{src}/td-txt/src", "{src}/td-regex/src"] {
        steps.push(Step::MkDir {
            path: directory.into(),
        });
    }
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::WriteFile {
        path: "{src}/td-txt/src/main.rs".into(),
        content: MAIN_RS.into(),
        exec: false,
    });
    for (path, source) in MODULES {
        steps.push(Step::WriteFile {
            path: (*path).into(),
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
                "{out}/bin/td-txt",
                "{src}/td-txt/src/main.rs",
            ],
        )
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-txt".into()],
        exec: true,
    });
    // Fail closed on any interpreter/needed/rpath: the shipped text tools must be
    // a self-contained static ELF with an empty runtime closure.
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-txt"]));

    Recipe::mesboot("td-txt", "0.1")
        .native_inputs(&[
            "rust-toolchain",
            "td-link",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(steps)
}
