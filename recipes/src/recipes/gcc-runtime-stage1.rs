use super::gcc_runtime::{gcc_configure, gcc_make, gcc_source, search_probe, TARGET_CFLAGS};
use super::glibc_x86_64::{glibc_configure, KERNEL_HEADER_DIRS, PREFIX as GLIBC_PREFIX};
use crate::shared::{unpack_into, unpack_keep_top, BUILD_ROOT_PATH as PATH, BUILD_ROOT_SH as SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// gcc-runtime-stage1 is the static GCC 14.3.0 runtime glibc-x86-64 links:
// libgcc.a, libgcc_eh.a and the crtbegin/crtend objects, built in the
// Debian build root before td glibc exists. glibc's own link needs a
// libgcc, and gcc-runtime is built against glibc, so this breaks the cycle
// without the root's libgcc: glibc's configure, run with Debian's gcc,
// installs td glibc's headers alone, and gcc-runtime's compiler proper
// builds libgcc's static parts against them. Nothing that needs a libc is
// linked, so there is no libgcc_s. gcc-runtime is then rebuilt against the
// glibc this links.
const PREFIX: &str = "/gcc-runtime-stage1";
const LIBGCC: &str = "{src}/bld/x86_64-pc-linux-gnu/libgcc";

/// The output: what glibc's links take from gcc's search path. Its shared
/// libraries link `-static-libgcc`, which names libgcc_eh.a too.
pub(crate) const OBJECTS: &[&str] = &[
    "libgcc.a",
    "libgcc_eh.a",
    "crtbegin.o",
    "crtbeginS.o",
    "crtbeginT.o",
    "crtend.o",
    "crtendS.o",
];

pub fn recipe() -> Recipe {
    let mut steps = gcc_source("gcc-runtime-stage1-source");
    steps.extend(glibc_headers());
    steps.extend(gcc_configure(PREFIX));
    steps.push(gcc_make(true, &[TARGET_CFLAGS, "all-gcc"]));
    steps.push(gcc_make(false, &[TARGET_CFLAGS, "configure-target-libgcc"]));
    steps.push(search_probe(PREFIX, false));
    // libgcc's own goals, not `all`, which links libgcc_s against a libc.
    let mut make = vec![
        "/usr/bin/make".to_string(),
        "-j{jobs}".to_string(),
        format!("SHELL={SH}"),
    ];
    make.extend(OBJECTS.iter().map(|object| (*object).to_string()));
    steps.push(Step::Run {
        argv: make,
        env: vec![
            ("PATH".into(), PATH.into()),
            ("CONFIG_SHELL".into(), SH.into()),
        ],
        dir: LIBGCC.into(),
    });
    steps.push(Step::MkDir {
        path: "{out}/lib".into(),
    });
    steps.push(Step::CopyFiles {
        files: OBJECTS.iter().map(|f| format!("{LIBGCC}/{f}")).collect(),
        dest: "{out}/lib".into(),
    });
    steps.push(Step::Require {
        paths: OBJECTS.iter().map(|f| format!("{{out}}/lib/{f}")).collect(),
        exec: false,
    });
    steps.push(object_checks());

    Recipe::mesboot("gcc-runtime-stage1", "14.3.0")
        .source_input("gcc-14-source")
        .build_root()
        .inputs(&[
            "gmp63",
            "mpfr421",
            "mpc131",
            "glibc-241-source",
            "linux-headers-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check gcc-runtime-stage1: build GCC 14.3.0's static libgcc in the Debian build root against td glibc's headers alone"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run gcc-runtime-stage1 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

/// A sysroot of td glibc's installed headers and the kernel's: glibc's
/// configure, as glibc-x86-64 runs it, then `make install-headers`. The
/// full install generates `gnu/stubs.h`; an empty one stands in.
fn glibc_headers() -> Vec<Step> {
    let mut steps = unpack_into("glibc-241-source", "{root}/glibc");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(Step::MkDir {
        path: "{root}/glibc/bld".into(),
    });
    steps.push(glibc_configure(
        "{root}/glibc",
        "{root}/glibc/bld",
        "/usr/bin/gcc",
        "-O2",
    ));
    steps.push(
        Step::run(
            "{root}/glibc/bld",
            &[
                "/usr/bin/make",
                &format!("SHELL={SH}"),
                "install-headers",
                "DESTDIR={root}/hdr",
            ],
        )
        .env("PATH", PATH)
        .env("CONFIG_SHELL", SH),
    );
    steps.push(Step::CopyTree {
        from: format!("{{root}}/hdr{GLIBC_PREFIX}/include"),
        dest: "{root}/sysroot/include".into(),
    });
    for dir in KERNEL_HEADER_DIRS {
        steps.push(Step::CopyTree {
            from: format!("{{root}}/kh/{dir}"),
            dest: format!("{{root}}/sysroot/include/{dir}"),
        });
    }
    steps.push(Step::WriteFile {
        path: "{root}/sysroot/include/gnu/stubs.h".into(),
        content: String::new(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec![
            "{root}/sysroot/include/features.h".into(),
            "{root}/sysroot/include/link.h".into(),
            "{root}/sysroot/include/linux/limits.h".into(),
        ],
        exec: false,
    });
    steps
}

/// Every object is x86-64, and every one holding a function (global, local,
/// weak or ifunc) was compiled by the GCC 14.3.0 this recipe builds, never
/// the root's 14.2.0, under the target profile with a remapped line table (hand-written assembly,
/// the profiler's named boundary, only remapped); no byte of the output
/// names the build.
fn object_checks() -> Step {
    let script = "set -e; mkdir -p '{root}/objects'; cd '{root}/objects'; \
                  ar x '{out}/lib/libgcc.a'; ar x '{out}/lib/libgcc_eh.a'; cp '{out}'/lib/crt*.o .; \
                  n=0; c=0; bad=; asm=; \
                  for o in *.o; do n=$((n+1)); \
                  h=$(readelf -h \"$o\"); \
                  case $h in *ELF64*'X86-64'*) ;; *) bad=\"$bad $o:arch\"; continue;; esac; \
                  nm --defined-only \"$o\" | grep -Eq ' [TtWi] ' || continue; c=$((c+1)); \
                  readelf -S \"$o\" | grep -Fq ' .debug_line ' || { bad=\"$bad $o:lines\"; continue; }; \
                  info=$(readelf --debug-dump=info \"$o\"); \
                  producers=$(printf '%s\\n' \"$info\" | grep 'DW_AT_producer' || true); \
                  if [ -z \"$producers\" ]; then bad=\"$bad $o:no-producer\"; \
                  elif printf '%s\\n' \"$producers\" | grep -Fq ' 14.2'; then bad=\"$bad $o:debian-gcc\"; \
                  elif printf '%s\\n' \"$producers\" | grep -q 'GNU AS '; then \
                  printf '%s\\n' \"$producers\" | grep -vq 'GNU AS ' && bad=\"$bad $o:mixed\"; \
                  asm=\"$asm $o\"; \
                  elif printf '%s\\n' \"$producers\" | grep -Ev 'GNU C[0-9]* 14\\.3\\.0 ' | grep -q .; then \
                  bad=\"$bad $o:producer\"; \
                  elif printf '%s\\n' \"$producers\" | grep -Fv -- '-fno-omit-frame-pointer' | grep -q .; then \
                  bad=\"$bad $o:frame-pointers\"; fi; \
                  printf '%s\\n' \"$info\" | grep -Eq 'DW_AT_comp_dir.*: /td-build(/|$)' || \
                  bad=\"$bad $o:comp-dir\"; \
                  done; \
                  echo \"objects: $n, with code: $c, assembly:$asm\"; \
                  [ -z \"$bad\" ] || { echo \"objects failing the profile:$bad\" >&2; exit 1; }; \
                  [ $c -gt 100 ] || { echo \"only $c objects hold code\" >&2; exit 1; }; \
                  for p in '{root}' '{src}'; do \
                  if grep -r -a -Fq \"$p\" '{out}'; then echo \"the output names $p\" >&2; exit 1; fi; done";
    Step::run("{root}", &[SH, "-c", script]).env("PATH", PATH)
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    /// The runtime is built from td glibc's headers in the root and no
    /// libc: no glibc output, no ladder input, and no libgcc goal that
    /// would link one.
    #[test]
    fn the_stage1_runtime_needs_no_libc() {
        let recipe = recipe();
        assert!(recipe.build_root);
        assert_eq!(recipe.native_inputs, None);
        let inputs = recipe.inputs.clone().unwrap_or_default();
        assert!(inputs.iter().any(|input| input == "glibc-241-source"));
        assert!(!inputs.iter().any(|input| input.contains("mesboot")));
        let steps = recipe.steps.unwrap_or_default();
        let runs: Vec<&Vec<String>> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => Some(argv),
                _ => None,
            })
            .collect();
        assert!(runs
            .iter()
            .any(|argv| argv.iter().any(|arg| arg == "install-headers")));
        for goal in ["all-target-libgcc", "all", "libgcc_s.so", "libgcc_s.so.1"] {
            assert!(
                !runs.iter().any(|argv| argv.iter().any(|arg| arg == goal)),
                "{goal}"
            );
        }
        assert!(runs
            .iter()
            .any(|argv| argv.iter().any(|arg| arg == "libgcc.a")
                && argv.iter().any(|arg| arg == "crtbeginT.o")));
    }
}
