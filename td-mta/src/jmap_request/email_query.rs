//! td-mail inbox paging over one retained snapshot and bounded external sort.
use super::{
    push::{Children, Name},
    Error, Worker,
};
use crate::{
    ids::MailboxId,
    sync::{QueryState, QueryType},
    wire,
};
use std::ops::Range;
use td_json::scan;
#[derive(Default)]
struct Filter {
    mailbox: Option<Option<MailboxId>>,
    after: Option<i64>,
    before: Option<i64>,
    search: Option<super::email_search::Search>,
}
impl Worker<'_, '_, '_, '_, '_> {
    fn email_filter(&mut self, range: Range<u64>, result: &mut Filter) -> Result<(), Error> {
        let mut fields = Children::new(range.clone(), scan::Kind::Object)?;
        let (mut operator, mut conditions, mut scalar) = (false, None, false);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"operator" => {
                    if self.read.is_none() || self.push_string(&item)?.bytes() != b"AND" {
                        return Err(Error::UnsupportedFilter);
                    }
                    operator = true;
                }
                b"conditions" if self.read.is_none() => return Err(Error::UnsupportedFilter),
                b"conditions" => conditions = Some(item),
                _ => scalar = true,
            }
        }
        match (operator, conditions) {
            (false, None) => self.email_condition(range, result),
            (true, Some(conditions)) if !scalar => {
                let mut conditions = Children::new(conditions.range, scan::Kind::Array)?;
                while let Some(item) = conditions.next(self)? {
                    self.email_condition(item.range, result)?;
                }
                Ok(())
            }
            _ => Err(Error::InvalidArguments),
        }
    }
    fn email_condition(&mut self, range: Range<u64>, result: &mut Filter) -> Result<(), Error> {
        let mut fields = Children::new(range, scan::Kind::Object)?;
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"text" if result.search.is_none() => {
                    result.search = Some(self.search_operand(&item)?)
                }
                b"minSize" => {
                    let size = self.query_integer(&item)?;
                    if size < 0 {
                        return Err(Error::InvalidArguments);
                    }
                    if size != 0 {
                        return Err(Error::UnsupportedFilter);
                    }
                }
                b"inMailbox" => {
                    let mailbox = self.push_string(&item)?.wire_id()?.local();
                    result.mailbox = Some(match result.mailbox {
                        None => mailbox,
                        Some(previous) if previous == mailbox => mailbox,
                        Some(None) => None,
                        Some(Some(_)) if mailbox.is_none() => None,
                        Some(Some(_)) => return Err(Error::UnsupportedFilter),
                    });
                }
                b"after" | b"before" if self.read.is_some() => {
                    let date = super::query_date::parse(self.push_string(&item)?.bytes())?;
                    if item.key.bytes() == b"after" {
                        result.after = Some(result.after.map_or(date, |old| old.max(date)));
                    } else {
                        result.before = Some(result.before.map_or(date, |old| old.min(date)));
                    }
                }
                _ => return Err(Error::UnsupportedFilter),
            }
        }
        Ok(())
    }
}
struct Arguments {
    account: Name,
    mailbox: Option<Option<MailboxId>>,
    after: Option<i64>,
    before: Option<i64>,
    ascending: bool,
    position: u64,
    limit: usize,
    include_limit: bool,
    total: bool,
    search: Option<super::email_search::Search>,
}
impl Worker<'_, '_, '_, '_, '_> {
    fn email_query_arguments(
        &mut self,
        range: Range<u64>,
        page: usize,
    ) -> Result<Arguments, Error> {
        let mut fields = Children::new(range, scan::Kind::Object)?;
        let mut account = None;
        let mut filter = Filter::default();
        let (mut ascending, mut total) = (false, false);
        let (mut position, mut limit, mut include_limit) = (0, page, true);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(self.push_string(&item)?),
                b"filter" if item.kind == scan::Kind::Null => (),
                b"filter" => self.email_filter(item.range, &mut filter)?,
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
            mailbox: filter.mailbox,
            after: filter.after,
            before: filter.before,
            ascending,
            position,
            limit,
            include_limit,
            total,
            search: filter.search,
        })
    }
}
impl Worker<'_, '_, '_, '_, '_> {
    fn search_page(
        &mut self,
        view: &mut dyn super::MailRead,
        query: crate::postgres_read::EmailQuery,
        search: &mut super::email_search::Search,
        limits: &crate::limits::Limits,
    ) -> Result<crate::postgres_read::EmailPage, Error> {
        use crate::postgres_read::EmailPage;
        let mut timer = crate::performance::Timer::start(crate::performance::Phase::JmapSearch);
        if search.matched() {
            let result = view.query(query);
            if result.is_ok() {
                timer.succeeded();
            }
            return result;
        }
        let (clock, mut work) = self
            .retention
            .method_body_context(self.execution)
            .map_err(Error::Storage)?;
        let mut result = EmailPage {
            state: QueryState::current(view.identity(), QueryType::Email),
            position: query.position,
            ids: Vec::new(),
            total: None,
            has_more: false,
        };
        result
            .ids
            .try_reserve_exact(query.limit)
            .map_err(|_| Error::StorageCapacity)?;
        let pattern = if crate::postgres_search::compatible(limits) {
            search.candidate_pattern()?
        } else {
            None
        };
        let batch = if pattern.is_some() { 256 } else { 32 };
        let (mut offset, mut matches) = (0u64, 0u64);
        while query.calculate_total || result.ids.len() < query.limit {
            self.check()?;
            super::email_search::charge(&mut work, clock.as_ref(), batch as u64)?;
            let page = view.search_candidates(
                crate::postgres_read::EmailQuery {
                    position: offset,
                    limit: batch,
                    calculate_total: false,
                    ..query
                },
                pattern.as_deref(),
            )?;
            if page.state != result.state || page.position != offset || page.ids.len() > batch {
                return Err(Error::State);
            }
            offset = offset
                .checked_add(page.ids.len() as u64)
                .ok_or(Error::StorageCapacity)?;
            for id in page.ids {
                self.check()?;
                if view.search_email(id, limits, search, clock.as_ref(), &mut work)? {
                    if matches >= query.position && result.ids.len() < query.limit {
                        result.ids.push(id);
                    }
                    matches = matches.checked_add(1).ok_or(Error::StorageCapacity)?;
                    if !query.calculate_total && result.ids.len() == query.limit {
                        break;
                    }
                }
            }
            if !page.has_more {
                break;
            }
        }
        if query.calculate_total {
            result.total = Some(matches);
        }
        view.check()?;
        super::email_search::charge(&mut work, clock.as_ref(), 0)?;
        timer.succeeded();
        Ok(result)
    }

    pub(super) fn email_query(
        &mut self,
        range: Range<u64>,
        page: usize,
        limits: &crate::limits::Limits,
    ) -> Result<Range<u64>, Error> {
        let arguments = self.email_query_arguments(range, page)?;
        if let Some(view) = self.read.take() {
            let result = self.postgres_email_query(view, arguments, limits);
            self.read = Some(view);
            return result;
        }
        Err(Error::Storage(crate::ports::Error::Invalid))
    }

    fn postgres_email_query(
        &mut self,
        view: &mut dyn super::MailRead,
        arguments: Arguments,
        limits: &crate::limits::Limits,
    ) -> Result<Range<u64>, Error> {
        use crate::postgres_read::{EmailQuery, EmailScope};
        self.check()?;
        let query = EmailQuery {
            scope: match arguments.mailbox {
                None => EmailScope::All,
                Some(Some(mailbox)) => EmailScope::Mailbox(mailbox),
                Some(None) => EmailScope::Empty,
            },
            after: arguments.after,
            before: arguments.before,
            ascending: arguments.ascending,
            position: arguments.position,
            limit: arguments.limit,
            calculate_total: arguments.total,
        };
        let result = if let Some(mut search) = arguments.search {
            self.search_page(view, query, &mut search, limits)?
        } else {
            view.query(query)?
        };
        let state = QueryState::current(view.identity(), QueryType::Email);
        if result.state != state
            || result.position != arguments.position
            || result.ids.len() > arguments.limit
            || result.total.is_some() != arguments.total
        {
            return Err(Error::State);
        }
        let start = self.retention.len();
        self.append(br#"{"accountId":"#)?;
        self.push_id(&arguments.account)?;
        self.append(br#","queryState":""#)?;
        self.append(
            state
                .encode(&mut [0; QueryState::WIRE_BYTES])
                .map_err(|_| Error::State)?
                .as_bytes(),
        )?;
        self.append(br#"","canCalculateChanges":false,"position":"#)?;
        self.append(result.position.to_string().as_bytes())?;
        if arguments.include_limit {
            self.append(br#","limit":"#)?;
            self.append(arguments.limit.to_string().as_bytes())?;
        }
        if let Some(total) = result.total {
            self.append(br#","total":"#)?;
            self.append(total.to_string().as_bytes())?;
        }
        self.append(br#","ids":["#)?;
        let mut first = true;
        for id in result.ids {
            self.check()?;
            if !first {
                self.append(b",")?;
            }
            first = false;
            self.append(b"\"")?;
            self.append(
                wire::encode_id(id, &mut [0; wire::IdKind::WIRE_BYTES])
                    .map_err(|_| Error::State)?
                    .as_bytes(),
            )?;
            self.append(b"\"")?;
        }
        self.append(b"]}")?;
        view.check()?;
        self.check()?;
        let end = self.retention.publish().map_err(Error::Storage)?;
        Ok(start..end)
    }
}
