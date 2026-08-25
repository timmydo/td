use crate::ladder::{post_bootstrap_path, split_target_debug, unpack_into, POST_BOOTSTRAP_SH};
use crate::types::{Recipe, Step, TextEdit};

// Source-built Git with local repository support and smart HTTP(S) transport.
// SSH is intentionally deferred until td has a reviewed client implementation;
// language runtimes, legacy WebDAV, localization, and direct OpenSSL use are
// excluded from this first daily-driver closure.
pub fn recipe() -> Recipe {
    let sgcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let curl = "{in:curl-x86-64}";
    let tls = "{in:libressl-x86-64}";
    let zlib = "{in:zlib-x86-64-self}";
    let path = format!("{{root}}/wb:{{tools}}:{sbin}:{}", post_bootstrap_path());

    let mut steps = unpack_into("git-x86-64-source", "{src}");
    steps.push(Step::ToolFarm {
        links: [
            "awk", "basename", "cat", "chmod", "cmp", "cp", "cut", "date", "diff", "dirname",
            "echo", "env", "expr", "false", "find", "grep", "head", "install", "ln", "ls", "mkdir",
            "mktemp", "mv", "printf", "pwd", "rm", "rmdir", "sed", "sort", "tail", "tar", "tee",
            "test", "touch", "tr", "true", "uname", "wc", "which", "xargs",
        ]
        .iter()
        .map(|name| ((*name).into(), "{in:busybox-x86-64}/bin/busybox".into()))
        .collect(),
    });
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: POST_BOOTSTRAP_SH.into(),
    });

    // SHELL_PATH is compiled into shipped scripts as /bin/sh. Generated build
    // inputs must nevertheless run with the declared BusyBox shell while the
    // recipe sandbox has no ambient /bin. Keep those two roles separate.
    steps.push(Step::substitute_text(
        "{src}/shared.mak",
        vec![TextEdit::new(
            "$(SHELL_PATH) \"$(1)/GIT-VERSION-GEN\"",
            "$(SHELL) \"$(1)/GIT-VERSION-GEN\"",
            1,
        )],
    ));
    steps.push(Step::substitute_text(
        "{src}/Makefile",
        vec![
            TextEdit::new(
                "$(QUIET_GEN)$(SHELL_PATH) ./tools/generate-configlist.sh",
                "$(QUIET_GEN)$(SHELL) ./tools/generate-configlist.sh",
                1,
            ),
            TextEdit::new(
                "$(QUIET_GEN)$(SHELL_PATH) ./tools/generate-cmdlist.sh",
                "$(QUIET_GEN)$(SHELL) ./tools/generate-cmdlist.sh",
                1,
            ),
            TextEdit::new(
                "$(QUIET_GEN)$(SHELL_PATH) ./tools/generate-hooklist.sh",
                "$(QUIET_GEN)$(SHELL) ./tools/generate-hooklist.sh",
                1,
            ),
            TextEdit::new(
                "REMOTE_CURL_ALIASES = git-remote-https$X git-remote-ftp$X git-remote-ftps$X",
                "REMOTE_CURL_ALIASES = git-remote-https$X",
                1,
            ),
        ],
    ));
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\n\
             exec \"{sgcc}\" -isystem \"{xglibc}/include\" \
             -B\"{sbin}/\" -B\"{xglibc}/lib\" \
             -L\"{xglibc}/lib\" -static-libgcc \"$@\" \
             -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map={{root}}=/td-build-root \
             -ffile-prefix-map={{src}}=/td-build \
             -ffile-prefix-map={curl}=/td-build/input/curl \
             -ffile-prefix-map={tls}=/td-build/input/libressl \
             -ffile-prefix-map={zlib}=/td-build/input/zlib \
             -Wl,--dynamic-linker -Wl,{xglibc}/lib/ld-linux-x86-64.so.2 \
             -Wl,--enable-new-dtags -Wl,-rpath -Wl,{xglibc}/lib \
             -Wl,--build-id=sha1\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/curl-config".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\n\
             case \"$1\" in\n\
             --vernum) printf '%s\\n' '081500';;\n\
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
             AR = {sbin}/ar\n\
             CURL_CONFIG = {{root}}/wb/curl-config\n\
             CURL_LDFLAGS = {curl}/lib/libcurl.a {tls}/lib/libssl.a {tls}/lib/libcrypto.a {zlib}/lib/libz.a -pthread\n\
             # Git's later EXTLIBS additions are exactly zlib and pthread for\n\
             # this configuration. Name pthread in CURL_LDFLAGS and zlib here\n\
             # so no implicit -lz or -lpthread search can escape the closure.\n\
             override EXTLIBS = {zlib}/lib/libz.a\n\
             CFLAGS = -O2 -I{curl}/include -I{zlib}/include\n\
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

    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64-self}/bin/make",
                "-j{jobs}",
                &format!("SHELL={POST_BOOTSTRAP_SH}"),
                "git",
                "git-remote-http",
                "git-remote-https",
                "git-sh-i18n--envsubst",
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
            ],
        )
        .env("PATH", &path)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64-self}/bin/make",
                "-C",
                "templates",
                &format!("SHELL={POST_BOOTSTRAP_SH}"),
                "SHELL_PATH=/bin/sh",
            ],
        )
        .env("PATH", &path)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("SOURCE_DATE_EPOCH", "1"),
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
        files: [
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
        ]
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
    steps.push(split_target_debug("{out}"));

    Recipe::mesboot("git-x86-64", "2.55.0")
        .source_input("git-x86-64-source")
        .native_inputs(&[
            "curl-x86-64",
            "libressl-x86-64",
            "zlib-x86-64-self",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "make-x86-64-self",
            "busybox-x86-64",
        ])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::recipe;
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
                    "zlib-x86-64-self",
                    "gcc-x86-64-self",
                    "binutils-x86-64-self",
                    "glibc-x86-64",
                    "make-x86-64-self",
                    "busybox-x86-64",
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
        assert!(config.contains(
            "override EXTLIBS = {in:zlib-x86-64-self}/lib/libz.a"
        ));
        assert!(!config.contains("CURLDIR ="));
        assert!(!config.contains("CURL_CFLAGS ="));
        assert!(!config.contains("SANE_TOOL_PATH ="));
        assert!(!config.contains("ZLIB_PATH ="));

        let build = steps.iter().find_map(|step| match step {
            Step::Run { argv, .. }
                if argv.first() == Some(&"{in:make-x86-64-self}/bin/make".to_string())
                    && argv.iter().any(|arg| arg == "git-remote-http") =>
            {
                Some(argv)
            }
            _ => None,
        });
        let build = build.expect("bounded Git build");
        assert!(build.iter().any(|arg| arg == "git"));
        assert!(build.iter().any(|arg| arg == "git-remote-https"));
        assert!(build.iter().any(|arg| arg == "git-sh-i18n--envsubst"));
        assert!(!build.iter().any(|arg| arg == "all"));
    }

    #[test]
    fn compiler_wrapper_keeps_the_target_profile_policy_last() {
        let steps = recipe().steps.expect("git steps");
        let wrapper = steps
            .iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{root}/wb/cc" => Some(content),
                _ => None,
            })
            .expect("compiler wrapper");
        let package_args = wrapper.find("\"$@\"").expect("package arguments");
        let profile_policy = wrapper
            .rfind("-fno-omit-frame-pointer")
            .expect("frame-pointer policy");
        assert!(package_args < profile_policy);
        for required in [
            "-g1",
            "-ffile-prefix-map={root}=/td-build-root",
            "-ffile-prefix-map={src}=/td-build",
            "-ffile-prefix-map={in:curl-x86-64}=/td-build/input/curl",
            "-ffile-prefix-map={in:libressl-x86-64}=/td-build/input/libressl",
            "-ffile-prefix-map={in:zlib-x86-64-self}=/td-build/input/zlib",
            "-Wl,--build-id=sha1",
        ] {
            assert!(
                wrapper.contains(required),
                "missing target policy {required}"
            );
        }
    }
}
