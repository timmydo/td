//! Retained Mailbox/get; live capability admission remains separate.
use super::{get_ids::Requested, push::Children, Error, Worker};
use crate::{
    format::{self, key::Key, row::Row, Table},
    ids::MailboxId,
    mailbox_response::{self, Projection, Rights},
    ports::{self, ReadView},
    store_fs::MAX_MAILBOX_COUNTS,
    sync::{DataState, DataType},
};
use std::ops::Range;
use td_json::scan;

pub(super) fn storage(error: ports::Error) -> Error {
    match error {
        ports::Error::Deadline => Error::ExecutionDeadline,
        ports::Error::Capacity => Error::StorageCapacity,
        ports::Error::Busy => Error::StorageBusy,
        error => Error::Storage(error),
    }
}
impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    pub(super) fn mailbox_get(
        &mut self,
        arguments: Range<u64>,
        maximum: usize,
    ) -> Result<Range<u64>, Error> {
        let mut fields = Children::new(arguments, scan::Kind::Object)?;
        let (mut account, mut ids, mut properties) = (None, None, None);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(item),
                b"ids" => ids = Some(item),
                b"properties" => properties = Some(item),
                _ => return Err(Error::InvalidArguments),
            }
        }
        let account = self.push_string(&account.ok_or(Error::InvalidArguments)?)?;
        let account_id = account.wire_id()?.local();
        let mut projection = Projection::all();
        if let Some(properties) = properties.filter(|item| item.kind != scan::Kind::Null) {
            projection = Projection::id_only();
            let mut children = Children::new(properties.range, scan::Kind::Array)?;
            while let Some(item) = children.next(self)? {
                let name = self.push_string(&item)?;
                projection
                    .include(
                        std::str::from_utf8(name.bytes()).map_err(|_| Error::InvalidArguments)?,
                    )
                    .map_err(|_| Error::InvalidArguments)?;
            }
        }
        let all = ids
            .as_ref()
            .is_none_or(|item| item.kind == scan::Kind::Null);
        let mut requested =
            self.get_ids::<MailboxId>(ids.filter(|item| item.kind != scan::Kind::Null), maximum)?;
        if account_id != Some(self.account) {
            return Err(Error::AccountNotFound);
        }
        self.check()?;
        let mut view = self
            .retention
            .method_view(self.account, self.execution)
            .map_err(storage)?;
        let mut scratch = vec![0; format::MAX_VALUE_BYTES];
        if all {
            let mut after = None::<[u8; 16]>;
            let mut key = [0; 16];
            while let Some(record) = view
                .next(
                    Table::Mailboxes,
                    after.as_ref().map(|a| a.as_slice()),
                    &mut key,
                    &mut scratch,
                )
                .map_err(storage)?
            {
                self.check()?;
                if requested.len() == maximum {
                    return Err(Error::RequestTooLarge);
                }
                let Key::Mailbox(id) = record.key else {
                    return Err(Error::State);
                };
                requested.push(Requested {
                    name: None,
                    id: Some(id),
                    found: false,
                });
                after = Some(*id.as_bytes());
            }
        }
        let identity = view.identity();
        let mut state_bytes = [0; DataState::WIRE_BYTES];
        let state = DataState {
            account: identity.account,
            epoch: identity.epoch,
            kind: DataType::Mailbox,
            sequence: identity.committed_sequence,
        }
        .encode(&mut state_bytes)
        .map_err(|_| Error::State)?;
        let start = self.retention.len();
        self.append(br#"{"accountId":"#)?;
        self.push_id(&account)?;
        self.append(br#","state":""#)?;
        self.append(state.as_bytes())?;
        self.append(br#"","list":["#)?;
        let mut output = vec![0; mailbox_response::MAX_OBJECT_BYTES];
        let mut batch = Vec::new();
        batch
            .try_reserve_exact(MAX_MAILBOX_COUNTS)
            .map_err(|_| Error::ResponseCapacity)?;
        let mut first = true;
        for chunk in requested.chunks_mut(MAX_MAILBOX_COUNTS) {
            batch.clear();
            for item in chunk.iter() {
                if let Some(id) = item.id {
                    batch.push(id);
                }
            }
            self.check()?;
            let counts = if projection.needs_counts() && !batch.is_empty() {
                view.mailbox_counts_many(&batch, &mut scratch)
                    .map_err(storage)?
            } else {
                Vec::new()
            };
            if projection.needs_counts() && counts.len() != batch.len() {
                return Err(Error::State);
            }
            let mut counts = counts.iter();
            for item in chunk {
                self.check()?;
                let Some(id) = item.id else {
                    continue;
                };
                let count = if projection.needs_counts() {
                    counts.next().ok_or(Error::State)?.as_ref()
                } else {
                    None
                };
                let Some((row, _)) = view.get(Key::Mailbox(id), &mut scratch).map_err(storage)?
                else {
                    continue;
                };
                let Row::Mailbox(_) = row else {
                    return Err(Error::Storage(ports::Error::Corrupt));
                };
                let rights = Rights {
                    may_read_items: true,
                    may_add_items: self.flags.is_some(),
                    may_remove_items: self.flags.is_some(),
                    may_set_seen: self.flags.is_some(),
                    may_set_keywords: self.flags.is_some(),
                    may_create_child: false,
                    may_rename: false,
                    may_delete: false,
                    may_submit: false,
                };
                let length = mailbox_response::encode(
                    &mut view,
                    id,
                    projection,
                    rights,
                    count,
                    &mut scratch,
                    &mut output,
                )
                .map_err(storage)?
                .ok_or(Error::State)?;
                if !first {
                    self.append(b",")?;
                }
                first = false;
                self.append(output.get(..length).ok_or(Error::State)?)?;
                item.found = true;
            }
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
#[path = "mailbox_get_tests.rs"]
pub(super) mod tests;
