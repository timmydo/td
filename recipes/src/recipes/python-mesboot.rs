use crate::ladder::{SH, mesboot0_inputs, mesboot0_path, unpack_into};
use crate::types::{Recipe, Step};

// CPython 3.11.1 — the python3 glibc 2.41's configure requires (>= 3.4,
// critical; scripts/gen-as-const.py generates core headers, re #469). Built at
// the gcc-14 tier against the SHARED glibc 2.16.0 (glibc-mesboot-shared), with
// glibc-241's own CC wrapper: a fully-static CPython is the finicky path
// (dlopen/NSS), so this links dynamically against the shared mesboot glibc that
// glibc-241's build tools already run against (LD_LIBRARY_PATH). The freshly
// built interpreter runs during its own build (deepfreeze, install-compile), so
// LD_LIBRARY_PATH is set on every step that runs it.
//
// NATIVE build (build==host==i686): CPython's C tools _freeze_module and
// _bootstrap_python generate the frozen/deepfreeze sources during `make`, so no
// pre-existing python is needed and --with-build-python (the cross-only escape
// hatch that WOULD need a host python) is deliberately NOT passed. This is the
// documented self-hosting path (Makefile.pre.in: "users can build Python
// without an existing Python installation") and exactly what live-bootstrap
// does. Minimal: --disable-shared (single interpreter, no libpython.so),
// --without-ensurepip, --disable-test-modules; the optional extension modules
// (ssl/zlib/ffi/readline/sqlite/curses) are skipped for want of their libs —
// gen-as-const.py needs only always-built C modules (argparse/re/subprocess/
// tempfile/os/collections). Modules/getbuildinfo.c bakes __DATE__/__TIME__ into
// the interpreter's build string, so SOURCE_DATE_EPOCH=1 is set EXPLICITLY on
// the compile/install steps — recipe Run steps execute under a cleared env
// (build.rs run_mesboot -> run_cmd), so there is no ambient default; gcc-14
// honors the var and pins those macros, keeping python3 reproducible. m4/bison
// reference neither macro, so they need no such override. Host-free build
// tools: mesboot0 + make-mesboot; binutils-244 supplies as/ld/ar/ranlib.
pub fn recipe() -> Recipe {
    let path = format!("{{in:binutils-244}}/bin:{}", mesboot0_path());
    let lp = "{in:glibc-mesboot-shared}/lib";
    let mut steps = unpack_into("python-mesboot-source", "{src}");
    // Retarget every `#! /bin/sh` shebang to the declared shell — the host-free
    // sandbox has no /bin/sh for a directly-exec'd build helper to fall back on.
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    steps.push(Step::ToolFarm {
        links: vec![
            ("awk".into(), "{in:gawk-mesboot0}/bin/awk".into()),
            ("make".into(), "{in:make-mesboot}/bin/make".into()),
        ],
    });
    // glibc-241's CC wrapper: gcc-14 vs the SHARED glibc 2.16.0, dynamic linker
    // and lib search bound to glibc-mesboot-shared.
    steps.push(Step::WriteFile {
        path: "{root}/wb/gcc".into(),
        content: format!(
            "#!{SH}\nexec \"{{in:gcc-14}}/stage/td/store/gcc-14.3.0/bin/gcc\" -B{{in:glibc-mesboot-shared}}/lib -L{{in:glibc-mesboot-shared}}/lib -isystem {{in:glibc-mesboot-shared}}/include -static-libgcc -Wl,--dynamic-linker -Wl,{{in:glibc-mesboot-shared}}/lib/ld-linux.so.2 \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "./configure",
                "--build=i686-unknown-linux-gnu",
                "--host=i686-unknown-linux-gnu",
                "--prefix={out}",
                "--disable-shared",
                "--without-ensurepip",
                "--disable-test-modules",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/gcc")
        // Force the platform in a clean sandbox rather than trusting uname
        // probes (as live-bootstrap does).
        .env("MACHDEP", "linux")
        .env("ac_sys_system", "Linux")
        .env("LD_LIBRARY_PATH", lp),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                "PYTHONDONTWRITEBYTECODE=1",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("LD_LIBRARY_PATH", lp)
        // getbuildinfo.c compiles here; pin __DATE__/__TIME__ (gcc-14 honors it).
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "PYTHONDONTWRITEBYTECODE=1",
                "install",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("LD_LIBRARY_PATH", lp)
        // install may relink getbuildinfo.o; keep the epoch pinned here too.
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    // Smoke-test the minimal interpreter: it must import exactly the always-built
    // modules glibc 2.41's gen-as-const.py chain uses, or the build failed to
    // produce a usable python3 (fail-closed in-build, like busybox's readelf).
    steps.push(
        Step::run(
            "{out}",
            &[
                "{out}/bin/python3",
                "-c",
                "import argparse, re, subprocess, tempfile, os, collections, sys; print(sys.version)",
            ],
        )
        .env("PATH", &path)
        .env("LD_LIBRARY_PATH", lp),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/python3".into()],
        exec: true,
    });
    Recipe::mesboot("python-mesboot", "3.11.1")
        .source_input("python-mesboot-source")
        .native_inputs(&["gcc-14", "glibc-mesboot-shared", "binutils-244", "make-mesboot"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
}
