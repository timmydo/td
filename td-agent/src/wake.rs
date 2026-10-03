//! The wake budget (DESIGN.md §3, "Wakes"): two models can wake each other
//! without end, so every turn a message from another conversation starts
//! counts against its receiver's budget, derived from the receiver's log.
//! After `BUDGET` such turns since the human last wrote to that
//! conversation or to the orchestrator, further messages are logged
//! without starting a turn, held for the human, who is told once.
//!
//! A `report` is a notification to the orchestrator, which another
//! workspace's own budget bounds, and does not count. Background exit
//! notices will count when they exist (increment 12); a schedule's firings
//! will not.

use std::collections::HashSet;

use crate::store::{Event, Held, Kind};

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

/// Whether `event` falls after the budget's renewal: after the human's
/// last message here, `seq`, and after they last wrote to the
/// orchestrator, at `elsewhere`. One in the same second as that is taken
/// to precede it.
fn since(event: &Event, seq: u64, elsewhere: Option<u64>) -> bool {
    event.seq > seq && elsewhere.is_none_or(|time| event.time > time)
}

/// The turns of `events` that messages from other conversations started
/// since the budget was renewed, the human having last written to the
/// orchestrator at `elsewhere`. Only a message's first turn counts: the
/// human asks one again (`C-r`), which the budget does not bound.
pub fn spent(events: &[Event], elsewhere: Option<u64>) -> usize {
    let seq = renewed(events);
    // One pass: a message is logged before any turn it starts.
    let mut wakers: HashSet<u64> = HashSet::new();
    let mut started: HashSet<u64> = HashSet::new();
    let mut count = 0;
    for event in events {
        match event.kind {
            Kind::Message { status: None, .. } => {
                wakers.insert(event.seq);
            }
            // Noted as started whether or not it counts, so a turn asked
            // again never does.
            Kind::Started { of, .. }
                if started.insert(of) && wakers.contains(&of) && since(event, seq, elsewhere) =>
            {
                count += 1;
            }
            _ => {}
        }
    }
    count
}

/// Whether the human has been told since the budget's renewal that it is
/// spent: a message has been held for it.
pub fn told(events: &[Event], elsewhere: Option<u64>) -> bool {
    let seq = renewed(events);
    events.iter().any(|e| {
        since(e, seq, elsewhere)
            && matches!(
                e.kind,
                Kind::Message {
                    held: Some(Held::Budget),
                    ..
                }
            )
    })
}

/// What the human is told when the budget is spent.
pub fn notice() -> String {
    format!(
        "messages from other conversations have started {BUDGET} turns here since you last wrote to this conversation or to the orchestrator; until you do, further messages are logged without starting a turn"
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
        assert_eq!(spent(&events, None), 3);
        // The human writes to the orchestrator: what came before it is
        // forgiven.
        assert_eq!(spent(&events, Some(101)), 1);
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
        assert_eq!(spent(&events, None), 0);
        assert!(!told(&events, None));
        let m = next();
        events.push(message(m, 130, None, Some(Held::Budget)));
        assert!(told(&events, None));
        assert!(!told(&events, Some(130)), "renewed at the same second");
    }
}
