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
pub fn recipe() -> Recipe {
    let bin = "{in:td-svc}/bin/td-svc";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
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
                  [sshd]\ntype=daemon\nexec=/bin/sshd -D -e -f /etc/ssh/sshd_config\nafter=netup,td-firstboot\nrestart=always\nready=/bin/td-netd reach 127.0.0.1 22\n\n\
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
    // A captured stream is a pipe and job control needs a terminal, so a unit
    // cannot be both a console and captured (DESIGN.md §7).
    steps.push(Step::WriteFile {
        path: "{root}/tty-and-log.conf".into(),
        content: "[greeter]\ntype=daemon\nexec=/etc/tty-session\ntty=ttyS0\nlog=/var/log/g.log\n"
            .into(),
        exec: false,
    });
    // Nothing to copy: `console=` without `log=` is the accepted-and-ignored
    // key the table forbids.
    steps.push(Step::WriteFile {
        path: "{root}/console-without-log.conf".into(),
        content: "[a]\ntype=daemon\nexec=/bin/true\nconsole=yes\n".into(),
        exec: false,
    });
    // A line the parser cannot read has an UNKNOWN intent, so it must fail its
    // stanza rather than start the service without it.
    steps.push(Step::WriteFile {
        path: "{root}/malformed-line.conf".into(),
        content: "[svc]\ntype=daemon\nexec=/bin/sshd -D\nrequires firewall\n".into(),
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
                     e=$('{bin}' check -f '{{root}}/tty-and-log.conf' 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'check accepted tty= with log= — a captured stream is a pipe, and job control needs a terminal' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'mutually exclusive' || {{ echo \"the tty=/log= exclusion must be named, got: $e\" >&2; exit 1; }}; \
                     e=$('{bin}' check -f '{{root}}/console-without-log.conf' 2>&1); \
                     [ $? -ne 0 ] || {{ echo 'check accepted console= with nothing captured to copy' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'needs log=' || {{ echo \"console= without log= must be named, got: $e\" >&2; exit 1; }}; \
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
                         : 'Reap it before the next leg binds the same socket path:'; \
                         : 'a supervisor still dying answers clear_stale, so bind gets'; \
                         : 'AddrInUse and the next one runs with no socket at all.'; \
                         wait $p 2>/dev/null || :; \
                     else \
                         echo 'note: /run is not writable in this sandbox; the control socket is re-proved by the boot oracle'; \
                     fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // The shutdown path, end to end in the shipped binary. Two legs, because the
    // interesting halves fail differently: a `reboot` REQUEST must record itself
    // and tear the services down, and a supervisor that comes back to a recorded
    // shutdown must NOT start anything (DESIGN.md I6).
    //
    // Neither leg can watch the handoff itself: `finish_shutdown` execs
    // /bin/<applet>, which does not exist in this sandbox, so td-svc logs the
    // failure and parks rather than exiting. Both legs kill it instead of
    // waiting on it, and assert through the marker and the filesystem — the
    // control socket stops being answered the moment the teardown finishes, so
    // a `status` poll for the final state would race the handoff.
    steps.push(Step::WriteFile {
        path: "{root}/down.conf".into(),
        content: format!(
            "[held-open]\ntype=daemon\n\
             exec={POST_BOOTSTRAP_SH} -c 'echo $$ > /run/td-svc/held.pid; while : ; do sleep 1; done'\n\
             restart=always\n"
        ),
        exec: false,
    });
    // A oneshot whose only job is to leave a trace if it ever runs.
    steps.push(Step::WriteFile {
        path: "{root}/resume.conf".into(),
        content: format!(
            "[tracer]\ntype=oneshot\nexec={POST_BOOTSTRAP_SH} -c 'echo ran > /run/td-svc/tracer-ran'\n"
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
                         rm -f /run/td-svc/shutdown /run/td-svc/tracer-ran /run/td-svc/held.pid; \
                         '{bin}' run -f '{{root}}/down.conf' >/dev/null 2>&1 & \
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
                             *) kill $p 2>/dev/null; echo \"td-svc never brought the daemon up before the shutdown leg, got: $o\" >&2; exit 1;; \
                         esac; \
                         r=$('{bin}' reboot 2>&1); \
                         case \"$r\" in \
                             *'reboot requested'*) : ;; \
                             *) kill $p 2>/dev/null; echo \"td-svc reboot was not accepted, got: $r\" >&2; exit 1;; \
                         esac; \
                         : 'The marker is written BEFORE anything is stopped, so it is'; \
                         : 'already on disk by the time the reply comes back.'; \
                         m=$(cat /run/td-svc/shutdown 2>&1); \
                         case \"$m\" in \
                             reboot) : ;; \
                             *) kill $p 2>/dev/null; echo \"the shutdown marker does not name the power action, got: $m\" >&2; exit 1;; \
                         esac; \
                         : 'And the teardown must actually stop the daemon. Watch the'; \
                         : 'DAEMON, not the socket: once the last unit is down td-svc'; \
                         : 'execs the applet - absent here - and parks, so it stops'; \
                         : 'answering status. Polling for the final phase would time'; \
                         : 'out over and over and then report a failure that did not'; \
                         : 'happen. The pid the daemon published stays readable after'; \
                         : 'the process it names is gone, so there is nothing to poll'; \
                         : 'the supervisor for. (kill -0 is pid-reuse sensitive in'; \
                         : 'principle; over this 30s window in a fresh pid namespace'; \
                         : 'with one spawner it is not a real hazard.)'; \
                         d=$(cat /run/td-svc/held.pid 2>/dev/null); \
                         case \"$d\" in \
                             ''|*[!0-9]*) kill $p 2>/dev/null; echo \"the daemon never published its pid, got: '$d'\" >&2; exit 1;; \
                         esac; \
                         i=0; \
                         while [ $i -lt 30 ]; do \
                             kill -0 \"$d\" 2>/dev/null || break; \
                             i=$((i+1)); \
                             sleep 1; \
                         done; \
                         kill $p 2>/dev/null; \
                         wait $p 2>/dev/null || :; \
                         if kill -0 \"$d\" 2>/dev/null; then \
                             echo 'the teardown did not stop a restart=always daemon' >&2; \
                             exit 1; \
                         fi; \
                         : 'I6: a supervisor that arrives to a recorded shutdown resumes'; \
                         : 'the teardown; it must never start a service. PID 1 respawns'; \
                         : 'td-svc unconditionally, so this is the boot after a crash'; \
                         : 'mid-teardown, and starting the tracer here would mean bringing'; \
                         : 'services up against filesystems /etc/shutdown has released.'; \
                         printf reboot > /run/td-svc/shutdown; \
                         rm -f /run/td-svc/tracer-ran; \
                         '{bin}' run -f '{{root}}/resume.conf' >/dev/null 2>&1 & \
                         q=$!; \
                         sleep 5; \
                         kill $q 2>/dev/null; \
                         wait $q 2>/dev/null || :; \
                         if [ -e /run/td-svc/tracer-ran ]; then \
                             echo 'td-svc started a service while a shutdown was recorded (I6) - a crash mid-teardown would bring services back up against released filesystems' >&2; \
                             exit 1; \
                         fi; \
                         rm -f /run/td-svc/shutdown /run/td-svc/tracer-ran /run/td-svc/held.pid; \
                     else \
                         echo 'note: /run is not writable in this sandbox; the shutdown path is re-proved by the boot oracle'; \
                     fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // Log capture, in the SHIPPED binary: a unit with log= has BOTH its
    // streams captured into the file it names, in a directory td-svc creates
    // 0700. Rotation and the shutdown close are crate tests — this leg is the
    // one that runs the real static binary against a real filesystem.
    steps.push(Step::WriteFile {
        path: "{root}/capture.conf".into(),
        content: format!(
            "[talker]\ntype=oneshot\n\
             exec={POST_BOOTSTRAP_SH} -c 'echo out-line; echo err-line >&2'\n\
             log={{root}}/captured/talker.log\n"
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
                         rm -f /run/td-svc/started /run/td-svc/shutdown; \
                         rm -rf '{{root}}/captured'; \
                         '{bin}' run -f '{{root}}/capture.conf' >/dev/null 2>&1 & \
                         p=$!; \
                         log='{{root}}/captured/talker.log'; \
                         i=0; \
                         while [ $i -lt 60 ]; do \
                             if [ -f \"$log\" ] && grep -q out-line \"$log\" && grep -q err-line \"$log\"; then break; fi; \
                             i=$((i+1)); sleep 1; \
                         done; \
                         kill $p 2>/dev/null; wait $p 2>/dev/null || :; \
                         [ -f \"$log\" ] || {{ echo 'no log file was created' >&2; exit 1; }}; \
                         grep -q out-line \"$log\" || {{ echo \"stdout was not captured, log holds: $(cat \"$log\")\" >&2; exit 1; }}; \
                         : 'stderr is the stream a services failures arrive on, and'; \
                         : 'the one it is easiest to leave unwired: separate pipe,'; \
                         : 'separate drain thread.'; \
                         grep -q err-line \"$log\" || {{ echo \"stderr was not captured, log holds: $(cat \"$log\")\" >&2; exit 1; }}; \
                         : 'And the directory td-svc created for it is not one'; \
                         : 'anyone else may drop a file into: PID 1 leaves a'; \
                         : 'permissive umask, so the mode has to be stated.'; \
                         d=$(ls -ld '{{root}}/captured' | cut -c1-10); \
                         case \"$d\" in \
                             drwx------) : ;; \
                             *) echo \"the log directory is $d, not drwx------\" >&2; exit 1;; \
                         esac; \
                         rm -rf '{{root}}/captured'; \
                         echo 'the shipped supervisor captured both streams into its log'; \
                     else \
                         echo 'note: no writable /run here; capture is covered by the crate tests'; \
                     fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // Eviction, in the SHIPPED binary: a supervisor that starts with a record
    // of what a PREVIOUS one left running kills it before starting anything.
    // Without this a td-svc death leaves the machine running two of
    // everything, and the second copy is unsupervised.
    //
    // The record is written by hand here because the failure being proved is
    // the SUCCESSOR's, not the writer's: what matters is that a td-svc which
    // finds a live pid in that file acts on it. `starttime` is read out of
    // /proc the same way the crate does — everything after the `) ` that ends
    // comm, then field 20 — because a record with the wrong one is exactly
    // what td-svc is supposed to ignore.
    steps.push(Step::WriteFile {
        path: "{root}/evict.conf".into(),
        content: format!(
            "[tracer]\ntype=oneshot\nexec={POST_BOOTSTRAP_SH} -c 'echo ran > /run/td-svc/evict-ran'\n"
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
                    "if mkdir -p /run/td-svc 2>/dev/null && [ -r /proc/self/stat ]; then \
                         rm -f /run/td-svc/started /run/td-svc/shutdown /run/td-svc/evict-ran; \
                         '{POST_BOOTSTRAP_SH}' -c 'while : ; do sleep 1; done' & \
                         op=$!; \
                         sleep 1; \
                         s=$(cat /proc/$op/stat 2>/dev/null); \
                         case \"$s\" in '') echo 'the stand-in orphan never started' >&2; exit 1;; esac; \
                         r=${{s#*') '}}; \
                         st=$(echo \"$r\" | cut -d' ' -f20); \
                         case \"$st\" in ''|*[!0-9]*) echo \"no starttime for the orphan, got: '$st'\" >&2; exit 1;; esac; \
                         echo \"$op $st 0 tracer\" > /run/td-svc/started; \
                         '{bin}' run -f '{{root}}/evict.conf' >/dev/null 2>&1 & \
                         p=$!; \
                         : 'A background job of a non-interactive shell leads no group,'; \
                         : 'so td-svc classifies it Process(pid) and signals it alone.'; \
                         : 'It is this shells child, so the kill leaves a zombie until'; \
                         : 'the wait below - hence state Z counts as evicted, not alive.'; \
                         i=0; alive=yes; \
                         while [ $i -lt 60 ]; do \
                             s=$(cat /proc/$op/stat 2>/dev/null); \
                             case \"$s\" in '') alive=no; break;; esac; \
                             r=${{s#*') '}}; \
                             case \"$(echo \"$r\" | cut -d' ' -f1)\" in Z) alive=no; break;; esac; \
                             i=$((i+1)); sleep 1; \
                         done; \
                         : 'And the unit still runs: eviction cleared the way, it did'; \
                         : 'not refuse the very unit it had just cleared.'; \
                         j=0; ran=no; \
                         while [ $j -lt 30 ]; do \
                             if [ -f /run/td-svc/evict-ran ]; then ran=yes; break; fi; \
                             j=$((j+1)); sleep 1; \
                         done; \
                         kill $p 2>/dev/null; wait $p 2>/dev/null || :; \
                         kill $op 2>/dev/null; wait $op 2>/dev/null || :; \
                         if [ \"$alive\" = yes ]; then \
                             echo 'the recorded orphan survived; a duplicate would have followed' >&2; exit 1; \
                         fi; \
                         if [ \"$ran\" != yes ]; then \
                             echo 'the unit never started after its orphan was evicted' >&2; exit 1; \
                         fi; \
                         rm -f /run/td-svc/started /run/td-svc/evict-ran; \
                         echo 'the shipped supervisor evicted a recorded orphan, then started its unit'; \
                     else \
                         echo 'note: no writable /run or no /proc here; eviction is covered by the crate tests'; \
                     fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // The Ctrl-Alt-Del sentinel, in the SHIPPED binary. Arming turns entirely
    // on this process being alive while td-svc holds the write end of its
    // stdin, and dying when that end closes: a sentinel that returned at once
    // would leave every arming to die instantly, re-arming on the backoff
    // forever with the kernel's own hard reset already disabled. Unit tests
    // cannot reach it — under a test harness `current_exe` is libtest, which
    // reads `cad-sentinel` as a filter and exits — so this is the only place
    // the applet itself runs.
    //
    // A FIFO plus a held fd is exactly td-svc's own mechanism. `exec 9>` also
    // unblocks the sentinel's `open` for read, so the ordering here is the
    // ordering arming uses.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "d=/tmp/td-svc-cad-$$; \
                     if mkdir -p \"$d\" 2>/dev/null && mkfifo \"$d/pipe\" 2>/dev/null; then \
                         '{bin}' cad-sentinel < \"$d/pipe\" & \
                         p=$!; \
                         exec 9> \"$d/pipe\"; \
                         sleep 2; \
                         : 'Still held: it must be alive, and not a zombie.'; \
                         st=`cut -d' ' -f3 /proc/$p/stat 2>/dev/null || echo '?'`; \
                         if [ \"$st\" = Z ] || [ \"$st\" = '?' ]; then \
                             echo \"FAIL: the sentinel did not block while its pipe was held (state $st)\" >&2; \
                             exec 9>&-; exit 1; \
                         fi; \
                         : 'Let go: it must exit promptly, and cleanly.'; \
                         exec 9>&-; \
                         wait $p; rc=$?; \
                         if [ \"$rc\" -ne 0 ]; then \
                             echo \"FAIL: the sentinel exited $rc when its pipe closed\" >&2; \
                             exit 1; \
                         fi; \
                         echo 'the cad sentinel blocked while the pipe was held and exited 0 when it closed'; \
                         rm -rf \"$d\"; \
                     else \
                         echo 'note: no writable /tmp or no mkfifo in this sandbox; the cad sentinel is re-proved by the boot oracle'; \
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
        content: "PASS: td-svc is a statically-linked ELF64 x86-64 executable (ET_EXEC) with no PT_INTERP and no dynamic NEEDED entry; `check` resolves the shipped boot chain in dependency order (td-firstboot before rootcheck before netup before sshd and the greeter), and refuses — each with a distinct named reason and a non-zero exit — a dependency cycle, a dependency on a unit that does not exist, a console unit made skippable by requires=, tty= together with log= (a captured stream is a pipe, and job control needs a terminal), console= with nothing captured to copy, a stanza line that is not key=value, restart= on a oneshot, and an unreadable table; an unknown subcommand exits 2; `td-svc run` supervises for real in the shipped static binary — two oneshots, the second ordered after the first, both spawned, the dependent released only once its dependency settled; where a writable /run lets it, the shipped binary binds the control socket under /run/td-svc and answers ITSELF over it — `status` reports a supervised daemon ready, `stop` drives a restart=always daemon to `stopped` and it is STILL stopped three seconds later (the failure mode being that it comes straight back), `restart` brings it up again, and an unknown unit is named rather than silently accepted; td-svc signals with `kill(2)` itself, so there is no longer an external `kill` whose operand parsing has to be taken on trust, and the teardown leg above drives that in the shipped binary — a restart=always daemon is signalled through its process group and stays down; the Ctrl-Alt-Del sentinel blocks for as long as the pipe td-svc holds is open and exits when it is closed, which is the whole arming mechanism; and log capture this landing added puts BOTH of a unit's streams into the file its log= names, in a directory td-svc creates drwx------\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("td-svc-test", "1.0")
        .native_inputs(&["td-svc", "binutils-x86-64-self", "busybox-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-svc-test: build-plan --auto builds td-svc (td's static service supervisor: dependency ordering, restart backoff, readiness probing, ordered shutdown, Ctrl-Alt-Del, and — in a later landing — log capture, statically linked by the /td/store target Rust + native GCC/binutils/glibc toolchain), asserts a self-contained static ELF64 x86-64 executable (ET_EXEC, no PT_INTERP, no dynamic NEEDED), and exercises the table validator: the shipped boot chain's order, the refusal paths for a cycle, an unknown dependency, a skippable console, an unimplemented key, an unparseable stanza line, restart= on a oneshot, and an unreadable table; and `td-svc run` actually supervising two ordered oneshots in the shipped static binary; and the shutdown path end to end - a `reboot` request records its marker and tears a restart=always daemon down, and a supervisor that starts with a marker already on disk resumes the teardown instead of starting anything; and the Ctrl-Alt-Del sentinel applet, which blocks while the pipe its parent holds is open and exits 0 when it closes"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-svc-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
