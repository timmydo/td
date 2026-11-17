#![allow(clippy::unwrap_used)]
use super::*;
use std::net::{Ipv4Addr, Ipv6Addr};

const SMALL: Policy = Policy {
    messages: Bucket {
        burst: 3,
        interval_ms: 1_000,
    },
    blocks: Bucket {
        burst: 10,
        interval_ms: 100,
    },
};

fn v4(last: u8) -> PeerKey {
    PeerKey::from(IpAddr::V4(Ipv4Addr::new(192, 0, 2, last)))
}

fn one() -> Charge {
    Charge::accepted(1)
}

fn evictions() -> u64 {
    crate::performance::snapshot(crate::performance::Phase::SmtpRateRefused)
        .unwrap()
        .unavailable
}

#[test]
fn burst_is_admitted_then_limited_until_one_interval_repays() {
    let mut rates = PeerRates::new(SMALL);
    let peer = v4(1);
    for _ in 0..3 {
        assert!(!rates.limited(peer, Tick(10)));
        assert_eq!(rates.charge(peer, one(), Tick(10)), Charged::Recorded);
    }
    assert!(rates.limited(peer, Tick(10)));
    assert!(rates.limited(peer, Tick(1_009)));
    assert!(!rates.limited(peer, Tick(1_010)));
    rates.charge(peer, one(), Tick(1_010));
    assert!(rates.limited(peer, Tick(1_010)));
    assert!(!rates.limited(v4(2), Tick(10)));
}

#[test]
fn idle_time_restores_only_the_burst() {
    let mut rates = PeerRates::new(SMALL);
    let peer = v4(1);
    rates.charge(peer, one(), Tick(0));
    for _ in 0..3 {
        assert!(!rates.limited(peer, Tick(1_000_000)));
        rates.charge(peer, one(), Tick(1_000_000));
    }
    assert!(rates.limited(peer, Tick(1_000_000)));
}

#[test]
fn bytes_limit_in_whole_blocks_once_per_message() {
    assert_eq!(
        Charge::accepted(SMTP_RATE_BLOCK + 1),
        Charge {
            messages: 1,
            blocks: 2
        }
    );
    assert_eq!(
        Charge::accepted(0),
        Charge {
            messages: 1,
            blocks: 0
        }
    );
    let mut rates = PeerRates::new(SMALL);
    let peer = v4(1);
    rates.charge(peer, Charge::accepted(9 * SMTP_RATE_BLOCK), Tick(0));
    assert!(!rates.limited(peer, Tick(0)));
    rates.charge(peer, Charge::accepted(1), Tick(0));
    assert!(rates.limited(peer, Tick(0)));
    assert!(!rates.limited(peer, Tick(100)));
    // One message over the burst is accepted into debt and then refused.
    let mut rates = PeerRates::new(SMALL);
    rates.charge(peer, Charge::accepted(32 * SMTP_RATE_BLOCK), Tick(0));
    assert!(rates.limited(peer, Tick(0)));
    assert!(rates.limited(peer, Tick(2_299)));
    assert!(!rates.limited(peer, Tick(2_300)));
}

#[test]
fn keys_are_exact_ipv4_and_ipv6_slash_64() {
    let a = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, 1));
    let b = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0xffff, 1, 2, 3));
    let c = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 3, 0, 0, 0, 1));
    assert_eq!(PeerKey::from(a), PeerKey::from(b));
    assert_ne!(PeerKey::from(a), PeerKey::from(c));
    assert_ne!(v4(1), v4(2));
    let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 1).to_ipv6_mapped());
    assert_eq!(PeerKey::from(mapped), v4(1));
    let mut rates = PeerRates::new(SMALL);
    for _ in 0..3 {
        rates.charge(PeerKey::from(a), one(), Tick(0));
    }
    assert!(rates.limited(PeerKey::from(b), Tick(0)));
    assert!(!rates.limited(PeerKey::from(c), Tick(0)));
}

#[test]
fn settled_entries_are_reused_before_any_debt_is_forgotten() {
    let mut rates = PeerRates::new(SMALL);
    for index in 0..SMTP_RATE_PEERS {
        let key = PeerKey::V6(index as u64);
        assert_eq!(rates.charge(key, one(), Tick(0)), Charged::Recorded);
    }
    // Every entry is repaid by 1,000 ms; a new key takes one without eviction.
    assert_eq!(
        rates.charge(PeerKey::V6(9_999), one(), Tick(1_000)),
        Charged::Recorded
    );
}

#[test]
fn a_full_table_forgets_unthrottled_keys_before_throttled_ones() {
    // Generous blocks let unthrottled keys owe far longer than the
    // throttled key's refusal lasts; they are still forgotten first.
    let policy = Policy {
        messages: SMALL.messages,
        blocks: Bucket {
            burst: 1_000,
            interval_ms: 100,
        },
    };
    let mut rates = PeerRates::new(policy);
    let heavy = PeerKey::V6(0);
    for _ in 0..3 {
        rates.charge(heavy, one(), Tick(0));
    }
    for index in 1..SMTP_RATE_PEERS {
        let key = PeerKey::V6(index as u64);
        rates.charge(key, Charge::accepted(500 * SMTP_RATE_BLOCK), Tick(0));
        assert!(!rates.limited(key, Tick(1)));
    }
    let before = evictions();
    for index in 0..SMTP_RATE_PEERS {
        assert_eq!(
            rates.charge(PeerKey::V6(10_000 + index as u64), one(), Tick(1)),
            Charged::Evicted
        );
        assert!(rates.limited(heavy, Tick(1)));
    }
    assert!(evictions() >= before + SMTP_RATE_PEERS as u64);
}

#[test]
fn an_all_throttled_table_forgets_the_key_nearest_admission() {
    let mut rates = PeerRates::new(SMALL);
    for index in 0..SMTP_RATE_PEERS {
        let key = PeerKey::V6(index as u64);
        let messages = if index == 7 { 4 } else { 5 };
        for _ in 0..messages {
            rates.charge(key, one(), Tick(0));
        }
        assert!(rates.limited(key, Tick(1)));
    }
    assert_eq!(
        rates.charge(PeerKey::V6(9_999), one(), Tick(1)),
        Charged::Evicted
    );
    assert!(!rates.limited(PeerKey::V6(7), Tick(1)));
    for index in (0..SMTP_RATE_PEERS).filter(|index| *index != 7) {
        assert!(rates.limited(PeerKey::V6(index as u64), Tick(1)));
    }
}

#[test]
fn saturating_debt_stays_limited_without_overflow() {
    let steep = Policy {
        messages: SMALL.messages,
        blocks: Bucket {
            burst: 1,
            interval_ms: u64::MAX,
        },
    };
    let mut rates = PeerRates::new(steep);
    let peer = v4(1);
    rates.charge(peer, Charge::accepted(usize::MAX), Tick(5));
    rates.charge(peer, Charge::accepted(usize::MAX), Tick(6));
    assert!(rates.limited(peer, Tick(u64::MAX - 1)));
    let wide = Policy {
        messages: Bucket {
            burst: u64::MAX,
            interval_ms: u64::MAX,
        },
        blocks: SMALL.blocks,
    };
    let mut rates = PeerRates::new(wide);
    rates.charge(peer, one(), Tick(u64::MAX));
    assert!(!rates.limited(peer, Tick(u64::MAX)));
}

#[test]
fn default_policy_admits_list_bursts_and_bounds_one_large_message() {
    let mut rates = PeerRates::new(Policy::DEFAULT);
    let peer = v4(1);
    for _ in 0..100 {
        assert!(!rates.limited(peer, Tick(0)));
        rates.charge(peer, Charge::accepted(200 * 1024), Tick(0));
    }
    assert!(rates.limited(peer, Tick(0)));
    assert!(!rates.limited(peer, Tick(SMTP_RATE_MESSAGE_MS)));
    // The largest message leaves a fresh peer admitted, and from an
    // exhausted block balance refuses for its own refill time only.
    let largest = Charge::accepted(crate::limits::MAX_MESSAGE_BYTES);
    let mut rates = PeerRates::new(Policy::DEFAULT);
    rates.charge(peer, largest, Tick(0));
    assert!(!rates.limited(peer, Tick(0)));
    let mut rates = PeerRates::new(Policy::DEFAULT);
    let full = Charge::accepted((SMTP_RATE_BLOCK_BURST as usize - 1) * SMTP_RATE_BLOCK);
    rates.charge(peer, full, Tick(0));
    rates.charge(peer, largest, Tick(0));
    let blocks = crate::limits::MAX_MESSAGE_BYTES.div_ceil(SMTP_RATE_BLOCK) as u64;
    assert!(rates.limited(peer, Tick(blocks * SMTP_RATE_BLOCK_MS - 1)));
    assert!(!rates.limited(peer, Tick(blocks * SMTP_RATE_BLOCK_MS)));
}

#[test]
fn table_stays_small() {
    assert!(std::mem::size_of::<PeerRates>() <= 10 * 1024);
}
