use crate::application::ApplicationDeclaration;
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};
use td_engine::launcher::LauncherDeclaration;
use td_engine::permissions::{FilesystemAccess, PermissionPolicy, PermissionSocket};

const APPLICATION_ENTRY: &str = crate::ladder::TD_JAIL_FIXTURE_ENTRY;
const APPLICATION_ALIAS: &str = crate::ladder::TD_JAIL_FIXTURE_ALIAS;
const APPLICATION_NAME: &str = crate::ladder::TD_JAIL_FIXTURE_NAME;
const APPLICATION_DISPLAY_NAME: &str = crate::ladder::TD_JAIL_FIXTURE_DISPLAY_NAME;
const APPLICATION_SEARCH_TERMS: &[&str] = crate::ladder::TD_JAIL_FIXTURE_SEARCH_TERMS;

pub fn recipe() -> Recipe {
    let Ok(declaration) = ApplicationDeclaration::new("empty-runtime", APPLICATION_ENTRY)
        .and_then(|declaration| declaration.with_alias(APPLICATION_ALIAS))
    else {
        return invalid_recipe("declaration");
    };
    let Ok(launcher) =
        LauncherDeclaration::new(APPLICATION_DISPLAY_NAME, APPLICATION_SEARCH_TERMS)
    else {
        return invalid_recipe("launcher");
    };
    let Ok(permissions) = PermissionPolicy::new()
        .with_socket(PermissionSocket::Wayland)
        .and_then(|permissions| {
            permissions.with_filesystem(
                crate::ladder::TD_JAIL_FIXTURE_DOWNLOAD_PERMISSION,
                FilesystemAccess::ReadWrite,
                true,
            )
        })
        .and_then(|permissions| {
            permissions.with_filesystem(
                crate::ladder::TD_JAIL_FIXTURE_PICTURES_PERMISSION,
                FilesystemAccess::ReadOnly,
                true,
            )
        })
        .and_then(|permissions| {
            permissions.with_filesystem(
                crate::ladder::TD_JAIL_FIXTURE_GRANT_FILE,
                FilesystemAccess::ReadOnly,
                false,
            )
        })
        .and_then(|permissions| {
            permissions.with_filesystem(
                crate::ladder::TD_JAIL_FIXTURE_GRANT_ROOT,
                FilesystemAccess::ReadOnly,
                false,
            )
        })
        .and_then(|permissions| permissions.with_memory_high(48 * 1024 * 1024))
        .and_then(|permissions| permissions.with_memory_max(64 * 1024 * 1024))
        .and_then(|permissions| permissions.with_pids_max(32))
    else {
        return invalid_recipe("permissions");
    };

    Recipe::mesboot(APPLICATION_NAME, "0.1")
        .inputs(&["td-compositor"])
        .payload_inputs(&["empty-runtime"])
        .steps(vec![
            Step::MkDir {
                path: "{out}/files/bin".into(),
            },
            Step::CopyFiles {
                files: vec!["{in:td-compositor}/bin/td-compositor".into()],
                dest: "{out}/files/bin".into(),
            },
            Step::validate_static_application(&declaration),
        ])
        .application(declaration)
        .application_launcher(launcher)
        .application_permissions(permissions)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-jail-fixture: package the td-built static Wayland client without executing it"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-jail-fixture 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

fn invalid_recipe(field: &str) -> Recipe {
    Recipe::mesboot(APPLICATION_NAME, "0.1").steps(vec![Step::Require {
        paths: vec![format!("{{out}}/invalid-application-{field}")],
        exec: false,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_is_one_static_wayland_application() {
        let recipe = recipe();
        let declaration = recipe.application.as_ref().expect("declaration");
        assert_eq!(declaration.runtime(), "empty-runtime");
        assert_eq!(declaration.entry(), APPLICATION_ENTRY);
        assert_eq!(declaration.alias(), Some(APPLICATION_ALIAS));
        assert_eq!(recipe.inputs, Some(vec!["td-compositor".into()]));
        let launcher = recipe.application_launcher.as_ref().expect("launcher");
        assert_eq!(launcher.display_name(), APPLICATION_DISPLAY_NAME);
        assert_eq!(
            launcher.search_terms().collect::<Vec<_>>(),
            APPLICATION_SEARCH_TERMS
        );
        assert_eq!(
            recipe.payload_inputs,
            Some(vec!["empty-runtime".into()])
        );
        assert_eq!(
            recipe
                .application_permissions
                .as_ref()
                .expect("permissions")
                .to_keyfile(),
            format!(
                "format=1\n\n[Context]\nsockets=wayland\n\n[Filesystem]\n{}=ro\n{}=ro\nxdg-download=rw:create\nxdg-pictures=ro:create\n\n[Resources]\nmemory-high=50331648\nmemory-max=67108864\npids-max=32\n",
                crate::ladder::TD_JAIL_FIXTURE_GRANT_ROOT,
                crate::ladder::TD_JAIL_FIXTURE_GRANT_FILE,
            )
        );
    }
}
