//! Mailbox/query over one native snapshot and quota-charged external sort.
use super::{
    mailbox_get::storage,
    mailbox_query_args::{Arguments, Mode},
    mailbox_query_filter::Filter,
    Error, Worker,
};
use crate::{
    external_sort::{self, Next, Progress, Sorter},
    format::{
        key::Key,
        row::{MailboxRow, Row, MAX_MAILBOX_NAME, MAX_ROLE},
        Table,
    },
    ids::MailboxId,
    mailbox_order,
    mailbox_tree::{self, Walk},
    ports::{self, ReadView},
    store_fs::{IndexReadView, SortError, SortSource, SortWork},
    sync::{QueryState, QueryType},
    wire,
};
use std::{cmp::Ordering, ops::Range};
const RECORD: usize = 1 + mailbox_tree::MAX_DEPTH * 16;
const RUN: usize = 256;
const _: () = assert!(
    super::mailbox_query_filter::MAX_RESIDENT_BYTES
        + (RUN + external_sort::MAX_MERGE_RUNS) * RECORD
        + 32 * 1024
        < 1 << 20
);

struct OwnedRow {
    name: [u8; MAX_MAILBOX_NAME],
    name_len: usize,
    role: [u8; MAX_ROLE],
    role_len: Option<usize>,
    parent: Option<MailboxId>,
    sort_order: u32,
    subscribed: bool,
}
impl OwnedRow {
    fn new() -> Self {
        Self {
            name: [0; MAX_MAILBOX_NAME],
            name_len: 0,
            role: [0; MAX_ROLE],
            role_len: None,
            parent: None,
            sort_order: 0,
            subscribed: false,
        }
    }
    fn assign(&mut self, row: MailboxRow<'_>) -> Result<(), ports::Error> {
        self.name
            .get_mut(..row.name.len())
            .ok_or(ports::Error::Corrupt)?
            .copy_from_slice(row.name.as_bytes());
        self.name_len = row.name.len();
        self.role_len = row.role.map(str::len);
        if let Some(role) = row.role {
            self.role
                .get_mut(..role.len())
                .ok_or(ports::Error::Corrupt)?
                .copy_from_slice(role.as_bytes());
        }
        self.parent = row.parent;
        self.sort_order = row.sort_order;
        self.subscribed = row.subscribed;
        Ok(())
    }
    fn row(&self) -> Result<MailboxRow<'_>, ports::Error> {
        let name = std::str::from_utf8(
            self.name
                .get(..self.name_len)
                .ok_or(ports::Error::Corrupt)?,
        )
        .map_err(|_| ports::Error::Corrupt)?;
        let role = self
            .role_len
            .map(|len| {
                self.role
                    .get(..len)
                    .ok_or(ports::Error::Corrupt)
                    .and_then(|bytes| std::str::from_utf8(bytes).map_err(|_| ports::Error::Corrupt))
            })
            .transpose()?;
        Ok(MailboxRow {
            name,
            parent: self.parent,
            role,
            sort_order: self.sort_order,
            subscribed: self.subscribed,
        })
    }
}
enum Stage {
    Validate,
    Scan,
    Path,
    FilterStart,
    Filter,
    Ancestor,
    Emit,
}
struct Source<'w, 'io, 'c, 's, 'r, 'sr, 'a, P> {
    worker: &'w mut Worker<'io, 'c, 's, 'r, 'sr, 'a, P>,
    view: IndexReadView<'c, 'r>,
    arguments: Arguments,
    depth: usize,
    nodes: usize,
    stage: Stage,
    filter: Option<Filter>,
    path: Option<Walk>,
    record: [u8; RECORD],
    ancestor: usize,
    row: OwnedRow,
    after: Option<[u8; 16]>,
    error: Option<Error>,
}
fn tree(error: crate::mailbox_parents::Error) -> Error {
    match error {
        crate::mailbox_parents::Error::View(error) => storage(error),
        _ => Error::Storage(ports::Error::Corrupt),
    }
}
fn count(record: &[u8; RECORD]) -> Result<usize, ports::Error> {
    let count = usize::from(*record.first().ok_or(ports::Error::Corrupt)?);
    if count == 0 || count > mailbox_tree::MAX_DEPTH {
        return Err(ports::Error::Corrupt);
    }
    Ok(count)
}
fn id_at(record: &[u8; RECORD], index: usize) -> Result<MailboxId, ports::Error> {
    if index >= count(record)? {
        return Err(ports::Error::Corrupt);
    }
    let start = 1 + index * 16;
    let bytes: [u8; 16] = record
        .get(start..start + 16)
        .ok_or(ports::Error::Corrupt)?
        .try_into()
        .map_err(|_| ports::Error::Corrupt)?;
    Ok(MailboxId::from_bytes(bytes))
}
fn record_id(record: &[u8; RECORD]) -> Result<MailboxId, ports::Error> {
    id_at(record, count(record)? - 1)
}
impl<P> Source<'_, '_, '_, '_, '_, '_, '_, P> {
    fn next_inner(
        &mut self,
        output: &mut [u8; RECORD],
        work: &mut dyn SortWork,
    ) -> Result<Next, Error> {
        self.worker.check()?;
        match self.stage {
            Stage::Validate => {
                if self.filter.is_none() {
                    self.filter = Some(Filter::new(
                        self.arguments.filter.clone(),
                        self.depth,
                        self.nodes,
                        true,
                    )?);
                }
                if self
                    .filter
                    .as_mut()
                    .ok_or(Error::State)?
                    .poll(self.worker, None, work)?
                    .is_some()
                {
                    if self.arguments.mode == Mode::Changes {
                        return Err(Error::CannotCalculateChanges);
                    }
                    self.filter
                        .as_mut()
                        .ok_or(Error::State)?
                        .reset(None, self.nodes)?;
                    self.stage = Stage::Scan;
                }
            }
            Stage::Scan => {
                work.charge(2048, 1).map_err(storage)?;
                let mut key = [0; 16];
                let mut value = [0; 2048];
                let Some(record) = self
                    .view
                    .next(
                        Table::Mailboxes,
                        self.after.as_ref().map(|a| a.as_slice()),
                        &mut key,
                        &mut value,
                    )
                    .map_err(storage)?
                else {
                    self.view.check_snapshot().map_err(storage)?;
                    self.filter = None;
                    return Ok(Next::Complete);
                };
                let (Key::Mailbox(id), Row::Mailbox(row)) = (record.key, record.row) else {
                    return Err(Error::Storage(ports::Error::Corrupt));
                };
                self.row.assign(row).map_err(storage)?;
                self.after = Some(*id.as_bytes());
                self.record.fill(0);
                if self.arguments.sort_tree || self.arguments.filter_tree {
                    self.path = Some(Walk::new(self.view.identity(), id).map_err(tree)?);
                    self.stage = Stage::Path;
                } else {
                    *self.record.first_mut().ok_or(Error::State)? = 1;
                    self.record
                        .get_mut(1..17)
                        .ok_or(Error::State)?
                        .copy_from_slice(id.as_bytes());
                    self.ancestor = 0;
                    self.stage = Stage::FilterStart;
                }
            }
            Stage::Path => {
                work.charge(2048, 1).map_err(storage)?;
                let path = self.path.as_mut().ok_or(Error::State)?;
                path.advance(&mut self.view, &mut [0; 2048]).map_err(tree)?;
                if path.is_complete() {
                    let path = self
                        .path
                        .take()
                        .ok_or(Error::State)?
                        .finish()
                        .map_err(tree)?;
                    *self.record.first_mut().ok_or(Error::State)? = path.from_root().len() as u8;
                    for (index, id) in path.from_root().enumerate() {
                        let start = 1 + index * 16;
                        self.record
                            .get_mut(start..start + 16)
                            .ok_or(Error::State)?
                            .copy_from_slice(id.as_bytes());
                    }
                    self.ancestor = count(&self.record).map_err(storage)? - 1;
                    self.stage = Stage::FilterStart;
                }
            }
            Stage::FilterStart => {
                self.filter
                    .as_mut()
                    .ok_or(Error::State)?
                    .reset(self.arguments.filter.clone(), self.nodes)?;
                self.stage = Stage::Filter;
            }
            Stage::Filter => {
                let row = self.row.row().map_err(storage)?;
                if let Some(matched) =
                    self.filter
                        .as_mut()
                        .ok_or(Error::State)?
                        .poll(self.worker, Some(row), work)?
                {
                    if !matched {
                        self.stage = Stage::Scan;
                    } else if self.arguments.filter_tree && self.ancestor != 0 {
                        self.ancestor -= 1;
                        self.stage = Stage::Ancestor;
                    } else {
                        self.stage = Stage::Emit;
                    }
                }
            }
            Stage::Ancestor => {
                work.charge(2048, 1).map_err(storage)?;
                let id = id_at(&self.record, self.ancestor).map_err(storage)?;
                let mut value = [0; 2048];
                let (Row::Mailbox(row), _) = self
                    .view
                    .get(Key::Mailbox(id), &mut value)
                    .map_err(storage)?
                    .ok_or(Error::Storage(ports::Error::Corrupt))?
                else {
                    return Err(Error::Storage(ports::Error::Corrupt));
                };
                self.row.assign(row).map_err(storage)?;
                self.stage = Stage::FilterStart;
            }
            Stage::Emit => {
                *output = self.record;
                self.stage = Stage::Scan;
                return Ok(Next::Record);
            }
        }
        Ok(Next::Skipped)
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
        let (mut left, mut right) = (record_id(a)?, record_id(b)?);
        if self.arguments.sort_tree {
            let (alen, blen) = (count(a)?, count(b)?);
            let mut diverged = false;
            for index in 0..alen.min(blen) {
                let (a, b) = (id_at(a, index)?, id_at(b, index)?);
                if a != b {
                    left = a;
                    right = b;
                    diverged = true;
                    break;
                }
            }
            if !diverged {
                return Ok(alen.cmp(&blen));
            }
        }
        work.charge(4096, 32)?;
        let (mut a, mut b) = ([0; 2048], [0; 2048]);
        let (Row::Mailbox(a), _) = self
            .view
            .get(Key::Mailbox(left), &mut a)?
            .ok_or(ports::Error::Corrupt)?
        else {
            return Err(ports::Error::Corrupt);
        };
        let (Row::Mailbox(b), _) = self
            .view
            .get(Key::Mailbox(right), &mut b)?
            .ok_or(ports::Error::Corrupt)?
        else {
            return Err(ports::Error::Corrupt);
        };
        mailbox_order::Plan::new(&self.arguments.sort)
            .map_err(|_| ports::Error::Corrupt)?
            .compare(
                mailbox_order::Key::new(left, a.name, a.sort_order)
                    .map_err(|_| ports::Error::Corrupt)?,
                mailbox_order::Key::new(right, b.name, b.sort_order)
                    .map_err(|_| ports::Error::Corrupt)?,
            )
            .map_err(|_| ports::Error::Corrupt)
    }
}
impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    pub(super) fn mailbox_query(
        &mut self,
        range: Range<u64>,
        page: usize,
        depth: usize,
        nodes: usize,
        mode: Mode,
    ) -> Result<Range<u64>, Error> {
        let arguments = self.mailbox_query_arguments(range, page, mode)?;
        let execution = self.execution;
        let (coordinator, spool, cap) = self
            .retention
            .method_sort_context(execution)
            .map_err(storage)?;
        let view = self
            .retention
            .method_view(self.account, execution)
            .map_err(storage)?;
        let state = QueryState::current(view.identity(), QueryType::Mailbox);
        let source = Source {
            worker: self,
            view,
            arguments,
            depth,
            nodes,
            stage: Stage::Validate,
            filter: None,
            path: None,
            record: [0; RECORD],
            ancestor: 0,
            row: OwnedRow::new(),
            after: None,
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
        if job.source_mut().arguments.mode != Mode::Query {
            return Err(Error::State);
        }
        let mut record = [0; RECORD];
        let mut position = {
            let args = &job.source_mut().arguments;
            if args.position < 0 {
                i128::from(output.records) + i128::from(args.position)
            } else {
                i128::from(args.position)
            }
        }
        .max(0);
        if let Some(anchor) = job.source_mut().arguments.anchor {
            let anchor = anchor.ok_or(Error::AnchorNotFound)?;
            let mut found = None;
            for index in 0..output.records {
                job.read_output(output, index, &mut record)
                    .map_err(storage)?;
                if record_id(&record).map_err(storage)? == anchor {
                    found = Some(index);
                    break;
                }
            }
            position = (i128::from(found.ok_or(Error::AnchorNotFound)?)
                + i128::from(job.source_mut().arguments.offset))
            .max(0);
        }
        if position > ((1u64 << 53) - 1) as i128 {
            // No first result exists beyond the end; keep the empty page's
            // position representable when anchor addition exceeds JMAP Int.
            position = i128::from(output.records);
        }
        let position = position as u64;
        let (limit, total, include_limit) = {
            let args = &job.source_mut().arguments;
            (args.limit, args.total, args.include_limit)
        };
        let start = job.source_mut().worker.retention.len();
        {
            let source = job.source_mut();
            source.worker.append(br#"{"accountId":"#)?;
            source.worker.push_id(&source.arguments.account)?;
            source.worker.append(br#","queryState":""#)?;
            source.worker.append(
                state
                    .encode(&mut [0; QueryState::WIRE_BYTES])
                    .map_err(|_| Error::State)?
                    .as_bytes(),
            )?;
            source
                .worker
                .append(br#"","canCalculateChanges":false,"position":"#)?;
            source.worker.append(position.to_string().as_bytes())?;
            if include_limit {
                source.worker.append(br#","limit":"#)?;
                source.worker.append(limit.to_string().as_bytes())?;
            }
            if total {
                source.worker.append(br#","total":"#)?;
                source
                    .worker
                    .append(output.records.to_string().as_bytes())?;
            }
            source.worker.append(br#","ids":["#)?;
        }
        let end = output.records.min(position.saturating_add(limit as u64));
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
#[path = "mailbox_query_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mailbox_query_changes_tests.rs"]
mod changes_tests;
