//! Arithmetic expansion: `$(( ... ))`.
//!
//! The full POSIX operator set over `i64`, including assignment and the ternary.
//! A name's value is itself an EXPRESSION, evaluated recursively as ash does, so
//! `e=1+2; echo $((e+3))` is 6 and a name reached from its own value is refused
//! rather than followed.

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
        toks: Vec::new(),
        pos: 0,
        pdepth: 0,
        live: true,
        resolving: Vec::new(),
    };
    p.eval_string(sh, text)
}

#[derive(Clone, Debug)]
enum Tk {
    Num(i64),
    Name(String),
    Op(&'static str),
}

/// Multi-character operators, longest first so `<<=` beats `<<` beats `<`.
const OPS: &[&str] = &[
    "<<=", ">>=", "**", "&&", "||", "<<", ">>", "<=", ">=", "==", "!=", "+=", "-=",
    "*=", "/=", "%=", "&=", "^=", "|=", "+", "-", "*", "/", "%", "<", ">", "!", "~",
    "&", "^", "|", "?", ":", "(", ")", "=", ",",
];

const PREC_UNARY: u8 = 15;

/// `**` binds TIGHTER than `*` and LOOSER than a unary sign, which is why the
/// scale needs a level between them: ash reads `-2**2` as `(-2)**2` -- 4, where
/// Python reads `-(2**2)` and gives -4. Right-associative, so `2**3**2` is 512.
const PREC_EXPONENT: u8 = 14;

/// Where a COMPLETED conditional waits. Right-associative, as `**` is.
const PREC_ASSIGN: u8 = 2;

/// Marks a region nothing inside may displace out of: a `(`, and a `?`, whose
/// middle expression busybox brackets with an implicit parenthesis.
const BARRIER: u8 = 0;

/// Binding power, mirroring this file's descent so the lexer can tell which
/// pending operators an incoming one displaces. Assignment and `**` are the
/// right-associative levels, which is what keeps `a = b = c` unreduced.
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
        "**" => PREC_EXPONENT,
        _ => PREC_UNARY,
    }
}

/// C `isspace`, not `char::is_whitespace`: ash separates arithmetic tokens with
/// the ASCII six and refuses an NBSP or EM SPACE outright, where Unicode's set
/// would silently read `$((<NBSP>1))` as 1.
fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r')
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
        if is_blank(c) {
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
            while chars.get(k).is_some_and(|n| is_blank(*n)) {
                k += 1;
            }
            let fwd = chars
                .get(k)
                .is_some_and(|n| n.is_ascii_alphabetic() || *n == '_');
            if operand_is_name || fwd {
                // It binds, so it is an operator over a name and is displaced
                // like a unary sign -- PENDING, not applied, so the name stays
                // reachable until something displaces it. Whether it COMPLETES
                // an operand is decided by what came BEFORE it and not by the
                // name it binds: a pair after a value is a postfix and is one,
                // a pair that followed an operator became a prefix and is not
                // (math.c:847-865). So `ends_operand` carries through
                // untouched either way.
                toks.push(Tk::Op(if c == '+' { "++" } else { "--" }));
                pending.push(PREC_UNARY);
                i += 2;
                continue;
            }
            // Otherwise busybox emits ONE sign and rescans from the second
            // (math.c:795-798, `expr++; goto tok_found1`). That IS the ordinary
            // single-character match below -- `++` is not in `OPS` at all -- so
            // this falls through to it and the half meets the same unary
            // handling as any other sign, including being discarded when it is
            // one.
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
            return Err(SYNTAX.into());
        };
        let mut discard = false;
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
                let right = p == PREC_ASSIGN || p == PREC_EXPONENT;
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
                } else if unary && op == "+" {
                    // busybox DISCARDS a unary plus rather than stacking it, so
                    // nothing later can apply it: `+a+--1` still holds `a` and is
                    // refused, where `-a+--1`, whose sign IS applied, splits.
                    // It emits no TOKEN either, which is what leaves the name
                    // behind one reachable: `+n=5` assigns.
                    discard = true;
                } else {
                    pending.push(if unary { PREC_UNARY } else { p });
                }
                ends_operand = false;
            }
        }
        if !discard {
            toks.push(Tk::Op(op));
        }
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

/// ash's catch-all: EVERY arithmetic parse failure is this one string
/// (math.c:501, 1000), whatever actually went wrong.
const SYNTAX: &str = "arithmetic syntax error";

/// A `:` APPLIED with no `?` pending (math.c:342, 945) -- `$((1:2))`. NOT a
/// `:` where a value was expected: `$((:2))` is the catch-all, and so is
/// `$((1:))`, whose colon never gets a right operand to be applied to.
/// math.c:350 is the mirror -- a `?` forced to apply without its colon,
/// `$(((1?2)))` -- and `stopped_at` does not serve it; see there.
const MALFORMED_TERNARY: &str = "malformed ?: operator";

/// A digit's value in an explicit base, or None where busybox's loop BREAKS
/// (math.c:551-595). The mapping is `| 0x20` arithmetic on the byte rather than
/// a table, because which characters are digits is a consequence of it: above
/// base 36 the letters split into `a-z` at 10-35 and `A-Z` at 36-61, so `64#z`
/// is 35 and `64#Z` is 61, and only `@` and `_` are named outright.
fn base_digit(c: char, base: u32) -> Option<u32> {
    let ch = u32::from(c);
    // Below `'0'` C's unsigned subtraction wraps past every base, which is how a
    // `+` or a blank ends the run rather than being read as a digit.
    let mut digit = ch.checked_sub(u32::from('0'))?;
    if digit >= 10 {
        if digit > u32::from('z') - u32::from('0') {
            return None;
        }
        digit = (ch | 0x20).checked_sub(u32::from('a') - 10)?;
        if base > 36 && ch <= u32::from('_') {
            digit = match c {
                '_' => 63,
                '@' => 62,
                _ if digit < 36 => digit + (36 - 10),
                // `[\]^`, which land between `Z` and `_` and name nothing.
                _ => return None,
            };
        }
        // What arrives here below 10 is `` ` ``, and `@` in the bases that did
        // not just name it 62. `:;<=>?` never reach it at all: the fold above
        // lands under `'a' - 10` for them and returns.
        if digit < 10 {
            return None;
        }
    }
    if digit >= base { None } else { Some(digit) }
}

/// `BASE#DIGITS` once the base is known. Out of range WRAPS, as everywhere else
/// here: busybox does not check, so `16#ffffffffffffffff` is -1.
fn lex_with_base(chars: &[char], start: usize, base: u32) -> Result<(i64, usize), String> {
    let mut i = start;
    let mut value: u64 = 0;
    while let Some(&c) = chars.get(i) {
        let Some(d) = base_digit(c, base) else { break };
        value = value
            .wrapping_mul(u64::from(base))
            .wrapping_add(u64::from(d));
        i += 1;
    }
    // A base with no digits after it is the error bash 5.2 made of `64#`, and
    // it is also how a digit the base lacks surfaces: the run stops before the
    // first one, so `2#7` reaches here having consumed nothing.
    if i == start {
        return Err(SYNTAX.into());
    }
    Ok((value as i64, i))
}

/// The explicit base of a `BASE#` prefix, and where its digits start. One or two
/// digits only, and NOT a leading `0`, which stays the octal prefix -- ash reads
/// the base off `nptr[0]`, so `0#1` never reaches this at all (math.c:597-635).
/// A base outside 2..=64 is None rather than an error: ash falls back to reading
/// the digits as decimal (math.c:634), which leaves the `#` as trailing input,
/// so `1#1` and `65#1` are refused a step later instead.
fn explicit_base(chars: &[char], start: usize) -> Option<(u32, usize)> {
    let d0 = chars.get(start)?.to_digit(10)?;
    if d0 == 0 {
        return None;
    }
    if chars.get(start + 1) == Some(&'#') {
        return if d0 > 1 { Some((d0, start + 2)) } else { None };
    }
    let d1 = chars.get(start + 1)?.to_digit(10)?;
    if chars.get(start + 2) != Some(&'#') {
        return None;
    }
    let base = d0 * 10 + d1;
    if base <= 64 { Some((base, start + 3)) } else { None }
}

/// `BASE#DIGITS`, `0x` hex, leading-`0` octal, else decimal — the C conventions
/// POSIX inherits, plus ash's explicit base.
fn lex_number(chars: &[char], start: usize) -> Result<(i64, usize), String> {
    if let Some((base, at)) = explicit_base(chars, start) {
        return lex_with_base(chars, at, base);
    }
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
        return Err(SYNTAX.into());
    }
    // Out of range WRAPS, which is the only way `-9223372036854775808` can be
    // written: the bound is unreachable as a positive literal. A digit the base
    // lacks is still an error, which keeps `08` from lexing as 8.
    let mut value: u64 = 0;
    for c in digits.chars() {
        let Some(d) = c.to_digit(radix) else {
            return Err(SYNTAX.into());
        };
        value = value
            .wrapping_mul(u64::from(radix))
            .wrapping_add(u64::from(d));
    }
    Ok((value as i64, i))
}

struct Arith {
    toks: Vec<Tk>,
    pos: usize,
    /// Parenthesis-nesting depth, bounded so `((((…))))` errors instead of
    /// overflowing the recursive-descent stack.
    pdepth: u32,
    /// False inside the untaken arm of a `?:`, which is the ONLY thing ash
    /// leaves unevaluated -- busybox's `evaluation_disabled`, counted from the
    /// conditional alone (math.c:951-991). The tokens are still consumed, so
    /// the walk continues; it just reads no name, assigns nothing, and does not
    /// divide by zero on the way through.
    live: bool,
    /// The names whose values are being evaluated, innermost last. A name that
    /// reaches itself is refused here rather than recursed into: `a=b; b=a` is
    /// otherwise unbounded, and the cycle can be any length.
    resolving: Vec<String>,
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

    /// Which of ash's two parse messages a stop point earns. ash's `:` is a
    /// binary operator, so it says `malformed ?: operator` where the colon was
    /// APPLIED -- a value either side and no `?` pending. Where it never got a
    /// right operand (`$((1:))`) the expression simply ended badly and it is
    /// the catch-all, as it is for a colon sitting where a value was expected
    /// -- that one never reaches here, being refused as a primary.
    ///
    /// An APPROXIMATION of ash, and knowingly. ash reduces a shunting yard, so
    /// which message it reaches is an evaluation ORDER; this descends, and can
    /// only ask what the tokens look like. Three shapes therefore still differ
    /// and the commit message enumerates them: a `?` forced to apply without
    /// its colon, two juxtaposed values before one, and a right-hand side whose
    /// own error ash reports first (`$((1:1/0))` is `divide by zero` there).
    fn stopped_at(&self) -> String {
        if !matches!(self.peek(), Some(Tk::Op(":"))) {
            return SYNTAX.into();
        }
        // "A token follows" is not the question -- `$((1:*))` has one and ash
        // still says the catch-all, because `*` cannot BEGIN the right operand
        // and so the colon is never applied. A prefix run either reaches a
        // value or it does not: `1:+2` is applied where `1:+` is not, so the
        // unary operators are stepped over rather than counted. Asked of the
        // TOKENS, never by parsing: `$((1:n=5))` would assign on the way to
        // deciding which message to print.
        let mut i = self.pos + 1;
        while matches!(self.toks.get(i), Some(Tk::Op("-" | "+" | "!" | "~" | "++" | "--"))) {
            i += 1;
        }
        match self.toks.get(i) {
            Some(Tk::Num(_) | Tk::Name(_) | Tk::Op("(")) => MALFORMED_TERNARY.into(),
            _ => SYNTAX.into(),
        }
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

    /// One whole expression, over its own token list. The parse state is swapped
    /// rather than a second parser built, so the depth bound and the
    /// enabled/disabled flags carry inward as ash's shared `math_state` does.
    fn eval_string(&mut self, sh: &mut Shell, text: &str) -> A<i64> {
        // Here rather than at the lookup because a compound assignment reads its
        // lvalue without descending through `expr_unary`.
        self.enter()?;
        let value = self.eval_string_inner(sh, text);
        self.leave();
        value
    }

    fn eval_string_inner(&mut self, sh: &mut Shell, text: &str) -> A<i64> {
        let toks = lex(text)?;
        // A null expression is 0, not a syntax error -- `$(( ))`, and the empty
        // or all-blank value that motivates it. Asked of the SOURCE rather than
        // of the tokens, because a discarded unary plus leaves none either and
        // `$((+++))` is an error in ash: something was written, and it was not
        // a value.
        if text.chars().all(is_blank) {
            return Ok(0);
        }
        let saved_toks = std::mem::replace(&mut self.toks, toks);
        let saved_pos = std::mem::replace(&mut self.pos, 0);
        let value = self.expr_comma(sh).and_then(|v| {
            if self.peek().is_some() {
                return Err(self.stopped_at());
            }
            Ok(v)
        });
        self.toks = saved_toks;
        self.pos = saved_pos;
        value
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
                    "+=" => cur.wrapping_add(rhs),
                    "-=" => cur.wrapping_sub(rhs),
                    "*=" => cur.wrapping_mul(rhs),
                    "/=" => divide(live, cur, rhs)?,
                    "%=" => remainder(live, cur, rhs)?,
                    "<<=" => cur.wrapping_shl(shift_count(rhs)),
                    ">>=" => cur.wrapping_shr(shift_count(rhs)),
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
            return Err(SYNTAX.into());
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
            let rhs = self.expr_and(sh)?;
            v = i64::from(v != 0 || rhs != 0);
        }
        Ok(v)
    }

    fn expr_and(&mut self, sh: &mut Shell) -> A<i64> {
        let mut v = self.expr_bitor(sh)?;
        while self.eat_op("&&") {
            let rhs = self.expr_bitor(sh)?;
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
                v.wrapping_shl(shift_count(rhs))
            } else {
                v.wrapping_shr(shift_count(rhs))
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
                v.wrapping_add(rhs)
            } else {
                v.wrapping_sub(rhs)
            };
        }
    }

    fn expr_mul(&mut self, sh: &mut Shell) -> A<i64> {
        let mut v = self.expr_exponent(sh)?;
        loop {
            let op = match self.peek_op() {
                Some(o @ ("*" | "/" | "%")) => o,
                _ => return Ok(v),
            };
            self.pos += 1;
            let rhs = self.expr_exponent(sh)?;
            v = match op {
                "*" => v.wrapping_mul(rhs),
                "/" => divide(self.live, v, rhs)?,
                _ => remainder(self.live, v, rhs)?,
            };
        }
    }

    /// Right-associative, so the recursion is on the RIGHT: `2**3**2` is
    /// `2**(3**2)`. Its left operand is a unary chain rather than another
    /// exponent, which is what makes `-2**2` the square of `-2`.
    fn expr_exponent(&mut self, sh: &mut Shell) -> A<i64> {
        let base = self.expr_unary(sh)?;
        if self.peek_op() != Some("**") {
            return Ok(base);
        }
        self.pos += 1;
        self.enter()?;
        let exp = self.expr_exponent(sh);
        self.leave();
        let exp = exp?;
        power(self.live, base, exp)
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
                Ok(v.wrapping_neg())
            }
            // Reached by the BACKWARD-bound pair `expr_primary` did not
            // take, after which ANYTHING may follow: `n*++1` and `n*++-n`
            // arrive here with no name in the expression at all, which is why
            // it stays fallible. A unary plus between the pair and its name is
            // not a token at all, so `n*++ +n` needs nothing special here.
            Some(o @ ("++" | "--")) => {
                let Some(Tk::Name(name)) = self.toks.get(self.pos + 1).cloned() else {
                    return Err(SYNTAX.into());
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
                    return Err(self.stopped_at());
                }
                Ok(v)
            }
            Some(Tk::Op(_)) => Err(SYNTAX.into()),
            None => Err(SYNTAX.into()),
        }
    }

    /// One `++`/`--` step, returning the value before and after it.
    fn step(&mut self, sh: &mut Shell, name: &str, up: bool) -> A<(i64, i64)> {
        let cur = self.name_value(sh, name)?;
        let new = if up { cur.wrapping_add(1) } else { cur.wrapping_sub(1) };
        if self.live {
            sh.set_var(name, &new.to_string())
                .map_err(|_| format!("{name}: is read only"))?;
        }
        Ok((cur, new))
    }

    /// A variable's value, evaluated as an expression in its own right.
    fn name_value(&mut self, sh: &mut Shell, name: &str) -> A<i64> {
        // The value is unused here and READING is a side effect for a dynamic
        // name, so the untaken `?:` branch must not reach the lookup at all.
        if !self.live {
            return Ok(0);
        }
        let Some(text) = crate::expand::var_value(sh, name) else {
            return Ok(0);
        };
        // The guard is asked BEFORE the shortcut below, because a name can be
        // reassigned to a plain number WHILE it is being resolved: `a=b;
        // b="a=5,a"` reaches `a` again holding `5`, which the shortcut would
        // answer for and ash reports the loop for.
        if self.resolving.iter().any(|n| n == name) {
            return Err("expression recursion loop detected".into());
        }
        if let Some(n) = plain_decimal(&text) {
            return Ok(n);
        }
        self.resolving.push(name.to_string());
        let value = self.eval_string(sh, &text);
        self.resolving.pop();
        value
    }
}

/// A value that is a plain decimal integer, which the full evaluator reaches
/// through a lex and a parse. An operand read is the inner loop of `while [ $i
/// -lt N ]; do i=$((i+1)); done`, and every one of them allocated twice.
///
/// A leading zero is refused because it would be octal, a value outside `i64`
/// because a literal that wide wraps and only the evaluator knows how, and
/// anything else -- blanks, `0x`, an operator -- because the evaluator reads
/// those. A sign is taken: ash applies unary minus and DISCARDS unary plus.
/// `the_fast_path_agrees_with_the_evaluator` is what holds the two together.
fn plain_decimal(text: &str) -> Option<i64> {
    let (negative, digits) = match text.as_bytes() {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        rest => (false, rest),
    };
    if digits.is_empty() || (digits.len() > 1 && digits.first() == Some(&b'0')) {
        return None;
    }
    let mut value: i64 = 0;
    for b in digits {
        let d = b.checked_sub(b'0').filter(|d| *d < 10)?;
        value = value.checked_mul(10)?.checked_add(i64::from(d))?;
    }
    Some(if negative { -value } else { value })
}

/// `**`. The loop is busybox's own (math.c:420-438) and not `pow`, because it
/// must WRAP at every step the same way -- `2**63` is `i64::MIN` and `2**64` is
/// 0 -- and because halving the exponent is what stops `3**999999999999999999`
/// running forever. A negative exponent is refused as ash refuses it, and that
/// guard also TERMINATES this loop: halving a negative and decrementing it
/// returns where it started, so `exp` would alternate between -1 and -2.
fn power(live: bool, base: i64, exp: i64) -> A<i64> {
    if exp < 0 {
        return if live {
            Err("exponent less than 0".into())
        } else {
            Ok(0)
        };
    }
    let (mut base, mut exp, mut acc) = (base, exp, 1i64);
    while exp != 0 {
        if exp & 1 == 0 {
            base = base.wrapping_mul(base);
            exp >>= 1;
        }
        acc = acc.wrapping_mul(base);
        exp -= 1;
    }
    Ok(acc)
}

/// The two that fail on a zero divisor -- `power` above is the third fallible
/// operator and refuses for its own reason. All three fold to 0 inside an
/// untaken `?:` arm, the one region ash leaves unevaluated, rather than
/// reporting an error the shell would never have hit. `i64::MIN / -1` wraps
/// rather than trapping.
fn divide(live: bool, a: i64, b: i64) -> A<i64> {
    if b == 0 {
        return if live {
            Err("divide by zero".into())
        } else {
            Ok(0)
        };
    }
    Ok(a.wrapping_div(b))
}

fn remainder(live: bool, a: i64, b: i64) -> A<i64> {
    if b == 0 {
        return if live {
            Err("divide by zero".into())
        } else {
            Ok(0)
        };
    }
    Ok(a.wrapping_rem(b))
}

/// ash masks the shift COUNT rather than rejecting it, so `1<<64` is 1 and a
/// negative count is its low bits: `1<<-1` is `1<<63`. Masking first is what
/// makes that true of a NEGATIVE `b`, whose sign bit `& 63` clears.
fn shift_count(b: i64) -> u32 {
    (b & 63) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(src: &str) -> Result<i64, String> {
        let mut sh = Shell::new_for_test();
        eval(&mut sh, src).map_err(|_| format!("evaluation of {src:?} failed"))
    }

    /// `ev` throws the message away; this keeps it. Which words a failure
    /// carries is the whole of what ash is being matched on below.
    fn ev_msg(src: &str) -> Result<i64, String> {
        let mut sh = Shell::new_for_test();
        try_eval(&mut sh, src)
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
        // Only the untaken `?:` arm is unevaluated, so only IT swallows the
        // error its operand would raise -- see the logical-operator rows.
        assert_eq!(ev("1 ? 2 : 1/0")?, 2);
        assert_eq!(ev("0 ? 1/0 : 2")?, 2);
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
    fn an_explicit_base_reads_its_own_digits() -> Result<(), String> {
        assert_eq!(ev("2#101")?, 5);
        assert_eq!(ev("16#ff")?, 255);
        assert_eq!(ev("64#1")?, 1);
        // An explicit base makes a leading zero an ordinary digit, where the
        // same characters bare would take it as the octal prefix.
        assert_eq!(ev("10#017")?, 17);
        // To 36 the letters are case-INSENSITIVE. Above it they split into two
        // ranges, so `z` stays 35 while `Z` becomes 61, and `@`/`_` are the two
        // digits that are nobody's other case. 36 and 37 are the boundary.
        assert_eq!(ev("36#z")?, 35);
        assert_eq!(ev("36#Z")?, 35);
        assert_eq!(ev("36#A")?, 10);
        assert_eq!(ev("37#A")?, 36);
        assert_eq!(ev("64#a")?, 10);
        assert_eq!(ev("64#z")?, 35);
        assert_eq!(ev("64#A")?, 36);
        assert_eq!(ev("64#Z")?, 61);
        assert_eq!(ev("64#@")?, 62);
        assert_eq!(ev("64#_")?, 63);
        assert_eq!(ev("64#__")?, 4095);
        // The run ends at the first character the base has no digit for, so a
        // literal COMPOSES rather than swallowing the operator after it.
        assert_eq!(ev("16#ff+1")?, 256);
        assert_eq!(ev("2#1+1")?, 2);
        // Out of range wraps, as every other literal here does.
        assert_eq!(ev("16#ffffffffffffffff")?, -1);
        Ok(())
    }

    #[test]
    fn a_base_with_no_digit_of_its_own_is_refused() -> Result<(), String> {
        // A digit the base LACKS ends the run before consuming anything, so
        // these have no digits at all rather than a truncated value. `63#_` and
        // `62#@` are that rule reaching the two named digits, and `64#[` is the
        // gap between `Z` and `_`, which names nothing in any base.
        //
        // `36#@`, `64#{` and `64#:` are the three ways a character can reach the
        // digit machinery and still not be one, and each is a different exit:
        // `@` folds to 9, below where the letters start; `{` is past `z`, so the
        // fold is never applied; and `:` is the boundary itself, the one
        // character whose RAW value is exactly 10 and whose fold underflows.
        // Each is a value rather than an error if its guard goes.
        for src in [
            "2#2", "8#8", "35#z", "63#_", "62#@", "64#[", "36#@", "64#{", "64#:", "2#+1", "2# 1",
            "2#",
        ] {
            assert!(ev(src).is_err(), "{src}");
        }
        // Only 2..=64 is a base. Outside it ash reads the digits as DECIMAL
        // instead and the `#` is left over, so the refusal comes a step later:
        // that is why `1#0` is an error rather than 0, and `05#4` is not 4 --
        // a leading `0` is the octal prefix and never a base. `1a#5` is the
        // same fallback reached the other way: a two-digit base's second slot
        // must be a DECIMAL digit, so `a` is not one and `1a` is read as a
        // number rather than as base 26.
        for src in ["1#0", "1#1", "65#10", "66#1", "0#0", "05#4", "01#1", "1a#5", "1F#5"] {
            assert!(ev(src).is_err(), "{src}");
        }
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
    fn a_discarded_plus_leaves_the_name_behind_it_assignable() -> Result<(), String> {
        let ev = |src: &str| -> Result<(i64, String), String> {
            let mut sh = Shell::new_for_test();
            sh.set_var("n", "2").map_err(|_| "set failed".to_string())?;
            let v = eval(&mut sh, src).map_err(|_| format!("eval {src}"))?;
            Ok((v, sh.get_var("n").unwrap_or_default()))
        };
        // A unary plus is no TOKEN, so what follows one is still an lvalue.
        assert_eq!(ev("+n=5")?, (5, "5".into()));
        assert_eq!(ev("+ n=5")?, (5, "5".into()));
        assert_eq!(ev("+ + n=5")?, (5, "5".into()));
        assert_eq!(ev("+n+=1")?, (3, "3".into()));
        assert_eq!(ev("+n*=2")?, (4, "4".into()));
        assert_eq!(ev("-(+n=5)")?, (-5, "5".into()));
        // Every OTHER unary stacks, so the name under it is a value by the time
        // the assignment wants one, and each of these is refused. Only the
        // refusal is pinned: `++n=5` steps before it fails, and what a REFUSED
        // expression left behind is not observable in ash, which exits on it.
        for src in ["-n=5", "!n=5", "~n=5", "(+n)=5", "++n=5", "+++n=5", "-+n=5"] {
            let mut sh = Shell::new_for_test();
            sh.set_var("n", "2").map_err(|_| "set failed".to_string())?;
            assert!(eval(&mut sh, src).is_err(), "{src}");
        }
        // Discarding every token is NOT a null expression: `$(( ))` is 0
        // because nothing was written, and these wrote something that was not
        // a value. The rule reads the source, so the two cannot be confused.
        for src in ["+", "++", "+++", "++ +", "+ +"] {
            let mut sh = Shell::new_for_test();
            assert!(eval(&mut sh, src).is_err(), "{src}");
        }
        assert_eq!(ev("")?, (0, "2".into()));
        assert_eq!(ev("   ")?, (0, "2".into()));
        Ok(())
    }

    #[test]
    fn a_prefix_pair_steps_the_name_behind_a_discarded_plus() -> Result<(), String> {
        let ev = |src: &str| -> Result<(i64, String, String), String> {
            let mut sh = Shell::new_for_test();
            sh.set_var("n", "2").map_err(|_| "set failed".to_string())?;
            sh.set_var("m", "5").map_err(|_| "set failed".to_string())?;
            let v = eval(&mut sh, src).map_err(|_| format!("eval {src}"))?;
            Ok((
                v,
                sh.get_var("n").unwrap_or_default(),
                sh.get_var("m").unwrap_or_default(),
            ))
        };
        // A unary plus is DISCARDED rather than stacked, so a prefix pair steps
        // the name behind any number of them. Every row here has a name and a
        // PENDING operator to its left, which is the only place the pair
        // survives as a prefix: with nothing to bind backward to it splits
        // instead, and the last two rows are that case and must not move.
        assert_eq!(ev("n*++ +n")?, (6, "3".into(), "5".into()));
        assert_eq!(ev("n*+++n")?, (6, "3".into(), "5".into()));
        assert_eq!(ev("n*+++m")?, (12, "2".into(), "6".into()));
        assert_eq!(ev("n*++ + +m")?, (12, "2".into(), "6".into()));
        assert_eq!(ev("n*--+n")?, (2, "1".into(), "5".into()));
        assert_eq!(ev("n-+++n")?, (-1, "3".into(), "5".into()));
        assert_eq!(ev("n,+++n")?, (3, "3".into(), "5".into()));
        assert_eq!(ev("n=+++n")?, (3, "3".into(), "5".into()));
        // ONLY a `+` vanishes. Every other unary stacks, so the pair lands
        // on a value rather than a name -- as it does on a number, a
        // parenthesis or a second pair, none of which is one. The state is
        // asserted beside the refusal because reaching one of these would step
        // the name and THEN fail, which `is_err` alone cannot tell from this.
        // The last two are the ones that need a pair, then a LONE `+`, then a
        // second pair: the lone one is binary only if the first pair completed
        // an operand, and a prefix pair does not. Get that wrong and the second
        // pair splits into signs that vanish, and this steps `n` and answers 6
        // -- the shortest shape that tells the two rules apart.
        for src in [
            "n*++-n", "n*--    -n", "n*++!n", "n*++~n", "n*++ ++n", "n*++++n",
            "n*++ +1", "n*++(n)", "n*+++ ++ +n", "n*-- + ++ +n",
        ] {
            let mut sh = Shell::new_for_test();
            sh.set_var("n", "2").map_err(|_| "set failed".to_string())?;
            assert!(eval(&mut sh, src).is_err(), "{src}");
            assert_eq!(sh.get_var("n").as_deref(), Some("2"), "{src}");
        }
        // Where the pair can split it still does, and a split one steps
        // NOTHING. `1*` is the same shape with a number, which is not a name.
        assert_eq!(ev("++ +n")?, (2, "2".into(), "5".into()));
        assert_eq!(ev("1*++ +n")?, (2, "2".into(), "5".into()));
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
        // Stepping past the bound wraps, as every operator here does.
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
        // every value but the bound -- only i64::MIN tells the two apart, and
        // since arithmetic wraps it tells them apart by ANSWER rather than by
        // which of them overflows.
        let mut sh = Shell::new_for_test();
        sh.set_var("n", &i64::MIN.to_string()).map_err(|_| "set failed".to_string())?;
        assert_eq!(eval(&mut sh, "1--(n)/2").map_err(|_| "eval")?, 4611686018427387905);
        assert_eq!(eval(&mut sh, "1 - -(n)/2").map_err(|_| "eval")?, 4611686018427387905);
        assert_eq!(eval(&mut sh, "1 - (-(n/2))").map_err(|_| "eval")?, -4611686018427387903);
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
        // `+`: only an operator binding tighter than `+` can tell it from `+`'s
        // level. `*` alone is no longer enough, since `**` sits between `*` and
        // unary -- the second row is what pins unary ABOVE `**` rather than
        // level with it, and it is the only one of the two that can.
        assert_eq!(arith_fixture("-a*--1")?, Some(-2), "unary above `*`");
        assert_eq!(arith_fixture("-a**--1")?, Some(-2), "unary above `**`");
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
        Ok(())
    }

    #[test]
    fn exponentiation_binds_tighter_than_multiply_and_looser_than_a_sign()
    -> Result<(), String> {
        let mut sh = Shell::new_for_test();
        let ev = |sh: &mut Shell, src: &str| {
            try_eval(sh, src).map_err(|e| format!("eval {src}: {e}"))
        };
        for (src, want) in [
            ("2**3", 8),
            ("2**0", 1),
            ("0**0", 1),
            // Right-associative, so the RIGHT pair reduces first.
            ("2**3**2", 512),
            ("(2**3)**2", 64),
            ("2**2**2**2", 65_536),
            ("2**2**0", 2),
            // Tighter than `*`, either side of it.
            ("2*3**2", 18),
            ("3**2*2", 18),
            // Looser than every unary, which is where ash parts from C and
            // Python: the sign is part of the BASE, so this is `(-2)**2`.
            ("-2**2", 4),
            ("-(2**2)", -4),
            ("~2**2", 9),
            ("!0**2", 1),
            ("2**+2", 4),
            ("(-2)**3", -8),
            // Every step wraps, so the bound is reachable and passable.
            ("2**62", 4_611_686_018_427_387_904),
            ("2**63", i64::MIN),
            ("2**64", 0),
            ("3**40", -6_289_078_614_652_622_815),
            ("10**19", -8_446_744_073_709_551_616),
            // Halving the exponent is what makes these return at all.
            ("3**999999999999999999", 2_657_844_495_946_263_211),
            ("7**9223372036854775807", 7_905_747_460_161_236_407),
            ("2**1000000", 0),
        ] {
            assert_eq!(ev(&mut sh, src)?, want, "{src}");
        }
        // A negative exponent is refused rather than rounded to zero, and the
        // refusal is the operator's own rather than a parse failure.
        for src in ["2**-1", "2**-2", "0**-1", "1**-2", "2**(1-2)"] {
            let e = try_eval(&mut sh, src).err().ok_or(format!("{src} evaluated"))?;
            assert!(e.contains("exponent less than 0"), "{src}: {e}");
        }
        // Inside the arm the conditional disables, that refusal is not raised,
        // which is the rule the zero divisor follows. A `&&`/`||` operand is
        // NOT such an arm, so the refusal stands there.
        for (src, want) in [("0 ? 2**-1 : 3", 3), ("1 ? 3 : 2**-1", 3)] {
            assert_eq!(ev(&mut sh, src)?, want, "{src}");
        }
        for src in ["0 && 2**-1", "1 || 2**-1"] {
            assert!(try_eval(&mut sh, src).is_err(), "{src}");
        }
        // `**` is one token where it binds and nothing where it does not; ash
        // has no `**=`, so that spelling stays a parse error.
        for src in ["2***3", "2**", "**2", "n**=2", "2*(*3)"] {
            assert!(try_eval(&mut sh, src).is_err(), "{src}");
        }
        // Blanks around it, and a value that holds one.
        assert_eq!(ev(&mut sh, "2** 3")?, 8);
        assert_eq!(ev(&mut sh, "2 **3")?, 8);
        // The lexer's own precedence MIRROR needs `**`'s level and its
        // right-associativity, and neither is reachable through a value: the
        // mirror decides whether an adjacent `++` BINDS. Two `**` in a row
        // displace nothing, so the name is still on top and the pair binds --
        // and is then refused, since `1` is not a name. Two `*` do displace it,
        // so the same characters split into `+` and `+1` and the answer is 4.
        // The pair must not be followed by a name or the forward rule decides
        // instead and both spellings agree.
        let mut sh = shell_with(&[("n", "2")])?;
        assert!(try_eval(&mut sh, "n**n**++1").is_err());
        assert_eq!(ev(&mut sh, "n*n*++1")?, 4);
        assert_eq!(ev(&mut sh, "n/n/++1")?, 1);
        let mut sh = shell_with(&[("v", "2**3")])?;
        assert_eq!(eval(&mut sh, "v+1").map_err(|_| "eval v")?, 9);
        Ok(())
    }

    #[test]
    fn neither_side_of_a_logical_operator_is_skipped() -> Result<(), String> {
        // ash evaluates BOTH sides of `&&` and `||`; the untaken `?:` arm is the
        // only thing it leaves alone. The RESULT is the same boolean under
        // either rule, so nothing but the effect tells them apart -- which is
        // what every row here is: an assignment, a step, or an error.
        for (src, want, m) in [
            ("0 && (m=7)", 0, "7"),
            ("1 || (m=7)", 1, "7"),
            ("0 && (m=7) && (m=8)", 0, "8"),
            // A dead side inside a LIVE conditional arm still runs.
            ("1 ? (0 && (m=7)) : 9", 0, "7"),
            ("0 ? 9 : (0 && (m=7))", 0, "7"),
            // A conditional in the right operand is evaluated, condition first.
            ("0 && ((m=1) ? 3 : 4)", 0, "1"),
            // The RESULT needs BOTH operands, which every row above reaches
            // from one alone: here the LEFT is true and the right decides.
            ("1 && (m=0)", 0, "0"),
            ("1 || (m=0)", 1, "0"),
            // A chain has to keep looping, not stop after the first operator.
            ("1 || (m=7) || (m=8)", 1, "8"),
            ("0 && (m=7) && (m=8)", 0, "8"),
        ] {
            let mut sh = Shell::new_for_test();
            assert_eq!(eval(&mut sh, src).map_err(|_| format!("eval {src}"))?, want, "{src}");
            assert_eq!(sh.get_var("m").as_deref(), Some(m), "{src}");
        }
        // The error an operand raises is raised on either side, which is the
        // half a short-circuiting shell swallows.
        for src in ["0 && 1/0", "1 || 1/0", "0 && 1%0", "0 && (5/0) && 1"] {
            let mut sh = Shell::new_for_test();
            assert!(try_eval(&mut sh, src).is_err(), "{src}");
        }
        // All four zero-divisor sites are inside the arm the conditional
        // disables, not just the bare `/` one: `%` and both compound forms.
        for (src, want) in [("0 ? 1%0 : 3", 3), ("0 ? 1/0 : 3", 3)] {
            let mut sh = Shell::new_for_test();
            assert_eq!(eval(&mut sh, src).map_err(|_| format!("eval {src}"))?, want, "{src}");
        }
        for src in ["0 ? (m/=0) : 3", "0 ? (m%=0) : 3"] {
            let mut sh = Shell::new_for_test();
            sh.set_var("m", "2").map_err(|_| "set failed".to_string())?;
            assert_eq!(eval(&mut sh, src).map_err(|_| format!("eval {src}"))?, 3, "{src}");
            assert_eq!(sh.get_var("m").as_deref(), Some("2"), "{src}");
        }
        // The conditional still disables its untaken arm, both ways round.
        for (src, want) in [("1 ? 2 : (0 && (m=7))", 2), ("0 ? (m=7) && 1 : 2", 2)] {
            let mut sh = Shell::new_for_test();
            assert_eq!(eval(&mut sh, src).map_err(|_| format!("eval {src}"))?, want, "{src}");
            assert_eq!(sh.get_var("m"), None, "{src}");
        }
        // Both operands of a logical operator step, so a repeated one steps
        // twice -- the shape that distinguishes eager from lazy on its own.
        for (src, want) in [("m++ && m++", 0), ("m++ || m++", 1)] {
            let mut sh = Shell::new_for_test();
            sh.set_var("m", "0").map_err(|_| "set failed".to_string())?;
            assert_eq!(eval(&mut sh, src).map_err(|_| format!("eval {src}"))?, want, "{src}");
            assert_eq!(sh.get_var("m").as_deref(), Some("2"), "{src}");
        }
        // A step on either side of one now TAKES, where the untaken `?:` arm
        // above skips it. Same operator, opposite answer.
        for (src, want) in [("0 && m++", 0), ("1 || m++", 1)] {
            let mut sh = Shell::new_for_test();
            sh.set_var("m", "5").map_err(|_| "set failed".to_string())?;
            assert_eq!(eval(&mut sh, src).map_err(|_| format!("eval {src}"))?, want, "{src}");
            assert_eq!(sh.get_var("m").as_deref(), Some("6"), "{src}");
        }
        Ok(())
    }

    /// A shell with the named variables set, for the value-as-expression rows.
    fn shell_with(vars: &[(&str, &str)]) -> Result<Shell, String> {
        let mut sh = Shell::new_for_test();
        for (name, value) in vars {
            sh.set_var(name, value)
                .map_err(|_| format!("set {name} failed"))?;
        }
        Ok(sh)
    }

    #[test]
    fn a_variables_value_is_an_expression_not_a_number() -> Result<(), String> {
        // Every row is a value ash evaluates rather than converts: an operator,
        // a chain of names, a parenthesis, a comma, a C prefix, and blanks.
        for (vars, src, want) in [
            (&[("e", "1+2")][..], "e", 3),
            (&[("e", "1+2")][..], "e + 3", 6),
            (&[("n", "m"), ("m", "5")][..], "n", 5),
            (&[("n", "m+1"), ("m", "5")][..], "n*2", 12),
            (&[("a", "b"), ("b", "c"), ("c", "d"), ("d", "7")][..], "a", 7),
            (&[("n", " 1 + 2 ")][..], "n", 3),
            (&[("n", "(1)")][..], "n", 1),
            (&[("n", "1,2")][..], "n", 2),
            (&[("n", "0x1f")][..], "n", 31),
            (&[("n", "010")][..], "n", 8),
            (&[("n", "-9223372036854775808")][..], "n", i64::MIN),
            // A value naming an UNSET variable is 0, which is why ash answers
            // `n=abc; echo $((n))` with 0 rather than complaining about `abc`.
            (&[("n", "abc")][..], "n", 0),
            (&[("n", "")][..], "n+1", 1),
            (&[("n", "   ")][..], "n+1", 1),
            // The name is popped after each lookup, so a repeat is not a cycle.
            (&[("a", "b"), ("b", "1")][..], "a+a", 2),
            (&[("x", "y"), ("y", "2")][..], "x*x", 4),
        ] {
            let mut sh = shell_with(vars)?;
            assert_eq!(eval(&mut sh, src).map_err(|_| format!("eval {src}"))?, want, "{src}");
        }
        // An error inside the value is the value's own error, not `bad number`.
        for (vars, src) in [
            (&[("n", "1+")][..], "n"),
            (&[("n", "1 2")][..], "n"),
            (&[("n", "1/0")][..], "n"),
            (&[("n", "08")][..], "n"), // two numbers, as `08` lexes everywhere
        ] {
            let mut sh = shell_with(vars)?;
            assert!(eval(&mut sh, src).is_err(), "{src}");
        }
        Ok(())
    }

    #[test]
    fn a_value_that_reaches_its_own_name_is_refused() -> Result<(), String> {
        for (vars, src) in [
            (&[("n", "n")][..], "n"),
            (&[("n", "n+1")][..], "n"),
            (&[("n", "n++")][..], "n"),
            (&[("a", "b"), ("b", "a")][..], "a"),
            (&[("a", "b"), ("b", "c"), ("c", "a")][..], "a"),
        ] {
            let mut sh = shell_with(vars)?;
            let err = try_eval(&mut sh, src).err().ok_or(format!("{src} evaluated"))?;
            assert!(err.contains("recursion loop"), "{src}: {err}");
        }
        // A name reassigned to a plain number while it is being resolved is
        // still that name, which is what puts the cycle guard ahead of the
        // fast path -- `a` holds `5` by the time the inner read reaches it.
        let mut sh = shell_with(&[("a", "b"), ("b", "a=5,a")])?;
        let err = try_eval(&mut sh, "a").err().ok_or("reassigned cycle evaluated")?;
        assert!(err.contains("recursion loop"), "{err}");
        // The guard is exact-name, in BOTH directions, and neither value may be
        // a plain decimal -- that answers before the guard is ever asked, which
        // is what made an earlier spelling of this row vacuous.
        for (vars, src) in [
            (&[("a", "aa"), ("aa", "1+0")][..], "a"),
            (&[("aa", "a+0"), ("a", "1+0")][..], "aa"),
        ] {
            let mut sh = shell_with(vars)?;
            assert_eq!(eval(&mut sh, src).map_err(|_| format!("eval {src}"))?, 1, "{src}");
        }
        // A name followed by `=` is never LOOKED UP, so assigning to the name
        // being resolved terminates where `n+1` would not.
        let mut sh = shell_with(&[("n", "n=5")])?;
        assert_eq!(eval(&mut sh, "n").map_err(|_| "eval n")?, 5);
        assert_eq!(sh.get_var("n").as_deref(), Some("5"));
        // Nothing is looked up under the untaken `?:` arm, so a cycle there is
        // never walked -- the same rule that keeps `$RANDOM` from being drawn.
        let mut sh = shell_with(&[("a", "b"), ("b", "a")])?;
        assert_eq!(eval(&mut sh, "1?7:a").map_err(|_| "eval ?:")?, 7);
        Ok(())
    }

    #[test]
    fn a_value_can_assign_and_step_as_ash_does() -> Result<(), String> {
        let mut sh = shell_with(&[("n", "m=7")])?;
        assert_eq!(eval(&mut sh, "n+1").map_err(|_| "eval")?, 8);
        assert_eq!(sh.get_var("m").as_deref(), Some("7"));
        let mut sh = shell_with(&[("n", "m++"), ("m", "3")])?;
        assert_eq!(eval(&mut sh, "n").map_err(|_| "eval")?, 3);
        assert_eq!(sh.get_var("m").as_deref(), Some("4"));
        // The WRITE side stays direct: `=` never reads the name, so a value that
        // is not a number is still assignable, and a compound reads it as one.
        let mut sh = shell_with(&[("n", "1+2")])?;
        assert_eq!(eval(&mut sh, "n=5").map_err(|_| "eval")?, 5);
        let mut sh = shell_with(&[("n", "1+2")])?;
        assert_eq!(eval(&mut sh, "n+=1").map_err(|_| "eval")?, 4);
        assert_eq!(sh.get_var("n").as_deref(), Some("4"));
        let mut sh = shell_with(&[("n", "1+2")])?;
        assert_eq!(eval(&mut sh, "n++").map_err(|_| "eval")?, 3);
        assert_eq!(sh.get_var("n").as_deref(), Some("4"));
        Ok(())
    }

    #[test]
    fn the_fast_path_agrees_with_the_evaluator() -> Result<(), String> {
        let mut sh = Shell::new_for_test();
        let mut texts: Vec<String> = [
            "0", "1", "9", "10", "-0", "+0", "-1", "+7", "123456789",
            "9223372036854775807", "-9223372036854775808", "1000000000000000000",
            // Each of these the fast path must refuse, and the reason differs.
            "01", "010", "08", "0x1f", "", " ", " 5 ", "5 ", "1+2", "-", "+", "--5",
            "9223372036854775808", "99999999999999999999", "1e3", "5a", ".5", "0b1",
            // The bytes just past `9`, which a digit test off by a few takes.
            ":", "1:", "9;", "2<", "3=", "4>", "8?",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        // A spread of ordinary counter values, which is what the path is for.
        for i in 0..400u32 {
            texts.push(i.wrapping_mul(7_919).to_string());
            texts.push(format!("-{}", i.wrapping_mul(104_729)));
        }
        let mut taken = 0usize;
        for text in &texts {
            let Some(fast) = plain_decimal(text) else {
                continue;
            };
            taken += 1;
            let slow = try_eval(&mut sh, text).map_err(|e| format!("{text:?}: {e}"))?;
            assert_eq!(fast, slow, "{text:?}");
        }
        assert!(taken > 700, "fast path took only {taken}");
        // The refusals are refusals of an ANSWER, not of a shape: each of these
        // evaluates to something other than its digits read as decimal.
        for (text, want) in [("010", 8), ("9223372036854775808", i64::MIN), ("", 0)] {
            assert_eq!(plain_decimal(text), None, "{text:?}");
            assert_eq!(try_eval(&mut sh, text).map_err(|_| "eval")?, want, "{text:?}");
        }
        Ok(())
    }

    #[test]
    fn only_the_c_blanks_separate_tokens() -> Result<(), String> {
        let mut sh = Shell::new_for_test();
        for text in [" 1 ", "\t1\t", "\r1\r", "\u{b}1\u{c}", "\n1\n", " 1 + 1 - 1 "] {
            assert_eq!(try_eval(&mut sh, text).map_err(|e| e)?, 1, "{text:?}");
        }
        // ash refuses a Unicode space outright rather than skipping it, so a
        // value or expression carrying one is an error and not a number.
        for text in ["\u{a0}1", "1\u{a0}", "1\u{a0}+1", "\u{2003}1", "1 +\u{2003}1", "1\u{3000}"] {
            assert!(try_eval(&mut sh, text).is_err(), "{text:?}");
        }
        Ok(())
    }

    #[test]
    fn a_null_expression_is_zero() -> Result<(), String> {
        let mut sh = Shell::new_for_test();
        // All SIX C blanks, not just the three anyone types. This rule asks the
        // SOURCE, so it carries its own copy of the blank set -- the one the
        // lexer's `only_the_c_blanks_separate_tokens` pins is a different
        // reader, and a predicate that dropped `\r`, `\v` or `\f` here would
        // turn a null expression into a syntax error with that test still green.
        for src in ["", " ", "\t\n", "\r", "\u{b}", "\u{c}", " \t\n\u{b}\u{c}\r"] {
            assert_eq!(eval(&mut sh, src).map_err(|_| format!("eval {src:?}"))?, 0);
        }
        Ok(())
    }

    #[test]
    fn a_chain_of_values_is_bounded_rather_than_overflowing() -> Result<(), String> {
        // A chain has no cycle for the name guard to catch, so the depth bound is
        // what stops it -- with an error rather than a native stack overflow.
        // Two levels per link: the value's own expression, and the operand frame
        // it stands in for. `expr_unary`'s enter() is the second.
        let chain = |n: u32| -> Vec<(String, String)> {
            let mut v: Vec<(String, String)> = (0..n)
                .map(|i| (format!("v{i}"), format!("v{}", i + 1)))
                .collect();
            v.push((format!("v{n}"), "9".into()));
            v
        };
        // The boundary is pinned by VALUE, not relative to the constant, or a
        // shrunken bound reads as passing.
        for (links, deep) in [(49, false), (50, true)] {
            let names = chain(links);
            let refs: Vec<(&str, &str)> =
                names.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let mut sh = shell_with(&refs)?;
            match try_eval(&mut sh, "v0") {
                Ok(v) => {
                    assert!(!deep, "{links} links evaluated");
                    assert_eq!(v, 9, "{links} links");
                }
                Err(e) => {
                    assert!(deep, "{links} links: {e}");
                    assert!(e.contains("too deeply"), "{links} links: {e}");
                }
            }
        }
        // A compound assignment reads its lvalue WITHOUT descending through
        // `expr_unary`, so this chain is bounded only by the level `eval_string`
        // takes. Long enough that losing that level overflows the native stack
        // rather than merely answering.
        let deep = 5_000u32;
        let names: Vec<(String, String)> = (0..deep)
            .map(|i| (format!("v{i}"), format!("v{}+=0", i + 1)))
            .collect();
        let refs: Vec<(&str, &str)> = names.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let mut sh = shell_with(&refs)?;
        let e = try_eval(&mut sh, "v0").err().ok_or("lvalue chain evaluated")?;
        assert!(e.contains("too deeply"), "{e}");
        Ok(())
    }

    #[test]
    fn arithmetic_wraps_where_ash_wraps() -> Result<(), String> {
        // ash's arithmetic is C's on a 64-bit int: everything wraps and only a
        // zero divisor fails. Checking instead made expressions ash answers into
        // fatal errors, and made `-9223372036854775808` unwritable, since the
        // bound is unreachable as a positive literal.
        for (src, want) in [
            ("9223372036854775807 + 1", i64::MIN),
            ("9223372036854775807 * 2", -2),
            ("-9223372036854775808 - 1", i64::MAX),
            ("-9223372036854775808 * -1", i64::MIN),
            ("-(-9223372036854775808)", i64::MIN),
            // A literal out of range is its low 64 bits, which is the only way
            // the bound itself can be written.
            ("9223372036854775808", i64::MIN),
            ("18446744073709551615", -1),
            ("18446744073709551616", 0),
            ("0xFFFFFFFFFFFFFFFF", -1),
            // `i64::MIN / -1` overflows rather than trapping.
            ("-9223372036854775808 / -1", i64::MIN),
            ("-9223372036854775808 % -1", 0),
            // The shift COUNT is masked to six bits, so it never rejects.
            ("1 << 63", i64::MIN),
            ("1 << 64", 1),
            ("1 << 65", 2),
            ("1 << -1", i64::MIN),
            ("1 >> -1", 0),
            ("-9223372036854775808 >> 1", -4611686018427387904),
        ] {
            assert_eq!(ev(src), Ok(want), "{src}");
        }
        // What still fails: a zero divisor, and a digit the base does not have.
        for src in ["1 / 0", "1 % 0", "08", "0x"] {
            assert!(ev(src).is_err(), "{src}");
        }
        // The literal that needs the ACCUMULATOR to wrap, not just the result:
        // a saturating multiply answers -1 here.
        assert_eq!(ev("0x10000000000000000"), Ok(0));
        // And all of it reaches the compound assignments, which share the
        // operators but had no bound test of their own -- `<<=` and `>>=` had
        // no test anywhere, so swapping the two was invisible.
        for (init, src, want) in [
            (1, "n <<= 4", 16),
            (16, "n >>= 2", 4),
            (-8, "n >>= 1", -4),
            (-1, "n >>= 1", -1),
            (1, "n <<= 64", 1),
            (1, "n <<= -1", i64::MIN),
            (i64::MAX, "n *= 2", -2),
            (i64::MIN, "n -= 1", i64::MAX),
            (i64::MAX, "n += 1", i64::MIN),
        ] {
            let mut sh = Shell::new_for_test();
            sh.set_var("n", &init.to_string())
                .map_err(|_| "set failed".to_string())?;
            assert_eq!(eval(&mut sh, src).map_err(|_| format!("eval {src}"))?, want, "{src}");
            assert_eq!(sh.get_var("n").as_deref(), Some(&want.to_string()[..]), "{src}");
        }
        Ok(())
    }

    #[test]
    fn errors_are_reported_not_panicked() {
        assert!(ev("1 / 0").is_err());
        assert!(ev("1 +").is_err());
        assert!(ev("1 @ 2").is_err());
        assert!(ev("(1").is_err());
        // Overflow is NOT in that list any more: it wraps, as ash's does.
        assert_eq!(ev("9223372036854775807 + 1"), Ok(i64::MIN));
    }

    /// ash has FIVE arithmetic diagnostics and no more (math.c): the catch-all,
    /// `divide by zero`, `exponent less than 0`, `malformed ?: operator` and
    /// `expression recursion loop detected`. Whatever a parse got wrong, it is
    /// the catch-all -- measured against busybox ash 1.37.0 over every row here.
    #[test]
    fn a_parse_failure_takes_ashs_one_word_for_it() {
        for src in [
            "1+", "1?", "*", "1 2", "0x", "2#", "2#9", "1+*2", "a b", "++", "1++",
            "$", "#", "1@2", "~~", "!", "1?2", "1?2?3", "[", "..", "0b12", "99#1",
            // A colon that never got a right operand, and one where a value was
            // expected: neither is APPLIED, so neither is ash's ternary message.
            "1:", ":", ":2", "1?2::3", "1??2:3",
            // A token DOES follow the colon in each of these, and ash still
            // says the catch-all: none of them can begin the right operand, so
            // the colon is never applied. `1:+` is the sharp one -- a prefix
            // run that reaches no value is not a value.
            "1:*", "(1:)", "1:,", "1::", "1:?", "1:/", "1:%", "1:=", "1:&&", "1:+",
        ] {
            assert_eq!(ev_msg(src), Err(SYNTAX.to_string()), "{src}");
        }
        // Applied with a value either side and no `?` pending -- ash's other
        // parse message, and the only thing that distinguishes it from the row
        // above is whether the colon got its second operand.
        for src in [
            "1:2", "1+1:2", "(1:2)", "1,2:3", "1?2:3:4",
            // A prefix run that DOES reach a value, which is what separates
            // these from `1:+` above.
            "1:n", "1:(2)", "1:-2", "1:!2", "1:~2", "1:+2",
        ] {
            assert_eq!(ev_msg(src), Err(MALFORMED_TERNARY.to_string()), "{src}");
        }
        // Against LITERALS once each, because every other row here compares a
        // constant with itself: renaming either one leaves the whole suite
        // green, and the words are the behaviour this commit is about.
        assert_eq!(ev_msg("1+"), Err("arithmetic syntax error".to_string()));
        assert_eq!(ev_msg("1:2"), Err("malformed ?: operator".to_string()));
        // The three that are not parse failures keep their own words.
        assert_eq!(ev_msg("1/0"), Err("divide by zero".to_string()));
        assert_eq!(ev_msg("1%0"), Err("divide by zero".to_string()));
        assert_eq!(ev_msg("2**-1"), Err("exponent less than 0".to_string()));
        // ...and a well-formed ternary is untouched by any of it.
        assert_eq!(ev("1?2:3"), Ok(2));
        assert_eq!(ev("0?2:3"), Ok(3));
        assert_eq!(ev("1?2:3?4:5"), Ok(2));
    }

    #[test]
    fn deeply_nested_exprs_error_instead_of_overflowing() {
        // Past MAX_EXPR_DEPTH the parser errors rather than recursing into a stack
        // overflow. Every recursion site is covered: parentheses and unary chains
        // (via expr_unary), right-associative assignment (`a=a=…=1`), right-nested
        // ternary (`0?1:0?1:…`), and `**`, which is right-associative too — the
        // last three recurse in expr_assign/expr_ternary/expr_exponent, NOT
        // through expr_unary, so they need their own brackets.
        assert!(ev(&("(".repeat(1000) + "1" + &")".repeat(1000))).is_err());
        assert!(ev(&("!".repeat(1000) + "1")).is_err());
        assert!(ev(&("-".repeat(1000) + "1")).is_err());
        assert!(ev(&("a=".repeat(1000) + "1")).is_err());
        assert!(ev(&("0?1:".repeat(1000) + "1")).is_err());
        assert!(ev(&("1**".repeat(1000) + "1")).is_err());
    }

    #[test]
    fn only_the_untaken_conditional_arm_skips_the_lookup() -> Result<(), String> {
        // Only the untaken `?:` arm skips the lookup. The dead side of `&&`/`||`
        // still resolves the name, which a self-referential value proves: ash
        // reports the loop for both of those and answers 2 for the conditional.
        let mut sh = shell_with(&[("x", "x")])?;
        assert!(try_eval(&mut sh, "1 || x").is_err());
        assert!(try_eval(&mut sh, "0 && x").is_err());
        assert_eq!(eval(&mut sh, "1 ? 2 : x").map_err(|_| "eval")?, 2);
        Ok(())
    }
}
