use crate::shared::{target_rustc, target_rustc_linked_by};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// td-link-test: every output kind td-link links, with no GCC or binutils in
// the inputs and no PATH but one holding a `cc` that is td-link. A proc-macro
// dylib, which rustc then loads and runs; a cdylib; and one program as a
// dynamic PIE that needs the cdylib, a static ET_EXEC and a static PIE. Each
// program reads its own ELF, the cdylib's and the proc macro's and checks
// their type, interpreter (td glibc's loader, or none), run path (td glibc's,
// as DT_RPATH) and needed libraries (td glibc's, never libgcc_s), that LLD
// linked them, that an unwind passes through gcc-runtime's unwinder, that a
// thread runs and that the proc macro expanded (td-link/DESIGN.md). The static
// ET_EXEC is linked once more as the image links a plain rustc: through `cc`,
// rustc's default linker name, found on a PATH whose `cc` is td-link; it must
// run and be byte-identical.
const MACRO_RS: &str = include_str!("../probes/td_link_macro.rs");
const CDYLIB_RS: &str = include_str!("../probes/td_link_cdylib.rs");
const PROGRAM_RS: &str = include_str!("../probes/td_link_program.rs");

const RUSTC: &str = "{in:rust-toolchain}/bin/rustc";
const INTERP: &str = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64/lib/ld-linux-x86-64.so.2";
const KINDS: &str =
    "--check-cfg=cfg(td_link_kind,values(\"dynamic_pie\",\"static_exec\",\"static_pie\"))";

fn rustc(args: &[&str]) -> Step {
    target_rustc("{src}", RUSTC, args)
        .env("PATH", "/nonexistent")
        .env("SOURCE_DATE_EPOCH", "1")
}

pub fn recipe() -> Recipe {
    let mut steps = Vec::new();
    for path in ["{out}", "{root}/lib", "{root}/bin", "{root}/cc"] {
        steps.push(Step::MkDir { path: path.into() });
    }
    for (name, content) in [
        ("td_link_macro.rs", MACRO_RS),
        ("td_link_cdylib.rs", CDYLIB_RS),
        ("td_link_program.rs", PROGRAM_RS),
    ] {
        steps.push(Step::WriteFile {
            path: format!("{{src}}/{name}"),
            content: content.into(),
            exec: false,
        });
    }
    steps.push(rustc(&[
        "--edition=2021",
        "--crate-type=proc-macro",
        // What Cargo passes a proc-macro crate: rustc does not add the
        // sysroot's proc_macro to the extern prelude by itself.
        "--extern",
        "proc_macro",
        "-o",
        "{root}/lib/libtd_link_macro.so",
        "{src}/td_link_macro.rs",
    ]));
    steps.push(rustc(&[
        "--edition=2021",
        "--crate-type=cdylib",
        "-o",
        "{root}/lib/libtd_link_cdylib.so",
        "{src}/td_link_cdylib.rs",
    ]));
    // The static programs run first: they load no shared object, so their
    // checks of the cdylib and proc macro report even when the dynamic
    // program could not be loaded.
    for (kind, flags) in [
        (
            "static_exec",
            &["-Ctarget-feature=+crt-static", "-Crelocation-model=static"][..],
        ),
        ("static_pie", &["-Ctarget-feature=+crt-static"]),
        (
            "dynamic_pie",
            &[
                // Needed by path: it exports nothing an --as-needed link keeps.
                "-Clink-arg=-Wl,--no-as-needed",
                "-Clink-arg={root}/lib/libtd_link_cdylib.so",
            ],
        ),
    ] {
        let cfg = format!("td_link_kind=\"{kind}\"");
        let binary = format!("{{root}}/bin/{kind}");
        let mut args = vec![
            "--edition=2021",
            "--crate-name=td_link_program",
            KINDS,
            "--cfg",
            &cfg,
            "--extern",
            "td_link_macro={root}/lib/libtd_link_macro.so",
        ];
        args.extend_from_slice(flags);
        args.extend_from_slice(&["-o", &binary, "{src}/td_link_program.rs"]);
        steps.push(rustc(&args).env("TD_LINK_TEST_INTERP", INTERP));
        // Each run also checks the two shared objects td-link linked.
        let run = [
            binary.as_str(),
            binary.as_str(),
            "{root}/lib/libtd_link_cdylib.so",
            "{root}/lib/libtd_link_macro.so",
        ];
        steps.push(Step::run("{root}", &run).env("PATH", "/nonexistent"));
    }
    // The image's /bin/cc is td-link. rustc resolves `cc` on PATH and
    // infers its linker flavour from that name, so this link is the one a
    // plain `rustc` or `cargo build` on a td system makes.
    steps.push(Step::Symlink {
        target: "{in:td-link}/bin/td-link".into(),
        link: "{root}/cc/cc".into(),
    });
    steps.push(
        target_rustc_linked_by(
            "{src}",
            RUSTC,
            &[
                "--edition=2021",
                "--crate-name=td_link_program",
                KINDS,
                "--cfg",
                "td_link_kind=\"static_exec\"",
                "--extern",
                "td_link_macro={root}/lib/libtd_link_macro.so",
                "-Ctarget-feature=+crt-static",
                "-Crelocation-model=static",
                "-o",
                "{root}/bin/static_exec_cc",
                "{src}/td_link_program.rs",
            ],
            "cc",
        )
        .env("PATH", "{root}/cc")
        .env("SOURCE_DATE_EPOCH", "1")
        .env("TD_LINK_TEST_INTERP", INTERP),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                "{root}/bin/static_exec_cc",
                "{root}/bin/static_exec_cc",
                "{root}/lib/libtd_link_cdylib.so",
                "{root}/lib/libtd_link_macro.so",
            ],
        )
        .env("PATH", "/nonexistent"),
    );
    steps.push(Step::CompareFiles {
        left: "{root}/bin/static_exec".into(),
        right: "{root}/bin/static_exec_cc".into(),
    });
    steps.push(Step::assert_static(&[
        "{root}/bin/static_exec",
        "{root}/bin/static_pie",
        "{root}/bin/static_exec_cc",
    ]));
    steps.push(Step::WriteFile {
        path: "{out}/td-link-test".into(),
        content: "dynamic PIE, static ET_EXEC, static PIE, proc-macro dylib, cdylib, cc\n".into(),
        exec: false,
    });

    Recipe::mesboot("td-link-test", "1.0")
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-link-test: td-link links a dynamic PIE, a static ET_EXEC, a static PIE, a proc-macro dylib and a cdylib with no GCC or binutils"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-link-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
