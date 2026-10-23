use crate::application::ApplicationDeclaration;
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};
use td_engine::application_spec::dynamic_application_policy;
use td_engine::launcher::LauncherDeclaration;
use td_engine::permissions::{FilesystemAccess, PermissionPolicy, PermissionSocket};

/// The runtime policy the declaration names. The runtime itself is the
/// Flathub ref td-app fetches into the account's cache (APPLICATIONS.md
/// §W.9), and td-app validates this package against it at admission.
const RUNTIME: &str = "freedesktop-platform-25-08";
const ENTRY: &str = "/app/bin/claude";
const VERSION: &str = "latest";

/// Memory and process ceilings for a coding agent: the native binary is a
/// 215 MB Bun executable that spawns shells, build tools and subagents, so the
/// 1 GiB / 1024-process defaults sized for a fixture would stop it mid-task.
const MEMORY_HIGH_BYTES: u64 = 3 * 1024 * 1024 * 1024;
const MEMORY_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const PIDS_MAX: u32 = 2048;

/// A declaration of Anthropic's signed native release stream. td-app fetches
/// and validates the binary and Flathub runtime before the jail launches it.
/// The program's own updater stays disabled; only td-app publishes executable
/// generations outside the jail's noexec private home.
pub fn recipe() -> Recipe {
    if dynamic_application_policy("claude", RUNTIME).is_none() {
        return invalid_recipe("dynamic-policy");
    }
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
    let Ok(launcher) = LauncherDeclaration::new("Claude Code", &["claude", "code", "agent", "ai"])
    else {
        return invalid_recipe("launcher");
    };
    let Ok(permissions) = PermissionPolicy::new()
        .with_network()
        // The compiler requires a Wayland socket of every application; a
        // terminal program opens no window, and the read-only socket bind
        // grants nothing it will use.
        .and_then(|value| value.with_socket(PermissionSocket::Wayland))
        // The fresh-terminal grant: stage 1 acquires the pseudo-terminal on
        // its own stdio and the entry gets three clones of it, or the launch
        // is refused with a diagnostic naming td-term's --command.
        .and_then(|value| value.with_terminal())
        .and_then(|value| value.with_filesystem("~/src", FilesystemAccess::ReadWrite, true))
        .and_then(|value| value.with_memory_high(MEMORY_HIGH_BYTES))
        .and_then(|value| value.with_memory_max(MEMORY_MAX_BYTES))
        .and_then(|value| value.with_pids_max(PIDS_MAX))
    else {
        return invalid_recipe("permissions");
    };
    Recipe::mesboot("claude", VERSION)
        .steps(vec![])
        .application(declaration)
        .application_launcher(launcher)
        .application_permissions(permissions)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check claude: compile the signed-release declaration without fetching payloads"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run claude 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

fn invalid_recipe(field: &str) -> Recipe {
    Recipe::mesboot("claude", VERSION).steps(vec![Step::Require {
        paths: vec![format!("{{out}}/invalid-claude-{field}")],
        exec: false,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaration_fetches_both_binary_and_runtime() {
        let recipe = recipe();
        assert!(recipe.is_foreign());
        assert!(!recipe.is_foreign_source());
        assert!(recipe.source_input.is_none());
        // No build stages the runtime: td-app fetches the declared ref.
        assert_eq!(recipe.payload_inputs, None);
        let source = td_engine::flathub_app::flathub_source("claude").expect("declared");
        assert_eq!(recipe.flathub_runtime(), Some(source.runtime));
        assert_eq!(
            source.application,
            Some(td_engine::flathub_app::CLAUDE_RELEASE_REF)
        );
        assert_eq!(source.entry, ENTRY);
        let declaration = recipe.application.as_ref().expect("declaration");
        assert_eq!(declaration.runtime(), RUNTIME);
        assert_eq!(declaration.entry(), ENTRY);
        assert_eq!(declaration.alias(), None);
        assert_eq!(
            declaration.environment().collect::<Vec<_>>(),
            vec![("DISABLE_UPDATES", "1"), ("SHELL", "/usr/bin/bash")]
        );
        assert!(recipe.steps.as_deref().is_some_and(<[Step]>::is_empty));
        // The policy td-app's validator closes against names no package
        // library root: the whole loader graph is the runtime's.
        let policy = dynamic_application_policy("claude", RUNTIME).expect("dynamic policy");
        assert!(policy.library_paths().is_empty());
        assert!(policy.optional_targets().is_empty());
        assert_eq!(policy.optional_links(), 0);
    }

    #[test]
    fn the_application_requests_a_terminal_and_a_project_tree_and_no_bus() {
        let recipe = recipe();
        let launcher = recipe.application_launcher.as_ref().expect("launcher");
        assert_eq!(launcher.display_name(), "Claude Code");
        let policy = recipe
            .application_permissions
            .expect("permission policy")
            .to_keyfile();
        assert_eq!(
            policy,
            "format=1\n\n[Context]\nshared=network\nsockets=wayland\ndevices=tty\n\n[Filesystem]\n~/src=rw:create\n\n[Resources]\nmemory-high=3221225472\nmemory-max=4294967296\npids-max=2048\n"
        );
    }
}
