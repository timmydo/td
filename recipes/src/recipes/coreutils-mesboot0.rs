use crate::ladder::unpack_into;
use crate::types::{Recipe, Step, TextEdit};

// GNU coreutils 5.0 — the tcc-era coreutils provider (re #469), a cycle-breaker
// below the first BASE_TOOLS consumer. The GCC/binutils rungs from
// binutils-mesboot0 up still name the HOST guix `coreutils` (via base_inputs);
// that is host-executable ingress the bootstrap must close. This rung builds the
// coreutils userland from source under tcc + mes libc — the same tcc/make/patch
// pattern as sed-mesboot0/bash-mesboot — so those rungs can consume td-built
// coreutils instead. It sits with sed-mesboot0/bash-mesboot below
// binutils-mesboot0, so nothing it depends on can depend on it.
//
// This is live-bootstrap's coreutils-5.0 pass1 (steps/coreutils-5.0,
// pass1.kaem + mk/main.mk), host-tool-free the same way its siblings are:
//
//   * No ./configure: live-bootstrap builds coreutils-5.0 with an EMPTY config.h
//     and `make` with ~50 -D on the tcc command line. td bakes that Makefile
//     (coreutils-mesboot0.mk) with tcc/mes paths, moving only the ten
//     metacharacter-bearing defines into config.h under -DHAVE_CONFIG_H (the
//     other ~40 stay global on the command line — see coreutils-mesboot0-config.h
//     and the .mk header).
//   * No host make shell: td's Make 3.80 drives the build; every recipe line is
//     metacharacter-free, so make execs tcc via its no-shell fast path.
//   * No host cp: the three generated headers live-bootstrap makes with `cp`
//     (lib/fnmatch.h, lib/ftw.h, lib/search.h — copies of the *_.h templates the
//     sources #include) are engine-native relative symlinks. The one file
//     live-bootstrap regenerates with `sed` (src/false.c from src/true.c) is
//     already shipped in the tarball BYTE-IDENTICAL to that regeneration
//     (verified), so it simply builds from the shipped source.
//   * The nine mes-libc/tcc source patches live-bootstrap's pass1.kaem applies
//     (patch -Np1) are shipped verbatim (SPDX headers trimmed to keep the wire
//     ASCII) and applied by td's own `patch` rung (patch-mesboot), in the same
//     order — the established td patch mechanism (gcc/binutils/glibc rungs).
//     live-bootstrap also `rm`s src/dircolors.h defensively; `dircolors` is not
//     in this build subset and nothing we compile #includes it (only the unbuilt
//     src/dircolors.c does), so that removal is a no-op we elide.
//
// Inputs are mes (headers + libc), tcc (compiler), make-mesboot0 (`make`), and
// patch-mesboot (`patch`) — no host tools. Builds live-bootstrap's 61-binary
// subset and installs them with the just-built `install` (`make install`).
const CONFIG_H: &str = include_str!("coreutils-mesboot0-config.h");
const MAKEFILE: &str = include_str!("coreutils-mesboot0.mk");

// The nine patches, in live-bootstrap pass1.kaem order (load-bearing:
// touch-getdate must precede touch-dereference — both edit src/touch.c). Each is
// the upstream hunk verbatim with only the non-ASCII SPDX header trimmed (`patch`
// ignores text before the first `---`/`diff` line anyway).
const PATCHES: &[(&str, &str)] = &[
    // lib/modechange.c: move the modechange.h include after <sys/stat.h>.
    ("modechange", include_str!("coreutils-mesboot0-modechange.patch")),
    // lib/quotearg.c + NEW lib/mbstate_t.h: mes libc has no mbstate_t; supply the
    // glibc-2.32 struct (the patch creates the header).
    ("mbstate", include_str!("coreutils-mesboot0-mbstate.patch")),
    // src/ls.c: strcoll -> strcmp (mes libc has no strcoll).
    ("ls-strcmp", include_str!("coreutils-mesboot0-ls-strcmp.patch")),
    // src/touch.c: no bison-generated get_date() yet — stub the -d parse to 0.
    ("touch-getdate", include_str!("coreutils-mesboot0-touch-getdate.patch")),
    // src/touch.c: add -h/--no-dereference (applied AFTER touch-getdate).
    ("touch-dereference", include_str!("coreutils-mesboot0-touch-dereference.patch")),
    // lib/tempname.c: uint64_t -> unsigned long long (tcc 0.9.26 lacks uint64_t).
    ("tac-uint64", include_str!("coreutils-mesboot0-tac-uint64.patch")),
    // src/expr.c: strcoll -> strcmp.
    ("expr-strcmp", include_str!("coreutils-mesboot0-expr-strcmp.patch")),
    // lib/memcoll.c strcoll -> strcmp + src/sort.c: hoist hard_LC_COLLATE decl
    // out of the compiled-out HAVE_SETLOCALE block.
    ("sort-locale", include_str!("coreutils-mesboot0-sort-locale.patch")),
    // src/uniq.c: fopen_safer (don't let fopen return stdin/stdout).
    ("uniq-fopen", include_str!("coreutils-mesboot0-uniq-fopen.patch")),
];

// The three lib/*_.h templates live-bootstrap copies to their include names
// (the sources #include <fnmatch.h>/<ftw.h>/<search.h>, resolved via -Ilib). A
// relative symlink is the host-`cp`-free equivalent (the sandbox has no
// coreutils — that is what this rung builds).
const COPIED_HEADERS: &[(&str, &str)] = &[
    ("fnmatch_.h", "fnmatch.h"),
    ("ftw_.h", "ftw.h"),
    ("search_.h", "search.h"),
];

// Every binary this rung installs (the .mk COREUTILS list + the seven multi-obj
// binaries) — all 61 are asserted to exist, be executable, and be fully static
// (re #469: a host loader/DT_NEEDED on any one would drag in a host libc at run
// time). Asserting the whole set, not a slice, is the strong provenance claim.
const ALL_BINS: &[&str] = &[
    // The 54 single-obj COREUTILS (built by the static pattern rule), in .mk order.
    "basename", "cat", "chmod", "cksum", "csplit", "cut", "dirname", "echo",
    "expand", "expr", "factor", "false", "fmt", "fold", "head", "hostname", "id",
    "join", "kill", "link", "ln", "logname", "mkfifo", "mkdir", "mknod", "nl",
    "od", "paste", "pathchk", "pr", "printf", "ptx", "pwd", "readlink", "rmdir",
    "seq", "sleep", "sort", "split", "sum", "tail", "tee", "tr", "tsort",
    "unexpand", "uniq", "unlink", "wc", "whoami", "tac", "test", "touch", "true",
    "yes",
    // The seven multi-obj binaries (their own explicit .mk rules).
    "cp", "ls", "install", "md5sum", "mv", "rm", "sha1sum",
];

// Smoke input: three unsorted lines the transform reorders. `sort -o proof`
// writes the sorted stream to `proof` (sort's own `-o`, no shell redirection).
const SMOKE_TXT: &str = "3\n1\n2\n";

// Known-good digests of SMOKE_TXT, verified against upstream coreutils. `md5sum
// -c` / `sha1sum -c` recompute over smoke.txt and exit non-zero on any mismatch
// — a functional check of the shared md5sum.o driver in both modes, with no
// shell (the checkfile format is `<digest>  <file>`, two spaces).
const SMOKE_MD5: &str = "6aedf78b5040f95db16c33d40e836165  smoke.txt\n";
const SMOKE_SHA1: &str = "b910a63eeea5ebafbf5564fc004ead6a7518f098  smoke.txt\n";

pub fn recipe() -> Recipe {
    let mut steps = unpack_into("coreutils-mesboot0-source", "{src}");

    // The three copied headers (live-bootstrap's `cp lib/X_.h lib/X.h`).
    for (template, name) in COPIED_HEADERS {
        steps.push(Step::Symlink {
            target: (*template).into(),
            link: format!("{{src}}/lib/{name}"),
        });
    }

    // config.h (the ten metacharacter-bearing defines) + the baked Makefile
    // (tcc/mes paths, the other ~40 defines, no-shell recipe lines).
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

    // Apply the nine patches with td's own `patch` (patch-mesboot), in order.
    // Each patch rides in as a build-root file (keeping the source tree clean);
    // `patch --force -p1` strips the leading `coreutils-5.0/` path component
    // exactly as live-bootstrap's `patch -Np1` does from the package dir.
    steps.push(Step::MkDir {
        path: "{root}/patches".into(),
    });
    for (name, content) in PATCHES {
        let path = format!("{{root}}/patches/{name}.patch");
        steps.push(Step::WriteFile {
            path: path.clone(),
            content: (*content).into(),
            exec: false,
        });
        steps.push(Step::run(
            "{src}",
            &[
                "{in:patch-mesboot}/bin/patch",
                "--force",
                "-p1",
                "-i",
                path.as_str(),
            ],
        ));
    }

    // Build all 61 binaries (default `all` target). `-f Makefile` is load-bearing:
    // coreutils-5.0 ships a maintainer GNUmakefile, and GNU make reads GNUmakefile
    // in preference to Makefile — that GNUmakefile `include`s absent Makefile.cfg/
    // Makefile.maint, and remaking those missing includes is what drove make to the
    // (nonexistent) $(SHELL). `-f Makefile` pins our baked Makefile and ignores the
    // GNUmakefile, exactly as live-bootstrap's pass1.kaem does (`make -f Makefile`).
    // LANG/LC_ALL neutralized for determinism; make finds tcc via the baked
    // absolute CC path, so no PATH.
    steps.push(
        Step::run("{src}", &["{in:make-mesboot0}/bin/make", "-f", "Makefile"])
            .env("LANG", "")
            .env("LC_ALL", ""),
    );

    // Install with the just-built `install` (live-bootstrap's `make install`):
    // its `install:` rule runs `src/install <all 61 binaries> {out}/bin`, so the
    // install step also end-to-end exercises the freshly built `install`. GNU
    // install does not create the destination, so make {out}/bin first.
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-mesboot0}/bin/make",
                "-f",
                "Makefile",
                "install",
                "PREFIX={out}",
            ],
        )
        .env("LANG", "")
        .env("LC_ALL", ""),
    );

    // All 61 installed binaries exist and are executable.
    let bins: Vec<String> = ALL_BINS
        .iter()
        .map(|b| format!("{{out}}/bin/{b}"))
        .collect();
    steps.push(Step::Require {
        paths: bins.clone(),
        exec: true,
    });

    // Runtime provenance (re #469): every binary is linked -static (LDFLAGS), so
    // none may carry a host loader (PT_INTERP) or host libc (DT_NEEDED) -- else it
    // would drag a host glibc in at run time. Assert it for all 61, not a slice.
    steps.push(Step::AssertStatic { paths: bins });

    // Smoke: the static mes-libc ELFs must actually run.
    //   * `true` exits 0 (a trivial static ELF runs).
    //   * `test`/`expr` exercise the string-compare path expr-strcmp patched.
    //   * `sort -o proof` + a content check proves sort parses input, compares
    //     (the memcoll/sort-locale strcmp path), and writes the exact bytes:
    //     "3\n1\n2\n" must become "1\n2\n3\n" or the follow-up SubstituteText
    //     reds. `wc`/`cat` add plain liveness.
    //   * `md5sum -c`/`sha1sum -c` recompute the digest of smoke.txt and exit
    //     non-zero on mismatch -- a functional check of the shared md5sum.o
    //     driver in both modes (exists+static alone would not catch a miscompiled
    //     hash), with no shell needed.
    steps.push(Step::run("{src}", &["{out}/bin/true"]));
    steps.push(Step::run("{src}", &["{out}/bin/test", "1", "=", "1"]));
    steps.push(Step::run("{src}", &["{out}/bin/expr", "a", "=", "a"]));
    steps.push(Step::WriteFile {
        path: "{src}/smoke.txt".into(),
        content: SMOKE_TXT.into(),
        exec: false,
    });
    steps.push(Step::run(
        "{src}",
        &["{out}/bin/sort", "-o", "proof", "smoke.txt"],
    ));
    steps.push(Step::substitute_text(
        "{src}/proof",
        vec![TextEdit::new("1\n2\n3\n", "1\n2\n3\n", 1)],
    ));
    steps.push(Step::run("{src}", &["{out}/bin/wc", "-l", "smoke.txt"]));
    steps.push(Step::run("{src}", &["{out}/bin/cat", "smoke.txt"]));
    steps.push(Step::WriteFile {
        path: "{src}/smoke.md5".into(),
        content: SMOKE_MD5.into(),
        exec: false,
    });
    steps.push(Step::run("{src}", &["{out}/bin/md5sum", "-c", "smoke.md5"]));
    steps.push(Step::WriteFile {
        path: "{src}/smoke.sha1".into(),
        content: SMOKE_SHA1.into(),
        exec: false,
    });
    steps.push(Step::run("{src}", &["{out}/bin/sha1sum", "-c", "smoke.sha1"]));

    Recipe::mesboot("coreutils-mesboot0", "5.0")
        .source_input("coreutils-mesboot0-source")
        .native_inputs(&["mes", "tcc", "make-mesboot0", "patch-mesboot"])
        .steps(steps)
}
