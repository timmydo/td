use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

/// The proportional faces' directory in the output; the image names it
/// `/etc/fonts/noto`.
pub const DIR: &str = "share/fonts/noto";

/// The same directory under a host user's XDG data home, where
/// `install-fonts` puts it and td-ui looks.
pub const DATA_DIR: &str = "fonts/noto";

/// Noto Sans and Noto Serif in their four styles, each by its pin and the
/// name it ships as, the distribution repository's own.
pub const FILES: &[(&str, &str)] = &[
    ("noto-sans-regular-source", "NotoSans-Regular.ttf"),
    ("noto-sans-bold-source", "NotoSans-Bold.ttf"),
    ("noto-sans-italic-source", "NotoSans-Italic.ttf"),
    ("noto-sans-bold-italic-source", "NotoSans-BoldItalic.ttf"),
    ("noto-serif-regular-source", "NotoSerif-Regular.ttf"),
    ("noto-serif-bold-source", "NotoSerif-Bold.ttf"),
    ("noto-serif-italic-source", "NotoSerif-Italic.ttf"),
    ("noto-serif-bold-italic-source", "NotoSerif-BoldItalic.ttf"),
];

/// The SIL Open Font License both families are under, by its pin and the
/// name it ships as beside the faces.
pub const NOTICE: (&str, &str) = ("noto-fonts-license-source", "OFL.txt");

// Pinned upstream data (AGENTS.md): compiled TrueType bytes td parses with
// td-ui's bounded reader and never executes, and their licence beside them.
// The recipe copies the pinned files and builds nothing.
pub fn recipe() -> Recipe {
    let file = |name: &str| format!("{{out}}/{DIR}/{name}");
    let shipped = || FILES.iter().chain(std::iter::once(&NOTICE));
    let mut steps = vec![Step::MkDir {
        path: format!("{{out}}/{DIR}"),
    }];
    steps.extend(shipped().map(|(pin, name)| Step::CopyFile {
        file: format!("{{in:{pin}}}"),
        to: file(name),
        exec: false,
    }));
    steps.push(Step::Require {
        paths: shipped().map(|(_, name)| file(name)).collect(),
        exec: false,
    });
    let pins: Vec<&str> = shipped().map(|(pin, _)| *pin).collect();
    Recipe::mesboot("noto-fonts", "2.015")
        .inputs(&pins)
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            "exec \"${TD_RECIPE_EVAL:-$PWD/target/release/td-recipe-eval}\" check-run noto-fonts 1\n",
        )
        .with_runner(CheckRunner::BuildOnly)])
}

/// What `td-builder install-fonts` puts in a host user's font directory,
/// the directory this recipe's output holds, one tab-separated line each:
/// `dir DATA_DIR`; `file URL SHA256 FILE PATH`, each face's pin and where
/// it goes; and `notice URL SHA256 FILE PATH`, the licence's.
pub fn install_plan() -> Result<Vec<String>, String> {
    let pinned = |kind: &str, key: &str, path: &str| {
        let pin = crate::source_pins::by_key(key).ok_or_else(|| format!("{key} is not pinned"))?;
        Ok::<_, String>(format!(
            "{kind}\t{}\t{}\t{}\t{path}",
            pin.url, pin.sha256, pin.file
        ))
    };
    let mut lines = vec![format!("dir\t{DATA_DIR}")];
    for (key, name) in FILES {
        lines.push(pinned("file", key, name)?);
    }
    let (key, name) = NOTICE;
    lines.push(pinned("notice", key, name)?);
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "https://raw.githubusercontent.com/notofonts/notofonts.github.io/28b15b4b43b7bed62b5cf6e6b0b5ff5846270535/fonts/";

    #[test]
    fn the_faces_are_unmarked_data_copied_from_their_pins_alone() {
        let recipe = recipe();
        assert!(!recipe.is_foreign());
        assert!(!recipe.build_root);
        assert_eq!(recipe.native_inputs, None);
        assert_eq!(recipe.source_input, None);
        let pins: Vec<String> = FILES
            .iter()
            .chain(std::iter::once(&NOTICE))
            .map(|(pin, _)| (*pin).to_string())
            .collect();
        assert_eq!(recipe.inputs, Some(pins));
        for (key, name) in FILES {
            let pin = crate::source_pins::by_key(key)
                .unwrap_or_else(|| unreachable!("{key} is not pinned"));
            assert!(!pin.foreign(), "{key} is marked");
            let family = name.split_once('-').map_or("", |(family, _)| family);
            assert_eq!(
                pin.url,
                format!("{COMMIT}{family}/unhinted/ttf/{name}"),
                "{key} is not the reviewed commit's {name}"
            );
        }
        let license = crate::source_pins::by_key(NOTICE.0)
            .unwrap_or_else(|| unreachable!("{} is not pinned", NOTICE.0));
        assert!(!license.foreign());
        assert_eq!(
            license.url,
            "https://raw.githubusercontent.com/notofonts/latin-greek-cyrillic/NotoSans-v2.015/OFL.txt"
        );
        let steps = recipe.steps.as_deref().unwrap_or_default();
        assert!(matches!(
            steps.first(),
            Some(Step::MkDir { path }) if *path == format!("{{out}}/{DIR}")
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
        assert_eq!(copied.len(), FILES.len() + 1);
        for ((pin, name), (from, to)) in FILES.iter().chain(std::iter::once(&NOTICE)).zip(copied) {
            assert_eq!(from, format!("{{in:{pin}}}"));
            assert_eq!(to, format!("{{out}}/share/fonts/noto/{name}"));
        }
        assert!(matches!(
            steps.last(),
            Some(Step::Require { paths, exec: false }) if paths.len() == FILES.len() + 1
        ));
        assert!(steps.iter().all(|step| matches!(
            step,
            Step::MkDir { .. } | Step::CopyFile { .. } | Step::Require { .. }
        )));
    }

    #[test]
    fn the_install_plan_is_the_recipes_pins_and_names() {
        let plan = install_plan().unwrap();
        let pin = |key: &str| crate::source_pins::by_key(key).unwrap();
        assert_eq!(DIR, format!("share/{DATA_DIR}"));
        let mut want = vec!["dir\tfonts/noto".to_string()];
        for (key, name) in FILES {
            let face = pin(key);
            want.push(format!(
                "file\t{}\t{}\t{}\t{name}",
                face.url, face.sha256, face.file
            ));
        }
        let license = pin(NOTICE.0);
        want.push(format!(
            "notice\t{}\t{}\t{}\tOFL.txt",
            license.url, license.sha256, license.file
        ));
        assert_eq!(plan, want);
        for line in &plan {
            assert!(!line.contains('\n'));
            assert!(line.split('\t').all(|field| !field.is_empty()), "{line}");
        }
    }
}
