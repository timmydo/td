//! Provisional groups and mailboxes with deterministic raw-item recovery.
pub use crate::header_message_ids::Extent;
use crate::{
    admission::work::{Charge, Meter, Stop},
    header_address_items as items, header_cfws, header_mailbox, header_phrase,
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    NestingLimit,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NestingLimit => f.write_str("address list comment nesting limit"),
            Self::Work(error) => write!(f, "address list work: {error}"),
            Self::InterpretationLimit => f.write_str("header interpretation limit"),
            Self::InvalidState => f.write_str("invalid address list cursor state"),
        }
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Address {
    Parsed(header_mailbox::Mailbox),
    Raw(Extent),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    BeginGroup(Option<Extent>),
    Mailbox(Address),
    EndGroup,
    Complete,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Group {
    None,
    Named,
    Unnamed,
}
enum Phase<'a> {
    Item,
    Empty(header_cfws::Cursor<'a>),
    GroupName(header_phrase::Cursor<'a>),
    CloseForNamed,
    BeginNamed,
    Mailbox(header_mailbox::Cursor<'a>),
    TrimStart,
    TrimEnd,
    EmitMailbox(Address),
    AfterItem,
    Complete,
}
/// Every event remains provisional until the whole field reaches Complete.
pub struct Cursor<'a> {
    source: &'a [u8],
    items: items::Cursor<'a>,
    phase: Phase<'a>,
    group: Group,
    item: Option<items::Item>,
    candidate: Extent,
    group_name: Option<Extent>,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub const fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            items: items::Cursor::new(source),
            phase: Phase::Item,
            group: Group::None,
            item: None,
            candidate: Extent { start: 0, end: 0 },
            group_name: None,
            failure: None,
        }
    }
    fn slice(&self, extent: Extent) -> Result<&'a [u8], Error> {
        self.source
            .get(extent.start..extent.end)
            .ok_or(Error::InvalidState)
    }
    fn item(&self) -> Result<items::Item, Error> {
        self.item.ok_or(Error::InvalidState)
    }
    fn empty(&mut self) -> Result<(), Error> {
        self.phase = Phase::Empty(header_cfws::Cursor::new(self.slice(self.candidate)?, 0));
        Ok(())
    }
    fn mailbox(&mut self) -> Result<(), Error> {
        self.phase = Phase::Mailbox(header_mailbox::Cursor::new(self.slice(self.candidate)?));
        Ok(())
    }
    fn offset(&self, extent: Extent) -> Result<Extent, Error> {
        let start = self
            .candidate
            .start
            .checked_add(extent.start)
            .ok_or(Error::InvalidState)?;
        let end = self
            .candidate
            .start
            .checked_add(extent.end)
            .ok_or(Error::InvalidState)?;
        if start > end || end > self.candidate.end {
            return Err(Error::InvalidState);
        }
        Ok(Extent { start, end })
    }
    fn emit(&mut self, mailbox: Address) -> Status {
        if self.group == Group::None {
            self.group = Group::Unnamed;
            self.phase = Phase::EmitMailbox(mailbox);
            Status::BeginGroup(None)
        } else {
            self.phase = Phase::AfterItem;
            Status::Mailbox(mailbox)
        }
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        let result = work
            .charge(
                now,
                Charge {
                    records: 1,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)
            .and_then(|()| self.step(now, work));
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        match &mut self.phase {
            Phase::Item => {
                let status = self.items.poll(now, work).map_err(|error| match error {
                    items::Error::NestingLimit => Error::NestingLimit,
                    items::Error::Work(stop) => Error::Work(stop),
                    items::Error::InvalidState => Error::InvalidState,
                })?;
                match status {
                    items::Status::Yield => {}
                    items::Status::Item(item) => {
                        self.candidate = Extent {
                            start: item.start,
                            end: item.end,
                        };
                        self.item = Some(item);
                        self.empty()?;
                    }
                    items::Status::Complete => {
                        if self.group != Group::None {
                            return Err(Error::InvalidState);
                        }
                        self.phase = Phase::Complete;
                        return Ok(Status::Complete);
                    }
                }
            }
            Phase::Empty(cursor) => match cursor.poll(now, work) {
                Ok(header_cfws::Status::Complete(end)) => {
                    if end.position
                        == self
                            .candidate
                            .end
                            .checked_sub(self.candidate.start)
                            .ok_or(Error::InvalidState)?
                    {
                        self.phase = Phase::AfterItem;
                    } else if self.group != Group::Named && self.item()?.colon.is_some() {
                        let colon = self.item()?.colon.ok_or(Error::InvalidState)?;
                        let name = Extent {
                            start: self.candidate.start,
                            end: colon,
                        };
                        self.group_name = Some(name);
                        self.phase =
                            Phase::GroupName(header_phrase::Cursor::new(self.slice(name)?));
                    } else {
                        self.mailbox()?;
                    }
                }
                Ok(_) => {}
                Err(header_cfws::Error::Malformed) => self.phase = Phase::TrimStart,
                Err(header_cfws::Error::NestingLimit) => return Err(Error::NestingLimit),
                Err(header_cfws::Error::Work(stop)) => return Err(Error::Work(stop)),
                Err(header_cfws::Error::InvalidState) => return Err(Error::InvalidState),
                Err(header_cfws::Error::InterpretationLimit) => {
                    return Err(Error::InterpretationLimit)
                }
            },
            Phase::GroupName(cursor) => match cursor.poll(now, work) {
                Ok(header_phrase::Status::Complete(_)) => {
                    let colon = self.item()?.colon.ok_or(Error::InvalidState)?;
                    self.candidate.start = colon.checked_add(1).ok_or(Error::InvalidState)?;
                    self.phase = if self.group == Group::Unnamed {
                        Phase::CloseForNamed
                    } else {
                        Phase::BeginNamed
                    };
                }
                Ok(_) => {}
                Err(header_phrase::Error::Malformed) => {
                    self.group_name = None;
                    self.phase = Phase::TrimStart;
                }
                Err(header_phrase::Error::NestingLimit) => return Err(Error::NestingLimit),
                Err(header_phrase::Error::Work(stop)) => return Err(Error::Work(stop)),
                Err(header_phrase::Error::InvalidState) => return Err(Error::InvalidState),
                Err(header_phrase::Error::InterpretationLimit) => {
                    return Err(Error::InterpretationLimit)
                }
            },
            Phase::CloseForNamed => {
                self.group = Group::None;
                self.phase = Phase::BeginNamed;
                return Ok(Status::EndGroup);
            }
            Phase::BeginNamed => {
                let name = self.group_name.take().ok_or(Error::InvalidState)?;
                self.group = Group::Named;
                self.empty()?;
                return Ok(Status::BeginGroup(Some(name)));
            }
            Phase::Mailbox(cursor) => match cursor.poll(now, work) {
                Ok(header_mailbox::Status::Yield) => {}
                Ok(header_mailbox::Status::Complete(mailbox)) => {
                    let name = match mailbox.name {
                        None => None,
                        Some(header_mailbox::Name::Phrase(extent)) => {
                            Some(header_mailbox::Name::Phrase(self.offset(extent)?))
                        }
                        Some(header_mailbox::Name::Comment(extent)) => {
                            Some(header_mailbox::Name::Comment(self.offset(extent)?))
                        }
                    };
                    let address = self.offset(mailbox.address)?;
                    return Ok(
                        self.emit(Address::Parsed(header_mailbox::Mailbox { name, address }))
                    );
                }
                Err(header_mailbox::Error::Malformed) => self.phase = Phase::TrimStart,
                Err(header_mailbox::Error::NestingLimit) => return Err(Error::NestingLimit),
                Err(header_mailbox::Error::Work(stop)) => return Err(Error::Work(stop)),
                Err(header_mailbox::Error::InvalidState) => return Err(Error::InvalidState),
                Err(header_mailbox::Error::InterpretationLimit) => {
                    return Err(Error::InterpretationLimit)
                }
            },
            Phase::TrimStart | Phase::TrimEnd => {
                if self.candidate.start > self.candidate.end {
                    return Err(Error::InvalidState);
                }
                if self.candidate.start == self.candidate.end {
                    self.phase = Phase::AfterItem;
                } else {
                    let leading = matches!(self.phase, Phase::TrimStart);
                    let position = if leading {
                        self.candidate.start
                    } else {
                        self.candidate
                            .end
                            .checked_sub(1)
                            .ok_or(Error::InvalidState)?
                    };
                    work.charge(
                        now,
                        Charge {
                            io_bytes: 1,
                            ..Charge::default()
                        },
                    )
                    .map_err(Error::Work)?;
                    let byte = self
                        .source
                        .get(position)
                        .copied()
                        .ok_or(Error::InvalidState)?;
                    if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
                        if leading {
                            self.candidate.start = self
                                .candidate
                                .start
                                .checked_add(1)
                                .ok_or(Error::InvalidState)?;
                        } else {
                            self.candidate.end = position;
                        }
                    } else if leading {
                        self.phase = Phase::TrimEnd;
                    } else {
                        return Ok(self.emit(Address::Raw(self.candidate)));
                    }
                }
            }
            Phase::EmitMailbox(mailbox) => {
                let mailbox = *mailbox;
                self.phase = Phase::AfterItem;
                return Ok(Status::Mailbox(mailbox));
            }
            Phase::AfterItem => {
                let separator = self.item()?.separator;
                self.phase = Phase::Item;
                if matches!(
                    separator,
                    items::Separator::Semicolon | items::Separator::End
                ) && self.group != Group::None
                {
                    self.group = Group::None;
                    return Ok(Status::EndGroup);
                }
            }
            Phase::Complete => return Ok(Status::Complete),
        }
        Ok(Status::Yield)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::ports::Deadline;
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 50_000_000,
                records: 50_000_000,
                ..Charge::default()
            },
        )
    }
    fn events(source: &[u8]) -> Result<Vec<Status>, Error> {
        let mut cursor = Cursor::new(source);
        assert!(std::mem::size_of_val(&cursor) <= 768);
        let mut meter = work();
        let mut events = Vec::new();
        for _ in 0..2_000_000 {
            let before = meter.remaining();
            let step = cursor.poll(Tick(1), &mut meter);
            let after = meter.remaining();
            assert!(before.io_bytes - after.io_bytes <= 161);
            assert!(before.records - after.records <= 35);
            assert_eq!(before.output_bytes, after.output_bytes);
            match step? {
                Status::Complete => {
                    assert_eq!(cursor.poll(Tick(100), &mut meter), Ok(Status::Complete));
                    assert_eq!(after, meter.remaining());
                    return Ok(events);
                }
                Status::Yield => {}
                status => events.push(status),
            }
        }
        panic!("address list did not finish");
    }
    fn text(source: &[u8], extent: Extent) -> String {
        String::from_utf8_lossy(&source[extent.start..extent.end]).into_owned()
    }
    fn labels(source: &[u8]) -> Vec<String> {
        events(source)
            .unwrap()
            .into_iter()
            .map(|event| match event {
                Status::BeginGroup(None) => "begin unnamed".to_owned(),
                Status::BeginGroup(Some(name)) => format!("begin {}", text(source, name)),
                Status::EndGroup => "end".to_owned(),
                Status::Mailbox(Address::Raw(extent)) => format!("raw {}", text(source, extent)),
                Status::Mailbox(Address::Parsed(mailbox)) => {
                    let name = match mailbox.name {
                        None => "-".to_owned(),
                        Some(header_mailbox::Name::Phrase(extent)) => {
                            format!("phrase {}", text(source, extent))
                        }
                        Some(header_mailbox::Name::Comment(extent)) => {
                            format!("comment {}", text(source, extent))
                        }
                    };
                    format!("mail {} | {name}", text(source, mailbox.address))
                }
                _ => panic!("unexpected retained status"),
            })
            .collect()
    }
    #[test]
    fn named_and_consecutive_unnamed_mailboxes_keep_absolute_extents() {
        assert_eq!(
            labels(b"a@b, c@d,Friends: Jo <e@f>,g@h; x@y,z@w"),
            [
                "begin unnamed",
                "mail a@b | -",
                "mail  c@d | -",
                "end",
                "begin Friends",
                "mail e@f | phrase  Jo ",
                "mail g@h | -",
                "end",
                "begin unnamed",
                "mail  x@y | -",
                "mail z@w | -",
                "end",
            ]
        );
        assert_eq!(
            labels(b"Group: a@b(Name);"),
            ["begin Group", "mail  a@b(Name) | comment (Name)", "end"]
        );
        assert_eq!(
            labels(b"\"G:x\": <@route:a@b>;"),
            ["begin \"G:x\"", "mail a@b | -", "end"]
        );
    }
    #[test]
    fn empty_lists_null_slots_comments_and_empty_named_groups() {
        for source in [b"".as_slice(), b" , (c),; ;\t", b"\r\n", b"\t\r\n "] {
            assert!(labels(source).is_empty(), "{source:?}");
        }
        assert_eq!(
            labels(b"G:;H: (c),,; \"\":;"),
            ["begin G", "end", "begin H", "end", "begin  \"\"", "end",]
        );
        assert_eq!(
            labels(b"a@b,, (c),c@d,"),
            ["begin unnamed", "mail a@b | -", "mail c@d | -", "end"]
        );
    }
    #[test]
    fn malformed_items_recover_without_nested_groups_or_empty_emails() {
        assert_eq!(
            labels(b"bad, a@b,\"unclosed,tail"),
            [
                "begin unnamed",
                "raw bad",
                "mail  a@b | -",
                "raw \"unclosed,tail",
                "end",
            ]
        );
        assert_eq!(
            labels(b"@bad: a@b, valid@b;"),
            ["begin unnamed", "raw @bad: a@b", "mail  valid@b | -", "end",]
        );
        assert_eq!(
            labels(b"G: a@b,H:c@d;"),
            ["begin G", "mail  a@b | -", "raw H:c@d", "end"]
        );
        assert_eq!(labels(b" \tbad\r\n "), ["begin unnamed", "raw bad", "end"]);
        assert_eq!(
            labels(b"a@b, (unclosed, tail"),
            [
                "begin unnamed",
                "mail a@b | -",
                "raw (unclosed, tail",
                "end",
            ]
        );
        assert_eq!(
            events(b"(\xff) a@b").unwrap(),
            vec![
                Status::BeginGroup(None),
                Status::Mailbox(Address::Raw(Extent { start: 0, end: 7 })),
                Status::EndGroup
            ]
        );
        assert_eq!(
            labels(b"(a\0b) G:c@d;"),
            ["begin unnamed", "raw (a\0b) G:c@d", "end"]
        );
        let raw = b"\0\xff";
        assert_eq!(
            events(raw).unwrap(),
            vec![
                Status::BeginGroup(None),
                Status::Mailbox(Address::Raw(Extent { start: 0, end: 2 })),
                Status::EndGroup
            ]
        );
    }
    #[test]
    fn missing_semicolon_and_stray_semicolon_have_deterministic_recovery() {
        assert_eq!(
            labels(b"G: a@b,c@d"),
            ["begin G", "mail  a@b | -", "mail c@d | -", "end"]
        );
        assert_eq!(
            labels(b";;a@b; ;c@d;"),
            [
                "begin unnamed",
                "mail a@b | -",
                "end",
                "begin unnamed",
                "mail c@d | -",
                "end"
            ]
        );
        assert_eq!(labels(b"G:"), ["begin G", "end"]);
    }
    #[test]
    fn final_completion_failure_retires_even_a_closed_provisional_group() {
        let source = b"a@b";
        let mut full_cursor = Cursor::new(source);
        let mut full = work();
        let before = full.remaining();
        while full_cursor.poll(Tick(1), &mut full).unwrap() != Status::Complete {}
        let records = before.records - full.remaining().records;
        let mut limited = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                records: records - 1,
                ..before
            },
        );
        let mut cursor = Cursor::new(source);
        let mut provisional = Vec::new();
        loop {
            match cursor.poll(Tick(1), &mut limited) {
                Ok(Status::Yield) => {}
                Ok(Status::Complete) => panic!("underbudget list completed"),
                Ok(event) => provisional.push(event),
                Err(error) => {
                    assert_eq!(error, Error::Work(Stop::Records));
                    break;
                }
            }
        }
        assert_eq!(provisional.len(), 3);
        assert_eq!(provisional.last(), Some(&Status::EndGroup));
        let mut fresh = work();
        let before = fresh.remaining();
        assert_eq!(
            cursor.poll(Tick(1), &mut fresh),
            Err(Error::Work(Stop::Records))
        );
        assert_eq!(before, fresh.remaining());
    }
    #[test]
    fn long_names_and_field_resource_errors_keep_fixed_state() {
        let source = format!("{}: {}@b;", "🐈".repeat(10_000), "é".repeat(10_000));
        assert_eq!(events(source.as_bytes()).unwrap().len(), 3);
        let source = format!("a@b,{}", "(".repeat(33));
        assert_eq!(events(source.as_bytes()), Err(Error::NestingLimit));
        for (io_bytes, records, now, expected) in [
            (0, 1000, Tick(1), Error::Work(Stop::IoBytes)),
            (1000, 0, Tick(1), Error::Work(Stop::Records)),
            (1000, 1000, Tick(100), Error::Work(Stop::Deadline)),
        ] {
            let mut cursor = Cursor::new(b"G: a@b;");
            let mut limited = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            assert_eq!(cursor.poll(now, &mut limited), Err(expected));
            let mut fresh = work();
            let before = fresh.remaining();
            assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(expected));
            assert_eq!(before, fresh.remaining());
        }
    }
}
