use crate::ladder::{post_bootstrap_path, split_target_debug, unpack_into, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// Codex's default Linux sandbox is Bubblewrap, not its deprecated Landlock
// compatibility path. Build the exact vendored Bubblewrap sources from the
// reviewed Codex release archive and link libcap statically. The standalone
// helper is intended for `/bin/bwrap` in the system-image increment; Codex
// deliberately prefers that system path.
pub fn recipe() -> Recipe {
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let binutils = "{in:binutils-x86-64-self}/bin";
    let glibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let source = "{src}/codex-rs/vendor/bubblewrap";
    // The planner gives a shared pin the consuming recipe's local source-input
    // name. The pin key remains `codex-source`; this build edge is named after
    // the `codex-bwrap` recipe so the later `codex` recipe can share the bytes.
    let mut steps = unpack_into("codex-bwrap-source", "{src}");

    steps.push(Step::WriteFile {
        path: format!("{source}/config.h"),
        content: "#pragma once\n#define PACKAGE_STRING \"bubblewrap 0.11.2\"\n".into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\n\
             unset C_INCLUDE_PATH CPLUS_INCLUDE_PATH LIBRARY_PATH\n\
             gcc_include=$(\"{gcc}\" -print-file-name=include) || exit 1\n\
             test -n \"$gcc_include\" || exit 1\n\
             exec \"{gcc}\" -static -nostdinc -isystem \"$gcc_include\" \
             -isystem \"{glibc}/include\" \
             -B\"{binutils}/\" -B\"{glibc}/lib\" -L\"{glibc}/lib\" \
             -static-libgcc \"$@\" -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map=\"{{root}}\"=/td-build-root \
             -ffile-prefix-map=\"{{src}}\"=/td-build \
             -ffile-prefix-map=\"{{in:libcap-x86-64}}\"=/td-build/input/libcap \
             -ffile-prefix-map=\"{{in:gcc-x86-64-self}}\"=/td-build/input/gcc \
             -ffile-prefix-map=\"{{in:gcc-x86-64-self}}/stage/td/store/gcc-14.3.0-x86_64-self\"=/td-build/input/gcc-runtime \
             -ffile-prefix-map=\"$gcc_include\"=/td-build/input/gcc-includes \
             -ffile-prefix-map=\"{{in:binutils-x86-64-self}}\"=/td-build/input/binutils \
             -ffile-prefix-map=\"{{in:glibc-x86-64}}\"=/td-build/input/glibc \
             -ffile-prefix-map=\"{glibc}\"=/td-build/input/glibc-runtime \
             -Wl,--build-id=sha1\n"
        ),
        exec: true,
    });
    // Bubblewrap uses CAP_LAST_CAP to empty the bounding set. Compile this
    // beside the vendored sources so an include-order regression cannot fall
    // back silently to td's older bootstrap UAPI header.
    steps.push(Step::WriteFile {
        path: format!("{source}/td-capability-contract.c"),
        content: "#include <sys/capability.h>\n\
                  #include <linux/capability.h>\n\
                  _Static_assert(CAP_LAST_CAP == CAP_CHECKPOINT_RESTORE,\n\
                    \"CAP_LAST_CAP omits modern capabilities\");\n\
                  _Static_assert(CAP_LAST_CAP == 40,\n\
                    \"unexpected capability ABI\");\n"
            .into(),
        exec: false,
    });
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::run(
        source,
        &[
            "{root}/wb/cc",
            "-O2",
            "-D_GNU_SOURCE",
            "-Wall",
            "-Wextra",
            "-Werror=shadow",
            "-Werror=empty-body",
            "-Werror=strict-prototypes",
            "-Werror=missing-prototypes",
            "-Werror=implicit-function-declaration",
            "-Werror=pointer-arith",
            "-Werror=init-self",
            "-Werror=missing-declarations",
            "-Werror=return-type",
            "-Werror=overflow",
            "-Werror=int-conversion",
            "-Werror=parentheses",
            "-Werror=incompatible-pointer-types",
            "-Werror=misleading-indentation",
            "-Werror=missing-include-dirs",
            "-Werror=aggregate-return",
            "-Werror=switch-default",
            "-Wswitch-enum",
            "-Wno-missing-field-initializers",
            "-Wno-error=missing-field-initializers",
            "-Werror=format=2",
            "-Werror=format-security",
            "-Werror=format-nonliteral",
            "-I.",
            "-I{in:libcap-x86-64}/include",
            "bubblewrap.c",
            "bind-mount.c",
            "network.c",
            "utils.c",
            "td-capability-contract.c",
            "{in:libcap-x86-64}/lib/libcap.a",
            "-o",
            "{out}/bin/bwrap",
        ],
    ));
    steps.push(Step::Require {
        paths: vec!["{out}/bin/bwrap".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/bwrap"]));
    // Canonical /td/store and /td-build-root paths may occur in debug inherited
    // from the already-built static libgcc input. Reject noncanonical host and
    // scratch roots while retaining that deterministic provenance.
    steps.push(
        Step::run(
            "{out}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                "version=$('{out}/bin/bwrap' --version 2>&1) || exit 1; \
                 test \"$version\" = 'bubblewrap 0.11.2' || { echo \"unexpected bwrap version: $version\" >&2; exit 1; }; \
                 help=$('{out}/bin/bwrap' --help 2>&1) || exit 1; \
                 for token in '--as-pid-1' '--argv0' '--perms'; do \
                     printf '%s\\n' \"$help\" | grep -Fq -- \"$token\" || { echo \"bwrap help omits Codex-required $token\" >&2; exit 1; }; \
                 done; \
                 runtime='{out}/bin/bwrap'; debug='{out}/lib/debug/bin/bwrap.debug'; \
                 test -f \"$debug\" || { echo 'bwrap debug companion is absent' >&2; exit 1; }; \
                 runtime_id=$('{in:binutils-x86-64-self}/bin/readelf' -n \"$runtime\" | grep -F 'Build ID:') || exit 1; \
                 debug_id=$('{in:binutils-x86-64-self}/bin/readelf' -n \"$debug\" | grep -F 'Build ID:') || exit 1; \
                 test \"$runtime_id\" = \"$debug_id\" || { echo 'bwrap runtime/debug build IDs differ' >&2; exit 1; }; \
                 if '{in:binutils-x86-64-self}/bin/readelf' -S \"$runtime\" | grep -Fq '.symtab'; then echo 'bwrap runtime was not stripped' >&2; exit 1; fi; \
                 '{in:binutils-x86-64-self}/bin/readelf' -S \"$debug\" | grep -Fq '.symtab' || { echo 'bwrap debug companion lacks symbols' >&2; exit 1; }; \
                 '{in:binutils-x86-64-self}/bin/readelf' -S \"$debug\" | grep -Fq '.debug_line' || { echo 'bwrap debug companion lacks line tables' >&2; exit 1; }; \
                 info=$('{in:binutils-x86-64-self}/bin/readelf' --debug-dump=info,rawline \"$debug\") || exit 1; \
                 printf '%s\\n' \"$info\" | grep -Eq 'DW_AT_comp_dir.*: /td-build/codex-rs/vendor/bubblewrap$' || { echo 'bwrap debug companion lacks its canonical source root' >&2; exit 1; }; \
                 for forbidden in '/gnu/store' '/td-input' '/home/' '/tmp/' '/.td/' 'guix-build'; do \
                     match=$(printf '%s\\n' \"$info\" | grep -F -m 1 \"$forbidden\") || true; \
                     if test -n \"$match\"; then echo \"bwrap debug companion retains forbidden path $forbidden: $match\" >&2; exit 1; fi; \
                 done",
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    Recipe::mesboot("codex-bwrap", "0.11.2-codex-0.148.0")
        .source_input("codex-source")
        .native_inputs(&[
            "libcap-x86-64",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check codex-bwrap: build the static helper and validate Codex's required capability and debug surfaces"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run codex-bwrap 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    #[test]
    fn helper_build_has_the_exact_source_built_closure() {
        let recipe = recipe();
        assert_eq!(recipe.source_input.as_deref(), Some("codex-source"));
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                [
                    "libcap-x86-64",
                    "gcc-x86-64-self",
                    "binutils-x86-64-self",
                    "glibc-x86-64",
                    "busybox-x86-64",
                ]
                .map(str::to_string)
                .as_slice()
            )
        );
        assert!(recipe.inputs.is_none());
        assert!(recipe
            .steps
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|step| matches!(
                step,
                Step::Unpack { input, .. } if input == "{in:codex-bwrap-source}"
            )));
    }

    #[test]
    fn helper_is_profiled_split_static_and_exercised() {
        let steps = recipe().steps.expect("codex-bwrap steps");
        let wrapper = steps
            .iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{root}/wb/cc" => Some(content),
                _ => None,
            })
            .expect("compiler wrapper");
        assert!(
            wrapper.find("\"$@\"").expect("package arguments")
                < wrapper
                    .rfind("-fno-omit-frame-pointer")
                    .expect("target policy")
        );
        for required in [
            "unset C_INCLUDE_PATH CPLUS_INCLUDE_PATH LIBRARY_PATH",
            "-print-file-name=include",
            "-static",
            "-nostdinc",
            "-g1",
            "-ffile-prefix-map=\"{root}\"=/td-build-root",
            "-ffile-prefix-map=\"{src}\"=/td-build",
            "-ffile-prefix-map=\"{in:libcap-x86-64}\"=/td-build/input/libcap",
            "-ffile-prefix-map=\"{in:gcc-x86-64-self}\"=/td-build/input/gcc",
            "-ffile-prefix-map=\"{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self\"=/td-build/input/gcc-runtime",
            "-ffile-prefix-map=\"$gcc_include\"=/td-build/input/gcc-includes",
            "-ffile-prefix-map=\"{in:binutils-x86-64-self}\"=/td-build/input/binutils",
            "-ffile-prefix-map=\"{in:glibc-x86-64}\"=/td-build/input/glibc",
            "-ffile-prefix-map=\"{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64\"=/td-build/input/glibc-runtime",
            "-Wl,--build-id=sha1",
        ] {
            assert!(
                wrapper.contains(required),
                "missing target policy {required}"
            );
        }
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::SplitDebugTree { root, objcopy }
                if root == "{out}"
                    && objcopy == "{in:binutils-x86-64-self}/bin/objcopy"
        )));
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::AssertStatic { paths } if paths == &["{out}/bin/bwrap".to_string()]
        )));
        let contract = steps
            .iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. }
                    if path.ends_with("td-capability-contract.c") =>
                {
                    Some(content)
                }
                _ => None,
            })
            .expect("capability header contract");
        assert!(contract.contains("CAP_LAST_CAP == CAP_CHECKPOINT_RESTORE"));
        let validation = steps
            .iter()
            .find_map(|step| match step {
                Step::Run { argv, .. } if argv.iter().any(|arg| arg.contains("runtime_id=")) => {
                    argv.get(2)
                }
                _ => None,
            })
            .expect("realized bwrap validation");
        for required in [
            "bubblewrap 0.11.2",
            "--as-pid-1",
            "--argv0",
            "--perms",
            ".debug_line",
            "/td-build/codex-rs/vendor/bubblewrap",
        ] {
            assert!(validation.contains(required), "validation omits {required}");
        }
        assert_eq!(recipe().checks.as_deref().unwrap_or_default().len(), 1);
    }
}
