use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
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
pub fn recipe() -> Recipe {
    let bin = "{in:td-util}/bin/td-util";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{bin}' 2>/dev/null) || {{ echo 'readelf -h failed on td-util' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || {{ echo 'td-util is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'td-util is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -qE 'Type:[[:space:]]+EXEC([[:space:]]|$)' || {{ echo 'td-util is not a static ET_EXEC — a DYN/PIE (Type: DYN, whose parenthetical also says Executable) would need runtime relocation' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "lout=$('{readelf}' -l '{bin}' 2>/dev/null) || {{ echo 'readelf -l failed on td-util (cannot verify absence of PT_INTERP)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$lout\" | grep -qi 'INTERP'; then echo 'td-util carries a PT_INTERP program header — it is not static' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "dout=$('{readelf}' -d '{bin}' 2>/dev/null) || {{ echo 'readelf -d failed on td-util (cannot verify absence of dynamic NEEDED)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$dout\" | grep -qi 'NEEDED'; then echo 'td-util has a dynamic NEEDED entry — its runtime closure is not empty' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // Applet roster: the /bin symlink farm this multicall will back is generated
    // from `--list`, so a dropped or renamed applet must red here rather than
    // strand a dead /bin symlink on the image.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "l=$('{bin}' --list) || {{ echo 'td-util --list failed' >&2; exit 1; }}; \
                     for a in clear dmesg free ps which; do \
                         printf '%s\\n' \"$l\" | grep -q -x -F \"$a\" || {{ echo \"td-util does not serve applet '$a'\" >&2; exit 1; }}; \
                     done; \
                     n=$(printf '%s\\n' \"$l\" | wc -l); \
                     [ \"$n\" -eq 5 ] || {{ echo \"td-util serves $n applets, expected exactly 5 — update this check deliberately when adding one\" >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // Dispatch through BOTH entry forms, plus the documented exit codes. `which`
    // is the hermetic applet (no /proc), so it carries the behavioural assertion:
    // resolving td-util's own directory must yield td-util's own path.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
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
                     \"$d/clear\" >/dev/null || {{ echo 'argv[0] dispatch: /bin/clear -> td-util failed' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // The /proc-backed applets, gated on /proc being mounted in this sandbox.
    // `free` must report a non-zero MemTotal and `ps` must list PID 1 — asserting
    // real parsed content, not merely a zero exit.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
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
                     fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: td-util is a statically-linked ELF64 x86-64 executable (ET_EXEC) with no PT_INTERP and no dynamic NEEDED entry; it serves exactly the five applets clear/dmesg/free/ps/which, dispatches through both the argv[0] and `td-util <applet>` forms, honours its exit codes (1 = not resolved, 2 = usage), and parses /proc for free/ps where /proc is mounted\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("td-util-test", "1.0")
        .native_inputs(&["td-util", "binutils-x86-64-self", "busybox-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-util-test: build-plan --auto builds td-util (td's static diagnostics multicall: clear/dmesg/free/ps/which, statically linked by the /td/store target Rust + native GCC/binutils/glibc toolchain), asserts a self-contained static ELF64 x86-64 executable (ET_EXEC, no PT_INTERP, no dynamic NEEDED), and exercises the applet roster, both dispatch forms, the exit codes, and the /proc parsers"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-util-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
