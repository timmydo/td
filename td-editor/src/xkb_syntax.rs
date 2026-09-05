//! Bounded lexical structure for self-contained XKB text; no file lookups.

use crate::xkb::{Diagnostic, Result};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_TOKENS: usize = 200_000;
const MAX_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    Word,
    String,
    Key,
    Punct,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Token<'a> {
    pub text: &'a str,
    pub kind: Kind,
    pub offset: usize,
    // An opening delimiter skips its complete, already balanced group.
    span: usize,
}

impl Token<'_> {
    pub fn is(self, text: &str) -> bool {
        match self.kind {
            Kind::Word => self.text.eq_ignore_ascii_case(text),
            Kind::Punct => self.text == text,
            _ => false,
        }
    }

    pub fn error(self, reason: &'static str) -> Diagnostic {
        Diagnostic {
            offset: self.offset,
            item: self.text.to_owned(),
            reason,
        }
    }

    pub fn string(self) -> Result<String> {
        if self.kind != Kind::String {
            return Err(self.error("expected quoted name"));
        }
        let mut out = String::new();
        let mut chars = self.text.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            out.push(match chars.next() {
                Some('\\') => '\\',
                Some('"') => '"',
                Some('n') => '\n',
                Some('r') => '\r',
                Some('t') => '\t',
                _ => return Err(self.error("unsupported string escape")),
            });
        }
        Ok(out)
    }
}

pub(crate) fn error(reason: &'static str) -> Diagnostic {
    Diagnostic {
        offset: 0,
        item: "keymap".to_owned(),
        reason,
    }
}

pub(crate) fn lex(source: &str) -> Result<Vec<Token<'_>>> {
    if source.len() > MAX_BYTES {
        return Err(error("keymap exceeds 1 MiB"));
    }
    // wl_keyboard's text-v1 payload may have exactly one final NUL.
    let source = source.strip_suffix('\0').unwrap_or(source);
    if let Some(offset) = source.find('\0') {
        return Err(at(source, offset, "interior NUL"));
    }
    let bytes = source.as_bytes();
    let mut tokens: Vec<Token<'_>> = Vec::new();
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut pos = 0;
    while let Some(&byte) = bytes.get(pos) {
        if byte.is_ascii_whitespace() {
            pos += 1;
            continue;
        }
        if byte == b'/' && bytes.get(pos + 1) == Some(&b'/') {
            pos += 2;
            while bytes.get(pos).is_some_and(|b| *b != b'\n') {
                pos += 1;
            }
            continue;
        }
        if byte == b'/' && bytes.get(pos + 1) == Some(&b'*') {
            let start = pos;
            pos += 2;
            loop {
                match (bytes.get(pos), bytes.get(pos + 1)) {
                    (Some(b'*'), Some(b'/')) => {
                        pos += 2;
                        break;
                    }
                    (Some(_), _) => pos += 1,
                    _ => return Err(at(source, start, "unterminated comment")),
                }
            }
            continue;
        }
        if tokens.len() == MAX_TOKENS {
            return Err(at(source, pos, "more than 200000 tokens"));
        }
        let start = pos;
        let (kind, text) = match byte {
            b'"' | b'<' => {
                let close = if byte == b'"' { b'"' } else { b'>' };
                pos += 1;
                let inner = pos;
                loop {
                    match bytes.get(pos).copied() {
                        Some(b) if b == close => break,
                        Some(0 | b'\n' | b'\r') | None => {
                            return Err(at(source, start, "unterminated string or key name"));
                        }
                        Some(b'\\') if byte == b'"' => {
                            pos += 1;
                            if !matches!(bytes.get(pos), Some(b'\\' | b'"' | b'n' | b'r' | b't')) {
                                return Err(at(source, pos, "unsupported string escape"));
                            }
                            pos += 1;
                        }
                        Some(b)
                            if byte == b'<'
                                && !(b.is_ascii_alphanumeric()
                                    || matches!(b, b'_' | b'+' | b'-')) =>
                        {
                            return Err(at(source, pos, "invalid key name"));
                        }
                        Some(_) => pos += 1,
                    }
                }
                let text = source
                    .get(inner..pos)
                    .ok_or_else(|| error("invalid string boundary"))?;
                pos += 1;
                (
                    if byte == b'"' {
                        Kind::String
                    } else {
                        Kind::Key
                    },
                    text,
                )
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' => {
                pos += 1;
                while bytes
                    .get(pos)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                {
                    pos += 1;
                }
                let text = source
                    .get(start..pos)
                    .ok_or_else(|| error("invalid word boundary"))?;
                if text.eq_ignore_ascii_case("include") {
                    return Err(at(source, start, "includes are not self-contained"));
                }
                (Kind::Word, text)
            }
            b'{' | b'}' | b'[' | b']' | b'(' | b')' | b'=' | b';' | b',' | b'+' | b'-' | b'.'
            | b'!' | b'~' | b'*' | b'/' | b'|' => {
                pos += 1;
                (
                    Kind::Punct,
                    source
                        .get(start..pos)
                        .ok_or_else(|| error("invalid punctuation"))?,
                )
            }
            _ => return Err(at(source, start, "invalid byte outside a quoted string")),
        };
        let token = Token {
            text,
            kind,
            offset: start,
            span: 1,
        };
        if kind == Kind::Punct {
            match byte {
                b'{' | b'[' | b'(' => {
                    if stack.len() == MAX_DEPTH {
                        return Err(token.error("nesting exceeds 32"));
                    }
                    stack.push((byte, tokens.len()));
                }
                b'}' | b']' | b')' => {
                    let expected = match byte {
                        b'}' => b'{',
                        b']' => b'[',
                        _ => b'(',
                    };
                    let (open, index) = stack
                        .pop()
                        .ok_or_else(|| token.error("unmatched delimiter"))?;
                    if open != expected {
                        return Err(token.error("mismatched delimiter"));
                    }
                    let span = tokens.len() - index + 1;
                    let opening = tokens
                        .get_mut(index)
                        .ok_or_else(|| token.error("missing delimiter"))?;
                    opening.span = span;
                }
                _ => {}
            }
        }
        tokens.push(token);
    }
    if let Some((_, index)) = stack.last() {
        return Err(tokens
            .get(*index)
            .ok_or_else(|| error("missing delimiter"))?
            .error("unclosed delimiter"));
    }
    Ok(tokens)
}

fn at(source: &str, offset: usize, reason: &'static str) -> Diagnostic {
    let item = source
        .get(offset..)
        .and_then(|s| s.chars().next())
        .map(String::from)
        .unwrap_or_default();
    Diagnostic {
        offset,
        item,
        reason,
    }
}

/// Split at a top-level delimiter without visiting nested groups repeatedly.
pub(crate) fn split<'a, 's>(
    mut tokens: &'a [Token<'s>],
    delimiter: &'static str,
) -> impl Iterator<Item = &'a [Token<'s>]> {
    std::iter::from_fn(move || {
        if tokens.is_empty() {
            return None;
        }
        let mut pos = 0;
        while let Some(token) = tokens.get(pos) {
            if token.is(delimiter) {
                let head = tokens.get(..pos)?;
                tokens = tokens.get(pos + 1..)?;
                return Some(head);
            }
            pos += token.span;
        }
        let tail = tokens;
        tokens = &[];
        Some(tail)
    })
}

pub(crate) fn statements<'a, 's>(
    tokens: &'a [Token<'s>],
) -> Result<impl Iterator<Item = &'a [Token<'s>]>> {
    if !tokens.is_empty() && !tokens.last().is_some_and(|t| t.is(";")) {
        return Err(tokens
            .last()
            .ok_or_else(|| error("missing statement"))?
            .error("missing semicolon"));
    }
    Ok(split(tokens, ";"))
}

pub(crate) fn block<'a, 's>(tokens: &'a [Token<'s>]) -> Result<(&'a [Token<'s>], &'a [Token<'s>])> {
    let mut pos = 0;
    while let Some(token) = tokens.get(pos) {
        if token.is("{") {
            if pos + token.span != tokens.len() {
                return Err(token.error("trailing block tokens"));
            }
            return Ok((
                tokens.get(..pos).ok_or_else(|| error("missing header"))?,
                tokens
                    .get(pos + 1..tokens.len() - 1)
                    .ok_or_else(|| error("missing body"))?,
            ));
        }
        pos += token.span;
    }
    Err(tokens
        .first()
        .map_or_else(|| error("expected block"), |t| t.error("expected block")))
}

pub(crate) fn assignment<'a, 's>(
    tokens: &'a [Token<'s>],
) -> Result<(&'a [Token<'s>], &'a [Token<'s>])> {
    let mut parts = split(tokens, "=");
    let left = parts.next().ok_or_else(|| error("missing assignment"))?;
    let right = parts
        .next()
        .ok_or_else(|| error("missing assignment value"))?;
    if left.is_empty() || right.is_empty() || parts.next().is_some() {
        return Err(error("invalid assignment"));
    }
    Ok((left, right))
}

pub(crate) fn index<'a, 's>(tokens: &'a [Token<'s>]) -> Result<&'a [Token<'s>]> {
    match (tokens.first(), tokens.get(1), tokens.last()) {
        (Some(_), Some(open), Some(close))
            if open.is("[") && close.is("]") && open.span + 1 == tokens.len() =>
        {
            tokens
                .get(2..tokens.len() - 1)
                .ok_or_else(|| error("missing index"))
        }
        _ => Err(tokens.first().map_or_else(
            || error("expected indexed field"),
            |t| t.error("expected indexed field"),
        )),
    }
}
