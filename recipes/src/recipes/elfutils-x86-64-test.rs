use crate::ladder::{mesboot0_inputs, mesboot0_path, unpack_keep_top, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// elfutils-x86-64-test: behavioral validation of the source-built static libelf
// (#529). The kernel rung needs objtool to LINK against libelf.a — and the
// static libelf.a is the tricky part: it is not self-contained like the shipped
// .so, so the link must pull libelf.a + libeu.a + libz.a in that order. A
// missing libeu.a (eu_tsearch) or a broken zlib bundle would fail objtool deep
// in the kernel build. So this rung reproduces exactly that link with td's own
// native toolchain: it compiles a small program that opens an ELF via libelf
// (elf_begin/elf_getshdrnum — the paths that reference libeu's search tree),
// links it `-lelf -leu -lz` against the recipe's archives, RUNS it against a
// real ELF (its own binary), and asserts it reports the section count. If the
// static archives don't link or don't work, this reds here — cheaply — instead
// of only in the full vmlinux build. Mirrors make-test / busybox-test (build the
// producer, run its output) using the make-x86-64 native-cc-wrapper shape.
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let elf = "{in:elfutils-x86-64}";
    let cip = format!("{xglibc}/include:{{root}}/kh");

    let mut steps = unpack_keep_top("linux-headers-x86-64", "{root}/kh");
    // Static native-cc wrapper that also sees the recipe's libelf headers + libs.
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{SH}\nexec \"{ngcc}\" -static -B{xglibc}/lib -I{elf}/include -L{elf}/lib \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/t.c".into(),
        content: "#include <libelf.h>\n#include <gelf.h>\n#include <fcntl.h>\n#include <stdio.h>\n\
                  int main(int argc, char **argv) {\n\
                  \tif (elf_version(EV_CURRENT) == EV_NONE) { fprintf(stderr, \"elf_version\\n\"); return 1; }\n\
                  \tint fd = open(argv[0], O_RDONLY);\n\
                  \tif (fd < 0) { perror(\"open\"); return 2; }\n\
                  \tElf *e = elf_begin(fd, ELF_C_READ, (Elf *)0);\n\
                  \tif (!e) { fprintf(stderr, \"elf_begin: %s\\n\", elf_errmsg(-1)); return 3; }\n\
                  \tsize_t n = 0;\n\
                  \tif (elf_getshdrnum(e, &n) != 0) { fprintf(stderr, \"getshdrnum\\n\"); return 4; }\n\
                  \tprintf(\"libelf OK: %zu sections\\n\", n);\n\
                  \telf_end(e);\n\
                  \treturn 0;\n\
                  }\n"
            .into(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "'{root}/wb/cc' -o '{root}/t.out' '{root}/t.c' -lelf -leu -lz || { echo 'linking a libelf program against the static libelf.a + libeu.a + libz.a failed' >&2; exit 1; }; \
                 out=$('{root}/t.out') || { echo 'the libelf test program crashed at runtime' >&2; exit 1; }; \
                 printf '%s\\n' \"$out\" | grep -q '^libelf OK: [0-9][0-9]* sections$' || { echo \"libelf program gave unexpected output: '$out'\" >&2; exit 1; }",
            ],
        )
        .env("PATH", &mesboot0_path())
        .env("C_INCLUDE_PATH", &cip),
    );

    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: elfutils 0.192 static libelf.a + libeu.a + libz.a, source-built by the native /td/store x86_64 toolchain, links and runs (the objtool linkage the kernel needs)\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("elfutils-x86-64-test", "1.0")
        .native_inputs(&[
            "elfutils-x86-64",
            "gcc-x86-64-native",
            "binutils-x86-64-native",
            "glibc-x86-64",
        ])
        .inputs_owned(mesboot0_inputs(&["linux-headers-x86-64"]))
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check elfutils-x86-64-test: build-plan --auto builds elfutils-x86-64 (static libelf.a + libeu.a + libz.a, source-built by the native /td/store x86_64 toolchain) and links+runs a libelf program against it"
: "${TD_RECIPE_EVAL:=$PWD/recipes/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run elfutils-x86-64-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
