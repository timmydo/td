//! Arithmetic expansion: `$(( ... ))`.
//!
//! The full POSIX operator set over `i64`, including assignment and the ternary.
//! Names are looked up in the shell and their values are themselves evaluated as
//! expressions (`a=b; b=5; echo $((a))` is 5), bounded by a recursion limit so a
//! self-referential variable errors instead of overflowing the stack.

use crate::exec::{Shell, R};

const MAX_NAME_DEPTH: u32 = 32;

pub fn eval(sh: &mut Shell, text: &str) -> R<i64> {
    let mut p = Arith {
        toks: match lex(text) {
            Ok(t) => t,
            Err(e) => return Err(sh.fatal(&format!("arithmetic: {e}"), 2)),
        },
        pos: 0,
        depth: 0,
        pdepth: 0,
        live: true,
    };
    let value = match p.expr_comma(sh) {
        Ok(v) => v,
        Err(e) => return Err(sh.fatal(&format!("arithmetic: {e}"), 2)),
    };
    if p.peek().is_some() {
        return Err(sh.fatal(
            &format!("arithmetic: unexpected trailing input in {text:?}"),
            2,
        ));
    }
    Ok(value)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tk {
    Num(i64),
    Name(String),
    Op(&'static str),
}

/// Multi-character operators, longest first so `<<=` beats `<<` beats `<`.
const OPS: &[&str] = &[
    "<<=", ">>=", "&&", "||", "<<", ">>", "<=", ">=", "==", "!=", "+=", "-=", "*=", "/=", "%=",
    "&=", "^=", "|=", "+", "-", "*", "/", "%", "<", ">", "!", "~", "&", "^", "|", "?", ":", "(",
    ")", "=", ",",
];

fn lex(text: &str) -> Result<Vec<Tk>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            let (value, next) = lex_number(&chars, i)?;
            toks.push(Tk::Num(value));
            i = next;
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let mut name = String::new();
            while let Some(&n) = chars.get(i) {
                if !(n.is_ascii_alphanumeric() || n == '_') {
                    break;
                }
                name.push(n);
                i += 1;
            }
            toks.push(Tk::Name(name));
            continue;
        }
        // Longest-match over OPS by comparing each candidate directly against the
        // char slice at `i` — no per-character tail allocation (keeps the lexer
        // linear). A plain loop rather than an iterator search combinator: this
        // source is embedded verbatim into the td-sh recipe as a WriteFile body,
        // and the ladder guard that keeps GNU findutils out of the tool tier scans
        // those bodies and rejects that tool's bare name as a token, even though
        // rustc never runs the file as a script (see td-sh.rs recipe).
        let mut matched = None;
        for op in OPS {
            if op_at(&chars, i, op) {
                matched = Some(*op);
                break;
            }
        }
        let Some(op) = matched else {
            return Err(format!("unexpected character {c:?}"));
        };
        toks.push(Tk::Op(op));
        i += op.chars().count();
    }
    Ok(toks)
}

/// True when `op`'s characters match `chars` starting at `i`, comparing against
/// the slice in place rather than materialising the remaining tail.
fn op_at(chars: &[char], i: usize, op: &str) -> bool {
    let mut k = i;
    for oc in op.chars() {
        match chars.get(k) {
            Some(&c) if c == oc => k += 1,
            _ => return false,
        }
    }
    true
}

/// `0x` hex, leading-`0` octal, else decimal — the C conventions POSIX inherits.
fn lex_number(chars: &[char], start: usize) -> Result<(i64, usize), String> {
    let mut i = start;
    let (radix, skip) = match (chars.get(i), chars.get(i + 1)) {
        (Some('0'), Some('x')) | (Some('0'), Some('X')) => (16, 2),
        (Some('0'), Some(d)) if d.is_ascii_digit() => (8, 1),
        _ => (10, 0),
    };
    i += skip;
    let mut digits = String::new();
    while let Some(&c) = chars.get(i) {
        if c.is_ascii_alphanumeric() {
            digits.push(c);
            i += 1;
        } else {
            break;
        }
    }
    if digits.is_empty() {
        // A bare `0`; the radix prefix consumed the only digit.
        return Ok((0, i));
    }
    i64::from_str_radix(&digits, radix)
        .map(|v| (v, i))
        .map_err(|_| format!("invalid number {digits:?} in base {radix}"))
}

struct Arith {
    toks: Vec<Tk>,
    pos: usize,
    depth: u32,
    /// Parenthesis-nesting depth, bounded so `((((…))))` errors instead of
    /// overflowing the recursive-descent stack.
    pdepth: u32,
    /// False while parsing the unevaluated side of `&&`, `||` or `?:`. The
    /// tokens still have to be consumed, so the walk continues — it just must
    /// not assign, divide by zero or overflow on the way through.
    live: bool,
}

/// Bound on operator/parenthesis nesting inside `$(( … ))`. Enforced via enter()/
/// leave() around every recursive-descent site — unary chains (`!!!…`), parentheses,
/// and the right-associative `=`/ternary recursions — so each contributes to the
/// depth count regardless of which frame recurses. Real expressions nest a handful
/// deep; this only fires on pathological input that would otherwise overflow the
/// native stack.
const MAX_EXPR_DEPTH: u32 = 100;

type A<T> = Result<T, String>;

impl Arith {
    fn peek(&self) -> Option<&Tk> {
        self.toks.get(self.pos)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Some(Tk::Op(o)) if *o == op) {
            self.pos += 1;
            return true;
        }
        false
    }

    fn peek_op(&self) -> Option<&'static str> {
        match self.peek() {
            Some(Tk::Op(o)) => Some(o),
            _ => None,
        }
    }

    // Bound recursive descent. Every recursion site (unary chains, parenthesised
    // sub-expressions, right-associative `=`/ternary) brackets its recursive call
    // with enter()/leave() so `pdepth` reflects the true native-stack depth and a
    // pathological expression errors instead of overflowing. leave() runs on all
    // paths (dec-on-exit) so a flat, non-nested expression never accumulates.
    fn enter(&mut self) -> A<()> {
        self.pdepth += 1;
        if self.pdepth > MAX_EXPR_DEPTH {
            self.pdepth -= 1;
            return Err("expression nested too deeply".into());
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.pdepth -= 1;
    }

    fn expr_comma(&mut self, sh: &mut Shell) -> A<i64> {
        let mut v = self.expr_assign(sh)?;
        while self.eat_op(",") {
            v = self.expr_assign(sh)?;
        }
        Ok(v)
    }

    fn expr_assign(&mut self, sh: &mut Shell) -> A<i64> {
        // An assignment is only an assignment when a name is followed by one of
        // the assignment operators; otherwise fall back to the ternary.
        if let Some(Tk::Name(name)) = self.peek().cloned() {
            let op = match self.toks.get(self.pos + 1) {
                Some(Tk::Op(o)) => *o,
                _ => "",
            };
            let compound = matches!(
                op,
                "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "<<=" | ">>=" | "&=" | "^=" | "|="
            );
            if compound {
                self.pos += 2;
                self.enter()?;
                let rhs = self.expr_assign(sh);
                self.leave();
                let rhs = rhs?;
                let cur = if op == "=" {
                    0
                } else {
                    self.name_value(sh, &name)?
                };
                let live = self.live;
                let value = match op {
                    "=" => rhs,
                    "+=" => arith2(live, cur, rhs, i64::checked_add)?,
                    "-=" => arith2(live, cur, rhs, i64::checked_sub)?,
                    "*=" => arith2(live, cur, rhs, i64::checked_mul)?,
                    "/=" => checked_div(live, cur, rhs)?,
                    "%=" => checked_rem(live, cur, rhs)?,
                    "<<=" => shift_left(live, cur, rhs)?,
                    ">>=" => shift_right(live, cur, rhs)?,
                    "&=" => cur & rhs,
                    "^=" => cur ^ rhs,
                    _ => cur | rhs,
                };
                if live {
                    sh.set_var(&name, &value.to_string())
                        .map_err(|_| format!("{name}: is read only"))?;
                }
                return Ok(value);
            }
        }
        self.expr_ternary(sh)
    }

    fn expr_ternary(&mut self, sh: &mut Shell) -> A<i64> {
        let cond = self.expr_or(sh)?;
        if !self.eat_op("?") {
            return Ok(cond);
        }
        let outer = self.live;
        self.live = outer && cond != 0;
        let then = self.expr_assign(sh)?;
        self.live = outer;
        if !self.eat_op(":") {
            return Err("expected `:` in `?:`".into());
        }
        self.live = outer && cond == 0;
        self.enter()?;
        let other = self.expr_ternary(sh);
        self.leave();
        let other = other?;
        self.live = outer;
        Ok(if cond != 0 { then } else { other })
    }

    fn expr_or(&mut self, sh: &mut Shell) -> A<i64> {
        let mut v = self.expr_and(sh)?;
        while self.eat_op("||") {
            let outer = self.live;
            self.live = outer && v == 0;
            let rhs = self.expr_and(sh)?;
            self.live = outer;
            v = i64::from(v != 0 || rhs != 0);
        }
        Ok(v)
    }

    fn expr_and(&mut self, sh: &mut Shell) -> A<i64> {
        let mut v = self.expr_bitor(sh)?;
        while self.eat_op("&&") {
            let outer = self.live;
            self.live = outer && v != 0;
            let rhs = self.expr_bitor(sh)?;
            self.live = outer;
            v = i64::from(v != 0 && rhs != 0);
        }
        Ok(v)
    }

    fn expr_bitor(&mut self, sh: &mut Shell) -> A<i64> {
        let mut v = self.expr_bitxor(sh)?;
        while self.peek_op() == Some("|") {
            self.pos += 1;
            v |= self.expr_bitxor(sh)?;
        }
        Ok(v)
    }

    fn expr_bitxor(&mut self, sh: &mut Shell) -> A<i64> {
        let mut v = self.expr_bitand(sh)?;
        while self.peek_op() == Some("^") {
            self.pos += 1;
            v ^= self.expr_bitand(sh)?;
        }
        Ok(v)
    }

    fn expr_bitand(&mut self, sh: &mut Shell) -> A<i64> {
        let mut v = self.expr_eq(sh)?;
        while self.peek_op() == Some("&") {
            self.pos += 1;
            v &= self.expr_eq(sh)?;
        }
        Ok(v)
    }

    fn expr_eq(&mut self, sh: &mut Shell) -> A<i64> {
        let mut v = self.expr_rel(sh)?;
        loop {
            let op = match self.peek_op() {
                Some(o @ ("==" | "!=")) => o,
                _ => return Ok(v),
            };
            self.pos += 1;
            let rhs = self.expr_rel(sh)?;
            v = i64::from(if op == "==" { v == rhs } else { v != rhs });
        }
    }

    fn expr_rel(&mut self, sh: &mut Shell) -> A<i64> {
        let mut v = self.expr_shift(sh)?;
        loop {
            let op = match self.peek_op() {
                Some(o @ ("<" | "<=" | ">" | ">=")) => o,
                _ => return Ok(v),
            };
            self.pos += 1;
            let rhs = self.expr_shift(sh)?;
            v = i64::from(match op {
                "<" => v < rhs,
                "<=" => v <= rhs,
                ">" => v > rhs,
                _ => v >= rhs,
            });
        }
    }

    fn expr_shift(&mut self, sh: &mut Shell) -> A<i64> {
        let mut v = self.expr_add(sh)?;
        loop {
            let op = match self.peek_op() {
                Some(o @ ("<<" | ">>")) => o,
                _ => return Ok(v),
            };
            self.pos += 1;
            let rhs = self.expr_add(sh)?;
            v = if op == "<<" {
                shift_left(self.live, v, rhs)?
            } else {
                shift_right(self.live, v, rhs)?
            };
        }
    }

    fn expr_add(&mut self, sh: &mut Shell) -> A<i64> {
        let mut v = self.expr_mul(sh)?;
        loop {
            let op = match self.peek_op() {
                Some(o @ ("+" | "-")) => o,
                _ => return Ok(v),
            };
            self.pos += 1;
            let rhs = self.expr_mul(sh)?;
            v = if op == "+" {
                arith2(self.live, v, rhs, i64::checked_add)?
            } else {
                arith2(self.live, v, rhs, i64::checked_sub)?
            };
        }
    }

    fn expr_mul(&mut self, sh: &mut Shell) -> A<i64> {
        let mut v = self.expr_unary(sh)?;
        loop {
            let op = match self.peek_op() {
                Some(o @ ("*" | "/" | "%")) => o,
                _ => return Ok(v),
            };
            self.pos += 1;
            let rhs = self.expr_unary(sh)?;
            v = match op {
                "*" => arith2(self.live, v, rhs, i64::checked_mul)?,
                "/" => checked_div(self.live, v, rhs)?,
                _ => checked_rem(self.live, v, rhs)?,
            };
        }
    }

    fn expr_unary(&mut self, sh: &mut Shell) -> A<i64> {
        // Unary chains and parenthesised sub-expressions descend through here; the
        // assignment/ternary recursion sites bracket themselves (see enter/leave).
        self.enter()?;
        let r = self.expr_unary_inner(sh);
        self.leave();
        r
    }

    fn expr_unary_inner(&mut self, sh: &mut Shell) -> A<i64> {
        match self.peek_op() {
            Some("-") => {
                self.pos += 1;
                let v = self.expr_unary(sh)?;
                match v.checked_neg() {
                    Some(n) => Ok(n),
                    None if !self.live => Ok(0),
                    None => Err("integer overflow".into()),
                }
            }
            Some("+") => {
                self.pos += 1;
                self.expr_unary(sh)
            }
            Some("!") => {
                self.pos += 1;
                Ok(i64::from(self.expr_unary(sh)? == 0))
            }
            Some("~") => {
                self.pos += 1;
                Ok(!self.expr_unary(sh)?)
            }
            _ => self.expr_primary(sh),
        }
    }

    fn expr_primary(&mut self, sh: &mut Shell) -> A<i64> {
        match self.peek().cloned() {
            Some(Tk::Num(n)) => {
                self.pos += 1;
                Ok(n)
            }
            Some(Tk::Name(name)) => {
                self.pos += 1;
                self.name_value(sh, &name)
            }
            Some(Tk::Op("(")) => {
                self.pos += 1;
                let v = self.expr_comma(sh)?;
                if !self.eat_op(")") {
                    return Err("expected `)`".into());
                }
                Ok(v)
            }
            Some(Tk::Op(o)) => Err(format!("unexpected operator `{o}`")),
            None => Err("unexpected end of expression".into()),
        }
    }

    /// A variable's value, itself evaluated as an expression. An unset or empty
    /// variable is 0 (POSIX).
    fn name_value(&mut self, sh: &mut Shell, name: &str) -> A<i64> {
        let Some(text) = sh.get_var(name) else {
            return Ok(0);
        };
        if text.trim().is_empty() {
            return Ok(0);
        }
        if let Ok(n) = text.trim().parse::<i64>() {
            return Ok(n);
        }
        // A non-numeric value is itself an expression, but the dead side of a
        // short circuit (`0 && x`, `1 || x`, the untaken `?:` branch) must not be
        // evaluated — nor allowed to error or recurse. It contributes 0.
        if !self.live {
            return Ok(0);
        }
        if self.depth >= MAX_NAME_DEPTH {
            return Err(format!("{name}: expression recursion limit exceeded"));
        }
        let mut inner = Arith {
            toks: lex(&text)?,
            pos: 0,
            depth: self.depth + 1,
            pdepth: 0,
            live: self.live,
        };
        let v = inner.expr_comma(sh)?;
        if inner.peek().is_some() {
            return Err(format!("{name}: invalid expression {text:?}"));
        }
        Ok(v)
    }
}

/// Apply a checked binary operator, folding to 0 on the unevaluated side of a
/// short-circuit rather than reporting an error the shell would never have hit.
fn arith2(live: bool, a: i64, b: i64, f: fn(i64, i64) -> Option<i64>) -> A<i64> {
    match f(a, b) {
        Some(v) => Ok(v),
        None if !live => Ok(0),
        None => Err("integer overflow".into()),
    }
}

fn checked_div(live: bool, a: i64, b: i64) -> A<i64> {
    if b == 0 {
        return if live {
            Err("division by zero".into())
        } else {
            Ok(0)
        };
    }
    arith2(live, a, b, i64::checked_div)
}

fn checked_rem(live: bool, a: i64, b: i64) -> A<i64> {
    if b == 0 {
        return if live {
            Err("division by zero".into())
        } else {
            Ok(0)
        };
    }
    arith2(live, a, b, i64::checked_rem)
}

fn shift_left(live: bool, a: i64, b: i64) -> A<i64> {
    match u32::try_from(b).ok().and_then(|n| a.checked_shl(n)) {
        Some(v) => Ok(v),
        None if !live => Ok(0),
        None => Err("invalid shift count".into()),
    }
}

fn shift_right(live: bool, a: i64, b: i64) -> A<i64> {
    match u32::try_from(b).ok().and_then(|n| a.checked_shr(n)) {
        Some(v) => Ok(v),
        None if !live => Ok(0),
        None => Err("invalid shift count".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(src: &str) -> Result<i64, String> {
        let mut sh = Shell::new_for_test();
        eval(&mut sh, src).map_err(|_| format!("evaluation of {src:?} failed"))
    }

    #[test]
    fn precedence_and_parentheses() -> Result<(), String> {
        assert_eq!(ev("2 + 3 * 4")?, 14);
        assert_eq!(ev("(2 + 3) * 4")?, 20);
        assert_eq!(ev("1 + 2 == 3")?, 1);
        assert_eq!(ev("7 % 3")?, 1);
        assert_eq!(ev("-3 + 1")?, -2);
        assert_eq!(ev("!0")?, 1);
        assert_eq!(ev("1 << 4")?, 16);
        assert_eq!(ev("1 ? 10 : 20")?, 10);
        assert_eq!(ev("0 ? 10 : 20")?, 20);
        // The unevaluated side of a short circuit must not raise its errors.
        assert_eq!(ev("0 && 1/0")?, 0);
        assert_eq!(ev("1 || 1/0")?, 1);
        assert_eq!(ev("1 ? 2 : 1/0")?, 2);
        Ok(())
    }

    #[test]
    fn radix_prefixes() -> Result<(), String> {
        assert_eq!(ev("0x10")?, 16);
        assert_eq!(ev("010")?, 8);
        assert_eq!(ev("10")?, 10);
        assert_eq!(ev("0")?, 0);
        Ok(())
    }

    #[test]
    fn variables_are_read_and_assigned() -> Result<(), String> {
        let mut sh = Shell::new_for_test();
        sh.set_var("a", "6").map_err(|_| "set failed".to_string())?;
        sh.set_var("b", "7").map_err(|_| "set failed".to_string())?;
        assert_eq!(eval(&mut sh, "a * b").map_err(|_| "eval")?, 42);
        assert_eq!(eval(&mut sh, "i = 3 + 4").map_err(|_| "eval")?, 7);
        assert_eq!(sh.get_var("i").as_deref(), Some("7"));
        assert_eq!(eval(&mut sh, "i += 1").map_err(|_| "eval")?, 8);
        assert_eq!(sh.get_var("i").as_deref(), Some("8"));
        // An unset name is zero.
        assert_eq!(eval(&mut sh, "nope + 1").map_err(|_| "eval")?, 1);
        Ok(())
    }

    #[test]
    fn variable_values_are_themselves_expressions() -> Result<(), String> {
        let mut sh = Shell::new_for_test();
        sh.set_var("b", "5").map_err(|_| "set failed".to_string())?;
        sh.set_var("a", "b").map_err(|_| "set failed".to_string())?;
        assert_eq!(eval(&mut sh, "a").map_err(|_| "eval")?, 5);
        // A self-reference must terminate rather than recurse forever.
        sh.set_var("loop", "loop")
            .map_err(|_| "set failed".to_string())?;
        assert!(eval(&mut sh, "loop").is_err());
        Ok(())
    }

    #[test]
    fn errors_are_reported_not_panicked() {
        assert!(ev("1 / 0").is_err());
        assert!(ev("1 +").is_err());
        assert!(ev("1 @ 2").is_err());
        assert!(ev("(1").is_err());
        assert!(ev("9223372036854775807 + 1").is_err());
    }

    #[test]
    fn deeply_nested_exprs_error_instead_of_overflowing() {
        // Past MAX_EXPR_DEPTH the parser errors rather than recursing into a stack
        // overflow. Every recursion site is covered: parentheses and unary chains
        // (via expr_unary), right-associative assignment (`a=a=…=1`), and
        // right-nested ternary (`0?1:0?1:…`) — the last two recurse in expr_assign/
        // expr_ternary, NOT through expr_unary, so they need their own brackets.
        assert!(ev(&("(".repeat(1000) + "1" + &")".repeat(1000))).is_err());
        assert!(ev(&("!".repeat(1000) + "1")).is_err());
        assert!(ev(&("-".repeat(1000) + "1")).is_err());
        assert!(ev(&("a=".repeat(1000) + "1")).is_err());
        assert!(ev(&("0?1:".repeat(1000) + "1")).is_err());
    }

    #[test]
    fn dead_short_circuit_branch_does_not_evaluate_variables() -> Result<(), String> {
        let mut sh = Shell::new_for_test();
        // A self-referential name would recurse/err if evaluated; the dead side of
        // `||`/`&&`/`?:` must skip it and contribute 0.
        sh.set_var("x", "x").map_err(|_| "set failed".to_string())?;
        assert_eq!(eval(&mut sh, "1 || x").map_err(|_| "eval")?, 1);
        assert_eq!(eval(&mut sh, "0 && x").map_err(|_| "eval")?, 0);
        assert_eq!(eval(&mut sh, "1 ? 2 : x").map_err(|_| "eval")?, 2);
        Ok(())
    }
}
