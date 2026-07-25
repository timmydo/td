//! Shell builtins.
//!
//! Each returns via `R<()>`, leaving its exit status in `Shell::status`; the
//! control-flow builtins (`exit`, `return`, `break`, `continue`) unwind through
//! the `Sig` error channel instead. Output goes through the shell descriptor
//! table (`process::write_fd`), never straight to `println!`, so a builtin obeys
//! the redirections in force.

use crate::exec::{Shell, Sig, R};
use crate::parser::parse;
use crate::process::{read_byte, write_fd};
use crate::{ast, exec};

#[derive(Clone, Copy, Debug)]
pub enum Builtin {
    Colon,
    True,
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
    Test,
    Eval,
    Cd,
    Pwd,
    Dot,
    Command,
    Wait,
}

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
        "read" => Builtin::Read,
        "test" | "[" => Builtin::Test,
        "eval" => Builtin::Eval,
        "cd" => Builtin::Cd,
        "pwd" => Builtin::Pwd,
        "." => Builtin::Dot,
        "command" => Builtin::Command,
        "wait" => Builtin::Wait,
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
            | Builtin::Continue
            | Builtin::Eval
            | Builtin::Exit
            | Builtin::Export
            | Builtin::Readonly
            | Builtin::Return
            | Builtin::Set
            | Builtin::Shift
            | Builtin::Unset
    )
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
        Builtin::Read => read(sh, argv),
        Builtin::Test => test(sh, argv),
        Builtin::Eval => eval(sh, argv),
        Builtin::Cd => cd(sh, argv),
        Builtin::Pwd => pwd(sh),
        Builtin::Dot => dot(sh, argv),
        Builtin::Command => command(sh, argv),
        Builtin::Wait => ok(sh),
    }
}

fn ok(sh: &mut Shell) -> R<()> {
    sh.set_status(0);
    Ok(())
}

fn status(sh: &mut Shell, code: i32) -> R<()> {
    sh.set_status(code);
    Ok(())
}

/// Write to stdout, setting `$?`: 0 on success, 1 on a broken pipe / I/O error (so a
/// write to a closed descriptor, `echo hi >&-`, fails visibly instead of silently
/// succeeding). Callers must NOT overwrite the status afterward.
fn out(sh: &mut Shell, bytes: &[u8]) -> R<()> {
    match write_fd(sh, 1, bytes) {
        Ok(()) => sh.set_status(0),
        Err(_) => sh.set_status(1),
    }
    Ok(())
}

fn err_line(sh: &mut Shell, msg: &str) {
    let _ = exec::write_stderr(sh, msg);
}

fn echo(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut i = 1usize;
    let mut newline = true;
    // Only `-n` is honoured; dash's echo does not take `-e`/`-E`.
    while let Some(arg) = argv.get(i) {
        if arg == "-n" {
            newline = false;
            i += 1;
        } else {
            break;
        }
    }
    let mut line = String::new();
    let mut first = true;
    while let Some(arg) = argv.get(i) {
        if !first {
            line.push(' ');
        }
        line.push_str(arg);
        first = false;
        i += 1;
    }
    if newline {
        line.push('\n');
    }
    // `out` sets `$?` (0, or 1 on write error); do not overwrite it.
    out(sh, line.as_bytes())
}

/// A deliberately small `printf`: `%s`, `%d`, `%%`, `\n`, `\t`, `\\` in the
/// format, cycling the format over the remaining arguments (POSIX behaviour).
fn printf(sh: &mut Shell, argv: &[String]) -> R<()> {
    let Some(format) = argv.get(1) else {
        err_line(sh, "printf: usage: printf format [arguments]");
        return status(sh, 2);
    };
    let args: Vec<&String> = argv.iter().skip(2).collect();
    let mut output = String::new();
    let mut errors: Vec<String> = Vec::new();
    let mut ai = 0usize;
    loop {
        let consumed_before = ai;
        format_once(format, &args, &mut ai, &mut output, &mut errors);
        // Re-run the format while it still consumes arguments (POSIX cycling).
        if ai >= args.len() || ai == consumed_before {
            break;
        }
    }
    for bad in &errors {
        err_line(sh, &format!("printf: {bad}: expected a numeric value"));
    }
    match write_fd(sh, 1, output.as_bytes()) {
        Ok(()) => sh.set_status(if errors.is_empty() { 0 } else { 1 }),
        Err(_) => sh.set_status(1),
    }
    Ok(())
}

fn format_once(
    format: &str,
    args: &[&String],
    ai: &mut usize,
    output: &mut String,
    errors: &mut Vec<String>,
) {
    let chars: Vec<char> = format.chars().collect();
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        match c {
            '\\' => {
                i += 1;
                match chars.get(i) {
                    Some('n') => output.push('\n'),
                    Some('t') => output.push('\t'),
                    Some('r') => output.push('\r'),
                    Some('\\') => output.push('\\'),
                    Some(&other) => {
                        output.push('\\');
                        output.push(other);
                    }
                    None => output.push('\\'),
                }
                i += 1;
            }
            '%' => {
                i += 1;
                match chars.get(i) {
                    Some('%') => output.push('%'),
                    Some('s') => output.push_str(next_arg(args, ai)),
                    Some('d') | Some('i') => {
                        let raw = next_arg(args, ai);
                        let t = raw.trim();
                        // An absent/empty argument is 0 with no error (POSIX); a
                        // present-but-non-numeric one prints 0 and fails.
                        if t.is_empty() {
                            output.push('0');
                        } else {
                            match t.parse::<i64>() {
                                Ok(n) => output.push_str(&n.to_string()),
                                Err(_) => {
                                    output.push('0');
                                    errors.push(raw.to_string());
                                }
                            }
                        }
                    }
                    Some(&other) => {
                        output.push('%');
                        output.push(other);
                    }
                    None => output.push('%'),
                }
                i += 1;
            }
            other => {
                output.push(other);
                i += 1;
            }
        }
    }
}

fn next_arg<'a>(args: &'a [&'a String], ai: &mut usize) -> &'a str {
    let s = args.get(*ai).map(|s| s.as_str()).unwrap_or("");
    *ai += 1;
    s
}

fn exit(sh: &mut Shell, argv: &[String]) -> R<()> {
    let code = match argv.get(1) {
        Some(s) => parse_status(sh, s)?,
        None => sh.status,
    };
    Err(Sig::Exit(code & 0xff))
}

fn ret(sh: &mut Shell, argv: &[String]) -> R<()> {
    let code = match argv.get(1) {
        Some(s) => parse_status(sh, s)?,
        None => sh.status,
    };
    Err(Sig::Return(code & 0xff))
}

fn parse_status(sh: &mut Shell, s: &str) -> R<i32> {
    match s.trim().parse::<i32>() {
        Ok(n) => Ok(n),
        Err(_) => {
            err_line(sh, &format!("td-sh: {s}: numeric argument required"));
            Err(Sig::Exit(2))
        }
    }
}

fn loop_ctl(sh: &mut Shell, argv: &[String], is_break: bool) -> R<()> {
    let n = match argv.get(1) {
        Some(s) => match s.parse::<u32>() {
            Ok(0) | Err(_) => {
                err_line(sh, &format!("td-sh: {}: bad loop count", argv.get(1).map(String::as_str).unwrap_or("")));
                return status(sh, 2);
            }
            Ok(n) => n,
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
        Some(s) => match s.parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                err_line(sh, &format!("shift: {s}: numeric argument required"));
                return status(sh, 2);
            }
        },
        None => 1,
    };
    if n > sh.params.len() {
        err_line(sh, "shift: shift count out of range");
        return status(sh, 1);
    }
    sh.params.drain(0..n);
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
        let (sign, rest) = match arg.strip_prefix('-') {
            Some(r) => (true, r),
            None => match arg.strip_prefix('+') {
                Some(r) => (false, r),
                None => break,
            },
        };
        if rest == "o" {
            let name = argv.get(i + 1).cloned().unwrap_or_default();
            apply_named_option(sh, &name, sign);
            i += 2;
            continue;
        }
        for c in rest.chars() {
            apply_option_letter(sh, c, sign);
        }
        i += 1;
    }
    // `--` present: the operands after it REPLACE the positional params, and an
    // empty operand list clears them (`set --`). Otherwise operands present without
    // `--` replace them, while a pure option change leaves them untouched.
    if saw_ddash || argv.get(i).is_some() {
        sh.params = argv.iter().skip(i).cloned().collect();
    }
    ok(sh)
}

fn apply_option_letter(sh: &mut Shell, c: char, on: bool) {
    match c {
        'e' => sh.opts.errexit = on,
        'u' => sh.opts.nounset = on,
        'x' => sh.opts.xtrace = on,
        'f' => sh.opts.noglob = on,
        'v' => sh.opts.verbose = on,
        'C' => sh.opts.noclobber = on,
        _ => {}
    }
}

fn apply_named_option(sh: &mut Shell, name: &str, on: bool) {
    match name {
        "errexit" => sh.opts.errexit = on,
        "nounset" => sh.opts.nounset = on,
        "xtrace" => sh.opts.xtrace = on,
        "noglob" => sh.opts.noglob = on,
        "verbose" => sh.opts.verbose = on,
        "noclobber" => sh.opts.noclobber = on,
        _ => {}
    }
}

fn unset(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut i = 1usize;
    // `-v` (variable) / `-f` (function); default is variable-then-function.
    let mut funcs_only = false;
    let mut vars_only = false;
    while let Some(arg) = argv.get(i) {
        match arg.as_str() {
            "-f" => funcs_only = true,
            "-v" => vars_only = true,
            _ => break,
        }
        i += 1;
    }
    let mut status_code = 0;
    while let Some(name) = argv.get(i) {
        if funcs_only {
            sh.funcs.remove(name);
        } else {
            if !vars_only && !sh.vars.contains_key(name) {
                sh.funcs.remove(name);
            }
            if !sh.unset_var(name) {
                err_line(sh, &format!("unset: {name}: cannot unset"));
                status_code = 1;
            }
        }
        i += 1;
    }
    status(sh, status_code)
}

fn export(sh: &mut Shell, argv: &[String], readonly: bool) -> R<()> {
    let mut any = false;
    for arg in argv.iter().skip(1) {
        if arg == "-p" {
            continue;
        }
        any = true;
        match arg.split_once('=') {
            Some((name, value)) => {
                if !ast::is_name(name) {
                    err_line(sh, &format!("export: {name}: not a valid identifier"));
                    return status(sh, 1);
                }
                sh.set_var(name, value)?;
                if readonly {
                    sh.set_readonly(name);
                } else {
                    sh.export(name);
                }
            }
            None => {
                if !ast::is_name(arg) {
                    err_line(sh, &format!("export: {arg}: not a valid identifier"));
                    return status(sh, 1);
                }
                if readonly {
                    sh.set_readonly(arg);
                } else {
                    sh.export(arg);
                }
            }
        }
    }
    let _ = any;
    ok(sh)
}

fn read(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut i = 1usize;
    let mut raw = false;
    while let Some(arg) = argv.get(i) {
        if arg == "-r" {
            raw = true;
            i += 1;
        } else if arg == "--" {
            i += 1;
            break;
        } else if arg.starts_with('-') && arg.len() > 1 {
            i += 1; // ignore unsupported flags rather than mis-treating them as names
        } else {
            break;
        }
    }
    let names: Vec<String> = argv.iter().skip(i).cloned().collect();

    let (line, terminated) = match read_logical_line(sh, raw)? {
        Some(l) => l,
        None => {
            // EOF with nothing read at all: clear the named variables and fail.
            for name in &names {
                sh.set_var(name, "")?;
            }
            return status(sh, 1);
        }
    };

    if names.is_empty() {
        // `read` with no variable stores the line in REPLY.
        sh.set_var("REPLY", &line)?;
    } else {
        let ifs = sh.get_var("IFS").unwrap_or_else(|| " \t\n".to_string());
        let fields = split_read(&line, &ifs, names.len());
        for (idx, name) in names.iter().enumerate() {
            let value = fields.get(idx).cloned().unwrap_or_default();
            sh.set_var(name, &value)?;
        }
    }
    // A line ended by EOF rather than a newline still assigns, but reports failure.
    if terminated {
        ok(sh)
    } else {
        status(sh, 1)
    }
}

/// Read one input line, honouring backslash-newline continuation unless `-r`.
/// Returns `None` only at end-of-input with nothing read (so a blank line reads as
/// an empty, successful line). The bool is whether a newline terminated the line: a
/// partial line at EOF returns `false`, which makes `read` report failure.
fn read_logical_line(sh: &mut Shell, raw: bool) -> R<Option<(String, bool)>> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut read_anything = false;
    loop {
        let seg_start = bytes.len();
        let mut got_newline = false;
        loop {
            match read_byte(sh, 0) {
                Ok(Some(b'\n')) => {
                    read_anything = true;
                    got_newline = true;
                    break;
                }
                Ok(Some(byte)) => {
                    read_anything = true;
                    bytes.push(byte);
                }
                Ok(None) => break,
                Err(e) => {
                    err_line(sh, &format!("read: {e}"));
                    return Err(Sig::Exit(2));
                }
            }
        }
        // A trailing backslash on this physical line (unless `-r`) splices the next.
        let seg_ends_bs = bytes.get(seg_start..).is_some_and(|s| s.last() == Some(&b'\\'));
        if !raw && seg_ends_bs {
            bytes.pop();
            if got_newline {
                continue;
            }
        }
        if !read_anything {
            return Ok(None);
        }
        // Bytes are decoded as UTF-8 (lossy) so multibyte input is not mangled.
        return Ok(Some((String::from_utf8_lossy(&bytes).into_owned(), got_newline)));
    }
}

/// Split a read line into at most `n` fields on IFS, with the last field taking
/// the unsplit remainder (POSIX `read` semantics).
fn split_read(line: &str, ifs: &str, n: usize) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let ws: Vec<char> = ifs.chars().filter(|c| *c == ' ' || *c == '\t' || *c == '\n').collect();
    let all: Vec<char> = ifs.chars().collect();
    let is_ws = |c: char| ws.contains(&c);
    let is_sep = |c: char| all.contains(&c);

    let chars: Vec<char> = line.chars().collect();
    let mut fields: Vec<String> = Vec::new();
    let mut i = 0usize;
    // Trim leading IFS whitespace.
    while chars.get(i).is_some_and(|&c| is_ws(c)) {
        i += 1;
    }
    while i < chars.len() {
        if fields.len() == n - 1 {
            // Last field: the remainder, with trailing IFS whitespace removed.
            let mut end = chars.len();
            while end > i && chars.get(end - 1).is_some_and(|&c| is_ws(c)) {
                end -= 1;
            }
            let rest: String = chars.get(i..end).unwrap_or(&[]).iter().collect();
            fields.push(rest);
            return fields;
        }
        let mut field = String::new();
        while let Some(&c) = chars.get(i) {
            if is_sep(c) {
                break;
            }
            field.push(c);
            i += 1;
        }
        fields.push(field);
        // Consume the separator: whitespace run, or a single non-ws separator
        // plus surrounding whitespace.
        while chars.get(i).is_some_and(|&c| is_ws(c)) {
            i += 1;
        }
        if chars.get(i).is_some_and(|&c| is_sep(c) && !is_ws(c)) {
            i += 1;
            while chars.get(i).is_some_and(|&c| is_ws(c)) {
                i += 1;
            }
        }
    }
    fields
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
    let list = match parse(&joined) {
        Ok(l) => l,
        Err(e) => {
            err_line(sh, &format!("td-sh: eval: {e}"));
            return status(sh, 2);
        }
    };
    exec::run_list(sh, &list)
}

fn cd(sh: &mut Shell, argv: &[String]) -> R<()> {
    let target = match argv.get(1) {
        Some(s) if s == "-" => match sh.get_var("OLDPWD") {
            Some(p) => p,
            None => {
                err_line(sh, "cd: OLDPWD not set");
                return status(sh, 1);
            }
        },
        Some(s) => s.clone(),
        None => match sh.get_var("HOME") {
            Some(h) => h,
            None => {
                err_line(sh, "cd: HOME not set");
                return status(sh, 1);
            }
        },
    };
    let new = sh.resolve(&target);
    let canonical = match new.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            err_line(sh, &format!("cd: {target}: {e}"));
            return status(sh, 1);
        }
    };
    if !canonical.is_dir() {
        err_line(sh, &format!("cd: {target}: not a directory"));
        return status(sh, 1);
    }
    let old = sh.cwd.to_string_lossy().into_owned();
    sh.cwd = canonical.clone();
    sh.set_var("OLDPWD", &old)?;
    sh.set_var("PWD", &canonical.to_string_lossy())?;
    ok(sh)
}

fn pwd(sh: &mut Shell) -> R<()> {
    let line = format!("{}\n", sh.cwd.to_string_lossy());
    // `out` sets `$?` (0, or 1 on write error); do not overwrite it.
    out(sh, line.as_bytes())
}

fn dot(sh: &mut Shell, argv: &[String]) -> R<()> {
    let Some(name) = argv.get(1) else {
        err_line(sh, ".: filename argument required");
        return status(sh, 2);
    };
    let path = sh.resolve(name);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            err_line(sh, &format!(".: {name}: {e}"));
            return status(sh, 1);
        }
    };
    let list = match parse(&text) {
        Ok(l) => l,
        Err(e) => {
            err_line(sh, &format!("td-sh: {name}: {e}"));
            return status(sh, 2);
        }
    };
    // A `return` in a sourced file returns from the `.`, not the process.
    match exec::run_list(sh, &list) {
        Err(Sig::Return(code)) => status(sh, code),
        other => other,
    }
}

/// `command name args`: run `name` as an external/builtin, bypassing functions.
/// The `-v`/`-V` query forms are supported enough for scripts that probe.
fn command(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut i = 1usize;
    let mut query = false;
    while let Some(arg) = argv.get(i) {
        match arg.as_str() {
            "-v" | "-V" => query = true,
            "-p" => {}
            "--" => {
                i += 1;
                break;
            }
            s if s.starts_with('-') && s.len() > 1 => {}
            _ => break,
        }
        i += 1;
    }
    let rest: Vec<String> = argv.iter().skip(i).cloned().collect();
    let Some(name) = rest.first() else {
        return ok(sh);
    };
    if query {
        // A builtin or function reports its bare name; an external reports its
        // resolved path; an unresolved name fails silently.
        // `out` sets `$?` (0, or 1 on write error); do not overwrite it.
        if crate::process::is_builtin(name) || sh.funcs.contains_key(name) {
            return out(sh, format!("{name}\n").as_bytes());
        }
        if let Some(path) = crate::process::resolve_program(sh, name) {
            return out(sh, format!("{}\n", path.display()).as_bytes());
        }
        return status(sh, 1);
    }
    if let Some(bi) = lookup(name) {
        return run(sh, bi, &rest);
    }
    crate::process::exec_external(sh, &rest, &[])
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
        4 if s(args, 0) == "!" => Ok(!three_arg(sh, args.get(1..).unwrap_or(&[]))?),
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
        return binary_op(s(args, 0), op, s(args, 2));
    }
    if s(args, 0) == "!" {
        return Ok(!two_arg(sh, args.get(1..).unwrap_or(&[]))?);
    }
    if s(args, 0) == "(" && s(args, 2) == ")" {
        return Ok(!s(args, 1).is_empty());
    }
    Err(format!("binary operator expected: `{op}`"))
}

/// Fetch an argument, defaulting to the empty string past the end.
fn s(args: &[String], i: usize) -> &str {
    args.get(i).map(String::as_str).unwrap_or("")
}

fn is_binary(op: &str) -> bool {
    matches!(
        op,
        "=" | "==" | "!=" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" | "<" | ">"
    )
}

fn binary_op(a: &str, op: &str, b: &str) -> Result<bool, String> {
    match op {
        "=" | "==" => Ok(a == b),
        "!=" => Ok(a != b),
        "<" => Ok(a < b),
        ">" => Ok(a > b),
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

fn unary_op(sh: &Shell, op: &str, arg: &str) -> Result<bool, String> {
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
        "t" => false, // no controlling terminal in the harness
        _ => return Err(format!("unknown unary operator `-{op}`")),
    })
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
            if u.len() == 1 && "znefdrwxshLt".contains(u) {
                let arg = self.args.get(self.pos + 1).map(String::as_str).unwrap_or("");
                self.pos += 2;
                return unary_op(self.sh, u, arg);
            }
        }
        // Binary op: `A OP B`.
        if let Some(op) = self.args.get(self.pos + 1).map(String::as_str) {
            if is_binary(op) {
                let b = self.args.get(self.pos + 2).map(String::as_str).unwrap_or("");
                let v = binary_op(&a, op, b)?;
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
    use crate::process::run_capturing;

    #[test]
    fn echo_joins_with_spaces_and_newline() {
        let (_, out, _) = run_capturing("echo a b c");
        assert_eq!(out, "a b c\n");
        let (_, out, _) = run_capturing("echo -n hi");
        assert_eq!(out, "hi");
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
            "1\n"
        );
    }
}
