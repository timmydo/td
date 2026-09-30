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
    for (phase, (c, s)) in tls_handshake_scenario::PHASES.into_iter().zip(samples) {
        let [malloc, calloc, realloc, free, posix, aligned] = c;
        println!("tls-native-handshake {phase} {malloc} {calloc} {realloc} {free} {posix} {aligned} {} {} {}", s.blocks, s.bytes, s.peak);
    }
    println!("tls-handshake-allocation-v1: native passed");
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
