use super::{number, Error};
use crate::{
    format::{
        self,
        row::{
            AttemptPhase, FailureReason, NotificationState, RecipientRow, RecipientState, Row,
            SubmissionRow,
        },
    },
    ids::{AttemptId, BlobId, EmailId, IdentityId, ThreadId},
    recipient_sweep::QueueGroup,
};
use tokio_postgres::Row as PgRow;

fn id(row: &PgRow, column: usize) -> Result<[u8; 16], Error> {
    let bytes: &[u8] = row.try_get(column).map_err(|_| Error::Integrity)?;
    bytes.try_into().map_err(|_| Error::Integrity)
}
fn optional_id(row: &PgRow, column: usize) -> Result<Option<[u8; 16]>, Error> {
    let bytes: Option<&[u8]> = row.try_get(column).map_err(|_| Error::Integrity)?;
    bytes
        .map(|bytes| bytes.try_into().map_err(|_| Error::Integrity))
        .transpose()
}
fn tag(row: &PgRow, column: usize) -> Result<u8, Error> {
    let value: i64 = row.try_get(column).map_err(|_| Error::Integrity)?;
    value.try_into().map_err(|_| Error::Integrity)
}
pub(super) fn submission(row: &PgRow) -> Result<SubmissionRow<'_>, Error> {
    let count: i64 = row.try_get(7).map_err(|_| Error::Integrity)?;
    let result = SubmissionRow {
        email: EmailId::from_bytes(id(row, 0)?),
        thread: ThreadId::from_bytes(id(row, 1)?),
        identity: IdentityId::from_bytes(id(row, 2)?),
        transmitted_blob: BlobId::from_bytes(id(row, 3)?),
        reverse_path: row.try_get(4).map_err(|_| Error::Integrity)?,
        send_at: row.try_get(5).map_err(|_| Error::Integrity)?,
        expires_at: row.try_get(6).map_err(|_| Error::Integrity)?,
        recipient_count: count.try_into().map_err(|_| Error::Integrity)?,
        completed_at: row.try_get(8).map_err(|_| Error::Integrity)?,
        notification: NotificationState::from_tag(tag(row, 9)?).map_err(|_| Error::Integrity)?,
        notification_email: optional_id(row, 10)?.map(EmailId::from_bytes),
    };
    Row::Submission(result)
        .encoded_len()
        .map_err(|_| Error::Integrity)?;
    Ok(result)
}
fn recipient(row: &PgRow) -> Result<RecipientRow<'_>, Error> {
    let count: i64 = row.try_get(5).map_err(|_| Error::Integrity)?;
    let uncertain = match tag(row, 3)? {
        0 => false,
        1 => true,
        _ => return Err(Error::Integrity),
    };
    let result = RecipientRow {
        address: row.try_get(1).map_err(|_| Error::Integrity)?,
        state: RecipientState::from_tag(tag(row, 2)?).map_err(|_| Error::Integrity)?,
        uncertain,
        attempt: optional_id(row, 4)?.map(AttemptId::from_bytes),
        attempt_count: count.try_into().map_err(|_| Error::Integrity)?,
        last_attempt_at: row.try_get(6).map_err(|_| Error::Integrity)?,
        phase: AttemptPhase::from_tag(tag(row, 7)?).map_err(|_| Error::Integrity)?,
        next_attempt_at: row.try_get(8).map_err(|_| Error::Integrity)?,
        rcpt_reply: row.try_get(9).map_err(|_| Error::Integrity)?,
        data_reply: row.try_get(10).map_err(|_| Error::Integrity)?,
        reason: FailureReason::from_tag(tag(row, 11)?).map_err(|_| Error::Integrity)?,
        diagnostic: row.try_get(12).map_err(|_| Error::Integrity)?,
    };
    Row::Recipient(result)
        .encoded_len()
        .map_err(|_| Error::Integrity)?;
    Ok(result)
}
// 128 bytes per recipient covers fixed fields, key and operation header;
// 4096 covers submission, change, account, attempt retirement and receipt.
pub(super) fn budget(count: i64, text: i64) -> Result<usize, Error> {
    if !(1..=1000).contains(&count) || text < 0 {
        return Err(Error::Integrity);
    }
    let count = usize::try_from(count).map_err(|_| Error::Capacity)?;
    let bytes = usize::try_from(text)
        .map_err(|_| Error::Capacity)?
        .checked_add(count * 128 + 4096)
        .ok_or(Error::Capacity)?;
    if bytes > format::MAX_TRANSACTION_BYTES || count + 5 > format::MAX_TRANSACTION_OPERATIONS {
        return Err(Error::Capacity);
    }
    Ok(bytes)
}
pub(super) struct Plan {
    pub changed: bool,
    pub completed_at: i64,
    pub bytes: Vec<u8>,
}
pub(super) fn reserve(
    before: SubmissionRow<'_>,
    rows: &[PgRow],
    sequence: u64,
    now: i64,
    budget: usize,
) -> Result<Plan, Error> {
    if rows.len() != before.recipient_count as usize {
        return Err(Error::Integrity);
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(budget)
        .map_err(|_| Error::Capacity)?;
    bytes.resize(budget, 0);
    let mut offset = 4096;
    let mut group = QueueGroup::new(before);
    let mut changed = false;
    for (ordinal, row) in rows.iter().enumerate() {
        let found: i64 = row.try_get(0).map_err(|_| Error::Integrity)?;
        if found != ordinal as i64
            || number(row.try_get(13).map_err(|_| Error::Integrity)?)? > sequence
        {
            return Err(Error::Integrity);
        }
        let mut value = recipient(row)?;
        group.recipient(value).map_err(|_| Error::Integrity)?;
        if value.state == RecipientState::Accepted
            || value.uncertain
            || value.phase == AttemptPhase::AcceptancePossible
        {
            return Err(Error::CannotUnsend);
        }
        changed |= value.state != RecipientState::Canceled;
        value.state = RecipientState::Canceled;
        value.phase = if value.attempt_count == 0 {
            AttemptPhase::None
        } else {
            AttemptPhase::Final
        };
        value.next_attempt_at = None;
        value.reason = FailureReason::Canceled;
        offset += 32; // Operation header and recipient key.
        let output = bytes.get_mut(offset..).ok_or(Error::Capacity)?;
        offset += Row::Recipient(value)
            .encode(output)
            .map_err(|_| Error::Integrity)?;
    }
    group.finish().map_err(|_| Error::Integrity)?;
    let completed_at = before.completed_at.unwrap_or(now);
    let after = SubmissionRow {
        completed_at: Some(completed_at),
        ..before
    };
    Row::Submission(after)
        .encode(bytes.get_mut(..4096).ok_or(Error::Capacity)?)
        .map_err(|_| Error::Integrity)?;
    Ok(Plan {
        changed,
        completed_at,
        bytes,
    })
}
