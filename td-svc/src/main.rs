#![forbid(unsafe_code)]
//! td-svc — td's service supervisor.
//!
//! PID 1 stays tiny and signal-free (a blocking `wait4(-1)` is its whole event
//! loop); everything that needs ordering, restart policy, log capture, an
//! ordered shutdown, or Ctrl-Alt-Del lives here instead.
//!
//! `DESIGN.md` beside this file is the normative specification, including the
//! invariants no compiler can check: the crate carries NO `unsafe` (unlike
//! td-init/td-login/td-netd/td-kexec, it is not a target-side exception), it
//! never uses `pre_exec`, liveness is read from `/proc` rather than inferred
//! from an exit status, and the console is never skippable.
//!
//! `td-svc check [-f FILE]` validates a table and prints the start order it
//! would use. `td-svc run [-f FILE]` supervises.

mod backoff;
mod control;
mod order;
mod procfs;
mod supervise;
mod table;

use std::io::Write;
use std::process::ExitCode;

const DEFAULT_PATH: &str = "/etc/td-svc.conf";

/// Write to stdout, treating a closed reader as a clean exit. `print!` PANICS
/// when the write fails, and Rust leaves SIGPIPE ignored, so `td-svc check |
/// head` would abort the process — which the no-panic rule forbids.
pub(crate) fn emit(text: &str) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Diagnostics to stderr. On a booted machine this is the console log, and
/// there is nowhere to report a failure to report a failure, so drop it.
pub(crate) fn emit_err(text: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes()).and_then(|()| err.flush());
}

fn usage() -> String {
    format!(
        "usage: td-svc <check|run> [-f FILE]\n       \
         td-svc <status|start|stop|restart> [NAME]\n  \
         check    validate FILE and print the start order (exit 1 on any complaint)\n  \
         run      supervise the services in FILE\n  \
         status   what the running supervisor is doing (all units, or NAME)\n  \
         start    start NAME, clearing any restart backoff\n  \
         stop     stop NAME and keep it stopped\n  \
         restart  stop NAME, then start it again\n\
         FILE defaults to {DEFAULT_PATH}; the last four talk to {socket}"
    ,
        socket = control::PATH
    )
}

/// What an argv means, split out so the routing is testable without running
/// a supervisor.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Check { path: String },
    Run { path: String },
    /// A request for the RUNNING supervisor, sent over the control socket.
    /// The verb and its argument travel verbatim — this process parses only
    /// enough to know it is not `check`/`run`, so the one authority on what a
    /// request means stays `Runtime::control`.
    Ctl { request: String },
    Usage(String),
}

/// The verbs that address a running supervisor rather than a file.
const CTL_VERBS: [&str; 4] = ["status", "start", "stop", "restart"];

fn route(args: &[String]) -> Route {
    let Some(verb) = args.first() else {
        return Route::Usage("no subcommand".into());
    };
    if CTL_VERBS.contains(&verb.as_str()) {
        // `-f` is not accepted here: it names a TABLE, and these verbs do not
        // read one. Silently ignoring it would let `td-svc stop -f other.conf`
        // look like it addressed another supervisor.
        for arg in args.iter().skip(1) {
            if arg.starts_with('-') {
                return Route::Usage(format!("{verb}: unexpected option '{arg}'"));
            }
        }
        return Route::Ctl {
            request: args.join(" "),
        };
    }
    let mut path = DEFAULT_PATH.to_string();
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-f" | "--file" => match rest.next() {
                Some(value) => path = value.clone(),
                None => return Route::Usage(format!("{arg} needs a path")),
            },
            other => return Route::Usage(format!("unexpected argument '{other}'")),
        }
    }
    match verb.as_str() {
        "check" => Route::Check { path },
        "run" => Route::Run { path },
        other => Route::Usage(format!("unknown subcommand '{other}'")),
    }
}

/// Read and parse a table, folding a read failure into the same complaint list
/// as a parse failure so callers have one thing to check.
fn load(path: &str) -> (Vec<table::Unit>, Vec<String>) {
    match std::fs::read_to_string(path) {
        Ok(text) => table::parse(&text),
        Err(e) => (Vec::new(), vec![format!("{path}: {e}")]),
    }
}

/// The report `check` prints: complaints first, then the resolved order.
///
/// Complaints come first deliberately — read through a pipe, a report that
/// ended with them would put the important part where `head` cannot see it.
fn report(units: &[table::Unit], problems: &[String]) -> (String, bool) {
    let plan = order::plan(units);
    let mut out = String::new();
    let mut clean = true;
    for problem in problems.iter().chain(plan.complaints().iter()) {
        out.push_str(&format!("td-svc: {problem}\n"));
        clean = false;
    }
    if plan.order.is_empty() {
        out.push_str("(no startable units)\n");
    } else {
        for (n, name) in plan.order.iter().enumerate() {
            out.push_str(&format!("{}. {name}\n", n + 1));
        }
    }
    (out, clean)
}

/// Collect argv without `std::env::args()`, which PANICS on an argument that is
/// not UTF-8. A path is bytes to the kernel, so that is reachable input, and
/// `panic=abort` would make it the end of the supervisor. A non-UTF-8 argument
/// is refused with usage instead.
fn args_utf8() -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for arg in std::env::args_os().skip(1) {
        match arg.into_string() {
            Ok(arg) => out.push(arg),
            Err(bad) => return Err(format!("argument {bad:?} is not UTF-8")),
        }
    }
    Ok(out)
}

fn main() -> ExitCode {
    let args = match args_utf8() {
        Ok(args) => args,
        Err(why) => {
            emit_err(&format!("td-svc: {why}\n{}\n", usage()));
            return ExitCode::from(2);
        }
    };
    match route(&args) {
        Route::Usage(why) => {
            emit_err(&format!("td-svc: {why}\n{}\n", usage()));
            ExitCode::from(2)
        }
        Route::Check { path } => {
            let (units, problems) = load(&path);
            let (text, clean) = report(&units, &problems);
            if let Err(e) = emit(&text) {
                emit_err(&format!("td-svc: {e}\n"));
                return ExitCode::FAILURE;
            }
            if clean {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Route::Ctl { request } => match control::ask(control::PATH, &request) {
            Ok(reply) => {
                if let Err(e) = emit(&reply) {
                    emit_err(&format!("td-svc: {e}\n"));
                    return ExitCode::FAILURE;
                }
                // The supervisor reports a refusal in the text, so the exit
                // status has to reflect it or a script cannot tell.
                if reply.starts_with("error:") {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(e) => {
                emit_err(&format!("td-svc: {e}\n"));
                ExitCode::FAILURE
            }
        },
        Route::Run { path } => {
            let (units, problems) = load(&path);
            for problem in &problems {
                emit_err(&format!("td-svc: {problem}\n"));
            }
            let (mut runtime, complaints) = supervise::Runtime::new(units, &path);
            control::spawn(runtime.events());
            for complaint in &complaints {
                emit_err(&format!("td-svc: {complaint}\n"));
            }
            runtime.run()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    #[test]
    fn subcommands_route_with_the_default_path() {
        assert_eq!(
            route(&argv(&["check"])),
            Route::Check {
                path: DEFAULT_PATH.into()
            }
        );
        assert_eq!(
            route(&argv(&["run"])),
            Route::Run {
                path: DEFAULT_PATH.into()
            }
        );
    }

    #[test]
    fn an_explicit_file_overrides_the_default() {
        assert_eq!(
            route(&argv(&["check", "-f", "/tmp/t.conf"])),
            Route::Check {
                path: "/tmp/t.conf".into()
            }
        );
        assert_eq!(
            route(&argv(&["check", "--file", "/tmp/t.conf"])),
            Route::Check {
                path: "/tmp/t.conf".into()
            }
        );
    }

    /// A typo must not be read as "use the default table and supervise" — on a
    /// booted machine that would start services nobody asked for.
    #[test]
    fn a_malformed_argv_is_usage_not_a_default() {
        assert!(matches!(route(&argv(&[])), Route::Usage(_)));
        assert!(matches!(route(&argv(&["runn"])), Route::Usage(_)));
        assert!(matches!(route(&argv(&["check", "-f"])), Route::Usage(_)));
        assert!(matches!(
            route(&argv(&["check", "extra"])),
            Route::Usage(_)
        ));
    }

    /// The four control verbs address the RUNNING supervisor, not a file, and
    /// the whole request travels verbatim so `Runtime::control` stays the one
    /// authority on what a verb means.
    #[test]
    fn the_control_verbs_route_to_the_socket_not_to_a_table() {
        for (argv, expect) in [
            (vec!["status"], "status"),
            (vec!["status", "sshd"], "status sshd"),
            (vec!["stop", "sshd"], "stop sshd"),
            (vec!["restart", "greeter"], "restart greeter"),
            (vec!["start", "netup"], "start netup"),
        ] {
            let args: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
            assert_eq!(
                route(&args),
                Route::Ctl {
                    request: expect.to_string()
                },
                "{argv:?} did not route to the control socket"
            );
        }
    }

    /// `-f` names a TABLE, and a control verb does not read one. Accepting and
    /// ignoring it would make `td-svc stop -f other.conf` look like it
    /// addressed a different supervisor when it addressed the only one there is.
    #[test]
    fn a_control_verb_refuses_a_table_option_rather_than_ignoring_it() {
        let args: Vec<String> = ["stop", "-f", "/etc/other.conf"]
            .iter()
            .map(|a| (*a).to_string())
            .collect();
        assert!(
            matches!(route(&args), Route::Usage(why) if why.contains("unexpected option")),
            "a control verb accepted -f"
        );
    }

    /// ...while `check` and `run` still take one, and still default it.
    #[test]
    fn check_and_run_still_take_a_table_path() {
        let args: Vec<String> = vec!["run".to_string()];
        assert_eq!(
            route(&args),
            Route::Run {
                path: DEFAULT_PATH.to_string()
            }
        );
        let args: Vec<String> = ["check", "-f", "/tmp/x.conf"]
            .iter()
            .map(|a| (*a).to_string())
            .collect();
        assert_eq!(
            route(&args),
            Route::Check {
                path: "/tmp/x.conf".to_string()
            }
        );
    }

    /// A client with no supervisor to talk to must say so in terms an operator
    /// can act on. A bare ENOENT sends them looking for a missing file.
    #[test]
    fn asking_a_socket_that_is_not_there_names_the_likely_cause() {
        let e = control::ask("/nonexistent/td-svc/control", "status").unwrap_err();
        assert!(
            e.contains("is td-svc running?"),
            "an absent supervisor reported {e:?}"
        );
    }

    #[test]
    fn a_clean_table_reports_its_order_and_succeeds() {
        let (units, problems) = table::parse(
            "[b]\ntype=oneshot\nexec=/bin/true\nafter=a\n[a]\ntype=oneshot\nexec=/bin/true\n",
        );
        let (text, clean) = report(&units, &problems);
        assert!(clean);
        assert!(text.contains("1. a"), "{text}");
        assert!(text.contains("2. b"), "{text}");
    }

    /// Exit status is what the image build keys on, so a table with any
    /// complaint must be non-clean even though it also produced usable units.
    #[test]
    fn a_table_with_any_complaint_is_not_clean() {
        let (units, problems) =
            table::parse("[a]\ntype=oneshot\nexec=/bin/true\n[b]\ntype=bogus\nexec=/x\n");
        let (text, clean) = report(&units, &problems);
        assert!(!clean);
        assert!(text.contains("unknown type"));
        // ...and the good unit is still reported, so the operator sees both.
        assert!(text.contains("1. a"));
    }

    #[test]
    fn complaints_precede_the_order_so_a_piped_report_shows_them() {
        let (units, problems) = table::parse("[a]\ntype=oneshot\nexec=/x\nafter=ghost\n");
        let (text, _) = report(&units, &problems);
        let complaint = text.match_indices("unknown unit").next().unwrap().0;
        let order = text.match_indices("no startable units").next().unwrap().0;
        assert!(complaint < order);
    }

    #[test]
    fn an_unreadable_table_is_a_complaint_not_a_panic() {
        let (units, problems) = load("/nonexistent/td-svc.conf");
        assert!(units.is_empty());
        assert_eq!(problems.len(), 1);
        let (_, clean) = report(&units, &problems);
        assert!(!clean);
    }
}
