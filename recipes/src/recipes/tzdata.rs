use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    let glibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let mut steps = vec![
        Step::Unpack {
            input: "{in:tzdata-source}".into(),
            dest: "{src}".into(),
            keep_top: true,
        },
        Step::MkDir {
            path: "{out}/share/zoneinfo".into(),
        },
        Step::run(
            "{src}",
            &[
                &format!("{glibc}/lib/ld-linux-x86-64.so.2"),
                "--library-path",
                &format!("{glibc}/lib"),
                &format!("{glibc}/sbin/zic"),
                "-b",
                // glibc zic 2024a needs fat output for 2026d's Canada rules.
                "fat",
                "-d",
                "{out}/share/zoneinfo",
                "africa",
                "antarctica",
                "asia",
                "australasia",
                "europe",
                "northamerica",
                "southamerica",
                "etcetera",
                "backward",
            ],
        )
        .env("LC_ALL", "C"),
        Step::CopyFiles {
            files: ["iso3166.tab", "zone.tab", "zone1970.tab", "zonenow.tab"]
                .iter()
                .map(|name| format!("{{src}}/{name}"))
                .collect(),
            dest: "{out}/share/zoneinfo".into(),
        },
        Step::MkDir {
            path: "{out}/share/doc/tzdata".into(),
        },
        Step::CopyFiles {
            files: vec!["{src}/LICENSE".into(), "{src}/version".into()],
            dest: "{out}/share/doc/tzdata".into(),
        },
    ];
    steps.push(Step::Require {
        paths: [
            "Etc/UTC",
            "America/Los_Angeles",
            "Europe/London",
            "Asia/Tokyo",
        ]
        .iter()
        .map(|name| format!("{{out}}/share/zoneinfo/{name}"))
        .collect(),
        exec: false,
    });
    Recipe::mesboot("tzdata", "2026d")
        .source_input("tzdata-source")
        .native_inputs(&["glibc-x86-64"])
        .steps(steps)
        // Retain the check-script compatibility entry; validation is native.
        .checks(vec![RecipeCheck::new(
            "exec \"${TD_RECIPE_EVAL:-$PWD/target/release/td-recipe-eval}\" check-run tzdata 1\n",
        )
        .with_runner(CheckRunner::Tzdata)])
}
