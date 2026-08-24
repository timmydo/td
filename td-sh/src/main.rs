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
//! The interpreter is safe `std` but for FOUR confined syscalls: a virtual
//! file-descriptor table replaces `dup2`, `Stdio::piped`/`CommandExt::exec`
//! cover the process primitives, and subshells clone shell state in-process. The
//! crate `#![deny(unsafe_code)]`s and `sys.rs` carries the single scoped
//! `#[allow]` — `umask(2)`, a DISPOSITION-ONLY `rt_sigaction(2)`, an
//! `ioctl(2)` restricted to three value-pinned requests, and `poll(2)` asking
//! about ONE descriptor, none of which `std` exposes an API for at all. That
//! is the EIGHTH target-side unsafe exception UNSAFE.md records; the
//! confinement tests below assert its roster against this crate's own source.
//!
//! Still deferred, and each a further reviewed amendment: the half of job
//! control that needs a job to be a PROCESS (see below), and
//! CATCHING a signal — `trap 'action' SIG` needs a handler, and a handler on
//! x86-64 needs a hand-laid `SA_RESTORER` trampoline to return through, where
//! the two code-free dispositions run no handler at all and so need none.
//! `trap '' SIG` is therefore already real, and reaches the children a script
//! starts, as POSIX requires. Pipeline stages now STREAM, on threads joined by
//! `std::io::pipe` descriptors — no `unsafe`, which is why that was a
//! refinement rather than a syscall question. Async lists (`&`) now stream too,
//! on threads of the same shape, with a real `$!` and a `wait` that answers for
//! them (`jobs.rs`), plus `jobs` and the `%N` jobspecs that name one. What is
//! still deferred is the part that needs a job to be a PROCESS: `fg`/`bg`, and
//! the process group a signal would be aimed at.
#![deny(unsafe_code)]

mod arith;
mod ast;
mod builtin;
mod complete;
mod exec;
mod expand;
mod funcs;
mod jobs;
mod lexer;
mod line;
mod parser;
mod pattern;
mod process;
mod random;
mod regex;
mod sys;
mod term;

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;

use exec::{run_program, Shell};
use process::SIGINT;

fn main() -> ExitCode {
    // Before anything else, and before any pipeline stage can exist: reading the
    // mask means briefly clearing it, so the one place that is safe to do is
    // here, while this thread is the only one.
    process::prime_umask();
    let args: Vec<String> = std::env::args().collect();
    // On a stack this crate chose rather than the one the process was launched
    // with, so `ulimit -s` cannot put the depth guards above where the stack
    // ends. Spawned AFTER the umask priming above, which needs to be alone.
    //
    // A thread the OS REFUSES falls back to the process's own stack rather than
    // ending the shell: that is what every version before this ran on, and a
    // guard that may sit above the end of a small stack beats a `/bin/sh` that
    // exits before reading the script.
    let started = process::on_shell_stack(|| run(&args)).unwrap_or_else(|_| run(&args));
    let code = match started {
        Ok(code) => code,
        Err(msg) => {
            // Almost everything here is raised before `run` built a shell, so
            // the name is the argv[0] it would have taken `$0` from
            // (ash.c:14589). A script that opens and then fails to READ is the
            // exception -- `$0` is the script by then -- and it stays argv[0]
            // because that message already names the file.
            let name = args.first().map_or("td-sh", String::as_str);
            let _ = writeln!(std::io::stderr(), "{name}: {msg}");
            2
        }
    };
    // POSIX exit status is 8 bits.
    ExitCode::from((code & 0xff) as u8)
}

/// Parse the command line and dispatch to the right execution mode.
fn run(args: &[String]) -> Result<i32, String> {
    let mut sh = Shell::new();
    // Settle "was this signal ignored when we started?" while this is still the
    // only thread — a stage asking for the first time during a SIBLING's guard,
    // or during the window a sibling's spawn opens, would read that ignore as
    // the answer and keep it. Every clone inherits what this records.
    builtin::prime_signal_entries(&mut sh);
    // `$0` is the name this shell was INVOKED by (`arg0 = xargv[0]`,
    // ash.c:14589); a script operand or `-c NAME` replaces it below.
    if let Some(a) = args.first() {
        sh.arg0 = a.clone();
    }
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
                            // Not a typo: `ash_msg` sets no status and `procargs`
                            // unwinds on the non-zero return (ash.c:14595).
                            let _ = exec::diag(
                                &sh,
                                &format!("illegal option {sign}o {name}"),
                            );
                            return Ok(0);
                        }
                        consumed += 1;
                    }
                }
                // The SIGN carries no meaning, as it does not for `l` below:
                // ash spells both in one arm, `/* -i, +i */` (ash.c:11493), the
                // same way it treats `-s`/`+s` and `-c`/`+c`. So `+i` ASKS for
                // an interactive shell rather than declining one, and there is
                // no third state to record -- `iflag == 2` is reachable only
                // where `i` went unmentioned.
                'i' => sh.interactive = true,
                // Not in the `set` table: this one is only ever a startup
                // question, and `set -l` mid-script would be asking to have
                // already been a login shell. The SIGN carries no meaning --
                // dash ORs it into `login` and busybox ash comments `-l or +l ==
                // --login` outright -- so `+l` asks for a login shell too, and
                // cannot cancel one `argv[0]` already declared.
                'l' => login = true,
                // Every other letter comes from the one table `set` reads, so the
                // command line cannot drift from it -- including how it is
                // refused: `setoption` raises, so this one is fatal at 2.
                other => {
                    if !builtin::apply_option_letter(&mut sh, other, on) {
                        return Err(format!("illegal option {sign}{other}"));
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
            // ash reaches the same `commandname = arg0` a script does, by
            // jumping INTO that branch (`goto setarg0`, ash.c:14626) once `-c`
            // has a name operand. So `-c CMD NAME` reports a line where bare
            // `-c` reports none -- the name is what makes the difference.
            sh.commandname = Some(name.clone());
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
        let file = std::fs::File::open(path)
            .map_err(|e| format!("can't open '{path}': {}", exec::strerror(&e)))?;
        sh.arg0 = path.clone();
        sh.params = args.iter().skip(i + 1).cloned().collect();
        Start::Script(file, path.clone())
    } else {
        // No script and no `-c`: interactive when asked, or when stdin is a
        // terminal; otherwise read and run the whole of stdin as a script.
        sh.params = args.iter().skip(i).cloned().collect();
        // dash sets sflag when no operand is left as well, which is what puts `s`
        // in `$-`; an EXPLICIT `-s` is what suppresses the prompt.
        let explicit_s = sh.opts.stdin;
        sh.opts.stdin = true;
        // `i` in any form decides on its own; the terminal is asked only where
        // it went unmentioned, which is ash's third `iflag` state (`iflag == 2`,
        // ash.c:14606). That clause alone is what this takes: ash's own guard
        // also wants `isatty(1)` and an `sflag` that an explicit `-s` SETS, and
        // this shell diverges on both -- older than this and not adopted here.
        if sh.interactive || (!explicit_s && std::io::stdin().is_terminal()) {
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
            file.read_to_string(&mut src)
                .map_err(|e| format!("{path}: {}", exec::strerror(&e)))?;
            // ash.c:14630 sets `commandname = arg0` for a script named on the
            // command line and nowhere else, which is why a script reports the
            // line of a failure and `-c` does not. Equal to `$0`, so the
            // component itself is dropped and only the line survives.
            sh.commandname = Some(sh.arg0.clone());
            sh.input_is_file = true;
            Ok(run_program(&mut sh, &src))
        }
        Start::Repl => {
            let code = repl(&mut sh);
            Ok(exec::run_exit_trap(&mut sh, code) & 0xff)
        }
        Start::Stdin => {
            let code = stdin_script(&mut sh);
            Ok(exec::run_exit_trap(&mut sh, code) & 0xff)
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
    // A profile is read from a real file, which is ash's `pf_fd > 0`: an
    // INTERACTIVE login still reports the line of a failure inside one, where
    // the same failure typed at its prompt reports none.
    //
    // The NAME is a deliberate divergence rather than an oversight. ash's
    // `read_profile` sets no `commandname`, so a broken profile there is a bare
    // `ash: syntax error: …` naming no file; this shell names it, as `.` names
    // a sourced file, because a login that fails on a file the operator cannot
    // identify is one they cannot repair. A builtin inside the profile still
    // overwrites this with its own name, so those diagnostics stay ash's.
    let was_file = std::mem::replace(&mut sh.input_is_file, true);
    let was_name = sh.commandname.replace(path.to_string());
    let ran = exec::run_source(sh, &src);
    sh.commandname = was_name;
    sh.input_is_file = was_file;
    match ran {
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

/// Where `stdin_script`'s parser gets its next line. A free function because
/// `Units` holds a plain `fn` pointer rather than a closure.
fn next_stdin_line() -> parser::More {
    match line::read_script_line() {
        // Verbatim, terminator included: whether the last line ended in a
        // newline is what decides whether a trailing `\` continues it.
        line::ScriptLine::Line(text) => parser::More::Line(text),
        line::ScriptLine::Eof => parser::More::Eof,
        line::ScriptLine::Failed(e) => parser::More::Failed(e),
    }
}

/// Run a script arriving on stdin, reading only as far as it has run.
///
/// A shell shares its stdin with the commands in it, so how much of that
/// descriptor the SCRIPT consumes is observable: `printf 'read v\nDATA\n' | sh`
/// gives `read` the line after it. This used to slurp stdin whole, so `read`
/// found end of input and `DATA` was then run as a command -- the opposite of
/// bash, and of POSIX, which requires a shell not to consume more of a
/// non-seekable input than the commands it has executed need.
///
/// The parser PULLS lines as the grammar needs them, so a unit spanning many
/// of them is parsed once rather than re-parsed per line. No editor, no
/// prompts, and no recovery: a syntax error or an abort ends a non-interactive
/// shell where it would return an interactive one to `PS1`. Commands BEFORE
/// the error have run by then, which is the other half of reading
/// incrementally and is what bash does too.
fn stdin_script(sh: &mut Shell) -> i32 {
    // The parser PULLS lines as the grammar needs them, so a unit spanning many
    // lines is lexed and parsed once rather than from the top per line, and the
    // shell consumes no more of a shared stdin than the commands it has run.
    let mut units = parser::Units::streaming(next_stdin_line);
    loop {
        let unit = units.next_unit(&sh.aliases);
        // A script whose input FAILED did not end, it broke. Checked before the
        // parse outcome because sealing the source on failure also makes the
        // half-read unit a syntax error, and the read is the truer report.
        if let Some(e) = units.source_error() {
            let _ = exec::diag(sh, &format!("stdin: {e}"));
            sh.set_status(2);
            return 2;
        }
        let Some(outcome) = unit else { return sh.status };
        match outcome {
            Ok(list) => match exec::run_list(sh, &list) {
                Ok(()) => {}
                Err(exec::Sig::Exit(code) | exec::Sig::Abort(code) | exec::Sig::Interrupt(code)) => {
                    return code
                }
                // Anything else -- a top-level `return`, which `run_source`
                // propagated with `?` -- ends the script at `$?`, as the
                // whole-of-stdin path did. `break`/`continue` never reach here;
                // `run_list` catches those lower down.
                Err(_) => return sh.status,
            },
            Err(e) => {
                // Through the shell's own fd 2, as `run_source` reported a parse
                // error before this, so a script that redirected it still sees
                // the message where it sent everything else.
                let _ = exec::diag(sh, &e.msg);
                sh.set_status(2);
                return 2;
            }
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
    // Only an interactive session persists history, and only from here: a
    // script's lines are not the operator's, and `sh -c` has none to keep.
    // A TYPED one at that: ash's history lives in its line editor, which a
    // non-terminal stdin never reaches, so `sh -i < script` must not create and
    // grow a file for a session nobody sat at.
    if std::io::stdin().is_terminal() {
        editor.open_history(sh);
    }
    // The session's line count. dash reads an interactive shell's input as one
    // stream, so `$LINENO` runs for the life of the session rather than
    // restarting at each prompt -- measured over a pty at 1, 2, 5, 7 for two
    // one-line commands, a four-line `if`, and one more command.
    let mut line_base: u32 = 1;
    loop {
        let mut buffer = String::new();
        let outcome = read_complete(&mut editor, sh, &mut buffer);
        // Counted before the run, and outside the emptiness test below: a blank
        // line at the prompt is a line the session read.
        let typed = u32::try_from(buffer.matches('\n').count()).unwrap_or(0);
        // POSIX's 128 + signal number, the status a shell reports for a command
        // its own SIGINT ended -- even though this one arrived as a keystroke.
        if matches!(outcome, ReadResult::Interrupted) {
            sh.set_status(130);
        }
        let ended = matches!(outcome, ReadResult::Eof);
        if !buffer.trim().is_empty() {
            match parser::parse_aliased_at(&buffer, &sh.aliases, line_base) {
                Ok(list) => {
                    if let Some(code) = exec::run_interactive_unit(sh, &list) {
                        return code;
                    }
                }
                Err(e) => {
                    let _ = exec::diag(sh, &e.msg);
                    sh.set_status(2);
                }
            }
        }
        line_base = line_base.saturating_add(typed);
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
    // Expanded per call rather than once, because `\w` is a fact about where
    // the shell is NOW and the command just run may have been a `cd`. The cwd
    // is the shell's LOGICAL one -- what `$PWD` and `pwd` report, and what
    // bash's `\w` follows -- and not the process's, which `cd` never moves.
    let home = sh.get_var("HOME");
    let user = sh.get_var("USER").or_else(|| sh.get_var("LOGNAME"));
    let env = line::PromptEnv {
        home: home.as_deref(),
        user: user.as_deref(),
        cwd: &sh.logical_cwd.clone(),
    };
    let mut prompt =
        line::expand_prompt(&sh.get_var("PS1").unwrap_or_else(|| r"\$ ".to_string()), &env);
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
        // Built per line rather than once: `commands` reads `PATH` and the
        // filesystem, and the command just run may have changed either.
        let cmds = |p: &str| complete::commands(sh, p);
        let ents = |d: &str, p: &str| complete::entries(sh, d, p);
        let src = complete::Source { commands: &cmds, entries: &ents };
        match editor.read(&prompt, interruptible, &src) {
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
        match parser::parse_probe(buffer, &sh.aliases) {
            Ok(_) => return ReadResult::Ready,
            Err(e) if e.is_incomplete() => {
                prompt =
                    line::expand_prompt(&sh.get_var("PS2").unwrap_or_else(|| "> ".to_string()), &env);
            }
            // A real syntax error: hand the buffer back so the caller reports it.
            Err(_) => return ReadResult::Ready,
        }
    }
}

/// Assertions about this crate's own SOURCE, which the compiler cannot make:
/// the unsafe surface is four syscalls, in one module, with one call site each,
/// writing two handler words and no others, issuing three ioctl requests and no
/// others, asking about one descriptor and one event, and named by three
/// modules and no others.
/// Needles that can reach this module's own text are built with `concat!` so
/// the module does not count itself, with two exceptions: the ioctl-request
/// needle asserts ABSENCE, where a self-match could only red, and the
/// declaration pin is a plain literal that scans `funcs.rs` alone.
#[cfg(test)]
mod confinement {
    /// Every file the recipe compiles. `covers_every_module` keeps it honest.
    const SOURCES: &[(&str, &str)] = &[
        ("main.rs", include_str!("main.rs")),
        ("arith.rs", include_str!("arith.rs")),
        ("ast.rs", include_str!("ast.rs")),
        ("builtin.rs", include_str!("builtin.rs")),
        ("complete.rs", include_str!("complete.rs")),
        ("exec.rs", include_str!("exec.rs")),
        ("expand.rs", include_str!("expand.rs")),
        ("funcs.rs", include_str!("funcs.rs")),
        ("jobs.rs", include_str!("jobs.rs")),
        ("lexer.rs", include_str!("lexer.rs")),
        ("line.rs", include_str!("line.rs")),
        ("parser.rs", include_str!("parser.rs")),
        ("pattern.rs", include_str!("pattern.rs")),
        ("process.rs", include_str!("process.rs")),
        ("random.rs", include_str!("random.rs")),
        ("regex.rs", include_str!("regex.rs")),
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

    /// The REPL must open the session's history. Everything about `HISTFILE`
    /// is tested against `Editor` directly, so deleting the one line that
    /// reaches it leaves every one of those tests green and the feature gone
    /// -- there is no unit test of `repl`, which wants a terminal.
    ///
    /// Over `code_only`, because the first version of this test passed against
    /// a build with the call COMMENTED OUT: the needle matched the comment.
    #[test]
    fn the_repl_opens_the_history() {
        let src = code_only(source("main.rs"));
        let repl = src
            .split_once(concat!("fn ", "repl(sh: &mut Shell)"))
            .map_or("", |(_, rest)| rest)
            .to_string();
        assert!(
            squeeze(&repl).contains(concat!("editor.", "open_history(sh)")),
            "`repl` no longer opens the history file"
        );
    }

    /// The reader at the prompt must probe with `parse_probe` and nothing else:
    /// it is the one caller that can be asked for another line, and swapping it
    /// back to the parse that RUNS the text would make a trailing `\<newline>`
    /// look finished and run half a typed line. `parse_probe` is tested
    /// directly, so that swap leaves every test green -- `repl` wants a
    /// terminal, exactly as the history call above does. Over `code_only` for
    /// the same reason that one is: a needle can match a comment.
    #[test]
    fn the_reader_at_the_prompt_probes_rather_than_parses() {
        let src = code_only(source("main.rs"));
        let read = src
            .split_once(concat!("fn ", "read_complete("))
            .map_or("", |(_, rest)| rest)
            .to_string();
        assert!(
            squeeze(&read).contains(concat!("parser::", "parse_probe(buffer")),
            "the prompt's reader no longer probes"
        );
        // And the mark that makes the probe one is set in a single place, which
        // is what `Scan::resumable`'s own doc claims and nothing else enforces.
        // Not a count of `parse_probe` CALLS beside it: tests call it too, and
        // a number that moves whenever one is added pins nothing.
        assert_eq!(count_code(concat!("scan.", "resumable()")), 1);
    }

    fn count(needle: &str) -> usize {
        SOURCES.iter().map(|(_, t)| t.matches(needle).count()).sum()
    }

    /// The same, ignoring comments. Prose that NAMES the lint or an alias is
    /// not a second surface, and these files explain themselves at length.
    ///
    /// A BLOCK comment counts as prose too, which the line filter alone could
    /// not manage: `/* pub table */` between two tokens carries a
    /// declaration's text, so widening the declaration loses a match and the
    /// decoy pays it straight back -- a green tree over a broken rule.
    fn code_only(text: &str) -> String {
        uncommented(text)
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Comments out, literals through untouched.
    ///
    /// Literal-aware because it has to be: this crate writes shell globs as
    /// DATA -- `split("dir/*")` in `expand.rs`, `pat("*/")` in `pattern.rs` --
    /// so a scan that could not tell a literal from a comment would swallow
    /// every line between one file's `/*` and the next file's `*/`. That is
    /// the same failure as the decoy, arriving from the other side: text
    /// removed is a match lost, and a lost match reds a count that should
    /// hold. Newlines inside a comment are KEPT so line-shaped needles below
    /// cannot straddle what was removed.
    fn uncommented(text: &str) -> String {
        let src: Vec<char> = text.chars().collect();
        let at = |k: usize| src.get(k).copied();
        let word = |k: usize| at(k).is_some_and(|c| c.is_alphanumeric() || c == '_');
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while let Some(c) = at(i) {
            // A raw string ends at a quote followed by its OWN number of `#`.
            // Only a LEADING `r` starts one: not the `r` ending a name, and
            // not the `r` NAMING a lifetime -- `'r"x"` is a lifetime and a
            // string, and reading it as a raw string swallowed whatever came
            // after. `b` and `c` prefix one too.
            let opener = !word(i.wrapping_sub(1)) && at(i.wrapping_sub(1)) != Some('\'');
            let raw = match c {
                'r' if opener => Some(i + 1),
                'b' | 'c' if opener && at(i + 1) == Some('r') => Some(i + 2),
                _ => None,
            };
            if let Some(mut h) = raw {
                let opens = h;
                while at(h) == Some('#') {
                    h += 1;
                }
                if at(h) == Some('"') {
                    let hashes = h - opens;
                    for k in i..=h {
                        if let Some(ch) = at(k) {
                            out.push(ch);
                        }
                    }
                    i = h + 1;
                    while let Some(ch) = at(i) {
                        out.push(ch);
                        i += 1;
                        if ch == '"' && (0..hashes).all(|k| at(i + k) == Some('#')) {
                            for k in 0..hashes {
                                if let Some(hc) = at(i + k) {
                                    out.push(hc);
                                }
                            }
                            i += hashes;
                            break;
                        }
                    }
                    continue;
                }
            }
            match c {
                '/' if at(i + 1) == Some('/') => {
                    while at(i).is_some_and(|n| n != '\n') {
                        i += 1;
                    }
                }
                '/' if at(i + 1) == Some('*') => {
                    // A comment SEPARATES tokens, so one leaves a space
                    // behind: `Fun/**/cs` is two tokens and joining them
                    // would synthesise an identifier the source never had.
                    out.push(' ');
                    // Rust's block comments NEST, so a depth and not a search
                    // for the first `*/`.
                    let mut depth = 1usize;
                    i += 2;
                    while depth > 0 && i < src.len() {
                        if at(i) == Some('/') && at(i + 1) == Some('*') {
                            depth += 1;
                            i += 2;
                        } else if at(i) == Some('*') && at(i + 1) == Some('/') {
                            depth -= 1;
                            i += 2;
                        } else {
                            if at(i) == Some('\n') {
                                out.push('\n');
                            }
                            i += 1;
                        }
                    }
                }
                '"' => {
                    out.push(c);
                    i += 1;
                    while let Some(ch) = at(i) {
                        out.push(ch);
                        i += 1;
                        if ch == '\\' {
                            if let Some(esc) = at(i) {
                                out.push(esc);
                                i += 1;
                            }
                        } else if ch == '"' {
                            break;
                        }
                    }
                }
                // `'a'` is a literal and `'static` is a lifetime. The escape
                // decides the first, a quote two characters along the second.
                '\'' if at(i + 1) == Some('\\') || at(i + 2) == Some('\'') => {
                    out.push(c);
                    i += 1;
                    while let Some(ch) = at(i) {
                        out.push(ch);
                        i += 1;
                        if ch == '\\' {
                            if let Some(esc) = at(i) {
                                out.push(esc);
                                i += 1;
                            }
                        } else if ch == '\'' {
                            break;
                        }
                    }
                }
                _ => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        out
    }

    /// What `code_only` must and must not remove. A comment that carries a
    /// declaration's text would otherwise PAY for a match that widening the
    /// declaration lost; a literal that carries the same characters must
    /// survive, because this crate writes shell globs as DATA and removing
    /// the code between two of them reds a count that should hold.
    #[test]
    fn comments_go_and_literals_stay() {
        let open = concat!("/", "*");
        let shut = concat!("*", "/");
        // The decoy: a block comment carrying a declaration.
        assert_eq!(code_only(&format!("let a = 1;\n{open} struct Funcs {shut}\n")), "let a = 1;\n");
        // Nested, as Rust allows.
        assert_eq!(code_only(&format!("a{open} b {open} c {shut} d {shut}e")), "a e");
        // A line comment, as before.
        assert_eq!(code_only("keep // drop\n"), "keep");
        // An opener INSIDE a line comment opens nothing.
        assert_eq!(code_only(&format!("// {open}\nkept\n")), "\nkept");
        // Literals survive whole -- both shapes are already in this crate.
        let glob = format!("split(\"dir{open}\");");
        assert_eq!(code_only(&glob), glob);
        let pat = format!("pat(\"{shut}\");");
        assert_eq!(code_only(&pat), pat);
        // And the code BETWEEN two of them is not swallowed.
        let pair = format!("{glob}\nkeepme\n{pat}");
        assert!(code_only(&pair).contains("keepme"), "code between two literals was swallowed");
        // A raw string keeps its hashes and its contents.
        let raw = "let r = r#\"a\"b\"#;";
        assert_eq!(code_only(raw), raw);
        // A lifetime is not a character literal.
        let life = "fn f<'a>(x: &'a str) -> &'a str { x }";
        assert_eq!(code_only(life), life);
        // A character literal is one.
        let ch = "let c = '\\'';";
        assert_eq!(code_only(ch), ch);
        // A C raw string is a literal too, and its `#` count ends it -- read
        // as anything else, the opener inside it starts a comment that eats
        // the rest of the file.
        let craw = format!("let s = cr#\"x\"{open}\"#;");
        assert_eq!(code_only(&craw), craw);
        // A LIFETIME called `r` in front of a string is not a raw string.
        let life_r = format!("m!('r \"x\" {open} gone {shut})");
        assert_eq!(code_only(&life_r), "m!('r \"x\"  )");
        // A comment separates tokens, so removing one leaves them separate.
        assert_eq!(code_only(&format!("Fun{open}{shut}cs")), "Fun cs");
        // And a comment spanning LINES keeps them apart: without the newlines
        // inside it, `foo` and `bar` below join into one line and a
        // line-shaped needle straddles what was removed.
        assert_eq!(code_only(&format!("foo\n{open} c\n{shut} bar")), "foo\n\nbar");
        // The forms below are the ones that DIVERGE. Review found the first
        // draft of each passing with the arm it was meant to pin removed: a
        // space after `'r`, a `'\''` for the character arm, and a `br` body
        // with no quote in it all read the same either way.
        //
        // A lifetime named `r` ABUTTING a string. Read as a raw string it
        // ends at the escaped quote and the scan desyncs from there.
        let life = format!("m!('r\"a\\\"b\") {open} gone {shut} after");
        let out = code_only(&life);
        assert!(!out.contains("gone"), "comment survived a lifetime: {out}");
        assert!(out.contains("after"), "code after it was eaten: {out}");
        // A character literal holding a QUOTE. Without the character arm it
        // opens a string and the comment after it reads as code -- the crate
        // has 31 of these, most of them in `lexer.rs`.
        let quote = format!("let q = '\"'; {open} gone {shut} after");
        let out = code_only(&quote);
        assert!(!out.contains("gone"), "comment survived a char literal: {out}");
        assert!(out.contains("after"), "code after it was eaten: {out}");
        // The `br` prefix, twin of the `cr` case above. The body needs a
        // quote in it or both readings consume the same span.
        let braw = format!("let s = br#\"x\"{open}\"#; after");
        let out = code_only(&braw);
        assert!(out.contains("after"), "a byte raw string was mis-ended: {out}");
        assert_eq!(out, braw, "a byte raw string is a literal, kept whole");
    }

    fn count_code(needle: &str) -> usize {
        SOURCES
            .iter()
            .map(|(_, t)| code_only(t).matches(needle).count())
            .sum()
    }

    /// `write_stderr` is the one write that skips the `$0` every diagnostic
    /// carries, so its CALLERS are the whole of the exception: `diag`, which
    /// composes ash's whole `ash_vmsg` prefix; `diag_applet`, which composes
    /// the `$0` alone for the libbb messages that reach no line; and
    /// `err_raw`, which is the four messages busybox writes with a bare
    /// `fprintf` (ash.c:3564, 3588, 11714, 11736). A FOURTH caller would drop
    /// the prefix SILENTLY -- those four are worded identically either way, so
    /// nothing but a comparison against ash sees it -- which is the failure
    /// one sink exists to prevent and no compiler checks. Four mentions in
    /// all: the definition and those three.
    #[test]
    fn the_diagnostic_sink_has_exactly_three_callers() {
        let needle = concat!("write_", "stderr(");
        assert_eq!(count_code(needle), 4, "a new caller of {needle} would bypass `$0`");
        assert_eq!(count_code(concat!("pub fn write_", "stderr(")), 1);
        // Named whole, so a fourth caller cannot arrive by REPLACING one. The
        // full sink builds its prefix over several lines now, so what is
        // pinned there is the call plus the buffer it is handed.
        let diag = concat!("write_", "stderr(sh, &out)");
        assert_eq!(code_only(source("exec.rs")).matches(diag).count(), 1);
        // The applet sink is the shape `diag` had before the line arrived, and
        // is pinned whole for the same reason: the two differ only in what
        // they compose, so a message reaching the wrong one is invisible here
        // and visible only against ash.
        let applet = concat!("write_", "stderr(sh, &format!(\"{}: {msg}\", sh.arg0))");
        assert_eq!(code_only(source("exec.rs")).matches(applet).count(), 1);
        let raw = concat!("exec::write_", "stderr(sh, msg)");
        assert_eq!(code_only(source("builtin.rs")).matches(raw).count(), 1);
    }

    /// What privacy cannot state about itself, in two parts. Both guard the
    /// accidental edit, and neither is sound against an author working
    /// around it. The compiler guards the `&Func` path against every other
    /// module; nothing guards it inside `funcs.rs`. Two gaps stay measured
    /// and open: the in-module one and the `complete.rs` forwarder. The
    /// third, a decoy comment against the declaration pin, is closed --
    /// `code_only` removes comments now.
    ///
    /// The map's DECLARATION. Any visibility on it puts every module back in
    /// a position to answer a lookup without the `/` rule, and this is one
    /// crate, so `pub(crate)` is `pub` by another name. Pinned as the whole
    /// squeezed declaration with its braces, so no modifier fits in any
    /// spelling or on any line. The count is over text `code_only` has taken
    /// the comments out of, so a decoy carrying the declaration cannot pay
    /// for the match that widening it loses.
    ///
    /// The modules that NAME the enumeration. `defined_names().any(|n| n == w)`
    /// answers a word question, and privacy cannot tell asking from listing.
    /// Pinned on the IDENTIFIER rather than on a call shape, because every
    /// form of call -- method, qualified, function item, through a binding --
    /// writes the name. That is the modules that write it, not reach: a
    /// forwarder in `complete.rs` carries it anywhere.
    #[test]
    fn the_map_carries_no_visibility_and_only_completion_enumerates() {
        let decl = "structFuncs{table:HashMap<String,Func>,}";
        assert_eq!(
            squeeze(&code_only(source("funcs.rs"))).matches(decl).count(),
            1,
            "the map must be the one field of `Funcs` and carry no visibility"
        );
        let enumerate = concat!("defined_", "names");
        for (module, _) in SOURCES.iter().filter(|(n, _)| *n != "funcs.rs") {
            let want = usize::from(*module == "complete.rs");
            assert_eq!(
                squeeze(&code_only(source(module))).matches(enumerate).count(),
                want,
                "{module}: completion names the enumeration once, every other module never"
            );
        }
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

    /// `sys.rs` carries no block comment at all. That used to be what made a
    /// LINE-based strip complete for the one file that may hold `unsafe`;
    /// `code_only` handles block comments now, so it is a backstop instead
    /// and it stays as one. The strip is a hand-written scan of Rust's
    /// literal grammar, and review found two spellings it had wrong -- a C
    /// raw string, and a lifetime in front of a string -- each of which ended
    /// a literal early and let the text after it read as a comment. Both are
    /// fixed and pinned. A third would hide a construct from every scan here
    /// without changing what the compiler sees, and this file is the one
    /// where that costs the most, so a BLOCK comment there does not depend
    /// on the scan being right. Only that class: a mis-lexed literal can
    /// still run a `//` to the end of its line, which this does not catch.
    /// It also means `sys.rs` may not hold `/*` as data.
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

    /// The `ioctl` roster is THREE REQUESTS — not syscalls, of which the crate
    /// has four — pinned by VALUE, since a name alone would let the number
    /// change under it.
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
    fn the_syscall_roster_is_exactly_four_and_value_pinned() {
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
                concat!("SYS", "_POLL:usize=7").to_string(),
                concat!("SYS", "_RT_SIGACTION:usize=13").to_string(),
                concat!("SYS", "_IOCTL:usize=16").to_string(),
                concat!("SYS", "_UMASK:usize=95").to_string(),
            ]
        );
    }

    /// `poll` has one meaning, so unlike `ioctl` there is no request roster to
    /// gate — what has to be pinned is the ARGUMENT. The buffer LENGTH already
    /// is, in the shipped build, but a length is only half of it: the kernel
    /// writes `nfds * sizeof(struct pollfd)` through that pointer, so `nfds`
    /// decides the same thing the length does. A bare `2` at the call site
    /// would be an out-of-bounds kernel write past an eight-byte buffer, from
    /// code the compiler reads as `deny(unsafe_code)` clean.
    #[test]
    fn the_poll_argument_is_one_descriptor_and_one_event() {
        // The crate's own tests name all of these freely, so the scan stops
        // where they begin -- and comments go too, as the ioctl scan does.
        let shipped = source("sys.rs")
            .split(concat!("#[cfg(", "test)]"))
            .next()
            .unwrap_or_default();
        let sys = squeeze(&code_only(shipped));
        for decl in [
            concat!("constPOLL", "IN:u32=0x001;"),
            concat!("constPOLL", "ERR:u32=0x008;"),
            concat!("constPOLL", "HUP:u32=0x010;"),
            concat!("constPOLL", "NVAL:u32=0x020;"),
            concat!("constPOLL", "FD_COUNT:usize=1;"),
            concat!("constPOLL", "FD_WORDS:usize=2;"),
        ] {
            assert_eq!(sys.matches(decl).count(), 1, "`{decl}` must be pinned by value");
        }
        // The length assertion is in the SHIPPED build, not a test, and it is
        // the two constants MULTIPLIED -- a count and a width each right on
        // their own still overrun if their product is not the buffer.
        assert_eq!(
            sys.matches(concat!(
                "const_:()=assert!(POLL", "FD_COUNT*POLL", "FD_WORDS*core::mem::size_of::<u32>()==8);"
            ))
            .count(),
            1,
            "the pollfd length must stay asserted in the shipped build"
        );
        // `POLLIN` three times and no more: the declaration, the ready set it
        // belongs to, and the ONE request that asks for it.
        assert_eq!(sys.matches(concat!("POLL", "IN")).count(), 3);
        // The count reaches the kernel as an ARGUMENT, so pinning the
        // declaration is not enough on its own -- the call must pass the name.
        assert_eq!(
            sys.matches(&format!(
                "{}{}",
                concat!("syscall", "4(SYS", "_POLL,request.as_mut_ptr()asusize,"),
                concat!("POLL", "FD_COUNT,")
            ))
            .count(),
            1,
            "the poll call must pass the named descriptor count"
        );
        // ... and exactly one request is ever built, asking for POLLIN alone.
        assert_eq!(
            sys.matches(concat!("poll", "fd(fd_word,POLL", "IN)")).count(),
            1,
            "one request, one event"
        );
        // Neighbours deliberately outside the surface: the other things poll
        // can be asked about, and the two syscalls that ask the same question
        // with a bigger argument.
        for absent in [
            concat!("POLL", "OUT"),
            concat!("POLL", "PRI"),
            concat!("pp", "oll"),
            concat!("sel", "ect"),
            concat!("epo", "ll"),
        ] {
            assert!(
                !sys.contains(absent),
                "{absent} must not appear in the SHIPPED half of sys.rs"
            );
        }
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
    /// subshell back the process state a fork would have kept for it, for the
    /// one that stops the shell listening to the terminal while a foreground
    /// child runs, and for the readiness question `read -t` asks, and
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
            5,
            "`{name}` should appear five times in code: its definition and its four calls"
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
            concat!("SYS", "_POLL,"),
        ] {
            assert_eq!(
                squeezed.matches(&format!("{}{number}", concat!("syscall", "4("))).count(),
                1,
                "each call must pass the named syscall number"
            );
        }
    }
}
