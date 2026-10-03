//! One complete mailbox; returned raw extents grant no delivery authority.
pub use crate::header_message_ids::Extent;
use crate::{
    admission::work::{Charge, Meter, Stop},
    header_addr_spec, header_cfws, header_delimited, header_message_ids, header_phrase,
    ports::Tick,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Malformed,
    NestingLimit,
    Work(Stop),
    InterpretationLimit,
    InvalidState,
}
impl From<header_message_ids::Error> for Error {
    fn from(error: header_message_ids::Error) -> Self {
        match error {
            header_message_ids::Error::Malformed => Self::Malformed,
            header_message_ids::Error::NestingLimit => Self::NestingLimit,
            header_message_ids::Error::Work(stop) => Self::Work(stop),
            header_message_ids::Error::InvalidState => Self::InvalidState,
            header_message_ids::Error::InterpretationLimit => Self::InterpretationLimit,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed mailbox"),
            Self::NestingLimit => f.write_str("mailbox comment nesting limit"),
            Self::Work(error) => write!(f, "mailbox work: {error}"),
            Self::InterpretationLimit => f.write_str("header interpretation limit"),
            Self::InvalidState => f.write_str("invalid mailbox cursor state"),
        }
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Name {
    Phrase(Extent),
    Comment(Extent),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mailbox {
    pub name: Option<Name>,
    pub address: Extent,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete(Mailbox),
}
enum Phase<'a> {
    Scan,
    ScanCfws(header_cfws::Cursor<'a>),
    ScanDelimited(header_delimited::Cursor<'a>),
    Prefix(header_cfws::Cursor<'a>),
    Phrase(header_phrase::Cursor<'a>),
    Tail(header_cfws::Cursor<'a>),
    RouteCfws(header_cfws::Cursor<'a>),
    RouteSyntax,
    RouteDomain(header_message_ids::Cursor<'a>),
    Address(header_addr_spec::Cursor<'a>),
    Comment(header_cfws::Cursor<'a>),
    Complete,
}
/// Input is one entire candidate, excluding any enclosing list/group separator.
pub struct Cursor<'a> {
    source: &'a [u8],
    phase: Phase<'a>,
    position: usize,
    open: Option<usize>,
    close: Option<usize>,
    colon: Option<usize>,
    name: Option<Name>,
    address: Extent,
    address_end: usize,
    route_seen: bool,
    route_need_comma: bool,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub const fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            phase: Phase::Scan,
            position: 0,
            open: None,
            close: None,
            colon: None,
            name: None,
            address: Extent {
                start: 0,
                end: source.len(),
            },
            address_end: 0,
            route_seen: false,
            route_need_comma: false,
            failure: None,
        }
    }
    fn slice(&self, extent: Extent) -> Result<&'a [u8], Error> {
        self.source
            .get(extent.start..extent.end)
            .ok_or(Error::InvalidState)
    }
    fn mailbox(&self) -> Mailbox {
        Mailbox {
            name: self.name,
            address: self.address,
        }
    }
    fn add(base: usize, amount: usize) -> Result<usize, Error> {
        base.checked_add(amount).ok_or(Error::InvalidState)
    }
    fn cfws(error: header_cfws::Error) -> Error {
        match error {
            header_cfws::Error::Malformed => Error::Malformed,
            header_cfws::Error::NestingLimit => Error::NestingLimit,
            header_cfws::Error::Work(stop) => Error::Work(stop),
            header_cfws::Error::InvalidState => Error::InvalidState,
            header_cfws::Error::InterpretationLimit => Error::InterpretationLimit,
        }
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete(self.mailbox()));
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
    fn tail(&mut self) -> Result<(), Error> {
        let start = Self::add(self.close.ok_or(Error::InvalidState)?, 1)?;
        self.phase = Phase::Tail(header_cfws::Cursor::new(self.source, start));
        Ok(())
    }
    fn address(&mut self) -> Result<(), Error> {
        self.phase = Phase::Address(header_addr_spec::Cursor::new(self.slice(self.address)?));
        Ok(())
    }
    fn route_cfws(&mut self) -> Result<(), Error> {
        let end = self.colon.ok_or(Error::InvalidState)?;
        let source = self.slice(Extent { start: 0, end })?;
        self.phase = Phase::RouteCfws(header_cfws::Cursor::new(source, self.position));
        Ok(())
    }
    fn scan_complete(&mut self) -> Result<(), Error> {
        match (self.open, self.close) {
            (None, None) => self.address(),
            (Some(open), Some(close)) => {
                self.address = Extent {
                    start: Self::add(open, 1)?,
                    end: close,
                };
                let prefix = self.slice(Extent {
                    start: 0,
                    end: open,
                })?;
                self.phase = Phase::Prefix(header_cfws::Cursor::new(prefix, 0));
                Ok(())
            }
            _ => Err(Error::Malformed),
        }
    }
    fn scan(&mut self, now: Tick, work: &mut Meter) -> Result<(), Error> {
        if self.position > self.source.len() {
            return Err(Error::InvalidState);
        }
        work.charge(
            now,
            Charge {
                io_bytes: u64::from(self.position < self.source.len()),
                ..Charge::default()
            },
        )
        .map_err(Error::Work)?;
        let Some(byte) = self.source.get(self.position).copied() else {
            return self.scan_complete();
        };
        match byte {
            b'(' | b' ' | b'\t' | b'\r' | b'\n' => {
                self.phase = Phase::ScanCfws(header_cfws::Cursor::new(self.source, self.position));
                return Ok(());
            }
            b'"' | b'[' => {
                let kind = if byte == b'"' {
                    header_delimited::Kind::QuotedString
                } else {
                    header_delimited::Kind::DomainLiteral
                };
                self.phase = Phase::ScanDelimited(header_delimited::Cursor::new(
                    self.source,
                    self.position,
                    kind,
                ));
                return Ok(());
            }
            b'<' if self.open.is_none() => self.open = Some(self.position),
            b'>' if self.open.is_some() && self.close.is_none() => self.close = Some(self.position),
            b'<' | b'>' | b';' => return Err(Error::Malformed),
            b',' if self.open.is_none() || self.close.is_some() => return Err(Error::Malformed),
            b':' => {
                if self.open.is_none() || self.close.is_some() || self.colon.is_some() {
                    return Err(Error::Malformed);
                }
                self.colon = Some(self.position);
            }
            _ => {}
        }
        self.position = Self::add(self.position, 1)?;
        Ok(())
    }
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status, Error> {
        match &mut self.phase {
            Phase::Scan => self.scan(now, work)?,
            Phase::ScanCfws(cursor) => {
                if let header_cfws::Status::Complete(end) =
                    cursor.poll(now, work).map_err(Self::cfws)?
                {
                    if end.position < self.position {
                        return Err(Error::InvalidState);
                    }
                    if end.position == self.position {
                        return Err(Error::Malformed);
                    }
                    self.position = end.position;
                    self.phase = Phase::Scan;
                }
            }
            Phase::ScanDelimited(cursor) => {
                let status = cursor.poll(now, work).map_err(|error| match error {
                    header_delimited::Error::Malformed => Error::Malformed,
                    header_delimited::Error::Work(stop) => Error::Work(stop),
                    header_delimited::Error::InvalidState => Error::InvalidState,
                })?;
                if let header_delimited::Status::Complete(extent) = status {
                    self.position = extent.end;
                    self.phase = Phase::Scan;
                }
            }
            Phase::Prefix(cursor) => {
                if let header_cfws::Status::Complete(end) =
                    cursor.poll(now, work).map_err(Self::cfws)?
                {
                    let open = self.open.ok_or(Error::InvalidState)?;
                    if end.position == open {
                        self.tail()?;
                    } else {
                        self.phase =
                            Phase::Phrase(header_phrase::Cursor::new(self.slice(Extent {
                                start: 0,
                                end: open,
                            })?));
                    }
                }
            }
            Phase::Phrase(cursor) => {
                let status = cursor.poll(now, work).map_err(|error| match error {
                    header_phrase::Error::Malformed => Error::Malformed,
                    header_phrase::Error::NestingLimit => Error::NestingLimit,
                    header_phrase::Error::Work(stop) => Error::Work(stop),
                    header_phrase::Error::InvalidState => Error::InvalidState,
                    header_phrase::Error::InterpretationLimit => Error::InterpretationLimit,
                })?;
                if matches!(status, header_phrase::Status::Complete(_)) {
                    self.name = Some(Name::Phrase(Extent {
                        start: 0,
                        end: self.open.ok_or(Error::InvalidState)?,
                    }));
                    self.tail()?;
                }
            }
            Phase::Tail(cursor) => {
                if let header_cfws::Status::Complete(end) =
                    cursor.poll(now, work).map_err(Self::cfws)?
                {
                    if end.position != self.source.len() {
                        return Err(Error::Malformed);
                    }
                    if self.colon.is_some() {
                        self.position = self.address.start;
                        self.route_cfws()?;
                    } else {
                        self.address()?;
                    }
                }
            }
            Phase::RouteCfws(cursor) => {
                if let header_cfws::Status::Complete(end) =
                    cursor.poll(now, work).map_err(Self::cfws)?
                {
                    self.position = end.position;
                    self.phase = Phase::RouteSyntax;
                }
            }
            Phase::RouteSyntax => {
                let end = self.colon.ok_or(Error::InvalidState)?;
                if self.position == end {
                    if !self.route_seen {
                        return Err(Error::Malformed);
                    }
                    self.address.start = Self::add(end, 1)?;
                    self.address()?;
                } else {
                    if self.position > end {
                        return Err(Error::InvalidState);
                    }
                    work.charge(
                        now,
                        Charge {
                            io_bytes: 1,
                            ..Charge::default()
                        },
                    )
                    .map_err(Error::Work)?;
                    match self
                        .source
                        .get(self.position)
                        .copied()
                        .ok_or(Error::InvalidState)?
                    {
                        b',' => {
                            self.position = Self::add(self.position, 1)?;
                            self.route_need_comma = false;
                            self.route_cfws()?;
                        }
                        b'@' if !self.route_need_comma => {
                            self.position = Self::add(self.position, 1)?;
                            self.phase = Phase::RouteDomain(
                                header_message_ids::Cursor::route_domain(self.slice(Extent {
                                    start: self.position,
                                    end,
                                })?),
                            );
                        }
                        _ => return Err(Error::Malformed),
                    }
                }
            }
            Phase::RouteDomain(cursor) => {
                if cursor.poll(now, work)? == header_message_ids::Status::Complete {
                    let length = cursor.route_domain_end().ok_or(Error::InvalidState)?;
                    self.position = Self::add(self.position, length)?;
                    self.route_seen = true;
                    self.route_need_comma = true;
                    self.route_cfws()?;
                }
            }
            Phase::Address(cursor) => {
                let status = cursor.poll(now, work).map_err(|error| match error {
                    header_addr_spec::Error::Malformed => Error::Malformed,
                    header_addr_spec::Error::NestingLimit => Error::NestingLimit,
                    header_addr_spec::Error::Work(stop) => Error::Work(stop),
                    header_addr_spec::Error::InvalidState => Error::InvalidState,
                    header_addr_spec::Error::InterpretationLimit => Error::InterpretationLimit,
                })?;
                match status {
                    header_addr_spec::Status::Part(extent) => self.address_end = extent.end,
                    header_addr_spec::Status::Complete => {
                        if self.name.is_some() {
                            self.phase = Phase::Complete;
                        } else {
                            self.phase = Phase::Comment(header_cfws::Cursor::new(
                                self.slice(self.address)?,
                                self.address_end,
                            ));
                        }
                    }
                    header_addr_spec::Status::Yield => {}
                }
            }
            Phase::Comment(cursor) => match cursor.poll(now, work).map_err(Self::cfws)? {
                header_cfws::Status::Comment(comment) if self.name.is_none() => {
                    self.name = Some(Name::Comment(Extent {
                        start: Self::add(self.address.start, comment.start)?,
                        end: Self::add(self.address.start, comment.end)?,
                    }));
                }
                header_cfws::Status::Complete(end) => {
                    if Self::add(self.address.start, end.position)? != self.address.end {
                        return Err(Error::InvalidState);
                    }
                    self.phase = Phase::Complete;
                }
                _ => {}
            },
            Phase::Complete => {}
        }
        Ok(if matches!(self.phase, Phase::Complete) {
            Status::Complete(self.mailbox())
        } else {
            Status::Yield
        })
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
                io_bytes: 20_000_000,
                records: 20_000_000,
                ..Charge::default()
            },
        )
    }
    fn parse(source: &[u8]) -> Result<Mailbox, Error> {
        let mut cursor = Cursor::new(source);
        assert!(std::mem::size_of_val(&cursor) <= 512);
        let mut meter = work();
        for _ in 0..2_000_000 {
            let before = meter.remaining();
            let step = cursor.poll(Tick(1), &mut meter);
            let after = meter.remaining();
            assert!(before.io_bytes - after.io_bytes <= 161);
            assert!(before.records - after.records <= 34);
            assert_eq!(before.output_bytes, after.output_bytes);
            if let Status::Complete(mailbox) = step? {
                assert_eq!(
                    cursor.poll(Tick(100), &mut meter),
                    Ok(Status::Complete(mailbox))
                );
                assert_eq!(after, meter.remaining());
                return Ok(mailbox);
            }
        }
        panic!("mailbox did not finish");
    }
    fn result(source: &str) -> (Option<(&str, bool)>, &str) {
        let mailbox = parse(source.as_bytes()).unwrap();
        let name = mailbox.name.map(|name| match name {
            Name::Phrase(extent) => (&source[extent.start..extent.end], false),
            Name::Comment(extent) => (&source[extent.start..extent.end], true),
        });
        (name, &source[mailbox.address.start..mailbox.address.end])
    }
    #[test]
    fn whole_mailboxes_preserve_raw_phrase_and_address_extents() {
        for (source, name, email) in [
            ("a@b", None, "a@b"),
            (
                " (leading) a . b @ (domain) c (Name)",
                Some(("(Name)", true)),
                " (leading) a . b @ (domain) c (Name)",
            ),
            (" (leading) <a@b>", None, "a@b"),
            (
                " John (c) Doe < a@b > (tail)",
                Some((" John (c) Doe ", false)),
                " a@b ",
            ),
            ("\"\"<\"\"@[]>", Some(("\"\"", false)), "\"\"@[]"),
            (
                "é e\u{301}<例@テスト>",
                Some(("é e\u{301}", false)),
                "例@テスト",
            ),
            (
                "=?utf-8?q?Name?= <a@b>",
                Some(("=?utf-8?q?Name?= ", false)),
                "a@b",
            ),
            ("<\"<a,:;>\"@[x<,;:>y]>", None, "\"<a,:;>\"@[x<,;:>y]"),
        ] {
            assert_eq!(result(source), (name, email), "{source:?}");
        }
    }
    #[test]
    fn first_comment_after_actual_address_is_fallback_and_phrase_wins() {
        for (source, expected) in [
            ("a(c)@b(first)(second)", Some(("(first)", true))),
            (
                "a@b (first(nested)) (second)",
                Some(("(first(nested))", true)),
            ),
            ("a@b(comment).c", None),
            ("<a@b(inner)> (outside)", Some(("(inner)", true))),
            ("<a@b> (outside)", None),
            ("Name<a@b(inner)>(outside)", Some(("Name", false))),
            ("(prefix)<a@b>", None),
            ("<@route(comment):a@b>", None),
        ] {
            assert_eq!(result(source).0, expected, "{source:?}");
        }
    }
    #[test]
    fn obsolete_routes_require_domains_and_discard_only_valid_prefix() {
        for (source, expected) in [
            ("<@a:a@b>", "a@b"),
            ("Name < (x),,@a . b, (c), @ [x,:;], : a@b >", " a@b "),
            ("<@例,@[x]:a@b>", "a@b"),
            ("<,@a,,:\"x:y\"@b>", "\"x:y\"@b"),
        ] {
            assert_eq!(result(source).1, expected, "{source:?}");
        }
        for source in [
            "<:a@b>",
            "<,, :a@b>",
            "<@a @b:a@b>",
            "<@a,b:a@b>",
            "<@a,:>",
            "<@a..b:a@b>",
            "<@\"a\":a@b>",
            "<@a:b:c@d>",
            "<@a;@b:a@b>",
            "<@a:a@b,>",
            "<@a:a@b extra>",
            "<@a:a@b>junk",
        ] {
            assert_eq!(
                parse(source.as_bytes()),
                Err(Error::Malformed),
                "{source:?}"
            );
        }
    }
    #[test]
    fn malformed_entire_items_never_return_partial_success() {
        for source in [
            b"".as_slice(),
            b" ",
            b"(name)",
            b"a@b,c@d",
            b"a@b;",
            b"G:a@b;",
            b"Name a@b",
            b"<>",
            b"Name <>",
            b"<a@b",
            b"a@b>",
            b"<<a@b>>",
            b"<a@b><c@d>",
            b"Name<a@b>bad",
            b"a@b(comment)junk",
            b"a@b[extra]",
            b"a@b\xff",
            b"a@b\0",
            b"\"unclosed<a@b>",
            b"a@[unclosed",
            b"a@b (unclosed",
        ] {
            assert_eq!(parse(source), Err(Error::Malformed), "{source:?}");
        }
        let source = format!("{} <{}@b>", "🐈".repeat(10_000), "é".repeat(10_000));
        let mailbox = parse(source.as_bytes()).unwrap();
        assert_eq!(mailbox.address.end - mailbox.address.start, 20_002);
        assert!(matches!(mailbox.name, Some(Name::Phrase(_))));
    }
    #[test]
    fn nonfolding_line_endings_refuse_instead_of_repeating_cfws() {
        for source in [
            b"\r".as_slice(),
            b"\n",
            b"\r\n",
            b"\ra@b",
            b"\na@b",
            b"\r\na@b",
            b"a@b\r",
            b"a@b\n",
            b"a@b\r\n",
            b"<a@b>\n",
        ] {
            assert_eq!(parse(source), Err(Error::Malformed), "{source:?}");
        }
        for source in [
            b"\r\n a@b\r\n ".as_slice(),
            b"\n\ta@b\n\t",
            b"Name\r\n <a@b>",
        ] {
            assert!(parse(source).is_ok(), "{source:?}");
        }
    }
    #[test]
    fn bare_mailbox_cost_includes_scan_and_comment_replay() {
        let mut cursor = Cursor::new(b"a@b");
        let mut meter = work();
        let before = meter.remaining();
        while !matches!(
            cursor.poll(Tick(1), &mut meter).unwrap(),
            Status::Complete(_)
        ) {}
        assert_eq!(before.io_bytes - meter.remaining().io_bytes, 12);
        assert_eq!(before.records - meter.remaining().records, 28);
    }
    #[test]
    fn every_child_budget_checkpoint_refuses_without_a_public_mailbox() {
        for source in [
            b"Name <(c),@route,,:a@b> (tail)".as_slice(),
            b"<@route:a@b(first)(second)>",
        ] {
            let mut cursor = Cursor::new(source);
            let mut full = work();
            let before = full.remaining();
            while !matches!(
                cursor.poll(Tick(1), &mut full).unwrap(),
                Status::Complete(_)
            ) {}
            let used_io = before.io_bytes - full.remaining().io_bytes;
            let used_records = before.records - full.remaining().records;
            for (io_limit, used) in [(true, used_io), (false, used_records)] {
                for limit in 0..used {
                    let mut cursor = Cursor::new(source);
                    let mut cap = before;
                    if io_limit {
                        cap.io_bytes = limit;
                    } else {
                        cap.records = limit;
                    }
                    let mut limited = Meter::new(Deadline::after(Tick(0), 100).unwrap(), cap);
                    let expected = Error::Work(if io_limit {
                        Stop::IoBytes
                    } else {
                        Stop::Records
                    });
                    loop {
                        match cursor.poll(Tick(1), &mut limited) {
                            Ok(Status::Yield) => {}
                            Ok(Status::Complete(_)) => panic!("underbudget mailbox published"),
                            Err(error) => {
                                assert_eq!(error, expected);
                                break;
                            }
                        }
                    }
                    let mut fresh = work();
                    let untouched = fresh.remaining();
                    assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(expected));
                    assert_eq!(untouched, fresh.remaining());
                }
            }
        }
    }
    #[test]
    fn byte_record_deadline_nesting_and_syntax_failures_latch() {
        let nested = "(".repeat(33);
        for (source, io_bytes, records, now, expected) in [
            (
                b"a@b".as_slice(),
                0,
                1000,
                Tick(1),
                Error::Work(Stop::IoBytes),
            ),
            (b"a@b", 1000, 0, Tick(1), Error::Work(Stop::Records)),
            (b"a@b", 1000, 1000, Tick(100), Error::Work(Stop::Deadline)),
            (nested.as_bytes(), 1000, 1000, Tick(1), Error::NestingLimit),
            (b"a@b bad", 1000, 1000, Tick(1), Error::Malformed),
        ] {
            let mut cursor = Cursor::new(source);
            let mut meter = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    ..Charge::default()
                },
            );
            let error = loop {
                match cursor.poll(now, &mut meter) {
                    Ok(Status::Complete(_)) => panic!("bad mailbox/work accepted"),
                    Ok(_) => {}
                    Err(error) => break error,
                }
            };
            assert_eq!(error, expected);
            let mut fresh = work();
            let before = fresh.remaining();
            assert_eq!(cursor.poll(Tick(1), &mut fresh), Err(expected));
            assert_eq!(before, fresh.remaining());
        }
    }
}
