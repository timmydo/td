use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

/// The outline face's directory in the output; the image names it
/// `/etc/fonts/jetbrains-mono-nerd`.
pub const DIR: &str = "share/fonts/jetbrains-mono-nerd";

/// The same directory under a host user's XDG data home, where
/// `install-fonts` puts it and td-ui looks.
pub const DATA_DIR: &str = "fonts/jetbrains-mono-nerd";

/// The four Mono styles td-ui reads, and the notices the release carries:
/// the SIL Open Font License, and the README naming each merged icon set
/// with its upstream and licence.
pub const FILES: &[&str] = &[
    "JetBrainsMonoNerdFontMono-Regular.ttf",
    "JetBrainsMonoNerdFontMono-Bold.ttf",
    "JetBrainsMonoNerdFontMono-Italic.ttf",
    "JetBrainsMonoNerdFontMono-BoldItalic.ttf",
    "OFL.txt",
    "README.md",
];

/// The licence notices the Nerd Fonts repository carries at the release's
/// tag, each its own pin (`nerd-fonts-SET-license-source`), shipped as
/// `licenses/SET/FILE`: the repository's own, then each merged icon set's.
pub const NOTICES: &[(&str, &str)] = &[
    ("nerd-fonts", "LICENSE"),
    ("codicons", "LICENSE.txt"),
    ("font-awesome", "LICENSE.txt"),
    ("materialdesign", "LICENSE"),
    ("octicons", "LICENSE"),
    ("pomicons", "LICENSE"),
    ("powerline-extra", "LICENSE"),
    ("powerline-symbols", "LICENSE.txt"),
    ("weather-icons", "OFL.txt"),
];

/// The release archive's pin.
const SOURCE: &str = "jetbrains-mono-nerd-font-source";

// Pinned upstream data (AGENTS.md): compiled TrueType bytes td parses with
// td-ui's bounded reader and never executes, and the notices beside them.
// The recipe copies the pinned files and builds nothing.
pub fn recipe() -> Recipe {
    let notice = |set: &str, file: &str| format!("{{out}}/{DIR}/{}", notice_path(set, file));
    let mut steps = vec![
        Step::Unpack {
            input: format!("{{in:{SOURCE}}}"),
            dest: "{src}".into(),
            keep_top: true,
        },
        Step::MkDir {
            path: format!("{{out}}/{DIR}"),
        },
        Step::CopyFiles {
            files: FILES.iter().map(|name| format!("{{src}}/{name}")).collect(),
            dest: format!("{{out}}/{DIR}"),
        },
    ];
    steps.extend(NOTICES.iter().map(|(set, file)| Step::CopyFile {
        file: format!("{{in:{}}}", notice_pin(set)),
        to: notice(set, file),
        exec: false,
    }));
    steps.push(Step::Require {
        paths: FILES
            .iter()
            .map(|name| format!("{{out}}/{DIR}/{name}"))
            .chain(NOTICES.iter().map(|(set, file)| notice(set, file)))
            .collect(),
        exec: false,
    });
    let pins: Vec<String> = NOTICES.iter().map(|(set, _)| notice_pin(set)).collect();
    let pins: Vec<&str> = pins.iter().map(String::as_str).collect();
    Recipe::mesboot("jetbrains-mono-nerd-font", "3.5.1")
        .source_input(SOURCE)
        .inputs(&pins)
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            "exec \"${TD_RECIPE_EVAL:-$PWD/target/release/td-recipe-eval}\" check-run jetbrains-mono-nerd-font 1\n",
        )
        .with_runner(CheckRunner::BuildOnly)])
}

fn notice_pin(set: &str) -> String {
    format!("nerd-fonts-{set}-license-source")
}

/// Where a notice ships, relative to the face's directory.
fn notice_path(set: &str, file: &str) -> String {
    format!("licenses/{set}/{file}")
}

/// What `td-builder install-fonts` puts in a host user's font directory,
/// the directory this recipe's output holds, one tab-separated line each:
/// `dir DATA_DIR`; `archive URL SHA256 FILE`, the release's pin; `member NAME`, each of
/// `FILES` copied from it; and `notice URL SHA256 FILE PATH`, each notice's
/// pin and where it goes.
pub fn install_plan() -> Result<Vec<String>, String> {
    let pin =
        |key: &str| crate::source_pins::by_key(key).ok_or_else(|| format!("{key} is not pinned"));
    let archive = pin(SOURCE)?;
    let mut lines = vec![
        format!("dir\t{DATA_DIR}"),
        format!(
            "archive\t{}\t{}\t{}",
            archive.url, archive.sha256, archive.file
        ),
    ];
    lines.extend(FILES.iter().map(|name| format!("member\t{name}")));
    for (set, file) in NOTICES {
        let notice = pin(&notice_pin(set))?;
        lines.push(format!(
            "notice\t{}\t{}\t{}\t{}",
            notice.url,
            notice.sha256,
            notice.file,
            notice_path(set, file)
        ));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_face_is_unmarked_data_copied_from_its_pins_alone() {
        let recipe = recipe();
        assert!(!recipe.is_foreign());
        assert_eq!(recipe.native_inputs, None);
        let pins: Vec<String> = NOTICES.iter().map(|(set, _)| notice_pin(set)).collect();
        assert_eq!(recipe.inputs, Some(pins.clone()));
        for pin in &pins {
            let pin = crate::source_pins::all()
                .into_iter()
                .find(|def| def.key == pin.as_str())
                .unwrap_or_else(|| unreachable!("{pin} is not pinned"));
            assert!(
                pin.url
                    .starts_with("https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/"),
                "{} is not the release tag's",
                pin.url
            );
        }
        let steps = recipe.steps.as_deref().unwrap_or_default();
        assert!(matches!(
            steps,
            [Step::Unpack { input, keep_top: true, .. }, Step::MkDir { .. }, Step::CopyFiles { files, .. }, ..]
                if input == "{in:jetbrains-mono-nerd-font-source}"
                    && *files == FILES.iter().map(|name| format!("{{src}}/{name}")).collect::<Vec<_>>()
        ));
        let copied: Vec<(&str, &str)> = steps
            .iter()
            .filter_map(|step| match step {
                Step::CopyFile {
                    file,
                    to,
                    exec: false,
                } => Some((file.as_str(), to.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(copied.len(), NOTICES.len());
        for ((set, file), (from, to)) in NOTICES.iter().zip(copied) {
            assert_eq!(from, format!("{{in:{}}}", notice_pin(set)));
            assert_eq!(to, format!("{{out}}/{DIR}/licenses/{set}/{file}"));
        }
        assert!(matches!(
            steps.last(),
            Some(Step::Require { paths, exec: false }) if paths.len() == FILES.len() + NOTICES.len()
        ));
        assert!(steps.iter().all(|step| matches!(
            step,
            Step::Unpack { .. }
                | Step::MkDir { .. }
                | Step::CopyFiles { .. }
                | Step::CopyFile { .. }
                | Step::Require { .. }
        )));
    }

    #[test]
    fn the_install_plan_is_the_recipes_pins_members_and_notices() {
        let plan = install_plan().unwrap();
        let pin = |key: &str| crate::source_pins::by_key(key).unwrap();
        let archive = pin(SOURCE);
        let mut want = vec![
            "dir\tfonts/jetbrains-mono-nerd".to_string(),
            format!(
                "archive\t{}\t{}\t{}",
                archive.url, archive.sha256, archive.file
            ),
        ];
        assert_eq!(DIR, format!("share/{DATA_DIR}"));
        for name in FILES {
            want.push(format!("member\t{name}"));
        }
        for (set, file) in NOTICES {
            let notice = pin(&notice_pin(set));
            want.push(format!(
                "notice\t{}\t{}\t{}\tlicenses/{set}/{file}",
                notice.url, notice.sha256, notice.file
            ));
        }
        assert_eq!(plan, want);
        // Every field is one nonempty token: no tab or newline in a value.
        for line in &plan {
            assert!(!line.contains('\n'));
            assert!(line.split('\t').all(|field| !field.is_empty()), "{line}");
        }
    }
}
