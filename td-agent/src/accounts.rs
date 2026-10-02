//! What has been spent (DESIGN.md §5). A conversation's spending is a
//! function of its log: every request's recorded cost, and for a request
//! a restart found started and never finished, its whole reservation. The
//! day's spending crosses conversations, so the window process keeps it
//! (`Ledger`), in the state directory's `spend`, granting each reservation
//! a conversation process asks for only while the day's limit holds.

use std::path::{Path, PathBuf};

use crate::cost;
use crate::store::{Event, Id, Kind};

/// What `events` spent, or the part of it for turn `turn` when given.
fn total(events: &[Event], turn: Option<u64>) -> u64 {
    // Each request's turn and reservation, by its sequence number.
    let mut requests: Vec<(u64, u64, u64)> = Vec::new();
    let mut sum = 0u64;
    // The requests a usage record charged.
    let mut charged: Vec<u64> = Vec::new();
    let in_turn = |of: u64| turn.is_none_or(|turn| turn == of);
    let find = |requests: &[(u64, u64, u64)], seq: u64| {
        requests
            .iter()
            .rev()
            .find(|(s, _, _)| *s == seq)
            .map(|(_, turn, reserved)| (*turn, *reserved))
    };
    for event in events {
        match &event.kind {
            Kind::Request { turn, reserved, .. } => requests.push((event.seq, *turn, *reserved)),
            Kind::Usage { request, cost, .. } => {
                charged.push(*request);
                if find(&requests, *request).is_some_and(|(turn, _)| in_turn(turn)) {
                    sum = sum.saturating_add(*cost);
                }
            }
            // A request whose usage was logged before its process died
            // has its cost; only one without counts its reservation.
            Kind::Interrupted { started } if !charged.contains(started) => {
                if let Some((turn, reserved)) = find(&requests, *started) {
                    if in_turn(turn) {
                        sum = sum.saturating_add(reserved);
                    }
                }
            }
            _ => {}
        }
    }
    sum
}

/// What the conversation has spent.
pub fn spent(events: &[Event]) -> u64 {
    total(events, None)
}

/// What turn `turn` (its `Started` record's sequence number) has spent.
pub fn turn_spent(events: &[Event], turn: u64) -> u64 {
    total(events, Some(turn))
}

/// The day a time falls in: days since the epoch, in UTC, since td-agent
/// carries no zone data.
pub fn day(seconds: u64) -> u64 {
    seconds / 86_400
}

/// The day's spending and the reservations still out, the window
/// process's alone. Each grant is written to `spend` before it is
/// answered, so a reservation is counted however the processes end.
pub struct Ledger {
    path: Option<PathBuf>,
    limit: Option<u64>,
    day: u64,
    spent: u64,
    /// Reservations granted and not yet settled: the conversation, its
    /// process's request id, the day it was granted on and its amount.
    held: Vec<(Id, u64, u64, u64)>,
}

/// The ledger's file in the state directory.
const FILE: &str = "spend";

impl Ledger {
    /// The ledger in `state`, as last written when it is today's.
    pub fn load(state: Option<&Path>, limit: Option<u64>, now: u64) -> Self {
        let today = day(now);
        let spent = state
            .and_then(|s| crate::store::read_bounded(&s.join(FILE), 256).ok())
            .and_then(|bytes| {
                let text = String::from_utf8(bytes).ok()?;
                let rest = text.strip_prefix("day ")?.strip_suffix('\n')?;
                let (day, rest) = rest.split_once(" spent ")?;
                (day.parse::<u64>().ok()? == today).then_some(rest.parse::<u64>().ok()?)
            })
            .unwrap_or(0);
        Self {
            path: state.map(Path::to_path_buf),
            limit,
            day: today,
            spent,
            held: Vec::new(),
        }
    }

    /// What today has spent and holds reserved.
    pub fn today(&mut self, now: u64) -> u64 {
        self.roll(now);
        self.spent
    }

    fn roll(&mut self, now: u64) {
        if day(now) != self.day {
            self.day = day(now);
            self.spent = 0;
        }
    }

    fn save(&self) -> Result<(), String> {
        match &self.path {
            Some(state) => crate::store::replace(
                state,
                FILE,
                format!("day {} spent {}\n", self.day, self.spent).as_bytes(),
            ),
            None => Ok(()),
        }
    }

    /// Grants `amount` for request `id` of conversation `conversation`,
    /// or says why not: past `max_cost_per_day`, or the grant could not be
    /// written down.
    pub fn reserve(
        &mut self,
        conversation: &Id,
        id: u64,
        amount: u64,
        now: u64,
    ) -> Result<(), String> {
        self.roll(now);
        cost::within("max_cost_per_day", self.limit, self.spent, amount)?;
        self.spent = self.spent.saturating_add(amount);
        if let Err(e) = self.save() {
            self.spent = self.spent.saturating_sub(amount);
            return Err(format!("the day's spending could not be recorded: {e}"));
        }
        self.held.push((conversation.clone(), id, self.day, amount));
        Ok(())
    }

    /// Request `id` of `conversation` cost `amount`: it replaces the
    /// reservation in the day it was granted on.
    pub fn settle(
        &mut self,
        conversation: &Id,
        id: u64,
        amount: u64,
        now: u64,
    ) -> Result<(), String> {
        self.roll(now);
        let Some(at) = self
            .held
            .iter()
            .position(|(c, i, _, _)| c == conversation && *i == id)
        else {
            return Ok(());
        };
        let (_, _, day, reserved) = self.held.swap_remove(at);
        if day == self.day {
            self.spent = self.spent.saturating_sub(reserved).saturating_add(amount);
            self.save()?;
        }
        Ok(())
    }

    /// A conversation process ended: what it still held stays spent, at
    /// its whole reservation, and is no longer waited on.
    pub fn forget(&mut self, conversation: &Id) {
        self.held.retain(|(c, _, _, _)| c != conversation);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::cost::{Tokens, ONE};
    use crate::store::{Basis, Purpose};

    fn event(seq: u64, kind: Kind) -> Event {
        Event { seq, time: 0, kind }
    }

    fn request(seq: u64, turn: u64, reserved: u64) -> Event {
        event(
            seq,
            Kind::Request {
                turn,
                purpose: Purpose::Turn,
                prefix: 0,
                head: String::new(),
                bytes: 0,
                reserved,
            },
        )
    }

    fn usage(seq: u64, request: u64, cost: u64) -> Event {
        event(
            seq,
            Kind::Usage {
                request,
                tokens: Tokens::default(),
                cost,
                basis: Basis::Reported,
            },
        )
    }

    #[test]
    fn a_conversation_spends_its_costs_and_its_interrupted_reservations() {
        let events = vec![
            request(1, 10, 500),
            usage(2, 1, 30),
            request(3, 10, 700),
            event(4, Kind::Interrupted { started: 3 }),
            request(5, 20, 900),
            usage(6, 5, 40),
            // An interrupted turn is no request: nothing to count.
            event(7, Kind::Interrupted { started: 20 }),
            // Its usage logged, then the process died before its finish:
            // the cost counts, once, and not the reservation.
            request(8, 20, 5000),
            usage(9, 8, 60),
            event(10, Kind::Interrupted { started: 8 }),
        ];
        assert_eq!(spent(&events), 30 + 700 + 40 + 60);
        assert_eq!(turn_spent(&events, 10), 730);
        assert_eq!(turn_spent(&events, 20), 100);
        assert_eq!(turn_spent(&events, 30), 0);
    }

    #[test]
    fn the_day_reserves_settles_and_refuses_past_its_limit() {
        let dir = std::env::temp_dir().join(format!(
            "td-agent-ledger-{}-{}",
            std::process::id(),
            crate::store::random_hex(4).unwrap()
        ));
        std::fs::create_dir(&dir).unwrap();
        let a = Id::parse(&"a".repeat(32)).unwrap();
        let noon = 20_000 * 86_400 + 43_200;
        let mut ledger = Ledger::load(Some(&dir), Some(ONE), noon);
        ledger.reserve(&a, 1, ONE / 2, noon).unwrap();
        ledger.reserve(&a, 2, ONE / 2, noon).unwrap();
        let refused = ledger.reserve(&a, 3, 1, noon).unwrap_err();
        assert!(
            refused.starts_with("max_cost_per_day is $1.0000"),
            "{refused}"
        );
        // A settled request counts what it cost, not what it reserved.
        ledger.settle(&a, 1, ONE / 10, noon).unwrap();
        assert_eq!(ledger.today(noon), ONE / 2 + ONE / 10);
        // A process gone with a reservation out leaves it counted.
        ledger.forget(&a);
        ledger.settle(&a, 2, 0, noon).unwrap();
        assert_eq!(ledger.today(noon), ONE / 2 + ONE / 10);
        // The day's total survives the window.
        let again = Ledger::load(Some(&dir), Some(ONE), noon + 60);
        assert_eq!(again.spent, ONE / 2 + ONE / 10);
        // A new day starts at nothing, and a reservation granted the day
        // before settles into that day, not this one.
        let mut ledger = Ledger::load(Some(&dir), Some(ONE), noon);
        ledger.reserve(&a, 9, ONE / 4, noon).unwrap();
        let tomorrow = noon + 86_400;
        assert_eq!(ledger.today(tomorrow), 0);
        ledger.settle(&a, 9, ONE / 8, tomorrow).unwrap();
        assert_eq!(ledger.today(tomorrow), 0);
        assert_eq!(Ledger::load(Some(&dir), None, tomorrow).spent, 0);
        // No limit refuses nothing.
        let mut open = Ledger::load(None, None, noon);
        open.reserve(&a, 1, u64::MAX, noon).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
