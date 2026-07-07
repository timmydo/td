use crate::ladder::{SH, base_inputs, base_path, sed_i, unpack_into};
use crate::types::{Recipe, Step};

// GNU Make 3.80 — bootstrap rung 4 (#378): tcc builds the first make (guix's
// make-mesboot0). Faithful port of the deleted build_make fn: crt/libs copied
// beside the sources (CC=tcc -static -L.), build.sh.in stubs, the lseek
// declaration commented out of make.h.
pub fn recipe() -> Recipe {
    let path = base_path();
    let cc = "CC=tcc -static -L. -I{in:mes}/include -I{in:mes}/include/x86";
    let cpp = "CPP=tcc -E -I{in:mes}/include -I{in:mes}/include/x86";
    let mut steps = unpack_into("make-mesboot0-source", "{src}");
    steps.push(Step::CopyFiles {
        files: vec![
            "{in:tcc}/lib/crt1.o".into(),
            "{in:tcc}/lib/crti.o".into(),
            "{in:tcc}/lib/crtn.o".into(),
            "{in:tcc}/lib/libc.a".into(),
            "{in:tcc}/lib/libtcc1.a".into(),
        ],
        dest: "{src}".into(),
    });
    steps.push(Step::ToolFarm {
        links: vec![("tcc".into(), "{in:tcc}/bin/tcc".into())],
    });
    steps.push(sed_i(
        "s/@LIBOBJS@/getloadavg.o/; s/@REMOTE@/stub/",
        &["build.sh.in"],
    ));
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./configure",
                cc,
                cpp,
                "LD=tcc",
                "--build=i686-unknown-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--disable-nls",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH),
    );
    steps.push(sed_i("s,^extern long int lseek.*,// &,", &["make.h"]));
    steps.push(
        Step::run("{src}", &[SH, "./build.sh"])
            .env("PATH", &path)
            .env("CONFIG_SHELL", SH),
    );
    steps.push(Step::CopyFiles {
        files: vec!["{src}/make".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/make".into()],
        exec: true,
    });
    Recipe::mesboot("make-mesboot0", "3.80")
        .source_input("make-mesboot0-source")
        .native_inputs(&["mes", "tcc"])
        .inputs_owned(base_inputs(&[]))
        .steps(steps)
}
