use crate::ladder::{mesboot0_inputs, mesboot0_path, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, Step};

// Host-free build tools: mesboot0 + make-mesboot; flex/bison dead (binutils-244-source). re #469.
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let nbin = "{in:binutils-x86-64-native}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let path = format!("{nbin}:{}", mesboot0_path());
    let cip = format!("{xglibc}/include:{{root}}/kh");
    let mut steps = unpack_into("binutils-x86-64-self-source", "{src}");

    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("awk".into(), "{in:gawk-mesboot0}/bin/awk".into()),
            ("make".into(), "{in:make-mesboot}/bin/make".into()),
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
                "{in:make-mesboot}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                "MAKEINFO=true",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("C_INCLUDE_PATH", &cip),
    );
    // `make install` must carry the SAME C_INCLUDE_PATH as the build step: the
    // engine runs every Step with `env -i` (no inherited env), and binutils'
    // `install-ld` target recompiles the GENERATED ldemul.c when its timestamp
    // beats ldemul.o (nondeterministic under -j). Our wb/cc wrapper only injects
    // -B{glibc}/lib, not the header path, so an install-time recompile without
    // C_INCLUDE_PATH fails with `stdio.h: No such file or directory` and reds the
    // gcc-x86-64-self-test gate. (The native binutils-x86-64 wrapper bakes
    // -idirafter {glibc}/include in, so it never hit this.) re #469.
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "MAKEINFO=true",
                "install",
                "prefix={out}",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("C_INCLUDE_PATH", &cip),
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
            "make-mesboot",
        ])
        .inputs_owned(mesboot0_inputs(&["linux-headers-x86-64"]))
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    // The install step recompiles the generated ldemul.c when its timestamp beats
    // ldemul.o (nondeterministic under -j). Because the engine runs every Step with
    // `env -i` and the wb/cc wrapper doesn't inject the header path, `make install`
    // MUST carry C_INCLUDE_PATH exactly as the build step does — otherwise the
    // recompile fails `stdio.h: No such file or directory` and flaky-reds the
    // gcc-x86-64-self-test gate.
    #[test]
    fn make_install_carries_the_build_c_include_path() {
        let steps = recipe().steps.expect("binutils-x86-64-self steps");
        let make_steps: Vec<&Vec<(String, String)>> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Run { argv, env, .. }
                    if argv.first().map(String::as_str)
                        == Some("{in:make-mesboot}/bin/make") =>
                {
                    Some(env)
                }
                _ => None,
            })
            .collect();
        // The build `make` and the `make install`.
        assert_eq!(make_steps.len(), 2, "expected a build and an install make step");
        for env in make_steps {
            let cinc = env
                .iter()
                .find(|(k, _)| k == "C_INCLUDE_PATH")
                .map(|(_, v)| v.as_str());
            assert_eq!(
                cinc,
                Some("{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64/include:{root}/kh"),
                "every make step must see the glibc + kernel headers so an \
                 install-time ldemul.c recompile can find stdio.h"
            );
        }
    }
}
