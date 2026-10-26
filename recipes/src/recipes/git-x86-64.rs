use crate::shared::{
    build_root_dynamic_cc, build_root_link_map_check, unpack_into, LinkedLibc, BUILD_ROOT_PATH,
    BUILD_ROOT_SH as SH,
};
use crate::types::{Recipe, Step, TextEdit};

// Source-built Git with local repository support and smart HTTP(S) transport.
// Git delegates SSH transport to the separately source-built OpenSSH client;
// language runtimes, legacy WebDAV, localization, and direct OpenSSL use remain
// outside this daily-driver closure.
//
// It compiles in the Debian build root against td glibc
// (`build_root_dynamic_cc`): each program links td glibc's libc.so.6 with its
// loader and RUNPATH, gcc-runtime's start files and libgcc, and the curl,
// LibreSSL and zlib archives, which its link map proves.

/// The ELF programs, each linked by its own make.
const PROGRAMS: &[&str] = &["git", "git-remote-http", "git-sh-i18n--envsubst"];

/// Shell scripts, generated without a link. The https alias is not made here:
/// its rule would revisit git-remote-http after that link's map was checked.
const SCRIPTS: &[&str] = &[
    "git-difftool--helper",
    "git-filter-branch",
    "git-merge-octopus",
    "git-merge-one-file",
    "git-merge-resolve",
    "git-mergetool",
    "git-quiltimport",
    "git-request-pull",
    "git-submodule",
    "git-web--browse",
    "git-mergetool--lib",
    "git-sh-i18n",
    "git-sh-setup",
];

/// Written only if a make after the per-program links links again.
const NO_LINK_MAP: &str = "{root}/no-link.map";

pub fn recipe() -> Recipe {
    let curl = "{in:curl-x86-64}";
    let tls = "{in:libressl-x86-64}";
    let zlib = "{in:zlib-x86-64}";
    let path = format!("{{root}}/wb:{BUILD_ROOT_PATH}");

    let mut steps = unpack_into("git-x86-64-source", "{src}");

    // SHELL_PATH=/bin/sh is compiled into shipped scripts, and Git's
    // generators run under it too: the root's /bin/sh, as upstream intends.
    steps.push(Step::substitute_text(
        "{src}/Makefile",
        vec![TextEdit::new(
            "REMOTE_CURL_ALIASES = git-remote-https$X git-remote-ftp$X git-remote-ftps$X",
            "REMOTE_CURL_ALIASES = git-remote-https$X",
            1,
        )],
    ));
    steps.extend(build_root_dynamic_cc());
    steps.push(Step::WriteFile {
        path: "{root}/wb/curl-config".into(),
        content: format!(
            "#!{SH}\n\
             case \"$1\" in\n\
             --vernum) printf '%s\\n' '081700';;\n\
             *) exit 1;;\n\
             esac\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{src}/config.mak".into(),
        content: format!(
            "prefix = {{out}}\n\
             bindir = {{out}}/bin\n\
             gitexecdir = {{out}}/libexec/git-core\n\
             template_dir = {{out}}/share/git-core/templates\n\
             sysconfdir = /etc\n\
             SHELL_PATH = /bin/sh\n\
             CC = {{root}}/wb/cc\n\
             AR = /usr/bin/ar\n\
             CURL_CONFIG = {{root}}/wb/curl-config\n\
             CURL_LDFLAGS = {curl}/lib/libcurl.a {tls}/lib/libssl.a {tls}/lib/libcrypto.a {zlib}/lib/libz.a -pthread\n\
             # The override drops Git's EXTLIBS additions for this\n\
             # configuration, zlib, pthread and config.mak.uname's -ldl. Name\n\
             # pthread in CURL_LDFLAGS and zlib here so no implicit -l search\n\
             # can escape the closure; dlopen is in td glibc's libc.so.6.\n\
             override EXTLIBS = {zlib}/lib/libz.a\n\
             CFLAGS = -O2 -I{curl}/include -I{zlib}/include \
             -ffile-prefix-map={curl}=/td-build/input/curl \
             -ffile-prefix-map={tls}=/td-build/input/libressl \
             -ffile-prefix-map={zlib}=/td-build/input/zlib\n\
             LDFLAGS = -Wl,--build-id=sha1\n\
             NO_PERL = YesPlease\n\
             PERL_PATH =\n\
             NO_PYTHON = YesPlease\n\
             PYTHON_PATH =\n\
             NO_TCLTK = YesPlease\n\
             NO_GETTEXT = YesPlease\n\
             NO_EXPAT = YesPlease\n\
             NO_OPENSSL = YesPlease\n\
             NO_RUST = YesPlease\n\
             NO_BASH_COMPLETION = YesPlease\n\
             SKIP_DASHED_BUILT_INS = YesPlease\n\
             INSTALL_SYMLINKS = YesPlease\n\
             NO_INSTALL_HARDLINKS = YesPlease\n\
             NO_CROSS_DIRECTORY_HARDLINKS = YesPlease\n"
        ),
        exec: false,
    });

    // One make per program: each link writes, and is checked against, its own
    // map. The scripts follow without a link.
    for program in PROGRAMS {
        let map = format!("{{root}}/{program}.map");
        steps.push(
            Step::run(
                "{src}",
                &["/usr/bin/make", "-j{jobs}", &format!("SHELL={SH}"), program],
            )
            .env("PATH", &path)
            .env("SHELL", SH)
            .env("TD_LINK_MAP", &map)
            .env("SOURCE_DATE_EPOCH", "1"),
        );
        steps.push(build_root_link_map_check(
            "{src}",
            &map,
            program,
            &[
                "gcc-runtime",
                "curl-x86-64",
                "libressl-x86-64",
                "zlib-x86-64",
            ],
            LinkedLibc::Shared,
        ));
    }
    // The scripts and templates link nothing: any link would write the
    // sentinel map, which must stay absent.
    let mut scripts = vec![
        "/usr/bin/make".to_string(),
        "-j{jobs}".to_string(),
        format!("SHELL={SH}"),
    ];
    scripts.extend(SCRIPTS.iter().map(|name| name.to_string()));
    steps.push(Step::Run {
        dir: "{src}".into(),
        argv: scripts,
        env: vec![
            ("PATH".into(), path.clone()),
            ("SHELL".into(), SH.into()),
            ("TD_LINK_MAP".into(), NO_LINK_MAP.into()),
            ("SOURCE_DATE_EPOCH".into(), "1".into()),
        ],
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                "/usr/bin/make",
                "-C",
                "templates",
                &format!("SHELL={SH}"),
                "SHELL_PATH=/bin/sh",
            ],
        )
        .env("PATH", &path)
        .env("SHELL", SH)
        .env("TD_LINK_MAP", NO_LINK_MAP)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                &format!(
                    "if [ -e '{NO_LINK_MAP}' ]; then echo 'a script make linked' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &path),
    );

    for dir in [
        "{out}/bin",
        "{out}/libexec/git-core",
        "{out}/share/git-core/templates",
    ] {
        steps.push(Step::MkDir { path: dir.into() });
    }
    steps.push(Step::CopyFiles {
        files: vec!["{src}/git".into()],
        dest: "{out}/bin".into(),
    });
    for name in ["git-receive-pack", "git-upload-archive", "git-upload-pack"] {
        steps.push(Step::Symlink {
            target: "git".into(),
            link: format!("{{out}}/bin/{name}"),
        });
    }
    steps.push(Step::CopyFiles {
        files: vec![
            "{src}/git-remote-http".into(),
            "{src}/git-sh-i18n--envsubst".into(),
        ],
        dest: "{out}/libexec/git-core".into(),
    });
    steps.push(Step::Symlink {
        target: "git-remote-http".into(),
        link: "{out}/libexec/git-core/git-remote-https".into(),
    });
    steps.push(Step::CopyFiles {
        files: SCRIPTS
            .iter()
            .map(|name| format!("{{src}}/{name}"))
            .collect(),
        dest: "{out}/libexec/git-core".into(),
    });
    steps.push(Step::CopyTree {
        from: "{src}/templates/blt".into(),
        dest: "{out}/share/git-core/templates".into(),
    });
    steps.push(Step::CopyTree {
        from: "{src}/mergetools".into(),
        dest: "{out}/libexec/git-core/mergetools".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/bin/git".into(),
            "{out}/bin/git-receive-pack".into(),
            "{out}/bin/git-upload-archive".into(),
            "{out}/bin/git-upload-pack".into(),
            "{out}/libexec/git-core/git-remote-http".into(),
            "{out}/libexec/git-core/git-remote-https".into(),
            "{out}/libexec/git-core/git-sh-i18n--envsubst".into(),
            "{out}/libexec/git-core/git-submodule".into(),
        ],
        exec: true,
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/libexec/git-core/mergetools/vimdiff".into(),
            "{out}/share/git-core/templates/hooks/pre-commit.sample".into(),
            "{out}/share/git-core/templates/info/exclude".into(),
        ],
        exec: false,
    });
    steps.push(Step::split_debug_tree("{out}", "/usr/bin/objcopy"));

    Recipe::mesboot("git-x86-64", "2.55.0")
        .source_input("git-x86-64-source")
        .build_root()
        .native_inputs(&[
            "curl-x86-64",
            "libressl-x86-64",
            "zlib-x86-64",
            "glibc-x86-64",
            "gcc-runtime",
        ])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::{recipe, PROGRAMS, SCRIPTS};
    use crate::types::Step;

    #[test]
    fn build_uses_the_exact_final_https_toolchain_closure() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                [
                    "curl-x86-64",
                    "libressl-x86-64",
                    "zlib-x86-64",
                    "glibc-x86-64",
                    "gcc-runtime",
                ]
                .map(str::to_string)
                .as_slice()
            )
        );
        assert!(recipe.inputs.is_none());
        let steps = recipe.steps.expect("git steps");
        let config = steps.iter().find_map(|step| match step {
            Step::WriteFile { path, content, .. } if path == "{src}/config.mak" => Some(content),
            _ => None,
        });
        let config = config.expect("Git build configuration");
        for required in [
            "SHELL_PATH = /bin/sh",
            "sysconfdir = /etc",
            "NO_PERL = YesPlease",
            "NO_PYTHON = YesPlease",
            "NO_TCLTK = YesPlease",
            "NO_GETTEXT = YesPlease",
            "NO_EXPAT = YesPlease",
            "NO_OPENSSL = YesPlease",
            "NO_RUST = YesPlease",
            "SKIP_DASHED_BUILT_INS = YesPlease",
        ] {
            assert!(config.contains(required), "missing Git policy {required}");
        }
        assert!(config.contains("CURL_LDFLAGS = {in:curl-x86-64}/lib/libcurl.a"));
        assert!(config.contains("override EXTLIBS = {in:zlib-x86-64}/lib/libz.a"));
        assert!(!config.contains("CURLDIR ="));
        assert!(!config.contains("CURL_CFLAGS ="));
        assert!(!config.contains("SANE_TOOL_PATH ="));
        assert!(!config.contains("ZLIB_PATH ="));

        let makes: Vec<_> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. }
                    if argv.first().is_some_and(|arg| arg == "/usr/bin/make") =>
                {
                    Some(argv)
                }
                _ => None,
            })
            .collect();
        for program in PROGRAMS {
            let map = format!("{{root}}/{program}.map");
            let link = steps
                .iter()
                .position(|step| {
                    matches!(step, Step::Run { argv, env, .. }
                        if argv.last().is_some_and(|arg| arg == program)
                            && env.iter().any(|(key, value)| key == "TD_LINK_MAP" && *value == map))
                })
                .expect("the program's own make writes its map");
            let checked = steps
                .iter()
                .position(|step| {
                    matches!(step, Step::Run { argv, .. }
                        if argv.iter().any(|arg| arg.contains(&format!("m='{map}'"))
                            && arg.contains(&format!("= '{program}'"))
                            && arg.contains("{in:curl-x86-64}")
                            && arg.contains("$sys/libc.so.6")))
                })
                .expect("link-map check");
            assert!(link < checked, "{program}'s map is checked after its link");
        }
        let scripts = makes
            .iter()
            .find(|argv| argv.iter().any(|arg| arg == "git-submodule"))
            .expect("script make");
        assert!(SCRIPTS
            .iter()
            .all(|name| scripts.iter().any(|arg| arg == name)));
        assert!(!PROGRAMS
            .iter()
            .any(|name| scripts.iter().any(|arg| arg == name)));
        assert!(!makes.iter().any(|argv| argv.iter().any(|arg| arg == "all")));
        assert!(!scripts.iter().any(|arg| arg == "git-remote-https"));
        let sentinel = steps.iter().position(|step| {
            matches!(step, Step::Run { argv, .. }
                if argv.iter().any(|arg| arg.contains("[ -e '{root}/no-link.map' ]")))
        });
        let last_make = steps.iter().rposition(|step| {
            matches!(step, Step::Run { argv, .. }
                if argv.first().is_some_and(|arg| arg == "/usr/bin/make"))
        });
        assert!(
            sentinel > last_make,
            "the no-link sentinel follows every make"
        );
        for argv_env in steps.iter().filter_map(|step| match step {
            Step::Run { argv, env, .. }
                if argv.first().is_some_and(|arg| arg == "/usr/bin/make")
                    && !PROGRAMS
                        .iter()
                        .any(|name| argv.last().is_some_and(|arg| arg == name)) =>
            {
                Some(env)
            }
            _ => None,
        }) {
            assert!(argv_env
                .iter()
                .any(|(key, value)| key == "TD_LINK_MAP" && value == "{root}/no-link.map"));
        }
    }

    #[test]
    fn build_root_wrapper_and_config_keep_the_target_profile() {
        let recipe = recipe();
        crate::shared::assert_build_root_dynamic_c(&recipe);
        let config = recipe
            .steps
            .unwrap_or_default()
            .into_iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{src}/config.mak" => {
                    Some(content)
                }
                _ => None,
            })
            .expect("Git build configuration");
        for required in [
            "-ffile-prefix-map={in:curl-x86-64}=/td-build/input/curl",
            "-ffile-prefix-map={in:libressl-x86-64}=/td-build/input/libressl",
            "-ffile-prefix-map={in:zlib-x86-64}=/td-build/input/zlib",
            "LDFLAGS = -Wl,--build-id=sha1",
        ] {
            assert!(config.contains(required), "missing {required}");
        }
    }
}
