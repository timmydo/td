//! The wake budget (DESIGN.md §3, "Wakes"): two models can wake each other
//! without end, so every turn a message from another conversation starts
//! counts against its receiver's budget, derived from the receiver's log.
//! After `BUDGET` such turns since the human last wrote to that
//! conversation, further messages are logged without starting a turn,
//! held for the human, who is told once.
//!
//! A background process's exit notice counts too: a model and its own
//! processes could wake each other as two models can. A report, which a
//! log from before there were only conversations may hold, was a
//! notification its sender's own budget bounded, and does not count; a
//! schedule's firings will not.

use std::collections::HashSet;

use crate::store::{Effect, Event, Held, Kind};

/// The turns messages may start between two of the human's messages.
pub const BUDGET: usize = 20;

/// Where the budget was last renewed in `events` itself: the sequence
/// number of the human's last message there, 0 for none.
fn renewed(events: &[Event]) -> u64 {
    events
        .iter()
        .rev()
        .find(|e| matches!(e.kind, Kind::User { .. }))
        .map_or(0, |e| e.seq)
}

/// The turns of `events` that messages from other conversations started
/// since the budget was renewed. Only a message's first turn counts: the
/// human asks one again (`C-r`), which the budget does not bound.
pub fn spent(events: &[Event]) -> usize {
    let seq = renewed(events);
    // One pass: a message is logged before any turn it starts.
    let mut wakers: HashSet<u64> = HashSet::new();
    let mut started: HashSet<u64> = HashSet::new();
    let mut count = 0;
    for event in events {
        match event.kind {
            Kind::Message { status: None, .. } | Kind::Ended { .. } => {
                wakers.insert(event.seq);
            }
            // A turn, noted as started whether or not it counts, so a turn
            // asked again never does; a compaction starts none.
            Kind::Started {
                effect: Effect::Turn,
                of,
            } if started.insert(of) && wakers.contains(&of) && event.seq > seq => {
                count += 1;
            }
            _ => {}
        }
    }
    count
}

/// Whether the human has been told since the budget's renewal that it is
/// spent: a message has been held for it.
pub fn told(events: &[Event]) -> bool {
    let seq = renewed(events);
    events.iter().any(|e| {
        e.seq > seq
            && matches!(
                e.kind,
                Kind::Message {
                    held: Some(Held::Budget),
                    ..
                } | Kind::Ended {
                    held: Some(Held::Budget),
                    ..
                }
            )
    })
}

/// What the human is told when the budget is spent.
pub fn notice() -> String {
    format!(
        "messages from other conversations and background processes' ends have started {BUDGET} turns here since you last wrote to this conversation; until you do, further ones are logged without starting a turn"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::store::{Effect, Id, Role};

    fn event(seq: u64, time: u64, kind: Kind) -> Event {
        Event { seq, time, kind }
    }

    fn message(seq: u64, time: u64, status: Option<&str>, held: Option<Held>) -> Event {
        event(
            seq,
            time,
            Kind::Message {
                delivery: format!("{seq:032x}"),
                from: Id::parse(&"a".repeat(32)).unwrap(),
                role: Role::Conversation,
                text: "x".into(),
                status: status.map(str::to_string),
                held,
            },
        )
    }

    fn started(seq: u64, time: u64, of: u64) -> Event {
        event(
            seq,
            time,
            Kind::Started {
                effect: Effect::Turn,
                of,
            },
        )
    }

    /// The human's compaction, though of a message, starts no turn: it
    /// neither counts nor keeps the message's turn from counting.
    #[test]
    fn a_compaction_is_no_messages_turn() {
        let mut events = vec![message(1, 100, None, None)];
        events.push(event(
            2,
            101,
            Kind::Started {
                effect: Effect::Compact,
                of: 1,
            },
        ));
        assert_eq!(spent(&events), 0);
        events.push(started(3, 102, 1));
        assert_eq!(spent(&events), 1);
    }

    #[test]
    fn turns_messages_start_count_until_the_human_writes() {
        let mut events = Vec::new();
        let mut seq = 0;
        let mut next = || {
            seq += 1;
            seq
        };
        for n in 0..3 {
            let m = next();
            events.push(message(m, 100 + n, None, None));
            events.push(started(next(), 100 + n, m));
        }
        // A report wakes without counting.
        let r = next();
        events.push(message(r, 110, Some("done"), None));
        events.push(started(next(), 110, r));
        // A turn the human asks again does not count.
        events.push(started(next(), 111, 1));
        assert_eq!(spent(&events), 3);
        // The human writes here.
        let u = next();
        events.push(event(
            u,
            120,
            Kind::User {
                delivery: "d".into(),
                text: "go on".into(),
            },
        ));
        assert_eq!(spent(&events), 0);
        assert!(!told(&events));
        let m = next();
        events.push(message(m, 130, None, Some(Held::Budget)));
        assert!(told(&events));
    }

    /// A background process's end that starts a turn counts as a message
    /// does, and one held for the budget tells the human.
    #[test]
    fn a_background_processs_end_counts_against_the_budget() {
        let ended = |seq, held| {
            event(
                seq,
                100,
                Kind::Ended {
                    number: seq,
                    how: "exit status 0".into(),
                    tail: None,
                    held,
                },
            )
        };
        let mut events = vec![ended(1, None), started(2, 100, 1), ended(3, None)];
        assert_eq!(spent(&events), 1);
        assert!(!told(&events));
        events.push(ended(4, Some(Held::Budget)));
        assert!(told(&events));
    }

    /// A schedule's firing is bounded by its own times: its turn is no
    /// wake.
    #[test]
    fn a_firings_turn_is_not_a_wake() {
        let fired = event(
            1,
            1,
            Kind::Fired {
                delivery: "d".into(),
                schedule: "0a1b2c3d".into(),
                author: Some(Id::parse(&"a".repeat(32)).unwrap()),
                text: "x".into(),
                skipped: None,
            },
        );
        assert_eq!(spent(&[fired, started(2, 1, 1)]), 0);
    }
}
