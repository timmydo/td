//! The `web` application: td-web, td's own document browser
//! (td-web/DESIGN.md), as a static package on the data-only static runtime.
//! Unlike mail and news it runs under its own service account, not as the
//! login user, so it has no `/bin` link: its launcher card starts
//! `system-x86-64`'s `[web]` unit, which runs it beside a fetch service of
//! its own (`[web-fetch]`). It keeps nothing between runs and needs no
//! configuration, so td-firstboot provisions none.
use crate::application::ApplicationDeclaration;
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};
use td_engine::launcher::LauncherDeclaration;
use td_engine::permissions::{PermissionPolicy, PermissionSocket};

const APPLICATION_NAME: &str = crate::shared::TD_WEB_NAME;
const APPLICATION_ENTRY: &str = crate::shared::TD_WEB_ENTRY;
const APPLICATION_DISPLAY_NAME: &str = crate::shared::TD_WEB_DISPLAY_NAME;
const APPLICATION_SEARCH_TERMS: &[&str] = crate::shared::TD_WEB_SEARCH_TERMS;
/// The source-built static binary this package wraps, and its recipe.
const PROGRAM: &str = "td-web";
const PROGRAM_RECIPE: &str = "td-web";

pub fn recipe() -> Recipe {
    // No environment: td-web is no other program's `BROWSER`, and opens
    // nothing through one of its own (td-web/DESIGN.md §7).
    let Ok(declaration) = ApplicationDeclaration::new("static-runtime", APPLICATION_ENTRY) else {
        return invalid_recipe("declaration");
    };
    let Ok(launcher) = LauncherDeclaration::new(APPLICATION_DISPLAY_NAME, APPLICATION_SEARCH_TERMS)
    else {
        return invalid_recipe("launcher");
    };
    // The fetch socket, its one way to the network (no `shared=network`),
    // and the Wayland socket it draws in. No filesystem grant: it keeps
    // nothing between runs. No terminal and no bus name. The memory bounds
    // hold a 16 MiB body, a picture of up to 16 Mi pixels decoded beside
    // it, and a rendering of markup at td-web's 2 MiB bound, which can
    // transiently cost some 300 MB (td-web/DESIGN.md §5); the tasks hold
    // the window's thread, td-ui's and up to four fetch workers with room,
    // since td-svc does not restart a program that exits on the cap.
    let Ok(permissions) = PermissionPolicy::new()
        .with_socket(PermissionSocket::Wayland)
        .and_then(|permissions| permissions.with_socket(PermissionSocket::Fetch))
        .and_then(|permissions| permissions.with_memory_high(512 * 1024 * 1024))
        .and_then(|permissions| permissions.with_memory_max(768 * 1024 * 1024))
        .and_then(|permissions| permissions.with_pids_max(32))
        .and_then(|permissions| permissions.with_cpu_max(100_000, 100_000))
    else {
        return invalid_recipe("permissions");
    };

    Recipe::mesboot(APPLICATION_NAME, "0.1")
        .inputs(&[PROGRAM_RECIPE])
        .payload_inputs(&["static-runtime"])
        .steps(vec![
            Step::MkDir {
                path: "{out}/files/bin".into(),
            },
            Step::CopyFiles {
                files: vec![format!("{{in:{PROGRAM_RECIPE}}}/bin/{PROGRAM}")],
                dest: "{out}/files/bin".into(),
            },
            // The profiler's companion contract follows the copied runtime,
            // as news's does: the build-ID-matched companion at the
            // runtime's own path below `lib/debug`, the assembly-exception
            // marker at that tree's root, both outside `files/`.
            Step::MkDir {
                path: "{out}/lib/debug/files/bin".into(),
            },
            Step::CopyFiles {
                files: vec![format!(
                    "{{in:{PROGRAM_RECIPE}}}/lib/debug/bin/{PROGRAM}.debug"
                )],
                dest: "{out}/lib/debug/files/bin".into(),
            },
            Step::CopyFiles {
                files: vec![format!(
                    "{{in:{PROGRAM_RECIPE}}}/lib/debug/.td-assembly-exception"
                )],
                dest: "{out}/lib/debug".into(),
            },
            Step::validate_static_application(&declaration),
        ])
        .application(declaration)
        .application_launcher(launcher)
        .application_permissions(permissions)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check web: package the source-built static td-web as the web application without executing it"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run web 1
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
    fn web_is_one_static_window_application_with_fetch_and_no_network() {
        let recipe = recipe();
        let declaration = recipe.application.as_ref().expect("declaration");
        assert_eq!(declaration.runtime(), "static-runtime");
        assert_eq!(declaration.entry(), APPLICATION_ENTRY);
        assert_eq!(declaration.alias(), None);
        // Not BROWSER, and no opener shipped beside it.
        assert_eq!(declaration.environment().count(), 0);
        assert_eq!(recipe.inputs, Some(vec![PROGRAM_RECIPE.into()]));
        assert_eq!(recipe.payload_inputs, Some(vec!["static-runtime".into()]));
        let launcher = recipe.application_launcher.as_ref().expect("launcher");
        assert_eq!(launcher.display_name(), APPLICATION_DISPLAY_NAME);
        assert_eq!(
            launcher.search_terms().collect::<Vec<_>>(),
            APPLICATION_SEARCH_TERMS
        );
        let permissions = recipe
            .application_permissions
            .as_ref()
            .expect("permissions");
        assert!(!permissions.network());
        assert!(!permissions.terminal());
        assert_eq!(permissions.session_bus().count(), 0);
        assert_eq!(
            permissions.to_keyfile(),
            "format=2\n\n[Context]\nsockets=wayland;fetch\n\n[Resources]\nmemory-high=536870912\nmemory-max=805306368\npids-max=32\ncpu-max=100000 100000\n"
        );
        let steps = recipe.steps.as_ref().expect("steps");
        for (file, dest) in [
            (
                format!("{{in:{PROGRAM_RECIPE}}}/bin/{PROGRAM}"),
                "{out}/files/bin",
            ),
            (
                format!("{{in:{PROGRAM_RECIPE}}}/lib/debug/bin/{PROGRAM}.debug"),
                "{out}/lib/debug/files/bin",
            ),
            (
                format!("{{in:{PROGRAM_RECIPE}}}/lib/debug/.td-assembly-exception"),
                "{out}/lib/debug",
            ),
        ] {
            assert!(steps.iter().any(|step| matches!(
                step,
                Step::CopyFiles { files, dest: to } if files == &[file.clone()] && to == dest
            )));
        }
        assert!(
            !steps
                .iter()
                .any(|step| matches!(step, Step::CopyTree { .. })),
            "a tree copy may not precede the static validator"
        );
        assert!(matches!(
            steps.last(),
            Some(Step::ValidateStaticApplication { entry, runtime })
                if entry == APPLICATION_ENTRY && runtime == "static-runtime"
        ));
    }
}
