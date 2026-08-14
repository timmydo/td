//! Shell builtins.
//!
//! Each returns via `R<()>`, leaving its exit status in `Shell::status`; the
//! control-flow builtins (`exit`, `return`, `break`, `continue`) unwind through
//! the `Sig` error channel instead. Output goes through the shell descriptor
//! table (`process::write_fd`), never straight to `println!`, so a builtin obeys
//! the redirections in force.

use crate::exec::{Local, Shell, Sig, R};
use crate::process::{self, write_fd};
use crate::{ast, exec};

#[derive(Clone, Copy, Debug)]
pub enum Builtin {
    Colon,
    True,
    Umask,
    False,
    Echo,
    Printf,
    Exit,
    Return,
    Break,
    Continue,
    Shift,
    Set,
    Unset,
    Export,
    Readonly,
    Read,
    Getopts,
    Test,
    Eval,
    Cd,
    Pwd,
    Dot,
    Times,
    Command,
    Wait,
    Alias,
    Unalias,
    Jobs,
    Exec,
    Trap,
    Local,
    Type,
}

/// Every name `lookup` answers to, for the completion candidate list. A `match`
/// arm and a list entry are one fact written twice, so a source-text test below
/// holds them together -- the compiler cannot.
pub const NAMES: &[&str] = &[
    ":", "[", ".", "alias", "break", "cd", "command", "continue", "echo", "eval", "exec", "exit",
    "export", "false", "getopts", "jobs", "local", "printf", "pwd", "read", "readonly", "return", "set",
    "shift", "source", "test", "times", "trap", "true", "type", "umask", "unalias", "unset",
    "wait",
];

pub fn lookup(name: &str) -> Option<Builtin> {
    Some(match name {
        ":" => Builtin::Colon,
        "true" => Builtin::True,
        "false" => Builtin::False,
        "echo" => Builtin::Echo,
        "printf" => Builtin::Printf,
        "exit" => Builtin::Exit,
        "return" => Builtin::Return,
        "break" => Builtin::Break,
        "continue" => Builtin::Continue,
        "shift" => Builtin::Shift,
        "set" => Builtin::Set,
        "unset" => Builtin::Unset,
        "export" => Builtin::Export,
        "readonly" => Builtin::Readonly,
        "umask" => Builtin::Umask,
        "read" => Builtin::Read,
        "getopts" => Builtin::Getopts,
        "test" | "[" => Builtin::Test,
        "eval" => Builtin::Eval,
        "cd" => Builtin::Cd,
        "pwd" => Builtin::Pwd,
        // ash gives both spellings one implementation (`dotcmd`, ash.c:10181
        // and 10244); the word they were called by survives in argv[0] and is
        // all that differs in what they print.
        "." | "source" => Builtin::Dot,
        "times" => Builtin::Times,
        "command" => Builtin::Command,
        "wait" => Builtin::Wait,
        "jobs" => Builtin::Jobs,
        "alias" => Builtin::Alias,
        "unalias" => Builtin::Unalias,
        "exec" => Builtin::Exec,
        "trap" => Builtin::Trap,
        "local" => Builtin::Local,
        "type" => Builtin::Type,
        _ => return None,
    })
}

/// POSIX "special built-in" (2.14): a variable assignment prefixed on one persists
/// in the current environment, and (outside interactive use) an error aborts the
/// script. Regular builtins get transient prefix assignments instead.
pub fn is_special(bi: Builtin) -> bool {
    matches!(
        bi,
        Builtin::Colon
            | Builtin::Dot
            | Builtin::Break
            | Builtin::Exec
            | Builtin::Continue
            | Builtin::Eval
            | Builtin::Exit
            | Builtin::Export
            | Builtin::Readonly
            | Builtin::Return
            | Builtin::Set
            | Builtin::Shift
            | Builtin::Times
            | Builtin::Trap
            | Builtin::Unset
    )
}

/// ash's `spclbltin` (`IS_BUILTIN_SPECIAL`, ash.c:10419): the POSIX list above
/// plus `local`. ash decides two things from that one value -- whether a
/// redirection error is fatal (10484) and whether the prefix assignments get a
/// local frame (10420) -- so both call sites read it here rather than each
/// restating the exception.
pub fn is_ash_special(bi: Builtin) -> bool {
    is_special(bi) || matches!(bi, Builtin::Local)
}

/// Whether this command WORD is in ash's `spclbltin` set: the POSIX list above,
/// plus `local`, ash's own addition -- which is what keeps a top-level `local` an
/// error rather than a frame that authorises itself.
///
/// A plain lookup with nothing named by hand: every word ash marks special
/// resolves to a `Builtin` here, `times` and `source` last. A hand-named arm
/// would be a second source of truth able to keep a word special after its
/// `is_special` entry went, which is why there is none.
///
/// One bit in ash (`name[0] & 1`, ash.c:8205), of whose three consequences this
/// answers two: it blocks the local-var frame `evalcommand` pushes, and it is
/// what `type` reports as "special". The third -- a redirection failure being
/// FATAL rather than a skipped command -- is decided on the `Builtin` itself,
/// which every special word now has.
pub fn is_ash_special_word(word: &str) -> bool {
    lookup(word).is_some_and(is_ash_special)
}

pub fn run(sh: &mut Shell, bi: Builtin, argv: &[String]) -> R<()> {
    match bi {
        Builtin::Colon | Builtin::True => ok(sh),
        Builtin::False => status(sh, 1),
        Builtin::Echo => echo(sh, argv),
        Builtin::Printf => printf(sh, argv),
        Builtin::Exit => exit(sh, argv),
        Builtin::Return => ret(sh, argv),
        Builtin::Break => loop_ctl(sh, argv, true),
        Builtin::Continue => loop_ctl(sh, argv, false),
        Builtin::Shift => shift(sh, argv),
        Builtin::Set => set(sh, argv),
        Builtin::Unset => unset(sh, argv),
        Builtin::Export => export(sh, argv, false),
        Builtin::Readonly => export(sh, argv, true),
        Builtin::Umask => umask_builtin(sh, argv),
        Builtin::Local => local(sh, argv),
        Builtin::Read => read(sh, argv),
        Builtin::Getopts => getopts(sh, argv),
        Builtin::Test => test(sh, argv),
        Builtin::Eval => eval(sh, argv),
        Builtin::Cd => cd(sh, argv),
        Builtin::Pwd => pwd(sh, argv),
        Builtin::Dot => dot(sh, argv),
        Builtin::Times => times(sh),
        Builtin::Command => command(sh, argv),
        Builtin::Type => type_of(sh, argv),
        Builtin::Wait => wait(sh, argv),
        Builtin::Jobs => jobs(sh, argv),
        Builtin::Alias => alias(sh, argv),
        Builtin::Unalias => unalias(sh, argv),
        Builtin::Exec => exec_cmd(sh, argv),
        Builtin::Trap => trap(sh, argv),
    }
}

fn ok(sh: &mut Shell) -> R<()> {
    sh.set_status(0);
    Ok(())
}

/// `jobs [-p]` -- what this shell has running.
///
/// A SUBSHELL has a table of its own and so lists nothing, which is not an
/// omission but the whole of what the corpus grades: its `jobs | wc -l` case
/// expects 0 under dash, because dash forks its pipeline stages and the stage
/// running `jobs` inherits no jobs. td-sh clones per stage for the same reason,
/// so the 0 falls out rather than being arranged.
///
/// `-p` is ids alone, one per line. The rest is deliberately not served, and for
/// two different reasons. `-n`, `-r` and `-s` select by a state this shell does
/// not have -- nothing here is ever STOPPED, a thread having no `SIGTSTP` -- so
/// there is nothing to filter by. `-l` is the opposite: dash's long form ADDS
/// the process id to a listing that otherwise lacks it, and every line here
/// already carries the job's id because there is no pid to add later, so
/// serving it would be answering a question about a number this shell does not
/// have. A `%…` operand is a scope decision rather than a gap: `spec_id` could
/// resolve it, and selecting one job to print is simply not done yet. All take
/// the 2 dash gives.
fn jobs(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut ids_only = false;
    let mut rest = argv.iter().skip(1);
    for arg in rest.by_ref() {
        // `nextopt`'s asymmetry: `--` ends the options and is consumed, a lone
        // `-` ends them and is NOT, so it reaches the operand arm below.
        let Some(letters) = arg.strip_prefix('-').filter(|f| !f.is_empty()) else {
            // Named as an operand so the diagnostic says which mistake it was:
            // `jobs %1` is a jobspec, not a bad option.
            err_line(sh, &format!("jobs: {arg}: selecting a job is not supported"));
            return status(sh, 2);
        };
        if letters == "-" {
            break; // the arg was `--`
        }
        for c in letters.chars() {
            if c == 'p' {
                ids_only = true;
            } else {
                err_line(sh, &format!("jobs: illegal option -{c}"));
                return status(sh, 2);
            }
        }
    }
    if let Some(operand) = rest.next() {
        err_line(sh, &format!("jobs: {operand}: selecting a job is not supported"));
        return status(sh, 2);
    }
    let lines: Vec<String> = if ids_only {
        sh.jobs.ids().iter().map(u32::to_string).collect()
    } else {
        sh.jobs.list()
    };
    // One write, not one per job: a sibling stage sharing this descriptor would
    // otherwise be able to interleave between two lines of one listing.
    let mut text = Vec::new();
    for line in &lines {
        text.extend_from_slice(line.as_bytes());
        text.push(b'\n');
    }
    // `out` sets `$?` (0, or 1 on write error); do not overwrite it.
    out(sh, &text)
}

/// `wait [id...]` -- collect background jobs.
///
/// With no operands every job is collected and the status is 0 however they
/// ended, which is POSIX and is what the corpus's own "status is lost" comment
/// means. With operands each is collected in the order WRITTEN and the status is
/// the LAST one's, so `wait $a $b $c` answers for `$c` even when it finished
/// first.
///
/// A `%…` jobspec is served through the same table `jobs` prints, resolving to
/// an id before anything is joined -- so `wait %1` and `wait $!` cannot disagree
/// about what a job's status was. What is refused is `-n` (collect whichever
/// finishes next), which is bash's and is not a per-id question at all, and a
/// jobspec this shell cannot resolve: both take dash's 2, a usage error, rather
/// than an approximation graded as an answer. bash answers 127 for the second,
/// and the corpus records dash at 2 -- `wait %nonexistent` -- which is the
/// reference this shell follows.
fn wait(sh: &mut Shell, argv: &[String]) -> R<()> {
    let operands = match argv.get(1..) {
        None => &[][..],
        Some(rest) => rest,
    };
    if operands.is_empty() {
        sh.jobs.wait_all();
        return status(sh, 0);
    }
    // `nextopt` reads options from the FRONT only, so a word past the first
    // non-option is an operand however it is spelled: `wait -- -5` is a bad
    // NUMBER. A lone `-` ends them without being consumed, as it does above.
    let mut operands = operands;
    if let Some(letters) = operands
        .first()
        .and_then(|w| w.strip_prefix('-'))
        .filter(|f| !f.is_empty())
    {
        // Anything but `--`, and this builtin serves no option at all.
        if letters != "-" {
            let c = letters.chars().next().unwrap_or('-');
            err_line(sh, &format!("wait: illegal option -{c}"));
            return status(sh, 2);
        }
        operands = operands.get(1..).unwrap_or(&[]);
    }
    if operands.is_empty() {
        sh.jobs.wait_all();
        return status(sh, 0);
    }
    let mut last = 0;
    for operand in operands {
        // A `%…` jobspec resolves to an id, and only then joins -- so `wait %1`
        // and `wait $!` take exactly the same path and cannot disagree about
        // what a job's status was. A spelling this shell cannot resolve is the
        // usage error dash reports rather than a silent 127: `%foo` names a
        // COMMAND, and the command text is not kept.
        if operand.starts_with('%') {
            let Some(id) = sh.jobs.spec_id(operand) else {
                err_line(sh, &format!("wait: {operand}: no such job"));
                return status(sh, 2);
            };
            last = sh.jobs.wait_id(id).unwrap_or(127);
            continue;
        }
        // A pid is DIGITS, so this is not `str::parse` -- that accepts a leading
        // `+`/`-`, which would make `wait +5` a number and, past a `--`, `wait
        // -5` one too.
        if operand.is_empty() || !operand.bytes().all(|b| b.is_ascii_digit()) {
            err_line(sh, &format!("wait: Illegal number: {operand}"));
            return status(sh, 2);
        }
        // Not one of this shell's jobs -- including one already collected, which
        // bash also reports as 127. An id too large for `u32` cannot be one
        // either, and takes the same answer rather than a different diagnostic.
        //
        // It does NOT end the loop: POSIX makes the status that of the LAST
        // operand, so the ones after an unknown id are still waited for. bash
        // agrees -- `wait <unknown> $p` reports `$p`'s status and `wait $p
        // <unknown>` reports 127 -- where returning here waited for neither.
        last = operand.parse::<u32>().ok().and_then(|id| sh.jobs.wait_id(id)).unwrap_or(127);
    }
    status(sh, last)
}

fn status(sh: &mut Shell, code: i32) -> R<()> {
    sh.set_status(code);
    Ok(())
}

/// Write to stdout, setting `$?`: 0 on success, 1 on an I/O error (so a write to a
/// closed descriptor, `echo hi >&-`, fails visibly instead of silently
/// succeeding). Callers must NOT overwrite the status afterward.
fn out(sh: &mut Shell, bytes: &[u8]) -> R<()> {
    match write_fd(sh, 1, bytes) {
        Ok(()) => sh.set_status(0),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Err(sigpipe()),
        Err(_) => sh.set_status(1),
    }
    Ok(())
}

/// A broken pipe ENDS the writer rather than setting `$?` and carrying on.
///
/// The reader is gone, so every later write fails the same way and a producer
/// that only recorded it would spin forever: `while :; do echo x; done | head -1`
/// is the shape, and it is not hypothetical -- it hung before streaming
/// pipelines existed too, building an unbounded buffer instead of blocking on a
/// full pipe. dash and bash end the producer by DYING of SIGPIPE. This shell
/// installs no handler and cannot die of it — the Rust runtime ignores SIGPIPE
/// before `main`, and un-ignoring it would change every write in the crate, so
/// it is its own landing — but it can end the same way a CALLER observes:
/// status 141, POSIX's 128 + SIGPIPE.
///
/// `Sig::Exit` rather than a variant of its own, so it is confined to the
/// pipeline STAGE exactly as a forked producer's death would be: `head` still
/// reports the pipeline's status and the enclosing shell carries on. The one
/// visible difference from a real signal death is that an EXIT trap in the
/// producer still runs, where a signalled shell would never reach one.
fn sigpipe() -> Sig {
    Sig::Exit(128 + i32::from(SIGPIPE))
}

/// The signal whose number this shell reports but never sends or installs: the
/// Rust runtime ignores it before `main`, so td-sh cannot die of it and says so
/// in a status instead. Named for the same reason `SIGCHLD` is — a bare 13 in an
/// arithmetic expression is the one kind of wrong number nothing else catches.
const SIGPIPE: u8 = 13;

fn err_line(sh: &mut Shell, msg: &str) {
    let _ = exec::write_stderr(sh, msg);
}

fn echo(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut i = 1usize;
    let mut newline = true;
    let mut escapes = false;
    // busybox's option loop (coreutils/echo.c): a `-` word is options only if
    // EVERY letter is `n`, `e` or `E`; one bad letter -- or a bare `-`, or `--`
    // -- makes that word and everything after it an operand. Flags accumulate
    // across words. `E` is accepted and does NOTHING: it never clears the flag,
    // so `echo -e -E` still interprets. That is busybox, not bash.
    while let Some(arg) = argv.get(i) {
        let Some(letters) = arg.strip_prefix('-') else { break };
        if letters.is_empty() || !letters.chars().all(|c| matches!(c, 'n' | 'e' | 'E')) {
            break;
        }
        for c in letters.chars() {
            match c {
                'n' => newline = false,
                'e' => escapes = true,
                _ => {}
            }
        }
        i += 1;
    }
    // Bytes, not a String: an octal escape can name a byte that is not UTF-8.
    let mut line: Vec<u8> = Vec::new();
    let mut first = true;
    let mut stopped = false;
    for arg in argv.iter().skip(i) {
        if !first {
            line.push(b' ');
        }
        first = false;
        if !escapes {
            line.extend_from_slice(arg.as_bytes());
            continue;
        }
        if process_escapes(arg, &mut line) {
            // `\c` ends the output where it stands, newline included.
            stopped = true;
            break;
        }
    }
    if newline && !stopped {
        line.push(b'\n');
    }
    // Nothing to write is not a write: busybox's `full_write` returns before the
    // syscall on a zero count, so a closed stdout cannot fail `echo -e '\c'` or
    // `echo -n ''`. printf, which goes through stdio, still reports on flush.
    if line.is_empty() {
        sh.set_status(0);
        return Ok(());
    }
    // `out` sets `$?` (0, or 1 on write error); do not overwrite it.
    out(sh, &line)
}

/// POSIX `printf`: format directives with flags/width/precision (including `*`
/// width/precision from arguments), the integer conversions `d i o u x X` (C
/// base-0 parsing, the `'c` char-code form, ash's i64/u64 range rules), the
/// float conversions `f F e E g G` (C `strtod` operands, C-exact output), plus
/// `c`, `s`, `b`, and format-string backslash escapes -- `%b` and the format
/// string share `echo -e`'s converter, as they do in busybox. The format
/// cycles over the remaining arguments (POSIX).
/// Matched to the dash/ash goldens (spec/builtin-printf), not bash: no `-v`, and
/// `%q`/`%(..)T` are rejected like dash/ash.
fn printf(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut idx = 1usize;
    // dash/ash accept a single leading `--` as end-of-options (bash's `-v` is not
    // supported: a `-v` there is just a format string, matching ash).
    if argv.get(idx).map(String::as_str) == Some("--") {
        idx += 1;
    }
    let Some(format) = argv.get(idx) else {
        err_line(sh, "printf: usage: printf format [arguments]");
        return status(sh, 2);
    };
    let args: Vec<&str> = argv.iter().skip(idx + 1).map(String::as_str).collect();
    let mut out_buf: Vec<u8> = Vec::new();
    let mut st = Pf { ai: 0, error: false, stop: false, errors: Vec::new() };
    loop {
        let before = st.ai;
        format_once(format, &args, &mut out_buf, &mut st);
        if st.stop || st.ai >= args.len() || st.ai == before {
            break;
        }
    }
    for e in &st.errors {
        err_line(sh, e);
    }
    match write_fd(sh, 1, &out_buf) {
        Ok(()) => sh.set_status(if st.error { 1 } else { 0 }),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return Err(sigpipe()),
        Err(_) => sh.set_status(1),
    }
    Ok(())
}

// printf run state threaded through the format walk.
struct Pf {
    ai: usize,           // next argument index
    error: bool,         // a conversion/parse error occurred => exit status 1
    stop: bool,          // `\c`, from `%b` or the format string: stop ALL
    // further output, format cycling included
    errors: Vec<String>, // stderr lines, emitted once after the walk
}

#[derive(Default, Clone, Copy)]
struct Flags {
    minus: bool,
    plus: bool,
    space: bool,
    hash: bool,
    zero: bool,
}

// Parsed field spec of one directive; `left` folds in a negative `*` width.
struct Spec {
    flags: Flags,
    width: usize,
    left: bool,
    prec: Option<i64>,
}

fn format_once(format: &str, args: &[&str], out: &mut Vec<u8>, st: &mut Pf) {
    let chars: Vec<char> = format.chars().collect();
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        match c {
            '\\' => {
                // The format string handles `\c` itself, BEFORE the converter,
                // and abandons the whole run rather than just this cycle
                // (printf.c:391). It also has no `\0` marker, unlike `%b`.
                if chars.get(i + 1) == Some(&'c') {
                    st.stop = true;
                    return;
                }
                let (byte, next) = echo_escape(&chars, i + 1);
                out.push(byte);
                i = next;
            }
            '%' => {
                i = conversion(&chars, i + 1, args, out, st);
                if st.stop {
                    return;
                }
            }
            other => {
                push_char(out, other);
                i += 1;
            }
        }
    }
}

// Parse and emit one `%` directive; `start` points just past the `%`. Returns
// the index following the directive.
// Width/precision are clamped to this bound before use. Defensive, not cosmetic:
// Rust's float formatter panics at precision >= 65536, and an unbounded width or
// integer precision would drive a multi-gigabyte allocation that aborts under
// panic=abort. No real format approaches this (the corpus max is ~10).
const MAX_FIELD: usize = 65535;

fn conversion(chars: &[char], start: usize, args: &[&str], out: &mut Vec<u8>, st: &mut Pf) -> usize {
    let mut i = start;
    if chars.get(i) == Some(&'%') {
        out.push(b'%');
        return i + 1;
    }
    let mut flags = Flags::default();
    loop {
        match chars.get(i) {
            Some('-') => flags.minus = true,
            Some('+') => flags.plus = true,
            Some(' ') => flags.space = true,
            Some('#') => flags.hash = true,
            Some('0') => flags.zero = true,
            _ => break,
        }
        i += 1;
    }
    let mut left = flags.minus;
    // Width: a decimal literal, or `*` taken from an argument (a negative `*`
    // width means left-justify with its magnitude).
    let width: usize;
    if chars.get(i) == Some(&'*') {
        let w = take_int(args, st);
        if w < 0 {
            left = true;
            width = w.unsigned_abs() as usize;
        } else {
            width = w as usize;
        }
        i += 1;
    } else {
        let (w, ni) = read_dec(chars, i);
        width = w as usize;
        i = ni;
    }
    // Precision: `.` then a decimal (bare `.` is 0) or `*` (a negative `.*` is
    // taken as omitted).
    let mut prec: Option<i64> = None;
    if chars.get(i) == Some(&'.') {
        i += 1;
        if chars.get(i) == Some(&'*') {
            let p = take_int(args, st);
            prec = if p < 0 { None } else { Some(p) };
            i += 1;
        } else {
            let (p, ni) = read_dec(chars, i);
            prec = Some(p);
            i = ni;
        }
    }
    let conv = match chars.get(i) {
        Some(&c) => c,
        None => {
            // Format ended mid-directive: emit the literal `%` and the modifiers
            // already consumed rather than swallowing them.
            out.push(b'%');
            for &c in chars.get(start..i).unwrap_or(&[]) {
                push_char(out, c);
            }
            return i;
        }
    };
    i += 1;
    let width = width.min(MAX_FIELD);
    let prec = prec.map(|p| if p < 0 { p } else { p.min(MAX_FIELD as i64) });
    let spec = Spec { flags, width, left, prec };
    match conv {
        's' => emit_str(out, args, st, &spec),
        'c' => emit_char(out, args, st, &spec),
        'b' => emit_b(out, args, st, &spec),
        'd' | 'i' | 'o' | 'u' | 'x' | 'X' => emit_int(out, args, st, conv, &spec),
        'f' | 'F' | 'e' | 'E' | 'g' | 'G' => emit_float(out, args, st, conv, &spec),
        _ => {
            // Unsupported directive. %q is a bash extension dash/ash reject;
            // %(..)T is not implemented yet (it needs strftime — a follow-up).
            // Both are handled the ash way: keep the prefix already emitted,
            // stop, exit status 1.
            st.errors.push(format!("printf: %{conv}: invalid directive"));
            st.error = true;
            st.stop = true;
        }
    }
    i
}

fn emit_str(out: &mut Vec<u8>, args: &[&str], st: &mut Pf, spec: &Spec) {
    let raw = take_arg(args, &mut st.ai).unwrap_or("");
    let bytes = raw.as_bytes();
    let body: &[u8] = match spec.prec {
        Some(p) if p >= 0 => bytes.get(0..p as usize).unwrap_or(bytes),
        _ => bytes,
    };
    pad_bytes(out, &[], &[], body, spec.width, spec.left, false);
}

fn emit_char(out: &mut Vec<u8>, args: &[&str], st: &mut Pf, spec: &Spec) {
    let raw = take_arg(args, &mut st.ai).unwrap_or("");
    // ash's %c takes the first BYTE, not the first code point; an empty argument
    // yields a NUL byte.
    let body: &[u8] = raw.as_bytes().get(0..1).unwrap_or(b"\0");
    pad_bytes(out, &[], &[], body, spec.width, spec.left, false);
}

fn emit_b(out: &mut Vec<u8>, args: &[&str], st: &mut Pf, spec: &Spec) {
    let raw = take_arg(args, &mut st.ai).unwrap_or("");
    let mut body: Vec<u8> = Vec::new();
    let hit_c = process_escapes(raw, &mut body);
    let shown: &[u8] = match spec.prec {
        Some(p) if p >= 0 => body.get(0..p as usize).unwrap_or(&body),
        _ => &body,
    };
    pad_bytes(out, &[], &[], shown, spec.width, spec.left, false);
    if hit_c {
        st.stop = true;
    }
}

fn emit_int(out: &mut Vec<u8>, args: &[&str], st: &mut Pf, conv: char, spec: &Spec) {
    let value: i128 = match take_arg(args, &mut st.ai) {
        None => 0, // an absent argument is a silent zero (POSIX)
        Some(raw) => match parse_int_arg(raw) {
            Some(v) if range_ok(v, conv) => v,
            _ => {
                st.errors.push(format!("printf: {raw}: expected a numeric value"));
                st.error = true;
                0
            }
        },
    };
    let flags = spec.flags;
    let mut sign: Vec<u8> = Vec::new();
    let mut mag: String = match conv {
        'd' | 'i' => {
            if value < 0 {
                sign.push(b'-');
            } else if flags.plus {
                sign.push(b'+');
            } else if flags.space {
                sign.push(b' ');
            }
            value.unsigned_abs().to_string()
        }
        'u' => as_u64(value).to_string(),
        'o' => format!("{:o}", as_u64(value)),
        'x' => format!("{:x}", as_u64(value)),
        'X' => format!("{:X}", as_u64(value)),
        _ => String::new(),
    };
    // Precision sets a minimum digit count; `.0` applied to 0 yields no digits.
    if let Some(p) = spec.prec {
        let p = if p < 0 { 0 } else { p as usize };
        if p == 0 && value == 0 {
            mag.clear();
        } else if mag.len() < p {
            let mut z = "0".repeat(p - mag.len());
            z.push_str(&mag);
            mag = z;
        }
    }
    // `#`: octal gains a leading 0, a non-zero hex gains a 0x/0X prefix.
    let mut prefix: Vec<u8> = Vec::new();
    if flags.hash {
        match conv {
            'o' if !mag.starts_with('0') => {
                let mut m = String::from("0");
                m.push_str(&mag);
                mag = m;
            }
            'x' if value != 0 => prefix.extend_from_slice(b"0x"),
            'X' if value != 0 => prefix.extend_from_slice(b"0X"),
            _ => {}
        }
    }
    // The 0 flag is ignored when a precision is given, or when left-justifying.
    let zero = flags.zero && !spec.left && spec.prec.is_none();
    pad_bytes(out, &sign, &prefix, mag.as_bytes(), spec.width, spec.left, zero);
}

fn emit_float(out: &mut Vec<u8>, args: &[&str], st: &mut Pf, conv: char, spec: &Spec) {
    let raw = take_arg(args, &mut st.ai).unwrap_or("");
    // The float path takes no `'c` char-code form, unlike the integer one: a
    // leading quote is just an unconvertible operand. No corpus golden pins this,
    // and the FreeBSD printf lineage dash descends from does apply the quote form
    // to floats too, so it is worth re-checking against dash/busybox source.
    let num = strtod(raw);
    // dash's check_conversion: an unconverted tail, or an out-of-range value, is
    // an error — but the (partially converted) value is still printed. An operand
    // absent altogether, or empty, converts to 0 with no complaint.
    if num.consumed < raw.len() {
        let why = if num.consumed == 0 { "expected a numeric value" } else { "not completely converted" };
        st.errors.push(format!("printf: {raw}: {why}"));
        st.error = true;
    } else if num.erange {
        st.errors.push(format!("printf: {raw}: Numerical result out of range"));
        st.error = true;
    }
    let value = num.value;
    let prec = match spec.prec {
        Some(p) if p >= 0 => p as usize,
        _ => 6,
    };
    let flags = spec.flags;
    let upper = matches!(conv, 'F' | 'E' | 'G');
    let mut sign: Vec<u8> = Vec::new();
    if value.is_sign_negative() {
        sign.push(b'-');
    } else if flags.plus {
        sign.push(b'+');
    } else if flags.space {
        sign.push(b' ');
    }
    let mag = value.abs();
    // C spells these "inf"/"nan" (uppercased by an uppercase conversion) and
    // ignores both the precision and the 0 flag for them.
    let (body, numeric) = if mag.is_nan() {
        (String::from(if upper { "NAN" } else { "nan" }), false)
    } else if mag.is_infinite() {
        (String::from(if upper { "INF" } else { "inf" }), false)
    } else {
        let s = match conv {
            'e' | 'E' => fmt_e(mag, prec, upper, flags.hash),
            'g' | 'G' => fmt_g(mag, prec, upper, flags.hash),
            _ => fmt_f(mag, prec, flags.hash),
        };
        (s, true)
    };
    // Unlike integers, a precision does NOT disable the 0 flag for floats.
    let zero = flags.zero && !spec.left && numeric;
    pad_bytes(out, &sign, &[], body.as_bytes(), spec.width, spec.left, zero);
}

// C `%f`: `prec` fraction digits, and `#` keeps the point that `.0` would drop.
// Rust's fixed formatting is correctly rounded (ties to even) like glibc's.
fn fmt_f(mag: f64, prec: usize, hash: bool) -> String {
    let mut s = format!("{:.*}", prec, mag);
    if hash && prec == 0 {
        s.push('.');
    }
    s
}

// C `%e`: one digit, `prec` fraction digits, then `e±dd` (at least two exponent
// digits). Rust rounds the same way but spells the exponent as "3.14e0".
fn fmt_e(mag: f64, prec: usize, upper: bool, hash: bool) -> String {
    let (digits, exp) = exp_digits(mag, prec);
    let mut s = String::new();
    s.push_str(digits.get(0..1).unwrap_or("0"));
    let frac = digits.get(1..).unwrap_or("");
    if !frac.is_empty() || hash {
        s.push('.');
    }
    s.push_str(frac);
    push_exp(&mut s, exp, upper);
    s
}

// C `%g`: `prec` significant digits (0 means 1); style `e` when the exponent is
// below -4 or at least the precision, else style `f`; without `#`, trailing
// fraction zeros (and a bare point) are dropped. Both styles are built from the
// ONE rounded digit string so they cannot round differently.
fn fmt_g(mag: f64, prec: usize, upper: bool, hash: bool) -> String {
    let p = prec.max(1);
    let (digits, exp) = exp_digits(mag, p - 1);
    if exp < -4 || exp >= p as i32 {
        let mut s = String::new();
        s.push_str(digits.get(0..1).unwrap_or("0"));
        let frac = digits.get(1..).unwrap_or("");
        let frac = if hash {
            // glibc quirk, reproduced because dash/ash print THROUGH glibc: when
            // rounding carries the exponent out of style f's range, glibc keeps
            // style f's fraction count (always 0 there) instead of style e's, so
            // `%#.6g` of 999999.5 is `1.e+06`, not C's `1.00000e+06`. Only a value
            // style f would have taken UNROUNDED carries like that; one already in
            // style e (exponent below -4) is formatted normally.
            if (-4..p as i32).contains(&decimal_exponent(mag)) { "" } else { frac }
        } else {
            frac.trim_end_matches('0')
        };
        if !frac.is_empty() || hash {
            s.push('.');
        }
        s.push_str(frac);
        push_exp(&mut s, exp, upper);
        s
    } else {
        let mut s = fixed_from_digits(&digits, exp);
        if hash {
            if !s.contains('.') {
                s.push('.');
            }
        } else if s.contains('.') {
            s = s.trim_end_matches('0').trim_end_matches('.').to_string();
        }
        s
    }
}

// The `prec + 1` correctly rounded significant digits of `mag` and its decimal
// exponent, read back out of Rust's exponential form ("d.ddde<exp>"). `mag` is
// finite and non-negative, so the shape is fixed; the fallbacks only keep a
// malformed read from panicking.
// `prec` MUST stay <= MAX_FIELD: Rust's formatter panics at 65536, so the clamp
// in `conversion()` is what keeps this call safe, with nothing to spare.
fn exp_digits(mag: f64, prec: usize) -> (String, i32) {
    let s = format!("{:.*e}", prec, mag);
    let (mant, e) = match s.split_once('e') {
        Some(parts) => parts,
        None => (s.as_str(), "0"),
    };
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    (digits, e.parse::<i32>().unwrap_or(0))
}

// The decimal exponent `mag` has BEFORE any rounding to a precision. Rust's
// shortest round-trip form carries the true one (it cannot round up into the
// next decade, since that would need the value to be that decade already).
fn decimal_exponent(mag: f64) -> i32 {
    let s = format!("{mag:e}");
    match s.split_once('e') {
        Some((_, e)) => e.parse::<i32>().unwrap_or(0),
        None => 0,
    }
}

// C's exponent suffix: sign always, at least two digits.
fn push_exp(s: &mut String, exp: i32, upper: bool) {
    s.push(if upper { 'E' } else { 'e' });
    s.push(if exp < 0 { '-' } else { '+' });
    let a = exp.unsigned_abs();
    if a < 10 {
        s.push('0');
    }
    s.push_str(&a.to_string());
}

// Lay significant `digits` out as plain fixed-point with the decimal exponent
// `exp` (the value is `0.digits * 10^(exp+1)`). Callers guarantee `exp` is small
// enough that the string stays bounded by the field clamp.
fn fixed_from_digits(digits: &str, exp: i32) -> String {
    let mut s = String::new();
    if exp < 0 {
        s.push_str("0.");
        for _ in 0..(-exp - 1) {
            s.push('0');
        }
        s.push_str(digits);
        return s;
    }
    let split = (exp as usize).saturating_add(1).min(digits.len());
    s.push_str(digits.get(0..split).unwrap_or(""));
    let frac = digits.get(split..).unwrap_or("");
    if !frac.is_empty() {
        s.push('.');
        s.push_str(frac);
    }
    s
}

// The result of C's `strtod` — the conversion dash's `getdouble` and busybox-ash's
// float path both delegate to: the value, how many bytes it consumed (0 == no
// conversion at all), and whether it over/underflowed (C's ERANGE).
struct Num {
    value: f64,
    consumed: usize,
    erange: bool,
}

// C `strtod`: optional whitespace and sign, then an `inf`/`nan` spelling, a C99
// hex float, or a decimal float. Stops at the first byte that cannot extend the
// number, so `"1 "` converts 1.0 AND reports a tail (dash's status 1).
fn strtod(s: &str) -> Num {
    let none = Num { value: 0.0, consumed: 0, erange: false };
    let b = s.as_bytes();
    let mut i = 0usize;
    while matches!(b.get(i), Some(&c) if c == b' ' || (0x09..=0x0d).contains(&c)) {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(&b'-') => {
            i += 1;
            true
        }
        Some(&b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let Some((mag, end, erange)) =
        parse_inf_nan(b, i).or_else(|| parse_hex_float(b, i)).or_else(|| parse_dec_float(b, i))
    else {
        return none;
    };
    Num { value: if neg { -mag } else { mag }, consumed: end, erange }
}

// `inf`/`infinity`/`nan`/`nan(chars)`, case-insensitive. A longer word that only
// starts with one of them keeps just the prefix ("infinit" converts "inf").
fn parse_inf_nan(b: &[u8], i: usize) -> Option<(f64, usize, bool)> {
    if word_at(b, i, b"infinity") {
        return Some((f64::INFINITY, i + 8, false));
    }
    if word_at(b, i, b"inf") {
        return Some((f64::INFINITY, i + 3, false));
    }
    if !word_at(b, i, b"nan") {
        return None;
    }
    let mut j = i + 3;
    if b.get(j) == Some(&b'(') {
        let mut k = j + 1;
        while matches!(b.get(k), Some(&c) if c.is_ascii_alphanumeric() || c == b'_') {
            k += 1;
        }
        if b.get(k) == Some(&b')') {
            j = k + 1;
        }
    }
    Some((f64::NAN, j, false))
}

fn word_at(b: &[u8], i: usize, word: &[u8]) -> bool {
    word.iter().enumerate().all(|(k, w)| b.get(i + k).is_some_and(|c| c.eq_ignore_ascii_case(w)))
}

// C99 hex float `0x<hex digits>[.<hex digits>][p[±]<decimal digits>]`. Without a
// hex digit the `0x` is not a prefix at all, so `"0x"` falls through to the
// decimal parse and converts just the `0` (as glibc does).
fn parse_hex_float(b: &[u8], i: usize) -> Option<(f64, usize, bool)> {
    if b.get(i) != Some(&b'0') || !matches!(b.get(i + 1), Some(&c) if c == b'x' || c == b'X') {
        return None;
    }
    let mut j = i + 2;
    let mut mant: u128 = 0;
    let mut dropped: i64 = 0; // significand digits past what `mant` can hold
    let mut nfrac: i64 = 0;
    let mut sticky = false;
    let mut any = false;
    let mut after_point = false;
    while let Some(&c) = b.get(j) {
        let d = match c {
            b'.' if !after_point => {
                after_point = true;
                j += 1;
                continue;
            }
            _ => match digit_val(c as char) {
                Some(d) if d < 16 => d,
                _ => break,
            },
        };
        any = true;
        if after_point {
            nfrac += 1;
        }
        // Absorb while there is room; beyond it only the fact that a dropped
        // digit was non-zero matters (the sticky bit for rounding).
        if mant < (1u128 << 124) {
            mant = (mant << 4) | d as u128;
        } else {
            if d != 0 {
                sticky = true;
            }
            dropped += 1;
        }
        j += 1;
    }
    if !any {
        return None;
    }
    let mut pexp: i64 = 0;
    if matches!(b.get(j), Some(&c) if c == b'p' || c == b'P') {
        let mut k = j + 1;
        let eneg = match b.get(k) {
            Some(&b'-') => {
                k += 1;
                true
            }
            Some(&b'+') => {
                k += 1;
                false
            }
            _ => false,
        };
        let mut digits = 0usize;
        let mut v: i64 = 0;
        while let Some(&c) = b.get(k) {
            if !c.is_ascii_digit() {
                break;
            }
            v = v.saturating_mul(10).saturating_add((c - b'0') as i64);
            digits += 1;
            k += 1;
        }
        // A `p` with no digits is not part of the number (glibc leaves it).
        if digits > 0 {
            pexp = if eneg { -v } else { v };
            j = k;
        }
    }
    let e = dropped.saturating_sub(nfrac).saturating_mul(4).saturating_add(pexp);
    let (mag, erange) = scale_pow2(mant, e, sticky);
    Some((mag, j, erange))
}

// Round `mant * 2^e` (with `sticky` recording significand bits already dropped)
// to the nearest f64, ties to even, and report C's ERANGE.
fn scale_pow2(mant: u128, e: i64, sticky: bool) -> (f64, bool) {
    if mant == 0 {
        return (0.0, false);
    }
    let mut m = mant;
    let mut lost = sticky;
    // Beyond this the result can only overflow or underflow, and the clamp keeps
    // every shift below in range.
    let mut exp = e.clamp(-(1 << 20), 1 << 20);
    let mut bits = 128 - i64::from(m.leading_zeros());
    // Reduce to 64 significant bits first so the rounding shift stays small.
    if bits > 64 {
        let sh = bits - 64;
        if m & ((1u128 << sh) - 1) != 0 {
            lost = true;
        }
        m >>= sh;
        exp += sh;
        bits = 64;
    }
    // Underflow is judged on the value rounded to a 53-bit significand with an
    // UNBOUNDED exponent, not on the exact value: IEEE leaves the choice open and
    // glibc detects tininess after rounding. So `0x0.fffffffffffffcp-1022` is in
    // range (it rounds up to 2^-1022) while `0x1.fffffffffffffp-1023` is not,
    // even though both land on the smallest normal.
    let (tm, texp, _) = round_shift(m, exp, lost, bits - 53);
    let tiny = texp + (128 - i64::from(tm.leading_zeros())) - 1 < -1022;
    // Discard down to a 53-bit significand, or further once the subnormal floor
    // (a quantum of 2^-1074) is the binding constraint.
    let drop = (bits - 53).max(-1074 - exp);
    let (m, exp, lost) = round_shift(m, exp, lost, drop);
    if m == 0 {
        return (0.0, true); // rounded away below the quantum
    }
    let bits = 128 - i64::from(m.leading_zeros());
    let msb = exp + bits - 1; // value == 1.f * 2^msb
    if msb > 1023 {
        return (f64::INFINITY, true);
    }
    let raw = if msb < -1022 {
        // Subnormal: `drop` left the quantum at 2^-1074, so rescaling `m` to it
        // gives the IEEE encoding directly (`msb < -1022` keeps it under 2^52).
        ((m << (exp + 1074).clamp(0, 52)) & ((1u128 << 52) - 1)) as u64
    } else {
        let shift = 53 - bits;
        let m53 = if shift >= 0 { m << shift } else { m >> shift.unsigned_abs() };
        (((msb + 1023) as u64) << 52) | ((m53 as u64) & ((1u64 << 52) - 1))
    };
    (f64::from_bits(raw), tiny && lost)
}

// Shift `m` right by `d` bits, rounding to nearest with ties to even. `lost`
// carries the sticky bit of everything already dropped in, and back out.
fn round_shift(m: u128, exp: i64, lost: bool, d: i64) -> (u128, i64, bool) {
    if d <= 0 {
        return (m, exp, lost);
    }
    if d >= 128 {
        // Even the rounding bit sits below the target quantum.
        return (0, exp + d, true);
    }
    let rem = m & ((1u128 << d) - 1);
    let half = 1u128 << (d - 1);
    let mut q = m >> d;
    if rem > half || (rem == half && (lost || q & 1 == 1)) {
        q += 1;
    }
    (q, exp + d, lost || rem != 0)
}

// A decimal float: digits with an optional point and an optional `e` exponent.
// The scanned prefix is handed to Rust's parser, which is correctly rounded and
// accepts exactly these forms.
fn parse_dec_float(b: &[u8], i: usize) -> Option<(f64, usize, bool)> {
    let mut j = i;
    let mut digits = 0usize;
    let mut nonzero = false;
    let mut after_point = false;
    while let Some(&c) = b.get(j) {
        if c == b'.' && !after_point {
            after_point = true;
        } else if c.is_ascii_digit() {
            digits += 1;
            nonzero |= c != b'0';
        } else {
            break;
        }
        j += 1;
    }
    if digits == 0 {
        return None;
    }
    if matches!(b.get(j), Some(&c) if c == b'e' || c == b'E') {
        let mut k = j + 1;
        if matches!(b.get(k), Some(&c) if c == b'-' || c == b'+') {
            k += 1;
        }
        let mut n = 0usize;
        while matches!(b.get(k), Some(&c) if c.is_ascii_digit()) {
            k += 1;
            n += 1;
        }
        // An `e` with no digits is not part of the number.
        if n > 0 {
            j = k;
        }
    }
    let text = std::str::from_utf8(b.get(i..j)?).ok()?;
    let mag = text.parse::<f64>().ok()?;
    // C's ERANGE. Rust's parser reports neither inexactness nor tininess, so a
    // subnormal result stands in for both. That differs from glibc only for
    // operands no one can write by hand: an EXACT subnormal (needs >=751
    // significant digits, e.g. 5^1074 e-1074, which glibc leaves in range), and
    // the one-2^-1075-wide window below 2^-1022 that rounds up to it (glibc
    // reports those out of range). The hex path decides both exactly.
    let erange = mag.is_infinite() || (nonzero && (mag == 0.0 || mag < f64::MIN_POSITIVE));
    Some((mag, j, erange))
}

fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

fn take_arg<'a>(args: &[&'a str], ai: &mut usize) -> Option<&'a str> {
    let a = args.get(*ai).copied();
    if a.is_some() {
        *ai += 1;
    }
    a
}

fn take_int(args: &[&str], st: &mut Pf) -> i64 {
    match take_arg(args, &mut st.ai) {
        None => 0,
        Some(raw) => match parse_int_arg(raw) {
            Some(v) if v >= i64::MIN as i128 && v <= i64::MAX as i128 => v as i64,
            _ => {
                st.errors.push(format!("printf: {raw}: expected a numeric value"));
                st.error = true;
                0
            }
        },
    }
}

fn read_dec(chars: &[char], start: usize) -> (i64, usize) {
    let mut v: i64 = 0;
    let mut j = start;
    while let Some(&c) = chars.get(j) {
        if c.is_ascii_digit() {
            v = v.saturating_mul(10).saturating_add((c as i64) - ('0' as i64));
            j += 1;
        } else {
            break;
        }
    }
    (v, j)
}

/// One escape, from `bb_process_escape_sequence`
/// (libbb/process_escape_sequence.c). `i` points just PAST the backslash.
/// Returns the byte and the index to resume at; an unrecognised sequence returns
/// a literal backslash and the SAME index, so the offending char is emitted next
/// as ordinary text.
///
/// Every backslash td-sh expands now comes through here, as in busybox, where
/// `echo -e`, `printf %b` and printf's format string all call this. It knows
/// `\e`, takes `\xHH` (two digits, where `\x` with no digit is literal), accepts
/// an octal run without the leading `0`, and stops a digit run at the first
/// digit that would carry the value past a byte -- leaving that digit as text,
/// which is why `\0400` is a space followed by `0`.
fn echo_escape(chars: &[char], i: usize) -> (u8, usize) {
    let start = i;
    let mut i = i;
    let mut base = 8u32;
    let mut digits = 0u32;
    if chars.get(i) == Some(&'x') {
        i += 1;
        base = 16;
        digits = 1; // so hex takes two digits where octal takes three
    }
    let mut n = 0u32;
    while digits < 3 {
        let Some(d) = chars.get(i).and_then(|c| c.to_digit(base)) else {
            if base == 16 {
                // Cannot underflow: the hex branch seeds `digits` at 1.
                digits -= 1;
                if digits == 0 {
                    return (b'\\', start); // `\x` with no hex digit: literal
                }
            }
            break;
        };
        let r = n * base + d;
        if r > u32::from(u8::MAX) {
            break;
        }
        n = r;
        i += 1;
        digits += 1;
    }
    if digits > 0 {
        return (n as u8, i);
    }
    let byte = match chars.get(i) {
        Some('a') => 0x07,
        Some('b') => 0x08,
        Some('e') => 0x1b,
        Some('f') => 0x0c,
        Some('n') => b'\n',
        Some('r') => b'\r',
        Some('t') => b'\t',
        Some('v') => 0x0b,
        Some('\\') => b'\\',
        _ => return (b'\\', i),
    };
    (byte, i + 1)
}

/// Run one `echo -e` operand or `printf %b` argument into `out`. One function
/// because busybox has one: echo.c's loop and printf.c's `print_esc_string` are
/// the same code down to the `\0` marker. True when `\c` cut the output short.
fn process_escapes(raw: &str, out: &mut Vec<u8>) -> bool {
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        if c != '\\' {
            push_char(out, c);
            i += 1;
            continue;
        }
        i += 1;
        if chars.get(i) == Some(&'c') {
            return true;
        }
        // `\0` before an octal digit is a marker, not a digit: echo.c drops it so
        // `\0101` reaches the converter as `101`.
        if chars.get(i) == Some(&'0') && chars.get(i + 1).is_some_and(|c| ('0'..='7').contains(c)) {
            i += 1;
        }
        let (byte, next) = echo_escape(&chars, i);
        out.push(byte);
        i = next;
    }
    false
}

// The `'c`/`"c` printf char-code form: a leading `'` or `"` makes the value the
// first BYTE after the quote (0 if nothing follows). Shared by the integer and
// float directives.
fn quote_char_code(s: &str) -> Option<i128> {
    let bytes = s.as_bytes();
    match bytes.first() {
        Some(&f) if f == b'\'' || f == b'"' => Some(bytes.get(1).map(|&b| b as i128).unwrap_or(0)),
        _ => None,
    }
}

// Parse a printf integer argument the way ash does: the `'c`/`"c` char-code
// form (first BYTE after the quote); otherwise leading whitespace is skipped,
// a C base-0 integer is read (0x hex, 0 octal, else decimal), and ANY trailing
// character (a stray digit-looking char OR trailing space) makes it invalid.
// `None` signals an error (the caller uses 0 and sets exit status 1).
fn parse_int_arg(s: &str) -> Option<i128> {
    if let Some(cc) = quote_char_code(s) {
        return Some(cc);
    }
    let trimmed = s.trim_start_matches([' ', '\t']);
    let tb: Vec<char> = trimmed.chars().collect();
    let mut idx = 0usize;
    let mut neg = false;
    match tb.get(idx) {
        Some('+') => idx += 1,
        Some('-') => {
            neg = true;
            idx += 1;
        }
        _ => {}
    }
    let (base, start) = detect_base(&tb, idx);
    let mut val: i128 = 0;
    let mut any = false;
    let mut j = start;
    while let Some(&c) = tb.get(j) {
        match digit_val(c) {
            Some(d) if (d as u32) < base => {
                val = val.checked_mul(base as i128)?.checked_add(d as i128)?;
                any = true;
                j += 1;
            }
            _ => break,
        }
    }
    if !any || j != tb.len() {
        return None;
    }
    Some(if neg { -val } else { val })
}

fn detect_base(tb: &[char], idx: usize) -> (u32, usize) {
    if tb.get(idx) == Some(&'0') {
        match tb.get(idx + 1) {
            Some('x') | Some('X') => (16, idx + 2),
            _ => (8, idx), // the leading 0 is itself an octal digit ("0" == 0)
        }
    } else {
        (10, idx)
    }
}

fn digit_val(c: char) -> Option<u8> {
    match c {
        '0'..='9' => Some(c as u8 - b'0'),
        'a'..='f' => Some(c as u8 - b'a' + 10),
        'A'..='F' => Some(c as u8 - b'A' + 10),
        _ => None,
    }
}

// Reinterpret a value known to be in [i64::MIN, u64::MAX] as u64: negatives wrap
// via two's complement (ash prints `%u` of -42 as 18446744073709551574).
fn as_u64(v: i128) -> u64 {
    if v < 0 {
        (v as i64) as u64
    } else {
        v as u64
    }
}

fn range_ok(v: i128, conv: char) -> bool {
    match conv {
        'd' | 'i' => v >= i64::MIN as i128 && v <= i64::MAX as i128,
        _ => v >= i64::MIN as i128 && v <= u64::MAX as i128,
    }
}

// Apply a field width to `sign + prefix + body`: space- or zero-pad on the left,
// or space-pad on the right when left-justifying. Zero padding lands between the
// sign/prefix and the body.
fn pad_bytes(out: &mut Vec<u8>, sign: &[u8], prefix: &[u8], body: &[u8], width: usize, left: bool, zero: bool) {
    let len = sign.len() + prefix.len() + body.len();
    let pad = width.saturating_sub(len);
    if left {
        out.extend_from_slice(sign);
        out.extend_from_slice(prefix);
        out.extend_from_slice(body);
        for _ in 0..pad {
            out.push(b' ');
        }
    } else if zero {
        out.extend_from_slice(sign);
        out.extend_from_slice(prefix);
        for _ in 0..pad {
            out.push(b'0');
        }
        out.extend_from_slice(body);
    } else {
        for _ in 0..pad {
            out.push(b' ');
        }
        out.extend_from_slice(sign);
        out.extend_from_slice(prefix);
        out.extend_from_slice(body);
    }
}

fn exit(sh: &mut Shell, argv: &[String]) -> R<()> {
    let code = match argv.get(1) {
        Some(s) => number(sh, argv, s)?,
        // POSIX: inside a trap action, "the last command" is the one that ran
        // before the trap, so a bare `exit` there reports the status the shell was
        // already exiting with -- not whatever the action's own last command left.
        None => sh.trap_status.unwrap_or(sh.status),
    };
    // No mask of its own: `set_status` and `main` are where a status narrows.
    Err(Sig::Exit(code))
}

fn ret(sh: &mut Shell, argv: &[String]) -> R<()> {
    let code = match argv.get(1) {
        Some(s) => number(sh, argv, s)?,
        None => sh.status,
    };
    Err(Sig::Return(code))
}

/// busybox ash's `is_number`: every character a digit, so a sign or a space
/// disqualifies. dash's `atomax10` takes `+1` and surrounding blanks; the chain
/// puts ash first, so the stricter reading wins.
fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// The operand rule `exit`, `return`, `break` and `shift` share: ash's
/// `is_number` digit test plus dash's `n > INT_MAX` bound, because ash has no
/// bound and `atoi`-overflows instead. Separate from the reporting half only
/// because `shift` wants the same rule with a different fatality.
fn parse_number(s: &str) -> Option<i32> {
    s.parse::<i32>().ok().filter(|_| is_all_digits(s))
}

fn number(sh: &mut Shell, argv: &[String], s: &str) -> R<i32> {
    parse_number(s).ok_or_else(|| badnum(sh, argv, s))
}

fn badnum(sh: &mut Shell, argv: &[String], s: &str) -> Sig {
    let cmd = argv.first().map(String::as_str).unwrap_or_default();
    err_line(sh, &format!("{cmd}: Illegal number: {s}"));
    sh.set_status(2);
    Sig::Abort(2)
}

fn loop_ctl(sh: &mut Shell, argv: &[String], is_break: bool) -> R<()> {
    let n = match argv.get(1) {
        // `breakcmd` runs the operand through `number` and THEN rejects a
        // non-positive count, so `break 0` reports the same way `break oops` does.
        Some(s) => match parse_number(s).filter(|n| *n > 0).and_then(|n| u32::try_from(n).ok()) {
            Some(n) => n,
            None => return Err(badnum(sh, argv, s)),
        },
        None => 1,
    };
    if sh.loop_depth == 0 {
        // Not in a loop: a no-op, as POSIX leaves it unspecified and dash warns.
        return ok(sh);
    }
    sh.set_status(0);
    if is_break {
        Err(Sig::Break(n))
    } else {
        Err(Sig::Continue(n))
    }
}

fn shift(sh: &mut Shell, argv: &[String]) -> R<()> {
    let n = match argv.get(1) {
        // The same `number()` rejection as `break`'s, INT_MAX bound included, so
        // `shift 2147483648` is a bad number and not a huge count.
        Some(s) => match parse_number(s).and_then(|n| usize::try_from(n).ok()) {
            Some(n) => n,
            None => return special_usage_error(sh, &format!("shift: Illegal number: {s}")),
        },
        None => 1,
    };
    // NOT fatal, unlike the bad-operand case above: busybox ash just `return 1`s
    // when the count overruns the parameters, and builtin-special.test.sh pins
    // ash's behaviour (`## N-I …ash`) over dash's abort.
    if n > sh.params.len() {
        err_line(sh, "shift: shift count out of range");
        return status(sh, 1);
    }
    sh.params.drain(0..n);
    // Shifting renumbers the arguments the cursor indexes, so dash restarts it.
    sh.getopts_optind = 1;
    sh.getopts_off = -1;
    ok(sh)
}

fn set(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut i = 1usize;
    let mut saw_ddash = false;
    while let Some(arg) = argv.get(i) {
        if arg == "--" {
            i += 1;
            saw_ddash = true;
            break;
        }
        // In the BUILTIN (not on the command line) a lone `-` turns -x and -v off
        // and ends option processing, so `set - -e` leaves `-e` as $1.
        if arg == "-" {
            sh.opts.xtrace = false;
            sh.opts.verbose = false;
            i += 1;
            break;
        }
        let (sign, rest) = match arg.strip_prefix('-') {
            Some(r) => (true, r),
            None => match arg.strip_prefix('+') {
                Some(r) => (false, r),
                None => break,
            },
        };
        let flag = if sign { '-' } else { '+' };
        // dash reads a cluster letter by letter, and EACH `o` in one consumes the
        // next argument as its name -- `set -eo nounset` is errexit plus nounset,
        // and `set -oo a b` applies both.
        let mut names = 0usize;
        for c in rest.chars() {
            if c == 'o' {
                // Bare `-o`/`+o` print the settings in dash (as a reusable `set`
                // command for `+o`). td-sh has neither listing, so a missing name
                // stays the no-op it was rather than becoming an error.
                if let Some(name) = argv.get(i + 1 + names) {
                    if !apply_named_option(sh, name, sign) {
                        err_line(sh, &format!("set: illegal option {flag}o {name}"));
                        return status(sh, 1);
                    }
                    names += 1;
                }
                continue;
            }
            if !apply_option_letter(sh, c, sign) {
                return special_usage_error(sh, &format!("set: illegal option {flag}{c}"));
            }
        }
        i += 1 + names;
    }
    // `--` present: the operands after it REPLACE the positional params, and an
    // empty operand list clears them (`set --`). Otherwise operands present without
    // `--` replace them, while a pure option change leaves them untouched.
    if saw_ddash || argv.get(i).is_some() {
        sh.params = argv.iter().skip(i).cloned().collect();
        // New positional parameters restart the `getopts` scan, as in dash --
        // otherwise a second option loop would resume at the old cursor. Only the
        // hidden cursor moves: dash leaves the OPTIND *variable* untouched until
        // the next `getopts` writes it (and touching it would break `readonly`).
        sh.getopts_optind = 1;
        sh.getopts_off = -1;
    }
    ok(sh)
}

/// One option letter. False means neither reference shell has it, so the caller
/// reports it rather than ignoring it. The command line reads this same table,
/// as dash's does, so the two cannot drift apart.
pub fn apply_option_letter(sh: &mut Shell, c: char, on: bool) -> bool {
    match c {
        'e' => sh.opts.errexit = on,
        'u' => sh.opts.nounset = on,
        'x' => sh.opts.xtrace = on,
        'f' => sh.opts.noglob = on,
        'v' => sh.opts.verbose = on,
        'C' => sh.opts.noclobber = on,
        'n' => sh.opts.noexec = on,
        'a' => sh.opts.allexport = on,
        's' => sh.opts.stdin = on,
        // The UNION of the two reference tables, since accepting a mode this
        // shell lacks never breaks a script while refusing one does: `V` is
        // dash's alone (ash spells vi with no letter) and `c` is ash's alone.
        'I' | 'i' | 'm' | 'b' | 'V' | 'E' | 'c' => {}
        _ => return false,
    }
    true
}

/// The `-o` names, split the same way as the letters. `pipefail` is the one
/// refusal worth naming: ash has it under BASH_PIPEFAIL, but accepting it as a
/// no-op would silently give every guarded pipeline the wrong status.
pub fn apply_named_option(sh: &mut Shell, name: &str, on: bool) -> bool {
    match name {
        "errexit" => sh.opts.errexit = on,
        "nounset" => sh.opts.nounset = on,
        "xtrace" => sh.opts.xtrace = on,
        "noglob" => sh.opts.noglob = on,
        "verbose" => sh.opts.verbose = on,
        "noclobber" => sh.opts.noclobber = on,
        "noexec" => sh.opts.noexec = on,
        "allexport" => sh.opts.allexport = on,
        "stdin" => sh.opts.stdin = on,
        "ignoreeof" | "interactive" | "monitor" | "vi" | "emacs" | "notify" | "nolog"
        | "debug" | "errtrace" => {}
        _ => return false,
    }
    true
}

/// Is the named `-o` option ON? The reader for `[[ -o name ]]`, which is why it
/// lives beside the writer: the two must agree about the names, and a name the
/// setter accepts as a no-op is one this has to answer for rather than call
/// unknown. An unrecognised name is FALSE, not an error, as bash's is.
pub fn named_option_is_set(sh: &Shell, name: &str) -> bool {
    match name {
        "errexit" => sh.opts.errexit,
        "nounset" => sh.opts.nounset,
        "xtrace" => sh.opts.xtrace,
        "noglob" => sh.opts.noglob,
        "verbose" => sh.opts.verbose,
        "noclobber" => sh.opts.noclobber,
        "noexec" => sh.opts.noexec,
        "allexport" => sh.opts.allexport,
        "stdin" => sh.opts.stdin,
        // The names the setter takes and ignores: off, because this shell has
        // no such behaviour to report as on.
        _ => false,
    }
}

/// A usage error in a SPECIAL builtin, and the ONE route for them: POSIX 2.8.1
/// ends a non-interactive shell on one, while dash's sh_error returns an
/// interactive one to its prompt. Reaching for `Shell::fatal` here instead would
/// silently drop that second half.
fn special_usage_error(sh: &mut Shell, msg: &str) -> R<()> {
    err_line(sh, msg);
    sh.set_status(2);
    Err(Sig::Abort(2))
}

fn unset(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut i = 1usize;
    // `nextopt("vf")`: flags may be clustered in one word, `--` ends them, and a
    // bare `-` is an operand rather than an option. The caller loop keeps the LAST
    // flag seen (`while ((i = nextopt("vf")) != '\0') flag = i;`), so `-fv` means
    // `-v`. No flag at all means the variable and nothing else.
    let mut mode = None;
    while let Some(arg) = argv.get(i) {
        let Some(flags) = arg.strip_prefix('-') else {
            break;
        };
        if flags.is_empty() {
            break;
        }
        i += 1;
        if flags == "-" {
            break;
        }
        for c in flags.chars() {
            match c {
                'f' | 'v' => mode = Some(c),
                _ => return special_usage_error(sh, &format!("unset: illegal option -{c}")),
            }
        }
    }
    while let Some(arg) = argv.get(i) {
        i += 1;
        if mode == Some('f') {
            sh.funcs.remove(arg);
            continue;
        }
        // dash unsets through setvar, which takes the name up to an `=` and
        // rejects only what it cannot parse as one -- so `unset b=c` unsets `b`,
        // but `unset 'a[1]'` is an error. `unset` is a special builtin, so that
        // error is fatal to a non-interactive shell rather than a status.
        let end = arg.bytes().position(|b| b == b'=').unwrap_or(arg.len());
        let name = arg.get(..end).unwrap_or("");
        if !ast::is_name(name) {
            return special_usage_error(sh, &format!("unset: {arg}: bad variable name"));
        }
        // No flag means the VARIABLE and nothing else: both references gate the
        // function on `-f` alone (`if (flag != 'f') { unsetvar(*ap); continue; }`,
        // the same lines in ash.c:14207 and dash's var.c:595). POSIX leaves the
        // no-variable case unspecified and bash unsets the function there; the
        // chain does not, so neither does this.
        //
        // A LOCALISED name keeps its entry, exactly as a bare `local` leaves one:
        // ash frees the `struct var` only when the flags come to exactly `VUNSET`
        // (ash.c:2440), which `mklocal`'s `VSTRFIXED` rules out. So `local a; unset
        // a; a=X` still exports where a global `unset a` takes the attribute away.
        // `set -a` is the other term in that test and `unset_var` answers it.
        let gone = if sh.is_local(name) {
            sh.unset_value(name)
        } else {
            sh.unset_var(name)
        };
        if !gone {
            // dash unsets through setvar too, so a readonly name aborts the shell
            // rather than reporting a status.
            return special_usage_error(sh, &format!("unset: {name}: is read only"));
        }
    }
    ok(sh)
}

fn export(sh: &mut Shell, argv: &[String], readonly: bool) -> R<()> {
    let cmd = if readonly { "readonly" } else { "export" };
    let mut any = false;
    let mut options = true;
    // Set for `readonly` too, which ignores it: its arms never read this
    // (ash.c:14143-14145).
    let mut unexport = false;
    for arg in argv.iter().skip(1) {
        if options && arg == "--" {
            options = false;
            continue;
        }
        // ash reads the cluster letter by letter and an unknown one is fatal, so
        // `export -px` is a usage error here. dash instead calls `nextopt` ONCE
        // and never looks at the rest, so `-px` lists and exits 0; this follows
        // ash.
        if options && arg.len() > 1 && arg.starts_with('-') {
            // A plain loop, not a search combinator: this source is embedded
            // verbatim in the td-sh recipe, whose ladder guard rejects that tool's
            // bare name as a token (see the note in arith.rs).
            let mut bad = None;
            for c in arg.chars().skip(1) {
                match c {
                    // `-p` is accepted and does NOTHING.
                    'p' => {}
                    'n' => unexport = true,
                    _ => {
                        bad = Some(c);
                        break;
                    }
                }
            }
            if let Some(c) = bad {
                return special_usage_error(sh, &format!("{cmd}: illegal option -{c}"));
            }
            continue;
        }
        options = false;
        any = true;
        match arg.split_once('=') {
            Some((name, value)) => {
                if !ast::is_name(name) {
                    return special_usage_error(sh, &format!("{cmd}: {name}: bad variable name"));
                }
                sh.set_var(name, value)?;
                if readonly {
                    sh.set_readonly(name);
                } else if !unexport {
                    sh.export(name);
                }
                // No `unexport` arm: an assignment goes through `setvar`
                // (ash.c:14164), which preserves the flags already there
                // (ash.c:2449), so it only declines to ADD the export.
            }
            None => {
                if !ast::is_name(arg) {
                    return special_usage_error(sh, &format!("{cmd}: {arg}: bad variable name"));
                }
                if readonly {
                    sh.set_readonly(arg);
                } else if unexport {
                    sh.unexport(arg);
                } else {
                    sh.export(arg);
                }
            }
        }
    }
    if any {
        return ok(sh);
    }
    // No operands: list. ash returns the moment it has one, so `export -p NAME`
    // prints nothing at all -- its own source notes bash differs there.
    let mut listed: Vec<(&str, Option<&str>)> = sh
        .vars
        .iter()
        .filter(|(_, v)| if readonly { v.readonly } else { v.exported })
        .map(|(n, v)| (n.as_str(), v.value.as_deref()))
        .collect();
    listed.sort_by(|a, b| a.0.cmp(b.0));
    let mut text = String::new();
    for (name, value) in &listed {
        // ash prints only the part of the name that IS a name and then drops the
        // value (`endofname`, ash.c:11580), so an environment entry like
        // `test-test=v` cannot turn the listing into something `eval` chokes on.
        // A valueless name prints bare for the same reason: `eval` must restore
        // the attribute without inventing a value the name does not have.
        // Bytes, not chars: this is an index into `name` below, and a char count
        // only happens to be one while every accepted char is ASCII.
        let end = match name.as_bytes().first() {
            Some(&b) if b == b'_' || b.is_ascii_alphabetic() => name
                .bytes()
                .position(|b| b != b'_' && !b.is_ascii_alphanumeric())
                .unwrap_or(name.len()),
            _ => 0,
        };
        text.push_str(cmd);
        text.push(' ');
        text.push_str(name.get(..end).unwrap_or(""));
        if let Some(v) = value.filter(|_| end == name.len()) {
            text.push('=');
            text.push_str(&single_quote(v));
        }
        text.push('\n');
    }
    if text.is_empty() {
        // ash reaches `out1fmt` zero times and returns 0. Writing an empty buffer
        // instead would report the write error a CLOSED stdout gives back, so
        // `readonly -p >&-` with nothing readonly would fail where ash succeeds.
        return ok(sh);
    }
    out(sh, text.as_bytes())
}

/// `local [name[=value]... | -]`, dash's. Not POSIX, but dash, busybox ash and
/// bash all have it and scripts are written to it, so a `/bin/sh` without one
/// cannot run them.
///
/// Each name's current binding is saved for the function's return to restore.
/// A bare `local x` starts the name UNSET whatever it held before, which is ash's
/// rule and not dash's -- see the match below. `unset` on a local then leaves it
/// unset for the rest of the call instead of revealing the global, because the
/// outer binding only comes back on unwind.
fn local(sh: &mut Shell, argv: &[String]) -> R<()> {
    if sh.localvar_depth == 0 {
        return special_usage_error(sh, "local: not in a function");
    }
    // Only the FIRST save of a name matters -- the frame unwinds newest-first, so
    // a later one is overwritten by it. Re-saving would let `while …; do local
    // x=$i; done` grow the frame without bound for the length of the call.
    for arg in argv.iter().skip(1) {
        if arg == "-" {
            if !sh.locals.iter().any(|l| matches!(l, Local::Opts(_))) {
                sh.locals.push(Local::Opts(sh.opts));
            }
            continue;
        }
        let (name, value) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (arg.as_str(), None),
        };
        if !ast::is_name(name) {
            // Fatal, as every other `local` error is: dash reaches this through
            // setvar, which aborts the shell. (dash only validates the valueless
            // form -- `local 1bad=x` slips past setvareq into a name no expansion
            // can name again. td-sh rejects both rather than store that.)
            return special_usage_error(sh, &format!("local: {name}: bad variable name"));
        }
        // First declaration of this name in this frame, which is the only one that
        // does anything: ash walks the frame and skips a name already in it. A
        // frame a terminating unwind DEFERRED counts as well -- an EXIT trap runs
        // inside the frame the shell died in, so a `local` there is a repeat.
        // THIS frame, not `is_local`: a name an OUTER frame declared is still
        // fresh here, and must be saved again so each return restores its own.
        let declared = |l: &Local| matches!(l, Local::Var(n, _) if n == name);
        let deferred = sh.pending_unwind.get(sh.pending_floor..).unwrap_or(&[]);
        let fresh = !sh.locals.iter().any(declared) && !deferred.iter().any(declared);
        if fresh {
            sh.locals
                .push(Local::Var(name.to_string(), sh.vars.get(name).cloned()));
            // After the save, so the restore puts back the flag as it was.
            sh.mark_local(name);
        }
        // Readonly is answered here for BOTH forms rather than left to `set_var`,
        // so each names the builtin as both references do; `set_var`'s own message
        // is shared with plain assignment, which must NOT grow a `local:` prefix.
        let rejected = match value {
            // `local x=v` keeps the attributes of the name it shadows, since
            // set_var writes through the existing entry.
            Some(v) => {
                let readonly = sh.vars.get(name).is_some_and(|old| old.readonly);
                if !readonly {
                    sh.set_var(name, v)?;
                }
                readonly
            }
            // A valueless `local x` UNSETS x -- ash's `mklocal` says so in as many
            // words ("local VAR unsets VAR") and dash, alone, leaves the outer value
            // showing through instead. The repeat declaration is what ash skips, so
            // `local x=1; local x` is still 1.
            None if fresh => !sh.unset_value(name),
            None => false,
        };
        if rejected {
            return special_usage_error(sh, &format!("local: {name}: is read only"));
        }
    }
    ok(sh)
}

/// POSIX `getopts optstring name [arg...]`, matched to dash. Two details drive
/// the shape: `OPTIND` is the 1-based index of the next WORD, so it advances when
/// a clustered word is ENTERED rather than once per letter (`-ab` reports
/// `OPTIND=2` for both `a` and `b`); and the offset inside that word is shell
/// state, not a variable. A leading `:` in `optstring` selects silent mode, which
/// reports a bad option or a missing argument through `OPTARG` instead of stderr.
fn getopts(sh: &mut Shell, argv: &[String]) -> R<()> {
    let (Some(optstring), Some(name)) = (argv.get(1), argv.get(2)) else {
        err_line(sh, "getopts: usage: getopts optstring var [arg...]");
        return status(sh, 2);
    };
    let args: Vec<String> = if argv.len() > 3 {
        argv.iter().skip(3).cloned().collect()
    } else {
        sh.params.clone()
    };
    // dash treats an OPTIND that is not a positive integer as fatal, rather than
    // quietly restarting the scan; ash ignores it (`is_number` guards its
    // `number()` call), which is a divergence of its own and not this one.
    let mut optind = sh.getopts_optind;
    if optind < 1 {
        let shown = sh.get_var("OPTIND").unwrap_or_default();
        err_line(sh, &format!("getopts: Illegal number: {shown}"));
        sh.set_status(2);
        return Err(Sig::Abort(2));
    }
    let mut off = sh.getopts_off;
    // A scan left past the end of a now-shorter argument list starts over.
    if optind > args.len() as i64 + 1 {
        optind = 1;
        off = -1;
    }

    // Continue inside the word already being consumed, else enter the next one.
    let inword = if off >= 1 && optind >= 2 {
        args.get((optind - 2) as usize).filter(|w| (w.len() as i64) > off).cloned()
    } else {
        None
    };
    let (word, at) = match inword {
        Some(w) => (w, off as usize),
        None => match args.get((optind - 1) as usize) {
            // `-` alone and a non-option word both end the scan without being
            // consumed; `--` ends it and IS consumed.
            Some(w) if w.starts_with('-') && w.len() > 1 => {
                optind += 1;
                if w == "--" {
                    return getopts_done(sh, name, optind);
                }
                (w.clone(), 1usize)
            }
            _ => return getopts_done(sh, name, optind),
        },
    };

    let silent = optstring.starts_with(':');
    let c = match word.as_bytes().get(at) {
        Some(&b) => b as char,
        None => return getopts_done(sh, name, optind),
    };
    let at = at + 1;
    let takes_arg = getopts_lookup(optstring, c);
    let mut letter = c;
    match takes_arg {
        None => {
            if silent {
                getopts_write(sh, "OPTARG", &c.to_string())?;
            } else {
                err_line(sh, &format!("Illegal option -{c}"));
                getopts_unset_optarg(sh)?;
            }
            letter = '?';
            off = if at >= word.len() { -1 } else { at as i64 };
        }
        Some(false) => {
            getopts_write(sh, "OPTARG", "")?;
            off = if at >= word.len() { -1 } else { at as i64 };
        }
        Some(true) => {
            off = -1;
            if at < word.len() {
                let tail = String::from_utf8_lossy(word.as_bytes().get(at..).unwrap_or(&[])).into_owned();
                getopts_write(sh, "OPTARG", &tail)?;
            } else if let Some(a) = args.get((optind - 1) as usize).cloned() {
                getopts_write(sh, "OPTARG", &a)?;
                optind += 1;
            } else if silent {
                getopts_write(sh, "OPTARG", &c.to_string())?;
                letter = ':';
            } else {
                // Bare and capitalised for the same reason its neighbour is:
                // `fprintf` to stderr (ash.c:11736), not a shell diagnostic.
                err_line(sh, &format!("No arg for -{c} option"));
                getopts_unset_optarg(sh)?;
                letter = '?';
            }
        }
    }
    // After `getopts_store`, whose OPTIND write resets the cursor via the hook.
    getopts_store(sh, name, optind, &letter.to_string())?;
    sh.getopts_optind = optind;
    sh.getopts_off = off;
    // An unusable variable name still leaves OPTIND/OPTARG updated (dash does the
    // parse first, then fails on the assignment).
    if !ast::is_name(name) {
        err_line(sh, &format!("getopts: {name}: not a valid identifier"));
        return status(sh, 2);
    }
    ok(sh)
}

// End of options: report `?` through `name` and stop. ash unsets OPTARG here
// too (ash.c:11692, the `atend` label).
fn getopts_done(sh: &mut Shell, name: &str, optind: i64) -> R<()> {
    getopts_unset_optarg(sh)?;
    getopts_store(sh, name, optind, "?")?;
    sh.getopts_optind = optind;
    sh.getopts_off = -1;
    status(sh, 1)
}

/// Any `getopts` write that can be REFUSED, which is every one of them once a
/// regular builtin's error stops killing the shell.
///
/// ash parks `shellparam.optind = -1` on entry (ash.c:11680) and puts the real
/// cursor back only after the last write lands (ash.c:11757), so a refusal
/// leaves the scan restartable: the next call reads that sentinel through
/// `(unsigned)optind > nparam + 1` (ash.c:11777) and starts at word 1. td-sh
/// has no unsigned reinterpretation to lean on, so it spells the restart out.
fn getopts_write(sh: &mut Shell, name: &str, value: &str) -> R<()> {
    let r = sh.set_var(name, value);
    if r.is_err() {
        sh.getopts_optind = 1;
        sh.getopts_off = -1;
    }
    r
}

/// The three `OPTARG` unsets, refusable for the same reason: ash's `unsetvar`
/// IS `setvar(s, NULL, 0)` (ash.c:2525), so a readonly OPTARG raises there
/// exactly as an assignment would.
fn getopts_unset_optarg(sh: &mut Shell) -> R<()> {
    if sh.unset_var("OPTARG") {
        return Ok(());
    }
    sh.getopts_optind = 1;
    sh.getopts_off = -1;
    // `unset_var` reports refusal as a bool, so this is the one refusal whose
    // message is not `set_var`'s own; it borrows the wording rather than
    // repeating it.
    Err(sh.readonly_fatal("OPTARG"))
}

// Publish OPTIND and the option letter, skipping a name that is not an identifier.
fn getopts_store(sh: &mut Shell, name: &str, optind: i64, letter: &str) -> R<()> {
    getopts_write(sh, "OPTIND", &optind.to_string())?;
    if ast::is_name(name) {
        getopts_write(sh, name, letter)?;
    }
    Ok(())
}

// Whether `c` is in `optstring`, and if so whether it takes an argument. The
// leading silent-mode `:` is NOT skipped, so like dash it can itself be matched.
fn getopts_lookup(optstring: &str, c: char) -> Option<bool> {
    let b = optstring.as_bytes();
    let mut k = 0usize;
    while let Some(&s) = b.get(k) {
        let takes = b.get(k + 1) == Some(&b':');
        if s as char == c {
            return Some(takes);
        }
        k += if takes { 2 } else { 1 };
    }
    None
}

/// ash's `read`: busybox's `shell_builtin_read` (shell/shell_common.c), driven by
/// `readcmd`'s `nextopt("p:u:rt:n:sd:")` (ash.c:14297). Options cluster and a
/// value-taking letter swallows the rest of its word (`-rn1`) or the next one.
/// Where dash has only `-r`/`-p` and insists on a variable name, this takes
/// `-n -s -t -u -d` and, given NO names, sets `REPLY` unsplit and untrimmed. An
/// unknown letter is status 2, which does NOT end the shell -- `read` is not a
/// special builtin.
fn read(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut i = 1usize;
    let mut raw = false;
    let mut silent = false;
    let mut opt_p: Option<String> = None;
    let mut opt_u: Option<String> = None;
    let mut opt_t: Option<String> = None;
    let mut opt_n: Option<String> = None;
    let mut opt_d: Option<String> = None;
    while let Some(arg) = argv.get(i) {
        let bytes = arg.as_bytes();
        if bytes.first() != Some(&b'-') || bytes.len() < 2 {
            break;
        }
        if arg == "--" {
            i += 1;
            break;
        }
        i += 1;
        let mut k = 1usize;
        while let Some(&c) = bytes.get(k) {
            k += 1;
            // A value-taking letter swallows the rest of its word, else the next
            // word -- which is why `read -rn1 v` and `read -r -n 1 v` agree, and
            // why `-d ''` reaches us as an empty string rather than a missing arg.
            let value = if matches!(c, b'p' | b'u' | b't' | b'n' | b'd') {
                let rest = arg.get(k..).unwrap_or("");
                k = bytes.len();
                if rest.is_empty() {
                    match argv.get(i) {
                        Some(next) => {
                            i += 1;
                            Some(next.clone())
                        }
                        None => {
                            // `nextopt`'s second message, and lowercase like its
                            // first (ash.c:2045).
                            err_line(sh, &format!("read: no arg for -{} option", c as char));
                            return status(sh, 2);
                        }
                    }
                } else {
                    Some(rest.to_string())
                }
            } else {
                None
            };
            match c {
                b'r' => raw = true,
                b's' => silent = true,
                b'p' => opt_p = value,
                b'u' => opt_u = value,
                b't' => opt_t = value,
                b'n' => opt_n = value,
                b'd' => opt_d = value,
                _ => {
                    err_line(sh, &format!("read: illegal option -{}", c as char));
                    return status(sh, 2);
                }
            }
        }
    }

    let names: Vec<String> = argv.iter().skip(i).cloned().collect();
    // Every name is checked BEFORE anything is read, and a bad one is status 1
    // with the name quoted -- ash copies bash's message here, and it is NOT the
    // usage 2 that a bad option gets.
    for name in &names {
        if !ast::is_name(name) {
            err_line(sh, &format!("read: '{name}': bad variable name"));
            return status(sh, 1);
        }
    }

    // `bb_strtou`: decimal digits and nothing else -- no sign, no space, no
    // trailing junk. `-n 0` is "no limit", as bash 3.2 also has it.
    let nchars = match &opt_n {
        Some(s) => match bb_strtoi(s) {
            Some(v) => i64::from(v),
            None => {
                err_line(sh, "read: invalid count");
                return status(sh, 2);
            }
        },
        None => 0,
    };
    let timeout = match &opt_t {
        Some(s) => match parse_read_timeout(s) {
            Some(ms) => Some(ms),
            None => {
                err_line(sh, "read: invalid timeout");
                return status(sh, 2);
            }
        },
        None => None,
    };
    let fd: u32 = match &opt_u {
        Some(s) => match bb_strtoi(s) {
            Some(v) => v.unsigned_abs(),
            None => {
                err_line(sh, "read: invalid file descriptor");
                return status(sh, 2);
            }
        },
        None => 0,
    };

    // `-t 0` asks whether a read would block and reads NOTHING, so it answers
    // FIRST and does nothing else on the way: no prompt for a line it will not
    // read, and no refusal of `-s`/`-n`, which describe how a read behaves and
    // so cannot be wrong about one that never happens. bash reports 1 for
    // `read -t 0 -s v` on a terminal, and printing a prompt there would leave
    // `P` on the screen with nothing reading after it. That case needs a
    // terminal to see, which is why no test below reaches it -- the same gap
    // the `-s`/`-n` refusal itself has.
    if timeout == Some(0) {
        return match read_ready_or_fail(sh, fd, 0) {
            Err(code) => status(sh, code),
            Ok(true) => ok(sh),
            Ok(false) => status(sh, 1),
        };
    }

    // The prompt goes to stderr, and ONLY when the source is a terminal. Noted
    // like every other fd-2 writer: `while :; do read -p 'P> ' v; done` with a
    // broken stderr is the same spin the rest of them had, and the terminal
    // gate is why no headless test can reach it.
    //
    // BEFORE the wait below, as ash orders it: a prompt printed after the poll
    // is a `read -t 5 -p 'P> '` that shows nothing for five seconds and then
    // either prompts for a line it has already read or -- on a timeout --
    // never prompts at all.
    if let Some(p) = &opt_p {
        if sh.fds.is_terminal(fd) {
            let _ = exec::note_epipe(sh, write_fd(sh, 2, p.as_bytes()));
        }
    }

    // `-s` (no echo) and `-n` (return without a newline) are both termios work,
    // which td-sh has no syscall surface for. On anything but a terminal they need
    // none and the byte semantics below are complete; ON one they cannot be
    // honoured, and failing loudly beats the two silent wrong answers -- echoing a
    // password, or waiting for the Enter the caller was told not to need.
    // Refused before the wait for the same reason the prompt is printed before
    // it: an option this shell will not honour should say so at once, not after
    // the caller's timeout has run.
    if sh.fds.is_terminal(fd) && (silent || nchars > 0) {
        let what = if silent { "-s" } else { "-n" };
        err_line(sh, &format!("read: {what}: needs termios on a terminal"));
        return status(sh, 2);
    }

    // A nonzero `-t` is `-t 0`'s question carried across the WHOLE read, and
    // `poll(2)` answers both -- it is the reason that syscall is on this surface
    // at all. Before it, neither could be served on the descriptors that can
    // actually block and both were refused; a pipe, a FIFO, a socket and an idle
    // terminal are exactly the cases `read -t` exists for.
    //
    // ONE ABSOLUTE DEADLINE, not one wait: ash computes `end_ms` once and polls
    // with what is left of it before EVERY byte. A single poll before the loop
    // would bound only the FIRST byte, so a writer that sends a partial line and
    // stops -- `printf ab` down a pipe it keeps open -- would leave `read -t 1`
    // blocked forever, having reported nothing and waited past the deadline it
    // was given.
    let deadline = match timeout {
        // Answered above; the arm is here so the match stays exhaustive over
        // what `timeout` can hold rather than resting on a `_`.
        Some(0) => None,
        // SATURATING, not a cast: `poll`'s timeout is an `int` and a NEGATIVE one
        // means wait forever, so a `-t` big enough to wrap would turn the one
        // thing this option exists to bound into an unbounded block. `i32::MAX`
        // ms is about 24 days, which is past any real deadline.
        Some(ms) => Some(
            std::time::Instant::now()
                + std::time::Duration::from_millis(ms.min(u64::from(i32::MAX.unsigned_abs()))),
        ),
        None => None,
    };
    let ifs = crate::expand::ifs_value(sh);
    let ifs_bytes: Vec<u8> = ifs.as_bytes().to_vec();
    let is_ifs = |c: u8| ifs_bytes.contains(&c);
    // C's `isspace` in the C locale, which is what `shell_builtin_read` uses to
    // tell a whitespace IFS char from a delimiting one.
    let is_space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r');
    // `-d ''` leaves `opt_d[0]` at the terminator, so the delimiter is NUL -- and
    // because the NUL skip below comes AFTER the delimiter test, that works.
    let delim: u8 = match &opt_d {
        Some(d) => d.as_bytes().first().copied().unwrap_or(0),
        None => b'\n',
    };

    let mut buffer: Vec<u8> = Vec::new();
    let mut startword = 1i32;
    let mut backslash = false;
    let mut vi = 0usize;
    let mut code = 0i32;
    // `-n 0` is no limit at all, so it is the absence of one rather than a count.
    let mut remaining: Option<i64> = if nchars > 0 { Some(nchars) } else { None };
    // What one input byte does. `Stop` is the C `break` -- it does NOT tick the
    // `-n` count -- while `Skip` is its `continue`, which does.
    enum Step {
        Put,
        Skip,
        Stop,
    }
    loop {
        // What is LEFT of the deadline, asked again for each byte. Zero means it
        // has already passed, and the poll is SKIPPED rather than made with a
        // zero timeout: poll spells that "ask and return at once", so a writer
        // streaming faster than the reader would answer ready every time and the
        // deadline would never bite -- `read -t 1` on an endless line with no
        // delimiter never returns. The short-circuit is what makes the deadline
        // a bound rather than a hint.
        //
        // ROUNDED UP, because the deadline is in nanoseconds and poll counts
        // whole milliseconds: truncating turns the last fraction of a
        // millisecond into "already passed", which made `read -t 0.001` report a
        // timeout for a line that was already in the pipe.
        if let Some(end) = deadline {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let left_ms = i32::try_from(left.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);
            let ready = if left_ms == 0 {
                Ok(false)
            } else {
                read_ready_or_fail(sh, fd, left_ms)
            };
            match ready {
                // Named apart from the loop's own `code`, which is the status a
                // completed read reports rather than a diagnostic's.
                Err(usage) => return status(sh, usage),
                // Timed out. ash reports 1 and returns from HERE, so whatever
                // followed the last field delimiter is dropped -- unlike the EOF
                // below, which assigns it. Fields the loop already completed
                // were assigned as it went and stand: `IFS=: read -t 1 a b` on
                // `x:y` leaves `a` set and `b` empty. bash differs on both
                // halves, reporting 142 and assigning the whole partial line.
                // The bytes taken off the descriptor are gone either way, which
                // is inherent to reading one at a time.
                Ok(false) => return status(sh, 1),
                Ok(true) => {}
            }
        }
        let c = match process::read_byte(sh, fd) {
            Ok(Some(b)) => b,
            Ok(None) => {
                // EOF still assigns what was read; only a timeout would not.
                code = 1;
                break;
            }
            // A read error is SILENT, as in ash: the descriptor being unreadable
            // (`-u` naming a closed one) reports only through the status.
            Err(_) => {
                code = 1;
                break;
            }
        };
        let step = 'step: {
            if !raw {
                if backslash {
                    backslash = false;
                    // Backslash-newline is a continuation: both bytes vanish.
                    // Anything else is taken literally, which is why an escaped
                    // delimiter or IFS char does not end a field.
                    break 'step if c == b'\n' { Step::Skip } else { Step::Put };
                }
                if c == b'\\' {
                    backslash = true;
                    break 'step Step::Skip;
                }
            }
            if c == delim {
                break 'step Step::Stop;
            }
            if c == 0 {
                break 'step Step::Skip;
            }
            // Splitting happens ONLY with names: `read` and `read REPLY` differ.
            if !names.is_empty() {
                let ifs_here = is_ifs(c);
                if startword > 0 && ifs_here {
                    if is_space(c) {
                        break 'step Step::Skip;
                    }
                    // A non-space IFS char: the first one after a field still
                    // separates, a second one starts an empty field.
                    startword -= 1;
                    if startword == 1 {
                        break 'step Step::Skip;
                    }
                }
                startword = 0;
                if vi + 1 < names.len() && ifs_here {
                    let value = String::from_utf8_lossy(&buffer).into_owned();
                    buffer.clear();
                    if let Some(name) = names.get(vi) {
                        sh.set_var(name, &value)?;
                    }
                    vi += 1;
                    startword = if is_space(c) { 2 } else { 1 };
                    break 'step Step::Skip;
                }
            }
            Step::Put
        };
        match step {
            Step::Stop => break,
            Step::Put => buffer.push(c),
            Step::Skip => {}
        }
        if let Some(left) = remaining.as_mut() {
            *left -= 1;
            if *left == 0 {
                break;
            }
        }
    }
    let _ = silent; // -s only suppresses terminal echo, which needs no termios here

    if names.is_empty() {
        // No names, no IFS removal of any kind.
        let value = String::from_utf8_lossy(&buffer).into_owned();
        sh.set_var("REPLY", &value)?;
    } else {
        while buffer.last().is_some_and(|&b| is_space(b) && is_ifs(b)) {
            buffer.pop();
        }
        // The last variable takes the remainder INCLUDING delimiters, but a
        // single trailing non-space delimiter is eaten when there were exactly
        // as many fields as names: `IFS=: read x y` gives `Y` for `X:Y:` and
        // `Y:Z:` for `X:Y:Z:`.
        if buffer.last().is_some_and(|&b| is_ifs(b)) {
            let mut keep = buffer.len() - 1;
            while keep > 0 {
                let prev = buffer.get(keep - 1).copied().unwrap_or(0);
                if is_space(prev) && is_ifs(prev) {
                    keep -= 1;
                } else {
                    break;
                }
            }
            let first_ifs = buffer.iter().position(|&b| is_ifs(b)).unwrap_or(buffer.len());
            if first_ifs >= keep {
                buffer.truncate(keep);
            }
        }
        let value = String::from_utf8_lossy(&buffer).into_owned();
        if let Some(name) = names.get(vi) {
            sh.set_var(name, &value)?;
        }
        for name in names.iter().skip(vi + 1) {
            sh.set_var(name, "")?;
        }
    }
    if code == 0 {
        ok(sh)
    } else {
        status(sh, code)
    }
}

/// busybox's `bb_strtou` as `read` uses it: decimal digits only. No sign, no
/// leading space, no trailing junk -- `+2`, ` 2` and `2x` are all errors. It
/// returns an `unsigned`, so a value past `u32::MAX` is one too.
fn bb_strtou(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u32>().ok()
}

/// `bb_strtou` for the two options busybox stores in an `int` and then rejects
/// when negative: `-n`'s count and `-u`'s descriptor stop at `INT_MAX`, where
/// `-t`'s milliseconds keep the full unsigned range.
fn bb_strtoi(s: &str) -> Option<i32> {
    bb_strtou(s).and_then(|v| i32::try_from(v).ok())
}

/// `read_ready` with the builtin's diagnostic attached: `Err(2)` is the usage
/// status the caller returns after the message has been printed.
fn read_ready_or_fail(sh: &mut Shell, fd: u32, timeout_ms: i32) -> Result<bool, i32> {
    match process::read_ready(sh, fd, timeout_ms) {
        Ok(ready) => Ok(ready),
        Err(e) => {
            err_line(sh, &format!("read: {e}"));
            Err(2)
        }
    }
}

/// `-t`'s timeout in milliseconds. bash 4.3 takes `N.NNN`, and busybox reads at
/// most THREE fractional digits: a non-digit AMONG THOSE THREE is an error, while
/// the fourth character on is never examined at all -- so `0.12x` is invalid and
/// `0.123x` is not. A bare trailing `.` is fine.
fn parse_read_timeout(s: &str) -> Option<u64> {
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (s, None),
    };
    let mut ms = bb_strtou(whole)? as u64;
    ms = ms.saturating_mul(1000);
    if let Some(frac) = frac {
        let mut scale = 100u64;
        for b in frac.bytes().take(3) {
            let d = (b as char).to_digit(10)?;
            ms = ms.saturating_add(u64::from(d) * scale);
            scale /= 10;
        }
    }
    Some(ms)
}

/// Signal numbers and the names `trap` accepts and prints, Linux/x86-64 order.
/// 0 is EXIT, POSIX's "condition" that is not a signal at all. dash matches the
/// name WITHOUT a `SIG` prefix, so `trap - SIGINT` is a bad trap while `INT` is not.
const SIGNALS: &[(u8, &str)] = &[
    (0, "EXIT"),
    (1, "HUP"),
    (2, "INT"),
    (3, "QUIT"),
    (4, "ILL"),
    (5, "TRAP"),
    (6, "ABRT"),
    (7, "BUS"),
    (8, "FPE"),
    (9, "KILL"),
    (10, "USR1"),
    (11, "SEGV"),
    (12, "USR2"),
    (13, "PIPE"),
    (14, "ALRM"),
    (15, "TERM"),
    (16, "STKFLT"),
    (17, "CHLD"),
    (18, "CONT"),
    (19, "STOP"),
    (20, "TSTP"),
    (21, "TTIN"),
    (22, "TTOU"),
    (23, "URG"),
    (24, "XCPU"),
    (25, "XFSZ"),
    (26, "VTALRM"),
    (27, "PROF"),
    (28, "WINCH"),
    (29, "IO"),
    (30, "PWR"),
    (31, "SYS"),
];

/// The highest number `trap` accepts as a condition. Linux has 64 signals; the
/// real-time ones above SYS have no name here, so they print as their number.
const MAX_SIGNAL: u8 = 64;

/// An operand that is an unsigned decimal integer -- POSIX's test for "this is a
/// condition, not an action", which is what makes `trap 0 EXIT` reset two traps
/// rather than set EXIT to the command `0`. Digits ONLY, as dash's `is_number` has
/// it: `trap ' 0 ' EXIT` sets an action. The range check belongs to decoding, not
/// here, so `trap 256 EXIT` is a bad CONDITION and not an action named `256`.
fn unsigned_int(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok()
}

fn decode_signal(s: &str) -> Option<u8> {
    if let Some(n) = unsigned_int(s) {
        return u8::try_from(n).ok().filter(|n| *n <= MAX_SIGNAL);
    }
    // dash compares names case-insensitively, so `trap - int 0 3` clears INT.
    for (n, name) in SIGNALS {
        if name.eq_ignore_ascii_case(s) {
            return Some(*n);
        }
    }
    None
}

fn signal_name(n: u8) -> String {
    for (num, name) in SIGNALS {
        if *num == n {
            return (*name).to_string();
        }
    }
    n.to_string()
}

/// SIGCHLD, the one signal the kernel WOULD take a disposition for that td-sh
/// still will not give it. `SIG_IGN` there does not mean "discard this signal":
/// it is POSIX's documented request that children be AUTO-REAPED, which takes
/// every command's exit status away from the shell that started it. Verified
/// before it was fixed here — `trap '' CHLD` left each external reporting "No
/// child processes" instead of what it exited with, while still running. bash
/// keeps SIGCHLD under its own control for the same reason. The trap is
/// recorded and printed like any other; only the kernel is left out of it.
const SIGCHLD: u8 = 17;

/// Whether td-sh may set `signo`'s disposition, asked of the kernel ONCE and
/// remembered — dash's `sigmode`, and for the same reason: after the first
/// change the kernel can no longer be asked what the process started with.
///
/// A non-default answer on first touch was installed by something other than
/// this shell. `execve` resets every caught signal to default, so it is either a
/// parent that ignored the signal — POSIX: one ignored on entry cannot be
/// trapped or reset, which is what makes `nohup` stick — or Rust's own runtime,
/// which ignores SIGPIPE and handles SEGV/BUS before `main`. Neither is td-sh's
/// to overwrite, and the SIGPIPE case is not academic: un-ignoring it would turn
/// every `yes | head` into a dead shell.
///
/// SIGPIPE is therefore the one signal `trap ''` cannot carry into a child:
/// `std::process::Command` undoes the runtime's ignore in every child it spawns,
/// and neither end of that is reachable from safe code. Undoing it in the shell
/// instead is a real fix and a separate one — it would make td-sh DIE on a
/// closed stdout the way dash does, which is a different failure mode for every
/// write in the crate.
///
/// A kernel that refuses the question is cached as "no", so a signal td-sh
/// cannot read is one it never writes.
/// Answer the question above for the signals the interrupt guard moves, while
/// this is still the only thread.
///
/// It is derived from the disposition the process is holding RIGHT NOW, which is
/// only "how the shell started" if nothing has changed it since. Sequentially
/// that held. With pipeline stages running at once it does not: a sibling's
/// guard is an ignore, and a stage asking for the first time during one would
/// cache "ignored on entry" — POSIX's never-touch-this answer — for a signal
/// that started at the default. The shell would then hand its own children that
/// ignore and stop being interruptible at all.
///
/// So EVERY signal is settled once, before any stage exists, and every clone
/// inherits the answers through `sig_may_set`. All of them rather than the two
/// the guard moves, because the guard is not the only thing that installs behind
/// a stage's back: a spawn briefly installs the spawning stage's own ignores,
/// and a sibling resolving one of those for the first time inside that window
/// would cache never-touch-this for a signal that started at the default — and a
/// cache is answered once and kept. Sixty-odd queries at startup, none after.
pub fn prime_signal_entries(sh: &mut Shell) {
    for sig in 1..=crate::sys::SIG_MAX {
        if crate::sys::changeable(sig) {
            let _ = may_set_signal(sh, sig);
        }
    }
}

pub fn may_set_signal(sh: &mut Shell, signo: u8) -> bool {
    if let Some(known) = sh.sig_may_set.get(&signo) {
        return *known;
    }
    let entry = crate::sys::signal_get(signo);
    let free = matches!(entry, Ok(Some(crate::sys::Disposition::Default)));
    sh.sig_may_set.insert(signo, free);
    free
}

/// Whether `signo`'s kernel disposition is td-sh's to install AT ALL.
///
/// Shared by the `trap` builtin and by the spawn that hands a child this shell's
/// intent, so the two cannot disagree about which signals the shell may move.
/// EXIT is not a signal and SIGKILL/SIGSTOP are not anyone's; SIGCHLD ignored is
/// POSIX's request that children be AUTO-REAPED, which would leave every
/// external command reporting `ECHILD` instead of its status; and a signal that
/// was not `SIG_DFL` on entry was installed by a parent or by Rust's own
/// runtime, which POSIX says stays where it is.
pub fn may_install(sh: &mut Shell, signo: u8) -> bool {
    signo != SIGCHLD && crate::sys::changeable(signo) && may_set_signal(sh, signo)
}

/// Move `signo`'s kernel disposition to where the trap table now says it is.
///
/// This is the half of `trap` that leaves the shell: an ignore is inherited
/// across `execve`, so `trap '' INT` has to reach the CHILDREN a script starts,
/// and shell-side bookkeeping alone cannot do that. The catching half cannot be
/// served at all — a handler needs an `SA_RESTORER` trampoline this crate does
/// not have — so a non-empty action asks for `SIG_DFL`, which is honest: the
/// shell dies on the signal rather than pretending an action will run.
///
/// Returns whether it succeeded, so a kernel that disagrees is REPORTED rather
/// than leaving the table and the process describing different shells.
fn apply_disposition(sh: &mut Shell, signo: u8, want: crate::sys::Disposition) -> bool {
    // dash is silent about the signals it may not move, and so is this: the trap
    // is still RECORDED, which is what `trap` prints.
    if !may_install(sh, signo) {
        return true;
    }
    // A PIPELINE STAGE records and stops there. The kernel disposition is one
    // cell shared by every stage, and each stage's `Subshell` restores what it
    // saw when it changed one -- so with stages running at once a stage captures
    // a SIBLING's disposition and puts it back after the pipeline ends, leaving
    // the parent holding it forever. `{ trap "" INT; sleep .1; } | { sleep .05;
    // trap - INT; sleep .2; }` left the shell ignoring SIGINT and needing a
    // SIGKILL, and the corruption outlives the pipeline that caused it.
    //
    // Recording is enough, because the only thing the kernel disposition buys
    // `trap ''` is reaching the CHILDREN a stage starts -- and `spawn_uninherited`
    // installs this stage's own intent across every spawn, reading the same table
    // this writes. What a stage gives up is protecting ITSELF from the signal,
    // which is already the case: a pipeline is not covered by the interrupt guard
    // either, for the same reason.
    if sh.concurrent {
        return true;
    }
    let prev = match crate::sys::signal_set(signo, want) {
        Ok(prev) => prev,
        Err(e) => {
            // `trap` is this function's only caller, so it names it as every
            // other diagnostic out of that builtin now does.
            err_line(sh, &format!("trap: {e}"));
            return false;
        }
    };
    // What the PROCESS now holds, which a spawn inside a pipeline needs and the
    // trap table cannot tell it: a stage that clears this leaves the record
    // standing, because clearing it in a stage does not reach the kernel.
    sh.sig_installed.retain(|s| *s != signo);
    if want == crate::sys::Disposition::Ignore {
        sh.sig_installed.push(signo);
    }
    // Only a CLONE has anything to undo, and only its first change to a signal
    // saw the value its parent left. `may_set_signal` has already established
    // the entry disposition was `SIG_DFL`, so `prev` is never a handler.
    if sh.cloned {
        if let Some(prev) = prev {
            if !sh.sig_undo.iter().any(|(n, _)| *n == signo) {
                sh.sig_undo.push((signo, prev == crate::sys::Disposition::Ignore));
            }
        }
    }
    true
}

/// `trap [action] condition...`. Only EXIT ever fires an ACTION: catching a
/// signal needs a handler this shell cannot install, so a non-empty action for
/// any other condition is recorded and reported but never delivered. An EMPTY
/// action is different — it is POSIX's "ignore", and `apply_disposition` gives
/// it to the kernel, which is what makes it hold across `exec` and in children.
fn trap(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut ops = argv.get(1..).unwrap_or_default();
    // dash's `nextopt(nullstr)`: `--` ends an empty option set, a bare `-` is the
    // reset ACTION, and any other `-x` is a usage error -- fatal, `trap` being a
    // special builtin, which is how `trap -p` ends a script with status 2.
    if let Some(rest) = ops.first().and_then(|a| a.strip_prefix('-')) {
        if rest == "-" {
            ops = ops.get(1..).unwrap_or_default();
        } else if !rest.is_empty() {
            let c = rest.chars().next().unwrap_or('-');
            return special_usage_error(sh, &format!("trap: illegal option -{c}"));
        }
    }
    let Some(first) = ops.first() else {
        let mut text = String::new();
        for (signo, action) in &sh.traps {
            text.push_str("trap -- ");
            text.push_str(&single_quote(action));
            text.push(' ');
            text.push_str(&signal_name(*signo));
            text.push('\n');
        }
        return out(sh, text.as_bytes());
    };
    // With one operand there is no action: `trap EXIT` and `trap 2` RESET.
    let (action, conditions) = if ops.len() > 1 && unsigned_int(first).is_none() {
        (Some(first.as_str()), ops.get(1..).unwrap_or_default())
    } else {
        (None, ops)
    };
    let mut code = 0;
    for name in conditions {
        let Some(signo) = decode_signal(name) else {
            err_line(sh, &format!("trap: {name}: invalid signal specification"));
            code = 1;
            continue;
        };
        let want = match action {
            None | Some("-") => {
                sh.traps.remove(&signo);
                crate::sys::Disposition::Default
            }
            Some(a) => {
                sh.traps.insert(signo, a.to_string());
                if a.is_empty() {
                    crate::sys::Disposition::Ignore
                } else {
                    crate::sys::Disposition::Default
                }
            }
        };
        // The table is written first, so `trap` reports what was asked for even
        // when the kernel would not take it -- dash records unconditionally too.
        if !apply_disposition(sh, signo, want) {
            code = 1;
        }
    }
    status(sh, code)
}

/// dash's `single_quote`: runs of ordinary text go in `'…'`, runs of quotes in
/// `"…"`, so `it's` prints as `'it'"'"'s'`.
fn single_quote(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    loop {
        let plain = rest.bytes().position(|b| b == b'\'').unwrap_or(rest.len());
        out.push('\'');
        out.push_str(rest.get(..plain).unwrap_or(""));
        out.push('\'');
        rest = rest.get(plain..).unwrap_or("");
        let quotes = rest.bytes().take_while(|&b| b == b'\'').count();
        if quotes == 0 {
            break;
        }
        out.push('"');
        out.push_str(rest.get(..quotes).unwrap_or(""));
        out.push('"');
        rest = rest.get(quotes..).unwrap_or("");
        if rest.is_empty() {
            break;
        }
    }
    out
}

/// `alias [name[=value] …]`: define, or print in a form that can be re-read.
/// dash looks for the `=` from the SECOND byte, so `-x` and `=y` are name
/// lookups rather than definitions.
fn alias(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut ret = 0;
    if argv.len() == 1 {
        let listing: Vec<String> = sh
            .aliases
            .iter()
            .map(|(name, value)| format!("{name}={}\n", single_quote(value)))
            .collect();
        for line in listing {
            // A failed write is reported, as it is for every other builtin that
            // prints (see `out`), rather than silently succeeding -- and a
            // BROKEN one ends the shell there, for the reason `sigpipe` gives.
            match write_fd(sh, 1, line.as_bytes()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                    return Err(sigpipe())
                }
                Err(_) => ret = 1,
            }
        }
        return status(sh, ret);
    }
    for arg in argv.iter().skip(1) {
        // Bytes, not chars: the `=` is looked for from the second BYTE, and a
        // name may start with a multi-byte character (`alias é=echo`).
        let eq = arg
            .as_bytes()
            .get(1..)
            .and_then(|tail| tail.iter().position(|&b| b == b'=').map(|at| at + 1));
        match eq {
            Some(at) => {
                let name = arg.get(..at).unwrap_or("").to_string();
                let value = arg.get(at + 1..).unwrap_or("").to_string();
                sh.aliases.insert(name, value);
            }
            None => match sh.aliases.get(arg) {
                Some(value) => {
                    let line = format!("{arg}={}\n", single_quote(value));
                    // Same rule as `out` and the bare listing above: a BROKEN
                    // pipe ends the writer, or `while :; do alias a; done |
                    // head -1` spins forever on a reader that is gone.
                    match write_fd(sh, 1, line.as_bytes()) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                            return Err(sigpipe())
                        }
                        Err(_) => ret = 1,
                    }
                }
                None => {
                    err_line(sh, &format!("alias: {arg} not found"));
                    ret = 1;
                }
            },
        }
    }
    status(sh, ret)
}

/// `unalias [-a] [name …]`. dash's option loop takes only `-a` and stops at
/// `--`; with no names left it reports success, so bare `unalias` is not an
/// error there.
fn unalias(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut i = 1usize;
    match argv.get(i).and_then(|arg| arg.strip_prefix('-')) {
        // A bare `-`, or no leading `-` at all, is already an operand.
        None | Some("") => {}
        Some("-") => i += 1,
        // `unaliascmd` RETURNS inside its first `nextopt` iteration, so an `a`
        // ends the scan and the rest of the cluster is never read: `unalias
        // -aZ` clears and succeeds where `-Za` refuses.
        Some(flags) => match flags.chars().next() {
            Some('a') => {
                sh.aliases.clear();
                return status(sh, 0);
            }
            Some(bad) => {
                err_line(sh, &format!("unalias: illegal option -{bad}"));
                return status(sh, 2);
            }
            // Unreachable: an empty `flags` is the arm above.
            None => {}
        },
    }
    let mut ret = 0;
    for name in argv.iter().skip(i) {
        if sh.aliases.remove(name).is_none() {
            err_line(sh, &format!("unalias: {name} not found"));
            ret = 1;
        }
    }
    status(sh, ret)
}

/// `exec [-a NAME] [--] [command [arg …]]`. With no command this is only a
/// carrier for its redirections, which the caller leaves in force instead of
/// restoring; with one it REPLACES this shell, so it returns only when the
/// command cannot be run.
fn exec_cmd(sh: &mut Shell, argv: &[String]) -> R<()> {
    let (i, arg0) = match exec_options(argv) {
        Ok(parsed) => parsed,
        Err(msg) => return special_usage_error(sh, &msg),
    };
    match argv.get(i..) {
        None | Some([]) => ok(sh),
        Some(words) => process::exec_replace(sh, words, arg0),
    }
}

/// `exec`'s `nextopt("a:")` scan (ash.c:10073): the index of the first COMMAND
/// word, and `-a`'s argv[0] override. `-a` eats the rest of its word, so there
/// is no cluster to walk.
fn exec_options(argv: &[String]) -> Result<(usize, Option<&str>), String> {
    let mut arg0: Option<&str> = None;
    let mut i = 1usize;
    while let Some(letters) = argv
        .get(i)
        .and_then(|w| w.strip_prefix('-'))
        .filter(|f| !f.is_empty())
    {
        i += 1;
        if letters == "-" {
            break; // the word was `--`
        }
        let mut cs = letters.chars();
        match cs.next() {
            Some('a') => {}
            Some(bad) => return Err(format!("exec: illegal option -{bad}")),
            // Unreachable: the filter above rejects an empty `letters`.
            None => break,
        }
        // Attached (`-aNAME`) or the next word, which `nextopt` takes RAW --
        // so `exec -a -- cmd` names the replacement `--`.
        let attached = cs.as_str();
        if attached.is_empty() {
            let Some(next) = argv.get(i) else {
                return Err("exec: no arg for -a option".to_owned());
            };
            arg0 = Some(next);
            i += 1;
        } else {
            arg0 = Some(attached);
        }
    }
    Ok((i, arg0))
}

/// Whether this `exec` is the bare form whose redirections STAY in force: it is
/// COMMAND words that decide, not argv length -- `exec -- 3>&1` has two words
/// and none of them a command.
pub fn exec_keeps_redirections(argv: &[String]) -> bool {
    match exec_options(argv) {
        Ok((i, _)) => matches!(argv.get(i..), None | Some([])),
        // A refusal took no command word either, so it unwinds like any other
        // failed builtin.
        Err(_) => false,
    }
}

fn eval(sh: &mut Shell, argv: &[String]) -> R<()> {
    let joined = argv
        .iter()
        .skip(1)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if joined.trim().is_empty() {
        return ok(sh);
    }
    // Unit at a time, so `eval "alias x=…\nx"` sees its own alias (dash's
    // evalstring parses and runs one command at a time too).
    exec::run_source(sh, &joined, "eval: ")
}

/// dash's `updatepwd`: build the LOGICAL path `dir` names from `curdir`, purely
/// lexically. `..` pops the previous component off the string without looking at
/// the filesystem -- which is why `cd nonexistent/..` succeeds, and why a path
/// through a symlink keeps the name it was reached by rather than its target.
fn update_pwd(curdir: &std::path::Path, dir: &str) -> std::path::PathBuf {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    // Bytes, not chars: a path is bytes on Unix, and the only characters this
    // inspects (`/` and `.`) are ASCII, so a directory whose name is not valid
    // UTF-8 passes through untouched rather than becoming U+FFFD.
    let curdir = curdir.as_os_str().as_bytes();
    let dir = dir.as_bytes();
    // `floor` is dash's `lim`: `..` may walk up to the root and no further -- NOT
    // merely back to where this started, so `cd ../..` from /tmp reaches /. Two
    // leading slashes are implementation-defined, and dash keeps them as the
    // floor instead. The two branches test that differently, and deliberately:
    // an ABSOLUTE operand must have exactly two (`dir[1] == '/' && dir[2] != '/'`,
    // so `///` collapses to one), while for a relative one dash looks only at
    // `curdir[1]`, so a curdir of `///x` still floors at two.
    let (mut out, floor): (Vec<u8>, usize) = if dir.starts_with(b"/") {
        if dir.starts_with(b"//") && !dir.starts_with(b"///") {
            (b"//".to_vec(), 2)
        } else {
            (b"/".to_vec(), 1)
        }
    } else {
        let mut s = curdir.to_vec();
        if !s.ends_with(b"/") {
            s.push(b'/');
        }
        let floor = if s.get(1) == Some(&b'/') { 2 } else { 1 };
        (s, floor)
    };
    for part in dir.split(|b| *b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                // Drop bytes until the one now at the end is the `/` that ended
                // the previous component.
                while out.len() > floor {
                    out.pop();
                    if out.ends_with(b"/") && out.len() > floor {
                        break;
                    }
                }
            }
            _ => {
                if !out.ends_with(b"/") {
                    out.push(b'/');
                }
                out.extend_from_slice(part);
            }
        }
    }
    if out.len() > floor && out.ends_with(b"/") {
        out.pop();
    }
    std::path::PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// `-L`/`-P` for `cd` and `pwd`. dash's `cdopt` XORs the physical bit whenever
/// the letter CHANGES, starting from `L`, which comes to the same thing as the
/// LAST of the two winning. Err carries the offending letter.
fn cd_opts(argv: &[String]) -> Result<(bool, usize), char> {
    let mut physical = false;
    let mut i = 1usize;
    while let Some(arg) = argv.get(i) {
        if arg == "--" {
            i += 1;
            break;
        }
        let Some(letters) = arg.strip_prefix('-') else {
            break;
        };
        // A lone `-` is the operand meaning $OLDPWD, not an option.
        if letters.is_empty() {
            break;
        }
        for c in letters.chars() {
            if c != 'L' && c != 'P' {
                return Err(c);
            }
            physical = c == 'P';
        }
        i += 1;
    }
    Ok((physical, i))
}

fn cd(sh: &mut Shell, argv: &[String]) -> R<()> {
    let (physical, i) = match cd_opts(argv) {
        Ok(v) => v,
        Err(c) => {
            err_line(sh, &format!("cd: illegal option -{c}"));
            return status(sh, 2);
        }
    };
    // `cd -` reports where it landed, as does a CDPATH hit (which td-sh does not
    // implement yet).
    let mut print = false;
    let dest = match argv.get(i) {
        Some(s) if s == "-" => {
            print = true;
            sh.get_var("OLDPWD").unwrap_or_default()
        }
        Some(s) => s.clone(),
        None => sh.get_var("HOME").unwrap_or_default(),
    };
    // dash never errors on an empty destination: an unset HOME or OLDPWD leaves
    // the empty string, which it then treats as `.`, so `cd -` without OLDPWD is
    // a successful no-move.
    let dest = if dest.is_empty() { "." } else { dest.as_str() };
    // dash's docd runs updatepwd ONLY in logical mode; `-P` hands the operand
    // straight to chdir, so `link/..` follows the link first instead of having
    // both components cancel lexically.
    let target = if physical {
        sh.resolve(dest)
    } else {
        update_pwd(&sh.logical_cwd, dest)
    };
    // The physical directory is what a child is started in and what a relative
    // path resolves against, so it is always the canonical one. `cd` is a REGULAR
    // builtin, so dash's sh_error here is caught and becomes a status rather than
    // ending the script -- but the status is 2, not 1.
    // `perror`, not `errmsg` (ash.c:2976): always the system's word, `cd`
    // having no per-site substitute the way a redirection does.
    let phys = match target.canonicalize() {
        Ok(p) if p.is_dir() => p,
        // Reached only when the name RESOLVES and is not a directory, which is
        // the one `cd` failure no syscall reports: the shell's cwd is a variable
        // rather than the process's, so there is no `chdir` to take the errno
        // from. ENOTDIR is what one would have answered.
        Ok(_) => {
            let e = std::io::Error::from_raw_os_error(exec::ENOTDIR);
            err_line(sh, &format!("cd: can't cd to {dest}: {}", exec::strerror(&e)));
            return status(sh, 2);
        }
        Err(e) => {
            err_line(sh, &format!("cd: can't cd to {dest}: {}", exec::strerror(&e)));
            return status(sh, 2);
        }
    };
    // With `-P` dash passes no logical path to setpwd, which then takes the
    // physical one; otherwise the name walked to is kept.
    let new = if physical { phys.clone() } else { target };
    // ash's `setpwd` order (ash.c:2865, 2883, 2885): OLDPWD is written BEFORE
    // `curdir` moves, so a refusal there leaves `pwd` where it was. The chdir is
    // committed either way -- `sh.cwd` is what children are given, and ash has
    // already chdir'd by the time `setpwd` runs.
    let old = sh.logical_cwd.to_string_lossy().into_owned();
    sh.cwd = phys;
    // Both are exported, as dash's setpwd writes them. The variables are the one
    // place the path has to become a String, so a name that is not UTF-8 is lossy
    // THERE while the shell's own directory keeps its bytes.
    sh.set_var("OLDPWD", &old)?;
    sh.export_var("OLDPWD");
    sh.logical_cwd = new.clone();
    sh.set_var("PWD", &new.to_string_lossy())?;
    sh.export_var("PWD");
    if print {
        use std::os::unix::ffi::OsStrExt;
        let mut line = new.as_os_str().as_bytes().to_vec();
        line.push(b'\n');
        return out(sh, &line);
    }
    ok(sh)
}

/// The bits `bb_parse_mode` works in: the nine rwx bits plus setuid, setgid and
/// sticky. A umask carries only the nine, but the parser handles all twelve and
/// the CALLER rejects a result that outgrew 0777 -- which is how `a+t` becomes
/// an illegal mode while `o=rwxs` is a legal one that changes nothing.
const S_ISUID: u32 = 0o4000;
const S_ISGID: u32 = 0o2000;
const S_ISVTX: u32 = 0o1000;
const FILEMODEBITS: u32 = S_ISUID | S_ISGID | S_ISVTX | 0o777;

const WHO_CHARS: &[u8] = b"augo";
const WHO_MASK: [u32; 4] = [FILEMODEBITS, S_ISUID | 0o700, S_ISGID | 0o070, 0o007];
const PERM_CHARS: &[u8] = b"rwxXst";
const PERM_MASK: [u32; 6] = [0o444, 0o222, 0o111, 0o111, S_ISUID | S_ISGID, S_ISVTX];

/// `umask`'s mode operand, ported from busybox `libbb/parse_mode.c`.
///
/// `current_mode` is what the clauses start from -- for a symbolic operand ash
/// hands in the PERMITTED bits, so `-` really does subtract permission. A clause
/// holds a LIST of actions (`u+r-w`), and each action sees what the last one
/// left. `umask_now` is the process mask, which limits a clause that names no
/// `who`. `None` is the mode ash calls illegal.
fn parse_mode(text: &str, current_mode: u32, umask_now: u32) -> Option<u32> {
    let b = text.as_bytes();
    // Numeric only when the FIRST character is an octal digit; `8` falls through
    // to the symbolic parser, which rejects it as a bad `who`.
    if b.first().is_some_and(|c| (b'0'..=b'7').contains(c)) {
        let mut v: u32 = 0;
        for c in b {
            if !(b'0'..=b'7').contains(c) {
                // `strtoul`'s trailing-character check: `0778` is not 077.
                return None;
            }
            v = v.checked_mul(8)?.checked_add(u32::from(c - b'0'))?;
        }
        return if v > FILEMODEBITS { None } else { Some(v) };
    }

    let mut new_mode = current_mode;
    let mut i = 0usize;
    while i < b.len() {
        if b.get(i) == Some(&b',') {
            // Empty clauses are allowed, and an empty mode changes nothing.
            i += 1;
            continue;
        }
        // A `who` list. Running off the end inside one is an error rather than
        // an implicit empty action, so `umask u` is illegal.
        let mut wholist = 0u32;
        while let Some(k) = b.get(i).and_then(|c| WHO_CHARS.iter().position(|w| w == c)) {
            wholist |= *WHO_MASK.get(k)?;
            i += 1;
            if i >= b.len() {
                return None;
            }
        }
        loop {
            let op = *b.get(i)?;
            if op != b'+' && op != b'-' {
                if op != b'=' {
                    return None;
                }
                // `=` clears BEFORE the perms are read, which is why `X` and a
                // permcopy in the same clause see the cleared value: `umask 0;
                // umask a=X` leaves execute off and so is 0777, not 0666.
                new_mode &= if wholist != 0 { !wholist } else { !FILEMODEBITS };
            }
            i += 1;

            // A permcopy (`u=g`) reads the running value, and only from u/g/o.
            let copy = b.get(i).and_then(|c| b"ugo".iter().position(|w| w == c));
            let mut permlist = match copy {
                Some(k) => {
                    let mut pl = *WHO_MASK.get(k + 1)? & 0o777 & new_mode;
                    for m in [0o444u32, 0o222, 0o111] {
                        if pl & m != 0 {
                            pl |= m;
                        }
                    }
                    i += 1;
                    pl
                }
                None => {
                    let mut pl = 0u32;
                    while let Some(k) =
                        b.get(i).and_then(|c| PERM_CHARS.iter().position(|p| p == c))
                    {
                        // `X` is execute only where execute already is.
                        if PERM_CHARS.get(k) != Some(&b'X') || new_mode & 0o111 != 0 {
                            pl |= *PERM_MASK.get(k)?;
                        }
                        i += 1;
                    }
                    pl
                }
            };
            if permlist != 0 {
                // A clause naming no `who` is limited by the CURRENT process
                // mask -- POSIX's rule, and note it is the mask, not the value
                // being built, so an earlier clause cannot widen it.
                permlist &= if wholist != 0 { wholist } else { !umask_now };
                if op == b'-' {
                    new_mode &= !permlist;
                } else {
                    new_mode |= permlist;
                }
            }
            // Anything left that is not a separator must be another action, so
            // `u=gr` is an error rather than a silently truncated `u=g`.
            match b.get(i) {
                None | Some(&b',') => break,
                _ => {}
            }
        }
    }
    Some(new_mode)
}

/// `umask -S`'s output: the bits a new file WOULD get, not the mask itself.
fn symbolic_mode(mask: u32) -> String {
    let permitted = 0o777 & !mask;
    let mut out = String::new();
    for (label, shift) in [('u', 6), ('g', 3), ('o', 0)] {
        if !out.is_empty() {
            out.push(',');
        }
        out.push(label);
        out.push('=');
        let t = (permitted >> shift) & 7;
        for (bit, ch) in [(4, 'r'), (2, 'w'), (1, 'x')] {
            if t & bit != 0 {
                out.push(ch);
            }
        }
    }
    out
}

/// ash's `umask`. Reading the mask needs no argument and setting it takes one;
/// extra operands are IGNORED (`umask 077 077` is a success), and an error is an
/// ordinary status 2 rather than a fatal, so a script continues unless `set -e`.
fn umask_builtin(sh: &mut Shell, argv: &[String]) -> R<()> {
    // ash's `nextopt("S")`: flags BUNDLE (`-SS`), `--` ends them, and a lone `-`
    // is an operand rather than an option.
    let mut idx = 1;
    let mut symbolic = false;
    while let Some(arg) = argv.get(idx) {
        let Some(flags) = arg.strip_prefix('-') else {
            break;
        };
        if flags.is_empty() {
            break;
        }
        idx += 1;
        if flags == "-" {
            break;
        }
        for c in flags.chars() {
            if c != 'S' {
                err_line(sh, &format!("umask: illegal option -{c}"));
                return status(sh, 2);
            }
            symbolic = true;
        }
    }
    let current = crate::sys::umask_get();
    let Some(text) = argv.get(idx) else {
        let line = if symbolic {
            format!("{}\n", symbolic_mode(current))
        } else {
            format!("{current:04o}\n")
        };
        return out(sh, line.as_bytes());
    };
    // ash inverts around the parse: a numeric operand IS the mask, but a
    // symbolic one names permissions, so the clauses run on the complement and
    // the result is complemented back. `isdigit` picks the branch, so `8` takes
    // the numeric path's spelling and the symbolic path's parser -- and is
    // rejected by it.
    let numeric = text.as_bytes().first().is_some_and(|c| c.is_ascii_digit());
    let entry = if numeric { current } else { current ^ 0o777 };
    let illegal = |sh: &mut Shell| {
        err_line(sh, &format!("umask: illegal mode: {text}"));
        status(sh, 2)
    };
    let Some(parsed) = parse_mode(text, entry, current) else {
        return illegal(sh);
    };
    // Whatever grew past the nine rwx bits -- setuid, setgid or sticky -- makes
    // the whole mode illegal; a umask has no say over them.
    if parsed > 0o777 {
        return illegal(sh);
    }
    let mask = if numeric { parsed } else { parsed ^ 0o777 };
    // Recorded BEFORE the readback is judged, because the mask is installed
    // either way: `umask_set` asks the kernel first and only then reports a
    // disagreement, so a clone that gave up here still has one to put back.
    sh.umask_changed = true;
    if let Err(e) = crate::sys::umask_set(mask) {
        err_line(sh, &e);
        return status(sh, 2);
    }
    status(sh, 0)
}

fn pwd(sh: &mut Shell, argv: &[String]) -> R<()> {
    let physical = match cd_opts(argv) {
        Ok((p, _)) => p,
        Err(c) => {
            err_line(sh, &format!("pwd: illegal option -{c}"));
            return status(sh, 2);
        }
    };
    // The LOGICAL cwd, which is what `cd` recorded -- not $PWD, which a script is
    // free to overwrite, and not a fresh lookup. `-P` reports the physical one,
    // which the shell already holds canonicalized.
    use std::os::unix::ffi::OsStrExt;
    let dir = if physical { &sh.cwd } else { &sh.logical_cwd };
    let mut line = dir.as_os_str().as_bytes().to_vec();
    line.push(b'\n');
    // `out` sets `$?` (0, or 1 on write error); do not overwrite it.
    out(sh, &line)
}

/// Where `.` looks: a name containing a slash is used as given -- unexamined, so
/// `. /dev/null` and a FIFO still work -- and anything else is looked up in PATH,
/// where only a REGULAR file counts. Unlike a command lookup there is no implicit
/// fallback to the current directory (dash reaches cwd only through an empty or
/// `.` PATH element) and executability is not required.
/// The path to OPEN, and the name to REPORT when opening it fails. They differ
/// once PATH resolves the file: ash's `find_dot_file` hands `setinputfile` the
/// concatenation it found rather than the operand (ash.c:13664).
fn dot_path(sh: &Shell, name: &str) -> Option<(std::path::PathBuf, String)> {
    if name.contains('/') {
        return Some((sh.resolve(name), name.to_string()));
    }
    let path = sh.get_var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        // An empty entry is the cwd and contributes NO prefix, so the name is
        // reported bare -- `padvance`'s handling, checked against ash.
        let found = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
        let candidate = sh.resolve(&found);
        if candidate.is_file() {
            return Some((candidate, found));
        }
    }
    None
}

/// The kernel reports `/proc/<pid>/stat`'s clock fields in USER_HZ, an ABI
/// constant decoupled from `CONFIG_HZ` so the file does not change meaning with
/// the machine's tick rate. It is per-ARCHITECTURE, not universal: 100 on every
/// one td targets, 1024 on alpha and ia64. Reading it without a libc means
/// `AT_CLKTCK` out of `/proc/self/auxv`, which is where a port would get it.
const USER_HZ: u64 = 100;

/// `utime stime cutime cstime` out of `/proc/self/stat`, in ticks.
///
/// Split after the LAST `)` rather than on whitespace: field 2 is the executable's
/// name, which the kernel copies verbatim, so a binary renamed `my )sh` puts both
/// a space and a paren inside it and counting fields from the start of the line
/// then reads four wrong numbers. The kernel guarantees the last `)` closes it.
fn cpu_ticks(stat: &str) -> Option<[u64; 4]> {
    let after = stat.rsplit_once(')')?.1;
    let mut fields = after.split_whitespace();
    // The first token after the name is `state`, field 3, so `utime` (14) is
    // eleven further on and the other three follow it directly.
    let utime = fields.nth(11)?.parse().ok()?;
    let stime = fields.next()?.parse().ok()?;
    let cutime = fields.next()?.parse().ok()?;
    let cstime = fields.next()?.parse().ok()?;
    Some([utime, stime, cutime, cstime])
}

/// ash's `times` format: minutes and seconds to three places, the shell's own
/// user and system time then its reaped children's -- which is what `cutime`
/// and `cstime` already count, so `/proc` answers this without `times(2)`.
/// `hz` is a parameter rather than the constant so the SCALING is a tested
/// function: at 100 every way of writing it agrees, which is the one value the
/// gate can see.
fn format_ticks(t: u64, hz: u64) -> String {
    let secs = t / hz;
    // Multiply before dividing: `1000 / hz` is 0 for any hz above 1000 and lossy
    // below it, so the scale has to be applied to the remainder itself.
    let frac = t % hz * 1000 / hz;
    format!("{}m{}.{:03}s", secs / 60, secs % 60, frac)
}

/// The two lines, as a function of the four numbers, so their ORDER is testable.
/// On an idle machine every one of them is 0, so a swapped pair -- children's
/// time reported as the shell's, or system as user -- is a well-formed answer
/// that agrees with the right one everywhere the gate can see.
fn times_text(ticks: [u64; 4]) -> String {
    let [ut, st, cut, cst] = ticks;
    let line = |a: u64, b: u64| {
        format!("{} {}\n", format_ticks(a, USER_HZ), format_ticks(b, USER_HZ))
    };
    line(ut, st) + &line(cut, cst)
}

/// What to do when the numbers are not there, which in ash cannot happen: it
/// reads `times(2)`, and this reads a mount. Report the failure rather than four
/// zeros, since a zero is an ANSWER and a script cannot tell it from a measured
/// one -- but do NOT be fatal about it, though `times` is a special builtin. A
/// script ending outright because a diagnostics builtin could not read `/proc` is
/// out of all proportion to what it asked for, and ash's own `times` never ends
/// one. Split from `times` because the gate has a `/proc`, so this is the only
/// way the arm can be reached.
fn times_out(sh: &mut Shell, ticks: Option<[u64; 4]>) -> R<()> {
    let Some(ticks) = ticks else {
        err_line(sh, "times: cannot read /proc/self/stat");
        return status(sh, 1);
    };
    // `out` sets `$?`; do not overwrite it.
    out(sh, times_text(ticks).as_bytes())
}

fn times(sh: &mut Shell) -> R<()> {
    // Bytes, not `read_to_string`: field 2 is the executable's own name copied
    // verbatim, so a binary renamed with a stray 0xFF makes the whole file
    // invalid UTF-8 -- and the numbers this wants are pure ASCII either way.
    let stat = std::fs::read("/proc/self/stat").ok();
    let stat = stat.as_deref().map(String::from_utf8_lossy);
    times_out(sh, stat.as_deref().and_then(cpu_ticks))
}

fn dot(sh: &mut Shell, argv: &[String]) -> R<()> {
    // ash prints the spelling it was CALLED by -- `source: can't open` against
    // `.: can't open` -- so the word comes from argv rather than a literal.
    let word = argv.first().map_or(".", String::as_str);
    // ash runs an option loop here despite accepting NO option, so the only
    // things it can do are end on `--` and refuse everything else. Refusing is
    // FATAL, `.` being special. A lone `-` is not an option but a FILENAME
    // (`. - ` reports `-: not found`), and the refusal names the first LETTER
    // rather than the word, so `. -abc f` is `illegal option -a`. One word is
    // examined rather than a loop of them because the first non-`--` option
    // word never returns.
    let mut rest = argv.get(1..).unwrap_or(&[]);
    match rest.first().map(String::as_str) {
        Some("--") => rest = rest.get(1..).unwrap_or(&[]),
        // A lone `-` needs no guard of its own: stripping the dash leaves
        // nothing to name, which is the same answer a word with no dash gets.
        Some(w) => {
            if let Some(bad) = w.strip_prefix('-').and_then(|o| o.chars().next()) {
                return special_usage_error(sh, &format!("{word}: illegal option -{bad}"));
            }
        }
        None => {}
    }
    let Some(name) = rest.first() else {
        // Not fatal in either shell: busybox ash returns 2 outright ("bash
        // compat" in its own words) and dash returns 0 without a word. Only a
        // file it cannot LOCATE raises.
        err_line(sh, &format!("{word}: filename argument required"));
        return status(sh, 2);
    };
    let Some((path, found)) = dot_path(sh, name) else {
        return special_usage_error(sh, &format!("{word}: {name}: not found"));
    };
    // Only a failure to OPEN raises. A read that then fails -- a directory, most
    // often -- reaches dash's input layer as EOF, so it sources nothing and
    // reports 0; `. ./dir/` is pinned that way for dash in the corpus.
    let text = match std::fs::File::open(&path) {
        Ok(mut f) => {
            let mut t = String::new();
            let _ = std::io::Read::read_to_string(&mut f, &mut t);
            t
        }
        // ash quotes the name here and nowhere else (`can't open '%s'`,
        // ash.c:11257), and reports through `perror` as `cd` does.
        Err(e) => {
            let why = exec::strerror(&e);
            return special_usage_error(sh, &format!("{word}: can't open '{found}': {why}"));
        }
    };
    let what = format!("{name}: ");
    // OPERANDS become the file's positional parameters, and only then: given
    // none, ash saves nothing and the file both SEES and KEEPS the caller's, so
    // a `set --` inside one leaks out. Given any, the frame is restored
    // afterwards and that same `set --` is undone. The difference is observable
    // either way round, which is why the save is conditional rather than
    // unconditional-with-an-empty-vector.
    let args = rest.get(1..).unwrap_or(&[]);
    // The getopts cursor travels WITH the parameters: ash's frame is one struct
    // (`struct shparam`, ash.c:2057) carrying `optind` and `optoff` beside the
    // list, and `dotcmd` copies the whole of it. It is not RESET for the file --
    // unlike a function call, `.` leaves the caller's cursor in place, so the
    // file continues the caller's scan. What the save is for is a `set --`
    // inside the file, which resets that cursor: without it the caller re-reads
    // the option it had already consumed.
    let saved = (!args.is_empty()).then(|| {
        (
            std::mem::replace(&mut sh.params, args.to_vec()),
            sh.getopts_optind,
            sh.getopts_off,
        )
    });
    // A `return` in a sourced file returns from the `.`, not the process -- and
    // is how such a file usually ends, so the frame has to come back on that
    // path as much as on the ordinary one. Not `?` for the same reason.
    let ran = exec::run_source(sh, &text, &what);
    // But NOT on a terminating unwind. ash restores after `cmdloop` RETURNS, so
    // an `exit` inside the file longjmps straight past it and the EXIT trap runs
    // with the FILE's operands still in place. Measured: `. ./f a b` where `f`
    // exits leaves the trap seeing `a b` where this shell showed the caller's.
    if let Some((params, optind, off)) = saved {
        if !matches!(&ran, Err(sig) if exec::terminating(sig)) {
            sh.params = params;
            sh.getopts_optind = optind;
            sh.getopts_off = off;
        }
    }
    match ran {
        Err(Sig::Return(code)) => status(sh, code),
        other => other,
    }
}

/// Where a name would be located on `PATH`, spelled the way ash spells it: the
/// element JOINED to the name rather than a resolved path (`padvance`), an EMPTY
/// element contributing nothing, and only a stat -- ash checks the execute bit
/// when a command RUNS, not when it is described.
fn describe_path(sh: &Shell, name: &str, path: Option<&str>) -> Option<String> {
    if name.contains('/') {
        // A name with a slash is answered as given, and merely has to EXIST --
        // `DO_ABS` stats it and does not ask what it is, so a directory answers.
        return sh.resolve(name).exists().then(|| name.to_string());
    }
    let owned;
    let path = match path {
        Some(p) => p,
        None => {
            owned = sh.get_var("PATH").unwrap_or_default();
            &owned
        }
    };
    for dir in path.split(':') {
        let joined = if dir.is_empty() {
            name.to_string()
        } else {
            format!("{dir}/{name}")
        };
        if sh.resolve(&joined).is_file() {
            return Some(joined);
        }
    }
    None
}

/// One name's line of ash's `describe_command` (ash.c:8734), with its status.
/// Keywords first, then aliases, then the command lookup -- so `alias q=w` on top
/// of a function named `q` reports the alias, and no alias can hide `while`.
fn describe_one(sh: &Shell, name: &str, verbose: bool, path: Option<&str>) -> (String, i32) {
    let mut text = String::new();
    if verbose {
        text.push_str(name);
    }
    if crate::parser::is_reserved(name) {
        text.push_str(if verbose { " is a shell keyword" } else { name });
    } else if let Some(value) = sh.aliases.get(name) {
        if !verbose {
            // The brief form prints a definition that can be read back, and
            // returns without the trailing name the other answers carry.
            return (format!("alias {name}={}\n", single_quote(value)), 0);
        }
        text.push_str(" is an alias for ");
        text.push_str(value);
    } else if sh.funcs.contains_key(name) {
        text.push_str(if verbose { " is a function" } else { name });
    } else if lookup(name).is_some() {
        if verbose {
            text.push_str(if is_ash_special_word(name) {
                " is a special shell builtin"
            } else {
                " is a shell builtin"
            });
        } else {
            text.push_str(name);
        }
    } else if let Some(found) = describe_path(sh, name, path) {
        if verbose {
            text.push_str(" is ");
        }
        text.push_str(&found);
    } else {
        // The verbose form says so on STDOUT, not stderr, and the brief form says
        // nothing at all; both are 127.
        let text = if verbose {
            format!("{name}: not found\n")
        } else {
            String::new()
        };
        return (text, 127);
    }
    text.push('\n');
    (text, 0)
}

/// Describe each name, answering the worst status any of them gave.
fn describe(sh: &mut Shell, names: &[String], verbose: bool, path: Option<&str>) -> R<()> {
    let mut text = String::new();
    let mut code = 0;
    for name in names {
        let (line, one) = describe_one(sh, name, verbose, path);
        text.push_str(&line);
        code |= one;
    }
    if text.is_empty() {
        return status(sh, code);
    }
    out(sh, text.as_bytes())?;
    // ash ORS a write failure into whatever the description answered
    // (`exitstatus |= ferror(stdout)`), so `type cd >&-` is 1 while `type zz >&-`
    // is still 127 -- the failure does not outrank the answer, it joins it.
    let write_failed = i32::from(sh.status != 0);
    sh.set_status(code | write_failed);
    Ok(())
}

/// `type name...`: ash's `typecmd`. A FIRST argument beginning with `-` turns the
/// verbose wording off and is otherwise ignored -- ash never asks WHICH option it
/// was, so `type -p` and `type -x` are the same request.
fn type_of(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut i = 1usize;
    let mut verbose = true;
    if argv.get(1).is_some_and(|a| a.starts_with('-')) {
        i = 2;
        verbose = false;
    }
    describe(sh, argv.get(i..).unwrap_or(&[]), verbose, None)
}

/// `command name args`: run `name` as an external/builtin, bypassing functions.
/// The `-v`/`-V` query forms are supported enough for scripts that probe.
fn command(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut i = 1usize;
    let mut exec_path = false;
    // ash's `evalcommand` loop collapses `command` wrappers before dispatching, and
    // the path `-p` sets persists across them into the EXECUTION lookup -- while a
    // query option ends the walk and `commandcmd` re-parses from THERE with its own
    // fresh path. That asymmetry is why `command -p command X` refuses to run an X
    // that is only on PATH, and `command -p command -v X` reports one.
    let (query, verbose, query_path) = loop {
        let mut query = false;
        let mut verbose = false;
        let mut level_path = false;
        // `nextopt("pvV")`: letters cluster in one word, `--` ends them, a bare `-`
        // is an operand. An unknown letter is a usage error -- but `command` is a
        // REGULAR builtin, so it is status 2 and the shell lives, unlike `export`'s.
        while let Some(arg) = argv.get(i) {
            let Some(opts) = arg.strip_prefix('-').filter(|o| !o.is_empty()) else {
                break;
            };
            i += 1;
            if opts == "-" {
                break;
            }
            // A plain loop, not a search combinator: this source is embedded
            // verbatim in the td-sh recipe, whose ladder guard rejects that tool's
            // bare name as a token (see the note in arith.rs).
            let mut bad = None;
            for c in opts.chars() {
                match c {
                    // `-V` only ever SETS the verbose wording, so `-vV` and `-Vv`
                    // are both verbose: ash ors the bit in and `-v` never clears it.
                    'v' => query = true,
                    'V' => {
                        query = true;
                        verbose = true;
                    }
                    'p' => level_path = true,
                    _ => {
                        bad = Some(c);
                        break;
                    }
                }
            }
            if let Some(c) = bad {
                err_line(sh, &format!("command: illegal option -{c}"));
                return status(sh, 2);
            }
        }
        exec_path |= level_path;
        if !query && argv.get(i).is_some_and(|n| n == "command") {
            i += 1;
            continue;
        }
        break (query, verbose, level_path);
    };
    let rest: Vec<String> = argv.iter().skip(i).cloned().collect();
    let Some(name) = rest.first() else {
        return ok(sh);
    };
    if query {
        // Only the FIRST operand is described; the rest are ash's `argptr` tail and
        // go unread.
        let one = [name.clone()];
        let path = query_path.then_some(crate::process::DEFAULT_UTILITY_PATH);
        return describe(sh, &one, verbose, path);
    }
    // `-p` moves only the LOOKUP, and it has to move the query and the execution
    // together at one level, or `command -pv` describes what `command -p` will not
    // run.
    let path = exec_path.then_some(crate::process::DEFAULT_UTILITY_PATH);
    if let Some(bi) = lookup(name) {
        // POSIX: `command` strips a builtin's special properties, which in dash
        // means the builtin runs inside a scratch local frame -- which is why
        // `command local s=1` leaves `s` as it was while a bare `local s=1` does
        // not. Wrapping every builtin is a no-op by construction: declaring into a
        // frame is what `local` is, so nothing else can observe one.
        let mark = sh.pending_unwind.len();
        let saved = std::mem::take(&mut sh.locals);
        let result = run(sh, bi, &rest);
        if result.as_ref().err().is_some_and(exec::terminating) {
            // The scratch frame follows what it wraps: on the way out it stays
            // standing for the EXIT trap, like any other frame.
            exec::defer_locals(sh);
        } else {
            // Load-bearing inside an EXIT trap, which runs with the dying frame
            // still deferred: draining to THIS mark takes off only what the
            // scratch frame contains and leaves that outer frame standing. Anything
            // deferred inside is newer than the scratch frame and must come off
            // first, or the frame's saved values are stale.
            exec::unwind_pending_to(sh, mark);
            exec::pop_locals(sh);
        }
        sh.locals = saved;
        return result;
    }
    crate::process::exec_external(sh, &rest, path, "", None)
}

// ---- test / [ ------------------------------------------------------------

fn test(sh: &mut Shell, argv: &[String]) -> R<()> {
    // For `[`, the final argument must be `]`.
    let is_bracket = argv.first().map(String::as_str) == Some("[");
    let mut args: Vec<String> = argv.iter().skip(1).cloned().collect();
    if is_bracket {
        match args.pop() {
            Some(ref last) if last == "]" => {}
            _ => {
                err_line(sh, "[: missing `]'");
                return status(sh, 2);
            }
        }
    }
    match eval_test(sh, &args) {
        Ok(true) => status(sh, 0),
        Ok(false) => status(sh, 1),
        Err(msg) => {
            err_line(sh, &format!("test: {msg}"));
            status(sh, 2)
        }
    }
}

fn eval_test(sh: &Shell, args: &[String]) -> Result<bool, String> {
    match args.len() {
        0 => Ok(false),
        1 => Ok(!s(args, 0).is_empty()),
        2 => two_arg(sh, args),
        3 => three_arg(sh, args),
        // `!` negates the 3-argument test -- except over `-a`/`-o`, which it binds
        // TIGHTER than, so `[ ! x -a "" ]` is `(!x) && ""`. The general parser
        // below has that precedence already.
        4 if s(args, 0) == "!" && !matches!(s(args, 2), "-a" | "-o") => {
            Ok(!three_arg(sh, args.get(1..).unwrap_or(&[]))?)
        }
        _ => {
            let mut p = TestParser { args, pos: 0, depth: 0, sh };
            let v = p.or_expr()?;
            if p.pos != args.len() {
                return Err(format!("unexpected argument `{}`", s(args, p.pos)));
            }
            Ok(v)
        }
    }
}

fn two_arg(sh: &Shell, args: &[String]) -> Result<bool, String> {
    let op = s(args, 0);
    if op == "!" {
        return Ok(s(args, 1).is_empty());
    }
    if let Some(unary) = op.strip_prefix('-') {
        if unary.len() == 1 {
            return unary_op(sh, unary, s(args, 1));
        }
    }
    Err(format!("unary operator expected: `{op}`"))
}

fn three_arg(sh: &Shell, args: &[String]) -> Result<bool, String> {
    let op = s(args, 1);
    if is_binary(op) {
        return binary_op(sh, s(args, 0), op, s(args, 2));
    }
    if s(args, 0) == "!" {
        return Ok(!two_arg(sh, args.get(1..).unwrap_or(&[]))?);
    }
    if s(args, 0) == "(" && s(args, 2) == ")" {
        return Ok(!s(args, 1).is_empty());
    }
    // POSIX leaves `A -a B` / `A -o B` unspecified; combine them as non-empty
    // string tests. This binds LAST: `!` and `( )` above claim their operands
    // first, and a leading unary operator claims the next word (dash-style), so
    // `[ ! -a B ]` and `[ -z -a ] ]` stay the syntax errors dash reports.
    if (op == "-a" || op == "-o") && !is_unary(s(args, 0)) {
        let (l, r) = (!s(args, 0).is_empty(), !s(args, 2).is_empty());
        return Ok(if op == "-a" { l && r } else { l || r });
    }
    Err(format!("binary operator expected: `{op}`"))
}

// The `-X` spellings `unary_op` understands.
fn is_unary(w: &str) -> bool {
    matches!(w.strip_prefix('-'), Some(u) if u.len() == 1 && "znefdrwxshLtbcpSugk".contains(u))
}

/// Fetch an argument, defaulting to the empty string past the end.
fn s(args: &[String], i: usize) -> &str {
    args.get(i).map(String::as_str).unwrap_or("")
}

// `==` is deliberately absent: it is a bash/ksh spelling that dash rejects as a
// missing binary operator (status 2), which is what the corpus goldens expect.
fn is_binary(op: &str) -> bool {
    matches!(
        op,
        "=" | "!=" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" | "<" | ">" | "-nt" | "-ot" | "-ef"
    )
}

pub fn binary_op(sh: &Shell, a: &str, op: &str, b: &str) -> Result<bool, String> {
    match op {
        "=" => Ok(a == b),
        "!=" => Ok(a != b),
        "<" => Ok(a < b),
        ">" => Ok(a > b),
        "-nt" | "-ot" | "-ef" => Ok(file_cmp(sh, a, op, b)),
        _ => {
            let x = int_arg(a)?;
            let y = int_arg(b)?;
            Ok(match op {
                "-eq" => x == y,
                "-ne" => x != y,
                "-lt" => x < y,
                "-le" => x <= y,
                "-gt" => x > y,
                "-ge" => x >= y,
                _ => return Err(format!("unknown operator `{op}`")),
            })
        }
    }
}

fn int_arg(s: &str) -> Result<i64, String> {
    s.trim()
        .parse::<i64>()
        .map_err(|_| format!("integer expression expected: `{s}`"))
}

pub fn unary_op(sh: &Shell, op: &str, arg: &str) -> Result<bool, String> {
    use std::os::unix::fs::FileTypeExt;
    let path = || sh.resolve(arg);
    Ok(match op {
        "z" => arg.is_empty(),
        "n" => !arg.is_empty(),
        "e" => path().symlink_metadata().is_ok(),
        "f" => path().is_file(),
        "d" => path().is_dir(),
        "r" => path().exists(),
        "w" => path().exists() && !read_only(&path()),
        "x" => is_executable(&path()),
        "s" => path().metadata().map(|m| m.len() > 0).unwrap_or(false),
        "h" | "L" => path()
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "b" => path().metadata().is_ok_and(|m| m.file_type().is_block_device()),
        "c" => path().metadata().is_ok_and(|m| m.file_type().is_char_device()),
        "p" => path().metadata().is_ok_and(|m| m.file_type().is_fifo()),
        "S" => path().metadata().is_ok_and(|m| m.file_type().is_socket()),
        "u" => mode_bit(&path(), 0o4000),
        "g" => mode_bit(&path(), 0o2000),
        "k" => mode_bit(&path(), 0o1000),
        // `-t FD`: a non-numeric operand is an error, not false. Only the three
        // standard descriptors can be probed without a raw isatty(3).
        "t" => match arg.trim().parse::<i64>() {
            Ok(n) => u32::try_from(n).is_ok_and(|fd| sh.fds.is_terminal(fd)),
            Err(_) => return Err(format!("integer expression expected: `{arg}`")),
        },
        _ => return Err(format!("unknown unary operator `-{op}`")),
    })
}

fn mode_bit(p: &std::path::Path, bit: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    p.metadata().is_ok_and(|m| m.mode() & bit != 0)
}

// `-nt`/`-ot` compare modification times; `-ef` is identity (same device and
// inode). An operand that cannot be stat'd makes the test false, as in dash.
fn file_cmp(sh: &Shell, a: &str, op: &str, b: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(x), Ok(y)) = (sh.resolve(a).metadata(), sh.resolve(b).metadata()) else {
        return false;
    };
    // Whole seconds, as dash's newerf/olderf compare st_mtime.
    let (xt, yt) = (x.mtime(), y.mtime());
    match op {
        "-nt" => xt > yt,
        "-ot" => xt < yt,
        _ => x.dev() == y.dev() && x.ino() == y.ino(),
    }
}

fn read_only(p: &std::path::Path) -> bool {
    p.metadata().map(|m| m.permissions().readonly()).unwrap_or(false)
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Cap on `test` expression nesting (`! ! ! …`, `( ( ( … ) ) )`), each level of which
/// recurses through `term`. Bounds the recursion so a pathological argv errors instead
/// of overflowing the stack. Far beyond any real `test` invocation.
const MAX_TEST_DEPTH: u32 = 256;

/// The recursive `test` grammar for 5+ arguments: `-o` (or) of `-a` (and) of
/// `!`/`(...)`/primary.
struct TestParser<'a> {
    args: &'a [String],
    pos: usize,
    depth: u32,
    sh: &'a Shell,
}

impl TestParser<'_> {
    fn peek(&self) -> Option<&str> {
        self.args.get(self.pos).map(String::as_str)
    }

    fn or_expr(&mut self) -> Result<bool, String> {
        let mut v = self.and_expr()?;
        while self.peek() == Some("-o") {
            self.pos += 1;
            let rhs = self.and_expr()?;
            v = v || rhs;
        }
        Ok(v)
    }

    fn and_expr(&mut self) -> Result<bool, String> {
        let mut v = self.term()?;
        while self.peek() == Some("-a") {
            self.pos += 1;
            let rhs = self.term()?;
            v = v && rhs;
        }
        Ok(v)
    }

    fn term(&mut self) -> Result<bool, String> {
        // Inc-on-enter/dec-on-exit tracks true nesting depth (`!`/`(`), so a long flat
        // `-a`/`-o` list does not accumulate toward the cap.
        self.depth += 1;
        if self.depth > MAX_TEST_DEPTH {
            self.depth -= 1;
            return Err("expression nested too deeply".into());
        }
        let r = self.term_inner();
        self.depth -= 1;
        r
    }

    fn term_inner(&mut self) -> Result<bool, String> {
        match self.peek() {
            Some("!") => {
                self.pos += 1;
                Ok(!self.term()?)
            }
            Some("(") => {
                self.pos += 1;
                let v = self.or_expr()?;
                if self.peek() != Some(")") {
                    return Err("missing `)'".into());
                }
                self.pos += 1;
                Ok(v)
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Result<bool, String> {
        let a = self.peek().unwrap_or("").to_string();
        // Unary op: `-z STR`, `-f FILE`, …
        if let Some(u) = a.strip_prefix('-') {
            if is_unary(&a) {
                let arg = self.args.get(self.pos + 1).map(String::as_str).unwrap_or("");
                self.pos += 2;
                return unary_op(self.sh, u, arg);
            }
        }
        // Binary op: `A OP B`.
        if let Some(op) = self.args.get(self.pos + 1).map(String::as_str) {
            if is_binary(op) {
                let b = self.args.get(self.pos + 2).map(String::as_str).unwrap_or("");
                let v = binary_op(self.sh, &a, op, b)?;
                self.pos += 3;
                return Ok(v);
            }
        }
        // Bare string: true when non-empty.
        self.pos += 1;
        Ok(!a.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use crate::process::{run_capturing, run_capturing_bytes};

    /// `/proc/<pid>/stat`'s clock fields, which `times` reads. The offsets are
    /// pinned against a real line because getting them wrong is not a crash: it
    /// reports some OTHER field as a time, and the machine that is idle enough
    /// to print zeros everywhere agrees with the right answer.
    #[test]
    fn the_cpu_fields_are_the_ones_times_reports() {
        // A real line, with utime/stime/cutime/cstime set to distinguishable
        // values (fields 14-17).
        let stat = "42 (td-sh) S 1 42 42 0 -1 4194304 982 0 0 0 \
                    11 22 33 44 20 0 1 0 12345 0 0";
        assert_eq!(super::cpu_ticks(stat), Some([11, 22, 33, 44]));
        // Field 2 is the executable name and may contain SPACES and even a
        // `)`, so the split is after the LAST one. Counting fields from the
        // start of the line reads four wrong numbers here and no error.
        let odd = "42 (od d) ick) S 1 42 42 0 -1 4194304 982 0 0 0 \
                   11 22 33 44 20 0 1 0 12345 0 0";
        assert_eq!(super::cpu_ticks(odd), Some([11, 22, 33, 44]));
        // A truncated line is None rather than a partial answer.
        assert_eq!(super::cpu_ticks("42 (td-sh) S 1 2 3"), None);
        assert_eq!(super::cpu_ticks("no parens here"), None);
        // USER_HZ is 100, so a tick is 10ms, and the format is ash's.
        let hz = super::USER_HZ;
        assert_eq!(super::format_ticks(0, hz), "0m0.000s");
        assert_eq!(super::format_ticks(1, hz), "0m0.010s");
        assert_eq!(super::format_ticks(100, hz), "0m1.000s");
        assert_eq!(super::format_ticks(6035, hz), "1m0.350s");
        assert_eq!(super::format_ticks(6000, hz), "1m0.000s");
        // The scale, at the tick rate that tells the two spellings apart. Every
        // arch td targets is 100; alpha and ia64 are 1024, where dividing 1000
        // by the rate FIRST is 0 and the fraction vanishes.
        assert_eq!(super::format_ticks(512, 1024), "0m0.500s");
        assert_eq!(super::format_ticks(1024, 1024), "0m1.000s");
        // The ORDER of the four, which nothing observable pins on an idle
        // machine: the shell's own user and system first, then its reaped
        // children's. Swap either pair and every live check still agrees,
        // because all four are 0 wherever the gate runs.
        assert_eq!(
            super::times_text([11, 22, 33, 44]),
            "0m0.110s 0m0.220s\n0m0.330s 0m0.440s\n"
        );
    }

    /// `times` prints two lines of two fields; the live values are checked by
    /// hand against ash under load, since a gate machine's are all zero.
    #[test]
    fn times_prints_two_lines_of_two() {
        let (status, out, err) = run_capturing("times");
        assert_eq!((status, err.as_str()), (0, ""));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "out: {out:?}");
        for l in lines {
            let fields: Vec<&str> = l.split_whitespace().collect();
            assert_eq!(fields.len(), 2, "line: {l:?}");
            for f in fields {
                assert!(
                    f.ends_with('s') && f.contains('m') && f.contains('.'),
                    "field: {f:?}"
                );
            }
        }
    }

    /// Parse one of `times`'s own fields back into ticks, so a test can compare
    /// two of them.
    fn ticks_of(field: &str) -> u64 {
        let (m, rest) = field.split_once('m').unwrap();
        let (s, frac) = rest.trim_end_matches('s').split_once('.').unwrap();
        let secs: u64 = m.parse::<u64>().unwrap() * 60 + s.parse::<u64>().unwrap();
        secs * super::USER_HZ + frac.parse::<u64>().unwrap() * super::USER_HZ / 1000
    }

    /// WHOSE cpu time `times` reports. `/proc/self/stat` is the whole process's;
    /// `/proc/thread-self/stat` is the calling thread's, and both exist, both
    /// parse, and both print a plausible answer. The difference only shows where
    /// td-sh differs from a forking shell: a pipeline stage is a THREAD here, so
    /// reading the thread's own file would report a stage's work as nobody's.
    /// That is what `times` is for, and ash -- which forks -- gets it right by
    /// construction.
    #[test]
    fn a_pipeline_stages_cpu_time_is_the_shells_cpu_time() {
        // ~300ms of stage work, against a 10ms tick. The main thread spends
        // microseconds spawning and joining, so a thread-self read moves by 0.
        let (st, out, err) = run_capturing(
            "times\n\
             i=0; while [ $i -lt 60000 ]; do i=$((i+1)); done | :\n\
             times",
        );
        assert_eq!((st, err.as_str()), (0, ""));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "out: {out:?}");
        let user = |l: &str| ticks_of(l.split_whitespace().next().unwrap());
        let (before, after) = (user(lines[0]), user(lines[2]));
        assert!(
            after >= before + 10,
            "a stage burned ~30 ticks and `times` saw {}: {out:?}",
            after - before
        );
    }

    /// `times` is a special builtin, but a missing `/proc` is not a usage error
    /// and must not end the script: ash reads `times(2)` and cannot fail at all,
    /// so a shell that dies here is one no ash script is written for. The gate
    /// has a `/proc`, so the arm is reachable only through `times_out`.
    #[test]
    fn times_without_proc_reports_and_carries_on() {
        let mut sh = crate::exec::Shell::new_for_test();
        assert!(super::times_out(&mut sh, None).is_ok());
        assert_eq!(sh.status, 1);
        // And a write failure reaches `$?` rather than being overwritten by a
        // hardcoded success -- `times >&-` is 1 in ash and was 0 here.
        let (st, out, _) = run_capturing("times >&-; echo st=$?");
        assert_eq!((st, out.as_str()), (0, "st=1\n"));
    }

    /// A name `lookup` answers to but `NAMES` omits is a builtin Tab never
    /// offers, and one in `NAMES` alone is a candidate that does not run.
    /// Neither is a compile error, so the two lists are compared as text.
    #[test]
    fn the_completion_names_are_the_names_lookup_answers_to() {
        const SRC: &str = include_str!("builtin.rs");
        let body = SRC
            .split_once("pub fn lookup(")
            .and_then(|(_, r)| r.split_once("_ => return None"))
            .map(|(b, _)| b)
            .unwrap();
        let mut arms: Vec<&str> = Vec::new();
        let mut rest = body;
        while let Some((_, after)) = rest.split_once('"') {
            let (lit, tail) = after.split_once('"').unwrap();
            arms.push(lit);
            rest = tail;
        }
        assert!(arms.len() > 20, "the match arms did not parse out of the source");
        arms.sort_unstable();
        let mut names = super::NAMES.to_vec();
        names.sort_unstable();
        assert_eq!(arms, names, "`lookup`'s arms and `NAMES` disagree");
    }

    #[test]
    fn echo_joins_with_spaces_and_newline() {
        let (_, out, _) = run_capturing("echo a b c");
        assert_eq!(out, "a b c\n");
        let (_, out, _) = run_capturing("echo -n hi");
        assert_eq!(out, "hi");
    }

    #[test]
    fn echo_expands_escapes_only_under_dash_e() {
        // ash's echo is busybox's, not dash's: escapes are OFF until `-e`.
        assert_eq!(run_capturing(r#"echo "a\tb""#).1, "a\\tb\n");
        assert_eq!(run_capturing(r#"echo -e "a\tb""#).1, "a\tb\n");
        // `-E` is accepted and does nothing -- it never CLEARS the flag, so this
        // still interprets. The obvious reading (bash's) would print `a\tb`.
        assert_eq!(run_capturing(r#"echo -e -E "a\tb""#).1, "a\tb\n");
        // Clusters, and flags accumulating across words.
        assert_eq!(run_capturing(r#"echo -en "a\tb""#).1, "a\tb");
        assert_eq!(run_capturing(r#"echo -n -e "a\tb""#).1, "a\tb");
        // One bad letter makes the whole word an operand, and a bare `-` or `--`
        // is an operand rather than a flag or a terminator.
        assert_eq!(run_capturing("echo -ez foo").1, "-ez foo\n");
        assert_eq!(run_capturing("echo --").1, "--\n");
        assert_eq!(run_capturing("echo -").1, "-\n");
        // Octal with and without the `\0` marker, and hex, which dash's `%b` has
        // no notion of.
        assert_eq!(run_capturing(r#"echo -e "\0101\101\x41""#).1, "AAA\n");
        // A digit that would carry past a byte ends the run and stays as text.
        assert_eq!(run_capturing(r#"echo -e "\0400""#).1, " 0\n");
        // `\x` with no hex digit is literal, and so is a partial run's tail.
        assert_eq!(run_capturing(r#"echo -e "\x""#).1, "\\x\n");
        assert_eq!(run_capturing(r#"echo -e "\x4z""#).1, "\u{4}z\n");
        // Hex takes at most TWO digits where octal takes three, and only a third
        // that WOULD have converted shows it: in `\x4z` the run ends at `z`
        // either way.
        assert_eq!(run_capturing(r#"echo -e "\x041""#).1, "\u{4}1\n");
        // 255 is the largest byte a run may reach, and only a run that lands on
        // it exactly tells `>` from `>=` -- `\0400` and `\777` break either way.
        // Bytes, not a lossy `String`: 0xff is not UTF-8.
        assert_eq!(
            run_capturing_bytes(r#"echo -e "\377|\xff""#).1,
            b"\xff|\xff\n"
        );
        // The `\0` marker is dropped before an OCTAL digit, not any digit: `\08`
        // is the NUL the bare `\0` converts to, then `8` as text.
        assert_eq!(run_capturing(r#"echo -e "\08""#).1, "\u{0}8\n");
        // Nothing to write is not a write, so a closed stdout is not an error
        // here -- unlike printf, which reports one.
        assert_eq!(run_capturing("echo -e '\\c' >&-; echo $?").1, "0\n");
        assert_eq!(run_capturing("echo -n '' >&-; echo $?").1, "0\n");
        assert_eq!(run_capturing("printf '' >&-; echo $?").1, "1\n");
        // `\e` is ESC here; `\u` is not an escape at all, unlike bash.
        assert_eq!(run_capturing(r#"echo -e "\e|\u6""#).1, "\u{1b}|\\u6\n");
        // An escape it does not know keeps its backslash.
        assert_eq!(run_capturing(r#"echo -e "\d\8""#).1, "\\d\\8\n");
        // `\c` ends the output where it stands -- newline and later operands too.
        assert_eq!(run_capturing(r#"echo -e "a\c" b; echo X"#).1, "aX\n");
        // Every `-n` is read, not just the first: the option loop keeps going
        // while a word is all flag letters.
        assert_eq!(run_capturing("echo -n -n").1, "");
    }

    #[test]
    fn test_string_and_integer_comparisons() {
        assert_eq!(run_capturing("[ abc = abc ]; echo $?").1, "0\n");
        assert_eq!(run_capturing("[ abc = abd ]; echo $?").1, "1\n");
        assert_eq!(run_capturing("[ 3 -lt 5 ]; echo $?").1, "0\n");
        assert_eq!(run_capturing("[ 5 -lt 5 ]; echo $?").1, "1\n");
        assert_eq!(run_capturing("[ -z '' ]; echo $?").1, "0\n");
        assert_eq!(run_capturing("[ -n x ]; echo $?").1, "0\n");
        assert_eq!(run_capturing("[ ! -n '' ]; echo $?").1, "0\n");
    }

    #[test]
    fn test_and_or_precedence() {
        assert_eq!(run_capturing("[ 1 -eq 1 -a 2 -eq 2 ]; echo $?").1, "0\n");
        assert_eq!(run_capturing("[ 1 -eq 1 -o 2 -eq 3 ]; echo $?").1, "0\n");
        assert_eq!(run_capturing("[ 1 -eq 2 -a 2 -eq 2 ]; echo $?").1, "1\n");
    }

    #[test]
    fn deeply_nested_test_expression_errors_instead_of_overflowing() {
        // `test ! ! ! … x` recurses through `term`; the depth cap must turn a
        // pathological expression into a status-2 error, not a stack overflow.
        let mut src = String::from("test");
        for _ in 0..5000 {
            src.push_str(" !");
        }
        src.push_str(" x; echo $?");
        assert_eq!(run_capturing(&src).1, "2\n");
    }

    #[test]
    fn set_replaces_positional_parameters() {
        assert_eq!(run_capturing("set -- a b c; echo $#").1, "3\n");
        assert_eq!(run_capturing("set -- a b c; echo $1 $3").1, "a c\n");
    }

    #[test]
    fn shift_drops_leading_params() {
        assert_eq!(run_capturing("set -- a b c d; shift 2; echo $# $1").1, "2 c\n");
    }

    #[test]
    fn read_splits_into_named_variables() {
        let (_, out, _) = run_capturing("printf 'x y z\\n' | { read a b; echo \"$a|$b\"; }");
        assert_eq!(out, "x|y z\n");
    }

    #[test]
    fn local_shadows_and_is_restored_on_return() {
        let (_, out, _) = run_capturing(
            "x=g; y=g; f() { local x=l y; y=mut; echo \"in $x $y\"; }; f; echo \"out $x $y\"",
        );
        // `local y` starts y UNSET, so `y=mut` is what it shows -- and neither
        // that write nor the shadowing of x escapes the call.
        assert_eq!(out, "in l mut\nout g g\n");
        // A name that did not exist before is removed again, not left empty.
        let (_, out, _) = run_capturing("f() { local n=1; }; f; echo \"[${n-unset}]\"");
        assert_eq!(out, "[unset]\n");
    }

    #[test]
    fn a_bare_local_leaves_the_name_unset() {
        // Every name starts unset, whether or not it existed; `set -u` is then
        // fatal. What it HELD is the case `a_valueless_local_starts_the_name_unset`
        // covers; this one is the name that never existed.
        let (_, out, _) = run_capturing("f() { local x; echo \"[${x-default}]\"; }; f");
        assert_eq!(out, "[default]\n");
        let (status, out, _) = run_capturing("set -u; f() { local x; echo $x; }; f");
        assert_eq!((status, out.as_str()), (2, ""));
    }

    #[test]
    fn unset_without_f_never_touches_a_function() {
        // Both references gate the function on `-f` alone -- the same three lines
        // in ash.c:14207 and dash's var.c:595. POSIX leaves the no-variable case
        // unspecified and bash unsets the function there; the chain does not.
        for (src, want) in [
            ("a() { echo FUNC; }; unset a; a", "FUNC\n"),
            ("a=v; a() { echo FUNC; }; unset a; a; echo \"[${a-U}]\"", "FUNC\n[U]\n"),
            ("a=v; a() { echo FUNC; }; unset -v a; a; echo \"[${a-U}]\"", "FUNC\n[U]\n"),
            // `-f` still takes it, and the LAST flag wins as the option loop keeps it.
            ("a() { echo FUNC; }; unset -f a; a; echo done", "done\n"),
            ("a() { echo FUNC; }; unset -v -f a; a; echo done", "done\n"),
            ("a() { echo FUNC; }; unset -f -v a; a", "FUNC\n"),
        ] {
            let (_, out, err) = run_capturing(src);
            assert_eq!(out, want, "{src}: {err}");
        }
    }

    #[test]
    fn unsetting_a_localised_name_keeps_its_attributes() {
        // ash frees the `struct var` only when it has no `VSTRFIXED`
        // (ash.c:2440), and `mklocal` sets exactly that -- so `unset` on a name the
        // frame declared leaves the entry standing, the same state a bare `local`
        // leaves, while `unset` on a global takes the attribute with the name.
        // Only a child sees the difference; the in-process half is that both read
        // as absent.
        for (src, want) in [
            ("a=A; f() { local a=L; unset a; echo \"[${a-U}]\"; }; f; echo \"after[$a]\"",
             "[U]\nafter[A]\n"),
            ("a=A; f() { local a; unset a; echo \"[${a-U}]\"; }; f; echo \"after[$a]\"",
             "[U]\nafter[A]\n"),
            ("export a=A; unset a; echo \"[${a-U}]\"", "[U]\n"),
        ] {
            let (_, out, err) = run_capturing(src);
            assert_eq!(out, want, "{src}: {err}");
        }
    }

    #[test]
    fn an_inner_frame_sees_an_outer_frames_declaration() {
        // ash keeps the answer on the variable (`VSTRFIXED`, ash.c:10020), not per
        // frame, so a function called from one that localised the name still reads
        // it as declared -- while `local` itself must NOT, or the inner
        // declaration is skipped and never restored.
        for (src, want) in [
            // The inner `local` still shadows and still unwinds.
            ("a=G; i() { local a=I; echo \"i=$a\"; }; o() { local a=O; i; echo \"o=$a\"; }; o; echo \"g=$a\"",
             "i=I\no=O\ng=G\n"),
            // The inner `unset` leaves the entry standing, so the outer frame's
            // restore still has something to put back.
            ("a=G; i() { unset a; }; o() { local a=O; i; echo \"o=[${a-U}]\"; }; o; echo \"g=$a\"",
             "o=[U]\ng=G\n"),
        ] {
            let (_, out, err) = run_capturing(src);
            assert_eq!(out, want, "{src}: {err}");
        }
    }

    #[test]
    fn a_function_called_from_an_exit_trap_gets_its_own_frame() {
        // The trap runs inside the frame the shell died in, so a `local` at the
        // trap's TOP level repeats that frame's declaration -- but one inside a
        // function the trap calls is fresh, and has to unwind back to the dying
        // frame's value rather than leak.
        let (_, out, err) = run_capturing(
            "trap 'g; echo after=$x' EXIT; g() { local x=I; echo in=$x; }; f() { local x=F; exit 7; }; f",
        );
        assert_eq!(out, "in=I\nafter=F\n", "{err}");
    }

    #[test]
    fn unset_takes_clustered_options_and_a_double_dash() {
        // `nextopt("vf")`: flags cluster in one word with the last winning, `--`
        // ends them, and a bare `-` falls through to be a (bad) name.
        for (src, want) in [
            ("a=1; unset -- a; echo \"[${a-U}]\"", "[U]\n"),
            ("f() { echo FUNC; }; unset -fv f; f", "FUNC\n"),
            ("a=1; f() { echo FUNC; }; unset -vf a f; echo \"a=[$a]\"", "a=[1]\n"),
            ("a=1; unset -v -- a; echo \"[${a-U}]\"", "[U]\n"),
        ] {
            let (_, out, err) = run_capturing(src);
            assert_eq!(out, want, "{src}: {err}");
        }
        for bad in ["a=1; unset - a", "a=1; unset -x a", "a=1; unset -- -- a"] {
            let (status, _, err) = run_capturing(bad);
            assert_eq!(status, 2, "{bad}: {err}");
        }
    }

    #[test]
    fn unsetting_a_local_does_not_reveal_the_global() {
        // dash restores the outer binding only when the frame unwinds, so within
        // the call the name stays unset.
        let (_, out, _) =
            run_capturing("x=g; f() { local x=l; unset x; echo \"[${x-gone}]\"; }; f; echo $x");
        assert_eq!(out, "[gone]\ng\n");
    }

    #[test]
    fn a_valueless_local_starts_the_name_unset() {
        // ash's `mklocal` unsets the name outright ("local VAR unsets VAR");
        // dash leaves the outer value visible, and td-sh followed dash. ash is the
        // first reference, and every expectation here was read off it.
        for (src, want) in [
            ("a=A; f() { local a; echo \"[$a]\"; }; f; echo \"after[$a]\"", "[]\nafter[A]\n"),
            // Only the valueless form: `=` still assigns, and a later plain
            // assignment is unaffected.
            ("a=A; f() { local a=B; echo \"[$a]\"; }; f", "[B]\n"),
            ("a=A; f() { local a; a=C; echo \"[$a]\"; }; f; echo \"after[$a]\"", "[C]\nafter[A]\n"),
            // Every name in the list, not just the first.
            ("a=A; f() { local b a c; echo \"[$a][$b][$c]\"; }; f", "[][][]\n"),
            // A REPEAT declaration in the same frame is the one case ash skips, so
            // this must NOT unset what the first one assigned.
            ("x=0; f() { local x=1; echo $x; local x; echo $x; }; f; echo $x", "1\n1\n0\n"),
            // ...and a repeat that DOES carry a value still assigns.
            ("x=0; f() { local x; local x=9; echo \"[$x]\"; }; f; echo $x", "[9]\n0\n"),
            // A nested frame unsets what the outer frame localised.
            ("x=0; g() { local x; echo \"g[$x]\"; }; f() { local x=1; g; echo \"f[$x]\"; }; f",
             "g[]\nf[1]\n"),
        ] {
            let (_, out, err) = run_capturing(src);
            assert_eq!(out, want, "{src}: {err}");
        }
        // The name is gone, not merely empty -- so it leaves the environment too.
        let (_, out, _) = run_capturing("export a=A; f() { local a; echo \"[${a-UNSET}]\"; }; f");
        assert_eq!(out, "[UNSET]\n");
    }

    #[test]
    fn a_declared_but_unset_name_reads_as_absent() {
        // ash's `mklocal` clears the VALUE and keeps the `struct var`, so a
        // localised name reads unset while its attributes survive. Only a child can
        // see the attribute half; `a_localised_name_still_exports_once_assigned` in
        // tests/conformance.rs covers that. These are the in-process half.
        assert_eq!(run_capturing("export x=G; f() { local x; echo \"[${x-UNSET}]\"; }; f").1,
                   "[UNSET]\n");
        assert_eq!(run_capturing("export x=G; f() { local x; }; f; echo \"[$x]\"").1, "[G]\n");
        // The same state reached the other way: `export x` before any value.
        assert_eq!(run_capturing("export x; echo \"[${x-UNSET}]\"").1, "[UNSET]\n");
        assert_eq!(run_capturing("export x; x=H; echo \"[$x]\"").1, "[H]\n");
        // An attributes-only entry is still a VARIABLE, so `unset` takes it and
        // leaves a function of the same name alone.
        assert_eq!(run_capturing("export x; x() { echo fn; }; unset x; x").1, "fn\n");
    }

    #[test]
    fn a_repeat_local_is_recognised_across_a_fork_and_an_unwind() {
        // The "only the first declaration acts" rule is about the FRAME, so it has
        // to survive both ways td-sh moves one: a subshell forks the frame, and a
        // terminating unwind defers it for the EXIT trap. Miss either and the
        // repeat looks fresh and wrongly unsets. All measured on ash.
        for (src, want) in [
            ("x=G; f() { local x=F; (local x; echo \"${x-UNSET}\"); }; f", "F\n"),
            ("x=G; f() { local x=F; echo \"$(local x; echo ${x-UNSET})\"; }; f", "F\n"),
            ("x=G; trap 'local x; echo ${x-UNSET}' EXIT; f() { local x=F; exit 7; }; f", "F\n"),
            // A name the dead frame did NOT declare is still a fresh declaration.
            ("x=G; trap 'local y; echo ${y-UNSET}' EXIT; f() { local x=F; exit 7; }; f",
             "UNSET\n"),
            // A subshell's own `local` is still its own: it does not escape.
            ("x=G; f() { (local x=S; echo \"in[$x]\"); echo \"out[$x]\"; }; f", "in[S]\nout[G]\n"),
        ] {
            let (_, out, err) = run_capturing(src);
            assert_eq!(out, want, "{src}: {err}");
        }
    }

    #[test]
    fn a_prefix_assignment_is_already_a_declaration_in_the_frame() {
        // ash's `evalcommand` applies a call's prefix assignments with `mklocal`
        // INTO the frame it just pushed (ash.c:10497), so a valueless `local` for
        // that name in the body is a repeat and leaves the binding alone. td-sh
        // kept them in a list of their own, where the repeat check could not see
        // them; they are in the frame's list now. All measured on ash.
        for (src, want) in [
            ("a=A; f() { local a; echo \"[${a-U}]\"; }; a=B f; echo \"after[$a]\"",
             "[B]\nafter[A]\n"),
            ("a=A; f() { local a b; echo \"[${a-U}][${b-U}]\"; }; a=B b=C f", "[B][C]\n"),
            // The `=` form assigns over it, as it always did.
            ("f() { local a=C; echo \"[$a]\"; }; a=B f", "[C]\n"),
            // A name the call did NOT prefix is still a fresh declaration.
            ("a=A; f() { local a; echo \"[${a-U}]\"; }; b=B f", "[U]\n"),
            // The binding is still the frame's: gone after the call either way.
            ("a=A; f() { local a; }; a=B f; echo \"[$a]\"", "[A]\n"),
            ("a=A; f() { local a; a=X; }; a=B f; echo \"[$a]\"", "[A]\n"),
        ] {
            let (_, out, err) = run_capturing(src);
            assert_eq!(out, want, "{src}: {err}");
        }
    }

    #[test]
    fn a_bare_local_of_optind_restarts_getopts() {
        // ash reaches the unset through `unsetvar`, which fires `getoptsreset` and
        // moves the hidden cursor back to word 1. dash never unsets here at all, so
        // it resumes mid-scan; ash decides it.
        let (_, out, err) =
            run_capturing("f() { getopts ab o; local OPTIND; getopts ab o; echo $o; }; f -a -b");
        assert_eq!(out, "a\n", "{err}");
    }

    #[test]
    fn local_names_itself_when_a_readonly_rejects_it() {
        // Both references prefix the builtin here, and both forms reach it: the
        // valueless one because it now unsets, the `=` one because it assigns.
        // Plain assignment shares set_var's message and must NOT gain the prefix.
        // `; echo REACHED` is what tells a FATAL error from a status of 2: ash
        // ends the script here, so nothing after the call runs.
        for src in [
            "readonly r=1; f() { local r; }; f; echo REACHED",
            "readonly r=1; f() { local r=2; }; f; echo REACHED",
        ] {
            let (code, out, err) = run_capturing(src);
            assert_eq!((code, out.as_str()), (2, ""), "{src}: {err}");
            assert!(err.contains("local: r: is read only"), "{src}: {err}");
        }
        let (_, _, err) = run_capturing("readonly r=1; r=2");
        assert!(err.contains("r: is read only") && !err.contains("local:"), "{err}");
    }

    #[test]
    fn each_invocation_unwinds_only_its_own_locals() {
        let (_, out, _) = run_capturing(
            "x=0; f() { local x=$1; [ $1 -gt 0 ] && f $(($1 - 1)); echo $x; }; f 2; echo end=$x",
        );
        assert_eq!(out, "0\n1\n2\nend=0\n");
    }

    #[test]
    fn redeclaring_a_local_saves_the_outer_binding_once() {
        // Re-declaring keeps the FIRST save, so a `local` in a loop cannot grow
        // the frame — and the outer value still comes back intact.
        let (_, out, _) = run_capturing(
            "x=g; f() { i=0; while [ $i -lt 3 ]; do local x=$i; i=$((i+1)); done; echo in=$x; }; f; echo out=$x",
        );
        assert_eq!(out, "in=2\nout=g\n");
        // A second, valueless declaration assigns nothing, so the value survives.
        let (_, out, _) = run_capturing("f() { local foo=bar; local foo; echo \"[${foo-u}]\"; }; f");
        assert_eq!(out, "[bar]\n");
    }

    #[test]
    fn command_strips_locals_special_properties() {
        // dash runs a `command`-invoked builtin in a scratch frame, so the local
        // is gone the moment `command` returns -- while a bare `local` persists.
        let (_, out, _) = run_capturing("f() { command local s=1; echo \"[${s-gone}]\"; }; f");
        assert_eq!(out, "[gone]\n");
        let (_, out, _) = run_capturing("f() { local s=1; echo \"[${s-gone}]\"; }; f");
        assert_eq!(out, "[1]\n");
    }

    #[test]
    fn local_outside_a_function_is_fatal() {
        let (status, _, err) = run_capturing("local x=1; echo unreached");
        assert_eq!(status, 2);
        assert!(err.contains("not in a function"));
    }

    #[test]
    fn local_dash_saves_the_option_set() {
        // `local -` restores the flags on return, so `set -f` inside is undone.
        let (_, out, _) =
            run_capturing("f() { local -; set -f; echo in=$-; }; f; case $- in *f*) echo still;; *) echo restored;; esac");
        assert!(out.contains("restored"), "got {out:?}");
    }

    #[test]
    fn an_unknown_shell_option_is_refused_not_ignored() {
        // Silently accepting one is the dangerous answer: a script that asks for
        // `pipefail` and is told nothing runs WITHOUT it.
        for src in ["set -q", "set -o pipefail", "set +o pipefail"] {
            let (_, _, err) = run_capturing(src);
            assert!(err.contains("illegal option"), "{src}: {err:?}");
        }
        // Options dash HAS are accepted, whether or not td-sh acts on them, and a
        // bare `-o` is not an option name.
        for src in ["set -e -u -x", "set -a; set -n; set -m", "set -o noglob", "set -o"] {
            let (status, _, err) = run_capturing(src);
            assert_eq!((status, err.as_str()), (0, ""), "{src}");
        }
    }

    /// The whole roster, WHOLE: ash carries the builtin's name in its diagnostic
    /// PREFIX (`p.sh: cd: line 1: …`, ash.c:1417) and td-sh prints that middle
    /// field alone, so a site that regains a `td-sh: ` or loses its name is a
    /// mismatch `contains` cannot see -- nor can it see the capital.
    #[test]
    fn every_option_refusal_is_worded_and_graded_the_way_ashs_is() {
        // A FATAL refusal reaches no `echo`, so the two output columns are what
        // says which of the two kinds it was.
        for (src, err, out, code) in [
            ("jobs -Z", "jobs: illegal option -Z\n", "after=2\n", 0),
            ("wait -Z", "wait: illegal option -Z\n", "after=2\n", 0),
            ("read -Z", "read: illegal option -Z\n", "after=2\n", 0),
            ("unalias -Z", "unalias: illegal option -Z\n", "after=2\n", 0),
            ("cd -Z", "cd: illegal option -Z\n", "after=2\n", 0),
            ("umask -Z", "umask: illegal option -Z\n", "after=2\n", 0),
            ("pwd -Z", "pwd: illegal option -Z\n", "after=2\n", 0),
            ("command -x true", "command: illegal option -x\n", "after=2\n", 0),
            ("set -o bogus", "set: illegal option -o bogus\n", "after=1\n", 0),
            ("set +o bogus", "set: illegal option +o bogus\n", "after=1\n", 0),
            ("set -q", "set: illegal option -q\n", "", 2),
            ("unset -z", "unset: illegal option -z\n", "", 2),
            ("export -q", "export: illegal option -q\n", "", 2),
            ("readonly -q", "readonly: illegal option -q\n", "", 2),
            ("trap -Z", "trap: illegal option -Z\n", "", 2),
            (". -Z", ".: illegal option -Z\n", "", 2),
            ("source -Z", "source: illegal option -Z\n", "", 2),
            ("exec -Z", "exec: illegal option -Z\n", "", 2),
            // The option scan runs BEFORE the no-command check, so a bad one is
            // fatal even where `exec` would otherwise have done nothing.
            ("exec -aX -Z", "exec: illegal option -Z\n", "", 2),
            // A CLUSTER is read letter by letter, so the refusal names the one
            // it stopped on rather than the word that carried it -- whichever
            // position in the word the bad letter is in.
            ("cd -Zq", "cd: illegal option -Z\n", "after=2\n", 0),
            ("jobs -xy", "jobs: illegal option -x\n", "after=2\n", 0),
            ("jobs -pZ", "jobs: illegal option -Z\n", "after=2\n", 0),
            ("wait -xy", "wait: illegal option -x\n", "after=2\n", 0),
            // `exec` can only ever show the bad-first one: `a` eats its word.
            ("exec -Za", "exec: illegal option -Z\n", "", 2),
            // The one ash spells with a capital, and the one that carries no
            // name: a bare `fprintf` (ash.c:11714), not `nextopt`'s.
            ("set -- -Z; getopts a: o", "Illegal option -Z\n", "after=0\n", 0),
        ] {
            let (status, o, e) = run_capturing(&format!("{src}; echo after=$?"));
            assert_eq!((status, o.as_str(), e.as_str()), (code, out, err), "{src}");
        }
    }

    /// The other message each of those two functions carries, which splits the
    /// same way and had been copied the same way round: `nextopt`'s is lowercase
    /// with a name (ash.c:2045), `getopts`'s is a bare capitalised `fprintf`
    /// (11736). And what is NOT an option: a lone `-` ends them without being
    /// consumed, so it reaches the operand path, as does anything past a `--`.
    #[test]
    fn a_missing_option_argument_and_a_non_option_are_ashs_too() {
        for (src, err, out, code) in [
            ("read -p", "read: no arg for -p option\n", "after=2\n", 0),
            ("read -u", "read: no arg for -u option\n", "after=2\n", 0),
            // `exec`'s is `nextopt`'s too, and fatal because `exec` is special.
            ("exec -a", "exec: no arg for -a option\n", "", 2),
            ("set -- -a; getopts a: o", "No arg for -a option\n", "after=0\n", 0),
            ("wait -", "wait: Illegal number: -\n", "after=2\n", 0),
            ("wait -- -5", "wait: Illegal number: -5\n", "after=2\n", 0),
            // `999` is a number, so the scan has STOPPED by `-Z`; `wait x -Z`
            // would fail on `x` first and pin nothing.
            ("wait 999 -Z", "wait: Illegal number: -Z\n", "after=2\n", 0),
            // Still td-sh's word for an operand rather than ash's `no such
            // job`; what this pins is that `-` reaches that path at all.
            ("jobs -", "jobs: -: selecting a job is not supported\n", "after=2\n", 0),
            // `unaliascmd` returns on the `a` without reading the rest, so the
            // cluster's ORDER decides -- the one builtin where it does.
            ("alias q=x; unalias -Za", "unalias: illegal option -Z\n", "after=2\n", 0),
        ] {
            let (status, o, e) = run_capturing(&format!("{src}; echo after=$?"));
            assert_eq!((status, o.as_str(), e.as_str()), (code, out, err), "{src}");
        }
        // `--` still ends them, a repeated valid letter is still accepted, and
        // a silent `getopts` still reports no message.
        for src in ["wait --", "jobs --", "jobs -pp", "set -- -a; getopts :a: o"] {
            let (status, _o, e) = run_capturing(&format!("{src}; echo after=$?"));
            assert_eq!((status, e.as_str()), (0, ""), "{src}");
        }
        // The other side of `unalias`'s early return: a bad letter AFTER the
        // `a` is never read, so this clears and succeeds.
        let (_s, out, e) = run_capturing("alias q=x; unalias -aZ; alias; echo after=$?");
        assert_eq!((out.as_str(), e.as_str()), ("after=0\n", ""));
    }

    /// `trap`'s two refusals now agree with each other: the option one is
    /// `nextopt`'s and the condition one is `ash_msg`'s, and neither names the
    /// shell.
    #[test]
    fn traps_two_refusals_are_worded_alike() {
        for src in ["trap - -Z", "trap '' -Z"] {
            let (status, _o, e) = run_capturing(&format!("{src}; echo after=$?"));
            assert_eq!((status, e.as_str()), (0, "trap: -Z: invalid signal specification\n"));
        }
        let (_s, out, _e) = run_capturing("trap a BOGUS; echo after=$?");
        assert_eq!(out, "after=1\n");
    }

    #[test]
    fn a_bad_set_option_ends_the_script_only_in_its_letter_form() {
        // `-o NAME` is `ash_msg` (ash.c:11420), a message and 1; a LETTER is
        // `ash_msg_and_raise_error` (11445). `$?` is read DIRECTLY -- the
        // `|| true` the corpus case wraps it in would mask the 1 that is the
        // point (`builtin-special.test.sh`, whose `## N-I …ash` records ash
        // carrying on, runs the `-o` form).
        let (status, out, _) = run_capturing("set -q; echo reached");
        assert_eq!((status, out.as_str()), (2, ""));
        let (_, out, _) = run_capturing("set -o invalid_; echo reached=$?");
        assert_eq!(out, "reached=1\n");
        // Fatal even mixed in with valid letters, and even reached through
        // `||`, which is what "fails whole script" means.
        let (status, out, _) = run_capturing("set -eq || true; echo reached");
        assert_eq!((status, out.as_str()), (2, ""));
        // `shift` overrunning stays non-fatal -- `shiftcmd`'s bare `return 1`.
        let (_, out, _) = run_capturing("set -- a; shift 3; echo reached");
        assert_eq!(out, "reached\n");
    }

    #[test]
    fn a_bad_loop_count_is_fatal_so_the_loop_cannot_spin() {
        // Bounded loops on purpose: if the fatality is ever lost, this fails on the
        // output it collected instead of hanging the gate.
        let (status, out, err) =
            run_capturing("for i in 1 2 3; do echo hi; break oops; done; echo AFTER");
        assert_eq!((status, out.as_str()), (2, "hi\n"));
        assert!(err.contains("break: Illegal number: oops"), "{err:?}");
        let (status, out, err) =
            run_capturing("for i in 1 2 3; do echo hi; continue oops; done; echo AFTER");
        assert_eq!((status, out.as_str()), (2, "hi\n"));
        assert!(err.contains("continue: Illegal number: oops"), "{err:?}");
        // `is_all_digits` is busybox's rule, so a sign disqualifies either way even
        // though dash's `atomax10` accepts `+1`.
        for bad in ["0", "-1", "+1", "1x", "''", "' 1'"] {
            let (status, _, _) = run_capturing(&format!("for i in 1 2; do break {bad}; done"));
            assert_eq!(status, 2, "break {bad}");
        }
        // Same rule for `shift`. The INT_MAX bound is part of it: over it is a bad
        // NUMBER, not a huge count, so it must not reach the non-fatal overrun
        // branch below it.
        for bad in ["oops", "2147483648"] {
            let (status, out, err) =
                run_capturing(&format!("set -- a b; shift {bad}; echo AFTER"));
            assert_eq!((status, out.as_str()), (2, ""), "shift {bad}");
            assert!(err.contains(&format!("shift: Illegal number: {bad}")), "{err:?}");
        }
    }

    #[test]
    fn a_wide_status_is_narrowed_to_a_byte_as_ashs_uint8_t_is() {
        // `number()` accepts these, so what narrows them is the STORE, not the
        // parse. dash keeps 256/257/300/2147483647; ash is `uint8_t exitstatus`.
        for (operand, want) in
            [("255", "255"), ("256", "0"), ("257", "1"), ("300", "44"), ("2147483647", "255")]
        {
            let (_, out, _) =
                run_capturing(&format!("f() {{ return {operand}; }}; f; echo $?"));
            assert_eq!(out, format!("{want}\n"), "return {operand}");
        }
        // Everywhere `$?` can be reached from, not just a function return: a
        // subshell, a command substitution, a pipeline stage, and an EXIT trap.
        for src in [
            "(exit 300)",
            "x=$(exit 300)",
            "f() { return 300; }; (f)",
            "true | { exit 300; }",
            "echo x | { f() { return 300; }; f; }",
        ] {
            let (_, out, _) = run_capturing(&format!("{src}; echo $?"));
            assert_eq!(out, "44\n", "{src}");
        }
        let (_, out, _) = run_capturing("trap 'echo trap=$?' EXIT; exit 300");
        assert_eq!(out, "trap=44\n");
        // And the whole program's status, which `main` hands to the process.
        let (status, _, _) = run_capturing("exit 300");
        assert_eq!(status, 44);
    }

    #[test]
    fn exit_and_return_reject_what_number_rejects() {
        // Sign, non-digit and empty fail `number()` in both references; the two
        // over-INT_MAX operands are dash's rule, since ash's `atoi` overflows there.
        // These are special builtins, so the failure ends the script.
        for bad in ["-1", "-2", "abc", "1x", "''", "2147483648", "99999999999999999999"] {
            let (status, out, err) = run_capturing(&format!("exit {bad}; echo AFTER"));
            assert_eq!((status, out.as_str()), (2, ""), "exit {bad}");
            assert!(err.contains("exit: Illegal number:"), "exit {bad}: {err:?}");
            let (status, out, err) =
                run_capturing(&format!("f() {{ return {bad}; }}; f; echo AFTER"));
            assert_eq!((status, out.as_str()), (2, ""), "return {bad}");
            assert!(err.contains("return: Illegal number:"), "return {bad}: {err:?}");
        }
    }

    #[test]
    fn sourcing_a_missing_file_is_fatal() {
        // Both ash and dash raise for a file they cannot locate, rather than
        // returning a status ("This aborts if file isn't found, which is POSIXly
        // correct", as busybox puts it).
        let (status, out, err) = run_capturing(". /no/such/file/here; echo reached");
        assert_eq!((status, out.as_str()), (2, ""));
        assert!(err.contains("/no/such/file/here"), "{err:?}");
        // A MISSING OPERAND is not that: ash returns 2 and dash 0, and neither
        // stops the script.
        let (_, out, _) = run_capturing(".; echo reached");
        assert_eq!(out, "reached\n");
    }

    #[test]
    fn noexec_skips_evaluation_but_not_parsing() {
        // Both shells test nflag at the top of evaltree, so `-n` stops at the next
        // COMMAND -- including one later in the same list or inside a compound --
        // and the `set +n` that would undo it can never run.
        for src in [
            "echo a\nset -n\necho no\nset +n\necho no2",
            "echo a; set -n; echo no",
            "echo a; if true; then set -n; echo no; fi; echo no2",
            "echo a; set -n; eval \"echo no\"",
        ] {
            let (_, out, _) = run_capturing(src);
            assert_eq!(out, "a\n", "{src}");
        }
        // The point of the mode: the skipped units are still parsed, so their
        // syntax errors are still reported.
        let (status, _, err) = run_capturing("set -n\nfor");
        assert_eq!(status, 2);
        assert!(err.contains("syntax error"), "{err:?}");
    }

    #[test]
    fn an_o_inside_a_cluster_takes_the_next_argument() {
        // dash reads the cluster letter by letter and each `o` consumes one name,
        // so this is errexit plus nounset -- not an illegal option.
        let (status, out, err) = run_capturing("set -eo nounset\necho $-");
        assert_eq!((status, err.as_str()), (0, ""));
        assert_eq!(out, "eu\n");
        // Two of them consume two names, leaving nothing behind as a parameter.
        let (status, out, _) = run_capturing("set -oo errexit noglob\necho $-[$1]");
        assert_eq!((status, out.as_str()), (0, "ef[]\n"));
    }

    #[test]
    fn update_pwd_builds_the_logical_path_lexically() {
        let cases = [
            // (curdir, dir, want)
            ("/tmp", "x", "/tmp/x"),
            ("/tmp", "/etc", "/etc"),
            ("/tmp", ".", "/tmp"),
            ("/tmp", "./x/./y", "/tmp/x/y"),
            // The whole point: `..` is a string operation, so an intermediate
            // component that does not exist is still cancelled by it.
            ("/tmp", "nonexistent/..", "/tmp"),
            ("/tmp", "..", "/"),
            // `..` walks past where it started, up to the root and no further.
            ("/tmp", "../..", "/"),
            ("/", "..", "/"),
            ("/a/b/c", "../../..", "/"),
            ("/a/b", "../x", "/a/x"),
            // Empty components collapse, and a trailing slash is dropped.
            ("/tmp", "a//b/", "/tmp/a/b"),
            ("/", "tmp", "/tmp"),
            // Exactly two leading slashes are preserved and act as the floor.
            ("/tmp", "//", "//"),
            ("/tmp", "///", "/"),
            ("//", "..", "//"),
            ("//a", "..", "//"),
            // A relative move floors at `//` whenever curdir's SECOND byte is a
            // slash -- dash tests only that byte, so `///x` floors there too.
            ("///x", "..", "//"),
            ("///", "..", "//"),
        ];
        for (curdir, dir, want) in cases {
            // Compare the BYTES: `Path`'s PartialEq goes through components, so
            // it calls `/`, `//` and `///` equal and would not see a slash bug.
            let got = super::update_pwd(std::path::Path::new(curdir), dir);
            assert_eq!(got.as_os_str(), want, "{curdir} + {dir}");
        }
    }

    #[test]
    fn cd_reports_two_and_keeps_going() {
        // `cd` is a REGULAR builtin, so dash's sh_error is caught: the status is
        // 2 (not 1) and the script continues. builtin-cd pins it for dash AND ash.
        let (_, out, _) = run_capturing("cd /nonexistent/dir; echo status=$?");
        assert_eq!(out, "status=2\n");
        // A `..` that cancels a nonexistent component never touches the disk.
        let (_, out, _) = run_capturing("cd /; cd nonexistent_ZZ/..; echo status=$? $PWD");
        assert_eq!(out, "status=0 /\n");
    }

    #[test]
    fn cd_dash_prints_where_it_landed() {
        // dash sets CD_PRINT for `cd -`, and an unset OLDPWD leaves the empty
        // destination it treats as `.` -- a successful no-move, not an error.
        let (_, out, _) = run_capturing("cd /; cd /tmp; cd -");
        assert_eq!(out, "/\n");
        let (_, out, _) = run_capturing("cd - >/dev/null; echo status=$?");
        assert_eq!(out, "status=0\n");
    }

    #[test]
    fn cd_takes_l_and_p_and_a_double_dash() {
        let (_, out, _) = run_capturing("cd -- /; echo $PWD");
        assert_eq!(out, "/\n");
        let (_, out, _) = run_capturing("cd -L /; pwd");
        assert_eq!(out, "/\n");
        // dash's cdopt XORs on each CHANGE of letter, so this is back to logical.
        let (_, out, _) = run_capturing("cd -P -L /; pwd");
        assert_eq!(out, "/\n");
        let (status, _, err) = run_capturing("cd -q /");
        assert_eq!(status, 2);
        assert!(err.contains("illegal option"), "{err:?}");
    }

    #[test]
    fn cd_keeps_the_name_it_walked_but_children_get_the_real_path() {
        // The distinction only shows through a symlink: -L (the default) keeps
        // the name, so `..` undoes the link; -P resolves it first, so `..` is the
        // parent of the TARGET. builtin-cd pins this but cannot run -- it needs
        // `ln -s` -- so build the tree here instead.
        let base = std::env::temp_dir().join(format!("td-sh-cd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("real/sub")).unwrap();
        std::os::unix::fs::symlink(base.join("real/sub"), base.join("deep")).unwrap();
        // canonicalize: on some hosts the temp dir is itself a symlink, and the
        // physical answers below are canonical by construction.
        let b = base.canonicalize().unwrap();
        let b = b.display();
        let (_, out, _) = run_capturing(&format!("cd {b}/deep; echo $PWD; pwd -P"));
        assert_eq!(out, format!("{b}/deep\n{b}/real/sub\n"));
        let (_, out, _) = run_capturing(&format!("cd {b}/deep; cd ..; pwd"));
        assert_eq!(out, format!("{b}\n"));
        let (_, out, _) = run_capturing(&format!("cd {b}; cd -P deep/..; pwd"));
        assert_eq!(out, format!("{b}/real\n"));
        let (_, out, _) = run_capturing(&format!("cd -P {b}/deep; echo $PWD"));
        assert_eq!(out, format!("{b}/real/sub\n"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn pwd_and_oldpwd_are_exported_and_logical() {
        // dash's setpwd writes both with VEXPORT.
        let mut sh = crate::exec::Shell::new_for_test();
        assert!(sh.get_var("PWD").is_none());
        let _ = super::cd(&mut sh, &["cd".into(), "/tmp".into()]);
        let _ = super::cd(&mut sh, &["cd".into(), "/".into()]);
        let exported: Vec<String> =
            sh.exported_env().into_iter().map(|(k, _)| k).collect();
        for want in ["PWD", "OLDPWD"] {
            assert!(exported.contains(&want.to_string()), "{want} not exported");
        }
        assert_eq!(sh.get_var("PWD").as_deref(), Some("/"));
        assert_eq!(sh.get_var("OLDPWD").as_deref(), Some("/tmp"));
        // `pwd` reports the logical cwd, not whatever $PWD was overwritten with.
        let (_, out, _) = run_capturing("cd /tmp; PWD=lying; pwd; echo $PWD");
        assert_eq!(out, "/tmp\nlying\n");
    }

    #[test]
    fn dollar_dash_is_in_dash_optlist_order() {
        // Not alphabetical and not the order they were set: dash prints them in
        // its optlist order, which puts f before u whichever way round they came.
        let (_, out, _) = run_capturing("set -uf\necho $-");
        assert_eq!(out, "fu\n");
        let (_, out, _) = run_capturing("set -fu\necho $-");
        assert_eq!(out, "fu\n");
    }

    #[test]
    fn set_minus_alone_clears_xtrace_and_ends_the_options() {
        // A lone `-` is the one dash spelling that turns -x/-v off, and it stops
        // option processing, so what follows is a positional parameter.
        let (_, out, _) = run_capturing("set -x -v; set -; echo [$-]");
        assert_eq!(out, "[]\n");
        let (_, out, _) = run_capturing("set - -e; echo [$-][$1]");
        assert_eq!(out, "[][-e]\n");
    }

    #[test]
    fn dot_looks_in_path_and_prefers_it_to_the_cwd() {
        // dash resolves a name with no slash against PATH only; a file of the same
        // name in the cwd does not win, and is not even reached unless PATH says so.
        let base = std::env::temp_dir().join(format!("td-sh-dot-{}", std::process::id()));
        let dir = base.join("dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cmd"), "echo from-path\n").unwrap();
        std::fs::write(base.join("cmd"), "echo from-cwd\n").unwrap();
        let base = base.display();
        let (status, out, _) =
            run_capturing(&format!("cd {base}; PATH=dir; . cmd; echo status=$?"));
        assert_eq!((status, out.as_str()), (0, "from-path\nstatus=0\n"));
        // A slash makes it a path again, so the cwd copy is reachable that way.
        let (_, out, _) = run_capturing(&format!("cd {base}; PATH=dir; . ./cmd"));
        assert_eq!(out, "from-cwd\n");
        // Not in PATH is fatal, and a directory in PATH is skipped, not sourced.
        let (status, out, err) =
            run_capturing(&format!("cd {base}; PATH=dir; . nope; echo reached"));
        assert_eq!((status, out.as_str()), (2, ""));
        assert!(err.contains("not found"), "{err:?}");
        std::fs::remove_dir_all(base.to_string()).unwrap();
    }

    /// `source` is `.` under another name, as it is in ash -- which gives both
    /// spellings one implementation. The two must agree on everything a caller
    /// can see EXCEPT the word inside the diagnostic, which names the spelling
    /// it was called by; a hardcoded `.` there is invisible until someone reads
    /// an error about a command they did not run.
    #[test]
    fn source_is_dot_under_another_name() {
        let dir = std::env::temp_dir().join(format!("td-sh-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s.sh"), "echo sourced\n").unwrap();
        // Quoted: a TMPDIR with a space in it would otherwise split into two
        // words and every case below would be testing a failed `cd`.
        let d = dir.display();
        for w in [".", "source"] {
            // Runs the file, and by PATH for a name with no slash.
            let (st, out, _) = run_capturing(&format!("cd '{d}'; {w} ./s.sh"));
            assert_eq!((st, out.as_str()), (0, "sourced\n"), "{w}");
            let (st, out, _) = run_capturing(&format!("cd '{d}'; PATH=.; {w} s.sh"));
            assert_eq!((st, out.as_str()), (0, "sourced\n"), "PATH {w}");
            // Both are special, so a file that cannot be sourced is FATAL: the
            // `echo` does not run and the shell exits 2 -- except for the last,
            // where ash returns 2 without ending the script.
            //
            // All THREE of `dot`'s messages name the spelling rather than the
            // implementation, and they are reached by three different failures:
            // an open that fails, a PATH search that finds nothing, and no
            // operand at all. A literal `.` left in any one of them is a
            // diagnostic about a command the caller never ran, and two of the
            // three are unreachable from the case that is easiest to write.
            //
            // The third asks for `$?` rather than a marker word because it is
            // the one that does NOT end the script, so its status is the
            // builtin's own -- and an `echo` that merely RAN would report 0
            // whatever the builtin returned.
            for (src, want) in [
                (format!("cd '{d}'; {w} ./nope.sh; echo reached"), (2, "")),
                (format!("cd '{d}'; PATH=.; {w} nope; echo reached"), (2, "")),
                (format!("{w}; echo st=$?"), (0, "st=2\n")),
            ] {
                let (st, out, err) = run_capturing(&src);
                assert_eq!((st, out.as_str()), want, "{src}");
                assert!(err.starts_with(&format!("{w}: ")), "{src}: {err:?}");
            }
            // Which `type` agrees with, reading the same predicate -- now a
            // plain lookup, with no word named by hand anywhere.
            let (_s, out, _e) = run_capturing(&format!("type {w}"));
            assert_eq!(out, format!("{w} is a special shell builtin\n"));
            let (_s, out, _e) = run_capturing(&format!("command -v {w}"));
            assert_eq!(out, format!("{w}\n"));
            assert!(super::is_ash_special_word(w), "{w}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `.`'s operands are the sourced file's positional parameters, and its
    /// leading `-` words are options it accepts none of. Both spellings, since
    /// they are one command.
    #[test]
    fn dot_takes_option_words_and_gives_the_file_its_operands() {
        let dir = std::env::temp_dir().join(format!("td-sh-dotargs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s.sh"), "echo \"in:[$1][$2] n=$#\"\n").unwrap();
        std::fs::write(dir.join("r.sh"), "echo \"in:[$1] n=$#\"; return 3\n").unwrap();
        std::fs::write(dir.join("set.sh"), "set -- SRC\n").unwrap();
        std::fs::write(dir.join("opt.sh"), "getopts ab i; echo \"inner:[$i][$OPTIND]\"\n").unwrap();
        std::fs::write(dir.join("exit.sh"), "exit 0\n").unwrap();
        let d = dir.display();
        for w in [".", "source"] {
            let sh = |src: &str| run_capturing(&format!("cd '{d}'; {src}"));

            // `--` ends the options and does NOT eat the operands.
            let (st, out, _e) = sh(&format!("{w} -- ./s.sh"));
            assert_eq!((st, out.as_str()), (0, "in:[][] n=0\n"), "{w}");
            let (_s, out, _e) = sh(&format!("{w} -- ./s.sh a b"));
            assert_eq!(out, "in:[a][b] n=2\n", "{w}");
            // Any other option is FATAL, `.` being special, and names the first
            // LETTER rather than the word -- so the refusal is over letters.
            // Compared WHOLE, not by `contains`: `illegal option -abc` contains
            // `illegal option -a`, so the loose form cannot tell the letter
            // from the word it came out of.
            for (src, letter) in [("-f ./s.sh", "-f"), ("-abc ./s.sh", "-a")] {
                let (st, out, err) = sh(&format!("{w} {src}; echo AFTER"));
                assert_eq!((st, out.as_str()), (2, ""), "{w} {src}");
                assert_eq!(err.trim_end(), format!("{w}: illegal option {letter}"), "{w} {src}");
            }
            // A lone `-` is not an option at all but a FILENAME, and so is a
            // second `--` once the first has ended them.
            for name in ["-", "--"] {
                let (st, _o, err) = sh(&format!("{w} -- {name}; echo AFTER"));
                assert_eq!(st, 2, "{w} {name}");
                assert!(err.contains(&format!("{name}: not found")), "{err:?}");
            }
            let (st, _o, err) = sh(&format!("{w} -; echo AFTER"));
            assert_eq!(st, 2, "{w} -");
            assert!(err.contains("-: not found"), "{err:?}");
            // A `--`-PREFIXED word is not `--`, and the letter it names is the
            // second dash: `. --x` is `illegal option --`, not `-x` and not the
            // end of the options. Ending them on any `--`-prefixed word, or
            // trimming the dashes before naming one, both pass every case above.
            for src in ["--x ./s.sh", "---", "--x"] {
                let (st, _o, err) = sh(&format!("{w} {src}; echo AFTER"));
                assert_eq!(st, 2, "{w} {src}");
                assert_eq!(err.trim_end(), format!("{w}: illegal option --"), "{w} {src}");
            }
            // `--` with NO operand after it is the no-operand case, not an
            // option error: status 2 and the script CARRIES ON. That is the one
            // shape here where the difference is fatal against recoverable.
            for src in ["--", ""] {
                let (st, out, _e) = sh(&format!("{w} {src}; echo \"AFTER st=$?\""));
                assert_eq!((st, out.as_str()), (0, "AFTER st=2\n"), "{w} [{src}]");
            }

            // Operands become the file's parameters and the caller's come back.
            let (_s, out, _e) =
                sh(&format!("set -- keep me; {w} ./s.sh a b; echo \"after:[$1][$2] n=$#\""));
            assert_eq!(out, "in:[a][b] n=2\nafter:[keep][me] n=2\n", "{w}");
            // Fewer operands than the caller had, and none at all where the
            // caller had some -- `$#` has to follow, not just `$1`.
            let (_s, out, _e) =
                sh(&format!("set -- keep me; {w} ./s.sh solo; echo \"after:[$1] n=$#\""));
            assert_eq!(out, "in:[solo][] n=1\nafter:[keep] n=2\n", "{w}");
            let (_s, out, _e) = sh(&format!("{w} ./s.sh a b; echo \"after:[$1] n=$#\""));
            assert_eq!(out, "in:[a][b] n=2\nafter:[] n=0\n", "{w}");
            // With NO operands the file sees the caller's frame and KEEPS it:
            // nothing was saved, so the file's own `set --` leaks out. With
            // operands the same `set --` is undone. That pair is what makes the
            // save conditional rather than unconditional over an empty vector.
            let (_s, out, _e) =
                sh(&format!("set -- keep me; {w} ./s.sh; echo \"after:[$1] n=$#\""));
            assert_eq!(out, "in:[keep][me] n=2\nafter:[keep] n=2\n", "{w}");
            let (_s, out, _e) =
                sh(&format!("set -- keep me; {w} ./set.sh; echo \"after:[$1] n=$#\""));
            assert_eq!(out, "after:[SRC] n=1\n", "{w}");
            let (_s, out, _e) =
                sh(&format!("set -- keep me; {w} ./set.sh a; echo \"after:[$1] n=$#\""));
            assert_eq!(out, "after:[keep] n=2\n", "{w}");
            // An EMPTY operand is still an operand: ash decides on the argument
            // being THERE (`args_need_save = argv[0]`, a pointer) and not on
            // what it holds, so `. f ""` saves the frame and `$1` is empty.
            let (_s, out, _e) =
                sh(&format!("set -- keep me; {w} ./set.sh ''; echo \"after:[$1] n=$#\""));
            assert_eq!(out, "after:[keep] n=2\n", "{w} empty operand");
            let (_s, out, _e) = sh(&format!("{w} ./s.sh ''"));
            assert_eq!(out, "in:[][] n=1\n", "{w} empty operand is $1");
            // A `return` is how such a file usually ends, so the frame has to
            // come back on that path too.
            let (_s, out, _e) = sh(&format!(
                "set -- keep me; {w} ./r.sh a b; echo \"st=$? after:[$1] n=$#\""
            ));
            assert_eq!(out, "in:[a] n=2\nst=3 after:[keep] n=2\n", "{w}");
            // Inside a function it is the FUNCTION's frame that comes back.
            let (_s, out, _e) = sh(&format!(
                "f() {{ {w} ./s.sh a b; echo \"fn:[$1] n=$#\"; }}; set -- top; f X Y"
            ));
            assert_eq!(out, "in:[a][b] n=2\nfn:[X] n=2\n", "{w}");
            // The getopts CURSOR is part of that frame. A `set --` inside the
            // file resets it, and without the save the caller re-reads the
            // option it had already consumed -- `o=a` twice instead of `a`
            // then `b`. The file is not given a fresh cursor, though: it
            // continues the caller's scan, which `set.sh` not touching
            // `OPTIND` would hide, so the two halves need separate cases.
            let (_s, out, _e) = sh(&format!(
                "set -- -a -b; getopts ab o; {w} ./set.sh ARG; getopts ab o; echo \"o=$o i=$OPTIND\""
            ));
            assert_eq!(out, "o=b i=3\n", "{w}");
            let (_s, out, _e) = sh(&format!(
                "set -- -a -b; getopts ab o; {w} ./opt.sh -b; echo \"i=$OPTIND\""
            ));
            assert_eq!(out, "inner:[?][2]\ni=2\n", "{w}");
            // ...and with no operands nothing is saved, so the file's own
            // scan is what the caller is left holding.
            let (_s, out, _e) = sh(&format!(
                "set -- -a -b; getopts ab o; {w} ./opt.sh; echo \"i=$OPTIND\""
            ));
            assert_eq!(out, "inner:[b][3]\ni=3\n", "{w}");
            // A TERMINATING unwind skips the restore, because ash's runs after
            // its `cmdloop` returns and an `exit` never gets there: the EXIT
            // trap sees the FILE's operands, not the caller's. The ordinary
            // path is the control -- there the trap sees the caller's.
            let (_s, out, _e) = sh(&format!(
                "trap 'echo \"trap:[$1] n=$#\"' EXIT; set -- keep; {w} ./exit.sh a b"
            ));
            assert_eq!(out, "trap:[a] n=2\n", "{w}");
            let (_s, out, _e) = sh(&format!(
                "trap 'echo \"trap:[$1] n=$#\"' EXIT; set -- keep; {w} ./s.sh a b; exit 0"
            ));
            assert_eq!(out, "in:[a][b] n=2\ntrap:[keep] n=1\n", "{w}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_readonly_variable_is_fatal_to_write_or_unset() {
        // dash reports both through sh_error, which ends the script with 2 rather
        // than a status the next command could test.
        for src in [
            "readonly foo=bar; foo=eggs; echo reached",
            "readonly R=foo; unset R; echo reached",
            "readonly x=1; f() { x=2; }; f; echo reached",
        ] {
            let (status, out, err) = run_capturing(src);
            assert_eq!((status, out.as_str()), (2, ""), "{src}");
            assert!(err.contains("is read only"), "{src}: {err:?}");
        }
    }

    #[test]
    fn an_unknown_option_to_export_or_readonly_is_fatal() {
        // On a special builtin an unknown option ends the script. `n` and `p`
        // are the only two that are not (`nextopt("np")`, ash.c:14137).
        for (src, err) in [
            ("export -z x; echo reached", "export: illegal option -z\n"),
            ("readonly -q x; echo reached", "readonly: illegal option -q\n"),
            // ash reads the WHOLE cluster, so a bad letter after a good one is
            // still fatal -- and names the letter it stopped on, not the word.
            // dash calls nextopt once and would list and exit 0.
            ("export -px; echo reached", "export: illegal option -x\n"),
            ("readonly -pq; echo reached", "readonly: illegal option -q\n"),
        ] {
            let (status, out, e) = run_capturing(src);
            assert_eq!((status, out.as_str(), e.as_str()), (2, "", err), "{src}");
        }
        // `-p` and `--` stay options, and everything after `--` is a name.
        let (status, out, _) = run_capturing("export -p; readonly -- a=1; echo \"[$a]\"");
        assert_eq!((status, out.as_str()), (0, "[1]\n"));
    }

    /// `export -n` takes the export attribute away and leaves the VALUE, and
    /// `readonly` accepts the same letter and ignores it (ash.c:14137-14145).
    #[test]
    fn export_minus_n_unexports_and_readonly_ignores_it() {
        // `-p` is accepted and does nothing, so `-n` wins over it either order.
        for opts in ["-n", "-pn", "-np", "-n -p"] {
            let src = format!("TDN=1; export TDN; export {opts} TDN; export -p");
            let (status, out, err) = run_capturing(&src);
            assert_eq!((status, err.as_str()), (0, ""), "{opts}");
            assert!(!out.contains("export TDN"), "{opts} left it exported: {out:?}");
        }
        // `-p` on its own must NOT do that: it is accepted and ignored, so a
        // name beside it stays exported.
        let (_, out, _) = run_capturing("TDN=1; export -p TDN; export -p");
        assert!(out.contains("export TDN='1'"), "{out:?}");
        // The VALUE survives -- this is not `unset`.
        let (_, out, _) = run_capturing("TDN=1; export TDN; export -n TDN; echo \"[$TDN]\"");
        assert_eq!(out, "[1]\n");
        // An ASSIGNMENT declines to ADD the export and never takes one away, so
        // these two spellings of `-n` differ in a way `unset` has no analogue for.
        let (_, out, _) = run_capturing("export -n TDN=2; export -p");
        assert!(!out.contains("export TDN"), "{out:?}");
        let (_, out, _) = run_capturing("TDN=1; export TDN; export -n TDN=2; export -p");
        assert!(out.contains("export TDN='2'"), "{out:?}");
        // A name that does not exist stays unset rather than becoming empty.
        let (status, out, _) = run_capturing("export -n TDNEW; echo \"[${TDNEW-UNSET}]\"");
        assert_eq!((status, out.as_str()), (0, "[UNSET]\n"));
        let (_, out, _) = run_capturing("export -n TDNEW; TDNEW=1; export -p");
        assert!(!out.contains("export TDNEW"), "{out:?}");
        // Under `set -a` that same absent name comes out EXPORTED, which is
        // `-n` marking for export the one thing it was asked not to.
        let (_, out, _) = run_capturing("set -a; export -n TDNEW; set +a; export -p");
        assert!(out.contains("export TDNEW"), "{out:?}");
        let (_, out, _) = run_capturing("set -a; export -n TDNEW; set +a; TDNEW=1; export -p");
        assert!(out.contains("export TDNEW='1'"), "{out:?}");
        // An existing name is NOT re-exported that way: that arm writes the
        // flags outright rather than going through `setvar` (ash.c:14160).
        let (_, out, _) = run_capturing("set -a; TDN=1; export TDN; export -n TDN; export -p");
        assert!(!out.contains("export TDN"), "{out:?}");
        // A CLONE must not carry it back: these are threads over one `Shell`,
        // so "the subshell did not leak" is a property of this shell rather
        // than of the fork every other one gets it from.
        for form in ["(export -n TDN)", "x=$(export -n TDN)", "export -n TDN | cat"] {
            let src = format!("TDN=1; export TDN; {form}; export -p");
            let (_, out, _) = run_capturing(&src);
            assert!(out.contains("export TDN='1'"), "{form} leaked: {out:?}");
        }
        // A FUNCTION shares the shell, so there it does reach the caller.
        let (_, out, _) = run_capturing("TDN=1; export TDN; f() { export -n TDN; }; f; export -p");
        assert!(!out.contains("export TDN"), "{out:?}");
        // Re-exporting afterwards works, and `-n` with no operand still lists.
        let (_, out, _) = run_capturing("TDN=1; export TDN; export -n TDN; export TDN; export -n");
        assert!(out.contains("export TDN='1'"), "{out:?}");
        // `readonly -n` is accepted and does NOT lift the attribute, so the
        // assignment after it is still the fatal one. `-n` is the ONLY thing
        // making this name readonly: preserving an attribute it never applied
        // would pass the obvious spelling and still be wrong. The third is the
        // other half of "ignores it" -- it must not unexport either.
        for src in ["readonly -n R; R=2; echo reached", "R=1; readonly R; readonly -n R; R=2"] {
            let (status, out, err) = run_capturing(src);
            assert_eq!((status, out.as_str()), (2, ""), "{src}");
            assert!(err.contains("is read only"), "{src}: {err:?}");
        }
        let (_, out, _) = run_capturing("TDN=1; export TDN; readonly -n TDN; export -p");
        assert!(out.contains("export TDN='1'"), "{out:?}");
        // `-n` takes nothing else away: not READONLY, not the LOCAL scope, and
        // not the distinction between a valueless name and an empty one.
        let (status, _, err) = run_capturing("X=1; export X; readonly X; export -n X; X=2");
        assert_eq!(status, 2);
        assert!(err.contains("is read only"), "{err:?}");
        // The LOCAL mark, which only `unset` reads: a name still marked keeps
        // its entry (and so its export) where an unmarked one is removed whole.
        let (_, out, _) =
            run_capturing("f() { local X; export -n X; export X; unset X; X=2; export -p; }; f");
        assert!(out.contains("export X='2'"), "{out:?}");
        let (_, out, _) = run_capturing("export TDNV; export -n TDNV; echo \"[${TDNV-UNSET}]\"");
        assert_eq!(out, "[UNSET]\n");
        // EVERY operand, not just the first.
        let (_, out, _) = run_capturing("A=1; B=2; export A B; export -n A B; export -p");
        assert!(!out.contains("export A") && !out.contains("export B"), "{out:?}");
    }

    /// A frame's binding for a name that did not exist is undone by UNSETTING
    /// it, and ash's `unsetvar` is `setvar(s, NULL, 0)` (ash.c:2525) -- so
    /// under `set -a` the entry outlives the frame, marked for export.
    #[test]
    fn a_frame_that_ends_under_allexport_leaves_its_fresh_names_exported() {
        // All four reach the one arm: two `local` forms and two TEMPORARY
        // assignments, which a test written around `local` alone would miss.
        for src in [
            "set -a; f() { local TDL; }; f",
            "set -a; f() { local TDL=1; }; f",
            "set -a; TDL=1 true",
            "set -a; g() { :; }; TDL=1 g",
        ] {
            let (_, out, _) = run_capturing(&format!("{src}; export -p"));
            assert!(out.contains("export TDL"), "{src}: {out:?}");
            // Marked for export, still WITHOUT a value: `-a` does not make the
            // pop a no-op, it only changes what the pop leaves behind.
            let (_, out, _) = run_capturing(&format!("{src}; echo \"[${{TDL-UNSET}}]\""));
            assert_eq!(out, "[UNSET]\n", "{src}");
        }
        // Without `set -a` the name goes entirely, which is the whole contrast.
        for src in ["f() { local TDL; }; f", "TDL=1 true"] {
            let (_, out, _) = run_capturing(&format!("{src}; export -p"));
            assert!(!out.contains("export TDL"), "{src}: {out:?}");
        }
        // A name the frame SHADOWED is restored, not replaced by the above.
        let (_, out, _) = run_capturing("X=o; set -a; f() { local X; }; f; echo \"[$X]\"");
        assert_eq!(out, "[o]\n");
        // What survives is a GLOBAL. Were it still marked local, `unset` would
        // clear the value and KEEP the entry, so the name would outlive that
        // too -- visible only once `set -a` is off, since with it on
        // `unset_var` delegates to `unset_value` anyway.
        for src in ["set -a; f() { local TDL; }; f", "set -a; TDL=1 true"] {
            let (_, out, _) = run_capturing(&format!("{src}; set +a; unset TDL; export -p"));
            assert!(!out.contains("export TDL"), "{src}: {out:?}");
        }
    }

    /// The other half: a name ash SEEDS is not a fresh one. `mklocal`'s
    /// `findvar` finds it (ash.c:10011), so the pop restores that entry rather
    /// than reaching the `unsetvar` `set -a` would re-export.
    #[test]
    fn a_name_ash_seeds_is_not_a_fresh_name_when_the_frame_ends() {
        // EVERY name on the roster, so dropping one is caught. The `unset`
        // leads because td-sh seeds some of them and the arm needs the name
        // absent; for the rest it does nothing.
        for name in [
            "IFS",
            "MAIL",
            "MAILPATH",
            "PATH",
            "PS1",
            "PS2",
            "PS4",
            "OPTIND",
            "LINENO",
            "FUNCNAME",
            "RANDOM",
            "EPOCHSECONDS",
            "EPOCHREALTIME",
            "HISTFILE",
        ] {
            for src in [
                format!("unset {name}; set -a; f() {{ local {name}; }}; f"),
                format!("unset {name}; set -a; {name}=1 true"),
            ] {
                let (_, out, _) = run_capturing(&format!("{src}; export -p"));
                assert!(!lists(&out, name), "{src}: {out:?}");
            }
        }
        // The flag would outlive `set +a`, so a survivor here reaches a CHILD's
        // environment and not just the listing.
        let (_, out, _) = run_capturing("set -a; f() { local MAIL; }; f; set +a; MAIL=x; export -p");
        assert!(!lists(&out, "MAIL"), "{out:?}");
        // A name ash does NOT seed still gets the survivor -- `LC_ALL` is in
        // `varinit_data` but under an `#if` this build has off, which is why
        // the roster was measured rather than read.
        for name in ["LC_ALL", "LC_CTYPE", "HOME", "TDZ"] {
            let src = format!("unset {name}; set -a; f() {{ local {name}; }}; f; export -p");
            let (_, out, _) = run_capturing(&src);
            assert!(lists(&out, name), "{src}: {out:?}");
        }
    }

    /// Whether `export -p` lists this exact name. A bare `contains` for MAIL is
    /// satisfied by `export MAILPATH`, which is the roster's own neighbour.
    fn lists(out: &str, name: &str) -> bool {
        out.lines().any(|l| {
            l.strip_prefix("export ")
                .and_then(|rest| rest.strip_prefix(name))
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
        })
    }

    /// ash's discard test ORs in the entry's OWN `VSTRFIXED` (ash.c:2440), so a
    /// name it seeds survives an `unset` and line 2449 puts the `VEXPORT` back.
    #[test]
    fn a_name_ash_seeds_keeps_its_entry_and_export_flag_past_unset() {
        // The flag survives, so a later assignment still reaches a child.
        let (_, out, _) = run_capturing("export MAIL=old; unset MAIL; MAIL=new; export -p");
        assert!(lists(&out, "MAIL"), "{out:?}");
        // Through a frame too, which is the same entry seen by `local`.
        let (_, out, _) =
            run_capturing("export MAIL=1; unset MAIL; f() { local MAIL=2; }; f; export -p");
        assert!(lists(&out, "MAIL"), "{out:?}");
        // An ORDINARY name keeps nothing: its entry really goes.
        let (_, out, _) = run_capturing("export TDZ=1; unset TDZ; TDZ=2; export -p");
        assert!(!lists(&out, "TDZ"), "{out:?}");
        // What survives is the FLAG and not a value: the name still reads
        // unset, and an unexported seeded name is not listed at all.
        for name in ["MAIL", "PATH", "HISTFILE"] {
            let src = format!("unset {name}; echo \"[${{{name}-U}}]\"");
            let (_, out, _) = run_capturing(&src);
            assert_eq!(out, "[U]\n", "{src}");
            let (_, out, _) = run_capturing(&format!("unset {name}; export -p"));
            assert!(!lists(&out, name), "{src}: {out:?}");
        }
        // The dynamic ones stay dead after their unset, as ash's comment on
        // `lookupvar` has it -- keeping the entry must not resurrect them.
        for name in ["RANDOM", "LINENO"] {
            let src = format!("unset {name}; echo \"[${{{name}-U}}]\"");
            let (_, out, _) = run_capturing(&src);
            assert_eq!(out, "[U]\n", "{src}");
        }
    }

    /// The same for `export -n` and `readonly`, which DECLARE a name rather
    /// than restoring one: `findvar` finds what ash seeds, so `exportcmd` edits
    /// the flags in place and never reaches the `setvar` that `set -a` would
    /// export through (ash.c:14158-14161).
    #[test]
    fn a_name_ash_seeds_is_not_a_fresh_name_for_export_n_or_readonly() {
        const SEEDED: [&str; 14] = [
            "IFS",
            "MAIL",
            "MAILPATH",
            "PATH",
            "PS1",
            "PS2",
            "PS4",
            "OPTIND",
            "LINENO",
            "FUNCNAME",
            "RANDOM",
            "EPOCHSECONDS",
            "EPOCHREALTIME",
            "HISTFILE",
        ];
        // Both spellings, because `export -n` CLEARS the flag whatever it was:
        // unlike `readonly` it has no prior state to preserve, so the seeded
        // name agrees with ash whether or not it was exported first.
        for name in SEEDED {
            for pre in [
                format!("unset {name}"),
                format!("export {name}=old; unset {name}"),
            ] {
                let src = format!("{pre}; set -a; export -n {name}; export -p");
                let (_, out, _) = run_capturing(&src);
                assert!(!lists(&out, name), "{src}: {out:?}");
            }
        }
        // A name ash does NOT seed still gets it, which is the contrast.
        for name in ["LC_ALL", "LC_CTYPE", "HOME", "TDZ"] {
            let src = format!("unset {name}; set -a; export -n {name}; export -p");
            let (_, out, _) = run_capturing(&src);
            assert!(lists(&out, name), "{src}: {out:?}");
        }
        // A VALUE puts it back on ash's `setvar` path (`p != NULL`,
        // ash.c:14155), where the export happens for a seeded name too.
        for decl in ["readonly MAIL=1", "export MAIL=1", "export -n MAIL=1"] {
            let src = format!("unset MAIL; set -a; {decl}; export -p");
            let (_, out, _) = run_capturing(&src);
            assert!(lists(&out, "MAIL"), "{src}: {out:?}");
        }
        // Plain `export` marks a seeded name however it gets there.
        let (_, out, _) = run_capturing("unset MAIL; set -a; export MAIL; export -p");
        assert!(lists(&out, "MAIL"), "{out:?}");
        // `readonly` needs BOTH halves to agree, and they pull opposite ways:
        // exported-first keeps the flag the entry carried across the unset,
        // clean has no flag to keep. A roster alone answers only the second,
        // which is why the entry itself is what this rests on.
        let (_, out, _) =
            run_capturing("export MAIL=old; unset MAIL; set -a; readonly MAIL; export -p");
        assert!(lists(&out, "MAIL"), "{out:?}");
        for name in SEEDED {
            let src = format!("unset {name}; set -a; readonly {name}; export -p");
            let (_, out, _) = run_capturing(&src);
            assert!(!lists(&out, name), "{src}: {out:?}");
        }
        // And the declaration still TOOK, for every one of them: withholding
        // the export must not be a `readonly` quietly dropped instead. RANDOM
        // is in that list because the `unset` clears its dynamic flag, so the
        // exemption that would let an assignment through no longer applies.
        for name in SEEDED {
            let src = format!("unset {name}; set -a; readonly {name}; {name}=x");
            let (_, _, err) = run_capturing(&src);
            assert!(err.contains(&format!("{name}: is read only")), "{src}: {err:?}");
        }
    }

    /// `readonly NAME` on a name that does not exist yet creates its entry
    /// through `setvar` (ash.c:14164), which ORs `VEXPORT` in under `set -a`
    /// (ash.c:2417) -- so the declaration marks it for export as well.
    #[test]
    fn a_readonly_declaration_under_allexport_marks_the_name_for_export() {
        let (_, out, _) = run_capturing("set -a; readonly TDR; set +a; export -p");
        assert!(out.contains("export TDR"), "{out:?}");
        // Without it, `readonly` says nothing about exporting.
        let (_, out, _) = run_capturing("readonly TDR; export -p");
        assert!(!out.contains("export TDR"), "{out:?}");
        // Either way it is READONLY, which is the half that was never in doubt.
        for src in ["set -a; readonly TDR; TDR=1", "readonly TDR; TDR=1"] {
            let (status, _, err) = run_capturing(src);
            assert_eq!(status, 2, "{src}");
            assert!(err.contains("is read only"), "{src}: {err:?}");
        }
        // An EXISTING name is untouched by this: the entry is already there, so
        // the insert that consults `set -a` never runs. The second spelling is
        // the one that pins it -- the first passes even if the existing arm
        // consults `set -a` too, since it is off.
        for src in ["TDR=1; readonly TDR; export -p", "TDR=1; set -a; readonly TDR; set +a; export -p"] {
            let (_, out, _) = run_capturing(src);
            assert!(!out.contains("export TDR"), "{src}: {out:?}");
        }
    }

    #[test]
    fn a_declaration_builtins_assignment_operand_is_not_field_split() {
        // ash's `pseudovarflag` (ash.c:10416): for these four, a word whose RAW
        // text starts `name=` is expanded as an assignment -- no splitting, no
        // globbing, and tilde after the `=` and after each unquoted `:`.
        for (src, want) in [
            ("x='a b'; export n=$x; echo \"[$n]\"", "[a b]\n"),
            ("x='a b'; readonly n=$x; echo \"[$n]\"", "[a b]\n"),
            ("x='a b'; f() { local n=$x; echo \"[$n]\"; }; f", "[a b]\n"),
            // `command` defers the decision to the word after it, as ash does.
            ("x='a b'; command export n=$x; echo \"[$n]\"", "[a b]\n"),
            // ... however many times it is repeated, and through `-p`, `--` and a
            // cluster, which is where ash's loop goes round again.
            ("x='a b'; command command export n=$x; echo \"[$n]\"", "[a b]\n"),
            ("x='a b'; command -p export n=$x; echo \"[$n]\"", "[a b]\n"),
            ("x='a b'; command -pp export n=$x; echo \"[$n]\"", "[a b]\n"),
            ("x='a b'; command -- export n=$x; echo \"[$n]\"", "[a b]\n"),
            // The wrapper need not be one word, or one field per word: ash resolves
            // FIELDS, expanding only as many words as it takes to get one.
            (
                "x='a b'; set -- command -p; \"$@\" export n=$x; echo \"[$n]\"",
                "[a b]\n",
            ),
            ("x='a b'; e=; command $e export n=$x; echo \"[$n]\"", "[a b]\n"),
            // An option `command` does not take, or a bare `-`, means `command`
            // itself runs -- and it is a regular builtin, so nothing is spared.
            ("x='a b'; command -pv export n=$x; echo \"[$n]\"", "export\n[]\n"),
            ("x='a b'; command -x export n=$x 2>/dev/null; echo \"[$n]\"", "[]\n"),
            ("x='a b'; command - export n=$x 2>/dev/null; echo \"[$n]\"", "[]\n"),
            // A function named `command` is not the builtin, so the walk never
            // starts and all three fields reach it.
            (
                "x='a b'; command() { echo \"[$#]\"; }; command export n=$x",
                "[3]\n",
            ),
            // ... while past one `command` the lookup drops functions, so a
            // function named `export` no longer shadows the builtin.
            (
                "x='a b'; export() { echo F; }; command export n=$x; echo \"[$n]\"",
                "[a b]\n",
            ),
            // Several operands, and only the assignment-shaped ones are spared.
            ("x='a b'; export a=1 n=$x b=2; echo \"[$a][$n][$b]\"", "[1][a b][2]\n"),
            // HOME after `=` and after an unquoted `:`, which plain assignment
            // already did and these did not.
            ("HOME=/h; export n=~/x; echo \"[$n]\"", "[/h/x]\n"),
            ("HOME=/h; export n=a:~/x; echo \"[$n]\"", "[a:/h/x]\n"),
            // `alias` is the fourth, and the only one that is not special.
            ("x='a b'; alias n=$x; alias n", "n='a b'\n"),
            // NOT an assignment: ash tests the unexpanded text, so quoting any
            // part of `name=` puts the word back under ordinary splitting --
            // including the `=` itself, which is why all three of these split.
            ("x='a b'; export \"n\"=$x; echo \"[$n]\"", "[a]\n"),
            ("x='a b'; export n\"=\"$x; echo \"[$n]\"", "[a]\n"),
            ("x='a b'; export \"n=\"$x; echo \"[$n]\"", "[a]\n"),
            // Nor is a word whose prefix is not a NAME. Only `alias` shows it:
            // `export` rejects the name either way, so the split is invisible
            // there, but an alias may be called `a-b` and the stray field lands
            // as a second operand.
            ("x='a b'; alias a-b=$x 2>/dev/null; alias a-b", "a-b='a'\n"),
            // ... and a word that only BECOMES `name=` after expansion never was.
            ("x='n=a b'; export $x; echo \"[$n]\"", "[a]\n"),
            // The name ends at the FIRST `=`; the rest is value, `=` and all.
            ("x='p q'; export n=a=$x; echo \"[$n]\"", "[a=p q]\n"),
            // A function of the name is not the builtin and gets no such rule.
            ("x='a b'; export() { echo \"[$#]\"; }; export n=$x", "[2]\n"),
            // An ordinary command is untouched -- including one that merely has a
            // declaration builtin's name among its LATER words, since the command
            // word is resolved once and nothing re-decides it.
            ("x='a b'; f() { echo \"[$#]\"; }; f n=$x", "[2]\n"),
            ("x='a b'; f() { echo \"[$#]\"; }; f export n=$x", "[3]\n"),
            ("x='a b'; printf '[%s]' export n=$x; echo", "[export][n=a][b]\n"),
            // The walk stops at the FIRST field that is not `command`, even when
            // one word carried several and a later one names a declaration builtin.
            (
                "x='a b'; set -- printf '[%s]' export; \"$@\" n=$x; echo",
                "[export][n=a][b]\n",
            ),
        ] {
            let (_, out, err) = run_capturing(src);
            assert_eq!(out, want, "{src}: {err}");
        }
    }

    #[test]
    fn type_describes_what_a_name_is() {
        // ash's `describe_command` wording, and its order: keyword, then alias,
        // then the command lookup -- so an alias outranks a function of the name
        // and nothing can hide a keyword.
        for (src, want) in [
            ("type while", "while is a shell keyword\n"),
            ("type cd", "cd is a shell builtin\n"),
            ("type eval", "eval is a special shell builtin\n"),
            // ash's special set includes `local`, which POSIX's does not.
            ("type local", "local is a special shell builtin\n"),
            ("alias q='ls -l'; type q", "q is an alias for ls -l\n"),
            ("q() { :; }; alias q=w; type q", "q is an alias for w\n"),
            ("f() { :; }; type f", "f is a function\n"),
            // A function outranks the BUILTIN of the same name, which is the
            // other half of the ordering the alias case pins.
            ("cd() { :; }; type cd", "cd is a function\n"),
            (
                "type while cd",
                "while is a shell keyword\ncd is a shell builtin\n",
            ),
        ] {
            let (status, out, err) = run_capturing(src);
            assert_eq!((status, out.as_str()), (0, want), "{src}: {err}");
        }
    }

    #[test]
    fn type_answers_a_path_as_given() {
        // cargo runs a test with the package root as its cwd, so these exist.
        // A name with a slash is answered AS GIVEN and merely has to EXIST: ash
        // stats it and does not ask whether it is executable, or even a file.
        assert_eq!(
            run_capturing("type ./Cargo.toml").1,
            "./Cargo.toml is ./Cargo.toml\n"
        );
        assert_eq!(run_capturing("type ./src").1, "./src is ./src\n");
        assert_eq!(run_capturing("type ./no-such-td-sh").0, 127);
        // A PATH element is JOINED to the name rather than resolved, and an empty
        // element contributes nothing at all -- `plain`, not `./plain`.
        assert_eq!(
            run_capturing("PATH=. type Cargo.toml").1,
            "Cargo.toml is ./Cargo.toml\n"
        );
        assert_eq!(
            run_capturing("PATH= type Cargo.toml").1,
            "Cargo.toml is Cargo.toml\n"
        );
        // A trailing slash is not normalised away -- it is concatenation, not a
        // path join, which is the only thing that tells the two apart.
        assert_eq!(
            run_capturing("PATH=./ type Cargo.toml").1,
            "Cargo.toml is .//Cargo.toml\n"
        );
    }

    #[test]
    fn type_reports_a_name_it_cannot_place() {
        // The verbose form says so on STDOUT, not stderr, and answers 127.
        let (status, out, err) = run_capturing("type td_sh_zz");
        assert_eq!(
            (status, out.as_str(), err.as_str()),
            (127, "td_sh_zz: not found\n", "")
        );
        // The worst status wins, and the names that DID place still print.
        let (status, out, _) = run_capturing("type cd td_sh_zz cd");
        assert_eq!(status, 127);
        assert_eq!(
            out,
            "cd is a shell builtin\ntd_sh_zz: not found\ncd is a shell builtin\n"
        );
        // No names at all is silence and 0.
        assert_eq!(run_capturing("type"), (0, String::new(), String::new()));
        // Nothing to say and a CLOSED stdout is still 0: ash writes nothing at
        // all, where writing an empty buffer would report the closed descriptor.
        assert_eq!(run_capturing("type >&-").0, 0);
        // A write failure is ORED into the answer rather than replacing it: a name
        // that could not be placed is still 127 when the write failed too, and a
        // name that WAS placed becomes 1.
        assert_eq!(run_capturing("type td_sh_zz >&-").0, 127);
        assert_eq!(run_capturing("type cd >&-").0, 1);
        assert_eq!(run_capturing("type cd td_sh_zz >&-").0, 127);
    }

    #[test]
    fn types_first_option_only_turns_the_wording_off() {
        // ash never asks WHICH option it was, so these are one request; and only
        // the FIRST argument is read as an option, so the second `-p` is a name
        // that cannot be placed -- silently, since the wording is off.
        for src in ["type -p cd", "type -- cd", "type -x cd"] {
            assert_eq!(run_capturing(src).1, "cd\n", "{src}");
        }
        assert_eq!(run_capturing("type -p -p"), (127, String::new(), String::new()));
        // A bare `-` IS that first option: ash tests the first byte and never
        // asks how long the word is, so it is not a name that cannot be placed.
        assert_eq!(run_capturing("type -"), (0, String::new(), String::new()));
        assert_eq!(run_capturing("type - cd").1, "cd\n");
        assert_eq!(run_capturing("type -p td_sh_zz"), (127, String::new(), String::new()));
    }

    #[test]
    fn command_v_and_big_v_answer_as_type_does() {
        assert_eq!(run_capturing("command -v while").1, "while\n");
        assert_eq!(
            run_capturing("command -V while").1,
            "while is a shell keyword\n"
        );
        // The brief form of an alias prints a definition that reads back, and no
        // trailing name -- the one answer that is not just `type`'s with the
        // wording removed.
        assert_eq!(
            run_capturing("alias q='ls -l'; command -v q").1,
            "alias q='ls -l'\n"
        );
        assert_eq!(
            run_capturing("alias q='ls -l'; command -V q").1,
            "q is an alias for ls -l\n"
        );
        // `-V` only ever SETS the verbose wording: `-v` after it cannot clear it.
        for src in ["command -vV export", "command -Vv export", "command -VV export"] {
            assert_eq!(
                run_capturing(src).1,
                "export is a special shell builtin\n",
                "{src}"
            );
        }
        // Only the FIRST operand is described.
        assert_eq!(run_capturing("command -v cd echo").1, "cd\n");
        // A query ENDS the walk: `command` is then the name being described, not
        // a wrapper to look through, so this describes `command` and runs nothing.
        assert_eq!(run_capturing("command -v command echo").1, "command\n");
        assert_eq!(
            run_capturing("command -V command echo").1,
            "command is a shell builtin\n"
        );
    }

    #[test]
    fn command_p_moves_the_lookup_and_only_the_lookup() {
        // ash's `bb_default_path`. Both halves have to move together: a query that
        // searched a different path from the execution would describe a command
        // that is not the one `command -p` runs.
        let probe = "td_sh_no_such_utility_69";
        assert_eq!(run_capturing(&format!("command -p {probe}")).0, 127);
        assert_eq!(run_capturing(&format!("command -pv {probe}")).0, 127);
        assert_eq!(
            run_capturing(&format!("command -pV {probe}")).1,
            format!("{probe}: not found\n")
        );
        // ... and PATH is what they stop reading: a name that IS on PATH is found
        // without `-p` and not with it.
        let src = "PATH=. command -v Cargo.toml";
        assert_eq!(run_capturing(src).1, "./Cargo.toml\n");
        assert_eq!(run_capturing("PATH=. command -pv Cargo.toml").0, 127);
        // `-p` reaches a BUILTIN regardless -- it is a path, not a bypass -- and
        // leaves the variable a child would inherit alone.
        assert_eq!(run_capturing("command -pv cd").1, "cd\n");
        assert_eq!(run_capturing("PATH=. command -p echo hi").1, "hi\n");
        assert_eq!(run_capturing("PATH=zz; command -p :; echo $PATH").1, "zz\n");
        // `type` has no such option: its lone flag only turns the wording off.
        assert_eq!(run_capturing("PATH=. type -p Cargo.toml").1, "./Cargo.toml\n");
        // `-p` counts wherever it sits in the cluster, including after a query
        // letter -- the two are read independently.
        for src in ["command -vp Cargo.toml", "command -Vp Cargo.toml"] {
            assert_eq!(run_capturing(&format!("PATH=. {src}")).0, 127, "{src}");
        }
        // The constant itself, since no host-independent case can tell it from
        // dash's `_PATH_STDPATH`: ash's `bb_default_path` is `BB_PATH_ROOT_PATH`
        // (libbb.h) less its `/sbin` pair, with the `BB_ADDITIONAL_PATH` hook that
        // appends to it left empty in the reference binary.
        assert_eq!(crate::process::DEFAULT_UTILITY_PATH, "/bin:/usr/bin");
    }

    #[test]
    fn command_reads_its_options_as_a_cluster() {
        // `nextopt("pvV")`, so `-pv` is two options and not an unknown one.
        for src in ["command -pv export", "command -v export"] {
            let (status, out, err) = run_capturing(src);
            assert_eq!((status, out.as_str()), (0, "export\n"), "{src}: {err}");
        }
        // `-V` reads as a cluster too. What that costs is asserted and nothing
        // else: td-sh's `-V` still answers as `-v` does, and ash's wording (`export
        // is a special shell builtin`) is `describe_command`'s whole output table,
        // which is a separate increment. Pinning either spelling here would pin the
        // part this test is not about.
        let (status, out, err) = run_capturing("command -Vp export");
        assert_eq!((status, err.as_str()), (0, ""), "{out:?}");
        assert!(!out.is_empty(), "-Vp described nothing");
        // An unknown letter is a usage error -- but `command` is REGULAR, so the
        // shell survives it, unlike `export -x`.
        let (status, out, err) = run_capturing("command -x export n=1; echo \"after=$?\"");
        assert_eq!((status, out.as_str()), (0, "after=2\n"), "{err}");
        assert_eq!(err, "command: illegal option -x\n");
        // A name that resolves to nothing is 127, not 1: `command -v` reports what
        // `describe_command` returns.
        assert_eq!(run_capturing("command -v td_sh_no_such_thing").0, 127);
        // `--` and a bare `-` end the options; the second is then an operand.
        assert_eq!(run_capturing("command -v -- export").1, "export\n");
        assert_eq!(run_capturing("command -- true; echo st=$?").1, "st=0\n");
        // No operand at all is a no-op, not an error.
        for src in ["command", "command -p", "command --", "command -v"] {
            let (status, out, err) = run_capturing(src);
            assert_eq!((status, out.as_str()), (0, ""), "{src}: {err}");
        }
    }

    #[test]
    fn export_marks_a_variable() {
        let (_, out, _) = run_capturing("export FOO=bar; echo $FOO");
        assert_eq!(out, "bar\n");
    }

    #[test]
    fn printf_formats_and_cycles() {
        assert_eq!(run_capturing("printf '%s-%s\\n' a b c d").1, "a-b\nc-d\n");
        assert_eq!(run_capturing("printf '%d\\n' 42").1, "42\n");
    }

    #[test]
    fn eval_runs_constructed_code() {
        let (_, out, _) = run_capturing("x='echo hi'; eval $x");
        assert_eq!(out, "hi\n");
    }

    #[test]
    fn set_double_dash_alone_clears_positionals() {
        assert_eq!(run_capturing("set -- a b c; set --; echo $#").1, "0\n");
        // A bare option change must NOT touch the positionals.
        assert_eq!(run_capturing("set -- a b c; set -e; echo $#").1, "3\n");
    }

    #[test]
    fn printf_reports_bad_numeric_argument() {
        let (status, out, err) = run_capturing("printf '%d\\n' abc");
        assert_eq!(out, "0\n");
        assert_eq!(status, 1);
        assert!(err.contains("expected a numeric value"), "err: {err:?}");
        // An absent argument is a silent zero.
        let (status, out, _) = run_capturing("printf '%d\\n'");
        assert_eq!(out, "0\n");
        assert_eq!(status, 0);
    }

    #[test]
    fn printf_width_precision_and_flags() {
        // Strings: width, left-justify, precision (truncation).
        assert_eq!(
            run_capturing("printf '[%5s][%-5s][%.2s]' abc abc abcdef").1,
            "[  abc][abc  ][ab]"
        );
        // Integers: zero-pad, sign flags, precision padding ahead of a sign.
        assert_eq!(
            run_capturing("printf '[%05d][%+d][% d][%6.4d]' 42 42 42 -42").1,
            "[00042][+42][ 42][ -0042]"
        );
        // Dynamic width/precision consumed from `*` arguments, in order.
        assert_eq!(run_capturing("printf '[%*.*f]' 8 2 3.14159").1, "[    3.14]");
    }

    #[test]
    fn printf_integer_bases_and_charcode() {
        // Hex/octal, the `#` alternate-form prefixes, unsigned two's-complement.
        assert_eq!(
            run_capturing("printf '[%x][%#x][%o][%#o][%X]' 255 255 8 8 255").1,
            "[ff][0xff][10][010][FF]"
        );
        assert_eq!(run_capturing("printf '[%u]' -1").1, "[18446744073709551615]");
        // Base-0 parsing: 0x hex and leading-0 octal in the argument.
        assert_eq!(run_capturing("printf '%d %d' 0x55 055").1, "85 45");
        // `'c` uses the first byte's code.
        assert_eq!(run_capturing("printf '%d %d' \"'A\" \"'z\"").1, "65 122");
    }

    #[test]
    fn printf_char_b_and_float() {
        // %c takes the first byte only.
        assert_eq!(run_capturing("printf '[%c][%cZ]' abc abc").1, "[a][aZ]");
        // %b evaluates its own escapes (\t, \0ooo octal) — unlike %s.
        assert_eq!(run_capturing("printf '[%b]' 'a\\tb\\0101'").1, "[a\tbA]");
        // %b \c stops all further output.
        assert_eq!(run_capturing("printf 'x%by' 'a\\cb'").1, "xa");
        // %f defaults to 6 digits of precision; honour width and the 0 flag.
        assert_eq!(run_capturing("printf '[%.2f][%08.3f]' 3.14159 3.14").1, "[3.14][0003.140]");
    }

    #[test]
    fn printf_format_escapes_dashdash_and_bad_directive() {
        // Format-string octal escape (\ooo => a byte).
        assert_eq!(run_capturing("printf '[\\101]'").1, "[A]");
        // A leading `--` ends options (dash/ash have no bash -v); format cycles.
        assert_eq!(run_capturing("printf -- '%s.' a b c").1, "a.b.c.");
        // An unsupported directive (bash's %q) is rejected like ash: keep the
        // already-emitted prefix, stop, exit status 1.
        let (status, out, err) = run_capturing("printf 'a%qb' x");
        assert_eq!(out, "a");
        assert_eq!(status, 1);
        assert!(err.contains("invalid directive"), "err: {err:?}");
    }

    #[test]
    fn printf_escapes_are_the_echo_converter() {
        // Both printf paths take busybox's converter, so both know `\e`, take a
        // two-digit `\xHH`, and stop an octal run before it leaves a byte.
        assert_eq!(run_capturing("printf '[\\x41\\e\\777]'").1, "[A\x1b?7]");
        assert_eq!(run_capturing("printf '[%b]' '\\x41\\e\\777'").1, "[A\x1b?7]");
        // The `\0` marker is `%b`'s alone: in a format string `\0101` is the
        // three-digit `\010` and a literal `1`, not `\101`.
        assert_eq!(run_capturing("printf '[\\0101][%b]' '\\0101'").1, "[\u{8}1][A]");
        // Format-string `\c` abandons the whole run -- the rest of the format,
        // and the cycling that would consume the remaining arguments -- but is
        // not an error.
        let (status, out, _) = run_capturing("printf '%s\\c%s\\n' 1 2 3 4");
        assert_eq!(out, "1");
        assert_eq!(status, 0);
    }

    #[test]
    fn printf_pathological_fields_are_bounded() {
        // A width or precision far past any real use must neither panic nor drive
        // an unbounded allocation: MAX_FIELD (65535) caps them. Rust's float
        // formatter itself panics at precision >= 65536, so an unclamped `%f`
        // precision would abort under panic=abort.
        let (status, out, _) = run_capturing("printf '%.9999999999f' 1");
        assert_eq!(status, 0);
        assert_eq!(out.len(), 65535 + 2); // "1." + 65535 fraction digits
        let (status, out, _) = run_capturing("printf '%9999999999s' x");
        assert_eq!(status, 0);
        assert_eq!(out.len(), 65535);
        // Dynamic `*` width/precision are clamped the same way.
        let (status, out, _) = run_capturing("printf '%.*d' 9999999999 5");
        assert_eq!(status, 0);
        assert_eq!(out.len(), 65535);
    }

    #[test]
    fn printf_nan_empty_char_and_incomplete_directive() {
        // NaN prints the C spelling, not Rust's "NaN".
        assert_eq!(run_capturing("printf '%f' nan").1, "nan");
        // %c of an empty argument is a NUL byte (matches dash/busybox-ash).
        assert_eq!(run_capturing("printf '%c' ''").1, "\0");
        // A format ending mid-directive keeps the literal `%` and the modifiers
        // already consumed rather than swallowing them.
        assert_eq!(run_capturing("printf 'x%5'").1, "x%5");
    }

    #[test]
    fn printf_exponent_and_general_float_forms() {
        // %e/%E: one digit, six by default, a signed two-digit-minimum exponent.
        assert_eq!(
            run_capturing("printf '[%e][%E][%.0e][%#.0e][%.3e]' 3.14 3.14 3.14 3.14 -3.14").1,
            "[3.140000e+00][3.140000E+00][3e+00][3.e+00][-3.140e+00]"
        );
        // A three-digit exponent keeps all three digits.
        assert_eq!(run_capturing("printf '[%e][%e]' 1e300 1e-300").1, "[1.000000e+300][1.000000e-300]");
        // %g picks style f or e by exponent and drops trailing zeros; `#` keeps
        // them (and the point), and precision 0 means 1 significant digit.
        assert_eq!(
            run_capturing("printf '[%g][%#g][%g][%g][%g]' 3 3 100000 1000000 0.00001").1,
            "[3][3.00000][100000][1e+06][1e-05]"
        );
        assert_eq!(
            run_capturing("printf '[%.3g][%G][%#.0g][%g]' 1234.5 0.0001 3 0.000123456789").1,
            "[1.23e+03][0.0001][3.][0.000123457]"
        );
        // Rounding that carries into the next decade must re-pick the style.
        assert_eq!(run_capturing("printf '[%g][%g]' 9.9999995 999999.5").1, "[10][1e+06]");
        // With `#`, that carry keeps style f's (zero) fraction count -- glibc's
        // spelling, which dash/ash inherit. A carry that stays inside style e
        // (9995 at .3g), and a value style f never applied to (exponent below
        // -4), both keep the full fraction.
        assert_eq!(
            run_capturing("printf '[%#.6g][%#.3g][%#.3g][%#.6g][%#.6g]' 999999.5 999.5 9995 1000000 0.00001").1,
            "[1.e+06][1.e+03][1.00e+04][1.00000e+06][1.00000e-05]"
        );
        // Width/justification/zero-fill apply to the whole converted field.
        assert_eq!(
            run_capturing("printf '[%12.3e][%-12.3e][%012.3e][%015.4g]' -3.14 -3.14 -3.14 1234567").1,
            "[  -3.140e+00][-3.140e+00  ][-003.140e+00][0000001.235e+06]"
        );
    }

    #[test]
    fn printf_float_infinity_and_nan_spellings() {
        // C spells these inf/nan, uppercased by an uppercase conversion, and
        // ignores the 0 flag for them (glibc pads with spaces).
        assert_eq!(
            run_capturing("printf '[%f][%F][%e][%E][%g][%G]' inf inf inf -inf nan nan").1,
            "[inf][INF][inf][-INF][nan][NAN]"
        );
        assert_eq!(run_capturing("printf '[%08f][%-8f][%+f]' inf inf inf").1, "[     inf][inf     ][+inf]");
        // The sign bit survives, including on NaN and negative zero.
        assert_eq!(run_capturing("printf '[%f][%g][%e]' -nan -0.0 -0").1, "[-nan][-0][-0.000000e+00]");
        // INFINITY/NAN spellings are case-insensitive; a longer word keeps only
        // the prefix that converted, which is then an unconverted tail.
        assert_eq!(run_capturing("printf '[%f][%f]' INFINITY Inf").1, "[inf][inf]");
        let (status, out, _) = run_capturing("printf '[%f]' infinit");
        assert_eq!(out, "[inf]");
        assert_eq!(status, 1);
    }

    #[test]
    fn printf_float_operands_follow_strtod() {
        // C99 hex floats convert like strtod, including a bare `0x` (which is not
        // a prefix at all, so only the `0` converts and `x` is a tail).
        assert_eq!(run_capturing("printf '[%g][%g][%g][%g]' 0x1p2 0x1.8p1 0X1P-1 0x10").1, "[4][3][0.5][16]");
        // Exactly representable subnormals are in range; inexact ones are not.
        assert_eq!(run_capturing("printf '[%.0e]' 0x1p-1074").1, "[5e-324]");
        assert_eq!(run_capturing("printf '[%.0e]' 0x1p-1074").0, 0);
        assert_eq!(run_capturing("printf '[%.0e]' 0x1.8p-1074").0, 1);
        // Tininess is judged after rounding to 53 bits, so two operands that both
        // land on the smallest normal split: `0x1.fffffffffffffp-1023` is tiny
        // first and rounds up (out of range), `0x0.fffffffffffffcp-1022` rounds up
        // to 2^-1022 before the test and is in range.
        let (status, out, _) = run_capturing("printf '[%.17g]' 0x1.fffffffffffffp-1023");
        assert_eq!(out, "[2.2250738585072014e-308]");
        assert_eq!(status, 1);
        let (status, out, _) = run_capturing("printf '[%.17g]' 0x0.fffffffffffffcp-1022");
        assert_eq!(out, "[2.2250738585072014e-308]");
        assert_eq!(status, 0);
        // A tail leaves the partial conversion in place but fails (dash's
        // check_conversion), and so does an out-of-range magnitude.
        let (status, out, err) = run_capturing("printf '[%f]' '1 '");
        assert_eq!(out, "[1.000000]");
        assert_eq!(status, 1);
        assert!(err.contains("not completely converted"), "err: {err:?}");
        let (status, out, err) = run_capturing("printf '[%f]' 1e400");
        assert_eq!(out, "[inf]");
        assert_eq!(status, 1);
        assert!(err.contains("out of range"), "err: {err:?}");
        assert_eq!(run_capturing("printf '[%f]' 1e-400"), (1, "[0.000000]".into(), "printf: 1e-400: Numerical result out of range\n".into()));
        // Leading whitespace is skipped; a wholly unconvertible operand is 0.
        assert_eq!(run_capturing("printf '[%g]' '  42'").1, "[42]");
        let (status, out, err) = run_capturing("printf '[%f]' abc");
        assert_eq!(out, "[0.000000]");
        assert_eq!(status, 1);
        assert!(err.contains("expected a numeric value"), "err: {err:?}");
        // An operand that is absent, or present but empty, is a silent zero.
        assert_eq!(run_capturing("printf '[%f]'"), (0, "[0.000000]".into(), String::new()));
        assert_eq!(run_capturing("printf '[%f]' ''"), (0, "[0.000000]".into(), String::new()));
    }

    #[test]
    fn getopts_scans_options_clusters_and_arguments() {
        // OPTIND names the next WORD, so it is already 2 for both letters of a
        // cluster; OPTARG is set empty for a flag that takes no argument.
        assert_eq!(
            run_capturing("set -- -ab; getopts ab o; echo \"$OPTIND $o [$OPTARG]\"; getopts ab o; echo \"$OPTIND $o [$OPTARG]\"").1,
            "2 a []\n2 b []\n"
        );
        // An option argument may be smooshed or the following word.
        assert_eq!(run_capturing("set -- -c10; getopts 'c:' o; echo \"$OPTIND $o $OPTARG\"").1, "2 c 10\n");
        assert_eq!(run_capturing("set -- -c 10; getopts 'c:' o; echo \"$OPTIND $o $OPTARG\"").1, "3 c 10\n");
        // End of options: status 1 and `?`, both for exhaustion and for `--`.
        assert_eq!(run_capturing("set -- ; getopts 'a' o; echo \"$? $o\"").1, "1 ?\n");
        assert_eq!(run_capturing("set -- -- -a; getopts 'a' o; echo \"$? $o $OPTIND\"").1, "1 ? 2\n");
        // A non-option word stops the scan without being consumed.
        assert_eq!(run_capturing("set -- x -a; getopts 'a' o; echo \"$? $OPTIND\"").1, "1 1\n");
    }

    #[test]
    fn getopts_reports_errors_and_restarts() {
        // Normal mode: unknown option and missing argument both report `?` with a
        // message, and still return 0 so the caller's loop sees them.
        let (_, out, err) = run_capturing("set -- -Z; getopts 'a:' o; echo \"$? $o\"");
        assert_eq!(out, "0 ?\n");
        // A bare `fprintf` (ash.c:11714), not `nextopt`'s shell diagnostic:
        // hence the capital, and no name.
        assert_eq!(err, "Illegal option -Z\n");
        let (_, out, err) = run_capturing("set -- -a; getopts 'a:' o; echo \"$? $o\"");
        assert_eq!(out, "0 ?\n");
        assert_eq!(err, "No arg for -a option\n");
        // A leading `:` selects silent mode: the letter comes back in OPTARG, and a
        // missing argument reports `:` instead of `?`.
        assert_eq!(run_capturing("set -- -Z; getopts ':a:' o; echo \"$o $OPTARG\"").1, "? Z\n");
        assert_eq!(run_capturing("set -- -a; getopts ':a:' o; echo \"$o $OPTARG\"").1, ": a\n");
        // New positional parameters restart the scan (dash), so a second loop over
        // a fresh argument list does not resume at the old OPTIND.
        assert_eq!(
            run_capturing("set -- -a; getopts 'a' o; set -- -b; getopts 'b' o; echo \"$o $OPTIND\"").1,
            "b 2\n"
        );
        // Assigning OPTIND restarts at a word boundary even when the value does
        // not change, so a half-consumed cluster is abandoned (dash's hook).
        assert_eq!(
            run_capturing("set -- -ab; getopts ab o; OPTIND=2; getopts ab o; echo \"$o $?\"").1,
            "? 1\n"
        );
        // OPTIND is 1 before any getopts, and a non-positive one is fatal.
        assert_eq!(run_capturing("echo $OPTIND").1, "1\n");
        // The cursor is per argument frame: a function scans its OWN arguments
        // from the start, the caller resumes where it left off, and `shift` (which
        // renumbers them) restarts it.
        assert_eq!(
            run_capturing("set -- -a; getopts a o; f() { getopts b i; echo \"$? $i $OPTIND\"; }; f -b").1,
            "0 b 2\n"
        );
        assert_eq!(run_capturing("set -- -a -b; getopts a o; shift; getopts b o; echo $o").1, "b\n");
        // `set` moves only the hidden cursor: $OPTIND keeps the value the last
        // getopts published, so a readonly OPTIND is not disturbed either.
        assert_eq!(run_capturing("set -- -a; getopts a o; set -- x; echo $OPTIND").1, "2\n");
        assert_eq!(run_capturing("readonly OPTIND; set -- x; echo ok=$?").1, "ok=0\n");
        // dash's number(): an all-digit OPTIND is taken (0 coerced up to 1), and
        // anything else -- negative, signed, padded, non-numeric -- is an error.
        // It ends the COMMAND and not the shell, `getopts` being regular, so the
        // status stands and the next command runs.
        assert_eq!(run_capturing("set -- -a; OPTIND=0; getopts a o; echo \"$o $?\"").1, "a 0\n");
        for bad in ["-1", "abc", "' 2'", "+1"] {
            let (_, out, err) =
                run_capturing(&format!("OPTIND={bad}; getopts a: x; echo \"st=$?\"; echo AFTER"));
            assert_eq!(out, "st=2\nAFTER\n", "OPTIND={bad}");
            assert!(err.contains("Illegal number"), "OPTIND={bad}: {err:?}");
        }
    }

    #[test]
    fn read_rejects_a_bad_variable_name() {
        // ash checks every name BEFORE reading and copies bash's message, which
        // QUOTES the name -- and it is status 1, not the usage 2 a bad option gets.
        for bad in ["1bad", "a-b", ""] {
            let (status, _, err) = run_capturing(&format!("echo hi | read '{bad}'"));
            assert_eq!(status, 1, "read '{bad}'");
            assert!(err.contains(&format!("'{bad}': bad variable name")), "err: {err:?}");
        }
    }

    #[test]
    fn read_takes_ashs_option_surface() {
        // `-r` keeps the backslash. Printed with `printf %s`, not `echo`, which
        // would expand it again.
        assert_eq!(
            run_capturing("printf 'a\\\\b\\n' | { read -r v; printf '[%s]\\n' \"$v\"; }").1,
            "[a\\b]\n"
        );
        // A value-taking letter swallows the rest of its word, so `-rn1` is `-r -n 1`.
        assert_eq!(run_capturing("echo hi | { read -rn1 v; echo var=$v; }").1, "var=h\n");
        assert_eq!(run_capturing("echo hi | { read -pfoo v; echo $v; }").1, "hi\n");
        assert_eq!(run_capturing("echo hi | { read -p 'x? ' v; echo $v; }"), (0, "hi\n".into(), String::new()));
        // An unknown letter is still a usage error rather than silently ignored.
        for bad in ["-N 1", "-q", "-zz"] {
            let (status, _, err) = run_capturing(&format!("echo hi | {{ read {bad} v; }}"));
            assert_eq!(status, 2, "read {bad}");
            assert!(err.contains("illegal option"), "read {bad} err: {err:?}");
        }
    }

    #[test]
    fn read_without_names_sets_reply_unsplit() {
        // With no names there is no field splitting AND no IFS trimming, so the
        // surrounding blanks survive -- `read` and `read REPLY` are not the same
        // command, which is the whole point of the `argv[0]` test in ash.
        assert_eq!(run_capturing("printf ' a \\n' | { read; printf '[%s]' \"$REPLY\"; }").1, "[ a ]");
        assert_eq!(run_capturing("printf ' a \\n' | { read REPLY; printf '[%s]' \"$REPLY\"; }").1, "[a]");
    }

    #[test]
    fn read_counts_bytes_for_dash_n() {
        assert_eq!(run_capturing("printf abcd | { read -n 2 v; echo \"[$v] $?\"; }").1, "[ab] 0\n");
        // `-n 0` is no limit, not "read nothing" (bash 3.2 does this too).
        assert_eq!(run_capturing("printf abcd | { read -n 0 v; echo \"[$v] $?\"; }").1, "[abcd] 1\n");
        // The count ticks on a SWALLOWED byte too -- ash's `continue` reaches the
        // `while (--nchars)` -- so the backslash of `\ab` spends one of the two.
        assert_eq!(run_capturing("printf '\\\\ab\\n' | { read -n 2 v; echo \"[$v]\"; }").1, "[a]\n");
        // A delimiter still ends it early, and that is not a failure.
        assert_eq!(run_capturing("echo b | { read -n 2 v; echo \"[$v] $?\"; }").1, "[b] 0\n");
    }

    #[test]
    fn read_delimiter_is_dash_d() {
        assert_eq!(run_capturing("printf 'a:b' | { read -d : v; echo \"[$v] $?\"; }").1, "[a] 0\n");
        // `-d ''` leaves the delimiter at the string terminator, so it is NUL --
        // and it works only because ash tests the delimiter BEFORE skipping NULs.
        assert_eq!(run_capturing("printf 'a\\0b\\n' | { read -d '' v; echo \"[$v] $?\"; }").1, "[a] 0\n");
        // The value may look like an option; it is consumed as `-d`'s argument.
        assert_eq!(run_capturing("echo foo-bar | { read -d -; echo reply=$REPLY; }").1, "reply=foo\n");
        // An ESCAPED delimiter is literal and does not end the read -- ash tests
        // the backslash before the delimiter, so `\:` survives a `-d :`. Under
        // `-r` there is no escape and the backslash itself is the value's last byte.
        assert_eq!(
            run_capturing("printf 'a\\\\:b:c\\n' | { read -d : v; printf '[%s]' \"$v\"; }").1,
            "[a:b]"
        );
        assert_eq!(
            run_capturing("printf 'a\\\\:b:c\\n' | { read -rd : v; printf '[%s]' \"$v\"; }").1,
            "[a\\]"
        );
    }

    #[test]
    fn read_ifs_follows_the_c_locale_and_the_two_step_word_state() {
        // `isspace` in the C locale, which is what decides a whitespace IFS byte
        // from a delimiting one, includes CR -- so a doubled `\r` separates two
        // fields rather than leaving one in the second.
        assert_eq!(
            run_capturing("printf 'a\\r\\rb\\n' | { IFS=$(printf '\\r') read p q; printf '[%s][%s]' \"$p\" \"$q\"; }").1,
            "[a][b]"
        );
        // After a field ends on a WHITESPACE delimiter, one following non-space
        // delimiter still separates rather than opening an empty field: ash tracks
        // that with a two-step `startword`, and collapsing it to one step makes
        // this `[X][:Y]`.
        assert_eq!(
            run_capturing("printf 'X :Y\\n' | { IFS=': ' read x y; printf '[%s][%s]' \"$x\" \"$y\"; }").1,
            "[X][Y]"
        );
        // A NUL byte is DROPPED, not stored -- and not merely tested after the
        // delimiter, which is a weaker property that `-d ''` alone would show.
        assert_eq!(run_capturing("printf 'a\\0b\\n' | { read v; printf '[%s]' \"$v\"; }").1, "[ab]");
    }

    #[test]
    fn read_last_name_eats_one_trailing_delimiter() {
        // The last name takes the remainder WITH its delimiters, except that a
        // single trailing non-space delimiter is dropped when the fields exactly
        // filled the names.
        let f = |s: &str| run_capturing(&format!("printf '{s}' | {{ IFS=: read x y; echo \"|$x|$y|\"; }}")).1;
        assert_eq!(f("X:Y:\\n"), "|X|Y|\n");
        assert_eq!(f("X:Y:Z:\\n"), "|X|Y:Z:|\n");
        assert_eq!(f("X:Y:Z\\n"), "|X|Y:Z|\n");
        // Trailing whitespace IFS goes regardless, and an unfilled name is empty.
        assert_eq!(run_capturing("printf 'a b \\n' | { read v; echo \"[$v]\"; }").1, "[a b]\n");
        assert_eq!(run_capturing("printf 'one\\n' | { read a b c; echo \"|$a|$b|$c|\"; }").1, "|one|||\n");
    }

    #[test]
    fn read_option_values_are_bb_strtou() {
        // busybox's `bb_strtou` takes decimal digits and nothing else, so a sign,
        // a leading space or a trailing letter is an error -- with a message naming
        // which option, and status 2.
        for (opt, msg) in [("n", "invalid count"), ("u", "invalid file descriptor"), ("t", "invalid timeout")] {
            for bad in ["-1", "+2", " 2", "2x", "x"] {
                let (status, _, err) = run_capturing(&format!("read -{opt} '{bad}' v </dev/null"));
                assert_eq!(status, 2, "read -{opt} '{bad}'");
                assert!(err.contains(msg), "read -{opt} '{bad}' err: {err:?}");
            }
        }
        // `-t` also takes bash 4.3's fractional form: THREE fractional digits are
        // read, a non-digit among those three is an error, and anything past them
        // is ignored. So `0.123x` is invalid while `0.123456xyz` is not.
        for good in ["1", "0.5", "1.", "00.5", "0.123x", "0.123456xyz"] {
            let (status, _, err) = run_capturing(&format!("read -t {good} v </dev/null; echo rc=$?"));
            assert_eq!((status, err.as_str()), (0, ""), "read -t {good}");
        }
        // `-n` and `-u` land in an `int` and stop at INT_MAX; `-t`'s milliseconds
        // are unsigned and go to UINT_MAX, so the ceilings are NOT the same.
        for (opt, msg) in [("n", "invalid count"), ("u", "invalid file descriptor")] {
            let (status, _, err) = run_capturing(&format!("read -{opt} 2147483648 v </dev/null"));
            assert_eq!(status, 2, "read -{opt} 2147483648");
            assert!(err.contains(msg), "err: {err:?}");
        }
        assert_eq!(run_capturing("read -t 2147483648 v </dev/null; echo $?").1, "1\n");
        let (status, _, err) = run_capturing("read -t 4294967296 v </dev/null");
        assert_eq!(status, 2);
        assert!(err.contains("invalid timeout"), "err: {err:?}");
        for bad in [".5", "0.x", "0.1x", "0.12x", "1e3"] {
            let (status, _, err) = run_capturing(&format!("read -t {bad} v </dev/null"));
            assert_eq!(status, 2, "read -t {bad}");
            assert!(err.contains("invalid timeout"), "read -t {bad} err: {err:?}");
        }
    }

    #[test]
    fn read_dash_t_asks_whether_a_read_would_block() {
        // `-t 0` reads NOTHING -- not even $REPLY -- and reports whether a read
        // would block. That is poll's question, not "are bytes left": a file at
        // EOF and a closed descriptor are both READY, which is why ash answers 0
        // for `/dev/zero`, for `/dev/null` and even for a closed descriptor.
        assert_eq!(run_capturing("read -t 0 </dev/null; echo $?").1, "0\n");
        assert_eq!(run_capturing("read -t 0.0 </dev/null; echo $?").1, "0\n");
        // A PIPELINE stage is answered now rather than refused, which is the
        // whole of this surface's point: its stdin is a real pipe whose writer
        // is a sibling stage, so the question is about another thread's
        // progress and only the kernel can settle it. Asserted on what the
        // STAGE printed, since the program's status is the last stage's `echo`.
        //
        // A NONZERO timeout, deliberately. `-t 0` on a live pipe is a race by
        // definition -- it asks whether the sibling has written YET -- and bash
        // loses it too, answering 1 about once in twelve runs of
        // `echo foo | { read -t 0; echo $?; }`. Asserting it would be asserting
        // the scheduler. With a timeout the answer is determinate: poll waits
        // for the write or for the EOF that follows it.
        for (cmd, want) in [
            ("echo x | { read -t 5 v; echo \"[$v] $?\"; }", "[x] 0\n"),
            // A producer that ends WITHOUT writing is ready too, because EOF is
            // not "bytes remain": the read returns at once, empty, reporting 1.
            (": | { read -t 5 v; echo \"[$v] $?\"; }", "[] 1\n"),
        ] {
            let (_, out, err) = run_capturing(cmd);
            assert_eq!(out, want, "{cmd} err: {err:?}");
        }
        // Where a read cannot block, a nonzero timeout is simply never reached.
        assert_eq!(run_capturing("read -t 5 x </dev/null; echo $?").1, "1\n");
        // A SUB-MILLISECOND timeout still reads what is already there. poll
        // counts whole milliseconds and the deadline is nanoseconds, so
        // truncating the remainder rather than rounding it up made the FIRST
        // poll of a 1ms deadline see zero and report a timeout at once.
        //
        // Read from a regular file, and exactly ONE byte of it. A file is
        // always poll-ready, so nothing but the rounding decides the answer,
        // and `-n 1` leaves the deadline to bound a single poll rather than a
        // whole line. Asking a sibling pipeline STAGE for the byte instead
        // would be asserting the scheduler -- the same objection the comment
        // above makes about `-t 0`, and measured: that form failed 6 times in
        // 20 with the machine loaded, where this one failed none.
        assert_eq!(
            run_capturing("read -t 0.001 -n 1 v < /proc/self/status; echo \"$?/$v\"").1,
            "0/N\n"
        );
        // The shell's own in-memory entries are answered without a syscall: a
        // here-document cursor cannot block whatever the kernel would say.
        assert_eq!(
            run_capturing("read -t 0 v <<EOF\nhi\nEOF\necho $?").1,
            "0\n"
        );
    }

    #[test]
    fn read_takes_a_descriptor_with_dash_u() {
        assert_eq!(
            run_capturing("exec 3<<EOF\nzz\nEOF\nread -u 3 w; echo $w").1,
            "zz\n"
        );
        // `-u` and `-d` compose, and with no names the value lands in REPLY.
        assert_eq!(
            run_capturing("read -u 3 -d 5 3<<EOF\n123456789\nEOF\necho reply=$REPLY").1,
            "reply=1234\n"
        );
    }

    #[test]
    fn test_file_type_and_mode_operators() {
        // Character device vs socket, on a node every Linux system has.
        assert_eq!(run_capturing("test -c /dev/zero; echo $?").1, "0\n");
        assert_eq!(run_capturing("test -S /dev/zero; echo $?").1, "1\n");
        assert_eq!(run_capturing("test -b /dev/zero; echo $?").1, "1\n");
        // Sticky but not setuid, on a directory this test OWNS. Asking `/tmp`
        // instead reds the gate on any host whose `/tmp` is a tmpfs mounted 0777
        // without S_ISVTX -- the corpus skips its own sticky-bit case for exactly
        // that reason, and a unit test may not reintroduce the assumption.
        use std::os::unix::fs::PermissionsExt;
        let sticky = std::env::temp_dir().join(format!("td-sh-sticky-{}", std::process::id()));
        std::fs::create_dir_all(&sticky).unwrap();
        std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let dir = sticky.display();
        assert_eq!(run_capturing(&format!("test -k {dir}; echo $?")).1, "0\n");
        assert_eq!(run_capturing(&format!("test -u {dir}; echo $?")).1, "1\n");
        std::fs::remove_dir_all(&sticky).unwrap();
        // `-ef` is identity, so a path is always the same file as itself.
        assert_eq!(run_capturing("test /dev/zero -ef /dev/zero; echo $?").1, "0\n");
        assert_eq!(run_capturing("test /dev/zero -ef /dev/null; echo $?").1, "1\n");
        // `-nt`/`-ot` are false when either operand cannot be stat'd.
        assert_eq!(run_capturing("test /nonexistent -nt /dev/zero; echo $?").1, "1\n");
        // `-t` takes a descriptor number: any integer is answerable, a word is not.
        assert_eq!(run_capturing("test -t 12345678910; echo $?").1, "1\n");
        assert_eq!(run_capturing("test -t invalid; echo $?").1, "2\n");
    }

    #[test]
    fn test_rejects_double_equals_and_takes_three_arg_and_or() {
        // dash has no `==`; it is a missing-binary-operator error (status 2).
        assert_eq!(run_capturing("[ a = a ]; echo $?").1, "0\n");
        assert_eq!(run_capturing("[ a == a ] 2>/dev/null; echo $?").1, "2\n");
        // The 3-argument `-a`/`-o` forms combine non-empty string tests.
        assert_eq!(run_capturing("[ foo -a '' ]; echo $?").1, "1\n");
        assert_eq!(run_capturing("[ foo -o '' ]; echo $?").1, "0\n");
        assert_eq!(run_capturing("[ foo -a bar ]; echo $?").1, "0\n");
        // But not when the first word is itself a unary operator, nor when `!` or
        // `( )` claim the operands first: dash reports those as syntax errors.
        assert_eq!(run_capturing("[ -z -a ] ] 2>/dev/null; echo $?").1, "2\n");
        assert_eq!(run_capturing("[ ! -a B ] 2>/dev/null; echo $?").1, "2\n");
        assert_eq!(run_capturing("[ ! -z '' ]; echo $?").1, "1\n");
        // `!` binds tighter than `-a`/`-o`, so the 4-argument form is
        // `(! A) && B`, not `!(A && B)`.
        assert_eq!(run_capturing("[ ! x -a '' ]; echo $?").1, "1\n");
        assert_eq!(run_capturing("[ ! '' -o x ]; echo $?").1, "0\n");
        // `-t` answers for the SHELL's descriptor, so a redirection is not a tty.
        assert_eq!(run_capturing("[ -t 0 ] < /dev/null; echo $?").1, "1\n");
    }

    #[test]
    fn read_distinguishes_blank_line_from_eof() {
        // A blank line reads as an empty, successful line; end-of-input fails.
        let (_, out, _) = run_capturing("printf '\\n' | { read x; echo \"s=$? v=[$x]\"; }");
        assert_eq!(out, "s=0 v=[]\n");
        let (_, out, _) = run_capturing("printf '' | { read x; echo \"s=$? v=[$x]\"; }");
        assert_eq!(out, "s=1 v=[]\n");
    }

    #[test]
    fn read_reports_failure_on_unterminated_final_line() {
        // A last line with no trailing newline still assigns, but read fails (EOF).
        let (_, out, _) = run_capturing("printf 'abc' | { read x; echo \"s=$? v=[$x]\"; }");
        assert_eq!(out, "s=1 v=[abc]\n");
    }

    #[test]
    fn read_preserves_multibyte_utf8() {
        let (_, out, _) = run_capturing("printf 'héllo\\n' | { read x; echo \"$x\"; }");
        assert_eq!(out, "héllo\n");
    }

    #[test]
    fn command_v_reports_builtins_and_fails_unknown() {
        assert_eq!(run_capturing("command -v echo").1, "echo\n");
        assert_eq!(
            run_capturing("command -v no_such_cmd_xyz; echo $?").1,
            "127\n"
        );
    }

    #[test]
    fn alias_defines_lists_and_removes() {
        // dash prints `name='value'` with no `alias ` prefix, quoting the value
        // so the listing can be re-read.
        assert_eq!(
            run_capturing("alias e=echo ll='ls -l'\nalias e ll").1,
            "e='echo'\nll='ls -l'\n"
        );
        assert_eq!(run_capturing("alias q=\"it's\"\nalias q").1, "q='it'\"'\"'s'\n");
        // A name with no `=` is a lookup, and a miss is status 1.
        assert_eq!(
            run_capturing("alias e=echo nonexistentZ; echo status=$?").1,
            "status=1\n"
        );
        assert_eq!(
            run_capturing("alias e=echo\nunalias e nonexistentZ; echo status=$?").1,
            "status=1\n"
        );
        // The `=` is looked for from the SECOND byte, so `--` is a lookup.
        assert_eq!(run_capturing("alias -- foo=echo; echo status=$?").1, "status=1\n");
        assert_eq!(run_capturing("alias -- foo=echo\nfoo x").1, "x\n");
    }

    #[test]
    fn unalias_takes_only_dashs_option_surface() {
        // dash's option loop leaves no names to remove, and reports success.
        assert_eq!(run_capturing("unalias; echo status=$?").1, "status=0\n");
        assert_eq!(
            run_capturing("alias a=echo b=echo\nunalias -a\nalias; echo status=$?").1,
            "status=0\n"
        );
        // `--` ends the options, so the name after it is removed, not treated as one.
        assert_eq!(
            run_capturing("alias foo=echo\nunalias -- foo\nfoo x 2>/dev/null; echo $?").1,
            "127\n"
        );
        let (status, _, err) = run_capturing("unalias -z");
        assert_eq!((status, err.as_str()), (2, "unalias: illegal option -z\n"));
    }

    #[test]
    fn alias_is_substituted_only_from_the_next_parse_unit() {
        // A whole line is parsed before any of it runs, so the alias defined on
        // it is not yet in force for the rest of that line.
        assert_eq!(
            run_capturing("alias e=echo; e one 2>/dev/null\ne two; e three").1,
            "two\nthree\n"
        );
        // ... but `eval` and `.`-style unit loops do see their own definitions.
        assert_eq!(
            run_capturing("eval \"alias hi='echo hello'\nhi inside\"\nhi outside").1,
            "hello inside\nhello outside\n"
        );
        // A subshell inherits aliases but cannot publish one back.
        assert_eq!(
            run_capturing("alias e_='echo ['\n( e_ subshell )\necho $(e_ cmdsub)").1,
            "[ subshell\n[ cmdsub\n"
        );
        assert_eq!(
            run_capturing("echo $(alias hi='echo hello')\nhi 2>/dev/null; echo $?").1,
            "\n127\n"
        );
        // A `;` does not end the unit, so the alias is still not in force; the
        // newline after it does.
        assert_eq!(
            run_capturing("alias e=echo; e one 2>/dev/null; echo two\ne three").1,
            "two\nthree\n"
        );
    }

    #[test]
    fn a_lexical_error_is_reported_where_the_parse_reaches_it() {
        // The source is lexed in one pass, but what stopped the lexer surfaces only
        // when a unit needs that text: the commands before it have already run.
        let (status, out, err) = run_capturing("echo hi\necho 'unterminated");
        assert_eq!((status, out.as_str()), (2, "hi\n"));
        assert!(err.contains("unmatched"), "err: {err:?}");
        // ... and the command the bad text belongs to does NOT run.
        let (status, out, _) = run_capturing("echo before\necho VISIBLE <<EOF\nbody\n");
        assert_eq!((status, out.as_str()), (2, "before\n"));
    }

    #[test]
    fn alias_substitution_follows_dashs_scan_rules() {
        // Only an unquoted literal in command position is a candidate, and an
        // alias is not re-entered while its own replacement is being scanned.
        assert_eq!(run_capturing("alias echo='echo foo'\necho bar").1, "foo bar\n");
        assert_eq!(
            run_capturing("alias hi='echo hello world'\nhi\necho hi\n'hi' || echo failed").1,
            "hello world\nhi\nfailed\n"
        );
        assert_eq!(
            run_capturing("alias e=echo\ncmd=e\n$cmd X 2>/dev/null; echo $?").1,
            "127\n"
        );
        // A replacement ending in a blank makes the NEXT word a candidate too.
        assert_eq!(
            run_capturing("alias hi='echo hello '\nalias punct='!!!'\nhi punct").1,
            "hello !!!\n"
        );
        assert_eq!(
            run_capturing("alias hi='echo hello'\nalias punct='!!!'\nhi punct").1,
            "hello punct\n"
        );
        // The replacement is rescanned, so its own first word expands as well.
        assert_eq!(
            run_capturing("alias hi='e_ hello world'\nalias e_='echo __'\nhi").1,
            "__ hello world\n"
        );
        // The value is text, expanded when the command runs, not when defined.
        assert_eq!(
            run_capturing("x=x\nalias echo-x='echo $x'\nx=y\necho-x hi").1,
            "y hi\n"
        );
    }

    #[test]
    fn alias_replacement_can_supply_grammar() {
        // A replacement is re-lexed into the surrounding parse, so it can carry
        // reserved words, operators and newlines.
        assert_eq!(
            run_capturing("alias e_='for i in 1 2 3; do echo $i;'\ne_ done").1,
            "1\n2\n3\n"
        );
        assert_eq!(run_capturing("alias L='{'\nL echo one; echo two; }").1, "one\ntwo\n");
        assert_eq!(run_capturing("alias L='('\nL echo one; echo two )").1, "one\ntwo\n");
        assert_eq!(
            run_capturing("alias e_='echo 1\necho 2\necho 3'\nvar='echo foo'\ne_ ${var}").1,
            "1\n2\n3 echo foo\n"
        );
        // A reserved word wins over an alias of the same name (dash checks
        // keywords first), and a redirection target is never a candidate.
        assert_eq!(run_capturing("alias done=echo\nfor i in 1; do echo $i; done").1, "1\n");
    }

    #[test]
    fn alias_is_not_substituted_in_a_case_pattern() {
        // A pattern is not a command word -- and the newline between patterns
        // does not make one, which is what distinguishes this from every other
        // newline in the scan.
        assert_eq!(
            run_capturing("alias p=z\ncase z in\np) echo WRONG;;\n*) echo right;;\nesac").1,
            "right\n"
        );
        // The arm's body IS a command position, in both pattern forms.
        assert_eq!(run_capturing("alias e=echo\ncase z in\nz) e ARM;;\nesac").1, "ARM\n");
        assert_eq!(run_capturing("alias e=echo\ncase z in\n(z) e ARM;;\nesac").1, "ARM\n");
        // A nested case restores the outer state on `esac`.
        assert_eq!(
            run_capturing("alias e=echo\ncase a in\na) case b in\nb) e IN;;\nesac\ne OUT;;\nesac").1,
            "IN\nOUT\n"
        );
        // A pattern list opens after `(` and continues after `|`, so neither
        // makes a command position while patterns are being read.
        assert_eq!(
            run_capturing("alias hi='echo HI'\ncase hi in (hi) echo pat;; *) echo no;; esac").1,
            "pat\n"
        );
        assert_eq!(
            run_capturing("alias hi='echo HI'\ncase hi in a|hi) echo pat;; *) echo no;; esac").1,
            "pat\n"
        );
    }

    #[test]
    fn alias_scan_spans_the_replacement_boundary() {
        // The redirection target can sit past the end of the replacement, so the
        // command word after it still expands.
        assert_eq!(
            run_capturing("alias r='>'\nalias e=echo\nr /dev/null e hi; echo rc=$?").1,
            "rc=0\n"
        );
        // A `\` inside a comment is not a line continuation, so the next line is
        // its own parse unit and sees the alias.
        assert_eq!(run_capturing("alias e=echo # \\\ne ok").1, "ok\n");
        // A real continuation still holds the unit open.
        assert_eq!(
            run_capturing("alias e_='echo '\nalias one='ONE '\ne_ one \\\n  one").1,
            "ONE ONE\n"
        );
        // A replacement that trails off in a comment comments out the rest of the
        // line it was written on, which is text the replacement does not contain.
        assert_eq!(run_capturing("alias a='#'\na echo SURVIVES\necho after").1, "after\n");
        assert_eq!(
            run_capturing("alias a='echo hi #'\na SURVIVES\necho after").1,
            "hi\nafter\n"
        );
        // ... but a comment CLOSED inside the replacement does not leak out.
        assert_eq!(
            run_capturing("alias a='echo one # c\necho two'\na THREE").1,
            "one\ntwo THREE\n"
        );
        // dash spends the trailing-blank check on the next token READ, so a
        // redirection in between consumes it and the filename's successor is a
        // plain word again.
        assert_eq!(
            run_capturing("alias e='echo hi '\nalias t=WORLD\ne >/dev/null t; echo rc=$?").1,
            "rc=0\n"
        );
        // A replacement that cannot be lexed at all is a hard error: no further
        // input can complete it, so the unit loop must not keep reading lines.
        let (status, out, _) = run_capturing("alias e='cat <<EOF'\ne\necho after");
        assert_eq!((status, out.as_str()), (2, ""));
        // The check reaches even the positions the grammar reads by name: the
        // loop variable of a `for` and the `in` of a `case`.
        assert_eq!(
            run_capturing("alias F='for '\nalias eye='i '\nF eye in 1 2; do echo $i; done").1,
            "1\n2\n"
        );
        assert_eq!(
            run_capturing("alias c='case x '\nalias IN='in '\nc IN x) echo hit;; esac").1,
            "hit\n"
        );
        // A redirection FILENAME is one too, since dash spends the check on the
        // next token read whatever the grammar wanted there.
        assert_eq!(
            run_capturing("alias e=': < '\nalias f=/dev/null\ne f; echo rc=$?").1,
            "rc=0\n"
        );
        // A replacement may also be the operator that ENDS a `for` word list, so
        // the check has to precede the is-it-a-word test.
        assert_eq!(
            run_capturing("alias F='for i in x '\nalias S=';'\nF S do echo $i; done").1,
            "x\n"
        );
    }

    #[test]
    fn exit_trap_runs_once_on_the_way_out() {
        // POSIX: the action runs with the exiting status in `$?`, and its own
        // status is discarded -- only an `exit` inside it changes the result.
        let (status, out, _) = run_capturing("trap 'echo bye' EXIT\necho hi");
        assert_eq!((status, out.as_str()), (0, "hi\nbye\n"));
        let (status, out, _) = run_capturing("f() { echo cleanup; exit 42; }\ntrap f EXIT");
        assert_eq!((status, out.as_str()), (42, "cleanup\n"));
        let (status, _, _) = run_capturing("f() { return 42; }\ntrap f EXIT");
        assert_eq!(status, 0);
        assert_eq!(run_capturing("trap 'echo $?' EXIT\n(exit 7)").1, "7\n");
        // A syntax error still leaves the shell exiting through its trap.
        let (status, out, _) = run_capturing("trap 'echo FAILED' EXIT\nfor");
        assert_eq!((status, out.as_str()), (2, "FAILED\n"));
        // The action is taken out of the table before it runs, so an `exit` inside
        // it cannot send the shell round again.
        assert_eq!(run_capturing("trap 'echo once; exit' EXIT\n:").1, "once\n");
        // POSIX: in a trap action "the last command" is the one BEFORE the trap, so
        // a bare `exit` there reports the status the shell was already exiting with
        // -- a cleanup action must not rewrite it. (busybox ash reports the action's
        // own status here; nothing in the corpus pins either, so this follows POSIX
        // and dash.) An explicit operand still wins.
        assert_eq!(run_capturing("trap 'false; exit' EXIT\nexit 3").0, 3);
        assert_eq!(run_capturing("trap 'exit 42' EXIT\nexit 3").0, 42);
        // ... and outside a trap a bare `exit` still reports the last command.
        assert_eq!(run_capturing("false\nexit").0, 1);
    }

    #[test]
    fn export_p_lists_sorted_and_single_quoted() {
        // ash's `showvars`: one line per name in lexicographic order, the value
        // single-quoted, and no `=` at all for a name that has no value.
        for (src, want) in [
            ("export a=1; export b; export -p", "export a='1'\nexport b\n"),
            // Sorted, not insertion-ordered -- the table is a hash.
            ("export z=1; export a=2; export -p", "export a='2'\nexport z='1'\n"),
            // Bare `export` is the same listing; `-p` only suppresses nothing.
            ("export z=1; export a=2; export", "export a='2'\nexport z='1'\n"),
            ("export z=1; export a=2; export -pp", "export a='2'\nexport z='1'\n"),
            // `--` ends the options and is NOT an operand, so this still lists.
            ("export z=1; export a=2; export --", "export a='2'\nexport z='1'\n"),
            ("export e=; export -p", "export e=''\n"),
            // The value goes in raw: single quotes are literal for everything but
            // a quote, which closes and re-opens around a `"`-quoted run.
            ("export s='a\\b$c'; export -p", "export s='a\\b$c'\n"),
            ("export q=\"it's\"; export -p", "export q='it'\"'\"'s'\n"),
            ("export n='x\ny'; export -p", "export n='x\ny'\n"),
            // Nothing exported, so nothing listed -- not a blank line.
            ("a=1; export -p", ""),
            // `unset` of an exported name takes the attribute, so it drops out.
            ("export a=A; unset a; export -p", ""),
        ] {
            let (status, out, err) = run_capturing(src);
            assert_eq!((status, out.as_str()), (0, want), "{src}: {err}");
        }
    }

    #[test]
    fn readonly_p_lists_only_readonly_names() {
        for (src, want) in [
            ("readonly r=5; readonly -p", "readonly r='5'\n"),
            ("readonly r=5; readonly", "readonly r='5'\n"),
            ("readonly r=5; readonly --", "readonly r='5'\n"),
            // Nothing readonly is a listing of nothing, which is NOT a write --
            // so a closed stdout is not an error the way a real line would be.
            ("export a=1; readonly -p >&-; echo rc=$?", "rc=0\n"),
            // The two attributes are listed by two different builtins, and a name
            // carrying both appears under each.
            ("export a=1; readonly a; readonly -p", "readonly a='1'\n"),
            ("export a=1; readonly a; export -p", "export a='1'\n"),
            ("export a=1; readonly -p", ""),
            ("readonly r; readonly -p", "readonly r\n"),
        ] {
            let (status, out, err) = run_capturing(src);
            assert_eq!((status, out.as_str()), (0, want), "{src}: {err}");
        }
    }

    #[test]
    fn an_operand_suppresses_the_listing() {
        // ash's `exportcmd` returns as soon as it has one operand, so `-p` with a
        // name prints nothing -- its own comment records that bash differs.
        for src in [
            "export a=1 b=2; export -p a",
            "export a=1; export -p a",
            "readonly r=5; readonly -p r",
            // The option is read letter by letter, so a repeat is still just `-p`
            // -- and still loses to the operand.
            "export a=1; export -pp a",
        ] {
            let (status, out, err) = run_capturing(src);
            assert_eq!((status, out.as_str()), (0, ""), "{src}: {err}");
        }
    }

    /// `trap ''` installs a REAL `SIG_IGN`, and a disposition is PROCESS-global.
    /// A test that leaves one behind both races its neighbours on cargo's thread
    /// pool and hands the rest of the run a test binary that ignores the signal
    /// -- a `cargo test` no longer answering Ctrl-C, say. This serialises the
    /// tests that touch one and puts back what it found. (`sys.rs`'s own tests
    /// hold a different lock, which is why they reserve signals 63/64: nothing
    /// here may name those.)
    static DISPOSITIONS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Dispositions {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(u8, crate::sys::Disposition)>,
    }

    impl Dispositions {
        fn held(signals: &[u8]) -> Self {
            let _lock = DISPOSITIONS.lock().unwrap_or_else(|e| e.into_inner());
            let saved = signals
                .iter()
                .filter_map(|s| crate::sys::signal_get(*s).ok().flatten().map(|d| (*s, d)))
                .collect();
            Self { _lock, saved }
        }
    }

    impl Drop for Dispositions {
        fn drop(&mut self) {
            for (signo, disp) in &self.saved {
                let _ = crate::sys::signal_set(*signo, *disp);
            }
        }
    }

    /// A nested guard does not hand the disposition back when the INNER one ends.
    ///
    /// This is a property of the reference count itself, not of a failure any
    /// shape reaches today: a pipeline stage is exempt from the guard before the
    /// count is touched, so no second holder exists to nest with. What it pins is
    /// what a future second `hold` site would depend on and could not see missing
    /// — an unconditional restore reds it — which is the whole reason the count
    /// is kept rather than reduced to a bool.
    #[test]
    fn a_nested_guard_does_not_hand_the_terminal_back_early() {
        let _held = Dispositions::held(&[2, 3]);
        let mut sh = crate::exec::Shell::new();
        for sig in [2u8, 3u8] {
            let _ = crate::sys::signal_set(sig, crate::sys::Disposition::Default);
        }
        let outer = crate::process::InterruptibleChild::hold(&mut sh);
        {
            let _inner = crate::process::InterruptibleChild::hold(&mut sh);
            for sig in [2u8, 3u8] {
                assert_eq!(
                    crate::sys::signal_get(sig).ok().flatten(),
                    Some(crate::sys::Disposition::Ignore),
                    "signal {sig} was not ignored with two guards standing"
                );
            }
        }
        // The inner one is gone and the outer is still waiting on its own child.
        for sig in [2u8, 3u8] {
            assert_eq!(
                crate::sys::signal_get(sig).ok().flatten(),
                Some(crate::sys::Disposition::Ignore),
                "signal {sig} was handed back while another guard was still held - \
                 that stage's next child would be created interruptible"
            );
        }
        drop(outer);
        for sig in [2u8, 3u8] {
            assert_eq!(
                crate::sys::signal_get(sig).ok().flatten(),
                Some(crate::sys::Disposition::Default),
                "signal {sig} was left ignored after the LAST guard went"
            );
        }
    }

    /// The guard the shell holds while a foreground child runs really does ignore
    /// BOTH the terminal's signals, and really does put both back.
    ///
    /// Held under the same lock as the other disposition tests, because this is
    /// process-global state and `trap` tests name signal 2 too. What a child
    /// running under the guard SEES is asserted end-to-end over the built binary
    /// instead (conformance's
    /// `the_shell_stops_listening_to_the_terminal_while_a_child_runs`, which
    /// reads both processes' masks out of `/proc`); what this pins is the
    /// mechanism that rests on.
    #[test]
    fn the_shell_stops_listening_to_the_terminal_while_a_child_runs() {
        let _held = Dispositions::held(&[2, 3]);
        let mut sh = crate::exec::Shell::new();
        for sig in [2u8, 3u8] {
            // Start from the default, which is the state `may_set_signal` admits.
            let _ = crate::sys::signal_set(sig, crate::sys::Disposition::Default);
            assert_eq!(
                crate::sys::signal_get(sig).ok().flatten(),
                Some(crate::sys::Disposition::Default),
                "the shell did not start from a default signal {sig}"
            );
        }
        {
            let _guard = crate::process::InterruptibleChild::hold(&mut sh);
            for sig in [2u8, 3u8] {
                assert_eq!(
                    crate::sys::signal_get(sig).ok().flatten(),
                    Some(crate::sys::Disposition::Ignore),
                    "signal {sig} was not ignored while a child was running - the \
                     keystroke would kill the shell beside the command"
                );
            }
        }
        for sig in [2u8, 3u8] {
            assert_eq!(
                crate::sys::signal_get(sig).ok().flatten(),
                Some(crate::sys::Disposition::Default),
                "signal {sig} was left ignored after the child ended - the next \
                 keystroke at the prompt would do nothing"
            );
        }
    }

    /// A signal the shell was HANDED ignored is not the guard's to move, and not
    /// its to hand back either. POSIX says a shell cannot reset one -- it is what
    /// makes `nohup` stick -- and a shell that already ignores SIGINT already
    /// survives a foreground child dying of it. The `trap '' INT` skip is covered
    /// end-to-end over the built binary; this is the other skip, which only a
    /// process that STARTED that way can show.
    #[test]
    fn a_signal_ignored_on_entry_is_not_the_guard_s_to_move() {
        let _held = Dispositions::held(&[2, 3]);
        // Ignored BEFORE the shell exists, so `may_set_signal`'s one question --
        // asked on first touch and cached, because after a change the process can
        // no longer be asked what it started with -- answers no.
        let _ = crate::sys::signal_set(2, crate::sys::Disposition::Ignore);
        let mut sh = crate::exec::Shell::new();
        assert!(
            !super::may_set_signal(&mut sh, 2),
            "an entry ignore was read as settable"
        );
        {
            let _guard = crate::process::InterruptibleChild::hold(&mut sh);
            assert_eq!(
                crate::sys::signal_get(2).ok().flatten(),
                Some(crate::sys::Disposition::Ignore),
                "the guard moved a signal it was handed ignored"
            );
        }
        // ...and put nothing back, which is the half that would break `nohup`:
        // handing back the default here would leave the shell killable by a
        // signal its parent had arranged for it to survive.
        assert_eq!(
            crate::sys::signal_get(2).ok().flatten(),
            Some(crate::sys::Disposition::Ignore),
            "the guard restored the default over an ignore it never installed"
        );
    }

    #[test]
    fn trap_prints_what_is_in_force() {
        let _held = Dispositions::held(&[2, 10, 15]);
        // dash's format, in signal-number order, with the action single-quoted.
        assert_eq!(
            run_capturing("trap 'echo test' TERM 2 EXIT\ntrap").1,
            "trap -- 'echo test' EXIT\ntrap -- 'echo test' INT\ntrap -- 'echo test' TERM\ntest\n"
        );
        // An empty action is POSIX's "ignore" and prints as such; a multi-line one
        // keeps its newlines inside the quotes.
        assert_eq!(run_capturing("trap '' USR1\ntrap").1, "trap -- '' USR1\n");
        assert_eq!(
            run_capturing("trap 'echo 1\necho 2' INT\ntrap").1,
            "trap -- 'echo 1\necho 2' INT\n"
        );
        assert_eq!(run_capturing("trap").1, "");
    }

    #[test]
    fn trap_conditions_follow_dashs_decoding() {
        // HUP and INT: this one RESETS rather than ignores, but a reset is a
        // write too, and the guard is what serialises it against the tests that
        // assert a disposition. KILL/STOP below never reach the kernel.
        let _held = Dispositions::held(&[1, 2]);
        // A `SIG` prefix is not accepted, the bare name is, and the comparison is
        // case-insensitive -- so `trap - int` clears INT.
        let (_, out, _) = run_capturing("trap - SIGINT\necho $?\ntrap - INT\necho $?");
        assert_eq!(out, "1\n0\n");
        assert_eq!(run_capturing("trap 'echo x' INT\ntrap - int\ntrap").1, "");
        // Numbers name signals, and 0 is EXIT.
        assert_eq!(run_capturing("trap 'echo x' 1\ntrap").1, "trap -- 'echo x' HUP\n");
        assert_eq!(run_capturing("trap 'echo x' 0\ntrap").1, "trap -- 'echo x' EXIT\nx\n");
        // Untrappable signals are still accepted, as dash accepts them.
        assert_eq!(run_capturing("trap 'echo hi' KILL STOP\necho $?").1, "0\n");
        // POSIX: an unsigned first operand means every operand is a CONDITION, so
        // this resets rather than setting the action to `0`.
        assert_eq!(run_capturing("trap 'echo noprint' EXIT\ntrap 0 EXIT\necho ok").1, "ok\n");
        // Out of range is a bad CONDITION, not an action that happens to be digits.
        let (_, out, _) = run_capturing("trap 'echo noprint' EXIT\ntrap 256 EXIT\necho $?");
        assert_eq!(out, "1\n");
        // Digits ONLY, as dash's `is_number` has it: with a blank in it this is an
        // action again, so it REPLACES the EXIT trap rather than resetting it.
        assert_eq!(run_capturing("trap 'echo noprint' EXIT\ntrap ' echo spaced ' EXIT").1, "spaced\n");
        // A single operand is a condition too: `trap EXIT` clears it.
        assert_eq!(run_capturing("trap 'echo noprint' EXIT\ntrap EXIT\necho ok").1, "ok\n");
    }

    #[test]
    fn trap_usage_errors_are_fatal_but_still_run_the_exit_trap() {
        // `trap` is a special builtin, so a bad option ends the script -- with the
        // EXIT trap that was already in force still firing.
        let (status, out, _) = run_capturing("trap 'echo trap-exit' EXIT\ntrap -1 EXIT\necho bad");
        assert_eq!((status, out.as_str()), (2, "trap-exit\n"));
        // A bad CONDITION is not fatal: it reports and carries on with status 1.
        let (status, out, _) = run_capturing("trap 'echo x' NOSUCH\necho status=$?");
        assert_eq!((status, out.as_str()), (0, "status=1\n"));
    }

    /// `trap ''` is the half of `trap` that leaves the shell, so it has to reach
    /// the kernel; `trap 'action'` is the half that cannot be served without a
    /// handler, so it asks for the default rather than pretending.
    ///
    /// Each `run_capturing` is a fresh SHELL in this one PROCESS, where a real
    /// script would be a process of its own — so INT is put back between them,
    /// which is what a new process would have found.
    /// `signal_get` without the `Result`/`Option` scaffolding, and without
    /// naming the module's items anywhere but here: the confinement tests refuse
    /// an import out of `sys`, and repeating the whole path at every assertion
    /// would drown them.
    fn disposition(signo: u8) -> Option<crate::sys::Disposition> {
        crate::sys::signal_get(signo).ok().flatten()
    }

    fn ignored(signo: u8) -> bool {
        disposition(signo) == Some(crate::sys::Disposition::Ignore)
    }

    fn defaulted(signo: u8) -> bool {
        disposition(signo) == Some(crate::sys::Disposition::Default)
    }

    /// Put a signal back to the default, which is what a fresh PROCESS would
    /// have found. Each `run_capturing` is a new shell but not a new process.
    fn reset(signo: u8) {
        let _ = crate::sys::signal_set(signo, crate::sys::Disposition::Default);
    }

    #[test]
    fn an_ignore_trap_reaches_the_kernel_and_a_catcher_asks_for_the_default() {
        let _held = Dispositions::held(&[2]);

        reset(2);
        assert_eq!(run_capturing("trap '' INT").0, 0);
        assert!(
            ignored(2),
            "`trap '' INT` did not reach the kernel, so no child would inherit it"
        );

        // A catcher needs a handler this shell has no trampoline for, so the
        // disposition it asks for is the honest one: the shell dies on the
        // signal rather than looking like an action is waiting to run.
        reset(2);
        assert_eq!(run_capturing("trap 'echo x' INT").0, 0);
        assert!(defaulted(2));

        // Within ONE shell the ignore can be taken back, because that shell
        // already knows the process started with INT defaulted.
        reset(2);
        assert_eq!(run_capturing("trap '' INT\ntrap - INT\ntrap").1, "");
        assert!(defaulted(2));

        // KILL and STOP are recorded and reported like any other condition, and
        // no syscall is made for either -- dash is silent about both too. The
        // kernel side is asserted rather than just the table, since a table
        // entry looks the same whether or not anything was issued.
        assert_eq!(
            run_capturing("trap '' KILL\ntrap\necho $?").1,
            "trap -- '' KILL\n0\n"
        );
        assert!(crate::sys::signal_get(9).is_err(), "SIGKILL is not readable or writable");
    }

    /// SIGCHLD is recorded like any other condition and NEVER handed to the
    /// kernel: `SIG_IGN` there asks for children to be auto-reaped, which takes
    /// every command's exit status away from the shell. Asserted against the
    /// kernel rather than against the output, because the table looks identical
    /// either way -- and this was verified red, with each external reporting
    /// "No child processes" instead of what it exited with.
    ///
    /// The criterion, stated so the next signal is checked against it rather
    /// than against this list: a signal belongs here when `SIG_IGN` asks the
    /// kernel for something OTHER than "discard it". SIGCHLD is the only one.
    /// URG/WINCH/CONT default to being discarded anyway, so ignoring them asks
    /// for what already happens; KILL/STOP are refused before the call.
    #[test]
    fn sigchld_is_recorded_but_never_handed_to_the_kernel() {
        // The number is pinned in two places -- the exclusion and the name table
        // `trap` decodes with -- and they have to agree, or the exclusion guards
        // a signal nobody can name.
        assert_eq!(super::decode_signal("CHLD"), Some(super::SIGCHLD));
        let _held = Dispositions::held(&[super::SIGCHLD]);
        assert_eq!(run_capturing("trap '' CHLD\ntrap").1, "trap -- '' CHLD\n");
        assert!(defaulted(super::SIGCHLD), "`trap '' CHLD` would auto-reap every child");
        assert_eq!(run_capturing("trap - CHLD\necho $?").1, "0\n");
        assert!(defaulted(super::SIGCHLD));
    }

    /// POSIX: a signal ignored on entry cannot be trapped or reset. That is what
    /// makes `nohup` stick, and it is asked of the kernel ONCE per signal —
    /// after the first change the process can no longer be asked what it started
    /// with. The trap is still RECORDED, as dash records it.
    #[test]
    fn a_signal_ignored_on_entry_is_not_the_shells_to_reset() {
        let _held = Dispositions::held(&[15]);
        // Standing in for the parent that ignored it before handing over.
        let prev = crate::sys::signal_set(15, crate::sys::Disposition::Ignore);
        assert_eq!(prev, Ok(Some(crate::sys::Disposition::Default)));

        assert_eq!(run_capturing("trap - TERM\ntrap").1, "");
        assert!(ignored(15), "`trap -` un-ignored it");

        assert_eq!(
            run_capturing("trap 'echo x' TERM\ntrap").1,
            "trap -- 'echo x' TERM\n"
        );
        assert!(ignored(15), "a catcher un-ignored it");
    }

    /// A subshell is a separate PROCESS in every other shell, so its dispositions
    /// cannot reach the parent. td-sh's are in-process clones, so the restore a
    /// fork gives for free is explicit — and invisible in the subshell's output,
    /// which is why it is asserted against the kernel.
    #[test]
    fn a_subshell_hands_back_the_dispositions_it_changed() {
        let _held = Dispositions::held(&[2]);

        for src in [
            "( trap '' INT )",
            "echo $(trap '' INT)",
            ": | trap '' INT",
            "( ( trap '' INT ) )",
        ] {
            reset(2);
            run_capturing(src);
            assert!(defaulted(2), "{src} leaked an ignore");
        }

        // ... and the other direction: a subshell that CLEARS an inherited
        // ignore must hand the ignore back, not the default it left behind.
        reset(2);
        run_capturing("trap '' INT\n( trap - INT )");
        assert!(ignored(2));

        // The emulated `exec` clears the trap TABLE the way a real `execve`
        // drops the image, but the undo is not a table entry -- it is the
        // parent's disposition, and it still has to come back.
        reset(2);
        run_capturing("( trap '' INT; exec no_such_cmd_xyz )");
        assert!(defaulted(2));
    }

    #[test]
    fn a_subshell_starts_with_no_inherited_traps() {
        let _held = Dispositions::held(&[2]);
        // POSIX 2.12: the parent's EXIT trap must not fire when a subshell, command
        // substitution or pipeline stage ends -- only once, for the shell itself.
        assert_eq!(
            run_capturing("trap 'echo EXIT TRAP' EXIT\necho $(echo sub)\n( echo shell )").1,
            "sub\nshell\nEXIT TRAP\n"
        );
        // ... but a trap the subshell sets ITSELF runs when it ends.
        assert_eq!(run_capturing("( trap 'echo inner' EXIT; echo body )\necho after").1,
            "body\ninner\nafter\n");
        // And it does not leak back out.
        assert_eq!(run_capturing("( trap 'echo inner' EXIT )\ntrap").1, "inner\n");
        // A trap set to IGNORE is the exception: POSIX keeps it ignored in the
        // subshell, so it is still reported there.
        assert_eq!(run_capturing("trap '' INT\n( trap )").1, "trap -- '' INT\n");
        assert_eq!(run_capturing("trap 'echo x' INT\n( trap )\necho end").1, "end\n");
        // A FAILED `exec` never replaced anything, so the trap still runs. (The
        // succeeding case -- where the emulated `exec` must drop the trap the way a
        // real `execve` drops the image -- needs an external, which this harness's
        // cleared environment has none of.)
        let (status, out, _) = run_capturing("( trap 'echo T' EXIT; exec no_such_cmd_xyz )");
        assert_eq!((status, out.as_str()), (127, "T\n"));
    }

    #[test]
    fn alias_reaches_the_positions_the_grammar_reads_by_name() {
        // A function body is a command position of its own (dash sets its checks
        // before parsing one), so an alias may supply the whole compound.
        assert_eq!(run_capturing("alias B='{ echo yes; }'\nf()\nB\nf").1, "yes\n");
        // `case … in` takes the check the same way `for … in` does.
        assert_eq!(run_capturing("alias I=in\ncase x I x) echo hit;; esac").1, "hit\n");
        // The token after a COMPOUND command is where dash looks for the
        // redirections that may follow it, and it looks with keywords and aliases
        // both on -- so an alias may supply that redirection, or a separator.
        assert_eq!(
            run_capturing("alias R='>/dev/null'\n{ echo hidden; } R\necho after").1,
            "after\n"
        );
        assert_eq!(run_capturing("alias foo='; echo X'\n(echo a) foo").1, "a\nX\n");
    }

    #[test]
    fn exec_without_a_command_keeps_its_redirections() {
        // The whole point of `exec` with no command word: the redirections are
        // NOT unwound when it returns.
        assert_eq!(run_capturing("exec 3>&1\necho hi 1>&3").1, "hi\n");
        assert_eq!(
            run_capturing("exec 3>&1\nexec 4>&1\necho three 1>&3\necho four 1>&4").1,
            "three\nfour\n"
        );
        // Bare `exec` is a no-op that succeeds.
        assert_eq!(run_capturing("exec; echo status=$?").1, "status=0\n");
        // "Bare" is about COMMAND WORDS, not argv length: each of these has
        // options and no command, so the descriptor has to survive.
        for opts in ["--", "-a NAME", "-aNAME", "-a NAME --"] {
            let (st, out, err) = run_capturing(&format!("exec {opts} 3>&1\necho hi 1>&3"));
            assert_eq!((st, out.as_str(), err.as_str()), (0, "hi\n", ""), "exec {opts}");
        }
    }

    #[test]
    fn exec_of_an_unknown_command_ends_the_shell() {
        // POSIX: a failed `exec` is fatal to a non-interactive shell.
        let (status, out, err) = run_capturing("exec no_such_cmd_xyz\necho NOTREACHED");
        assert_eq!((status, out.as_str()), (127, ""));
        assert!(err.contains("not found"), "err: {err:?}");
    }

    /// What `nextopt` treats as an option and what it hands through. The `-a`
    /// VALUE landing on argv[0] needs a real spawn and is pinned in
    /// `tests/conformance.rs`; this is the parsing either side of it.
    #[test]
    fn exec_takes_dash_a_and_a_double_dash_and_refuses_the_rest() {
        // `--` is consumed; a lone `-` and a `+` word are not options at all.
        // Reaching `not found` is the proof each got PAST the scan.
        for src in ["exec -- no_such_cmd_xyz", "exec -", "exec +Z"] {
            let (status, _o, err) = run_capturing(&format!("{src}\necho NOTREACHED"));
            assert_eq!(status, 127, "{src}");
            assert!(err.contains("not found"), "{src}: {err:?}");
        }
        // `-a` eats the next word RAW, so `--` is a NAME here and not a
        // terminator -- and with no command left, `exec` does nothing at 0.
        let (status, out, err) = run_capturing("exec -a -- ; echo after=$?");
        assert_eq!((status, out.as_str(), err.as_str()), (0, "after=0\n", ""));
        // Attached and separate spellings both consume, leaving no command.
        for src in ["exec -a renamed", "exec -arenamed", "exec -a x -a y"] {
            let (status, out, err) = run_capturing(&format!("{src}; echo after=$?"));
            assert_eq!((status, out.as_str(), err.as_str()), (0, "after=0\n", ""), "{src}");
        }
    }

    #[test]
    fn read_reports_an_io_error_without_killing_the_shell() {
        // Reading a directory is EISDIR: the builtin fails, the shell carries on.
        // dash falls into its ordinary assignment path afterwards, so the
        // destination ends up empty rather than keeping its old value.
        let (status, out, _) = run_capturing("x=old; read x < .; echo \"[$x] $?\"; echo alive");
        assert_eq!((status, out.as_str()), (0, "[] 1\nalive\n"));
    }

    #[test]
    fn exec_failure_is_confined_to_a_subshell() {
        // `exec` must never take the rest of the script with it from an
        // in-process clone: the subshell ends, the parent carries on.
        let (status, out, _) =
            run_capturing("( exec no_such_cmd_xyz ) 2>/dev/null; echo after=$?");
        assert_eq!((status, out.as_str()), (0, "after=127\n"));
        let (_, out, _) =
            run_capturing("for i in 1 2; do ( exec no_such_cmd_xyz ) 2>/dev/null; done; echo done");
        assert_eq!(out, "done\n");
    }

    #[test]
    fn unset_takes_a_name_up_to_an_equals() {
        // dash's setvar takes the name up to `=`, so this unsets `b` rather than
        // failing -- but a name it cannot parse at all is still fatal.
        assert_eq!(
            run_capturing("a=1 b=2\nunset a b=c; echo after rc=$?").1,
            "after rc=0\n"
        );
        assert_eq!(run_capturing("unset %; echo NOTREACHED").0, 2);
        assert_eq!(run_capturing("unset 'a[1]'; echo NOTREACHED").0, 2);
        // The last of `-f`/`-v` wins, as in dash's option loop.
        assert_eq!(
            run_capturing("x=var\nx() { echo func; }\nunset -f -v x\necho [$x]").1,
            "[]\n"
        );
    }

    #[test]
    fn alias_name_may_start_with_a_multibyte_character() {
        // dash looks for the `=` from the second BYTE; slicing there by char
        // boundary would make this a lookup instead of a definition.
        assert_eq!(run_capturing("alias é=echo\né works").1, "works\n");
    }
}
