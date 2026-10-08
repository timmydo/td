//! Final-state checks from QUEUE.md; no attempt/transition authority.
use crate::format::row::{
    AttemptPhase, FailureReason, NotificationState, RecipientRow, RecipientState, SubmissionRow,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueError {
    RecipientState,
    Completion,
    Notification,
    PartialCancellation,
}

pub(crate) struct Group {
    completed: bool,
    notification: NotificationState,
    pending: bool,
    failed: bool,
    canceled: bool,
    other: bool,
}
impl Group {
    pub(crate) fn new(row: SubmissionRow<'_>) -> Self {
        Self {
            completed: row.completed_at.is_some(),
            notification: row.notification,
            pending: false,
            failed: false,
            canceled: false,
            other: false,
        }
    }
    pub(crate) fn recipient(&mut self, row: RecipientRow<'_>) -> Result<(), QueueError> {
        validate(row)?;
        self.pending |= matches!(
            row.state,
            RecipientState::Queued | RecipientState::InFlight | RecipientState::RetryWait
        ) || (row.state == RecipientState::OutcomeUnknown
            && row.next_attempt_at.is_some());
        self.failed |= matches!(
            row.state,
            RecipientState::Failed | RecipientState::OutcomeUnknown
        );
        self.canceled |= row.state == RecipientState::Canceled;
        self.other |= row.state != RecipientState::Canceled;
        Ok(())
    }
    pub(crate) fn finish(&self) -> Result<(), QueueError> {
        if self.completed == self.pending {
            return Err(QueueError::Completion);
        }
        if self.canceled && self.other {
            return Err(QueueError::PartialCancellation);
        }
        let notice = self.completed && self.failed;
        if !(self.completed && self.canceled)
            && (self.notification != NotificationState::None) != notice
        {
            return Err(QueueError::Notification);
        }
        Ok(())
    }
}

// Stored replies can be longer than one wire line after normalization. Only
// their code/separator is needed here; wire parsing and normalization are separate.
fn reply_class(reply: Option<&str>) -> Option<u16> {
    let bytes = reply?.as_bytes();
    let [class @ b'2'..=b'5', b'0'..=b'5', b'0'..=b'9'] = bytes.get(..3)? else {
        return None;
    };
    if bytes.len() > 3 && bytes.get(3) != Some(&b' ') {
        return None;
    }
    Some(u16::from(class - b'0'))
}
fn validate(row: RecipientRow<'_>) -> Result<(), QueueError> {
    use AttemptPhase as P;
    use FailureReason as F;
    use RecipientState as S;
    let attempted = row.attempt_count != 0;
    // The caller has already checked the codec's attempt/phase presence rules.
    let final_phase = if attempted { P::Final } else { P::None };
    let next = row.next_attempt_at.is_some();
    let retry_reason = matches!(
        row.reason,
        F::SmtpTemporary | F::Network | F::Tls | F::Authentication | F::Protocol
    );
    let latest = row.data_reply.or(row.rcpt_reply);
    let valid = match row.state {
        S::Queued => row.phase == P::None && next && !row.uncertain && row.reason == F::None,
        S::InFlight => {
            matches!(row.phase, P::Prepared | P::Body | P::AcceptancePossible)
                && !next
                && row.reason == F::None
        }
        S::RetryWait => {
            row.phase == P::Final
                && next
                && !row.uncertain
                && retry_reason
                && (row.reason != F::SmtpTemporary || reply_class(latest) == Some(4))
        }
        S::Accepted => {
            row.phase == P::Final
                && !next
                && row.reason == F::None
                && reply_class(row.rcpt_reply) == Some(2)
                && reply_class(row.data_reply) == Some(2)
        }
        S::Failed => {
            row.phase == final_phase
                && !next
                && !row.uncertain
                && (row.reason == F::Expired
                    || (row.reason == F::SmtpPermanent
                        && attempted
                        && reply_class(latest) == Some(5)))
        }
        S::Canceled => {
            row.phase == final_phase && !next && !row.uncertain && row.reason == F::Canceled
        }
        S::OutcomeUnknown => {
            matches!(row.phase, P::AcceptancePossible | P::Final)
                && row.uncertain
                && (retry_reason
                    || matches!(row.reason, F::Uncertain | F::SmtpPermanent | F::Expired))
                && (row.reason != F::SmtpTemporary || reply_class(latest) == Some(4))
                && (row.reason != F::SmtpPermanent || reply_class(latest) == Some(5))
                && (next == !matches!(row.reason, F::Expired | F::SmtpPermanent))
        }
    };
    if !valid || (!attempted && (row.rcpt_reply.is_some() || row.data_reply.is_some())) {
        return Err(QueueError::RecipientState);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        format::row::Row,
        ids::{AttemptId, EmailId},
    };
    fn recipient(state: RecipientState) -> RecipientRow<'static> {
        let Row::Recipient(mut row) = super::super::tests::recipient() else {
            panic!("fixture");
        };
        row.state = state;
        if state != RecipientState::Queued {
            row.attempt = Some(AttemptId::from_bytes([1; 16]));
            row.attempt_count = 1;
            row.last_attempt_at = Some(1);
            row.phase = AttemptPhase::Final;
        }
        row.next_attempt_at = None;
        match state {
            RecipientState::Queued => row.next_attempt_at = Some(0),
            RecipientState::InFlight => row.phase = AttemptPhase::Prepared,
            RecipientState::RetryWait => {
                row.reason = FailureReason::Network;
                row.next_attempt_at = Some(2);
            }
            RecipientState::Accepted => {
                row.rcpt_reply = Some("250 recipient ok");
                row.data_reply = Some("250 stored");
            }
            RecipientState::Failed => row.reason = FailureReason::Expired,
            RecipientState::Canceled => row.reason = FailureReason::Canceled,
            RecipientState::OutcomeUnknown => {
                row.reason = FailureReason::Expired;
                row.uncertain = true;
            }
        }
        row
    }
    fn submission(completed: bool, notification: NotificationState) -> SubmissionRow<'static> {
        let Row::Submission(mut row) = super::super::tests::submission(1) else {
            panic!("fixture");
        };
        row.completed_at = completed.then_some(-1);
        row.notification = notification;
        row.notification_email =
            (notification == NotificationState::Stored).then_some(EmailId::from_bytes([2; 16]));
        row
    }
    fn check(row: RecipientRow<'_>) -> Result<(), QueueError> {
        Row::Recipient(row).encoded_len().unwrap();
        validate(row)
    }
    #[test]
    fn states_enforce_phase_retry_uncertainty_reason_and_actual_replies() {
        use RecipientState as S;
        for state in [
            S::Queued,
            S::InFlight,
            S::RetryWait,
            S::Accepted,
            S::Failed,
            S::Canceled,
            S::OutcomeUnknown,
        ] {
            let row = recipient(state);
            assert_eq!(check(row), Ok(()));
            let mut wrong = row;
            if state == S::Queued {
                wrong.attempt = Some(AttemptId::from_bytes([1; 16]));
                wrong.attempt_count = 1;
                wrong.last_attempt_at = Some(1);
                wrong.phase = AttemptPhase::Final;
            } else {
                wrong.phase = if state == S::InFlight {
                    AttemptPhase::Final
                } else {
                    AttemptPhase::Prepared
                };
            }
            assert_eq!(check(wrong), Err(QueueError::RecipientState));
            wrong = row;
            wrong.reason = if row.reason == FailureReason::None {
                FailureReason::Canceled
            } else {
                FailureReason::None
            };
            assert_eq!(check(wrong), Err(QueueError::RecipientState));
            if state != S::OutcomeUnknown {
                wrong = row;
                wrong.next_attempt_at = if row.next_attempt_at.is_some() {
                    None
                } else {
                    Some(2)
                };
                assert_eq!(check(wrong), Err(QueueError::RecipientState));
            }
        }
        for phase in [AttemptPhase::Body, AttemptPhase::AcceptancePossible] {
            let mut row = recipient(S::InFlight);
            row.phase = phase;
            row.uncertain = true;
            assert_eq!(check(row), Ok(()));
        }
        for state in [S::RetryWait, S::Failed] {
            let mut row = recipient(state);
            row.uncertain = true;
            assert_eq!(check(row), Err(QueueError::RecipientState));
        }
        for state in [S::Failed, S::Canceled] {
            let mut row = recipient(state);
            row.attempt = None;
            row.attempt_count = 0;
            row.last_attempt_at = None;
            row.phase = AttemptPhase::None;
            assert_eq!(check(row), Ok(()));
            row.rcpt_reply = Some("550 old");
            assert_eq!(check(row), Err(QueueError::RecipientState));
        }
        for reply in [
            None,
            Some("354 continue"),
            Some("550 failed"),
            Some("250-fake"),
            Some("299 invalid"),
            Some("ok"),
        ] {
            let mut row = recipient(S::Accepted);
            row.data_reply = reply;
            assert_eq!(check(row), Err(QueueError::RecipientState));
            row = recipient(S::Accepted);
            row.rcpt_reply = reply;
            assert_eq!(check(row), Err(QueueError::RecipientState));
        }
        let mut unknown = recipient(S::OutcomeUnknown);
        for reason in [
            FailureReason::Network,
            FailureReason::Tls,
            FailureReason::Authentication,
            FailureReason::Protocol,
            FailureReason::Uncertain,
        ] {
            unknown.reason = reason;
            unknown.next_attempt_at = None;
            assert_eq!(check(unknown), Err(QueueError::RecipientState));
            unknown.next_attempt_at = Some(2);
            assert_eq!(check(unknown), Ok(()));
        }
        unknown.reason = FailureReason::Expired;
        assert_eq!(check(unknown), Err(QueueError::RecipientState));
        unknown.next_attempt_at = None;
        assert_eq!(check(unknown), Ok(()));
        unknown.reason = FailureReason::SmtpPermanent;
        assert_eq!(check(unknown), Err(QueueError::RecipientState));
        unknown.rcpt_reply = Some("550 refused");
        assert_eq!(check(unknown), Ok(()));
        unknown.next_attempt_at = Some(2);
        assert_eq!(check(unknown), Err(QueueError::RecipientState));
        let mut row = recipient(S::Accepted);
        row.uncertain = true;
        assert_eq!(check(row), Ok(()));
        for (state, reason, reply) in [
            (S::RetryWait, FailureReason::SmtpTemporary, "450 defer"),
            (S::Failed, FailureReason::SmtpPermanent, "550 refuse"),
        ] {
            let mut row = recipient(state);
            row.reason = reason;
            assert_eq!(check(row), Err(QueueError::RecipientState));
            row.rcpt_reply = Some(reply);
            assert_eq!(check(row), Ok(()));
            row.data_reply = Some("250 old positive");
            assert_eq!(check(row), Err(QueueError::RecipientState));
            row.rcpt_reply = None;
            row.data_reply = Some(reply);
            assert_eq!(check(row), Ok(()));
        }
    }
    #[test]
    fn group_completion_notifications_and_atomic_cancellation() {
        use RecipientState as S;
        for states in [
            &[S::Queued][..],
            &[S::InFlight][..],
            &[S::RetryWait][..],
            &[S::Accepted][..],
            &[S::Failed][..],
            &[S::Canceled][..],
            &[S::OutcomeUnknown][..],
            &[S::Accepted, S::Failed][..],
            &[S::Accepted, S::Queued][..],
        ] {
            let pending = states
                .iter()
                .any(|s| matches!(s, S::Queued | S::InFlight | S::RetryWait));
            let failed = states
                .iter()
                .any(|s| matches!(s, S::Failed | S::OutcomeUnknown));
            for completed in [false, true] {
                for notice in [
                    NotificationState::None,
                    NotificationState::Pending,
                    NotificationState::Stored,
                ] {
                    let mut group = Group::new(submission(completed, notice));
                    for &state in states {
                        group.recipient(recipient(state)).unwrap();
                    }
                    let expected = if completed == pending {
                        Err(QueueError::Completion)
                    } else if !states.iter().all(|s| *s == S::Canceled)
                        && (notice != NotificationState::None) != (completed && failed)
                    {
                        Err(QueueError::Notification)
                    } else {
                        Ok(())
                    };
                    assert_eq!(group.finish(), expected);
                }
            }
        }
        let mut row = recipient(S::OutcomeUnknown);
        row.reason = FailureReason::Uncertain;
        row.next_attempt_at = Some(2);
        let mut group = Group::new(submission(false, NotificationState::None));
        group.recipient(row).unwrap();
        assert_eq!(group.finish(), Ok(()));
        let mut group = Group::new(submission(true, NotificationState::None));
        group.recipient(recipient(S::Canceled)).unwrap();
        group.recipient(recipient(S::Accepted)).unwrap();
        assert_eq!(group.finish(), Err(QueueError::PartialCancellation));
    }
}
