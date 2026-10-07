#![deny(unsafe_code)]
//! td-util — the static, dependency-free multicall behind td's diagnostics and
//! PRE-PIVOT userland: the applets that must run where uutils' coreutils cannot,
//! and that need no syscall surface beyond what safe `std` already exposes.
//!
//! Two kinds of caller, one reason. `clear`/`which`/`free`/`ps`/`dmesg`/`less` are
//! names uutils does not provide at all — `less` not quite: uutils has a pager
//! behind a feature flag that compiles a crossterm stack in, which is a dependency
//! this image does not take, so busybox was being carried for `more` alone.
//! `cat`/`chmod`/`chown`/`ln`/`mkdir`/`printf`/`readlink`/`rm`/`sleep`/`test` it
//! DOES provide — dynamically linked, against a runtime closure that the pre-pivot
//! initramfs has no loader for and that the boot self-check exists to report the
//! breakage of. Both sets need a binary that works when the closure does not, which
//! is what this one is.
//!
//! uutils owns those `/bin` names (the farms are disjoint), so these are reached
//! as `td-util <applet>` — the same explicit form the `busybox <applet>` calls
//! they replace used.
//!
//! Dispatch is on argv[0]'s basename, the busybox/uutils convention, so a
//! `/bin/<applet> -> td-util` symlink runs that applet. An explicit
//! `td-util <applet> [args]` form covers the un-symlinked case.
//!
//! A third kind replaces busybox in the build graph: `find`, `xargs`, `cmp`,
//! `diff`, `gzip`/`gunzip`/`zcat` and `cpio`, the tools recipes and the
//! initramfs build drive. Each takes the subset those callers use and refuses
//! the rest rather than guessing at it. `gzip` decodes with the engine's
//! inflater and CRC, included below, so one decoder serves source preparation
//! and this.
//!
//! Almost everything here is safe `std`: `/proc` and `/dev/kmsg` are ordinary
//! files, and the build tools walk, read and spawn through `std`. `less` is
//! the exception, and the reason this crate root `deny`s the
//! unsafe lint rather than `forbid`ding it — a pager that cannot take a keystroke
//! without waiting for Enter, or ask how many rows the screen has, is not a pager,
//! and both are `ioctl(2)`. That surface is ONE syscall with THREE pinned requests,
//! confined to `sys.rs` with `term.rs` its only caller; it is the SEVENTH
//! target-side unsafe exception UNSAFE.md records, and `mod confinement` below
//! asserts every part of it against this crate's own source. The applets that would
//! need a syscall beyond that roster (reboot/poweroff/halt, switch_root, cttyhack,
//! init) are deliberately absent — adding one is a further reviewed amendment, not
//! a drive-by.

mod cat;
mod cmp;
mod cpio;
// The engine's CRC and inflater, the ones source preparation trusts, rather
// than a second copy. Std-only; the confinement tests below pin these two
// attributes and scan both files.
#[path = "../../engine/src/crc32.rs"]
#[allow(dead_code)]
// The engine file's own tests, held to the engine's lints rather than these.
#[cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]
mod crc32;
mod deflate;
mod diff;
mod dmesg;
mod fileattr;
mod fileops;
mod find;
mod free;
mod glob;
mod gz;
#[path = "../../engine/src/gzip.rs"]
#[allow(dead_code)]
// The engine file's own tests, held to the engine's lints rather than these.
#[cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]
mod gzip;
mod less;
mod printf;
mod procfs;
mod ps;
mod sleep;
mod sys;
mod term;
mod test;
mod which;
mod xargs;

use std::io::Write;
use std::process::ExitCode;

type Applet = fn(&[String]) -> Result<u8, String>;

/// Every applet this multicall serves, paired with the function that runs it.
/// ONE table, so a name cannot exist without an arm or an arm without a name —
/// `--list`, argv[0] dispatch, and the shipped /bin symlink farm all read it.
const APPLETS: &[(&str, Applet)] = &[
    ("cat", cat::run),
    ("chmod", fileattr::chmod),
    ("chown", fileattr::chown),
    ("clear", clear),
    ("cmp", cmp::run),
    ("cpio", cpio::run),
    ("diff", diff::run),
    ("dmesg", dmesg::run),
    ("find", find::run),
    ("free", free::run),
    ("gunzip", gz::gunzip),
    ("gzip", gz::gzip),
    ("less", less::run),
    ("ln", fileops::ln),
    ("mkdir", fileops::mkdir),
    ("printf", printf::run),
    ("ps", ps::run),
    ("readlink", fileops::readlink),
    ("rm", fileops::rm),
    ("sleep", sleep::run),
    ("test", test::run),
    ("which", which::run),
    ("xargs", xargs::run),
    ("zcat", gz::zcat),
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
    emit_bytes(text.as_bytes())
}

/// `emit` for bytes that need not be UTF-8: file contents, paths.
pub fn emit_bytes(bytes: &[u8]) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Diagnostics to stderr. `eprintln!` panics on a failed write for the same
/// reason `println!` does (and `panic = "abort"` turns that into a SIGABRT on
/// `2>/dev/full`), and there is nowhere left to report a failure to report a
/// failure — so drop it and let the exit code carry the outcome.
pub fn emit_err(text: &str) {
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

/// The crate's `unsafe` surface, asserted against its own source TEXT.
///
/// td-util is the SEVENTH target-side unsafe exception UNSAFE.md records, and
/// like the six before it the exception is worth what its confinement is worth.
/// The compiler checks that `unsafe` appears only where a scoped `#[allow]`
/// permits it; it cannot check that there is exactly ONE such allow, that the
/// assembly body is the audited one, that the syscall number and the three ioctl
/// requests are the amended ones, or that nothing but `term.rs` can reach them.
/// These do, by reading `src/` off disk.
#[cfg(test)]
mod confinement {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use std::path::Path;
    use td_source_scan::{self as scan, squeeze, unsafe_allows, unsafe_blocks, unsafe_items};

    /// Every source file of the crate, read from disk: see `scan::read_tree`.
    fn walk() -> scan::Tree {
        let base = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
        scan::read_tree(base).unwrap()
    }

    fn sources() -> Vec<(String, String)> {
        walk().rs
    }

    fn source(name: &str) -> String {
        scan::source(&sources(), name)
    }

    fn squeezed() -> String {
        scan::squeezed(&sources())
    }

    /// How a syscall number is declared, assembled rather than written out: the
    /// scan above refuses this spelling in every file but `sys.rs`, and this
    /// file is one of the sources it reads.
    const DECL: &str = concat!("const", "SYS_");

    /// The syscall UNSAFE.md records for this crate, with the x86_64 number it
    /// must carry. A SECOND is a reviewed amendment; this is what makes that
    /// more than an aspiration.
    ///
    /// The NUMBER is pinned, not just the name: renumbering `SYS_IOCTL` to a
    /// neighbour would otherwise change which kernel call this crate makes
    /// while every name-based assertion still passed. 16 is `ioctl`; 15 is
    /// `rt_sigreturn` and 17 is `pread64`, which is what a fat-finger would reach.
    const AMENDED: &[(&str, &str)] = &[("SYS_IOCTL", "16")];

    const CALL: &str = concat!("sys", "call3");

    /// The THREE permitted `ioctl` requests, value-pinned. `ioctl(2)` is one
    /// syscall onto an unbounded space of operations, so the number in `rax` is
    /// not the surface — the request in `rsi` is, and it is what a fourth entry
    /// here would widen. Every one of these is about the shape or the read mode
    /// of a terminal this process already holds open.
    const REQUESTS: &[(&str, &str)] = &[
        ("TCGETS", "0x5401"),
        ("TCSETS", "0x5402"),
        ("TIOCGWINSZ", "0x5413"),
    ];

    /// The typed wrappers, one per request, and `syscall3`'s only callers.
    const WRAPPERS: &[&str] = &["termios_get", "termios_set", "window_size"];

    /// Neighbours of the three that are deliberately NOT in the surface, each
    /// one request-number digit away from something admitted. Checked against
    /// `sys.rs` alone: it is the only file whose text can reach the kernel, since
    /// the three call sites below are pinned whole and all three live there.
    const REFUSED: &[&str] = &["TCSETSW", "TCSETSF", "TIOCSWINSZ", "TIOCSTI", "TIOCSCTTY"];

    /// The engine files this crate compiles by `#[path]`, and the squeezed
    /// attribute that reaches each. Nothing else may leave `src/`.
    const ENGINE: &[(&str, &str)] = &[
        (
            "crc32.rs",
            concat!("#[", "path=\"../../engine/src/crc32.rs\"]"),
        ),
        (
            "gzip.rs",
            concat!("#[", "path=\"../../engine/src/gzip.rs\"]"),
        ),
    ];

    /// The engine files are compiled under this crate's `deny`, so an `allow`
    /// or an `unsafe` token in one would escape the `src/` scans: refuse both.
    #[test]
    fn the_engine_includes_are_pinned_and_hold_no_unsafe() {
        let main = squeeze(&source("main.rs"));
        for (file, attr) in ENGINE {
            assert_eq!(main.matches(attr).count(), 1, "{file}'s #[path] attribute");
            let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../engine/src")).join(file);
            let text = scan::strip_comments(&std::fs::read_to_string(&path).unwrap());
            assert!(!text.contains(concat!("un", "safe")), "{file} names unsafe");
            for construct in scan::DECOUPLING {
                assert!(!squeeze(&text).contains(construct), "{file}: `{construct}`");
            }
        }
    }

    /// Reading the directory only equals reading `main.rs` if the two agree, so
    /// assert it: a `mod` whose file the scan missed — an `#[path]` attribute
    /// pointing outside `src/`, say — would make every assertion below vacuous
    /// for exactly the file that needed checking.
    #[test]
    fn the_scan_covers_every_module_the_crate_declares() {
        let files = sources();
        let declared: Vec<String> = scan::declared_modules(&files)
            .into_iter()
            .filter(|d| !ENGINE.iter().any(|(f, _)| f == d))
            .collect();
        for target in &declared {
            assert!(
                files.iter().any(|(f, _)| f == target),
                "module '{target}' is declared but was not scanned"
            );
        }
        assert_eq!(
            declared.len(),
            22,
            "expected twenty-two modules in src/ beside the crate root"
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

    /// `src/` holds these files and nothing else.
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
    fn src_holds_exactly_the_scanned_modules() {
        let scan::Tree { rs, other } = walk();
        let paths: Vec<&str> = rs.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "cat.rs",
                "cmp.rs",
                "cpio.rs",
                "deflate.rs",
                "diff.rs",
                "dmesg.rs",
                "fileattr.rs",
                "fileops.rs",
                "find.rs",
                "free.rs",
                "glob.rs",
                "gz.rs",
                "less.rs",
                "main.rs",
                "printf.rs",
                "procfs.rs",
                "ps.rs",
                "sleep.rs",
                "sys.rs",
                "term.rs",
                "test.rs",
                "which.rs",
                "xargs.rs",
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
        // group with the lint somewhere in the middle of it.
        assert_eq!(
            sources()
                .iter()
                .map(|(_, t)| unsafe_allows(t))
                .sum::<usize>(),
            1,
            "the crate must carry exactly ONE scoped unsafe allow (the asm body in sys.rs)"
        );
        assert_eq!(
            sources()
                .iter()
                .map(|(_, t)| unsafe_blocks(t))
                .sum::<usize>(),
            1,
            "the crate must carry exactly ONE unsafe block (the asm body in sys.rs)"
        );
        for form in ["fn", "impl", "trait"] {
            assert_eq!(
                sources()
                    .iter()
                    .map(|(_, t)| unsafe_items(t, form))
                    .sum::<usize>(),
                0,
                "no item of this form may exist in this crate: {form}"
            );
        }
        // These would make the scanned text stop describing the compiled crate,
        // and every assertion here reads text. The pinned engine includes are
        // the exceptions, each removed once.
        let mut squeezed = squeezed();
        for (_, attr) in ENGINE {
            squeezed = squeezed.replacen(attr, "", 1);
        }
        for construct in scan::DECOUPLING {
            assert_eq!(
                squeezed.matches(construct).count(),
                0,
                "`{construct}` would decouple the compiled crate from the scanned source"
            );
        }
    }

    /// The scoped allow is only sound under a crate-level `deny`.
    ///
    /// td-util was `forbid(unsafe_code)` until `less` needed to take a keystroke
    /// without waiting for Enter. `forbid` cannot be relaxed by an inner
    /// attribute, which is exactly why it had to become `deny` — and `deny` CAN
    /// be relaxed, so the count below is what stops a second module quietly
    /// allowing itself the same thing.
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
            squeezed.matches(&format!("{}![deny({lint})]", "#")).count(),
            1,
            concat!("the crate root must DENY unsafe", "_code")
        );
        assert_eq!(
            squeezed
                .matches(&format!("{}![forbid({lint})]", "#"))
                .count(),
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

        // Every mention of the prefix is accounted for: one declaration and one
        // use per wrapper, and NOTHING else. Without this the roster is a claim
        // about `const` items only — a `static SYS_IOCTL: usize = 200;` inside a
        // wrapper shadows the roster's at the point of use and leaves the
        // declaration scan, the call-site pin (which matches the NAME) and the
        // named-constant check all green.
        assert_eq!(
            sys.matches(concat!("SYS", "_")).count(),
            AMENDED.len() + WRAPPERS.len(),
            "sys.rs names a syscall constant somewhere that is neither its one \
             declaration nor one of its three call sites; it may be shadowed there"
        );
    }

    /// The three requests are declared once each, value-pinned, in `sys.rs`.
    ///
    /// `ioctl(2)`'s number says nothing about what it DOES; the request does.
    /// `TCSETS` mistyped as `TCSETSF` still compiles, still passes every scan
    /// that reads the syscall number, and discards whatever the reader had
    /// already typed at the terminal.
    #[test]
    fn the_ioctl_requests_are_exactly_three_and_value_pinned() {
        let sys = squeeze(&source("sys.rs"));
        for (name, value) in REQUESTS {
            assert_eq!(
                sys.matches(&format!("const{name}:usize={value};")).count(),
                1,
                "{name} must be declared exactly once in sys.rs as {value}"
            );
            // Declared once and used once: any further mention is a place the
            // pinned constant could be shadowed or recomputed.
            assert_eq!(
                sys.matches(name).count(),
                2,
                "{name} must be named exactly twice: its declaration and its call site"
            );
            for (file, text) in sources() {
                if file == "sys.rs" {
                    continue;
                }
                assert!(
                    !squeeze(&text).contains(&format!("const{name}")),
                    "{file} declares an ioctl request; they belong in sys.rs"
                );
            }
        }
        for refused in REFUSED {
            assert_eq!(
                sys.matches(refused).count(),
                0,
                "{refused} is deliberately outside this crate's ioctl surface"
            );
        }
    }

    /// The call sites select their syscall through the ONE named constant, and
    /// the entry point is named nowhere but at its definition and those calls.
    ///
    /// Without the first half the roster is only a claim about declarations: a
    /// bare `(999, ...)` would reach an unaudited kernel call while leaving the
    /// constant untouched. Without the second, `let f = syscall3;` binds the
    /// entry point to a name this scan does not know and every later call goes
    /// through it — the pins below still match their sites, and the crate has a
    /// syscall surface nobody counted.
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
            // `SYS_IOCTL + 1` selects a different call while spelling an audited
            // name.
            let rest = after.get(selector.len()..).unwrap_or("").trim_start();
            assert!(
                rest.starts_with(','),
                "the selector must be '{selector}' itself, not an expression built from it"
            );
            selected.push(selector);
        }
        assert_eq!(
            sites,
            WRAPPERS.len(),
            "expected exactly one call site per typed wrapper"
        );
        assert!(
            selected.iter().all(|s| s == "SYS_IOCTL"),
            "every call site must select the one amended syscall: {selected:?}"
        );
        // ...and the definition, and NOTHING else. The loop skips any mention
        // not followed by `(`, which is the function ITEM: bind it once and
        // every later call goes through a name this scan does not know.
        assert_eq!(
            mentions,
            sites + 1,
            "mentioned somewhere that is not a call or its definition"
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
    /// to `syscall3`; only this pins where those arguments land, and `in("rsi")`
    /// changed to `in("rdx")` compiles, passes every other assertion, and hands
    /// the kernel a request number where the buffer pointer belongs.
    ///
    /// `options(nomem)` being ABSENT is part of the pin, not an omission: TCGETS
    /// and TIOCGWINSZ have the kernel WRITE through one of those pointers, and
    /// promising the compiler this asm touches no memory would let it keep a
    /// stale buffer across the call.
    #[test]
    fn the_confined_block_is_pinned_whole() {
        const BLOCK: &str = concat!(
            "un",
            "safe{core",
            "::arch::a",
            "sm!(\"syscall\",inlateout(\"rax\")nasisize=>ret,",
            "in(\"rdi\")a1,in(\"rsi\")a2,in(\"rdx\")a3,",
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

    /// The call sites, pinned WHOLE — every register, not just the selector.
    ///
    /// The roster pins WHICH syscall and the requests pin their values; this
    /// pins what is handed to each call. A descriptor and a request swapped past
    /// each other would still be `ioctl(2)`, still use named constants, and
    /// would ask file descriptor 0x5401 for something.
    #[test]
    fn every_call_site_is_pinned_whole() {
        const ARGUMENTS: &[&str] = &[
            "(SYS_IOCTL,fdasusize,TCGETS,out.as_mut_ptr()asusize,)",
            "(SYS_IOCTL,fdasusize,TCSETS,termios.as_ptr()asusize,)",
            "(SYS_IOCTL,fdasusize,TIOCGWINSZ,out.as_mut_ptr()asusize,)",
        ];
        assert_eq!(ARGUMENTS.len(), WRAPPERS.len(), "one pin per typed wrapper");
        let sys = squeeze(&source("sys.rs"));
        for arguments in ARGUMENTS {
            assert_eq!(
                sys.matches(&format!("{CALL}{arguments}")).count(),
                1,
                "the {CALL} call site is not the pinned one: {arguments}"
            );
        }
        // Three call sites in the crate, all in sys.rs, plus the definition.
        assert_eq!(
            squeezed().matches(&format!("{CALL}(")).count(),
            ARGUMENTS.len() + 1,
            "the raw entry point must be defined once and called once per wrapper"
        );
    }

    /// The raw entry point is private to its module.
    #[test]
    fn the_raw_entry_point_is_private_to_its_module() {
        let sys = squeeze(&source("sys.rs"));
        // The scoped allow is part of the pin: matching a bare `fn syscall3(`
        // would let a SECOND, unannotated definition satisfy "declared exactly
        // once" as long as the annotated one moved elsewhere in the file.
        assert_eq!(
            sys.matches(&format!(
                "{}{CALL}(",
                concat!("#[allow(un", "safe_code)]fn")
            ))
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

    /// Only the terminal layer reaches the three wrappers.
    ///
    /// The wrappers are `pub`, so module privacy cannot hold them — this does.
    /// `term.rs` is where the termios layout lives and where raw mode is read
    /// back before it is trusted; a second caller would be a second place that
    /// decides what a termios byte means, which is the one thing that file
    /// exists to prevent.
    #[test]
    fn only_the_terminal_layer_reaches_the_ioctl_wrappers() {
        let prefix = concat!("sys", "::");
        // NOTHING may import an item OUT of the syscall module, or alias it.
        // Tokens off the un-squeezed text, because the forms differ only by
        // whitespace once it is gone: `use crate::{sys as t};` squeezes into one
        // word, and a prefix scan never sees the brace-group or the alias at
        // all. Either would give an audited call a name the scans below do not
        // look for, so the module may be named ONE way and no other.
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
                if !item
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|token| token == "sys")
                {
                    continue;
                }
                assert_eq!(
                    squeeze(item),
                    "usecrate::sys",
                    "{name} names the syscall module some way other than plain \
                     `use crate::sys;`; every call to it must stay spelled `{prefix}…` \
                     so the scans below can see it"
                );
            }
        }
        for wrapper in WRAPPERS {
            let mention = format!("{prefix}{wrapper}");
            let call = format!("{mention}(");
            for (name, text) in sources() {
                let squeezed = squeeze(&text);
                let calls = squeezed.matches(&call).count();
                let mentions = squeezed.matches(&mention).count();
                // Every MENTION is a call. `let get = crate::sys::termios_get;`
                // is a mention that is not one, and every later `get(...)` is a
                // call this scan cannot see.
                assert_eq!(
                    mentions, calls,
                    "{name} names {wrapper} somewhere that is not a call; it would be \
                     reachable under another name"
                );
                if name != "term.rs" {
                    assert_eq!(calls, 0, "{name} calls {wrapper}; only term.rs may");
                }
            }
            // Inside sys.rs the wrappers are reachable UNQUALIFIED, which no
            // scan above covers. Its own tests do exactly that, so the
            // production half is what gets checked — everything before the test
            // module, where the only mention may be the definition itself.
            let sys = squeeze(&source("sys.rs"));
            let production = match sys.match_indices(concat!("#[cfg(", "test)]")).next() {
                Some((at, _)) => sys.get(..at).unwrap_or(&sys),
                None => sys.as_str(),
            };
            assert_eq!(
                production.matches(&format!("{wrapper}(")).count(),
                1,
                "sys.rs calls its own {wrapper} outside its tests; term.rs is the only \
                 caller the confinement admits"
            );
        }
        // ...and term.rs really does call all three, or one wrapper is dead code
        // and the assertions above are vacuous for it.
        let term = squeeze(&source("term.rs"));
        for wrapper in WRAPPERS {
            assert!(
                term.contains(&format!("{prefix}{wrapper}(")),
                "term.rs never calls {wrapper}; the confinement above says nothing"
            );
        }
    }

    /// The pager BORROWS the terminal; it must never move it into a reader.
    ///
    /// A source-text assertion because the gate has no terminal, and because the
    /// failure is silent in exactly the way that matters. Rust drops locals in
    /// reverse declaration order, so a `BufReader` that OWNS the `File` closes the
    /// descriptor before the raw-mode guard restores through it; the restore then
    /// fails EBADF into a `let _`, and every interactive run ends at a shell with
    /// no echo and no line editing. Nothing about that is visible from the code at
    /// the call site, which is why it is pinned here.
    ///
    /// The capacity is part of the pin for a different reason: in raw mode a read
    /// returns whatever has been typed, so a larger buffer swallows whatever the
    /// reader typed ahead for their SHELL and discards it on exit.
    #[test]
    fn the_command_reader_borrows_the_terminal_rather_than_owning_it() {
        let less = squeeze(&source("less.rs"));
        assert_eq!(
            less.matches("BufReader::with_capacity(1,&tty)").count(),
            1,
            "the command reader must borrow the terminal, with a one-byte buffer"
        );
        assert_eq!(
            less.matches("BufReader::new(tty)").count(),
            0,
            "moving the terminal into the reader closes it before the raw-mode \
             guard can restore through it"
        );
    }

    /// Raw mode is READ BACK before it is trusted, and restored by `Drop`.
    ///
    /// Source-text assertions because neither is observable without a real
    /// terminal, which the gate does not have. Both matter more than they look:
    /// a `TCSETS` can succeed having applied only part of what was asked, and a
    /// terminal still in canonical mode is indistinguishable from one whose
    /// reader has not typed yet — the pager would simply appear to hang. And
    /// without `Drop`, every exit path that is not the happy one (`q`, EOF, a
    /// write error, a `?` three frames down) leaves the operator at a shell
    /// prompt with no echo and no line editing.
    #[test]
    fn raw_mode_is_read_back_and_restored() {
        // `match_indices` rather than the obvious lookup method: this file is
        // embedded verbatim into the recipe, and the ladder guard scans that
        // text for host-tool names the method happens to spell.
        let at = |hay: &str, what: &str| hay.match_indices(what).next().map(|(i, _)| i);
        let term = squeeze(&source("term.rs"));
        let start = at(&term, concat!("fnraw(tty:Borrowed", "Fd<'_>)"));
        assert!(start.is_some(), "term::raw must exist");
        let body = term.get(start.unwrap_or_default()..).unwrap_or_default();
        // Bounded to raw() and the readback_refusal() helper that follows it,
        // whose refusals are counted below. Left running to end-of-file, every
        // scan below would be answered by some other function — or by this
        // module's own tests, which legitimately spell all of these.
        let ends = at(body, "pubfnsize(");
        assert!(
            ends.is_some(),
            "term::size must follow raw(); the scan needs a bound"
        );
        let rest = body.get(..ends.unwrap_or(body.len())).unwrap_or_default();
        // Without a `?`: a `TCSETS` that applied part of the patch and still
        // reported failure would otherwise leave that half-raw terminal behind,
        // so the patch's own failure restores too. Where it begins is where
        // "after the terminal was patched" begins.
        let patch = concat!("sys", "::termios_set(fd,&want)");
        let set = at(rest, patch);
        let readback = at(rest, concat!("sys", "::termios_get(fd,&mutgot)"));
        // The refusal is the readback's verdict, asked of the word `raw` WROTE:
        // handed the word it read instead, the whole-word compare is a tautology.
        let refuse = at(rest, "readback_refusal(&got,&saved,lflag)");
        assert!(set.is_some(), "raw() must set the patched termios");
        assert!(
            readback.is_some(),
            "raw() must read the termios back; a partial TCSETS is silent"
        );
        assert!(
            refuse.is_some(),
            "raw() must refuse, through readback_refusal over the word it wrote, \
             a terminal that did not switch"
        );
        assert!(
            set < readback && readback < refuse,
            "the order must be set, read back, then refuse: {set:?} {readback:?} {refuse:?}"
        );
        // EVERY exit after the terminal was patched must put it back. The guard
        // that would normally do it does not exist until `raw` returns `Ok`, and
        // the caller treats `Err` as "carry on without raw mode" — so a bare `?`
        // between the `TCSETS` and the `Ok` hands back a half-raw terminal that
        // nothing owns and nobody will restore.
        let restore = concat!("sys", "::termios_set(fd,&saved)");
        let after_set = rest
            .get(set.unwrap_or_default() + patch.len()..)
            .unwrap_or_default();
        let mut cursor = 0usize;
        let mut exits = 0usize;
        for (at, _) in after_set.match_indices("returnErr(") {
            let segment = after_set.get(cursor..at).unwrap_or_default();
            assert!(
                segment.contains(restore),
                "an early return out of raw() does not restore the terminal first"
            );
            cursor = at;
            exits += 1;
        }
        assert_eq!(
            exits, 3,
            "raw() must refuse three ways — a failed patch, a failed readback, \
             and a readback `readback_refusal` refuses — each restoring the \
             terminal first"
        );
        // ...and `readback_refusal` refuses three ways: the flag word not being
        // EXACTLY the one written (not merely ICANON and ECHO clear, which a
        // zeroed word also has), the control bytes not taking, and any OTHER
        // byte moving.
        assert!(
            after_set.contains("read_u32(got,LFLAG_AT)!=lflag"),
            "the readback must compare the whole local-flag word"
        );
        assert_eq!(
            after_set.matches("returnSome(").count(),
            3,
            "readback_refusal must refuse the flag word, the control bytes and \
             any other byte"
        );
        // A `?` after the patch is an exit that skips all of the above.
        assert!(
            !after_set.contains("?;"),
            "raw() must not use `?` after patching the terminal: it returns without \
             restoring"
        );
        // The patched buffer STARTS as the kernel's own bytes. `only_the_patch_changed`
        // is the runtime guard, but it can only fire against a real terminal, which
        // the gate does not have — so the one line it depends on is pinned here.
        // Built from zeros instead, every other check still passes: a zeroed
        // c_lflag has ICANON and ECHO clear, and VMIN/VTIME are set explicitly.
        // What reaches the console is c_cflag = 0, which is B0 — a hang-up.
        assert!(
            rest.contains("letmutwant=saved;"),
            "raw() must patch the kernel's own termios, never construct one"
        );
        // The readback checks the CONTROL BYTES too, not just the flag word. A
        // TCSETS that applied ICANON/ECHO but not these leaves a terminal that
        // passes the flag check while a command read waits for several keystrokes,
        // or times out and reads EOF — which this pager treats as `q`, so it looks
        // like a pager that quits by itself.
        for slot in ["CC_AT+VMIN", "CC_AT+VTIME"] {
            assert!(
                after_set.contains(slot),
                "the readback must verify {slot}, not only the flag word"
            );
        }
        // The guard BORROWS the descriptor. It issues a syscall from `Drop` on a
        // descriptor it does not own, so a bare `RawFd` would let the terminal be
        // closed — or worse, closed and RECYCLED — before the restore reaches it.
        // With a borrow the compiler refuses that whole family; without one it is
        // a comment about declaration order.
        assert!(
            term.contains(concat!("fd:Borrowed", "Fd<'a>")),
            "Raw must borrow the terminal, not store a bare descriptor"
        );
        assert!(
            term.contains("implDropforRaw<'_>"),
            "the guard must restore the terminal in Drop, not on the happy path only"
        );
        assert!(
            term.contains(concat!(
                "sys",
                "::termios_set(self.fd.as_raw_fd(),&self.saved)"
            )),
            "Drop must write back the bytes the kernel gave us, not a reconstructed termios"
        );
    }
}
