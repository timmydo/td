use crate::ladder::{
    build_root_glibc_cc, unpack_into, BUILD_ROOT_PATH as PATH, BUILD_ROOT_SH as SH,
};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// json-c's static library for cryptsetup's LUKS2 metadata (td-install/
// ENCRYPTION.md increment 3). Only libjson-c.a and its public headers leave the
// derivation; no json-c program is built.
//
// Built in the Debian build root with its cmake and gcc, against a sysroot of
// td glibc, under the shipped target profile (td-profiler/DESIGN.md §2): its
// objects are linked into cryptsetup, which ships, and a frame-pointer-less
// caller truncates every stack above it. CMake's configure probes link
// build-only programs through that sysroot (`build_root_glibc_cc`), so they
// see td glibc's symbols.

/// json-c 0.18's installed public headers, plus the two its configure step
/// generates into the build tree.
const HEADERS: &[&str] = &[
    "arraylist.h",
    "debug.h",
    "json_c_version.h",
    "json_inttypes.h",
    "json_object.h",
    "json_object_iterator.h",
    "json_patch.h",
    "json_pointer.h",
    "json_tokener.h",
    "json_types.h",
    "json_util.h",
    "json_visit.h",
    "linkhash.h",
    "printbuf.h",
];

pub fn recipe() -> Recipe {
    let mut steps = unpack_into("json-c-x86-64-source", "{src}");
    steps.extend(build_root_glibc_cc());
    steps.push(
        Step::run(
            "{root}",
            &[
                "/usr/bin/cmake",
                "-S",
                "{src}",
                "-B",
                "{root}/build",
                "-DCMAKE_BUILD_TYPE=Release",
                "-DCMAKE_INSTALL_PREFIX=/json-c",
                "-DCMAKE_C_COMPILER={root}/wb/cc",
                "-DCMAKE_MAKE_PROGRAM=/usr/bin/make",
                "-DBUILD_SHARED_LIBS=OFF",
                "-DBUILD_STATIC_LIBS=ON",
                "-DBUILD_APPS=OFF",
                "-DBUILD_TESTING=OFF",
                "-DDISABLE_EXTRA_LIBS=ON",
                "-DDISABLE_WERROR=ON",
                "-DENABLE_RDRAND=OFF",
                "-DENABLE_THREADING=OFF",
            ],
        )
        .env("PATH", PATH)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    // The libc probes that select json-c's code paths found td glibc.
    steps.push(
        Step::run(
            "{root}/build",
            &[
                SH,
                "-c",
                "set -e; for d in HAVE_STRDUP HAVE_SNPRINTF HAVE_VASPRINTF HAVE_STRNCASECMP \
                 HAVE_USELOCALE HAVE_DECL_NAN HAVE_DECL_INFINITY; do \
                 grep -Eq \"^#define $d( |$)\" config.h || { echo \"json-c configure lost $d\" >&2; exit 1; }; done",
            ],
        )
        .env("PATH", PATH),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                "/usr/bin/cmake",
                "--build",
                "{root}/build",
                "--target",
                "json-c",
                "--parallel",
                "{jobs}",
            ],
        )
        .env("PATH", PATH)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::MkDir {
        path: "{out}/lib".into(),
    });
    steps.push(Step::MkDir {
        path: "{out}/include/json-c".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{root}/build/libjson-c.a".into()],
        dest: "{out}/lib".into(),
    });
    steps.push(Step::CopyFiles {
        files: HEADERS
            .iter()
            .map(|header| format!("{{src}}/{header}"))
            .chain([
                "{root}/build/json.h".into(),
                "{root}/build/json_config.h".into(),
            ])
            .collect(),
        dest: "{out}/include/json-c".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/lib/libjson-c.a".into(),
            "{out}/include/json-c/json.h".into(),
            "{out}/include/json-c/json_config.h".into(),
        ],
        exec: false,
    });

    Recipe::mesboot("json-c-x86-64", "0.18")
        .source_input("json-c-x86-64-source")
        .build_root()
        .native_inputs(&["glibc-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check json-c-x86-64: build json-c 0.18's static library in the Debian build root against td glibc"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run json-c-x86-64 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    #[test]
    fn archives_are_compiled_under_the_shipped_target_profile() {
        let recipe = recipe();
        assert!(recipe.build_root);
        let wrapper = recipe
            .steps
            .unwrap_or_default()
            .into_iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{root}/wb/cc" => Some(content),
                _ => None,
            })
            .expect("compiler wrapper");
        let package = wrapper.find("\"$@\"").expect("package flags");
        let policy = wrapper
            .find("-fno-omit-frame-pointer")
            .expect("target profile");
        assert!(package < policy, "package flags could override the profile");
        for required in [
            "--sysroot={root}/sysroot",
            "-g1",
            "-ffile-prefix-map={root}=/td-build-root",
            "-ffile-prefix-map={src}=/td-build",
        ] {
            assert!(wrapper.contains(required), "missing {required}");
        }
    }
}
