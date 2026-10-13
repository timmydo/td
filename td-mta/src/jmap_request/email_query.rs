//! td-mail inbox paging over one retained snapshot and bounded external sort.
use super::{
    mailbox_get::storage,
    push::{Children, Name},
    Error, Worker,
};
use crate::{
    external_sort::{self, Next, Progress, Sorter},
    format::{key::Key, row::Row, Table, MAX_VALUE_BYTES},
    ids::{EmailId, MailboxId},
    ports::{self, ReadView},
    store_fs::{IndexReadView, SortError, SortSource, SortWork},
    sync::{QueryState, QueryType},
    wire,
};
use std::{cmp::Ordering, ops::Range};
use td_json::scan;
const RECORD: usize = 24;
const RUN: usize = 256;
struct Arguments {
    account: Name,
    mailbox: Option<Option<MailboxId>>,
    ascending: bool,
    position: u64,
    limit: usize,
    include_limit: bool,
    total: bool,
}
impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    fn email_query_arguments(
        &mut self,
        range: Range<u64>,
        page: usize,
    ) -> Result<Arguments, Error> {
        let mut fields = Children::new(range, scan::Kind::Object)?;
        let mut account = None;
        let mut mailbox = None;
        let (mut ascending, mut total) = (false, false);
        let (mut position, mut limit, mut include_limit) = (0, page, true);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(self.push_string(&item)?),
                b"filter" if item.kind == scan::Kind::Null => (),
                b"filter" => {
                    let mut filter = Children::new(item.range, scan::Kind::Object)?;
                    while let Some(item) = filter.next(self)? {
                        match item.key.bytes() {
                            b"inMailbox" => {
                                mailbox = Some(self.push_string(&item)?.wire_id()?.local())
                            }
                            _ => return Err(Error::UnsupportedFilter),
                        }
                    }
                }
                b"sort" if item.kind == scan::Kind::Null => (),
                b"sort" => {
                    let mut sort = Children::new(item.range, scan::Kind::Array)?;
                    if let Some(item) = sort.next(self)? {
                        let mut comparator = Children::new(item.range, scan::Kind::Object)?;
                        let mut property = false;
                        ascending = true;
                        while let Some(item) = comparator.next(self)? {
                            match item.key.bytes() {
                                b"property" => {
                                    if self
                                        .push_string(&item)
                                        .map_err(|error| {
                                            if error == Error::InvalidArguments
                                                && item.kind == scan::Kind::String
                                            {
                                                Error::UnsupportedSort
                                            } else {
                                                error
                                            }
                                        })?
                                        .bytes()
                                        != b"receivedAt"
                                    {
                                        return Err(Error::UnsupportedSort);
                                    }
                                    property = true;
                                }
                                b"isAscending" => ascending = self.query_boolean(&item)?,
                                b"collation" if item.kind == scan::Kind::Null => (),
                                b"collation" if item.kind == scan::Kind::String => (),
                                b"collation" => return Err(Error::InvalidArguments),
                                _ => return Err(Error::InvalidArguments),
                            }
                        }
                        if !property {
                            return Err(Error::InvalidArguments);
                        }
                        if sort.next(self)?.is_some() {
                            return Err(Error::UnsupportedSort);
                        }
                    }
                }
                b"collapseThreads" => {
                    if self.query_boolean(&item)? {
                        return Err(Error::InvalidArguments);
                    }
                }
                b"calculateTotal" => total = self.query_boolean(&item)?,
                b"position" => {
                    position = u64::try_from(self.query_integer(&item)?)
                        .map_err(|_| Error::InvalidArguments)?
                }
                b"limit" if item.kind == scan::Kind::Null => (),
                b"limit" => {
                    let requested = u64::try_from(self.query_integer(&item)?)
                        .map_err(|_| Error::InvalidArguments)?;
                    limit = page.min(usize::try_from(requested).unwrap_or(usize::MAX));
                    include_limit = requested > page as u64;
                }
                _ => return Err(Error::InvalidArguments),
            }
        }
        let account = account.ok_or(Error::InvalidArguments)?;
        if account.wire_id()?.local() != Some(self.account) {
            return Err(Error::AccountNotFound);
        }
        Ok(Arguments {
            account,
            mailbox,
            ascending,
            position,
            limit,
            include_limit,
            total,
        })
    }
}
struct Source<'w, 'io, 'c, 's, 'r, 'sr, 'a, P> {
    worker: &'w mut Worker<'io, 'c, 's, 'r, 'sr, 'a, P>,
    view: IndexReadView<'c, 'r>,
    arguments: Arguments,
    after: Option<[u8; 16]>,
    value: Vec<u8>,
    pending: Option<(EmailId, i64)>,
    error: Option<Error>,
}
fn record_id(record: &[u8; RECORD]) -> Result<EmailId, ports::Error> {
    let id = record
        .get(8..)
        .ok_or(ports::Error::Corrupt)?
        .try_into()
        .map_err(|_| ports::Error::Corrupt)?;
    Ok(EmailId::from_bytes(id))
}
impl<P> Source<'_, '_, '_, '_, '_, '_, '_, P> {
    fn next_inner(
        &mut self,
        output: &mut [u8; RECORD],
        work: &mut dyn SortWork,
    ) -> Result<Next, Error> {
        self.worker.check()?;
        if self.arguments.mailbox == Some(None)
            || (self.arguments.limit == 0 && !self.arguments.total)
        {
            return Ok(Next::Complete);
        }
        let (id, date) = if let Some(pending) = self.pending.take() {
            pending
        } else {
            work.charge(MAX_VALUE_BYTES as u64, 1).map_err(storage)?;
            let mut key = [0; 16];
            let Some(record) = self
                .view
                .next(
                    Table::Emails,
                    self.after.as_ref().map(|id| id.as_slice()),
                    &mut key,
                    &mut self.value,
                )
                .map_err(storage)?
            else {
                self.view.check_snapshot().map_err(storage)?;
                return Ok(Next::Complete);
            };
            let (Key::Email(id), Row::Email(row)) = (record.key, record.row) else {
                return Err(Error::Storage(ports::Error::Corrupt));
            };
            self.after = Some(*id.as_bytes());
            self.pending = Some((id, row.received_at));
            // The row may consume the full 64 KiB turn. Membership is a new turn.
            return Ok(Next::Skipped);
        };
        if let Some(Some(mailbox)) = self.arguments.mailbox {
            work.charge(32, 1).map_err(storage)?;
            match self
                .view
                .get(Key::Membership(id, mailbox), &mut [])
                .map_err(storage)?
            {
                None => return Ok(Next::Skipped),
                Some((Row::Membership, _)) => (),
                Some(_) => return Err(Error::Storage(ports::Error::Corrupt)),
            }
        }
        output
            .get_mut(..8)
            .ok_or(Error::State)?
            .copy_from_slice(&((date as u64) ^ (1 << 63)).to_be_bytes());
        output
            .get_mut(8..)
            .ok_or(Error::State)?
            .copy_from_slice(id.as_bytes());
        Ok(Next::Record)
    }
}
impl<P> SortSource<RECORD> for Source<'_, '_, '_, '_, '_, '_, '_, P> {
    fn next(
        &mut self,
        output: &mut [u8; RECORD],
        work: &mut dyn SortWork,
    ) -> Result<Next, ports::Error> {
        self.next_inner(output, work).map_err(|error| {
            self.error = Some(error);
            ports::Error::Invalid
        })
    }
    fn compare(
        &mut self,
        a: &[u8; RECORD],
        b: &[u8; RECORD],
        work: &mut dyn SortWork,
    ) -> Result<Ordering, ports::Error> {
        work.charge(0, 1)?;
        let order = a
            .get(..8)
            .ok_or(ports::Error::Corrupt)?
            .cmp(b.get(..8).ok_or(ports::Error::Corrupt)?);
        Ok((if self.arguments.ascending {
            order
        } else {
            order.reverse()
        })
        .then_with(|| a.get(8..).cmp(&b.get(8..))))
    }
}
impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    pub(super) fn email_query(
        &mut self,
        range: Range<u64>,
        page: usize,
    ) -> Result<Range<u64>, Error> {
        let arguments = self.email_query_arguments(range, page)?;
        let execution = self.execution;
        let (coordinator, spool, cap) = self
            .retention
            .method_sort_context(execution)
            .map_err(storage)?;
        let view = self
            .retention
            .method_view(self.account, execution)
            .map_err(storage)?;
        let state = QueryState::current(view.identity(), QueryType::Email);
        let mut value = Vec::new();
        value
            .try_reserve_exact(MAX_VALUE_BYTES)
            .map_err(|_| Error::StorageCapacity)?;
        value.resize(MAX_VALUE_BYTES, 0);
        let source = Source {
            worker: self,
            view,
            arguments,
            after: None,
            value,
            pending: None,
            error: None,
        };
        let (mut workspace, mut job) = coordinator
            .sort_job::<RECORD, _>(spool, RUN, cap, execution, source)
            .map_err(|error| match error {
                SortError::Store(error) => storage(error),
                SortError::Ledger(_) => Error::StorageBusy,
            })?;
        let output = {
            let (run, heads) = workspace.buffers();
            let mut sorter = Sorter::new(run, heads).map_err(|_| Error::State)?;
            loop {
                match sorter.poll(&mut job) {
                    Ok(Progress::Pending) => (),
                    Ok(Progress::Complete(output)) => break output,
                    Err(error) => {
                        if let Some(error) = job.source_mut().error.take() {
                            return Err(error);
                        }
                        return Err(match error {
                            external_sort::Error::Backend(error) => storage(error),
                            _ => Error::State,
                        });
                    }
                }
            }
        };
        drop(workspace);
        let start = job.source_mut().worker.retention.len();
        let (position, limit) = {
            let source = job.source_mut();
            let args = &source.arguments;
            let worker = &mut source.worker;
            worker.append(br#"{"accountId":"#)?;
            worker.push_id(&args.account)?;
            worker.append(br#","queryState":""#)?;
            worker.append(
                state
                    .encode(&mut [0; QueryState::WIRE_BYTES])
                    .map_err(|_| Error::State)?
                    .as_bytes(),
            )?;
            worker.append(br#"","canCalculateChanges":false,"position":"#)?;
            worker.append(args.position.to_string().as_bytes())?;
            if args.include_limit {
                worker.append(br#","limit":"#)?;
                worker.append(args.limit.to_string().as_bytes())?;
            }
            if args.total {
                worker.append(br#","total":"#)?;
                worker.append(output.records.to_string().as_bytes())?;
            }
            worker.append(br#","ids":["#)?;
            (args.position, args.limit)
        };
        let end = output.records.min(position.saturating_add(limit as u64));
        let mut record = [0; RECORD];
        for index in position..end {
            job.read_output(output, index, &mut record)
                .map_err(storage)?;
            let id = record_id(&record).map_err(storage)?;
            let worker = &mut job.source_mut().worker;
            if index != position {
                worker.append(b",")?;
            }
            worker.append(b"\"")?;
            worker.append(
                wire::encode_id(id, &mut [0; wire::IdKind::WIRE_BYTES])
                    .map_err(|_| Error::State)?
                    .as_bytes(),
            )?;
            worker.append(b"\"")?;
        }
        let source = job.source_mut();
        source.worker.append(b"]}")?;
        source.view.check_snapshot().map_err(storage)?;
        source.worker.check()?;
        let end = source.worker.retention.publish().map_err(Error::Storage)?;
        Ok(start..end)
    }
}
#[cfg(test)]
#[path = "email_query_tests.rs"]
mod tests;
