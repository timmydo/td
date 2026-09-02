use crate::buffer::BufferCharge;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClientResourceSnapshot {
    pub objects: usize,
    pub shm_pools: usize,
    pub shm_bytes: usize,
    pub frame_callbacks: usize,
    pub cached_commits: usize,
    pub deferred_events: usize,
    pub deferred_bytes: usize,
    /// High-water of td's OWN memory holding copied client pixels. Named for
    /// the kind because it counts one: a buffer whose bytes are a card's does
    /// not belong in this number, and a reader comparing it against a host
    /// ceiling has to be able to tell.
    pub copied_shm_bytes: usize,
    /// High-water of how many client buffers were held at once, which the
    /// bytes do not say. `APPLICATIONS.md` §M's fourth row counts per
    /// outstanding lifetime as well as per kind, and a kind that costs td no
    /// bytes still costs it a holding.
    pub copied_buffers: usize,
}

#[derive(Default)]
pub(crate) struct ClientResourceHighWater {
    objects: AtomicUsize,
    shm_pools: AtomicUsize,
    shm_bytes: AtomicUsize,
    frame_callbacks: AtomicUsize,
    cached_commits: AtomicUsize,
    deferred_events: AtomicUsize,
    deferred_bytes: AtomicUsize,
    copied_shm_bytes: AtomicUsize,
    copied_buffers: AtomicUsize,
}

impl ClientResourceHighWater {
    pub(crate) fn observe_objects(&self, value: usize) {
        self.objects.fetch_max(value, Ordering::Relaxed);
    }

    pub(crate) fn observe_shm(&self, pools: usize, bytes: usize) {
        self.shm_pools.fetch_max(pools, Ordering::Relaxed);
        self.shm_bytes.fetch_max(bytes, Ordering::Relaxed);
    }

    pub(crate) fn observe_frame_callbacks(&self, value: usize) {
        self.frame_callbacks.fetch_max(value, Ordering::Relaxed);
    }

    pub(crate) fn observe_cached_commits(&self, value: usize) {
        self.cached_commits.fetch_max(value, Ordering::Relaxed);
    }

    pub(crate) fn observe_deferred(&self, events: usize, bytes: usize) {
        self.deferred_events.fetch_max(events, Ordering::Relaxed);
        self.deferred_bytes.fetch_max(bytes, Ordering::Relaxed);
    }

    /// Each quantity keeps its own high-water. They are deliberately not
    /// required to come from the same instant: the largest total this client
    /// ever held and the most buffers it ever held are separate facts, and a
    /// pair taken together would report neither.
    pub(crate) fn observe_copied(&self, charge: BufferCharge) {
        self.copied_shm_bytes
            .fetch_max(charge.host_bytes(), Ordering::Relaxed);
        self.copied_buffers
            .fetch_max(charge.held(), Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> ClientResourceSnapshot {
        ClientResourceSnapshot {
            objects: self.objects.load(Ordering::Relaxed),
            shm_pools: self.shm_pools.load(Ordering::Relaxed),
            shm_bytes: self.shm_bytes.load(Ordering::Relaxed),
            frame_callbacks: self.frame_callbacks.load(Ordering::Relaxed),
            cached_commits: self.cached_commits.load(Ordering::Relaxed),
            deferred_events: self.deferred_events.load(Ordering::Relaxed),
            deferred_bytes: self.deferred_bytes.load(Ordering::Relaxed),
            copied_shm_bytes: self.copied_shm_bytes.load(Ordering::Relaxed),
            copied_buffers: self.copied_buffers.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_counter_is_a_monotonic_high_water() {
        let metrics = ClientResourceHighWater::default();
        metrics.observe_objects(9);
        metrics.observe_objects(3);
        metrics.observe_shm(4, 80);
        metrics.observe_shm(2, 100);
        metrics.observe_frame_callbacks(7);
        metrics.observe_frame_callbacks(1);
        metrics.observe_cached_commits(6);
        metrics.observe_cached_commits(2);
        metrics.observe_deferred(12, 900);
        metrics.observe_deferred(8, 1_000);
        metrics.observe_copied(BufferCharge::shm(4_096));
        metrics.observe_copied(BufferCharge::shm(4));

        assert_eq!(
            metrics.snapshot(),
            ClientResourceSnapshot {
                objects: 9,
                shm_pools: 4,
                shm_bytes: 100,
                frame_callbacks: 7,
                cached_commits: 6,
                deferred_events: 12,
                deferred_bytes: 1_000,
                copied_shm_bytes: 4_096,
                copied_buffers: 1,
            }
        );
    }
}
