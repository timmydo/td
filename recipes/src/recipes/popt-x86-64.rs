use crate::shared::{
    build_root_static_cc, unpack_into, unpack_keep_top, BUILD_ROOT_PATH, BUILD_ROOT_SH as SH,
};
use crate::types::{Recipe, Step};

// popt's static library for cryptsetup's command-line parser (td-install/
// ENCRYPTION.md increment 3). Only libpopt.a and popt.h leave the derivation.
//
// Built in the Debian build root against td glibc and gcc-runtime
// (`shared::build_root_static_cc`), under the shipped target profile
// (td-profiler/DESIGN.md §2), because its objects are linked into cryptsetup.
pub fn recipe() -> Recipe {
    let path = format!("{{root}}/wb:{BUILD_ROOT_PATH}");
    let cip = "{root}/kh";

    let mut steps = unpack_into("popt-x86-64-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.extend(build_root_static_cc());
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
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
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("AR", "/usr/bin/ar")
        .env("RANLIB", "/usr/bin/ranlib")
        .env("C_INCLUDE_PATH", cip)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}/src",
            &[
                "/usr/bin/make",
                "-j{jobs}",
                "libpopt.la",
                &format!("SHELL={SH}"),
                &format!("CONFIG_SHELL={SH}"),
            ],
        )
        .env("PATH", &path)
        .env("CC", "{root}/wb/cc")
        .env("AR", "/usr/bin/ar")
        .env("RANLIB", "/usr/bin/ranlib")
        .env("C_INCLUDE_PATH", cip)
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
        .build_root()
        .native_inputs(&["glibc-x86-64", "gcc-runtime"])
        .inputs(&["linux-headers-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    #[test]
    fn archives_are_compiled_in_the_build_root_under_the_target_profile() {
        crate::shared::assert_build_root_static_c(&super::recipe());
    }
}
