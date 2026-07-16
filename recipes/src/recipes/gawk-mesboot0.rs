use crate::ladder::unpack_into;
use crate::types::{Recipe, Step, TextEdit};

// GNU awk 3.0.4 — the tcc-era `gawk` provider, a cycle-breaker one tier below the
// first host-tool consumer. This rung builds `gawk` from source under tcc + mes
// libc — the same tcc/make pattern as grep-mesboot0/sed-mesboot0/
// coreutils-mesboot0 — so the GCC/binutils rungs from binutils-mesboot0 up
// consume a td-built `awk` (via the ToolFarm) instead of a host tool. It sits
// with its siblings below binutils-mesboot0, depending only on {mes, tcc,
// make-mesboot0}, none of which (transitively) depends on gawk, so there is no
// cycle.
//
// This is live-bootstrap's gawk-3.0.4 (steps/gawk-3.0.4, mk/main.mk — its tcc +
// mes-libc build), NOT the heavier gcc-mesboot1-era gawk 3.1.8 the separate
// `gawk-mesboot` rung builds. Host-tool-free the same way its siblings are:
//
//   * No ./configure: live-bootstrap builds gawk-3.0.4 with NO config.h and its
//     own Makefile, passing every feature macro as -D on the tcc line. td bakes
//     that Makefile (gawk-mesboot0.mk) with tcc/mes paths, moving only the one
//     string-valued define (DEFPATH) into config.h under -DHAVE_CONFIG_H (its
//     escaped `"` is a shell metacharacter td's no-shell make cannot pass — see
//     gawk-mesboot0-config.h and the .mk).
//   * No host make shell: td's Make 3.80 drives the build; every recipe line is
//     metacharacter-free, so make execs tcc via its stock no-shell fast path.
//   * No host bison: live-bootstrap `rm`s the shipped awktab.c and regenerates it
//     with host `bison`. td ships no host bison and uses the shipped Bison-1.25
//     parser AS-IS — its established pattern (grep/sed/binutils use their shipped
//     generated parsers, re #468). The shipped awktab.c compiles under tcc (its
//     alloca preamble reaches <malloc.h>/<alloca.h> only on sparc/sgi/MSDOS/AIX,
//     and its alloca() binds to mes libc's alloca — the .mk drops gawk's own
//     alloca.o, which mes would otherwise redefine, keeping -DC_ALLOCA=1 only to
//     declare the extern; see the .mk's GAWK_SRC note).
//   * No host install/ln: live-bootstrap's `install:` runs host `install -D gawk`
//     + `ln -s` for the `awk` alias. This rung installs the one `gawk` binary and
//     its `awk` symlink with engine-native Steps instead.
//
// Inputs are mes (headers + libc) and tcc (compiler) + make-mesboot0 (`make`) —
// no host tools, and no bash: every acceptance test below is either exit-code
// based or fail-closed inside gawk itself, so the rung needs no shell.
const CONFIG_H: &str = include_str!("gawk-mesboot0-config.h");
const MAKEFILE: &str = include_str!("gawk-mesboot0.mk");

// The tcc-era `gawk` was blocked until the tcc rung's double->int fold fix (re
// #469; tcc.rs's HIDDEND_LL patch, see #488/#491): gawk stores every number as a
// C double and converts to int at runtime for `%d`, `int()`, array subscripts,
// and substr/length offsets, so a tcc whose __fixdfdi returned 0 corrupted every
// gawk arithmetic result. This arithmetic self-test is the awk analogue of
// tcc-dttest.c: it fail-closes (exit 1) if any double->int conversion is wrong,
// so the rung reds if that fix ever regresses under gawk. `sprintf("%d", ...)`
// and `int()` drive the runtime double->int helper directly; substr/length drive
// the integer string-index arithmetic the subs.awk splice below relies on.
const ARITH_AWK: &str = "BEGIN { \
if (sprintf(\"%d\", 3 + 5) != \"8\") exit 1; \
if (sprintf(\"%d\", 1024 * 1024) != \"1048576\") exit 1; \
if (int(100000 / 7) != 14285) exit 1; \
if (substr(\"abcdefgh\", 3, 4) != \"cdef\") exit 1; \
if (length(\"hello\") != 5) exit 1; \
a[3 + 5] = \"ok\"; if (a[8] != \"ok\") exit 1; \
exit 0 }";

// The two capability probes autoconf-2.64 config.status runs before it trusts
// `awk` (config.status: `$AWK 'BEGIN { getline <"..." }'` and
// `$AWK 'BEGIN { print "a\rb" }'`), reproduced fail-closed: read the 5-line
// subs.in with getline (must count 5) and confirm the CR escape is one char.
const PROBE_AWK: &str = "BEGIN { \
n = 0; while ((getline line < \"subs.in\") > 0) n++; if (n != 5) exit 1; \
if (length(\"a\\rb\") != 3) exit 1; \
exit 0 }";

// The "full binutils acceptance test" (re #469): the EXACT substitution engine
// autoconf-2.64 config.status writes to $tmp/subs.awk and runs as `$AWK -f
// subs.awk` over every template (every Makefile.in, config.h.in) the
// binutils-mesboot0 / gcc-core-mesboot0 rungs generate. Its `split`/`length`/
// `substr(line, 1, len)` / `substr(line, len + keylen + 3)` / `len += ...`
// arithmetic on string lengths and indices is exactly what a miscompiled gawk
// (broken double->int) would corrupt, producing garbled Makefiles. The seeding
// is config.status's own idiom too: the S[] values are assigned, then
// `for (key in S) S_is_set[key] = 1` derives the membership set by iterating the
// array (NOT hand-seeded) -- so this also exercises gawk's `for (key in array)`,
// the load-bearing statement that decides which @VAR@ markers get substituted.
// The adaptations from real config.status are two, both immaterial to the
// arithmetic: the output sink -- `print line` writes to the file `subs.out` (via
// mes's correct Linux O_* fcntl.h) instead of stdout, so the rung asserts exact
// bytes with SubstituteText and needs no shell -- and dropping config.status's
// trailing `FS = ""` (the split passes an explicit "@" separator, so FS never
// participates, and gawk 3.0.4's empty-FS handling is beside the point here).
const SUBS_AWK: &str = "\
BEGIN {\n\
S[\"CC\"] = \"tcc\"\n\
S[\"PREFIX\"] = \"/td/store\"\n\
for (key in S) S_is_set[key] = 1\n\
}\n\
{\n\
  line = $0\n\
  nfields = split(line, field, \"@\")\n\
  len = length(field[1])\n\
  for (i = 2; i < nfields; i++) {\n\
    key = field[i]\n\
    keylen = length(key)\n\
    if (S_is_set[key]) {\n\
      value = S[key]\n\
      line = substr(line, 1, len) \"\" value \"\" substr(line, len + keylen + 3)\n\
      len += length(value) + length(field[++i])\n\
    } else {\n\
      len += 1 + keylen\n\
    }\n\
  }\n\
  print line > \"subs.out\"\n\
}\n";

// A Makefile.in-shaped template with @VAR@ markers exercising every branch and
// offset case: a lone found marker (@CC@, @PREFIX@); two found on one line (the
// running-offset case); an UNSET-then-found line (@NOPE@ then @CC@ -- the unset
// marker's `len += 1 + keylen` must leave the running offset correct so the later
// @CC@ still splices at the right index); and a lone unset (@NOPE@) that survives
// verbatim. This is exactly the substr/len arithmetic a miscompiled gawk corrupts.
const SUBS_IN: &str = "CC = @CC@\n\
prefix = @PREFIX@\n\
both = @CC@ in @PREFIX@\n\
mix = @NOPE@ then @CC@\n\
unset = @NOPE@\n";

// The exact output the splice must produce (traced by hand): @CC@ -> tcc,
// @PREFIX@ -> /td/store, both markers on line 3 spliced with correct running
// offsets, the mixed line's @NOPE@ kept while its @CC@ -> tcc at the right index,
// and the lone @NOPE@ left intact. A broken double->int corrupts the substr
// indices and this block never appears -> the SubstituteText (expect 1) reds.
const SUBS_EXPECTED: &str = "CC = tcc\n\
prefix = /td/store\n\
both = tcc in /td/store\n\
mix = @NOPE@ then tcc\n\
unset = @NOPE@\n";

pub fn recipe() -> Recipe {
    let mut steps = unpack_into("gawk-mesboot0-source", "{src}");

    // config.h (the one string-valued DEFPATH define) + the baked Makefile
    // (tcc/mes paths, live-bootstrap's quote-free defines, no-shell recipe lines).
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

    // Build `gawk` (default `all` target). `-f Makefile` pins the baked Makefile
    // (gawk-3.0.4 ships a maintainer-oriented Makefile.in flow; GNU make would
    // prefer a GNUmakefile if one appeared, so pin defensively as the siblings
    // do). LANG/LC_ALL neutralized for determinism; make finds tcc via the baked
    // absolute CC path, so no PATH.
    steps.push(
        Step::run("{src}", &["{in:make-mesboot0}/bin/make", "-f", "Makefile"])
            .env("LANG", "")
            .env("LC_ALL", ""),
    );

    // Install the single binary and its `awk` alias (engine-native, the host-free
    // stand-in for live-bootstrap's `install -D` + `ln -s`). The relative symlink
    // (same dir) survives a store relocation and resolves to `gawk`.
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/gawk".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Symlink {
        target: "gawk".into(),
        link: "{out}/bin/awk".into(),
    });

    // gawk and its awk alias exist and are executable.
    steps.push(Step::Require {
        paths: vec!["{out}/bin/gawk".into(), "{out}/bin/awk".into()],
        exec: true,
    });

    // Runtime provenance (re #469): the link is -static (LDFLAGS), so gawk must
    // carry no host loader (PT_INTERP) or host libc (DT_NEEDED) -- else it would
    // drag a host glibc in at run time. Assert the real ELF; awk is a symlink.
    steps.push(Step::assert_static(&["{out}/bin/gawk"]));

    // Smoke 1: --version proves the static mes-libc ELF actually runs (exit 0).
    steps.push(Step::run("{src}", &["{out}/bin/gawk", "--version"]));

    // Smoke 2: the arithmetic self-test (the double->int regression guard).
    steps.push(Step::run("{src}", &["{out}/bin/gawk", ARITH_AWK]));

    // The subs.awk acceptance fixtures.
    steps.push(Step::WriteFile {
        path: "{src}/subs.awk".into(),
        content: SUBS_AWK.into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{src}/subs.in".into(),
        content: SUBS_IN.into(),
        exec: false,
    });

    // Smoke 3: config.status's getline + CR capability probes (needs subs.in).
    steps.push(Step::run("{src}", &["{out}/bin/gawk", PROBE_AWK]));

    // Smoke 4: run the exact config.status substitution engine over subs.in
    // (writing subs.out), then assert its bytes are exactly SUBS_EXPECTED. A
    // miscompiled gawk corrupts the substr/len arithmetic, subs.out diverges, the
    // expected block is absent, and the SubstituteText (expect 1) reds the rung.
    steps.push(Step::run(
        "{src}",
        &["{out}/bin/gawk", "-f", "subs.awk", "subs.in"],
    ));
    steps.push(Step::substitute_text(
        "{src}/subs.out",
        vec![TextEdit::new(SUBS_EXPECTED, SUBS_EXPECTED, 1)],
    ));

    Recipe::mesboot("gawk-mesboot0", "3.0.4")
        .source_input("gawk-mesboot0-source")
        .native_inputs(&["mes", "tcc", "make-mesboot0"])
        .steps(steps)
}
