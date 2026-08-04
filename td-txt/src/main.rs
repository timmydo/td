#![forbid(unsafe_code)]
//! td-txt — the static, dependency-free multicall behind td's TEXT userland: the
//! `grep` and `sed` on the image, which busybox owned until this corpus covered the
//! shapes the image's own scripts use (uutils ships neither).
//!
//! Dispatch is on argv[0]'s basename, the busybox/uutils/td-util convention, so a
//! `/bin/<applet> -> td-txt` symlink runs that applet. An explicit
//! `td-txt <applet> [args]` form covers the un-symlinked case.
//!
//! Everything here is safe `std` (`#![forbid(unsafe_code)]`): a regex engine and
//! line I/O need none of the target-side syscall exceptions UNSAFE.md records for
//! td-kexec/td-netd/td-init.
//!
//! Behavior is scored against the conformance corpus in `../spec` — the vendored
//! GNU grep regex suites and GNU sed testsuite, plus td-txt's own case files —
//! driven by `../tests/conformance.rs`. That corpus, not this file, is the
//! contract.

mod grep;
mod regex;
mod sed;
mod util;

use std::process::ExitCode;

type Applet = fn(&[Vec<u8>]) -> i32;

/// Applets run on a thread with this much stack rather than on `main`'s default
/// 8 MiB. The regex matcher backtracks by recursion, and a repetition whose body
/// is not a single byte costs a frame per iteration (`\(ab\)*` over a long
/// line); the engine caps that depth, but the cap is only safe with headroom
/// under it. A thread stack is RESERVED address space — only touched pages
/// commit — so sizing it for the cap costs nothing a short-lived process
/// notices.
const APPLET_STACK: usize = 256 << 20;

// The depth cap is only a guard if the stack can actually hold that many
// frames; otherwise the process aborts BEFORE the cap reports `too complex`,
// and an abort is not an error a caller can handle. Checked here so raising
// either constant without the other fails the build.
const _: () = assert!(
    (regex::MAX_REPEAT_DEPTH as usize) * regex::REPEAT_FRAME_BUDGET <= APPLET_STACK,
    "MAX_REPEAT_DEPTH * REPEAT_FRAME_BUDGET must fit in APPLET_STACK"
);

/// Every applet this multicall serves. ONE table, so a name cannot exist without
/// an arm or an arm without a name — `--list`, argv[0] dispatch and the shipped
/// /bin symlink farm all read it. `system-x86-64`'s `shape_check` compares this
/// list, through `--list`, against the names it packs.
const APPLETS: &[(&str, Applet)] = &[("grep", grep::main), ("sed", sed::main)];

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

fn basename(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|b| *b == b'/') {
        Some(i) => path.get(i + 1..).unwrap_or(path),
        None => path,
    }
}

fn usage() -> String {
    format!(
        "td-txt: static multicall; applets: {}\n\
         usage: td-txt <applet> [args]  (or invoke through a /bin/<applet> symlink)",
        names().join(" ")
    )
}

fn main() -> ExitCode {
    // Raw bytes, not `String`: a pattern or a filename need not be UTF-8, and
    // `std::env::args` PANICS on one that is not.
    let argv = util::args_bytes();
    let prog = argv.first().map(Vec::as_slice).unwrap_or(b"td-txt");
    let self_name = String::from_utf8_lossy(basename(prog)).into_owned();

    // Invoked under an applet name (the /bin symlink): the applet gets the whole
    // argv, so its own argv[0] is that name.
    if let Some(run) = lookup(&self_name) {
        return ExitCode::from(clamp(spawn(run, argv)));
    }
    // Invoked as td-txt: the applet is argv[1], and it gets argv[1..] — again
    // leaving the applet name at its argv[0].
    match argv.get(1).map(Vec::as_slice) {
        // Through `print_line` for the reason its doc gives: `td-txt --list | head -1`
        // must not panic the way `println!` does on a closed reader. A genuine write
        // failure is exit 2, the multicall's own usage status.
        Some(b"--list") => tell(&names().join("\n")),
        Some(b"--help" | b"-h") => tell(&usage()),
        Some(name) => {
            let applet = String::from_utf8_lossy(name).into_owned();
            match lookup(&applet) {
                Some(run) => {
                    ExitCode::from(clamp(spawn(run, argv.get(1..).unwrap_or_default().to_vec())))
                }
                None => {
                    eprintln!("td-txt: unknown applet `{applet}'");
                    eprintln!("{}", usage());
                    ExitCode::from(2)
                }
            }
        }
        None => {
            eprintln!("{}", usage());
            ExitCode::from(2)
        }
    }
}

/// One informational line on stdout, exit 0 — or exit 2 if the write genuinely
/// failed (a closed READER is not a failure; see `util::print_line`).
fn tell(text: &str) -> ExitCode {
    match util::print_line(text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("td-txt: write error: {}", util::errmsg(&e));
            ExitCode::from(2)
        }
    }
}

/// Run an applet on a big-stack thread (see `APPLET_STACK`). A thread that
/// cannot be spawned, or that dies, is reported as an error status rather than
/// propagated as a panic.
fn spawn(run: Applet, argv: Vec<Vec<u8>>) -> i32 {
    let owned = argv.clone();
    match std::thread::Builder::new()
        .stack_size(APPLET_STACK)
        .spawn(move || run(&owned))
    {
        // A joined panic is reported as an error status, not re-raised.
        Ok(handle) => handle.join().unwrap_or(2),
        // Falling back to main's 8 MiB stack would silently void the assert
        // above and turn a diagnosed `too complex` into an abort, so a thread
        // that cannot be created is reported instead.
        Err(e) => {
            eprintln!("td-txt: cannot start applet thread: {e}");
            2
        }
    }
}

/// `sed 2q300` is legal and exits 44: an exit status IS a byte, and the kernel
/// takes the low 8 bits. Masking is what the caller observes from GNU, so it is
/// what td-txt reports rather than substituting an error code of its own.
fn clamp(code: i32) -> u8 {
    (code & 0xff) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_dispatches_on_the_last_path_component() {
        assert_eq!(basename(b"/bin/grep"), b"grep");
        assert_eq!(basename(b"sed"), b"sed");
        assert_eq!(basename(b"./td-txt"), b"td-txt");
    }

    #[test]
    fn every_applet_name_resolves() {
        for name in names() {
            assert!(lookup(name).is_some(), "{name} is listed but does not dispatch");
        }
        assert!(lookup("awk").is_none());
    }

    /// The two names `system-x86-64`'s `TD_TXT_APPLETS` packs as `/bin` symlinks.
    /// `awk` is deliberately absent — it stays busybox's.
    #[test]
    fn the_applet_table_is_the_bin_farm_this_multicall_serves() {
        assert_eq!(names(), vec!["grep", "sed"]);
    }
}
