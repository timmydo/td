// `deny`, not `forbid`: `sys.rs` carries ONE scoped `#[allow]` on the
// `syscall` body, and `forbid` cannot be relaxed by an inner attribute — that
// is what it is for. The roster is held by main.rs's confinement tests, which
// count what the compiler cannot.
#![deny(unsafe_code)]
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
mod cad;
mod control;
mod evict;
mod order;
mod procfs;
mod supervise;
mod sys;
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
         td-svc <status|start|stop|restart> [NAME]\n       \
         td-svc <reload|reboot|poweroff|halt>\n  \
         check     validate FILE and print the start order (exit 1 on any complaint)\n  \
         run       supervise the services in FILE\n  \
         status    what the running supervisor is doing (all units, or NAME)\n  \
         start     start NAME, clearing any restart backoff\n  \
         stop      stop NAME and keep it stopped\n  \
         restart   stop NAME, then start it again\n  \
         reload    re-read FILE; on any complaint the running table is kept\n  \
         reboot    stop every service, run /etc/shutdown, then reset\n  \
         poweroff  the same, then power off\n  \
         halt      the same, then halt\n\
         FILE defaults to {DEFAULT_PATH}; everything but check/run talks to {socket}\n\
         ({sentinel} is internal: `run` spawns it to catch Ctrl-Alt-Del, and it \
         blocks on stdin until its parent lets go)"
    ,
        socket = control::PATH,
        sentinel = cad::SENTINEL_VERB
    )
}

/// What an argv means, split out so the routing is testable without running
/// a supervisor.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Check { path: String },
    Run { path: String },
    /// The Ctrl-Alt-Del sentinel: block until our parent lets go. Not a control
    /// verb — it addresses no supervisor and reads no table.
    CadSentinel,
    /// A request for the RUNNING supervisor, sent over the control socket.
    /// The verb and its argument travel verbatim — this process parses only
    /// enough to know it is not `check`/`run`, so the one authority on what a
    /// request means stays `Runtime::control`.
    Ctl { request: String },
    Usage(String),
}

/// The verbs that address a running supervisor rather than a file.
const CTL_VERBS: [&str; 8] = [
    "status", "start", "stop", "restart", "reload", "reboot", "poweroff", "halt",
];

fn route(args: &[String]) -> Route {
    let Some(verb) = args.first() else {
        return Route::Usage("no subcommand".into());
    };
    if verb == cad::SENTINEL_VERB {
        // Takes nothing. An argument here means whoever typed it expected this
        // to do something, and it does exactly one thing.
        if let Some(extra) = args.get(1) {
            return Route::Usage(format!("{verb}: takes no argument (got {extra:?})"));
        }
        return Route::CadSentinel;
    }
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
        Route::CadSentinel => match cad::sentinel() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                emit_err(&format!("td-svc: {e}\n"));
                ExitCode::FAILURE
            }
        },
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
            (vec!["reload"], "reload"),
            (vec!["reboot"], "reboot"),
            (vec!["poweroff"], "poweroff"),
            (vec!["halt"], "halt"),
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
        // And EVERY verb, so adding one to CTL_VERBS without routing it — or
        // dropping one — cannot pass. `/etc/tty-session` ends by exec'ing
        // `td-svc reboot`; if that stopped reaching the socket the greeter
        // could no longer end a boot.
        for verb in CTL_VERBS {
            let args = vec![verb.to_string()];
            assert_eq!(
                route(&args),
                Route::Ctl {
                    request: verb.to_string()
                },
                "{verb} is a control verb but did not route to the socket"
            );
        }
    }

    /// The sentinel verb routes to the sentinel.
    ///
    /// `arm_cad` spawns THIS binary with this argv and nothing else checks
    /// that the two agree. A verb that stopped routing would exit with a usage
    /// error the instant it started, so every arming would "succeed" and the
    /// sentinel would die at once — leaving the machine re-arming on the
    /// backoff forever, unarmed, with the kernel's own hard reset disabled.
    /// Asserted against `cad::SENTINEL_VERB` rather than a literal, because
    /// the spawner reads that constant too: a test written against the string
    /// would still pass if both sides were renamed apart from the router.
    #[test]
    fn the_sentinel_verb_routes_to_the_sentinel() {
        assert_eq!(
            route(&argv(&[cad::SENTINEL_VERB])),
            Route::CadSentinel,
            "the argv arm_cad spawns does not reach the sentinel"
        );
        // And it is not a control verb, or it would be sent to the socket of a
        // supervisor that is trying to spawn it.
        assert!(
            !CTL_VERBS.contains(&cad::SENTINEL_VERB),
            "the sentinel verb is also a control verb"
        );
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

#[cfg(test)]
mod confinement {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use std::path::Path;

    /// Every source file of the crate, READ FROM DISK rather than listed here.
    ///
    /// A hand-written list is a hole: adding `mod newmod;` alongside a
    /// `newmod.rs` full of `unsafe` would leave every assertion below passing
    /// because they never saw the file. The directory is the authority.
    /// Sub-directories too: `mod deep;` inside a module puts `deep.rs` one level
    /// down, and a scan that stopped at the top would never read it.
    /// Files are keyed by their path RELATIVE to `src/`, never by basename: a
    /// `decoy/sys.rs` is not the `sys.rs` the crate compiles, and a key that
    /// could not tell them apart would let the decoy answer for the real file.
    /// A non-`.rs` file under `src/` is not inert — it is exactly what the two
    /// refused constructs compile — so it is collected separately rather than
    /// skipped, and asserted absent below.
    fn collect(base: &Path, dir: &Path, out: &mut Vec<(String, String)>, other: &mut Vec<String>) {
        let entries = std::fs::read_dir(dir).unwrap();
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(base, &path, out, other);
                continue;
            }
            let name = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_str()
                .unwrap_or_default()
                .to_string();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                other.push(name);
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            out.push((name, strip_comments(&text)));
        }
    }

    fn walk() -> (Vec<(String, String)>, Vec<String>) {
        let base = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
        let (mut out, mut other) = (Vec::new(), Vec::new());
        collect(base, base, &mut out, &mut other);
        out.sort();
        other.sort();
        (out, other)
    }

    fn sources() -> Vec<(String, String)> {
        walk().0
    }

    /// The scans below read tokens off raw text, so a comment BETWEEN two tokens
    /// slips past all of them: `un`+`safe /* here */ {` is one construct to the
    /// compiler. Rust's lexer is not reachable from a test, so this is the part
    /// that matters — comments, and the literals a comment marker hides inside.
    /// Newlines are preserved so the per-line scans still see lines.
    fn strip_comments(text: &str) -> String {
        let src: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        let at = |i: usize| src.get(i).copied();
        while i < src.len() {
            let c = at(i).unwrap_or(' ');
            if c == '/' && at(i + 1) == Some('/') {
                while i < src.len() && at(i) != Some('\n') {
                    i += 1;
                }
                continue;
            }
            if c == '/' && at(i + 1) == Some('*') {
                let mut depth = 1usize; // Rust's block comments nest.
                let mut newlines = 0usize;
                i += 2;
                while i < src.len() && depth > 0 {
                    if at(i) == Some('/') && at(i + 1) == Some('*') {
                        depth += 1;
                        i += 2;
                    } else if at(i) == Some('*') && at(i + 1) == Some('/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        if at(i) == Some('\n') {
                            newlines += 1;
                        }
                        i += 1;
                    }
                }
                // A space, so the tokens either side stay separate tokens.
                out.push(' ');
                for _ in 0..newlines {
                    out.push('\n');
                }
                continue;
            }
            // Raw strings hold no escapes, so their terminator is the quote
            // followed by as many hashes as opened them. Modelling them is not
            // optional: a raw string containing an unbalanced `/*` would
            // otherwise open a block comment that swallows the rest of the file,
            // hiding a real unsafe block from the count while it still compiles.
            if c == 'r' || (c == 'b' && at(i + 1) == Some('r')) {
                let mut k = if c == 'b' { i + 2 } else { i + 1 };
                let mut hashes = 0usize;
                while at(k) == Some('#') {
                    hashes += 1;
                    k += 1;
                }
                if at(k) == Some('"') {
                    for j in i..=k {
                        if let Some(ch) = at(j) {
                            out.push(ch);
                        }
                    }
                    i = k + 1;
                    while let Some(ch) = at(i) {
                        if ch == '"' {
                            let mut seen = 0usize;
                            while seen < hashes && at(i + 1 + seen) == Some('#') {
                                seen += 1;
                            }
                            if seen == hashes {
                                for j in 0..=hashes {
                                    if let Some(c2) = at(i + j) {
                                        out.push(c2);
                                    }
                                }
                                i += hashes + 1;
                                break;
                            }
                        }
                        out.push(ch);
                        i += 1;
                    }
                    continue;
                }
            }
            if c == '"' {
                out.push(c);
                i += 1;
                while i < src.len() {
                    let s = at(i).unwrap_or(' ');
                    out.push(s);
                    i += 1;
                    if s == '\\' {
                        if let Some(escaped) = at(i) {
                            out.push(escaped);
                            i += 1;
                        }
                        continue;
                    }
                    if s == '"' {
                        break;
                    }
                }
                continue;
            }
            // A quote opens a char literal or a lifetime. A lifetime is a quote,
            // an identifier, and NO closing quote; only a char literal can hold a
            // comment marker, and it is copied whole so `'/'` cannot open one.
            if c == '\'' {
                let ident = at(i + 1).is_some_and(|n| n.is_alphabetic() || n == '_');
                if !(ident && at(i + 2) != Some('\'')) {
                    let mut k = i + 1;
                    let mut close = None;
                    while let Some(ch) = at(k) {
                        match ch {
                            '\\' => k += 2,
                            '\'' => {
                                close = Some(k);
                                break;
                            }
                            _ => k += 1,
                        }
                    }
                    if let Some(end) = close {
                        for j in i..=end {
                            if let Some(ch) = at(j) {
                                out.push(ch);
                            }
                        }
                        i = end + 1;
                        continue;
                    }
                }
            }
            out.push(c);
            i += 1;
        }
        out
    }

    fn source(name: &str) -> String {
        for (n, text) in sources() {
            if n == name {
                return text;
            }
        }
        String::new()
    }

    /// Where a `mod` line declared inside `file` looks for its submodule.
    fn submodule_dir(file: &str) -> String {
        if file == "main.rs" {
            String::new()
        } else {
            format!("{}/", file.trim_end_matches(".rs"))
        }
    }

    /// Reading the directory only equals reading `main.rs` if the two agree, so
    /// assert it: a `mod` whose file the scan missed — an `#[path]` attribute
    /// pointing outside `src/`, say — would make every assertion below vacuous
    /// for exactly the file that needed checking.
    #[test]
    fn the_scan_covers_every_module_the_crate_declares() {
        let files = sources();
        let mut declared = Vec::new();
        for (path, text) in &files {
            for line in text.lines() {
                // `pub mod` and `pub(crate) mod` declare a module just as much.
                let mut line = line.trim();
                if let Some(unprefixed) = line.strip_prefix("pub") {
                    let unprefixed = unprefixed.trim_start();
                    line = match unprefixed.strip_prefix('(') {
                        Some(vis) => match vis.split_once(')') {
                            Some((_, after)) => after.trim_start(),
                            None => unprefixed,
                        },
                        None => unprefixed,
                    };
                }
                let Some(rest) = line.strip_prefix("mod ") else {
                    continue;
                };
                let Some(name) = rest.strip_suffix(';') else {
                    continue; // an inline `mod tests {` block, already in this file
                };
                let target = format!("{}{name}.rs", submodule_dir(path));
                assert!(
                    files.iter().any(|(f, _)| *f == target),
                    "module '{target}' is declared but was not scanned"
                );
                declared.push(target);
            }
        }
        assert_eq!(
            declared.len(),
            9,
            "expected nine modules beside the crate root"
        );
        // ...and nothing scanned is orphaned: a file present but declared by no
        // `mod` line is either dead or reached a way this scan does not model.
        for (path, _) in &files {
            assert!(
                path == "main.rs" || declared.iter().any(|d| d == path),
                "'{path}' was scanned but no `mod` line declares it"
            );
        }
    }

    /// `src/` holds these ten files and nothing else.
    ///
    /// The scan above proves every `mod` line has a file and every file has a
    /// `mod` line, which is a closed loop that says nothing about WHICH files:
    /// a `mod` line and its file added together satisfy both halves. This pins
    /// the set the AGENTS.md amendment was written against.
    ///
    /// The second half is why the collector keeps non-`.rs` files rather than
    /// skipping them: `src/sys.inc` is invisible to a `.rs`-only scan and
    /// compiles perfectly well through the constructs refused below.
    #[test]
    fn src_holds_exactly_the_ten_scanned_modules() {
        let (rs, other) = walk();
        let paths: Vec<&str> = rs.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "backoff.rs",
                "cad.rs",
                "control.rs",
                "evict.rs",
                "main.rs",
                "order.rs",
                "procfs.rs",
                "supervise.rs",
                "sys.rs",
                "table.rs",
            ],
            "the crate's file set changed"
        );
        assert!(
            other.is_empty(),
            "src/ holds a non-.rs file no scan here reads: {other:?}"
        );
    }

    /// What follows each `unsafe` keyword, with everything the compiler treats
    /// as noise in between removed: any gap (one space, several, a newline, a
    /// stripped comment, or none at all before a brace) and the `extern "ABI"`
    /// of an unsafe foreign item. Reading the following TOKEN rather than
    /// matching a fixed spelling is what stops this whole family of evasions —
    /// `unsafe  fn` and `unsafe extern "C" fn` are the same item to rustc.
    fn after_unsafe(text: &str) -> Vec<&str> {
        let word = concat!("un", "safe");
        let mut out = Vec::new();
        for (offset, _) in text.match_indices(word) {
            let Some(rest) = text.get(offset + word.len()..) else {
                continue;
            };
            // `unsafe_code` is ONE identifier; a brace or a gap means two tokens.
            if rest.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let mut rest = rest.trim_start();
            while let Some(tail) = rest.strip_prefix("extern") {
                rest = tail.trim_start();
                let Some(tail) = rest.strip_prefix('"') else {
                    break;
                };
                match tail.split_once('"') {
                    Some((_, after)) => rest = after.trim_start(),
                    None => break,
                }
            }
            out.push(rest);
        }
        out
    }

    fn unsafe_blocks(text: &str) -> usize {
        after_unsafe(text)
            .iter()
            .filter(|rest| rest.starts_with('{'))
            .count()
    }

    fn unsafe_items(text: &str, keyword: &str) -> usize {
        after_unsafe(text)
            .iter()
            .filter(|rest| match rest.strip_prefix(keyword) {
                Some(after) => !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'),
                None => false,
            })
            .count()
    }

    /// Every source with whitespace squeezed out. Whitespace is not a token
    /// boundary the compiler cares about — `# [path`, `include !` and
    /// `macro_rules !` all compile — so a construct refused by exact substring
    /// is one space away from being allowed.
    fn squeeze(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    fn squeezed() -> String {
        let mut out = String::new();
        for (_, text) in sources() {
            out.push_str(&squeeze(&text));
            out.push('\n');
        }
        out
    }

    /// Count `allow(..)` groups that name the unsafe lint anywhere inside them,
    /// so a multi-lint allow is worth exactly as much as a lone one.
    fn unsafe_allows(text: &str) -> usize {
        let lint = concat!("un", "safe_code");
        let mut count = 0;
        // `expect` re-permits the lint exactly as `allow` does — only noisily
        // when unused — so both are counted. Whitespace before the group is
        // legal Rust and is skipped rather than assumed absent: an
        // `#[allow (…)]` the scan walked past would be a permission nobody sees.
        for keyword in ["allow", "expect"] {
            for (offset, _) in text.match_indices(keyword) {
                let Some(rest) = text.get(offset + keyword.len()..) else {
                    continue;
                };
                let Some(rest) = rest.trim_start().strip_prefix('(') else {
                    continue;
                };
                let group = match rest.match_indices(')').next() {
                    Some((end, _)) => rest.get(..end).unwrap_or(rest),
                    None => rest,
                };
                if group
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|t| t == lint)
                {
                    count += 1;
                }
            }
        }
        count
    }

    /// The syscall AGENTS.md records for this crate, with the x86_64 number it
    /// must carry. A SECOND is a reviewed amendment; this is what makes that
    /// more than an aspiration.
    ///
    /// The NUMBER is pinned, not just the name: renumbering `SYS_KILL` to a
    /// neighbour would otherwise change which kernel call this crate makes
    /// while every name-based assertion still passed. 62 is `kill`; 61 is
    /// `wait4`, which is what a fat-finger would reach.
    const AMENDED: &[(&str, &str)] = &[("SYS_KILL", "62")];

    const CALL: &str = concat!("sys", "call2");

    #[test]
    fn exactly_one_scoped_allow_and_one_unsafe_block_in_the_whole_crate() {
        // Every spelling counts against the same budget of one: the outer form,
        // the inner form a new module could carry at its top, and a multi-lint
        // group with the lint somewhere in the middle of it.
        assert_eq!(
            sources().iter().map(|(_, t)| unsafe_allows(t)).sum::<usize>(),
            1,
            "the crate must carry exactly ONE scoped unsafe allow (the asm body in sys.rs)"
        );
        assert_eq!(
            sources().iter().map(|(_, t)| unsafe_blocks(t)).sum::<usize>(),
            1,
            "the crate must carry exactly ONE unsafe block (the asm body in sys.rs)"
        );
        for form in ["fn", "impl", "trait"] {
            assert_eq!(
                sources().iter().map(|(_, t)| unsafe_items(t, form)).sum::<usize>(),
                0,
                "no item of this form may exist in this crate: {form}"
            );
        }
        // These would make the scanned text stop describing the compiled crate,
        // and every assertion here reads text.
        let squeezed = squeezed();
        for construct in [
            concat!("[", "path="),
            concat!(",", "path="),
            concat!("inc", "lude!"),
            concat!("macro_", "rules!"),
        ] {
            assert_eq!(
                squeezed.matches(construct).count(),
                0,
                "`{construct}` would decouple the compiled crate from the scanned source"
            );
        }
    }

    /// The scoped allow is only sound under a crate-level `deny`.
    ///
    /// td-svc was `forbid(unsafe_code)` until it took `kill(2)`. `forbid` cannot
    /// be relaxed by an inner attribute, which is exactly why it had to become
    /// `deny` — and `deny` CAN be relaxed, so the count below is what stops a
    /// second module quietly allowing itself the same thing.
    #[test]
    fn the_unsafe_lint_is_named_exactly_twice() {
        let squeezed = squeezed();
        let lint = concat!("unsafe_", "code");
        assert_eq!(
            squeezed.matches(lint).count(),
            2,
            "the unsafe lint must be named exactly twice: the crate deny and the one scoped allow"
        );
        assert_eq!(
            squeezed
                .matches(&format!("{}![deny({lint})]", "#"))
                .count(),
            1,
            concat!("the crate root must DENY unsafe", "_code")
        );
        assert_eq!(
            squeezed.matches(&format!("{}![forbid({lint})]", "#")).count(),
            0,
            "forbid cannot host the scoped allow sys.rs needs; this must be deny"
        );
    }

    /// Inline assembly is reachable only through the one pinned body.
    #[test]
    fn the_only_route_to_inline_assembly_is_the_pinned_one() {
        let squeezed = squeezed();
        for form in [
            concat!("as", "m!"),
            concat!("global_as", "m!"),
            concat!("naked_as", "m!"),
        ] {
            let expected = usize::from(form == concat!("as", "m!"));
            assert_eq!(
                squeezed.matches(form).count(),
                expected,
                "inline assembly outside the one pinned body: {form}"
            );
        }
    }

    /// The roster is exactly one syscall, declared in one place, value-pinned.
    ///
    /// Read off the WHITESPACE-SQUEEZED source, because a line is not a unit
    /// Rust cares about: two `const SYS_…` items sharing one line are two items
    /// to the compiler, and the second shadows the roster's at every call site
    /// below it. A line-oriented scan with a fixed-spacing prefix would see one
    /// and pass.
    #[test]
    fn the_syscall_surface_is_exactly_one() {
        let sys = squeeze(&source("sys.rs"));
        let mut declared: Vec<(String, String)> = Vec::new();
        for (offset, _) in sys.match_indices(DECL) {
            let rest = sys.get(offset + DECL.len()..).unwrap_or_default();
            if let Some((name, value)) = rest.split_once(':') {
                let number = value
                    .strip_prefix("usize=")
                    .unwrap_or(value)
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                declared.push((format!("{}{name}", "SYS_"), number));
            }
        }
        // Every declaration the scan SAW was also parsed. Without this the
        // roster is a claim about the declarations that happened to match a
        // spelling, and one written any other way is simply absent from it.
        assert_eq!(
            declared.len(),
            sys.matches(DECL).count(),
            "a syscall-number declaration was not parsed"
        );
        declared.sort();
        let expected: Vec<(String, String)> = AMENDED
            .iter()
            .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
            .collect();
        assert_eq!(declared, expected, "the confined syscall roster changed");

        // And nowhere else may declare one, or the roster above is a claim
        // about a single file rather than about the crate.
        for (name, text) in sources() {
            if name == "sys.rs" {
                continue;
            }
            assert!(
                !squeeze(&text).contains(DECL),
                "{name} declares a syscall number; the roster lives in sys.rs"
            );
        }

        // Every mention of the prefix is accounted for: one declaration and
        // one call-site use per amended syscall, and NOTHING else. Without
        // this the roster is a claim about `const` items only — a `static
        // SYS_KILL: usize = 200;` inside the wrapper shadows the roster's at
        // the point of use, selects `tkill(2)`, and leaves the declaration
        // scan, the call-site pin (which matches the NAME) and the named
        // -constant check all green.
        assert_eq!(
            sys.matches(concat!("SYS", "_")).count(),
            2 * AMENDED.len(),
            "sys.rs names a syscall constant somewhere that is neither its one \
             declaration nor its one call site; it may be shadowed there"
        );
    }

    /// How a syscall number is declared, assembled rather than written out: the
    /// scan above refuses this spelling in every file but `sys.rs`, and this
    /// file is one of the sources it reads.
    const DECL: &str = concat!("const", "SYS_");

    /// The call site selects its syscall through the ONE named constant, and
    /// the entry point is named nowhere but at its definition and that call.
    ///
    /// Without the first half the roster is only a claim about declarations: a
    /// bare `(999, ...)` would reach an unaudited kernel call while leaving the
    /// constant untouched. Without the second, `let f = syscall2;` binds the
    /// entry point to a name this scan does not know and every later call goes
    /// through it — the pin below still matches its one site, and the crate has
    /// a syscall surface nobody counted.
    #[test]
    fn every_syscall_call_site_uses_a_named_constant() {
        let text = source("sys.rs");
        let text = text.as_str();
        let mut sites = 0usize;
        let mut mentions = 0usize;
        let mut selected: Vec<String> = Vec::new();
        for (offset, _) in text.match_indices(CALL) {
            mentions += 1;
            let after = match text.get(offset + CALL.len()..) {
                Some(a) => match a.trim_start().strip_prefix('(') {
                    Some(args) => args.trim_start(),
                    None => continue,
                },
                None => continue,
            };
            // The definition itself takes `(n: usize, ...)`.
            if after.starts_with("n: usize") {
                continue;
            }
            sites += 1;
            let selector: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            assert!(
                AMENDED.iter().any(|(name, _)| *name == selector),
                "called with '{selector}', which is not the one amended syscall"
            );
            // The constant must BE the argument, not the start of an expression:
            // `SYS_KILL + 1` selects a different call while spelling an audited
            // name.
            let rest = after.get(selector.len()..).unwrap_or("").trim_start();
            assert!(
                rest.starts_with(','),
                "the selector must be '{selector}' itself, not an expression built from it"
            );
            selected.push(selector);
        }
        selected.sort();
        let roster: Vec<String> = AMENDED.iter().map(|(n, _)| (*n).to_string()).collect();
        assert_eq!(selected, roster, "the amended syscall is issued exactly once");
        assert_eq!(sites, 1, "expected exactly one call site");
        // ...and the definition, and NOTHING else. The loop skips any mention
        // not followed by `(`, which is the function ITEM: bind it once and
        // every later call goes through a name this scan does not know.
        assert_eq!(
            mentions,
            sites + 1,
            "mentioned somewhere that is not the call or its definition"
        );
    }

    /// The one permitted `unsafe` block is a REGION, and counting tokens inside
    /// it never bounds it: a second `asm!`, a second instruction in the SAME
    /// `asm!` ("syscall", "syscall"), or a raw pointer dereference all fit
    /// without changing any count — and `#[inline]` then multiplies whatever was
    /// added across every caller. So the block is pinned WHOLE, with whitespace
    /// squeezed out so the pin is immune to reformatting.
    ///
    /// This is what pins the REGISTERS. Everything else here pins what is handed
    /// to `syscall2`; only this pins where those arguments land, and `in("rsi")`
    /// changed to `in("rdx")` compiles, passes every other assertion, and sends
    /// signal 0 to every process the caller can reach.
    #[test]
    fn the_confined_block_is_pinned_whole() {
        const BLOCK: &str = concat!(
            "un",
            "safe{core",
            "::arch::a",
            "sm!(\"syscall\",inlateout(\"rax\")nasisize=>ret,",
            "in(\"rdi\")a1,in(\"rsi\")a2,",
            "out(\"rcx\")_,out(\"r11\")_,options(nostack),);}"
        );
        let squeezed = squeezed();
        assert_eq!(
            squeezed.matches(BLOCK).count(),
            1,
            "the confined block's body changed; re-audit it and update this pin"
        );
        assert_eq!(
            squeezed.matches(concat!("a", "sm!")).count(),
            1,
            "exactly one inline-assembly invocation may exist in this crate"
        );
    }

    /// The call site, pinned WHOLE — both registers, not just the selector.
    ///
    /// The roster pins WHICH call; this pins what is handed to it. A target and
    /// a signal swapped past each other would still be `kill(2)`, still use the
    /// named constant, and would send signal-number-`pid` to process `sig`.
    #[test]
    fn every_call_site_is_pinned_whole() {
        const ARGUMENTS: &[&str] = &["(SYS_KILL,targetasisizeasusize,signalasisizeasusize,)"];
        assert_eq!(ARGUMENTS.len(), AMENDED.len(), "one pin per amended syscall");
        let sys = squeeze(&source("sys.rs"));
        for arguments in ARGUMENTS {
            assert_eq!(
                sys.matches(&format!("{CALL}{arguments}")).count(),
                1,
                "the {CALL} call site is not the pinned one: {arguments}"
            );
        }
        // One call site in the crate, and it is in sys.rs.
        assert_eq!(
            squeezed().matches(&format!("{CALL}(")).count(),
            2,
            "the raw entry point must be defined once and called once"
        );
    }

    /// The raw entry point is private to its module.
    #[test]
    fn the_raw_entry_point_is_private_to_its_module() {
        let sys = squeeze(&source("sys.rs"));
        // The scoped allow is part of the pin: matching a bare `fn syscall2(`
        // would let a SECOND, unannotated definition satisfy "declared exactly
        // once" as long as the annotated one moved elsewhere in the file.
        assert_eq!(
            sys.matches(&format!("{}{CALL}(", concat!("#[allow(un", "safe_code)]fn")))
                .count(),
            1,
            "the raw entry point must be declared exactly once, under the scoped allow"
        );
        assert_eq!(
            sys.matches(&format!("fn{CALL}(")).count(),
            1,
            "the raw entry point must be declared exactly once"
        );
        assert_eq!(
            sys.matches(&format!("pubfn{CALL}(")).count(),
            0,
            "the raw entry point must not be public; module privacy is its confinement"
        );
        for (name, text) in sources() {
            if name == "sys.rs" {
                continue;
            }
            assert!(
                !squeeze(&text).contains(CALL),
                "{name} names the raw syscall entry point; only sys.rs may"
            );
        }
    }

    /// Only the one signal helper reaches the wrapper.
    ///
    /// The wrapper is `pub`, so module privacy cannot hold this one — the
    /// scan does. Every signal td-svc sends goes through `send_signal`, which
    /// is where ESRCH is turned into success (I3) and where a refusal is
    /// reported; a second caller would be a second policy.
    #[test]
    fn only_the_signal_path_reaches_the_kill_wrapper() {
        let needle = concat!("sys::", "kill(");
        let mention = concat!("sys::", "kill");
        // NOTHING may name the syscall module in a `use`, in any form. Tokens
        // off the un-squeezed text, because the forms differ only by
        // whitespace once it is gone: `use crate::{sys as signals};` squeezes
        // into one word, and a prefix scan for `use crate::sys` never sees the
        // brace-group or the alias at all. Either would give the one audited
        // call a name the scans below do not look for.
        for (name, text) in sources() {
            for (offset, _) in text.match_indices("use") {
                let rest = text.get(offset..).unwrap_or_default();
                // The keyword, not a word ending in it (`reuse`), and followed
                // by ANY whitespace — a newline or a tab after `use` is the
                // same item to rustc and would slip a fixed `"use "` scan.
                let before_is_word = text
                    .get(..offset)
                    .and_then(|b| b.chars().next_back())
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
                let after_is_space = rest
                    .get(3..)
                    .and_then(|a| a.chars().next())
                    .is_some_and(char::is_whitespace);
                if before_is_word || !after_is_space {
                    continue;
                }
                let item = match rest.match_indices(';').next() {
                    Some((end, _)) => rest.get(..end).unwrap_or(rest),
                    None => rest,
                };
                assert!(
                    !item
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .any(|token| token == "sys"),
                    "{name} imports from the syscall module; every call to it must be \
                     written out in full so the scans below can see it"
                );
            }
        }
        // Every MENTION of the wrapper is a call. `let fire = crate::sys::kill;`
        // is a mention that is not one, and every later `fire(...)` is a call
        // this scan cannot see — the count below would still read 1.
        for (name, text) in sources() {
            let squeezed = squeeze(&text);
            let calls = squeezed.matches(&squeeze(needle)).count();
            let mentions = squeezed.matches(&squeeze(mention)).count();
            assert_eq!(
                mentions, calls,
                "{name} names the kill wrapper somewhere that is not a call; it would be \
                 reachable under another name"
            );
            if name == "supervise.rs" {
                assert_eq!(
                    calls, 1,
                    "supervise.rs must reach kill(2) exactly once, from send_signal"
                );
            } else {
                assert_eq!(calls, 0, "{name} calls the kill wrapper; only send_signal may");
            }
        }
        // Inside sys.rs the wrapper is reachable UNQUALIFIED, which no scan
        // above covers: `sys::kill(` is not how a sibling item there would
        // spell it. Its own tests do exactly that, so the production half is
        // what gets checked — everything before the test module, where the
        // only `kill(` may be the definition itself.
        let sys = squeeze(&source("sys.rs"));
        let production = match sys.match_indices(concat!("#[cfg(", "test)]")).next() {
            Some((at, _)) => sys.get(..at).unwrap_or(&sys),
            None => sys.as_str(),
        };
        assert_eq!(
            production.matches(concat!("kill", "(")).count(),
            1,
            "sys.rs calls its own kill wrapper outside its tests; send_signal is the \
             only caller the roster admits"
        );
        // And that one call is inside `send_signal`.
        let text = source("supervise.rs");
        // `match_indices`, not the obvious lookup method: this file is embedded
        // verbatim into the recipe, and the ladder guard scans that text for
        // host-tool names the method happens to spell.
        let at = text
            .match_indices(needle)
            .next()
            .map(|(i, _)| i)
            .expect("the kill call site vanished");
        let before = text.get(..at).unwrap_or_default();
        let owner = before
            .match_indices("fn ")
            .last()
            .map(|(i, _)| before.get(i..).unwrap_or_default())
            .unwrap_or_default();
        assert!(
            owner.starts_with("fn send_signal"),
            "the kill(2) call must live in send_signal, not in {}",
            owner.lines().next().unwrap_or_default()
        );
    }
}
