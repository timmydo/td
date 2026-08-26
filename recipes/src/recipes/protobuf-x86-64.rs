use crate::ladder::{
    post_bootstrap_path, relocate_ld_scripts, unpack_into, unpack_keep_top, POST_BOOTSTRAP_SH,
};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// Protobuf 31.1's protoc is a build-only input for Codex's code-mode protocol.
// Build it from the official source with td's native C++ platform instead of
// executing the target-specific binaries shipped by protoc-bin-vendored. The
// exact Abseil release declared by Protobuf is a separate fixed-output source;
// CMake's local FetchContent override consumes it with downloads disabled.
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let ngpp = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/g++";
    let nbin = "{in:binutils-x86-64-self}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let cmake = "{in:cmake-x86-64}/bin/cmake";
    let path = format!("{{root}}/wb:{{tools}}:{nbin}:{}", post_bootstrap_path());
    let mut steps = unpack_into("protobuf-x86-64-source", "{src}");

    steps.extend(unpack_into("abseil-cpp-x86-64-source", "{root}/abseil"));
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    // Static C++ links pull glibc's libm.a GNU ld script. Relocate a private
    // sysroot copy so its GROUP members resolve through -B/-L instead of the
    // configured absolute store prefix.
    steps.push(Step::CopyTree {
        from: format!("{xglibc}/lib"),
        dest: "{root}/sysroot/lib".into(),
    });
    steps.push(relocate_ld_scripts(
        "{root}/sysroot",
        "/td/store/glibc-2.41-x86_64",
    ));
    steps.push(Step::ToolFarm {
        links: [
            "awk", "basename", "cat", "chmod", "cmp", "cp", "cut", "dirname", "echo", "env",
            "expr", "false", "find", "grep", "head", "install", "ln", "ls", "mkdir", "mktemp",
            "mv", "printf", "pwd", "rm", "sed", "sort", "tail", "test", "touch", "tr", "true",
            "uname", "wc", "which", "xargs",
        ]
        .iter()
        .map(|name| ((*name).into(), "{in:busybox-x86-64}/bin/busybox".into()))
        .collect(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\nexec \"{ngcc}\" -static -idirafter \"{xglibc}/include\" \
             -idirafter \"{{root}}/kh\" -B\"{nbin}/\" \
             -B\"{{root}}/sysroot/lib\" -L\"{{root}}/sysroot/lib\" \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/c++".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\nexec \"{ngpp}\" -static -static-libgcc -static-libstdc++ \
             -idirafter \"{xglibc}/include\" -idirafter \"{{root}}/kh\" \
             -B\"{nbin}/\" -B\"{{root}}/sysroot/lib\" \
             -L\"{{root}}/sysroot/lib\" \"$@\"\n"
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
                "-DCMAKE_C_COMPILER={root}/wb/cc",
                "-DCMAKE_CXX_COMPILER={root}/wb/c++",
                "-DCMAKE_MAKE_PROGRAM={root}/wb/make",
                "-DCMAKE_AR={in:binutils-x86-64-self}/bin/ar",
                "-DCMAKE_LINKER={in:binutils-x86-64-self}/bin/ld",
                "-DCMAKE_NM={in:binutils-x86-64-self}/bin/nm",
                "-DCMAKE_OBJCOPY={in:binutils-x86-64-self}/bin/objcopy",
                "-DCMAKE_RANLIB={in:binutils-x86-64-self}/bin/ranlib",
                "-DCMAKE_STRIP={in:binutils-x86-64-self}/bin/strip",
                "-DBUILD_SHARED_LIBS=OFF",
                "-Dprotobuf_INSTALL=OFF",
                "-Dprotobuf_BUILD_TESTS=OFF",
                "-Dprotobuf_BUILD_CONFORMANCE=OFF",
                "-Dprotobuf_BUILD_EXAMPLES=OFF",
                "-Dprotobuf_BUILD_PROTOBUF_BINARIES=ON",
                "-Dprotobuf_BUILD_PROTOC_BINARIES=ON",
                "-Dprotobuf_WITH_ZLIB=OFF",
                "-Dprotobuf_FORCE_FETCH_DEPENDENCIES=ON",
                "-DFETCHCONTENT_FULLY_DISCONNECTED=ON",
                "-DFETCHCONTENT_SOURCE_DIR_ABSL={root}/abseil",
                "-DABSL_BUILD_TESTING=OFF",
                "-DABSL_PROPAGATE_CXX_STD=ON",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_BOOTSTRAP_SH)
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
                "protoc",
                "--parallel",
                "{jobs}",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_BOOTSTRAP_SH)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{root}/build/protoc-31.1.0".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Symlink {
        target: "protoc-31.1.0".into(),
        link: "{out}/bin/protoc".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/protoc".into()],
        exec: true,
    });
    steps.push(Step::assert_static(&["{out}/bin/protoc"]));
    steps.push(Step::run("{out}", &["{out}/bin/protoc", "--version"]).env("PATH", &path));

    Recipe::mesboot("protobuf-x86-64", "31.1")
        .source_input("protobuf-x86-64-source")
        .native_inputs(&[
            "cmake-x86-64",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "make-x86-64-self",
            "busybox-x86-64",
        ])
        .inputs_owned(
            [
                "linux-headers-x86-64",
                "abseil-cpp-x86-64-source",
            ]
            .map(str::to_string)
            .to_vec(),
        )
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check protobuf-x86-64: build source protoc with td's native C++ platform and validate its static executable"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run protobuf-x86-64 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn protobuf_and_abseil_are_both_fixed_source_inputs() {
        let recipe = recipe();
        assert_eq!(
            recipe.source_input.as_deref(),
            Some("protobuf-x86-64-source")
        );
        assert!(recipe
            .inputs
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|input| input == "abseil-cpp-x86-64-source"));
    }
}
