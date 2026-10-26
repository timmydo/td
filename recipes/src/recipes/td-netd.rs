use crate::shared::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

// td-netd — target-built static network bring-up daemon (link-up + DHCP +
// resolv.conf; resolve/reach).
//
// This recipe compiles the td-netd CRATE's `src/main.rs` with its modules and
// td-fido's AES (shared by path; td-fido/DESIGN.md) into a statically-linked
// target ELF, exactly like td-kexec. The crate source is embedded via
// `include_str!` so the lintable/
// testable crate and the shipped binary are ONE source of truth and cannot drift;
// the path escapes the `recipes/src/recipes/*.rs` catalog glob, so it is not
// itself a recipe module.
//
// Static (not the dynamic /td/store link `Recipe::rust` emits) because a fully
// static ELF is the only way to guarantee td-netd never touches glibc's NSS
// (getaddrinfo/gethostbyname), which static glibc resolves by dlopen at runtime:
// td-netd does its own DNS and connects to numeric SocketAddrs, so there is no NSS
// call to fail. The static shape also lets the daemon run in a minimal /run or
// initramfs context with an empty runtime closure (no PT_INTERP, no DT_NEEDED).
//
// The static flags mirror td-kexec / the control-plane static build: `+crt-static`
// pulls libc.a/libm.a, `relocation-model=static` yields a classic ET_EXEC with no
// PT_INTERP. The linker is td-link, which writes no interpreter or run path
// into a static link for `assert_static` to fail on.
//
// The static link needs the full target toolchain (no target rustc in the loop
// sandbox); the sibling td-netd-test carries that build+assert check.
//
// The embedded source is written out with a WriteFile below, which the ladder
// `no_bootstrap_step_invokes_host_find_or_xargs` guard scans as a command surface.
// A `.rs` body is read only INSIDE its string literals, so `Iterator::find` as an
// identifier is free; what must not appear is a bare `find`/`xargs` in a LITERAL,
// which reads exactly as a command name would. That guard's roster exempts named
// reviewed bodies from even that, and none of td-netd's is on it.
const SOURCES: &[(&str, &str)] = &[
    (
        "{src}/td-netd/src/main.rs",
        include_str!("../../../td-netd/src/main.rs"),
    ),
    (
        "{src}/td-netd/src/genl.rs",
        include_str!("../../../td-netd/src/genl.rs"),
    ),
    (
        "{src}/td-netd/src/sha1.rs",
        include_str!("../../../td-netd/src/sha1.rs"),
    ),
    (
        "{src}/td-netd/src/sys.rs",
        include_str!("../../../td-netd/src/sys.rs"),
    ),
    (
        "{src}/td-netd/src/wifi.rs",
        include_str!("../../../td-netd/src/wifi.rs"),
    ),
    (
        "{src}/td-netd/src/wpa.rs",
        include_str!("../../../td-netd/src/wpa.rs"),
    ),
    (
        "{src}/td-fido/src/fido_aes.rs",
        include_str!("../../../td-fido/src/fido_aes.rs"),
    ),
];

pub fn recipe() -> Recipe {
    let rustc = "{in:rust-toolchain}/bin/rustc";

    let mut steps = Vec::new();
    for dir in ["{out}/bin", "{src}/td-netd/src", "{src}/td-fido/src"] {
        steps.push(Step::MkDir { path: dir.into() });
    }
    for (path, content) in SOURCES {
        steps.push(Step::WriteFile {
            path: (*path).into(),
            content: (*content).into(),
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
                // daemon carries no unwinder. The shared target policy
                // deliberately preserves symbols.
                "-C",
                "panic=abort",
                "-o",
                "{out}/bin/td-netd",
                "{src}/td-netd/src/main.rs",
            ],
        )
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-netd".into()],
        exec: true,
    });
    // Fail closed on any interpreter/needed/rpath: the daemon must be a
    // self-contained static ELF with an empty runtime closure (NSS-free).
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-netd"]));

    Recipe::mesboot("td-netd", "0.1")
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .steps(steps)
}
