//! Bounded character-reference decoding after an already-consumed ampersand.
const MAX_NAME: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Names {
    /// The renderer's existing named-reference table.
    Html,
    /// amp, lt, gt, quot, apos and nbsp only, with their HTML legacy flags.
    Basic,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Legacy ambiguity is interpreted differently in text and attributes.
pub enum Context {
    Text,
    Attribute,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    /// Poll again, retaining any unconsumed input/last pair.
    Yield,
    /// Supply another scalar or explicit EOF.
    NeedInput,
    /// Handle the final scalar, then retire this reference.
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// One bounded turn; an emitted scalar can accompany completion.
pub struct Progress {
    pub consumed: bool,
    pub scalar: Option<char>,
    pub status: Status,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    State,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid character-reference state")
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug)]
enum Phase {
    Start,
    Hash,
    Number { hex: bool, any: bool, value: u32 },
    Name,
    Resolve { candidate: usize, exact: bool },
    Flush,
    Complete,
}
/// Pure parser state: no source, allocation, I/O or admission authority.
/// Retain an unconsumed input/last pair. A consumed last scalar closes input;
/// subsequent drain polls need no more input. A nonfinal None requests input.
/// Complete may carry the final output scalar, and leaves the next source
/// character unconsumed unless a decoded reference consumed its semicolon.
#[derive(Clone, Debug)]
pub struct Reference {
    phase: Phase,
    names: Names,
    context: Context,
    bytes: [u8; MAX_NAME + 1],
    len: usize,
    position: usize,
    replacement: Option<char>,
    ended: bool,
    failed: bool,
}
impl Reference {
    /// Begin immediately after the caller consumes an ampersand.
    pub const fn new(names: Names, context: Context) -> Self {
        let bytes = [b'&'; MAX_NAME + 1];
        Self {
            phase: Phase::Start,
            names,
            context,
            bytes,
            len: 1,
            position: 0,
            replacement: None,
            ended: false,
            failed: false,
        }
    }
    /// Advance one turn. None with last=true denotes EOF.
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
            scalar,
            status: if matches!(self.phase, Phase::Complete) {
                Status::Complete
            } else {
                Status::Yield
            },
        }
    }
    fn push(&mut self, value: char) -> Result<(), Error> {
        *self.bytes.get_mut(self.len).ok_or(Error::State)? =
            u8::try_from(u32::from(value)).map_err(|_| Error::State)?;
        self.len = self.len.checked_add(1).ok_or(Error::State)?;
        Ok(())
    }
    fn lookup(&self, count: usize) -> Result<Option<(char, bool)>, Error> {
        let end = count.checked_add(1).ok_or(Error::State)?;
        let name = std::str::from_utf8(self.bytes.get(1..end).ok_or(Error::State)?)
            .map_err(|_| Error::State)?;
        Ok(match self.names {
            Names::Html => crate::lookup_entity(name),
            Names::Basic if matches!(name, "amp" | "lt" | "gt" | "quot" | "apos" | "nbsp") => {
                crate::lookup_entity(name)
            }
            Names::Basic => None,
        })
    }
    fn step(&mut self, input: Option<char>, last: bool) -> Result<Progress, Error> {
        if matches!(self.phase, Phase::Complete) {
            return Ok(self.progress(false, None));
        }
        if matches!(self.phase, Phase::Flush) {
            let scalar = if let Some(value) = self.replacement.take() {
                Some(value)
            } else if self.position < self.len {
                let value = char::from(*self.bytes.get(self.position).ok_or(Error::State)?);
                self.position = self.position.checked_add(1).ok_or(Error::State)?;
                Some(value)
            } else {
                None
            };
            if self.position == self.len && self.replacement.is_none() {
                self.phase = Phase::Complete;
            }
            return Ok(self.progress(false, scalar));
        }
        if input.is_none() && !last {
            return Ok(Progress {
                consumed: false,
                scalar: None,
                status: Status::NeedInput,
            });
        }
        let mut consumed = false;
        match self.phase {
            Phase::Start => match input {
                Some('#') => {
                    self.push('#')?;
                    self.phase = Phase::Hash;
                    consumed = true;
                }
                Some(value) if value.is_ascii_alphanumeric() => {
                    self.push(value)?;
                    self.phase = Phase::Name;
                    consumed = true;
                }
                _ => self.phase = Phase::Flush,
            },
            Phase::Hash => {
                let hex = matches!(input, Some('x' | 'X'));
                if let Some(value) = input.filter(|_| hex) {
                    self.push(value)?;
                    consumed = true;
                }
                self.phase = Phase::Number {
                    hex,
                    any: false,
                    value: 0,
                };
            }
            Phase::Number { hex, any, value } => {
                let digit = input.and_then(|c| c.to_digit(if hex { 16 } else { 10 }));
                if let Some(digit) = digit {
                    self.phase = Phase::Number {
                        hex,
                        any: true,
                        value: value
                            .saturating_mul(if hex { 16 } else { 10 })
                            .saturating_add(digit)
                            .min(0x11_0000),
                    };
                    consumed = true;
                } else {
                    if any {
                        self.replacement = Some(crate::from_codepoint(value));
                        self.position = self.len;
                        consumed = input == Some(';');
                    }
                    self.phase = Phase::Flush;
                }
            }
            Phase::Name => {
                if let Some(value) =
                    input.filter(|c| c.is_ascii_alphanumeric() && self.len <= MAX_NAME)
                {
                    self.push(value)?;
                    consumed = true;
                } else {
                    self.phase = Phase::Resolve {
                        candidate: self.len.checked_sub(1).ok_or(Error::State)?,
                        exact: input == Some(';'),
                    };
                }
            }
            Phase::Resolve { candidate, exact } => {
                if exact && input != Some(';') {
                    return Err(Error::State);
                }
                if candidate <= 1 {
                    self.phase = Phase::Flush;
                } else {
                    let found = self.lookup(candidate)?;
                    let legacy_allowed = self.context == Context::Text
                        || (candidate.checked_add(1) == Some(self.len)
                            && !input.is_some_and(|c| c.is_ascii_alphanumeric() || c == '='));
                    if let Some((value, _)) =
                        found.filter(|(_, legacy)| exact || (*legacy && legacy_allowed))
                    {
                        self.replacement = Some(value);
                        self.position = candidate.checked_add(1).ok_or(Error::State)?;
                        self.phase = Phase::Flush;
                        consumed = exact;
                    } else {
                        self.phase = Phase::Resolve {
                            candidate: candidate.checked_sub(1).ok_or(Error::State)?,
                            exact: false,
                        };
                    }
                }
            }
            Phase::Flush | Phase::Complete => return Err(Error::State),
        }
        Ok(self.progress(consumed, None))
    }
}
const _: () = assert!(std::mem::size_of::<Reference>() <= 128);
const _: () = assert!(crate::ENTITIES.len() <= 512);

#[cfg(test)]
#[path = "reference_tests.rs"]
mod tests;
