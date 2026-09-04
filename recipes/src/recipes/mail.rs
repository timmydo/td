//! The `mail` application: tmc, td's JMAP mail client, as a static package on
//! the empty runtime. It runs in a td-term window at boot (`system-x86-64`'s
//! `[mail]` unit) and reads `$XDG_CONFIG_HOME/tmc/config.toml`, which
//! td-firstboot provisions once under the login user's jail state. Saved
//! attachments land in the `xdg-download` grant, the directory Firefox shares.
use crate::application::ApplicationDeclaration;
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};
use td_engine::launcher::LauncherDeclaration;
use td_engine::permissions::{FilesystemAccess, PermissionPolicy, PermissionSocket};

const APPLICATION_NAME: &str = crate::ladder::TD_MAIL_NAME;
const APPLICATION_ENTRY: &str = crate::ladder::TD_MAIL_ENTRY;
const APPLICATION_DISPLAY_NAME: &str = crate::ladder::TD_MAIL_DISPLAY_NAME;
const APPLICATION_SEARCH_TERMS: &[&str] = crate::ladder::TD_MAIL_SEARCH_TERMS;
/// The source-built static binary this package wraps, and its recipe.
const PROGRAM: &str = "tmc";
const PROGRAM_RECIPE: &str = "tmc";

pub fn recipe() -> Recipe {
    let Ok(declaration) = ApplicationDeclaration::new("empty-runtime", APPLICATION_ENTRY) else {
        return invalid_recipe("declaration");
    };
    let Ok(launcher) =
        LauncherDeclaration::new(APPLICATION_DISPLAY_NAME, APPLICATION_SEARCH_TERMS)
    else {
        return invalid_recipe("launcher");
    };
    // Network for the protocol the program speaks, the Wayland socket the jail
    // requires of every application, and the terminal grant: td-term hands the
    // launcher a fresh pty and td-jail makes it the program's controlling
    // terminal. No bus name: the program has no D-Bus client, and the image's
    // one bus-holding application stays Firefox (system-x86-64's tripwire).
    let Ok(permissions) = PermissionPolicy::new()
        .with_network()
        .and_then(|permissions| permissions.with_socket(PermissionSocket::Wayland))
        .and_then(|permissions| permissions.with_terminal())
        .and_then(|permissions| {
            permissions.with_filesystem("xdg-download", FilesystemAccess::ReadWrite, true)
        })
        .and_then(|permissions| permissions.with_memory_high(192 * 1024 * 1024))
        .and_then(|permissions| permissions.with_memory_max(256 * 1024 * 1024))
        // Tasks, threads included: an async client's runtime keeps a worker
        // per vCPU and a blocking pool, and a cap hit aborts a program that
        // td-svc will not restart, so the fixture's 32 rather than a tighter
        // number.
        .and_then(|permissions| permissions.with_pids_max(32))
        .and_then(|permissions| permissions.with_cpu_max(50_000, 100_000))
    else {
        return invalid_recipe("permissions");
    };

    Recipe::mesboot(APPLICATION_NAME, "0.1")
        .inputs(&[PROGRAM_RECIPE])
        .payload_inputs(&["empty-runtime"])
        .steps(vec![
            Step::MkDir {
                path: "{out}/files/bin".into(),
            },
            Step::CopyFiles {
                files: vec![format!("{{in:{PROGRAM_RECIPE}}}/bin/{PROGRAM}")],
                dest: "{out}/files/bin".into(),
            },
            // The profiler's companion contract follows the copied runtime: the
            // object index expects its build-ID-matched debug companion at the
            // runtime's own path below this package's `lib/debug`, and reads
            // the assembly-exception marker at that tree's root. Both sit outside
            // `files/`, where the jail never looks, and the producing output
            // is not in the image's closure. MkDir and CopyFiles are the
            // materialization steps the planner lets precede the validator.
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
echo ">> recipe-check mail: package the source-built static tmc as the mail application without executing it"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run mail 1
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
    fn mail_is_one_static_terminal_application_without_a_bus_name() {
        let recipe = recipe();
        let declaration = recipe.application.as_ref().expect("declaration");
        assert_eq!(declaration.runtime(), "empty-runtime");
        assert_eq!(declaration.entry(), APPLICATION_ENTRY);
        assert_eq!(declaration.alias(), None);
        assert_eq!(recipe.inputs, Some(vec![PROGRAM_RECIPE.into()]));
        assert_eq!(recipe.payload_inputs, Some(vec!["empty-runtime".into()]));
        let launcher = recipe.application_launcher.as_ref().expect("launcher");
        assert_eq!(launcher.display_name(), APPLICATION_DISPLAY_NAME);
        assert_eq!(
            launcher.search_terms().collect::<Vec<_>>(),
            APPLICATION_SEARCH_TERMS
        );
        let permissions = recipe.application_permissions.as_ref().expect("permissions");
        assert!(permissions.network());
        assert!(permissions.terminal());
        assert_eq!(permissions.session_bus().count(), 0);
        assert_eq!(
            permissions.to_keyfile(),
            "format=2\n\n[Context]\nshared=network\nsockets=wayland\ndevices=tty\n\n[Filesystem]\nxdg-download=rw:create\n\n[Resources]\nmemory-high=201326592\nmemory-max=268435456\npids-max=32\ncpu-max=50000 100000\n"
        );
        let steps = recipe.steps.as_ref().expect("steps");
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::CopyFiles { files, dest }
                if files == &[format!("{{in:{PROGRAM_RECIPE}}}/bin/{PROGRAM}")]
                    && dest == "{out}/files/bin"
        )));
        // The companion lands where the object index derives it from the
        // copied runtime, `lib/debug/files/bin/<program>.debug`, and the marker
        // at the root of that tree.
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::CopyFiles { files, dest }
                if files == &[format!("{{in:{PROGRAM_RECIPE}}}/lib/debug/bin/{PROGRAM}.debug")]
                    && dest == "{out}/lib/debug/files/bin"
        )));
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::CopyFiles { files, dest }
                if files == &[format!("{{in:{PROGRAM_RECIPE}}}/lib/debug/.td-assembly-exception")]
                    && dest == "{out}/lib/debug"
        )));
        assert!(
            !steps.iter().any(|step| matches!(step, Step::CopyTree { .. })),
            "a tree copy may not precede the static validator"
        );
        assert!(matches!(
            steps.last(),
            Some(Step::ValidateStaticApplication { entry, runtime })
                if entry == APPLICATION_ENTRY && runtime == "empty-runtime"
        ));
    }
}
