use crate::ladder::unpack_into;
use crate::types::{Recipe, Step};

// GNU diffutils 2.7 — the tcc-era `diffutils` provider (re #469), the LAST of the
// five BASE_TOOLS host tools to gain a provider (after grep/sed/coreutils/gawk).
// It ships `cmp` and `diff`. The GCC/binutils rungs from binutils-mesboot0 up
// still name the HOST guix `diffutils` (base_inputs stages it and each rung farms
// `cmp`/`diff` in from `{in:diffutils}/bin`); that is host-executable ingress the
// bootstrap must close. This rung builds `cmp` and `diff` from source under tcc +
// mes libc — the same tcc/make pattern as grep-mesboot0/sed-mesboot0/
// coreutils-mesboot0/gawk-mesboot0 — so those rungs can consume td-built
// `cmp`/`diff` instead. It sits with its siblings below binutils-mesboot0,
// depending only on {mes, tcc, make-mesboot0}, none of which (transitively)
// depends on diffutils, so there is no cycle. (The consumer rewiring — flipping
// `{in:diffutils}` to `{in:diffutils-mesboot0}` and dropping `diffutils` from
// BASE_TOOLS — is #469's later atomic cutover, not this provider PR, exactly as
// grep/sed/coreutils/gawk landed their providers first.)
//
// This is live-bootstrap's diffutils-2.7 (steps/diffutils-2.7, mk/main.mk — its
// tcc + mes-libc build). Host-tool-free the same way its siblings are:
//
//   * No ./configure: live-bootstrap builds diffutils-2.7 with its own Makefile,
//     passing every feature macro as -D on the tcc line. td bakes that Makefile
//     (diffutils-mesboot0.mk) with tcc/mes paths. diffutils-2.7's system.h and
//     version.c #include <config.h> UNCONDITIONALLY, so a config.h is mandatory
//     (unlike grep-2.4); its sole content is the one string-valued define
//     (NULL_DEVICE), whose escaped `"` is a shell metacharacter td's no-shell make
//     cannot pass (see diffutils-mesboot0-config.h and the .mk).
//   * No host make shell: td's Make 3.80 drives the build; every recipe line is
//     metacharacter-free, so make execs tcc via its stock no-shell fast path.
//   * mes-libc deltas (see the .mk header): -DHAVE_STRING_H routes the string/mem
//     macros to the ANSI names mes ships (mes lacks index/rindex/bcmp/bcopy);
//     -Dvfork=fork substitutes fork for the vfork mes lacks (diff's paginator
//     path, never reached here); alloca.o is dropped (mes ships GNU C alloca).
//   * No host install: live-bootstrap's `install:` runs host `install`. This rung
//     installs the two binaries (`cmp`, `diff`) with engine-native Steps instead
//     (two independent binaries — no symlink/alias, unlike gawk's `awk`).
//
// Inputs are mes (headers + libc) and tcc (compiler) + make-mesboot0 (`make`) —
// no host tools, and no bash: every acceptance test below is exit-code based, so
// the rung needs no shell.
const CONFIG_H: &str = include_str!("diffutils-mesboot0-config.h");
const MAKEFILE: &str = include_str!("diffutils-mesboot0.mk");

// Acceptance fixtures. `cmp` and `diff` are pure byte/line comparators with no
// floating point, so the tcc double->int fold that gated gawk (re #469) does not
// apply here; the risk this rung guards is instead that a comparator built under
// tcc + mes libc actually WALKS the bytes and honours its filter flags. Every
// test below is fail-closed on exit code: the CORRECT answer is "equal" (exit 0),
// but reaching exit 0 requires the comparison logic — byte walk, whitespace/case
// folding, initial-offset skip — to be intact. If the line/byte comparator were
// broken to always-differ, or a filter (-w/-i/-i N) failed to fold, the tool
// would report a difference and exit 1, and the Run step would red the rung.
//
// A_TXT and its byte-identical twin B_TXT: the plain-equal case.
const A_TXT: &str = "alpha one\nbeta two\ngamma three\n";
const B_TXT: &str = "alpha one\nbeta two\ngamma three\n";
// C_TXT differs from A_TXT only in whitespace (double space in the beta line):
// `diff -w` (ignore-all-space) must fold it back to equal.
const C_TXT: &str = "alpha one\nbeta  two\ngamma three\n";
// D_TXT differs from A_TXT only in case (GAMMA vs gamma): `diff -i` (ignore-case)
// must fold it back to equal.
const D_TXT: &str = "alpha one\nbeta two\nGAMMA three\n";
// E_TXT is A_TXT with its first 4 bytes ("alph") overwritten by "XXXX"; the tail
// from byte 4 on ("a one\n...") is byte-identical to A_TXT's. `cmp -i 4` skips the
// first 4 bytes of BOTH files, so the compared tails are equal (exit 0) — this
// exercises cmp's seek-past-prefix plus the tail byte walk.
const E_TXT: &str = "XXXXa one\nbeta two\ngamma three\n";

pub fn recipe() -> Recipe {
    let mut steps = unpack_into("diffutils-mesboot0-source", "{src}");

    // config.h (the one string-valued NULL_DEVICE define, mandatory on the -I.
    // path) + the baked Makefile (tcc/mes paths, live-bootstrap's quote-free
    // defines plus the mes-libc deltas, no-shell recipe lines).
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

    // Build `cmp` and `diff` (default `all` target). `-f Makefile` pins the baked
    // Makefile (defensive, as the siblings do). LANG/LC_ALL neutralized for
    // determinism; make finds tcc via the baked absolute CC path, so no PATH.
    steps.push(
        Step::run("{src}", &["{in:make-mesboot0}/bin/make", "-f", "Makefile"])
            .env("LANG", "")
            .env("LC_ALL", ""),
    );

    // Install the two binaries (engine-native, the host-free stand-in for
    // live-bootstrap's `install`). No alias — cmp and diff are distinct programs.
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/cmp".into(), "{src}/diff".into()],
        dest: "{out}/bin".into(),
    });

    // cmp and diff exist and are executable.
    steps.push(Step::Require {
        paths: vec!["{out}/bin/cmp".into(), "{out}/bin/diff".into()],
        exec: true,
    });

    // Runtime provenance (re #469): the link is -static (LDFLAGS), so cmp/diff must
    // carry no host loader (PT_INTERP) or host libc (DT_NEEDED) — else they would
    // drag a host glibc in at run time.
    steps.push(Step::assert_static(&["{out}/bin/cmp", "{out}/bin/diff"]));

    // Smoke 1/2: --version proves each static mes-libc ELF actually runs (exit 0).
    steps.push(Step::run("{src}", &["{out}/bin/cmp", "--version"]));
    steps.push(Step::run("{src}", &["{out}/bin/diff", "--version"]));

    // Acceptance fixtures.
    steps.push(Step::WriteFile {
        path: "{src}/a.txt".into(),
        content: A_TXT.into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{src}/b.txt".into(),
        content: B_TXT.into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{src}/c.txt".into(),
        content: C_TXT.into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{src}/d.txt".into(),
        content: D_TXT.into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{src}/e.txt".into(),
        content: E_TXT.into(),
        exec: false,
    });

    // cmp on byte-identical files: plain, then silent (-s, exit-code only). Both
    // exit 0 iff cmp walks the bytes and finds no difference.
    steps.push(Step::run("{src}", &["{out}/bin/cmp", "a.txt", "b.txt"]));
    steps.push(Step::run("{src}", &["{out}/bin/cmp", "-s", "a.txt", "b.txt"]));
    // cmp --ignore-initial: skip the first 4 bytes of each, compare equal tails.
    steps.push(Step::run(
        "{src}",
        &["{out}/bin/cmp", "-i", "4", "a.txt", "e.txt"],
    ));

    // diff on byte-identical files: full, then brief (-q). Both exit 0 (no diff).
    steps.push(Step::run("{src}", &["{out}/bin/diff", "a.txt", "b.txt"]));
    steps.push(Step::run("{src}", &["{out}/bin/diff", "-q", "a.txt", "b.txt"]));
    // diff -w folds the whitespace-only difference (C_TXT) back to equal (exit 0).
    steps.push(Step::run(
        "{src}",
        &["{out}/bin/diff", "-w", "a.txt", "c.txt"],
    ));
    // diff -i folds the case-only difference (D_TXT) back to equal (exit 0).
    steps.push(Step::run(
        "{src}",
        &["{out}/bin/diff", "-i", "a.txt", "d.txt"],
    ));

    Recipe::mesboot("diffutils-mesboot0", "2.7")
        .source_input("diffutils-mesboot0-source")
        .native_inputs(&["mes", "tcc", "make-mesboot0"])
        .steps(steps)
}
