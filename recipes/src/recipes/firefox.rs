use crate::application::ApplicationDeclaration;
use crate::types::{Recipe, Step};
use td_engine::application_spec::dynamic_application_policy;
use td_engine::launcher::LauncherDeclaration;
use td_engine::permissions::{BusAccess, FilesystemAccess, PermissionPolicy, PermissionSocket};

/// The runtime policy the declaration names; td-app holds the fetched
/// runtime to it at admission.
const RUNTIME: &str = "freedesktop-platform-25-08";
const ENTRY: &str = "/app/bin/firefox";
/// The branch of the app ref it follows; the version itself floats with
/// Flathub (APPLICATIONS.md §W.9).
const VERSION: &str = "stable";

/// Flathub's Firefox, as a declaration alone: the package carries its
/// manifest, spec and launcher row, the spec names the app and runtime refs
/// in td-engine's Flathub table, and the account's td-app fetches, verifies
/// and validates both at its first launch (APPLICATIONS.md §W.9). No byte of
/// it is in the image, so the recipe has no source, no payload and no steps.
pub fn recipe() -> Recipe {
    if dynamic_application_policy("firefox", RUNTIME).is_none() {
        return invalid_recipe("dynamic-policy");
    }
    let Ok(declaration) = ApplicationDeclaration::new(RUNTIME, ENTRY)
        .and_then(|value| value.with_alias("org.mozilla.firefox"))
        .and_then(|value| value.with_environment("GTK_USE_PORTAL", "1"))
        .and_then(|value| value.with_environment("MOZ_DISABLE_AUTO_SAFE_MODE", "1"))
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
        .and_then(|value| value.with_socket(PermissionSocket::PulseAudio))
        .and_then(|value| value.with_filesystem("xdg-download", FilesystemAccess::ReadWrite, true))
        // The files the portal's OpenURI.OpenFile copies for Firefox to open,
        // prepared by td-authd's portal handoff (APPLICATIONS.md §E).
        .and_then(|value| value.with_filesystem("~/Opened", FilesystemAccess::ReadOnly, false))
        .and_then(|value| value.with_session_bus("org.mozilla.firefox", BusAccess::Own))
        // The remote-control name appends an encoding of the profile; the
        // portal's OpenURI hands links to it (APPLICATIONS.md §E).
        .and_then(|value| value.with_session_bus("org.mozilla.firefox.*", BusAccess::Own))
    else {
        return invalid_recipe("permissions");
    };
    Recipe::mesboot("firefox", VERSION)
        .steps(Vec::new())
        .application(declaration)
        .application_launcher(launcher)
        .application_permissions(permissions)
}

fn invalid_recipe(field: &str) -> Recipe {
    Recipe::mesboot("firefox", VERSION).steps(vec![Step::Require {
        paths: vec![format!("{{out}}/invalid-firefox-{field}")],
        exec: false,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_package_is_a_declaration_whose_refs_td_app_fetches() {
        let recipe = recipe();
        let source = td_engine::flathub_app::flathub_source("firefox").expect("declared");
        assert_eq!(recipe.flathub_runtime(), Some(source.runtime));
        assert_eq!(source.application, "app/org.mozilla.firefox/x86_64/stable");
        assert_eq!(source.policy_runtime, RUNTIME);
        assert!(recipe.is_foreign());
        assert!(!recipe.is_foreign_source());
        assert_eq!(recipe.source_input, None);
        assert_eq!(recipe.payload_inputs, None);
        assert!(recipe.steps.as_deref().is_some_and(<[Step]>::is_empty));
        let declaration = recipe.application.as_ref().expect("declaration");
        assert_eq!(declaration.runtime(), RUNTIME);
        assert_eq!(declaration.entry(), ENTRY);
        assert_eq!(declaration.entry(), source.entry);
        assert_eq!(declaration.alias(), Some("org.mozilla.firefox"));
        assert_eq!(
            declaration.environment().collect::<Vec<_>>(),
            vec![
                ("GTK_USE_PORTAL", "1"),
                ("MOZ_DISABLE_AUTO_SAFE_MODE", "1"),
                ("MOZ_ENABLE_WAYLAND", "1"),
            ]
        );
        assert!(dynamic_application_policy("firefox", RUNTIME).is_some());
    }

    #[test]
    fn first_policy_is_wayland_pulse_network_download_handoff_and_the_firefox_bus_names() {
        let policy = recipe()
            .application_permissions
            .expect("permission policy")
            .to_keyfile();
        assert_eq!(
            policy,
            "format=1\n\n[Context]\nshared=network\nsockets=wayland;pulseaudio\n\n[Filesystem]\nxdg-download=rw:create\n~/Opened=ro\n\n[Session Bus Policy]\norg.mozilla.firefox=own\norg.mozilla.firefox.*=own\n"
        );
    }
}
