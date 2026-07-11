use crate::types::{Recipe, Step};

// GNU Mes 0.27.1 — bootstrap rung 2 (#378, guix's mes-boot): MesCC self-hosts
// mes on the stage0 mescc-tools. The build is the ENGINE's mes_boot::run (re
// #469) — the Rust port of configure.sh/bootstrap.sh/install.sh — so the rung
// declares NO host tools at all: stage0 (a recipe output) compiles and links,
// the just-built mes runs upstream's mescc.scm, and the engine does the file
// orchestration. First rung with an empty BASE_TOOLS footprint.
pub fn recipe() -> Recipe {
    let mut steps = vec![Step::MesBoot {
        source: "{in:mes-source}".into(),
        nyacc: "{in:nyacc}".into(),
        stage0: "{in:stage0}".into(),
    }];
    // The mes modules + nyacc into the guile 3.0 site dir consumers put on
    // GUILE_LOAD_PATH (install.sh's own site dir is 2.2 — GUILE=true's
    // effective-version default; the engine unpacked nyacc at {root}/nyacc).
    steps.push(Step::CopyTree {
        from: "{out}/share/mes/module".into(),
        dest: "{out}/share/guile/site/3.0".into(),
    });
    steps.push(Step::CopyTree {
        from: "{root}/nyacc/module".into(),
        dest: "{out}/share/guile/site/3.0".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/bin/mescc".into(),
            "{out}/lib/x86-mes/libc+tcc.a".into(),
        ],
        exec: false,
    });
    Recipe::mesboot("mes", "0.27.1")
        .source_input("mes-source")
        .native_inputs(&["stage0"])
        .inputs_owned(vec!["nyacc".to_string()])
        .steps(steps)
}
