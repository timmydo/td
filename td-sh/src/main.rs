//! td-sh — td's target-built POSIX `/bin/sh`, the busybox-`sh`/`ash` replacement
//! for the shipped image (system-x86-64).
//!
//! This is the SHIPPED binary. The recipe (`recipes/src/recipes/td-sh.rs`)
//! compiles this crate root and its sibling modules directly with rustc into a
//! static ET_EXEC with an empty runtime closure, so `/bin/sh` can run in stage-1
//! init before the dynamic uutils glibc closure is reachable. The conformance
//! harness that scores this binary against the Oils spec corpus lives in the
//! crate's `lib.rs` and is host-side test tooling — it is NOT part of this
//! binary and runs the built shell as a subprocess.
//!
//! The interpreter is safe `std` but for THREE confined syscalls: a virtual
//! file-descriptor table replaces `dup2`, `Stdio::piped`/`CommandExt::exec`
//! cover the process primitives, and subshells clone shell state in-process. The
//! crate `#![deny(unsafe_code)]`s and `sys.rs` carries the single scoped
//! `#[allow]` — `umask(2)`, a DISPOSITION-ONLY `rt_sigaction(2)`, and an
//! `ioctl(2)` restricted to three value-pinned requests, none of which `std`
//! exposes an API for at all. That is the EIGHTH target-side unsafe exception
//! UNSAFE.md records; the confinement tests below assert its roster against this
//! crate's own source.
//!
//! Still deferred, and each a further reviewed amendment: job control, and
//! CATCHING a signal — `trap 'action' SIG` needs a handler, and a handler on
//! x86-64 needs a hand-laid `SA_RESTORER` trampoline to return through, where
//! the two code-free dispositions run no handler at all and so need none.
//! `trap '' SIG` is therefore already real, and reaches the children a script
//! starts, as POSIX requires. Concurrent (streaming) pipelines are NOT among
//! the deferrals — `std::io::pipe` is stable and needs no `unsafe`; today's
//! stages are still run sequentially with the producer buffered, which is a
//! refinement rather than a syscall question.
#![deny(unsafe_code)]

mod arith;
mod ast;
mod builtin;
mod exec;
mod expand;
mod lexer;
mod line;
mod parser;
mod pattern;
mod process;
mod random;
mod sys;
mod term;

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;

use exec::{run_program, Shell};
use process::SIGINT;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let code = match run(&args) {
        Ok(code) => code,
        Err(msg) => {
            let _ = writeln!(std::io::stderr(), "td-sh: {msg}");
            2
        }
    };
    // POSIX exit status is 8 bits.
    ExitCode::from((code & 0xff) as u8)
}

/// Parse the command line and dispatch to the right execution mode.
fn run(args: &[String]) -> Result<i32, String> {
    let mut sh = Shell::new();
    let mut i = 1usize;
    let mut minus_c = false;
    // A LOGIN shell, by the convention every shell since the Bourne one has used:
    // `argv[0]` beginning with `-`. That is not decoration — it is the whole
    // channel `login` has for saying so, and td-login spells it (`login_arg0`),
    // as getty and su do. `-l` asks for the same thing outright.
    let mut login = args.first().is_some_and(|a| a.starts_with('-'));

    // Leading options, parsed as dash's `options(cmdline=1)` does.
    while let Some(arg) = args.get(i) {
        // On the COMMAND LINE both of these only end option processing; the other
        // meanings dash gives them (`-` clearing -x/-v, `--` resetting the
        // parameters) belong to the `set` builtin, so `sh -x - foo.sh` runs the
        // script rather than reading stdin.
        if arg == "-" || arg == "--" {
            i += 1;
            break;
        }
        // dash parses `+x` here exactly as `set` does, so both signs are options
        // and neither is an operand.
        let (on, flags) = match arg.strip_prefix('-') {
            Some(f) => (true, f),
            None => match arg.strip_prefix('+') {
                Some(f) => (false, f),
                None => break,
            },
        };
        let sign = if on { '-' } else { '+' };
        // Extra argv entries this cluster took: EACH `o` in it consumes one name.
        let mut consumed = 0usize;
        for c in flags.chars() {
            match c {
                // dash records that it saw `-c` and takes the command only once
                // the whole option list is done, which is why `sh -c -x 'echo hi'`
                // traces rather than running `-x`.
                'c' => minus_c = true,
                'o' => {
                    // `-o` without a name lists the settings in dash; td-sh has no
                    // listing yet, so it is the same no-op `set -o` is.
                    if let Some(name) = args.get(i + 1 + consumed) {
                        // dash keeps iflag in the same optlist as the rest, so
                        // `-o interactive` does what `-i` does; `-o stdin` needs
                        // no special case, the shared table carries it.
                        if name == "interactive" {
                            sh.interactive = on;
                        } else if !builtin::apply_named_option(&mut sh, name, on) {
                            return Err(format!("Illegal option {sign}o {name}"));
                        }
                        consumed += 1;
                    }
                }
                'i' => sh.interactive = on,
                // Not in the `set` table: this one is only ever a startup
                // question, and `set -l` mid-script would be asking to have
                // already been a login shell. The SIGN carries no meaning --
                // dash ORs it into `login` and busybox ash comments `-l or +l ==
                // --login` outright -- so `+l` asks for a login shell too, and
                // cannot cancel one `argv[0]` already declared.
                'l' => login = true,
                // Every other letter comes from the one table `set` reads, so the
                // command line cannot drift from it.
                other => {
                    if !builtin::apply_option_letter(&mut sh, other, on) {
                        return Err(format!("Illegal option {sign}{other}"));
                    }
                }
            }
        }
        i += 1 + consumed;
    }

    // What this shell was invoked to run. Decided -- and its operands bound --
    // BEFORE the profiles, because a profile is sourced in the shell it is
    // setting up rather than in a shell that precedes it: `$0`, `$@` and `$-`
    // have to already be what the session will see, or a `set --` in one is
    // overwritten a moment later and `case $- in *i*)`, the guard nearly every
    // distributed `/etc/profile` opens with, answers for the wrong shell.
    enum Start {
        Command(String),
        /// The OPEN file and the name it was opened by, for the read's message.
        Script(std::fs::File, String),
        Repl,
        Stdin,
    }
    // A script operand is only an operand when `-s` did not claim stdin.
    let script_operand = if sh.opts.stdin { None } else { args.get(i) };
    let start = if minus_c {
        // `-c COMMAND [name [arg...]]`: the command is the first operand, taken
        // after the whole option list, and the word after it is $0. A MISSING one
        // raises here, before the profiles: a usage error is not a login.
        let cmd = args
            .get(i)
            .ok_or_else(|| "-c requires an argument".to_string())?
            .clone();
        if let Some(name) = args.get(i + 1) {
            sh.arg0 = name.clone();
            sh.params = args.iter().skip(i + 2).cloned().collect();
        }
        Start::Command(cmd)
    } else if let Some(path) = script_operand {
        // `td-sh script [arg...]`. OPENED here, before the profiles, because ash
        // opens it in `procargs` ahead of its login-profile block: a login whose
        // script does not exist fails with `can't open` and reads no profile at
        // all. Checked against busybox ash rather than assumed.
        //
        // Opened, not READ: ash's `setinputfile` takes a descriptor and the
        // reading happens after, so a script that is a fifo or `/dev/stdin`
        // blocks where the writer is waiting rather than before `/etc/profile`
        // has run.
        let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
        sh.arg0 = path.clone();
        sh.params = args.iter().skip(i + 1).cloned().collect();
        Start::Script(file, path.clone())
    } else {
        // No script and no `-c`: interactive when stdin is a terminal, otherwise
        // read and run the whole of stdin as a script.
        sh.params = args.iter().skip(i).cloned().collect();
        // dash sets sflag when no operand is left as well, which is what puts `s`
        // in `$-`; an EXPLICIT `-s` is what suppresses the prompt.
        let explicit_s = sh.opts.stdin;
        sh.opts.stdin = true;
        if !explicit_s && std::io::stdin().is_terminal() {
            sh.interactive = true;
            Start::Repl
        } else {
            Start::Stdin
        }
    };

    // The profiles come BEFORE anything the shell was invoked to do, `-c`
    // included, because that is where a login shell's environment is set up and
    // a command that runs without it runs in a different shell than the operator
    // asked for. An `exit` in one ends the shell THERE, so nothing below runs.
    if login {
        if let Some(code) = read_profiles(&mut sh) {
            return Ok(exec::run_exit_trap(&mut sh, code) & 0xff);
        }
    }

    match start {
        Start::Command(cmd) => Ok(run_program(&mut sh, &cmd)),
        Start::Script(mut file, path) => {
            let mut src = String::new();
            file.read_to_string(&mut src).map_err(|e| format!("{path}: {e}"))?;
            Ok(run_program(&mut sh, &src))
        }
        Start::Repl => {
            let code = repl(&mut sh);
            Ok(exec::run_exit_trap(&mut sh, code) & 0xff)
        }
        Start::Stdin => {
            let mut src = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut src) {
                // Reported and exited HERE rather than through `?`, because this
                // is where the shell environment ends and an EXIT trap a profile
                // installed has to run before it does.
                let _ = exec::write_stderr(&sh, &format!("td-sh: stdin: {e}"));
                return Ok(exec::run_exit_trap(&mut sh, 2) & 0xff);
            }
            Ok(run_program(&mut sh, &src))
        }
    }
}

/// Run the login profiles in THIS shell, so what they export survives into the
/// session: `/etc/profile` first, then the user's own. `Some(code)` means one of
/// them ran an `exit` and the shell is over.
///
/// A missing or unreadable profile is not an error and not a diagnostic — an
/// account with no `.profile` is the ordinary case, and a login that complained
/// about it would put a message on every console every boot. A profile that
/// FAILS mid-way is not fatal either, which is what keeps a typo in one from
/// costing the operator their session -- though its `$?` does carry, as it does
/// under both references.
///
/// `$HOME/.profile` is spelled out, as dash (`main.c`) and busybox ash both
/// spell it -- the bare `.profile` that leans on the caller having chdir'd is
/// original BSD ash, not either shell this one is graded against. td-login does
/// chdir, but a login shell that reads whatever `.profile` happens to sit in the
/// current directory is a different and worse thing when it has not.
fn read_profiles(sh: &mut Shell) -> Option<i32> {
    if let Some(code) = read_profile(sh, "/etc/profile") {
        return Some(code);
    }
    // `HOME` is read only once the system profile has FINISHED, because it is a
    // variable that profile may set and the file to read is whatever it says by
    // then — the ordering every other shell gets for free by sourcing the second
    // file with the first one's assignments already in the environment.
    if let Some(home) = sh.get_var("HOME").filter(|h| !h.is_empty()) {
        if let Some(code) = read_profile(sh, &format!("{}/.profile", home.trim_end_matches('/'))) {
            return Some(code);
        }
    }
    // `$?` is NOT reset on the way out. A profile's last command is the last
    // command, and both references leave it that way (dash's `main.c` and
    // busybox `ash.c` run the profiles and fall straight through to `minusc` or
    // `cmdloop` without touching `exitstatus`). Tidying it would be a divergence
    // for a cosmetic gain at one prompt.
    None
}

/// One profile, SOURCED the way `.` sources a file rather than run as a program
/// of its own. The difference is the whole of what a profile is for: `exit` in
/// one ends the login shell and an EXIT trap it sets fires when the SESSION
/// ends, where a program-shaped run would swallow the first and fire the second
/// immediately.
fn read_profile(sh: &mut Shell, path: &str) -> Option<i32> {
    let src = std::fs::read_to_string(path).ok()?;
    match exec::run_source(sh, &src, &format!("{path}: ")) {
        Ok(()) => None,
        // td's own `/etc/profile` ends its autotest branch with `exit 0`, which
        // is what powers the VM off; a shell that swallowed it would sit at a
        // prompt on ttyS0 until the harness timed the boot out.
        Err(exec::Sig::Exit(code)) => Some(code),
        // A FATAL error -- a syntax error, or `${x:?}` -- ends the login too,
        // unless the shell is interactive. That is ash's rule rather than a
        // choice of ours: its top-level handler exits when `iflag == 0`, so
        // `sh -l -c cmd` over a broken profile never runs `cmd` while
        // `sh -i -l -c cmd` recovers and does. td-login starts an interactive
        // shell, so the case that matters on the image is the recovering one --
        // a typo in `/etc/profile` does not cost the operator their session.
        // Nothing is unwound on the way out: the EXIT trap is owed the dying
        // frame's bindings, which is why `unwind_pending_to` is bounded.
        //
        // An INTERRUPT falls the same way, and deliberately rather than by
        // landing in the catch-all below. The references disagree here: bash
        // abandons the rest of the profile and runs the command anyway, busybox
        // ash dies of the signal and never reaches it. This crate tracks ash,
        // and ash's answer is this same top-level handler -- so a Ctrl-C during
        // `sh -l -c cmd`'s profile ends the login at 130 rather than starting a
        // session the operator interrupted, while `-i` recovers to its prompt.
        Err(exec::Sig::Abort(code) | exec::Sig::Interrupt(code)) if !sh.interactive => Some(code),
        Err(exec::Sig::Abort(code) | exec::Sig::Interrupt(code)) => {
            // Recovering, not exiting, so the two halves a prompt does: the
            // bindings `Sig::Abort` unwound PAST go away -- without this,
            // `f() { local PATH=/bad; : ${x:?}; }; f` in a profile leaves the
            // session holding that `PATH` -- and `$?` is the failure's.
            exec::unwind_pending_to(sh, 0);
            sh.set_status(code);
            None
        }
        // A `return` outside a function is unspecified; ash takes the operand
        // as `$?` and carries on, which is what `return 5` reports there.
        Err(exec::Sig::Return(code)) => {
            sh.set_status(code);
            None
        }
        // A stray break/continue is not worth ending a login over.
        Err(_) => {
            exec::unwind_pending_to(sh, 0);
            None
        }
    }
}

/// A minimal read-eval-print loop: read lines, accumulating while the parse is
/// merely incomplete (an open quote, `if`, here-doc, …), then run the command.
///
/// The prompt is written by the EDITOR rather than here, because a line being
/// edited is redrawn with its prompt on every keystroke — printing it once up
/// front would put it on the screen twice.
fn repl(sh: &mut Shell) -> i32 {
    let mut editor = line::Editor::new();
    loop {
        let mut buffer = String::new();
        let outcome = read_complete(&mut editor, sh, &mut buffer);
        // POSIX's 128 + signal number, the status a shell reports for a command
        // its own SIGINT ended -- even though this one arrived as a keystroke.
        if matches!(outcome, ReadResult::Interrupted) {
            sh.set_status(130);
        }
        let ended = matches!(outcome, ReadResult::Eof);
        if !buffer.trim().is_empty() {
            match parser::parse_aliased(&buffer, &sh.aliases) {
                Ok(list) => {
                    if let Some(code) = exec::run_interactive_unit(sh, &list) {
                        return code;
                    }
                }
                Err(e) => {
                    let _ = writeln!(std::io::stderr(), "td-sh: {e}");
                    sh.set_status(2);
                }
            }
        }
        if ended {
            break;
        }
    }
    sh.status
}

/// What one read at the prompt ended with.
enum ReadResult {
    Ready,
    Eof,
    /// Ctrl-C at the prompt. Distinct from `Ready` with an empty buffer because
    /// it also sets `$?`, which an operator can see with `echo $?`.
    Interrupted,
}

/// Read lines into `buffer` until it parses (or fails with a non-continuation
/// error), or input ends.
fn read_complete(
    editor: &mut line::Editor,
    sh: &mut Shell,
    buffer: &mut String,
) -> ReadResult {
    let mut prompt = sh.get_var("PS1").unwrap_or_else(|| "$ ".to_string());
    loop {
        // `trap '' INT` makes SIGINT do nothing, and the Ctrl-C keystroke only
        // stands in for SIGINT while the editor has signal generation off — so
        // it must do nothing too. BOTH ignores count: the shell's own, and the
        // one it may have been started under, which POSIX says it cannot reset
        // and which is what makes `nohup sh` stay un-interruptible. With `ISIG`
        // cleared the kernel no longer enforces the second, so this is the only
        // thing that does.
        // A NON-EMPTY `trap 'action' INT` is not consulted: the keystroke
        // abandons the line and sets `$?`, and the action does not run. That
        // follows from catching being deferred — the disposition surface can
        // ask for `SIG_DFL` or `SIG_IGN` and nothing else — and it is where
        // td-sh differs from dash, which runs the action.
        let ignored_on_entry = !builtin::may_set_signal(sh, SIGINT);
        let interruptible = !ignored_on_entry
            && !sh.traps.get(&SIGINT).is_some_and(String::is_empty);
        match editor.read(&prompt, interruptible) {
            line::Input::Eof => return ReadResult::Eof,
            // Ctrl-C throws away the WHOLE unit, not just the line it arrived
            // on: half of an unfinished `if` is not something the operator
            // wants to keep typing into, and dash abandons the same way.
            line::Input::Interrupted => {
                buffer.clear();
                return ReadResult::Interrupted;
            }
            line::Input::Line(text) => {
                buffer.push_str(&text);
                buffer.push('\n');
            }
        }
        match parser::parse_aliased(buffer, &sh.aliases) {
            Ok(_) => return ReadResult::Ready,
            Err(e) if e.starts_with(ast::INCOMPLETE) => {
                prompt = sh.get_var("PS2").unwrap_or_else(|| "> ".to_string());
            }
            // A real syntax error: hand the buffer back so the caller reports it.
            Err(_) => return ReadResult::Ready,
        }
    }
}

/// Assertions about this crate's own SOURCE, which the compiler cannot make:
/// the unsafe surface is three syscalls, in one module, with one call site each,
/// writing two handler words and no others, issuing three ioctl requests and no
/// others, and named by three modules and no others.
/// Every needle is built with `concat!` so this module's own text does not
/// count itself.
#[cfg(test)]
mod confinement {
    /// Every file the recipe compiles. `covers_every_module` keeps it honest.
    const SOURCES: &[(&str, &str)] = &[
        ("main.rs", include_str!("main.rs")),
        ("arith.rs", include_str!("arith.rs")),
        ("ast.rs", include_str!("ast.rs")),
        ("builtin.rs", include_str!("builtin.rs")),
        ("exec.rs", include_str!("exec.rs")),
        ("expand.rs", include_str!("expand.rs")),
        ("lexer.rs", include_str!("lexer.rs")),
        ("line.rs", include_str!("line.rs")),
        ("parser.rs", include_str!("parser.rs")),
        ("pattern.rs", include_str!("pattern.rs")),
        ("process.rs", include_str!("process.rs")),
        ("random.rs", include_str!("random.rs")),
        ("sys.rs", include_str!("sys.rs")),
        ("term.rs", include_str!("term.rs")),
    ];

    fn source(name: &str) -> &'static str {
        SOURCES
            .iter()
            .filter(|(n, _)| *n == name)
            .map(|(_, t)| *t)
            .next()
            .unwrap_or("")
    }

    fn squeeze(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    fn count(needle: &str) -> usize {
        SOURCES.iter().map(|(_, t)| t.matches(needle).count()).sum()
    }

    /// The same, ignoring comment lines. Prose that NAMES the lint or an alias
    /// is not a second surface, and these files explain themselves at length.
    fn code_only(text: &str) -> String {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn count_code(needle: &str) -> usize {
        SOURCES
            .iter()
            .map(|(_, t)| code_only(t).matches(needle).count())
            .sum()
    }

    /// A module missing from SOURCES is a module none of the scans below can
    /// see, so an unsafe block there would be invisible to all of them.
    #[test]
    fn covers_every_module() {
        let mut declared: Vec<String> = source("main.rs")
            .lines()
            .map(str::trim)
            .filter_map(|l| {
                // A visibility prefix still declares a module, and `pub mod
                // sneaky;` slipping past this would hide sneaky.rs from every
                // scan below. The brace form (`mod tests {`) has no `;`, so it
                // drops out here as it should.
                let rest = l
                    .strip_prefix("pub(crate) ")
                    .or_else(|| l.strip_prefix("pub "))
                    .unwrap_or(l);
                rest.strip_prefix("mod ").and_then(|r| r.strip_suffix(';'))
            })
            .map(|n| format!("{n}.rs"))
            .collect();
        declared.push("main.rs".to_string());
        declared.sort_unstable();
        let mut listed: Vec<String> = SOURCES.iter().map(|(n, _)| (*n).to_string()).collect();
        listed.sort_unstable();
        assert_eq!(declared, listed, "SOURCES and main.rs's `mod` lines disagree");
    }

    /// Exactly one scoped allow and exactly one unsafe block, both in `sys.rs`.
    ///
    /// Counted on WHITESPACE-STRIPPED code, because `unsafe` and its brace are
    /// two tokens: `unsafe\n{` is the same construct to the compiler and a
    /// different string to a scan, and nothing here enforces rustfmt.
    #[test]
    fn the_unsafe_surface_is_one_allow_and_one_block() {
        let allow = concat!("#[allow(", "unsafe", "_code)]");
        let block = concat!("unsafe", "{");
        let all: String = SOURCES
            .iter()
            .map(|(_, t)| squeeze(&code_only(t)))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(all.matches(allow).count(), 1, "expected one scoped allow");
        assert_eq!(all.matches(block).count(), 1, "expected one unsafe block");
        let sys = squeeze(&code_only(source("sys.rs")));
        assert_eq!(sys.matches(allow).count(), 1);
        assert_eq!(sys.matches(block).count(), 1);
    }

    /// `sys.rs` carries no BLOCK comment, which is what makes the line-based
    /// `code_only` strip complete for the one file that may hold `unsafe`: a
    /// `/* */` between two tokens would otherwise hide a construct from every
    /// scan here without changing what the compiler sees.
    #[test]
    fn the_syscall_module_has_no_block_comments() {
        assert!(
            !source("sys.rs").contains(concat!("/", "*")),
            "sys.rs must use line comments only"
        );
    }

    /// The lint is named twice in the whole crate: denied at the root, allowed
    /// once in `sys.rs`. A third mention is a second surface.
    #[test]
    fn the_unsafe_lint_is_named_exactly_twice() {
        assert_eq!(count_code(concat!("unsafe", "_code")), 2);
        assert_eq!(
            code_only(source("main.rs"))
                .matches(concat!("#![deny(", "unsafe", "_code)]"))
                .count(),
            1,
            "the crate root must still deny the lint"
        );
    }

    /// The roster is THREE syscalls, pinned by VALUE — a name alone would let
    /// the number change under it.
    #[test]
    fn the_ioctl_requests_are_exactly_three_and_value_pinned() {
        // `ioctl(2)` is ONE syscall onto an unbounded space of operations, so
        // the number in `rax` is not the surface -- the request in `rsi` is.
        // The in-code roster refuses anything outside `IOCTL_REQUESTS`, but a
        // wrong VALUE inside it is still a member: `TCSETS` mistyped as 0x5404
        // is `TCSETSF`, which UNSAFE.md excludes on the ground that it DISCARDS
        // terminal input another process may own. Nothing observable at the
        // call site tells those two apart.
        const REQUESTS: &[(&str, &str)] = &[
            ("TCGETS", "0x5401"),
            ("TCSETS", "0x5402"),
            ("TIOCGWINSZ", "0x5413"),
        ];
        // Neighbours deliberately outside the surface, each a digit away from
        // something admitted: the two `TCSETS` variants that drain or discard
        // pending I/O, the winsize SETTER, the input injector, and the
        // controlling-terminal call that is td-init's.
        const REFUSED: &[&str] =
            &["TCSETSW", "TCSETSF", "TIOCSWINSZ", "TIOCSTI", "TIOCSCTTY"];
        // The crate's own tests name these freely, so the scan stops where they
        // begin -- and comments go too, or a `// TIOCSTI` would red it.
        let shipped = source("sys.rs")
            .split(concat!("#[cfg(", "test)]"))
            .next()
            .unwrap_or_default();
        let sys = squeeze(&code_only(shipped));
        for (name, value) in REQUESTS {
            assert_eq!(
                sys.matches(&format!("const{name}:usize={value};")).count(),
                1,
                "{name} must be declared exactly once in sys.rs as {value}"
            );
            // Three mentions and no more: the declaration, the roster the gate
            // checks against, and the ONE wrapper that issues it. A fourth is a
            // place the pinned value could be shadowed or recomputed.
            assert_eq!(
                sys.matches(name).count(),
                3,
                "{name} must be named exactly three times in sys.rs"
            );
        }
        for refused in REFUSED {
            assert_eq!(
                sys.matches(refused).count(),
                0,
                "{refused} is deliberately outside this crate's ioctl surface"
            );
        }
        // ...and no other module may declare a request at all: sys.rs is the
        // only file whose text can reach the kernel.
        for (file, text) in SOURCES {
            if *file == "sys.rs" {
                continue;
            }
            for (name, _) in REQUESTS {
                assert!(
                    !squeeze(&code_only(text)).contains(&format!("const{name}")),
                    "{file} declares an ioctl request; they belong in sys.rs"
                );
            }
        }
    }

    #[test]
    fn the_syscall_roster_is_exactly_three_and_value_pinned() {
        let decl = concat!("const", "SYS", "_");
        let sys = squeeze(source("sys.rs"));
        let mut seen: Vec<String> = Vec::new();
        for (offset, _) in sys.match_indices(decl) {
            let rest = sys.get(offset + decl.len()..).unwrap_or_default();
            let text = rest.split(';').next().unwrap_or_default();
            seen.push(format!("{}{text}", concat!("SYS", "_")));
        }
        assert_eq!(
            seen.len(),
            sys.matches(decl).count(),
            "a syscall-number declaration was not parsed"
        );
        assert_eq!(
            seen,
            vec![
                concat!("SYS", "_RT_SIGACTION:usize=13").to_string(),
                concat!("SYS", "_IOCTL:usize=16").to_string(),
                concat!("SYS", "_UMASK:usize=95").to_string(),
            ]
        );
    }

    /// The `rt_sigaction` surface is DISPOSITION-ONLY, which is what makes it
    /// small enough to take without an `SA_RESTORER` trampoline: the two handler
    /// words are pinned by value, `sys.rs` is the only file that names either,
    /// and each is written in exactly one place — `Disposition`'s two arms.
    /// A THIRD handler word, or one built by arithmetic, is a handler, and a
    /// handler is a different surface.
    #[test]
    fn only_the_two_code_free_handlers_are_ever_installed() {
        // The crate's own tests name both words freely -- they assert what
        // `decode` makes of each -- so the scan stops where they begin.
        let shipped = source("sys.rs")
            .split(concat!("#[cfg(", "test)]"))
            .next()
            .unwrap_or_default();
        let sys = squeeze(&code_only(shipped));
        for decl in [
            concat!("constSIG", "_DFL:usize=0;"),
            concat!("constSIG", "_IGN:usize=1;"),
        ] {
            assert_eq!(sys.matches(decl).count(), 1, "`{decl}` must be pinned by value");
        }
        // Both appear exactly three times in code: the declaration, the decode
        // that reads one back, and the `Disposition` arm that asks for it.
        for name in [concat!("SIG", "_DFL"), concat!("SIG", "_IGN")] {
            assert_eq!(sys.matches(name).count(), 3, "{name} is named somewhere new");
        }
        for (name, text) in SOURCES {
            if *name == "sys.rs" {
                continue;
            }
            let code = code_only(text);
            assert!(
                !code.contains(concat!("SIG", "_DFL")) && !code.contains(concat!("SIG", "_IGN")),
                "{name} names a raw handler word"
            );
        }
        // The struct is four words and the handler is the FIRST, written out
        // rather than indexed so the order is visible at the one write.
        assert!(
            sys.contains(concat!("letact:[usize;SIGACTION", "_WORDS]=[handler,0,0,0];")),
            "the installed action is no longer `handler` followed by three zeros"
        );
    }

    /// The assembly body is pinned WHOLE, including which register each argument
    /// lands in and that `options(nomem)` stays absent — which is no longer
    /// merely a promise kept for a future pointer argument: `rt_sigaction` has
    /// the kernel write the previous action through `a3` and read the new one
    /// through `a2`, and both addresses escape only as integers.
    #[test]
    fn the_confined_block_is_pinned_whole() {
        let sys = squeeze(source("sys.rs"));
        let body = squeeze(concat!(
            "core::arch::", "asm", "!(\n",
            "    \"syscall\",\n",
            "    inlateout(\"rax\") n as isize => ret,\n",
            "    in(\"rdi\") a1,\n",
            "    in(\"rsi\") a2,\n",
            "    in(\"rdx\") a3,\n",
            "    in(\"r10\") a4,\n",
            "    out(\"rcx\") _,\n",
            "    out(\"r11\") _,\n",
            "    options(nostack),\n",
            ");"
        ));
        assert!(sys.contains(&body), "the confined assembly body changed");
        assert_eq!(count(concat!("arch::", "asm", "!")), 1, "one asm site only");
    }

    /// THREE modules reach the wrappers and no others: `builtin.rs` for the
    /// `umask` and `trap` builtins, `process.rs` for the guards that give a
    /// subshell back the process state a fork would have kept for it and for
    /// the one that stops the shell listening to the terminal while a
    /// foreground child runs, and
    /// `term.rs` for the terminal mode and width the line editor needs — and
    /// `term.rs` is the ONLY module that knows what a `termios` byte means, so
    /// the layout lives beside the readback that checks it. Nothing renames the
    /// module — an alias would give the audited calls a name no scan looks for.
    #[test]
    fn only_the_named_callers_reach_the_syscall_module() {
        // Matched on the bare `sys::` rather than the fully-qualified spelling:
        // `use crate::sys;` followed by `sys::set(…)` names the same function
        // and would slip past a `crate::sys::` scan.
        let call = concat!("sys", "::");
        for (name, text) in SOURCES {
            let uses = code_only(text).matches(call).count();
            match *name {
                "builtin.rs" | "process.rs" | "term.rs" => {
                    assert!(uses > 0, "{name} stopped calling")
                }
                _ => assert_eq!(uses, 0, "{name} reaches the syscall module"),
            }
        }
        // Importing an ITEM out of the module is a way to reach it under a name
        // the scan above does not look for: `use crate::sys::set;` leaves only a
        // bare `set(…)` behind. Importing the MODULE (`use crate::sys;`) hides
        // nothing -- every call still reads `sys::…`, which is what is counted
        // above -- so that one form is allowed, and it is the only one, which is
        // the same rule td-util states for its own syscall module.
        let item_import = concat!("use crate::", "sys", "::");
        let alias_a = concat!("sys", " as ");
        let alias_b = concat!(" as ", "sys");
        for (name, text) in SOURCES {
            let code = code_only(text);
            assert!(
                !code.contains(item_import),
                "{name} imports an item out of the syscall module"
            );
            assert!(
                !code.contains(alias_a) && !code.contains(alias_b),
                "{name} aliases the syscall module"
            );
            // ... and the only import that may name it is the module itself.
            // Checked over WHOLE STATEMENTS rather than lines, because both
            // `use crate::{sys::set};` and a `use` broken across two lines put
            // the module's name in an import that no line-based scan matches.
            let flat = code.split_whitespace().collect::<Vec<_>>().join(" ");
            for stmt in flat.split(';') {
                let head =
                    stmt.trim_start_matches(|c: char| c.is_whitespace() || c == '{' || c == '}');
                if !head.starts_with("use ") || !head.contains(concat!("s", "ys")) {
                    continue;
                }
                assert_eq!(
                    head.trim(),
                    concat!("use crate::", "sys"),
                    "{name}: unexpected import `{head}`"
                );
            }
        }
    }

    /// The raw entry point has ONE call site PER SYSCALL. Module privacy stops
    /// another MODULE reaching `syscall4`, but not another wrapper inside
    /// `sys.rs`, and a second wrapper is a second syscall however safe its
    /// signature looks.
    #[test]
    fn the_raw_syscall_has_one_call_site_per_syscall() {
        let code = code_only(source("sys.rs"));
        let name = concat!("syscall", "4");
        assert_eq!(
            code.matches(name).count(),
            4,
            "`{name}` should appear four times in code: its definition and its three calls"
        );
        assert_eq!(
            code.matches(concat!("fn ", "syscall", "4(")).count(),
            1,
            "more than one raw entry point"
        );
        // The number reaches the kernel as an ARGUMENT, so pinning the `SYS_*`
        // declarations is not enough on its own: `syscall4(90, x, 0, 0, 0)`
        // names no constant and would satisfy the roster test above.
        let squeezed = squeeze(&code);
        for number in [
            concat!("SYS", "_UMASK,"),
            concat!("SYS", "_RT_SIGACTION,"),
            concat!("SYS", "_IOCTL,"),
        ] {
            assert_eq!(
                squeezed.matches(&format!("{}{number}", concat!("syscall", "4("))).count(),
                1,
                "each call must pass the named syscall number"
            );
        }
    }
}
