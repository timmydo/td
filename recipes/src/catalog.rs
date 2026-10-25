//! The package catalog — every td recipe, declared in Rust.
//!
//! Keyed by a stable STEM, the recipe's file name, not its `name`: two recipes
//! may share a name. The `recipe-rs` gate proves the surface is
//! self-consistent.
//!
//! Each recipe lives in its own self-registering file `src/recipes/<stem>.rs`
//! (github issue #295): the file name IS the stem, `pub fn recipe() -> Recipe`
//! is the registration, and `build.rs` generates the stem-sorted registry
//! (module declarations + the `all()` table) included below. Adding a recipe
//! touches only its new file: no Rust source line is shared, so parallel recipe
//! PRs don't collide on a central table (the mk/gates/ one-file-per-entry property).

use crate::types::Recipe;

/// Look up a recipe by file stem (e.g. "zlib-x86-64").
pub fn lookup(stem: &str) -> Option<Recipe> {
    all().into_iter().find(|(s, _)| *s == stem).map(|(_, r)| r)
}

/// Every migrated recipe, paired with its `.ts` file stem, sorted by stem.
pub fn all() -> Vec<(&'static str, Recipe)> {
    registry::all()
}

/// The `td-*` directories recipe `stem`'s own file names, which is how it
/// embeds crate sources. A change under one of them can change what the
/// recipe builds; so can a change to one of `shared_embeds`, which every
/// recipe reads, and no other change to a crate can, short of the evaluator
/// itself. Read from the sources at build time by `build.rs`, so it is the
/// table of the binary that answers. Empty for an unknown stem.
pub fn named_dirs(stem: &str) -> &'static [&'static str] {
    registry::named_dirs()
        .iter()
        .find(|(s, _)| *s == stem)
        .map_or(&[][..], |(_, dirs)| dirs)
}

/// What this crate's shared modules compile in from `td-*` crates outside
/// their tests, which any recipe may use: repository-relative files, or a
/// whole crate directory where an embedded file reads further files of its
/// own. Sorted.
pub fn shared_embeds() -> &'static [&'static str] {
    registry::shared_embeds()
}

/// The recipes whose evaluation reads repository file `path` under
/// `recipes/` directly: its own recipe and those whose code names its
/// module, for a recipe file; the recipes that embed it, for anything else.
/// None for a path no recipe reads this way, such as a shared module, which
/// every recipe compiles in. Read from the sources by `build.rs`.
pub fn recipe_file_readers(path: &str) -> Option<&'static [&'static str]> {
    registry::recipe_file_readers()
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, stems)| *stems)
}

/// The files under `recipes/` the shared modules and the evaluator read in
/// production code (a recipe module they name, a recipe they look up by a
/// literal name, a file they embed): an edit to one can change any check.
pub fn recipe_evaluator_reads() -> &'static [&'static str] {
    registry::recipe_evaluator_reads()
}

/// The recipes whose code may read any recipe file — by a glob, a group
/// or a catalog lookup by name — and so read every change under `recipes/`.
pub fn recipe_wide_readers() -> &'static [&'static str] {
    registry::recipe_wide_readers()
}

/// The whole `recipe_file_readers` table, path-sorted.
pub fn recipe_file_reader_table() -> &'static [(&'static str, &'static [&'static str])] {
    registry::recipe_file_readers()
}

/// `stem`'s source digest as this binary was compiled: the recipe files and
/// embedded files its evaluation reads, its own and, transitively, those of
/// the recipes whose modules it names. Read from the sources by `build.rs`.
pub fn recipe_source_digest(stem: &str) -> Option<&'static str> {
    registry::recipe_source_digests()
        .iter()
        .find(|(s, _)| *s == stem)
        .map(|(_, d)| *d)
}

/// The outline face's recipe, whose install plan `install-fonts-plan`
/// prints.
pub use registry::jetbrains_mono_nerd_font as outline_face;

mod registry {
    include!(concat!(env!("OUT_DIR"), "/registry.rs"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_debug_consumers_require_only_retained_symbols() {
        let mut compile_directory_consumers = Vec::new();
        for (stem, recipe) in all() {
            for step in recipe.steps.as_deref().unwrap_or_default() {
                let text = match step {
                    crate::types::Step::Run { argv, .. } => argv.join(" "),
                    // Rust source a recipe compiles (td-profiler's reader)
                    // names sections; it reads no companion at build time.
                    crate::types::Step::WriteFile { path, .. }
                        if stem == "td-profiler" && path.ends_with(".rs") =>
                    {
                        continue;
                    }
                    crate::types::Step::WriteFile { content, .. } => content.clone(),
                    _ => continue,
                };
                if text.contains("DW_AT_comp_dir") {
                    assert!(
                        !text.contains("lib/debug"),
                        "{stem}: split debug companion requires DW_AT_comp_dir"
                    );
                    compile_directory_consumers.push(stem);
                }
                if !text.contains("lib/debug") {
                    continue;
                }
                assert!(
                    !text.contains("--debug-dump="),
                    "{stem}: split debug companion carries no DWARF to dump"
                );
                for section in td_engine::target_profile::PRUNED_DEBUG_SECTIONS {
                    assert!(
                        !text.contains(section),
                        "{stem}: split debug companion requires pruned section {section}"
                    );
                }
            }
        }
        compile_directory_consumers.sort_unstable();
        compile_directory_consumers.dedup();
        assert_eq!(
            compile_directory_consumers,
            ["curl-x86-64-test", "libressl-x86-64-test", "zlib-x86-64"]
        );
    }

    #[test]
    fn debian_runtime_replaces_the_source_bootstrap() {
        assert!(lookup("gcc-runtime-stage1").is_none());
        for (stem, kind) in [("glibc-x86-64", "glibc-v2"), ("gcc-runtime", "gcc-v1")] {
            let recipe = lookup(stem).unwrap();
            assert!(recipe.build_root);
            assert!(recipe.source_input.is_none());
            assert!(recipe.native_inputs.is_none());
            assert!(
                matches!(recipe.steps.as_deref(), Some([crate::types::Step::DebianRuntime { kind: actual }]) if actual == kind)
            );
        }
        for key in [
            "gcc-14-source",
            "glibc-241-source",
            "gmp63",
            "mpfr421",
            "mpc131",
        ] {
            assert!(crate::source_pins::all().iter().all(|pin| pin.key != key));
        }
    }

    #[test]
    fn every_recipe_emits_canonical_json_and_round_trips() {
        for (stem, r) in all() {
            let canon = r.to_json().to_canonical();
            // Structural self-consistency: re-parsing the emitted JSON and
            // re-canonicalising yields the same bytes (the durable round-trip).
            let reparsed = crate::json::parse(&canon)
                .unwrap_or_else(|e| panic!("{stem}: emitted invalid JSON: {e}"));
            assert_eq!(reparsed.to_canonical(), canon, "{stem}: not idempotent");
            assert!(
                !r.name.is_empty() && !r.version.is_empty(),
                "{stem}: missing fields"
            );
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
        assert!(
            seed.is_foreign(),
            "the prebuilt source pin must mark its recipe"
        );
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
            "td-audio",
            "td-authd",
            "td-boot",
            "td-btrfs",
            "td-busd",
            "td-compositor",
            "td-firstboot",
            "td-init",
            "td-install",
            "td-install-qemu-test",
            "td-jail",
            "td-kexec",
            "td-link",
            "td-link-runtime",
            "td-link-test",
            "td-login",
            "td-netd",
            "td-photo-test",
            "td-profiler",
            "td-seatd",
            "td-secret",
            "td-secret-vm-test",
            "td-sh",
            "td-svc",
            "td-txt",
            "td-update",
            "td-util",
            "td-vm-guest",
        ];
        let mut covered = Vec::new();
        let mut td_link_stages = Vec::new();
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
                for required in td_engine::target_profile::DIRECT_RUSTC_ARGS
                    .iter()
                    .filter(|arg| !arg.starts_with("--remap-path-prefix="))
                {
                    assert!(
                        argv.iter().any(|arg| arg == required),
                        "{stem}: direct rustc omitted {required}"
                    );
                }
                // td-link links every Rust output; only its own bootstrap
                // names another, and td-link-test names it `cc`, as the
                // image's /bin/cc does.
                let linkers: Vec<&str> = argv
                    .iter()
                    .filter_map(|arg| arg.strip_prefix("-Clinker="))
                    .collect();
                assert_eq!(linkers.len(), 1, "{stem}: one linker per rustc");
                assert!(
                    !argv.iter().any(|arg| arg.contains("linker-features")),
                    "{stem}: td-link is the linker; no lld opt-out"
                );
                let linker = linkers.first().copied().unwrap_or_default();
                match stem {
                    "td-link" => {
                        let output = argv
                            .iter()
                            .position(|arg| arg == "-o")
                            .and_then(|at| argv.get(at + 1))
                            .cloned()
                            .unwrap_or_default();
                        td_link_stages.push((linker.to_string(), output));
                    }
                    "td-link-test" if linker == "cc" => {}
                    _ => assert_eq!(linker, crate::shared::TD_LINK, "{stem}: links with td-link"),
                }
                let build_remaps: Vec<&str> = argv
                    .iter()
                    .filter(|arg| {
                        arg.starts_with("--remap-path-prefix=") && arg.ends_with("=/td-build-root")
                    })
                    .map(String::as_str)
                    .collect();
                let source_remaps: Vec<&str> = argv
                    .iter()
                    .filter(|arg| {
                        arg.starts_with("--remap-path-prefix=") && arg.ends_with("=/td-build")
                    })
                    .map(String::as_str)
                    .collect();
                assert_eq!(build_remaps.len(), 1, "{stem}: build-root remap drifted");
                assert_eq!(source_remaps.len(), 1, "{stem}: source-root remap drifted");
                remap_sources.push((
                    build_remaps
                        .first()
                        .copied()
                        .unwrap_or_default()
                        .to_string(),
                    source_remaps
                        .first()
                        .copied()
                        .unwrap_or_default()
                        .to_string(),
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
            if stem == "td-link" {
                assert!(
                    steps.iter().any(|step| matches!(
                        step,
                        crate::types::Step::CompareFiles { left, right }
                            if left == "{out}/bin/td-link" && right == "{root}/stage3/td-link"
                    )),
                    "td-link: the shipped driver must reproduce itself"
                );
            }
            if stem == "td-boot" {
                // Its td-fido, td-fat, td-tpm, td-json and td-protector
                // rlibs, then the binary, under each of two roots.
                assert_eq!(
                    remap_sources.len(),
                    12,
                    "td-boot must build its libraries and binary at two roots"
                );
                let mut roots = remap_sources.clone();
                roots.sort();
                roots.dedup();
                assert_eq!(roots.len(), 2, "td-boot must build two roots");
                for root in &roots {
                    assert_eq!(
                        remap_sources.iter().filter(|seen| *seen == root).count(),
                        6,
                        "td-boot must build its libraries and binary under each root"
                    );
                }
                let (first, second) = (roots.first(), roots.get(1));
                assert!(
                    first.map(|r| &r.0) != second.map(|r| &r.0)
                        && first.map(|r| &r.1) != second.map(|r| &r.1),
                    "td-boot's reproducibility oracle must vary both remap inputs"
                );
            }
            if stem != "td-link" {
                assert!(
                    recipe
                        .native_inputs
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .any(|input| input == "td-link"),
                    "{stem}: a direct Rust recipe declares td-link, its linker"
                );
            }
            // td-link-test ships no ELF, only its verdict, and runs outside
            // the root to prove td-link needs no GCC or binutils; the others
            // build in the Debian build root and split with its objcopy.
            assert!(
                stem == "td-link-test" || recipe.build_root,
                "{stem}: a direct Rust recipe builds in the build root"
            );
            crate::shared::assert_no_tool_farm(&recipe, &[]);
            let objcopy = "/usr/bin/objcopy";
            assert!(
                stem == "td-link-test"
                    || steps.iter().any(|step| matches!(
                        step,
                        crate::types::Step::SplitDebugTree { root, objcopy: used }
                            if root == "{out}" && used == objcopy
                    )),
                "{stem}: missing target debug-companion split"
            );
        }
        // td-link's bootstrap: only the throwaway first stage is linked by
        // the root's GCC; it links the shipped driver, which links the third
        // stage that CompareFiles holds byte-identical to it.
        assert_eq!(
            td_link_stages,
            [
                ("/usr/bin/gcc", "{root}/stage1/td-link"),
                ("{root}/stage1/td-link", "{out}/bin/td-link"),
                ("{out}/bin/td-link", "{root}/stage3/td-link"),
            ]
            .map(|(linker, output)| (linker.to_string(), output.to_string())),
            "td-link must bootstrap through its own stages"
        );
        assert_eq!(
            covered, expected,
            "the reviewed direct-rustc roster changed"
        );
    }

    /// A Cargo target builds in the Debian build root, whose gcc compiles its
    /// build scripts' C against td glibc's headers and whose objcopy splits
    /// its companions; td-link links it. No tool farm is an input.
    #[test]
    fn every_cargo_target_builds_in_the_root_with_td_link_and_td_glibc() {
        let mut cargo = Vec::new();
        for (stem, recipe) in all() {
            if !matches!(recipe.build_system, crate::types::BuildSystem::Rust) {
                continue;
            }
            cargo.push(stem);
            assert!(
                recipe.build_root,
                "{stem}: Cargo target must declare the build root"
            );
            assert_eq!(
                recipe.native_inputs.as_deref().unwrap_or_default(),
                ["rust-toolchain", "td-link", "glibc-x86-64"],
                "{stem}: a Cargo target's toolchain, linker and C headers"
            );
            crate::shared::assert_no_tool_farm(&recipe, &[]);
            // Only these build scripts compile C, each to an exact count.
            let objects = match stem {
                "td-net" => Some(30),
                "uutils" => Some(48),
                _ => None,
            };
            assert_eq!(recipe.c_objects, objects, "{stem}: build-script C objects");
        }
        cargo.sort_unstable();
        assert_eq!(
            cargo,
            [
                "fd",
                "ripgrep",
                "td-agent",
                "td-dua",
                "td-editor",
                "td-mail",
                "td-net",
                "td-news",
                "td-open",
                "td-pass",
                "td-photo",
                "td-portal",
                "td-review",
                "td-setup",
                "td-taskmgr",
                "td-term",
                "uutils",
            ]
        );
    }

    #[test]
    fn every_assembly_exception_names_a_declared_target_recipe() {
        for &(stem, _) in td_engine::target_profile::ASSEMBLY_EXCEPTIONS {
            let recipe = lookup(stem)
                .unwrap_or_else(|| panic!("assembly exception names missing recipe {stem}"));
            if matches!(stem, "glibc-x86-64" | "gcc-runtime" | "rust-toolchain") {
                continue;
            }
            let generic_cargo_split =
                matches!(recipe.build_system, crate::types::BuildSystem::Rust);
            let typed_split = recipe
                .steps
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|step| matches!(step, crate::types::Step::SplitDebugTree { .. }));
            assert!(
                generic_cargo_split || typed_split,
                "{stem}: assembly exception would not reach the marker-producing splitter"
            );
        }

        assert!(
            td_engine::target_profile::output_assembly_exceptions("td-util")
                .iter()
                .any(|(source, _)| *source == "rust-toolchain")
        );

        let mut rust_outputs: Vec<&str> = all()
            .into_iter()
            .filter_map(|(stem, recipe)| {
                let cargo = matches!(recipe.build_system, crate::types::BuildSystem::Rust);
                let direct = recipe
                    .steps
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .any(|step| {
                        matches!(
                            step,
                            crate::types::Step::Run { argv, .. }
                                if argv.first().is_some_and(|arg| arg.ends_with("/rustc"))
                                    && argv.iter().any(|arg| arg.ends_with(".rs"))
                        )
                    });
                (cargo || direct).then_some(stem)
            })
            .collect();
        rust_outputs.sort_unstable();
        assert_eq!(
            rust_outputs,
            td_engine::target_profile::RUST_PROFILED_RECIPES,
            "the transitive Rust/LLVM assembly-boundary roster changed"
        );
    }

    /// A local source with sibling trees is staged under basenames, so its
    /// `cargo_subdir` must be its own basename; `trees` is a verified-red
    /// lever ONLY for that basename shape (re #469 local-source-roster
    /// split: routing no longer keys off this list at all — every recipe
    /// edit routes by its own file path under `recipes/*`, and every staged
    /// tree's identity is re-derived live, not by name) — a new or renamed
    /// sibling tree must still land here so the `cargo_subdir` assertion
    /// above actually covers it. The roster of LONE local-source crates (no
    /// siblings) is different: it is what td-builder's affected-checks
    /// routes by its own literal list (`local_source_crate`,
    /// builder/src/affected.rs names this test), so the two literals must
    /// agree.
    #[test]
    fn local_source_trees_are_staged_by_basename_and_routed_by_the_builder() {
        let mut trees: Vec<String> = Vec::new();
        let mut lone: Vec<String> = Vec::new();
        for (stem, recipe) in all() {
            let siblings = recipe.local_source_trees.clone().unwrap_or_default();
            let Some(main) = recipe.local_source.clone() else {
                assert!(
                    siblings.is_empty(),
                    "{stem}: local_source_trees without a local_source stage nothing"
                );
                continue;
            };
            if siblings.is_empty() {
                // A tree with no siblings is a crate of its own; the builder
                // routes it by the literal roster in `local_source_crate`.
                lone.push(main);
                continue;
            }
            let basename = std::path::Path::new(&main)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string);
            assert_eq!(
                recipe.cargo_subdir, basename,
                "{stem}: cargo_subdir must name the main tree's basename"
            );
            trees.push(main);
            trees.extend(siblings);
        }
        trees.sort_unstable();
        trees.dedup();
        assert_eq!(
            trees,
            [
                "engine",
                "net",
                "td-agent",
                "td-authd",
                "td-boot",
                "td-busd",
                "td-civil",
                "td-compositor",
                "td-dua",
                "td-editor",
                "td-encoding",
                "td-fat",
                "td-fetch-client",
                "td-fido",
                "td-firstboot",
                "td-fs",
                "td-html",
                "td-install",
                "td-jpeg",
                "td-json",
                "td-kv",
                "td-mail",
                "td-markdown",
                "td-news",
                "td-open",
                "td-pass",
                "td-photo",
                "td-png",
                "td-portal",
                "td-protector",
                "td-regex",
                "td-review",
                "td-secret",
                "td-setup",
                "td-taskmgr",
                "td-term",
                "td-test-compositor",
                "td-toml",
                "td-tpm",
                "td-ui",
                "td-util"
            ]
        );
        lone.sort_unstable();
        lone.dedup();
        assert_eq!(
            lone,
            ["td-install-qemu-test"],
            "a lone local-source crate joined; add it to builder/src/affected.rs local_source_crate"
        );
    }

    /// The post-Rust tool farm links no awk, so a recipe built on it runs
    /// none: the image and the kexec spike need no gawk.
    #[test]
    fn a_post_rust_farm_recipe_runs_no_awk() {
        let mut checked = 0;
        for (stem, recipe) in all() {
            let steps = recipe.steps.as_deref().unwrap_or_default();
            let post_rust_farm = steps.iter().any(|step| match step {
                crate::types::Step::ToolFarm { links } => {
                    assert!(
                        !links.iter().any(|(name, _)| name == "awk"),
                        "{stem}: a tool farm links awk"
                    );
                    links
                        .iter()
                        .any(|(_, target)| target.contains("{in:td-sh}"))
                }
                _ => false,
            });
            if !post_rust_farm {
                continue;
            }
            checked += 1;
            for step in steps {
                if let crate::types::Step::Run { argv, .. } = step {
                    // The image's shape check names awk among the applets
                    // it proves absent; nothing else may.
                    assert!(
                        !argv
                            .iter()
                            .any(|arg| arg.replace("in vi more awk;", "").contains("awk")),
                        "{stem} runs awk with a farm that has none: {argv:?}"
                    );
                }
            }
        }
        assert!(
            checked >= 3,
            "system-x86-64 and the kexec spikes: {checked}"
        );
    }

    /// A build-root C recipe that splits companions links code that ships,
    /// and takes its start files and libgcc from gcc-runtime;
    /// the profiler's boundary lists name exactly
    /// those recipes. Direct Rust recipes, td-link's own aside, are
    /// `RUST_PROFILED_RECIPES`' to name.
    #[test]
    fn build_root_linked_outputs_match_the_profile_boundary() {
        let mut linked: Vec<&str> = all()
            .into_iter()
            .filter(|(stem, recipe)| {
                recipe.build_root
                    && (*stem == "td-link"
                        || !td_engine::target_profile::RUST_PROFILED_RECIPES.contains(stem))
                    && recipe
                        .steps
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .any(|step| matches!(step, crate::types::Step::SplitDebugTree { .. }))
            })
            .map(|(stem, _)| stem)
            .collect();
        linked.sort_unstable();
        let mut expected = td_engine::target_profile::BUILD_ROOT_LINKED.to_vec();
        expected.sort_unstable();
        assert_eq!(linked, expected);
    }

    #[test]
    fn the_deployment_has_an_external_debug_ceiling() {
        for (stem, expected_scope, expected_report, expected_ceiling) in [
            (
                "system-x86-64",
                "deployment",
                "{out}/deployment/debug-size",
                td_engine::target_profile::DEPLOYMENT_DEBUG_CEILING_BYTES,
            ),
            (
                "server-x86-64",
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
                [(expected_scope, expected_report, expected_ceiling,)],
                "{stem}: debug measurement must use its reviewed scope ceiling"
            );
        }
    }

    #[test]
    fn source_built_runtime_toolchain_keeps_frames_lines_ids_and_remapped_paths() {
        for (stem, expected_tail_uses) in [("zlib-x86-64", 1)] {
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
    }
}

#[cfg(test)]
mod named_dirs_tests {
    use super::*;

    /// A recipe's own spellings make its directories; portal's local-source
    /// siblings do not create embed edges. What the shared modules compile
    /// in is kept apart, by file, and leaves out the test-only mounts of
    /// td-busd's app policy and td-authd's primary account: those are in no
    /// evaluator binary. Paths are assembled here, since this file is under
    /// the build script's own scan.
    #[test]
    fn named_dirs_carry_the_embeds_the_recipes_spell() {
        let portal = named_dirs("td-portal");
        for dir in ["td-authd", "td-portal"] {
            assert!(portal.contains(&dir), "td-portal: {portal:?}");
        }
        assert!(
            !portal.contains(&"td-secret"),
            "local-source sibling is not an embed"
        );
        assert!(named_dirs("td-sh").contains(&"td-sh"));
        let file = |dir: &str, rest: &str| [dir, "/", rest].concat();
        assert_eq!(
            shared_embeds(),
            [
                file("td-boot", "src/protocol.rs"),
                file("td-civil", "src/tzif.rs"),
                file("td-fs", "src/real_file.rs"),
                file("td-install-qemu-test", "src/protocol.rs"),
                file("td-install", "src/timezones.rs"),
                file("td-profiler", "src/contract.rs"),
                file("td-update", "src/upstream.rs"),
            ],
            "the shared modules' production embeds, by file"
        );
        assert!(shared_embeds()
            .iter()
            .all(|e| !e.starts_with("td-busd") && !e.starts_with("td-authd")));
        for (stem, _) in all() {
            let dirs = named_dirs(stem);
            assert!(
                dirs.iter().zip(dirs.iter().skip(1)).all(|(a, b)| a < b),
                "{stem}: {dirs:?}"
            );
            assert!(
                dirs.iter().all(|d| d.starts_with("td-")),
                "{stem}: {dirs:?}"
            );
        }
        assert!(named_dirs("no-such-recipe").is_empty());
    }

    /// The evaluator's check-content part holds what a check asserts or
    /// builds from though every key holds it — a check body, the source
    /// pins, a recipe file the evaluator reads, a crate file a shared module
    /// compiles in (tzdata#1 asserts td-civil's TZif reader) — and its
    /// shared code stays out of it, since a branch run may defer a change to
    /// that alone.
    #[test]
    fn the_check_content_part_of_the_evaluator_stands_apart() {
        let checks = registry::evaluator_check_fingerprint_files();
        let all = registry::evaluator_fingerprint_files();
        for content in [
            "recipes/src/bin/td_recipe_eval/checks/tzdata.rs",
            "recipes/src/bin/td_recipe_eval/checks/rust_toolchain.rs",
            "recipes/src/recipes/jetbrains-mono-nerd-font.rs",
            "recipes/src/source_pins.rs",
        ] {
            assert!(checks.contains(&content), "{content} not check content");
        }
        // Every crate file a shared module compiles in, a directory entry by
        // its sources.
        assert!(!shared_embeds().is_empty());
        for embed in shared_embeds() {
            let content = if embed.contains('/') {
                checks.contains(embed)
            } else {
                let src = format!("{embed}/src/");
                checks.iter().any(|f| f.starts_with(&src))
            };
            assert!(content, "shared embed {embed} not check content");
        }
        for shared in [
            "recipes/src/types.rs",
            "recipes/src/catalog.rs",
            "recipes/src/bin/td_recipe_eval/check_runner.rs",
            "engine/src/sha256.rs",
        ] {
            assert!(all.contains(&shared), "{shared} not fingerprinted");
            assert!(!checks.contains(&shared), "{shared} is check content");
        }
        // The boot harnesses are the oracles' alone.
        assert!(!checks.contains(&"recipes/src/bin/td_recipe_eval/checks/qemu_boot.rs"));
        assert!(checks.iter().all(|f| all.contains(f)));
    }

    /// Every check's verdict key holds the evaluator fingerprint, so what
    /// every recipe compiles in belongs to it: the shared modules, each
    /// shared embed (a directory by its sources), and a recipe file the
    /// evaluator's own code reads. Any other recipe file, and a file only
    /// recipes embed, keys a check by the source digest of each recipe in
    /// its closure, which changes with it and with what it names.
    #[test]
    fn the_fingerprint_holds_the_shared_embeds() {
        let files = registry::evaluator_fingerprint_files();
        for kept in [
            "recipes/src/types.rs",
            "recipes/src/catalog.rs",
            "recipes/src/probes/rust_clippy.rs",
            "recipes/src/recipes/jetbrains-mono-nerd-font.rs",
            "engine/src/sha256.rs",
        ] {
            assert!(files.contains(&kept), "{kept} not fingerprinted");
        }
        for per_recipe in [
            "recipes/src/recipes/zlib-x86-64.rs",
            "recipes/src/recipes/uutils.rs",
        ] {
            assert!(!files.contains(&per_recipe), "{per_recipe} fingerprinted");
        }
        let digest = |s| recipe_source_digest(s).expect(s);
        assert_ne!(digest("uutils"), digest("fd"));
        assert_eq!(digest("uutils").len(), 64);
        // Every recipe has one, and none is the empty set's: each reads at
        // least its own file.
        const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        for (stem, _) in all() {
            assert!(
                recipe_source_digest(stem).is_some_and(|d| d != EMPTY),
                "{stem}"
            );
        }
        assert!(!shared_embeds().is_empty());
        for embed in shared_embeds() {
            let found = if embed.contains('/') {
                files.contains(embed)
            } else {
                let src = format!("{embed}/src/");
                files.iter().any(|f| f.starts_with(&src))
            };
            assert!(found, "shared embed {embed} not fingerprinted");
        }
    }

    /// The evaluator's own sources under `src/bin/` are outside the shared
    /// scan, on the ground that they embed crate files only in their test
    /// modules: a runtime include there would change a check's assertions
    /// without changing any recipe, and neither the scope nor the verdict
    /// key would see it. So every include naming `td-` must lie inside a
    /// top-level `#[cfg(test)] mod`, found by its column-0 attribute over
    /// `mod x {` and closed by brace count outside literals and comments —
    /// not merely after the file's first `#[cfg(test)]`, which qemu_boot.rs
    /// puts on single items in the middle of production code; a file mount
    /// directly under its own `#[cfg(test)]` onto `mod x;` is test-only as
    /// well. The test modules' own embeds prove the spans are found.
    ///
    /// The one exception is a line that is exactly `#[path = "..."]`
    /// naming a file in `build.rs`'s own-source list, read with comments
    /// stripped, so the oracles' verdict key holds its bytes (as
    /// `host-evaluator`, `HOST_CHECK_SOURCES`): the face and Unifont
    /// modules the screen oracles draw the compositor's chrome text and
    /// td-setup's recovery key with. Only `qemu_boot/update.rs` and
    /// `live.rs`, which serve `qemu-boot-live`, and `recovery_screen.rs` and
    /// `encrypted_boot.rs`, which serve `qemu-boot-encrypted`, may name
    /// them. Both are commands rather than recipe checks, run on demand
    /// outside the integration tier as their own `td-recipe-eval` verbs,
    /// and qemu_boot is a boot harness no recipe check runs
    /// (`no_recipe_check_runs_a_boot_harness`), so no check scope is owed.
    #[test]
    fn the_evaluator_embeds_crate_files_only_in_its_test_modules() {
        use td_engine::embed_scan::{block_end, strip_comments};
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = manifest
            .parent()
            .expect("repository root")
            .canonicalize()
            .expect("canonical root");
        let build =
            strip_comments(&std::fs::read_to_string(manifest.join("build.rs")).expect("build.rs"));
        let list = build
            .find("let mut files: Vec<PathBuf> = [")
            .and_then(|start| {
                let end = start + build.get(start..)?.find("\n    ]")?;
                build.get(start..end)
            })
            .expect("build.rs's own-source list");
        let fingerprinted: Vec<&str> = list.split('"').skip(1).step_by(2).collect();
        let mounts = [
            "crate::atlas",
            "crate::coverage",
            "crate::face",
            "crate::font",
            "crate::sfnt",
        ];
        let bin = manifest.join("src/bin");
        let mut pending = vec![bin];
        let (mut seen, mut in_tests, mut mounted) = (0usize, 0usize, 0usize);
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("list src/bin") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let code = strip_comments(&std::fs::read_to_string(&path).expect("read"));
                let oracle = [
                    "update.rs",
                    "live.rs",
                    "recovery_screen.rs",
                    "encrypted_boot.rs",
                ]
                .iter()
                .any(|name| path.ends_with(format!("checks/qemu_boot/{name}")));
                for module in mounts {
                    assert!(
                        oracle || !code.contains(module),
                        "{}: names {module}, mounted for the screen oracles only",
                        path.display()
                    );
                }
                let mut spans: Vec<(usize, usize)> = Vec::new();
                let mut offset = 0usize;
                let mut after_attr = false;
                for line in code.split_inclusive('\n') {
                    let text = line.trim_end();
                    if after_attr && text.starts_with("mod ") && text.ends_with('{') {
                        let open = offset + line.rfind('{').expect("brace");
                        let close = block_end(&code, open).unwrap_or_else(|| {
                            panic!(
                                "{}: test module at byte {open} never closes",
                                path.display()
                            )
                        });
                        spans.push((open, close));
                    }
                    after_attr = text == "#[cfg(test)]";
                    offset += line.len();
                }
                let lines: Vec<&str> = code.split_inclusive('\n').collect();
                let mut offset = 0usize;
                for (n, line) in lines.iter().enumerate() {
                    // A file mount under its own `#[cfg(test)]`, onto `mod x;`.
                    let test_mount = line.trim_start().starts_with("#[path")
                        && n.checked_sub(1)
                            .and_then(|before| lines.get(before))
                            .is_some_and(|before| before.trim_end() == "#[cfg(test)]")
                        && lines.get(n + 1).is_some_and(|after| {
                            let after = after.trim_end();
                            after.starts_with("mod ") && after.ends_with(';')
                        });
                    let embeds = line.contains("include_str!")
                        || line.contains("include_bytes!")
                        || line.contains("#[path");
                    // The whole line is the mount, so nothing else rides on it.
                    let fingerprinted = line
                        .trim()
                        .strip_prefix("#[path = \"")
                        .and_then(|rest| rest.strip_suffix("\"]"))
                        .filter(|rel| !rel.contains('"'))
                        .and_then(|rel| path.parent()?.join(rel).canonicalize().ok())
                        .and_then(|file| {
                            file.strip_prefix(&root)
                                .ok()?
                                .to_str()
                                .map(|rel| fingerprinted.contains(&rel))
                        })
                        .unwrap_or(false);
                    let in_span = spans
                        .iter()
                        .any(|(open, close)| (*open..*close).contains(&offset));
                    if embeds && line.contains("td-") && fingerprinted {
                        mounted += 1;
                    } else if embeds && line.contains("td-") {
                        assert!(
                            test_mount || in_span,
                            "{}:{}: embed of a crate file outside a test module: {}",
                            path.display(),
                            n + 1,
                            line.trim()
                        );
                        in_tests += usize::from(in_span);
                    }
                    offset += line.len();
                }
                seen += 1;
            }
        }
        assert!(seen > 3, "scanned {seen} evaluator sources");
        assert!(
            in_tests >= 4,
            "the test modules' own embeds prove the spans: {in_tests}"
        );
        assert_eq!(
            mounted, 9,
            "the fingerprinted face, Unifont and server-account mounts"
        );
    }
}
