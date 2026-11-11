//! CSS Syntax Level 3 (2021), Unicode input and bounded owned output.

use crate::{
    Error, MAX_DECLARATIONS, MAX_INPUT_BYTES, MAX_NESTING, MAX_RULES, MAX_TOKENS, MAX_TOKEN_BYTES,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed<T> {
    value: T,
    errors: usize,
}

impl<T> Parsed<T> {
    pub fn value(&self) -> &T {
        &self.value
    }

    pub fn into_inner(self) -> T {
        self.value
    }

    pub fn errors(&self) -> usize {
        self.errors
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Number {
    pub representation: String,
    pub integer: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Token {
    Ident(String),
    Function(String),
    AtKeyword(String),
    Hash {
        value: String,
        id: bool,
    },
    String(String),
    BadString,
    Url(String),
    BadUrl,
    Number(Number),
    Percentage(Number),
    Dimension(Number, String),
    Whitespace,
    Cdo,
    Cdc,
    Open(char),
    Close(char),
    /// Includes the colon, semicolon and comma tokens.
    Delim(char),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Component {
    Token(Token),
    Block {
        kind: char,
        values: Vec<Component>,
    },
    Function {
        name: String,
        values: Vec<Component>,
    },
}

impl Component {
    pub(crate) fn whitespace(&self) -> bool {
        matches!(self, Self::Token(Token::Whitespace))
    }

    pub(crate) fn delim(&self, ch: char) -> bool {
        matches!(self, Self::Token(Token::Delim(c)) if *c == ch)
    }

    pub(crate) fn ident(&self) -> Option<&str> {
        match self {
            Self::Token(Token::Ident(name)) => Some(name),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub(crate) name: Option<String>,
    pub(crate) prelude: Vec<Component>,
    pub(crate) block: Option<Vec<Component>>,
}

impl Rule {
    /// None denotes a qualified rule; Some denotes an at-rule.
    pub fn at_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn prelude(&self) -> &[Component] {
        &self.prelude
    }

    pub fn block(&self) -> Option<&[Component]> {
        self.block.as_deref()
    }

    pub fn declarations(&self) -> Result<Parsed<Vec<Declaration>>, Error> {
        declarations(self.block.as_deref().unwrap_or_default())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    name: String,
    value: Vec<Component>,
    important: bool,
}

impl Declaration {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn value(&self) -> &[Component] {
        &self.value
    }

    pub fn important(&self) -> bool {
        self.important
    }
}

fn space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n')
}

fn start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

fn name_char(c: char) -> bool {
    start(c) || c.is_ascii_digit() || c == '-'
}

fn valid_escape(a: Option<char>, b: Option<char>) -> bool {
    a == Some('\\') && b != Some('\n')
}

fn starts_ident(a: Option<char>, b: Option<char>, c: Option<char>) -> bool {
    match a {
        Some('-') => b.is_some_and(start) || b == Some('-') || valid_escape(b, c),
        Some(ch) if start(ch) => true,
        _ => valid_escape(a, b),
    }
}

fn starts_number(a: Option<char>, b: Option<char>, c: Option<char>) -> bool {
    match a {
        Some('+' | '-') => {
            b.is_some_and(|x| x.is_ascii_digit())
                || (b == Some('.') && c.is_some_and(|x| x.is_ascii_digit()))
        }
        Some('.') => b.is_some_and(|x| x.is_ascii_digit()),
        Some(x) => x.is_ascii_digit(),
        None => false,
    }
}

fn push(text: &mut String, ch: char) -> Result<(), Error> {
    if text.len() + ch.len_utf8() > MAX_TOKEN_BYTES {
        return Err(Error::Limit("MAX_TOKEN_BYTES"));
    }
    text.push(ch);
    Ok(())
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    errors: usize,
}

impl Lexer {
    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn take(&mut self) -> Option<char> {
        let ch = self.peek(0)?;
        self.pos += 1;
        Some(ch)
    }

    fn error(&mut self) {
        self.errors = self.errors.saturating_add(1);
    }

    fn ident_start(&self) -> bool {
        starts_ident(self.peek(0), self.peek(1), self.peek(2))
    }

    // The backslash has already been consumed.
    fn escape(&mut self) -> char {
        if self.peek(0).is_some_and(|c| c.is_ascii_hexdigit()) {
            let mut value = 0u32;
            for _ in 0..6 {
                let Some(digit) = self.peek(0).and_then(|c| c.to_digit(16)) else {
                    break;
                };
                self.pos += 1;
                value = value * 16 + digit;
            }
            if self.peek(0).is_some_and(space) {
                self.pos += 1;
            }
            if value == 0 {
                '\u{fffd}'
            } else {
                char::from_u32(value).unwrap_or('\u{fffd}')
            }
        } else if let Some(ch) = self.take() {
            ch
        } else {
            self.error();
            '\u{fffd}'
        }
    }

    fn name(&mut self) -> Result<String, Error> {
        let mut text = String::new();
        loop {
            match self.peek(0) {
                Some(ch) if name_char(ch) => {
                    self.pos += 1;
                    push(&mut text, ch)?;
                }
                Some('\\') if valid_escape(self.peek(0), self.peek(1)) => {
                    self.pos += 1;
                    push(&mut text, self.escape())?;
                }
                _ => return Ok(text),
            }
        }
    }

    fn string(&mut self, quote: char) -> Result<Token, Error> {
        let mut text = String::new();
        loop {
            match self.peek(0) {
                None => {
                    self.error();
                    return Ok(Token::String(text));
                }
                Some(ch) if ch == quote => {
                    self.pos += 1;
                    return Ok(Token::String(text));
                }
                Some('\n') => {
                    self.error();
                    return Ok(Token::BadString);
                }
                Some('\\') => {
                    self.pos += 1;
                    match self.peek(0) {
                        Some('\n') => self.pos += 1,
                        None => {}
                        Some(_) => push(&mut text, self.escape())?,
                    }
                }
                Some(ch) => {
                    self.pos += 1;
                    push(&mut text, ch)?;
                }
            }
        }
    }

    fn bad_url(&mut self) -> Token {
        self.error();
        while let Some(ch) = self.take() {
            if ch == ')' {
                break;
            }
            if ch == '\\' && self.peek(0) != Some('\n') {
                let _ = self.escape();
            }
        }
        Token::BadUrl
    }

    fn url(&mut self) -> Result<Token, Error> {
        let mut text = String::new();
        while self.peek(0).is_some_and(space) {
            self.pos += 1;
        }
        loop {
            match self.take() {
                None => {
                    self.error();
                    return Ok(Token::Url(text));
                }
                Some(')') => return Ok(Token::Url(text)),
                Some(ch) if space(ch) => {
                    while self.peek(0).is_some_and(space) {
                        self.pos += 1;
                    }
                    match self.peek(0) {
                        Some(')') => {
                            self.pos += 1;
                            return Ok(Token::Url(text));
                        }
                        None => {
                            self.error();
                            return Ok(Token::Url(text));
                        }
                        _ => return Ok(self.bad_url()),
                    }
                }
                Some('"' | '\'' | '(' | '\0'..='\x08' | '\x0b' | '\x0e'..='\x1f' | '\x7f') => {
                    return Ok(self.bad_url());
                }
                Some('\\') => {
                    if self.peek(0) == Some('\n') {
                        return Ok(self.bad_url());
                    }
                    push(&mut text, self.escape())?;
                }
                Some(ch) => push(&mut text, ch)?,
            }
        }
    }

    fn number(&mut self) -> Result<Token, Error> {
        let mut representation = String::new();
        let mut integer = true;
        if matches!(self.peek(0), Some('+' | '-')) {
            if let Some(ch) = self.take() {
                push(&mut representation, ch)?;
            }
        }
        self.digits(&mut representation)?;
        if self.peek(0) == Some('.') && self.peek(1).is_some_and(|c| c.is_ascii_digit()) {
            integer = false;
            self.pos += 1;
            push(&mut representation, '.')?;
            self.digits(&mut representation)?;
        }
        if matches!(self.peek(0), Some('e' | 'E'))
            && (self.peek(1).is_some_and(|c| c.is_ascii_digit())
                || (matches!(self.peek(1), Some('+' | '-'))
                    && self.peek(2).is_some_and(|c| c.is_ascii_digit())))
        {
            integer = false;
            if let Some(ch) = self.take() {
                push(&mut representation, ch)?;
            }
            if matches!(self.peek(0), Some('+' | '-')) {
                if let Some(ch) = self.take() {
                    push(&mut representation, ch)?;
                }
            }
            self.digits(&mut representation)?;
        }
        let number = Number {
            representation,
            integer,
        };
        if self.ident_start() {
            Ok(Token::Dimension(number, self.name()?))
        } else if self.peek(0) == Some('%') {
            self.pos += 1;
            Ok(Token::Percentage(number))
        } else {
            Ok(Token::Number(number))
        }
    }

    fn digits(&mut self, text: &mut String) -> Result<(), Error> {
        while let Some(ch) = self.peek(0).filter(|c| c.is_ascii_digit()) {
            self.pos += 1;
            push(text, ch)?;
        }
        Ok(())
    }

    fn token(&mut self) -> Result<Option<Token>, Error> {
        while self.peek(0) == Some('/') && self.peek(1) == Some('*') {
            self.pos += 2;
            loop {
                match self.take() {
                    Some('*') if self.peek(0) == Some('/') => {
                        self.pos += 1;
                        break;
                    }
                    None => {
                        self.error();
                        return Ok(None);
                    }
                    _ => {}
                }
            }
        }
        let Some(ch) = self.peek(0) else {
            return Ok(None);
        };
        if space(ch) {
            while self.peek(0).is_some_and(space) {
                self.pos += 1;
            }
            return Ok(Some(Token::Whitespace));
        }
        if starts_number(self.peek(0), self.peek(1), self.peek(2)) {
            return self.number().map(Some);
        }
        if ch == '-' && self.peek(1) == Some('-') && self.peek(2) == Some('>') {
            self.pos += 3;
            return Ok(Some(Token::Cdc));
        }
        if self.ident_start() {
            let name = self.name()?;
            if self.peek(0) != Some('(') {
                return Ok(Some(Token::Ident(name)));
            }
            self.pos += 1;
            if name.eq_ignore_ascii_case("url") {
                // Leave the final whitespace before a quoted URL for tokenization.
                while self.peek(0).is_some_and(space) && self.peek(1).is_some_and(space) {
                    self.pos += 1;
                }
                if !matches!(self.peek(0), Some('"' | '\''))
                    && !(self.peek(0).is_some_and(space)
                        && matches!(self.peek(1), Some('"' | '\'')))
                {
                    return self.url().map(Some);
                }
            }
            return Ok(Some(Token::Function(name)));
        }
        self.pos += 1;
        let token = match ch {
            '"' | '\'' => self.string(ch)?,
            '#' if self.peek(0).is_some_and(name_char)
                || valid_escape(self.peek(0), self.peek(1)) =>
            {
                let id = self.ident_start();
                Token::Hash {
                    value: self.name()?,
                    id,
                }
            }
            '@' if self.ident_start() => Token::AtKeyword(self.name()?),
            '<' if self.peek(0) == Some('!')
                && self.peek(1) == Some('-')
                && self.peek(2) == Some('-') =>
            {
                self.pos += 3;
                Token::Cdo
            }
            '(' | '[' | '{' => Token::Open(ch),
            ')' | ']' | '}' => Token::Close(ch),
            '\\' => {
                self.error();
                Token::Delim(ch)
            }
            _ => Token::Delim(ch),
        };
        Ok(Some(token))
    }
}

pub fn tokenize(input: &str) -> Result<Parsed<Vec<Token>>, Error> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Error::Limit("MAX_INPUT_BYTES"));
    }
    let mut chars = Vec::with_capacity(input.len());
    let mut source = input
        .strip_prefix('\u{feff}')
        .unwrap_or(input)
        .chars()
        .peekable();
    while let Some(ch) = source.next() {
        chars.push(match ch {
            '\r' => {
                if source.peek() == Some(&'\n') {
                    let _ = source.next();
                }
                '\n'
            }
            '\x0c' => '\n',
            '\0' => '\u{fffd}',
            _ => ch,
        });
    }
    let mut lexer = Lexer {
        chars,
        pos: 0,
        errors: 0,
    };
    let mut tokens = Vec::new();
    while let Some(token) = lexer.token()? {
        if tokens.len() == MAX_TOKENS {
            return Err(Error::Limit("MAX_TOKENS"));
        }
        tokens.push(token);
    }
    Ok(Parsed {
        value: tokens,
        errors: lexer.errors,
    })
}

struct Components {
    tokens: std::vec::IntoIter<Token>,
    errors: usize,
}

impl Components {
    fn list(&mut self, close: Option<char>, depth: usize) -> Result<Vec<Component>, Error> {
        let mut values = Vec::new();
        while let Some(token) = self.tokens.next() {
            if matches!(token, Token::Close(ch) if Some(ch) == close) {
                return Ok(values);
            }
            let component = match token {
                Token::Open(kind) => {
                    if depth == MAX_NESTING {
                        return Err(Error::Limit("MAX_NESTING"));
                    }
                    let end = match kind {
                        '(' => ')',
                        '[' => ']',
                        _ => '}',
                    };
                    Component::Block {
                        kind,
                        values: self.list(Some(end), depth + 1)?,
                    }
                }
                Token::Function(name) => {
                    if depth == MAX_NESTING {
                        return Err(Error::Limit("MAX_NESTING"));
                    }
                    Component::Function {
                        name,
                        values: self.list(Some(')'), depth + 1)?,
                    }
                }
                other => {
                    if matches!(other, Token::Close(_)) {
                        self.errors = self.errors.saturating_add(1);
                    }
                    Component::Token(other)
                }
            };
            values.push(component);
        }
        if close.is_some() {
            self.errors = self.errors.saturating_add(1);
        }
        Ok(values)
    }
}

pub fn parse_components(input: &str) -> Result<Parsed<Vec<Component>>, Error> {
    let tokens = tokenize(input)?;
    let mut parser = Components {
        tokens: tokens.value.into_iter(),
        errors: tokens.errors,
    };
    let value = parser.list(None, 0)?;
    Ok(Parsed {
        value,
        errors: parser.errors,
    })
}

pub fn parse_stylesheet(input: &str) -> Result<Parsed<Vec<Rule>>, Error> {
    let components = parse_components(input)?;
    rule_list(components.value, components.errors, true)
}

pub(crate) fn rule_list(
    values: Vec<Component>,
    mut errors: usize,
    top: bool,
) -> Result<Parsed<Vec<Rule>>, Error> {
    let mut source = values.into_iter().peekable();
    let mut rules = Vec::new();
    while let Some(first) = source.next() {
        if matches!(first, Component::Token(Token::Whitespace))
            || (top && matches!(first, Component::Token(Token::Cdo | Token::Cdc)))
        {
            continue;
        }
        let (name, mut prelude, first_block) = match first {
            Component::Token(Token::AtKeyword(name)) => (Some(name), Vec::new(), None),
            Component::Block { kind: '{', values } => (None, Vec::new(), Some(values)),
            other => (None, vec![other], None),
        };
        let mut block = first_block;
        let mut terminated = block.is_some();
        if block.is_none() {
            for component in source.by_ref() {
                match component {
                    Component::Block { kind: '{', values } => {
                        block = Some(values);
                        terminated = true;
                        break;
                    }
                    Component::Token(Token::Delim(';')) if name.is_some() => {
                        terminated = true;
                        break;
                    }
                    other => prelude.push(other),
                }
            }
        }
        if !terminated {
            errors = errors.saturating_add(1);
            if name.is_none() {
                break;
            }
        }
        if rules.len() == MAX_RULES {
            return Err(Error::Limit("MAX_RULES"));
        }
        rules.push(Rule {
            name,
            prelude,
            block,
        });
    }
    Ok(Parsed {
        value: rules,
        errors,
    })
}

pub(crate) fn trim(mut values: &[Component]) -> &[Component] {
    while values.first().is_some_and(Component::whitespace) {
        values = values.get(1..).unwrap_or_default();
    }
    while values.last().is_some_and(Component::whitespace) {
        values = values.get(..values.len() - 1).unwrap_or_default();
    }
    values
}

pub(crate) fn declarations(values: &[Component]) -> Result<Parsed<Vec<Declaration>>, Error> {
    let mut source = values.iter().peekable();
    let mut output = Vec::new();
    let mut errors = 0usize;
    while let Some(first) = source.next() {
        if first.whitespace() || first.delim(';') {
            continue;
        }
        if matches!(first, Component::Token(Token::AtKeyword(_))) {
            let mut terminated = false;
            for item in source.by_ref() {
                if item.delim(';') || matches!(item, Component::Block { kind: '{', .. }) {
                    terminated = true;
                    break;
                }
            }
            if !terminated {
                errors = errors.saturating_add(1);
            }
            continue;
        }
        let mut candidate = vec![first.clone()];
        while source.peek().is_some_and(|c| !c.delim(';')) {
            if let Some(item) = source.next() {
                candidate.push(item.clone());
            }
        }
        let Some(name) = first.ident() else {
            errors = errors.saturating_add(1);
            continue;
        };
        let rest = trim(candidate.get(1..).unwrap_or_default());
        if !rest.first().is_some_and(|c| c.delim(':')) {
            errors = errors.saturating_add(1);
            continue;
        }
        let mut value = trim(rest.get(1..).unwrap_or_default());
        let mut important = false;
        if value
            .last()
            .and_then(Component::ident)
            .is_some_and(|n| n.eq_ignore_ascii_case("important"))
        {
            let before = trim(value.get(..value.len() - 1).unwrap_or_default());
            if before.last().is_some_and(|c| c.delim('!')) {
                important = true;
                value = trim(before.get(..before.len() - 1).unwrap_or_default());
            }
        }
        if output.len() == MAX_DECLARATIONS {
            return Err(Error::Limit("MAX_DECLARATIONS"));
        }
        output.push(Declaration {
            name: name.to_owned(),
            value: value.to_vec(),
            important,
        });
    }
    Ok(Parsed {
        value: output,
        errors,
    })
}

pub fn parse_declarations(input: &str) -> Result<Parsed<Vec<Declaration>>, Error> {
    let components = parse_components(input)?;
    let mut parsed = declarations(&components.value)?;
    parsed.errors = parsed.errors.saturating_add(components.errors);
    Ok(parsed)
}

#[cfg(test)]
mod tests;
