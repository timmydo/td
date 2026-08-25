use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    let sgcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let zlib = "{in:zlib-x86-64-self}";
    let path = format!("{{tools}}:{sbin}:{}", post_bootstrap_path());

    let mut steps = vec![
        Step::MkDir {
            path: "{root}/archive".into(),
        },
        Step::run(
            "{root}/archive",
            &[
                "{in:binutils-x86-64-self}/bin/ar",
                "x",
                &format!("{zlib}/lib/libz.a"),
            ],
        ),
        Step::run(
            "{root}/archive",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                "objects=0; found_adler=0; \
                 for object in *.o; do \
                     test -f \"$object\" || continue; objects=$((objects+1)); \
                     info=$('{in:binutils-x86-64-self}/bin/readelf' --debug-dump=info \"$object\") || exit 1; \
                     sections=$('{in:binutils-x86-64-self}/bin/readelf' -S \"$object\") || exit 1; \
                     printf '%s\\n' \"$sections\" | grep -Eq '^[[:space:]]*\\[[[:space:]]*[0-9]+\\][[:space:]]+\\.debug_line[[:space:]]' || { echo \"zlib member $object has no line table\" >&2; exit 1; }; \
                     printf '%s\\n' \"$info\" | grep -Eq 'DW_AT_comp_dir.*: /td-build$' || { echo \"zlib member $object does not use /td-build\" >&2; exit 1; }; \
                     if printf '%s\\n' \"$info\" | grep -Fq 'adler32.c'; then found_adler=1; fi; \
                     header=$('{in:binutils-x86-64-self}/bin/readelf' -h \"$object\") || exit 1; \
                     printf '%s\\n' \"$header\" | grep -Eq 'Class:[[:space:]]+ELF64' || { echo \"zlib member $object is not ELF64\" >&2; exit 1; }; \
                     printf '%s\\n' \"$header\" | grep -Eq 'Machine:[[:space:]]+Advanced Micro Devices X86-64' || { echo \"zlib member $object is not x86-64\" >&2; exit 1; }; \
                     if printf '%s\\n' \"$info\" | grep -Eq 'guix-build|/gnu/store|/td-build-root'; then echo \"zlib member $object retains a foreign build path\" >&2; exit 1; fi; \
                 done; \
                 test \"$objects\" -gt 0 || { echo 'libz.a has no objects' >&2; exit 1; }; \
                 test \"$found_adler\" = 1 || { echo 'libz.a lacks adler32.c' >&2; exit 1; }",
            ],
        )
        .env("PATH", &path),
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                "if grep -a -Fq '/gnu/store' '{in:zlib-x86-64-self}/lib/libz.a'; then echo 'zlib archive retains a foreign store reference' >&2; exit 1; fi",
            ],
        )
        .env("PATH", &path),
    ];
    steps.push(Step::WriteFile {
        path: "{root}/zlib-test.c".into(),
        content: r#"#include <zlib.h>
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
"#
        .into(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                sgcc,
                "-isystem",
                &format!("{xglibc}/include"),
                "-B",
                &format!("{sbin}/"),
                "-B",
                &format!("{xglibc}/lib"),
                "-L",
                &format!("{xglibc}/lib"),
                "-static-libgcc",
                "-Wl,--dynamic-linker",
                &format!("-Wl,{xglibc}/lib/ld-linux-x86-64.so.2"),
                "-Wl,--enable-new-dtags",
                "-Wl,-rpath",
                &format!("-Wl,{xglibc}/lib"),
                &format!("-I{zlib}/include"),
                "-o",
                "{root}/zlib-test",
                "{root}/zlib-test.c",
                &format!("{zlib}/lib/libz.a"),
            ],
        )
        .env("PATH", &path),
    );
    steps.push(Step::run("{root}", &["{root}/zlib-test"]));
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: zlib 1.3.1 covers memory, gzip-file, and inflateBack APIs\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("zlib-x86-64-self-test", "1.0")
        .native_inputs(&[
            "zlib-x86-64-self",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check zlib-x86-64-self-test: build final-toolchain zlib and exercise memory, gzip-file, and inflateBack APIs"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run zlib-x86-64-self-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn validation_uses_only_final_zlib_and_toolchain_outputs() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                [
                    "zlib-x86-64-self",
                    "gcc-x86-64-self",
                    "binutils-x86-64-self",
                    "glibc-x86-64",
                    "busybox-x86-64",
                ]
                .map(str::to_string)
                .as_slice()
            )
        );
        assert!(recipe.inputs.is_none());
    }
}
