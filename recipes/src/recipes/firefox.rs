use crate::application::ApplicationDeclaration;
use crate::types::{Recipe, Step};
use td_engine::launcher::LauncherDeclaration;
use td_engine::permissions::{BusAccess, FilesystemAccess, PermissionPolicy, PermissionSocket};
use td_engine::application_spec::dynamic_application_policy;

const RUNTIME: &str = "freedesktop-platform-25-08";
const ENTRY: &str = "/app/bin/firefox";

/// The exact Flathub Firefox deploy, admitted only after the builder has
/// resolved its launcher, every application ELF edge, and the selected 25.08
/// runtime without executing imported bytes.
pub fn recipe() -> Recipe {
    let Some(dynamic_policy) = dynamic_application_policy("firefox", RUNTIME) else {
        return invalid_recipe("dynamic-policy");
    };
    let Ok(declaration) = ApplicationDeclaration::new(RUNTIME, ENTRY)
        .and_then(|value| value.with_alias("org.mozilla.firefox"))
        .and_then(|value| value.with_environment("MOZ_ENABLE_WAYLAND", "1"))
    else {
        return invalid_recipe("declaration");
    };
    let Ok(launcher) =
        LauncherDeclaration::new("Firefox", &["firefox", "browser", "web", "internet"])
    else {
        return invalid_recipe("launcher");
    };
    let Ok(permissions) = PermissionPolicy::new()
        .with_network()
        .and_then(|value| value.with_socket(PermissionSocket::Wayland))
        .and_then(|value| value.with_filesystem("xdg-download", FilesystemAccess::ReadWrite, true))
        .and_then(|value| value.with_session_bus("org.mozilla.firefox", BusAccess::Own))
    else {
        return invalid_recipe("permissions");
    };
    Recipe::mesboot("firefox", "154.0")
        .source_input("firefox-154-source")
        .payload_inputs(&[RUNTIME])
        .steps(vec![
            Step::CopyTree {
                from: "{payload:firefox-source}".into(),
                dest: "{out}/files".into(),
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
}

fn invalid_recipe(field: &str) -> Recipe {
    Recipe::mesboot("firefox", "154.0")
        .source_input("firefox-154-source")
        .payload_inputs(&[RUNTIME])
        .steps(vec![Step::Require {
            paths: vec![format!("{{out}}/invalid-firefox-{field}")],
            exec: false,
        }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_is_bound_to_one_runtime_and_terminal_validator() {
        let recipe = recipe();
        assert!(recipe.is_foreign());
        assert!(recipe.is_foreign_source());
        assert_eq!(recipe.source_input.as_deref(), Some("firefox-154-source"));
        assert_eq!(recipe.payload_inputs, Some(vec![RUNTIME.into()]));
        let declaration = recipe.application.as_ref().expect("declaration");
        assert_eq!(declaration.runtime(), RUNTIME);
        assert_eq!(declaration.entry(), ENTRY);
        assert_eq!(declaration.alias(), Some("org.mozilla.firefox"));
        assert_eq!(
            declaration.environment().collect::<Vec<_>>(),
            vec![("MOZ_ENABLE_WAYLAND", "1")]
        );
        assert!(matches!(
            recipe.steps.as_deref(),
            Some([
                Step::CopyTree { from, dest },
                Step::ValidateDynamicApplication {
                    entry,
                    runtime,
                    library_paths,
                    optional_targets,
                    optional_links,
                }
            ])
                if from == "{payload:firefox-source}"
                    && dest == "{out}/files"
                    && entry == ENTRY
                    && runtime == RUNTIME
                    && library_paths == dynamic_application_policy("firefox", RUNTIME)
                        .expect("dynamic policy")
                        .library_paths()
                    && optional_targets == dynamic_application_policy("firefox", RUNTIME)
                        .expect("dynamic policy")
                        .optional_targets()
                    && *optional_links == 102
        ));
    }

    #[test]
    fn first_policy_is_wayland_network_download_and_one_bus_name() {
        let policy = recipe()
            .application_permissions
            .expect("permission policy")
            .to_keyfile();
        assert_eq!(
            policy,
            "format=1\n\n[Context]\nshared=network\nsockets=wayland\n\n[Filesystem]\nxdg-download=rw:create\n\n[Session Bus Policy]\norg.mozilla.firefox=own\n"
        );
    }
}
