use crate::ladder::{mesboot0_inputs, mesboot0_path, unpack_keep_top, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// elfutils-x86-64-test: behavioral validation of the source-built static libelf
// (#529). The kernel rung needs objtool to LINK against libelf.a — and the
// static libelf.a is the tricky part: it is not self-contained like the shipped
// .so, so the link must pull libelf.a + libeu.a + libz.a in that order. A
// missing libeu.a (eu_tsearch) or a broken zlib bundle would fail objtool deep
// in the kernel build. So this rung reproduces exactly that link with td's own
// native toolchain: it compiles a program that opens an ELF via libelf and calls
// the SAME APIs objtool uses (elf_getdata/elf_strptr/gelf_getshdr — the paths
// that pull libelf's decompression object and libeu's search tree), plus an
// explicit zlibVersion(), links it `-lelf -leu -lz` against the recipe's
// archives, RUNS it against a real ELF (its own binary), and asserts it read
// section data. Because these are STATIC archives, calling those APIs is what
// forces libeu.a + libz.a members into the link — a bare elf_getshdrnum() would
// leave them unreferenced and let an empty/broken libz.a pass (Codex review). If
// the static archives don't link or don't work, this reds here — cheaply —
// instead of only in the full vmlinux build. Mirrors make-test / busybox-test
// (build the producer, run its output) using the make-x86-64 wrapper shape.
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
    // The program deliberately exercises the SAME libelf APIs objtool uses, so the
    // STATIC link genuinely REQUIRES members from all three archives (a bare
    // elf_getshdrnum() would leave `-leu`/`-lz` unreferenced, and the linker would
    // silently accept an empty/broken libz.a — Codex review). elf_getdata() drags
    // in libelf's decompression path (an unconditional relocation to
    // __libelf_decompress_elf in elf_compress.o → zlib's inflate*), elf_strptr()
    // over the section-name table exercises libeu's search tree, and an explicit
    // zlibVersion() from the bundled zlib.h makes libz.a a HARD link requirement
    // that also proves it is a working archive, not just present. Runs on its own
    // ELF (argv[0]) and asserts it actually read section data.
    steps.push(Step::WriteFile {
        path: "{root}/t.c".into(),
        content: "#include <libelf.h>\n#include <gelf.h>\n#include <zlib.h>\n#include <fcntl.h>\n#include <stdio.h>\n\
                  int main(int argc, char **argv) {\n\
                  \t(void)argc;\n\
                  \tif (elf_version(EV_CURRENT) == EV_NONE) { fprintf(stderr, \"elf_version\\n\"); return 1; }\n\
                  \tint fd = open(argv[0], O_RDONLY);\n\
                  \tif (fd < 0) { perror(\"open\"); return 2; }\n\
                  \tElf *e = elf_begin(fd, ELF_C_READ, (Elf *)0);\n\
                  \tif (!e) { fprintf(stderr, \"elf_begin: %s\\n\", elf_errmsg(-1)); return 3; }\n\
                  \tsize_t nsec = 0;\n\
                  \tif (elf_getshdrnum(e, &nsec) != 0) { fprintf(stderr, \"getshdrnum\\n\"); return 4; }\n\
                  \tsize_t shstrndx = 0;\n\
                  \tif (elf_getshdrstrndx(e, &shstrndx) != 0) { fprintf(stderr, \"getshdrstrndx\\n\"); return 5; }\n\
                  \tsize_t withdata = 0;\n\
                  \tElf_Scn *scn = (Elf_Scn *)0;\n\
                  \twhile ((scn = elf_nextscn(e, scn)) != (Elf_Scn *)0) {\n\
                  \t\tGElf_Shdr shdr;\n\
                  \t\tif (gelf_getshdr(scn, &shdr) != &shdr) { fprintf(stderr, \"gelf_getshdr\\n\"); return 6; }\n\
                  \t\t(void)elf_strptr(e, shstrndx, shdr.sh_name);\n\
                  \t\tif (elf_getdata(scn, (Elf_Data *)0) != (Elf_Data *)0) withdata++;\n\
                  \t}\n\
                  \tif (withdata == 0) { fprintf(stderr, \"no section data — elf_getdata path not exercised\\n\"); return 7; }\n\
                  \tprintf(\"libelf OK: %zu sections, %zu with data, zlib %s\\n\", nsec, withdata, zlibVersion());\n\
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
                 printf '%s\\n' \"$out\" | grep -q '^libelf OK: [0-9][0-9]* sections, [0-9][0-9]* with data, zlib [0-9]' || { echo \"libelf program gave unexpected output: '$out'\" >&2; exit 1; }",
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
        content: "PASS: elfutils 0.192 static libelf.a + libeu.a + libz.a, source-built by the native /td/store x86_64 toolchain, links and runs the objtool libelf APIs (elf_getdata decompression path + zlibVersion), forcing every archive into the link\n".into(),
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
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run elfutils-x86-64-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
