use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

// td-profiler is compiled directly with the source-built target rustc and no
// third-party crates. It is static because the collector is a system oracle:
// opening perf and explaining a broken runtime must not depend on that runtime's
// dynamic loader. The crate is the one source of truth; this recipe embeds every
// sibling module beside main.rs for direct rustc module resolution.
const MAIN_RS: &str = include_str!("../../../td-profiler/src/main.rs");
const MODULES: &[(&str, &str)] = &[
    (
        "collector",
        include_str!("../../../td-profiler/src/collector.rs"),
    ),
    (
        "contract",
        include_str!("../../../td-profiler/src/contract.rs"),
    ),
    ("cpuset", include_str!("../../../td-profiler/src/cpuset.rs")),
    ("dwarf", include_str!("../../../td-profiler/src/dwarf.rs")),
    (
        "evidence",
        include_str!("../../../td-profiler/src/evidence.rs"),
    ),
    ("event", include_str!("../../../td-profiler/src/event.rs")),
    ("index", include_str!("../../../td-profiler/src/index.rs")),
    ("json", include_str!("../../../td-profiler/src/json.rs")),
    ("perf", include_str!("../../../td-profiler/src/perf.rs")),
    ("raw", include_str!("../../../td-profiler/src/raw.rs")),
    ("report", include_str!("../../../td-profiler/src/report.rs")),
    ("state", include_str!("../../../td-profiler/src/state.rs")),
    ("symbol", include_str!("../../../td-profiler/src/symbol.rs")),
    ("sys", include_str!("../../../td-profiler/src/sys.rs")),
    (
        "topology",
        include_str!("../../../td-profiler/src/topology.rs"),
    ),
];

#[cfg(test)]
fn declared_modules() -> Vec<&'static str> {
    MAIN_RS
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("mod ")
                .and_then(|rest| rest.strip_suffix(';'))
        })
        .collect()
}

pub fn recipe() -> Recipe {
    let rustc = "{in:rust-toolchain}/bin/rustc";

    let mut steps = vec![
        Step::MkDir {
            path: "{out}/bin".into(),
        },
        Step::WriteFile {
            path: "{src}/main.rs".into(),
            content: MAIN_RS.into(),
            exec: false,
        },
    ];
    for (name, source) in MODULES {
        steps.push(Step::WriteFile {
            path: format!("{{src}}/{name}.rs"),
            content: (*source).into(),
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
                "-C",
                "panic=abort",
                "-o",
                "{out}/bin/td-profiler",
                "{src}/main.rs",
            ],
        )
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-profiler".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-profiler"]));

    Recipe::mesboot("td-profiler", "0.1")
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recipe_embeds_exactly_the_declared_modules() {
        let mut declared = declared_modules();
        let mut written: Vec<_> = MODULES.iter().map(|(name, _)| *name).collect();
        declared.sort_unstable();
        written.sort_unstable();
        assert_eq!(written, declared);
        assert_eq!(written.len(), 15);
    }

    #[test]
    fn the_target_binary_is_static_profiled_and_split() {
        let recipe = recipe();
        let steps = recipe.steps.unwrap_or_default();
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::AssertStatic { paths } if paths == &["{out}/bin/td-profiler"]
        )));
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::SplitDebugTree { root, .. } if root == "{out}"
        )));
    }

    #[test]
    fn collector_pins_the_privileged_perf_boundary_before_credential_drop() {
        let collector = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "collector").then_some(*source))
            .unwrap_or_else(|| unreachable!("collector module is not staged"));
        assert!(collector.contains("/proc/sys/kernel/perf_event_paranoid"));
        assert!(collector.contains("if paranoid < 1"));
        assert!(
            collector
                .find("start_observation(&config)")
                .unwrap_or(usize::MAX)
                < collector.find("drop_credentials").unwrap_or(0),
            "perf descriptors must be opened before the service drops credentials"
        );
    }
}
