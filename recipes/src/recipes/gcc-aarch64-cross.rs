use crate::ladder::{
    gcc_install_headers_without_tar, ladder_split_debug, post_rust_inputs, post_rust_tool_farm,
    unpack_into, unpack_keep_top, POST_RUST_SH,
};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step, TextEdit};

// Native x86-64 build tools that emit ARM objects for the portable mail build.
// Rust supplies target libc and compiler runtime; build neither here.
pub fn recipe() -> Recipe {
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin";
    let binutils = "{in:binutils-x86-64-self}/bin";
    let glibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let path = format!("{{tools}}:{binutils}:{gcc}:{{in:make-x86-64-self}}/bin");
    let flags = format!(
        "-O2 -fno-omit-frame-pointer -g1 -idirafter {glibc}/include \
         -idirafter {{root}}/kh -B{binutils}/ -B{glibc}/lib -L{glibc}/lib \
         -ffile-prefix-map={{root}}=/td-build-root -ffile-prefix-map={{src}}=/td-build"
    );
    let compiler = |name| format!("{gcc}/{name} -static");
    let cc = compiler("gcc");
    let cxx = compiler("g++");
    let environment = |step: Step| {
        step.env("PATH", &path)
            .env("CONFIG_SHELL", POST_RUST_SH)
            .env("SHELL", POST_RUST_SH)
            .env("CC", &cc)
            .env("CXX", &cxx)
            // GMP probes this command without CFLAGS_FOR_BUILD or LDFLAGS.
            .env("CC_FOR_BUILD", &format!("{cc} {flags}"))
            .env("CXX_FOR_BUILD", &format!("{cxx} {flags}"))
            .env("CPP", &format!("{cc} -E"))
            .env(
                "CPPFLAGS",
                &format!("-idirafter {glibc}/include -idirafter {{root}}/kh"),
            )
            .env("MAKEINFO", "true")
            .env("CFLAGS", &flags)
            .env("CXXFLAGS", &flags)
            .env("CFLAGS_FOR_BUILD", &flags)
            .env("CXXFLAGS_FOR_BUILD", &flags)
            .env("LDFLAGS", "-static -Wl,--build-id=sha1")
            .env("SOURCE_DATE_EPOCH", "1")
    };
    let mut steps = unpack_into("gcc-aarch64-cross-source", "{src}");
    steps.extend(unpack_into("binutils-244-source", "{root}/binutils"));
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(post_rust_tool_farm(Some("{in:gawk-x86-64-self}/bin/gawk")));
    for (input, directory, link) in [
        ("gmp63", "gmp-6.3.0", "gmp"),
        ("mpfr421", "mpfr-4.2.1", "mpfr"),
        ("mpc131", "mpc-1.3.1", "mpc"),
    ] {
        steps.extend(unpack_keep_top(input, "{src}"));
        steps.push(Step::Symlink {
            target: directory.into(),
            link: format!("{{src}}/{link}"),
        });
    }
    // Avoid the shell's legacy backtick quote handling in GMP's wrapper.
    steps.push(Step::substitute_text(
        "{src}/gmp-6.3.0/config.sub",
        vec![TextEdit::new(
            r#"configfsf_sub="`echo \"$0\" | sed 's/config.sub$/configfsf.sub/'`""#,
            r#"configfsf_sub="$(echo "$0" | sed 's/config.sub$/configfsf.sub/')""#,
            1,
        )],
    ));
    steps.push(gcc_install_headers_without_tar());
    for directory in ["{src}", "{root}/binutils"] {
        steps.push(Step::PatchShebangs {
            dir: directory.into(),
            shell: POST_RUST_SH.into(),
        });
    }
    for directory in ["{root}/binutils-build", "{root}/gcc-build"] {
        steps.push(Step::MkDir {
            path: directory.into(),
        });
    }
    steps.push(environment(Step::run(
        "{root}/binutils-build",
        &[
            POST_RUST_SH,
            "{root}/binutils/configure",
            "--build=x86_64-pc-linux-gnu",
            "--host=x86_64-pc-linux-gnu",
            "--target=aarch64-linux-musl",
            "--prefix={out}",
            "--disable-nls",
            "--disable-gold",
            "--disable-werror",
            "--enable-deterministic-archives",
            "--disable-plugins",
            "--disable-gprofng",
            "--disable-multilib",
            "--disable-shared",
            "--enable-static",
        ],
    )));
    let make = |directory: &str, targets: &[&str]| {
        let shell = format!("SHELL={POST_RUST_SH}");
        // GCC derives these from CC/CXX for a native build/host pair and
        // overwrites the environment; GMP probes them without other flags.
        let build_cc = format!("CC_FOR_BUILD={cc} {flags}");
        let build_cxx = format!("CXX_FOR_BUILD={cxx} {flags}");
        let mut args = vec![
            "{in:make-x86-64-self}/bin/make",
            "-j{jobs}",
            shell.as_str(),
            "MAKEINFO=true",
            &build_cc,
            &build_cxx,
        ];
        if directory == "{root}/binutils-build" {
            // Libtool's -static only chooses static project libraries.
            // -all-static also prevents a host dynamic-loader dependency.
            args.push("AM_LDFLAGS=-all-static");
        }
        args.extend_from_slice(targets);
        environment(Step::run(directory, &args))
    };
    steps.push(make("{root}/binutils-build", &["all"]));
    steps.push(make("{root}/binutils-build", &["install"]));
    steps.push(Step::assert_static(&[
        "{out}/bin/aarch64-linux-musl-as",
        "{out}/bin/aarch64-linux-musl-ar",
        "{out}/bin/aarch64-linux-musl-ld",
        "{out}/bin/aarch64-linux-musl-nm",
        "{out}/bin/aarch64-linux-musl-objcopy",
    ]));
    steps.push(environment(Step::run(
        "{root}/gcc-build",
        &[
            POST_RUST_SH,
            "{src}/configure",
            "--build=x86_64-pc-linux-gnu",
            "--host=x86_64-pc-linux-gnu",
            "--target=aarch64-linux-musl",
            "--prefix={out}",
            "--with-as={out}/bin/aarch64-linux-musl-as",
            "--with-ld={out}/bin/aarch64-linux-musl-ld",
            // Reinforce GCC's generic GMP host selection; no m4 is needed.
            "--disable-assembly",
            "--enable-languages=c",
            "--without-headers",
            "--with-newlib",
            "--disable-bootstrap",
            "--disable-multilib",
            "--disable-shared",
            "--disable-threads",
            "--disable-nls",
            "--disable-libssp",
            "--disable-libgomp",
            "--disable-libquadmath",
            "--disable-libatomic",
            "--disable-libvtv",
            "--disable-libitm",
            "--disable-libstdcxx",
            "--disable-libcc1",
            "--disable-lto",
            "--disable-plugin",
            "--disable-decimal-float",
            "--disable-werror",
        ],
    )));
    steps.push(make("{root}/gcc-build", &["all-gcc"]));
    steps.push(make("{root}/gcc-build", &["install-gcc"]));
    steps.push(Step::Require {
        paths: vec![
            "{out}/bin/aarch64-linux-musl-gcc".into(),
            "{out}/bin/aarch64-linux-musl-as".into(),
            "{out}/bin/aarch64-linux-musl-ar".into(),
            "{out}/bin/aarch64-linux-musl-ld".into(),
            "{out}/bin/aarch64-linux-musl-nm".into(),
            "{out}/bin/aarch64-linux-musl-objcopy".into(),
        ],
        exec: true,
    });
    steps.push(ladder_split_debug("{out}"));
    steps.push(Step::assert_static(&[
        "{out}/bin/aarch64-linux-musl-gcc",
        "{out}/libexec/gcc/aarch64-linux-musl/14.3.0/cc1",
        "{out}/bin/aarch64-linux-musl-as",
        "{out}/bin/aarch64-linux-musl-ar",
        "{out}/bin/aarch64-linux-musl-ld",
        "{out}/bin/aarch64-linux-musl-nm",
        "{out}/bin/aarch64-linux-musl-objcopy",
    ]));
    steps.push(Step::WriteFile {
        path: "{root}/arm-probe.c".into(),
        content: "#if !defined(__aarch64__) || !defined(__linux__)\n\
                  #error wrong target\n\
                  #endif\n\
                  _Static_assert(sizeof(void *) == 8, \"pointer width\");\n\
                  _Static_assert(sizeof(long) == 8, \"long width\");\n\
                  _Static_assert(__BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__, \"byte order\");\n\
                  unsigned long arm_probe(unsigned long x) { return x * 17 + 3; }\n"
            .into(),
        exec: false,
    });
    steps.push(Step::run(
        "{root}",
        &[
            "{out}/bin/aarch64-linux-musl-gcc",
            "-nostdinc",
            "-march=armv8-a",
            "-fno-omit-frame-pointer",
            "-mno-omit-leaf-frame-pointer",
            "-g1",
            "-ffile-prefix-map={root}=/td-build-root",
            "-O2",
            "-fPIC",
            "-c",
            "{root}/arm-probe.c",
            "-o",
            "{root}/arm-probe.o",
        ],
    ));
    steps.push(Step::run(
        "{root}",
        &[
            "{out}/bin/aarch64-linux-musl-ar",
            "rcsD",
            "{root}/arm-probe.a",
            "{root}/arm-probe.o",
        ],
    ));
    steps.push(Step::run(
        "{root}",
        &[
            "{out}/bin/aarch64-linux-musl-ld",
            "-r",
            "--whole-archive",
            "{root}/arm-probe.a",
            "-o",
            "{root}/arm-linked.o",
        ],
    ));
    Recipe::mesboot("gcc-aarch64-cross", "14.3.0")
        .source_input("gcc-14-source")
        .native_inputs(&post_rust_inputs(
            Some("gawk-x86-64-self"),
            &[
                "gcc-x86-64-self",
                "binutils-x86-64-self",
                "glibc-x86-64",
                "make-x86-64-self",
            ],
        ))
        .inputs(&[
            "binutils-244-source",
            "linux-headers-x86-64",
            "gmp63",
            "mpfr421",
            "mpc131",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check gcc-aarch64-cross: build static host tools and compile, archive and link an ARM object"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run gcc-aarch64-cross 1
"#,
        ).with_runner(CheckRunner::BuildOnly)])
}
