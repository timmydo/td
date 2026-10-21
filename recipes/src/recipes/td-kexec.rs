use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

// td-kexec — target-built static guest kexec helper (Phase-0 kexec spike).
//
// This recipe compiles the td-kexec CRATE's `src/main.rs` (a standalone crate,
// linted and unit-tested there) into a statically-linked target ELF. The crate
// source is embedded via `include_str!` so the lintable/testable crate and the
// shipped binary are ONE source of truth and cannot drift; the path escapes the
// `recipes/src/recipes/*.rs` catalog glob, so it is not itself a recipe module.
//
// Why mesboot-style (rustc invoked directly) rather than `Recipe::rust`:
//   * the source is td-owned and embedded, not a fetched crate with a Cargo
//     manifest/lock, so there is nothing for the cargo build path to resolve;
//   * the target-Rust build path has no static-link mode — it only knows the
//     dynamic /td/store link. A static ELF is required so the guest helper has
//     an EMPTY runtime closure (no PT_INTERP, no DT_NEEDED): it runs inside a
//     minimal kexec initramfs that carries no dynamic loader and no libc.so.
//
// The static flags mirror the control-plane static build: `+crt-static` pulls
// libc.a/libm.a, and `relocation-model=static` yields a classic ET_EXEC with no
// PT_INTERP (dodging the static-PIE startup path). The linker is td-link, which
// takes td glibc's static start files for that shape and writes no PT_INTERP or
// RUNPATH for `assert_static` to fail-close on.
//
// The actual static link needs the full target toolchain (no target rustc in
// the loop sandbox); the sibling td-kexec-test carries that build+assert check.
//
// The volume-key handoff (`--fds-key`) compiles three shared sources by
// `#[path]`: td-boot's protocol (the archive member and key length), and the
// engine's SHA-256 and newc writer. Each is staged where the relative include
// resolves, beside `td-kexec/src/main.rs`.
const MAIN_RS: &str = include_str!("../../../td-kexec/src/main.rs");
const PROTOCOL_RS: &str = include_str!("../../../td-boot/src/protocol.rs");
const SHA256_RS: &str = include_str!("../../../engine/src/sha256.rs");
const CPIO_RS: &str = include_str!("../../../engine/src/cpio.rs");

pub fn recipe() -> Recipe {
    let rustc = "{in:rust-toolchain}/bin/rustc";

    let mut steps = Vec::new();
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    for dir in [
        "{src}/td-kexec/src",
        "{src}/td-boot/src",
        "{src}/engine/src",
    ] {
        steps.push(Step::MkDir { path: dir.into() });
    }
    for (path, content) in [
        ("{src}/td-kexec/src/main.rs", MAIN_RS),
        ("{src}/td-boot/src/protocol.rs", PROTOCOL_RS),
        ("{src}/engine/src/sha256.rs", SHA256_RS),
        ("{src}/engine/src/cpio.rs", CPIO_RS),
    ] {
        steps.push(Step::WriteFile {
            path: path.into(),
            content: content.into(),
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
                // direct rustc build): abort — not unwind — on panic so the
                // confined guest helper carries no unwinder. The shared target
                // policy deliberately preserves symbols.
                "-C",
                "panic=abort",
                "-o",
                "{out}/bin/td-kexec",
                "{src}/td-kexec/src/main.rs",
            ],
        )
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-kexec".into()],
        exec: true,
    });
    // Fail closed on any interpreter/needed/rpath: the guest binary must be a
    // self-contained static ELF with an empty runtime closure.
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-kexec"]));

    Recipe::mesboot("td-kexec", "0.1")
        .native_inputs(&[
            "rust-toolchain",
            "td-link",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(steps)
}
