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
