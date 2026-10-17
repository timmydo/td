//! Core single-account method policies and bounded string decoding.
use super::{AccountId, Error, Worker};
pub(super) use crate::jmap_request::children::{Children, Item, Name};
use std::ops::Range;
use td_json::{arena, scan};

const STRING_BYTES: usize = 1538;

impl Worker<'_, '_, '_, '_, '_> {
    pub(super) fn query_boolean(&mut self, item: &Item) -> Result<bool, Error> {
        if item.kind != scan::Kind::Boolean {
            return Err(Error::InvalidArguments);
        }
        let mut first = [0];
        self.check()?;
        self.retention
            .read_at(item.range.start, &mut first)
            .map_err(Error::Storage)?;
        match first.first() {
            Some(b't') => Ok(true),
            Some(b'f') => Ok(false),
            _ => Err(Error::State),
        }
    }
    pub(super) fn push_string(&mut self, item: &Item) -> Result<Name, Error> {
        if item.kind != scan::Kind::String {
            return Err(Error::InvalidArguments);
        }
        // Accepted IDs/properties are ASCII; six-byte escapes are the largest
        // spelling. Creation references are not generic wire IDs.
        let mut bytes = [0; STRING_BYTES];
        let length = usize::try_from(
            item.range
                .end
                .checked_sub(item.range.start)
                .ok_or(Error::State)?,
        )
        .map_err(|_| Error::InvalidArguments)?;
        let input = bytes.get_mut(..length).ok_or(Error::InvalidArguments)?;
        self.check()?;
        self.retention
            .read_at(item.range.start, input)
            .map_err(Error::Storage)?;
        let mut tokens = [arena::Token::EMPTY; 1];
        let document = arena::parse(
            input,
            &mut tokens,
            arena::ParseLimits {
                max_bytes: STRING_BYTES,
                max_depth: 1,
                max_tokens: 1,
            },
        )
        .map_err(|_| Error::State)?;
        let value = document.root().as_string_bytes().ok_or(Error::State)?;
        let mut name = Name::new();
        name.push(value)?;
        self.check()?;
        Ok(name)
    }
    pub(super) fn push_id(&mut self, value: &Name) -> Result<(), Error> {
        self.append(b"\"")?;
        self.append(value.bytes())?;
        self.append(b"\"")
    }
}

impl Worker<'_, '_, '_, '_, '_> {
    pub(super) fn push_method(
        &mut self,
        name: &str,
        arguments: Range<u64>,
        maximum: usize,
    ) -> Result<Range<u64>, Error> {
        let mut fields = Children::new(arguments, scan::Kind::Object)?;
        let mut ids = None;
        let mut properties = None;
        let mut create = None;
        let mut update = None;
        let mut destroy = None;
        while let Some(item) = fields.next(self)? {
            match (name, item.key.bytes()) {
                ("PushSubscription/get", b"ids") => ids = Some(item),
                ("PushSubscription/get", b"properties") => properties = Some(item),
                ("PushSubscription/set", b"create") => create = Some(item),
                ("PushSubscription/set", b"update") => update = Some(item),
                ("PushSubscription/set", b"destroy") => destroy = Some(item),
                _ => return Err(Error::InvalidArguments),
            }
        }
        if let Some(properties) = properties.filter(|item| item.kind != scan::Kind::Null) {
            let mut values = Children::new(properties.range, scan::Kind::Array)?;
            while let Some(item) = values.next(self)? {
                let property = self.push_string(&item)?;
                match property.bytes() {
                    b"url" | b"keys" => return Err(Error::Forbidden),
                    b"id" | b"deviceClientId" | b"verificationCode" | b"expires" | b"types" => (),
                    _ => return Err(Error::InvalidArguments),
                }
            }
        }
        let start = self.retention.len();
        if name == "PushSubscription/get" {
            self.append(br#"{"list":[],"notFound":"#)?;
            self.push_ids(ids, maximum, false)?;
            self.append(b"}")?;
        } else {
            self.append(br#"{"created":null,"updated":null,"destroyed":null,"notCreated":"#)?;
            let created = self.push_map(create, maximum, b"forbidden")?;
            self.append(br#","notUpdated":"#)?;
            let updated = self.push_map(
                update,
                maximum.checked_sub(created).ok_or(Error::RequestTooLarge)?,
                b"notFound",
            )?;
            self.append(br#","notDestroyed":"#)?;
            self.push_ids(
                destroy,
                maximum
                    .checked_sub(created)
                    .and_then(|n| n.checked_sub(updated))
                    .ok_or(Error::RequestTooLarge)?,
                true,
            )?;
            self.append(b"}")?;
        }
        self.check()?;
        let end = self.retention.publish().map_err(Error::Storage)?;
        Ok(start..end)
    }
    fn push_ids(&mut self, value: Option<Item>, maximum: usize, errors: bool) -> Result<(), Error> {
        let Some(value) = value.filter(|item| item.kind != scan::Kind::Null) else {
            return self.append(if errors { b"null" } else { b"[]" });
        };
        let mut children = Children::new(value.range, scan::Kind::Array)?;
        let mut seen = Vec::new();
        seen.try_reserve_exact(maximum)
            .map_err(|_| Error::ResponseCapacity)?;
        let mut count = 0usize;
        while let Some(item) = children.next(self)? {
            count = count
                .checked_add(1)
                .filter(|n| *n <= maximum)
                .ok_or(Error::RequestTooLarge)?;
            let id = self.push_string(&item)?;
            id.id()?;
            let mut duplicate = false;
            for old in &seen {
                self.check()?;
                if old == &id {
                    duplicate = true;
                    break;
                }
            }
            if duplicate {
                continue;
            }
            if seen.is_empty() {
                self.append(if errors { b"{" } else { b"[" })?;
            } else {
                self.append(b",")?;
            }
            self.push_id(&id)?;
            if errors {
                self.append(br#":{"type":"notFound"}"#)?;
            }
            seen.push(id);
        }
        if seen.is_empty() {
            self.append(if errors { b"null" } else { b"[]" })
        } else {
            self.append(if errors { b"}" } else { b"]" })
        }
    }
    fn push_map(
        &mut self,
        value: Option<Item>,
        maximum: usize,
        error: &[u8],
    ) -> Result<usize, Error> {
        let Some(value) = value.filter(|item| item.kind != scan::Kind::Null) else {
            self.append(b"null")?;
            return Ok(0);
        };
        let mut children = Children::new(value.range, scan::Kind::Object)?;
        let mut count = 0usize;
        while let Some(item) = children.next(self)? {
            count = count
                .checked_add(1)
                .filter(|n| *n <= maximum)
                .ok_or(Error::RequestTooLarge)?;
            if item.kind != scan::Kind::Object {
                return Err(Error::InvalidArguments);
            }
            item.key.id()?;
            if count > 1 {
                self.append(b",")?;
            } else {
                self.append(b"{")?;
            }
            self.push_id(&item.key)?;
            self.append(br#":{"type":""#)?;
            self.append(error)?;
            self.append(b"\"}")?;
        }
        self.append(if count == 0 { b"null" } else { b"}" })?;
        Ok(count)
    }
}

impl Worker<'_, '_, '_, '_, '_> {
    pub(super) fn blob_copy(&mut self, arguments: Range<u64>) -> Result<Range<u64>, Error> {
        let mut fields = Children::new(arguments, scan::Kind::Object)?;
        let mut source = None;
        let mut destination = None;
        let mut blobs = None;
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"fromAccountId" => source = Some(item),
                b"accountId" => destination = Some(item),
                b"blobIds" => blobs = Some(item),
                _ => return Err(Error::InvalidArguments),
            }
        }
        let source = self.copy_account(source.ok_or(Error::InvalidArguments)?)?;
        let destination = self.copy_account(destination.ok_or(Error::InvalidArguments)?)?;
        let mut blobs = Children::new(
            blobs.ok_or(Error::InvalidArguments)?.range,
            scan::Kind::Array,
        )?;
        while let Some(item) = blobs.next(self)? {
            self.push_string(&item)?.id()?;
        }
        self.check()?;
        // RFC 8620 section 6.3 copies between different accounts. There is
        // only one accessible account, so no successful pair is possible.
        Err(if destination != Some(self.account) {
            Error::AccountNotFound
        } else if source != Some(self.account) {
            Error::FromAccountNotFound
        } else {
            Error::InvalidArguments
        })
    }
    fn copy_account(&mut self, item: Item) -> Result<Option<AccountId>, Error> {
        let name = self.push_string(&item)?;
        Ok(name.wire_id()?.local())
    }
}
