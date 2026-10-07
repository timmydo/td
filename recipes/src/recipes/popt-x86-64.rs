use crate::ladder::{
    post_rust_inputs, post_rust_tool_farm, unpack_into, unpack_keep_top, POST_RUST_SH,
};
use crate::types::{Recipe, Step};

// popt's static library for cryptsetup's command-line parser (td-install/
// ENCRYPTION.md increment 3). Only libpopt.a and popt.h leave the derivation.
//
// Built by the self-hosted toolchain under the shipped target profile
// (td-profiler/DESIGN.md §2) because its objects are linked into cryptsetup.
pub fn recipe() -> Recipe {
    let sgcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let path = format!("{{root}}/wb:{{tools}}:{{in:make-x86-64-self}}/bin:{sbin}");
    let cip = format!("{xglibc}/include:{{root}}/kh");

    let mut steps = unpack_into("popt-x86-64-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(post_rust_tool_farm("{in:gawk-x86-64-self}/bin/gawk"));
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: POST_RUST_SH.into(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{POST_RUST_SH}\nexec \"{sgcc}\" -static -B\"{sbin}/\" -B{xglibc}/lib -L{xglibc}/lib \"$@\" \
             -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map={{root}}=/td-build-root \
             -ffile-prefix-map={{src}}=/td-build\n"
        ),
        exec: true,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                POST_RUST_SH,
                "./configure",
                "--build=x86_64-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--prefix=/td/store/popt-1.19-x86_64",
                "--disable-shared",
                "--enable-static",
                "--disable-nls",
                "--disable-rpath",
                "--without-libiconv-prefix",
                "--without-libintl-prefix",
                // Without iconv, popt's help output never reaches glibc's
                // gconv modules, which a static binary would dlopen. Both
                // probes define HAVE_ICONV, so both are answered.
                "ac_cv_func_iconv=no",
                "am_cv_func_iconv=no",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_RUST_SH)
        .env("SHELL", POST_RUST_SH)
        .env("CC", "{root}/wb/cc")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-self}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-self}/bin/ranlib")
        .env("C_INCLUDE_PATH", &cip)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}/src",
            &[
                "{in:make-x86-64-self}/bin/make",
                "-j{jobs}",
                "libpopt.la",
                &format!("SHELL={POST_RUST_SH}"),
                &format!("CONFIG_SHELL={POST_RUST_SH}"),
            ],
        )
        .env("PATH", &path)
        .env("CC", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-self}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-self}/bin/ranlib")
        .env("C_INCLUDE_PATH", &cip)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::MkDir {
        path: "{out}/lib".into(),
    });
    steps.push(Step::MkDir {
        path: "{out}/include".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/src/.libs/libpopt.a".into()],
        dest: "{out}/lib".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/src/popt.h".into()],
        dest: "{out}/include".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/lib/libpopt.a".into(), "{out}/include/popt.h".into()],
        exec: false,
    });

    Recipe::mesboot("popt-x86-64", "1.19")
        .source_input("popt-x86-64-source")
        .native_inputs(&post_rust_inputs(
            "gawk-x86-64-self",
            &[
                "gcc-x86-64-self",
                "binutils-x86-64-self",
                "glibc-x86-64",
                "make-x86-64-self",
            ],
        ))
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
