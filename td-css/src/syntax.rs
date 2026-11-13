//! CSS Syntax Level 3 (2021), Unicode input and bounded owned output.

use crate::{
    Error, MAX_DECLARATIONS, MAX_INPUT_BYTES, MAX_NESTING, MAX_RULES, MAX_TOKENS, MAX_TOKEN_BYTES,
};
use td_alloc::{Budget, BudgetIntoIter, BudgetShared, BudgetString, BudgetVec};

/// Immutable token text. Short UTF-8 values live in their enclosing owner;
/// longer values share fallibly allocated, budget-owned storage.
pub struct Text(TextStorage);
enum TextStorage {
    Inline(td_alloc::SmallText<14>),
    Medium(BudgetShared<td_alloc::SmallText<62>>),
    Shared(BudgetShared<BudgetString>),
}
impl Text {
    fn inline(value: &str) -> Option<Self> {
        Some(Self(TextStorage::Inline(td_alloc::SmallText::copy(value)?)))
    }
    pub(crate) fn from_string(memory: &Budget, value: BudgetString) -> Result<Self, Error> {
        if let Some(text) = Self::inline(&value) {
            return Ok(text);
        }
        if let Some(fixed) = td_alloc::SmallText::<62>::copy(&value) {
            return Ok(Self(TextStorage::Medium(BudgetShared::new(memory, fixed)?)));
        }
        Ok(Self(TextStorage::Shared(BudgetShared::new(memory, value)?)))
    }
    #[inline]
    fn bytes(&self) -> &[u8] {
        match &self.0 {
            TextStorage::Inline(value) => value.as_bytes(),
            TextStorage::Medium(value) => value.as_bytes(),
            TextStorage::Shared(value) => value.as_bytes(),
        }
    }
    #[inline]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            TextStorage::Inline(value) => value.as_str(),
            TextStorage::Medium(value) => value.as_str(),
            TextStorage::Shared(value) => value.as_str(),
        }
    }
    pub(crate) fn heap_capacity(&self) -> usize {
        match &self.0 {
            TextStorage::Inline(_) => 0,
            TextStorage::Medium(_) => 62,
            TextStorage::Shared(value) => value.capacity(),
        }
    }
    #[inline]
    pub fn try_clone(&self) -> Result<Self, Error> {
        Ok(Self(match &self.0 {
            TextStorage::Inline(value) => TextStorage::Inline(*value),
            TextStorage::Medium(value) => TextStorage::Medium(value.try_clone()?),
            TextStorage::Shared(value) => TextStorage::Shared(value.try_clone()?),
        }))
    }
}
impl std::ops::Deref for Text {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl AsRef<str> for Text {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl std::borrow::Borrow<str> for Text {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}
impl std::fmt::Debug for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}
impl PartialEq for Text {
    fn eq(&self, other: &Self) -> bool {
        self.bytes() == other.bytes()
    }
}
impl Eq for Text {}
impl PartialOrd for Text {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Text {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.bytes().cmp(other.bytes())
    }
}
impl std::hash::Hash for Text {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.as_str().hash(h);
    }
}
pub(crate) fn text(memory: &Budget, value: &str) -> Result<Text, Error> {
    if let Some(text) = Text::inline(value) {
        return Ok(text);
    }
    if let Some(fixed) = td_alloc::SmallText::<62>::copy(value) {
        return Ok(Text(TextStorage::Medium(BudgetShared::new(memory, fixed)?)));
    }
    Text::from_string(memory, BudgetString::copy(memory, value)?)
}
pub(crate) fn lower_text(memory: &Budget, value: &str) -> Result<Text, Error> {
    if let Some(mut text) = Text::inline(value) {
        if let TextStorage::Inline(value) = &mut text.0 {
            value.make_ascii_lowercase();
        }
        return Ok(text);
    }
    let mut value = BudgetString::copy(memory, value)?;
    value.make_ascii_lowercase();
    Text::from_string(memory, value)
}
struct TextBuilder<'a> {
    memory: &'a Budget,
    fixed: td_alloc::SmallText<62>,
    heap: Option<BudgetString>,
}
impl<'a> TextBuilder<'a> {
    fn new(memory: &'a Budget) -> Self {
        Self {
            memory,
            fixed: td_alloc::SmallText::new(),
            heap: None,
        }
    }
    #[inline]
    fn try_push(&mut self, ch: char) -> Result<(), Error> {
        if let Some(heap) = &mut self.heap {
            if heap.len().saturating_add(ch.len_utf8()) > MAX_TOKEN_BYTES {
                return Err(Error::Limit("MAX_TOKEN_BYTES"));
            }
            heap.try_push(ch)?;
            return Ok(());
        }
        let mut scratch = [0; 4];
        let value = ch.encode_utf8(&mut scratch);
        if self.fixed.try_push_str(value).is_ok() {
            return Ok(());
        }
        let prefix = self.fixed.as_str();
        let end = prefix.len() + value.len();
        // The bounded fixed prefix and one scalar fit below MAX_TOKEN_BYTES.
        let mut heap = match td_alloc::BudgetString::with_capacity(self.memory, 128) {
            Err(td_alloc::BudgetError::Limit | td_alloc::BudgetError::Overflow) => {
                td_alloc::BudgetString::with_capacity(self.memory, end)?
            }
            result => result?,
        };
        heap.try_push_str(prefix)?;
        heap.try_push_str(value)?;
        self.heap = Some(heap);
        Ok(())
    }
    fn finish(self) -> Result<Text, Error> {
        match self.heap {
            Some(value) => Text::from_string(self.memory, value),
            None => {
                let fixed = self.fixed;
                let value = fixed.as_str();
                if let Some(text) = Text::inline(value) {
                    Ok(text)
                } else {
                    Ok(Text(TextStorage::Medium(BudgetShared::new(
                        self.memory,
                        fixed,
                    )?)))
                }
            }
        }
    }
}
pub(crate) fn keyword(value: &str, choices: &[&'static str]) -> Option<&'static str> {
    choices
        .iter()
        .copied()
        .find(|choice| value.eq_ignore_ascii_case(choice))
}
pub(crate) fn copy_components(
    values: &[Component],
    memory: &Budget,
) -> Result<BudgetVec<Component>, Error> {
    let mut result = td_alloc::BudgetVec::with_capacity(memory, values.len())?;
    for value in values {
        result
            .push_within_capacity(value.try_clone()?)
            .map_err(|_| Error::InvalidContext)?;
    }
    Ok(result)
}
impl Number {
    pub(crate) fn try_clone(&self) -> Result<Self, Error> {
        Ok(Self {
            representation: self.representation.try_clone()?,
            integer: self.integer,
        })
    }
}
impl Token {
    pub(crate) fn try_clone(&self) -> Result<Self, Error> {
        Ok(match self {
            Self::Ident(s) => Self::Ident(s.try_clone()?),
            Self::Function(s) => Self::Function(s.try_clone()?),
            Self::AtKeyword(s) => Self::AtKeyword(s.try_clone()?),
            Self::String(s) => Self::String(s.try_clone()?),
            Self::Url(s) => Self::Url(s.try_clone()?),
            Self::Hash { value, id } => Self::Hash {
                value: value.try_clone()?,
                id: *id,
            },
            Self::Number(n) => Self::Number(n.try_clone()?),
            Self::Percentage(n) => Self::Percentage(n.try_clone()?),
            Self::Dimension(n, s) => Self::Dimension(n.try_clone()?, s.try_clone()?),
            Self::BadString => Self::BadString,
            Self::BadUrl => Self::BadUrl,
            Self::Whitespace => Self::Whitespace,
            Self::Cdo => Self::Cdo,
            Self::Cdc => Self::Cdc,
            Self::Open(c) => Self::Open(*c),
            Self::Close(c) => Self::Close(*c),
            Self::Delim(c) => Self::Delim(*c),
        })
    }
}
impl Component {
    pub(crate) fn try_clone(&self) -> Result<Self, Error> {
        Ok(match self {
            Self::Token(t) => Self::Token(t.try_clone()?),
            Self::Block { kind, values } => Self::Block {
                kind: *kind,
                values: copy_components(values, values.budget())?,
            },
            Self::Function { name, values } => Self::Function {
                name: name.try_clone()?,
                values: copy_components(values, values.budget())?,
            },
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
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

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Number {
    pub representation: Text,
    pub integer: bool,
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum Token {
    Ident(Text),
    Function(Text),
    AtKeyword(Text),
    Hash {
        value: Text,
        id: bool,
    },
    String(Text),
    BadString,
    Url(Text),
    BadUrl,
    Number(Number),
    Percentage(Number),
    Dimension(Number, Text),
    Whitespace,
    Cdo,
    Cdc,
    Open(char),
    Close(char),
    /// Includes the colon, semicolon and comma tokens.
    Delim(char),
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum Component {
    Token(Token),
    Block {
        kind: char,
        values: BudgetVec<Component>,
    },
    Function {
        name: Text,
        values: BudgetVec<Component>,
    },
}

impl Component {
    pub(crate) fn whitespace(&self) -> bool {
        matches!(self, Self::Token(Token::Whitespace))
    }

    pub(crate) fn delim(&self, ch: char) -> bool {
        matches!(self, Self::Token(Token::Delim(c)) if *c == ch)
    }

    pub(crate) fn ident_owned(&self) -> Option<&Text> {
        match self {
            Self::Token(Token::Ident(name)) => Some(name),
            _ => None,
        }
    }
    pub(crate) fn ident(&self) -> Option<&str> {
        match self {
            Self::Token(Token::Ident(name)) => Some(name),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Rule {
    pub(crate) name: Option<Text>,
    pub(crate) prelude: BudgetVec<Component>,
    pub(crate) block: Option<BudgetVec<Component>>,
}

impl Rule {
    /// None denotes a qualified rule; Some denotes an at-rule.
    pub fn at_name(&self) -> Option<&str> {
        self.name.as_ref().map(|name| name.as_str())
    }

    pub fn prelude(&self) -> &[Component] {
        &self.prelude
    }

    pub fn block(&self) -> Option<&[Component]> {
        self.block.as_ref().map(|v| v.as_slice())
    }

    pub fn declarations(&self) -> Result<Parsed<BudgetVec<Declaration>>, Error> {
        declarations(
            self.block
                .as_ref()
                .map(|v| v.as_slice())
                .unwrap_or_default(),
            self.prelude.budget(),
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Declaration {
    name: Text,
    value: BudgetVec<Component>,
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

fn append_char(text: &mut TextBuilder<'_>, ch: char) -> Result<(), Error> {
    text.try_push(ch)
}

enum Input<'a> {
    Ascii(&'a [u8]),
    Unicode(BudgetVec<char>),
}
struct Lexer<'a> {
    chars: Input<'a>,
    memory: &'a Budget,
    pos: usize,
    errors: usize,
}

impl Lexer<'_> {
    fn peek(&self, offset: usize) -> Option<char> {
        match &self.chars {
            Input::Ascii(bytes) => bytes.get(self.pos + offset).copied().map(char::from),
            Input::Unicode(chars) => chars.get(self.pos + offset).copied(),
        }
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

    fn name(&mut self) -> Result<Text, Error> {
        let memory = self.memory;
        let mut text = TextBuilder::new(memory);
        loop {
            match self.peek(0) {
                Some(ch) if name_char(ch) => {
                    self.pos += 1;
                    append_char(&mut text, ch)?;
                }
                Some('\\') if valid_escape(self.peek(0), self.peek(1)) => {
                    self.pos += 1;
                    append_char(&mut text, self.escape())?;
                }
                _ => return text.finish(),
            }
        }
    }

    fn string(&mut self, quote: char) -> Result<Token, Error> {
        let memory = self.memory;
        let mut text = TextBuilder::new(memory);
        loop {
            match self.peek(0) {
                None => {
                    self.error();
                    return Ok(Token::String(text.finish()?));
                }
                Some(ch) if ch == quote => {
                    self.pos += 1;
                    return Ok(Token::String(text.finish()?));
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
                        Some(_) => append_char(&mut text, self.escape())?,
                    }
                }
                Some(ch) => {
                    self.pos += 1;
                    append_char(&mut text, ch)?;
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
        let memory = self.memory;
        let mut text = TextBuilder::new(memory);
        while self.peek(0).is_some_and(space) {
            self.pos += 1;
        }
        loop {
            match self.take() {
                None => {
                    self.error();
                    return Ok(Token::Url(text.finish()?));
                }
                Some(')') => return Ok(Token::Url(text.finish()?)),
                Some(ch) if space(ch) => {
                    while self.peek(0).is_some_and(space) {
                        self.pos += 1;
                    }
                    match self.peek(0) {
                        Some(')') => {
                            self.pos += 1;
                            return Ok(Token::Url(text.finish()?));
                        }
                        None => {
                            self.error();
                            return Ok(Token::Url(text.finish()?));
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
                    append_char(&mut text, self.escape())?;
                }
                Some(ch) => append_char(&mut text, ch)?,
            }
        }
    }

    fn number(&mut self) -> Result<Token, Error> {
        let memory = self.memory;
        let mut representation = TextBuilder::new(memory);
        let mut integer = true;
        if matches!(self.peek(0), Some('+' | '-')) {
            if let Some(ch) = self.take() {
                append_char(&mut representation, ch)?;
            }
        }
        self.digits(&mut representation)?;
        if self.peek(0) == Some('.') && self.peek(1).is_some_and(|c| c.is_ascii_digit()) {
            integer = false;
            self.pos += 1;
            append_char(&mut representation, '.')?;
            self.digits(&mut representation)?;
        }
        if matches!(self.peek(0), Some('e' | 'E'))
            && (self.peek(1).is_some_and(|c| c.is_ascii_digit())
                || (matches!(self.peek(1), Some('+' | '-'))
                    && self.peek(2).is_some_and(|c| c.is_ascii_digit())))
        {
            integer = false;
            if let Some(ch) = self.take() {
                append_char(&mut representation, ch)?;
            }
            if matches!(self.peek(0), Some('+' | '-')) {
                if let Some(ch) = self.take() {
                    append_char(&mut representation, ch)?;
                }
            }
            self.digits(&mut representation)?;
        }
        let number = Number {
            representation: representation.finish()?,
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

    fn digits(&mut self, text: &mut TextBuilder<'_>) -> Result<(), Error> {
        while let Some(ch) = self.peek(0).filter(|c| c.is_ascii_digit()) {
            self.pos += 1;
            append_char(text, ch)?;
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

fn lexer<'a>(input: &'a str, memory: &'a Budget) -> Result<Lexer<'a>, Error> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Error::Limit("MAX_INPUT_BYTES"));
    }
    if input.is_ascii() && !input.bytes().any(|c| matches!(c, b'\r' | 0x0c | 0)) {
        return Ok(Lexer {
            chars: Input::Ascii(input.as_bytes()),
            memory,
            pos: 0,
            errors: 0,
        });
    }
    let mut chars = td_alloc::BudgetVec::with_capacity(memory, input.len())?;
    let mut source = input
        .strip_prefix('\u{feff}')
        .unwrap_or(input)
        .chars()
        .peekable();
    while let Some(ch) = source.next() {
        chars
            .push_within_capacity(match ch {
                '\r' => {
                    if source.peek() == Some(&'\n') {
                        let _ = source.next();
                    }
                    '\n'
                }
                '\x0c' => '\n',
                '\0' => '\u{fffd}',
                _ => ch,
            })
            .map_err(|_| Error::InvalidContext)?;
    }
    Ok(Lexer {
        chars: Input::Unicode(chars),
        memory,
        pos: 0,
        errors: 0,
    })
}

pub fn tokenize(input: &str) -> Result<Parsed<BudgetVec<Token>>, Error> {
    tokenize_with_budget(input, &crate::memory()?)
}
pub fn tokenize_with_budget(
    input: &str,
    memory: &Budget,
) -> Result<Parsed<BudgetVec<Token>>, Error> {
    let mut lexer = lexer(input, memory)?;
    let mut tokens = BudgetVec::new(memory)?;
    while let Some(token) = lexer.token()? {
        if tokens.len() == MAX_TOKENS {
            return Err(Error::Limit("MAX_TOKENS"));
        }
        tokens.try_push(token)?;
    }
    Ok(Parsed {
        value: tokens,
        errors: lexer.errors,
    })
}

struct Components<'a> {
    memory: &'a Budget,
    tokens: BudgetIntoIter<Token>,
    errors: usize,
}

// Count immediate children, skipping nested lists with a fixed bounded stack.
// Exact reservation avoids repeated growth while building common rule bodies.
fn component_capacity(tokens: &[Token], close: Option<char>) -> Result<usize, Error> {
    let mut ends = ['\0'; MAX_NESTING];
    let mut depth = 0usize;
    let mut count = 0usize;
    for token in tokens {
        if depth == 0 {
            if matches!(token, Token::Close(ch) if Some(*ch) == close) {
                break;
            }
            count += 1;
        }
        let end = match token {
            Token::Open('(') | Token::Function(_) => Some(')'),
            Token::Open('[') => Some(']'),
            Token::Open('{') => Some('}'),
            Token::Close(ch) if depth > 0 && ends.get(depth - 1) == Some(ch) => {
                depth -= 1;
                None
            }
            _ => None,
        };
        if let Some(end) = end {
            *ends.get_mut(depth).ok_or(Error::Limit("MAX_NESTING"))? = end;
            depth += 1;
        }
    }
    Ok(count)
}

impl Components<'_> {
    fn list(&mut self, close: Option<char>, depth: usize) -> Result<BudgetVec<Component>, Error> {
        let capacity = component_capacity(self.tokens.as_slice(), close)?;
        let mut values = td_alloc::BudgetVec::with_capacity(self.memory, capacity)?;
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
            values
                .push_within_capacity(component)
                .map_err(|_| Error::InvalidContext)?;
        }
        if close.is_some() {
            self.errors = self.errors.saturating_add(1);
        }
        Ok(values)
    }
}

pub fn parse_components(input: &str) -> Result<Parsed<BudgetVec<Component>>, Error> {
    parse_components_with_budget(input, &crate::memory()?)
}
pub fn parse_components_with_budget(
    input: &str,
    memory: &Budget,
) -> Result<Parsed<BudgetVec<Component>>, Error> {
    let tokens = tokenize_with_budget(input, memory)?;
    let mut parser = Components {
        memory,
        tokens: tokens.value.into_iter(),
        errors: tokens.errors,
    };
    let value = parser.list(None, 0)?;
    Ok(Parsed {
        value,
        errors: parser.errors,
    })
}

pub fn parse_stylesheet(input: &str) -> Result<Parsed<BudgetVec<Rule>>, Error> {
    parse_stylesheet_with_budget(input, &crate::memory()?)
}
pub fn parse_stylesheet_with_budget(
    input: &str,
    memory: &Budget,
) -> Result<Parsed<BudgetVec<Rule>>, Error> {
    let components = parse_components_with_budget(input, memory)?;
    rule_list(components.value, components.errors, true)
}

pub(crate) fn rule_list(
    values: BudgetVec<Component>,
    mut errors: usize,
    top: bool,
) -> Result<Parsed<BudgetVec<Rule>>, Error> {
    let memory = values.budget().try_clone()?;
    let mut source = values.into_iter().peekable();
    let mut rules = BudgetVec::new(&memory)?;
    while let Some(first) = source.next() {
        if matches!(first, Component::Token(Token::Whitespace))
            || (top && matches!(first, Component::Token(Token::Cdo | Token::Cdc)))
        {
            continue;
        }
        let (name, mut prelude, first_block) = match first {
            Component::Token(Token::AtKeyword(name)) => {
                (Some(name), BudgetVec::new(&memory)?, None)
            }
            Component::Block { kind: '{', values } => {
                (None, BudgetVec::new(&memory)?, Some(values))
            }
            other => (
                None,
                {
                    let mut list = BudgetVec::new(&memory)?;
                    list.try_push(other)?;
                    list
                },
                None,
            ),
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
                    other => prelude.try_push(other)?,
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
        rules.try_push(Rule {
            name,
            prelude,
            block,
        })?;
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

pub(crate) fn declarations(
    values: &[Component],
    memory: &Budget,
) -> Result<Parsed<BudgetVec<Declaration>>, Error> {
    let mut source = values;
    let mut output = BudgetVec::new(memory)?;
    let mut errors = 0usize;
    while let Some((first, rest)) = source.split_first() {
        let current = source;
        source = rest;
        if first.whitespace() || first.delim(';') {
            continue;
        }
        if matches!(first, Component::Token(Token::AtKeyword(_))) {
            let mut terminated = false;
            while let Some((item, rest)) = source.split_first() {
                source = rest;
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
        let end = source
            .iter()
            .position(|c| c.delim(';'))
            .unwrap_or(source.len());
        let candidate = current.get(..end + 1).ok_or(Error::InvalidContext)?;
        source = source.get(end..).ok_or(Error::InvalidContext)?;
        let Some(name) = first.ident_owned() else {
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
        output.try_push(Declaration {
            name: name.try_clone()?,
            value: copy_components(value, memory)?,
            important,
        })?;
    }
    Ok(Parsed {
        value: output,
        errors,
    })
}

pub fn parse_declarations(input: &str) -> Result<Parsed<BudgetVec<Declaration>>, Error> {
    parse_declarations_with_budget(input, &crate::memory()?)
}
pub fn parse_declarations_with_budget(
    input: &str,
    memory: &Budget,
) -> Result<Parsed<BudgetVec<Declaration>>, Error> {
    let components = parse_components_with_budget(input, memory)?;
    let mut parsed = declarations(&components.value, memory)?;
    parsed.errors = parsed.errors.saturating_add(components.errors);
    Ok(parsed)
}

#[cfg(test)]
mod tests;
