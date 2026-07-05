use crate::ladder::{SH, base_inputs, base_path, sed_i, unpack_into};
use crate::types::{Recipe, Step};

// TinyCC (mes fork, 0.9.26-1149-g46a75d0c) — bootstrap rung 3 (#378): MesCC
// builds tcc (guix's tcc-boot0). Faithful port of the deleted build_tcc fn.
// prefix={out} bakes tcc's crt search at {out}/lib (the binutils rung's proven
// contract: crt via the baked prefix, libc via LIBRARY_PATH, headers via
// C_INCLUDE_PATH); the built tcc + libs install there.
pub fn recipe() -> Recipe {
    let path = base_path();
    let common = |s: Step| -> Step {
        s.env("PATH", &path)
            .env("MES_PREFIX", "{in:mes}")
            .env("GUILE_LOAD_PATH", "{in:mes}/share/guile/site/3.0")
            .env("host", "i686-linux-gnu")
            .env("ONE_SOURCE", "true")
            .env("prefix", "{out}")
    };
    let mut steps = unpack_into("tcc-source", "{src}");
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
            ("mescc".into(), "{in:mes}/bin/mescc".into()),
            ("mes".into(), "{in:mes}/bin/mes".into()),
        ],
    });
    steps.push(sed_i("s/volatile//", &["conftest.c"]));
    steps.push(common(Step::run(
        "{src}",
        &[
            SH,
            "./configure",
            "--cc=mescc",
            "--prefix={out}",
            "--elfinterp=/lib/mes-loader",
            "--crtprefix=.",
            "--tccdir=.",
        ],
    )));
    steps.push(
        common(Step::run("{src}", &[SH, "bootstrap.sh"]))
            .env("MES_ARENA", "20000000")
            .env("MES_MAX_ARENA", "20000000")
            .env("MES_STACK", "6000000"),
    );
    steps.push(Step::CopyFiles {
        files: vec!["{src}/tcc".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec![
            "{src}/libc.a".into(),
            "{src}/libtcc1.a".into(),
            "{src}/crt1.o".into(),
            "{src}/crti.o".into(),
            "{src}/crtn.o".into(),
        ],
        dest: "{out}/lib".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/tcc".into()],
        exec: true,
    });
    Recipe::mesboot("tcc", "0.9.26-1149-g46a75d0c")
        .native_inputs(&["stage0", "mes"])
        .inputs_owned(base_inputs(&[]))
        .steps(steps)
}
