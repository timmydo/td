use crate::ladder::unpack_into;
use crate::types::{Recipe, Step, TextEdit};

// GNU patch 2.5.9 — bootstrap rung 5 (#378), host-tool-free (re #469). td's
// tcc-built make (make-mesboot0) builds patch (guix's patch-mesboot). The old
// recipe ran the tarball's autoconf ./configure under HOST bash + coreutils/
// sed/grep and drove make with host bash as $(SHELL); that is host-executable
// ingress the bootstrap no longer admits, so this rung drops ALL of it and
// follows the tcc/make/oyacc pattern instead:
//
//   * No ./configure: config.h is the EXACT file patch's configure emits for the
//     tcc + mes-libc target, captured once from a real configure run against
//     td's own tcc + mes (every host include-path env neutralized, so tcc saw
//     only mes + tcc headers as the sandbox does) and pinned as
//     patch-mesboot-config.h. That eliminates the host shell AND the host
//     coreutils/sed/grep configure invoked for feature detection.
//   * No host make shell: td's Make 3.80 drives the build with a baked,
//     metacharacter-free Makefile (patch-mesboot.mk), so make execs tcc via its
//     no-shell fast path. configure's compile rule embeds -Ded_PROGRAM=\"ed\"
//     (escaped quotes = a shell metacharacter); ed_PROGRAM moves into config.h
//     so no recipe line forces the (nonexistent) $(SHELL).
//   * No host sed: the one source edit (disabling pch.c's hunk-cleanup loop,
//     carried verbatim from the old recipe) is a count-checked SubstituteText.
//
// Inputs are mes (headers + libc) + tcc (compiler) + make-mesboot0 (`make`) —
// no host tools. Build proven green against td's own tcc/mes/make: patch links
// -static, reports 2.5.9, and applies a unified diff.
const CONFIG_H: &str = include_str!("patch-mesboot-config.h");
const MAKEFILE: &str = include_str!("patch-mesboot.mk");

// Smoke inputs: a two-line file and the unified diff that rewrites its second
// line. `patch orig diff` exits 0 only if the hunk applies, so the run proves
// the built i386 static ELF actually parses and applies a patch (not just
// --version) — the rung's whole purpose.
const SMOKE_TXT: &str = "hello\nworld\n";
const SMOKE_DIFF: &str = "--- smoke.txt\n+++ smoke.txt\n@@ -1,2 +1,2 @@\n hello\n-world\n+PATCHED\n";

pub fn recipe() -> Recipe {
    let mut steps = unpack_into("patch-mesboot-source", "{src}");

    // tcc's crt/libc/libtcc1 beside the sources, so the Makefile's `-L.` finds
    // them at link time (mirrors configure's Makefile; tcc locates its own
    // libtcc1 via its baked store path, so no -B is needed in the sandbox).
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

    // pch.c patch (carried from the old recipe): neutralize another_hunk()'s
    // leading p_line free loop. Set p_end = -1 directly (the invariant the
    // following `assert(p_end == -1)` checks) and turn the loop into `while (0)`
    // so its body compiles but never runs. Count-checked: the anchor is unique.
    steps.push(Step::substitute_text(
        "{src}/pch.c",
        vec![TextEdit::new(
            "    while (p_end >= 0) {",
            "    p_end = -1;\n    while (0) {",
            1,
        )],
    ));

    // Pinned configure output + baked Makefile (tcc/mes paths baked in).
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

    // Build `patch` with td's Make 3.80 driving tcc (no shell — fast path only).
    // LANG/LC_ALL neutralized for determinism; make finds tcc via the baked
    // absolute CC path, so no PATH is needed.
    steps.push(
        Step::run("{src}", &["{in:make-mesboot0}/bin/make", "patch"])
            .env("LANG", "")
            .env("LC_ALL", ""),
    );

    // Install patch.
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/patch".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/patch".into()],
        exec: true,
    });

    // Runtime provenance (re #469): the link is -static, so patch must carry no
    // host loader (PT_INTERP) or host libc (DT_NEEDED); red the rung otherwise.
    steps.push(Step::assert_static(&["{out}/bin/patch"]));

    // Smoke: the installed patch must RUN and APPLY a hunk (exit 0), through the
    // version banner and a real unified-diff apply.
    steps.push(Step::run("{src}", &["{out}/bin/patch", "--version"]));
    steps.push(Step::WriteFile {
        path: "{src}/smoke.txt".into(),
        content: SMOKE_TXT.into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{src}/smoke.diff".into(),
        content: SMOKE_DIFF.into(),
        exec: false,
    });
    steps.push(Step::run(
        "{src}",
        &["{out}/bin/patch", "smoke.txt", "smoke.diff"],
    ));

    Recipe::mesboot("patch-mesboot", "2.5.9")
        .source_input("patch-mesboot-source")
        .native_inputs(&["mes", "tcc", "make-mesboot0"])
        .steps(steps)
}
