use crate::ladder::{
    build_root_glibc_cc, unpack_into, BUILD_ROOT_GLIBC as GLIBC, BUILD_ROOT_PATH as PATH,
    BUILD_ROOT_SH as SH,
};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// zlib 1.3.1's shared libz.so.1 for x86_64: the one zlib the upstream Rust
// toolchain's libLLVM and rust-lld need at run time (rust-toolchain links it
// into its lib/). Built in the Debian build root: Debian's gcc compiles
// zlib's sources against a sysroot of td glibc under the shipped target
// profile (`build_root_glibc_cc`), and Debian's ld links the library from td
// glibc's and gcc-runtime's start files alone (`-nostdlib -z defs`, every
// object named), so no file of the root's libc or libgcc reaches it. The
// probe links a program against the result the same way and round-trips
// data through it.
const RUNTIME: &str = "{in:gcc-runtime}/lib";

const PROBE: &str = "#include <stdio.h>\n\
                     #include <string.h>\n\
                     #include <zlib.h>\n\
                     \n\
                     int main(void)\n\
                     {\n\
                     \x20 const char in[] = \"td zlib td zlib td zlib td zlib\";\n\
                     \x20 unsigned char packed[128], out[128];\n\
                     \x20 uLongf plen = sizeof packed, olen = sizeof out;\n\
                     \x20 if (compress(packed, &plen, (const Bytef *)in, sizeof in) != Z_OK) return 1;\n\
                     \x20 if (uncompress(out, &olen, packed, plen) != Z_OK) return 2;\n\
                     \x20 if (olen != sizeof in || memcmp(in, out, olen) != 0) return 3;\n\
                     \x20 printf(\"%s %d\\n\", zlibVersion(), plen < sizeof in);\n\
                     \x20 return 0;\n\
                     }\n";

pub fn recipe() -> Recipe {
    let mut steps = unpack_into("zlib-x86-64-source", "{src}");
    steps.extend(build_root_glibc_cc());
    // The library link: the objects zlib's Makefile passes, between td
    // glibc's and gcc-runtime's start files, against td glibc's libc.
    steps.push(Step::WriteFile {
        path: "{root}/wb/ldshared".into(),
        content: format!(
            "#!{SH}\nset -e\nout=; objs=\n\
             while [ $# -gt 0 ]; do case $1 in -o) out=$2; shift;; *.o|*.lo) objs=\"$objs $1\";; esac; shift; done\n\
             [ -n \"$out\" ] && [ -n \"$objs\" ] || {{ echo 'ldshared: no output or objects' >&2; exit 1; }}\n\
             exec /usr/bin/ld -nostdlib -shared -z defs -soname libz.so.1 \
             --version-script {{src}}/zlib.map --build-id=sha1 --eh-frame-hdr -o \"$out\" \
             {GLIBC}/lib/crti.o {RUNTIME}/crtbeginS.o $objs {GLIBC}/lib/libc.so.6 \
             {GLIBC}/lib/libc_nonshared.a {RUNTIME}/crtendS.o {GLIBC}/lib/crtn.o\n"
        ),
        exec: true,
    });
    steps.push(
        Step::run("{src}", &[SH, "./configure", "--prefix=/zlib", "--shared"])
            .env("PATH", PATH)
            .env("CC", "{root}/wb/cc")
            .env("CFLAGS", "-O2")
            .env("LDSHARED", "{root}/wb/ldshared")
            .env("SOURCE_DATE_EPOCH", "1"),
    );
    // Every configure probe passed: hidden visibility, no fallback defines.
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                "set -e; f=$(grep '^SFLAGS=' Makefile); \
                 case $f in *-DHAVE_HIDDEN*) ;; *) echo \"configure lost hidden visibility: $f\" >&2; exit 1;; esac; \
                 case $f in *-DNO_*|*-DHAS_*) echo \"configure fell back: $f\" >&2; exit 1;; esac",
            ],
        )
        .env("PATH", PATH),
    );
    steps.push(
        Step::run("{src}", &["make", "-j{jobs}", "libz.so.1.3.1"])
            .env("PATH", PATH)
            .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::MkDir {
        path: "{out}/lib".into(),
    });
    steps.push(Step::MkDir {
        path: "{out}/include".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/libz.so.1.3.1".into()],
        dest: "{out}/lib".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/zlib.h".into(), "{src}/zconf.h".into()],
        dest: "{out}/include".into(),
    });
    for link in ["libz.so.1", "libz.so"] {
        steps.push(Step::Symlink {
            target: "libz.so.1.3.1".into(),
            link: format!("{{out}}/lib/{link}"),
        });
    }
    steps.extend(probe());
    steps.push(Step::split_debug_tree("{out}", "/usr/bin/objcopy"));

    Recipe::mesboot("zlib-x86-64", "1.3.1")
        .source_input("zlib-x86-64-source")
        .build_root()
        .native_inputs(&["glibc-x86-64", "gcc-runtime"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check zlib-x86-64: build zlib 1.3.1 in the Debian build root against td glibc and gcc-runtime alone"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run zlib-x86-64 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

/// Link a program against the built library from td glibc and gcc-runtime
/// alone, run it, and check the library's shape.
fn probe() -> Vec<Step> {
    let build = format!(
        "set -e; g={GLIBC}/lib; r={RUNTIME}; \
         '{{root}}/wb/cc' -c -O2 -fPIE -I'{{out}}/include' -o probe.o probe.c; \
         ld -nostdlib --build-id=sha1 --eh-frame-hdr -pie -dynamic-linker $g/ld-linux-x86-64.so.2 \
         -rpath $g:'{{out}}/lib' -o probe $g/Scrt1.o $g/crti.o $r/crtbeginS.o probe.o \
         '{{out}}/lib/libz.so.1' $g/libc.so.6 $g/libc_nonshared.a $g/ld-linux-x86-64.so.2 \
         $r/crtendS.o $g/crtn.o"
    );
    let verify = "set -e; out=$(./probe); \
                  [ \"$out\" = '1.3.1 1' ] || { echo \"probe: $out\" >&2; exit 1; }; \
                  d=$(readelf -d '{out}/lib/libz.so.1.3.1'); \
                  n=$(printf '%s\\n' \"$d\" | grep NEEDED | sed 's/.*\\[\\(.*\\)\\]/\\1/' | tr '\\n' ' '); \
                  [ \"$n\" = 'libc.so.6 ' ] || { echo \"libz.so.1 needs: $n\" >&2; exit 1; }; \
                  printf '%s\\n' \"$d\" | grep -q 'SONAME.*\\[libz.so.1\\]' || { echo 'libz soname is not libz.so.1' >&2; exit 1; }; \
                  case $d in *RPATH*|*RUNPATH*) echo 'libz.so.1 has a search path' >&2; exit 1;; esac; \
                  id=$(readelf -n '{out}/lib/libz.so.1.3.1' | sed -n 's/.*Build ID: //p'); \
                  [ ${#id} = 40 ] || { echo \"libz.so.1 build ID is not SHA-1: $id\" >&2; exit 1; }";
    vec![
        Step::MkDir {
            path: "{root}/probe".into(),
        },
        Step::WriteFile {
            path: "{root}/probe/probe.c".into(),
            content: PROBE.into(),
            exec: false,
        },
        Step::run("{root}/probe", &[SH, "-c", &build]).env("PATH", PATH),
        Step::run("{root}/probe", &[SH, "-c", verify]).env("PATH", PATH),
    ]
}
