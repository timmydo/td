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
//! The interpreter is safe `std` but for ONE confined syscall: a virtual
//! file-descriptor table replaces `dup2`, `Stdio::piped`/`CommandExt::exec`
//! cover the process primitives, and subshells clone shell state in-process. The
//! crate `#![deny(unsafe_code)]`s and `sys.rs` carries the single scoped
//! `#[allow]` — `umask(2)`, which `std` exposes no API for at all. That is the
//! EIGHTH target-side unsafe exception UNSAFE.md records; the confinement tests
//! below assert its roster against this crate's own source.
//!
//! Still deferred, and each a further reviewed amendment: job control and signal
//! traps beyond `EXIT`. Concurrent (streaming) pipelines are NOT among them —
//! `std::io::pipe` is stable and needs no `unsafe`; today's stages are still run
//! sequentially with the producer buffered, which is a refinement rather than a
//! syscall question.
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
mod sys;

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

/// Assertions about this crate's own SOURCE, which the compiler cannot make:
/// the unsafe surface is one syscall, in one module, with one call site, named
/// by two modules and no others.
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
        ("parser.rs", include_str!("parser.rs")),
        ("pattern.rs", include_str!("pattern.rs")),
        ("process.rs", include_str!("process.rs")),
        ("random.rs", include_str!("random.rs")),
        ("sys.rs", include_str!("sys.rs")),
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

    /// The roster is ONE syscall, pinned by VALUE — a name alone would let the
    /// number change under it.
    #[test]
    fn the_syscall_roster_is_exactly_one_and_value_pinned() {
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
        assert_eq!(seen, vec![concat!("SYS", "_UMASK:usize=95").to_string()]);
    }

    /// The assembly body is pinned WHOLE, including which register each argument
    /// lands in and that `options(nomem)` stays absent.
    #[test]
    fn the_confined_block_is_pinned_whole() {
        let sys = squeeze(source("sys.rs"));
        let body = squeeze(concat!(
            "core::arch::", "asm", "!(\n",
            "    \"syscall\",\n",
            "    inlateout(\"rax\") n as isize => ret,\n",
            "    in(\"rdi\") a1,\n",
            "    out(\"rcx\") _,\n",
            "    out(\"r11\") _,\n",
            "    options(nostack),\n",
            ");"
        ));
        assert!(sys.contains(&body), "the confined assembly body changed");
        assert_eq!(count(concat!("arch::", "asm", "!")), 1, "one asm site only");
    }

    /// TWO modules reach the wrappers and no others: `builtin.rs` for the
    /// builtin itself, and `process.rs` for the scope guard that gives a
    /// subshell back the mask a fork would have kept for it. Nothing renames the
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
                "builtin.rs" | "process.rs" => assert!(uses > 0, "{name} stopped calling"),
                _ => assert_eq!(uses, 0, "{name} reaches the syscall module"),
            }
        }
        // Both callers spell it out in full, so ANY import of the module is a
        // way to reach it under a name the scan above does not look for --
        // `use crate::sys::set;` leaves only a bare `set(…)` behind.
        let import = concat!("use crate::", "sys");
        let alias_a = concat!("sys", " as ");
        let alias_b = concat!(" as ", "sys");
        for (name, text) in SOURCES {
            let code = code_only(text);
            assert!(!code.contains(import), "{name} imports out of the syscall module");
            assert!(
                !code.contains(alias_a) && !code.contains(alias_b),
                "{name} aliases the syscall module"
            );
        }
    }

    /// The raw entry point has ONE call site. Module privacy stops another
    /// MODULE reaching `syscall1`, but not another wrapper inside `sys.rs`, and
    /// a second wrapper is a second syscall however safe its signature looks.
    #[test]
    fn the_raw_syscall_has_exactly_one_call_site() {
        let code = code_only(source("sys.rs"));
        let name = concat!("syscall", "1");
        assert_eq!(
            code.matches(name).count(),
            2,
            "`{name}` should appear twice in code: its definition and its one call"
        );
        assert_eq!(
            code.matches(concat!("fn ", "syscall", "1(")).count(),
            1,
            "more than one raw entry point"
        );
        // The number reaches the kernel as an ARGUMENT, so pinning the `SYS_*`
        // declarations is not enough on its own: `syscall1(90, x)` names no
        // constant and would satisfy the roster test above.
        assert_eq!(
            squeeze(&code).matches(concat!("syscall", "1(", "SYS", "_UMASK,")).count(),
            1,
            "the one call must pass the named syscall number"
        );
    }
}
