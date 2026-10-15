//! Bounded identity/time query used by td-mail's lost-send reconciliation.
use super::{push::Children, Error, Worker};
use crate::{
    postgres_read::SubmissionQuery,
    sync::{QueryState, QueryType},
    wire,
};
use std::ops::Range;
use td_json::scan;

impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    pub(super) fn submission_query(
        &mut self,
        range: Range<u64>,
        page: usize,
    ) -> Result<Range<u64>, Error> {
        let mut fields = Children::new(range, scan::Kind::Object)?;
        let (mut account, mut identity, mut after, mut before) = (None, None, None, None);
        let (mut ascending, mut total) = (false, false);
        let (mut position, mut limit, mut include_limit) =
            (0, page.min(crate::postgres_read::MAX_SUBMISSION_PAGE), true);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(self.push_string(&item)?),
                b"filter" => {
                    let mut filter = Children::new(item.range, scan::Kind::Object)?;
                    while let Some(item) = filter.next(self)? {
                        match item.key.bytes() {
                            b"identityIds" => {
                                let mut ids = Children::new(item.range, scan::Kind::Array)?;
                                let item = ids.next(self)?.ok_or(Error::UnsupportedFilter)?;
                                identity = Some(self.push_string(&item)?.wire_id()?.local());
                                if ids.next(self)?.is_some() {
                                    return Err(Error::UnsupportedFilter);
                                }
                            }
                            b"after" => {
                                after = Some(super::query_date::parse(
                                    self.push_string(&item)?.bytes(),
                                )?)
                            }
                            b"before" => {
                                before = Some(super::query_date::parse(
                                    self.push_string(&item)?.bytes(),
                                )?)
                            }
                            _ => return Err(Error::UnsupportedFilter),
                        }
                    }
                }
                b"sort" => {
                    let mut sort = Children::new(item.range, scan::Kind::Array)?;
                    let item = sort.next(self)?.ok_or(Error::UnsupportedSort)?;
                    let mut comparator = Children::new(item.range, scan::Kind::Object)?;
                    let mut property = false;
                    ascending = true;
                    while let Some(item) = comparator.next(self)? {
                        match item.key.bytes() {
                            b"property" => {
                                if self.push_string(&item)?.bytes() != b"sendAt" {
                                    return Err(Error::UnsupportedSort);
                                }
                                property = true;
                            }
                            b"isAscending" => ascending = self.query_boolean(&item)?,
                            _ => return Err(Error::UnsupportedSort),
                        }
                    }
                    if !property {
                        return Err(Error::InvalidArguments);
                    }
                    if sort.next(self)?.is_some() {
                        return Err(Error::UnsupportedSort);
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
                    include_limit = requested > limit as u64;
                    limit = limit.min(usize::try_from(requested).unwrap_or(usize::MAX));
                }
                _ => return Err(Error::InvalidArguments),
            }
        }
        let account = account.ok_or(Error::InvalidArguments)?;
        let identity = identity.ok_or(Error::UnsupportedFilter)?;
        let after = after.ok_or(Error::UnsupportedFilter)?;
        let before = before.ok_or(Error::UnsupportedFilter)?;
        if account.wire_id()?.local() != Some(self.account) {
            return Err(Error::AccountNotFound);
        }
        self.check()?;
        let view = self.read.as_mut().ok_or(Error::State)?;
        let state = QueryState::current(view.identity(), QueryType::Submission);
        // A well-formed foreign identity matches nothing; it never broadens
        // the query to every identity or supplies a synthetic local ID.
        let result = if let Some(identity) = identity {
            Some(view.query_submissions(SubmissionQuery {
                identity,
                after,
                before,
                ascending,
                position,
                limit,
                calculate_total: total,
            })?)
        } else {
            None
        };
        if let Some(result) = &result {
            if result.state != state
                || result.position != position
                || result.ids.len() > limit
                || result.total.is_some() != total
            {
                return Err(Error::State);
            }
        }
        let start = self.retention.len();
        self.append(br#"{"accountId":"#)?;
        self.push_id(&account)?;
        self.append(br#", "queryState":""#)?;
        self.append(
            state
                .encode(&mut [0; QueryState::WIRE_BYTES])
                .map_err(|_| Error::State)?
                .as_bytes(),
        )?;
        self.append(br#"","canCalculateChanges":false,"position":"#)?;
        self.append(position.to_string().as_bytes())?;
        if include_limit {
            self.append(br#", "limit":"#)?;
            self.append(limit.to_string().as_bytes())?;
        }
        if total {
            self.append(br#", "total":"#)?;
            self.append(
                result
                    .as_ref()
                    .and_then(|page| page.total)
                    .unwrap_or(0)
                    .to_string()
                    .as_bytes(),
            )?;
        }
        self.append(br#", "ids":["#)?;
        if let Some(result) = result {
            for (index, id) in result.ids.into_iter().enumerate() {
                self.check()?;
                if index != 0 {
                    self.append(b",")?;
                }
                self.append(b"\"")?;
                self.append(
                    wire::encode_id(id, &mut [0; wire::IdKind::WIRE_BYTES])
                        .map_err(|_| Error::State)?
                        .as_bytes(),
                )?;
                self.append(b"\"")?;
            }
        }
        self.append(b"]}")?;
        self.read.as_mut().ok_or(Error::State)?.check()?;
        self.check()?;
        let end = self.retention.publish().map_err(Error::Storage)?;
        Ok(start..end)
    }
}
