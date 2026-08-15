//! Arithmetic expansion: `$(( ... ))`.
//!
//! The full POSIX operator set over `i64`, including assignment and the ternary.
//! A name's value must itself be a number: dash resolves it with `strtoimax` and
//! errors on anything else, so `e=1+2; echo $((e+3))` is a fatal error rather
//! than bash's 6 — the value is data, never a nested expression to evaluate.

use crate::exec::{Shell, R};

/// `$(( ))`'s entry point: a malformed expression is FATAL, which is what dash
/// does and what every caller here wants.
pub fn eval(sh: &mut Shell, text: &str) -> R<i64> {
    match try_eval(sh, text) {
        Ok(v) => Ok(v),
        Err(msg) => Err(sh.fatal(&msg, 2)),
    }
}

/// The same evaluation, reporting a malformed expression as a MESSAGE instead
/// of ending the shell. `[[ ]]` needs this: bash answers `[[ 1+ -eq 2 ]]` with
/// a diagnostic and a false result and carries on, where routing it through
/// `eval` above kills a non-interactive shell at the first bad expression.
pub fn try_eval(sh: &mut Shell, text: &str) -> Result<i64, String> {
    let mut p = Arith {
        toks: lex(text).map_err(|e| format!("arithmetic: {e}"))?,
        pos: 0,
        pdepth: 0,
        live: true,
        ternary_dead: false,
    };
    let value = p.expr_comma(sh).map_err(|e| format!("arithmetic: {e}"))?;
    if p.peek().is_some() {
        return Err(format!("arithmetic: unexpected trailing input in {text:?}"));
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
    "&=", "^=", "|=", "+", "-", "*", "/", "%", "<", ">", "!", "~", "&", "^", "|", "?",
    ":", "(", ")", "=", ",",
];

const PREC_UNARY: u8 = 14;

/// The one right-associative level, and where a COMPLETED conditional waits.
const PREC_ASSIGN: u8 = 2;

/// Marks a region nothing inside may displace out of: a `(`, and a `?`, whose
/// middle expression busybox brackets with an implicit parenthesis.
const BARRIER: u8 = 0;

/// Binding power, mirroring this file's descent so the lexer can tell which
/// pending operators an incoming one displaces. Assignment is the one
/// right-associative level, which is what keeps `a = b = c` unreduced.
fn prec(op: &str) -> u8 {
    match op {
        "," => 1,
        "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "<<=" | ">>=" | "&=" | "^=" | "|=" => PREC_ASSIGN,
        "?" | ":" => 3,
        "||" => 4,
        "&&" => 5,
        "|" => 6,
        "^" => 7,
        "&" => 8,
        "==" | "!=" => 9,
        "<" | "<=" | ">" | ">=" => 10,
        "<<" | ">>" => 11,
        "+" | "-" => 12,
        "*" | "/" | "%" => 13,
        _ => PREC_UNARY,
    }
}

fn lex(text: &str) -> Result<Vec<Tk>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0usize;
    // Whether the numstack TOP is a variable, which is what busybox asks before
    // splitting a pair (math.c:791) -- not "the last token was a name":
    // `var_name` survives a PENDING operator and dies when one is APPLIED
    // (math.c:498). Knowing which pending ones an operator applies is the whole
    // reason `prec` is mirrored here.
    let mut operand_is_name = false;
    // Whether the last token COMPLETED an operand, which is what makes the next
    // `+`/`-` binary. A postfix pair completes one; a prefix pair does not.
    let mut ends_operand = false;
    let mut pending: Vec<u8> = Vec::new();
    while let Some(&c) = chars.get(i) {
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            let (value, next) = lex_number(&chars, i)?;
            toks.push(Tk::Num(value));
            operand_is_name = false;
            ends_operand = true;
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
            operand_is_name = true;
            ends_operand = true;
            continue;
        }
        // An adjacent `++`/`--` is ONE token only where it binds -- backward to a
        // name already lexed, or forward to one after it. Otherwise a single
        // character is emitted and the scan resumes at the SECOND, which may
        // itself begin a binding pair (math.c:780-801). It has to happen here:
        // by parse time the pair is one token and the re-scan is gone.
        if (c == '+' || c == '-') && chars.get(i + 1) == Some(&c) {
            let mut k = i + 2;
            while chars.get(k).is_some_and(|n| n.is_whitespace()) {
                k += 1;
            }
            let fwd = chars
                .get(k)
                .is_some_and(|n| n.is_ascii_alphabetic() || *n == '_');
            if !operand_is_name && !fwd {
                toks.push(Tk::Op(if c == '+' { "+" } else { "-" }));
                ends_operand = false;
                i += 1;
                continue;
            }
            // It binds, so it is an operator over a name and is displaced like a
            // unary sign -- PENDING, not applied, so the name stays reachable
            // until something displaces it. Binding BACKWARD also completes an
            // operand, which is what makes the next `+`/`-` binary.
            toks.push(Tk::Op(if c == '+' { "++" } else { "--" }));
            ends_operand = operand_is_name;
            pending.push(PREC_UNARY);
            i += 2;
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
        match op {
            "(" => {
                pending.push(BARRIER);
                ends_operand = false;
            }
            // `:` closes the conditional's implicit parenthesis, which busybox
            // synthesizes as a real RPAREN (math.c:840-842). Two things then
            // differ from `)`: the conditional itself stays PENDING, so only a
            // `,` is low enough to reduce it, and it still WANTS an operand,
            // which is what keeps a sign after it unary.
            ")" | ":" => {
                while pending.last().is_some_and(|p| *p != BARRIER) {
                    pending.pop();
                }
                pending.pop();
                operand_is_name = false;
                if op == ":" {
                    pending.push(PREC_ASSIGN);
                }
                ends_operand = op == ")";
            }
            _ => {
                // A unary sign takes no operand from the stack, so it displaces
                // nothing; `-a` still leaves `a` reachable until `+` arrives.
                let unary = matches!(op, "+" | "-" | "!" | "~") && !ends_operand;
                let p = prec(op);
                let right = p == PREC_ASSIGN;
                if !unary {
                    while pending
                        .last()
                        .is_some_and(|q| *q != BARRIER && (*q > p || (*q == p && !right)))
                    {
                        pending.pop();
                        operand_is_name = false;
                    }
                }
                if op == "?" {
                    pending.push(BARRIER);
                } else if !(unary && op == "+") {
                    // busybox DISCARDS a unary plus rather than stacking it, so
                    // nothing later can apply it: `+a+--1` still holds `a` and is
                    // refused, where `-a+--1`, whose sign IS applied, splits.
                    pending.push(if unary { PREC_UNARY } else { p });
                }
                ends_operand = false;
            }
        }
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
        // `0x` with no hex digit after it is not a hex constant: it falls through
        // to the decimal run, which then fails to parse — so both of this file's
        // number parsers reject it, as dash does by lexing `0` and a name `x`.
        (Some('0'), Some('x' | 'X')) if chars.get(i + 2).is_some_and(char::is_ascii_hexdigit) => {
            (16, 2)
        }
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
        return Err("expected a digit".into());
    }
    i64::from_str_radix(&digits, radix)
        .map(|v| (v, i))
        .map_err(|_| format!("invalid number {digits:?} in base {radix}"))
}

struct Arith {
    toks: Vec<Tk>,
    pos: usize,
    /// Parenthesis-nesting depth, bounded so `((((…))))` errors instead of
    /// overflowing the recursive-descent stack.
    pdepth: u32,
    /// False while parsing the unevaluated side of `&&`, `||` or `?:`. The
    /// tokens still have to be consumed, so the walk continues — it just must
    /// not assign, divide by zero or overflow on the way through.
    live: bool,
    /// The untaken side of a `?:` specifically. ash does not evaluate that one
    /// AT ALL, where it DOES evaluate the dead side of `&&`/`||` for effect --
    /// so this is the only place a DYNAMIC read must be skipped, drawing being
    /// the side effect in question.
    ternary_dead: bool,
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
                // The lvalue is read BEFORE the right-hand side, as ash does:
                // an rhs that assigns or steps the same name must not be the
                // value this operator accumulates onto. `=` never reads it, so
                // an unset or non-numeric name stays assignable.
                let cur = if op == "=" {
                    0
                } else {
                    self.name_value(sh, &name)?
                };
                self.enter()?;
                let rhs = self.expr_assign(sh);
                self.leave();
                let rhs = rhs?;
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
        let outer_dead = self.ternary_dead;
        self.live = outer && cond != 0;
        self.ternary_dead = outer_dead || cond == 0;
        let then = self.expr_assign(sh)?;
        self.live = outer;
        self.ternary_dead = outer_dead;
        if !self.eat_op(":") {
            return Err("expected `:` in `?:`".into());
        }
        self.live = outer && cond == 0;
        self.ternary_dead = outer_dead || cond != 0;
        self.enter()?;
        let other = self.expr_ternary(sh);
        self.leave();
        let other = other?;
        self.live = outer;
        self.ternary_dead = outer_dead;
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
                self.negate(v)
            }
            Some("+") => {
                self.pos += 1;
                self.expr_unary(sh)
            }
            // The lexer only emits this token where it BINDS, and a backward
            // binding is consumed as a postfix by `expr_primary`, so reaching
            // here means a name follows. The arm is still fallible rather than
            // asserting: nothing in the type system says so.
            Some(o @ ("++" | "--")) => {
                let Some(Tk::Name(name)) = self.toks.get(self.pos + 1).cloned() else {
                    return Err(format!("expected a name after `{o}`"));
                };
                self.pos += 2;
                Ok(self.step(sh, &name, o == "++")?.1)
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
                // A postfix `++`/`--` applies ONLY directly after a name: after a
                // number or a `)` the same token is a binary operator followed by
                // a unary one, which is why `$((1--1))` is 2 and `$(((n)--1))` is
                // n+1 while `$((n--1))` is a syntax error.
                match self.peek_op() {
                    Some(o @ ("++" | "--")) => {
                        let up = o == "++";
                        self.pos += 1;
                        Ok(self.step(sh, &name, up)?.0)
                    }
                    _ => self.name_value(sh, &name),
                }
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

    fn negate(&self, v: i64) -> A<i64> {
        match v.checked_neg() {
            Some(n) => Ok(n),
            None if !self.live => Ok(0),
            None => Err("integer overflow".into()),
        }
    }

    /// One `++`/`--` step, returning the value before and after it. Wrapping,
    /// not checked as the rest of this file is: ash steps past i64's bounds
    /// silently.
    fn step(&mut self, sh: &mut Shell, name: &str, up: bool) -> A<(i64, i64)> {
        let cur = self.name_value(sh, name)?;
        let new = if up { cur.wrapping_add(1) } else { cur.wrapping_sub(1) };
        if self.live {
            sh.set_var(name, &new.to_string())
                .map_err(|_| format!("{name}: is read only"))?;
        }
        Ok((cur, new))
    }

    /// A variable's value as a number. An unset or empty variable is 0; anything
    /// else must BE a number, not an expression that evaluates to one.
    fn name_value(&mut self, sh: &mut Shell, name: &str) -> A<i64> {
        // The value is unused here and READING is a side effect for a dynamic
        // name, so the untaken `?:` branch must not reach the lookup at all.
        if self.ternary_dead {
            return Ok(0);
        }
        let Some(text) = crate::expand::var_value(sh, name) else {
            return Ok(0);
        };
        if let Some(n) = strtoimax(&text) {
            return Ok(n);
        }
        // The dead side of a short circuit (`0 && x`, `1 || x`, the untaken `?:`
        // branch) is never evaluated, so a junk value there cannot error.
        if !self.live {
            return Ok(0);
        }
        Err(format!("{name}: bad number"))
    }
}

/// C `strtoimax(value, &end, 0)` with dash's "the whole value or nothing" check:
/// the base comes from the C prefix and any leftover input rejects the value.
/// Blanks around the number are skipped and a value that is empty or all blanks
/// is 0 — `a=' '; echo $((a))` prints 0 in dash, so the check cannot be
/// strtoimax's literal `*end == '\0'`, which would convert nothing there.
fn strtoimax(text: &str) -> Option<i64> {
    // Bytes, not chars: every character this accepts is ASCII, so a non-ASCII
    // byte can only end the digit run — and a name is read once per arithmetic
    // operand, which `while [ $i -lt N ]; i=$((i+1))` makes an inner loop.
    // C `isspace`, not `char::is_whitespace`: a value carries arbitrary UTF-8 and
    // strtoimax converts nothing from a NBSP-prefixed one.
    let text = text.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r'));
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return Some(0);
    }
    let mut i = 0usize;
    let negative = match bytes.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let radix = match (bytes.get(i), bytes.get(i + 1)) {
        (Some(b'0'), Some(b'x' | b'X')) if bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit) => {
            i += 2;
            16
        }
        (Some(b'0'), _) => 8, // a lone `0` is octal zero, which parses the same
        _ => 10,
    };
    let start = i;
    while bytes.get(i).is_some_and(|b| char::from(*b).is_digit(radix)) {
        i += 1;
    }
    if i == start || i != bytes.len() {
        return None;
    }
    // Magnitude first, then apply the sign: i64::MIN has no positive counterpart,
    // so `-9223372036854775808` has to be recognised as the whole value it is.
    let magnitude = u64::from_str_radix(text.get(start..i)?, radix).ok()?;
    if !negative {
        return i64::try_from(magnitude).ok();
    }
    if magnitude == i64::MIN.unsigned_abs() {
        return Some(i64::MIN);
    }
    i64::try_from(magnitude).ok()?.checked_neg()
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
    fn increment_binds_to_a_name_and_to_nothing_else() -> Result<(), String> {
        let ev = |src: &str, start: &str| -> Result<(i64, String), String> {
            let mut sh = Shell::new_for_test();
            sh.set_var("n", start).map_err(|_| "set failed".to_string())?;
            let v = eval(&mut sh, src).map_err(|_| format!("eval {src}"))?;
            Ok((v, sh.get_var("n").unwrap_or_default()))
        };
        // The four forms: the value differs before/after, the store does not.
        assert_eq!(ev("n++", "1")?, (1, "2".into()));
        assert_eq!(ev("++n", "1")?, (2, "2".into()));
        assert_eq!(ev("n--", "5")?, (5, "4".into()));
        assert_eq!(ev("--n", "5")?, (4, "4".into()));
        // With no NAME to bind to they are simply two unary operators, which is
        // the half that made `++n` answer `n` before any of this existed.
        assert_eq!(ev("++5", "1")?, (5, "1".into()));
        assert_eq!(ev("++(n)", "1")?, (1, "1".into()));
        assert_eq!(ev("1++1", "1")?, (2, "1".into()));
        assert_eq!(ev("1--1", "1")?, (2, "1".into()));
        assert_eq!(ev("(n)--1", "5")?, (6, "5".into()));
        // After a name the token IS taken, so what follows has no operator.
        assert!(eval(&mut Shell::new_for_test(), "n--1").is_err());
        // A pair that binds neither way contributes ONE character and the scan
        // resumes at the second, which may itself begin a binding pair. So a run
        // of three is an operator followed by a prefix step, not the other way
        // round -- busybox spells this out at math.c:780-786.
        assert_eq!(ev("7+++n", "1")?, (9, "2".into()));
        assert_eq!(ev("+++n", "1")?, (2, "2".into()));
        assert_eq!(ev("---n", "1")?, (0, "0".into()));
        assert_eq!(ev("1+++n", "1")?, (3, "2".into()));
        assert_eq!(ev("1---n", "1")?, (1, "0".into()));
        assert_eq!(ev("-----n", "1")?, (0, "0".into()));
        assert_eq!(ev("+++++n", "1")?, (2, "2".into()));
        assert_eq!(ev("++++n", "1")?, (2, "2".into()));
        // With no name anywhere the run is just unary operators.
        assert_eq!(ev("--7", "1")?, (7, "1".into()));
        // Binding BACKWARD wins over splitting, and it is the last OPERAND that
        // decides rather than the last token: after `n +` the operand is still
        // `n`, so this binds and leaves `1` with no operator before it.
        assert!(eval(&mut Shell::new_for_test(), "n+--1").is_err());
        // And binding FORWARD to a name beats reading the token as a binary
        // operator, which leaves the binary position empty. `1 ++ b` is a
        // syntax error rather than `1 + (+b)`, and b is not stepped.
        // The gap before the name is skipped WHOLE, so two spaces bind as one.
        for src in ["1 ++ b", "1 ++  b", "1++b", "1 -- b", "1--b", "0--n", "(n)--m", "n----n"] {
            let mut sh = Shell::new_for_test();
            sh.set_var("b", "3").map_err(|_| "set failed".to_string())?;
            sh.set_var("m", "2").map_err(|_| "set failed".to_string())?;
            sh.set_var("n", "5").map_err(|_| "set failed".to_string())?;
            assert!(eval(&mut sh, src).is_err(), "{src}");
            assert_eq!(sh.get_var("b").as_deref(), Some("3"), "{src}");
        }
        // A `(` is not a name, so there the token still decomposes.
        assert_eq!(ev("1--(n)", "5")?, (6, "5".into()));
        assert_eq!(ev("1++(n)", "5")?, (6, "5".into()));
        // But an OPEN paren does not clear the name under it -- only a closed
        // `(EXPR)` does (math.c:913) -- so this binds backward across the paren
        // and leaves a prefix pair with a number after it.
        for src in ["n*(--1)", "n*(++1)"] {
            let mut sh = Shell::new_for_test();
            sh.set_var("n", "1").map_err(|_| "set failed".to_string())?;
            assert!(eval(&mut sh, src).is_err(), "{src}");
        }
        // A NUMBER does clear it, so a pair after one splits.
        assert_eq!(ev("n*2--1", "1")?, (3, "1".into()));
        assert_eq!(ev("n-2--1", "1")?, (0, "1".into()));
        // A name may begin with `_`, which the forward test has to accept or
        // the pair splits and the step is silently lost.
        let mut sh = Shell::new_for_test();
        sh.set_var("_x", "5").map_err(|_| "set failed".to_string())?;
        assert_eq!(eval(&mut sh, "1+++_x").map_err(|_| "eval")?, 7);
        assert_eq!(sh.get_var("_x").as_deref(), Some("6"));
        // Precedence: the step happens before the surrounding unary applies.
        assert_eq!(ev("-n++", "1")?, (-1, "2".into()));
        assert_eq!(ev("!n++", "1")?, (0, "2".into()));
        assert_eq!(ev("n++ + n++", "1")?, (3, "3".into()));
        assert_eq!(ev("++n + n++", "1")?, (4, "3".into()));
        assert_eq!(ev("n+++1", "1")?, (2, "2".into()));
        assert_eq!(ev("++ ++n", "1")?, (2, "2".into()));
        // ash WRAPS here where every other operator in this file is checked.
        assert_eq!(ev("n++", "9223372036854775807")?, (i64::MAX, i64::MIN.to_string()));
        assert_eq!(ev("n--", "-9223372036854775808")?, (i64::MIN, i64::MAX.to_string()));
        // An unset name steps from zero rather than refusing.
        let mut sh = Shell::new_for_test();
        assert_eq!(eval(&mut sh, "x++").map_err(|_| "eval")?, 0);
        assert_eq!(sh.get_var("x").as_deref(), Some("1"));
        Ok(())
    }

    #[test]
    fn a_split_pair_keeps_ordinary_unary_precedence() -> Result<(), String> {
        // Splitting in the LEXER is what buys this: `1--(n)/2` becomes the same
        // tokens as `1 - -(n)/2`, so the minus binds tighter than `/` for free.
        // Deciding it in the parser instead gave `1 - (-(n/2))`, which agrees on
        // every value but the bound -- only i64::MIN tells the two apart.
        let mut sh = Shell::new_for_test();
        sh.set_var("n", &i64::MIN.to_string()).map_err(|_| "set failed".to_string())?;
        assert!(eval(&mut sh, "1--(n)/2").is_err());
        assert!(eval(&mut sh, "1 - -(n)/2").is_err());
        // Away from the bound the two spellings agree, which is what makes the
        // pair above a statement about parsing rather than about overflow.
        sh.set_var("n", "8").map_err(|_| "set failed".to_string())?;
        assert_eq!(eval(&mut sh, "1--(n)/2").map_err(|_| "eval")?, 5);
        assert_eq!(eval(&mut sh, "1 - -(n)/2").map_err(|_| "eval")?, 5);
        Ok(())
    }

    #[test]
    fn an_applied_operator_clears_the_name_a_pair_could_bind_to() -> Result<(), String> {
        // busybox asks whether its numstack TOP is a name, and applying an
        // operator replaces that top with a value. A merely PENDING operator
        // does not, so the two sides of this differ by whether a reduction has
        // happened yet -- which is why the lexer has to mirror precedence.
        let ev = |src: &str| -> Result<Option<i64>, String> {
            let mut sh = Shell::new_for_test();
            for (k, v) in [("a", "2"), ("b", "3"), ("n", "1")] {
                sh.set_var(k, v).map_err(|_| "set failed".to_string())?;
            }
            Ok(eval(&mut sh, src).ok())
        };
        // Reduced, so the pair splits into two unary signs.
        assert_eq!(ev("a+b+--1")?, Some(6), "a+b+--1");
        assert_eq!(ev("a*b+--1")?, Some(7), "a*b+--1");
        assert_eq!(ev("-a+--1")?, Some(-1), "-a+--1");
        assert_eq!(ev("1?a:--1")?, Some(2), "1?a:--1");
        assert_eq!(ev("a+b+c+--1")?, Some(6), "a+b+c+--1");
        // NOT reduced -- one operand, or a higher-precedence operator still
        // pending -- so the name is still on top and the pair binds to it.
        for src in ["n+--1", "a+b*--1", "a,--1"] {
            assert_eq!(ev(src)?, None, "{src}");
        }
        Ok(())
    }

    fn arith_fixture(src: &str) -> Result<Option<i64>, String> {
        let mut sh = Shell::new_for_test();
        for (k, v) in [("a", "2"), ("b", "3"), ("m", "4"), ("n", "1")] {
            sh.set_var(k, v).map_err(|_| "set failed".to_string())?;
        }
        Ok(eval(&mut sh, src).ok())
    }

    #[test]
    fn a_conditional_brackets_its_middle_expression() -> Result<(), String> {
        // busybox brackets the middle with an implicit parenthesis and
        // synthesizes the closing one at `:` (math.c:840-842). So nothing in the
        // middle reduces what is pending outside it -- `b` is still the
        // assignment's lvalue when `--` arrives, and the pair binds to it --
        // while `:` reduces the middle exactly as `)` does.
        for src in ["a?b=--1:0", "a?b+--1:0", "a?b?--1:0:0"] {
            assert_eq!(arith_fixture(src)?, None, "{src}");
        }
        // Past the `:` the middle has reduced, so the same pair splits. The
        // third arm still WANTS an operand, which is what keeps a sign after
        // `:` unary where one after `)` would be binary.
        assert_eq!(arith_fixture("1?a:--1")?, Some(2), "1?a:--1");
        assert_eq!(arith_fixture("1?a:++1")?, Some(2), "1?a:++1");
        assert_eq!(arith_fixture("0?a:--1")?, Some(1), "0?a:--1");
        // A COMPLETED conditional is still pending, at the right-associative
        // assignment level -- so a `,` reduces it and the pair after that
        // splits, while an assignment or a tighter operator does not and the
        // third arm's name is still on top. Only `,` is low enough.
        assert_eq!(arith_fixture("1?a:b,--1")?, Some(1), "1?a:b,--1");
        assert_eq!(arith_fixture("0?a:b,--1")?, Some(1), "0?a:b,--1");
        for src in ["0?a:b=--1", "0?a:b||--1", "0?a:b?--1:0"] {
            assert_eq!(arith_fixture(src)?, None, "{src}");
        }
        // Binding forward reaches past the lvalue and steps.
        let mut sh = Shell::new_for_test();
        for (k, v) in [("b", "3"), ("n", "1")] {
            sh.set_var(k, v).map_err(|_| "set failed".to_string())?;
        }
        assert_eq!(eval(&mut sh, "1?b=--n:0").map_err(|_| "eval")?, 0);
        assert_eq!(sh.get_var("n").as_deref(), Some("0"));
        assert_eq!(sh.get_var("b").as_deref(), Some("0"));
        Ok(())
    }

    #[test]
    fn a_unary_plus_is_discarded_rather_than_stacked() -> Result<(), String> {
        // busybox drops a unary plus instead of stacking it, so there is nothing
        // for a later operator to apply and the name stays on top: `+a+--1` is
        // refused where `-a+--1`, whose sign really is applied, answers. Only
        // `+` is dropped -- the other three unary operators all reduce.
        for src in ["+a+--1", "+a*--1"] {
            assert_eq!(arith_fixture(src)?, None, "{src}");
        }
        for (src, want) in [("-a+--1", -1), ("!a+--1", 1), ("~a+--1", -2)] {
            assert_eq!(arith_fixture(src)?, Some(want), "{src}");
        }
        Ok(())
    }

    #[test]
    fn the_precedence_mirror_is_pinned_level_by_level() -> Result<(), String> {
        // The mirror decides which pending operators an incoming one applies,
        // and applying any of them clears the name a pair could bind to. So each
        // LEVEL is observable: a row splits where the operator to its left has
        // been reduced and binds backward where it has not. Without these the
        // table is unpinned -- every one of these rows was reachable by a
        // one-token edit that the rest of the suite passed.
        assert_eq!(arith_fixture("a,b,--1")?, Some(1), "`,` below assignment");
        assert_eq!(arith_fixture("a&&b||--1")?, Some(1), "`||` below `&&`");
        // A unary must sit above the TIGHTEST binary level, not merely above
        // `+`: only an operator that binds tighter than `+` can tell 14 from 12,
        // and nothing distinguishes 14 from 13, since no binary level is 14.
        assert_eq!(arith_fixture("-a*--1")?, Some(-2), "unary above `*`");
        // A pending operator BELOW `?` is reduced by it, and a postfix pair
        // completes an operand where a prefix one does not -- both observable
        // only through what the pair after them does.
        assert_eq!(arith_fixture("a<b?--1:0")?, Some(1), "`?` reduces `<`");
        assert_eq!(arith_fixture("a+++--1")?, Some(3), "a postfix ends an operand");
        assert_eq!(arith_fixture("a---++1")?, Some(1), "and so does a postfix `--`");
        for (src, why) in [
            ("a+b*+--1", "an operator does not end one"),
            ("a=b=--1", "assignment is right-associative"),
            ("a,b=--1", "assignment above `,`"),
            ("a=b?--1:0", "`?` above assignment"),
            ("a?b||--1:0", "`?` opens a region `||` cannot leave"),
            ("a|b^--1", "`^` above `|`"),
            ("a==b<--1", "relational above equality"),
            ("a<<b+--1", "additive above shift"),
            ("a*b++--1", "a bound pair displaces nothing"),
        ] {
            assert_eq!(arith_fixture(src)?, None, "{src}: {why}");
        }
        Ok(())
    }

    #[test]
    fn a_compound_assignment_reads_its_lvalue_first() -> Result<(), String> {
        // ash captures the lvalue BEFORE evaluating the right-hand side, so an
        // rhs that steps or assigns the same name is not what the operator
        // accumulates onto. Reading it afterwards made `b+=b++` answer 7 where
        // ash answers 6 -- a wrong VALUE rather than a refusal, and one this
        // shell could not even express until `++` existed. The last row is the
        // same defect without an increment, which is why it is a fix here and
        // not a consequence of the rest of this commit.
        for (src, want, after) in [
            ("b+=b++", 6, "6"),
            ("b|=b++", 3, "3"),
            ("b-=b--", 0, "0"),
            ("b=b++", 3, "3"),
            ("b+=(b=1)", 4, "4"),
        ] {
            let mut sh = Shell::new_for_test();
            sh.set_var("b", "3").map_err(|_| "set failed".to_string())?;
            assert_eq!(eval(&mut sh, src).map_err(|_| format!("eval {src}"))?, want, "{src}");
            assert_eq!(sh.get_var("b").as_deref(), Some(after), "{src}");
        }
        Ok(())
    }

    #[test]
    fn a_dead_conditional_arm_is_parsed_like_a_live_one() -> Result<(), String> {
        // A DELIBERATE divergence, and one rule seen from both sides: a name
        // pushed while evaluation is disabled carries no `var_name` in busybox
        // (math.c:748), so inside an untaken arm its pairs can only bind
        // forward. That makes ash accept the first two rows and refuse the
        // third; this shell parses both arms alike and does the opposite of ash
        // on each. Copying it would mean copying the refusal too, since they are
        // the same rule -- and the value of an untaken arm is discarded, so all
        // that is at stake is whether it parses.
        for src in ["1?0:m--1", "0?m--1:0", "1?0:b+--1", "0?b=--1:0", "1?0:m--(n)"] {
            assert_eq!(arith_fixture(src)?, None, "{src}");
        }
        for src in ["1?0:m++", "0?m--:0"] {
            assert_eq!(arith_fixture(src)?, Some(0), "{src}");
        }
        // The same shapes in a LIVE arm, where the two shells agree on every one
        // measured -- which is what makes the rows above about deadness alone.
        for src in ["0?0:m--1", "1?m--1:0", "0?0:b+--1", "1?b=--1:0"] {
            assert_eq!(arith_fixture(src)?, None, "{src}");
        }
        Ok(())
    }

    #[test]
    fn a_dead_branch_does_not_step() -> Result<(), String> {
        // The untaken `?:` arm must not store: `live` is what stops it, and the
        // read is a side effect of its own for a dynamic name. The start value
        // is 5 rather than 1 because a dead READ answers 0, so a dead step would
        // store 1 -- which from 1 is indistinguishable from not storing at all.
        for (src, want, after) in [("n ? 0 : n++", 0, "5"), ("0 ? ++n : 7", 7, "5")] {
            let mut sh = Shell::new_for_test();
            sh.set_var("n", "5").map_err(|_| "set failed".to_string())?;
            assert_eq!(eval(&mut sh, src).map_err(|_| format!("eval {src}"))?, want);
            assert_eq!(sh.get_var("n").as_deref(), Some(after), "{src}");
        }
        // `&&`/`||` short-circuit through `live` ALONE -- `ternary_dead` stays
        // false there, so a guard keyed on the other flag passes every row above
        // and still steps here. (ash steps in both; not short-circuiting side
        // effects at all is its behaviour, and this shell's divergence.)
        for (src, want) in [("0 && n++", 0), ("1 || n++", 1)] {
            let mut sh = Shell::new_for_test();
            sh.set_var("n", "5").map_err(|_| "set failed".to_string())?;
            assert_eq!(eval(&mut sh, src).map_err(|_| format!("eval {src}"))?, want);
            assert_eq!(sh.get_var("n").as_deref(), Some("5"), "{src}");
        }
        Ok(())
    }

    #[test]
    fn a_variables_value_must_be_a_number_not_an_expression() -> Result<(), String> {
        let mut sh = Shell::new_for_test();
        for (name, value) in [("b", "5"), ("a", "b"), ("e", "1+2"), ("blank", " ")] {
            sh.set_var(name, value)
                .map_err(|_| "set failed".to_string())?;
        }
        // `a` holds a name and `e` an expression; neither is evaluated further.
        assert!(eval(&mut sh, "a").is_err());
        assert!(eval(&mut sh, "e + 3").is_err());
        assert_eq!(eval(&mut sh, "blank").map_err(|_| "eval")?, 0);
        assert_eq!(eval(&mut sh, "b + 1").map_err(|_| "eval")?, 6);
        Ok(())
    }

    #[test]
    fn a_names_value_parses_with_the_c_prefix_rules() {
        assert_eq!(strtoimax(""), Some(0));
        assert_eq!(strtoimax("  "), Some(0));
        assert_eq!(strtoimax("0"), Some(0));
        assert_eq!(strtoimax(" \t 42 "), Some(42)); // blanks around it are skipped
        assert_eq!(strtoimax("4 2"), None); // blanks inside it are not
        assert_eq!(strtoimax("0x1f"), Some(31));
        assert_eq!(strtoimax("-0x1f"), Some(-31));
        assert_eq!(strtoimax("010"), Some(8));
        assert_eq!(strtoimax("08"), None); // 8 is not an octal digit
        assert_eq!(strtoimax("0x"), None); // the prefix alone converts nothing
        assert_eq!(strtoimax("-9223372036854775808"), Some(i64::MIN));
        assert_eq!(strtoimax("9223372036854775808"), None);
        assert_eq!(strtoimax("1+2"), None);
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
        // A name whose value is not a number errors if evaluated; the dead side of
        // `||`/`&&`/`?:` must skip it and contribute 0.
        sh.set_var("x", "x").map_err(|_| "set failed".to_string())?;
        assert_eq!(eval(&mut sh, "1 || x").map_err(|_| "eval")?, 1);
        assert_eq!(eval(&mut sh, "0 && x").map_err(|_| "eval")?, 0);
        assert_eq!(eval(&mut sh, "1 ? 2 : x").map_err(|_| "eval")?, 2);
        Ok(())
    }
}
