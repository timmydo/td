use crate::ladder::unpack_into;
use crate::types::{Recipe, Step, TextEdit};

// OpenBSD yacc 6.6 (portable) — the from-source `yacc` the bash rung needs to
// regenerate its parser from parse.y (re #469). live-bootstrap builds this exact
// version under tcc + mes libc to provide `yacc`; td does the same, with two
// differences that keep it host-tool-free:
//
//   * No `patch`/`sed`: the two upstream patches (mes-libc gaps in main.c, tcc's
//     assigned-`extern` bug in defs.h) are applied as engine-native, count-checked
//     `SubstituteText` edits — each transcribed from the live-bootstrap hunk.
//   * No shell + no host `make` recipe shell: td's Make 3.80 drives the build, but
//     every recipe line (the baked Makefile's link line + make's built-in `%.o:%.c`
//     rule) is metacharacter-free, so make execs tcc via its no-shell fast path.
//
// The Makefile is live-bootstrap's oyacc mk/main.mk with td's tcc/mes store paths
// baked in and the host-tool `install` target dropped — the engine installs `yacc`
// and smoke-tests it. config.h is touched empty (portable.{c,h} include it; the
// build defines no HAVE_CONFIG_H). Inputs are mes (headers + getopt.h), tcc (the
// compiler + libgetopt.a), and make-mesboot0 (the `make` binary) — no host tools.
const MAKEFILE: &str = include_str!("oyacc.mk");

// A minimal grammar for the smoke: `-d` makes the built yacc emit both y.tab.c
// (the generated parser) and y.tab.h (the token header), proving the i386 static
// ELF actually RUNS and regenerates a parser — exactly the bash rung's use.
const SMOKE_Y: &str = "%token A\n%%\ns: A ;\n";

pub fn recipe() -> Recipe {
    let mut steps = unpack_into("oyacc-source", "{src}");

    // Patch 1 — tcc.patch (defs.h): in tcc an assigned-to `extern` does not work.
    steps.push(Step::substitute_text(
        "{src}/defs.h",
        vec![TextEdit::new(
            "extern char *__progname;",
            "char *__progname;",
            1,
        )],
    ));

    // Patch 2 — meslibc.patch (main.c): mes libc has no <paths.h>/`_PATH_TMP`, no
    // `sig_atomic_t`, and no `mkstemp`. Hardcode /tmp and swap the temp-file
    // creation to `mktemp`+`fopen` (with a `char *` in place of the fd).
    steps.push(Step::substitute_text(
        "{src}/main.c",
        vec![
            TextEdit::new("#include <paths.h>", "#include <getopt.h>", 1),
            TextEdit::new("volatile sig_atomic_t sigdie;", "volatile int sigdie;", 1),
            TextEdit::new("tmpdir = _PATH_TMP;", "tmpdir = \"/tmp\";", 1),
            TextEdit::new("int fd;", "char *fname;", 1),
            TextEdit::new(
                "fd = mkstemp(action_file_name);\n\tif (fd == -1 || (action_file = fdopen(fd, \"w\")) == NULL)",
                "fname = mktemp(action_file_name);\n\tif (!fname || (action_file = fopen(fname, \"w\")) == NULL)",
                1,
            ),
            TextEdit::new(
                "fd = mkstemp(text_file_name);\n\tif (fd == -1 || (text_file = fdopen(fd, \"w\")) == NULL)",
                "fname = mktemp(text_file_name);\n\tif (!fname || (text_file = fopen(fname, \"w\")) == NULL)",
                1,
            ),
            TextEdit::new(
                "fd = mkstemp(union_file_name);\n\t\tif (fd == -1 || (union_file = fdopen(fd, \"w\")) == NULL)",
                "fname = mktemp(union_file_name);\n\t\tif (!fname || (union_file = fopen(fname, \"w\")) == NULL)",
                1,
            ),
        ],
    ));

    // touch config.h (empty): portable.{c,h} `#include "config.h"`, but no
    // HAVE_CONFIG_H is defined so an empty file is all the build needs.
    steps.push(Step::WriteFile {
        path: "{src}/config.h".into(),
        content: String::new(),
        exec: false,
    });
    // The td-adapted Makefile (tcc/mes paths baked in; host install target dropped).
    steps.push(Step::WriteFile {
        path: "{src}/Makefile".into(),
        content: MAKEFILE.into(),
        exec: false,
    });

    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    // Build `yacc` with td's Make 3.80 driving tcc (no shell — fast path only).
    // LANG/LC_ALL neutralized for determinism; make finds tcc via the baked
    // absolute CC path, so no PATH is needed.
    steps.push(
        Step::run("{src}", &["{in:make-mesboot0}/bin/make"])
            .env("LANG", "")
            .env("LC_ALL", ""),
    );
    steps.push(Step::CopyFiles {
        files: vec!["{src}/yacc".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/yacc".into()],
        exec: true,
    });

    // Smoke: the installed yacc must RUN and regenerate a parser from a grammar
    // (writes its scratch under the sandbox's /tmp, then y.tab.{c,h} in cwd).
    steps.push(Step::WriteFile {
        path: "{src}/smoke.y".into(),
        content: SMOKE_Y.into(),
        exec: false,
    });
    steps.push(Step::run("{src}", &["{out}/bin/yacc", "-d", "smoke.y"]));
    steps.push(Step::Require {
        paths: vec!["{src}/y.tab.c".into(), "{src}/y.tab.h".into()],
        exec: false,
    });

    Recipe::mesboot("oyacc", "6.6")
        .source_input("oyacc-source")
        .native_inputs(&["mes", "tcc", "make-mesboot0"])
        .steps(steps)
}
