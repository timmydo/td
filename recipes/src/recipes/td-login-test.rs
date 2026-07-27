use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// td-login-test: build-shape AND behavioural validation of the credential
// multicall.
//
// Per repo policy that recipes test their output, this asserts the shipped
// td-login binary is the self-contained STATIC ELF its slot requires,
// re-proving with an independent readelf walk what the producer's
// `assert_static` fail-closes on:
//   1. ELF64 x86-64 *executable* (readelf: class ELF64, machine x86-64, type
//      EXEC) — EXEC (not DYN) is the non-PIE static shape,
//   2. NO PT_INTERP program header,
//   3. NO dynamic NEEDED entry — an EMPTY runtime closure.
//
// It then EXERCISES what a build sandbox can. The credential switch itself
// cannot run here: it needs root and a target account database, and the sandbox
// has neither — that half is covered by the crate's unit tests (which pin the
// ordering against the source) and, on the image, by the boot itself plus the
// TD-LOGIN-RUN-OK readback the health target gates on. What IS decidable here is
// everything before the switch: the roster, both dispatch forms, the documented
// exit codes, the argv refusals, and the `verify-credentials` readback probe the
// image's health target depends on — asserted BOTH ways, so a probe that always
// passed would red here rather than green-light a broken switch on the image.
//
// The static link needs the full target toolchain, so this is DAILY/operator
// tier like its siblings.
pub fn recipe() -> Recipe {
    let bin = "{in:td-login}/bin/td-login";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{bin}' 2>/dev/null) || {{ echo 'readelf -h failed on td-login' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || {{ echo 'td-login is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'td-login is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -qE 'Type:[[:space:]]+EXEC([[:space:]]|$)' || {{ echo 'td-login is not a static ET_EXEC — a DYN/PIE (Type: DYN, whose parenthetical also says Executable) would need runtime relocation' >&2; exit 1; }}"
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
                    "lout=$('{readelf}' -l '{bin}' 2>/dev/null) || {{ echo 'readelf -l failed on td-login (cannot verify absence of PT_INTERP)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$lout\" | grep -qi 'INTERP'; then echo 'td-login carries a PT_INTERP program header — it is not static' >&2; exit 1; fi"
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
                    "dout=$('{readelf}' -d '{bin}' 2>/dev/null) || {{ echo 'readelf -d failed on td-login (cannot verify absence of dynamic NEEDED)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$dout\" | grep -qi 'NEEDED'; then echo 'td-login has a dynamic NEEDED entry — its runtime closure is not empty' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // Applet roster: the /bin symlink farm this multicall backs is generated
    // from `--list`, so a dropped or renamed applet must red here rather than
    // strand a dead /bin/login on the image. `verify-credentials` must NOT be
    // listed — it is a probe, and a farm entry for it would be a /bin name no
    // list in system-x86-64 accounts for.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "l=$('{bin}' --list) || {{ echo 'td-login --list failed' >&2; exit 1; }}; \
                     for a in login su; do \
                         printf '%s\\n' \"$l\" | grep -q -x -F \"$a\" || {{ echo \"td-login does not serve applet '$a'\" >&2; exit 1; }}; \
                     done; \
                     n=$(printf '%s\\n' \"$l\" | wc -l); \
                     [ \"$n\" -eq 2 ] || {{ echo \"td-login serves $n applets, expected exactly 2 — update this check deliberately when adding one\" >&2; exit 1; }}; \
                     if printf '%s\\n' \"$l\" | grep -q -x -F verify-credentials; then echo 'verify-credentials is a probe, not an applet; listing it would put an unaccounted name in the /bin farm' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // Dispatch through BOTH entry forms, plus the documented exit codes and the
    // argv refusals. Every leg here runs UNPRIVILEGED-or-not without switching
    // credentials: each one fails before `creds::apply` is reached.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "'{bin}' no-such-applet >/dev/null 2>&1; \
                     [ $? -eq 2 ] || {{ echo 'td-login must exit 2 on an unknown applet (usage error)' >&2; exit 1; }}; \
                     '{bin}' >/dev/null 2>&1; \
                     [ $? -eq 2 ] || {{ echo 'td-login must exit 2 with no argument at all' >&2; exit 1; }}; \
                     e=$('{bin}' su -Z 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'su accepted an unknown option instead of refusing it' >&2; exit 1; }}; \
                     case \"$e\" in *unrecognised*) : ;; *) echo \"su refused an unknown option for the wrong reason: $e\" >&2; exit 1;; esac; \
                     e=$('{bin}' su -s relative/sh root 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'su accepted a RELATIVE -s shell; it execs by absolute path with no PATH search, so a relative one resolves against the caller directory' >&2; exit 1; }}; \
                     e=$('{bin}' login -f 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'login accepted -f with no user name' >&2; exit 1; }}; \
                     e=$('{bin}' login -f root extra 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'login accepted -f USER together with a bare user name — two different sessions' >&2; exit 1; }}; \
                     d='{{root}}/argv0'; mkdir -p \"$d\"; \
                     ln -sf '{bin}' \"$d/su\" || {{ echo 'could not build the argv[0] symlink' >&2; exit 1; }}; \
                     ln -sf '{bin}' \"$d/login\" || {{ echo 'could not build the argv[0] symlink' >&2; exit 1; }}; \
                     e=$(\"$d/su\" -Z 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'argv[0] dispatch: /bin/su -> td-login did not reach su — this is the form the shipped symlink farm uses' >&2; exit 1; }}; \
                     case \"$e\" in su:*) : ;; *) echo \"argv[0] dispatch reached the wrong applet: $e\" >&2; exit 1;; esac; \
                     e=$(\"$d/login\" --nope 2>&1); \
                     case \"$e\" in login:*) : ;; *) echo \"argv[0] dispatch: /bin/login -> td-login reached the wrong applet: $e\" >&2; exit 1;; esac; \
                     ln -sf '{bin}' \"$d/verify-credentials\" || {{ echo 'could not build the probe symlink' >&2; exit 1; }}; \
                     \"$d/verify-credentials\" --uid 0 --gid 0 >/dev/null 2>&1; \
                     [ $? -eq 2 ] || {{ echo 'the readback probe must NOT be reachable by argv[0]; a /bin symlink for it would be an unaccounted farm name' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // The readback probe, asserted BOTH ways against this sandbox's own
    // credentials. This is the leg the image's TD-LOGIN-RUN-OK marker rests on:
    // a probe that always exits 0 would gate nothing, and the boot oracle would
    // green a switch that left a residual credential behind.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "if [ -r /proc/self/status ]; then \
                         u=$(grep '^Uid:' /proc/self/status | tr -s '\\t' ' ' | cut -d' ' -f2); \
                         g=$(grep '^Gid:' /proc/self/status | tr -s '\\t' ' ' | cut -d' ' -f2); \
                         gr=$(grep '^Groups:' /proc/self/status | cut -f2- | tr ' ' ',' | sed -e 's/,*$//'); \
                         '{bin}' verify-credentials --uid 4294967294 --gid \"$g\" --groups \"$gr\" >/dev/null 2>&1 && \
                             {{ echo 'verify-credentials accepted a WRONG uid — the marker it gates would prove nothing' >&2; exit 1; }}; \
                         '{bin}' verify-credentials --uid \"$u\" --gid 4294967294 --groups \"$gr\" >/dev/null 2>&1 && \
                             {{ echo 'verify-credentials accepted a WRONG gid' >&2; exit 1; }}; \
                         '{bin}' verify-credentials --uid \"$u\" --gid \"$g\" --groups 4294967294 >/dev/null 2>&1 && \
                             {{ echo 'verify-credentials accepted a WRONG supplementary set — a residual group is exactly the escalation it exists to catch' >&2; exit 1; }}; \
                         '{bin}' verify-credentials --gid \"$g\" >/dev/null 2>&1 && \
                             {{ echo 'verify-credentials ran without --uid instead of refusing' >&2; exit 1; }}; \
                         ok=1; why=''; \
                         case \",$gr,\" in *\",$g,\"*) : ;; *) ok=0; why=\"this sandbox's primary gid $g is absent from its own supplementary set [$gr], and the probe folds the gid in the way login/su do\";; esac; \
                         if [ \"$u\" != 0 ]; then \
                             c=$(grep '^CapPrm:' /proc/self/status | cut -f2)$(grep '^CapEff:' /proc/self/status | cut -f2)$(grep '^CapAmb:' /proc/self/status | cut -f2); \
                             case \"$c\" in *[!0]*) ok=0; why=\"this sandbox runs as uid $u while still holding capabilities ($c), which the probe reads as a residual credential\";; esac; \
                         fi; \
                         if [ \"$ok\" = 1 ]; then \
                             '{bin}' verify-credentials --uid \"$u\" --gid \"$g\" --groups \"$gr\" || \
                                 {{ echo \"verify-credentials rejected this process's OWN credentials (uid=$u gid=$g groups=$gr)\" >&2; exit 1; }}; \
                             echo 'note: verify-credentials agreed with /proc/self/status and rejected all four mismatches'; \
                         else \
                             echo \"note: the four mismatch legs ran; the AGREEMENT leg does not apply to this process ($why) — it asserts the shape of a session td-login built, and the image asserts that one through TD-LOGIN-RUN-OK\"; \
                         fi; \
                     else \
                         echo 'note: /proc not mounted in this sandbox; the readback probe is asserted by the boot oracle (TD-LOGIN-RUN-OK)'; \
                     fi; \
                     exit 0"
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
        content: "PASS: td-login is a statically-linked ELF64 x86-64 executable (ET_EXEC) with no PT_INTERP and no dynamic NEEDED entry; it serves exactly the two applets login/su, dispatches through both the argv[0] and `td-login <applet>` forms, keeps verify-credentials off the argv[0] farm, refuses the ambiguous and unknown argv forms, and its credential readback agrees with /proc/self/status while rejecting a wrong uid, gid or supplementary set\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("td-login-test", "1.0")
        .native_inputs(&["td-login", "binutils-x86-64-self", "busybox-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check td-login-test: build-plan --auto builds td-login (td's static credential multicall: login/su, statically linked by the /td/store target Rust + native GCC/binutils/glibc toolchain), asserts a self-contained static ELF64 x86-64 executable (ET_EXEC, no PT_INTERP, no dynamic NEEDED), and exercises the applet roster, both dispatch forms, the argv refusals, and the credential readback probe the image's TD-LOGIN-RUN-OK marker rests on"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-login-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
