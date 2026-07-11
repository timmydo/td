use crate::ladder::unpack_into;
use crate::types::{Recipe, Step};

// TinyCC (mes fork, 0.9.26-1149-g46a75d0c) — bootstrap rung 3 (#378), the FIRST
// rung cut off host executables (re #469).
//
// The tarball's configure + bootstrap.sh + boot.sh are POSIX sh (case/$()/test/
// command -v) that kaem cannot run, so this rung does NOT run them; it runs a
// pinned kaem BUILD SCRIPT (tcc.kaem) under stage0's audited `kaem` seed. That
// keeps the package's build PROCEDURE in a script the seed executes — not
// transcribed into td-builder (the backed-out engine port grew the host TCB;
// see #469). The engine performs only generic orchestration, exactly as the mes
// rung's Step::MesBoot does: unpack the source, write config.h + the kaem
// script, pre-create the output dirs, run kaem, copy the built `tcc`, assert it.
//
// The only executables the build invokes are recipe outputs: the mes rung's
// `mes` running upstream mescc.scm (compiles the first tcc, `tcc-mes`), stage0's
// M1/hex2/blood-elf (mescc's assembler/linker, via the step env), and the tcc
// binaries this builds (native static ELF). No host shell or coreutils: the
// rung declares NO base tools — inputs are stage0 + mes only.
//
// config.h carries only the CONFIG_TCC_* string search paths, because kaem
// strips the embedded C string-literal quotes a WriteFile preserves. The
// quote-free bootstrap flags bootstrap.sh/boot.sh pass as -D (ONE_SOURCE,
// CONFIG_TCCBOOT, ...) go on the kaem command line per tcc.c compile instead:
// tcc.c tests `#ifdef ONE_SOURCE` before it includes config.h, so a config.h
// define would land too late. tcc's search is baked at {out}/lib (crt/libc) and
// {out}/lib/tcc (libtcc1); the kaem script writes the rebuilt libs straight
// there, so no cp/mkdir applet is needed.
const CONFIG_H: &str = include_str!("tcc-config.h");
const BUILD_KAEM: &str = include_str!("tcc.kaem");

pub fn recipe() -> Recipe {
    let mut steps = unpack_into("tcc-source", "{src}");
    // The output layout tcc's baked CONFIG_TCC_* search expects; the kaem build
    // installs crt/libc/libtcc1 into these during the boot generations.
    for d in ["{out}/bin", "{out}/lib", "{out}/lib/tcc"] {
        steps.push(Step::MkDir { path: d.into() });
    }
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
    // Drive the build under stage0's kaem. --strict fails the rung on the first
    // non-zero command (fail-closed). MES_*/M1/HEX2/BLOOD_ELF are the env mes's
    // mescc.scm reads; PATH points only at the stage0 seed bin.
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
        .env("MES_ARENA", "20000000")
        .env("MES_MAX_ARENA", "20000000")
        .env("MES_STACK", "6000000")
        .env("MES_PREFIX", "{in:mes}")
        .env("GUILE_LOAD_PATH", "{in:mes}/share/guile/site/3.0")
        .env("M1", "{in:stage0}/AMD64/bin/M1")
        .env("HEX2", "{in:stage0}/AMD64/bin/hex2")
        .env("BLOOD_ELF", "{in:stage0}/AMD64/artifact/blood-elf-0")
        .env("PATH", "{in:stage0}/AMD64/bin")
        .env("LANG", "")
        .env("LC_ALL", ""),
    );
    // The build leaves the final `tcc` in the tree; crt/libc/libtcc1 are already
    // installed under {out}/lib by the kaem script.
    steps.push(Step::CopyFiles {
        files: vec!["{src}/tcc".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/tcc".into()],
        exec: true,
    });
    Recipe::mesboot("tcc", "0.9.26-1149-g46a75d0c")
        .source_input("tcc-source")
        .native_inputs(&["stage0", "mes"])
        .steps(steps)
}
