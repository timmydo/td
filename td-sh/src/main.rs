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
//! The interpreter is pure safe `std`: a virtual file-descriptor table replaces
//! `dup2`, `std::io::pipe`/`CommandExt::exec` cover the process primitives, and
//! subshells clone shell state in-process — so the crate stays
//! `#![deny(unsafe_code)]`. Job control, signal traps beyond `EXIT`, `umask` and
//! truly concurrent (streaming) pipelines are the features that would need raw
//! syscalls; adding them is a reviewed AGENTS.md amendment (a confined
//! `syscall.rs` mirroring td-kexec/td-netd), deferred until an interactive shell
//! actually requires it.
#![deny(unsafe_code)]

mod arith;
mod ast;
mod builtin;
mod exec;
mod expand;
mod lexer;
mod parser;
mod pattern;
mod process;
mod random;

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;

use exec::{run_program, Shell};

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

    // `-c COMMAND [name [arg...]]`: the command is the first operand, taken after
    // the whole option list, and the word after it is $0.
    if minus_c {
        let cmd = args
            .get(i)
            .ok_or_else(|| "-c requires an argument".to_string())?
            .clone();
        if let Some(name) = args.get(i + 1) {
            sh.arg0 = name.clone();
            sh.params = args.iter().skip(i + 2).cloned().collect();
        }
        return Ok(run_program(&mut sh, &cmd));
    }

    // A script-file operand: `td-sh script [arg...]`.
    if !sh.opts.stdin {
        if let Some(path) = args.get(i) {
            sh.arg0 = path.clone();
            sh.params = args.iter().skip(i + 1).cloned().collect();
            let src =
                std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            return Ok(run_program(&mut sh, &src));
        }
    }

    // No script and no `-c`: interactive when stdin is a terminal, otherwise read
    // and run the whole of stdin as a script.
    sh.params = args.iter().skip(i).cloned().collect();
    // dash sets sflag when no operand is left as well, which is what puts `s` in
    // `$-`; an EXPLICIT `-s` is what suppresses the prompt.
    let explicit_s = sh.opts.stdin;
    sh.opts.stdin = true;
    if !explicit_s && std::io::stdin().is_terminal() {
        sh.interactive = true;
        let code = repl(&mut sh);
        return Ok(exec::run_exit_trap(&mut sh, code));
    }
    let mut src = String::new();
    std::io::stdin()
        .read_to_string(&mut src)
        .map_err(|e| format!("stdin: {e}"))?;
    Ok(run_program(&mut sh, &src))
}

/// A minimal read-eval-print loop: read lines, accumulating while the parse is
/// merely incomplete (an open quote, `if`, here-doc, …), then run the command.
fn repl(sh: &mut Shell) -> i32 {
    let stdin = std::io::stdin();
    loop {
        let ps1 = sh.get_var("PS1").unwrap_or_else(|| "$ ".to_string());
        let _ = write!(std::io::stdout(), "{ps1}");
        let _ = std::io::stdout().flush();

        let mut buffer = String::new();
        let ended = matches!(read_complete(&stdin, sh, &mut buffer), ReadResult::Eof);
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

enum ReadResult {
    Ready,
    Eof,
}

/// Read lines into `buffer` until it parses (or fails with a non-continuation
/// error), or stdin ends.
fn read_complete(stdin: &std::io::Stdin, sh: &Shell, buffer: &mut String) -> ReadResult {
    loop {
        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => return ReadResult::Eof,
            Ok(_) => buffer.push_str(&line),
            Err(_) => return ReadResult::Eof,
        }
        match parser::parse_aliased(buffer, &sh.aliases) {
            Ok(_) => return ReadResult::Ready,
            Err(e) if e.starts_with(ast::INCOMPLETE) => {
                let ps2 = sh.get_var("PS2").unwrap_or_else(|| "> ".to_string());
                let _ = write!(std::io::stdout(), "{ps2}");
                let _ = std::io::stdout().flush();
            }
            // A real syntax error: hand the buffer back so the caller reports it.
            Err(_) => return ReadResult::Ready,
        }
    }
}
