use crate::ladder::{SH, base_inputs, base_path, unpack_into};
use crate::types::{Recipe, Step};

// GNU Mes 0.27.1 — bootstrap rung 2 (#378): MesCC self-hosts mes on the stage0
// mescc-tools (guix's mes-boot). Faithful port of the deleted build_mes_prefix
// shell fn: nyacc unpacked beside the tree, GUILE=true (no guile — mes drives
// itself), the M2/M1/hex2/blood-elf seed tools on PATH + M1/HEX2/BLOOD_ELF env,
// configure.sh --prefix={out}, bootstrap.sh, install.sh, then the mes modules +
// nyacc copied into the guile site dir (consumers put it on GUILE_LOAD_PATH).
pub fn recipe() -> Recipe {
    let path = base_path();
    let glp = "{root}/nyacc/module:{src}/mes/module:{src}/module";
    let common = |s: Step| -> Step {
        s.env("PATH", &path)
            .env("GUILE_LOAD_PATH", glp)
            .env("MES_PREFIX", "{src}")
            .env("MES_ARENA", "100000000")
            .env("MES_MAX_ARENA", "100000000")
            .env("MES_STACK", "8000000")
            .env("GUILE", "true")
            .env("MES_FOR_BUILD", "mes")
    };
    let mut steps = unpack_into("mes-source", "{src}");
    steps.extend(unpack_into("nyacc", "{root}/nyacc"));
    steps.push(Step::ToolFarm {
        links: vec![
            ("M2-Planet".into(), "{in:stage0}/AMD64/artifact/M2".into()),
            (
                "blood-elf".into(),
                "{in:stage0}/AMD64/artifact/blood-elf-0".into(),
            ),
            ("M1".into(), "{in:stage0}/AMD64/bin/M1".into()),
            ("hex2".into(), "{in:stage0}/AMD64/bin/hex2".into()),
            ("kaem".into(), "{in:stage0}/AMD64/bin/kaem".into()),
        ],
    });
    // CC set-but-EMPTY on configure ONLY (the deleted fn's exact env): configure.sh
    // falls through to mescc; the generated bootstrap.sh must NOT see an empty CC
    // (its ${CC-…} default would be overridden by the empty).
    steps.push(
        common(Step::run(
            "{src}",
            &[SH, "configure.sh", "--prefix={out}", "--host=i686-linux-gnu"],
        ))
        .env("CC", ""),
    );
    for script in ["bootstrap.sh", "install.sh"] {
        steps.push(
            common(Step::run("{src}", &[SH, script]))
                .env("M1", "{in:stage0}/AMD64/bin/M1")
                .env("HEX2", "{in:stage0}/AMD64/bin/hex2")
                .env("BLOOD_ELF", "{in:stage0}/AMD64/artifact/blood-elf-0"),
        );
    }
    // The mes modules + nyacc into the guile site dir (mes installs its site dir
    // under share/guile/site/<effective version> — 3.0 for the pinned 0.27.1).
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
        .inputs_owned(base_inputs(&["nyacc"]))
        .steps(steps)
}
