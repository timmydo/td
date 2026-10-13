//! The projection requested by td-mail, retained before any response publication.
use super::{
    get_ids::Requested,
    mailbox_get::storage,
    push::{Children, Name},
    Error, Worker,
};
use crate::{
    admission::work::Meter,
    body_value_options::Options,
    email_property::{Field, Property},
    email_response::Stored,
    format::{key::Key, Table},
    ids::EmailId,
    limits::Limits,
    ports::{Clock, ReadView},
    sync::{DataState, DataType},
};
use std::ops::Range;
use td_json::scan;
#[path = "email_get_body.rs"]
mod body;
const DEFAULTS: &[&str] = &[
    "id",
    "threadId",
    "from",
    "to",
    "cc",
    "subject",
    "receivedAt",
    "preview",
    "textBody",
    "htmlBody",
    "bodyValues",
    "keywords",
    "mailboxIds",
    "attachments",
];
struct Selection {
    stored: Stored,
    headers: Vec<Name>,
    text: bool,
    html: bool,
    attachments: bool,
    preview: bool,
    values: bool,
}
impl Selection {
    fn new() -> Self {
        Self {
            stored: Stored::id_only(),
            headers: Vec::new(),
            text: false,
            html: false,
            attachments: false,
            preview: false,
            values: false,
        }
    }
    fn include(&mut self, name: Name, work: &mut Meter, clock: &dyn Clock) -> Result<(), Error> {
        let text = std::str::from_utf8(name.bytes()).map_err(|_| Error::InvalidArguments)?;
        let mut parser = crate::email_property::Cursor::new(text);
        let property = loop {
            if let crate::email_property::Status::Complete(property) = parser
                .poll(body::now(clock)?, work)
                .map_err(|error| match error {
                    crate::email_property::Error::Work(crate::admission::work::Stop::Deadline) => {
                        Error::ExecutionDeadline
                    }
                    crate::email_property::Error::Work(_) => Error::StorageCapacity,
                    _ => Error::InvalidArguments,
                })?
            {
                break property.ok_or(Error::InvalidArguments)?;
            }
        };
        match property {
            Property::Header(_) => {
                if !self.headers.contains(&name) {
                    if self.headers.len() == 64 {
                        return Err(Error::RequestTooLarge);
                    }
                    self.headers
                        .try_reserve(1)
                        .map_err(|_| Error::ResponseCapacity)?;
                    self.headers.push(name);
                }
            }
            Property::Field(field) => match field {
                Field::TextBody => self.text = true,
                Field::HtmlBody => self.html = true,
                Field::Attachments => self.attachments = true,
                Field::Preview => self.preview = true,
                Field::BodyValues => self.values = true,
                Field::BodyStructure | Field::Headers | Field::HasAttachment => {
                    return Err(Error::InvalidArguments)
                }
                field => self.stored.include(field).map_err(storage)?,
            },
        }
        Ok(())
    }
    fn body(&self) -> bool {
        self.text || self.html || self.attachments || self.preview || self.values
    }
}
impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    pub(super) fn email_get(
        &mut self,
        range: Range<u64>,
        limits: &Limits,
    ) -> Result<Range<u64>, Error> {
        let (clock, mut work) = self
            .retention
            .method_body_context(self.execution)
            .map_err(storage)?;
        let mut fields = Children::new(range, scan::Kind::Object)?;
        let (mut account, mut ids, mut properties) = (None, None, None);
        let mut options = Options::default();
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(self.push_string(&item)?),
                b"ids" => ids = Some(item),
                b"properties" => properties = Some(item),
                b"fetchTextBodyValues" => options.fetch_text = self.query_boolean(&item)?,
                b"fetchHTMLBodyValues" => options.fetch_html = self.query_boolean(&item)?,
                b"fetchAllBodyValues" => options.fetch_all = self.query_boolean(&item)?,
                b"maxBodyValueBytes" => {
                    options.max_bytes = u64::try_from(self.query_integer(&item)?)
                        .map_err(|_| Error::InvalidArguments)?
                }
                _ => return Err(Error::InvalidArguments),
            }
        }
        let account = account.ok_or(Error::InvalidArguments)?;
        let mut selected = Selection::new();
        if let Some(properties) = properties.filter(|v| v.kind != scan::Kind::Null) {
            let mut fields = Children::new(properties.range, scan::Kind::Array)?;
            while let Some(item) = fields.next(self)? {
                selected.include(self.push_string(&item)?, &mut work, clock.as_ref())?;
            }
        } else {
            for text in DEFAULTS {
                let mut name = Name::new();
                name.push(text.as_bytes())?;
                selected.include(name, &mut work, clock.as_ref())?;
            }
        }
        let all = ids.as_ref().is_none_or(|v| v.kind == scan::Kind::Null);
        let mut requested = self.get_ids::<EmailId>(
            ids.filter(|v| v.kind != scan::Kind::Null),
            limits.objects_per_method,
        )?;
        if account.wire_id()?.local() != Some(self.account) {
            return Err(Error::AccountNotFound);
        }
        let mut view = self
            .retention
            .method_view(self.account, self.execution)
            .map_err(storage)?;
        let mut row = body::buffer(crate::format::MAX_VALUE_BYTES)?;
        if all {
            let mut after = None::<[u8; 16]>;
            let mut key = [0; 16];
            loop {
                self.check()?;
                let Some(record) = view
                    .next(
                        Table::Emails,
                        after.as_ref().map(|v| v.as_slice()),
                        &mut key,
                        &mut row,
                    )
                    .map_err(storage)?
                else {
                    break;
                };
                let Key::Email(id) = record.key else {
                    return Err(Error::State);
                };
                if requested.len() == limits.objects_per_method {
                    return Err(Error::RequestTooLarge);
                }
                after = Some(*id.as_bytes());
                requested.push(Requested {
                    name: None,
                    id: Some(id),
                    found: false,
                });
            }
        }
        let identity = view.identity();
        let mut state = [0; DataState::WIRE_BYTES];
        let state = DataState {
            account: identity.account,
            epoch: identity.epoch,
            kind: DataType::Email,
            sequence: identity.committed_sequence,
        }
        .encode(&mut state)
        .map_err(|_| Error::State)?;
        let start = self.retention.len();
        self.append(br#"{"accountId":"#)?;
        self.push_id(&account)?;
        self.append(br#","state":""#)?;
        self.append(state.as_bytes())?;
        self.append(br#"","list":["#)?;
        let mut first = true;
        for item in &mut requested {
            self.check()?;
            let Some(id) = item.id else {
                continue;
            };
            let Some(metadata) = view.email_metadata(id, &mut row).map_err(storage)? else {
                continue;
            };
            if !first {
                self.append(b",")?;
            }
            first = false;
            let mut sink_error = None;
            let found = crate::email_response::write_members(
                &mut view,
                id,
                selected.stored,
                &mut row,
                |bytes| {
                    self.append(bytes).map_err(|e| {
                        sink_error = Some(e);
                        crate::ports::Error::Invalid
                    })
                },
                false,
            );
            if let Some(error) = sink_error {
                return Err(error);
            }
            if !found.map_err(storage)? {
                return Err(Error::State);
            }
            if selected.body() || !selected.headers.is_empty() {
                let result = body::write(
                    self,
                    &mut view,
                    metadata.blob,
                    limits,
                    (&selected, options),
                    clock.as_ref(),
                    &mut work,
                );
                self.check()?;
                if let Some(stop) = work.stopped() {
                    return Err(match stop {
                        crate::admission::work::Stop::Deadline => Error::ExecutionDeadline,
                        _ => Error::StorageCapacity,
                    });
                }
                result?;
            }
            self.append(b"}")?;
            item.found = true;
        }
        self.append(br#"],"notFound":["#)?;
        self.get_not_found(&requested)?;
        self.append(b"]}")?;
        view.check_snapshot().map_err(storage)?;
        self.check()?;
        let end = self.retention.publish().map_err(Error::Storage)?;
        Ok(start..end)
    }
}

#[cfg(test)]
#[path = "email_get_tests.rs"]
mod tests;
