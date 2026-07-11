use crate::ladder::unpack_into;
use crate::types::{Recipe, Step};

// GNU Make 3.80 — bootstrap rung 4 (#378), the SECOND rung cut off host
// executables (re #469). tcc builds the first make (guix's make-mesboot0).
//
// The tarball's configure + build.sh are POSIX sh that kaem cannot run, so this
// rung does NOT run them; it runs a pinned kaem BUILD SCRIPT (build.kaem) under
// stage0's audited `kaem` seed, compiling each object with tcc and linking
// `make` — a faithful transcription of the build.sh that configure emits for
// the tcc + mes-libc target. config.h is the EXACT file that configure produces
// for that target, captured once and pinned (make-mesboot0-config.h), so no
// host shell regenerates it. The engine performs only generic orchestration,
// exactly as the tcc rung does: unpack, copy tcc's crt/libc beside the sources,
// write config.h + the kaem script + a smoke makefile, run kaem, copy `make`,
// assert it. Inputs are stage0 + mes + tcc only — NO base tools.
//
// The only executable the build invokes is tcc (a recipe output); it is a
// self-contained native compiler (no mescc/M1/hex2), so the step needs no mes
// env. tcc finds its crt/libc/libtcc1 via the store paths baked into it, so no
// -B is needed; crt/libc are still copied into the source tree to mirror
// build.sh's `-L.`. The lseek redeclaration the old recipe sed'd out of make.h
// does not arise with this config.h + mes 0.27.1 headers (build proven clean),
// so no make.h patch is applied — and stage0 ships no sed/replace anyway.
const CONFIG_H: &str = include_str!("make-mesboot0-config.h");
const BUILD_KAEM: &str = include_str!("make-mesboot0.kaem");
// A shell-free smoke makefile: `make -n` prints the recipe without running it,
// so no POSIX shell is needed to prove make parses a file and resolves a target.
const SMOKE_MK: &str = "smoke:\n\tprintf 'make-mesboot0 ok\\n'\n";

pub fn recipe() -> Recipe {
    let mut steps = unpack_into("make-mesboot0-source", "{src}");
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    // tcc's crt/libc beside the sources so build.kaem's `-L.` + the `.` entry in
    // tcc's baked CRTPREFIX/LIBPATHS resolve them (mirrors build.sh; the baked
    // {in:tcc}/lib paths resolve too, so this is belt-and-suspenders). libtcc1.a
    // is auto-linked from tcc's baked CONFIG_TCCDIR.
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
    steps.push(Step::WriteFile {
        path: "{src}/config.h".into(),
        content: CONFIG_H.into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{src}/build.kaem".into(),
        content: BUILD_KAEM.into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{src}/td-smoke.mk".into(),
        content: SMOKE_MK.into(),
        exec: false,
    });
    // Drive the build under stage0's kaem. --strict fails the rung on the first
    // non-zero command (fail-closed). tcc needs no mes env; PATH points only at
    // the stage0 seed bin and the locale is neutralized for determinism.
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:stage0}/AMD64/bin/kaem",
                "--verbose",
                "--strict",
                "--file",
                "build.kaem",
            ],
        )
        .env("PATH", "{in:stage0}/AMD64/bin")
        .env("LANG", "")
        .env("LC_ALL", ""),
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
        .native_inputs(&["stage0", "mes", "tcc"])
        .steps(steps)
}
