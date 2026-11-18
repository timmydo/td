//! Age-bounded HTTP write receipt ledgers inside the caller's fenced account
//! transaction (POSTGRES.md section 217).
use tokio_postgres::Client;

/// Six publication deadlines. A receipt only settles its own uncertain
/// operation, and its publisher never replays an uncertain COMMIT.
pub(crate) const RETENTION_MS: i64 = 60_000;
const _: () = assert!(
    RETENTION_MS > 0 && RETENTION_MS as u128 >= 6 * crate::postgres_probe::MAX_DURATION.as_millis()
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Capacity,
    Integrity,
    Database,
}
impl Error {
    /// The publishing family's own refusal for each outcome.
    pub(crate) fn map<E>(self, capacity: E, integrity: E, database: E) -> E {
        match self {
            Self::Capacity => capacity,
            Self::Integrity => integrity,
            Self::Database => database,
        }
    }
}

/// One family's ledger; a family sweeps only its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ledger {
    Uploads,
    Drafts,
    Folders,
    Submissions,
    Cancellations,
    Flags,
}
impl Ledger {
    fn expire(self) -> &'static str {
        match self {
            Self::Uploads => {
                "DELETE FROM td_mta.upload_receipts WHERE account=$1 AND created_at<$2"
            }
            Self::Drafts => "DELETE FROM td_mta.draft_receipts WHERE account=$1 AND created_at<$2",
            Self::Folders => {
                "DELETE FROM td_mta.mailbox_create_receipts WHERE account=$1 AND created_at<$2"
            }
            Self::Submissions => {
                "DELETE FROM td_mta.submission_receipts WHERE account=$1 AND send_at<$2"
            }
            Self::Cancellations => {
                "DELETE FROM td_mta.submission_cancel_receipts WHERE account=$1 AND created_at<$2"
            }
            Self::Flags => "DELETE FROM td_mta.flag_receipts WHERE account=$1 AND created_at<$2",
        }
    }
    fn count(self) -> &'static str {
        match self {
            Self::Uploads => "SELECT count(*) FROM (SELECT 1 FROM td_mta.upload_receipts WHERE account=$1 LIMIT $2) bounded",
            Self::Drafts => "SELECT count(*) FROM (SELECT 1 FROM td_mta.draft_receipts WHERE account=$1 LIMIT $2) bounded",
            Self::Folders => "SELECT count(*) FROM (SELECT 1 FROM td_mta.mailbox_create_receipts WHERE account=$1 LIMIT $2) bounded",
            Self::Submissions => "SELECT count(*) FROM (SELECT 1 FROM td_mta.submission_receipts WHERE account=$1 LIMIT $2) bounded",
            Self::Cancellations => "SELECT count(*) FROM (SELECT 1 FROM td_mta.submission_cancel_receipts WHERE account=$1 LIMIT $2) bounded",
            Self::Flags => "SELECT count(*) FROM (SELECT 1 FROM td_mta.flag_receipts WHERE account=$1 LIMIT $2) bounded",
        }
    }
}

/// Capacity inspection counts only a swept table's rows inside retention.
pub(crate) fn live(table: &str) -> Option<&'static str> {
    Some(match table {
            "upload_receipts" => "SELECT count(*) FROM (SELECT 1 FROM td_mta.upload_receipts WHERE account=$1 AND created_at>=(extract(epoch FROM clock_timestamp())*1000)::bigint-$2 LIMIT $3) bounded",
            "draft_receipts" => "SELECT count(*) FROM (SELECT 1 FROM td_mta.draft_receipts WHERE account=$1 AND created_at>=(extract(epoch FROM clock_timestamp())*1000)::bigint-$2 LIMIT $3) bounded",
            "mailbox_create_receipts" => "SELECT count(*) FROM (SELECT 1 FROM td_mta.mailbox_create_receipts WHERE account=$1 AND created_at>=(extract(epoch FROM clock_timestamp())*1000)::bigint-$2 LIMIT $3) bounded",
            "submission_receipts" => "SELECT count(*) FROM (SELECT 1 FROM td_mta.submission_receipts WHERE account=$1 AND send_at>=(extract(epoch FROM clock_timestamp())*1000)::bigint-$2 LIMIT $3) bounded",
            "submission_cancel_receipts" => "SELECT count(*) FROM (SELECT 1 FROM td_mta.submission_cancel_receipts WHERE account=$1 AND created_at>=(extract(epoch FROM clock_timestamp())*1000)::bigint-$2 LIMIT $3) bounded",
            "flag_receipts" => "SELECT count(*) FROM (SELECT 1 FROM td_mta.flag_receipts WHERE account=$1 AND created_at>=(extract(epoch FROM clock_timestamp())*1000)::bigint-$2 LIMIT $3) bounded",
            _ => return None,
    })
}

/// The caller holds this account's row FOR UPDATE. Samples the database clock
/// once, deletes only this account's receipts in `ledger` older than the
/// retention before it, then refuses while `ceiling` younger ones remain.
/// The sample stamps the caller's new receipt.
pub(crate) async fn admit(
    client: &Client,
    ledger: Ledger,
    account: &[u8],
    ceiling: i64,
) -> Result<i64, Error> {
    let now: i64 = client
        .query_one(
            "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
            &[],
        )
        .await
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Integrity)?;
    let cutoff = now.checked_sub(RETENTION_MS).ok_or(Error::Integrity)?;
    client
        .execute(ledger.expire(), &[&account, &cutoff])
        .await
        .map_err(|_| Error::Database)?;
    let count: i64 = client
        .query_one(ledger.count(), &[&account, &ceiling])
        .await
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Integrity)?;
    if count >= ceiling {
        return Err(Error::Capacity);
    }
    Ok(now)
}
