use crate::ladder::unpack_into;
use crate::types::{Recipe, Step, TextEdit};

// GNU sed 4.0.9 — the tcc-era `sed` provider (re #469), a cycle-breaker one tier
// below the first BASE_TOOLS consumer. The GCC/binutils rungs from
// binutils-mesboot0 up still name the HOST guix `sed` (via base_inputs); that is
// host-executable ingress the bootstrap must close. This rung builds `sed` from
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
//   * td-originated source fixes for two INDEPENDENT sed-4.0.9 + mes-0.27.1 bugs
//     that only surface because td routes an autoconf `config.status' through
//     this tcc-era sed; live-bootstrap never does, so it ships no patch (re #469:
//     the binutils/gcc-core rungs' configure). Three SubstituteTexts below, two
//     for bug (a) and one for bug (b):
//
//     (a) INPUT side (two sites). mes defines `stdin' as (FILE*)0 == NULL, so an
//     OPEN stdin (a pipe or a `<file' redirect) has input->fp == NULL —
//     indistinguishable, by an `input->fp' test alone, from "no open stream".
//     sed's test_eof() and last_file_with_data_p() both make exactly that test,
//     so an open stdin is misread as EOF and the `N'/`n'/`$' commands fire a
//     false EOF on stdin's FIRST line. That silently truncates config.status's
//     subs.awk (assembled by a sed pipeline whose stages read stdin and join
//     lines with `N'). Two SubstituteTexts switch both guards to sed's own
//     no-stream sentinel (read_fn == read_always_fail).
//
//     (b) OUTPUT side (one site). mes stdio is UNBUFFERED (fwrite writes straight
//     through write(2)), yet its fflush() does a gratuitous fsync() for any
//     fd >= 3, and fsync() is EINVAL on a non-syncable fd (a pipe, or a char
//     device like /dev/null). sed's ck_fflush() panics + exit(4)s on any
//     fflush()==EOF whose errno isn't EBADF, so when config.status pipes gcc's
//     Makefile through sed that spurious fsync EINVAL aborts sed mid-write and
//     truncates the Makefile (surfacing as `No rule to make target all.indirect').
//     A third SubstituteText widens ck_fflush()'s benign-errno guard to also
//     ignore EINVAL — safe because unbuffered mes already wrote every byte at the
//     write(2) in fwrite, so the fsync carries no data-integrity signal.
//
//     See the steps for the per-site rationale, and the smoke tests at the tail
//     for the regressions: stdin pipes / `file -' lists catch (a); a `w /dev/null'
//     write catches (b) — /dev/null is a non-syncable char device that returns the
//     same fsync EINVAL a pipe does, so it exercises the identical ck_fflush path
//     config.status's Makefile-writing sed hits, and (unlike a pipe capture) its
//     bare `Step::run' asserts on sed's exit status, which is what actually reds.
//
// Build inputs are mes (headers + libc), tcc (compiler), and make-mesboot0
// (`make`) — no host tools. bash-mesboot is a test-only input: it supplies the
// stdin PIPE / `file -' list for the regression smoke below (it depends on none
// of sed's consumers, so it adds no cycle), and never touches the build itself.
const CONFIG_H: &str = include_str!("sed-mesboot0-config.h");
const MAKEFILE: &str = include_str!("sed-mesboot0.mk");

// Smoke input: one line the transform rewrites. `sed -n 's/h.llo/world/w proof'`
// writes the substituted line to `proof` (sed's own `w` flag — no shell
// redirection). The `.` makes it a real regex match (exercising the bundled
// regex engine, not a literal compare), and `w` writes the pattern space plus a
// trailing newline, so `proof` must be exactly "world\n". The follow-up
// SubstituteText REQUIRES exactly one "world\n" there, so the rung reds unless
// the built i386 static ELF actually RAN and produced that exact line: a crash
// reds at the run step; a mis-substitution or a mangled/absent newline reds at
// the content check.
const SMOKE_TXT: &str = "hello\n";

pub fn recipe() -> Recipe {
    let mut steps = unpack_into("sed-mesboot0-source", "{src}");

    // td-originated source fix (re #469), at the two sites that share one bug.
    // sed 4.0.9 uses an `input->fp' test as its "no open stream" sentinel:
    // test_eof() checks `!input->fp' and last_file_with_data_p() checks
    // `input->fp'. On glibc that is correct — a closed/absent stream has a NULL fp.
    // But mes-0.27.1 defines `stdin' as (FILE*)0 == NULL, so an OPEN stdin — a pipe
    // OR a `<file' redirect — ALSO has input->fp == NULL and is misread as
    // end-of-file. That makes `N', `n', and `$' fire a false EOF on the FIRST line
    // whenever sed reads stdin, so e.g. `printf 'a\nb\n' | sed N' emits only "a".
    // autoconf-2.64 config.status assembles subs.awk by feeding conf.subs through a
    // sed pipeline whose stages read stdin (a `<file' redirect into a pipe) and join
    // lines with `N', so on mes those stages false-EOF on their first line and
    // binutils/gcc-core config.status silently produces a truncated, unparsable
    // subs.awk.
    //
    // Fix both sites by testing sed's OWN no-stream sentinel instead of fp:
    // read_fn == read_always_fail. open_next_file() sets read_fn = read_always_fail
    // ONLY on a failed open (its "a redundancy" line), and closedown() sets it up
    // front (execute.c:555) — BEFORE its own `!input->fp' early-out, which on an
    // open mes stdin skips the later fp=NULL, so read_fn is the MORE reliable
    // sentinel; every successful open — a named file OR stdin — sets
    // read_fn = read_file_line. So `read_fn == read_always_fail' means exactly "no
    // valid open stream": equivalent to `!fp' on glibc and correct on mes.
    // read_always_fail is a static fn declared above both call sites. A named FILE
    // argument was never affected (fp is a real fd, non-NULL) — which is why the
    // file-fed smoke never caught this; the stdin smoke tests at the tail drive the
    // exact `N'/`$' idioms over a pipe and a `file -' list, the paths that red an
    // unpatched sed.
    steps.push(Step::substitute_text(
        "{src}/sed/execute.c",
        vec![TextEdit::new(
            "  if (!input->fp)\n    return separate_files || last_file_with_data_p(input);",
            "  if (input->read_fn == read_always_fail) /* mes stdin is (FILE*)0, so an open stdin has fp==NULL; use sed's own no-stream sentinel */\n    return separate_files || last_file_with_data_p(input);",
            1,
        )],
    ));
    // The companion site: last_file_with_data_p() peeks whether any REMAINING input
    // (the next file, which may be stdin `-') still has data, to decide if `$'
    // matches now. Its `if (input->fp)' has the same mes blind spot — an open stdin
    // (fp==NULL) is skipped as "no stream", so `$' matches the PREVIOUS file's last
    // line and stdin is never read. Same sentinel, inverted sense (a valid stream
    // is read_fn != read_always_fail). The tab before `{' matches the source's own
    // indentation, keeping the match unique to this site.
    steps.push(Step::substitute_text(
        "{src}/sed/execute.c",
        vec![TextEdit::new(
            "      if (input->fp)\n\t{",
            "      if (input->read_fn != read_always_fail) /* mes: an open stdin has fp==NULL; test sed's no-stream sentinel, not fp */\n\t{",
            1,
        )],
    ));

    // OUTPUT-side fix (re #469), a third site sharing the mes root cause but a
    // different mechanism than the two above. mes stdio is UNBUFFERED: fwrite()
    // writes straight through the write(2) syscall, so a FILE* holds no pending
    // bytes and fflush() has nothing to flush. mes's fflush() nonetheless does a
    // gratuitous fsync() for any fd >= 3 (fd < 3 — stdin/out/err — it no-ops),
    // and fsync() returns -1/EINVAL on a NON-syncable fd: a pipe, a socket, or a
    // char device like /dev/null. sed routes output through ck_fflush() — per
    // line for a `w'-command / in-place target (output_line: fp != stdout), and
    // for every tracked file at closedown (ck_fclose(NULL)). ck_fflush() panics
    // ("Couldn't flush %s: %s") and exit(4)s on any fflush()==EOF whose errno is
    // not EBADF. So the moment gcc's config.status pipes its Makefile through
    // this sed, that spurious fsync EINVAL aborts sed mid-write, truncating the
    // generated gcc/Makefile — which surfaces two steps later as `make: *** No
    // rule to make target all.indirect'. (The literal `%s' in the message is a
    // second tell: td's minimal config.h defines neither HAVE_VFPRINTF nor
    // HAVE_DOPRNT, so panic() fputs() the raw format — confirming the abort is
    // this ck_fflush(), not a formatting bug.) Widen the benign-errno guard to
    // also ignore EINVAL, exactly as it already ignores EBADF. This loses no
    // error detection: under unbuffered mes every byte reached the kernel at the
    // write(2) in fwrite (a genuine write failure already panics there, in
    // ck_fwrite), so fflush()'s fsync carries no data-integrity signal — EINVAL
    // means only "this fd cannot be fsync()ed", never "data was lost". A real
    // fsync EIO would still panic. errno.h (EINVAL == 22 in mes) is already
    // included here. The `w /dev/null' smoke at the tail reds an unpatched sed.
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

    // stdin regression (re #469): the file-fed smoke above never exercises the mes
    // stdin==NULL bug — a named FILE has a real fp. Drive the exact idioms that
    // tripped config.status, using the td-built bash-mesboot only to supply stdin.
    // Each sub-test compares sed's output to the expected with the `test' builtin:
    // a mismatch exits non-zero and reds the rung. Command substitution `$(…)'
    // captures the WHOLE output (bar trailing newlines) and `test STR = STR' is a
    // full-string compare, so leading/embedded garbage cannot slip past (a plain
    // substring check would let e.g. "14" satisfy "4"). An UNPATCHED sed — which
    // false-EOFs on stdin's first line — yields "a", "1", "2" respectively and
    // reds. The bash `-c' lines use `|' (a metacharacter) — that is bash's job
    // here, not make's no-shell path. printf and test are bash-mesboot builtins.
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

    // OUTPUT-side regression (re #469): the fflush/fsync panic the ck_fflush
    // patch above fixes. sed's `w' command ck_fopen()s its target and, because
    // the target is not stdout, ck_fflush()es it PER LINE (output_line). A
    // regular-file target — the `w proof' smoke above — fsync()s fine and so
    // never exercised this; a NON-syncable target does. `w /dev/null' writes each
    // line of smoke.txt to the char device /dev/null (rbind'd into the sandbox),
    // whose fsync() returns EINVAL. An UNPATCHED sed turns that into a "Couldn't
    // flush" panic + exit(4) on the FIRST line; the run step then reds. A patched
    // sed ignores the benign EINVAL and exits 0. No stdin/bash needed — a bare
    // run whose sole failure mode is the panic, so the run step's exit code IS
    // sed's (that exit-code assertion is what actually reds an unpatched sed).
    // This is the same fsync-EINVAL code path config.status's Makefile-writing sed
    // PIPE hits (a pipe is non-syncable exactly as /dev/null is), so a separate
    // pipe smoke would exercise no new code path; and because unbuffered mes
    // delivers each `w' byte to its target at the write(2) in ck_fwrite BEFORE the
    // flush's fsync panics, a pipe smoke that compared CAPTURED OUTPUT would be a
    // false green (the bytes arrive whether or not sed then panics, and the panic's
    // exit(4) is swallowed inside a `$(... | ...)' capture). /dev/null with the
    // exit-code assertion is therefore the canonical regression for bug (b).
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
