#![deny(unsafe_code)]
//! td-init — the static, dependency-free multicall behind td's boot glue: the
//! busybox applets that cannot be written in safe `std` because each one needs a
//! raw Linux syscall.
//!
//! `reboot`/`poweroff`/`halt` (`reboot(2)`), `switch_root` (`mount(MS_MOVE)` +
//! `chroot(2)`), `mount`/`umount` (`mount(2)` + `umount2(2)`), `cttyhack`
//! (`setsid(2)` + `TIOCSCTTY`), `init` (`wait4(2)`), and `hostname`
//! (`sethostname(2)` — the `-F` flag uutils lacks).
//!
//! The sibling multicall `td-util` covers the applets that need NO syscall
//! surface and is `#![forbid(unsafe_code)]` as a result. The split is deliberate:
//! everything that can be safe lives there, and this crate's `unsafe` is confined
//! to `sys.rs` — one `syscall5` body under a scoped `#[allow]`. That is the THIRD
//! target-side unsafe exception AGENTS.md records, after td-kexec and td-netd.
//! The `deny` above is the first line of that, not the last: `mod confinement`
//! below is what actually holds the surface to ten syscalls and one asm body,
//! because a lint level can be demoted and the compiler cannot count syscalls.
//!
//! Dispatch is on argv[0]'s basename, the busybox/uutils convention, so a
//! `/bin/<applet> -> td-init` symlink runs that applet — including the
//! `/sbin/init` the kernel execs. An explicit `td-init <applet> [args]` form
//! covers the un-symlinked case.

mod cttyhack;
mod devt;
mod halt;
mod hostname;
mod init;
mod losetup;
mod mknod;
mod mount;
mod switchroot;
mod syncfs;
mod sys;

use std::io::Write;
use std::process::ExitCode;

type Applet = fn(&[String]) -> Result<u8, String>;

/// Every applet this multicall serves, paired with the function that runs it.
/// ONE table, so a name cannot exist without an arm or an arm without a name —
/// `--list`, argv[0] dispatch, and the shipped /bin symlink farm all read it.
const APPLETS: &[(&str, Applet)] = &[
    ("cttyhack", cttyhack::run),
    ("halt", halt::halt),
    ("hostname", hostname::run),
    ("init", init::run),
    ("losetup", losetup::run),
    ("mknod", mknod::run),
    ("mount", mount::mount),
    ("poweroff", halt::poweroff),
    ("reboot", halt::reboot),
    ("switch_root", switchroot::run),
    ("sync", syncfs::run),
    ("umount", mount::umount),
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
    // `rsplit` always yields at least one item, so the fallback is unreachable;
    // `unwrap_or` states that without an `unwrap` the lint would reject.
    path.rsplit('/').next().unwrap_or(path)
}

/// Write to stdout, treating a closed reader as a clean exit. `print!`/`println!`
/// PANIC when the write fails, and Rust leaves SIGPIPE ignored, so a piped
/// `init --dry-run | head` would abort the process — which the no-panic rule
/// forbids.
pub(crate) fn emit(text: &str) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Diagnostics to stderr — for init, this IS the console log. `eprintln!` panics
/// on a failed write (and `panic = "abort"` turns that into a SIGABRT), which for
/// PID 1 means a kernel panic over an unwritable console. There is nowhere left
/// to report a failure to report a failure, so drop it.
pub(crate) fn emit_err(text: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes()).and_then(|()| err.flush());
}

fn usage() -> String {
    format!(
        "td-init: static multicall; applets: {}\n\
         usage: td-init <applet> [args]  (or invoke through a /bin/<applet> symlink)",
        names().join(" ")
    )
}

/// What an argv means. Separated from `main` because the PID-1 rule below is
/// not testable through a process that is not PID 1.
#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    /// Run this applet, with argv from `args_from` onwards. `fallback` marks the
    /// PID-1 rescue below, which is worth a console line because nothing in the
    /// argv asked for it.
    Applet {
        name: &'a str,
        args_from: usize,
        fallback: bool,
    },
    /// Print the applet roster.
    List,
    /// Print usage and exit 2.
    Usage,
}

/// Route an argv: argv[0]'s basename first (the busybox convention, and how the
/// kernel's `/sbin/init` arrives), then the explicit `td-init <applet>` form.
///
/// `pid1` is not a detail: every route that exits is a kernel panic there. So
/// PID 1 never reaches `List` or `Usage` — a dispatch miss becomes init, which
/// is what an `init=` naming the store path directly rather than a symlink
/// needs in order to boot at all.
fn route<'a>(argv: &'a [String], pid1: bool) -> Route<'a> {
    let prog = argv.first().map(String::as_str).unwrap_or("td-init");
    let self_name = basename(prog);
    if lookup(self_name).is_some() {
        return Route::Applet {
            name: self_name,
            args_from: 1,
            fallback: false,
        };
    }
    match argv.get(1).map(String::as_str) {
        Some(a) if lookup(a).is_some() => Route::Applet {
            name: a,
            args_from: 2,
            fallback: false,
        },
        Some("--list") if !pid1 => Route::List,
        _ if pid1 => Route::Applet {
            name: "init",
            args_from: 1,
            fallback: true,
        },
        _ => Route::Usage,
    }
}

fn main() -> ExitCode {
    // `args_os`, not `args`: the latter PANICS on a non-UTF-8 argument, and the
    // kernel command line is not obliged to be UTF-8.
    let argv: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let pid1 = std::process::id() == 1;

    let (name, args): (&str, &[String]) = match route(&argv, pid1) {
        Route::List => {
            let listing = format!("{}\n", names().join("\n"));
            return match emit(&listing) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    emit_err(&format!("td-init: {e}\n"));
                    ExitCode::from(1)
                }
            };
        }
        Route::Usage => {
            emit_err(&format!("{}\n", usage()));
            return ExitCode::from(2);
        }
        Route::Applet {
            name,
            args_from,
            fallback,
        } => {
            if fallback {
                let prog = argv.first().map(String::as_str).unwrap_or("td-init");
                emit_err(&format!(
                    "td-init: {}: no applet in argv; running as init because this is PID 1\n",
                    basename(prog)
                ));
            }
            (name, argv.get(args_from..).unwrap_or(&[]))
        }
    };

    let result = match lookup(name) {
        Some(run) => run(args),
        None => Err(format!("no such applet '{name}'")),
    };

    if let Err(msg) = &result {
        emit_err(&format!("{name}: {msg}\n"));
    }
    match outcome(&result, pid1) {
        Outcome::Exit(code) => ExitCode::from(code),
        Outcome::Rescue => rescue(),
    }
}

/// What to do with an applet's return value. `route` refuses to send PID 1
/// anywhere that exits, and then this is where that rule was being broken:
/// EVERY applet can return, and a return here is `main` exiting, which for PID 1
/// is "Attempted to kill init" — a kernel panic. switch_root's preflight is the
/// case that matters. Its whole purpose is to report a bad new root BEFORE the
/// mounts move; reporting it and then panicking the kernel spends that work on a
/// better-timed panic instead of a survivable one.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Exit(u8),
    /// PID 1 may not exit at all — not even successfully.
    Rescue,
}

fn outcome(result: &Result<u8, String>, pid1: bool) -> Outcome {
    if pid1 {
        return Outcome::Rescue;
    }
    match result {
        Ok(code) => Outcome::Exit(*code),
        Err(_) => Outcome::Exit(1),
    }
}

/// Shells to try, in order, when PID 1 has nothing left to run.
const RESCUE_SHELLS: [&str; 2] = ["/bin/sh", "/bin/td-sh"];

/// Keep PID 1 alive after an applet returned. A shell is the useful outcome: at
/// this point nothing has been moved or chrooted, so the initramfs the operator
/// booted is entirely intact and its shell can diagnose what the applet
/// refused. `cttyhack` gives that shell a controlling terminal and only returns
/// if the exec failed.
///
/// If no shell execs, park instead of returning. Parking reaps, because PID 1
/// still inherits every orphan on the system, and a zombie it never reaps is a
/// slot leak for as long as the machine is up.
fn rescue() -> ExitCode {
    emit_err("td-init: PID 1 cannot exit; starting a rescue shell\n");
    for shell in RESCUE_SHELLS {
        if std::path::Path::new(shell).exists() {
            let argv = [shell.to_string()];
            if let Err(e) = cttyhack::run(&argv) {
                emit_err(&format!("td-init: {e}\n"));
            }
        }
    }
    emit_err("td-init: no rescue shell; parking\n");
    loop {
        // Blocking, so this costs nothing while idle; the sleep is only for the
        // no-children case, which returns immediately and would otherwise spin.
        if let Ok(sys::Reaped::NoChildren) = sys::wait_any(false) {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn basename_strips_every_leading_component() {
        assert_eq!(basename("/sbin/init"), "init");
        assert_eq!(basename("reboot"), "reboot");
        assert_eq!(basename("/td/store/abc-td-init/bin/switch_root"), "switch_root");
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
    }

    fn argv(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_string()).collect()
    }

    fn applet<'a>(name: &'a str, args_from: usize) -> Route<'a> {
        Route::Applet {
            name,
            args_from,
            fallback: false,
        }
    }

    /// Both dispatch forms, and the un-dispatchable cases off PID 1.
    #[test]
    fn argv_routes_by_basename_then_by_the_explicit_form() {
        assert_eq!(route(&argv(&["/sbin/init"]), false), applet("init", 1));
        assert_eq!(route(&argv(&["/bin/halt", "-n"]), false), applet("halt", 1));
        assert_eq!(
            route(&argv(&["td-init", "switch_root", "/mnt"]), false),
            applet("switch_root", 2)
        );
        assert_eq!(route(&argv(&["td-init", "--list"]), false), Route::List);
        assert_eq!(route(&argv(&["td-init"]), false), Route::Usage);
        assert_eq!(route(&argv(&["td-init", "nosuch"]), false), Route::Usage);
        assert_eq!(route(&[], false), Route::Usage);
    }

    /// PID 1 has no exiting routes. `List` and `Usage` both return from `main`,
    /// and PID 1 returning is a kernel panic — so an `init=` naming the store
    /// path directly (no symlink, hence no applet basename) must still boot.
    #[test]
    fn pid_1_is_never_routed_to_something_that_exits() {
        let rescue = Route::Applet {
            name: "init",
            args_from: 1,
            fallback: true,
        };
        assert_eq!(
            route(&argv(&["/td/store/abc-td-init/bin/td-init"]), true),
            rescue
        );
        assert_eq!(route(&argv(&["td-init", "nosuch"]), true), rescue);
        assert_eq!(route(&argv(&["td-init", "--list"]), true), rescue);
        assert_eq!(route(&[], true), rescue);
        // A real applet still wins over the rescue, so `/sbin/init` and an
        // explicit `td-init init` are unaffected by it.
        assert_eq!(route(&argv(&["/sbin/init", "-f", "/etc/x"]), true), applet("init", 1));
        assert_eq!(route(&argv(&["td-init", "init"]), true), applet("init", 2));
        // ...including the irreversible ones: an initramfs runs switch_root as
        // PID 1 and must not be turned into an init by this rule.
        assert_eq!(
            route(&argv(&["td-init", "switch_root", "/mnt", "/sbin/init"]), true),
            applet("switch_root", 2)
        );
    }

    /// ...and having been routed to one, PID 1 must not exit when it RETURNS.
    /// The applet above is the case: `switch_root` refusing a bad new root is
    /// the preflight working, and exiting on it converts a survivable refusal
    /// into the kernel panic the preflight exists to avoid. Success is no
    /// different — PID 1 exiting 0 panics exactly as PID 1 exiting 1 does.
    #[test]
    fn pid_1_never_exits_whatever_the_applet_returned() {
        assert_eq!(outcome(&Err("no init in the new root".into()), true), Outcome::Rescue);
        assert_eq!(outcome(&Ok(0), true), Outcome::Rescue);
        // Anywhere else the exit status is the applet's own, and an error is 1.
        assert_eq!(outcome(&Ok(0), false), Outcome::Exit(0));
        assert_eq!(outcome(&Ok(3), false), Outcome::Exit(3));
        assert_eq!(outcome(&Err("x".into()), false), Outcome::Exit(1));
    }

    /// The roster is the shipped /bin symlink farm, so a rename is a visible
    /// change to the image, not an internal one.
    #[test]
    fn the_roster_is_the_amended_twelve() {
        assert_eq!(
            names(),
            vec![
                "cttyhack",
                "halt",
                "hostname",
                "init",
                "losetup",
                "mknod",
                "mount",
                "poweroff",
                "reboot",
                "switch_root",
                "sync",
                "umount"
            ]
        );
    }
}

/// The unsafe confinement, asserted against the crate's own source.
///
/// The crate-root deny plus ONE scoped allow is compiler-enforced, but a SECOND
/// scoped allow would be equally compiler-legal — and widening this surface is an
/// AGENTS.md amendment, not an edit. So the shape is asserted here too. Needles
/// are assembled with `concat!` so the literals never appear in the sources being
/// scanned, where they would count themselves.
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
    /// Newlines are preserved so the per-line roster scan still sees lines.
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

    /// The syscalls AGENTS.md records for this crate, with the x86_64 number
    /// each name must carry. An ELEVENTH is a reviewed amendment; this test is
    /// what makes that more than an aspiration.
    ///
    /// The NUMBERS are pinned, not just the names: renumbering `SYS_MOUNT` to
    /// 165's neighbour would otherwise change which kernel call this crate makes
    /// while every name-based assertion still passed.
    const AMENDED: &[(&str, &str)] = &[
        ("SYS_CHROOT", "161"),
        ("SYS_IOCTL", "16"),
        ("SYS_MKNOD", "133"),
        ("SYS_MOUNT", "165"),
        ("SYS_REBOOT", "169"),
        ("SYS_SETHOSTNAME", "170"),
        ("SYS_SETSID", "112"),
        ("SYS_SYNC", "162"),
        ("SYS_UMOUNT2", "166"),
        ("SYS_WAIT4", "61"),
    ];

    fn source(name: &str) -> String {
        for (n, text) in sources() {
            if n == name {
                return text;
            }
        }
        String::new()
    }

    /// Count unsafe BLOCKS, tolerating any spelling of the gap before the brace:
    /// no space, several, or a newline are one construct to the compiler, so
    /// matching a single literal spelling would leave the evasion this test
    /// exists to prevent. The needle is assembled rather than written out, since
    /// this file is one of the files being scanned.
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
                if group.split(|c: char| !c.is_alphanumeric() && c != '_').any(|t| t == lint) {
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
        // `mod` lines from EVERY scanned file, not just the crate root: a module
        // declared one level down is still a file this crate compiles.
        let mut declared = Vec::new();
        for (path, text) in &files {
            for line in text.lines() {
                // `pub mod` and `pub(crate) mod` declare a module just as much;
                // missing them would red the count below with a message naming
                // neither the file nor the cause.
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
        assert_eq!(declared.len(), 11, "expected eleven modules beside the crate root");
        // ...and nothing scanned is orphaned: a file present but declared by no
        // `mod` line is either dead or reached a way this scan does not model,
        // and either way the counts above stop meaning what they say. Matching on
        // the full relative path is what keeps a `decoy/sys.rs` from passing as
        // declared on the strength of the real `mod sys;`.
        for (path, _) in &files {
            assert!(
                path == "main.rs" || declared.iter().any(|d| d == path),
                "'{path}' was scanned but no `mod` line declares it"
            );
        }
    }

    /// `src/` holds these eight files and nothing else.
    ///
    /// The scan above proves every `mod` line has a file and every file has a
    /// `mod` line, which is a closed loop that says nothing about WHICH files:
    /// a `mod` line and its file added together satisfy both halves. A file
    /// list is a hole only when it is the sole authority — here the directory
    /// still supplies every byte the assertions read, and this pins the set the
    /// AGENTS.md amendment was written against. A new module is an amendment.
    ///
    /// The second half is why the collector keeps non-`.rs` files rather than
    /// skipping them: `src/sys.inc` is invisible to a `.rs`-only scan and
    /// compiles perfectly well through the constructs refused below.
    #[test]
    fn src_holds_exactly_the_twelve_scanned_modules() {
        let (rs, other) = walk();
        let paths: Vec<&str> = rs.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "cttyhack.rs",
                "devt.rs",
                "halt.rs",
                "hostname.rs",
                "init.rs",
                "losetup.rs",
                "main.rs",
                "mknod.rs",
                "mount.rs",
                "switchroot.rs",
                "syncfs.rs",
                "sys.rs",
            ],
            "the crate's file set changed"
        );
        assert!(
            other.is_empty(),
            "src/ holds a non-.rs file no scan here reads: {other:?}"
        );
    }

    #[test]
    fn exactly_one_scoped_allow_and_one_unsafe_block_in_the_whole_crate() {
        // Every spelling counts against the same budget of one: the outer form,
        // the inner form a new module could carry at its top, and a multi-lint
        // group with the lint somewhere in the middle of it — which a needle
        // matching only the contiguous one-lint spelling would walk past.
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
        // see and a macro can expand one audited call into two. None is needed
        // here, so the whole class is refused rather than modelled.
        //
        // `path` is refused as an ATTRIBUTE NAME, not as the `#[path` spelling:
        // `#[cfg_attr(all(), path = "…")]` is the same redirect wearing a
        // condition that is always true, and it never contains `#[path`. Both
        // positions an attribute name can take are covered — first in the
        // brackets, or after a comma inside a wrapper.
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
            source("main.rs").matches(concat!("#![deny(un", "safe_code)]")).count(),
            1,
            "the crate root must deny the unsafe lint so the scoped allow is the confinement"
        );
    }

    /// The lint's LEVEL is part of the confinement, and changing it is not
    /// itself `unsafe` — so no count above sees it. A module-level
    /// `#![warn(...)]` demotes the crate root's `deny` to a warning, and the
    /// build then ACCEPTS the asm entry point below: a ninth kernel-entry
    /// `syscall` outside sys.rs, with every assertion in this module intact
    /// (demonstrated — `objdump` counted nine).
    ///
    /// So the lint is named EXACTLY twice in the whole crate: denied at the
    /// root, allowed on the one asm body. Any third mention — a re-level, a
    /// second allow, a `forbid` somewhere that reads as reassuring — reds.
    #[test]
    fn the_unsafe_lint_is_named_exactly_twice() {
        const LINT: &str = concat!("un", "safe_code");
        let mut total = 0;
        for (path, text) in sources() {
            let count = text.matches(LINT).count();
            // main.rs denies it, sys.rs allows it on the asm body. Nowhere else
            // may name it at all, at any level.
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
    /// function the linker keeps (an `.init_array` entry runs it before main),
    /// and reaches the kernel exactly as well. Aliasing it on import also
    /// strips the `asm!` substring the pin counts.
    ///
    /// Both are closed by naming the MODULE rather than the macro: the
    /// assembly module may be named once, by the pinned block, and nowhere
    /// else — an alias has to name it too.
    #[test]
    fn the_only_route_to_inline_assembly_is_the_pinned_one() {
        let squeezed = squeezed();
        assert_eq!(
            squeezed.matches(concat!("core", "::arch")).count(),
            1,
            "the assembly module may be named only by the pinned block; an alias is a second name for the same door"
        );
        // Its other spelling, and the two macros themselves — an alias of a
        // re-export would name neither the module above nor `asm!`.
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
    /// Rust cares about: `const SYS_SYNC: usize = 162;` and a second
    /// `const SYS_SETHOSTNAME: usize = 39;` sharing one line are two items to
    /// the compiler, and the second shadows the roster's at every call site
    /// below it — `hostname -F` then issues `write(2)` with every name, number
    /// and call-site assertion here still green. Squeezed, position on a line
    /// stops existing.
    #[test]
    fn the_syscall_surface_is_the_amended_ten() {
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
        // Nothing was dropped by the parse: an entry the loop skipped would
        // leave a roster that matches while an eleventh declaration exists.
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

    /// `TIOCSCTTY`'s argument decides whether the terminal is TAKEN from the
    /// session that holds it. It is pinned to 0 in one place and asserted from
    /// the source here because nothing else would notice a 1: a steal and a
    /// clean claim are indistinguishable to every other test in this crate.
    #[test]
    fn the_controlling_terminal_is_never_stolen() {
        let sys = source("sys.rs");
        assert!(
            sys.contains("const NO_STEAL: usize = 0;"),
            "the steal flag must stay pinned OFF"
        );
        // Exactly two mentions — one declaration, one use — so a second binding
        // that shadows the pinned 0 cannot slip in beside it. The call site
        // that passes it is pinned whole below, with the other seven.
        assert_eq!(
            sys.matches("NO_STEAL").count(),
            2,
            "the steal flag is declared once and used once"
        );
    }

    /// Neither ioctl REQUEST can be shadowed out from under its pin.
    ///
    /// The value pins below assert a pinned line is PRESENT; they do not bound
    /// how many bindings of that name exist, and an inner one wins. Leave the
    /// pinned line where it is, add a second `const LOOP_SET_FD` inside
    /// `attach_loop` set to `0x4c04`, and the call becomes `LOOP_SET_STATUS64`,
    /// which reads the third argument as a POINTER to a struct — a wild kernel
    /// read at the address of a file descriptor — with every other assertion in
    /// this module still green. Two mentions each: one declaration, one use.
    #[test]
    fn neither_ioctl_request_can_be_shadowed() {
        let sys = source("sys.rs");
        for request in ["TIOCSCTTY", "LOOP_SET_FD"] {
            assert_eq!(
                sys.matches(request).count(),
                2,
                "{request} is declared once and used once; a second binding shadows the pin"
            );
        }
    }

    /// All ten calls, pinned WHOLE — every register, not just the selector.
    ///
    /// `every_syscall_call_site_uses_a_named_constant` reads argument ONE and
    /// stops. The other five are where the amendment's restrictions actually
    /// live: `ioctl` is "restricted to `TIOCSCTTY`", but that is a claim about
    /// the request register, and a different request number is a different
    /// kernel operation with the roster, the declaration pins and the selector
    /// scan all still green. Same for `reboot`'s two magics — a wrong magic is
    /// a reboot that silently does nothing — and for `mount`'s flags and data
    /// arriving from the option table rather than being composed here.
    ///
    /// Squeezed, so reformatting the multi-line calls does not red this; the
    /// argument lists are literals but `CALL` is assembled, since main.rs may
    /// not name the raw entry point.
    #[test]
    fn every_call_site_is_pinned_whole() {
        const ARGUMENTS: &[&str] = &[
            "(SYS_REBOOT,LINUX_REBOOT_MAGIC1,LINUX_REBOOT_MAGIC2,cmd,0,0)",
            "(SYS_SYNC,0,0,0,0,0)",
            "(SYS_MOUNT,source.as_ptr()asusize,target.as_ptr()asusize,nullable(fstype),flags,nullable(data),)",
            "(SYS_UMOUNT2,target.as_ptr()asusize,flags,0,0,0)",
            "(SYS_CHROOT,path.as_ptr()asusize,0,0,0,0)",
            "(SYS_SETHOSTNAME,name.as_ptr()asusize,name.len(),0,0,0,)",
            "(SYS_SETSID,0,0,0,0,0)",
            "(SYS_MKNOD,path.as_ptr()asusize,mode,dev,0,0)",
            "(SYS_IOCTL,fdasusize,TIOCSCTTY,NO_STEAL,0,0)",
            "(SYS_IOCTL,loop_fdasusize,LOOP_SET_FD,backing_fdasusize,0,0,)",
            "(SYS_WAIT4,PID_ANY,ptr::addr_of_mut!(status)asusize,opts,0,0,)",
        ];
        // One pin per CALL SITE, which is one more than the roster: `ioctl` is
        // issued twice, once per permitted request.
        assert_eq!(
            ARGUMENTS.len(),
            AMENDED.len() + 1,
            "one pin per call site; ioctl has two"
        );
        let sys = squeeze(&source("sys.rs"));
        for arguments in ARGUMENTS {
            assert_eq!(
                sys.matches(&format!("{CALL}{arguments}")).count(),
                1,
                "this call's arguments changed; re-audit it and update the pin: {arguments}"
            );
        }
    }

    /// AGENTS.md confines `ioctl` by FLAG, not just by number: "restricted to
    /// TIOCSCTTY" is one constant, and editing it keeps every name and number in
    /// the roster intact — 0x540e to 0x5412 turns the terminal claim into
    /// TIOCSTI, which injects input into another session's terminal. The reboot
    /// magics are pinned for the same reason: a wrong magic is a reboot that
    /// silently does nothing.
    ///
    /// `mount(2)` used to be confined the same way — one pinned `MS_MOVE` — and
    /// the amendment that added the `mount`/`umount` applets is exactly the one
    /// that gave it the real flag word. What replaces that pin is this list:
    /// every bit the crate may set, spelled out with its value, so a mistyped
    /// `MS_NOSUID` is not a mount that silently permits setuid. Which of them
    /// any given call composes is held by the option table (see below).
    #[test]
    fn the_flags_the_syscalls_are_restricted_to_are_pinned() {
        let sys = source("sys.rs");
        for decl in [
            "pub const MS_RDONLY: usize = 0x1;",
            "pub const MS_NOSUID: usize = 0x2;",
            "pub const MS_NODEV: usize = 0x4;",
            "pub const MS_NOEXEC: usize = 0x8;",
            "pub const MS_SYNCHRONOUS: usize = 0x10;",
            "pub const MS_REMOUNT: usize = 0x20;",
            "pub const MS_NOATIME: usize = 0x400;",
            "pub const MS_NODIRATIME: usize = 0x800;",
            "pub const MS_BIND: usize = 0x1000;",
            "pub const MS_MOVE: usize = 0x2000;",
            "pub const MS_RELATIME: usize = 0x0020_0000;",
            "pub const MNT_FORCE: usize = 0x1;",
            "pub const MNT_DETACH: usize = 0x2;",
            "const TIOCSCTTY: usize = 0x540e;",
            "const LOOP_SET_FD: usize = 0x4c00;",
            "const LINUX_REBOOT_MAGIC1: usize = 0xfee1_dead;",
            "const LINUX_REBOOT_MAGIC2: usize = 0x2812_1969;",
            "pub const REBOOT_RESTART: usize = 0x0123_4567;",
            "pub const REBOOT_HALT: usize = 0xcdef_0123;",
            "pub const REBOOT_POWER_OFF: usize = 0x4321_fedc;",
        ] {
            assert_eq!(
                sys.matches(decl).count(),
                1,
                "`{decl}` is part of the amended surface and must read exactly so"
            );
        }
    }

    /// ...and `sys.rs` declares NOTHING ELSE of the kind.
    ///
    /// The pins above prove each listed bit reads as its right value. They say
    /// nothing about a FOURTEENTH constant beside them, and the flag word is a
    /// runtime parameter now rather than the frozen `MS_MOVE` the old call-site
    /// pin froze — so `pub const MS_REC: usize = 0x4000;` would satisfy every
    /// assertion here, become composable by the option table, and reach mount
    /// propagation, an operation this crate has no business performing, with
    /// the roster, the call-site pins and the naming confinement all green.
    ///
    /// This is the half of the widened confinement that lives in the source;
    /// `mount.rs` carries the other half, pinning the option table's BITS to
    /// these same constants so a bare `0x4000` in the table cannot reach the
    /// kernel either.
    #[test]
    fn the_mount_flag_roster_is_exactly_the_amended_set() {
        const EXPECTED: &[&str] = &[
            "MNT_DETACH",
            "MNT_FORCE",
            "MS_BIND",
            "MS_MOVE",
            "MS_NOATIME",
            "MS_NODEV",
            "MS_NODIRATIME",
            "MS_NOEXEC",
            "MS_NOSUID",
            "MS_RDONLY",
            "MS_RELATIME",
            "MS_REMOUNT",
            "MS_SYNCHRONOUS",
        ];
        // sys.rs alone, because main.rs's own pins above quote these
        // declarations verbatim and would count themselves.
        let sys = squeeze(&source("sys.rs"));
        let mut declared: Vec<String> = Vec::new();
        for (offset, _) in sys.match_indices("const") {
            let rest = sys.get(offset + "const".len()..).unwrap_or_default();
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if name.starts_with(concat!("MS", "_")) || name.starts_with(concat!("MNT", "_")) {
                declared.push(name);
            }
        }
        declared.sort();
        assert_eq!(declared, EXPECTED, "the mount-flag surface changed");
    }

    /// The two filesystem wrappers are reachable from `mount.rs` and, for the
    /// pivot's `MS_MOVE`, `switchroot.rs` — and nowhere else.
    ///
    /// `sys::mount` takes `flags: usize`, so any module that can call it can
    /// pass a numeric literal and reach an operation no option table describes.
    /// That was structurally impossible while the wrapper had no flags
    /// parameter; naming is what replaces it.
    #[test]
    fn the_filesystem_wrappers_are_called_only_from_the_two_permitted_modules() {
        for (call, permitted) in [
            (concat!("sys::", "mount"), &["mount.rs", "switchroot.rs"][..]),
            (concat!("sys::", "umount"), &["mount.rs"][..]),
            // `attach_loop` takes two raw descriptors, so any module that can
            // call it can bind an arbitrary open file to an arbitrary loop
            // device. One caller is what keeps the read-back below meaningful.
            (concat!("sys::", "attach_loop"), &["losetup.rs"][..]),
            // `mknod` takes `mode`, whose top bits are the node TYPE and so choose
            // the driver class. A caller outside `mknod.rs` could compose a
            // character node and skip the readback that makes this applet safe.
            (concat!("sys::", "mknod"), &["mknod.rs"][..]),
        ] {
            for (path, text) in sources() {
                if path == "sys.rs" || path == "main.rs" || permitted.contains(&path.as_str()) {
                    continue;
                }
                assert_eq!(
                    text.matches(call).count(),
                    0,
                    "'{path}' calls {call}; only {permitted:?} may"
                );
            }
        }
        // A NAMED import defeats it the same way a glob does: `use
        // crate::sys::attach_loop;` then a bare `attach_loop(...)` matches
        // nothing the loop above looks for. No module but the permitted ones
        // may import out of `sys` at all, so every reach is qualified and
        // therefore visible.
        for (path, text) in sources() {
            if path == "sys.rs" || path == "main.rs" {
                continue;
            }
            let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            // Whitespace is already squeezed out of the text, so these forms
            // carry none either — `use crate::sys as raw` becomes
            // `usecrate::sysas`, and a pattern with a space in it would match
            // nothing at all while looking like it did.
            for form in [
                concat!("use", "crate::sys::"),
                concat!("use", "super::sys::"),
                concat!("use", "crate::sysas"),
                concat!("use", "super::sysas"),
            ] {
                assert_eq!(
                    squeezed.matches(form).count(),
                    0,
                    "'{path}' imports out of the syscall module ('{form}'); every reach \
                     must be spelled `sys::<wrapper>` where the caller scan can see it"
                );
            }
        }
        // A glob import would name neither wrapper and defeat the scan above.
        assert_eq!(
            squeezed().matches(concat!("sys::", "*")).count(),
            0,
            "a glob import of the syscall module hides which wrappers a caller reaches"
        );
    }

    /// Where a mount flag may be NAMED. The pins above say what each bit is
    /// worth; this says who may compose one, which is the half that decides
    /// what the kernel is actually asked to do.
    ///
    /// `mount.rs` is entitled to all of them: the option table every `-o` word
    /// routes through, the `-r`/`-w` shorthands for the bit that table already
    /// owns, and `umount -r`'s read-only remount. `switch_root` is entitled to
    /// exactly `MS_MOVE`, twice: the API-mount move and the root move. Anywhere
    /// else, a flag word would be a mount operation no table describes and no
    /// `-o` spelling can reach.
    #[test]
    fn the_mount_flag_names_are_confined_to_the_table_that_composes_them() {
        const MS: &str = concat!("MS", "_");
        const MNT: &str = concat!("MNT", "_");
        for (path, text) in sources() {
            match path.as_str() {
                // Declares them; and main.rs pins them, just above.
                "sys.rs" | "main.rs" | "mount.rs" => {}
                "switchroot.rs" => {
                    assert_eq!(
                        text.matches(MS).count(),
                        2,
                        "switch_root moves two mounts and may name no other flag"
                    );
                    assert_eq!(
                        text.matches(MNT).count(),
                        0,
                        "switch_root never unmounts — it moves and frees"
                    );
                }
                _ => assert_eq!(
                    text.matches(MS).count() + text.matches(MNT).count(),
                    0,
                    "'{path}' composes a mount flag; only mount.rs's option table may"
                ),
            }
        }
    }

    /// `reboot(2)`'s command word decides what the call DOES, and the wrapper
    /// takes it as a plain `usize` from its caller — so the roster's "reboot is
    /// issued once" says nothing about which of its operations. The crate may
    /// name exactly five of them: the three commands the applets implement, and
    /// the two magics. A sixth would be a new kernel operation (KEXEC boots a
    /// different kernel; CAD_ON hands Ctrl-Alt-Del to the BIOS) reached through
    /// an audited wrapper with every other assertion here green.
    #[test]
    fn only_the_three_reboot_commands_are_named() {
        const PREFIX: &str = concat!("REB", "OOT_");
        let allowed = ["RESTART", "HALT", "POWER_OFF", "MAGIC1", "MAGIC2"];
        let squeezed = squeezed();
        for (offset, _) in squeezed.match_indices(PREFIX) {
            let rest = squeezed.get(offset + PREFIX.len()..).unwrap_or_default();
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            assert!(
                allowed.contains(&name.as_str()),
                "this crate names a reboot operation it does not implement: {PREFIX}{name}"
            );
        }
    }

    /// Safe `std` reaches some of these kernel calls too, and the amendment is
    /// about the RAW surface — `std` is trusted, so this module does not police
    /// it in general. `chroot` is the exception worth naming: `std` has one, and
    /// a second path to a syscall the roster lists would make "chroot is issued
    /// exactly once" read as a claim about the binary when it is a claim about
    /// `sys.rs`. One route, so the two agree.
    #[test]
    fn the_rostered_syscalls_have_no_second_route_through_std() {
        assert_eq!(
            squeezed().matches(concat!("fs::", "chroot")).count(),
            0,
            "use sys::chroot, the audited wrapper, not std's"
        );
    }

    /// The raw-syscall entry point, assembled rather than written out: the test
    /// below asserts the name appears in NO source but `sys.rs`, and this file
    /// is one of the sources it scans.
    const CALL: &str = concat!("sys", "call5");

    /// Every call site selects its syscall through one of the eight named
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
        // The gap before the paren is whitespace to the compiler, so a call
        // spelled with one is a call like any other — and one a needle ending
        // in `(` never sees.
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
                "called with '{selector}', which is not one of the amended ten"
            );
            // The constant must BE the argument, not the start of an expression:
            // `SYS_REBOOT - 130` selects getpid(2) while spelling an audited
            // name, so the next thing after it has to be the comma.
            let rest = after.get(selector.len()..).unwrap_or("").trim_start();
            assert!(
                rest.starts_with(','),
                "the selector must be '{selector}' itself, not an expression built from it"
            );
            selected.push(selector);
        }
        // Each of the ten exactly once, EXCEPT ioctl, which is issued twice
        // because it is the one syscall with two permitted requests
        // (`TIOCSCTTY` for cttyhack, `LOOP_SET_FD` for losetup). Membership
        // alone would let every site name SYS_REBOOT while a wrapper quietly
        // issued a different call than the one it is named for; spelling the
        // expected multiset out keeps that closed while the roster widens.
        selected.sort();
        let mut roster: Vec<String> = AMENDED.iter().map(|(n, _)| (*n).to_string()).collect();
        roster.push("SYS_IOCTL".to_string());
        roster.sort();
        assert_eq!(
            selected, roster,
            "each amended syscall is issued exactly once, and ioctl exactly twice"
        );
        // One call per wrapper: reboot, sync, mount, umount2, chroot,
        // sethostname, setsid, ioctl x2, wait4.
        assert_eq!(sites, 11, "expected exactly eleven call sites");
        // ...and the definition, and NOTHING else. The loop skips any mention
        // not followed by `(`, which is the function ITEM: bind it once and
        // every later call goes through a name this scan does not know.
        assert_eq!(
            mentions,
            sites + 1,
            "mentioned somewhere that is not one of the eleven calls or its definition"
        );
    }

    /// The raw entry point is private to `sys.rs`, and the module header says so
    /// — "its confinement is module privacy plus the typed wrappers being
    /// its only callers". Nothing checked it: the scan above reads `sys.rs`
    /// alone, so exporting the function and calling it from another module put
    /// a ninth syscall in the binary with all eight audited sites intact.
    #[test]
    fn the_raw_entry_point_is_private_to_its_module() {
        // Pinned to the scoped allow that immediately precedes it, which is
        // both stricter and more honest than a leading newline: `pub` split
        // onto the line above satisfies "\nfn", and squeezing makes the line
        // irrelevant anyway. The allow is unique (asserted above), so this
        // fixes the definition's visibility as the empty one.
        let sys = squeeze(&source("sys.rs"));
        assert_eq!(
            sys.matches(&format!("{}{CALL}(", concat!("#[allow(un", "safe_code)]fn"))).count(),
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
    /// without changing any count — and `#[inline]` then multiplies whatever
    /// was added across all eight wrappers. So the block is pinned WHOLE.
    ///
    /// Compared with whitespace squeezed out, which makes the pin immune to
    /// reformatting and to every respelling that hid code from earlier
    /// revisions of these scans. Assembled from fragments because main.rs is
    /// itself scanned, and a literal here would be a second block and a second
    /// instruction to the assertions above.
    #[test]
    fn the_confined_block_is_pinned_whole() {
        const BLOCK: &str = concat!(
            "un",
            "safe{core",
            "::arch::a",
            "sm!(\"syscall\",inlateout(\"rax\")nasisize=>ret,",
            "in(\"rdi\")a1,in(\"rsi\")a2,in(\"rdx\")a3,in(\"r10\")a4,in(\"r8\")a5,",
            "out(\"rcx\")_,out(\"r11\")_,options(nostack),);}"
        );
        let squeezed = squeezed();
        assert_eq!(
            squeezed.matches(BLOCK).count(),
            1,
            "the confined block's body changed; re-audit it and update this pin"
        );
        // ...and no inline assembly anywhere else in the crate.
        assert_eq!(
            squeezed.matches(concat!("a", "sm!")).count(),
            1,
            "exactly one inline-assembly invocation may exist in this crate"
        );
    }
}
