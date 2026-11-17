//! Per-peer accepted-mail rate for inbound SMTP (DESIGN.md section 9).
//!
//! Each key keeps one theoretical arrival time per currency (GCRA): a peer is
//! limited while either lies beyond its burst tolerance. Charges follow
//! acceptance, so a remembered key costs its sender an accepted message.
use crate::limits::{
    SMTP_RATE_BLOCK, SMTP_RATE_BLOCK_BURST, SMTP_RATE_BLOCK_MS, SMTP_RATE_MESSAGE_BURST,
    SMTP_RATE_MESSAGE_MS, SMTP_RATE_PEERS,
};
use crate::ports::Tick;
use std::net::IpAddr;

/// The exact IPv4 address, or the IPv6 /64 one host can hold entirely.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerKey {
    V4(u32),
    V6(u64),
}
impl From<IpAddr> for PeerKey {
    fn from(address: IpAddr) -> Self {
        match address {
            IpAddr::V4(v4) => Self::V4(u32::from(v4)),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => Self::V4(u32::from(v4)),
                None => Self::V6((u128::from(v6) >> 64) as u64),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bucket {
    pub burst: u64,
    pub interval_ms: u64,
}
impl Bucket {
    fn tolerance(self) -> u64 {
        self.burst
            .saturating_sub(1)
            .saturating_mul(self.interval_ms)
    }
    fn limited(self, arrival: u64, now: u64) -> bool {
        self.excess(arrival, now) > 0
    }
    /// Time until this balance admits again; zero while it admits.
    fn excess(self, arrival: u64, now: u64) -> u64 {
        arrival.saturating_sub(now).saturating_sub(self.tolerance())
    }
    fn charge(self, arrival: u64, now: u64, units: u64) -> u64 {
        arrival
            .max(now)
            .saturating_add(units.saturating_mul(self.interval_ms))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Policy {
    pub messages: Bucket,
    pub blocks: Bucket,
}
impl Policy {
    pub const DEFAULT: Self = Self {
        messages: Bucket {
            burst: SMTP_RATE_MESSAGE_BURST,
            interval_ms: SMTP_RATE_MESSAGE_MS,
        },
        blocks: Bucket {
            burst: SMTP_RATE_BLOCK_BURST,
            interval_ms: SMTP_RATE_BLOCK_MS,
        },
    };
}

/// One accepted message, charged once however many local accounts it
/// reached: each account's intake from the peer is bounded alike, and a
/// list post to several accounts does not spend the budget several times.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Charge {
    messages: u64,
    blocks: u64,
}
impl Charge {
    pub fn accepted(bytes: usize) -> Self {
        Self {
            messages: 1,
            blocks: u64::try_from(bytes.div_ceil(SMTP_RATE_BLOCK)).unwrap_or(u64::MAX),
        }
    }
}

/// Count one MAIL refused for rate; eviction counts as unavailable.
pub fn refused() {
    counter(crate::performance::Outcome::Busy);
}
fn counter(outcome: crate::performance::Outcome) {
    crate::performance::record(
        crate::performance::Phase::SmtpRateRefused,
        std::time::Duration::ZERO,
        outcome,
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Charged {
    Recorded,
    /// Another key's debt was forgotten to remember this one.
    Evicted,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    key: PeerKey,
    messages: u64,
    blocks: u64,
}
impl Entry {
    fn settled(self, now: u64) -> bool {
        self.messages <= now && self.blocks <= now
    }
}

/// Fixed table owned by the receiving network loop; it never allocates.
pub struct PeerRates {
    policy: Policy,
    entries: [Option<Entry>; SMTP_RATE_PEERS],
}
impl PeerRates {
    pub const fn new(policy: Policy) -> Self {
        Self {
            policy,
            entries: [None; SMTP_RATE_PEERS],
        }
    }
    pub fn limited(&self, key: PeerKey, now: Tick) -> bool {
        self.entries
            .iter()
            .flatten()
            .find(|entry| entry.key == key)
            .is_some_and(|entry| self.throttled(*entry, now.0))
    }
    fn throttled(&self, entry: Entry, now: u64) -> bool {
        self.policy.messages.limited(entry.messages, now)
            || self.policy.blocks.limited(entry.blocks, now)
    }
    /// Eviction order: unthrottled keys before throttled ones, then the
    /// nearest to admitting (remaining debt, or remaining refusal time).
    fn standing(&self, entry: Entry, now: u64) -> (bool, u64) {
        if self.throttled(entry, now) {
            let messages = self.policy.messages.excess(entry.messages, now);
            (
                true,
                messages.max(self.policy.blocks.excess(entry.blocks, now)),
            )
        } else {
            (false, entry.messages.max(entry.blocks).saturating_sub(now))
        }
    }
    /// A full table forgets a key rather than refusing mail: failing closed
    /// would let anyone with many addresses stop receiving for everyone.
    pub fn charge(&mut self, key: PeerKey, charge: Charge, now: Tick) -> Charged {
        let now = now.0;
        let mut outcome = Charged::Recorded;
        let known = self
            .entries
            .iter()
            .position(|entry| entry.is_some_and(|entry| entry.key == key));
        let position = match known {
            Some(position) => position,
            None => {
                let free = self
                    .entries
                    .iter()
                    .position(|entry| entry.is_none_or(|entry| entry.settled(now)));
                let position = free.unwrap_or_else(|| {
                    outcome = Charged::Evicted;
                    counter(crate::performance::Outcome::Unavailable);
                    self.entries
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, entry)| {
                            entry.map_or((false, 0), |entry| self.standing(entry, now))
                        })
                        .map_or(0, |(position, _)| position)
                });
                if let Some(entry) = self.entries.get_mut(position) {
                    *entry = Some(Entry {
                        key,
                        messages: now,
                        blocks: now,
                    });
                }
                position
            }
        };
        if let Some(Some(entry)) = self.entries.get_mut(position) {
            entry.messages = self
                .policy
                .messages
                .charge(entry.messages, now, charge.messages);
            entry.blocks = self.policy.blocks.charge(entry.blocks, now, charge.blocks);
        }
        outcome
    }
}

#[cfg(test)]
#[path = "smtp_rate_tests.rs"]
mod tests;
