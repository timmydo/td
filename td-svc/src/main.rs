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
        "usage: td-svc <check|run> [-f FILE]\n  \
         check  validate FILE and print the start order (exit 1 on any complaint)\n  \
         run    supervise the services in FILE\n\
         FILE defaults to {DEFAULT_PATH}"
    )
}

/// What an argv means, split out so the routing is testable without running
/// a supervisor.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Check { path: String },
    Run { path: String },
    Usage(String),
}

fn route(args: &[String]) -> Route {
    let Some(verb) = args.first() else {
        return Route::Usage("no subcommand".into());
    };
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
        Route::Run { path } => {
            let (units, problems) = load(&path);
            for problem in &problems {
                emit_err(&format!("td-svc: {problem}\n"));
            }
            let (mut runtime, complaints) = supervise::Runtime::new(units);
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
