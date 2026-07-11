use crate::types::{Recipe, Step};

// TinyCC (mes fork, 0.9.26-1149-g46a75d0c) — bootstrap rung 3 (#378), guix's
// tcc-boot0. The build is the ENGINE's tcc_boot::run (re #469): the Rust port of
// the tcc tarball's configure/bootstrap.sh/boot.sh. MesCC (the mes rung output)
// compiles the first tcc, which self-hosts through six generations to the
// installed tcc + libc.a/libtcc1.a/crt objects at {out}. The rung declares NO
// host tools — its only inputs are the stage0 and mes recipe outputs (and the
// tcc source), the second rung with an empty BASE_TOOLS footprint.
pub fn recipe() -> Recipe {
    let steps = vec![
        Step::TccBoot {
            source: "{in:tcc-source}".into(),
            mes: "{in:mes}".into(),
            stage0: "{in:stage0}".into(),
        },
        Step::Require {
            paths: vec![
                "{out}/bin/tcc".into(),
                "{out}/lib/libc.a".into(),
                "{out}/lib/libtcc1.a".into(),
                "{out}/lib/crt1.o".into(),
            ],
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/bin/tcc".into()],
            exec: true,
        },
    ];
    Recipe::mesboot("tcc", "0.9.26-1149-g46a75d0c")
        .source_input("tcc-source")
        .native_inputs(&["stage0", "mes"])
        .steps(steps)
}
