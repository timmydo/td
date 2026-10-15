//! Historical submission identity and bounded reconciliation windows.
use super::{check_changed, Error, Snapshot};
use crate::{
    ids::{EmailId, IdentityId, SubmissionId, ThreadId},
    ports::ViewIdentity,
    sync::{QueryState, QueryType},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubmissionMetadata {
    pub view: ViewIdentity,
    pub submission: SubmissionId,
    pub email: EmailId,
    pub thread: ThreadId,
    pub identity: IdentityId,
    pub send_at: i64,
}
#[derive(Clone, Copy, Debug)]
pub struct SubmissionQuery {
    pub identity: IdentityId,
    pub after: i64,
    pub before: i64,
    pub ascending: bool,
    pub position: u64,
    pub limit: usize,
    pub calculate_total: bool,
}
#[derive(Debug)]
pub struct SubmissionPage {
    pub state: QueryState,
    pub position: u64,
    pub ids: Vec<SubmissionId>,
    pub total: Option<u64>,
    pub has_more: bool,
}
pub const MAX_SUBMISSION_PAGE: usize = 1024;

impl Snapshot<'_> {
    /// Historical Email/Thread IDs survive user deletion and never authorize
    /// reading a current message or a transmitted body.
    pub fn submission(
        &mut self,
        submission: SubmissionId,
    ) -> Result<Option<SubmissionMetadata>, Error> {
        self.check()?;
        let view = self.identity;
        let result = self.worker.run(|client| Box::pin(async move {
            let row = client.query_opt(
                "SELECT email_id,thread_id,identity_id,send_at,changed FROM td_mta.submissions WHERE account=$1 AND id=$2",
                &[&view.account.as_bytes().as_slice(), &submission.as_bytes().as_slice()],
            ).await.map_err(|_| Error::Database)?;
            let Some(row) = row else { return Ok(None); };
            check_changed(row.try_get(4).map_err(|_| Error::Integrity)?, view)?;
            let id = |column| -> Result<[u8;16], Error> {
                let bytes: &[u8] = row.try_get(column).map_err(|_| Error::Integrity)?;
                bytes.try_into().map_err(|_| Error::Integrity)
            };
            Ok(Some(SubmissionMetadata {
                view, submission,
                email: EmailId::from_bytes(id(0)?),
                thread: ThreadId::from_bytes(id(1)?),
                identity: IdentityId::from_bytes(id(2)?),
                send_at: row.try_get(3).map_err(|_| Error::Integrity)?,
            }))
        })).map_err(Error::Connection).and_then(|result| result);
        self.retain(result)
    }

    /// td-mail's identity/time reconciliation filter: after is inclusive,
    /// before exclusive, with raw SubmissionId ascending to break date ties.
    pub fn query_submissions(&mut self, query: SubmissionQuery) -> Result<SubmissionPage, Error> {
        self.check()?;
        if query.limit > MAX_SUBMISSION_PAGE {
            return self.retain(Err(Error::Capacity));
        }
        let view = self.identity;
        let result = self.worker.run(|client| Box::pin(async move {
            let mut page = SubmissionPage {
                state: QueryState::current(view, QueryType::Submission),
                position: query.position,
                ids: Vec::new(),
                total: query.calculate_total.then_some(0),
                has_more: false,
            };
            if query.after >= query.before { return Ok(page); }
            let account = view.account.as_bytes().as_slice();
            let identity = query.identity.as_bytes().as_slice();
            if query.calculate_total {
                let row = client.query_one(
                    "SELECT count(*) FROM td_mta.submissions WHERE account=$1 AND identity_id=$2 AND send_at >= $3 AND send_at < $4",
                    &[&account, &identity, &query.after, &query.before],
                ).await.map_err(|_| Error::Database)?;
                let total: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
                page.total = Some(u64::try_from(total).map_err(|_| Error::Integrity)?);
            }
            let Ok(position) = i64::try_from(query.position) else { return Ok(page); };
            let limit = i64::try_from(query.limit + 1).map_err(|_| Error::Capacity)?;
            let sql = if query.ascending {
                "SELECT id,send_at,changed FROM td_mta.submissions WHERE account=$1 AND identity_id=$2 AND send_at >= $3 AND send_at < $4 ORDER BY send_at ASC,id ASC LIMIT $5 OFFSET $6"
            } else {
                "SELECT id,send_at,changed FROM td_mta.submissions WHERE account=$1 AND identity_id=$2 AND send_at >= $3 AND send_at < $4 ORDER BY send_at DESC,id ASC LIMIT $5 OFFSET $6"
            };
            let rows = client.query(sql, &[&account, &identity, &query.after, &query.before, &limit, &position]).await.map_err(|_| Error::Database)?;
            page.ids.try_reserve_exact(query.limit).map_err(|_| Error::Capacity)?;
            page.has_more = rows.len() > query.limit;
            let mut previous: Option<(i64, [u8;16])> = None;
            for row in rows {
                let id: &[u8] = row.try_get(0).map_err(|_| Error::Integrity)?;
                let id: [u8;16] = id.try_into().map_err(|_| Error::Integrity)?;
                let date: i64 = row.try_get(1).map_err(|_| Error::Integrity)?;
                check_changed(row.try_get(2).map_err(|_| Error::Integrity)?, view)?;
                if let Some((old_date, old_id)) = previous {
                    let order = old_date.cmp(&date);
                    let order = if query.ascending { order } else { order.reverse() };
                    if !order.then_with(|| old_id.cmp(&id)).is_lt() { return Err(Error::Integrity); }
                }
                previous = Some((date, id));
                if page.ids.len() < query.limit { page.ids.push(SubmissionId::from_bytes(id)); }
            }
            Ok(page)
        })).map_err(Error::Connection).and_then(|result| result);
        self.retain(result)
    }
}
