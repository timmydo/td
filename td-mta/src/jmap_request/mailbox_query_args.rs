//! Strict retained Mailbox/query arguments; whole filters are validated later.
use super::{
    push::{Children, Item, Name},
    Error, Worker,
};
use crate::{
    ids::MailboxId,
    mailbox_order::{Comparator, MAX_COMPARATORS},
};
use std::ops::Range;
use td_json::scan;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Query,
    Changes,
}

pub(super) struct Arguments {
    pub mode: Mode,
    pub account: Name,
    pub filter: Option<Range<u64>>,
    pub sort: Vec<Comparator>,
    pub sort_tree: bool,
    pub filter_tree: bool,
    pub position: i64,
    pub anchor: Option<Option<MailboxId>>,
    pub offset: i64,
    pub limit: usize,
    pub include_limit: bool,
    pub total: bool,
}
impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
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
    pub(super) fn mailbox_query_arguments(
        &mut self,
        range: Range<u64>,
        page: usize,
        mode: Mode,
    ) -> Result<Arguments, Error> {
        let mut fields = Children::new(range, scan::Kind::Object)?;
        let mut account = None;
        let mut since = false;
        let mut filter = None;
        let mut sort = Vec::new();
        let (mut sort_tree, mut filter_tree, mut total) = (false, false, false);
        let (mut position, mut offset) = (0, 0);
        let mut anchor = None;
        let (mut limit, mut include_limit) = (page, true);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(self.push_string(&item)?),
                b"filter" => match item.kind {
                    scan::Kind::Null => (),
                    scan::Kind::Object => filter = Some(item.range),
                    _ => return Err(Error::InvalidArguments),
                },
                b"sort" if item.kind == scan::Kind::Null => (),
                b"sort" => sort = self.mailbox_query_sort(item)?,
                b"sortAsTree" if mode == Mode::Query => sort_tree = self.query_boolean(&item)?,
                b"filterAsTree" if mode == Mode::Query => {
                    filter_tree = self.query_boolean(&item)?
                }
                b"calculateTotal" => total = self.query_boolean(&item)?,
                b"position" if mode == Mode::Query => position = self.query_integer(&item)?,
                b"anchorOffset" if mode == Mode::Query => offset = self.query_integer(&item)?,
                b"anchor" if mode == Mode::Query && item.kind == scan::Kind::Null => (),
                b"anchor" if mode == Mode::Query => {
                    anchor = Some(self.push_string(&item)?.wire_id()?.local())
                }
                b"limit" if mode == Mode::Query && item.kind == scan::Kind::Null => (),
                b"limit" if mode == Mode::Query => {
                    let requested = self.query_integer(&item)?;
                    if requested < 0 {
                        return Err(Error::InvalidArguments);
                    }
                    limit = page.min(usize::try_from(requested).unwrap_or(usize::MAX));
                    include_limit = requested as u64 > page as u64;
                }
                b"sinceQueryState" if mode == Mode::Changes => {
                    if item.kind != scan::Kind::String {
                        return Err(Error::InvalidArguments);
                    }
                    since = true;
                }
                b"maxChanges" if mode == Mode::Changes => {
                    if item.kind != scan::Kind::Null && self.query_integer(&item)? < 0 {
                        return Err(Error::InvalidArguments);
                    }
                }
                b"upToId" if mode == Mode::Changes => {
                    if item.kind != scan::Kind::Null {
                        self.push_string(&item)?.wire_id()?;
                    }
                }
                _ => return Err(Error::InvalidArguments),
            }
        }
        if mode == Mode::Changes && !since {
            return Err(Error::InvalidArguments);
        }
        let account = account.ok_or(Error::InvalidArguments)?;
        let local = account.wire_id()?.local();
        if local != Some(self.account) {
            return Err(Error::AccountNotFound);
        }
        Ok(Arguments {
            mode,
            account,
            filter,
            sort,
            sort_tree,
            filter_tree,
            position,
            anchor,
            offset,
            limit,
            include_limit,
            total,
        })
    }
    fn query_sort_string(&mut self, item: &Item) -> Result<Name, Error> {
        if item.kind != scan::Kind::String {
            return Err(Error::InvalidArguments);
        }
        self.push_string(item).map_err(|error| {
            if error == Error::InvalidArguments {
                Error::UnsupportedSort
            } else {
                error
            }
        })
    }
    fn mailbox_query_sort(&mut self, item: Item) -> Result<Vec<Comparator>, Error> {
        if item.kind != scan::Kind::Array {
            return Err(Error::InvalidArguments);
        }
        let mut children = Children::new(item.range, scan::Kind::Array)?;
        let mut result = Vec::new();
        result
            .try_reserve_exact(MAX_COMPARATORS)
            .map_err(|_| Error::ResponseCapacity)?;
        while let Some(item) = children.next(self)? {
            if result.len() == MAX_COMPARATORS {
                return Err(Error::UnsupportedSort);
            }
            if item.kind != scan::Kind::Object {
                return Err(Error::InvalidArguments);
            }
            let mut fields = Children::new(item.range, scan::Kind::Object)?;
            let (mut property, mut collation) = (None, None);
            let mut ascending = true;
            while let Some(item) = fields.next(self)? {
                match item.key.bytes() {
                    b"property" => property = Some(item),
                    b"isAscending" => ascending = self.query_boolean(&item)?,
                    b"collation" => collation = Some(item),
                    _ => return Err(Error::InvalidArguments),
                }
            }
            let property = self.query_sort_string(&property.ok_or(Error::InvalidArguments)?)?;
            let property = std::str::from_utf8(property.bytes()).map_err(|_| Error::State)?;
            let collation = match collation.filter(|item| item.kind != scan::Kind::Null) {
                Some(item) if item.kind != scan::Kind::String => {
                    return Err(Error::InvalidArguments)
                }
                Some(_) if property == "sortOrder" => None,
                Some(item) => Some(self.query_sort_string(&item)?),
                None => None,
            };
            let collation = collation
                .as_ref()
                .map(|name| std::str::from_utf8(name.bytes()))
                .transpose()
                .map_err(|_| Error::State)?;
            result.push(
                Comparator::new(property, ascending, collation)
                    .map_err(|_| Error::UnsupportedSort)?,
            );
        }
        Ok(result)
    }
}
