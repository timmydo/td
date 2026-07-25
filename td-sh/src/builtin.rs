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
