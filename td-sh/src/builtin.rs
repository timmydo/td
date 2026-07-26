//! Shell builtins.
//!
//! Each returns via `R<()>`, leaving its exit status in `Shell::status`; the
//! control-flow builtins (`exit`, `return`, `break`, `continue`) unwind through
//! the `Sig` error channel instead. Output goes through the shell descriptor
//! table (`process::write_fd`), never straight to `println!`, so a builtin obeys
//! the redirections in force.

use crate::exec::{Local, Shell, Sig, R};
use crate::process::{self, read_byte, write_fd};
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
    Getopts,
    Test,
    Eval,
    Cd,
    Pwd,
    Dot,
    Command,
    Wait,
    Alias,
    Unalias,
    Exec,
    Trap,
    Local,
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
        "getopts" => Builtin::Getopts,
        "test" | "[" => Builtin::Test,
        "eval" => Builtin::Eval,
        "cd" => Builtin::Cd,
        "pwd" => Builtin::Pwd,
        "." => Builtin::Dot,
        "command" => Builtin::Command,
        "wait" => Builtin::Wait,
        "alias" => Builtin::Alias,
        "unalias" => Builtin::Unalias,
        "exec" => Builtin::Exec,
        "trap" => Builtin::Trap,
        "local" => Builtin::Local,
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
            | Builtin::Trap
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
        Builtin::Local => local(sh, argv),
        Builtin::Read => read(sh, argv),
        Builtin::Getopts => getopts(sh, argv),
        Builtin::Test => test(sh, argv),
        Builtin::Eval => eval(sh, argv),
        Builtin::Cd => cd(sh, argv),
        Builtin::Pwd => pwd(sh),
        Builtin::Dot => dot(sh, argv),
        Builtin::Command => command(sh, argv),
        Builtin::Wait => ok(sh),
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

/// POSIX `printf`: format directives with flags/width/precision (including `*`
/// width/precision from arguments), the integer conversions `d i o u x X` (C
/// base-0 parsing, the `'c` char-code form, ash's i64/u64 range rules), the
/// float conversions `f F e E g G` (C `strtod` operands, C-exact output), plus
/// `c`, `s`, `b` (its own escape set with `\c` early stop), and format-string
/// backslash escapes. The format cycles over the remaining arguments (POSIX).
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
        Err(_) => sh.set_status(1),
    }
    Ok(())
}

// printf run state threaded through the format walk.
struct Pf {
    ai: usize,           // next argument index
    error: bool,         // a conversion/parse error occurred => exit status 1
    stop: bool,          // %b `\c`: stop ALL further output (and format cycling)
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
            '\\' => i = format_escape(&chars, i, out),
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
    let hit_c = process_b_escapes(raw, &mut body);
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

fn read_octal(chars: &[char], start: usize, max: usize) -> (u8, usize) {
    let mut val: u32 = 0;
    let mut j = start;
    let mut n = 0usize;
    while n < max {
        match chars.get(j) {
            Some(&c) if ('0'..='7').contains(&c) => {
                val = val * 8 + (c as u32 - '0' as u32);
                j += 1;
                n += 1;
            }
            _ => break,
        }
    }
    ((val & 0xff) as u8, j)
}

// Format-string backslash escapes (dash/ash): the C control escapes, `\\`, and
// `\ooo` octal (1-3 digits). `\x`, `\u`, `\U` and any other escape are left
// literal (backslash kept), matching dash.
fn format_escape(chars: &[char], at: usize, out: &mut Vec<u8>) -> usize {
    let j = at + 1;
    match chars.get(j) {
        Some('a') => out.push(0x07),
        Some('b') => out.push(0x08),
        Some('f') => out.push(0x0c),
        Some('n') => out.push(b'\n'),
        Some('r') => out.push(b'\r'),
        Some('t') => out.push(b'\t'),
        Some('v') => out.push(0x0b),
        Some('\\') => out.push(b'\\'),
        Some(&c) if ('0'..='7').contains(&c) => {
            let (byte, nj) = read_octal(chars, j, 3);
            out.push(byte);
            return nj;
        }
        Some(&other) => {
            out.push(b'\\');
            push_char(out, other);
        }
        None => {
            out.push(b'\\');
            return j;
        }
    }
    j + 1
}

// %b escapes (dash/ash): the C control escapes, `\\`, `\0ooo`/`\ooo` octal, and
// `\c` which stops ALL output. `\x`/`\u` are left literal (matching dash/ash).
// Returns true if `\c` was seen.
fn process_b_escapes(raw: &str, out: &mut Vec<u8>) -> bool {
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        if c != '\\' {
            push_char(out, c);
            i += 1;
            continue;
        }
        i += 1;
        match chars.get(i) {
            Some('a') => out.push(0x07),
            Some('b') => out.push(0x08),
            Some('c') => return true,
            Some('f') => out.push(0x0c),
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('v') => out.push(0x0b),
            Some('\\') => out.push(b'\\'),
            Some('0') => {
                // `\0ooo`: a leading-0 marker, then up to 3 octal digits.
                let (byte, ni) = read_octal(&chars, i + 1, 3);
                out.push(byte);
                i = ni;
                continue;
            }
            Some(&d) if ('1'..='7').contains(&d) => {
                let (byte, ni) = read_octal(&chars, i, 3);
                out.push(byte);
                i = ni;
                continue;
            }
            Some(&other) => {
                out.push(b'\\');
                push_char(out, other);
            }
            None => {
                out.push(b'\\');
                return false;
            }
        }
        i += 1;
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
        Some(s) => parse_status(sh, s)?,
        // POSIX: inside a trap action, "the last command" is the one that ran
        // before the trap, so a bare `exit` there reports the status the shell was
        // already exiting with -- not whatever the action's own last command left.
        None => sh.trap_status.unwrap_or(sh.status),
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
        // New positional parameters restart the `getopts` scan, as in dash --
        // otherwise a second option loop would resume at the old cursor. Only the
        // hidden cursor moves: dash leaves the OPTIND *variable* untouched until
        // the next `getopts` writes it (and touching it would break `readonly`).
        sh.getopts_optind = 1;
        sh.getopts_off = -1;
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

/// A usage error in a SPECIAL builtin, and the ONE route for them: POSIX 2.8.1
/// ends a non-interactive shell on one, while dash's sh_error returns an
/// interactive one to its prompt. Reaching for `Shell::fatal` here instead would
/// silently drop that second half.
fn special_usage_error(sh: &mut Shell, msg: &str) -> R<()> {
    err_line(sh, msg);
    sh.set_status(2);
    if sh.interactive {
        return Ok(());
    }
    Err(Sig::Exit(2))
}

fn unset(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut i = 1usize;
    // `-v` (variable) / `-f` (function); default is variable-then-function. dash's
    // option loop keeps the LAST one given, so `-f -v` means `-v`.
    let mut mode = None;
    while let Some(arg) = argv.get(i) {
        match arg.as_str() {
            "-f" => mode = Some('f'),
            "-v" => mode = Some('v'),
            _ => break,
        }
        i += 1;
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
        if mode.is_none() && !sh.vars.contains_key(name) {
            sh.funcs.remove(name);
        }
        if !sh.unset_var(name) {
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
    for arg in argv.iter().skip(1) {
        if options && arg == "--" {
            options = false;
            continue;
        }
        // dash's nextopt knows only `-p` here, and reads a cluster letter by
        // letter, so `-px` reports the `x`. An unknown one is fatal: that is what
        // makes bash's `export -n` end a dash script rather than warn.
        if options && arg.len() > 1 && arg.starts_with('-') {
            // A plain loop, not a search combinator: this source is embedded
            // verbatim in the td-sh recipe, whose ladder guard rejects that tool's
            // bare name as a token (see the note in arith.rs).
            let mut bad = None;
            for c in arg.chars().skip(1) {
                if c != 'p' {
                    bad = Some(c);
                    break;
                }
            }
            if let Some(c) = bad {
                return special_usage_error(sh, &format!("td-sh: {cmd}: Illegal option -{c}"));
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
                } else {
                    sh.export(name);
                }
            }
            None => {
                if !ast::is_name(arg) {
                    return special_usage_error(sh, &format!("{cmd}: {arg}: bad variable name"));
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

/// `local [name[=value]... | -]`, dash's. Not POSIX, but dash, busybox ash and
/// bash all have it and scripts are written to it, so a `/bin/sh` without one
/// cannot run them.
///
/// Each name's current binding is saved for the function's return to restore.
/// Only the `=` form ASSIGNS; a bare `local x` declares and leaves whatever the
/// name held, so it starts out unset only when there was no prior binding (which
/// is why `set -u; f() { local x; echo $x; }` is an error, not a blank line).
/// `unset` on a local then leaves it unset for the rest of the call instead of
/// revealing the global, because the outer binding only comes back on unwind.
fn local(sh: &mut Shell, argv: &[String]) -> R<()> {
    if !sh.in_function {
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
        let existed = sh.vars.contains_key(name);
        if !sh
            .locals
            .iter()
            .any(|l| matches!(l, Local::Var(n, _) if n == name))
        {
            sh.locals
                .push(Local::Var(name.to_string(), sh.vars.get(name).cloned()));
        }
        match value {
            // `local x=v` keeps the attributes of the name it shadows, since
            // set_var writes through the existing entry.
            Some(v) => sh.set_var(name, v)?,
            // A valueless `local x` only DECLARES: dash assigns nothing unless the
            // argument carries `=`, so an existing value survives being localised
            // (`local x=1; local x` is still 1) and only a name that did not exist
            // starts out unset.
            None if !existed => {
                sh.vars.remove(name);
            }
            None => {}
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
    // dash treats an OPTIND that is not a positive integer as fatal (the shell
    // exits 2), rather than quietly restarting the scan.
    let mut optind = sh.getopts_optind;
    if optind < 1 {
        let shown = sh.get_var("OPTIND").unwrap_or_default();
        err_line(sh, &format!("getopts: Illegal number: {shown}"));
        return Err(Sig::Exit(2));
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
                sh.set_var("OPTARG", &c.to_string())?;
            } else {
                err_line(sh, &format!("getopts: Illegal option -{c}"));
                sh.unset_var("OPTARG");
            }
            letter = '?';
            off = if at >= word.len() { -1 } else { at as i64 };
        }
        Some(false) => {
            sh.set_var("OPTARG", "")?;
            off = if at >= word.len() { -1 } else { at as i64 };
        }
        Some(true) => {
            off = -1;
            if at < word.len() {
                let tail = String::from_utf8_lossy(word.as_bytes().get(at..).unwrap_or(&[])).into_owned();
                sh.set_var("OPTARG", &tail)?;
            } else if let Some(a) = args.get((optind - 1) as usize).cloned() {
                sh.set_var("OPTARG", &a)?;
                optind += 1;
            } else if silent {
                sh.set_var("OPTARG", &c.to_string())?;
                letter = ':';
            } else {
                err_line(sh, &format!("getopts: No arg for -{c} option"));
                sh.unset_var("OPTARG");
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

// End of options: report `?` through `name` and stop, leaving OPTARG alone.
fn getopts_done(sh: &mut Shell, name: &str, optind: i64) -> R<()> {
    getopts_store(sh, name, optind, "?")?;
    sh.getopts_optind = optind;
    sh.getopts_off = -1;
    status(sh, 1)
}

// Publish OPTIND and the option letter, skipping a name that is not an identifier.
fn getopts_store(sh: &mut Shell, name: &str, optind: i64, letter: &str) -> R<()> {
    sh.set_var("OPTIND", &optind.to_string())?;
    if ast::is_name(name) {
        sh.set_var(name, letter)?;
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

/// POSIX `read`, with dash's option surface: `-r` and `-p prompt`, clustered
/// (`-rp x`) or smooshed (`-pfoo`), `--` ending them. Every other letter is a
/// usage error (status 2, which does NOT end the shell -- `read` is not a
/// special builtin). That includes dash's own `-t`: rejecting a timeout td-sh
/// cannot implement is a deliberate DEVIATION from dash, not its surface.
fn read(sh: &mut Shell, argv: &[String]) -> R<()> {
    let mut i = 1usize;
    let mut raw = false;
    let mut prompt: Option<String> = None;
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
            match c {
                b'r' => raw = true,
                b'p' => {
                    // The rest of the word is the prompt, else the next word is.
                    let rest = arg.get(k..).unwrap_or("");
                    if rest.is_empty() {
                        match argv.get(i) {
                            Some(next) => {
                                prompt = Some(next.clone());
                                i += 1;
                            }
                            None => {
                                err_line(sh, "read: No arg for -p option");
                                return status(sh, 2);
                            }
                        }
                    } else {
                        prompt = Some(rest.to_string());
                    }
                    k = bytes.len();
                }
                _ => {
                    err_line(sh, &format!("read: Illegal option -{}", c as char));
                    return status(sh, 2);
                }
            }
        }
    }
    // dash prompts before validating the operands, only when it is actually
    // reading from a terminal, and writes the prompt bare (no newline).
    if let Some(p) = &prompt {
        if sh.fds.is_terminal(0) {
            let _ = write_fd(sh, 2, p.as_bytes());
        }
    }
    let names: Vec<String> = argv.iter().skip(i).cloned().collect();
    if names.is_empty() {
        // dash requires at least one variable name; there is no bare-`read`
        // fallback to $REPLY (that is a bash/ksh extension).
        err_line(sh, "read: arg count");
        return status(sh, 2);
    }
    for bad in &names {
        if !ast::is_name(bad) {
            err_line(sh, &format!("read: {bad}: bad variable name"));
            return status(sh, 2);
        }
    }

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

    {
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
/// partial line at EOF returns `false`, which makes `read` report failure. `None`
/// is end of input with nothing read -- or a reported I/O error, which dash also
/// leaves the destinations empty for.
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
                    // A read error fails the BUILTIN; it does not kill the shell
                    // (`read x < some-directory` is EISDIR). dash breaks out of
                    // its read loop and falls into the ordinary assignment path,
                    // so the destinations end up empty -- which is what returning
                    // end-of-input here does.
                    err_line(sh, &format!("read: {e}"));
                    return Ok(None);
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

/// `trap [action] condition...`. Only EXIT ever fires: installing a real signal
/// handler needs syscalls this shell cannot make, so the other conditions are
/// recorded and reported but never delivered.
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
            return special_usage_error(sh, &format!("td-sh: trap: Illegal option -{c}"));
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
            err_line(sh, &format!("td-sh: trap: {name}: invalid signal specification"));
            code = 1;
            continue;
        };
        match action {
            None | Some("-") => {
                sh.traps.remove(&signo);
            }
            Some(a) => {
                sh.traps.insert(signo, a.to_string());
            }
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
            // prints (see `out`), rather than silently succeeding.
            if write_fd(sh, 1, line.as_bytes()).is_err() {
                ret = 1;
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
                    if write_fd(sh, 1, line.as_bytes()).is_err() {
                        ret = 1;
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
        Some(flags) => {
            for bad in flags.chars() {
                if bad != 'a' {
                    err_line(sh, &format!("unalias: Illegal option -{bad}"));
                    return status(sh, 2);
                }
            }
            sh.aliases.clear();
            return status(sh, 0);
        }
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

/// `exec [command [arg …]]`. With no command this is only a carrier for its
/// redirections, which the caller leaves in force instead of restoring; with one
/// it REPLACES this shell, so it returns only when the command cannot be run.
fn exec_cmd(sh: &mut Shell, argv: &[String]) -> R<()> {
    match argv.get(1..) {
        None | Some([]) => ok(sh),
        Some(words) => process::exec_replace(sh, words),
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
    let what = format!("{name}: ");
    // A `return` in a sourced file returns from the `.`, not the process.
    match exec::run_source(sh, &text, &what) {
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
        // POSIX: `command` strips a builtin's special properties, which in dash
        // means the builtin runs inside a scratch local frame -- which is why
        // `command local s=1` leaves `s` as it was while a bare `local s=1` does
        // not. Wrapping every builtin is a no-op by construction: declaring into a
        // frame is what `local` is, so nothing else can observe one.
        let saved = std::mem::take(&mut sh.locals);
        let result = run(sh, bi, &rest);
        exec::pop_locals(sh);
        sh.locals = saved;
        return result;
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

fn binary_op(sh: &Shell, a: &str, op: &str, b: &str) -> Result<bool, String> {
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

fn unary_op(sh: &Shell, op: &str, arg: &str) -> Result<bool, String> {
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
    fn local_shadows_and_is_restored_on_return() {
        let (_, out, _) = run_capturing(
            "x=g; y=g; f() { local x=l y; y=mut; echo \"in $x $y\"; }; f; echo \"out $x $y\"",
        );
        // `local y` declares without assigning, so y keeps `g` until the function
        // writes to it -- and that write does not escape.
        assert_eq!(out, "in l mut\nout g g\n");
        // A name that did not exist before is removed again, not left empty.
        let (_, out, _) = run_capturing("f() { local n=1; }; f; echo \"[${n-unset}]\"");
        assert_eq!(out, "[unset]\n");
    }

    #[test]
    fn a_bare_local_leaves_the_name_unset() {
        // Only a name that did not exist starts out unset; `set -u` is then fatal.
        let (_, out, _) = run_capturing("f() { local x; echo \"[${x-default}]\"; }; f");
        assert_eq!(out, "[default]\n");
        let (status, out, _) = run_capturing("set -u; f() { local x; echo $x; }; f");
        assert_eq!((status, out.as_str()), (2, ""));
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
        // bash's `export -n` is not dash's, and on a special builtin an unknown
        // option ends the script.
        for src in ["export -n undef; echo reached", "readonly -q x; echo reached"] {
            let (status, out, err) = run_capturing(src);
            assert_eq!((status, out.as_str()), (2, ""), "{src}");
            assert!(err.contains("Illegal option"), "{src}: {err:?}");
        }
        // `-p` and `--` stay options, and everything after `--` is a name.
        let (status, out, _) = run_capturing("export -p; readonly -- a=1; echo \"[$a]\"");
        assert_eq!((status, out.as_str()), (0, "[1]\n"));
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
        assert!(err.contains("Illegal option -Z"), "err: {err:?}");
        let (_, out, err) = run_capturing("set -- -a; getopts 'a:' o; echo \"$? $o\"");
        assert_eq!(out, "0 ?\n");
        assert!(err.contains("No arg for -a"), "err: {err:?}");
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
        // anything else -- negative, signed, padded, non-numeric -- is fatal.
        assert_eq!(run_capturing("set -- -a; OPTIND=0; getopts a o; echo \"$o $?\"").1, "a 0\n");
        for bad in ["-1", "abc", "' 2'", "+1"] {
            let (status, out, _) = run_capturing(&format!("OPTIND={bad}; getopts a: x; echo unreached"));
            assert_eq!((status, out.as_str()), (2, ""), "OPTIND={bad} should be fatal");
        }
    }

    #[test]
    fn read_rejects_a_bad_variable_name() {
        // dash validates the operands; `getopts` in this file does too.
        let (status, _, err) = run_capturing("echo hi | read 1bad");
        assert_eq!(status, 2);
        assert!(err.contains("bad variable name"), "err: {err:?}");
    }

    #[test]
    fn read_takes_only_dashs_option_surface() {
        // `-r` still works, clustered or not, and `--` ends the options.
        assert_eq!(run_capturing("printf 'a\\\\b\\n' | { read -r v; echo \"[$v]\"; }").1, "[a\\b]\n");
        // bash-only flags (and dash's own -t, which needs a timeout td-sh cannot
        // implement) are usage errors, not silently ignored.
        for bad in ["-n 1", "-N 1", "-d :", "-u 3", "-t 1", "-s", "-rn1"] {
            let (status, _, err) = run_capturing(&format!("echo hi | {{ read {bad} v; }}"));
            assert_eq!(status, 2, "read {bad} should be a usage error");
            assert!(err.contains("Illegal option"), "read {bad} err: {err:?}");
        }
        // dash requires a variable name; there is no bare-`read` $REPLY fallback.
        let (status, _, err) = run_capturing("echo hi | read");
        assert_eq!(status, 2);
        assert!(err.contains("arg count"), "err: {err:?}");
        // `-p` is accepted with its argument attached or separate (the prompt only
        // prints on a terminal, which the test harness is not).
        assert_eq!(run_capturing("echo hi | { read -p 'x? ' v; echo $v; }"), (0, "hi\n".into(), String::new()));
        assert_eq!(run_capturing("echo hi | { read -pfoo v; echo $v; }").1, "hi\n");
    }

    #[test]
    fn test_file_type_and_mode_operators() {
        // Character device vs socket, on a node every Linux system has.
        assert_eq!(run_capturing("test -c /dev/zero; echo $?").1, "0\n");
        assert_eq!(run_capturing("test -S /dev/zero; echo $?").1, "1\n");
        assert_eq!(run_capturing("test -b /dev/zero; echo $?").1, "1\n");
        // /tmp carries the sticky bit but not setuid/setgid.
        assert_eq!(run_capturing("test -k /tmp; echo $?").1, "0\n");
        assert_eq!(run_capturing("test -u /tmp; echo $?").1, "1\n");
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
            "1\n"
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
        assert_eq!(status, 2);
        assert!(err.contains("Illegal option -z"), "err: {err:?}");
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
    fn trap_prints_what_is_in_force() {
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

    #[test]
    fn a_subshell_starts_with_no_inherited_traps() {
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
    }

    #[test]
    fn exec_of_an_unknown_command_ends_the_shell() {
        // POSIX: a failed `exec` is fatal to a non-interactive shell.
        let (status, out, err) = run_capturing("exec no_such_cmd_xyz\necho NOTREACHED");
        assert_eq!((status, out.as_str()), (127, ""));
        assert!(err.contains("not found"), "err: {err:?}");
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
