use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

/// The realized-output check for td-editor: require the built binary,
/// assert the static shape the application runtime demands, then execute
/// it in the three modes that need no display. `--help` proves the entry
/// dispatches; `--replay` on the sandbox's null stdin enters td-ui's
/// consecutive-frame runner and returns at its clean EOF, so the headless
/// entry is wired, though no frame is decoded; `--font-license` writes the
/// three notices td-ui embeds from the staged compositor assets, proven by
/// exit status alone since the sandbox holds no tool to read them back.
/// The face, the renderer and a window are exercised by the crate's own
/// tests under the native harness on the host preflight; this is
/// target-artifact coverage of the static binary, not of the window.
pub fn recipe() -> Recipe {
    let bin = "{in:td-editor}/bin/td-editor";
    Recipe::mesboot("td-editor-test", "1.0")
        .native_inputs(&["td-editor"])
        .steps(vec![
            Step::Require {
                paths: vec![bin.into()],
                exec: true,
            },
            Step::assert_static(&[bin]),
            Step::run("{root}", &[bin, "--help"]),
            Step::run("{root}", &[bin, "--replay"]),
            Step::run("{root}", &[bin, "--font-license"]),
            Step::MkDir {
                path: "{out}".into(),
            },
            Step::WriteFile {
                path: "{out}/result".into(),
                content: "PASS: static target td-editor executed help, an empty headless replay and the embedded font licence notices\n".into(),
                exec: false,
            },
            Step::Require {
                paths: vec!["{out}/result".into()],
                exec: false,
            },
        ])
        .checks(vec![RecipeCheck::new(
            "exec \"$TD_RECIPE_EVAL\" check-run td-editor-test 1\n",
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry here is something a build would stay green without: a
    /// missing Require, a missing static assertion, a mode never run, or a
    /// result line claiming more than was checked; and the order is the
    /// proof's, so a run cannot precede the shape it relies on.
    #[test]
    fn companion_requires_static_binary_and_executes_the_headless_modes() {
        let recipe = recipe();
        assert_eq!(recipe.native_inputs, Some(vec!["td-editor".into()]));
        let steps = recipe.steps.expect("steps");
        let bin = "{in:td-editor}/bin/td-editor";
        let required = steps
            .iter()
            .position(|s| {
                matches!(s, Step::Require { paths, exec: true } if paths.iter().any(|p| p == bin))
            })
            .expect("nothing requires td-editor");
        let static_at = steps
            .iter()
            .position(
                |s| matches!(s, Step::AssertStatic { paths } if paths.iter().any(|p| p == bin)),
            )
            .expect("nothing asserts td-editor is static");
        let result = steps
            .iter()
            .position(|s| {
                matches!(s, Step::WriteFile { path, content, .. }
                    if path == "{out}/result" && content.contains("td-editor"))
            })
            .expect("the result does not mention what it proved");
        for args in [
            vec![bin, "--help"],
            vec![bin, "--replay"],
            vec![bin, "--font-license"],
        ] {
            let run = steps
                .iter()
                .position(|s| matches!(s, Step::Run { argv, .. } if *argv == args))
                .unwrap_or_else(|| panic!("nothing runs {args:?}"));
            assert!(required < static_at && static_at < run && run < result);
        }
    }

    /// The three modes are the ones main.rs answers without a display, and
    /// the replay runner returns cleanly on immediate EOF, which is what the
    /// sandbox's stdin gives it: pinned against the sources, so a renamed
    /// flag or a runner that started refusing empty input reds here and not
    /// only in the sandbox.
    #[test]
    fn the_headless_modes_are_the_entry_points_main_dispatches() {
        let main = include_str!("../../../td-editor/src/main.rs");
        for flag in ["--help", "--replay", "--font-license"] {
            assert!(
                main.contains(&format!("[arg] if arg == \"{flag}\"")),
                "{flag}"
            );
        }
        // The `--replay` arm reaches td-ui's runner through the editor's
        // replay module, so the EOF pinned below is the one the step meets.
        assert!(main.contains("td_editor::replay::run(&mut io::stdin().lock()"));
        let editor_replay = include_str!("../../../td-editor/src/replay.rs");
        assert!(editor_replay.contains("td_ui::replay::run(input, output,"));
        let runner = include_str!("../../../td-ui/src/replay.rs");
        assert!(
            runner.contains("Ok(0) => return Ok(()),"),
            "EOF between frames is no longer clean"
        );
    }
}
