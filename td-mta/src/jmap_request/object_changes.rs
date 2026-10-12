//! Retained object changes; capability activation remains separate.
use super::{
    mailbox_get::storage,
    push::{Children, Item},
    Error, Worker,
};
use crate::{
    ids::{EmailId, MailboxId, ThreadId},
    jmap_changes,
    ports::ChangeAction,
    sync::{DataState, DataType},
    wire,
};
use std::ops::Range;
use td_json::{arena, scan};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ChangesKind {
    Mailbox,
    Thread,
    Email,
}
impl ChangesKind {
    fn data_type(self) -> DataType {
        match self {
            Self::Mailbox => DataType::Mailbox,
            Self::Thread => DataType::Thread,
            Self::Email => DataType::Email,
        }
    }
    fn encode(self, id: [u8; 16], output: &mut [u8]) -> Result<&str, Error> {
        match self {
            Self::Mailbox => wire::encode_id(MailboxId::from_bytes(id), output),
            Self::Thread => wire::encode_id(ThreadId::from_bytes(id), output),
            Self::Email => wire::encode_id(EmailId::from_bytes(id), output),
        }
        .map_err(|_| Error::State)
    }
}
fn changes(error: jmap_changes::Error) -> Error {
    match error {
        jmap_changes::Error::CannotCalculateChanges => Error::CannotCalculateChanges,
        jmap_changes::Error::Store(error) => storage(error),
        jmap_changes::Error::Invalid | jmap_changes::Error::Incomplete => Error::State,
    }
}
impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    pub(super) fn object_changes(
        &mut self,
        arguments: Range<u64>,
        maximum: usize,
        kind: ChangesKind,
    ) -> Result<Range<u64>, Error> {
        let mut fields = Children::new(arguments, scan::Kind::Object)?;
        let (mut account, mut since, mut limit) = (None, None, None);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(item),
                b"sinceState" => since = Some(item),
                b"maxChanges" => limit = Some(item),
                _ => return Err(Error::InvalidArguments),
            }
        }
        let account = self.push_string(&account.ok_or(Error::InvalidArguments)?)?;
        let account_id = account.wire_id()?.local();
        let since = self.changes_state(&since.ok_or(Error::InvalidArguments)?)?;
        let mut maximum = maximum.min(jmap_changes::MAX_CHANGES);
        if let Some(limit) = limit.filter(|item| item.kind != scan::Kind::Null) {
            let limit = self.changes_uint(&limit)?;
            if limit == 0 {
                return Err(Error::InvalidArguments);
            }
            maximum = maximum.min(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        if account_id != Some(self.account) {
            return Err(Error::AccountNotFound);
        }
        let since = since.ok_or(Error::CannotCalculateChanges)?;
        self.check()?;
        let mut view = self
            .retention
            .method_view(self.account, self.execution)
            .map_err(storage)?;
        let mut cursor = jmap_changes::Cursor::new(&mut view, kind.data_type(), since, maximum)
            .map_err(changes)?;
        loop {
            self.check()?;
            if cursor.poll().map_err(changes)? {
                break;
            }
        }
        let page = cursor.finish().map_err(changes)?;
        let start = self.retention.len();
        self.append(br#"{"accountId":"#)?;
        self.push_id(&account)?;
        let mut state = [0; DataState::WIRE_BYTES];
        self.append(br#","oldState":""#)?;
        self.append(
            page.old_state
                .encode(&mut state)
                .map_err(|_| Error::State)?
                .as_bytes(),
        )?;
        self.append(br#"","newState":""#)?;
        self.append(
            page.new_state
                .encode(&mut state)
                .map_err(|_| Error::State)?
                .as_bytes(),
        )?;
        self.append(br#"","hasMoreChanges":"#)?;
        self.append(if page.has_more { b"true" } else { b"false" })?;
        for (name, action) in [
            (b"created".as_slice(), ChangeAction::Created),
            (b"updated".as_slice(), ChangeAction::Updated),
            (b"destroyed".as_slice(), ChangeAction::Destroyed),
        ] {
            self.append(b",\"")?;
            self.append(name)?;
            self.append(b"\":[")?;
            let mut first = true;
            let mut id = [0; wire::IdKind::WIRE_BYTES];
            for change in &page.changes {
                self.check()?;
                if change.action != action {
                    continue;
                }
                if !first {
                    self.append(b",")?;
                }
                first = false;
                self.append(b"\"")?;
                self.append(kind.encode(change.id, &mut id)?.as_bytes())?;
                self.append(b"\"")?;
            }
            self.append(b"]")?;
        }
        if kind == ChangesKind::Mailbox {
            self.append(br#","updatedProperties":null"#)?;
        }
        self.append(b"}")?;
        view.check_snapshot().map_err(storage)?;
        self.check()?;
        let end = self.retention.publish().map_err(Error::Storage)?;
        Ok(start..end)
    }
    fn changes_state(&mut self, item: &Item) -> Result<Option<DataState>, Error> {
        if item.kind != scan::Kind::String {
            return Err(Error::InvalidArguments);
        }
        let length = item
            .range
            .end
            .checked_sub(item.range.start)
            .ok_or(Error::State)?;
        // A valid state is fixed ASCII, with at most six bytes per escape.
        let mut bytes = [0; DataState::WIRE_BYTES * 6 + 2];
        if length > bytes.len() as u64 {
            return Ok(None);
        }
        let input = bytes.get_mut(..length as usize).ok_or(Error::State)?;
        self.check()?;
        self.retention
            .read_at(item.range.start, input)
            .map_err(Error::Storage)?;
        let mut tokens = [arena::Token::EMPTY; 1];
        let document = arena::parse(
            input,
            &mut tokens,
            arena::ParseLimits {
                max_bytes: DataState::WIRE_BYTES * 6 + 2,
                max_depth: 1,
                max_tokens: 1,
            },
        )
        .map_err(|_| Error::State)?;
        let value = document.root().as_str().ok_or(Error::State)?;
        let state = DataState::decode(value).ok();
        self.check()?;
        Ok(state)
    }
    fn changes_uint(&mut self, item: &Item) -> Result<u64, Error> {
        u64::try_from(self.query_integer(item)?).map_err(|_| Error::InvalidArguments)
    }
    pub(super) fn query_integer(&mut self, item: &Item) -> Result<i64, Error> {
        if item.kind != scan::Kind::Number {
            return Err(Error::InvalidArguments);
        }
        let mut number = Unsigned::default();
        let mut position = item.range.start;
        let mut bytes = [0; 256];
        while position < item.range.end {
            self.check()?;
            let length = (item.range.end - position).min(bytes.len() as u64) as usize;
            let chunk = bytes.get_mut(..length).ok_or(Error::State)?;
            self.retention
                .read_at(position, chunk)
                .map_err(Error::Storage)?;
            for byte in chunk {
                number.byte(*byte);
            }
            position += length as u64;
        }
        self.check()?;
        let negative = number.negative;
        number.negative = false;
        let value = i64::try_from(number.finish().ok_or(Error::InvalidArguments)?)
            .map_err(|_| Error::InvalidArguments)?;
        if negative {
            value.checked_neg().ok_or(Error::InvalidArguments)
        } else {
            Ok(value)
        }
    }
}
// The retained scanner already checked JSON number syntax. Keep exact decimal
// significance, not a floating conversion that could round a fraction to UInt.
#[derive(Default)]
struct Unsigned {
    negative: bool,
    fraction: bool,
    exponent: bool,
    exponent_negative: bool,
    exponent_value: i64,
    decimals: i64,
    significant: i64,
    zeros: i64,
    prefix: u64,
}
impl Unsigned {
    fn byte(&mut self, byte: u8) {
        match byte {
            b'-' if self.exponent => self.exponent_negative = true,
            b'-' => self.negative = true,
            b'.' => self.fraction = true,
            b'e' | b'E' => self.exponent = true,
            b'0'..=b'9' if self.exponent => {
                // Coefficient lengths stay below u32::MAX - 16 (the JSON cap is 4 MiB).
                self.exponent_value =
                    (self.exponent_value * 10 + i64::from(byte - b'0')).min(i64::from(u32::MAX));
            }
            b'0'..=b'9' => {
                if self.fraction {
                    self.decimals += 1;
                }
                if self.significant != 0 || byte != b'0' {
                    self.significant += 1;
                    if self.significant <= 16 {
                        self.prefix = self.prefix * 10 + u64::from(byte - b'0');
                    }
                    self.zeros = if byte == b'0' { self.zeros + 1 } else { 0 };
                }
            }
            _ => (),
        }
    }
    fn finish(self) -> Option<u64> {
        if self.significant == 0 {
            return Some(0);
        }
        if self.negative {
            return None;
        }
        let exponent = if self.exponent_negative {
            -self.exponent_value
        } else {
            self.exponent_value
        };
        let shift = exponent - self.decimals + self.zeros;
        let digits = self.significant - self.zeros;
        if shift < 0 || digits + shift > 16 {
            return None;
        }
        let mut value = self.prefix;
        for _ in digits..self.significant.min(16) {
            value /= 10;
        }
        for _ in 0..shift {
            value *= 10;
        }
        (value < (1u64 << 53)).then_some(value)
    }
}

#[cfg(test)]
#[path = "mailbox_changes_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "thread_changes_tests.rs"]
mod thread_tests;

#[cfg(test)]
#[path = "email_changes_tests.rs"]
mod email_tests;
