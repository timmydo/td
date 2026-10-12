//! Incremental filter validation and evaluation over retained argument ranges.
use super::{
    push::{Children, Item},
    Error, Worker,
};
use crate::{
    filter_logic::{Evaluation, Operator},
    format::row::MailboxRow,
    jmap_request::children::Progress,
    mailbox_filter::{Condition, NamePattern, Parent, Role, MAX_TEXT_BYTES},
    store_fs::SortWork,
};
use std::ops::Range;
use td_json::{arena, scan};

struct Fields {
    children: Children,
    operator: Option<Operator>,
    conditions: Option<Range<u64>>,
    has_condition: bool,
    value: bool,
}
enum Frame {
    Fields(Fields),
    Children(Children),
}
const MAX_FRAMES: usize = crate::filter_logic::MAX_DEPTH;
pub(super) const MAX_RESIDENT_BYTES: usize = MAX_FRAMES * std::mem::size_of::<Frame>()
    + 64 * (MAX_TEXT_BYTES + std::mem::size_of::<String>())
    + 96 * 1024;
pub(super) struct Filter {
    frames: Vec<Frame>,
    evaluation: Option<Evaluation>,
    maximum_depth: usize,
    distinct: Option<Vec<String>>,
    pending_text: Option<(String, usize)>,
    complete: Option<bool>,
}
fn logic(error: crate::filter_logic::Error) -> Error {
    match error {
        crate::filter_logic::Error::DepthLimit | crate::filter_logic::Error::NodeLimit => {
            Error::UnsupportedFilter
        }
        _ => Error::State,
    }
}
impl Fields {
    fn new(range: Range<u64>) -> Result<Self, Error> {
        Ok(Self {
            children: Children::new(range, scan::Kind::Object)?,
            operator: None,
            conditions: None,
            has_condition: false,
            value: true,
        })
    }
}
impl Filter {
    pub(super) fn new(
        range: Option<Range<u64>>,
        depth: usize,
        nodes: usize,
        validate: bool,
    ) -> Result<Self, Error> {
        if depth == 0 || depth > MAX_FRAMES {
            return Err(Error::UnsupportedFilter);
        }
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(depth)
            .map_err(|_| Error::ResponseCapacity)?;
        let complete = if let Some(range) = range {
            frames.push(Frame::Fields(Fields::new(range)?));
            None
        } else {
            Some(true)
        };
        let distinct = if validate {
            let mut names = Vec::new();
            names
                .try_reserve_exact(64)
                .map_err(|_| Error::ResponseCapacity)?;
            Some(names)
        } else {
            None
        };
        Ok(Self {
            frames,
            evaluation: Some(Evaluation::new(depth, nodes).map_err(logic)?),
            maximum_depth: depth,
            distinct,
            pending_text: None,
            complete,
        })
    }
    pub(super) fn reset(&mut self, range: Option<Range<u64>>, nodes: usize) -> Result<(), Error> {
        self.frames.clear();
        self.distinct = None;
        self.pending_text = None;
        self.evaluation = Some(Evaluation::new(self.maximum_depth, nodes).map_err(logic)?);
        self.complete = if let Some(range) = range {
            self.frames.push(Frame::Fields(Fields::new(range)?));
            None
        } else {
            Some(true)
        };
        Ok(())
    }
    pub(super) fn poll<P>(
        &mut self,
        worker: &mut Worker<'_, '_, '_, '_, '_, '_, P>,
        row: Option<MailboxRow<'_>>,
        work: &mut dyn SortWork,
    ) -> Result<Option<bool>, Error> {
        work.charge(4096, 1).map_err(super::mailbox_get::storage)?;
        worker.check()?;
        if let Some(value) = self.complete {
            return Ok(Some(value));
        }
        if let Some((text, index)) = self.pending_text.take() {
            work.charge(0, 16).map_err(super::mailbox_get::storage)?;
            let distinct = self.distinct.as_mut().ok_or(Error::State)?;
            if let Some(old) = distinct.get(index) {
                if old != &text {
                    self.pending_text = Some((text, index + 1));
                }
            } else {
                if distinct.len() == 64 {
                    return Err(Error::UnsupportedFilter);
                }
                distinct.push(text);
            }
            return Ok(None);
        }
        let frame = self.frames.pop().ok_or(Error::State)?;
        match frame {
            Frame::Fields(mut fields) => {
                match fields.children.poll(worker).map_err(|error| {
                    if error == Error::InvalidArguments {
                        Error::UnsupportedFilter
                    } else {
                        error
                    }
                })? {
                    Progress::Pending => self.frames.push(Frame::Fields(fields)),
                    Progress::Item(item) => {
                        match item.key.bytes() {
                            b"operator" => {
                                if item.kind != scan::Kind::String {
                                    return Err(Error::InvalidArguments);
                                }
                                let name = worker.push_string(&item).map_err(|error| {
                                    if error == Error::InvalidArguments {
                                        Error::UnsupportedFilter
                                    } else {
                                        error
                                    }
                                })?;
                                fields.operator = Some(
                                    Operator::parse(
                                        std::str::from_utf8(name.bytes())
                                            .map_err(|_| Error::State)?,
                                    )
                                    .ok_or(Error::UnsupportedFilter)?,
                                );
                            }
                            b"conditions" => {
                                if item.kind != scan::Kind::Array {
                                    return Err(Error::InvalidArguments);
                                }
                                fields.conditions = Some(item.range);
                            }
                            _ => {
                                fields.has_condition = true;
                                let (matched, text) = condition(worker, &item, row, work)?;
                                fields.value &= matched;
                                if self.distinct.is_some() {
                                    self.pending_text = text.map(|text| (text, 0));
                                }
                            }
                        }
                        self.frames.push(Frame::Fields(fields));
                    }
                    Progress::Complete => {
                        let evaluation = self.evaluation.as_mut().ok_or(Error::State)?;
                        if fields.operator.is_some() || fields.conditions.is_some() {
                            if fields.has_condition {
                                return Err(Error::InvalidArguments);
                            }
                            let operator = fields.operator.ok_or(Error::InvalidArguments)?;
                            let conditions = fields.conditions.ok_or(Error::InvalidArguments)?;
                            evaluation.begin(operator).map_err(logic)?;
                            self.frames.push(Frame::Children(Children::new(
                                conditions,
                                scan::Kind::Array,
                            )?));
                        } else {
                            evaluation.condition(fields.value).map_err(logic)?;
                        }
                    }
                }
            }
            Frame::Children(mut children) => match children.poll(worker)? {
                Progress::Pending => self.frames.push(Frame::Children(children)),
                Progress::Item(item) => {
                    if item.kind != scan::Kind::Object {
                        return Err(Error::InvalidArguments);
                    }
                    self.frames.push(Frame::Children(children));
                    if self.frames.len() >= self.maximum_depth {
                        return Err(Error::UnsupportedFilter);
                    }
                    self.frames.push(Frame::Fields(Fields::new(item.range)?));
                }
                Progress::Complete => self
                    .evaluation
                    .as_mut()
                    .ok_or(Error::State)?
                    .end()
                    .map_err(logic)?,
            },
        }
        if self.frames.is_empty() {
            let value = self
                .evaluation
                .take()
                .ok_or(Error::State)?
                .finish()
                .map_err(logic)?;
            self.complete = Some(value);
        }
        worker.check()?;
        Ok(self.complete)
    }
}
fn condition<P>(
    worker: &mut Worker<'_, '_, '_, '_, '_, '_, P>,
    item: &Item,
    row: Option<MailboxRow<'_>>,
    work: &mut dyn SortWork,
) -> Result<(bool, Option<String>), Error> {
    let mut condition = Condition::default();
    match item.key.bytes() {
        b"parentId" if item.kind == scan::Kind::Null => condition.parent = Parent::Root,
        b"parentId" => condition.parent = Parent::exact(worker.push_string(item)?.wire_id()?),
        b"hasAnyRole" => condition.has_any_role = Some(worker.query_boolean(item)?),
        b"isSubscribed" => condition.subscribed = Some(worker.query_boolean(item)?),
        b"role" if item.kind == scan::Kind::Null => condition.role = Role::Exact(None),
        b"role" => {
            if item.kind != scan::Kind::String {
                return Err(Error::InvalidArguments);
            }
            if item
                .range
                .end
                .checked_sub(item.range.start)
                .ok_or(Error::State)?
                > (crate::format::row::MAX_ROLE * 6 + 2) as u64
            {
                return Ok((false, None));
            }
            let name = match worker.push_string(item) {
                Ok(name) => name,
                Err(Error::InvalidArguments) => return Ok((false, None)),
                Err(error) => return Err(error),
            };
            let text = std::str::from_utf8(name.bytes()).map_err(|_| Error::State)?;
            return Ok((row.is_none_or(|row| row.role == Some(text)), None));
        }
        b"name" => {
            let text = worker.query_text(item, work)?;
            let mut scalars = [char::default(); MAX_TEXT_BYTES];
            let mut fallback = [0; MAX_TEXT_BYTES];
            work.charge(0, 64).map_err(super::mailbox_get::storage)?;
            let pattern = NamePattern::compile(&text, &mut scalars, &mut fallback)
                .map_err(|_| Error::State)?;
            let matched = row
                .map(|row| pattern.matches(row.name))
                .transpose()
                .map_err(|_| Error::State)?
                .unwrap_or(true);
            return Ok((matched, Some(text)));
        }
        _ => return Err(Error::UnsupportedFilter),
    }
    Ok((
        row.map(|row| condition.matches(row))
            .transpose()
            .map_err(|_| Error::State)?
            .unwrap_or(true),
        None,
    ))
}
impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    fn query_text(&mut self, item: &Item, work: &mut dyn SortWork) -> Result<String, Error> {
        if item.kind != scan::Kind::String {
            return Err(Error::InvalidArguments);
        }
        const ENCODED: usize = MAX_TEXT_BYTES * 6 + 2;
        let length = usize::try_from(
            item.range
                .end
                .checked_sub(item.range.start)
                .ok_or(Error::State)?,
        )
        .map_err(|_| Error::UnsupportedFilter)?;
        if length > ENCODED {
            return Err(Error::UnsupportedFilter);
        }
        work.charge(length as u64, 64)
            .map_err(super::mailbox_get::storage)?;
        let mut bytes = [0; ENCODED];
        let bytes = bytes.get_mut(..length).ok_or(Error::State)?;
        self.retention
            .read_at(item.range.start, bytes)
            .map_err(Error::Storage)?;
        let mut tokens = [arena::Token::EMPTY; 1];
        let document = arena::parse(
            bytes,
            &mut tokens,
            arena::ParseLimits {
                max_bytes: ENCODED,
                max_depth: 1,
                max_tokens: 1,
            },
        )
        .map_err(|_| Error::State)?;
        let text = document.root().as_str().ok_or(Error::State)?;
        if text.len() > MAX_TEXT_BYTES {
            return Err(Error::UnsupportedFilter);
        }
        let mut owned = String::new();
        owned
            .try_reserve_exact(text.len())
            .map_err(|_| Error::ResponseCapacity)?;
        owned.push_str(text);
        self.check()?;
        Ok(owned)
    }
}
