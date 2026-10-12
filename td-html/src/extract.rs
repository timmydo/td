//! Fixed-state HTML text extraction, without rendering or a document tree.
use crate::reference::{Context, Names, Reference, Status as ReferenceStatus};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    /// Poll again, retaining unconsumed input.
    Yield,
    /// Supply a scalar or explicit EOF.
    NeedInput,
    /// Handle any final scalar before retiring the cursor.
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub consumed: bool,
    pub scalar: Option<char>,
    pub status: Status,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Diagnostics {
    pub malformed_markup: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    State,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid HTML extraction state")
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Other,
    Block,
    Head,
    Body,
    Title,
    Script,
    Style,
}
impl Kind {
    fn raw_name(self) -> &'static [u8] {
        match self {
            Self::Script => b"script",
            Self::Style => b"style",
            Self::Title => b"title",
            _ => b"",
        }
    }
}
#[derive(Clone, Copy)]
enum Phase {
    Text,
    Open,
    CloseOpen,
    Name,
    BeforeAttribute,
    Attribute,
    AfterAttribute,
    BeforeValue,
    Value(Option<char>),
    AfterQuotedValue,
    Slash,
    Declaration,
    Bang,
    Dash,
    Comment { dashes: u8, start: bool, bang: bool },
    Raw(u8),
    Complete,
}
/// Pure parser progress. The caller owns ordered input, source identity,
/// work/output admission and publication. Each poll consumes at most one
/// input scalar and emits at most one output scalar. Retain unconsumed input.
/// A consumed last scalar closes input; drain with None until Complete.
/// Complete may carry a final scalar. Diagnostics are passive observations.
#[derive(Clone)]
pub struct Extractor {
    phase: Phase,
    reference: Option<Reference>,
    name: [u8; 10],
    name_len: usize,
    name_long: bool,
    kind: Kind,
    raw: Kind,
    closing: bool,
    attribute: bool,
    head: bool,
    ended: bool,
    failed: bool,
    diagnostics: Diagnostics,
}
impl Default for Extractor {
    fn default() -> Self {
        Self::new()
    }
}
impl Extractor {
    pub const fn new() -> Self {
        Self {
            phase: Phase::Text,
            reference: None,
            name: [0; 10],
            name_len: 0,
            name_long: false,
            kind: Kind::Other,
            raw: Kind::Other,
            closing: false,
            attribute: false,
            head: false,
            ended: false,
            failed: false,
            diagnostics: Diagnostics {
                malformed_markup: false,
            },
        }
    }
    pub fn diagnostics(&self) -> Diagnostics {
        self.diagnostics
    }
    pub fn poll(&mut self, input: Option<char>, last: bool) -> Result<Progress, Error> {
        if self.failed {
            return Err(Error::State);
        }
        let input = if self.ended { None } else { input };
        let last = self.ended || last;
        let result = self.step(input, last);
        if let Ok(progress) = result {
            if progress.consumed && last {
                self.ended = true;
            }
        } else {
            self.failed = true;
        }
        result
    }
    fn progress(&self, consumed: bool, scalar: Option<char>) -> Progress {
        Progress {
            consumed,
            scalar: scalar.map(|c| match c {
                '\u{a0}' => ' ',
                '\0' => '\u{fffd}',
                _ => c,
            }),
            status: if matches!(self.phase, Phase::Complete) {
                Status::Complete
            } else {
                Status::Yield
            },
        }
    }
    fn reset_name(&mut self) {
        self.name_len = 0;
        self.name_long = false;
    }
    fn push_name(&mut self, value: char) -> Result<(), Error> {
        if self.name_long {
            return Ok(());
        }
        if self.name_len == self.name.len() || !value.is_ascii() {
            self.name_long = true;
        } else {
            *self.name.get_mut(self.name_len).ok_or(Error::State)? =
                u8::try_from(u32::from(value.to_ascii_lowercase())).map_err(|_| Error::State)?;
            self.name_len = self.name_len.checked_add(1).ok_or(Error::State)?;
        }
        Ok(())
    }
    fn name(&self) -> Result<&str, Error> {
        if self.name_long {
            return Ok("");
        }
        std::str::from_utf8(self.name.get(..self.name_len).ok_or(Error::State)?)
            .map_err(|_| Error::State)
    }
    fn tag_kind(&self) -> Result<Kind, Error> {
        Ok(match self.name()? {
            "head" => Kind::Head,
            "body" => Kind::Body,
            "title" => Kind::Title,
            "script" => Kind::Script,
            "style" => Kind::Style,
            "address" | "article" | "aside" | "blockquote" | "br" | "div" | "dl" | "dt" | "dd"
            | "fieldset" | "figcaption" | "figure" | "footer" | "form" | "h1" | "h2" | "h3"
            | "h4" | "h5" | "h6" | "header" | "hr" | "li" | "main" | "nav" | "ol" | "p" | "pre"
            | "section" | "table" | "td" | "th" | "tr" | "ul" => Kind::Block,
            _ => Kind::Other,
        })
    }
    fn visible_attribute(&self) -> bool {
        self.attribute
            && !self.closing
            && !self.head
            && !matches!(self.kind, Kind::Head | Kind::Script | Kind::Style)
    }
    fn finish_tag(&mut self) -> Option<char> {
        let space = self.kind == Kind::Block && !self.head;
        if self.kind == Kind::Head {
            self.head = !self.closing;
        }
        if self.kind == Kind::Body && !self.closing {
            self.head = false;
        }
        if !self.closing
            && (matches!(self.kind, Kind::Script | Kind::Style)
                || (self.head && self.kind == Kind::Title))
        {
            self.raw = self.kind;
            self.phase = Phase::Raw(0);
        } else {
            self.phase = Phase::Text;
        }
        self.closing = false;
        space.then_some(' ')
    }
    fn malformed(&mut self) {
        self.diagnostics.malformed_markup = true;
    }
    fn step(&mut self, input: Option<char>, last: bool) -> Result<Progress, Error> {
        if matches!(self.phase, Phase::Complete) {
            return Ok(self.progress(false, None));
        }
        if let Some(reference) = self.reference.as_mut() {
            let p = reference.poll(input, last).map_err(|_| Error::State)?;
            if p.status == ReferenceStatus::Complete {
                self.reference = None;
            }
            let mut out = self.progress(p.consumed, p.scalar);
            if p.status == ReferenceStatus::NeedInput {
                out.status = Status::NeedInput;
            }
            return Ok(out);
        }
        let Some(value) = input else {
            if !last {
                return Ok(Progress {
                    consumed: false,
                    scalar: None,
                    status: Status::NeedInput,
                });
            }
            if !matches!(self.phase, Phase::Text) || self.head {
                self.malformed();
            }
            let scalar = if matches!(self.phase, Phase::Value(_)) && self.visible_attribute() {
                Some(' ')
            } else if matches!(self.phase, Phase::Open) && !self.head {
                Some('<')
            } else {
                None
            };
            self.phase = Phase::Complete;
            return Ok(self.progress(false, scalar));
        };
        if value == '\0' {
            self.malformed();
        }
        let mut consumed = true;
        let mut scalar = None;
        match self.phase {
            Phase::Text => match value {
                '<' => self.phase = Phase::Open,
                '&' if !self.head => {
                    self.reference = Some(Reference::new(Names::Basic, Context::Text))
                }
                _ if !self.head => scalar = Some(value),
                _ => {}
            },
            Phase::Open => match value {
                '/' => self.phase = Phase::CloseOpen,
                '!' => self.phase = Phase::Bang,
                '?' => self.phase = Phase::Declaration,
                c if c.is_ascii_alphabetic() => {
                    self.reset_name();
                    self.push_name(c)?;
                    self.closing = false;
                    self.phase = Phase::Name;
                }
                _ => {
                    self.malformed();
                    self.phase = Phase::Text;
                    if !self.head {
                        scalar = Some('<');
                    }
                    consumed = false;
                }
            },
            Phase::CloseOpen => {
                if value.is_ascii_alphabetic() {
                    self.reset_name();
                    self.push_name(value)?;
                    self.closing = true;
                    self.phase = Phase::Name;
                } else {
                    self.malformed();
                    self.phase = if value == '>' {
                        Phase::Text
                    } else {
                        Phase::Declaration
                    };
                }
            }
            Phase::Name => {
                if space(value) || matches!(value, '/' | '>') {
                    self.kind = self.tag_kind()?;
                    self.phase = Phase::BeforeAttribute;
                    consumed = false;
                } else {
                    if matches!(value, '<' | '\'' | '"' | '=' | '\0') {
                        self.malformed();
                    }
                    self.push_name(value)?;
                }
            }
            Phase::BeforeAttribute => match value {
                '>' => scalar = self.finish_tag(),
                '/' => self.phase = Phase::Slash,
                c if space(c) => {}
                _ => {
                    self.reset_name();
                    if matches!(value, '<' | '\'' | '"' | '=' | '\0') {
                        self.malformed();
                    }
                    self.push_name(value)?;
                    self.attribute = false;
                    self.phase = Phase::Attribute;
                }
            },
            Phase::Attribute => {
                if space(value) || matches!(value, '=' | '/' | '>') {
                    self.attribute = matches!(self.name()?, "alt" | "title");
                    self.phase = Phase::AfterAttribute;
                    consumed = false;
                } else {
                    if matches!(value, '<' | '\'' | '"' | '\0') {
                        self.malformed();
                    }
                    self.push_name(value)?;
                }
            }
            Phase::AfterAttribute => match value {
                '=' => self.phase = Phase::BeforeValue,
                c if space(c) => {}
                _ => {
                    self.phase = Phase::BeforeAttribute;
                    consumed = false;
                }
            },
            Phase::BeforeValue => match value {
                c if space(c) => {}
                '>' => {
                    self.malformed();
                    scalar = self.finish_tag();
                }
                c => {
                    let quote = matches!(c, '\'' | '"').then_some(c);
                    self.phase = Phase::Value(quote);
                    if self.visible_attribute() {
                        scalar = Some(' ');
                    }
                    consumed = quote.is_some();
                }
            },
            Phase::Value(quote) => {
                if value == '\0'
                    || (quote.is_none() && matches!(value, '<' | '\'' | '"' | '=' | '`'))
                {
                    self.malformed();
                }
                let end = quote.map_or_else(|| space(value) || value == '>', |q| value == q);
                if end {
                    if self.visible_attribute() {
                        scalar = Some(' ');
                    }
                    self.phase = if quote.is_some() {
                        Phase::AfterQuotedValue
                    } else {
                        Phase::BeforeAttribute
                    };
                    consumed = quote.is_some() || space(value);
                } else if self.visible_attribute() {
                    if value == '&' {
                        self.reference = Some(Reference::new(Names::Basic, Context::Attribute));
                    } else {
                        scalar = Some(value);
                    }
                }
            }
            Phase::AfterQuotedValue => {
                if !space(value) && !matches!(value, '/' | '>') {
                    self.malformed();
                }
                self.phase = Phase::BeforeAttribute;
                consumed = false;
            }
            Phase::Slash => {
                if value == '>' {
                    scalar = self.finish_tag();
                } else {
                    self.malformed();
                    self.phase = Phase::BeforeAttribute;
                    consumed = false;
                }
            }
            Phase::Bang => {
                self.phase = if value == '-' {
                    Phase::Dash
                } else {
                    Phase::Declaration
                };
                if value != '-' {
                    consumed = false;
                }
            }
            Phase::Dash => {
                if value == '-' {
                    self.phase = Phase::Comment {
                        dashes: 0,
                        start: true,
                        bang: false,
                    };
                } else {
                    self.phase = Phase::Declaration;
                    consumed = false;
                }
            }
            Phase::Declaration => {
                if value == '>' {
                    self.phase = Phase::Text;
                }
            }
            Phase::Comment {
                dashes,
                start,
                bang,
            } => {
                if value == '>' && (dashes == 2 || start || bang) {
                    if dashes != 2 || bang {
                        self.malformed();
                    }
                    self.phase = Phase::Text;
                } else {
                    self.phase = Phase::Comment {
                        dashes: if value == '-' {
                            if bang {
                                1
                            } else {
                                dashes.saturating_add(1).min(2)
                            }
                        } else {
                            0
                        },
                        start: start && dashes == 0 && value == '-',
                        bang: dashes == 2 && value == '!',
                    };
                }
            }
            Phase::Raw(position) => {
                let name = self.raw.raw_name();
                let end = name.len().checked_add(2).ok_or(Error::State)?;
                let pos = usize::from(position);
                if pos == end && (space(value) || matches!(value, '/' | '>')) {
                    self.kind = self.raw;
                    self.raw = Kind::Other;
                    self.closing = true;
                    self.phase = Phase::BeforeAttribute;
                    consumed = false;
                } else {
                    let expected = match pos {
                        0 => Some(b'<'),
                        1 => Some(b'/'),
                        _ => name.get(pos.checked_sub(2).ok_or(Error::State)?).copied(),
                    };
                    let matched =
                        expected.is_some_and(|c| value.to_ascii_lowercase() == char::from(c));
                    self.phase = Phase::Raw(if matched {
                        position.checked_add(1).ok_or(Error::State)?
                    } else if value == '<' {
                        1
                    } else {
                        0
                    });
                }
            }
            Phase::Complete => return Err(Error::State),
        }
        Ok(self.progress(consumed, scalar))
    }
}
fn space(value: char) -> bool {
    matches!(value, ' ' | '\t' | '\n' | '\r' | '\u{c}')
}
const _: () = assert!(std::mem::size_of::<Extractor>() <= 256);

#[cfg(test)]
#[path = "extract_tests.rs"]
mod tests;
