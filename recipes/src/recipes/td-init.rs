use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

// td-init — target-built static multicall for td's boot glue.
//
// This recipe compiles the td-init CRATE's binary source (`src/main.rs` plus its
// sibling modules) into a statically-linked target ELF. The sources are embedded
// via `include_str!` so the lintable/testable crate and the shipped binary are
// ONE source of truth and cannot drift; the path escapes the
// `recipes/src/recipes/*.rs` catalog glob, so it is not itself a recipe module.
//
// SCOPE: the busybox applets that need a RAW SYSCALL — `init` (wait4), `reboot`/
// `poweroff`/`halt` (reboot), `sync` (sync), `switch_root` (mount MS_MOVE +
// chroot), `mount`/`umount` (mount + umount2), `cttyhack` (setsid + TIOCSCTTY),
// `getty` (both of those plus TCGETS/TCSETS — the last busybox name on the image),
// `losetup` (ioctl LOOP_SET_FD), `mknod` (mknod), `devpts` (no syscall of its own
// — it mounts through the `mount` applet), and `hostname` (sethostname,
// the `-F` flag uutils lacks). That is the complement of td-util,
// which covers the applets safe `std` already reaches and is
// `#![forbid(unsafe_code)]` as a result. The crate confines its `unsafe` to one
// `syscall5` body under a scoped `#[allow]` beneath a crate-level `deny` — the
// THIRD target-side unsafe exception recorded in UNSAFE.md, after td-kexec and
// td-netd.
//
// system-x86-64 SHIPS this: /init (PID 1), both initramfses' mount pair, the
// deployment initramfs' switch_root/losetup/mknod trio, and a /bin farm.
// Unlike the td-util cutover, most of these applets cannot be probed from the
// greeter — running `reboot` successfully ends the boot, `mount` mutates the
// running system, and `init`/`switch_root` have already done their work by the
// time a greeter exists — so the evidence is layered instead: system-x86-64's
// shape check EXECUTES this binary at BUILD time to dry-run the image's own
// inittab and drive switch_root's fail-early refusal, the greeter probes the rest
// (the irreversible ones through their refusal paths), and the boot oracle
// exercises the success paths by booting three times — every filesystem on the
// machine is now mounted by this binary.
//
// Why mesboot-style (rustc invoked directly) rather than `Recipe::rust`, and why
// static: identical to td-sh/td-util/td-kexec. Every applet here runs where the
// dynamic closure is not reachable — switch_root and init run in the initramfs
// before the real root is mounted at all — so the binary must be a static ET_EXEC
// with an EMPTY runtime closure, which the cargo target-Rust path cannot produce
// (it only knows the dynamic /td/store link). `+crt-static` pulls libc.a/libm.a
// and `relocation-model=static` yields a classic ET_EXEC with no PT_INTERP. The
// linker is td's native gcc with `-B` at glibc's crt objects and binutils' as/ld.
//
// The actual static link needs the full target toolchain (no target rustc in
// the loop sandbox); the sibling td-init-test carries that build+assert check.
//
// The crate root (`main.rs`) declares each sibling module with `mod NAME;`, so a
// single `rustc src/main.rs` pulls them all in — but only if every module file is
// present next to it in {src}. Keep MODULES in sync with `main.rs`'s `mod` lines.
//
// Every source below is written out with a WriteFile, which the ladder
// `no_bootstrap_step_invokes_host_find_or_xargs` guard scans as a command
// surface. A `.rs` body is read only INSIDE its string literals, so an
// identifier like `Iterator::find` is free; what must not appear is a bare
// `find`/`xargs` in a LITERAL, which reads exactly as a command name would.
// That guard's roster exempts named reviewed bodies from even that, and none
// of td-init's is on it.
const MAIN_RS: &str = include_str!("../../../td-init/src/main.rs");

// (module basename, source text). rustc resolves `mod NAME;` to `{src}/NAME.rs`.
const MODULES: &[(&str, &str)] = &[
    ("cttyhack", include_str!("../../../td-init/src/cttyhack.rs")),
    ("devpts", include_str!("../../../td-init/src/devpts.rs")),
    ("devt", include_str!("../../../td-init/src/devt.rs")),
    ("getty", include_str!("../../../td-init/src/getty.rs")),
    ("halt", include_str!("../../../td-init/src/halt.rs")),
    ("hostname", include_str!("../../../td-init/src/hostname.rs")),
    ("init", include_str!("../../../td-init/src/init.rs")),
    ("losetup", include_str!("../../../td-init/src/losetup.rs")),
    ("mknod", include_str!("../../../td-init/src/mknod.rs")),
    ("mount", include_str!("../../../td-init/src/mount.rs")),
    ("switchroot", include_str!("../../../td-init/src/switchroot.rs")),
    ("syncfs", include_str!("../../../td-init/src/syncfs.rs")),
    ("sys", include_str!("../../../td-init/src/sys.rs")),
    ("term", include_str!("../../../td-init/src/term.rs")),
];

/// The embedded source of one applet module. Lets a consumer that hard-codes a diagnostic
/// td-init emits — system-x86-64's refusal probes — pin that string to the source it came
/// from, instead of the two drifting apart until a boot oracle nobody ran notices.
#[cfg(test)]
pub(crate) fn module_source(name: &str) -> Option<&'static str> {
    MODULES.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
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
                &linker,
                "-L",
                glib,
                &lib_b,
                &bin_b,
                // The synthesized libgcc_eh.a lives here (see above).
                "-Clink-arg=-L{root}/eh",
                "-Clink-arg=-static-libgcc",
                "-o",
                "{out}/bin/td-init",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-init".into()],
        exec: true,
    });
    // Fail closed on any interpreter/needed/rpath: PID 1 and switch_root run
    // before the dynamic closure exists, so a runtime dependency here is a
    // kernel panic, not a degraded boot.
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-init"]));

    Recipe::mesboot("td-init", "0.1")
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
    /// Same guard as td-util's, added here for the same reason.
    #[test]
    fn modules_match_the_mod_lines_in_main_rs() {
        let mut declared: Vec<&str> = MAIN_RS
            .lines()
            .map(str::trim)
            .filter_map(|l| l.strip_prefix("mod ").and_then(|r| r.strip_suffix(';')))
            .collect();
        declared.sort_unstable();
        // `#[cfg(test)] mod tests { ... }` and `mod confinement { ... }` are brace
        // forms, so the `;` suffix above already excludes them; assert that rather
        // than trusting it.
        for inline in ["tests", "confinement"] {
            assert!(
                !declared.contains(&inline),
                "`mod {inline}` is inline, not a file, and must not be embedded"
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
}
