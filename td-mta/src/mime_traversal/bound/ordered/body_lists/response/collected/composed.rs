//! Private body metadata members; authenticated blob locators and publication follow.
pub mod retained;
use super::{Error, Serialized, View};
use crate::{admission::work::Charge, mime_traversal::Media, ports::Tick};
pub use td_json::string::{Progress, Status};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Structure,
    Lists,
}
#[derive(Clone, Copy)]
enum List {
    Text,
    Html,
    Attachments,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Prepare,
    StartPart,
    Fragment,
    Subparts,
    Descend,
    Advance,
    Close,
    ListPrepare,
    NextList,
    Copy,
    Complete,
}
#[derive(Clone, Copy)]
enum Bytes {
    Static(&'static [u8]),
    Fragment(usize),
}
/// Only complete original collection ownership enters the bounded composition.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::Cursor<'_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::Cursor<'_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use td_mta::{mime_traversal::bound::ordered::body_lists::response::collected::{View, composed::{Cursor, Mode}}, ports::Tick};
/// fn substitute(view: View<'_, '_, '_, '_>) { let _ = Cursor::new(view, Mode::Lists, Tick(1)); }
/// ```
pub struct Cursor<'a, 'w, 'n, 'c, 'o> {
    source: Serialized<'a, 'w, 'n, 'c, 'o>,
    mode: Mode,
    phase: Phase,
    index: usize,
    list: List,
    position: usize,
    parents: [u16; crate::mime_traversal::MAX_DEPTH],
    opened: usize,
    bytes: Bytes,
    offset: usize,
    after: Phase,
    credit: u8,
    failure: Option<Error>,
}
impl<'a, 'w, 'n, 'c, 'o> Cursor<'a, 'w, 'n, 'c, 'o> {
    pub fn new(
        mut source: Serialized<'a, 'w, 'n, 'c, 'o>,
        mode: Mode,
        now: Tick,
    ) -> Result<Self, Error> {
        source.check_deadline(now)?;
        Ok(Self {
            source,
            mode,
            phase: Phase::Prepare,
            index: 0,
            list: List::Text,
            position: 0,
            parents: [0; crate::mime_traversal::MAX_DEPTH],
            opened: 0,
            bytes: Bytes::Static(b""),
            offset: 0,
            after: Phase::Complete,
            credit: 0,
            failure: None,
        })
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn charge(&mut self, now: Tick, steps: u64, output: u64) -> Result<(), Error> {
        let structure = &mut self.source.projected.structure;
        structure
            .budget
            .charge(structure.work, now, 0, steps, &mut self.credit)
            .map_err(Error::Admission)?;
        structure
            .work
            .charge(
                now,
                Charge {
                    output_bytes: output,
                    ..Charge::default()
                },
            )
            .map_err(|error| Error::Admission(crate::nfc::Error::Work(error)))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.charge(now, 0, 0);
        self.outcome(result)
    }
    pub fn value(&self) -> Option<(Mode, View<'_, '_, '_, 'o>)> {
        if self.failure.is_some() || self.phase != Phase::Complete {
            return None;
        }
        Some((self.mode, self.source.value()?))
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.phase == Phase::Complete {
            return Ok(Progress {
                written: 0,
                status: Status::Complete,
            });
        }
        self.check_deadline(now)?;
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        let result = self.step(now, output);
        self.outcome(result)
    }
    fn begin(&mut self, bytes: Bytes, after: Phase) {
        self.bytes = bytes;
        self.offset = 0;
        self.after = after;
        self.phase = Phase::Copy;
    }
    fn bytes(&self) -> Result<&[u8], Error> {
        match self.bytes {
            Bytes::Static(bytes) => Ok(bytes),
            Bytes::Fragment(index) => Ok(self
                .source
                .cells
                .get(index)
                .and_then(|cell| cell.retained)
                .ok_or(Error::InvalidState)?
                .fragment),
        }
    }
    fn node(&self) -> Result<crate::mime_traversal::Part, Error> {
        let part = self
            .source
            .projected
            .structure
            .parts()?
            .get(self.index)
            .copied()
            .ok_or(Error::InvalidState)?;
        let retained = self
            .source
            .cells
            .get(self.index)
            .and_then(|cell| cell.retained)
            .ok_or(Error::InvalidState)?;
        let ordinal = self
            .index
            .checked_add(1)
            .and_then(|i| u16::try_from(i).ok())
            .ok_or(Error::InvalidState)?;
        if part.ordinal != ordinal || retained.end.part != part {
            return Err(Error::InvalidState);
        }
        Ok(part)
    }
    fn list(&self) -> &[u16] {
        match self.list {
            List::Text => self.source.projected.lists.text,
            List::Html => self.source.projected.lists.html,
            List::Attachments => self.source.projected.lists.attachments,
        }
    }
    fn after_part(&self) -> Result<Phase, Error> {
        if self.mode == Mode::Structure
            && self.opened == 0
            && self.index.checked_add(1).ok_or(Error::InvalidState)?
                == self.source.projected.structure.parts()?.len()
        {
            Ok(Phase::Complete)
        } else {
            Ok(Phase::Advance)
        }
    }
    fn step(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.charge(now, 1, 0)?;
        let mut written = 0;
        match self.phase {
            Phase::Prepare => match self.mode {
                Mode::Structure => {
                    self.begin(Bytes::Static(b"\"bodyStructure\":"), Phase::StartPart)
                }
                Mode::Lists => self.begin(Bytes::Static(b"\"textBody\":["), Phase::ListPrepare),
            },
            Phase::StartPart => {
                let part = self.node()?;
                if self.mode == Mode::Structure {
                    let parent = if self.opened == 0 {
                        0
                    } else {
                        *self
                            .parents
                            .get(self.opened - 1)
                            .ok_or(Error::InvalidState)?
                    };
                    if usize::from(part.depth)
                        != self.opened.checked_add(1).ok_or(Error::InvalidState)?
                        || part.parent != parent
                    {
                        return Err(Error::InvalidState);
                    }
                } else if part.media == Media::Multipart {
                    return Err(Error::InvalidState);
                }
                self.begin(
                    Bytes::Static(if self.mode == Mode::Lists && self.position != 0 {
                        b",{"
                    } else {
                        b"{"
                    }),
                    Phase::Fragment,
                );
            }
            Phase::Fragment => self.begin(Bytes::Fragment(self.index), Phase::Subparts),
            Phase::Subparts => {
                let part = self.node()?;
                if self.mode == Mode::Structure && part.media == Media::Multipart {
                    let next = self.index.checked_add(1).ok_or(Error::InvalidState)?;
                    let child = self.source.projected.structure.parts()?.get(next);
                    if child.is_some_and(|child| child.parent == part.ordinal) {
                        let target = self
                            .parents
                            .get_mut(self.opened)
                            .ok_or(Error::InvalidState)?;
                        *target = part.ordinal;
                        self.opened = self.opened.checked_add(1).ok_or(Error::InvalidState)?;
                        self.begin(Bytes::Static(b",\"subParts\":["), Phase::Descend);
                    } else {
                        return Err(Error::InvalidState);
                    }
                } else {
                    self.begin(Bytes::Static(b",\"subParts\":null}"), self.after_part()?);
                }
            }
            Phase::Descend => {
                self.index = self.index.checked_add(1).ok_or(Error::InvalidState)?;
                self.phase = Phase::StartPart;
            }
            Phase::Advance => match self.mode {
                Mode::Lists => {
                    self.position = self.position.checked_add(1).ok_or(Error::InvalidState)?;
                    self.phase = Phase::ListPrepare;
                }
                Mode::Structure => {
                    let next = self.index.checked_add(1).ok_or(Error::InvalidState)?;
                    match self.source.projected.structure.parts()?.get(next) {
                        Some(part)
                            if usize::from(part.depth)
                                == self.opened.checked_add(1).ok_or(Error::InvalidState)?
                                && self.opened != 0 =>
                        {
                            self.index = next;
                            self.begin(Bytes::Static(b","), Phase::StartPart);
                        }
                        Some(part) if usize::from(part.depth) <= self.opened => {
                            self.phase = Phase::Close
                        }
                        None if self.opened != 0 => self.phase = Phase::Close,
                        None => return Err(Error::InvalidState),
                        _ => return Err(Error::InvalidState),
                    }
                }
            },
            Phase::Close => {
                self.opened = self.opened.checked_sub(1).ok_or(Error::InvalidState)?;
                self.begin(Bytes::Static(b"]}"), self.after_part()?);
            }
            Phase::ListPrepare => {
                if let Some(ordinal) = self.list().get(self.position).copied() {
                    self.index = usize::from(ordinal)
                        .checked_sub(1)
                        .ok_or(Error::InvalidState)?;
                    self.phase = Phase::StartPart;
                } else {
                    self.begin(Bytes::Static(b"]"), Phase::NextList);
                }
            }
            Phase::NextList => {
                self.position = 0;
                match self.list {
                    List::Text => {
                        self.list = List::Html;
                        self.begin(Bytes::Static(b",\"htmlBody\":["), Phase::ListPrepare);
                    }
                    List::Html => {
                        self.list = List::Attachments;
                        self.begin(Bytes::Static(b",\"attachments\":["), Phase::ListPrepare);
                    }
                    List::Attachments => self.begin(
                        Bytes::Static(if self.source.projected.lists.has_attachment {
                            b",\"hasAttachment\":true"
                        } else {
                            b",\"hasAttachment\":false"
                        }),
                        Phase::Complete,
                    ),
                }
            }
            Phase::Copy => {
                let length = self.bytes()?.len();
                let remaining = length.checked_sub(self.offset).ok_or(Error::InvalidState)?;
                written = remaining.min(output.len()).min(64);
                self.charge(now, 0, written as u64)?;
                let end = self
                    .offset
                    .checked_add(written)
                    .ok_or(Error::InvalidState)?;
                // Reborrow the same immutable generated bytes only after funding.
                let bytes = self
                    .bytes()?
                    .get(self.offset..end)
                    .ok_or(Error::InvalidState)?;
                output
                    .get_mut(..written)
                    .ok_or(Error::InvalidState)?
                    .copy_from_slice(bytes);
                self.offset = end;
                if end == length {
                    self.phase = self.after;
                }
            }
            Phase::Complete => {}
        }
        Ok(Progress {
            written,
            status: if self.phase == Phase::Complete {
                Status::Complete
            } else {
                Status::Yield
            },
        })
    }
    pub fn finish(mut self, now: Tick) -> Result<Composed<'a, 'w, 'n, 'c, 'o>, Error> {
        self.check_deadline(now)?;
        if self.phase != Phase::Complete {
            return self.outcome(Err(Error::InvalidState));
        }
        Ok(Composed {
            source: self.source,
            mode: self.mode,
        })
    }
}
/// Complete emitted member composition, still provisional and not retained wire proof.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::Composed<'_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::Composed<'_, '_, '_, '_, '_>>();
/// ```
pub struct Composed<'a, 'w, 'n, 'c, 'o> {
    source: Serialized<'a, 'w, 'n, 'c, 'o>,
    mode: Mode,
}
impl<'w, 'n, 'c, 'o> Composed<'_, 'w, 'n, 'c, 'o> {
    pub fn value(&self) -> Option<(Mode, View<'_, '_, '_, 'o>)> {
        Some((self.mode, self.source.value()?))
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.source.check_deadline(now)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<
        (
            (Mode, View<'w, 'n, 'c, 'o>),
            &'w mut crate::admission::work::Meter,
            &'w mut crate::nfc::HeaderBudget,
        ),
        Error,
    > {
        let (view, work, budget) = self.source.finish(now)?;
        Ok(((self.mode, view), work, budget))
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_, '_, '_, '_>>()
        + std::mem::size_of::<crate::nfc::HeaderBudget>()
        <= 1024
);
const _: () = assert!(std::mem::size_of::<Composed<'_, '_, '_, '_, '_>>() <= 256);

#[cfg(test)]
mod tests;
