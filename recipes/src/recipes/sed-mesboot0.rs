use crate::ladder::unpack_into;
use crate::types::{Recipe, Step, TextEdit};

// GNU sed 4.0.9 — the tcc-era `sed` provider (re #469), a cycle-breaker one tier
// below the first host-tool consumer. The GCC/binutils rungs from
// binutils-mesboot0 up once named the HOST guix `sed`; that host-executable
// ingress is now closed (re #469). This rung builds `sed` from
// source under tcc + mes libc — the tcc/make/oyacc/patch pattern — so those
// rungs can consume a td-built `sed` instead. Its BUILD inputs are {mes, tcc,
// make-mesboot0} — the same set as its siblings oyacc and patch-mesboot — plus a
// test-only dependency on bash-mesboot (which supplies stdin for the regression
// smoke). None of those, transitively, depends on sed, so there is no cycle.
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
//   * Two td-originated source fixes for INDEPENDENT sed-4.0.9 + mes-0.27.1 bugs
//     that surface only because td pipes an autoconf `config.status' through this
//     tcc-era sed (live-bootstrap never does, so it ships no patch; re #469, the
//     binutils/gcc-core configure). Both stem from mes libc quirks; each
//     SubstituteText below carries its own rationale and the tail its regression:
//       (a) an open mes stdin has fp==NULL and is misread as EOF (execute.c, two
//           sites; truncates config.status's subs.awk);
//       (b) mes's fflush() does a gratuitous fsync() that is EINVAL on a
//           non-syncable fd, aborting sed mid-write (utils.c; truncates Makefile).
//
// Build inputs are mes (headers + libc), tcc (compiler), and make-mesboot0
// (`make`) — no host tools. bash-mesboot is a test-only input: it supplies the
// stdin PIPE / `file -' list for the regression smoke below (it depends on none
// of sed's consumers, so it adds no cycle), and never touches the build itself.
const CONFIG_H: &str = include_str!("sed-mesboot0-config.h");
const MAKEFILE: &str = include_str!("sed-mesboot0.mk");

// Smoke input: one line the transform rewrites. `sed -n 's/h.llo/world/w proof'`
// writes the substituted line to `proof` via sed's `w` flag (no shell); the `.`
// forces a real regex match and `w` appends a newline, so `proof` must be exactly
// "world\n". The follow-up SubstituteText requires that, so the rung reds unless
// the built i386 static ELF actually ran and produced it.
const SMOKE_TXT: &str = "hello\n";

pub fn recipe() -> Recipe {
    let mut steps = unpack_into("sed-mesboot0-source", "{src}");

    // Fix (a), site 1 of 2 (re #469). sed 4.0.9 uses `!input->fp' as its "no open
    // stream" test. On glibc a closed stream has NULL fp; but mes-0.27.1 defines
    // `stdin' as (FILE*)0, so an OPEN stdin (pipe or `<file') ALSO has fp==NULL and
    // is misread as EOF — `N'/`n'/`$' then false-EOF on stdin's first line (so
    // `printf 'a\nb\n' | sed N' emits only "a"), which is how config.status's
    // subs.awk comes out truncated. Test sed's OWN sentinel instead:
    // read_fn == read_always_fail (set only on a failed open) means "no valid open
    // stream" — == `!fp' on glibc, correct on mes. Do NOT revert to the fp test; a
    // named file has a real fp, so only the tail's stdin smokes red this.
    steps.push(Step::substitute_text(
        "{src}/sed/execute.c",
        vec![TextEdit::new(
            "  if (!input->fp)\n    return separate_files || last_file_with_data_p(input);",
            "  if (input->read_fn == read_always_fail) /* mes stdin is (FILE*)0, so an open stdin has fp==NULL; use sed's own no-stream sentinel */\n    return separate_files || last_file_with_data_p(input);",
            1,
        )],
    ));
    // Fix (a), site 2: last_file_with_data_p() peeks whether the next input (maybe
    // stdin `-') still has data to decide if `$' matches now. Same fp blind spot,
    // inverted sense — a valid stream is read_fn != read_always_fail. The tab
    // before `{' keeps the match unique to this site.
    steps.push(Step::substitute_text(
        "{src}/sed/execute.c",
        vec![TextEdit::new(
            "      if (input->fp)\n\t{",
            "      if (input->read_fn != read_always_fail) /* mes: an open stdin has fp==NULL; test sed's no-stream sentinel, not fp */\n\t{",
            1,
        )],
    ));

    // Fix (b) (re #469): the output-side mes bug. mes stdio is UNBUFFERED (fwrite
    // goes straight to write(2)), yet its fflush() still does a gratuitous fsync()
    // for any fd >= 3, and fsync() is EINVAL on a non-syncable fd (pipe, /dev/null).
    // sed's ck_fflush() panics + exit(4)s on any fflush()==EOF whose errno isn't
    // EBADF, so config.status piping gcc's Makefile through sed aborts mid-write
    // (surfacing as `No rule to make target all.indirect'). Widen the guard to also
    // ignore EINVAL, as it already ignores EBADF: safe because unbuffered mes wrote
    // every byte at fwrite's write(2) (a real write failure panics in ck_fwrite), so
    // the fsync carries no data signal — a real fsync EIO still panics. Do NOT
    // narrow this back. The `w /dev/null' smoke reds an unpatched sed.
    steps.push(Step::substitute_text(
        "{src}/lib/utils.c",
        vec![TextEdit::new(
            "  if (fflush(stream) == EOF && errno != EBADF)",
            "  if (fflush(stream) == EOF && errno != EBADF && errno != EINVAL) /* mes stdio is unbuffered; fflush() only does a gratuitous fsync() that is EINVAL on a non-syncable fd (pipe/char-dev) - nothing was pending, so treat EINVAL like the already-ignored EBADF */",
            1,
        )],
    ));

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
        &["{out}/bin/sed", "-n", "s/h.llo/world/w proof", "smoke.txt"],
    ));
    steps.push(Step::substitute_text(
        "{src}/proof",
        vec![TextEdit::new("world\n", "world\n", 1)],
    ));

    // Fix (a) regressions (re #469): the file-fed smoke above can't catch the mes
    // stdin==NULL bug (a named file has a real fp). Drive the exact idioms that
    // tripped config.status, with bash-mesboot supplying stdin; `test STR = STR'
    // full-string-compares sed's captured output, so an unpatched sed (which
    // false-EOFs on stdin's first line, yielding "a"/"1"/"2") reds. The `-c' `|' is
    // bash's job, not make's no-shell path; printf/test are bash builtins.
    //
    //   1. `N'-join over a stdin PIPE  → test_eof / `N' (the subs.awk failure mode)
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:bash-mesboot}/bin/bash",
                "-c",
                "exp=$(printf 'a-b\\nc-d'); out=$(printf 'a\\nb\\nc\\nd\\n' | {out}/bin/sed 'N;s/\\n/-/'); test \"$out\" = \"$exp\"",
            ],
        )
        .env("LANG", "")
        .env("LC_ALL", ""),
    );
    //   2. `$=' last-line count over a stdin PIPE → test_eof / `$'
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:bash-mesboot}/bin/bash",
                "-c",
                "out=$(printf 'a\\nb\\nc\\nd\\n' | {out}/bin/sed -n '$='); test \"$out\" = 4",
            ],
        )
        .env("LANG", "")
        .env("LC_ALL", ""),
    );
    //   3. `$p' across a `file -' list — stdin is the LAST input, so the correct
    //      last line is stdin's "x" (a buggy last_file_with_data_p prints f1.txt's
    //      "2" and never reads stdin). f1.txt is the earlier, non-stdin input.
    steps.push(Step::WriteFile {
        path: "{src}/f1.txt".into(),
        content: "1\n2\n".into(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:bash-mesboot}/bin/bash",
                "-c",
                "out=$(printf 'x\\n' | {out}/bin/sed -n '$p' f1.txt -); test \"$out\" = x",
            ],
        )
        .env("LANG", "")
        .env("LC_ALL", ""),
    );

    // Fix (b) regression (re #469). `w /dev/null' writes each line to the char
    // device /dev/null, whose fsync() is EINVAL; an unpatched sed panics + exit(4)s
    // on the first line and this bare Step::run reds (its exit code IS sed's). A
    // pipe hits the identical fsync-EINVAL path, so /dev/null loses no coverage —
    // and a pipe test comparing CAPTURED output would be a false green: unbuffered
    // mes delivers the byte before the flush panics, so it arrives either way and
    // sed's exit(4) is swallowed inside `$(...)'. Assert on exit status, as here.
    steps.push(
        Step::run("{src}", &["{out}/bin/sed", "-n", "w /dev/null", "smoke.txt"])
            .env("LANG", "")
            .env("LC_ALL", ""),
    );

    Recipe::mesboot("sed-mesboot0", "4.0.9")
        .source_input("sed-mesboot0-source")
        .native_inputs(&["mes", "tcc", "make-mesboot0", "bash-mesboot"])
        .steps(steps)
}
