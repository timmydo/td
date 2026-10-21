use crate::ladder::{unpack_into, unpack_keep_top, BUILD_ROOT_PATH as PATH, BUILD_ROOT_SH as SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step, TextEdit};

// gcc-runtime is GCC 14.3.0's target runtime alone: libgcc.a, libgcc_eh.a,
// libgcc_s.so.1 and the crtbegin/crtend objects. It is built in the Debian
// build root: Debian's gcc and g++ compile the pinned GCC source's compiler
// proper, which then compiles libgcc against td glibc. That compiler and
// everything else of the root stay in the build; the output holds only the
// runtime files, compiled from source by the just-built compiler.
//
// The compiler's target search is a sysroot of td glibc alone
// (--with-build-sysroot), so its headers, start files and libc come from
// glibc-x86-64; the search probe shows Debian's /usr/include and /usr/lib are
// not searched. The probes compile with that compiler, link with Debian's ld
// under `-nostdlib`, naming every object, and run a dynamic PIE and a static
// ET_EXEC built from td glibc and this output alone. libgcc_s.so.1's SHA-1
// build ID comes from --enable-linker-build-id.
const GLIBC: &str = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
const GCC_LIB: &str = "{root}/inst/gcc-runtime/lib/gcc/x86_64-pc-linux-gnu/14.3.0";

/// Files of the runtime, from the installed libgcc's version directory.
const RUNTIME_OBJECTS: &[&str] = &[
    "libgcc.a",
    "libgcc_eh.a",
    "crtbegin.o",
    "crtbeginS.o",
    "crtbeginT.o",
    "crtend.o",
    "crtendS.o",
];

const PROBE: &str = "#include <stdio.h>\n\
                     #include <unwind.h>\n\
                     \n\
                     static _Unwind_Reason_Code step(struct _Unwind_Context *c, void *n)\n\
                     {\n\
                     \x20 (void)c;\n\
                     \x20 ++*(int *)n;\n\
                     \x20 return _URC_NO_REASON;\n\
                     }\n\
                     \n\
                     __attribute__((noinline)) static int frames(void)\n\
                     {\n\
                     \x20 int n = 0;\n\
                     \x20 _Unwind_Backtrace(step, &n);\n\
                     \x20 return n;\n\
                     }\n\
                     \n\
                     int main(int argc, char **argv)\n\
                     {\n\
                     \x20 (void)argv;\n\
                     \x20 volatile unsigned __int128 a = ((unsigned __int128)argc << 100) + 12345;\n\
                     \x20 volatile unsigned __int128 b = 1000003;\n\
                     \x20 unsigned long long q = (unsigned long long)(a / b % 1000000007u);\n\
                     \x20 int n = frames();\n\
                     \x20 printf(\"%llu %d\\n\", q, n >= 2);\n\
                     \x20 return 0;\n\
                     }\n";

pub fn recipe() -> Recipe {
    let target_flags = "-g1 -O2 -fPIC -fno-omit-frame-pointer \
                        -ffile-prefix-map={root}=/td-build-root -ffile-prefix-map={src}=/td-build";
    let mut steps = unpack_into("gcc-runtime-source", "{src}");
    for t in ["gmp63", "mpfr421", "mpc131"] {
        steps.extend(unpack_keep_top(t, "{src}"));
    }
    for (target, link) in [
        ("gmp-6.3.0", "{src}/gmp"),
        ("mpfr-4.2.1", "{src}/mpfr"),
        ("mpc-1.3.1", "{src}/mpc"),
    ] {
        steps.push(Step::Symlink {
            target: target.into(),
            link: link.into(),
        });
    }
    // libgcc appends plain `-g` to the caller's flags and `-g0` for
    // crtbegin/crtend; pin both to the bounded line-table policy.
    steps.push(Step::substitute_text(
        "{src}/libgcc/Makefile.in",
        vec![
            TextEdit::new("LIBGCC2_DEBUG_CFLAGS = -g", "LIBGCC2_DEBUG_CFLAGS = -g1", 1),
            TextEdit::new("$(MULTILIB_CFLAGS) -g0 \\", "$(MULTILIB_CFLAGS) -g1 \\", 1),
        ],
    ));
    steps.push(Step::CopyTree {
        from: format!("{GLIBC}/include"),
        dest: "{root}/sysroot/include".into(),
    });
    steps.push(Step::CopyTree {
        from: format!("{GLIBC}/lib"),
        dest: "{root}/sysroot/lib".into(),
    });
    // glibc's libc.so and libm.so scripts name its configured prefix, which
    // is not where the sysroot copy lives; ld finds the bare names on its
    // search path instead.
    steps.push(
        Step::run(
            "{root}/sysroot/lib",
            &[
                SH,
                "-c",
                "set -e; for f in libc.so libm.so; do \
                 grep -q 'GNU ld script' \"$f\"; \
                 sed -i 's,/td/store/glibc-2.41-x86_64/lib/,,g' \"$f\"; \
                 if grep -q /td/store/ \"$f\"; then echo \"$f still names a store path\" >&2; \
                 exit 1; fi; done",
            ],
        )
        .env("PATH", PATH),
    );
    steps.push(Step::MkDir {
        path: "{src}/bld".into(),
    });
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                SH,
                "../configure",
                "--prefix=/gcc-runtime",
                "--build=x86_64-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--target=x86_64-pc-linux-gnu",
                "--with-as=/usr/bin/as",
                "--with-ld=/usr/bin/ld",
                "--with-build-sysroot={root}/sysroot",
                "--with-native-system-header-dir=/include",
                "--enable-languages=c",
                "--enable-shared",
                "--enable-threads=posix",
                "--enable-linker-build-id",
                "--disable-bootstrap",
                "--disable-multilib",
                "--disable-nls",
                "--disable-libatomic",
                "--disable-libgomp",
                "--disable-libitm",
                "--disable-libquadmath",
                "--disable-libsanitizer",
                "--disable-libssp",
                "--disable-libvtv",
                "--disable-lto",
                "--disable-plugin",
                "--disable-libcc1",
                "--disable-decimal-float",
                "--disable-werror",
            ],
        )
        .env("PATH", PATH)
        .env("CONFIG_SHELL", SH)
        .env("CC", "/usr/bin/gcc")
        .env("CXX", "/usr/bin/g++"),
    );
    let flags = format!("CFLAGS_FOR_TARGET={target_flags}");
    for goal in ["all-gcc", "all-target-libgcc"] {
        steps.push(
            Step::run(
                "{src}/bld",
                &[
                    "/usr/bin/make",
                    "-j{jobs}",
                    &format!("SHELL={SH}"),
                    "MAKEINFO=true",
                    &flags,
                    goal,
                ],
            )
            .env("PATH", PATH)
            .env("CONFIG_SHELL", SH),
        );
    }
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                "/usr/bin/make",
                &format!("SHELL={SH}"),
                "MAKEINFO=true",
                "install-target-libgcc",
                "DESTDIR={root}/inst",
            ],
        )
        .env("PATH", PATH)
        .env("CONFIG_SHELL", SH),
    );
    steps.push(search_probe());
    steps.push(Step::MkDir {
        path: "{out}/lib".into(),
    });
    let mut files: Vec<String> = RUNTIME_OBJECTS
        .iter()
        .map(|f| format!("{GCC_LIB}/{f}"))
        .collect();
    files.push("{root}/inst/gcc-runtime/lib64/libgcc_s.so.1".into());
    steps.push(Step::CopyFiles {
        files,
        dest: "{out}/lib".into(),
    });
    // The link-time name; the installed one is GCC's own script, which also
    // names this soname.
    steps.push(Step::WriteFile {
        path: "{out}/lib/libgcc_s.so".into(),
        content: "GROUP ( libgcc_s.so.1 -lgcc )\n".into(),
        exec: false,
    });
    let mut required: Vec<String> = RUNTIME_OBJECTS
        .iter()
        .map(|f| format!("{{out}}/lib/{f}"))
        .collect();
    required.push("{out}/lib/libgcc_s.so.1".into());
    steps.push(Step::Require {
        paths: required,
        exec: false,
    });
    steps.extend(probes());
    steps.push(Step::split_debug_tree("{out}", "/usr/bin/objcopy"));

    Recipe::mesboot("gcc-runtime", "14.3.0")
        .source_input("gcc-14-source")
        .build_root()
        .native_inputs(&["glibc-x86-64"])
        .inputs(&["gmp63", "mpfr421", "mpc131"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check gcc-runtime: build GCC 14.3.0's libgcc in the Debian build root and link programs from td glibc and it alone"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run gcc-runtime 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

/// The just-built compiler, run as libgcc's build runs it.
const XGCC: &str = "{src}/bld/gcc/xgcc -B{src}/bld/gcc/ --sysroot={root}/sysroot";

/// The just-built compiler finds start files and libc in the td glibc
/// sysroot, searches no include or library directory outside its build tree,
/// that sysroot and its unpopulated install prefix, and hands ld the sysroot,
/// under which every one of ld's own search directories lies.
fn search_probe() -> Step {
    let script = format!(
        r#"set -e
[ ! -e /gcc-runtime ] || {{ echo '/gcc-runtime exists in the build root' >&2; exit 1; }}
x='{XGCC}'
inside() {{
  case $(realpath -m "$1") in
    '{{src}}/bld/'*|'{{root}}/sysroot/'*|/gcc-runtime/*) ;;
    *) echo "$2 outside the build and sysroot: $1" >&2; exit 1;;
  esac
}}
for f in crti.o crt1.o libc.so; do
  p=$($x -print-file-name=$f)
  case $(realpath -m "$p") in
    '{{root}}/sysroot/'*) ;;
    *) echo "$f resolves to $p" >&2; exit 1;;
  esac
done
dirs=$($x -print-search-dirs | sed -n 's/^libraries: =//p')
[ -n "$dirs" ] || {{ echo 'no library search dirs' >&2; exit 1; }}
IFS=:
for d in $dirs; do inside "$d" 'library search dir'; done
unset IFS
inc=$($x -E -v -x c /dev/null 2>&1 >/dev/null |
  sed -n '/^#include <...> search starts here:$/,/^End of search list.$/p' | sed '1d;$d')
[ -n "$inc" ] || {{ echo 'no include search dirs' >&2; exit 1; }}
for d in $inc; do inside "$d" 'include dir'; done
: > link.o
l=$($x -### -o link link.o 2>&1 | grep collect2)
case $l in
  *'--sysroot={{root}}/sysroot'*) ;;
  *) echo "the link does not pass the sysroot: $l" >&2; exit 1;;
esac
sd=$(ld --verbose | grep -o 'SEARCH_DIR("[^"]*")')
[ -n "$sd" ] || {{ echo 'ld names no search dirs' >&2; exit 1; }}
for s in $sd; do
  case $s in
    'SEARCH_DIR("='*) ;;
    *) echo "ld searches $s outside the sysroot" >&2; exit 1;;
  esac
done
"#
    );
    Step::run("{src}/bld", &[SH, "-c", &script]).env("PATH", PATH)
}

/// Link the probe as a dynamic PIE and as a static ET_EXEC from td glibc and
/// this output alone, run both, and check the shared runtime's shape.
fn probes() -> Vec<Step> {
    let compile = format!(
        "set -e; {XGCC} -c -O2 -fPIE -fno-omit-frame-pointer -o probe-pie.o probe.c; \
         {XGCC} -c -O2 -fno-pie -fno-omit-frame-pointer -o probe-nopie.o probe.c"
    );
    let link = format!(
        "set -e; g={GLIBC}/lib; r={{out}}/lib; \
         ld -nostdlib --build-id=sha1 --eh-frame-hdr -pie -dynamic-linker $g/ld-linux-x86-64.so.2 \
         -rpath $g:$r -o dyn $g/Scrt1.o $g/crti.o $r/crtbeginS.o probe-pie.o \
         $g/libc.so.6 $g/libc_nonshared.a $g/ld-linux-x86-64.so.2 -L$r -lgcc -lgcc_s \
         $r/crtendS.o $g/crtn.o; \
         ld -nostdlib --build-id=sha1 --eh-frame-hdr -static -o static $g/crt1.o $g/crti.o \
         $r/crtbeginT.o probe-nopie.o --start-group $g/libc.a $r/libgcc.a $r/libgcc_eh.a \
         --end-group $r/crtend.o $g/crtn.o"
    );
    let verify = "set -e; for p in dyn static; do out=$(./$p); \
                  [ \"$out\" = '19083919 1' ] || { echo \"$p: $out\" >&2; exit 1; }; done; \
                  h=$(readelf -h -l static); \
                  case $h in *'EXEC (Executable file)'*) ;; *) echo 'static is not ET_EXEC' >&2; exit 1;; esac; \
                  case $h in *INTERP*) echo 'static has an interpreter' >&2; exit 1;; esac; \
                  readelf -d dyn | grep -q 'NEEDED.*libgcc_s.so.1' || { echo 'dyn does not use libgcc_s' >&2; exit 1; }; \
                  d=$(readelf -d '{out}/lib/libgcc_s.so.1'); \
                  n=$(printf '%s\\n' \"$d\" | grep NEEDED | sed 's/.*\\[\\(.*\\)\\]/\\1/' | tr '\\n' ' '); \
                  [ \"$n\" = 'libc.so.6 ld-linux-x86-64.so.2 ' ] || { echo \"libgcc_s.so.1 needs: $n\" >&2; exit 1; }; \
                  case $d in *RPATH*|*RUNPATH*) echo 'libgcc_s.so.1 has a search path' >&2; exit 1;; esac; \
                  id=$(readelf -n '{out}/lib/libgcc_s.so.1' | sed -n 's/.*Build ID: //p'); \
                  [ ${#id} = 40 ] || { echo \"libgcc_s.so.1 build ID is not SHA-1: $id\" >&2; exit 1; }";
    vec![
        Step::MkDir {
            path: "{root}/probe".into(),
        },
        Step::WriteFile {
            path: "{root}/probe/probe.c".into(),
            content: PROBE.into(),
            exec: false,
        },
        Step::run("{root}/probe", &[SH, "-c", &compile]).env("PATH", PATH),
        Step::run("{root}/probe", &[SH, "-c", &link]).env("PATH", PATH),
        Step::run("{root}/probe", &[SH, "-c", verify]).env("PATH", PATH),
    ]
}
