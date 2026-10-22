use crate::shared::{BUILD_ROOT_PATH, BUILD_ROOT_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// The Debian build root (AGENTS.md) as a sandbox root: its gcc
// compiles a freestanding program that makes its two syscalls itself, so the
// output links nothing of the root, and the build runs it. The output gate
// (build_root.rs) refuses an interpreter or RUNPATH outside the store; this
// check also asserts the ELF is static and carries no interpreter.
const HELLO: &str = "static long sys3(long n, long a, long b, long c)\n\
                     {\n\
                     \x20 long r;\n\
                     \x20 __asm__ volatile(\"syscall\" : \"=a\"(r)\n\
                     \x20                  : \"a\"(n), \"D\"(a), \"S\"(b), \"d\"(c)\n\
                     \x20                  : \"rcx\", \"r11\", \"memory\");\n\
                     \x20 return r;\n\
                     }\n\
                     \n\
                     void _start(void)\n\
                     {\n\
                     \x20 static const char msg[] = \"hello from the build root\\n\";\n\
                     \x20 sys3(1, 1, (long)msg, sizeof msg - 1);\n\
                     \x20 sys3(60, 0, 0, 0);\n\
                     \x20 for (;;) {\n\
                     \x20 }\n\
                     }\n";

pub fn recipe() -> Recipe {
    let script = "set -e; \
                  v=$(gcc --version); \
                  case $v in *' 14.2.0'*) ;; *) echo \"gcc: $v\" >&2; exit 1;; esac; \
                  out=$('{out}/bin/hello'); \
                  [ \"$out\" = 'hello from the build root' ] || { echo \"hello: $out\" >&2; exit 1; }; \
                  h=$(readelf -h -l '{out}/bin/hello'); \
                  case $h in *'EXEC (Executable file)'*) ;; *) echo 'hello is not ET_EXEC' >&2; exit 1;; esac; \
                  case $h in *INTERP*) echo 'hello has an interpreter' >&2; exit 1;; esac";
    let steps = vec![
        Step::MkDir {
            path: "{root}/test".into(),
        },
        Step::WriteFile {
            path: "{root}/test/hello.c".into(),
            content: HELLO.into(),
            exec: false,
        },
        Step::MkDir {
            path: "{out}/bin".into(),
        },
        Step::run(
            "{root}/test",
            &[
                "/usr/bin/gcc",
                "-O2",
                "-ffreestanding",
                "-fno-stack-protector",
                "-fno-asynchronous-unwind-tables",
                "-nostdlib",
                "-static",
                "-no-pie",
                "-o",
                "{out}/bin/hello",
                "hello.c",
            ],
        )
        .env("PATH", BUILD_ROOT_PATH),
        Step::run("{root}/test", &[BUILD_ROOT_SH, "-c", script]).env("PATH", BUILD_ROOT_PATH),
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: the Debian build root's gcc 14.2.0 built a freestanding static \
                      x86-64 program that ran in the root\n"
                .into(),
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/result".into(), "{out}/bin/hello".into()],
            exec: false,
        },
    ];

    Recipe::mesboot("build-root-hello-test", "1.0")
        .build_root()
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check build-root-hello-test: compile and run a freestanding program with the Debian build root's gcc"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run build-root-hello-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
