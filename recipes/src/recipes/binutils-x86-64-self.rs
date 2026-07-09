use crate::ladder::{base_inputs, base_path, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, Step};

pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let nbin = "{in:binutils-x86-64-native}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let path = format!("{nbin}:{}", base_path());
    let cip = format!("{xglibc}/include:{{root}}/kh");
    let mut steps = unpack_into("binutils-x86-64-self-source", "{src}");

    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("awk".into(), "{in:gawk}/bin/awk".into()),
            ("flex".into(), "{in:flex}/bin/flex".into()),
            ("lex".into(), "{in:flex}/bin/flex".into()),
            ("bison".into(), "{in:bison}/bin/bison".into()),
            ("yacc".into(), "{in:bison}/bin/bison".into()),
            ("make".into(), "{in:make}/bin/make".into()),
        ],
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{SH}\n\
             for a in \"$@\"; do case \"$a\" in -shared) exec \"{ngcc}\" -B{xglibc}/lib \"$@\";; esac; done\n\
             exec \"{ngcc}\" -static -B{xglibc}/lib \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./configure",
                "--build=x86_64-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--target=x86_64-pc-linux-gnu",
                "--prefix=/td/store/binutils-2.44-x86_64-self",
                "--disable-nls",
                "--disable-gold",
                "--disable-werror",
                "--enable-deterministic-archives",
                "--disable-plugins",
                "--disable-gprofng",
                "--disable-multilib",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("C_INCLUDE_PATH", &cip),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash}/bin/bash",
                "CONFIG_SHELL={in:bash}/bin/bash",
                "MAKEINFO=true",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("C_INCLUDE_PATH", &cip),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make}/bin/make",
                "SHELL={in:bash}/bin/bash",
                "MAKEINFO=true",
                "install",
                "prefix={out}",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH),
    );
    steps.push(Step::Require {
        paths: vec![
            "{out}/bin/as".into(),
            "{out}/bin/ld".into(),
            "{out}/bin/readelf".into(),
        ],
        exec: true,
    });

    Recipe::mesboot("binutils-x86-64-self", "2.44")
        .source_input("binutils-244-source")
        .native_inputs(&[
            "gcc-x86-64-native",
            "binutils-x86-64-native",
            "glibc-x86-64",
        ])
        .inputs_owned(base_inputs(&[
            "linux-headers-x86-64",
            "flex",
            "bison",
            "make",
        ]))
        .steps(steps)
}
