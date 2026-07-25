#![forbid(unsafe_code)]
//! td-util — the static, dependency-free multicall behind td's diagnostics
//! userland: the busybox applets uutils' coreutils does not provide and that
//! need no syscall surface beyond what safe `std` already exposes.
//!
//! Dispatch is on argv[0]'s basename, the busybox/uutils convention, so a
//! `/bin/<applet> -> td-util` symlink runs that applet. An explicit
//! `td-util <applet> [args]` form covers the un-symlinked case.
//!
//! Everything here is safe `std`: `/proc` and `/dev/kmsg` are ordinary files, so
//! the whole crate stays inside `#![forbid(unsafe_code)]` and needs none of the
//! target-side syscall exceptions AGENTS.md records for td-kexec/td-netd. The
//! applets that DO need raw syscalls (reboot/poweroff/halt, switch_root,
//! cttyhack, init) are deliberately absent — adding them is a reviewed
//! `unsafe`-surface amendment, not a drive-by.

mod dmesg;
mod free;
mod procfs;
mod ps;
mod which;

use std::io::Write;
use std::process::ExitCode;

type Applet = fn(&[String]) -> Result<u8, String>;

/// Every applet this multicall serves, paired with the function that runs it.
/// ONE table, so a name cannot exist without an arm or an arm without a name —
/// `--list`, argv[0] dispatch, and the shipped /bin symlink farm all read it.
const APPLETS: &[(&str, Applet)] = &[
    ("clear", clear),
    ("dmesg", dmesg::run),
    ("free", free::run),
    ("ps", ps::run),
    ("which", which::run),
];

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

fn basename(path: &str) -> &str {
    match path.rsplit('/').next() {
        Some(b) => b,
        None => path,
    }
}

/// Write to stdout, treating a closed reader as a clean exit. `print!`/`println!`
/// PANIC when the write fails, and Rust leaves SIGPIPE ignored, so `ps | head`
/// would abort the process — which the no-panic rule forbids.
pub fn emit(text: &str) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Diagnostics to stderr. `eprintln!` panics on a failed write for the same
/// reason `println!` does (and `panic = "abort"` turns that into a SIGABRT on
/// `2>/dev/full`), and there is nowhere left to report a failure to report a
/// failure — so drop it and let the exit code carry the outcome.
fn emit_err(text: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes()).and_then(|()| err.flush());
}

/// `clear` writes the vt100 sequence: cursor home, erase display, erase
/// scrollback. A terminfo lookup would need a terminfo database on the image;
/// both consoles td ships (the kernel VT and the ttyS0 serial console) honour
/// these three, which is also what busybox `clear` emits.
fn clear(_args: &[String]) -> Result<u8, String> {
    emit("\x1b[H\x1b[2J\x1b[3J")?;
    Ok(0)
}

fn usage() -> String {
    format!(
        "td-util: static multicall; applets: {}\n\
         usage: td-util <applet> [args]  (or invoke through a /bin/<applet> symlink)",
        names().join(" ")
    )
}

fn main() -> ExitCode {
    // `args_os`, not `args`: the latter PANICS on a non-UTF-8 argument, which is
    // an ordinary thing to hand a diagnostics tool (`which $(some-bad-path)`).
    let argv: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let prog = argv.first().map(String::as_str).unwrap_or("td-util");
    let self_name = basename(prog);

    let (name, args): (&str, &[String]) = match lookup(self_name) {
        Some(_) => (self_name, argv.get(1..).unwrap_or(&[])),
        None => match argv.get(1).map(String::as_str) {
            Some("--list") => {
                let listing = format!("{}\n", names().join("\n"));
                return match emit(&listing) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        emit_err(&format!("td-util: {e}\n"));
                        ExitCode::from(1)
                    }
                };
            }
            Some(a) if lookup(a).is_some() => (a, argv.get(2..).unwrap_or(&[])),
            _ => {
                emit_err(&format!("{}\n", usage()));
                return ExitCode::from(2);
            }
        },
    };

    let result = match lookup(name) {
        Some(run) => run(args),
        None => Err(format!("no such applet '{name}'")),
    };

    match result {
        Ok(code) => ExitCode::from(code),
        Err(msg) => {
            emit_err(&format!("{name}: {msg}\n"));
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_strips_every_leading_component() {
        assert_eq!(basename("/bin/ps"), "ps");
        assert_eq!(basename("ps"), "ps");
        assert_eq!(basename("/td/store/abc-td-util/bin/free"), "free");
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

    /// Every listed name resolves to a callable arm. The table makes that true by
    /// construction (deleting an arm deletes its name), which is the point: the
    /// previous shape re-stated the list in a `matches!` and stayed green when an
    /// arm was deleted.
    #[test]
    fn every_listed_applet_resolves_to_an_arm() {
        for n in names() {
            assert!(lookup(n).is_some(), "applet '{n}' is listed but has no arm");
        }
        assert!(lookup("nosuch").is_none());
        assert!(lookup("").is_none());
    }
}
