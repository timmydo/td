//! Native allocator diagnostics are qualified only by the isolated musl link.
#![cfg(test)]
#![deny(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]

#[cfg(all(
    td_native_alloc_probe,
    not(all(target_os = "linux", target_arch = "x86_64", target_env = "musl"))
))]
compile_error!("native allocation qualification requires Linux x86-64 musl");

#[cfg(td_native_alloc_probe)]
#[path = "support/allocation_registry.rs"]
mod allocation_registry;
#[cfg(td_native_alloc_probe)]
#[path = "support/native_allocation_controls.rs"]
mod native_allocation_controls;
#[cfg(td_native_alloc_probe)]
#[path = "support/native_allocator_bridge.rs"]
mod native_allocator_bridge;

#[cfg(td_native_alloc_probe)]
#[path = "support/tls_allocation_scenario.rs"]
mod tls_allocation_scenario;

#[cfg(td_native_alloc_probe)]
fn tls_clients() {
    use native_allocator_bridge::{calls, TD_MTA_NATIVE_REGISTRY as REGISTRY};
    let mut samples = [(calls(), REGISTRY.snapshot()); tls_allocation_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_allocation_scenario::run(|| *slots.next().unwrap() = (calls(), REGISTRY.snapshot()));
    assert!(slots.next().is_none());
    for (_, snapshot) in &samples {
        assert!(!snapshot.invalid);
    }
    assert_eq!(samples.get(3), samples.get(4), "reservation allocated");
    assert_eq!(samples.get(7), samples.get(8), "capacity refusal allocated");
    assert_eq!(
        samples.get(9).unwrap().1.bytes,
        samples.get(10).unwrap().1.bytes,
        "warm construction retained C boundary bytes"
    );
    assert_eq!(
        samples.get(9).unwrap().1.blocks,
        samples.get(10).unwrap().1.blocks,
        "warm construction retained C boundary blocks"
    );
    for (phase, (c, s)) in tls_allocation_scenario::PHASES.into_iter().zip(samples) {
        let [malloc, calloc, realloc, free, posix, aligned] = c;
        println!(
            "tls-native {phase} {malloc} {calloc} {realloc} {free} {posix} {aligned} {} {} {}",
            s.blocks, s.bytes, s.peak
        );
    }
    println!("tls-client-allocation-v1: native passed");
}

#[cfg(td_native_alloc_probe)]
#[path = "support/tls_handshake_scenario.rs"]
mod tls_handshake_scenario;

#[cfg(td_native_alloc_probe)]
fn tls_handshake() {
    use native_allocator_bridge::{calls, TD_MTA_NATIVE_REGISTRY as REGISTRY};
    let mut samples = [(calls(), REGISTRY.snapshot()); tls_handshake_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_handshake_scenario::run(|| *slots.next().unwrap() = (calls(), REGISTRY.snapshot()));
    assert!(slots.next().is_none());
    for (_, snapshot) in &samples {
        assert!(!snapshot.invalid);
    }
    assert_eq!(
        samples.get(7).unwrap().1.bytes,
        samples.get(8).unwrap().1.bytes,
        "warm records retained C boundary bytes"
    );
    assert_eq!(
        samples.get(7).unwrap().1.blocks,
        samples.get(8).unwrap().1.blocks,
        "warm records retained C boundary blocks"
    );
    assert_eq!(
        samples
            .get(10)
            .unwrap()
            .1
            .bytes
            .checked_sub(samples.get(11).unwrap().1.bytes),
        Some(4 * td_mta::tls_io::TLS_WIRE_BYTES),
        "returned wire buffers did not release their requested bytes"
    );
    assert_eq!(
        samples
            .get(10)
            .unwrap()
            .1
            .blocks
            .checked_sub(samples.get(11).unwrap().1.blocks),
        Some(4),
        "returned wire buffers did not release four tracked blocks"
    );
    for (phase, (c, s)) in tls_handshake_scenario::PHASES.into_iter().zip(samples) {
        let [malloc, calloc, realloc, free, posix, aligned] = c;
        println!("tls-native-handshake {phase} {malloc} {calloc} {realloc} {free} {posix} {aligned} {} {} {}", s.blocks, s.bytes, s.peak);
    }
    println!("tls-handshake-allocation-v2: native passed");
}

#[cfg(td_native_alloc_probe)]
fn tls_large_chain() {
    use native_allocator_bridge::{calls, TD_MTA_NATIVE_REGISTRY as REGISTRY};
    let mut samples = [(calls(), REGISTRY.snapshot()); tls_handshake_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_handshake_scenario::run_large(|| *slots.next().unwrap() = (calls(), REGISTRY.snapshot()));
    assert!(slots.next().is_none());
    for (_, snapshot) in &samples {
        assert!(!snapshot.invalid);
    }
    assert_eq!(
        samples.get(7).unwrap().1.bytes,
        samples.get(8).unwrap().1.bytes,
        "warm records retained C boundary bytes"
    );
    assert_eq!(
        samples.get(7).unwrap().1.blocks,
        samples.get(8).unwrap().1.blocks,
        "warm records retained C boundary blocks"
    );
    assert_eq!(
        samples
            .get(10)
            .unwrap()
            .1
            .bytes
            .checked_sub(samples.get(11).unwrap().1.bytes),
        Some(4 * td_mta::tls_io::TLS_WIRE_BYTES),
        "returned wire buffers did not release their requested bytes"
    );
    assert_eq!(
        samples
            .get(10)
            .unwrap()
            .1
            .blocks
            .checked_sub(samples.get(11).unwrap().1.blocks),
        Some(4),
        "returned wire buffers did not release four tracked blocks"
    );
    for (phase, (c, s)) in tls_handshake_scenario::PHASES.into_iter().zip(samples) {
        let [malloc, calloc, realloc, free, posix, aligned] = c;
        println!("tls-native-large-chain {phase} {malloc} {calloc} {realloc} {free} {posix} {aligned} {} {} {}", s.blocks, s.bytes, s.peak);
    }
    println!("tls-large-chain-allocation-v2: native passed");
}

#[cfg(td_native_alloc_probe)]
#[path = "support/tls_fragment_scenario.rs"]
mod tls_fragment_scenario;

#[cfg(td_native_alloc_probe)]
fn tls_fragments() {
    use native_allocator_bridge::{calls, TD_MTA_NATIVE_REGISTRY as REGISTRY};
    let mut samples = [(calls(), REGISTRY.snapshot()); tls_fragment_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_fragment_scenario::run(|| *slots.next().unwrap() = (calls(), REGISTRY.snapshot()));
    assert!(slots.next().is_none());
    for (_, snapshot) in &samples {
        assert!(!snapshot.invalid);
    }
    assert_eq!(
        samples.get(9).unwrap().1.bytes,
        samples.get(10).unwrap().1.bytes,
        "repeated refusals retained C boundary bytes"
    );
    assert_eq!(
        samples.get(9).unwrap().1.blocks,
        samples.get(10).unwrap().1.blocks,
        "repeated refusals retained C boundary blocks"
    );
    for (phase, (c, s)) in tls_fragment_scenario::PHASES.into_iter().zip(samples) {
        let [malloc, calloc, realloc, free, posix, aligned] = c;
        println!("tls-native-fragment {phase} {malloc} {calloc} {realloc} {free} {posix} {aligned} {} {} {}", s.blocks, s.bytes, s.peak);
    }
    println!("tls-fragment-allocation-v1: native passed");
}

#[cfg(td_native_alloc_probe)]
fn tls_certificate_list() {
    use native_allocator_bridge::{calls, TD_MTA_NATIVE_REGISTRY as REGISTRY};
    let mut samples =
        [(calls(), REGISTRY.snapshot()); tls_fragment_scenario::CERTIFICATE_LIST_PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_fragment_scenario::run_certificate_list(|| {
        *slots.next().unwrap() = (calls(), REGISTRY.snapshot())
    });
    assert!(slots.next().is_none());
    for (_, snapshot) in &samples {
        assert!(!snapshot.invalid);
    }
    assert!(
        samples
            .get(5)
            .unwrap()
            .1
            .peak
            .checked_sub(samples.get(4).unwrap().1.peak)
            .unwrap()
            >= 512 * 1024,
        "certificate list did not exercise decoded-entry allocation"
    );
    assert!(
        samples
            .get(9)
            .unwrap()
            .1
            .peak
            .checked_sub(samples.get(1).unwrap().1.bytes)
            .unwrap()
            <= td_mta::limits::TLS_SESSION_BYTES + td_mta::limits::TLS_HANDSHAKE_BYTES,
        "certificate-list processing exceeds planned requested-byte allowance"
    );
    assert_eq!(
        samples.get(8).unwrap().1.bytes,
        samples.get(9).unwrap().1.bytes,
        "repeated refusals retained C boundary bytes"
    );
    assert_eq!(
        samples.get(8).unwrap().1.blocks,
        samples.get(9).unwrap().1.blocks,
        "repeated refusals retained C boundary blocks"
    );
    for (phase, (c, s)) in tls_fragment_scenario::CERTIFICATE_LIST_PHASES
        .into_iter()
        .zip(samples)
    {
        let [malloc, calloc, realloc, free, posix, aligned] = c;
        println!("tls-native-certificate-list {phase} {malloc} {calloc} {realloc} {free} {posix} {aligned} {} {} {}", s.blocks, s.bytes, s.peak);
    }
    println!("tls-certificate-list-allocation-v1: native passed");
}

#[cfg(td_native_alloc_probe)]
#[path = "support/entropy_worker_scenario.rs"]
mod entropy_worker_scenario;

#[cfg(td_native_alloc_probe)]
fn entropy_workers() {
    use native_allocator_bridge::{calls, TD_MTA_NATIVE_REGISTRY as REGISTRY};
    let mut samples = [(calls(), REGISTRY.snapshot()); entropy_worker_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    entropy_worker_scenario::run(|| *slots.next().unwrap() = (calls(), REGISTRY.snapshot()));
    assert!(slots.next().is_none());
    for (_, snapshot) in &samples {
        assert!(!snapshot.invalid);
    }
    assert_eq!(samples.get(3), samples.get(4), "warm entropy allocated");
    assert!(
        samples
            .get(1)
            .unwrap()
            .0
            .into_iter()
            .zip(samples.get(2).unwrap().0)
            .take(2)
            .any(|(before, after)| after > before),
        "cold entropy did not allocate"
    );
    for (phase, (c, s)) in entropy_worker_scenario::PHASES.into_iter().zip(samples) {
        let [malloc, calloc, realloc, free, posix, aligned] = c;
        println!("tls-native-entropy {phase} {malloc} {calloc} {realloc} {free} {posix} {aligned} {} {} {}", s.blocks, s.bytes, s.peak);
    }
    println!("tls-entropy-allocation-v1: native passed");
}

#[cfg(td_native_alloc_probe)]
fn main() {
    use td_crypto::Entropy;
    let zero_resize = std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--zero-resize");
    native_allocation_controls::run(zero_resize);
    if zero_resize {
        println!("native-allocation-probe-v1: zero-resize invalidated");
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--sqlite-body")
    {
        sqlite_body();
        return;
    }
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
    // Retain actual provider allocation/RNG code for the final symbol audit.
    let calls_before = native_allocator_bridge::calls();
    let mut entropy = td_crypto::SystemEntropy::try_new().unwrap();
    entropy.fill(&mut [0; 32]).unwrap();
    assert!(
        !native_allocator_bridge::TD_MTA_NATIVE_REGISTRY
            .snapshot()
            .invalid
    );
    assert!(calls_before
        .into_iter()
        .zip(native_allocator_bridge::calls())
        .take(2)
        .any(|(before, after)| after > before));
    let [registry, counters, thread_flag] = native_allocator_bridge::instrumentation_storage();
    println!("native_registry_storage_bytes={registry}\nnative_counter_storage_bytes={counters}\nnative_thread_flag_bytes={thread_flag}\nnative-allocation-probe-v1: forwarding provider diagnostic passed");
}

#[cfg(not(td_native_alloc_probe))]
fn main() {
    println!("native allocation probe unqualified: use the isolated musl build");
}

#[cfg(td_native_alloc_probe)]
#[path = "support/tls_generation_scenario.rs"]
mod tls_generation_scenario;

#[cfg(td_native_alloc_probe)]
fn tls_generations(scenario: tls_generation_scenario::Scenario) {
    use native_allocator_bridge::{calls, TD_MTA_NATIVE_REGISTRY as REGISTRY};
    let label = scenario.label();
    let mut samples = [(calls(), REGISTRY.snapshot()); tls_generation_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_generation_scenario::run(scenario, || {
        *slots.next().unwrap() = (calls(), REGISTRY.snapshot())
    });
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|(_, s)| !s.invalid));
    let retained = samples
        .get(4)
        .unwrap()
        .1
        .bytes
        .checked_sub(samples.get(3).unwrap().1.bytes)
        .unwrap();
    assert!(
        samples
            .get(3)
            .unwrap()
            .1
            .bytes
            .checked_sub(samples.get(2).unwrap().1.bytes)
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
            .1
            .peak
            .checked_sub(samples.get(2).unwrap().1.bytes)
            .unwrap()
            <= 2 * td_mta::limits::TLS_GENERATION_BYTES,
        "generation overlap exceeds planned requested-byte allowance"
    );
    for (peak, baseline) in [(3, 2), (4, 3), (8, 3)] {
        assert!(
            samples
                .get(peak)
                .unwrap()
                .1
                .peak
                .checked_sub(samples.get(baseline).unwrap().1.bytes)
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
        samples.get(3).unwrap().1.bytes,
        samples.get(7).unwrap().1.bytes,
        "old generation release retained C boundary bytes"
    );
    assert_eq!(
        samples.get(3).unwrap().1.blocks,
        samples.get(7).unwrap().1.blocks,
        "old generation release retained C boundary blocks"
    );
    assert_eq!(
        samples.get(7).unwrap().1.bytes,
        samples.get(8).unwrap().1.bytes,
        "replacement retained C boundary bytes"
    );
    assert_eq!(
        samples.get(7).unwrap().1.blocks,
        samples.get(8).unwrap().1.blocks,
        "replacement retained C boundary blocks"
    );
    for (phase, (c, s)) in tls_generation_scenario::PHASES.into_iter().zip(samples) {
        let [malloc, calloc, realloc, free, posix, aligned] = c;
        println!("tls-native-{label} {phase} {malloc} {calloc} {realloc} {free} {posix} {aligned} {} {} {}", s.blocks, s.bytes, s.peak);
    }
    println!("tls-{label}-allocation-v1: native passed");
}

#[cfg(td_native_alloc_probe)]
#[path = "support/tls_remote_chain_scenario.rs"]
mod tls_remote_chain_scenario;

#[cfg(td_native_alloc_probe)]
fn tls_remote_chain() {
    use native_allocator_bridge::{calls, TD_MTA_NATIVE_REGISTRY as REGISTRY};
    let mut samples = [(calls(), REGISTRY.snapshot()); tls_remote_chain_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_remote_chain_scenario::run(|| *slots.next().unwrap() = (calls(), REGISTRY.snapshot()));
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|(_, s)| !s.invalid));
    assert_eq!(
        samples.get(6).unwrap().1.bytes,
        samples.get(7).unwrap().1.bytes,
        "remote records retained C boundary bytes"
    );
    assert_eq!(
        samples.get(6).unwrap().1.blocks,
        samples.get(7).unwrap().1.blocks,
        "remote records retained C boundary blocks"
    );
    assert_eq!(
        samples
            .get(8)
            .unwrap()
            .1
            .bytes
            .checked_sub(samples.get(9).unwrap().1.bytes),
        Some(2 * td_mta::tls_io::TLS_WIRE_BYTES)
    );
    assert_eq!(
        samples
            .get(8)
            .unwrap()
            .1
            .blocks
            .checked_sub(samples.get(9).unwrap().1.blocks),
        Some(2)
    );
    assert!(
        samples
            .get(6)
            .unwrap()
            .1
            .bytes
            .checked_sub(samples.get(2).unwrap().1.bytes)
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
            .1
            .peak
            .checked_sub(samples.get(2).unwrap().1.bytes)
            .unwrap()
            <= td_mta::limits::TLS_SESSION_BYTES + processing,
        "remote processing exceeds planned requested-byte allowance"
    );
    assert!(
        samples
            .get(7)
            .unwrap()
            .1
            .peak
            .checked_sub(samples.get(5).unwrap().1.bytes)
            .unwrap()
            <= td_mta::limits::TLS_ESTABLISHED_PROCESSING_BYTES,
        "established processing exceeds its own planned allowance"
    );
    let scenario = tls_remote_chain_scenario::label();
    for (phase, (c, s)) in tls_remote_chain_scenario::PHASES.into_iter().zip(samples) {
        let [malloc, calloc, realloc, free, posix, aligned] = c;
        println!("tls-native-{scenario} {phase} {malloc} {calloc} {realloc} {free} {posix} {aligned} {} {} {}", s.blocks, s.bytes, s.peak);
    }
    println!("tls-{scenario}-allocation-v1: native passed");
}

#[cfg(td_native_alloc_probe)]
use td_mta::metadata_sweep;
#[cfg(all(test, td_native_alloc_probe))]
use td_mta::sync;
#[cfg(td_native_alloc_probe)]
pub use td_mta::{mailbox_sweep, reference_sweep};
#[cfg(td_native_alloc_probe)]
#[path = "../src/account_checks.rs"]
#[allow(unused)] // Match the source-recompiled filesystem report types.
mod account_checks;
#[cfg(td_native_alloc_probe)]
use td_mta::{
    admission, bounded, config, format, ids, limits, mailbox_parents, ownership, ports,
    row_references, store_paths,
};
#[cfg(td_native_alloc_probe)]
#[path = "../src/recipient_sweep.rs"]
#[allow(dead_code)]
mod recipient_sweep;
#[cfg(td_native_alloc_probe)]
#[path = "support/sqlite_body_scenario.rs"]
mod sqlite_body_scenario;
#[cfg(td_native_alloc_probe)]
#[path = "../src/store_fs.rs"]
#[allow(unused)]
pub mod store_fs;

#[cfg(td_native_alloc_probe)]
fn sqlite_body() {
    use native_allocator_bridge::{calls, TD_MTA_NATIVE_REGISTRY as REGISTRY};
    let mut samples = vec![(calls(), REGISTRY.snapshot()); sqlite_body_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    sqlite_body_scenario::run(|| *slots.next().unwrap() = (calls(), REGISTRY.snapshot()));
    assert!(slots.next().is_none());
    let baseline = samples.first().unwrap().1;
    assert!(samples.iter().all(|(_, s)| !s.invalid));
    // Bound the entire C boundary, including allocator forwarding overhead.
    for (_, sample) in &samples {
        assert!(
            sample.peak.saturating_sub(baseline.bytes) <= 17 * 1024 * 1024,
            "maximum body exceeded native requested-byte envelope"
        );
    }
    let dropped = samples.last().unwrap().1;
    assert_eq!(
        dropped.bytes, baseline.bytes,
        "SQLite teardown retained C allocations"
    );
    assert_eq!(
        dropped.blocks, baseline.blocks,
        "SQLite teardown retained C blocks"
    );
    assert!(
        samples
            .get(2)
            .unwrap()
            .0
            .iter()
            .zip(samples.first().unwrap().0)
            .take(2)
            .any(|(after, before)| *after > before),
        "body scenario did not exercise wrapped native allocation"
    );
    for (phase, (c, s)) in sqlite_body_scenario::PHASES.iter().zip(samples) {
        let [malloc, calloc, realloc, free, posix, aligned] = c;
        println!(
            "sqlite-native {phase} {malloc} {calloc} {realloc} {free} {posix} {aligned} {} {} {}",
            s.blocks, s.bytes, s.peak
        );
    }
    println!("sqlite-body-allocation-v1: native passed");
}
