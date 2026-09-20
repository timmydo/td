use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

/// The realized-output check for td-photo: require the built binary, assert
/// the static shape the system tree demands, then run it on the target.
/// `--help` proves the entry dispatches and `--replay` on the sandbox's
/// null stdin enters td-ui's frame runner and returns at its clean EOF, as
/// td-editor-test proves for the editor. The rest is the frame: a fixture
/// program (`fixtures/td_photo_synth.rs`, the uncompressed synthetic writer
/// of the crate's tests restated alone) is compiled by the target rustc as
/// the direct td recipes are and writes one Z 8 frame into a roll, which
/// the built td-photo then probes with the raw strip decoded, develops
/// with an exposure and a built-in look, edits a sidecar for and lists.
/// A window, its thumbnails and the Wayland client are the crate's own
/// tests under the native harness on the host preflight; this is
/// target-artifact coverage of the static binary and its pipeline, not of
/// the window.
pub fn recipe() -> Recipe {
    let bin = "{in:td-photo}/bin/td-photo";
    // The fixture is linked static exactly as `static_local_source_program`
    // links td-mail: the self-hosted toolchains install under a nested
    // stage/td/store/<pkg> DESTDIR, rust-toolchain flat, and gcc-x86-64-self
    // folds the unwinder into libgcc.a, so the libgcc_eh.a a `-static`
    // rustc link asks for is synthesized from it.
    let rustc = "{in:rust-toolchain}/bin/rustc";
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let gccbin = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin";
    let bbin = "{in:binutils-x86-64-self}/bin";
    let glib = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64/lib";
    let objcopy = "{in:binutils-x86-64-self}/bin/objcopy";
    let ranlib = "{in:binutils-x86-64-self}/bin/ranlib";
    let libgcc_a = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/lib/gcc/x86_64-pc-linux-gnu/14.3.0/libgcc.a";
    let linker = format!("-Clinker={gcc}");
    let lib_b = format!("-Clink-arg=-B{glib}");
    let bin_b = format!("-Clink-arg=-B{bbin}");
    let path = format!("{bbin}:{gccbin}");
    let synth = "{out}/bin/td-photo-synth";
    let frame = "{root}/roll/DSC_0001.NEF";
    Recipe::mesboot("td-photo-test", "1.0")
        .native_inputs(&[
            "td-photo",
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(vec![
            Step::Require {
                paths: vec![bin.into()],
                exec: true,
            },
            Step::assert_static(&[bin]),
            Step::run("{root}", &[bin, "--help"]),
            Step::run("{root}", &[bin, "--replay"]),
            Step::MkDir {
                path: "{out}/bin".into(),
            },
            Step::MkDir {
                path: "{src}".into(),
            },
            Step::WriteFile {
                path: "{src}/synth.rs".into(),
                content: include_str!("../fixtures/td_photo_synth.rs").into(),
                exec: false,
            },
            Step::MkDir {
                path: "{root}/eh".into(),
            },
            Step::run("{root}", &[objcopy, libgcc_a, "{root}/eh/libgcc_eh.a"]).env("PATH", &path),
            Step::run("{root}", &[ranlib, "{root}/eh/libgcc_eh.a"]).env("PATH", &path),
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
                    "-C",
                    "panic=abort",
                    &linker,
                    "-L",
                    glib,
                    &lib_b,
                    &bin_b,
                    "-Clink-arg=-L{root}/eh",
                    "-Clink-arg=-static-libgcc",
                    "-o",
                    synth,
                    "{src}/synth.rs",
                ],
            )
            .env("PATH", &path)
            .env("SOURCE_DATE_EPOCH", "1"),
            Step::Require {
                paths: vec![synth.into()],
                exec: true,
            },
            split_target_debug("{out}"),
            Step::assert_static(&[synth]),
            Step::MkDir {
                path: "{root}/roll".into(),
            },
            Step::run("{root}", &[synth, frame]),
            Step::run("{root}", &[bin, "probe", frame, "--decode"]),
            Step::run(
                "{root}",
                &[
                    bin,
                    "develop",
                    frame,
                    "{root}/developed.ppm",
                    "--long-edge",
                    "16",
                    "--exposure",
                    "0.5",
                    "--look",
                    "mono",
                ],
            ),
            Step::run("{root}", &[bin, "edit", frame, "exposure", "0.50"]),
            Step::run("{root}", &[bin, "list", "{root}/roll"]),
            Step::WriteFile {
                path: "{out}/result".into(),
                content: "PASS: static target td-photo executed help, an empty headless replay, and probed, developed, edited and listed a synthetic Z 8 frame\n".into(),
                exec: false,
            },
            Step::Require {
                paths: vec!["{out}/result".into()],
                exec: false,
            },
        ])
        .checks(vec![RecipeCheck::new(
            "exec \"$TD_RECIPE_EVAL\" check-run td-photo-test 1\n",
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry here is something a build would stay green without: a
    /// missing Require, a missing static assertion, a verb never run, or a
    /// result line claiming more than was checked; and the order is the
    /// proof's, so a run cannot precede the shape it relies on, nor a verb
    /// the frame it reads.
    #[test]
    fn companion_requires_static_binary_and_runs_the_verbs_on_the_frame() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs,
            Some(vec![
                "td-photo".into(),
                "rust-toolchain".into(),
                "gcc-x86-64-self".into(),
                "binutils-x86-64-self".into(),
                "glibc-x86-64".into(),
            ])
        );
        let steps = recipe.steps.expect("steps");
        let bin = "{in:td-photo}/bin/td-photo";
        let synth = "{out}/bin/td-photo-synth";
        let frame = "{root}/roll/DSC_0001.NEF";
        let position = |what: &str, found: Option<usize>| -> usize {
            found.unwrap_or_else(|| panic!("nothing {what}"))
        };
        let requires = |path: &str| {
            steps.iter().position(|s| {
                matches!(s, Step::Require { paths, exec: true } if paths.iter().any(|p| p == path))
            })
        };
        let asserts = |path: &str| {
            steps.iter().position(
                |s| matches!(s, Step::AssertStatic { paths } if paths.iter().any(|p| p == path)),
            )
        };
        let runs = |args: &[&str]| {
            steps
                .iter()
                .position(|s| matches!(s, Step::Run { argv, .. } if *argv == args))
        };
        let required = position("requires td-photo", requires(bin));
        let static_at = position("asserts td-photo is static", asserts(bin));
        let result = position(
            "writes a result naming what it proved",
            steps.iter().position(|s| {
                matches!(s, Step::WriteFile { path, content, .. }
                    if path == "{out}/result" && content.contains("td-photo"))
            }),
        );
        for args in [vec![bin, "--help"], vec![bin, "--replay"]] {
            let run = position(&format!("runs {args:?}"), runs(&args));
            assert!(required < static_at && static_at < run && run < result);
        }
        // The fixture is staged, compiled by the target rustc, required,
        // split and asserted static before it writes the frame.
        let staged = position(
            "stages the fixture source",
            steps.iter().position(|s| {
                matches!(s, Step::WriteFile { path, content, .. }
                    if path == "{src}/synth.rs" && content.contains("fn main() -> ExitCode"))
            }),
        );
        let compiled = position(
            "compiles the fixture",
            steps.iter().position(|s| {
                matches!(s, Step::Run { argv, .. }
                    if argv.first().is_some_and(|a| a.ends_with("/rustc"))
                        && argv.iter().any(|a| a == "{src}/synth.rs")
                        && argv.iter().any(|a| a == synth))
            }),
        );
        let synth_required = position("requires the fixture", requires(synth));
        let split = position(
            "splits the debug companion",
            steps
                .iter()
                .position(|s| matches!(s, Step::SplitDebugTree { root, .. } if root == "{out}")),
        );
        let synth_static = position("asserts the fixture is static", asserts(synth));
        let written = position("writes the frame", runs(&[synth, frame]));
        assert!(staged < compiled && compiled < synth_required);
        assert!(synth_required < split && split < synth_static && synth_static < written);
        let verbs: [Vec<&str>; 4] = [
            vec![bin, "probe", frame, "--decode"],
            vec![
                bin,
                "develop",
                frame,
                "{root}/developed.ppm",
                "--long-edge",
                "16",
                "--exposure",
                "0.5",
                "--look",
                "mono",
            ],
            vec![bin, "edit", frame, "exposure", "0.50"],
            vec![bin, "list", "{root}/roll"],
        ];
        for args in verbs {
            let run = position(&format!("runs {args:?}"), runs(&args));
            assert!(static_at < run && written < run && run < result);
        }
        assert_eq!(
            recipe.checks.map(|checks| checks.len()),
            Some(1),
            "one build-only check"
        );
    }

    /// The verbs, flags and values the check runs are the ones main.rs
    /// dispatches and accepts, pinned against the sources so a renamed verb,
    /// a tightened bound (`--long-edge 16` is the floor) or a dropped
    /// built-in look reds here on the host preflight and not an hour later
    /// in recipe-checks, as td-editor-test pins the editor's modes.
    #[test]
    fn the_verbs_and_values_are_the_ones_main_dispatches() {
        let main = include_str!("../../../td-photo/src/main.rs");
        for flag in ["--help", "--replay"] {
            assert!(main.contains(&format!("if flag == \"{flag}\"")), "{flag}");
        }
        for verb in ["probe", "develop", "edit", "list"] {
            assert!(main.contains(&format!("if verb == \"{verb}\"")), "{verb}");
        }
        assert!(main.contains("&[\"--decode\"]"));
        assert!(main.contains("&[\"--long-edge\", \"--exposure\", \"--look\", \"--crop\"]"));
        assert!(main.contains("(16..=nef::MAX_AXIS).contains(n)"));
        assert!(main.contains("(-5.0..=5.0).contains(v)"));
        // The `--replay` arm reaches td-ui's runner, whose clean EOF the
        // editor's companion pins.
        assert!(main.contains("td_ui::replay::run(&mut io::stdin().lock()"));
        let look = include_str!("../../../td-photo/src/look.rs");
        assert!(look.contains("\"mono\","), "the mono look is not built in");
        let library = include_str!("../../../td-photo/src/library.rs");
        assert!(library.contains("Self::Exposure => \"exposure\""));
    }
}
