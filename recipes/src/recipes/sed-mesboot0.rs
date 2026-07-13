use crate::ladder::unpack_into;
use crate::types::{Recipe, Step, TextEdit};

// GNU sed 4.0.9 — the tcc-era `sed` provider (re #469), a cycle-breaker one tier
// below the first BASE_TOOLS consumer. The GCC/binutils rungs from
// binutils-mesboot0 up still name the HOST guix `sed` (via base_inputs); that is
// host-executable ingress the bootstrap must close. This rung builds `sed` from
// source under tcc + mes libc — the tcc/make/oyacc/patch pattern — so those
// rungs can consume a td-built `sed` instead. It sits with oyacc and
// patch-mesboot on {mes, tcc, make-mesboot0}, below bash-mesboot, so nothing it
// depends on can depend on it.
//
// This is live-bootstrap's sed-4.0.9 (the exact tcc-era version it builds, NOT
// the heavier gcc-mesboot1-era sed-4.2.2 the separate `sed-mesboot` rung uses),
// host-tool-free the same way its siblings are:
//
//   * No ./configure: live-bootstrap builds sed-4.0.9 with an EMPTY config.h and
//     `make LIBC=mes`. td bakes that Makefile (sed-mesboot0.mk) with tcc/mes
//     paths, and a config.h holding only the three string-valued defines
//     live-bootstrap passes as -D (their escaped quotes can't cross td's
//     no-shell make; sed.h / lib include config.h under -DHAVE_CONFIG_H, which
//     the Makefile's CFLAGS set — see sed-mesboot0-config.h).
//   * No host make shell: td's Make 3.80 drives the build; every recipe line is
//     metacharacter-free, so make execs tcc via its no-shell fast path.
//   * No `cp`: the one generated file live-bootstrap's mk makes with a `cp`
//     rule — lib/regex.h, a copy of lib/regex_.h that lib/regex.c and sed.h
//     #include — is created here as an engine-native relative symlink.
//   * No source patches: live-bootstrap applies none to sed-4.0.9, and td uses
//     the same pinned tcc 0.9.26 + mes 0.27.1, so the sources compile as-is.
//
// Inputs are mes (headers + libc), tcc (compiler), and make-mesboot0 (`make`) —
// no host tools.
const CONFIG_H: &str = include_str!("sed-mesboot0-config.h");
const MAKEFILE: &str = include_str!("sed-mesboot0.mk");

// Smoke input: one line the transform rewrites. `sed -n 's/hello/world/w proof'`
// writes the substituted line to `proof` (sed's own `w` flag — no shell
// redirection). The follow-up SubstituteText then REQUIRES exactly one "world"
// in `proof`, so the rung reds unless the built i386 static ELF actually RAN and
// performed the substitution (a crash reds at the run step; a mis-substitution
// reds at the content check).
const SMOKE_TXT: &str = "hello\n";

pub fn recipe() -> Recipe {
    let mut steps = unpack_into("sed-mesboot0-source", "{src}");

    // lib/regex.h is live-bootstrap's `cp lib/regex_.h lib/regex.h` (its mk's
    // only generated file): lib/regex.c does `#include <regex.h>` and sed.h does
    // `#include "regex.h"`, both resolved via -Ilib. A relative symlink is the
    // host-`cp`-free equivalent (the sandbox has no coreutils).
    steps.push(Step::Symlink {
        target: "regex_.h".into(),
        link: "{src}/lib/regex.h".into(),
    });

    // config.h (the three string defines) + the baked Makefile (tcc/mes paths).
    steps.push(Step::WriteFile {
        path: "{src}/config.h".into(),
        content: CONFIG_H.into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{src}/Makefile".into(),
        content: MAKEFILE.into(),
        exec: false,
    });

    // Build `sed/sed` with td's Make 3.80 driving tcc (no shell — fast path
    // only). LANG/LC_ALL neutralized for determinism; make finds tcc via the
    // baked absolute CC path, so no PATH is needed.
    steps.push(
        Step::run("{src}", &["{in:make-mesboot0}/bin/make"])
            .env("LANG", "")
            .env("LC_ALL", ""),
    );

    // Install sed.
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/sed/sed".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/sed".into()],
        exec: true,
    });

    // Runtime provenance (re #469): the link is -static, so sed must carry no
    // host loader (PT_INTERP) or host libc (DT_NEEDED) — else it would drag a
    // host glibc in at run time. Red the rung here if that regresses.
    steps.push(Step::assert_static(&["{out}/bin/sed"]));

    // Smoke: --version proves the static mes-libc ELF runs; the substitution +
    // content check proves it parses a script and edits a stream (see SMOKE_TXT).
    steps.push(Step::run("{src}", &["{out}/bin/sed", "--version"]));
    steps.push(Step::WriteFile {
        path: "{src}/smoke.txt".into(),
        content: SMOKE_TXT.into(),
        exec: false,
    });
    steps.push(Step::run(
        "{src}",
        &["{out}/bin/sed", "-n", "s/hello/world/w proof", "smoke.txt"],
    ));
    steps.push(Step::substitute_text(
        "{src}/proof",
        vec![TextEdit::new("world", "world", 1)],
    ));

    Recipe::mesboot("sed-mesboot0", "4.0.9")
        .source_input("sed-mesboot0-source")
        .native_inputs(&["mes", "tcc", "make-mesboot0"])
        .steps(steps)
}
