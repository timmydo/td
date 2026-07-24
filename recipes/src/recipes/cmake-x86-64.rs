use crate::ladder::{mesboot0_inputs, relocate_ld_scripts, unpack_into, unpack_keep_top, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// CMake 3.31.12 is the one explicitly approved new build-only dependency for the
// source Rust bridge. Rust's in-tree LLVM 22 requires CMake >= 3.20. This rung
// bootstraps CMake from its release source with td's native GCC/G++, native GNU
// Make, and BusyBox userland. All bundled third-party libraries are used; OpenSSL,
// Qt, curses, Sphinx, Ninja, and host libraries are absent.
//
// The produced cmake is fully static. It can therefore configure LLVM inside a
// later recipe sandbox without inheriting a runtime loader/library closure of its
// own. The Make wrapper forces the declared bash as GNU Make's recipe shell,
// overriding CMake's generated `SHELL = /bin/sh` in a root with no host /bin.
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let ngpp = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/g++";
    let nbin = "{in:binutils-x86-64-self}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let path = format!("{{root}}/wb:{{tools}}:{nbin}");
    let mut steps = unpack_into("cmake-x86-64-source", "{src}");

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
            "awk", "basename", "cat", "chmod", "cmp", "cp", "cut", "date", "dirname", "echo",
            "env", "expr", "false", "find", "grep", "head", "install", "ln", "ls", "mkdir",
            "mktemp", "mv", "printf", "pwd", "rm", "sed", "sort", "tail", "tee", "test", "touch",
            "tr", "true", "uname", "wc", "which", "xargs",
        ]
        .iter()
        .map(|name| ((*name).into(), "{in:busybox-x86-64}/bin/busybox".into()))
        .collect(),
    });
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    // CMake itself is static, but shared-library compiler probes must remain
    // possible. The wrapper only drops -static when the caller requests -shared.
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{SH}\n\
             for a in \"$@\"; do case \"$a\" in -shared) exec \"{ngcc}\" \
             -idirafter \"{xglibc}/include\" -idirafter \"{{root}}/kh\" \
             -B\"{nbin}/\" -B\"{{root}}/sysroot/lib\" \
             -L\"{{root}}/sysroot/lib\" \"$@\";; esac; done\n\
             exec \"{ngcc}\" -static -idirafter \"{xglibc}/include\" \
             -idirafter \"{{root}}/kh\" -B\"{nbin}/\" \
             -B\"{{root}}/sysroot/lib\" -L\"{{root}}/sysroot/lib\" \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/c++".into(),
        content: format!(
            "#!{SH}\n\
             for a in \"$@\"; do case \"$a\" in -shared) exec \"{ngpp}\" \
             -idirafter \"{xglibc}/include\" -idirafter \"{{root}}/kh\" \
             -B\"{nbin}/\" -B\"{{root}}/sysroot/lib\" \
             -L\"{{root}}/sysroot/lib\" \"$@\";; esac; done\n\
             exec \"{ngpp}\" -static -static-libgcc -static-libstdc++ \
             -idirafter \"{xglibc}/include\" -idirafter \"{{root}}/kh\" \
             -B\"{nbin}/\" -B\"{{root}}/sysroot/lib\" \
             -L\"{{root}}/sysroot/lib\" \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/make".into(),
        content: format!("#!{SH}\nexec \"{{in:make-x86-64}}/bin/make\" SHELL=\"{SH}\" \"$@\"\n"),
        exec: true,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./bootstrap",
                "--prefix={out}",
                "--parallel={jobs}",
                "--no-system-libs",
                "--no-qt-gui",
                "--no-debugger",
                "--",
                "-DCMAKE_BUILD_TYPE=Release",
                "-DCMAKE_USE_OPENSSL=OFF",
                "-DBUILD_TESTING=OFF",
                "-DBUILD_CursesDialog=OFF",
                "-DBUILD_QtDialog=OFF",
                "-DSPHINX_MAN=OFF",
                "-DSPHINX_HTML=OFF",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("CXX", "{root}/wb/c++")
        .env("MAKE", "{root}/wb/make")
        .env("CFLAGS", "-O2")
        .env("CXXFLAGS", "-O2")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run("{src}", &["{root}/wb/make", "-j{jobs}"])
            .env("PATH", &path)
            .env("CONFIG_SHELL", SH)
            .env("SHELL", SH)
            .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run("{src}", &["{root}/wb/make", "install"])
            .env("PATH", &path)
            .env("CONFIG_SHELL", SH)
            .env("SHELL", SH)
            .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/cmake".into()],
        exec: true,
    });
    steps.push(Step::assert_static(&["{out}/bin/cmake"]));
    steps.push(Step::run("{out}", &["{out}/bin/cmake", "--version"]).env("PATH", &path));

    Recipe::mesboot("cmake-x86-64", "3.31.12")
        .source_input("cmake-x86-64-source")
        .native_inputs(&[
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "make-x86-64",
            "busybox-x86-64",
        ])
        .inputs_owned(mesboot0_inputs(&["linux-headers-x86-64"]))
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check cmake-x86-64: build source CMake with td's POSIX native GCC and validate its static executable"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run cmake-x86-64 daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
