//! Bounded selection of JMAP header fields/forms; no value parsing or JSON.
use crate::{
    admission::work::{Charge, Meter, Stop},
    ports::Tick,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Context {
    Email,
    BodyPart,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Form {
    Raw,
    Text,
    Addresses,
    GroupedAddresses,
    MessageIds,
    Date,
    URLs,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Occurrence {
    Last,
    All,
}

/// Names borrow the decoded request key, or a canonical convenience alias.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Property<'a> {
    requested: &'a str,
    name: &'a str,
    form: Form,
    occurrence: Occurrence,
}
impl<'a> Property<'a> {
    pub const fn requested(&self) -> &'a str {
        self.requested
    }
    pub const fn name(&self) -> &'a str {
        self.name
    }
    pub const fn form(&self) -> Form {
        self.form
    }
    pub const fn occurrence(&self) -> Occurrence {
        self.occurrence
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status<'a> {
    Yield,
    Complete(Option<Property<'a>>),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Work(Stop),
    InvalidProperty,
    ForbiddenForm,
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Work(error) => write!(f, "header property work: {error}"),
            Self::InvalidProperty => f.write_str("invalid header property"),
            Self::ForbiddenForm => f.write_str("forbidden header form"),
            Self::InvalidState => f.write_str("invalid header property state"),
        }
    }
}
impl std::error::Error for Error {}
impl From<Stop> for Error {
    fn from(stop: Stop) -> Self {
        Self::Work(stop)
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Prefix,
    Alias,
    Name,
    Suffix,
    Classify,
    Complete,
}
/// One parser per decoded JSON property key; the caller retains the job meter.
pub struct Cursor<'a> {
    source: &'a str,
    context: Context,
    position: usize,
    name_end: usize,
    row: usize,
    phase: Phase,
    form: Form,
    occurrence: Occurrence,
    result: Option<Property<'a>>,
    failure: Option<Error>,
}
impl<'a> Cursor<'a> {
    pub const fn new(source: &'a str, context: Context) -> Self {
        Self {
            source,
            context,
            position: 7,
            name_end: 7,
            row: 0,
            phase: Phase::Prefix,
            form: Form::Raw,
            occurrence: Occurrence::Last,
            result: None,
            failure: None,
        }
    }
    pub fn poll(&mut self, now: Tick, work: &mut Meter) -> Result<Status<'a>, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete(self.result));
        }
        let result = self.step(now, work);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn step(&mut self, now: Tick, work: &mut Meter) -> Result<Status<'a>, Error> {
        match self.phase {
            Phase::Prefix => {
                charge(now, work, self.source.len().min(7), 1)?;
                self.phase = if self.source.starts_with("header:") {
                    Phase::Name
                } else if self.context == Context::Email {
                    Phase::Alias
                } else {
                    return Ok(self.finish(None));
                };
            }
            Phase::Alias => {
                let Some(&(key, name, form)) = ALIASES.get(self.row) else {
                    charge(now, work, 0, 1)?;
                    return Ok(self.finish(None));
                };
                charge(
                    now,
                    work,
                    if key.len() == self.source.len() {
                        key.len()
                    } else {
                        0
                    },
                    1,
                )?;
                if key == self.source {
                    return Ok(self.finish(Some(Property {
                        requested: self.source,
                        name,
                        form,
                        occurrence: Occurrence::Last,
                    })));
                }
                self.row = self.row.checked_add(1).ok_or(Error::InvalidState)?;
            }
            Phase::Name => {
                for _ in 0..32 {
                    if self.position == self.source.len() {
                        charge(now, work, 0, 1)?;
                        self.name_end = self.position;
                        return self.named();
                    }
                    charge(now, work, 1, 1)?;
                    let byte = *self
                        .source
                        .as_bytes()
                        .get(self.position)
                        .ok_or(Error::InvalidState)?;
                    if byte == b':' {
                        self.name_end = self.position;
                        if self.name_end == 7 {
                            return Err(Error::InvalidProperty);
                        }
                        self.phase = Phase::Suffix;
                        return Ok(Status::Yield);
                    }
                    if !(33..=126).contains(&byte) {
                        return Err(Error::InvalidProperty);
                    }
                    self.position = self.position.checked_add(1).ok_or(Error::InvalidState)?;
                }
            }
            Phase::Suffix => {
                let suffix = self
                    .source
                    .get(self.name_end..)
                    .ok_or(Error::InvalidState)?;
                if suffix.len() > ":asGroupedAddresses:all".len() {
                    charge(now, work, 0, 1)?;
                    return Err(Error::InvalidProperty);
                }
                // Prepay the bounded suffix removal and seven exact form comparisons.
                charge(
                    now,
                    work,
                    suffix.len().checked_mul(8).ok_or(Error::InvalidState)?,
                    8,
                )?;
                let form = if let Some(form) = suffix.strip_suffix(":all") {
                    self.occurrence = Occurrence::All;
                    form
                } else {
                    suffix
                };
                self.form = match form {
                    "" if self.occurrence == Occurrence::All => Form::Raw,
                    ":asRaw" => Form::Raw,
                    ":asText" => Form::Text,
                    ":asAddresses" => Form::Addresses,
                    ":asGroupedAddresses" => Form::GroupedAddresses,
                    ":asMessageIds" => Form::MessageIds,
                    ":asDate" => Form::Date,
                    ":asURLs" => Form::URLs,
                    _ => return Err(Error::InvalidProperty),
                };
                return self.named();
            }
            Phase::Classify => {
                let name = self.name()?;
                let Some(&(field, allowed)) = FIELDS.get(self.row) else {
                    charge(now, work, 0, 1)?;
                    return self.selected();
                };
                let same_length = field.len() == name.len();
                charge(now, work, if same_length { name.len() } else { 0 }, 1)?;
                if same_length && field.eq_ignore_ascii_case(name) {
                    if self.form == allowed
                        || (allowed == Form::Addresses && self.form == Form::GroupedAddresses)
                    {
                        return self.selected();
                    }
                    return Err(Error::ForbiddenForm);
                }
                self.row = self.row.checked_add(1).ok_or(Error::InvalidState)?;
            }
            Phase::Complete => return Ok(Status::Complete(self.result)),
        }
        Ok(Status::Yield)
    }
    fn name(&self) -> Result<&'a str, Error> {
        self.source
            .get(7..self.name_end)
            .filter(|name| !name.is_empty())
            .ok_or(Error::InvalidProperty)
    }
    fn named(&mut self) -> Result<Status<'a>, Error> {
        self.name()?;
        if self.form == Form::Raw {
            return self.selected();
        }
        self.row = 0;
        self.phase = Phase::Classify;
        Ok(Status::Yield)
    }
    fn selected(&mut self) -> Result<Status<'a>, Error> {
        let result = Property {
            requested: self.source,
            name: self.name()?,
            form: self.form,
            occurrence: self.occurrence,
        };
        Ok(self.finish(Some(result)))
    }
    fn finish(&mut self, result: Option<Property<'a>>) -> Status<'a> {
        self.result = result;
        self.phase = Phase::Complete;
        Status::Complete(result)
    }
}
fn charge(now: Tick, work: &mut Meter, bytes: usize, records: u64) -> Result<(), Error> {
    work.charge(
        now,
        Charge {
            io_bytes: u64::try_from(bytes).map_err(|_| Error::InvalidState)?,
            records,
            ..Charge::default()
        },
    )?;
    Ok(())
}
const ALIASES: &[(&str, &str, Form)] = &[
    ("messageId", "Message-ID", Form::MessageIds),
    ("inReplyTo", "In-Reply-To", Form::MessageIds),
    ("references", "References", Form::MessageIds),
    ("sender", "Sender", Form::Addresses),
    ("from", "From", Form::Addresses),
    ("to", "To", Form::Addresses),
    ("cc", "Cc", Form::Addresses),
    ("bcc", "Bcc", Form::Addresses),
    ("replyTo", "Reply-To", Form::Addresses),
    ("subject", "Subject", Form::Text),
    ("sentAt", "Date", Form::Date),
];
// RFC 5322 (including obsolete fields) and RFC 2369 constrain forms.
// Other field names accept every form; that does not authorize encoded words.
const FIELDS: &[(&str, Form)] = &[
    ("From", Form::Addresses),
    ("Sender", Form::Addresses),
    ("Reply-To", Form::Addresses),
    ("To", Form::Addresses),
    ("Cc", Form::Addresses),
    ("Bcc", Form::Addresses),
    ("Resent-From", Form::Addresses),
    ("Resent-Sender", Form::Addresses),
    ("Resent-Reply-To", Form::Addresses),
    ("Resent-To", Form::Addresses),
    ("Resent-Cc", Form::Addresses),
    ("Resent-Bcc", Form::Addresses),
    ("Message-ID", Form::MessageIds),
    ("In-Reply-To", Form::MessageIds),
    ("References", Form::MessageIds),
    ("Resent-Message-ID", Form::MessageIds),
    ("Date", Form::Date),
    ("Resent-Date", Form::Date),
    ("Subject", Form::Text),
    ("Comments", Form::Text),
    ("Keywords", Form::Text),
    ("Return-Path", Form::Raw),
    ("Received", Form::Raw),
    ("List-Help", Form::URLs),
    ("List-Unsubscribe", Form::URLs),
    ("List-Subscribe", Form::URLs),
    ("List-Post", Form::URLs),
    ("List-Owner", Form::URLs),
    ("List-Archive", Form::URLs),
];

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::ports::Deadline;
    fn work() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records: 2_000_000,
                ..Charge::default()
            },
        )
    }
    fn parse(source: &str) -> Result<Option<Property<'_>>, Error> {
        let mut cursor = Cursor::new(source, Context::Email);
        let mut work = work();
        for _ in 0..100_000 {
            let before = work.remaining();
            let status = cursor.poll(Tick(1), &mut work);
            let after = work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 184);
            assert!(before.records - after.records <= 32);
            assert_eq!(before.output_bytes, after.output_bytes);
            match status? {
                Status::Yield => {}
                Status::Complete(value) => {
                    assert_eq!(
                        cursor.poll(Tick(100), &mut work),
                        Ok(Status::Complete(value))
                    );
                    assert_eq!(work.remaining(), after);
                    return Ok(value);
                }
            }
        }
        panic!("selector did not finish");
    }
    #[test]
    fn literal_requests_preserve_case_forms_and_occurrences() {
        for (key, name, form, occurrence) in [
            ("header:SUBJect", "SUBJect", Form::Raw, Occurrence::Last),
            ("header:SUBJect:all", "SUBJect", Form::Raw, Occurrence::All),
            (
                "header:Subject:asText",
                "Subject",
                Form::Text,
                Occurrence::Last,
            ),
            (
                "header:Received:asRaw:all",
                "Received",
                Form::Raw,
                Occurrence::All,
            ),
            (
                "header:Reply-To:asAddresses",
                "Reply-To",
                Form::Addresses,
                Occurrence::Last,
            ),
            (
                "header:rEsEnT-RePlY-To:asGroupedAddresses:all",
                "rEsEnT-RePlY-To",
                Form::GroupedAddresses,
                Occurrence::All,
            ),
            (
                "header:Message-ID:asMessageIds:all",
                "Message-ID",
                Form::MessageIds,
                Occurrence::All,
            ),
            (
                "header:Resent-Date:asDate",
                "Resent-Date",
                Form::Date,
                Occurrence::Last,
            ),
            (
                "header:List-Unsubscribe:asURLs",
                "List-Unsubscribe",
                Form::URLs,
                Occurrence::Last,
            ),
            (
                "header:Content-Description:asText",
                "Content-Description",
                Form::Text,
                Occurrence::Last,
            ),
        ] {
            let value = parse(key).unwrap().unwrap();
            assert_eq!(
                (
                    value.requested(),
                    value.name(),
                    value.form(),
                    value.occurrence()
                ),
                (key, name, form, occurrence)
            );
        }
        assert!(std::mem::size_of::<Cursor<'_>>() <= 128);
        assert!(std::mem::size_of::<Property<'_>>() <= 48);
    }
    #[test]
    fn aliases_and_non_header_keys_have_distinct_results() {
        for (key, name, form) in [
            ("messageId", "Message-ID", Form::MessageIds),
            ("inReplyTo", "In-Reply-To", Form::MessageIds),
            ("references", "References", Form::MessageIds),
            ("sender", "Sender", Form::Addresses),
            ("from", "From", Form::Addresses),
            ("to", "To", Form::Addresses),
            ("cc", "Cc", Form::Addresses),
            ("bcc", "Bcc", Form::Addresses),
            ("replyTo", "Reply-To", Form::Addresses),
            ("subject", "Subject", Form::Text),
            ("sentAt", "Date", Form::Date),
        ] {
            assert_eq!(
                parse(key),
                Ok(Some(Property {
                    requested: key,
                    name,
                    form,
                    occurrence: Occurrence::Last
                }))
            );
        }
        for key in [
            "",
            "headers",
            "Subject",
            "Header:Subject",
            "header",
            "subject:all",
            "sentat",
            "bodyStructure",
        ] {
            assert_eq!(parse(key), Ok(None), "{key}");
        }
    }
    #[test]
    fn body_parts_recognize_parameterized_keys_without_email_aliases() {
        for (key, expected) in [
            ("subject", None),
            ("from", None),
            ("sentAt", None),
            ("header:Subject:asText", Some(Form::Text)),
            (
                "header:From:asGroupedAddresses",
                Some(Form::GroupedAddresses),
            ),
            ("header:Content-Description:asText", Some(Form::Text)),
        ] {
            let mut cursor = Cursor::new(key, Context::BodyPart);
            let mut work = work();
            let mut complete = false;
            for _ in 0..100 {
                if let Status::Complete(value) = cursor.poll(Tick(1), &mut work).unwrap() {
                    assert_eq!(value.map(|p| p.form()), expected);
                    complete = true;
                    break;
                }
            }
            assert!(complete);
        }
    }
    #[test]
    fn syntax_and_field_form_authorization_are_separate() {
        for key in [
            "header:",
            "header::all",
            "header:X:",
            "header:X:asraw",
            "header:X:ALL",
            "header:X:all:asRaw",
            "header:X:all:all",
            "header:X:asText:asText",
            "header: X",
            "header:X Y",
            "header:X\t",
            "header:X\r\n",
            "header:X\0",
            "header:X\u{7f}",
            "header:é",
            "header:X:asTexté",
        ] {
            assert_eq!(parse(key), Err(Error::InvalidProperty), "{key:?}");
        }
        for key in [
            "header:Subject:asAddresses",
            "header:To:asText",
            "header:Date:asURLs",
            "header:Message-ID:asDate",
            "header:Return-Path:asAddresses",
            "header:Received:asText",
            "header:List-Post:asMessageIds",
            "header:Resent-Reply-To:asText",
            "header:from:asDate",
            "header:rEcEiVeD:asText",
            "header:SUBJECT:asAddresses",
            "header:Subject:asGroupedAddresses",
            "header:Date:asGroupedAddresses",
        ] {
            assert_eq!(parse(key), Err(Error::ForbiddenForm), "{key}");
        }
        // These fields are outside the RFC 5322/2369 restriction set.
        for field in ["X-Custom", "List-Id", "Content-Type", "Content-Description"] {
            for form in [
                "Raw",
                "Text",
                "Addresses",
                "GroupedAddresses",
                "MessageIds",
                "Date",
                "URLs",
            ] {
                assert!(parse(&format!("header:{field}:as{form}:all"))
                    .unwrap()
                    .is_some());
            }
        }
        assert_eq!(
            parse(&format!("header:X:{}", "a".repeat(100_000))),
            Err(Error::InvalidProperty)
        );
        let punctuation = "!\"#$%&'()*+,-./0123456789;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~";
        assert_eq!(
            parse(&format!("header:{punctuation}"))
                .unwrap()
                .unwrap()
                .name(),
            punctuation
        );
    }
    #[test]
    fn long_names_yield_and_failures_cannot_be_retried_with_fresh_work() {
        let source = format!("header:{}:asDate:all", "x".repeat(100_000));
        let value = parse(&source).unwrap().unwrap();
        assert_eq!(value.name().len(), 100_000);
        assert_eq!(value.form(), Form::Date);
        for (capacity, expected) in [
            (
                Charge {
                    io_bytes: 40,
                    records: 1000,
                    ..Charge::default()
                },
                Error::Work(Stop::IoBytes),
            ),
            (
                Charge {
                    io_bytes: 1000,
                    records: 2,
                    ..Charge::default()
                },
                Error::Work(Stop::Records),
            ),
        ] {
            let mut cursor = Cursor::new(&source, Context::Email);
            let mut limited = Meter::new(Deadline::after(Tick(0), 100).unwrap(), capacity);
            assert_eq!(cursor.poll(Tick(1), &mut limited), Ok(Status::Yield));
            let mut refused = false;
            for _ in 0..4 {
                if let Err(error) = cursor.poll(Tick(1), &mut limited) {
                    assert_eq!(error, expected);
                    refused = true;
                    break;
                }
            }
            assert!(refused);
            assert_eq!(cursor.poll(Tick(1), &mut work()), Err(expected));
        }
        for key in [source.as_str(), "subject", "header:Subject:asText"] {
            let mut cursor = Cursor::new(key, Context::Email);
            let mut work = work();
            assert_eq!(cursor.poll(Tick(1), &mut work), Ok(Status::Yield));
            assert_eq!(
                cursor.poll(Tick(100), &mut work),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(
                cursor.poll(Tick(1), &mut self::work()),
                Err(Error::Work(Stop::Deadline))
            );
        }
        for key in ["header:X:all:all", "header:Subject:asDate"] {
            let expected = parse(key).unwrap_err();
            let mut cursor = Cursor::new(key, Context::Email);
            for _ in 0..100 {
                if cursor.poll(Tick(1), &mut work()).is_err() {
                    break;
                }
            }
            assert_eq!(cursor.poll(Tick(1), &mut work()), Err(expected));
        }
    }
}
