use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// td-svc-test: build-shape AND behavioural validation of the service supervisor.
//
// Per repo policy that recipes test their output, this asserts the shipped
// td-svc binary is the self-contained STATIC ELF its slot requires, re-proving
// with an independent readelf walk what the producer's `assert_static`
// fail-closes on:
//   1. ELF64 x86-64 *executable* (readelf: class ELF64, machine x86-64, type
//      EXEC) — EXEC (not DYN) is the non-PIE static shape,
//   2. NO PT_INTERP program header,
//   3. NO dynamic NEEDED entry — an EMPTY runtime closure.
//
// It then EXERCISES the validator. `td-svc check` is what the image build runs
// over the shipped table, so its EXIT STATUS is load-bearing: a table that
// silently validated would put the ordering regression on the machine instead
// of in the build. Each refusal leg drives a distinct rejection, because a
// validator that exits 1 for the wrong reason is a validator that will exit 0
// for the wrong reason later.
//
// The `kill` leg is here for a reason worth stating: td-svc's stop path (a
// later landing) signals a process GROUP by shelling out to the uutils
// `/bin/kill` with a negative operand, which is the crate's one unverified
// external assumption — a leading-dash operand can be read as a flag. Proving
// the pinned uutils accepts it HERE means landing 3 builds on a checked fact
// rather than on faith. It is gated on the staged multicall actually running,
// so this recipe stays green wherever it runs.
//
// It exercises the argv SHAPE td-svc composes — `kill -SIG -PGID`, one signal
// name and one negative operand — against this shell's own process group, with
// SIGCONT, which is a no-op for a group that is not stopped. (td-svc sends
// -TERM/-KILL; neither can be aimed at this shell's own group without killing
// the build, and the question under test is how the OPERAND parses, which is
// the same for every signal name.) Earlier it drove `kill -0 -- -1`, which is
// neither the form td-svc uses nor a safe operand — `-1` is every process the
// caller may signal — and it accepted any status but 125/2, so uutils' own
// exit-1 usage error read as a pass.
pub fn recipe() -> Recipe {
    let bin = "{in:td-svc}/bin/td-svc";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let uu = "{in:uutils}/bin/coreutils";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{bin}' 2>/dev/null) || {{ echo 'readelf -h failed on td-svc' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || {{ echo 'td-svc is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'td-svc is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -qE 'Type:[[:space:]]+EXEC([[:space:]]|$)' || {{ echo 'td-svc is not a static ET_EXEC — a DYN/PIE would need runtime relocation, and PID 1s only child must come up before any closure' >&2; exit 1; }}"
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
                    "lout=$('{readelf}' -l '{bin}' 2>/dev/null) || {{ echo 'readelf -l failed on td-svc (cannot verify absence of PT_INTERP)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$lout\" | grep -qi 'INTERP'; then echo 'td-svc carries a PT_INTERP program header — it is not static' >&2; exit 1; fi"
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
                    "dout=$('{readelf}' -d '{bin}' 2>/dev/null) || {{ echo 'readelf -d failed on td-svc (cannot verify absence of dynamic NEEDED)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$dout\" | grep -qi 'NEEDED'; then echo 'td-svc has a dynamic NEEDED entry — its runtime closure is not empty' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // Fixture tables, written as files rather than heredocs so the shell layer
    // stays a plain invocation.
    steps.push(Step::WriteFile {
        path: "{root}/good.conf".into(),
        content: "# the shipped boot chain's shape\n\
                  [hostname]\ntype=oneshot\nexec=/bin/hostname -F /etc/hostname\n\n\
                  [td-firstboot]\ntype=oneshot\nexec=/bin/td-firstboot provision\n\n\
                  [rootcheck]\ntype=oneshot\nexec=/etc/rootcheck\nafter=td-firstboot\n\n\
                  [netup]\ntype=oneshot\nexec=/etc/netup\nafter=rootcheck\n\n\
                  [sshd]\ntype=daemon\nexec=/bin/sshd serve\nafter=netup,td-firstboot\nrestart=always\nready=/bin/td-netd reach 127.0.0.1 22\n\n\
                  [greeter]\ntype=daemon\nexec=/etc/tty-session\nafter=netup\ntty=ttyS0\nrestart=always\n"
            .into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{root}/cycle.conf".into(),
        content: "[a]\ntype=oneshot\nexec=/bin/true\nafter=b\n\n\
                  [b]\ntype=oneshot\nexec=/bin/true\nafter=a\n"
            .into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{root}/unknown.conf".into(),
        content: "[a]\ntype=oneshot\nexec=/bin/true\nafter=nosuchunit\n".into(),
        exec: false,
    });
    // The console-is-never-skippable invariant (DESIGN.md I5).
    steps.push(Step::WriteFile {
        path: "{root}/skippable-console.conf".into(),
        content: "[netup]\ntype=oneshot\nexec=/etc/netup\n\n\
                  [greeter]\ntype=daemon\nexec=/etc/tty-session\ntty=ttyS0\nrequires=netup\n"
            .into(),
        exec: false,
    });
    // A key whose behaviour has not landed is REFUSED, not accepted and
    // ignored — a table that takes log= while output still goes to td-svc's
    // stderr promises a file that will never exist.
    steps.push(Step::WriteFile {
        path: "{root}/unimplemented-key.conf".into(),
        content: "[greeter]\ntype=daemon\nexec=/etc/tty-session\ntty=ttyS0\nlog=/var/log/g.log\n"
            .into(),
        exec: false,
    });
    // A line the parser cannot read has an UNKNOWN intent, so it must fail its
    // stanza rather than start the service without it.
    steps.push(Step::WriteFile {
        path: "{root}/malformed-line.conf".into(),
        content: "[svc]\ntype=daemon\nexec=/bin/sshd serve\nrequires firewall\n".into(),
        exec: false,
    });
    // A key the supervisor would silently ignore reads in the table as a
    // guarantee it does not make.
    steps.push(Step::WriteFile {
        path: "{root}/oneshot-restart.conf".into(),
        content: "[a]\ntype=oneshot\nexec=/bin/true\nrestart=always\n".into(),
        exec: false,
    });

    // The validator: a clean table exits 0 and prints the resolved order with
    // dependencies first; each malformed table exits 1 naming its own fault.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "o=$('{bin}' check -f '{{root}}/good.conf') || {{ echo 'check rejected a valid table' >&2; exit 1; }}; \
                     printf '%s\\n' \"$o\" | grep -q 'td-firstboot' || {{ echo 'check printed no order' >&2; exit 1; }}; \
                     fb=$(printf '%s\\n' \"$o\" | grep -n 'td-firstboot' | cut -d: -f1); \
                     rc=$(printf '%s\\n' \"$o\" | grep -n 'rootcheck'    | cut -d: -f1); \
                     nu=$(printf '%s\\n' \"$o\" | grep -n 'netup'        | cut -d: -f1); \
                     sd=$(printf '%s\\n' \"$o\" | grep -n 'sshd'         | cut -d: -f1); \
                     gr=$(printf '%s\\n' \"$o\" | grep -n 'greeter'      | cut -d: -f1); \
                     [ \"$fb\" -lt \"$rc\" ] || {{ echo 'td-firstboot must precede rootcheck — it mints the identity rootcheck asserts' >&2; exit 1; }}; \
                     [ \"$rc\" -lt \"$nu\" ] || {{ echo 'rootcheck must precede netup' >&2; exit 1; }}; \
                     [ \"$nu\" -lt \"$sd\" ] || {{ echo 'netup must precede sshd — sshd binds loopback' >&2; exit 1; }}; \
                     [ \"$nu\" -lt \"$gr\" ] || {{ echo 'netup must precede the greeter' >&2; exit 1; }}"
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
                    "e=$('{bin}' check -f '{{root}}/cycle.conf' 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'check accepted a dependency cycle' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'cycle' || {{ echo \"a cycle must be named as one, got: $e\" >&2; exit 1; }}; \
                     e=$('{bin}' check -f '{{root}}/unknown.conf' 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'check accepted a dependency on a unit that does not exist' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'unknown unit' || {{ echo \"an unknown dependency must be named, got: $e\" >&2; exit 1; }}; \
                     e=$('{bin}' check -f '{{root}}/skippable-console.conf' 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'check accepted a console unit made skippable by requires= — DESIGN.md I5' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'never skippable' || {{ echo \"the console invariant must be named, got: $e\" >&2; exit 1; }}; \
                     e=$('{bin}' check -f '{{root}}/unimplemented-key.conf' 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'check accepted log=, whose behaviour has not landed — it would silently do nothing' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'not implemented yet' || {{ echo \"an unimplemented key must be named as one, got: $e\" >&2; exit 1; }}; \
                     e=$('{bin}' check -f '{{root}}/malformed-line.conf' 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'check accepted a stanza with an unparseable line — the service would run without the requires= it names' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'expected key=value' || {{ echo \"an unparseable line must be named, got: $e\" >&2; exit 1; }}; \
                     e=$('{bin}' check -f '{{root}}/oneshot-restart.conf' 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'check accepted restart= on a oneshot, which the supervisor would silently ignore' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'restart= applies to daemons' || {{ echo \"restart= on a oneshot must be named, got: $e\" >&2; exit 1; }}; \
                     e=$('{bin}' check -f '{{root}}/nosuchfile.conf' 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'check accepted an unreadable table' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'nosuchfile.conf' || {{ echo \"an unreadable table must be named, got: $e\" >&2; exit 1; }}; \
                     '{bin}' bogus-subcommand >/dev/null 2>&1; \
                     [ $? -eq 2 ] || {{ echo 'td-svc must exit 2 on an unknown subcommand (usage error)' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // `run`, not just `check`. Everything above exercises the VALIDATOR; the
    // event loop and the ordering it actually APPLIES were proven only by host
    // unit tests, never by the shipped static binary. This leg supervises for
    // real: two oneshots, the second ordered after the first, each writing a
    // marker. `second`'s marker is the one waited on, so its existence proves
    // both that the loop spawns and that it released a unit whose dependency
    // had to settle first; `first`'s distinguishes "nothing ran" from "the
    // ordering never released the dependent".
    steps.push(Step::WriteFile {
        path: "{root}/run.conf".into(),
        content: format!(
            "[first]\ntype=oneshot\nexec={POST_BOOTSTRAP_SH} -c 'echo 1 > {{root}}/ran-first'\n\n\
             [second]\ntype=oneshot\nexec={POST_BOOTSTRAP_SH} -c 'echo 2 > {{root}}/ran-second'\nafter=first\n"
        ),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "'{bin}' run -f '{{root}}/run.conf' >/dev/null 2>&1 & \
                     p=$!; \
                     i=0; \
                     while [ $i -lt 30 ]; do \
                         [ -f '{{root}}/ran-second' ] && break; \
                         i=$((i+1)); \
                         sleep 1; \
                     done; \
                     kill $p 2>/dev/null; \
                     : 'and WAIT for it to go. This supervisor binds the fixed'; \
                     : 'control socket path, so a leg that starts the next one'; \
                     : 'while this is still dying can see a live socket, decline'; \
                     : 'to bind, and then lose it when this process exits.'; \
                     wait $p 2>/dev/null || :; \
                     [ -f '{{root}}/ran-first' ] || {{ echo 'td-svc run started no service at all — the event loop is dead in the shipped static binary' >&2; exit 1; }}; \
                     [ -f '{{root}}/ran-second' ] || {{ echo 'td-svc run started the first unit but never released the one ordered after it' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // The control socket, driven by the shipped binary on BOTH ends.
    //
    // Host tests cover the protocol and the state machine; what they cannot
    // cover is that this static binary binds a real `AF_UNIX` socket under
    // `/run` and answers itself over it. The socket path is fixed (an operator
    // has to be able to find it), so this leg needs a writable `/run` and says
    // so rather than silently passing when it has none — the same shape as the
    // `kill` leg below.
    steps.push(Step::WriteFile {
        path: "{root}/ctl.conf".into(),
        content: format!(
            "[held-open]\ntype=daemon\nexec={POST_BOOTSTRAP_SH} -c 'while : ; do sleep 1; done'\n\
             restart=always\n"
        ),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "if mkdir -p /run/td-svc 2>/dev/null; then \
                         '{bin}' run -f '{{root}}/ctl.conf' >/dev/null 2>&1 & \
                         p=$!; \
                         i=0; \
                         while [ $i -lt 30 ]; do \
                             o=$('{bin}' status 2>/dev/null) && \
                             case \"$o\" in *'held-open ready'*) break;; esac; \
                             i=$((i+1)); \
                             sleep 1; \
                         done; \
                         o=$('{bin}' status 2>&1); \
                         case \"$o\" in \
                             *'held-open ready'*) : ;; \
                             *) kill $p 2>/dev/null; echo \"td-svc status never reported the daemon ready over the control socket, got: $o\" >&2; exit 1;; \
                         esac; \
                         s=$('{bin}' stop held-open 2>&1) || {{ kill $p 2>/dev/null; echo \"td-svc stop failed: $s\" >&2; exit 1; }}; \
                         i=0; \
                         while [ $i -lt 30 ]; do \
                             o=$('{bin}' status held-open 2>/dev/null); \
                             case \"$o\" in *'held-open stopped'*) break;; esac; \
                             i=$((i+1)); \
                             sleep 1; \
                         done; \
                         case \"$o\" in \
                             *'held-open stopped'*) : ;; \
                             *) kill $p 2>/dev/null; echo \"a restart=always daemon did not reach stopped after td-svc stop, got: $o\" >&2; exit 1;; \
                         esac; \
                         : 'and it must STAY stopped. Reaching `stopped` once proves'; \
                         : 'the TERM landed; the failure mode this guards is the unit'; \
                         : 'coming straight back, which only a later look can see.'; \
                         sleep 3; \
                         o=$('{bin}' status held-open 2>&1); \
                         case \"$o\" in \
                             *'held-open stopped'*) : ;; \
                             *) kill $p 2>/dev/null; echo \"a stopped restart=always daemon came back on its own, got: $o\" >&2; exit 1;; \
                         esac; \
                         r=$('{bin}' restart held-open 2>&1) || {{ kill $p 2>/dev/null; echo \"td-svc restart failed: $r\" >&2; exit 1; }}; \
                         i=0; \
                         while [ $i -lt 30 ]; do \
                             o=$('{bin}' status held-open 2>/dev/null); \
                             case \"$o\" in *'held-open ready'*) break;; esac; \
                             i=$((i+1)); \
                             sleep 1; \
                         done; \
                         case \"$o\" in \
                             *'held-open ready'*) : ;; \
                             *) kill $p 2>/dev/null; echo \"td-svc restart did not bring a stopped daemon back, got: $o\" >&2; exit 1;; \
                         esac; \
                         e=$('{bin}' status nosuchunit 2>&1); \
                         case \"$e\" in \
                             *'no such service'*) : ;; \
                             *) kill $p 2>/dev/null; echo \"an unknown unit must be named, got: $e\" >&2; exit 1;; \
                         esac; \
                         kill $p 2>/dev/null; \
                     else \
                         echo 'note: /run is not writable in this sandbox; the control socket is re-proved by the boot oracle'; \
                     fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // The one external assumption the no-unsafe stop path rests on: that the
    // pinned uutils `kill` reads `-<pgid>` as a process group and not as a flag.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "if '{uu}' true >/dev/null 2>&1; then \
                         '{uu}' kill -l >/dev/null 2>&1 || {{ echo 'uutils kill -l failed — the applet is not usable' >&2; exit 1; }}; \
                         if [ -r /proc/self/stat ]; then \
                             pg=$(cut -d' ' -f5 /proc/self/stat); \
                             case \"$pg\" in ''|*[!0-9]*) echo \"could not read this shells process group from /proc (got '$pg')\" >&2; exit 1;; esac; \
                             '{uu}' kill -CONT \"-$pg\" >/dev/null 2>&1; \
                             s=$?; \
                             [ \"$s\" -eq 0 ] || {{ echo \"the pinned uutils kill rejected 'kill -CONT -$pg' (exit $s) — that is the exact argv td-svcs stop path composes, one signal name and one negative process-group operand\" >&2; exit 1; }}; \
                         else \
                             echo 'note: /proc is not readable in this sandbox; the negative process-group operand is re-proved by the boot oracle'; \
                         fi; \
                     else \
                         echo 'note: staged uutils multicall did not run in this sandbox; the kill operand form is re-proved by the boot oracle'; \
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
        content: "PASS: td-svc is a statically-linked ELF64 x86-64 executable (ET_EXEC) with no PT_INTERP and no dynamic NEEDED entry; `check` resolves the shipped boot chain in dependency order (td-firstboot before rootcheck before netup before sshd and the greeter), and refuses — each with a distinct named reason and a non-zero exit — a dependency cycle, a dependency on a unit that does not exist, a console unit made skippable by requires=, a key whose behaviour has not landed (log=), a stanza line that is not key=value, restart= on a oneshot, and an unreadable table; an unknown subcommand exits 2; `td-svc run` supervises for real in the shipped static binary — two oneshots, the second ordered after the first, both spawned, the dependent released only once its dependency settled; where a writable /run lets it, the shipped binary binds the control socket under /run/td-svc and answers ITSELF over it — `status` reports a supervised daemon ready, `stop` drives a restart=always daemon to `stopped` and it is STILL stopped three seconds later (the failure mode being that it comes straight back), `restart` brings it up again, and an unknown unit is named rather than silently accepted; and where the sandbox stages the uutils multicall over a readable /proc, `kill -CONT -<pgid>` — the same one-signal-name-and-one-negative-operand argv shape td-svc's stop path composes with -TERM/-KILL — exits 0, so the negative process-group operand is not read as a flag\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("td-svc-test", "1.0")
        .native_inputs(&[
            "td-svc",
            "binutils-x86-64-self",
            "busybox-x86-64",
            "uutils",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-svc-test: build-plan --auto builds td-svc (td's static service supervisor: dependency ordering, restart backoff, readiness probing, and — in later landings — log capture, ordered shutdown and Ctrl-Alt-Del, statically linked by the /td/store target Rust + native GCC/binutils/glibc toolchain), asserts a self-contained static ELF64 x86-64 executable (ET_EXEC, no PT_INTERP, no dynamic NEEDED), and exercises the table validator: the shipped boot chain's order, the refusal paths for a cycle, an unknown dependency, a skippable console, an unimplemented key, an unparseable stanza line, restart= on a oneshot, and an unreadable table; and `td-svc run` actually supervising two ordered oneshots in the shipped static binary"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-svc-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
