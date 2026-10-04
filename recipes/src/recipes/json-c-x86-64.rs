use crate::ladder::{post_bootstrap_path, unpack_into, unpack_keep_top, POST_BOOTSTRAP_SH};
use crate::types::{Recipe, Step};

// json-c's static library for cryptsetup's LUKS2 metadata (td-install/
// ENCRYPTION.md increment 3). Only libjson-c.a and its public headers leave the
// derivation; no json-c program is built.
//
// Built by the self-hosted toolchain under the shipped target profile
// (td-profiler/DESIGN.md §2): its objects are linked into cryptsetup, which
// ships, and a frame-pointer-less caller truncates every stack above it.
pub fn recipe() -> Recipe {
    let sgcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let cmake = "{in:cmake-x86-64}/bin/cmake";
    let path = format!("{{root}}/wb:{{tools}}:{sbin}:{}", post_bootstrap_path());

    let mut steps = unpack_into("json-c-x86-64-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: [
            "awk", "cat", "cmp", "cp", "dirname", "echo", "env", "false", "grep", "ln", "ls",
            "mkdir", "mv", "printf", "rm", "sed", "sh", "test", "touch", "tr", "true", "uname",
        ]
        .iter()
        .map(|name| ((*name).into(), "{in:busybox-x86-64}/bin/busybox".into()))
        .collect(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\nexec \"{sgcc}\" -static -idirafter \"{xglibc}/include\" \
             -idirafter \"{{root}}/kh\" -B\"{sbin}/\" -B{xglibc}/lib -L{xglibc}/lib \"$@\" \
             -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map={{root}}=/td-build-root \
             -ffile-prefix-map={{src}}=/td-build\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/make".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\nexec \"{{in:make-x86-64-self}}/bin/make\" \
             SHELL=\"{POST_BOOTSTRAP_SH}\" \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                cmake,
                "-S",
                "{src}",
                "-B",
                "{root}/build",
                "-DCMAKE_BUILD_TYPE=Release",
                "-DCMAKE_INSTALL_PREFIX=/td/store/json-c-0.18-x86_64",
                "-DCMAKE_C_COMPILER={root}/wb/cc",
                "-DCMAKE_MAKE_PROGRAM={root}/wb/make",
                "-DCMAKE_AR={in:binutils-x86-64-self}/bin/ar",
                "-DCMAKE_RANLIB={in:binutils-x86-64-self}/bin/ranlib",
                "-DCMAKE_NM={in:binutils-x86-64-self}/bin/nm",
                "-DCMAKE_STRIP={in:binutils-x86-64-self}/bin/strip",
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
        .env("PATH", &path)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                cmake,
                "--build",
                "{root}/build",
                "--target",
                "json-c",
                "--parallel",
                "{jobs}",
            ],
        )
        .env("PATH", &path)
        .env("SHELL", POST_BOOTSTRAP_SH)
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
    // The installed public header set of json-c 0.18's CMakeLists.txt, plus
    // the two headers its configure step generates into the build tree.
    steps.push(Step::CopyFiles {
        files: [
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
        ]
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
        .native_inputs(&[
            "cmake-x86-64",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "make-x86-64-self",
            "busybox-x86-64",
        ])
        .inputs(&["linux-headers-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    #[test]
    fn archives_are_compiled_under_the_shipped_target_profile() {
        let recipe = recipe();
        let inputs = recipe.native_inputs.clone().unwrap_or_default();
        for required in ["gcc-x86-64-self", "binutils-x86-64-self"] {
            assert!(inputs.iter().any(|input| input == required), "{required}");
        }
        for bootstrap in ["gcc-x86-64-native", "binutils-x86-64-native"] {
            assert!(
                !inputs.iter().any(|input| input == bootstrap),
                "{bootstrap}"
            );
        }
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
            "-g1",
            "-ffile-prefix-map={root}=/td-build-root",
            "-ffile-prefix-map={src}=/td-build",
        ] {
            assert!(wrapper.contains(required), "missing {required}");
        }
    }
}
