#![deny(unsafe_code)]
//! td-login — the static, dependency-free multicall behind td's credential
//! switch: the busybox applets that change who a process is.
//!
//! `login` starts a session for a named user (this is what getty execs, through
//! `/etc/autologin`), and `su` runs a shell or one command as another user (this
//! is what every unprivileged health leg on a td image goes through). They are
//! one binary because they are one operation with two front ends: resolve an
//! account, decide whether a session may start, and then change credentials —
//! once, in one place, in one order.
//!
//! **td-login/THREAT-MODEL.md is the specification.** A credential-ordering bug
//! here is privilege escalation, not a malfunction, so the ordering, the
//! post-condition check that proves it took, and the confinement tests that pin
//! both are written down there and asserted here. Read it before changing
//! `creds.rs`, `sys.rs`, or `session.rs`.
//!
//! The crate is `#![deny(unsafe_code)]` with ONE scoped exception: `sys.rs`'s
//! `syscall2`, through which `setgroups`/`setgid`/`setuid` go. That is the
//! FOURTH target-side unsafe surface UNSAFE.md records, after td-kexec, td-netd
//! and td-init, and the narrowest of them. THREAT-MODEL.md section 2 says why it
//! is not `CommandExt::uid()`: `groups` is unstable on the pinned stable rustc,
//! and `std` applies credentials in a forked child where nothing can read back
//! what actually took. `mod confinement` below is what holds the surface — a
//! lint level can be demoted without using `unsafe` at all, and the compiler
//! cannot count syscalls or check that three calls happen in the right order.
//!
//! Dispatch is on argv[0]'s basename, the busybox/uutils convention, so a
//! `/bin/<applet> -> td-login` symlink runs that applet. An explicit
//! `td-login <applet> [args]` form covers the un-symlinked case, and
//! `td-login verify-credentials …` is the boot oracle's readback probe.

mod cgroup;
mod creds;
mod db;
mod exec_as;
mod login;
mod session;
mod status;
mod su;
mod sys;
mod tty;

use std::io::Write;
use std::process::ExitCode;

/// The account `su` targets when none is named, and the only uid that can reach
/// a credential switch at all.
pub const ROOT: &str = "root";

type Applet = fn(&[String]) -> Result<u8, String>;

/// Every applet this multicall serves, paired with the function that runs it.
/// ONE table, so a name cannot exist without an arm or an arm without a name —
/// `--list`, argv[0] dispatch, and the shipped /bin symlink farm all read it.
const APPLETS: &[(&str, Applet)] = &[("login", login::run), ("su", su::run)];

/// The multicall's own readback subcommand, not an applet: it gets no `/bin`
/// symlink and no place in the farm. `/etc/bootsuccess` runs it THROUGH `su` so
/// the kernel's view of the switched process is compared against what the switch
/// asked for — the one regression every other check on the image would pass.
const VERIFY: &str = "verify-credentials";

/// `exec-as USER -- PROGRAM [ARG…]`: run a literal argv as another user, with no
/// shell between the supervisor and the program.
///
/// A SUBCOMMAND rather than an applet, for the reason `verify-credentials` is
/// one: APPLICATIONS.md's units spell it `/bin/td-login exec-as tester -- …` and
/// never invoke it by basename, so a `/bin/exec-as` symlink would put a name on
/// the image that nothing calls and no farm list accounts for. It is also a
/// name a person could mistake for a general-purpose "run this as anyone" tool,
/// which is worth not hanging in `/bin` beside `su`.
const EXEC_AS: &str = "exec-as";

/// A plain loop rather than an iterator search: this file is embedded verbatim
/// into the recipe, and the ladder guard scans step content for host-tool names
/// that the search combinator happens to share.
fn lookup(name: &str) -> Option<Applet> {
    for (n, run) in APPLETS {
        if *n == name {
            return Some(*run);
        }
    }
    None
}

fn names() -> Vec<&'static str> {
    APPLETS.iter().map(|(n, _)| *n).collect()
}

pub(crate) fn basename(path: &str) -> &str {
    // `rsplit` always yields at least one item, so the fallback is unreachable;
    // `unwrap_or` states that without an `unwrap` the lint would reject.
    path.rsplit('/').next().unwrap_or(path)
}

/// Write to stdout, treating a closed reader as a clean exit. `print!`/`println!`
/// PANIC when the write fails, and Rust leaves SIGPIPE ignored, so a piped
/// `td-login --list | head` would abort the process — which the no-panic rule
/// forbids. `login`'s user-name prompt goes through here, so it also flushes:
/// a prompt still sitting in a buffer is a terminal that looks hung.
pub(crate) fn emit(text: &str) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Diagnostics to stderr — on a td image this IS the console. `eprintln!` panics
/// on a failed write (and `panic = "abort"` turns that into a SIGABRT), and
/// there is nowhere left to report a failure to report a failure, so drop it and
/// let the exit code carry the outcome.
pub(crate) fn emit_err(text: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes()).and_then(|()| err.flush());
}

fn usage() -> String {
    format!(
        "td-login: static multicall; applets: {}\n\
         usage: td-login <applet> [args]  (or invoke through a /bin/<applet> symlink)\n\
         usage: td-login {VERIFY} --uid U --gid G [--groups G[,G…]]\n\
         usage: td-login {EXEC_AS} USER -- PROGRAM [ARG…]",
        names().join(" ")
    )
}

/// What an argv means. Separated from `main` so every route is testable.
#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    /// Run this applet, with argv from `args_from` onwards.
    Applet { name: &'a str, args_from: usize },
    /// Read this process's credentials back and compare.
    Verify { args_from: usize },
    /// Run a literal argv as another user.
    ExecAs { args_from: usize },
    /// Print the applet roster.
    List,
    /// Print usage and exit 2.
    Usage,
}

/// Route an argv: argv[0]'s basename first (the busybox convention, and how the
/// shipped `/bin/login` and `/bin/su` symlinks arrive), then the explicit
/// `td-login <applet>` form.
///
/// NEITHER subcommand is reachable by basename; see `VERIFY` and `EXEC_AS` for
/// why. That is roster hygiene and NOT a boundary: `/bin/td-login` is itself a
/// shipped symlink, so anything on the image can already spell
/// `td-login exec-as`. What stops an unprivileged caller is `creds::may_switch`
/// over all four uid columns — never the absence of a second symlink.
fn route(argv: &[String]) -> Route<'_> {
    let prog = argv.first().map(String::as_str).unwrap_or("td-login");
    if lookup(basename(prog)).is_some() {
        return Route::Applet {
            name: basename(prog),
            args_from: 1,
        };
    }
    match argv.get(1).map(String::as_str) {
        Some(a) if lookup(a).is_some() => Route::Applet {
            name: a,
            args_from: 2,
        },
        Some(v) if v == VERIFY => Route::Verify { args_from: 2 },
        Some(v) if v == EXEC_AS => Route::ExecAs { args_from: 2 },
        Some("--list") => Route::List,
        _ => Route::Usage,
    }
}

/// `verify-credentials --uid U --gid G [--groups G[,G…]]`: read
/// `/proc/self/status` and assert this process's credentials are EXACTLY the
/// given set — all four uid columns, all four gid columns, and the supplementary
/// set with the primary gid folded in, which is how `login`/`su` compute it.
///
/// `--groups` takes the SUPPLEMENTARY list, not the whole set: it is the list
/// `/etc/group` yields, so the probe on the image and the code being probed are
/// written the same way and cannot be right for different reasons. Stated
/// exactly, the assertion is `kernel_groups == sort(dedup(groups + {gid}))` --
/// `Credentials::new` folds the primary gid in, as `login`/`su` do. So this
/// probe answers "is this process the session td-login says it built", which is
/// what it is for; it is NOT a general-purpose "are my credentials X" oracle,
/// and pointing it at a process nobody switched fails whenever that process's
/// primary gid is absent from its own supplementary set.
fn verify(args: &[String]) -> Result<u8, String> {
    let mut uid = None;
    let mut gid = None;
    let mut groups: Vec<u32> = Vec::new();
    let mut seen_groups = false;
    let mut i = 0usize;
    while let Some(flag) = args.get(i) {
        let Some(value) = args.get(i + 1) else {
            return Err(format!("{flag} needs an argument"));
        };
        i += 2;
        // Each flag once. A repeat is a caller that means two different things,
        // and last-wins would quietly assert the weaker of them.
        match flag.as_str() {
            "--uid" | "--gid" => {
                let slot = if flag == "--uid" { &mut uid } else { &mut gid };
                if slot.is_some() {
                    return Err(format!("{flag} given more than once"));
                }
                *slot = Some(number(value)?);
            }
            "--groups" => {
                if seen_groups {
                    return Err("--groups given more than once".into());
                }
                seen_groups = true;
                for token in value.split(',') {
                    if !token.is_empty() {
                        groups.push(number(token)?);
                    }
                }
            }
            other => return Err(format!("unrecognised argument {other:?}")),
        }
    }
    let (Some(uid), Some(gid)) = (uid, gid) else {
        return Err(format!("{VERIFY} needs both --uid and --gid"));
    };
    let want = creds::Credentials::new(uid, gid, &groups);
    want.matches(&status::Status::read()?)?;
    Ok(0)
}

fn number(token: &str) -> Result<u32, String> {
    token
        .parse::<u32>()
        .map_err(|_| format!("{token:?} is not an unsigned 32-bit id"))
}

fn main() -> ExitCode {
    // `args_os`, not `args`: the latter PANICS on a non-UTF-8 argument, and a
    // caller is not obliged to hand us one. Refused rather than transcoded:
    // U+FFFD is itself a real byte sequence, so a lossy conversion turns bytes
    // this program cannot represent into a DIFFERENT name, path or argument
    // that it can -- and then looks the wrong thing up, or execs it. Everything
    // on a td image is ASCII; a name that is not is not a name here.
    let mut argv: Vec<String> = Vec::new();
    for arg in std::env::args_os() {
        match arg.into_string() {
            Ok(text) => argv.push(text),
            Err(raw) => {
                emit_err(&format!(
                    "td-login: argument {raw:?} is not valid UTF-8; refusing it rather than \
                     transcoding it into a different one\n"
                ));
                return ExitCode::from(2);
            }
        }
    }

    let (name, result) = match route(&argv) {
        Route::List => {
            let listing = format!("{}\n", names().join("\n"));
            return match emit(&listing) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    emit_err(&format!("td-login: {e}\n"));
                    ExitCode::from(1)
                }
            };
        }
        Route::Usage => {
            emit_err(&format!("{}\n", usage()));
            return ExitCode::from(2);
        }
        Route::Verify { args_from } => (VERIFY, verify(argv.get(args_from..).unwrap_or(&[]))),
        Route::ExecAs { args_from } => (
            EXEC_AS,
            exec_as::run(argv.get(args_from..).unwrap_or(&[])),
        ),
        Route::Applet { name, args_from } => {
            let args = argv.get(args_from..).unwrap_or(&[]);
            let run = match lookup(name) {
                Some(run) => run,
                None => return ExitCode::from(2),
            };
            (name, run(args))
        }
    };

    match result {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            emit_err(&format!("{name}: {message}\n"));
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn basename_strips_every_leading_component() {
        assert_eq!(basename("/bin/login"), "login");
        assert_eq!(basename("su"), "su");
        assert_eq!(basename("/td/store/abc-td-login/bin/login"), "login");
        // A trailing slash yields an empty basename rather than the parent -- it
        // is not an applet, so dispatch falls through to the argv[1] form.
        assert_eq!(basename("/bin/"), "");
    }

    #[test]
    fn applet_list_is_sorted_and_unique() {
        let mut sorted = names();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, names(), "APPLETS must be sorted and duplicate-free");
    }

    /// Every listed name resolves to a callable arm — true by construction, since
    /// deleting an arm deletes its name from the same table.
    #[test]
    fn every_listed_applet_resolves_to_an_arm() {
        for n in names() {
            assert!(lookup(n).is_some(), "applet '{n}' is listed but has no arm");
        }
        assert!(lookup("nosuch").is_none());
        assert!(lookup("").is_none());
        // Both subcommands are held out of the roster by
        // `the_subcommands_are_not_in_the_applet_roster`, which owns that rule
        // for `verify-credentials` and `exec-as` together — stated once rather
        // than half here and half there.
    }

    /// The roster is the shipped /bin symlink farm, so a rename is a visible
    /// change to the image, not an internal one.
    #[test]
    fn the_roster_is_the_credential_pair() {
        assert_eq!(names(), vec!["login", "su"]);
    }

    fn argv(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn argv_routes_by_basename_then_by_the_explicit_form() {
        assert_eq!(
            route(&argv(&["/bin/login", "-f", "tester"])),
            Route::Applet {
                name: "login",
                args_from: 1
            }
        );
        assert_eq!(
            route(&argv(&["/bin/su", "-c", "id"])),
            Route::Applet {
                name: "su",
                args_from: 1
            }
        );
        assert_eq!(
            route(&argv(&["td-login", "su", "tester"])),
            Route::Applet {
                name: "su",
                args_from: 2
            }
        );
        assert_eq!(
            route(&argv(&["td-login", VERIFY, "--uid", "0"])),
            Route::Verify { args_from: 2 }
        );
        assert_eq!(
            route(&argv(&["td-login", EXEC_AS, "tester", "--", "/bin/td-busd"])),
            Route::ExecAs { args_from: 2 }
        );
        assert_eq!(route(&argv(&["td-login", "--list"])), Route::List);
        assert_eq!(route(&argv(&["td-login"])), Route::Usage);
        assert_eq!(route(&argv(&["td-login", "nosuch"])), Route::Usage);
        assert_eq!(route(&[]), Route::Usage);
        // Neither subcommand has a basename route: a `/bin/verify-credentials`
        // or `/bin/exec-as` symlink must dispatch to usage, not to the
        // subcommand. `exec-as` matters more than the probe here — it is the
        // one that changes credentials, and a name in `/bin` is a name any
        // caller can reach without spelling `td-login` at all.
        assert_eq!(route(&argv(&[&format!("/bin/{VERIFY}")])), Route::Usage);
        assert_eq!(route(&argv(&[&format!("/bin/{EXEC_AS}")])), Route::Usage);
    }

    /// The subcommands are not applets, so they must be absent from the roster
    /// `--list` prints and the shipped `/bin` symlink farm is built from.
    #[test]
    fn the_subcommands_are_not_in_the_applet_roster() {
        for name in [VERIFY, EXEC_AS] {
            assert!(
                !names().contains(&name),
                "{name} is a subcommand, so a /bin/{name} symlink would be an \
                 unaccounted name on the image"
            );
            assert!(lookup(name).is_none(), "{name} must not resolve as an applet");
        }
    }

    /// The probe's argv, including the forms the generated boot script uses.
    /// It runs unprivileged inside `su`, so it asserts against whatever the
    /// runner is; here only the parse and the comparison are exercised.
    #[test]
    fn the_readback_probe_parses_and_compares() {
        assert!(verify(&argv(&["--uid"])).is_err());
        assert!(verify(&argv(&["--uid", "1000"])).is_err(), "needs --gid too");
        assert!(verify(&argv(&["--uid", "x", "--gid", "1"])).is_err());
        // A repeated flag means two different assertions; last-wins would
        // silently make the probe prove the weaker one.
        for repeated in [
            vec!["--uid", "0", "--uid", "1000", "--gid", "0"],
            vec!["--uid", "0", "--gid", "0", "--gid", "1000"],
            vec!["--uid", "0", "--gid", "0", "--groups", "0", "--groups", "10"],
        ] {
            let err = verify(&argv(&repeated)).unwrap_err();
            assert!(err.contains("more than once"), "got: {err}");
        }
        assert!(verify(&argv(&["--nope", "1"])).is_err());
        let Ok(now) = status::Status::read() else {
            return; // no /proc in this sandbox
        };
        // Ask for exactly what this process already is: the supplementary list
        // minus the primary gid, which `Credentials::new` folds back in.
        let gid = now.gid[status::EFFECTIVE];
        let supplementary: Vec<String> = now
            .groups
            .iter()
            .filter(|g| **g != gid)
            .map(|g| g.to_string())
            .collect();
        let ok = argv(&[
            "--uid",
            &now.uid[status::EFFECTIVE].to_string(),
            "--gid",
            &gid.to_string(),
            "--groups",
            &supplementary.join(","),
        ]);
        // Only meaningful when this process's four uid columns already agree,
        // which they do for an ordinary `cargo test` runner.
        if now.uid.iter().all(|u| *u == now.uid[status::EFFECTIVE])
            && now.gid.iter().all(|g| *g == gid)
        {
            assert_eq!(verify(&ok).unwrap(), 0);
        }
        // A wrong expectation must never pass, whatever the runner is.
        let mut wrong = ok.clone();
        wrong[1] = (now.uid[status::EFFECTIVE].wrapping_add(1)).to_string();
        assert!(verify(&wrong).is_err());
    }
}

/// The unsafe confinement AND the credential ordering, asserted against the
/// crate's own source.
///
/// Adapted from `td-init/src/main.rs`'s `mod confinement`, which is the shape
/// UNSAFE.md's target-side unsafe exceptions are held to. The scanning machinery
/// is the same; the assertions are this crate's. Two of them are new here and
/// are the reason this module exists at all: the three credential syscalls are
/// issued exactly once each, IN ORDER, from one function — and the crate never
/// reaches a second credential mechanism through `Command`.
///
/// Needles are assembled with `concat!` so the literals never appear in the
/// sources being scanned, where they would count themselves.
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

    /// Where a file's `mod x;` puts `x.rs`: beside `main.rs` for the crate root,
    /// and in a like-named subdirectory for every other module.
    fn submodule_dir(file: &str) -> String {
        if file == "main.rs" {
            String::new()
        } else {
            format!("{}/", file.trim_end_matches(".rs"))
        }
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

    /// The syscalls UNSAFE.md records for this crate, with the x86_64 number
    /// each name must carry. A FOURTH is a reviewed amendment; this test is what
    /// makes that more than an aspiration.
    ///
    /// The NUMBERS are pinned, not just the names: 105 and 106 are one apart, so
    /// a transposition sets the uid where the gid was meant and every name-based
    /// assertion still passes.
    const AMENDED: &[(&str, &str)] = &[
        ("SYS_SETGID", "106"),
        ("SYS_SETGROUPS", "116"),
        ("SYS_SETUID", "105"),
    ];

    /// The ORDER the three must be issued in, and the file that issues them.
    /// This is THREAT-MODEL.md section 2 Layer 1 as an assertion: supplementary
    /// groups while still privileged, then the primary group, then the uid —
    /// which is the call that takes the privilege the other two need away.
    const ORDER: &[&str] = &["setgroups", "setgid", "setuid"];
    const SWITCH: &str = "creds.rs";

    fn source(name: &str) -> String {
        for (n, text) in sources() {
            if n == name {
                return text;
            }
        }
        String::new()
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

    /// Reading the directory only equals reading `main.rs` if the two agree, so
    /// assert it: a `mod` whose file the scan missed would make every assertion
    /// below vacuous for exactly the file that needed checking.
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
            10,
            "expected ten modules beside the crate root"
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

    /// `src/` holds these eleven files and nothing else.
    ///
    /// The scan above proves every `mod` line has a file and every file has a
    /// `mod` line, which is a closed loop that says nothing about WHICH files:
    /// a `mod` line and its file added together satisfy both halves. This pins
    /// the set the UNSAFE.md amendment was written against.
    ///
    /// The second half is why the collector keeps non-`.rs` files rather than
    /// skipping them: `src/sys.inc` is invisible to a `.rs`-only scan and
    /// compiles perfectly well through the constructs refused below.
    #[test]
    fn src_holds_exactly_the_eleven_scanned_modules() {
        let (rs, other) = walk();
        let paths: Vec<&str> = rs.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "cgroup.rs", "creds.rs", "db.rs", "exec_as.rs", "login.rs", "main.rs",
                "session.rs", "status.rs", "su.rs", "sys.rs", "tty.rs",
            ],
            "the crate's file set changed"
        );
        assert!(
            other.is_empty(),
            "src/ holds a non-.rs file no scan here reads: {other:?}"
        );
    }

    /// Every credential switch is a front end that decided it may.
    ///
    /// `creds::apply` is pinned to one call site below, which is the switch
    /// itself. `session::enter` is the thing that CALLS it — switch, then exec —
    /// so "no second way to become somebody" is a claim about `enter`'s callers
    /// and nothing else asserts it. A future module that built a `Session` by
    /// hand and called `enter` would name neither `apply` nor
    /// `may_start_session`, so every other scan in this file stays green while
    /// the crate grows a switch nobody authorized.
    ///
    /// Two halves, and the second is the point: exactly the three front ends
    /// call it, AND each of them reaches `authorize`.
    #[test]
    fn every_credential_switch_is_a_front_end_that_authorized_first() {
        let enter = concat!("session::", "enter(");
        let decide = concat!("authorize", "(");
        let callers: Vec<(String, String)> = sources()
            .into_iter()
            .filter(|(_, text)| text.contains(enter))
            .collect();
        let paths: Vec<&str> = callers.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            ["exec_as.rs", "login.rs", "su.rs"],
            "`session::enter` switches credentials and execs; its callers are the \
             front ends and adding one is a reviewed change"
        );
        for (path, text) in &callers {
            assert!(
                text.contains(decide),
                "{path} switches credentials without naming `authorize`, so it \
                 decided for itself whether the session may start"
            );
        }
    }

    /// One authentication decision, asserted rather than claimed.
    ///
    /// `db::may_start_session` answers whether an account may start a session at
    /// all — the locked and needs-a-password refusals — so a front end naming it
    /// is one deciding that policy for itself. It may be named in its own module
    /// and in `login.rs`, where `authorize` lives, and nowhere else. `su` carried
    /// its own copy of four of `authorize`'s five steps until this landing: two
    /// places to change one policy, with the compiler checking neither.
    ///
    /// The name is assembled rather than spelled, as the unsafe-lint scan's is —
    /// written whole, this test's own source would make `main.rs` a caller.
    #[test]
    fn the_session_policy_is_decided_in_one_place() {
        let call = concat!("may_start_", "session");
        let namers: Vec<String> = sources()
            .into_iter()
            .filter(|(_, text)| text.contains(call))
            .map(|(path, _)| path)
            .collect();
        assert_eq!(
            namers,
            ["db.rs", "login.rs"],
            "the session policy must be reached through login::authorize alone"
        );
    }

    #[test]
    fn exactly_one_scoped_allow_and_one_unsafe_block_in_the_whole_crate() {
        // Every spelling counts against the same budget of one: the outer form,
        // the inner form a new module could carry at its top, and a multi-lint
        // group with the lint somewhere in the middle of it.
        assert_eq!(
            sources()
                .iter()
                .map(|(_, text)| unsafe_allows(text))
                .sum::<usize>(),
            1,
            "the crate must carry exactly ONE scoped unsafe allow (the asm body in sys.rs)"
        );
        assert_eq!(
            sources()
                .iter()
                .map(|(_, text)| unsafe_blocks(text))
                .sum::<usize>(),
            1,
            "the crate must carry exactly ONE unsafe block (the asm body in sys.rs)"
        );
        for form in ["fn", "impl", "trait"] {
            assert_eq!(
                sources()
                    .iter()
                    .map(|(_, text)| unsafe_items(text, form))
                    .sum::<usize>(),
                0,
                "no item of this form may exist in this crate: {form}"
            );
        }
        // These constructs would make the scanned text stop describing the
        // compiled crate, and every assertion here reads text. A `path`
        // attribute and `include!` pull code in from a file the directory scan
        // never sees; `macro_rules!` builds tokens that no longer appear in the
        // source, so `hidden!(unsafe)` is an unsafe block the block count cannot
        // see and a macro can expand one audited call into two.
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
        assert_eq!(
            source("main.rs")
                .matches(concat!("#![deny(un", "safe_code)]"))
                .count(),
            1,
            "the crate root must deny the unsafe lint so the scoped allow is the confinement"
        );
    }

    /// The lint's LEVEL is part of the confinement, and changing it is not
    /// itself `unsafe` — so no count above sees it. A module-level
    /// `#![warn(...)]` demotes the crate root's `deny` to a warning, and the
    /// build then accepts a second asm entry point with every other assertion
    /// in this module intact.
    ///
    /// So the lint is named EXACTLY twice in the whole crate: denied at the
    /// root, allowed on the one asm body.
    #[test]
    fn the_unsafe_lint_is_named_exactly_twice() {
        const LINT: &str = concat!("un", "safe_code");
        let mut total = 0;
        for (path, text) in sources() {
            let count = text.matches(LINT).count();
            let expected = usize::from(path == "main.rs" || path == "sys.rs");
            assert_eq!(
                count, expected,
                "'{path}' names the unsafe lint {count} time(s), expected {expected}"
            );
            total += count;
        }
        assert_eq!(total, 2, "the lint is denied once and allowed once");
    }

    /// Inline assembly has more than one entry point, and the block pin only
    /// ever saw `asm!`. `global_asm!` needs no `unsafe` block at all, emits a
    /// function the linker keeps, and reaches the kernel exactly as well.
    /// Both are closed by naming the MODULE rather than the macro: the assembly
    /// module may be named once, by the pinned block, and nowhere else.
    #[test]
    fn the_only_route_to_inline_assembly_is_the_pinned_one() {
        let squeezed = squeezed();
        assert_eq!(
            squeezed.matches(concat!("core", "::arch")).count(),
            1,
            "the assembly module may be named only by the pinned block; an alias is a second name for the same door"
        );
        for shut in [
            concat!("std", "::arch"),
            concat!("global_", "asm"),
            concat!("naked_", "asm"),
        ] {
            assert_eq!(
                squeezed.matches(shut).count(),
                0,
                "'{shut}' is another way into inline assembly"
            );
        }
    }

    /// The syscall numbers are declared in one place, so the roster is readable
    /// straight out of the source — name AND number.
    ///
    /// Read off the WHITESPACE-SQUEEZED source, because a line is not a unit
    /// Rust cares about: two `const SYS_…` items sharing one line are two items
    /// to the compiler, and the second shadows the roster's at every call site
    /// below it.
    #[test]
    fn the_syscall_surface_is_the_amended_three() {
        const DECL: &str = concat!("const", "SYS_");
        let sys = squeeze(&source("sys.rs"));
        let mut declared = Vec::new();
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
                declared.push((format!("SYS_{name}"), number));
            }
        }
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
    }

    /// All three calls, pinned WHOLE — every register, not just the selector.
    ///
    /// For `setgroups` the second register is the LENGTH and the third the
    /// pointer; transposing them is a call that still compiles, still returns 0
    /// for a short list, and sets a group set nobody asked for.
    #[test]
    fn every_call_site_is_pinned_whole() {
        const ARGUMENTS: &[&str] = &[
            "(SYS_SETGROUPS,list.len(),list.as_ptr()asusize,)",
            "(SYS_SETGID,gidasusize,0)",
            "(SYS_SETUID,uidasusize,0)",
        ];
        assert_eq!(ARGUMENTS.len(), AMENDED.len(), "one pin per amended syscall");
        let sys = squeeze(&source("sys.rs"));
        for arguments in ARGUMENTS {
            assert_eq!(
                sys.matches(&format!("{CALL}{arguments}")).count(),
                1,
                "this call's arguments changed; re-audit it and update the pin: {arguments}"
            );
        }
    }

    /// The raw-syscall entry point, assembled rather than written out: the test
    /// below asserts the name appears in NO source but `sys.rs`, and this file
    /// is one of the sources it scans.
    const CALL: &str = concat!("sys", "call2");

    /// Every call site selects its syscall through one of the three named
    /// constants. Without this the roster above is only a claim about
    /// declarations: a bare `(999, ...)` would reach an unaudited kernel call
    /// while leaving every constant untouched.
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
                "called with '{selector}', which is not one of the amended three"
            );
            // The constant must BE the argument, not the start of an expression:
            // `SYS_SETUID + 1` selects setgid(2) while spelling an audited name.
            let rest = after.get(selector.len()..).unwrap_or("").trim_start();
            assert!(
                rest.starts_with(','),
                "the selector must be '{selector}' itself, not an expression built from it"
            );
            selected.push(selector);
        }
        selected.sort();
        let roster: Vec<String> = AMENDED.iter().map(|(n, _)| (*n).to_string()).collect();
        assert_eq!(
            selected, roster,
            "each amended syscall is issued exactly once"
        );
        assert_eq!(sites, 3, "expected exactly three call sites");
        // ...and the definition, and NOTHING else. The loop skips any mention
        // not followed by `(`, which is the function ITEM: bind it once and
        // every later call goes through a name this scan does not know.
        assert_eq!(
            mentions,
            sites + 1,
            "mentioned somewhere that is not one of the three calls or its definition"
        );
    }

    /// The raw entry point is private to `sys.rs`: the module header says its
    /// confinement is module privacy plus the three typed wrappers being its
    /// only callers, and the scan above reads `sys.rs` alone — so exporting the
    /// function and calling it from another module would put a fourth syscall
    /// in the binary with all three audited sites intact.
    #[test]
    fn the_raw_entry_point_is_private_to_its_module() {
        let sys = squeeze(&source("sys.rs"));
        assert_eq!(
            sys.matches(&format!("{}{CALL}(", concat!("#[allow(un", "safe_code)]fn")))
                .count(),
            1,
            "the raw entry point must be the item under the scoped allow, with no visibility"
        );
        for (path, text) in sources() {
            if path == "sys.rs" {
                continue;
            }
            assert_eq!(
                text.matches(CALL).count(),
                0,
                "'{path}' names the raw entry point; only sys.rs may"
            );
        }
    }

    /// The one permitted `unsafe` block is a REGION, and counting tokens inside
    /// it never bounds it: a second `asm!`, a second instruction in the SAME
    /// `asm!` ("syscall", "syscall"), or a raw pointer dereference all fit
    /// without changing any count — and `#[inline]` then multiplies whatever was
    /// added across all three wrappers. So the block is pinned WHOLE, with
    /// whitespace squeezed out so the pin is immune to reformatting.
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

    // ── the credential-ordering assertions ──────────────────────────────────
    //
    // Everything above this line is td-init's confinement, adapted. What follows
    // is why this crate needs its own: the compiler is perfectly happy with a
    // `setuid` before a `setgroups`, and so is every other test here.

    /// The three credential syscalls are issued from ONE function, in the ONE
    /// order that is not an escalation.
    ///
    /// Byte offsets, not lines: `sys::setuid(u)?; sys::setgroups(g)?;` on one
    /// line is the wrong order spelled compactly. Each wrapper is called exactly
    /// once in the whole crate, so there is no second switch site whose order
    /// this says nothing about.
    #[test]
    fn the_credential_syscalls_are_issued_once_each_in_order() {
        let mut offsets = Vec::new();
        for (path, text) in sources() {
            for (i, call) in ORDER.iter().enumerate() {
                let needle = format!("{}::{call}(", concat!("s", "ys"));
                let count = text.matches(&needle).count();
                if path != SWITCH {
                    assert_eq!(
                        count, 0,
                        "'{path}' issues {needle} — the credential switch lives in {SWITCH} alone"
                    );
                    continue;
                }
                assert_eq!(
                    count, 1,
                    "{SWITCH} must issue {needle} exactly once, found {count}"
                );
                let at = text.match_indices(&needle).next().map(|(o, _)| o);
                offsets.push((i, at.unwrap_or_default()));
            }
        }
        assert_eq!(offsets.len(), ORDER.len(), "one call site per syscall");
        // Sorted by source position, the calls must come out in roster order.
        let mut by_position = offsets.clone();
        by_position.sort_by_key(|(_, at)| *at);
        let order: Vec<usize> = by_position.iter().map(|(i, _)| *i).collect();
        assert_eq!(
            order,
            (0..ORDER.len()).collect::<Vec<usize>>(),
            "the credential syscalls are out of order in {SWITCH}: they must run \
             {ORDER:?} — groups and gid need the privilege setuid takes away \
             (td-login/THREAT-MODEL.md section 2)"
        );
    }

    /// There is exactly ONE place where td-login stops being root, and it is the
    /// statement immediately before the exec.
    ///
    /// A second caller of `apply` would be a second switch this crate's reviewers
    /// never looked at; a caller that is not `session::enter` would be a switch
    /// with no exec after it, which is a process running as somebody else for
    /// reasons nobody stated.
    #[test]
    fn privilege_is_dropped_in_exactly_one_place() {
        let needle = format!("{}::apply(", concat!("cre", "ds"));
        for (path, text) in sources() {
            let expected = usize::from(path == "session.rs");
            assert_eq!(
                text.matches(&needle).count(),
                expected,
                "'{path}' calls {needle} {} time(s), expected {expected} — the drop \
                 belongs to session::enter and nowhere else",
                text.matches(&needle).count()
            );
        }
        let session = source("session.rs");
        assert!(
            session
                .find("cgroup::join(session.creds.target_uid())")
                .unwrap_or(usize::MAX)
                < session.find(&needle).unwrap_or(0),
            "the fixed session cgroup join must be attempted while td-login is still root"
        );
    }

    /// No second credential mechanism.
    ///
    /// `CommandExt` can set a uid, a gid and (on nightly) a group list on the
    /// child instead. Reaching for it would apply credentials in a forked child
    /// where `creds::apply`'s readback cannot see them — every assertion above
    /// green, and the process that actually runs the user's shell never verified.
    /// `env_clear` is pinned in the same breath: without it the session inherits
    /// whatever the caller set (THREAT-MODEL.md section 5).
    #[test]
    fn the_child_process_api_is_never_used_to_set_credentials() {
        let squeezed = squeezed();
        for shut in [
            concat!(".u", "id("),
            concat!(".g", "id("),
            concat!(".gro", "ups("),
            concat!("pre_", "exec"),
        ] {
            assert_eq!(
                squeezed.matches(shut).count(),
                0,
                "'{shut}' is a second way to set credentials, applied where nothing can \
                 read it back"
            );
        }
        assert_eq!(
            squeezed.matches(concat!("env_", "clear()")).count(),
            1,
            "the session's environment must be replaced wholesale, exactly once"
        );
    }

    /// td-login must never be reachable as a setuid-root program (THREAT-MODEL.md
    /// section 4). Nothing in the crate may set a mode with the setuid or setgid
    /// bit, and the one place it sets a mode at all is the terminal hand-over.
    #[test]
    fn no_mode_this_crate_sets_carries_a_setuid_bit() {
        const MODE: &str = concat!("from_", "mode(");
        let squeezed = squeezed();
        assert_eq!(
            squeezed.matches(MODE).count(),
            2,
            "modes are written in one place (the terminal hand-over: set, and put \
             back when the chown that follows it fails) and nowhere else"
        );
        // ...and neither site names a literal it could grow a setuid bit in: one
        // is the pinned constant, the other is the mode the node already had.
        assert_eq!(
            squeezed.matches(&format!("{MODE}TTY_MODE)")).count(),
            1,
            "the terminal's mode must be the pinned TTY_MODE"
        );
        assert_eq!(
            squeezed.matches(&format!("{MODE}was&MODE_BITS)")).count(),
            1,
            "the rollback must restore the mode read off that same inode"
        );
        let tty = source("tty.rs");
        assert_eq!(
            tty.matches("const TTY_MODE: u32 = 0o600;").count(),
            1,
            "TTY_MODE must read exactly 0o600 — owner read/write, no setuid bit, \
             nothing for anyone else"
        );
        assert_eq!(
            tty.matches("const MODE_BITS: u32 = 0o7777;").count(),
            1,
            "MODE_BITS is the chmod(2) mask, so the rollback restores the whole \
             previous mode rather than a subset of it"
        );
    }
}
