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
    let mut command: Option<String> = None;
    let mut read_stdin = false;

    // Leading options: `-c`, `-s`, and the `set`-style single-letter flags.
    while let Some(arg) = args.get(i) {
        if arg == "--" {
            i += 1;
            break;
        }
        let Some(flags) = arg.strip_prefix('-') else {
            break;
        };
        if flags.is_empty() {
            // A bare `-` means read from stdin.
            read_stdin = true;
            i += 1;
            break;
        }
        let mut consumed_value = false;
        for c in flags.chars() {
            match c {
                'c' => {
                    let cmd = args
                        .get(i + 1)
                        .ok_or_else(|| "-c requires an argument".to_string())?;
                    command = Some(cmd.clone());
                    consumed_value = true;
                }
                's' => read_stdin = true,
                'e' => sh.opts.errexit = true,
                'u' => sh.opts.nounset = true,
                'x' => sh.opts.xtrace = true,
                'f' => sh.opts.noglob = true,
                'v' => sh.opts.verbose = true,
                'C' => sh.opts.noclobber = true,
                'i' => sh.interactive = true,
                other => return Err(format!("unknown option -{other}")),
            }
        }
        i += 1;
        if consumed_value {
            i += 1;
            break;
        }
    }

    // `-c COMMAND [name [arg...]]`: the word after COMMAND is $0.
    if let Some(cmd) = command {
        if let Some(name) = args.get(i) {
            sh.arg0 = name.clone();
            sh.params = args.iter().skip(i + 1).cloned().collect();
        }
        return Ok(run_program(&mut sh, &cmd));
    }

    // A script-file operand: `td-sh script [arg...]`.
    if !read_stdin {
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
    if !read_stdin && std::io::stdin().is_terminal() {
        sh.interactive = true;
        return Ok(repl(&mut sh));
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
                Ok(list) => match exec::run_list(sh, &list) {
                    Ok(()) => {}
                    Err(exec::Sig::Exit(code)) => return code,
                    Err(_) => {}
                },
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
