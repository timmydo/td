//! The `mail` application: td-mail, td's JMAP mail client, as a static
//! package on the data-only static runtime. It opens a td-ui window of its own at
//! boot (`system-x86-64`'s `[mail]` unit) and reads
//! `$XDG_CONFIG_HOME/td-mail/config.toml`, which
//! td-firstboot provisions once under the login user's jail state. Saved
//! attachments land in the `xdg-download` grant, the directory Firefox shares.
//! The package also ships td-editor beside it as `/app/bin/td-editor` and
//! names it in the manifest's `EDITOR` (APPLICATIONS.md §W.5): td-mail
//! composes in `$EDITOR`, the jail's `/app` sees no editor that is not
//! packaged with the application, and the editor is a Wayland client the
//! jail's existing `sockets=wayland` grant admits, so it opens its own
//! toplevel beside the mail window.
use crate::application::ApplicationDeclaration;
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};
use td_engine::launcher::LauncherDeclaration;
use td_engine::permissions::{FilesystemAccess, PermissionPolicy, PermissionSocket};

const APPLICATION_NAME: &str = crate::ladder::TD_MAIL_NAME;
const APPLICATION_ENTRY: &str = crate::ladder::TD_MAIL_ENTRY;
const APPLICATION_DISPLAY_NAME: &str = crate::ladder::TD_MAIL_DISPLAY_NAME;
const APPLICATION_SEARCH_TERMS: &[&str] = crate::ladder::TD_MAIL_SEARCH_TERMS;
/// The source-built static binary this package wraps, and its recipe.
const PROGRAM: &str = "td-mail";
const PROGRAM_RECIPE: &str = "td-mail";
/// The editor the package ships for composition, and where td-mail finds it.
/// The entry must stay a command of plain words: td-mail runs such an
/// `$EDITOR` directly (`plain_command` in td-mail/src/ui/mod.rs, which
/// pins this value), and anything else it hands to a shell the runtime
/// does not have.
const EDITOR: &str = "td-editor";
const EDITOR_RECIPE: &str = "td-editor";
const EDITOR_ENTRY: &str = "/app/bin/td-editor";

pub fn recipe() -> Recipe {
    // td-mail selects `[ui].editor`, then `$EDITOR`, then `vi`; td-firstboot's
    // provisioned configuration sets no editor, so the manifest's `EDITOR`
    // is the one td-mail runs, directly and without a shell, since the
    // static runtime has none (td-mail/README.md).
    let Ok(declaration) = ApplicationDeclaration::new("static-runtime", APPLICATION_ENTRY)
        .and_then(|declaration| declaration.with_environment("EDITOR", EDITOR_ENTRY))
    else {
        return invalid_recipe("declaration");
    };
    let Ok(launcher) =
        LauncherDeclaration::new(APPLICATION_DISPLAY_NAME, APPLICATION_SEARCH_TERMS)
    else {
        return invalid_recipe("launcher");
    };
    // The fetch socket for the protocol the program speaks (APPLICATIONS.md
    // §W.8; no network of its own) and the Wayland socket, which is the
    // window the program draws in as well as the one the jail requires of
    // every application. No terminal grant: the client draws through td-ui,
    // and a pty would be a descriptor nothing reads. The credential client
    // needs only the default portal grant; mail still owns no bus name.
    let Ok(permissions) = PermissionPolicy::new()
        .with_socket(PermissionSocket::Wayland)
        .and_then(|permissions| permissions.with_socket(PermissionSocket::Fetch))
        .and_then(|permissions| {
            permissions.with_filesystem("xdg-download", FilesystemAccess::ReadWrite, true)
        })
        .and_then(|permissions| permissions.with_memory_high(192 * 1024 * 1024))
        .and_then(|permissions| permissions.with_memory_max(256 * 1024 * 1024))
        // Tasks, threads included: the client keeps a backend thread beside
        // the screen's, and a cap hit aborts a program that td-svc will not
        // restart, so the fixture's 32 rather than a tighter number.
        .and_then(|permissions| permissions.with_pids_max(32))
        .and_then(|permissions| permissions.with_cpu_max(50_000, 100_000))
    else {
        return invalid_recipe("permissions");
    };

    Recipe::mesboot(APPLICATION_NAME, "0.1")
        .inputs(&[PROGRAM_RECIPE, "td-secret", EDITOR_RECIPE])
        .payload_inputs(&["static-runtime"])
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
            Step::CopyFiles {
                files: vec!["{in:td-secret}/bin/td-secret".into()],
                dest: "{out}/files/bin".into(),
            },
            Step::CopyFiles {
                files: vec!["{in:td-secret}/lib/debug/bin/td-secret.debug".into()],
                dest: "{out}/lib/debug/files/bin".into(),
            },
            // The editor, static and split by its own recipe (td-editor-test
            // asserts the shape), and its companion at the path the object
            // index derives from the copied runtime. The package's one
            // assembly-exception marker above, td-mail's bytes, covers it
            // too: the index reads one boundary set per store item, and a
            // std-only Rust output on the same toolchain has exactly
            // td-mail's three, which the test below pins
            // (td-profiler/DESIGN.md's union rule).
            Step::CopyFiles {
                files: vec![format!("{{in:{EDITOR_RECIPE}}}/bin/{EDITOR}")],
                dest: "{out}/files/bin".into(),
            },
            Step::CopyFiles {
                files: vec![format!(
                    "{{in:{EDITOR_RECIPE}}}/lib/debug/bin/{EDITOR}.debug"
                )],
                dest: "{out}/lib/debug/files/bin".into(),
            },
            Step::validate_static_application(&declaration),
        ])
        .application(declaration)
        .application_launcher(launcher)
        .application_permissions(permissions)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check mail: package the source-built static td-mail as the mail application without executing it"
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
    fn mail_is_one_static_window_application_without_a_terminal_or_a_bus_name() {
        let recipe = recipe();
        let declaration = recipe.application.as_ref().expect("declaration");
        assert_eq!(declaration.runtime(), "static-runtime");
        assert_eq!(declaration.entry(), APPLICATION_ENTRY);
        assert_eq!(declaration.alias(), None);
        // The editor is named by the path the package copies it to, and by
        // nothing else: the manifest carries no other environment.
        assert_eq!(
            declaration.environment().collect::<Vec<_>>(),
            [("EDITOR", EDITOR_ENTRY)]
        );
        assert_eq!(EDITOR_ENTRY, format!("/app/bin/{EDITOR}"));
        assert_eq!(
            recipe.inputs,
            Some(vec![PROGRAM_RECIPE.into(), "td-secret".into(), EDITOR_RECIPE.into()])
        );
        assert_eq!(recipe.payload_inputs, Some(vec!["static-runtime".into()]));
        let launcher = recipe.application_launcher.as_ref().expect("launcher");
        assert_eq!(launcher.display_name(), APPLICATION_DISPLAY_NAME);
        assert_eq!(
            launcher.search_terms().collect::<Vec<_>>(),
            APPLICATION_SEARCH_TERMS
        );
        let permissions = recipe.application_permissions.as_ref().expect("permissions");
        // The fetch grant, not the network: the keyfile below says which.
        assert!(!permissions.network());
        // No terminal: the client is a Wayland client of its own window.
        assert!(!permissions.terminal());
        assert_eq!(permissions.session_bus().count(), 0);
        assert_eq!(
            permissions.to_keyfile(),
            "format=2\n\n[Context]\nsockets=wayland;fetch\n\n[Filesystem]\nxdg-download=rw:create\n\n[Resources]\nmemory-high=201326592\nmemory-max=268435456\npids-max=32\ncpu-max=50000 100000\n"
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
        // The editor and its companion land beside td-mail's, before the
        // validator.
        let validator = steps
            .iter()
            .position(|step| matches!(step, Step::ValidateStaticApplication { .. }))
            .expect("validator");
        for (file, dest) in [
            (format!("{{in:{EDITOR_RECIPE}}}/bin/{EDITOR}"), "{out}/files/bin"),
            (
                format!("{{in:{EDITOR_RECIPE}}}/lib/debug/bin/{EDITOR}.debug"),
                "{out}/lib/debug/files/bin",
            ),
        ] {
            let copy = steps
                .iter()
                .position(|step| matches!(
                    step,
                    Step::CopyFiles { files, dest: got } if files == &[file.clone()] && got == dest
                ))
                .unwrap_or_else(|| panic!("{file} is not copied to {dest}"));
            assert!(copy < validator, "{file} is copied after the validator");
        }
        // One marker, td-mail's, stands for every runtime in the package:
        // the editor's boundary set must be td-mail's exactly.
        assert_eq!(
            td_engine::target_profile::output_assembly_exceptions(EDITOR_RECIPE),
            td_engine::target_profile::output_assembly_exceptions(PROGRAM_RECIPE)
        );
        assert_eq!(
            td_engine::target_profile::output_assembly_exceptions("td-secret"),
            td_engine::target_profile::output_assembly_exceptions(PROGRAM_RECIPE)
        );
        assert!(
            !steps.iter().any(|step| matches!(step, Step::CopyTree { .. })),
            "a tree copy may not precede the static validator"
        );
        assert!(matches!(
            steps.last(),
            Some(Step::ValidateStaticApplication { entry, runtime })
                if entry == APPLICATION_ENTRY && runtime == "static-runtime"
        ));
    }
}
