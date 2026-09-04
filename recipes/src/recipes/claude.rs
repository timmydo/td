use crate::application::ApplicationDeclaration;
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};
use td_engine::application_spec::dynamic_application_policy;
use td_engine::launcher::LauncherDeclaration;
use td_engine::permissions::{FilesystemAccess, PermissionPolicy, PermissionSocket};

const RUNTIME: &str = "freedesktop-platform-25-08";
const ENTRY: &str = "/app/bin/claude";
const VERSION: &str = "2.1.260";

/// Memory and process ceilings for a coding agent: the native binary is a
/// 215 MB Bun executable that spawns shells, build tools and subagents, so the
/// 1 GiB / 1024-process defaults sized for a fixture would stop it mid-task.
const MEMORY_HIGH_BYTES: u64 = 3 * 1024 * 1024 * 1024;
const MEMORY_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const PIDS_MAX: u32 = 2048;

/// The exact upstream Claude Code native release, admitted as a marked payload
/// and placed at its entry name without being executed. The vendor publishes
/// each version as one glibc-dynamic ELF beside a manifest of SHA-256 sums, so
/// the pin is checkable against upstream's own hash. This increment packages
/// the seed only: it requests no terminal and is not yet selected into the
/// image, and its updater is disabled because nothing in the private home may
/// execute (APPLICATIONS.md §C). Later increments add the terminal grant, the
/// caller's working directory, image selection, and the executable state
/// subtree that lets the application update itself.
pub fn recipe() -> Recipe {
    let Some(dynamic_policy) = dynamic_application_policy("claude", RUNTIME) else {
        return invalid_recipe("dynamic-policy");
    };
    let Ok(declaration) = ApplicationDeclaration::new(RUNTIME, ENTRY)
        // The runtime's /bin/sh is bash, but Claude Code's Bash tool reads
        // SHELL and wants bash or zsh by name.
        .and_then(|value| value.with_environment("SHELL", "/usr/bin/bash"))
        // Every update path stays off until the executable state subtree
        // exists; the private home is noexec, so an installed update could
        // not run and would only fill the state directory.
        .and_then(|value| value.with_environment("DISABLE_UPDATES", "1"))
    else {
        return invalid_recipe("declaration");
    };
    let Ok(launcher) =
        LauncherDeclaration::new("Claude Code", &["claude", "code", "agent", "ai"])
    else {
        return invalid_recipe("launcher");
    };
    let Ok(permissions) = PermissionPolicy::new()
        .with_network()
        // The compiler requires a Wayland socket of every application; a
        // terminal program opens no window, and the read-only socket bind
        // grants nothing it will use.
        .and_then(|value| value.with_socket(PermissionSocket::Wayland))
        .and_then(|value| value.with_filesystem("~/src", FilesystemAccess::ReadWrite, true))
        .and_then(|value| value.with_memory_high(MEMORY_HIGH_BYTES))
        .and_then(|value| value.with_memory_max(MEMORY_MAX_BYTES))
        .and_then(|value| value.with_pids_max(PIDS_MAX))
    else {
        return invalid_recipe("permissions");
    };
    Recipe::mesboot("claude", VERSION)
        .source_input("claude-code-source")
        .payload_inputs(&[RUNTIME])
        .steps(vec![
            // `/app` is the package's `files/` root, so the entry lands at
            // `files/bin/claude`.
            Step::CopyFile {
                file: "{payload:claude-source}".into(),
                to: "{out}/files/bin/claude".into(),
                exec: true,
            },
            Step::validate_dynamic_application(
                &declaration,
                dynamic_policy.library_paths(),
                dynamic_policy.optional_targets(),
                dynamic_policy.optional_links(),
            ),
        ])
        .application(declaration)
        .application_launcher(launcher)
        .application_permissions(permissions)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check claude: package the pinned Claude Code payload without executing it"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run claude 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

fn invalid_recipe(field: &str) -> Recipe {
    Recipe::mesboot("claude", VERSION)
        .source_input("claude-code-source")
        .payload_inputs(&[RUNTIME])
        .steps(vec![Step::Require {
            paths: vec![format!("{{out}}/invalid-claude-{field}")],
            exec: false,
        }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_is_placed_at_its_entry_and_bound_to_one_runtime_validator() {
        let recipe = recipe();
        assert!(recipe.is_foreign());
        assert!(recipe.is_foreign_source());
        assert_eq!(recipe.source_input.as_deref(), Some("claude-code-source"));
        assert_eq!(recipe.payload_inputs, Some(vec![RUNTIME.into()]));
        let declaration = recipe.application.as_ref().expect("declaration");
        assert_eq!(declaration.runtime(), RUNTIME);
        assert_eq!(declaration.entry(), ENTRY);
        assert_eq!(declaration.alias(), None);
        assert_eq!(
            declaration.environment().collect::<Vec<_>>(),
            vec![("DISABLE_UPDATES", "1"), ("SHELL", "/usr/bin/bash")]
        );
        assert!(matches!(
            recipe.steps.as_deref(),
            Some([
                Step::CopyFile { file, to, exec: true },
                Step::ValidateDynamicApplication {
                    entry,
                    runtime,
                    library_paths,
                    optional_targets,
                    optional_links: 0,
                },
            ]) if file == "{payload:claude-source}"
                && to == "{out}/files/bin/claude"
                && entry == ENTRY
                && runtime == RUNTIME
                && library_paths == dynamic_application_policy("claude", RUNTIME)
                    .expect("dynamic policy")
                    .library_paths()
                && optional_targets == dynamic_application_policy("claude", RUNTIME)
                    .expect("dynamic policy")
                    .optional_targets()
        ));
        // The policy the validator closes against is the one that names no
        // package library root: the whole loader graph is the runtime's.
        let policy = dynamic_application_policy("claude", RUNTIME).expect("dynamic policy");
        assert!(policy.library_paths().is_empty());
        assert!(policy.optional_targets().is_empty());
        assert_eq!(policy.optional_links(), 0);
    }

    #[test]
    fn the_seed_requests_a_project_tree_and_no_bus_or_update_path() {
        let recipe = recipe();
        let launcher = recipe.application_launcher.as_ref().expect("launcher");
        assert_eq!(launcher.display_name(), "Claude Code");
        let policy = recipe
            .application_permissions
            .expect("permission policy")
            .to_keyfile();
        assert_eq!(
            policy,
            "format=1\n\n[Context]\nshared=network\nsockets=wayland\n\n[Filesystem]\n~/src=rw:create\n\n[Resources]\nmemory-high=3221225472\nmemory-max=4294967296\npids-max=2048\n"
        );
    }
}
