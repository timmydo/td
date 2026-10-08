//! Preserve retained queue history against the pre-transaction rows.
use super::*;
use crate::format::row::{RecipientRow, SubmissionRow};

pub(super) fn validate(
    native: &Native,
    view: &mut TransactionView<'_>,
    operations: Operations<'_, '_>,
    scratch: &mut [u8],
) -> Result<(), ports::Error> {
    for position in 0..operations.len() {
        native.check()?;
        let Some(key) = operations.queue_put_key(position)? else {
            continue;
        };
        let Some((previous, _)) = view.get(key, scratch)? else {
            continue;
        };
        let mut replaced = false;
        for (scan, later) in (position + 1..operations.len()).enumerate() {
            if scan % 64 == 0 {
                native.check()?;
            }
            if operations.queue_key(later)? == Some(key) {
                replaced = true;
                break;
            }
        }
        if replaced {
            continue;
        }
        native.check()?;
        let Value::Row(Mutation::Put { row, .. }) = operations.get(position)?.value() else {
            continue;
        };

        let valid = match (previous, row) {
            (Row::Submission(previous), Row::Submission(next)) => submission(previous, next),
            (Row::Recipient(previous), Row::Recipient(next)) => recipient(previous, next),
            _ => return Err(ports::Error::Corrupt),
        };
        if !valid {
            return Err(ports::Error::Conflict);
        }
    }
    Ok(())
}
fn submission(old: SubmissionRow<'_>, next: SubmissionRow<'_>) -> bool {
    old.email == next.email
        && old.thread == next.thread
        && old.identity == next.identity
        && old.transmitted_blob == next.transmitted_blob
        && old.reverse_path == next.reverse_path
        && old.send_at == next.send_at
        && old.expires_at == next.expires_at
        && old.recipient_count == next.recipient_count
}
fn recipient(old: RecipientRow<'_>, next: RecipientRow<'_>) -> bool {
    old.address == next.address
        && (!old.uncertain || next.uncertain)
        && next.attempt_count >= old.attempt_count
        && (next.attempt_count != old.attempt_count
            || (next.attempt == old.attempt && next.last_attempt_at == old.last_attempt_at))
}
