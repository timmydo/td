use crate::ladder::{SH, base_inputs, base_path, sed_i, unpack_into};
use crate::types::{Recipe, Step};

// GNU patch 2.5.9 — bootstrap rung 5 (#378): the tcc-built make builds patch
// (guix's patch-mesboot). Faithful port of the deleted build_patch fn (the
// pch.c backtracking loop is disabled — the mes-era toolchain can't build it).
pub fn recipe() -> Recipe {
    let path = base_path();
    let cc = "CC=tcc -static -L. -I{in:mes}/include -I{in:mes}/include/x86";
    let cpp = "CPP=tcc -E -I{in:mes}/include -I{in:mes}/include/x86";
    let mut steps = unpack_into("patch-mesboot-source", "{src}");
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
        links: vec![
            ("tcc".into(), "{in:tcc}/bin/tcc".into()),
            ("make".into(), "{in:make-mesboot0}/bin/make".into()),
        ],
    });
    steps.push(sed_i(
        "s/^    while (p_end >= 0) {/    p_end = -1;\\n    while (0) {/",
        &["pch.c"],
    ));
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./configure",
                cc,
                cpp,
                "AR=tcc -ar",
                "LD=tcc",
                "--build=i686-unknown-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--disable-nls",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot0}/bin/make",
                "SHELL={in:bash}/bin/bash",
                "CONFIG_SHELL={in:bash}/bin/bash",
                cc,
                "AR=tcc -ar",
            ],
        )
        .env("PATH", &path),
    );
    steps.push(Step::CopyFiles {
        files: vec!["{src}/patch".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/patch".into()],
        exec: true,
    });
    Recipe::mesboot("patch-mesboot", "2.5.9")
        .native_inputs(&["mes", "tcc", "make-mesboot0"])
        .inputs_owned(base_inputs(&[]))
        .steps(steps)
}
