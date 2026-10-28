//! Authenticated passive source inventory and metadata.
use super::{push::Children, Error, Worker};
use crate::{
    email_response::Stored,
    ids::EmailId,
    postgres_read::{ReplicaInventoryCursor, MAX_REPLICA_INVENTORY_PAGE},
    sync::{DataState, DataType},
    wire,
};
use std::ops::Range;
use td_json::scan;

impl Worker<'_, '_, '_, '_, '_> {
    fn replica_state(&mut self, state: DataState) -> Result<(), Error> {
        let mut bytes = [0; DataState::WIRE_BYTES];
        self.append(
            state
                .encode(&mut bytes)
                .map_err(|_| Error::State)?
                .as_bytes(),
        )
    }

    fn replica_source(&mut self, state: DataState) -> Result<(), Error> {
        let mut bytes = [0; 32];
        wire::hex(state.epoch.as_bytes(), &mut bytes).map_err(|_| Error::State)?;
        self.append(&bytes)
    }

    pub(super) fn replica_inventory(
        &mut self,
        arguments: Range<u64>,
        maximum: usize,
    ) -> Result<Range<u64>, Error> {
        let mut timer =
            crate::performance::Timer::start(crate::performance::Phase::ReplicaInventory);
        let maximum = maximum.min(MAX_REPLICA_INVENTORY_PAGE);
        let mut fields = Children::new(arguments, scan::Kind::Object)?;
        let (mut account, mut cursor, mut limit) = (None, None, maximum);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(self.push_string(&item)?),
                b"cursor" if item.kind == scan::Kind::Null => (),
                b"cursor" => {
                    let text = self.push_string(&item)?;
                    cursor = Some(
                        ReplicaInventoryCursor::decode(
                            std::str::from_utf8(text.bytes())
                                .map_err(|_| Error::InvalidArguments)?,
                        )
                        .map_err(|_| Error::InvalidArguments)?,
                    );
                }
                b"limit" => {
                    limit = usize::try_from(self.query_integer(&item)?)
                        .map_err(|_| Error::InvalidArguments)?;
                    if limit == 0 {
                        return Err(Error::InvalidArguments);
                    }
                    if limit > maximum {
                        return Err(Error::RequestTooLarge);
                    }
                }
                _ => return Err(Error::InvalidArguments),
            }
        }
        let account = account.ok_or(Error::InvalidArguments)?;
        if account.wire_id()?.local() != Some(self.account) {
            return Err(Error::AccountNotFound);
        }
        let mut read = self.read.take();
        let result = (|| {
            let view = read.as_mut().ok_or(Error::State)?;
            let page = view.replica_inventory(cursor, limit)?;
            let start = self.retention.len();
            self.append(br#"{"accountId":"#)?;
            self.push_id(&account)?;
            self.append(br#","sourceId":""#)?;
            self.replica_source(page.observed)?;
            self.append(br#"","baseState":""#)?;
            self.replica_state(page.base)?;
            self.append(br#"","state":""#)?;
            self.replica_state(page.observed)?;
            self.append(br#"","ids":["#)?;
            let mut first = true;
            for id in page.ids {
                self.check()?;
                if !first {
                    self.append(b",")?;
                }
                first = false;
                let mut bytes = [0; wire::IdKind::WIRE_BYTES];
                self.append(b"\"")?;
                self.append(
                    wire::encode_id(id, &mut bytes)
                        .map_err(|_| Error::State)?
                        .as_bytes(),
                )?;
                self.append(b"\"")?;
            }
            self.append(br#"],"nextCursor":"#)?;
            match page.next {
                None => self.append(b"null")?,
                Some(cursor) => {
                    let mut bytes = [0; ReplicaInventoryCursor::WIRE_BYTES];
                    self.append(b"\"")?;
                    self.append(
                        cursor
                            .encode(&mut bytes)
                            .map_err(|_| Error::State)?
                            .as_bytes(),
                    )?;
                    self.append(b"\"")?;
                }
            }
            self.append(b"}")?;
            view.check()?;
            self.check()?;
            Ok(start..self.retention.publish().map_err(Error::Storage)?)
        })();
        self.read = read;
        if result.is_ok() {
            timer.succeeded();
        }
        result
    }

    pub(super) fn replica_metadata(
        &mut self,
        arguments: Range<u64>,
        maximum: usize,
    ) -> Result<Range<u64>, Error> {
        let mut timer =
            crate::performance::Timer::start(crate::performance::Phase::ReplicaMetadata);
        let maximum = maximum.min(crate::postgres_body::MAX_METADATA_IDS);
        let mut fields = Children::new(arguments, scan::Kind::Object)?;
        let (mut account, mut ids) = (None, None);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(self.push_string(&item)?),
                b"ids" if item.kind == scan::Kind::Array => ids = Some(item),
                _ => return Err(Error::InvalidArguments),
            }
        }
        let account = account.ok_or(Error::InvalidArguments)?;
        if account.wire_id()?.local() != Some(self.account) {
            return Err(Error::AccountNotFound);
        }
        let mut requested =
            self.get_ids::<EmailId>(Some(ids.ok_or(Error::InvalidArguments)?), maximum)?;
        let mut read = self.read.take();
        let result = (|| {
            let view = &mut **read.as_mut().ok_or(Error::State)?;
            let identity = view.identity();
            let state = DataState {
                account: identity.account,
                epoch: identity.epoch,
                kind: DataType::Email,
                sequence: identity.committed_sequence,
            };
            let mut selected = Vec::new();
            selected
                .try_reserve_exact(requested.len())
                .map_err(|_| Error::StorageCapacity)?;
            selected.extend(requested.iter().filter_map(|item| item.id));
            view.prefetch_metadata(&selected, Stored::all())?;
            let mut row = Vec::new();
            row.try_reserve_exact(crate::format::MAX_VALUE_BYTES)
                .map_err(|_| Error::StorageCapacity)?;
            row.resize(crate::format::MAX_VALUE_BYTES, 0);
            let start = self.retention.len();
            self.append(br#"{"accountId":"#)?;
            self.push_id(&account)?;
            self.append(br#","sourceId":""#)?;
            self.replica_source(state)?;
            self.append(br#"","state":""#)?;
            self.replica_state(state)?;
            self.append(br#"","list":["#)?;
            let mut first = true;
            for item in &mut requested {
                self.check()?;
                let Some(id) = item.id else {
                    continue;
                };
                let Some(metadata) = view.replica_metadata(id)? else {
                    continue;
                };
                if metadata.identity != identity || metadata.email != id {
                    return Err(Error::State);
                }
                if !first {
                    self.append(b",")?;
                }
                first = false;
                let mut sink_error = None;
                let found = crate::email_response::write_members(
                    view,
                    id,
                    Stored::all(),
                    &mut row,
                    |bytes| {
                        self.append(bytes).map_err(|error| {
                            sink_error = Some(error);
                            crate::ports::Error::Invalid
                        })
                    },
                    false,
                );
                if let Some(error) = sink_error {
                    return Err(error);
                }
                if !found.map_err(super::postgres_read_error)? {
                    return Err(Error::State);
                }
                self.append(br#", "receivedAtMs":""#)?;
                self.append(metadata.received_at.to_string().as_bytes())?;
                self.append(br#"","bodySha256":""#)?;
                let mut digest = [0; 64];
                wire::hex(&metadata.body.digest, &mut digest).map_err(|_| Error::State)?;
                self.append(&digest)?;
                self.append(br#""}"#)?;
                item.found = true;
            }
            self.append(br#"],"notFound":["#)?;
            self.get_not_found(&requested)?;
            self.append(b"]}")?;
            view.check()?;
            self.check()?;
            Ok(start..self.retention.publish().map_err(Error::Storage)?)
        })();
        self.read = read;
        if result.is_ok() {
            timer.succeeded();
        }
        result
    }
}
