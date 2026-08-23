//! The package catalog — every td recipe, declared in Rust.
//!
//! Keyed by a stable STEM (not the recipe name): the `-perturbed`
//! self-discrimination twins deliberately share a recipe `name` with their base
//! (e.g. `hello-perturbed` is name `hello`), so the stem is the stable key. The
//! `recipe-rs` gate proves the surface is self-consistent.
//!
//! Each recipe lives in its own self-registering file `src/recipes/<stem>.rs`
//! (github issue #295): the file name IS the stem, `pub fn recipe() -> Recipe`
//! is the registration, and `build.rs` generates the stem-sorted registry
//! (module declarations + the `all()` table) included below. Adding a recipe
//! touches only its new file: no Rust source line is shared, so parallel recipe
//! PRs don't collide on a central table (the mk/gates/ one-file-per-entry property).

use crate::types::Recipe;

/// Look up a recipe by `.ts` file stem (e.g. "hello", "gzip-perturbed").
pub fn lookup(stem: &str) -> Option<Recipe> {
    all().into_iter().find(|(s, _)| *s == stem).map(|(_, r)| r)
}

/// Every migrated recipe, paired with its `.ts` file stem, sorted by stem.
pub fn all() -> Vec<(&'static str, Recipe)> {
    registry::all()
}

mod registry {
    include!(concat!(env!("OUT_DIR"), "/registry.rs"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recipe_emits_canonical_json_and_round_trips() {
        for (stem, r) in all() {
            let canon = r.to_json().to_canonical();
            // Structural self-consistency: re-parsing the emitted JSON and
            // re-canonicalising yields the same bytes (the durable round-trip).
            let reparsed = crate::json::parse(&canon)
                .unwrap_or_else(|e| panic!("{stem}: emitted invalid JSON: {e}"));
            assert_eq!(reparsed.to_canonical(), canon, "{stem}: not idempotent");
            assert!(!r.name.is_empty() && !r.version.is_empty(), "{stem}: missing fields");
        }
    }

    #[test]
    fn catalog_is_sorted_and_stems_are_unique() {
        // The generated registry must stay stem-sorted (the stable `list`
        // order) with no duplicate stems, whatever read_dir order build.rs saw.
        let stems: Vec<&str> = all().into_iter().map(|(s, _)| s).collect();
        let mut sorted = stems.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(stems, sorted, "catalog stems are not sorted+unique");
    }

    #[test]
    fn first_seed_binds_one_foreign_pin_to_one_payload_runtime() {
        let seed = lookup("ripgrep-seed").expect("ripgrep seed recipe");
        assert!(seed.is_foreign(), "the prebuilt source pin must mark its recipe");
        assert!(seed.is_foreign_source());
        assert_eq!(seed.name, "ripgrep-seed");
        assert_eq!(seed.version, "15.2.0");
        assert_eq!(seed.source_input.as_deref(), Some("ripgrep-seed-source"));
        assert_eq!(seed.payload_inputs, Some(vec!["empty-runtime".to_string()]));
        let declaration = seed.application.as_ref().expect("application declaration");
        assert_eq!(declaration.runtime(), "empty-runtime");
        assert_eq!(declaration.entry(), "/app/bin/rg");
        let launcher = seed
            .application_launcher
            .as_ref()
            .expect("application launcher declaration");
        assert_eq!(launcher.display_name(), "Ripgrep");
        assert_eq!(
            launcher.search_terms().collect::<Vec<_>>(),
            vec!["ripgrep", "rg", "search", "text", "files"]
        );
        assert_eq!(
            seed.application_permissions
                .as_ref()
                .map(td_engine::permissions::PermissionPolicy::to_keyfile)
                .as_deref(),
            Some("format=1\n")
        );
        assert!(seed.steps.as_ref().is_some_and(|steps| {
            steps.last().is_some_and(|step| {
                matches!(
                    step,
                    crate::types::Step::ValidateStaticApplication { entry, runtime }
                        if entry == declaration.entry() && runtime == declaration.runtime()
                )
            }) && steps.iter().any(|step| {
                matches!(
                    step,
                    crate::types::Step::Unpack { input, .. }
                        if input == "{payload:ripgrep-seed-source}"
                )
            })
        }));

        let runtime = lookup("empty-runtime").expect("empty runtime recipe");
        assert!(!runtime.is_foreign());
        assert!(runtime.application.is_none());
        assert!(runtime.source_input.is_none());
    }

    #[test]
    fn every_direct_target_rust_recipe_uses_the_global_profile_and_companion_step() {
        let expected = [
            "td-boot",
            "td-busd",
            "td-compositor",
            "td-firstboot",
            "td-init",
            "td-install",
            "td-jail",
            "td-kexec",
            "td-login",
            "td-netd",
            "td-seatd",
            "td-sh",
            "td-svc",
            "td-txt",
            "td-util",
        ];
        let mut covered = Vec::new();
        for (stem, recipe) in all() {
            let Some(steps) = recipe.steps.as_ref() else {
                continue;
            };
            let rustc_runs: Vec<&Vec<String>> = steps
                .iter()
                .filter_map(|step| match step {
                    crate::types::Step::Run { argv, .. }
                        if argv.first().is_some_and(|arg| arg.ends_with("/rustc"))
                            && argv.iter().any(|arg| arg.ends_with(".rs")) =>
                    {
                        Some(argv)
                    }
                    _ => None,
                })
                .collect();
            if rustc_runs.is_empty() {
                continue;
            }
            covered.push(stem);
            let mut remap_sources = Vec::new();
            for argv in rustc_runs {
                let source_position = argv
                    .iter()
                    .rposition(|arg| arg.ends_with(".rs"))
                    .expect("source-compiling rustc invocation has a .rs input");
                let policy_position = argv
                    .iter()
                    .position(|arg| arg == td_engine::target_profile::DIRECT_RUSTC_ARGS[0])
                    .unwrap_or(0);
                assert!(
                    policy_position > source_position,
                    "{stem}: target policy must follow recipe-local rustc options"
                );
                for required in td_engine::target_profile::DIRECT_RUSTC_ARGS.iter().take(4) {
                    assert!(
                        argv.iter().any(|arg| arg == required),
                        "{stem}: direct rustc omitted {required}"
                    );
                }
                let build_remaps: Vec<&str> = argv
                    .iter()
                    .filter(|arg| {
                        arg.starts_with("--remap-path-prefix=")
                            && arg.ends_with("=/td-build-root")
                    })
                    .map(String::as_str)
                    .collect();
                let source_remaps: Vec<&str> = argv
                    .iter()
                    .filter(|arg| {
                        arg.starts_with("--remap-path-prefix=")
                            && arg.ends_with("=/td-build")
                    })
                    .map(String::as_str)
                    .collect();
                assert_eq!(build_remaps.len(), 1, "{stem}: build-root remap drifted");
                assert_eq!(source_remaps.len(), 1, "{stem}: source-root remap drifted");
                remap_sources.push((
                    build_remaps.first().copied().unwrap_or_default().to_string(),
                    source_remaps.first().copied().unwrap_or_default().to_string(),
                ));
                let strip_options: Vec<&str> = argv
                    .iter()
                    .filter(|arg| arg.contains("strip="))
                    .map(String::as_str)
                    .collect();
                assert_eq!(
                    strip_options,
                    ["-Cstrip=none"],
                    "{stem}: direct rustc must preserve symbols for its companion"
                );
            }
            if stem == "td-boot" {
                assert_eq!(remap_sources.len(), 2, "td-boot must build two roots");
                assert_ne!(
                    remap_sources.first(),
                    remap_sources.get(1),
                    "td-boot's reproducibility oracle must vary both remap inputs"
                );
            }
            assert!(
                steps.iter().any(|step| matches!(
                    step,
                    crate::types::Step::SplitDebugTree { root, objcopy }
                        if root == "{out}"
                            && objcopy == "{in:binutils-x86-64-self}/bin/objcopy"
                )),
                "{stem}: missing target debug-companion split"
            );
        }
        assert_eq!(
            covered, expected,
            "the reviewed direct-rustc roster changed"
        );
    }

    #[test]
    fn every_cargo_target_declares_the_objcopy_used_by_the_runner() {
        for (stem, recipe) in all() {
            if !matches!(recipe.build_system, crate::types::BuildSystem::Rust) {
                continue;
            }
            let inputs = recipe.native_inputs.as_deref().unwrap_or_default();
            assert!(
                inputs.iter().any(|input| input == "binutils-x86-64-self"),
                "{stem}: Cargo target must declare binutils-x86-64-self for debug splitting"
            );
        }
    }

    #[test]
    fn every_assembly_exception_names_a_declared_target_recipe() {
        for (stem, _) in td_engine::target_profile::ASSEMBLY_EXCEPTIONS {
            let recipe = lookup(stem)
                .unwrap_or_else(|| panic!("assembly exception names missing recipe {stem}"));
            if matches!(stem, "gcc-x86-64-stage1" | "gcc-x86-64-native") {
                continue;
            }
            let generic_cargo_split = matches!(recipe.build_system, crate::types::BuildSystem::Rust);
            let typed_split = recipe.steps.as_deref().unwrap_or_default().iter().any(|step| {
                matches!(step, crate::types::Step::SplitDebugTree { .. })
            });
            assert!(
                generic_cargo_split || typed_split,
                "{stem}: assembly exception would not reach the marker-producing splitter"
            );
        }

        // These compiler rungs are build-only provenance for libgcc objects
        // linked into later outputs. They do not need companions of their own,
        // but the marker on each split consumer must name the actual rung.
        assert!(td_engine::target_profile::output_assembly_exceptions("glibc-x86-64")
            .iter()
            .any(|(source, _)| *source == "gcc-x86-64-stage1"));
        for stem in ["binutils-x86-64-self", "gcc-x86-64-self"] {
            assert!(td_engine::target_profile::output_assembly_exceptions(stem)
                .iter()
                .any(|(source, _)| *source == "gcc-x86-64-native"));
        }

        let mut rust_outputs: Vec<&str> = all()
            .into_iter()
            .filter_map(|(stem, recipe)| {
                let cargo = matches!(recipe.build_system, crate::types::BuildSystem::Rust);
                let direct = recipe.steps.as_deref().unwrap_or_default().iter().any(|step| {
                    matches!(
                        step,
                        crate::types::Step::Run { argv, .. }
                            if argv.first().is_some_and(|arg| arg.ends_with("/rustc"))
                                && argv.iter().any(|arg| arg.ends_with(".rs"))
                    )
                });
                (cargo || direct || stem == "rust-toolchain").then_some(stem)
            })
            .collect();
        rust_outputs.sort_unstable();
        assert_eq!(
            rust_outputs,
            td_engine::target_profile::RUST_PROFILED_RECIPES,
            "the transitive Rust/LLVM assembly-boundary roster changed"
        );
    }

    #[test]
    fn deployment_and_toolchain_have_independent_external_debug_ceilings() {
        for (stem, expected_scope, expected_report, expected_ceiling) in [
            (
                "rust-toolchain",
                "rust-toolchain",
                "{out}/share/td/debug-size",
                td_engine::target_profile::TOOLCHAIN_DEBUG_CEILING_BYTES,
            ),
            (
                "system-x86-64",
                "deployment",
                "{out}/deployment/debug-size",
                td_engine::target_profile::DEPLOYMENT_DEBUG_CEILING_BYTES,
            ),
        ] {
            let recipe = lookup(stem).unwrap_or_else(|| panic!("missing recipe {stem}"));
            let sizes: Vec<(&str, &str, u64)> = recipe
                .steps
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter_map(|step| match step {
                    crate::types::Step::AssertDebugSize {
                        report,
                        scope,
                        ceiling,
                        ..
                    } => Some((scope.as_str(), report.as_str(), *ceiling)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                sizes,
                [(
                    expected_scope,
                    expected_report,
                    expected_ceiling,
                )],
                "{stem}: debug measurement must use its reviewed scope ceiling"
            );
        }
    }

    #[test]
    fn source_built_runtime_toolchain_keeps_frames_lines_ids_and_remapped_paths() {
        for (stem, expected_tail_uses) in [
            ("binutils-x86-64-self", 2),
            ("gcc-x86-64-self", 4),
            ("glibc-x86-64", 1),
            ("rust-toolchain", 2),
        ] {
            let recipe = lookup(stem).unwrap_or_else(|| panic!("missing target recipe {stem}"));
            let json = recipe.to_json().to_canonical();
            for required in [
                "-fno-omit-frame-pointer",
                "-g1",
                "--build-id=sha1",
                "/td-build-root",
                "splitDebugTree",
            ] {
                assert!(
                    json.contains(required),
                    "{stem}: omitted target policy {required}"
                );
            }
            let wrappers: Vec<&str> = recipe
                .steps
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter_map(|step| match step {
                    crate::types::Step::WriteFile { content, .. }
                        if content.contains("-fno-omit-frame-pointer") =>
                    {
                        Some(content.as_str())
                    }
                    _ => None,
                })
                .collect();
            let tail_uses = wrappers
                .iter()
                .map(|content| content.matches("\"$@\" -fno-omit-frame-pointer").count())
                .sum::<usize>();
            assert_eq!(
                tail_uses, expected_tail_uses,
                "{stem}: wrapper policy no longer follows every caller argument list"
            );
            for content in wrappers {
                let root_map = content
                    .rfind("=/td-build-root")
                    .unwrap_or_else(|| panic!("{stem}: profile wrapper remaps the build root"));
                let source_map = content
                    .rfind("=/td-build")
                    .unwrap_or_else(|| panic!("{stem}: profile wrapper remaps package source"));
                assert!(
                    root_map < source_map,
                    "{stem}: the specific package-source remap must follow the build-root remap"
                );
            }
        }
        let rust = lookup("rust-toolchain")
            .expect("rust toolchain recipe")
            .to_json()
            .to_canonical();
        for required in [
            "debuginfo-level = 1",
            "frame-pointers = true",
            "release-debuginfo = false",
            "remap-debuginfo = false",
            "RUSTFLAGS_NOT_BOOTSTRAP",
            "/td-cargo/vendor",
            "strip = false",
        ] {
            assert!(rust.contains(required), "rust bootstrap omitted {required}");
        }
    }
}
