//! Bounded due-work hints for a current route owner; never SMTP authority.
use crate::{
    ids::{RouteClaimId, SubmissionId},
    postgres_probe::{self, Request},
    postgres_route_health,
    postgres_route_owners::{self as owners, Claim},
};
use std::time::Instant;
use td_crypto::TrustStore;

pub const PAGE_SIZE: usize = 64;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Fresh,
    Retry,
    Expiry,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Owner(owners::Error),
    RouteHealth(postgres_route_health::Error),
    Policy,
    Cursor,
    Busy,
    Capacity,
    Integrity,
    Database,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub submission: SubmissionId,
    pub due_at: i64,
}
/// A pagination hint bound to the same owner generation and candidate kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cursor {
    operation: RouteClaimId,
    generation: u64,
    kind: Kind,
    last: Candidate,
}
#[derive(Debug)]
pub struct Page {
    pub observed_at: i64,
    pub candidates: Vec<Candidate>,
    pub next: Option<Cursor>,
}
/// Fresh absence hint for one scheduling turn, never delivery authority.
/// Notices are account-wide, matching their publisher; unfinished work and
/// recovery belong to the selected route. A positive hint proves no eligibility.
pub fn idle_observation(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    claim: &Claim,
    generation: u64,
) -> Result<Option<i64>, Error> {
    let claim = claim.clone();
    postgres_probe::with_scoped_mailbox_client(request, trust, deadline, move |scope| {
        Box::pin(async move {
            let (client, current_policy) = owners::begin_scoped(scope, &claim, true, false)
                .await
                .map_err(Error::Owner)?;
            if !current_policy {
                return Err(Error::Policy);
            }
            idle_query(client, &claim, generation).await
        })
    })
    .map_err(Error::Connection)?
}
async fn idle_query(
    client: &tokio_postgres::Client,
    claim: &Claim,
    generation: u64,
) -> Result<Option<i64>, Error> {
    owners::check_locked(client, claim, generation)
        .await
        .map_err(Error::Owner)?;
    let binding = claim.binding();
    let account = binding.account();
    let work: bool = client.query_typed_one(
                "SELECT EXISTS(SELECT 1 FROM td_mta.submissions s JOIN td_mta.submission_routes r ON r.account=s.account AND r.submission_id=s.id WHERE s.account=$1 AND r.route_name=$2 AND r.destination=$3 AND s.completed_at IS NULL) OR EXISTS(SELECT 1 FROM td_mta.submissions WHERE account=$1 AND completed_at IS NOT NULL AND notification=1) OR EXISTS(SELECT 1 FROM td_mta.submission_attempts WHERE account=$1 AND route_name=$2 AND destination=$3 AND active)",
                &[
                    (&account.as_bytes().as_slice(), tokio_postgres::types::Type::BYTEA),
                    (&binding.route().name(), tokio_postgres::types::Type::TEXT),
                    (&binding.route().destination().as_slice(), tokio_postgres::types::Type::BYTEA),
                ],
            ).await.map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Integrity)?;
    let observed_at = owners::check_locked(client, claim, generation)
        .await
        .map_err(Error::Owner)?;
    Ok((!work).then_some(observed_at))
}

/// One foreground worker's finite idle connection, shared across account turns.
/// Its slot remains reserved until close; no observation authorizes delivery.
pub struct IdleReader<'a, 'r> {
    slots: &'a crate::postgres_read::ReadSlots,
    request: &'a Request<'r>,
    trust: &'a TrustStore,
    session: Option<(
        postgres_probe::ReadSession,
        crate::postgres_read::ReadPermit<'a>,
    )>,
}
impl<'a, 'r> IdleReader<'a, 'r> {
    pub fn new(
        slots: &'a crate::postgres_read::ReadSlots,
        request: &'a Request<'r>,
        trust: &'a TrustStore,
    ) -> Self {
        Self {
            slots,
            request,
            trust,
            session: None,
        }
    }
    pub(crate) fn maintain(&mut self) {
        if self
            .session
            .as_ref()
            .is_some_and(|(session, _)| session.needs_retirement(Instant::now()))
        {
            self.close();
        }
    }
    pub fn close(&mut self) {
        if let Some((session, permit)) = self.session.take() {
            let now = Instant::now();
            session.retire(
                now.checked_add(std::time::Duration::from_millis(100))
                    .unwrap_or(now),
            );
            drop(permit);
        }
    }
    pub fn observe(
        &mut self,
        deadline: Instant,
        claim: &Claim,
        generation: u64,
    ) -> Result<Option<i64>, Error> {
        if self
            .session
            .as_ref()
            .is_some_and(|(session, _)| !session.reusable(deadline))
        {
            self.close();
        }
        if self
            .session
            .as_mut()
            .is_some_and(|(session, _)| session.check_idle(deadline).is_err())
        {
            self.close();
        }
        let reused = self.session.is_some();
        if self.session.is_none() {
            let permit = self.slots.acquire().map_err(|_| Error::Busy)?;
            let session = postgres_probe::ReadSession::open(self.request, self.trust, deadline)
                .map_err(Error::Connection)?;
            self.session = Some((session, permit));
        }
        let claim = claim.clone();
        let result = self
            .session
            .as_mut()
            .ok_or(Error::Integrity)?
            .0
            .read(
                claim.binding().epoch(),
                postgres_probe::ReadIsolation::ReadCommitted,
                deadline,
                move |client| {
                    Box::pin(async move {
                        if !owners::lock_account_policy(client, &claim, true, false)
                            .await
                            .map_err(Error::Owner)?
                        {
                            return Err(Error::Policy);
                        }
                        idle_query(client, &claim, generation).await
                    })
                },
            )
            .map_err(Error::Connection)
            .and_then(|result| result);
        if reused {
            crate::performance::record(
                crate::performance::Phase::DbReuse,
                std::time::Duration::ZERO,
                if result.is_ok() {
                    crate::performance::Outcome::Success
                } else {
                    crate::performance::Outcome::Failed
                },
            );
        }
        if result.is_err() {
            self.close();
        }
        result
    }
}
impl Drop for IdleReader<'_, '_> {
    fn drop(&mut self) {
        self.close();
    }
}

/// Holds policy/account/route locks for this bounded read only. The returned
/// page is not a snapshot continuation or a reservation; restart at None on
/// the next scheduling pass. Recheck queue-clock guards and every publication
/// fence before preparing work, and never infer permission to send from a hint.
/// Expiry hints bypass delivery pauses and active attempts; the expiry publisher
/// still requires confirmed clock admission and a closed/fenced transport.
pub fn scan(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    claim: &Claim,
    generation: u64,
    kind: Kind,
    after: Option<Cursor>,
) -> Result<Page, Error> {
    if after.is_some_and(|cursor| {
        cursor.operation != claim.operation()
            || cursor.generation != generation
            || cursor.kind != kind
    }) {
        return Err(Error::Cursor);
    }
    let claim = claim.clone();
    postgres_probe::with_mailbox_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            if !owners::begin(client, &claim, true, false).await.map_err(Error::Owner)? {
                return Err(Error::Policy);
            }
            let observed_at = owners::check_locked(client, &claim, generation).await.map_err(Error::Owner)?;
            if kind != Kind::Expiry {
                postgres_route_health::admit_locked(client, &claim, observed_at).await.map_err(Error::RouteHealth)?;
            }
            let binding = claim.binding();
            let account = binding.account();
            let account = account.as_bytes().as_slice();
            if kind != Kind::Expiry {
                let busy:bool = client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.submission_attempts WHERE account=$1 AND route_name=$2 AND active)", &[&account,&binding.route().name()]).await.map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
                if busy { return Err(Error::Busy); }
            }
            let states: &[i64] = match kind { Kind::Fresh => &[1], Kind::Retry => &[3,7], Kind::Expiry => &[] };
            let after_id = after.as_ref().map(|cursor| cursor.last.submission.as_bytes().as_slice());
            let after_due = after.map_or(0, |cursor| cursor.last.due_at);
            let rows = if kind == Kind::Expiry {
                client.query("SELECT s.id,s.expires_at FROM td_mta.submissions s JOIN td_mta.submission_routes r ON r.account=s.account AND r.submission_id=s.id WHERE s.account=$1 AND r.route_name=$2 AND r.destination=$3 AND s.completed_at IS NULL AND s.expires_at<=$4 AND EXISTS(SELECT 1 FROM td_mta.recipients p WHERE p.account=s.account AND p.submission_id=s.id AND (p.state IN (1,2,3) OR (p.state=7 AND p.next_attempt_at IS NOT NULL))) AND ($5::bytea IS NULL OR (s.expires_at,s.id)>($6,$5)) ORDER BY s.expires_at,s.id LIMIT 65", &[&account,&binding.route().name(),&binding.route().destination().as_slice(),&observed_at,&after_id,&after_due]).await.map_err(|_|Error::Database)?
            } else {
                client.query("SELECT id,due FROM (SELECT s.id,min(p.next_attempt_at) AS due FROM td_mta.submissions s JOIN td_mta.submission_routes r ON r.account=s.account AND r.submission_id=s.id JOIN td_mta.recipients p ON p.account=s.account AND p.submission_id=s.id WHERE s.account=$1 AND r.route_name=$2 AND r.destination=$3 AND s.completed_at IS NULL AND s.send_at<=$4 AND s.expires_at>$4 AND p.state=ANY($7) AND p.next_attempt_at<=$4 GROUP BY s.id) candidates WHERE ($5::bytea IS NULL OR (due,id)>($6,$5)) ORDER BY due,id LIMIT 65", &[&account,&binding.route().name(),&binding.route().destination().as_slice(),&observed_at,&after_id,&after_due,&states]).await.map_err(|_|Error::Database)?
            };
            if rows.len() > PAGE_SIZE+1 { return Err(Error::Integrity); }
            let mut candidates = Vec::new();
            candidates.try_reserve_exact(rows.len().min(PAGE_SIZE)).map_err(|_|Error::Capacity)?;
            let mut previous = after.map(|cursor| cursor.last);
            for (index, row) in rows.iter().enumerate() {
                let bytes: &[u8] = row.try_get(0).map_err(|_|Error::Integrity)?;
                let submission = SubmissionId::from_bytes(bytes.try_into().map_err(|_|Error::Integrity)?);
                let due_at:i64 = row.try_get(1).map_err(|_|Error::Integrity)?;
                if due_at > observed_at || previous.is_some_and(|last| (due_at,submission)<=(last.due_at,last.submission)) {
                    return Err(Error::Integrity);
                }
                let candidate = Candidate {submission,due_at};
                previous = Some(candidate);
                if index < PAGE_SIZE { candidates.push(candidate); }
            }
            let next = if rows.len()>PAGE_SIZE {
                Some(Cursor { operation:claim.operation(), generation, kind,
                    last:*candidates.last().ok_or(Error::Integrity)? })
            } else { None };
            owners::check_locked(client, &claim, generation).await.map_err(Error::Owner)?;
            Ok(Page {observed_at,candidates,next})
        })
    }).map_err(Error::Connection)?
}
