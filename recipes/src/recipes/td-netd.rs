use crate::types::{Recipe, Step};

// td-netd — target-built static network bring-up daemon (link-up + DHCP +
// resolv.conf; resolve/reach).
//
// This recipe compiles the td-netd CRATE's `src/main.rs` (a standalone crate,
// linted and unit-tested there) into a statically-linked target ELF, exactly like
// td-kexec. The crate source is embedded via `include_str!` so the lintable/
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
// PT_INTERP. The linker is td's native gcc with `-B` at glibc's crt objects and
// binutils' as/ld, minus the dynamic-linker/rpath args `assert_static` fails on.
// rustc itself runs off its own $ORIGIN-rpath dylibs and glibc-x86-64's PT_INTERP
// loader (glibc-x86-64 is a declared input for exactly that reason).
//
// The static link needs the full target toolchain, so it is DAILY/operator tier
// (no target rustc in the per-change sandbox); the sibling td-netd-test carries
// that daily build+assert check.
//
// The embedded source is written out with a WriteFile below, which the ladder
// `no_bootstrap_step_invokes_host_find_or_xargs` guard scans as a command surface.
// So the .rs must not contain the literal tokens `find`/`xargs` (it uses plain loops
// instead of `Iterator::find`) — they would trip the host-tool-tier guard even though
// rustc never interprets the file as a shell script.
const MAIN_RS: &str = include_str!("../../../td-netd/src/main.rs");

pub fn recipe() -> Recipe {
    // The self-hosted toolchains install under a nested stage/td/store/<pkg>
    // DESTDIR (re the /td/store prefix); rust-toolchain installs flat.
    let rustc = "{in:rust-toolchain}/bin/rustc";
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let gccbin = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin";
    let bbin = "{in:binutils-x86-64-self}/bin";
    let glib = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64/lib";
    // gcc-x86-64-self folds the unwinder objects INTO libgcc.a and never emits a
    // separate static libgcc_eh.a. A `-static` rustc link still passes `-lgcc_eh`
    // (prebuilt libstd references `_Unwind_*` even under panic=abort), so ld reds
    // "cannot find -lgcc_eh". Synthesize one from libgcc.a (which DOES define
    // `_Unwind_Resume` et al.) into {root}/eh and add it to the link search path —
    // the standard libgcc.a→libgcc_eh.a workaround, identical to td-kexec.
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
                // direct rustc build): abort — not unwind — on panic so the
                // daemon carries no unwinder, and strip symbols.
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
                "{out}/bin/td-netd",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-netd".into()],
        exec: true,
    });
    // Fail closed on any interpreter/needed/rpath: the daemon must be a
    // self-contained static ELF with an empty runtime closure (NSS-free).
    steps.push(Step::assert_static(&["{out}/bin/td-netd"]));

    Recipe::mesboot("td-netd", "0.1")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(steps)
}
