//! Preserve retained queue history against the pre-transaction rows.
use super::*;
use crate::format::row::{
    AttemptPhase, FailureReason, NotificationState, RecipientRow, RecipientState, SubmissionRow,
};

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
        && (old.completed_at.is_none() || old.completed_at == next.completed_at)
        && match old.notification {
            NotificationState::None => {
                next.notification == NotificationState::None
                    || (old.completed_at.is_none()
                        && next.notification == NotificationState::Pending)
            }
            NotificationState::Pending => next.notification != NotificationState::None,
            NotificationState::Stored => {
                next.notification == NotificationState::Stored
                    && old.notification_email == next.notification_email
            }
        }
}
fn recipient(old: RecipientRow<'_>, next: RecipientRow<'_>) -> bool {
    old.address == next.address
        && terminal_recipient(old, next)
        && reply_history(old, next)
        && acceptance_boundary(old, next)
        && (!old.uncertain || next.uncertain)
        && if next.attempt_count == old.attempt_count {
            (next.state != RecipientState::InFlight || old.state == RecipientState::InFlight)
                && next.attempt == old.attempt
                && next.last_attempt_at == old.last_attempt_at
                && active_phase(old, next)
        } else {
            let eligible = matches!(
                old.state,
                RecipientState::Queued | RecipientState::RetryWait
            ) || (old.state == RecipientState::OutcomeUnknown
                && old.next_attempt_at.is_some());
            eligible
                && next.state == RecipientState::InFlight
                && next.phase == AttemptPhase::Prepared
                && next.diagnostic.is_empty()
                && old.attempt_count.checked_add(1) == Some(next.attempt_count)
                && next.attempt != old.attempt
        }
}

fn reply_history(old: RecipientRow<'_>, next: RecipientRow<'_>) -> bool {
    if old.state != RecipientState::InFlight || next.state == RecipientState::Canceled {
        return old.rcpt_reply == next.rcpt_reply && old.data_reply == next.data_reply;
    }
    !matches!(
        old.phase,
        AttemptPhase::Body | AttemptPhase::AcceptancePossible
    ) || old.rcpt_reply == next.rcpt_reply
}

fn acceptance_boundary(old: RecipientRow<'_>, next: RecipientRow<'_>) -> bool {
    use FailureReason as F;
    use RecipientState as S;
    let exposed = old.state == S::InFlight && old.phase == AttemptPhase::AcceptancePossible;
    if !exposed {
        return next.state != S::Accepted || old.state == S::Accepted;
    }
    let data_class = crate::recipient_sweep::reply_class(next.data_reply);
    match next.state {
        S::Accepted | S::InFlight | S::OutcomeUnknown => true,
        S::Queued | S::Canceled => false,
        S::RetryWait => {
            old.data_reply.is_none() && next.reason == F::SmtpTemporary && data_class == Some(4)
        }
        S::Failed => {
            old.data_reply.is_none()
                && match next.reason {
                    F::SmtpPermanent => data_class == Some(5),
                    F::Expired => matches!(data_class, Some(4 | 5)),
                    _ => false,
                }
        }
    }
}

fn active_phase(old: RecipientRow<'_>, next: RecipientRow<'_>) -> bool {
    if old.state != RecipientState::InFlight || next.state != RecipientState::InFlight {
        return true;
    }
    use AttemptPhase as P;
    matches!(
        (old.phase, next.phase),
        (P::Prepared, P::Prepared | P::Body)
            | (P::Body, P::Body | P::AcceptancePossible)
            | (P::AcceptancePossible, P::AcceptancePossible)
    )
}

fn terminal_recipient(old: RecipientRow<'_>, next: RecipientRow<'_>) -> bool {
    use RecipientState as S;
    let valid_state = match old.state {
        S::Accepted | S::Canceled => next.state == old.state,
        S::Failed => matches!(next.state, S::Failed | S::Canceled),
        S::OutcomeUnknown if old.next_attempt_at.is_none() => {
            next.state == S::OutcomeUnknown && next.next_attempt_at.is_none()
        }
        _ => return true,
    };
    valid_state
        && old.attempt_count == next.attempt_count
        && old.phase == next.phase
        && old.uncertain == next.uncertain
        && (old.reason == next.reason
            || (old.state == S::Failed
                && next.state == S::Canceled
                && next.reason == FailureReason::Canceled))
}
