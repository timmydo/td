use crate::ladder::unpack_into;
use crate::types::{Recipe, Step};

// GNU grep 2.4 — the tcc-era grep provider, a cycle-breaker below the first
// host-tool consumer. This rung builds grep from source under tcc + mes libc —
// the same tcc/make pattern as sed-mesboot0/coreutils-mesboot0 — so the
// GCC/binutils rungs from binutils-mesboot0 up consume td-built grep instead of
// a host tool. It sits with its siblings below binutils-mesboot0, so nothing it
// depends on can depend on it.
//
// This is live-bootstrap's grep-2.4 (steps/grep-2.4, mk/main.mk), host-tool-free
// the same way its siblings are:
//
//   * No ./configure: live-bootstrap builds grep-2.4 with NO config.h and `make`
//     with six -D on the tcc command line. td bakes that Makefile
//     (grep-mesboot0.mk) with tcc/mes paths, moving only the two string-valued
//     defines (PACKAGE/VERSION) into config.h under -DHAVE_CONFIG_H (their
//     escaped `"` is a shell metacharacter); the other four are quote-free and
//     stay global on the command line (see grep-mesboot0-config.h and the .mk).
//   * No host make shell: td's Make 3.80 drives the build; every recipe line is
//     metacharacter-free, so make execs tcc via its stock no-shell fast path.
//   * No host install/ln: live-bootstrap's `install:` target runs host
//     `install -D grep` + `ln -sf` for the egrep/fgrep aliases. This rung
//     installs the one `grep` binary and its two symlinks with engine-native
//     Steps instead (the sandbox has neither install nor ln — that is what the
//     bootstrap builds).
//   * grep-2.4 needs no patches (live-bootstrap ships none for it), so unlike
//     coreutils-mesboot0 this rung declares no patch input.
//
// egrep/fgrep are BRE-default symlinks to the one `grep` binary, exactly as
// live-bootstrap installs them: grep-2.4 selects the egrep/fgrep matcher at
// COMPILE time (egrepmat.o/fgrepmat.o), and live-bootstrap's GREP_SRC builds
// only grepmat.o (matcher = 0 -> defaults to "grep"/BRE). argv[0] never selects
// the matcher in this version, so `egrep PAT` behaves as `grep PAT`; ERE/fixed
// come from grep's -E/-F flags. The bootstrap's later native grep rebuild
// provides mode-selecting egrep/fgrep; here fidelity to live-bootstrap governs.
//
// Inputs are mes (headers + libc), tcc (compiler), and make-mesboot0 (`make`) —
// no host tools.
const CONFIG_H: &str = include_str!("grep-mesboot0-config.h");
const MAKEFILE: &str = include_str!("grep-mesboot0.mk");

// Smoke input the matcher must discriminate. `ap` matches apple/apricot but NOT
// banana/cherry/a.*b, so grep and grep -v of the same pattern both select a
// non-empty set (both exit 0). That pair is the discrimination proof: a
// miscompiled matcher that matched everything would empty the -v run (exit 1 ->
// red), and one that matched nothing would empty the plain run. The literal
// `a.*b` line makes the -F (fixed-string) check mode-sensitive: `.*` is a
// regex-metacharacter string that matches EVERY line under BRE but only the one
// literal line under -F, so -F/-F -v of `.*` discriminate exactly when the kwset
// fixed matcher is really selected (a silent -F -> BRE fallback would match all,
// emptying the -v run -> red).
const SMOKE_TXT: &str = "apple\nbanana\ncherry\napricot\na.*b\n";

pub fn recipe() -> Recipe {
    let mut steps = unpack_into("grep-mesboot0-source", "{src}");

    // config.h (the two string-valued PACKAGE/VERSION defines) + the baked
    // Makefile (tcc/mes paths, the four quote-free defines, no-shell recipe lines).
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

    // Build `grep` (default `all` target). `-f Makefile` pins the baked Makefile:
    // grep-2.4 ships no GNUmakefile, but coreutils taught us GNU make reads a
    // maintainer GNUmakefile in preference to Makefile, so pin it defensively (as
    // live-bootstrap's kaem does). LANG/LC_ALL neutralized for determinism; make
    // finds tcc via the baked absolute CC path, so no PATH.
    steps.push(
        Step::run("{src}", &["{in:make-mesboot0}/bin/make", "-f", "Makefile"])
            .env("LANG", "")
            .env("LC_ALL", ""),
    );

    // Install the single binary and its egrep/fgrep aliases (engine-native, the
    // host-free stand-in for live-bootstrap's `install -D` + `ln -sf`). Relative
    // symlinks (same dir) survive a store relocation; both resolve to `grep`.
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/grep".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Symlink {
        target: "grep".into(),
        link: "{out}/bin/egrep".into(),
    });
    steps.push(Step::Symlink {
        target: "grep".into(),
        link: "{out}/bin/fgrep".into(),
    });

    // grep and both alias symlinks exist and are executable.
    steps.push(Step::Require {
        paths: vec![
            "{out}/bin/grep".into(),
            "{out}/bin/egrep".into(),
            "{out}/bin/fgrep".into(),
        ],
        exec: true,
    });

    // Runtime provenance (re #469): the link is -static (LDFLAGS), so grep must
    // carry no host loader (PT_INTERP) or host libc (DT_NEEDED) -- else it would
    // drag a host glibc in at run time. Assert the real ELF; egrep/fgrep are
    // symlinks to it.
    steps.push(Step::assert_static(&["{out}/bin/grep"]));

    // Smoke: the static mes-libc ELF must actually run and DISCRIMINATE. grep has
    // no file-output option and the sandbox has no shell for redirection, so each
    // check is exit-code based (a Run reds the rung on any non-zero exit): a
    // matched pattern and its `-v` inversion both exit 0 only if grep selects a
    // proper non-empty subset for that engine.
    //   * BRE `ap`: matches apple/apricot, inverts to banana/cherry/a.*b.
    //   * ERE `a(pp|pr)` (-E): mode-sensitive on its own -- BRE reads `(`/`|` as
    //     literals and matches nothing (exit 1 -> red), so a passing -E run
    //     proves the ERE dfa/regex engine is active; -v confirms discrimination.
    //   * fixed `.*` (-F): mode-sensitive via the literal a.*b line -- correct
    //     fixed matching selects that one line and inverts to the other four,
    //     whereas a -F -> BRE fallback matches all and empties the -v run (red).
    //   * --version prints the banner (proves config.h's VERSION reached grep.c).
    //   * egrep/fgrep run through the symlinks (BRE-default, as installed).
    steps.push(Step::WriteFile {
        path: "{src}/fruit.txt".into(),
        content: SMOKE_TXT.into(),
        exec: false,
    });
    steps.push(Step::run("{src}", &["{out}/bin/grep", "ap", "fruit.txt"]));
    steps.push(Step::run("{src}", &["{out}/bin/grep", "-v", "ap", "fruit.txt"]));
    steps.push(Step::run(
        "{src}",
        &["{out}/bin/grep", "-E", "a(pp|pr)", "fruit.txt"],
    ));
    steps.push(Step::run(
        "{src}",
        &["{out}/bin/grep", "-E", "-v", "a(pp|pr)", "fruit.txt"],
    ));
    steps.push(Step::run("{src}", &["{out}/bin/grep", "-F", ".*", "fruit.txt"]));
    steps.push(Step::run(
        "{src}",
        &["{out}/bin/grep", "-F", "-v", ".*", "fruit.txt"],
    ));
    steps.push(Step::run("{src}", &["{out}/bin/grep", "--version"]));
    steps.push(Step::run("{src}", &["{out}/bin/egrep", "ap", "fruit.txt"]));
    steps.push(Step::run("{src}", &["{out}/bin/fgrep", "apple", "fruit.txt"]));

    Recipe::mesboot("grep-mesboot0", "2.4")
        .source_input("grep-mesboot0-source")
        .native_inputs(&["mes", "tcc", "make-mesboot0"])
        .steps(steps)
}
