//! The interpreter: shell state, the command-tree walker, redirections and
//! command substitution. Builtins live in `builtin.rs`; process spawning and
//! pipelines in `process.rs`.
//!
//! Control flow that unwinds the tree — `break`, `continue`, `return`, `exit`
//! and a fatal expansion error — travels as the `Sig` error variant of `R`, so
//! the ordinary `?` operator carries it out to the right handler. A normal
//! command just leaves its status in `Shell::status`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::ast::{AndOr, Cmd, Conn, List, Pipeline, Redir, Sep, Word, INCOMPLETE};
use crate::builtin;
use crate::expand;
use crate::lexer::Aliases;
use crate::parser::parse_aliased;
use crate::pattern;
use crate::process::{self, Fds};

/// A non-local transfer of control. Only `Sig::Exit` leaves the interpreter; the
/// loop and function forms are caught by their construct.
#[derive(Clone, Copy, Debug)]
pub enum Sig {
    Break(u32),
    Continue(u32),
    Return(i32),
    Exit(i32),
}

pub type R<T> = Result<T, Sig>;

#[derive(Clone, Debug)]
pub struct Var {
    pub value: String,
    pub exported: bool,
    pub readonly: bool,
}

/// The `set -o` flags the interpreter honours.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opts {
    pub errexit: bool,   // -e
    pub nounset: bool,   // -u
    pub xtrace: bool,    // -x
    pub noglob: bool,    // -f
    pub verbose: bool,   // -v
    pub noclobber: bool, // -C
}

impl Opts {
    /// The `$-` letters, in a fixed order.
    pub fn letters(&self) -> String {
        let mut s = String::new();
        for (on, c) in [
            (self.errexit, 'e'),
            (self.nounset, 'u'),
            (self.xtrace, 'x'),
            (self.noglob, 'f'),
            (self.verbose, 'v'),
            (self.noclobber, 'C'),
        ] {
            if on {
                s.push(c);
            }
        }
        s
    }
}

pub struct Shell {
    pub vars: HashMap<String, Var>,
    pub funcs: HashMap<String, Arc<Cmd>>,
    pub params: Vec<String>, // positional parameters $1..
    pub arg0: String,        // $0
    pub status: i32,         // $?
    pub last_bg: u32,        // $!
    pub opts: Opts,
    pub cwd: PathBuf,
    pub fds: Fds,
    pub in_function: bool,
    pub loop_depth: u32,
    /// Runtime recursion depth (function calls + command substitution), bounded so
    /// `f() { f; }; f` and `$( $( … ) )` error instead of overflowing the stack.
    pub run_depth: u32,
    /// Count of command substitutions performed, used to decide the exit status of
    /// an assignment-only command (`x=$(cmd)` takes the last substitution's status).
    pub cmdsubst_count: u64,
    /// Nesting depth of `errexit`-suppressed contexts (an `if`/`while`/`until`
    /// condition). Non-zero means a failing command must NOT trigger `set -e`, and
    /// it propagates into compounds nested inside the condition.
    pub errexit_suppressed: u32,
    pub interactive: bool,
    /// `getopts` scan cursor, hidden like dash's: the 1-based index of the next
    /// WORD and the byte offset inside the word being consumed (-1 == start a
    /// fresh one). Hidden rather than read back out of $OPTIND because dash keeps
    /// it per argument frame -- a function gets its own, and `set`/`shift` reset
    /// it -- while the OPTIND *variable* is global and only written by `getopts`.
    pub getopts_optind: i64,
    pub getopts_off: i64,
    /// Aliases in force. They are consumed at PARSE time, so only the unit loop
    /// and the other parse entry points read this.
    pub aliases: Aliases,
    /// This `Shell` is an in-process CLONE (subshell, async list, command
    /// substitution), not a forked process. `exec` must not replace the real
    /// process from one, or the rest of the script would be lost with it.
    pub cloned: bool,
}

/// Bound on nested command execution — enforced once, at the `run_command` choke
/// point every compound/nested command descends through (subshells, groups,
/// if/for/while bodies, function calls, `eval`, `.`/source, and command
/// substitution). Deep shell recursion is almost always a bug; this fires well
/// past any legitimate script but well before the native stack overflows (which
/// would SIGABRT — there is no unsafe/stack-probe escape hatch here).
const MAX_RUN_DEPTH: u32 = 256;

impl Shell {
    pub fn new() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut vars = HashMap::new();
        for (k, v) in std::env::vars() {
            vars.insert(
                k,
                Var {
                    value: v,
                    exported: true,
                    readonly: false,
                },
            );
        }
        let mut sh = Shell {
            vars,
            funcs: HashMap::new(),
            params: Vec::new(),
            arg0: "td-sh".to_string(),
            status: 0,
            last_bg: 0,
            opts: Opts::default(),
            cwd,
            fds: Fds::new(),
            in_function: false,
            loop_depth: 0,
            run_depth: 0,
            cmdsubst_count: 0,
            errexit_suppressed: 0,
            interactive: false,
            getopts_optind: 1,
            getopts_off: -1,
            aliases: Aliases::new(),
            cloned: false,
        };
        // POSIX seeds these when absent; scripts assume they exist.
        if sh.get_var("IFS").is_none() {
            let _ = sh.set_var("IFS", " \t\n");
        }
        // POSIX: OPTIND is 1 at shell start, overriding any imported value.
        let _ = sh.set_var("OPTIND", "1");
        if sh.get_var("PPID").is_none() {
            // Best-effort; std has no getppid, so leave it to the environment.
        }
        sh
    }

    /// A shell with no inherited environment — deterministic for unit tests.
    #[cfg(test)]
    pub fn new_for_test() -> Self {
        let mut sh = Shell {
            vars: HashMap::new(),
            funcs: HashMap::new(),
            params: Vec::new(),
            arg0: "td-sh".to_string(),
            status: 0,
            last_bg: 0,
            opts: Opts::default(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            fds: Fds::new(),
            in_function: false,
            loop_depth: 0,
            run_depth: 0,
            cmdsubst_count: 0,
            errexit_suppressed: 0,
            interactive: false,
            getopts_optind: 1,
            getopts_off: -1,
            aliases: Aliases::new(),
            cloned: false,
        };
        let _ = sh.set_var("IFS", " \t\n");
        let _ = sh.set_var("OPTIND", "1");
        sh
    }

    pub fn get_var(&self, name: &str) -> Option<String> {
        self.vars.get(name).map(|v| v.value.clone())
    }

    /// Assign a shell variable, honouring the readonly attribute. Errors are
    /// returned as `Sig::Exit(1)` after a message, so `?` reports them.
    pub fn set_var(&mut self, name: &str, value: &str) -> R<()> {
        // Assigning OPTIND at all -- even the value it already holds -- restarts
        // `getopts` at a word boundary, as dash's OPTIND hook does. `getopts`
        // itself re-establishes the offset after publishing OPTIND.
        if name == "OPTIND" {
            // dash's OPTIND hook: any assignment moves the cursor and abandons a
            // half-consumed word. dash's number() takes an all-digit string only
            // (so " 2", "+1" and "-1" are all rejected) and coerces 0 up to 1; a
            // rejected value parks -1, which `getopts` reports when it next runs.
            let digits = !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
            self.getopts_optind =
                if digits { value.parse::<i64>().unwrap_or(i64::MAX).max(1) } else { -1 };
            self.getopts_off = -1;
        }
        match self.vars.get_mut(name) {
            Some(v) if v.readonly => {
                return Err(self.fatal(&format!("{name}: is read only"), 1));
            }
            Some(v) => v.value = value.to_string(),
            None => {
                self.vars.insert(
                    name.to_string(),
                    Var {
                        value: value.to_string(),
                        exported: false,
                        readonly: false,
                    },
                );
            }
        }
        Ok(())
    }

    pub fn export(&mut self, name: &str) {
        if let Some(v) = self.vars.get_mut(name) {
            v.exported = true;
        } else {
            self.vars.insert(
                name.to_string(),
                Var {
                    value: String::new(),
                    exported: true,
                    readonly: false,
                },
            );
        }
    }

    pub fn set_readonly(&mut self, name: &str) {
        if let Some(v) = self.vars.get_mut(name) {
            v.readonly = true;
        } else {
            self.vars.insert(
                name.to_string(),
                Var {
                    value: String::new(),
                    exported: false,
                    readonly: true,
                },
            );
        }
    }

    pub fn unset_var(&mut self, name: &str) -> bool {
        match self.vars.get(name) {
            Some(v) if v.readonly => false,
            _ => {
                self.vars.remove(name);
                true
            }
        }
    }

    /// The environment a child process inherits: every exported variable.
    pub fn exported_env(&self) -> Vec<(String, String)> {
        self.vars
            .iter()
            .filter(|(_, v)| v.exported)
            .map(|(k, v)| (k.clone(), v.value.clone()))
            .collect()
    }

    /// Resolve a path against the shell's logical cwd.
    pub fn resolve(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    pub fn set_status(&mut self, code: i32) {
        self.status = code & 0xff;
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse and run a whole program, returning the final `$?`. A parse error prints
/// to stderr and yields 2 (the POSIX syntax-error status).
pub fn run_program(sh: &mut Shell, src: &str) -> i32 {
    match run_source(sh, src, "") {
        Ok(()) => sh.status,
        Err(Sig::Exit(code)) => code,
        // A stray break/continue/return at the top level is not an error worth
        // aborting the process over; POSIX leaves it unspecified.
        Err(_) => sh.status,
    }
}

/// Run `src` one top-level unit at a time, as dash reads a script: a command is
/// parsed only once everything before it has run. That is what makes an `alias`
/// visible to the next line but not to the rest of its own line. A syntax error
/// stops the run with status 2, reported as `td-sh: {what}{error}`.
pub fn run_source(sh: &mut Shell, src: &str, what: &str) -> R<()> {
    let mut off = 0usize;
    loop {
        let Some(rest) = src.get(off..) else {
            return Ok(());
        };
        match next_unit(rest, &sh.aliases) {
            None => return Ok(()),
            Some(Err(e)) => {
                let _ = write_stderr(sh, &format!("td-sh: {what}{e}"));
                sh.set_status(2);
                return Ok(());
            }
            Some(Ok((list, used))) => {
                off += used;
                run_list(sh, &list)?;
            }
        }
    }
}

/// The next parse unit: the shortest run of whole lines that parses. `None` once
/// only blanks remain; the `usize` is how many bytes of `src` it consumed.
fn next_unit(src: &str, aliases: &Aliases) -> Option<Result<(List, usize), String>> {
    if src.trim().is_empty() {
        return None;
    }
    let mut end = 0usize;
    loop {
        end = match src
            .as_bytes()
            .get(end..)
            .and_then(|tail| tail.iter().position(|&b| b == b'\n'))
        {
            Some(off) => end + off + 1,
            None => src.len(),
        };
        let more = end < src.len();
        let head = src.get(..end).unwrap_or(src);
        match parse_aliased(head, aliases) {
            Ok(list) => return Some(Ok((list, end))),
            Err(e) if more && e.starts_with(INCOMPLETE) => continue,
            Err(e) => return Some(Err(e)),
        }
    }
}


pub fn run_list(sh: &mut Shell, list: &List) -> R<()> {
    for (and_or, sep) in &list.items {
        if *sep == Sep::Bg {
            // No job control yet: run the async list in an ISOLATED subshell so its
            // variable/cwd/option changes cannot leak, then continue immediately
            // with $?=0. True background execution, a real $! and a functional
            // `wait` are deferred (see the crate-root note); $! is a placeholder.
            let mut child = process::fork_shell(sh);
            let _ = run_and_or(&mut child, and_or);
            sh.last_bg = std::process::id();
            sh.set_status(0);
        } else {
            run_and_or(sh, and_or)?;
        }
    }
    Ok(())
}

/// Run a list as an `if`/`elif`/`while`/`until` CONDITION: `errexit` is suppressed
/// for the whole subtree (POSIX), so a failing test does not exit the shell. The
/// suppression is a counter, so it also covers compounds nested in the condition.
fn run_condition(sh: &mut Shell, list: &List) -> R<()> {
    sh.errexit_suppressed += 1;
    let result = run_list(sh, list);
    sh.errexit_suppressed = sh.errexit_suppressed.saturating_sub(1);
    result
}

fn run_and_or(sh: &mut Shell, and_or: &AndOr) -> R<()> {
    let n_rest = and_or.rest.len();
    // Operand 0 is the structurally-last only when there is no `&&`/`||` tail.
    run_operand(sh, &and_or.first, n_rest == 0)?;
    for (idx, (conn, pipe)) in and_or.rest.iter().enumerate() {
        let go = match conn {
            Conn::And => sh.status == 0,
            Conn::Or => sh.status != 0,
        };
        if !go {
            continue;
        }
        run_operand(sh, pipe, idx + 1 == n_rest)?;
    }
    Ok(())
}

/// Run one `&&`/`||` operand. POSIX ignores `errexit` while executing any operand
/// that is not the structurally-last, and any `!`-negated pipeline. The exemption
/// must cover the WHOLE (possibly compound/function) operand — a failing command
/// nested inside an exempt operand must not exit the shell — so it is a
/// suppression scope, not a post-hoc check. Only the final, non-negated operand,
/// once run, is subject to `errexit`.
fn run_operand(sh: &mut Shell, pipe: &Pipeline, is_last: bool) -> R<()> {
    let exempt = !is_last || pipe.bang;
    if exempt {
        sh.errexit_suppressed += 1;
    }
    let result = run_pipeline(sh, pipe);
    if exempt {
        sh.errexit_suppressed = sh.errexit_suppressed.saturating_sub(1);
    }
    result?;
    if !exempt {
        maybe_errexit(sh)?;
    }
    Ok(())
}

fn maybe_errexit(sh: &mut Shell) -> R<()> {
    if sh.opts.errexit && sh.errexit_suppressed == 0 && sh.status != 0 {
        return Err(Sig::Exit(sh.status));
    }
    Ok(())
}

fn run_pipeline(sh: &mut Shell, pipe: &Pipeline) -> R<()> {
    if pipe.cmds.len() == 1 {
        if let Some(cmd) = pipe.cmds.first() {
            run_command(sh, cmd)?;
        }
    } else {
        process::run_pipeline(sh, &pipe.cmds)?;
    }
    if pipe.bang {
        sh.set_status(i32::from(sh.status == 0));
    }
    Ok(())
}

/// Run one command in the current shell (not a pipeline stage). This is the single
/// choke point where execution nesting is bounded: every compound body, function
/// call, `eval`/`.`, and command-substitution body re-enters here, so one guard
/// covers them all (and their compositions) against a native stack overflow.
pub fn run_command(sh: &mut Shell, cmd: &Cmd) -> R<()> {
    if sh.run_depth >= MAX_RUN_DEPTH {
        return Err(sh.fatal("maximum recursion depth exceeded", 2));
    }
    sh.run_depth += 1;
    let result = run_command_inner(sh, cmd);
    sh.run_depth -= 1;
    result
}

fn run_command_inner(sh: &mut Shell, cmd: &Cmd) -> R<()> {
    match cmd {
        Cmd::Simple {
            assigns,
            words,
            redirs,
        } => run_simple(sh, assigns, words, redirs),
        Cmd::Subshell { body, redirs } => {
            let list = body.clone();
            let redirs = redirs.clone();
            process::run_subshell(sh, &list, &redirs)
        }
        Cmd::Group { body, redirs } => with_redirs(sh, redirs, |sh| run_list(sh, body)),
        Cmd::If {
            arms,
            otherwise,
            redirs,
        } => with_redirs(sh, redirs, |sh| run_if(sh, arms, otherwise)),
        Cmd::For {
            var,
            words,
            body,
            redirs,
        } => with_redirs(sh, redirs, |sh| run_for(sh, var, words.as_deref(), body)),
        Cmd::Loop {
            until,
            cond,
            body,
            redirs,
        } => with_redirs(sh, redirs, |sh| run_loop(sh, *until, cond, body)),
        Cmd::Case {
            word,
            items,
            redirs,
        } => with_redirs(sh, redirs, |sh| run_case(sh, word, items)),
        Cmd::FuncDef { name, body } => {
            sh.funcs.insert(name.clone(), body.clone());
            sh.set_status(0);
            Ok(())
        }
    }
}

fn run_if(
    sh: &mut Shell,
    arms: &[crate::ast::IfArm],
    otherwise: &Option<List>,
) -> R<()> {
    for arm in arms {
        run_condition(sh, &arm.cond)?;
        if sh.status == 0 {
            return run_list(sh, &arm.body);
        }
    }
    if let Some(body) = otherwise {
        return run_list(sh, body);
    }
    sh.set_status(0);
    Ok(())
}

fn run_for(sh: &mut Shell, var: &str, words: Option<&[Word]>, body: &List) -> R<()> {
    let items: Vec<String> = match words {
        Some(ws) => expand::expand_word_list(sh, ws)?,
        None => sh.params.clone(),
    };
    sh.set_status(0);
    sh.loop_depth += 1;
    let result = (|| {
        for item in items {
            sh.set_var(var, &item)?;
            match run_list(sh, body) {
                Ok(()) => {}
                Err(Sig::Break(n)) => return break_out(sh, n),
                Err(Sig::Continue(n)) => {
                    if n > 1 && sh.loop_depth > 1 {
                        return Err(Sig::Continue(n - 1));
                    }
                    // `continue N` past the outermost loop continues this one.
                }
                Err(other) => return Err(other),
            }
        }
        Ok(())
    })();
    sh.loop_depth -= 1;
    result
}

fn run_loop(sh: &mut Shell, until: bool, cond: &List, body: &List) -> R<()> {
    sh.set_status(0);
    sh.loop_depth += 1;
    // The loop's status is the last body command's (or 0 if the body never
    // ran) — NOT the condition's, which is non-zero on the exit iteration.
    let mut body_status = 0;
    let result = (|| {
        loop {
            run_condition(sh, cond)?;
            let go = if until { sh.status != 0 } else { sh.status == 0 };
            if !go {
                break;
            }
            match run_list(sh, body) {
                Ok(()) => body_status = sh.status,
                Err(Sig::Break(n)) => {
                    body_status = sh.status;
                    return break_out(sh, n);
                }
                Err(Sig::Continue(n)) => {
                    body_status = sh.status;
                    if n > 1 && sh.loop_depth > 1 {
                        return Err(Sig::Continue(n - 1));
                    }
                    // `continue N` past the outermost loop continues this one.
                }
                Err(other) => return Err(other),
            }
        }
        Ok(())
    })();
    sh.loop_depth -= 1;
    sh.set_status(body_status);
    result
}

/// A `break N` that names an enclosing loop turns into a break of the remaining
/// levels once this loop has stopped. When `N` exceeds the number of enclosing
/// loops, POSIX exits all of them and then continues normally — so the break is
/// only propagated while an enclosing loop actually exists (`loop_depth` still
/// counts this one at the catch point, hence `> 1`).
fn break_out(sh: &mut Shell, n: u32) -> R<()> {
    if n > 1 && sh.loop_depth > 1 {
        Err(Sig::Break(n - 1))
    } else {
        Ok(())
    }
}

fn run_case(sh: &mut Shell, word: &Word, items: &[crate::ast::CaseItem]) -> R<()> {
    let subject = expand::expand_single(sh, word)?;
    for item in items {
        for pat in &item.patterns {
            let chars = expand::expand_pattern(sh, pat)?;
            let units = pattern::compile(&chars);
            if pattern::matches(&units, &subject) {
                return run_list(sh, &item.body);
            }
        }
    }
    sh.set_status(0);
    Ok(())
}

fn run_simple(
    sh: &mut Shell,
    assigns: &[crate::ast::Assign],
    words: &[Word],
    redirs: &[Redir],
) -> R<()> {
    let cmdsubst_before = sh.cmdsubst_count;
    let argv = expand::expand_word_list(sh, words)?;
    // No command name — either none was given (`a=1 b=2`) or every word field-split
    // away (`x=new $empty`). POSIX: the assignments affect the CURRENT shell,
    // redirections are performed then dropped (`>file` truncates), and the exit
    // status is the last command substitution's, or 0 if this command performed
    // none. `cmdsubst_before` captures whether any substitution ran (in the words
    // above or the assignments below) so an unrelated prior `$?` is not carried in.
    if argv.is_empty() {
        let saved = match process::apply_redirs(sh, redirs)? {
            process::RedirOutcome::Applied(s) => s,
            // A failed redirection skips the command; the assignments do not run.
            process::RedirOutcome::Failed => return Ok(()),
        };
        let result = (|| {
            for a in assigns {
                let value = expand::expand_assign(sh, &a.value)?;
                sh.set_var(&a.name, &value)?;
            }
            if sh.cmdsubst_count == cmdsubst_before {
                sh.set_status(0);
            }
            Ok(())
        })();
        process::restore_redirs(sh, saved);
        return result;
    }

    if sh.opts.xtrace {
        let _ = write_stderr(sh, &format!("+ {}", argv.join(" ")));
    }

    // A function call runs in the current shell with the assignments applied for
    // its duration and the words as its positional parameters.
    if let Some(cmd) = argv.first().and_then(|name| sh.funcs.get(name)).cloned() {
        return call_function(sh, &cmd, &argv, assigns, redirs);
    }

    if let Some(bi) = builtin::lookup(argv.first().map(String::as_str).unwrap_or("")) {
        return run_builtin(sh, bi, &argv, assigns, redirs);
    }

    // External command: assignments become part of its environment only.
    let env_overrides = expand_assignments(sh, assigns)?;
    let saved = match process::apply_redirs(sh, redirs)? {
        process::RedirOutcome::Applied(s) => s,
        // A failed redirection skips the command without exiting the shell.
        process::RedirOutcome::Failed => return Ok(()),
    };
    let result = process::exec_external(sh, &argv, &env_overrides);
    process::restore_redirs(sh, saved);
    result
}

fn run_builtin(
    sh: &mut Shell,
    bi: builtin::Builtin,
    argv: &[String],
    assigns: &[crate::ast::Assign],
    redirs: &[Redir],
) -> R<()> {
    if builtin::is_special(bi) {
        // POSIX special builtins (`:`, `.`, `eval`, `export`, `set`, `shift`, …):
        // prefix assignments persist in the current shell. Redirections precede the
        // assignments (POSIX 2.9.1 order), so a failed redirection skips both.
        let saved = match process::apply_redirs(sh, redirs)? {
            process::RedirOutcome::Applied(s) => s,
            // A redirection error on a special builtin aborts a NON-interactive shell
            // (POSIX); an interactive shell reports it and continues. `$?` is already 1.
            process::RedirOutcome::Failed => {
                if sh.interactive {
                    return Ok(());
                }
                return Err(Sig::Exit(sh.status));
            }
        };
        let result = (|| {
            for a in assigns {
                let value = expand::expand_assign(sh, &a.value)?;
                sh.set_var(&a.name, &value)?;
                // `exec`'s prefix bindings go to the replacement process, so they
                // are exported as well as set (dash's listsetvar VEXPORT).
                if matches!(bi, builtin::Builtin::Exec) {
                    sh.export(&a.name);
                }
            }
            builtin::run(sh, bi, argv)
        })();
        // Bare `exec` is the one builtin whose redirections are the POINT: they
        // stay in force for the rest of the shell instead of being unwound here.
        // With a command word they belong to the replacement process, so a FAILED
        // `exec` (which only returns in an interactive shell) must still unwind.
        if matches!(bi, builtin::Builtin::Exec) && argv.len() == 1 {
            return result;
        }
        process::restore_redirs(sh, saved);
        return result;
    }
    // Regular builtins (`echo`, `read`, `test`, `cd`, …): a prefix assignment is
    // transient — visible only for the builtin's own run, like an external
    // command's environment — so save and restore each affected variable. It is also
    // exported for that run so a builtin that itself execs an external utility
    // (`FOO=bar command extcmd`) passes it through; the saved prior `Var` carries the
    // original export flag, which the restore below puts back.
    let mut saved_vars: Vec<(String, Option<Var>)> = Vec::with_capacity(assigns.len());
    for a in assigns {
        let value = expand::expand_assign(sh, &a.value)?;
        saved_vars.push((a.name.clone(), sh.vars.get(&a.name).cloned()));
        sh.set_var(&a.name, &value)?;
        sh.export(&a.name);
    }
    let result = match process::apply_redirs(sh, redirs)? {
        process::RedirOutcome::Applied(saved) => {
            let r = builtin::run(sh, bi, argv);
            process::restore_redirs(sh, saved);
            r
        }
        // A failed redirection skips a regular builtin; `$?` is already 1. The
        // transient prefix assignments are still rolled back below.
        process::RedirOutcome::Failed => Ok(()),
    };
    for (name, prev) in saved_vars.into_iter().rev() {
        match prev {
            Some(v) => {
                sh.vars.insert(name, v);
            }
            None => {
                sh.vars.remove(&name);
            }
        }
    }
    result
}

fn expand_assignments(
    sh: &mut Shell,
    assigns: &[crate::ast::Assign],
) -> R<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(assigns.len());
    for a in assigns {
        let value = expand::expand_assign(sh, &a.value)?;
        out.push((a.name.clone(), value));
    }
    Ok(out)
}

fn call_function(
    sh: &mut Shell,
    body: &Arc<Cmd>,
    argv: &[String],
    assigns: &[crate::ast::Assign],
    redirs: &[Redir],
) -> R<()> {
    // Recursion is bounded centrally in `run_command` (the function body re-enters
    // there), so `f() { f; }; f` errors instead of overflowing the stack.
    // Temporary assignments precede the call and are visible inside it; dash
    // keeps them set afterward for a function, so we do too.
    for a in assigns {
        let value = expand::expand_assign(sh, &a.value)?;
        sh.set_var(&a.name, &value)?;
    }
    let new_params = argv.get(1..).unwrap_or(&[]).to_vec();
    let saved_params = std::mem::replace(&mut sh.params, new_params);
    // The cursor belongs to the argument frame, so the function scans its own
    // arguments from the start and the caller resumes where it left off. The
    // OPTIND variable is global and deliberately NOT restored.
    let saved_getopts = (sh.getopts_optind, sh.getopts_off);
    sh.getopts_optind = 1;
    sh.getopts_off = -1;
    let was_in_function = sh.in_function;
    let saved_loop_depth = sh.loop_depth;
    sh.in_function = true;
    sh.loop_depth = 0;
    let result = match process::apply_redirs(sh, redirs)? {
        process::RedirOutcome::Applied(saved) => {
            let r = run_command(sh, body);
            process::restore_redirs(sh, saved);
            r
        }
        // A failed redirection skips the function body; `$?` is already 1.
        process::RedirOutcome::Failed => Ok(()),
    };
    sh.params = saved_params;
    (sh.getopts_optind, sh.getopts_off) = saved_getopts;
    sh.in_function = was_in_function;
    sh.loop_depth = saved_loop_depth;
    match result {
        Err(Sig::Return(code)) => {
            sh.set_status(code);
            Ok(())
        }
        other => other,
    }
}

/// Run `body` with `redirs` applied, restoring the descriptors afterward even if
/// the body unwinds.
pub fn with_redirs<F>(sh: &mut Shell, redirs: &[Redir], body: F) -> R<()>
where
    F: FnOnce(&mut Shell) -> R<()>,
{
    if redirs.is_empty() {
        return body(sh);
    }
    let saved = match process::apply_redirs(sh, redirs)? {
        process::RedirOutcome::Applied(s) => s,
        // A failed redirection skips the compound command; `$?` is already 1.
        process::RedirOutcome::Failed => return Ok(()),
    };
    let result = body(sh);
    process::restore_redirs(sh, saved);
    result
}

/// `$(...)` / `` `...` ``: run the code with stdout captured, strip trailing
/// newlines, and return the text. Runs in a subshell so its state changes do not
/// leak, matching POSIX.
pub fn command_subst(sh: &mut Shell, code: &str) -> R<String> {
    // Nesting is bounded centrally in `run_command` (the substituted body re-enters
    // there), so `$( $( … ) )` errors instead of overflowing the stack.
    // Counted so an assignment-only command can adopt the last substitution's $?.
    sh.cmdsubst_count = sh.cmdsubst_count.wrapping_add(1);
    let mut out = process::capture_stdout(sh, code)?;
    while out.ends_with('\n') {
        out.pop();
    }
    Ok(out)
}

/// The word after a here-document body has been assembled into a `Word`; expand
/// it to the text fed to the command.
pub fn here_body(sh: &mut Shell, body: &Word) -> R<String> {
    expand::expand_single(sh, body)
}

/// Text of a redirection target word (single-word expansion, no split/glob).
pub fn redir_target(sh: &mut Shell, r: &Redir) -> R<String> {
    expand::expand_single(sh, &r.word)
}

/// Write `msg` plus a newline to the shell's current stderr.
pub fn write_stderr(sh: &Shell, msg: &str) -> std::io::Result<()> {
    process::write_fd(sh, 2, format!("{msg}\n").as_bytes())
}

#[cfg(test)]
mod tests {
    fn run(src: &str) -> (i32, String, String) {
        crate::process::run_capturing(src)
    }

    #[test]
    fn echo_and_exit_status() {
        let (status, out, _) = run("echo hello world");
        assert_eq!(status, 0);
        assert_eq!(out, "hello world\n");
        let (status, _, _) = run("exit 7");
        assert_eq!(status, 7);
    }

    #[test]
    fn and_or_short_circuits() {
        let (_, out, _) = run("false && echo no; true || echo no; echo done");
        assert_eq!(out, "done\n");
    }

    #[test]
    fn if_takes_the_right_branch() {
        let (_, out, _) = run("if true; then echo yes; else echo no; fi");
        assert_eq!(out, "yes\n");
        let (_, out, _) = run("if false; then echo yes; else echo no; fi");
        assert_eq!(out, "no\n");
    }

    #[test]
    fn for_loop_iterates() {
        let (_, out, _) = run("for x in a b c; do echo $x; done");
        assert_eq!(out, "a\nb\nc\n");
    }

    #[test]
    fn while_loop_counts() {
        let (_, out, _) = run("i=0; while [ $i -lt 3 ]; do echo $i; i=$((i + 1)); done");
        assert_eq!(out, "0\n1\n2\n");
    }

    #[test]
    fn case_matches_a_pattern() {
        let (_, out, _) = run("x=banana; case $x in apple) echo a ;; b*) echo b ;; *) echo o ;; esac");
        assert_eq!(out, "b\n");
    }

    #[test]
    fn function_sees_positional_params() {
        let (_, out, _) = run("greet() { echo \"hi $1\"; }; greet world");
        assert_eq!(out, "hi world\n");
    }

    #[test]
    fn subshell_status_propagates() {
        let (_, out, _) = run("( exit 4 ); echo $?");
        assert_eq!(out, "4\n");
    }

    #[test]
    fn break_and_continue() {
        let (_, out, _) = run("for x in 1 2 3 4; do if [ $x = 3 ]; then break; fi; echo $x; done");
        assert_eq!(out, "1\n2\n");
        let (_, out, _) =
            run("for x in 1 2 3; do if [ $x = 2 ]; then continue; fi; echo $x; done");
        assert_eq!(out, "1\n3\n");
    }

    #[test]
    fn errexit_stops_on_failure() {
        let (status, out, _) = run("set -e; false; echo unreached");
        assert_eq!(out, "");
        assert_eq!(status, 1);
    }

    #[test]
    fn errexit_exempts_conditions_and_nonfinal_operands() {
        // A failing `if`/`while` condition, a `!`-negated pipeline, and a non-final
        // `&&`/`||` operand must NOT trip errexit.
        let (_, out, _) = run("set -e; if false; then echo x; fi; echo a");
        assert_eq!(out, "a\n");
        let (_, out, _) = run("set -e; ! false; echo b");
        assert_eq!(out, "b\n");
        let (_, out, _) = run("set -e; false && echo no; echo c");
        assert_eq!(out, "c\n");
        let (_, out, _) = run("set -e; false || true; echo d");
        assert_eq!(out, "d\n");
    }

    #[test]
    fn prefix_assignment_is_transient_for_regular_builtins() {
        // A prefix on a regular builtin (echo) is visible only for that command.
        let (_, out, _) = run("FOO=bar echo hi; echo \"[${FOO}]\"");
        assert_eq!(out, "hi\n[]\n");
    }

    #[test]
    fn prefix_assignment_persists_for_special_builtins() {
        // A prefix on a special builtin (`:`) stays set in the current shell.
        let (_, out, _) = run("FOO=bar :; echo \"[${FOO}]\"");
        assert_eq!(out, "[bar]\n");
    }

    #[test]
    fn break_and_continue_with_a_level_count() {
        let (_, out, _) =
            run("for i in 1 2; do for j in a b; do break 2; done; echo $i; done; echo done");
        assert_eq!(out, "done\n");
        let (_, out, _) = run(
            "for i in 1 2; do for j in a b; do continue 2; done; echo inner$i; done; echo done",
        );
        assert_eq!(out, "done\n");
    }

    #[test]
    fn unbounded_recursion_is_caught_not_crashed() {
        // The runtime depth guard turns runaway recursion into a graceful fatal
        // error (controlled unwind + message) rather than a stack-overflow SIGSEGV.
        let (status, _out, err) = run("f() { f; }; f; echo unreached");
        assert_eq!(status, 2);
        assert!(err.contains("recursion depth exceeded"), "err: {err:?}");
    }

    #[test]
    fn pipeline_stages_do_not_leak_assignments() {
        // POSIX runs every pipeline stage in a subshell, so an assignment in a stage
        // is invisible to the parent.
        let (_, out, _) = run("y=1; { y=2; } | true; echo $y");
        assert_eq!(out, "1\n");
    }

    #[test]
    fn redirection_failure_is_not_fatal_for_a_regular_command() {
        // Without `-e`, a failed redirection reports the error and sets a non-zero
        // status, but the shell continues (dash) rather than exiting. The command
        // itself does not run, so no `x` is printed.
        let (_status, out, err) = run("echo x </nonexistent/td-sh-nope; echo survived");
        assert_eq!(out, "survived\n");
        assert!(err.contains("nonexistent"), "err: {err:?}");
    }

    #[test]
    fn errexit_exemption_covers_a_compound_operand() {
        // A function on the non-final side of `||` is exempt from errexit for its
        // WHOLE body: an inner `false` must not exit before `echo survived` runs.
        let (status, out, _) =
            run("set -e; f() { false; echo survived; }; f || echo fallback; echo end");
        assert_eq!(out, "survived\nend\n");
        assert_eq!(status, 0);
    }

    #[test]
    fn subshell_in_a_condition_inherits_errexit_suppression() {
        // A subshell evaluated as an `if` condition is part of the suppressed
        // context: an inner `false` must not exit before `echo survived` runs.
        let (_status, out, _) =
            run("set -e; if (false; echo survived); then echo yes; fi; echo end");
        assert_eq!(out, "survived\nyes\nend\n");
    }

    #[test]
    fn prefix_assignment_exports_to_an_external_via_command() {
        // `FOO=bar command extcmd` must pass FOO into the external's environment even
        // though a prefix on a regular builtin (`command`) is otherwise transient.
        if !std::path::Path::new("/bin/sh").exists() {
            return; // hermetic guard: no host shell to exec the probe script
        }
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let uniq = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("td-sh-prefix-{}-{}", std::process::id(), uniq));
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(b"#!/bin/sh\nprintf %s \"$FOO\"\n").unwrap();
            let mut perm = f.metadata().unwrap().permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&path, perm).unwrap();
        }
        let (_status, out, _err) = run(&format!("FOO=bar command '{}'", path.display()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(out, "bar");
    }

    #[test]
    fn interactive_special_builtin_survives_a_redirection_error() {
        // POSIX: a redirection error on a special builtin aborts a non-interactive
        // shell but NOT an interactive one. Redirections also precede the prefix
        // assignments, so the failed `export`'s FOO must not persist.
        let s = crate::process::run_capturing_interactive(
            "export FOO=bar >/nonexistent/dir/x; echo \"[${FOO}]\"",
        );
        assert_eq!(s, "[]\n");
    }

    #[test]
    fn builtin_write_error_sets_nonzero_status() {
        // A builtin write to a closed descriptor (`>&-`) fails visibly ($?=1) rather
        // than being masked back to 0.
        let (_s, out, _e) = run("echo hi >&-; echo $?");
        assert_eq!(out, "1\n");
    }

    #[test]
    fn errexit_triggers_on_a_builtin_write_error() {
        // Under `set -e` the failed write must abort the shell before `survived`.
        let (status, out, _e) = run("set -e; echo hi >&-; echo survived");
        assert_eq!(out, "");
        assert_eq!(status, 1);
    }

    #[test]
    fn subshell_redirection_target_side_effect_does_not_leak() {
        // A `${x:=…}` assignment in a SUBSHELL's redirection target stays in the
        // subshell (POSIX): the parent's `x` is untouched.
        let (_s, out, _e) = run("unset x; (:) >\"${x:=/dev/null}\"; echo \"${x-unset}\"");
        assert_eq!(out, "unset\n");
    }
}
