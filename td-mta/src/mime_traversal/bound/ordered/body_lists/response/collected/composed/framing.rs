//! Shared bounded framing; adapters retain original ownership and admission.
use super::{Error, Mode, Progress, Status};
use crate::{
    mime_traversal::{Media, Part},
    ports::Tick,
};
#[derive(Clone, Copy)]
pub(crate) enum List {
    Text,
    Html,
    Attachments,
}
#[derive(Clone, Copy)]
enum Property {
    List(List),
    HasAttachment,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Prepare,
    AfterStructure,
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
// Adapters own freshness domains beyond the original job (for example a file pin).
pub(crate) trait Source {
    fn charge(&mut self, now: Tick, steps: u64, output: u64, credit: &mut u8) -> Result<(), Error>;
    fn parts(&self) -> Result<&[Part], Error>;
    fn node(&self, index: usize) -> Result<Part, Error>;
    fn fragment(&self, index: usize) -> Result<&[u8], Error>;
    fn list(&self, list: List) -> &[u16];
    fn has_attachment(&self) -> bool;
}
pub(crate) struct Frame {
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
    properties: u8,
    follow_lists: bool,
    sub_parts: bool,
}
impl Frame {
    pub(crate) fn new(mode: Mode) -> Self {
        Self {
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
            properties: 15,
            follow_lists: false,
            sub_parts: true,
        }
    }
    pub(crate) fn selected_lists(properties: u8) -> Self {
        let mut frame = Self::new(Mode::Lists);
        frame.properties = properties & 15;
        if frame.properties == 0 {
            frame.phase = Phase::Complete;
        }
        frame
    }
    pub(crate) fn requested(structure: bool, properties: u8) -> Self {
        if !structure {
            return Self::selected_lists(properties);
        }
        let mut frame = Self::new(Mode::Structure);
        frame.properties = properties & 15;
        frame.follow_lists = frame.properties != 0;
        frame
    }
    pub(crate) fn with_sub_parts(mut self, sub_parts: bool) -> Self {
        self.sub_parts = sub_parts;
        self
    }
    fn next_property(&self, after: Option<List>) -> Option<Property> {
        let start = match after {
            None => 0,
            Some(List::Text) => 1,
            Some(List::Html) => 2,
            Some(List::Attachments) => 3,
        };
        if start == 0 && self.properties & 1 != 0 {
            Some(Property::List(List::Text))
        } else if start <= 1 && self.properties & 2 != 0 {
            Some(Property::List(List::Html))
        } else if start <= 2 && self.properties & 4 != 0 {
            Some(Property::List(List::Attachments))
        } else if self.properties & 8 != 0 {
            Some(Property::HasAttachment)
        } else {
            None
        }
    }
    fn begin_property(&mut self, source: &impl Source, property: Property, first: bool) {
        match property {
            Property::List(list) => {
                self.list = list;
                let bytes: &'static [u8] = match (list, first) {
                    (List::Text, true) => b"\"textBody\":[",
                    (List::Text, false) => b",\"textBody\":[",
                    (List::Html, true) => b"\"htmlBody\":[",
                    (List::Html, false) => b",\"htmlBody\":[",
                    (List::Attachments, true) => b"\"attachments\":[",
                    (List::Attachments, false) => b",\"attachments\":[",
                };
                self.begin(Bytes::Static(bytes), Phase::ListPrepare);
            }
            Property::HasAttachment => self.begin(
                Bytes::Static(match (first, source.has_attachment()) {
                    (true, true) => b"\"hasAttachment\":true",
                    (true, false) => b"\"hasAttachment\":false",
                    (false, true) => b",\"hasAttachment\":true",
                    (false, false) => b",\"hasAttachment\":false",
                }),
                Phase::Complete,
            ),
        }
    }
    // Legacy label only; combined requested frames use mode as the active walk.
    // Requested wrappers expose immutable Properties and discard this value.
    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }
    pub(crate) fn complete(&self) -> bool {
        self.phase == Phase::Complete
    }
    pub(crate) fn check_deadline(
        &mut self,
        source: &mut impl Source,
        now: Tick,
    ) -> Result<(), Error> {
        source.charge(now, 0, 0, &mut self.credit)
    }
    fn begin(&mut self, bytes: Bytes, after: Phase) {
        self.bytes = bytes;
        self.offset = 0;
        self.after = after;
        self.phase = Phase::Copy;
    }
    fn bytes<'v>(&self, source: &'v impl Source) -> Result<&'v [u8], Error> {
        match self.bytes {
            Bytes::Static(bytes) => Ok(bytes),
            Bytes::Fragment(index) => source.fragment(index),
        }
    }
    fn after_part(&self, source: &impl Source) -> Result<Phase, Error> {
        if self.mode == Mode::Structure
            && self.opened == 0
            && self.index.checked_add(1).ok_or(Error::InvalidState)? == source.parts()?.len()
        {
            Ok(Phase::Complete)
        } else {
            Ok(Phase::Advance)
        }
    }
    pub(crate) fn step(
        &mut self,
        source: &mut impl Source,
        now: Tick,
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        source.charge(now, 1, 0, &mut self.credit)?;
        let mut written = 0;
        match self.phase {
            Phase::Prepare => match self.mode {
                Mode::Structure => {
                    self.begin(Bytes::Static(b"\"bodyStructure\":"), Phase::StartPart)
                }
                Mode::Lists => {
                    let property = self.next_property(None).ok_or(Error::InvalidState)?;
                    self.begin_property(source, property, true);
                }
            },
            Phase::AfterStructure => {
                let property = self.next_property(None).ok_or(Error::InvalidState)?;
                self.begin_property(source, property, false);
            }
            Phase::StartPart => {
                let part = source.node(self.index)?;
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
                if !self.sub_parts {
                    if self.mode == Mode::Structure && (self.opened != 0 || self.index != 0) {
                        return Err(Error::InvalidState);
                    }
                    self.begin(
                        Bytes::Static(b"}"),
                        if self.mode == Mode::Structure {
                            Phase::Complete
                        } else {
                            Phase::Advance
                        },
                    );
                } else {
                    let part = source.node(self.index)?;
                    let empty = source.fragment(self.index)?.is_empty();
                    if self.mode == Mode::Structure && part.media == Media::Multipart {
                        let next = self.index.checked_add(1).ok_or(Error::InvalidState)?;
                        let child = source.parts()?.get(next);
                        if child.is_some_and(|child| child.parent == part.ordinal) {
                            let target = self
                                .parents
                                .get_mut(self.opened)
                                .ok_or(Error::InvalidState)?;
                            *target = part.ordinal;
                            self.opened = self.opened.checked_add(1).ok_or(Error::InvalidState)?;
                            self.begin(
                                Bytes::Static(if empty {
                                    b"\"subParts\":["
                                } else {
                                    b",\"subParts\":["
                                }),
                                Phase::Descend,
                            );
                        } else {
                            return Err(Error::InvalidState);
                        }
                    } else {
                        self.begin(
                            Bytes::Static(if empty {
                                b"\"subParts\":null}"
                            } else {
                                b",\"subParts\":null}"
                            }),
                            self.after_part(source)?,
                        );
                    }
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
                    match source.parts()?.get(next) {
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
                self.begin(Bytes::Static(b"]}"), self.after_part(source)?);
            }
            Phase::ListPrepare => {
                if let Some(ordinal) = source.list(self.list).get(self.position).copied() {
                    self.index = usize::from(ordinal)
                        .checked_sub(1)
                        .ok_or(Error::InvalidState)?;
                    self.phase = Phase::StartPart;
                } else {
                    let after = if self.next_property(Some(self.list)).is_some() {
                        Phase::NextList
                    } else {
                        Phase::Complete
                    };
                    self.begin(Bytes::Static(b"]"), after);
                }
            }
            Phase::NextList => {
                self.position = 0;
                let property = self
                    .next_property(Some(self.list))
                    .ok_or(Error::InvalidState)?;
                self.begin_property(source, property, false);
            }
            Phase::Copy => {
                let length = self.bytes(source)?.len();
                let remaining = length.checked_sub(self.offset).ok_or(Error::InvalidState)?;
                written = remaining.min(output.len()).min(64);
                source.charge(now, 0, written as u64, &mut self.credit)?;
                let end = self
                    .offset
                    .checked_add(written)
                    .ok_or(Error::InvalidState)?;
                // Reborrow the same immutable generated bytes only after funding.
                let bytes = self
                    .bytes(source)?
                    .get(self.offset..end)
                    .ok_or(Error::InvalidState)?;
                output
                    .get_mut(..written)
                    .ok_or(Error::InvalidState)?
                    .copy_from_slice(bytes);
                self.offset = end;
                if end == length {
                    self.phase = self.after;
                    if self.phase == Phase::Complete && self.follow_lists {
                        self.follow_lists = false;
                        self.mode = Mode::Lists;
                        self.phase = Phase::AfterStructure;
                    }
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
}
