//! Dedicated process: no libtest worker or concurrent harness allocations.
#![cfg(test)]
#![deny(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "support/allocation_counter.rs"]
mod allocation_counter;
#[path = "support/allocation_registry.rs"]
mod allocation_registry;
#[path = "support/allocation_shim.rs"]
mod allocation_shim;

#[path = "support/tls_allocation_scenario.rs"]
mod tls_allocation_scenario;

use allocation_shim::TD_MTA_ALLOCATION_COUNTERS as COUNTERS;
use std::hint::black_box;
use td_crypto::Digest;

fn forwarding() {
    let before = COUNTERS.snapshot();
    let mut bytes = Vec::<u8>::with_capacity(black_box(16));
    bytes.resize(16, 0x5a);
    black_box(&mut bytes);
    bytes.reserve_exact(48);
    bytes.truncate(8);
    bytes.shrink_to_fit();
    assert_eq!(bytes.as_slice(), &[0x5a; 8]);
    // Valid u8 layout, deliberately beyond the runner's address/data ceiling.
    assert!(bytes.try_reserve_exact(isize::MAX as usize - 8).is_err());
    assert_eq!(bytes.as_slice(), &[0x5a; 8]);
    drop(bytes);
    let zeroes = vec![0u8; black_box(37)];
    assert!(black_box(&zeroes).iter().all(|&b| b == 0));
    drop(zeroes);
    #[repr(align(4096))]
    struct Aligned([u8; 4096]);
    let aligned_before = COUNTERS.snapshot();
    let aligned = Box::new(Aligned([7; 4096]));
    assert_eq!(black_box(&*aligned) as *const Aligned as usize % 4096, 0);
    assert_eq!(black_box(&aligned.0).first(), Some(&7));
    let aligned_live = COUNTERS.snapshot();
    assert_eq!(aligned_live.alloc, aligned_before.alloc + 1);
    assert_eq!(
        aligned_live.live,
        aligned_before.live + std::mem::size_of::<Aligned>()
    );
    drop(aligned);
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(after.live, before.live);
    assert!(after.alloc > before.alloc);
    assert!(after.zeroed > before.zeroed);
    assert!(after.realloc >= before.realloc + 3);
    assert!(after.failed > before.failed);
    assert!(after.free > before.free);
    assert!(after.peak >= before.live + 4096);
}

fn hot_paths() {
    let mut scratch = [0; 512];
    let data = [0x5a; 4096];
    let registry = allocation_registry::Registry::<4>::new();
    let before = COUNTERS.snapshot();
    for _ in 0..64 {
        registry.insert(64, 0).unwrap();
        registry.insert(128, 17).unwrap();
        assert_eq!(registry.remove(64), Ok(0));
        assert_eq!(registry.remove(128), Ok(17));
        assert_eq!(registry.snapshot().bytes, 0);
        assert!(!registry.snapshot().invalid);
        let mut digest = td_crypto::Sha256::try_new().unwrap();
        digest.update(black_box(&data)).unwrap();
        black_box(digest.finish().unwrap());
        let mut line = td_mta::smtp_wire::LineReader::new(&mut scratch).unwrap();
        assert!(!line.feed(black_box(b"EHLO example")).unwrap().complete);
        assert!(line.feed(black_box(b".test\r\n")).unwrap().complete);
        assert_eq!(line.line(), Some(b"EHLO example.test".as_slice()));
        line.advance().unwrap();
        assert!(line.feed(black_box(b"bad\n")).is_err());
        assert!(line.feed(black_box(b"NOOP\r\n")).is_err());
        assert!(td_mta::smtp_wire::LineReader::new(&mut []).is_err());
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid);
    assert_eq!(
        after, before,
        "Rust allocator activity in measured hot paths"
    );
}

fn tls_clients() {
    let mut samples = [COUNTERS.snapshot(); tls_allocation_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_allocation_scenario::run(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    for sample in &samples {
        assert!(!sample.invalid);
    }
    assert_eq!(samples.get(3), samples.get(4), "reservation allocated");
    assert_eq!(samples.get(7), samples.get(8), "capacity refusal allocated");
    assert_eq!(
        samples.get(9).unwrap().live,
        samples.get(10).unwrap().live,
        "warm construction retained Rust bytes"
    );
    for (phase, s) in tls_allocation_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-client-allocation-v1: rust passed");
}

#[path = "support/tls_handshake_scenario.rs"]
mod tls_handshake_scenario;

fn tls_handshake() {
    let mut samples = [COUNTERS.snapshot(); tls_handshake_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_handshake_scenario::run(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    for sample in &samples {
        assert!(!sample.invalid);
    }
    assert_eq!(
        samples.get(7).unwrap().live,
        samples.get(8).unwrap().live,
        "warm records retained Rust bytes"
    );
    assert_eq!(
        samples
            .get(10)
            .unwrap()
            .live
            .checked_sub(samples.get(11).unwrap().live),
        Some(4 * td_mta::tls_io::TLS_WIRE_BYTES),
        "returned wire buffers did not release their requested bytes"
    );
    for (phase, s) in tls_handshake_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust-handshake {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-handshake-allocation-v2: rust passed");
}

fn tls_large_chain() {
    let mut samples = [COUNTERS.snapshot(); tls_handshake_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_handshake_scenario::run_large(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    for sample in &samples {
        assert!(!sample.invalid);
    }
    assert_eq!(
        samples.get(7).unwrap().live,
        samples.get(8).unwrap().live,
        "warm records retained Rust bytes"
    );
    assert_eq!(
        samples
            .get(10)
            .unwrap()
            .live
            .checked_sub(samples.get(11).unwrap().live),
        Some(4 * td_mta::tls_io::TLS_WIRE_BYTES),
        "returned wire buffers did not release their requested bytes"
    );
    for (phase, s) in tls_handshake_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust-large-chain {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-large-chain-allocation-v2: rust passed");
}

#[path = "support/tls_fragment_scenario.rs"]
mod tls_fragment_scenario;

fn tls_fragments() {
    let mut samples = [COUNTERS.snapshot(); tls_fragment_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_fragment_scenario::run(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    for sample in &samples {
        assert!(!sample.invalid);
    }
    assert_eq!(
        samples.get(9).unwrap().live,
        samples.get(10).unwrap().live,
        "repeated refusals retained Rust bytes"
    );
    for (phase, s) in tls_fragment_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust-fragment {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-fragment-allocation-v1: rust passed");
}

fn tls_certificate_list() {
    let mut samples = [COUNTERS.snapshot(); tls_fragment_scenario::CERTIFICATE_LIST_PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_fragment_scenario::run_certificate_list(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    for sample in &samples {
        assert!(!sample.invalid);
    }
    assert!(
        samples
            .get(5)
            .unwrap()
            .peak
            .checked_sub(samples.get(4).unwrap().peak)
            .unwrap()
            >= 512 * 1024,
        "certificate list did not exercise decoded-entry allocation"
    );
    assert!(
        samples
            .get(9)
            .unwrap()
            .peak
            .checked_sub(samples.get(1).unwrap().live)
            .unwrap()
            <= td_mta::limits::TLS_SESSION_BYTES + td_mta::limits::TLS_HANDSHAKE_BYTES,
        "certificate-list processing exceeds planned requested-byte allowance"
    );
    assert_eq!(
        samples.get(8).unwrap().live,
        samples.get(9).unwrap().live,
        "repeated refusals retained Rust bytes"
    );
    for (phase, s) in tls_fragment_scenario::CERTIFICATE_LIST_PHASES
        .into_iter()
        .zip(samples)
    {
        println!(
            "tls-rust-certificate-list {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-certificate-list-allocation-v1: rust passed");
}

#[path = "support/entropy_worker_scenario.rs"]
mod entropy_worker_scenario;

fn entropy_workers() {
    let mut samples = [COUNTERS.snapshot(); entropy_worker_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    entropy_worker_scenario::run(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    for sample in &samples {
        assert!(!sample.invalid);
    }
    assert_eq!(samples.get(3), samples.get(4), "warm entropy allocated");
    for (phase, s) in entropy_worker_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust-entropy {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-entropy-allocation-v1: rust passed");
}

fn main() {
    allocation_counter::Counters::verify_model();
    forwarding();
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-clients")
    {
        tls_clients();
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-handshake")
    {
        tls_handshake();
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--entropy-workers")
    {
        entropy_workers();
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-certificate-list")
    {
        tls_certificate_list();
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-fragments")
    {
        tls_fragments();
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-remote-chain")
    {
        tls_remote_chain();
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-generation-trust")
    {
        tls_generations(tls_generation_scenario::Scenario::Trust);
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-generation-routing")
    {
        tls_generations(tls_generation_scenario::Scenario::Routing);
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-generations")
    {
        tls_generations(tls_generation_scenario::Scenario::Ordinary);
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-large-chain")
    {
        tls_large_chain();
        return;
    }
    hot_paths();
    println!("rust-allocation-probe-v1: counter-model forwarding hot-paths passed");
}

#[path = "support/tls_generation_scenario.rs"]
mod tls_generation_scenario;

fn tls_generations(scenario: tls_generation_scenario::Scenario) {
    let label = scenario.label();
    let mut samples = [COUNTERS.snapshot(); tls_generation_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_generation_scenario::run(scenario, || *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|s| !s.invalid));
    let retained = samples
        .get(4)
        .unwrap()
        .live
        .checked_sub(samples.get(3).unwrap().live)
        .unwrap();
    assert!(
        samples
            .get(3)
            .unwrap()
            .live
            .checked_sub(samples.get(2).unwrap().live)
            .unwrap()
            <= td_mta::limits::TLS_GENERATION_BYTES,
        "first generation exceeds planned requested-byte allowance"
    );
    assert!(
        retained <= td_mta::limits::TLS_GENERATION_BYTES,
        "candidate exceeds planned requested-byte allowance"
    );
    assert!(
        samples
            .get(8)
            .unwrap()
            .peak
            .checked_sub(samples.get(2).unwrap().live)
            .unwrap()
            <= 2 * td_mta::limits::TLS_GENERATION_BYTES,
        "generation overlap exceeds planned requested-byte allowance"
    );
    for (peak, baseline) in [(3, 2), (4, 3), (8, 3)] {
        assert!(
            samples
                .get(peak)
                .unwrap()
                .peak
                .checked_sub(samples.get(baseline).unwrap().live)
                .unwrap()
                <= td_mta::limits::TLS_GENERATION_BYTES,
            "generation construction exceeds its own planned allowance"
        );
    }
    if scenario == tls_generation_scenario::Scenario::Routing {
        assert!(
            retained <= 1024 * 1024,
            "large routing candidate exceeds retained-byte fixture allowance"
        );
    }
    assert_eq!(
        samples.get(5),
        samples.get(6),
        "generation refusal allocated"
    );
    assert_eq!(
        samples.get(3).unwrap().live,
        samples.get(7).unwrap().live,
        "old generation release retained Rust bytes"
    );
    assert_eq!(
        samples.get(7).unwrap().live,
        samples.get(8).unwrap().live,
        "replacement retained Rust bytes"
    );
    for (phase, s) in tls_generation_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust-{label} {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-{label}-allocation-v1: rust passed");
}

#[path = "support/tls_remote_chain_scenario.rs"]
mod tls_remote_chain_scenario;

fn tls_remote_chain() {
    let mut samples = [COUNTERS.snapshot(); tls_remote_chain_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_remote_chain_scenario::run(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|s| !s.invalid));
    assert_eq!(
        samples.get(6).unwrap().live,
        samples.get(7).unwrap().live,
        "remote records retained Rust bytes"
    );
    assert_eq!(
        samples
            .get(8)
            .unwrap()
            .live
            .checked_sub(samples.get(9).unwrap().live),
        Some(2 * td_mta::tls_io::TLS_WIRE_BYTES)
    );
    assert!(
        samples
            .get(6)
            .unwrap()
            .live
            .checked_sub(samples.get(2).unwrap().live)
            .unwrap()
            <= td_mta::limits::TLS_SESSION_BYTES,
        "remote session exceeds planned requested-byte allowance"
    );
    let processing =
        td_mta::limits::TLS_HANDSHAKE_BYTES.max(td_mta::limits::TLS_ESTABLISHED_PROCESSING_BYTES);
    assert!(
        samples
            .get(7)
            .unwrap()
            .peak
            .checked_sub(samples.get(2).unwrap().live)
            .unwrap()
            <= td_mta::limits::TLS_SESSION_BYTES + processing,
        "remote processing exceeds planned requested-byte allowance"
    );
    assert!(
        samples
            .get(7)
            .unwrap()
            .peak
            .checked_sub(samples.get(5).unwrap().live)
            .unwrap()
            <= td_mta::limits::TLS_ESTABLISHED_PROCESSING_BYTES,
        "established processing exceeds its own planned allowance"
    );
    let scenario = tls_remote_chain_scenario::label();
    for (phase, s) in tls_remote_chain_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust-{scenario} {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-{scenario}-allocation-v1: rust passed");
}
