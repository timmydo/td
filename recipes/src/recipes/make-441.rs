use crate::ladder::{SH, mesboot0_inputs, mesboot0_path, unpack_into};
use crate::types::{Recipe, Step};

// GNU Make 4.4.1 — the make glibc 2.41's configure REQUIRES (critical version
// gate `[4-9].*`, i.e. >= 4.0; re #469). Built at the gcc-14 tier, STATIC against
// the static glibc 2.16.0 (glibc-mesboot), like m4-mesboot/bison-mesboot and
// glibc-x86-64's BUILD_CC — a static i686 make runs in BOTH glibc build sandboxes
// (native glibc-241 and cross glibc-x86-64, whose build-time helpers run on i686)
// with no interp/RUNPATH story.
//
// Distinct from the two other makes on purpose: make-mesboot (3.82) is too old
// for glibc's gate, and make-x86-64 (also 4.4.1) is an ELF64 x86_64 tool linked
// against glibc-x86-64 — using it as the glibc build driver would be a wrong-arch
// binary AND a hard glibc<->make dependency cycle. The build DRIVER here is
// make-mesboot; the new make does not build itself. Reuses the shared Make 4.4.1
// source pin (make-x86-64-source). Host-free build tools: mesboot0 + make-mesboot;
// binutils-244 supplies as/ld/ar/ranlib.
pub fn recipe() -> Recipe {
    let path = format!("{{in:binutils-244}}/bin:{}", mesboot0_path());
    // build-plan exposes a recipe's source under its local `<name>-source`
    // alias even when sourceInput reuses a differently named shared pin.
    let mut steps = unpack_into("make-441-source", "{src}");
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
    // static gcc-14 vs the static glibc 2.16.0 (glibc-x86-64's BUILD_CC shape).
    steps.push(Step::WriteFile {
        path: "{root}/wb/gcc".into(),
        content: format!(
            "#!{SH}\nexec \"{{in:gcc-14}}/stage/td/store/gcc-14.3.0/bin/gcc\" -static -idirafter {{in:glibc-mesboot}}/include -B{{in:glibc-mesboot}}/lib \"$@\"\n"
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
                "--disable-dependency-tracking",
                "--disable-nls",
                // No guile input; build $(guile …) support out for a glibc-only
                // closure (same as make-x86-64).
                "--without-guile",
                // No dynamic `load' extension: it links dlopen/libdl, which a
                // static glibc binary cannot use at runtime without the matching
                // shared glibc (absent from the glibc-free sandbox), and glibc's
                // build never uses `load'. Keeps the static closure clean.
                "--disable-load",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/gcc"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
                "MAKEINFO=true",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot}/bin/make",
                "SHELL={in:bash-mesboot}/bin/bash",
                "MAKEINFO=true",
                "install",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH),
    );
    // Smoke-test the built make: it must run and report GNU Make 4.4.1, the
    // version glibc 2.41's critical gate needs (fail-closed in-build, like
    // busybox's readelf check). Capture make's OWN exit status (not just grep's,
    // which a crashed make + buffered output could mask) and match the version
    // literally (escaped dots, so `4.4.1' is not a wildcard).
    steps.push(
        Step::run(
            "{out}",
            &[
                SH,
                "-c",
                "v=$('{out}/bin/make' --version) \
                 || { echo 'make-441: make --version failed' >&2; exit 1; }; \
                 printf '%s\\n' \"$v\" | grep -q 'GNU Make 4\\.4\\.1' \
                 || { echo 'make-441: built make is not GNU Make 4.4.1' >&2; exit 1; }",
            ],
        )
        .env("PATH", &path),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/make".into()],
        exec: true,
    });
    // Static contract (engine-native, re #469): no PT_INTERP, DT_NEEDED, or run-
    // path, so the make runs in the glibc-free glibc sandboxes with no host loader.
    steps.push(Step::assert_static(&["{out}/bin/make"]));
    // Arch contract: the make must be i686 (ELF32 / Intel 80386), NOT the wrong-
    // arch x86_64 that would also run on this kernel and silently pass --version.
    // Mirrors make-x86-64's ELF64 assertion, with binutils-244's i686 readelf.
    steps.push(
        Step::run(
            "{out}",
            &[
                SH,
                "-c",
                "h=$('{in:binutils-244}/bin/readelf' -h '{out}/bin/make'); \
                 printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF32' || { echo 'make-441: make is not ELF32' >&2; exit 1; }; \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -q  '80386' || { echo 'make-441: make is not i686 (80386)' >&2; exit 1; }",
            ],
        )
        .env("PATH", &path),
    );
    Recipe::mesboot("make-441", "4.4.1")
        .source_input("make-x86-64-source")
        .native_inputs(&["gcc-14", "glibc-mesboot", "binutils-244", "make-mesboot"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
}
