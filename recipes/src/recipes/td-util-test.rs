use crate::ladder::{post_rust_inputs, post_rust_tool_farm, unpack_into, POST_RUST_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// td-util-test: build-shape AND behavioural validation of the diagnostics multicall.
//
// Per repo policy that recipes test their output, this asserts the shipped
// td-util binary is the self-contained STATIC ELF its slot requires, re-proving
// with an independent readelf walk what the producer's `assert_static`
// fail-closes on:
//   1. ELF64 x86-64 *executable* (readelf: class ELF64, machine x86-64, type
//      EXEC) — EXEC (not DYN) is the non-PIE static shape,
//   2. NO PT_INTERP program header,
//   3. NO dynamic NEEDED entry — an EMPTY runtime closure.
//
// It then EXERCISES the applets. Unlike td-sh's exit-0 smoke, td-util has real
// observable output, so the behavioural legs assert it: multicall dispatch works
// through both entry forms (argv[0] basename and `td-util <applet>`), the applet
// roster matches, and the documented exit codes hold. The /proc-backed applets
// are gated on /proc actually being mounted in the sandbox so this recipe stays
// green wherever it runs; the boot oracle is what exercises them on the image.
//
// The steps run under td-sh with the post-Rust tool farm, whose grep is td-txt
// and whose coreutils are uutils. The farm's cmp, diff, gzip and the rest are
// td-util's own, so no assertion about td-util is graded by one of them: the
// subject is always invoked by its store path, and a byte comparison is the
// shell's. The format crossings take oracles td did not write: zlib's own
// minigzip, compiled here from zlib's source against the post-Rust libz, and a
// newc archive GNU cpio 2.15 wrote.
pub fn recipe() -> Recipe {
    let bin = "{in:td-util}/bin/td-util";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let sgcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let zlib = "{in:zlib-x86-64-self}";
    let mut steps = vec![post_rust_tool_farm("{in:gawk-x86-64-self}/bin/gawk")];

    steps.push(
        Step::run(
            "{root}",
            &[
                POST_RUST_SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{bin}' 2>/dev/null) || {{ echo 'readelf -h failed on td-util' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || {{ echo 'td-util is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'td-util is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -qE 'Type:[[:space:]]+EXEC([[:space:]]|$)' || {{ echo 'td-util is not a static ET_EXEC — a DYN/PIE (Type: DYN, whose parenthetical also says Executable) would need runtime relocation' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", "{tools}"),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_RUST_SH,
                "-c",
                &format!(
                    "lout=$('{readelf}' -l '{bin}' 2>/dev/null) || {{ echo 'readelf -l failed on td-util (cannot verify absence of PT_INTERP)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$lout\" | grep -qi 'INTERP'; then echo 'td-util carries a PT_INTERP program header — it is not static' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", "{tools}"),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_RUST_SH,
                "-c",
                &format!(
                    "dout=$('{readelf}' -d '{bin}' 2>/dev/null) || {{ echo 'readelf -d failed on td-util (cannot verify absence of dynamic NEEDED)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$dout\" | grep -qi 'NEEDED'; then echo 'td-util has a dynamic NEEDED entry — its runtime closure is not empty' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", "{tools}"),
    );

    // Applet roster: the /bin symlink farm this multicall will back is generated
    // from `--list`, so a dropped or renamed applet must red here rather than
    // strand a dead /bin symlink on the image.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_RUST_SH,
                "-c",
                &format!(
                    "l=$('{bin}' --list) || {{ echo 'td-util --list failed' >&2; exit 1; }}; \
                     for a in cat chmod chown clear cmp cpio diff dmesg free gunzip gzip less ln mkdir od printf ps readlink rm sleep test uname which zcat; do \
                         printf '%s\\n' \"$l\" | grep -q -x -F \"$a\" || {{ echo \"td-util does not serve applet '$a'\" >&2; exit 1; }}; \
                     done; \
                     n=$(printf '%s\\n' \"$l\" | wc -l); \
                     [ \"$n\" -eq 26 ] || {{ echo \"td-util serves $n applets, expected exactly 26 — update this check deliberately when adding one\" >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", "{tools}"),
    );

    // Dispatch through BOTH entry forms, plus the documented exit codes. `which`
    // is the hermetic applet (no /proc), so it carries the behavioural assertion:
    // resolving td-util's own directory must yield td-util's own path.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_RUST_SH,
                "-c",
                &format!(
                    "p='{{in:td-util}}/bin'; \
                     out=$(PATH=\"$p\" '{bin}' which td-util) || {{ echo 'td-util which td-util failed' >&2; exit 1; }}; \
                     [ \"$out\" = \"$p/td-util\" ] || {{ echo \"which resolved '$out', expected '$p/td-util'\" >&2; exit 1; }}; \
                     PATH=\"$p\" '{bin}' which no-such-command-xyz >/dev/null 2>&1; \
                     [ $? -eq 1 ] || {{ echo 'which must exit 1 when a name does not resolve' >&2; exit 1; }}; \
                     '{bin}' no-such-applet >/dev/null 2>&1; \
                     [ $? -eq 2 ] || {{ echo 'td-util must exit 2 on an unknown applet (usage error)' >&2; exit 1; }}; \
                     '{bin}' clear >/dev/null || {{ echo 'td-util clear failed' >&2; exit 1; }}; \
                     d='{{root}}/argv0'; mkdir -p \"$d\"; \
                     ln -sf '{bin}' \"$d/which\" || {{ echo 'could not build the argv[0] symlink' >&2; exit 1; }}; \
                     ln -sf '{bin}' \"$d/clear\" || {{ echo 'could not build the argv[0] symlink' >&2; exit 1; }}; \
                     out=$(PATH=\"$p\" \"$d/which\" td-util) || {{ echo 'argv[0] dispatch: /bin/which -> td-util failed — this is the form the shipped symlink farm uses' >&2; exit 1; }}; \
                     [ \"$out\" = \"$p/td-util\" ] || {{ echo \"argv[0] dispatch resolved '$out', expected '$p/td-util'\" >&2; exit 1; }}; \
                     \"$d/clear\" >/dev/null || {{ echo 'argv[0] dispatch: /bin/clear -> td-util failed' >&2; exit 1; }}; \
                     '{bin}' test -d / || {{ echo 'test -d / must exit 0 (true)' >&2; exit 1; }}; \
                     '{bin}' test -d /no-such-dir-xyz; \
                     [ $? -eq 1 ] || {{ echo 'test must exit 1 for false, not 2 — a boot script reads that as an error' >&2; exit 1; }}; \
                     '{bin}' test 1 -lt 5 || {{ echo 'test 1 -lt 5 must be true' >&2; exit 1; }}; \
                     '{bin}' test '' -lt 5 >/dev/null 2>&1; \
                     [ $? -eq 2 ] || {{ echo 'a non-numeric integer operand must be an error, not false: `while test \"$n\" -lt 5` with n unset would never end' >&2; exit 1; }}; \
                     for op in -r -w -x; do \
                       '{bin}' test \"$op\" / >/dev/null 2>&1; \
                       [ $? -eq 2 ] || {{ echo \"the SHIPPED binary must refuse $op: it is access(2), and a mode-bits answer reads root own bit on 0755 /var, passing a system that had failed\" >&2; exit 1; }}; \
                     done; \
                     '{bin}' test ' 1 ' -eq 1 || {{ echo 'surrounding blanks must parse: the live boot conditionals were written against a test that accepts them' >&2; exit 1; }}; \
                     i=1; while [ $i -le 100 ]; do echo $i; i=$((i+1)); done > '{{root}}/less-in'; \
                     out=$('{bin}' less '{{root}}/less-in' </dev/null) || {{ echo 'less failed on a file' >&2; exit 1; }}; \
                     [ \"$(printf '%s\\n' \"$out\" | wc -l)\" -eq 100 ] || {{ echo 'less must copy through UNPAGED when stdout is not a terminal — otherwise its prompts land in the data of every pipeline' >&2; exit 1; }}; \
                     printf '%s\\n' \"$out\" | grep -q -x -F 100 || {{ echo 'less dropped the last line of a non-terminal copy' >&2; exit 1; }}; \
                     printf '%s\\n' \"$out\" | grep -q -- '--More--' && {{ echo 'less emitted a pager prompt into a pipe' >&2; exit 1; }}; \
                     '{bin}' less -N '{{root}}/less-in' >/dev/null 2>&1; \
                     [ $? -eq 2 ] || {{ echo 'less must refuse an option it cannot honour rather than silently ignoring it, and exit 2 for a usage error as the rest of this multicall does' >&2; exit 1; }}; \
                     '{bin}' printf 'a\\nb\\n' > '{{root}}/-N'; \
                     out=$(cd '{{root}}' && '{bin}' less -- -N </dev/null) || {{ echo 'less -- must page a file whose name looks like an option' >&2; exit 1; }}; \
                     [ \"$out\" = \"$(printf 'a\\nb')\" ] || {{ echo 'less -- consumed the wrong operand' >&2; exit 1; }}; \
                     (cd '{{root}}' && '{bin}' less -N </dev/null) >/dev/null 2>&1; \
                     [ $? -eq 2 ] || {{ echo 'without -- the same argument must still be refused as an option, or the leg above proves nothing' >&2; exit 1; }}; \
                     printf 'x\\000y\\n' > '{{root}}/less-bin'; \
                     '{bin}' less '{{root}}/less-bin' </dev/null | grep -q -a -- y || {{ echo 'less must pass NON-TEXT bytes through rather than refusing the file — a pager is pointed at logs' >&2; exit 1; }}; \
                     '{bin}' less '{{root}}/no-such-file' >/dev/null 2>&1; \
                     [ $? -eq 1 ] || {{ echo 'less must exit 1 on an unreadable operand' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", "{tools}"),
    );

    // The build-tool applets in the SHIPPED binary (opt-level=s, panic=abort,
    // which cargo's tests never build), each crossed with an independent
    // implementation: td gzip read by zlib's minigzip and the reverse, and a
    // GNU cpio newc archive read by td cpio. minigzip -d copies a headerless
    // stream through, so td gzip's output must also be a whole gzip member's
    // size, and every comparison keeps the final newline with a sentinel.
    // The archive is GNU cpio 2.15's `-o -H newc --reproducible --owner=0:0`
    // output for t, t/a ("alpha"), t/s and t/s/b ("bravo"), transcribed byte
    // for byte as a printf format with the block padding after its trailer
    // trimmed. The other two build-tool applets are left to the crate's
    // process tests, which the in-sandbox cargo gate runs: their names are
    // the retired findutils words, which the ladder guard refuses in any Run
    // argv, so this text names them nowhere — the count of 26 above pins
    // that they are served.
    steps.extend(unpack_into("td-util-test-source", "{root}/zlib"));
    steps.push(
        Step::run(
            "{root}/zlib",
            &[
                sgcc,
                "-static",
                "-O2",
                "-isystem",
                &format!("{xglibc}/include"),
                &format!("-B{sbin}/"),
                &format!("-B{xglibc}/lib"),
                &format!("-L{xglibc}/lib"),
                &format!("-I{zlib}/include"),
                "test/minigzip.c",
                &format!("{zlib}/lib/libz.a"),
                "-o",
                "{root}/minigzip",
            ],
        )
        .env("PATH", sbin),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_RUST_SH,
                "-c",
                &format!(
                    "mg='{{root}}/minigzip'; \
                     d='{{root}}/tools'; mkdir -p \"$d/t/s\" \"$d/x\" && cd \"$d\" || exit 1; \
                     printf 'a\\n' > t/a; printf 'b\\n' > t/s/b; \
                     '{bin}' cmp t/a t/a || {{ echo 'cmp of a file with itself must exit 0' >&2; exit 1; }}; \
                     '{bin}' cmp -s t/a t/s/b; \
                     [ $? -eq 1 ] || {{ echo 'cmp -s must exit 1 on differing files' >&2; exit 1; }}; \
                     '{bin}' diff -u t/a t/s/b > u.diff; \
                     [ $? -eq 1 ] || {{ echo 'diff must exit 1 on differing files' >&2; exit 1; }}; \
                     grep -q -x -F -- '+b' u.diff || {{ echo 'diff -u lost the added line' >&2; exit 1; }}; \
                     '{bin}' gzip -c t/a > a.gz && \"$mg\" -d < a.gz > a.out || {{ echo 'minigzip could not read td gzip' >&2; exit 1; }}; \
                     [ \"$(wc -c < a.gz)\" -ge 20 ] || {{ echo 'td gzip wrote no gzip member: minigzip -d would copy a headerless stream through' >&2; exit 1; }}; \
                     [ \"$(cat a.out; printf .)\" = \"$(printf 'a\\n.')\" ] || {{ echo 'td gzip round-tripped through minigzip to the wrong bytes' >&2; exit 1; }}; \
                     \"$mg\" < t/s/b > b.gz && '{bin}' zcat b.gz > b.out || {{ echo 'td zcat could not read zlib gzip' >&2; exit 1; }}; \
                     [ \"$(cat b.out; printf .)\" = \"$(printf 'b\\n.')\" ] || {{ echo 'td zcat decoded zlib gzip to the wrong bytes' >&2; exit 1; }}; \
                     printf '07070100000000000041ED0000000000000000000000020000000100000000000000000000000000000000000000000000000200000000t\\00007070100000001000081A40000000000000000000000010000000100000006000000000000000000000000000000000000000400000000t/a\\000\\000\\000alpha\\n\\000\\00007070100000002000041ED0000000000000000000000020000000100000000000000000000000000000000000000000000000400000000t/s\\000\\000\\00007070100000003000081A40000000000000000000000010000000100000006000000000000000000000000000000000000000600000000t/s/b\\000bravo\\n\\000\\00007070100000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000B00000000TRAILER!!!\\000\\000\\000\\000' > g.cpio; \
                     [ \"$(wc -c < g.cpio)\" -eq 600 ] || {{ echo 'the GNU cpio fixture did not reproduce' >&2; exit 1; }}; \
                     [ \"$('{bin}' cpio -t -F g.cpio | tr '\\n' ' ')\" = 't t/a t/s t/s/b ' ] || {{ echo 'td cpio -t misread a GNU cpio newc archive' >&2; exit 1; }}; \
                     (cd x && '{bin}' cpio -i -d -F ../g.cpio) || {{ echo 'td cpio -i failed' >&2; exit 1; }}; \
                     [ \"$(cat x/t/a; printf .)\" = \"$(printf 'alpha\\n.')\" ] && [ \"$(cat x/t/s/b; printf .)\" = \"$(printf 'bravo\\n.')\" ] || {{ echo 'td cpio -i extracted the wrong bytes' >&2; exit 1; }}; \
                     '{bin}' sleep 0.01 || {{ echo 'sleep must take a fractional second' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", "{tools}"),
    );

    // The /proc-backed applets, gated on /proc being mounted in this sandbox.
    // `free` must report a non-zero MemTotal and `ps` must list PID 1 — asserting
    // real parsed content, not merely a zero exit — and `uname` the kernel's
    // name beside the compiled machine.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_RUST_SH,
                "-c",
                &format!(
                    "if [ -r /proc/meminfo ] && [ -r /proc/1/stat ]; then \
                         f=$('{bin}' free) || {{ echo 'td-util free failed' >&2; exit 1; }}; \
                         printf '%s\\n' \"$f\" | grep -q '^Mem:' || {{ echo 'free printed no Mem: row' >&2; exit 1; }}; \
                         t=$(printf '%s\\n' \"$f\" | grep '^Mem:' | tr -s ' ' | cut -d' ' -f2); \
                         [ \"$t\" -gt 0 ] 2>/dev/null || {{ echo \"free reported MemTotal '$t' — /proc/meminfo parse regressed\" >&2; exit 1; }}; \
                         '{bin}' free -h >/dev/null || {{ echo 'td-util free -h failed' >&2; exit 1; }}; \
                         p=$('{bin}' ps) || {{ echo 'td-util ps failed' >&2; exit 1; }}; \
                         printf '%s\\n' \"$p\" | grep -qE '^ +1 ' || {{ echo 'ps did not list PID 1 — /proc scan regressed' >&2; exit 1; }}; \
                     else \
                         echo 'note: /proc not mounted in this sandbox; free/ps content asserted by the boot oracle'; \
                     fi; \
                     if [ -r /proc/sys/kernel/ostype ]; then \
                         u=$('{bin}' uname -smo) || {{ echo 'td-util uname failed' >&2; exit 1; }}; \
                         [ \"$u\" = 'Linux x86_64 GNU/Linux' ] || {{ echo \"uname -smo printed '$u'\" >&2; exit 1; }}; \
                         r=$('{bin}' uname -r) && [ -n \"$r\" ] || {{ echo 'uname -r printed no release' >&2; exit 1; }}; \
                     else \
                         echo 'note: /proc not mounted in this sandbox; uname is asserted by the cmake build'; \
                     fi"
                ),
            ],
        )
        .env("PATH", "{tools}"),
    );

    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: td-util is a statically-linked ELF64 x86-64 executable (ET_EXEC) with no PT_INTERP and no dynamic NEEDED entry; it serves exactly twenty-six applets, among them cat/chmod/chown/clear/cmp/cpio/diff/dmesg/free/gunzip/gzip/less/ln/mkdir/od/printf/ps/readlink/rm/sleep/test/uname/which/zcat, dispatches through both the argv[0] and `td-util <applet>` forms, honours its exit codes (`which` 1 = not resolved, 2 = usage; `test` 0 = true, 1 = false, 2 = bad expression), interoperates with zlib's minigzip and a GNU cpio newc archive, and parses /proc for free/ps/uname where /proc is mounted\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("td-util-test", "1.0")
        .source_input("zlib-x86-64-source")
        .native_inputs(&post_rust_inputs(
            "gawk-x86-64-self",
            &[
                "binutils-x86-64-self",
                "gcc-x86-64-self",
                "glibc-x86-64",
                "zlib-x86-64-self",
            ],
        ))
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-util-test: build-plan --auto builds td-util (td's static diagnostics, pager and initramfs userland multicall plus build tools: cat/chmod/chown/clear/cmp/cpio/diff/dmesg/free/gunzip/gzip/less/ln/mkdir/od/printf/ps/readlink/rm/sleep/test/uname/which/zcat and two more, statically linked by the /td/store target Rust + native GCC/binutils/glibc toolchain), asserts a self-contained static ELF64 x86-64 executable (ET_EXEC, no PT_INTERP, no dynamic NEEDED), and exercises the applet roster, both dispatch forms, the exit codes, and the /proc parsers"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-util-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
