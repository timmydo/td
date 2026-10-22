use crate::shared::{
    relocate_ld_scripts, unpack_into, unpack_keep_top, BUILD_ROOT_PATH as PATH, BUILD_ROOT_SH as SH,
};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step, TextEdit};

// glibc 2.41 for x86_64, the shared and static libc every td program and
// the C packages link, installed at /td/store/glibc-2.41-x86_64 under
// DESTDIR={out}/stage. Built in the Debian build root: Debian's gcc
// compiles glibc's sources, which glibc itself builds -nostdinc against its
// own tree, the kernel's UAPI headers and gcc's freestanding headers, and
// Debian's ld links them with -nostdlib. The one library glibc's links take
// from the compiler, libgcc.a, and the crtbegin/crtend objects of its
// programs come from gcc-runtime-stage1, which the wrapper's -B puts ahead
// of Debian's; the wrapper records every link and has it write a map, and
// the map check proves no recorded link loaded a file from outside the
// build and gcc-runtime-stage1. The
// relocated ld scripts and the kernel headers copied into include/ are the
// staged tree's contract with gcc-runtime and the build-root C recipes.

/// glibc's install prefix, which every consumer names.
pub(crate) const PREFIX: &str = "/td/store/glibc-2.41-x86_64";
const STAGE: &str = "{out}/stage/td/store/glibc-2.41-x86_64";
const RUNTIME: &str = "{in:gcc-runtime-stage1}/lib";

/// The kernel UAPI trees copied into the staged include/.
pub(crate) const KERNEL_HEADER_DIRS: &[&str] = &[
    "linux",
    "asm",
    "asm-generic",
    "mtd",
    "rdma",
    "scsi",
    "sound",
    "video",
    "xen",
    "drm",
    "misc",
];

/// The links glibc makes that must take libgcc.a from gcc-runtime-stage1:
/// the shared libc, the loader's relocatable core, libm, and a static and
/// a dynamic program.
const LIBGCC_LINKS: &[&str] = &[
    "libc.so",
    "librtld.os",
    "libm.so",
    "ldconfig",
    "iconvconfig",
];

/// The compiler: Debian's gcc with gcc-runtime-stage1's libgcc and start
/// files first and the target profile after glibc's own flags. With
/// TD_LINK_MAPS set, every link is recorded there and writes a map: a
/// record `N.link` (from mktemp, so no two links share one) holds the
/// output as given to `-o`, the map's path and the directory the link ran
/// in, and the map is `N.map`. glibc's two rtld links pass their own
/// `-Map`, which is recorded in place of one; any other `-Map` on a link
/// is refused, as is a link with no `-o`, so no link goes unrecorded.
/// Compiles and queries (`-c`, `-S`, `-E`, `-M`, `-MM`, `-fsyntax-only`,
/// `-print-*`, `--print-*`, `-dump*`, `-v`, `--version`, `--help`,
/// `-###`) are not links. BUILD_CC, Debian's gcc unwrapped, builds only `make check`'s
/// annexc and isomac, which this build does not run.
const CC: &str = "#!/usr/bin/bash\n\
                  map=()\n\
                  if [ -n \"${TD_LINK_MAPS:-}\" ]; then\n\
                  \x20 out=; rec=; m=; next=0; mode=link; query=0; own=()\n\
                  \x20 for a in \"$@\"; do\n\
                  \x20   if [ $next = 1 ]; then out=$a; next=0; continue; fi\n\
                  \x20   case $a in\n\
                  \x20     -o) next=1;;\n\
                  \x20     -o*) out=${a#-o};;\n\
                  \x20     -c|-S|-E|-M|-MM|-fsyntax-only) mode=compile;;\n\
                  \x20     -print-*|--print-*|-dump*|-v|--version|--help|-###) query=1;;\n\
                  \x20     *-Map*) own+=(\"$a\");;\n\
                  \x20   esac\n\
                  \x20 done\n\
                  \x20 if [ $mode = link ]; then\n\
                  \x20   if [ -z \"$out\" ]; then\n\
                  \x20     [ $query = 1 ] || { echo \"td cc: refusing a link with no -o: $*\" >&2; exit 1; }\n\
                  \x20   else\n\
                  \x20     case ${#own[@]}:$out:${own[0]:-} in\n\
                  \x20       0:*) rec=$(mktemp \"$TD_LINK_MAPS/XXXXXXXXXX.link\") || exit 1\n\
                  \x20         m=${rec%.link}.map; map=(\"-Wl,-Map=$m\");;\n\
                  \x20       1:*/elf/librtld.map.o:\"-Wl,-Map,${out%.o}T\") m=${out%.o};;\n\
                  \x20       1:*/elf/librtld.os:\"-Wl,-Map,$out.map\") m=$out.map;;\n\
                  \x20       *) echo \"td cc: refusing a map td does not read: $out: ${own[*]}\" >&2; exit 1;;\n\
                  \x20     esac\n\
                  \x20     [ -n \"${rec:-}\" ] || { rec=$(mktemp \"$TD_LINK_MAPS/XXXXXXXXXX.link\") || exit 1; }\n\
                  \x20     printf '%s\\n' \"$out\" \"$m\" \"$PWD\" > \"$rec\" || exit 1\n\
                  \x20   fi\n\
                  \x20 fi\n\
                  fi\n\
                  exec /usr/bin/gcc -B{in:gcc-runtime-stage1}/lib/ \"${map[@]}\" \"$@\" \
                  -fno-omit-frame-pointer -g1 -ffile-prefix-map={root}=/td-build-root \
                  -ffile-prefix-map={src}=/td-build -Wl,--build-id=sha1\n";

/// glibc's configure of the tree at `source` in `dir`, with compiler `cc`
/// and `cflags`. The build triple is Debian's spelling, so configure treats
/// the build as a cross build: `make install` runs nothing it built (no
/// ldconfig cache, no gconv cache), as the ladder's cross build did not.
/// glibc is C: the root's g++, which would link Debian's libstdc++ into a
/// build-time support program, is refused, as the ladder had none.
pub(crate) fn glibc_configure(source: &str, dir: &str, cc: &str, cflags: &str) -> Step {
    let configure = format!("{source}/configure");
    let slibdir = format!("libc_cv_slibdir={PREFIX}/lib");
    let prefix = format!("--prefix={PREFIX}");
    Step::run(
        dir,
        &[
            SH,
            &configure,
            &prefix,
            "--build=x86_64-linux-gnu",
            "--host=x86_64-pc-linux-gnu",
            "--with-headers={root}/kh",
            "--enable-kernel=3.2.0",
            "--disable-werror",
            "--disable-nscd",
            "--without-selinux",
            &slibdir,
            "libc_cv_cxx_link_ok=no",
        ],
    )
    .env("PATH", PATH)
    .env("CONFIG_SHELL", SH)
    .env("CC", cc)
    .env("BUILD_CC", "/usr/bin/gcc")
    .env("CFLAGS", cflags)
}

pub fn recipe() -> Recipe {
    let cflags = "-O2 -g1 -fno-omit-frame-pointer -ffile-prefix-map={root}=/td-build-root -ffile-prefix-map={src}=/td-build";
    let mut steps = unpack_into("glibc-x86-64-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    // localedata/Makefile gzips each installed charmap. td ships no gzip and
    // localedef reads a raw charmap directly, so the charmaps stay raw under
    // their unsuffixed names.
    steps.push(Step::substitute_text(
        "{src}/localedata/Makefile",
        vec![TextEdit::new("\tgzip -9n $(@:.gz=)", "\ttrue", 1)],
    ));
    steps.push(Step::MkDir {
        path: "{root}/wb".into(),
    });
    steps.push(Step::MkDir {
        path: "{root}/maps".into(),
    });
    // gcc's temporaries, such as the object `make install` assembles from
    // /dev/null to learn the link format, lie in the build, where the map
    // check accepts them.
    steps.push(Step::MkDir {
        path: "{root}/tmp".into(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: CC.into(),
        exec: true,
    });
    steps.push(Step::MkDir {
        path: "{src}/bld".into(),
    });
    steps.push(glibc_configure(
        "{src}",
        "{src}/bld",
        "{root}/wb/cc",
        cflags,
    ));
    steps.push(configuration_check());
    steps.push(
        Step::run(
            "{src}/bld",
            &["/usr/bin/make", "-j{jobs}", &format!("SHELL={SH}")],
        )
        .env("PATH", PATH)
        .env("CONFIG_SHELL", SH)
        .env("TD_LINK_MAPS", "{root}/maps")
        .env("TMPDIR", "{root}/tmp"),
    );
    steps.push(
        Step::run(
            "{src}/bld",
            &[
                "/usr/bin/make",
                &format!("SHELL={SH}"),
                "install",
                "DESTDIR={out}/stage",
            ],
        )
        .env("PATH", PATH)
        .env("CONFIG_SHELL", SH)
        .env("TD_LINK_MAPS", "{root}/maps")
        .env("TMPDIR", "{root}/tmp"),
    );
    steps.push(link_maps_check());
    steps.extend(scripts());
    // Relocate every GNU ld script under lib/*.so from absolute member paths to
    // bare names (ld finds them via -L); real ELFs and absent scripts are skipped.
    steps.push(relocate_ld_scripts(STAGE, PREFIX));
    // kernel UAPI headers into the staged include dir (glibc-installed headers
    // win collisions, so copy the kernel trees subdir-wise).
    for kd in KERNEL_HEADER_DIRS {
        steps.push(Step::CopyTree {
            from: format!("{{root}}/kh/{kd}"),
            dest: format!("{STAGE}/include/{kd}"),
        });
    }
    steps.push(Step::Require {
        paths: vec![
            format!("{STAGE}/lib/libc.so.6"),
            format!("{STAGE}/lib/ld-linux-x86-64.so.2"),
            format!("{STAGE}/include/linux/limits.h"),
        ],
        exec: false,
    });
    steps.push(output_scan());
    steps.push(Step::split_debug_tree(STAGE, "/usr/bin/objcopy"));
    Recipe::mesboot("glibc-x86-64", "2.41")
        .source_input("glibc-241-source")
        .build_root()
        .native_inputs(&["gcc-runtime-stage1"])
        .inputs(&["linux-headers-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check glibc-x86-64: build glibc 2.41 in the Debian build root, linked with gcc-runtime-stage1 alone"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run glibc-x86-64 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

/// configure saw the build as a cross build and found none of the root's
/// optional libraries, as the ladder build did not; glibc compiles
/// `-nostdinc` against gcc's freestanding headers and the kernel's alone,
/// never the root's /usr/include; Debian's gcc defaults to PIE, so the
/// static programs link static-PIE; and gcc puts gcc-runtime-stage1's
/// start files and libgcc first.
fn configuration_check() -> Step {
    let script = format!(
        "set -e; c=config.make; \
         for want in 'cross-compiling = yes' 'have-selinux = no' 'LIBGD = no' 'enable-cet = no' \
         'build-pie-default = yes' \
         'sysincludes = -nostdinc -isystem /usr/lib/gcc/x86_64-linux-gnu/14/include -isystem {{root}}/kh'; do \
         grep -Fqx \"$want\" $c || {{ echo \"config.make lacks: $want\" >&2; grep -E '^(sysincludes|build-pie-default) ' $c >&2 || true; exit 1; }}; done; \
         grep -Eqx 'CXX = ?' $c || {{ echo 'configure found a C++ compiler' >&2; exit 1; }}; \
         rt=$(realpath -m '{RUNTIME}'); \
         for f in crtbegin.o crtbeginS.o crtbeginT.o crtend.o crtendS.o libgcc.a libgcc_eh.a; do \
         case $(realpath -m \"$('{{root}}/wb/cc' -print-file-name=$f)\") in \
         \"$rt\"/*) ;; *) echo \"$f resolves outside gcc-runtime-stage1\" >&2; exit 1;; esac; done"
    );
    Step::run("{src}/bld", &[SH, "-c", &script]).env("PATH", PATH)
}

/// Every link the wrapper recorded while `make` and `make install` ran has
/// a map that records exactly that output and loads files only from the
/// build, the source tree and gcc-runtime-stage1, a relative path
/// resolving against the directory the link ran in; every map written is
/// a recorded link's. The named links load gcc-runtime-stage1's libgcc.a,
/// and at least `floor` links ran, so a build that bypassed the wrapper
/// cannot pass with nothing recorded.
fn link_maps_script(wanted: &[&str], floor: usize) -> String {
    let wanted = wanted.join(" ");
    format!(
        "set -e; fail() {{ echo \"link maps: $*\" >&2; exit 1; }}; \
         rt=$(realpath -m '{RUNTIME}'); \
         set -- \"$(realpath -m '{{root}}')\" \"$(realpath -m '{{src}}')\" \"$rt\"; \
         n=0; gcc=' '; \
         for m in '{{root}}'/maps/*.map; do \
         [ -e \"$m\" ] || continue; [ -e \"${{m%.map}}.link\" ] || fail \"$m is no recorded link's\"; done; \
         for rec in '{{root}}'/maps/*.link; do \
         [ -e \"$rec\" ] || continue; \
         {{ IFS= read -r out && IFS= read -r m && IFS= read -r cwd; }} < \"$rec\" || fail \"$rec is malformed\"; \
         [ -s \"$m\" ] || fail \"the link of $out wrote no map $m\"; n=$((n+1)); \
         outs=$(grep -c '^OUTPUT(' \"$m\" || true); [ \"$outs\" = 1 ] || fail \"$m records $outs outputs\"; \
         o=$(sed -n 's/^OUTPUT(\\([^ ]*\\) .*/\\1/p' \"$m\"); \
         [ \"$o\" = \"$out\" ] || fail \"$m is the link of $o, recorded as $out\"; \
         while IFS= read -r line; do \
         case $line in 'LOAD '*) f=${{line#LOAD }};; *) continue;; esac; \
         [ \"$f\" = 'linker stubs' ] && continue; \
         case $f in /*) r=$(realpath -m \"$f\");; *) r=$(realpath -m \"$cwd/$f\");; esac; \
         ok=0; for a in \"$@\"; do case $r in \"$a\"/*) ok=1;; esac; done; \
         [ $ok = 1 ] || fail \"$o loaded $f from outside the build and gcc-runtime-stage1\"; \
         [ \"$r\" = \"$rt/libgcc.a\" ] && gcc=\"$gcc${{o##*/}} \"; \
         done < \"$m\"; done; \
         [ $n -ge {floor} ] || fail \"only $n links were recorded\"; \
         for want in {wanted}; do case $gcc in *\" $want \"*) ;; \
         *) fail \"$want did not load gcc-runtime-stage1's libgcc.a\";; esac; done; \
         echo \"link maps: $n links, none outside the build and gcc-runtime-stage1\""
    )
}

fn link_maps_check() -> Step {
    let script = link_maps_script(LIBGCC_LINKS, 250);
    Step::run("{src}/bld", &[SH, "-c", &script]).env("PATH", PATH)
}

/// The scripts glibc installs in bin/ that td ships: each POSIX sh apart
/// from upstream's `#!/bin/bash` and, in ldd and sotruss, bash's `$"…"`
/// translated strings, which a POSIX shell prints with a stray `$`. Their
/// shebang becomes `#!/bin/sh` and the strings plain ones, counted.
const SH_SCRIPTS: &[(&str, usize)] = &[("ldd", 15), ("sotruss", 12), ("tzselect", 0)];

/// The scripts td does not ship: xtrace needs bash's `read -p`, GNU expr,
/// binutils' addr2line and an X terminal; mtrace is a Perl program, and
/// td ships no Perl.
const DROPPED_SCRIPTS: &[&str] = &["xtrace", "mtrace"];

/// glibc's scripts made POSIX sh and smoke-tested under the root's dash:
/// each parses (`dash -n` reads the whole file, where running one reads
/// only as far as it goes), prints its help and version, and ldd, given
/// the staged loader in place of the install prefix's (which the build,
/// like a td system, does not have), traces one of glibc's own programs.
fn scripts() -> Vec<Step> {
    let mut steps = Vec::new();
    for (script, translated) in SH_SCRIPTS {
        let mut edits = vec![TextEdit::new("#!/bin/bash\n", "#!/bin/sh\n", 1)];
        if *translated > 0 {
            edits.push(TextEdit::new("$\"", "\"", *translated));
        }
        steps.push(Step::substitute_text(
            &format!("{STAGE}/bin/{script}"),
            edits,
        ));
    }
    let dropped: Vec<String> = DROPPED_SCRIPTS
        .iter()
        .map(|script| format!("'{STAGE}/bin/{script}'"))
        .collect();
    let parse: String = SH_SCRIPTS
        .iter()
        .map(|(script, _)| format!(" {script}"))
        .collect();
    let script = format!(
        "set -e; fail() {{ echo \"glibc scripts: $*\" >&2; exit 1; }}; \
         rm {dropped}; \
         b='{STAGE}/bin'; d=/usr/bin/dash; t='{{root}}/script-smoke'; mkdir -p \"$t\"; \
         for s in{parse}; do $d -n \"$b/$s\" || fail \"$s does not parse as POSIX sh\"; done; \
         first() {{ out=$(\"$@\") || fail \"$* failed\"; printf '%s\\n' \"$out\" | head -n 1; }}; \
         [ \"$(first $d \"$b/ldd\" --help)\" = 'Usage: ldd [OPTION]... FILE...' ] || fail 'ldd --help'; \
         [ \"$(first $d \"$b/ldd\" --version)\" = 'ldd (GNU libc) 2.41' ] || fail 'ldd --version'; \
         [ \"$(first $d \"$b/sotruss\" --help)\" = 'Usage: sotruss [OPTION...] [--] EXECUTABLE [EXECUTABLE-OPTION...]' ] || fail 'sotruss --help'; \
         [ \"$(first $d \"$b/sotruss\" --version)\" = 'sotruss (GNU libc) 2.41' ] || fail 'sotruss --version'; \
         [ \"$(first $d \"$b/tzselect\" --version)\" = 'tzselect (GNU libc) 2.41' ] || fail 'tzselect --version'; \
         case $(first $d \"$b/tzselect\" --help) in 'Usage: tzselect '*) ;; *) fail 'tzselect --help';; esac; \
         sed \"s|^RTLDLIST=.*|RTLDLIST='{STAGE}/lib/ld-linux-x86-64.so.2'|\" \"$b/ldd\" > \"$t/ldd\"; \
         [ \"$(grep -c '^RTLDLIST=' \"$t/ldd\")\" = 1 ] || fail 'ldd names no one RTLDLIST'; \
         traced=$(LD_LIBRARY_PATH='{STAGE}/lib' $d \"$t/ldd\" \"$b/getent\") || fail \"ldd getent: $traced\"; \
         case $traced in *'libc.so.6 => {STAGE}/lib/libc.so.6 ('*) ;; *) fail \"ldd getent: $traced\";; esac; \
         echo \"glibc scripts: {count} POSIX sh, smoke-tested under dash\"",
        dropped = dropped.join(" "),
        count = SH_SCRIPTS.len(),
    );
    steps.push(Step::run("{root}", &[SH, "-c", &script]).env("PATH", PATH));
    steps
}

/// No installed file names the build, the source tree, gcc-runtime-stage1,
/// whose objects are linked in, not referenced, or the root's bash; no
/// ELF names the root's /usr/include (a partial witness: line tables
/// name only headers that contribute line entries; config.make's pinned
/// `sysincludes = -nostdinc ...` is the guarantee); and the only scripts are
/// SH_SCRIPTS, in bin/, each starting `#!/bin/sh`.
fn output_scan() -> Step {
    let script = format!(
        "set -e; fail() {{ echo \"$*\" >&2; exit 1; }}; rt=$(realpath -m '{{in:gcc-runtime-stage1}}'); \
         for p in '{{root}}' '{{src}}' \"$rt\" /bin/bash; do \
         if grep -r -a -l -F \"$p\" '{{out}}' >&2; then \
         fail \"the files above name $p\"; fi; done; \
         n=0; elves=0; shopt -s globstar dotglob nullglob; \
         for f in '{{out}}'/**; do \
         [ -f \"$f\" ] && [ ! -L \"$f\" ] || continue; \
         case $(head -c 4 \"$f\" | od -An -c | tr -d ' \\n') in \
         177ELF) elves=$((elves+1)); \
         if grep -a -Fq /usr/include \"$f\"; then fail \"$f names /usr/include\"; fi;; \
         '#!'*) n=$((n+1)); \
         case $f in '{STAGE}'/bin/*) ;; *) fail \"$f is a script outside bin/\";; esac; \
         IFS= read -r line < \"$f\"; [ \"$line\" = '#!/bin/sh' ] || fail \"$f starts $line, not #!/bin/sh\";; \
         esac; done; \
         [ $n = {count} ] || fail \"the output ships $n scripts, expected {count}\"; \
         [ $elves -gt 100 ] || fail \"only $elves ELF files were read\"",
        count = SH_SCRIPTS.len(),
    );
    Step::run("{out}", &[SH, "-c", &script]).env("PATH", PATH)
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    /// glibc builds in the root from gcc-runtime-stage1 alone: no ladder
    /// compiler, glibc or tool, the stage1 runtime ahead of Debian's, the
    /// target profile after glibc's own flags, and the map check after the
    /// mapped make and before the debug split.
    #[test]
    fn glibc_links_the_stage1_runtime_in_the_build_root() {
        let recipe = recipe();
        assert!(recipe.build_root);
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(["gcc-runtime-stage1".to_string()].as_slice())
        );
        assert_eq!(
            recipe.inputs.as_deref(),
            Some(["linux-headers-x86-64".to_string()].as_slice())
        );
        let steps = recipe.steps.unwrap_or_default();
        let wrapper = steps
            .iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{root}/wb/cc" => Some(content),
                _ => None,
            })
            .expect("compiler wrapper");
        let runtime = wrapper
            .find("-B{in:gcc-runtime-stage1}/lib/ ")
            .expect("stage1 runtime");
        let args = wrapper.find("\"${map[@]}\" \"$@\"").expect("caller flags");
        let policy = wrapper.find("-fno-omit-frame-pointer").expect("profile");
        assert!(runtime < args && args < policy);
        let position = |what: &str, found: &dyn Fn(&Step) -> bool| {
            steps
                .iter()
                .position(|step| found(step))
                .unwrap_or_else(|| panic!("{what}"))
        };
        let made = position("mapped make", &|step| {
            matches!(step, Step::Run { env, .. }
                if env.iter().any(|(k, v)| k == "TD_LINK_MAPS" && v == "{root}/maps"))
        });
        let checked = position("map check", &|step| {
            matches!(step, Step::Run { argv, .. }
                if argv.iter().any(|arg| arg.contains("did not load gcc-runtime-stage1's libgcc.a")))
        });
        let split = position("split", &|step| matches!(step, Step::SplitDebugTree { .. }));
        assert!(made < checked && checked < split);
        let installed = steps
            .iter()
            .position(|step| {
                matches!(step, Step::Run { argv, env, .. }
                    if argv.iter().any(|arg| arg == "install")
                        && env.iter().any(|(k, v)| k == "TD_LINK_MAPS" && v == "{root}/maps"))
            })
            .expect("make install records its links");
        assert!(installed < checked);
    }

    /// A fixture build's paths: the build directory, the source tree under
    /// it, and gcc-runtime-stage1's output, with its placeholders replaced.
    struct Fixture {
        _tmp: crate::test_temp::TempDir,
        root: String,
        src: String,
        runtime: String,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let tmp = crate::test_temp::TempDir::new(tag).unwrap();
            let base = tmp.path().to_string_lossy().into_owned();
            let fixture = Fixture {
                root: format!("{base}/root"),
                src: format!("{base}/root/src"),
                runtime: format!("{base}/store/gcc-runtime-stage1"),
                _tmp: tmp,
            };
            for dir in [
                format!("{}/maps", fixture.root),
                format!("{}/bld/elf", fixture.src),
                format!("{}/lib", fixture.runtime),
            ] {
                std::fs::create_dir_all(dir).unwrap();
            }
            fixture
        }

        fn expand(&self, text: &str) -> String {
            text.replace("{root}", &self.root)
                .replace("{src}", &self.src)
                .replace("{in:gcc-runtime-stage1}", &self.runtime)
        }
    }

    /// The map check reads every recorded link's map and refuses a load
    /// from the root, a relative path climbing out of the build, a map of
    /// two outputs or of another output, a recorded link with no map, a
    /// map no link recorded, and a named link without stage1's libgcc.a.
    #[test]
    fn the_link_map_check_reads_every_recorded_link() {
        let f = Fixture::new("td-glibc-maps");
        let script = f.expand(&super::link_maps_script(&["libc.so"], 2));
        let (root, src, rt) = (&f.root, &f.src, &f.runtime);
        let libc = format!(
            "OUTPUT({src}/bld/libc.so elf64-x86-64)\nLOAD {src}/bld/csu/crti.o\n\
             LOAD {rt}/lib/libgcc.a\nLOAD ../csu/abi-note.o\nLOAD linker stubs\n"
        );
        let prog =
            format!("OUTPUT({src}/bld/elf/ldconfig elf64-x86-64)\nLOAD {rt}/lib/crtbeginT.o\n");
        let record = |name: &str, out: &str, map: &str, map_text: Option<&str>| {
            std::fs::write(
                format!("{root}/maps/{name}.link"),
                format!("{out}\n{map}\n{src}/elf\n"),
            )
            .unwrap();
            if let Some(text) = map_text {
                std::fs::write(map, text).unwrap();
            }
        };
        let run = |refusal: Option<&str>, case: &str| {
            let run = std::process::Command::new("bash")
                .arg("-c")
                .arg(&script)
                .current_dir(src)
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&run.stderr);
            match refusal {
                None => assert!(run.status.success(), "{case}: {stderr}"),
                Some(reason) => {
                    assert!(!run.status.success(), "{case} passed");
                    assert!(stderr.contains(reason), "{case}: {stderr}");
                }
            }
        };
        let reset = || {
            std::fs::remove_dir_all(format!("{root}/maps")).unwrap();
            std::fs::create_dir_all(format!("{root}/maps")).unwrap();
        };
        let clean = |libc_map: &str| {
            reset();
            record(
                "a",
                &format!("{src}/bld/libc.so"),
                &format!("{root}/maps/a.map"),
                Some(libc_map),
            );
            record(
                "b",
                &format!("{src}/bld/elf/ldconfig"),
                &format!("{root}/maps/b.map"),
                Some(&prog),
            );
        };
        clean(&libc);
        run(None, "clean");
        let cases = [
            (
                format!("{libc}LOAD /usr/lib/gcc/x86_64-linux-gnu/14/libgcc_eh.a\n"),
                "from outside the build",
            ),
            (
                format!("{libc}LOAD ../../../../usr/lib/x86_64-linux-gnu/libc.a\n"),
                "from outside the build",
            ),
            (
                format!("{libc}OUTPUT({src}/bld/other elf64-x86-64)\n"),
                "records 2 outputs",
            ),
            (libc.replace("bld/libc.so", "bld/libm.so"), "recorded as"),
            (
                libc.replace(&format!("LOAD {rt}/lib/libgcc.a\n"), ""),
                "libc.so did not load gcc-runtime-stage1's libgcc.a",
            ),
        ];
        for (map, reason) in cases {
            clean(&map);
            run(Some(reason), &map);
        }
        // A recorded link whose map was never written.
        clean(&libc);
        record(
            "c",
            &format!("{src}/bld/elf/sln"),
            &format!("{root}/maps/c.map"),
            None,
        );
        run(Some("wrote no map"), "unmapped link");
        // A map no link recorded.
        clean(&libc);
        std::fs::write(format!("{root}/maps/d.map"), &prog).unwrap();
        run(Some("is no recorded link's"), "unrecorded map");
        // Fewer links than the floor: nothing was recorded.
        reset();
        run(Some("only 0 links were recorded"), "no links");
    }

    /// The wrapper records each link and adds a map to it, passes glibc's
    /// two rtld maps through in place of its own, and refuses a link with
    /// no `-o` and any other `-Map`; compiles and queries are not links.
    #[test]
    fn the_wrapper_records_every_link() {
        let f = Fixture::new("td-glibc-cc");
        let (root, src) = (&f.root, &f.src);
        let gcc = format!("{root}/gcc");
        std::fs::write(&gcc, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gcc, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cc = f
            .expand(super::CC)
            .replace("exec /usr/bin/gcc", &format!("exec {gcc}"));
        let wrapper = format!("{root}/cc");
        std::fs::write(&wrapper, cc).unwrap();
        let maps = format!("{root}/maps");
        let records = || {
            let mut found: Vec<Vec<String>> = std::fs::read_dir(&maps)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "link"))
                .map(|path| {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let map = format!("{}.map", path.to_string_lossy().trim_end_matches(".link"));
                    text.lines()
                        .map(|line| line.replace(&map, "<own>"))
                        .collect()
                })
                .collect();
            found.sort();
            std::fs::remove_dir_all(&maps).unwrap();
            std::fs::create_dir_all(&maps).unwrap();
            found
        };
        let cc = |args: &[&str]| {
            std::process::Command::new("bash")
                .arg(&wrapper)
                .args(args)
                .env("TD_LINK_MAPS", &maps)
                .env("PWD", src)
                .current_dir(src)
                .output()
                .unwrap()
        };
        let elf = format!("{src}/bld/elf");
        let line = |out: &str| vec![out.to_string(), "<own>".to_string(), src.clone()];

        let link = cc(&["-shared", "-o", "libc.so", "a.o"]);
        assert!(
            link.status.success(),
            "{}",
            String::from_utf8_lossy(&link.stderr)
        );
        let args = String::from_utf8_lossy(&link.stdout);
        assert!(args.contains(&format!("-Wl,-Map={maps}/")), "{args}");
        assert_eq!(records(), [line("libc.so")]);
        assert!(cc(&["-ofused", "a.o"]).status.success());
        assert_eq!(records(), [line("fused")]);

        let rtld_map = format!("{elf}/librtld.map");
        let rtld = cc(&[
            "-o",
            &format!("{rtld_map}.o"),
            "x.os",
            &format!("-Wl,-Map,{rtld_map}T"),
        ]);
        assert!(rtld.status.success());
        assert!(!String::from_utf8_lossy(&rtld.stdout).contains("-Wl,-Map="));
        let rtld_os = format!("{elf}/librtld.os");
        assert!(cc(&[
            "-r",
            "-o",
            &rtld_os,
            "x.os",
            &format!("-Wl,-Map,{rtld_os}.map")
        ])
        .status
        .success());
        assert_eq!(
            records(),
            [
                vec![format!("{rtld_map}.o"), rtld_map.clone(), src.clone()],
                vec![rtld_os.clone(), format!("{rtld_os}.map"), src.clone()],
            ]
        );

        for compile in [
            &["-c", "-o", "a.o", "a.c"][..],
            &["-S", "a.c"],
            &["-E", "-dM", "-"],
            &["-print-file-name=libgcc.a"],
            &["-Wl,-z,mark-plt", "--print-file-name=crtbeginS.o"],
            &["-dumpmachine"],
            &["--version"],
        ] {
            assert!(cc(compile).status.success(), "{compile:?}");
        }
        assert!(records().is_empty());

        for (refused, why) in [
            (&["a.o"][..], "no -o"),
            (
                &["-o", "prog", "a.o", "-Wl,-Map=x.map"],
                "a map td does not read",
            ),
            (
                &["-o", "prog", "a.o", "-Xlinker", "-Map", "-Xlinker", "x"],
                "a map td does not read",
            ),
            (
                &["-o", &rtld_os, "x.os", "-Wl,-Map,elsewhere.map"],
                "a map td does not read",
            ),
        ] {
            let run = cc(refused);
            assert!(!run.status.success(), "{refused:?}");
            let stderr = String::from_utf8_lossy(&run.stderr);
            assert!(stderr.contains(why), "{refused:?}: {stderr}");
        }
        assert!(records().is_empty());
    }
}
