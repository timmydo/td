use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

// td-firstboot — target-built static binary that mints td's PER-MACHINE identity.
//
// The image is a read-only erofs root that boots any number of machines, so a
// machine-id or SSH host key baked into it would be shared by all of them. Those
// files live on the persistent /var subvolume, /etc reaches each through one
// reviewed symlink (MUTABLE_ETC in system-x86-64.rs), and this program creates them
// on a machine's first boot. Per-file symlinks rather than an /etc overlay: the
// overlay would retire SYSTEM_ETC_RO_MARKER for the whole directory.
//
// SCOPE: the /var side only. It carries no crypto — OpenSSH `ssh-keygen` mints the
// Ed25519 key — so the crate is pure safe std needing no syscall surface (its entropy is
// /dev/random read as an ordinary file), which keeps it `#![forbid(unsafe_code)]`
// and adds NO target-side unsafe exception to UNSAFE.md.
//
// Why mesboot-style (rustc invoked directly) and static, as for td-util/td-sh: this
// runs at sysinit on a machine whose identity does not exist yet, and a tool that
// needs a dynamic closure to provision the system cannot report why the system has
// none. `+crt-static` pulls libc.a/libm.a and `relocation-model=static` yields a
// classic ET_EXEC with no PT_INTERP; the linker is td's native gcc with `-B` at
// glibc's crt objects and binutils' as/ld. The link needs the full target
// toolchain, so the sibling td-firstboot-test carries it.
//
// The crate root declares each sibling module with `mod NAME;`, so a single
// `rustc src/main.rs` pulls them all in — but only if every module file sits beside
// it in {src}. Keep MODULES in sync with `main.rs`'s `mod` lines.
//
// Every source below is written out with a WriteFile, which the ladder
// `no_bootstrap_step_invokes_host_find_or_xargs` guard scans as a command
// surface. A `.rs` body is read only INSIDE its string literals, so an
// identifier like `Iterator::find` is free; what must not appear is a bare
// `find`/`xargs` in a LITERAL, which reads exactly as a command name would.
// That guard's roster exempts named reviewed bodies from even that, and none
// of td-firstboot's is on it.
pub(crate) const MAIN_RS: &str = include_str!("../../../td-firstboot/src/main.rs");

// (module basename, source text). rustc resolves `mod NAME;` to `{src}/NAME.rs`.
const MODULES: &[(&str, &str)] = &[
    (
        "machineid",
        include_str!("../../../td-firstboot/src/machineid.rs"),
    ),
    ("mounts", include_str!("../../../td-firstboot/src/mounts.rs")),
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
    // separate static libgcc_eh.a, so a `-static` rustc link's `-lgcc_eh` reds.
    // Synthesize one from libgcc.a — the standard workaround, and the same one
    // td-util/td-init use.
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
                "{out}/bin/td-firstboot",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-firstboot".into()],
        exec: true,
    });
    // Fail closed on any interpreter/needed/rpath: this runs before the machine has
    // an identity, and a provisioning tool that cannot start has nothing to report
    // its own failure with.
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-firstboot"]));

    Recipe::mesboot("td-firstboot", "0.1")
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
    use super::*;
    use crate::ladder::{
        TD_FIRSTBOOT_HOST_KEY_PREFIX, TD_FIRSTBOOT_NEW_MARKER, TD_FIRSTBOOT_STABLE_MARKER,
    };

    /// Read a `const NAME: &str = "…";` value straight out of the embedded source.
    /// The shipped binary is built from that text, so this is what the running
    /// program will actually use — not a second declaration that could drift.
    fn source_const<'a>(source: &'a str, name: &str) -> Option<&'a str> {
        let (_, after) = source.split_once(&format!("const {name}: &str = \""))?;
        let (value, _) = after.split_once('"')?;
        Some(value)
    }

    /// The markers are a contract between three places: this crate's `ladder`
    /// consts, the qemu oracle that greps for them, and the SEPARATE td-firstboot
    /// crate that prints them. Only the first two share a declaration, so the third
    /// is checked by reading the literals back out of the source the recipe embeds.
    #[test]
    fn the_shipped_binary_prints_the_markers_the_ladder_declares() {
        for (name, marker) in [
            ("NEW_MARKER", TD_FIRSTBOOT_NEW_MARKER),
            ("STABLE_MARKER", TD_FIRSTBOOT_STABLE_MARKER),
            ("HOST_KEY_PREFIX", TD_FIRSTBOOT_HOST_KEY_PREFIX),
        ] {
            assert_eq!(
                source_const(MAIN_RS, name),
                Some(marker),
                "td-firstboot's `{name}` and the ladder const the oracle greps for have drifted \
                 (or the const was renamed); the boot would emit a marker nothing is looking for"
            );
        }
    }

    /// `mod NAME;` and the files written beside `main.rs` must be the same set: a
    /// module declared but not written fails the build with a confusing rustc
    /// "file not found", and a file written but not declared is dead weight in the
    /// derivation.
    #[test]
    fn every_declared_module_is_written_beside_main_rs() {
        let mut declared: Vec<&str> = MAIN_RS
            .lines()
            .filter_map(|line| line.trim().strip_prefix("mod "))
            .filter_map(|rest| rest.strip_suffix(';'))
            .collect();
        declared.sort_unstable();
        let mut written: Vec<&str> = MODULES.iter().map(|(name, _)| *name).collect();
        written.sort_unstable();
        assert_eq!(
            declared, written,
            "MODULES and main.rs's `mod` lines disagree"
        );
    }

    /// The rustc invocation must be flag-for-flag the one td-util already uses.
    /// That recipe's output is a proven static ET_EXEC with an empty closure, and
    /// this link cannot be tried on a host without the target toolchain — so
    /// agreeing with the known-good command line is the strongest check available
    /// per-change, and a mistyped or dropped flag reds here instead of in a full-toolchain
    /// image build.
    #[test]
    fn the_rustc_link_matches_the_proven_td_util_one() {
        fn rustc_argv(stem: &str) -> Vec<String> {
            let recipe = crate::catalog::lookup(stem).unwrap_or_else(|| unreachable!("{stem}"));
            for step in recipe.steps.unwrap_or_default() {
                if let Step::Run { argv, .. } = step {
                    if argv.iter().any(|arg| arg.ends_with("/bin/rustc")) {
                        return argv;
                    }
                }
            }
            Vec::new()
        }
        let mine = rustc_argv("td-firstboot");
        let proven = rustc_argv("td-util");
        assert!(!mine.is_empty() && !proven.is_empty(), "no rustc step found");
        // The ONLY differences may be the output path and the crate root; every
        // other argument is toolchain/link configuration and must match.
        let normalize = |argv: Vec<String>| -> Vec<String> {
            argv.into_iter()
                .map(|arg| {
                    if arg.ends_with("/bin/td-util") || arg.ends_with("/bin/td-firstboot") {
                        "{out}/bin/<crate>".to_string()
                    } else if arg.ends_with("/main.rs") {
                        "{src}/main.rs".to_string()
                    } else {
                        arg
                    }
                })
                .collect()
        };
        assert_eq!(
            normalize(mine),
            normalize(proven),
            "the td-firstboot rustc link diverges from td-util's proven static one"
        );
    }

    /// The static link is the whole reason this is a mesboot recipe rather than a
    /// `Recipe::rust` one; losing either flag would silently ship a dynamic binary
    /// that cannot run before the closure is reachable.
    #[test]
    fn the_binary_is_linked_static_and_asserted_static() {
        let recipe = recipe();
        let steps = recipe.steps.unwrap_or_default();
        let flags: Vec<String> = steps
            .iter()
            .flat_map(|step| match step {
                Step::Run { argv, .. } => argv.clone(),
                _ => Vec::new(),
            })
            .collect();
        for flag in ["target-feature=+crt-static", "relocation-model=static"] {
            assert!(
                flags.iter().any(|arg| arg == flag),
                "the rustc link dropped `{flag}`"
            );
        }
        assert!(
            steps
                .iter()
                .any(|step| matches!(step, Step::AssertStatic { .. })),
            "nothing asserts the shipped td-firstboot has an empty runtime closure"
        );
    }
}
