use crate::shared::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

const MAIN_RS: &str = include_str!("../../../td-seatd/src/main.rs");

pub fn recipe() -> Recipe {
    let rustc = "{in:rust-toolchain}/bin/rustc";

    let steps = vec![
        Step::MkDir {
            path: "{out}/bin".into(),
        },
        Step::WriteFile {
            path: "{src}/main.rs".into(),
            content: MAIN_RS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{src}/backlight.rs".into(),
            content: include_str!("../../../td-backlight/src/lib.rs").into(),
            exec: false,
        },
        target_rustc(
            "{src}",
            rustc,
            &[
                "--edition",
                "2021",
                "--crate-type",
                "rlib",
                "--crate-name",
                "td_backlight",
                "--target",
                "x86_64-unknown-linux-gnu",
                "-C",
                "opt-level=s",
                "-C",
                "target-feature=+crt-static",
                "-C",
                "relocation-model=static",
                "-o",
                "{src}/libtd_backlight.rlib",
                "{src}/backlight.rs",
            ],
        )
        .env("SOURCE_DATE_EPOCH", "1"),
        target_rustc(
            "{src}",
            rustc,
            &[
                "--extern",
                "td_backlight={src}/libtd_backlight.rlib",
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
                "-o",
                "{out}/bin/td-seatd",
                "{src}/main.rs",
            ],
        )
        .env("SOURCE_DATE_EPOCH", "1"),
        Step::Require {
            paths: vec!["{out}/bin/td-seatd".into()],
            exec: true,
        },
        split_target_debug("{out}"),
        Step::assert_static(&["{out}/bin/td-seatd"]),
    ];

    Recipe::mesboot("td-seatd", "0.1")
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::super::system_x86_64::AUDIO_RUNTIME;
    use super::*;

    #[test]
    fn recipe_embeds_the_linted_crate_source() {
        let steps = recipe().steps.expect("td-seatd steps");
        let production = MAIN_RS
            .split("\n#[cfg(test)]")
            .next()
            .expect("td-seatd production source");
        assert!(steps.iter().any(|step| {
            matches!(
                step,
                Step::WriteFile { path, content, .. }
                    if path == "{src}/main.rs" && content == MAIN_RS
            )
        }));
        assert!(production.contains("#![forbid(unsafe_code)]"));
        assert!(production.contains("prepare_owned_runtime(path, account, 0o700)"));
        assert!(production.contains("verify_owner_mode(path, account, mode)"));
        assert!(production.contains(&format!("const AUDIO_RUNTIME: &str = {AUDIO_RUNTIME:?};")));
        assert!(production.contains("verify_runtime_base(base, require_root_base)?;"));
        assert!(production.contains("prepare_owned_runtime(path, account, 0o755)"));
        assert!(production
            .contains("prepare_audio_runtime(audio_runtime, assignment.audio, require_char)?;"));
        assert!(production.contains("verify_owner_mode(audio_runtime, assignment.audio, 0o755)?;"));
    }
}
