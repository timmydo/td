use crate::ladder::{mesboot0_inputs, unpack_into, unpack_keep_top, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step, TextEdit};

// rust-toolchain is the shipped, source-built Rust 1.96.0 toolchain. The exact
// upstream Rust 1.95.0 snapshot is transformed by the sibling `rust-stage0`
// recipe and used only to enter this build. Rust's source release supplies the
// compiler, standard-library, in-tree Cargo, vendored crate closure, and LLVM
// source. No ambient network, host /bin or /usr, prebuilt LLVM, or downloaded
// stage0 artifact is copied into this output.
//
// `build.full-bootstrap = true` is load-bearing: stage1 builds the in-tree std,
// then stage1 rebuilds rustc as stage2 and stage2 rebuilds the final std instead
// of uplifting stage1. Cargo is built from `src/tools/cargo` by the source-built
// compiler. CMake is the explicitly approved build-only dependency used to build
// LLVM; Ninja is disabled and the td-built GNU Make drives its generated graph.
//
// td Cargo builds are normatively offline and git dependencies are unsupported.
// The source Cargo manifest is therefore narrowed to curl/libgit2 without their
// OpenSSL/SSH features. That avoids adding Perl, OpenSSL, or an extra rustls-ffi
// crate outside the pinned upstream source closure. The corresponding vendored
// checksum and Cargo.lock edits are literal and count-checked below.
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let ngpp = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/g++";
    let nbin = "{in:binutils-x86-64-self}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let py = "{in:python-mesboot}/bin/python3";
    let path = format!("{{tools}}:{nbin}");
    let mut steps = unpack_into("rust-source", "{src}");

    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    for dir in [
        "{root}/wb",
        "{root}/home",
        "{root}/tmp",
        "{root}/cargo-home",
    ] {
        steps.push(Step::MkDir { path: dir.into() });
    }
    // Cargo verifies every vendored file against each crate's
    // .cargo-checksum.json. Keep that closure byte-identical while rewriting
    // upstream Rust/LLVM shell entry points, then restore it before the
    // intentional manifest + checksum edits below.
    steps.push(Step::run(
        "{root}",
        &[
            "{in:busybox-x86-64}/bin/busybox",
            "mv",
            "{src}/vendor",
            "{root}/vendor",
        ],
    ));
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    steps.push(Step::run(
        "{root}",
        &[
            "{in:busybox-x86-64}/bin/busybox",
            "mv",
            "{root}/vendor",
            "{src}/vendor",
        ],
    ));

    // Native compiler/linker wrappers for LLVM, rustc, std, Cargo build scripts,
    // and every build-time executable. Outputs are dynamic against td glibc with
    // an absolute td interpreter/RUNPATH; libgcc and libstdc++ are static so the
    // final compiler closure has no unbuilt shared-GCC edge.
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{SH}\nexec \"{ngcc}\" -idirafter \"{xglibc}/include\" \
             -idirafter \"{{root}}/kh\" -B\"{nbin}/\" -B\"{xglibc}/lib\" \
             -L\"{xglibc}/lib\" -static-libgcc \
             -Wl,--dynamic-linker=\"{xglibc}/lib/ld-linux-x86-64.so.2\" \
             -Wl,--enable-new-dtags -Wl,-rpath -Wl,\"{xglibc}/lib\" \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/c++".into(),
        content: format!(
            "#!{SH}\nexec \"{ngpp}\" -idirafter \"{xglibc}/include\" \
             -idirafter \"{{root}}/kh\" -B\"{nbin}/\" -B\"{xglibc}/lib\" \
             -L\"{xglibc}/lib\" -static-libgcc -static-libstdc++ \
             -Wl,--dynamic-linker=\"{xglibc}/lib/ld-linux-x86-64.so.2\" \
             -Wl,--enable-new-dtags -Wl,-rpath -Wl,\"{xglibc}/lib\" \"$@\"\n"
        ),
        exec: true,
    });
    // CMake's Unix Makefiles spell `SHELL = /bin/sh`. A command-line SHELL
    // override propagates through recursive make and keeps that host path dead.
    steps.push(Step::WriteFile {
        path: "{root}/wb/make".into(),
        content: format!("#!{SH}\nexec \"{{in:make-x86-64}}/bin/make\" SHELL=\"{SH}\" \"$@\"\n"),
        exec: true,
    });
    steps.push(Step::ToolFarm {
        links: [
            "awk", "basename", "cat", "chmod", "cmp", "comm", "cp", "cut", "date", "dirname",
            "echo", "env", "expr", "false", "find", "grep", "head", "install", "ln", "ls", "mkdir",
            "mktemp", "mv", "printf", "pwd", "readlink", "realpath", "rm", "rmdir", "sed", "sleep",
            "sort", "tail", "tee", "test", "touch", "tr", "true", "uname", "wc", "which", "xargs",
        ]
        .iter()
        .map(|name| ((*name).into(), "{in:busybox-x86-64}/bin/busybox".into()))
        .chain([
            ("sh".into(), SH.into()),
            ("bash".into(), SH.into()),
            ("python".into(), py.into()),
            ("python3".into(), py.into()),
            ("cc".into(), "{root}/wb/cc".into()),
            ("gcc".into(), "{root}/wb/cc".into()),
            ("c++".into(), "{root}/wb/c++".into()),
            ("g++".into(), "{root}/wb/c++".into()),
            ("make".into(), "{root}/wb/make".into()),
            ("cmake".into(), "{in:cmake-x86-64}/bin/cmake".into()),
            ("ar".into(), format!("{nbin}/ar")),
            ("ranlib".into(), format!("{nbin}/ranlib")),
            ("ld".into(), format!("{nbin}/ld")),
            ("as".into(), format!("{nbin}/as")),
            ("nm".into(), format!("{nbin}/nm")),
            ("objcopy".into(), format!("{nbin}/objcopy")),
            ("objdump".into(), format!("{nbin}/objdump")),
            ("readelf".into(), format!("{nbin}/readelf")),
            ("strip".into(), format!("{nbin}/strip")),
        ])
        .collect(),
    });

    // Remove native TLS/SSH transports from in-tree Cargo. td's recipe graph
    // fetches fixed-output sources before Cargo and invokes Cargo offline; these
    // optional transports would otherwise require undeclared OpenSSL + Perl.
    steps.push(Step::substitute_text(
        "{src}/src/tools/cargo/Cargo.toml",
        vec![
            TextEdit::new(
                "curl = \"0.4.49\"",
                "curl = { version = \"0.4.49\", default-features = false }",
                1,
            ),
            TextEdit::new(
                "curl-sys = \"0.4.87\"",
                "curl-sys = { version = \"0.4.87\", default-features = false }",
                1,
            ),
            TextEdit::new(
                "git2 = \"0.20.4\"",
                "git2 = { version = \"0.20.4\", default-features = false }",
                1,
            ),
        ],
    ));
    steps.push(Step::substitute_text(
        "{src}/vendor/git2-curl-0.21.0/Cargo.toml",
        vec![TextEdit::new(
            "[dependencies.curl]\nversion = \"0.4.33\"",
            "[dependencies.curl]\nversion = \"0.4.33\"\ndefault-features = false",
            1,
        )],
    ));
    steps.push(Step::substitute_text(
        "{src}/vendor/git2-curl-0.21.0/.cargo-checksum.json",
        vec![TextEdit::new(
            "49cac7eabb933177c492b5fa3a57813fb19e7471bb64d76777d172b81588738d",
            "7d11eab05615bd37af038f624b6df23ffacde4ecea7857d41dc37a0ad8dcc0d5",
            1,
        )],
    ));
    steps.push(Step::substitute_text(
        "{src}/src/tools/cargo/Cargo.lock",
        vec![
            TextEdit::new(
                " \"curl-sys\",\n \"libc\",\n \"openssl-probe\",\n \"openssl-sys\",\n \"schannel\",",
                " \"curl-sys\",\n \"libc\",\n \"schannel\",",
                1,
            ),
            TextEdit::new(
                " \"libnghttp2-sys\",\n \"libz-sys\",\n \"openssl-sys\",\n \"pkg-config\",",
                " \"libnghttp2-sys\",\n \"libz-sys\",\n \"pkg-config\",",
                1,
            ),
            TextEdit::new(
                " \"libgit2-sys\",\n \"log\",\n \"openssl-probe\",\n \"openssl-sys\",\n \"url\",",
                " \"libgit2-sys\",\n \"log\",\n \"url\",",
                1,
            ),
            TextEdit::new(
                " \"cc\",\n \"libc\",\n \"libssh2-sys\",\n \"libz-sys\",\n \"openssl-sys\",\n \"pkg-config\",",
                " \"cc\",\n \"libc\",\n \"libz-sys\",\n \"pkg-config\",",
                1,
            ),
            TextEdit::new(
                "[[package]]\nname = \"libssh2-sys\"\nversion = \"0.3.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"220e4f05ad4a218192533b300327f5150e809b54c4ec83b5a1d91833601811b9\"\ndependencies = [\n \"cc\",\n \"libc\",\n \"libz-sys\",\n \"openssl-sys\",\n \"pkg-config\",\n \"vcpkg\",\n]\n\n",
                "",
                1,
            ),
            TextEdit::new(
                "[[package]]\nname = \"openssl-probe\"\nversion = \"0.1.6\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"d05e27ee213611ffe7d6348b942e8f942b37114c00cc03cec254295a4a17852e\"\n\n",
                "",
                1,
            ),
        ],
    ));

    steps.push(Step::WriteFile {
        path: "{src}/bootstrap.toml".into(),
        content: format!(
            r#"change-id = 154508

[build]
build = "x86_64-unknown-linux-gnu"
host = ["x86_64-unknown-linux-gnu"]
target = ["x86_64-unknown-linux-gnu"]
build-dir = "{{root}}/rust-build"
rustc = "{{in:rust-stage0}}/bin/rustc"
rustdoc = "{{in:rust-stage0}}/bin/rustdoc"
cargo = "{{in:rust-stage0}}/bin/cargo"
python = "{py}"
submodules = false
locked-deps = true
vendor = true
full-bootstrap = true
extended = true
tools = ["cargo"]
docs = false
compiler-docs = false
sanitizers = false
profiler = false
optimized-compiler-builtins = false
cargo-native-static = false
jobs = {{jobs}}

[rust]
channel = "stable"
optimize = true
debug = false
debug-assertions = false
debuginfo-level = 0
debuginfo-level-std = 0
debuginfo-level-tools = 0
incremental = false
rpath = true
remap-debuginfo = true
download-rustc = false
codegen-backends = ["llvm"]
lld = false
llvm-tools = false

[llvm]
download-ci-llvm = false
ninja = false
optimize = true
release-debuginfo = false
assertions = false
tests = false
plugins = false
static-libstdcpp = true
libzstd = false
targets = "X86"
experimental-targets = ""
link-shared = false
build-config = {{ LLVM_ENABLE_ZLIB = "OFF", LLVM_ENABLE_ZSTD = "OFF", LLVM_ENABLE_TERMINFO = "OFF", LLVM_ENABLE_LIBEDIT = "OFF", LLVM_ENABLE_LIBXML2 = "OFF", LLVM_ENABLE_CURL = "OFF", LLVM_INCLUDE_TESTS = "OFF", LLVM_INCLUDE_BENCHMARKS = "OFF", LLVM_INCLUDE_EXAMPLES = "OFF" }}

[target.x86_64-unknown-linux-gnu]
cc = "{{root}}/wb/cc"
cxx = "{{root}}/wb/c++"
linker = "{{root}}/wb/cc"
ar = "{nbin}/ar"
ranlib = "{nbin}/ranlib"
llvm-libunwind = "no"
sanitizers = false
profiler = false
rpath = true
optimized-compiler-builtins = false
jemalloc = false
"#
        ),
        exec: false,
    });

    steps.push(
        Step::run(
            "{src}",
            &[
                py,
                "x.py",
                "build",
                "--stage",
                "2",
                "library/std",
                "compiler/rustc",
                "src/tools/rustdoc",
                "src/tools/cargo",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("CXX", "{root}/wb/c++")
        .env("AR", &format!("{nbin}/ar"))
        .env("RANLIB", &format!("{nbin}/ranlib"))
        .env("HOME", "{root}/home")
        .env("CARGO_HOME", "{root}/cargo-home")
        .env("CARGO_NET_OFFLINE", "true")
        .env("TMPDIR", "{root}/tmp")
        .env("SOURCE_DATE_EPOCH", "1")
        // python-mesboot is an i686 bootstrap tool dynamically linked to this
        // declared libc. x86_64 products carry their own td RUNPATH and ignore
        // the wrong-class candidates while inheriting this build-only setting.
        .env("LD_LIBRARY_PATH", "{in:glibc-mesboot-shared}/lib")
        .env("MAKEFLAGS", "")
        .env("MFLAGS", "")
        .env("GNUMAKEFLAGS", "")
        .env("MAKELEVEL", ""),
    );

    // Both stages and the final std must exist before anything is installed.
    // `full-bootstrap = true` above makes these real builds rather than uplifted
    // copies. The equality checks below reject an uplifted compiler or std, while
    // the daily bridge check independently rejects stage0 references and exact
    // stage0 artifact copies in the installed tree.
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "test -x '{root}/rust-build/x86_64-unknown-linux-gnu/stage1/bin/rustc' && \
                 test -x '{root}/rust-build/x86_64-unknown-linux-gnu/stage2/bin/rustc' && \
                 ls '{root}'/rust-build/x86_64-unknown-linux-gnu/stage1/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-*.rlib >/dev/null && \
                 ls '{root}'/rust-build/x86_64-unknown-linux-gnu/stage2/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-*.rlib >/dev/null && \
                 test -x '{root}/rust-build/x86_64-unknown-linux-gnu/stage2-tools-bin/cargo' || { echo 'full-bootstrap did not produce all stage1/stage2 rustc, std, and Cargo outputs' >&2; exit 1; }; \
                 ! cmp -s '{root}/rust-build/x86_64-unknown-linux-gnu/stage1/bin/rustc' '{root}/rust-build/x86_64-unknown-linux-gnu/stage2/bin/rustc' || { echo 'full-bootstrap uplifted stage1 rustc as stage2' >&2; exit 1; }; \
                 for s1 in '{root}'/rust-build/x86_64-unknown-linux-gnu/stage1/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-*.rlib; do \
                   for s2 in '{root}'/rust-build/x86_64-unknown-linux-gnu/stage2/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-*.rlib; do \
                     cmp -s \"$s1\" \"$s2\" && { echo 'full-bootstrap uplifted a stage1 libstd into stage2' >&2; exit 1; } || :; \
                   done; \
                 done",
            ],
        )
        .env("PATH", &path),
    );
    steps.push(Step::CopyTree {
        from: "{root}/rust-build/x86_64-unknown-linux-gnu/stage2".into(),
        dest: "{out}".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{root}/rust-build/x86_64-unknown-linux-gnu/stage2-tools-bin/cargo".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/bin/rustc".into(),
            "{out}/bin/rustdoc".into(),
            "{out}/bin/cargo".into(),
        ],
        exec: true,
    });
    steps.push(
        Step::run("{out}", &["{out}/bin/rustc", "--version", "--verbose"]).env("PATH", &path),
    );
    steps.push(Step::run("{out}", &["{out}/bin/cargo", "--version"]).env("PATH", &path));
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "readelf -l '{out}/bin/rustc' | grep -F '{in:glibc-x86-64}' >/dev/null || { echo 'stage2 rustc does not use td glibc' >&2; exit 1; }; \
                 readelf -d '{out}/bin/cargo' | grep -E 'libssl|libcrypto|libssh2' && { echo 'source Cargo retained a forbidden TLS/SSH native dependency' >&2; exit 1; } || :; \
                 for llvm in '{out}'/lib/libLLVM*.so*; do test ! -e \"$llvm\" || { echo 'stage2 copied a prebuilt/shared LLVM' >&2; exit 1; }; done; \
                 grep -R -a -F -l '{in:rust-stage0}' '{out}' >'{root}/stage0-refs' && { echo 'stage2 output references rust-stage0' >&2; exit 1; } || :",
            ],
        )
        .env("PATH", &path),
    );

    Recipe::mesboot("rust-toolchain", "1.96.0")
        .source_input("rust-source")
        .native_inputs(&[
            "rust-stage0",
            "cmake-x86-64",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "python-mesboot",
            "glibc-mesboot-shared",
            "make-x86-64",
            "busybox-x86-64",
        ])
        .inputs_owned(mesboot0_inputs(&["linux-headers-x86-64"]))
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check rust-toolchain: full-bootstraps and validates the source-built /td/store Rust 1.96.0 stage2 toolchain"
: "${TD_RECIPE_EVAL:=$PWD/recipes/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run rust-toolchain daily 1
"#,
        )
        .with_runner(CheckRunner::RustToolchain)])
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    #[test]
    fn vendored_crates_are_held_out_of_the_recursive_shebang_rewrite() {
        let steps = recipe().steps.expect("rust recipe steps");
        let patch = steps
            .iter()
            .position(|step| {
                matches!(
                    step,
                    Step::PatchShebangs { dir, .. } if dir == "{src}"
                )
            })
            .expect("source shebang rewrite");
        let before = steps.get(patch.saturating_sub(1)).expect("vendor hold-out");
        let after = steps.get(patch + 1).expect("vendor restore");
        assert!(matches!(
            before,
            Step::Run { argv, .. }
                if matches!(
                    argv.as_slice(),
                    [busybox, mv, from, to]
                        if busybox == "{in:busybox-x86-64}/bin/busybox"
                            && mv == "mv"
                            && from == "{src}/vendor"
                            && to == "{root}/vendor"
                )
        ));
        assert!(matches!(
            after,
            Step::Run { argv, .. }
                if matches!(
                    argv.as_slice(),
                    [busybox, mv, from, to]
                        if busybox == "{in:busybox-x86-64}/bin/busybox"
                            && mv == "mv"
                            && from == "{root}/vendor"
                            && to == "{src}/vendor"
                )
        ));
    }
}
