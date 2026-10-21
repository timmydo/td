use crate::ladder::{
    build_root_glibc_cc, build_root_link_map_check, unpack_into, BUILD_ROOT_GLIBC as GLIBC,
    BUILD_ROOT_PATH as PATH, BUILD_ROOT_SH as SH,
};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// zlib 1.3.1 for x86_64: the shared libz.so.1 the upstream Rust toolchain's
// libLLVM and rust-lld need at run time (rust-toolchain links it into its
// lib/), and the static libz.a curl, Git and td-util-test's
// minigzip link. Built in the Debian build root: Debian's gcc compiles
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

/// The archive's API test: version agreement, level-9 memory round trip,
/// the gzip file API, and raw deflate decoded by inflateBack.
const ARCHIVE_TEST: &str = r#"#include <zlib.h>
#include <stdio.h>
#include <string.h>

struct input_state {
    unsigned char *data;
    unsigned int length;
    int used;
};

struct output_state {
    unsigned char *data;
    unsigned int length;
    unsigned int capacity;
};

static int fail(const char *message, int code) {
    fputs(message, stderr);
    fputc('\n', stderr);
    return code;
}

static unsigned int input_chunk(void *opaque, unsigned char **buffer) {
    struct input_state *state = opaque;
    if (state->used)
        return 0;
    state->used = 1;
    *buffer = state->data;
    return state->length;
}

static int output_chunk(void *opaque, unsigned char *buffer, unsigned int length) {
    struct output_state *state = opaque;
    if (length > state->capacity - state->length)
        return 1;
    memcpy(state->data + state->length, buffer, length);
    state->length += length;
    return 0;
}

int main(void) {
    static const unsigned char input[] = "td zlib round trip";
    if (strcmp(zlibVersion(), ZLIB_VERSION) != 0 ||
        strcmp(ZLIB_VERSION, "1.3.1") != 0)
        return fail("zlib version/header mismatch", 1);
    unsigned char compressed[128];
    unsigned char output[128];
    uLongf compressed_len = sizeof(compressed);
    uLongf output_len = sizeof(output);
    if (compress2(compressed, &compressed_len, input, sizeof(input),
            Z_BEST_COMPRESSION) != Z_OK)
        return fail("zlib memory compression failed", 2);
    if (uncompress(output, &output_len, compressed, compressed_len) != Z_OK ||
        output_len != sizeof(input) || memcmp(input, output, sizeof(input)) != 0)
        return fail("zlib memory round trip failed", 3);

    gzFile file = gzopen("zlib-test.gz", "wb9");
    if (file == NULL || gzwrite(file, input, sizeof(input)) != (int)sizeof(input) ||
        gzclose(file) != Z_OK)
        return fail("zlib gzip write failed", 4);
    memset(output, 0, sizeof(output));
    file = gzopen("zlib-test.gz", "rb");
    if (file == NULL || gzread(file, output, sizeof(input)) != (int)sizeof(input) ||
        gzclose(file) != Z_OK || memcmp(input, output, sizeof(input)) != 0)
        return fail("zlib gzip read round trip failed", 5);

    z_stream encoder = {0};
    unsigned char raw[128];
    if (deflateInit2(&encoder, Z_BEST_COMPRESSION, Z_DEFLATED, -MAX_WBITS,
            8, Z_DEFAULT_STRATEGY) != Z_OK)
        return fail("zlib raw deflate initialization failed", 6);
    encoder.next_in = (unsigned char *)input;
    encoder.avail_in = sizeof(input);
    encoder.next_out = raw;
    encoder.avail_out = sizeof(raw);
    if (deflate(&encoder, Z_FINISH) != Z_STREAM_END) {
        deflateEnd(&encoder);
        return fail("zlib raw deflate failed", 7);
    }
    unsigned int raw_length = sizeof(raw) - encoder.avail_out;
    if (deflateEnd(&encoder) != Z_OK)
        return fail("zlib raw deflate finalization failed", 8);

    z_stream decoder = {0};
    unsigned char window[1U << MAX_WBITS];
    memset(output, 0, sizeof(output));
    struct input_state source = {raw, raw_length, 0};
    struct output_state sink = {output, 0, sizeof(output)};
    if (inflateBackInit(&decoder, MAX_WBITS, window) != Z_OK)
        return fail("zlib inflateBack initialization failed", 9);
    int result = inflateBack(&decoder, input_chunk, &source, output_chunk, &sink);
    if (inflateBackEnd(&decoder) != Z_OK || result != Z_STREAM_END ||
        sink.length != sizeof(input) || memcmp(input, output, sizeof(input)) != 0)
        return fail("zlib inflateBack round trip failed", 10);
    return 0;
}
"#;

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
        Step::run("{src}", &["make", "-j{jobs}", "libz.a", "libz.so.1.3.1"])
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
        files: vec!["{src}/libz.a".into(), "{src}/libz.so.1.3.1".into()],
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
    steps.extend(archive_checks());
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

/// Every archive member is an x86-64 ELF64 object with a line table whose
/// compile directory is remapped, no byte of the archive names a build
/// directory or foreign store, and the archive links statically, from td
/// glibc and gcc-runtime (proved from the link map), into `ARCHIVE_TEST`.
/// The member scan reads `.debug_info`'s unit attributes; header line rows
/// keep `/td-build-root/sysroot/include`, a canonical remap root.
fn archive_checks() -> Vec<Step> {
    let members = "set -e; ar x '{out}/lib/libz.a'; objects=0; adler=0; \
                   for o in *.o; do objects=$((objects+1)); \
                   readelf -S \"$o\" | grep -Fq ' .debug_line ' || { echo \"libz.a: $o has no line table\" >&2; exit 1; }; \
                   info=$(readelf --debug-dump=info \"$o\"); \
                   printf '%s\\n' \"$info\" | grep -Eq 'DW_AT_comp_dir.*: /td-build$' || { echo \"libz.a: $o does not use /td-build\" >&2; exit 1; }; \
                   case $info in *adler32.c*) adler=1;; esac; \
                   case $info in */td-build-root*|*/gnu/store*|*guix-build*) echo \"libz.a: $o retains a build path\" >&2; exit 1;; esac; \
                   h=$(readelf -h \"$o\"); \
                   case $h in *ELF64*) ;; *) echo \"libz.a: $o is not ELF64\" >&2; exit 1;; esac; \
                   case $h in *'X86-64'*) ;; *) echo \"libz.a: $o is not x86-64\" >&2; exit 1;; esac; \
                   done; \
                   [ $objects -gt 0 ] && [ $adler = 1 ] || { echo 'libz.a lacks its objects' >&2; exit 1; }; \
                   for p in '{root}' '{src}' /gnu/store; do \
                   if grep -a -Fq \"$p\" '{out}/lib/libz.a'; then echo \"libz.a names $p\" >&2; exit 1; fi; done";
    let link = format!(
        "set -e; '{{root}}/wb/cc' -O2 -I'{{out}}/include' -c -o zlib-test.o zlib-test.c; \
         '{{root}}/wb/cc' -static -B{RUNTIME}/ \
         -Wl,-Map='{{root}}/archive-test/zlib-test.map' -o zlib-test zlib-test.o libz.a; \
         ./zlib-test; \
         case $(readelf -l zlib-test) in *INTERP*) echo 'zlib-test has an interpreter' >&2; exit 1;; esac"
    );
    vec![
        Step::MkDir {
            path: "{root}/archive".into(),
        },
        Step::run("{root}/archive", &[SH, "-c", members]).env("PATH", PATH),
        Step::MkDir {
            path: "{root}/archive-test".into(),
        },
        Step::WriteFile {
            path: "{root}/archive-test/zlib-test.c".into(),
            content: ARCHIVE_TEST.into(),
            exec: false,
        },
        // A copy under the build directory, which the map check admits.
        Step::CopyFiles {
            files: vec!["{out}/lib/libz.a".into()],
            dest: "{root}/archive-test".into(),
        },
        Step::run("{root}/archive-test", &[SH, "-c", &link]).env("PATH", PATH),
        build_root_link_map_check(
            "{root}/archive-test",
            "{root}/archive-test/zlib-test.map",
            "zlib-test",
            &["gcc-runtime"],
        ),
    ]
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

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    #[test]
    fn the_archive_is_built_installed_and_checked_before_the_split() {
        let steps = recipe().steps.unwrap_or_default();
        let made = steps
            .iter()
            .position(|step| {
                matches!(step, Step::Run { argv, .. }
                    if argv.iter().any(|arg| arg == "libz.a")
                        && argv.iter().any(|arg| arg == "libz.so.1.3.1"))
            })
            .expect("make builds both libraries");
        let installed = steps
            .iter()
            .position(|step| {
                matches!(step, Step::CopyFiles { files, dest }
                    if dest == "{out}/lib" && files.iter().any(|file| file == "{src}/libz.a"))
            })
            .expect("libz.a is installed");
        let scanned = steps
            .iter()
            .position(|step| {
                matches!(step, Step::Run { argv, .. }
                    if argv.iter().any(|arg| arg.contains("ar x '{out}/lib/libz.a'")))
            })
            .expect("member scan");
        let mapped = steps
            .iter()
            .position(|step| {
                matches!(step, Step::Run { argv, .. }
                    if argv.iter().any(|arg| arg.contains("zlib-test.map'; fail()")))
            })
            .expect("link-map check");
        let split = steps
            .iter()
            .position(|step| matches!(step, Step::SplitDebugTree { .. }))
            .expect("debug split");
        assert!(made < installed && installed < scanned && scanned < mapped && mapped < split);
    }
}
