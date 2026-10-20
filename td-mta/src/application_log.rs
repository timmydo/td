//! Nonblocking producers and one bounded, best-effort database sink per service.
use crate::{
    clock::RuntimeClock,
    ids::{BootId, StoreEpoch, SubmissionId},
    observability::{Context, Event, Kind, Severity},
    ports::Clock,
    postgres_input::Loaded,
    postgres_logs,
};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, SyncSender},
    Arc,
};
use std::time::{Duration, Instant};
use td_crypto::{Entropy, SystemEntropy};

const QUEUE: usize = 384;
const KINDS: usize = 27;
pub struct Producer {
    sender: SyncSender<Event>,
    clock: Arc<RuntimeClock>,
    boot: BootId,
    generation: u64,
    minimum: Severity,
    next: [AtomicU64; KINDS],
    dropped: AtomicU64,
    failures: AtomicU64,
    degraded: AtomicBool,
}
pub struct Writer {
    receiver: Receiver<Event>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Status {
    pub dropped: u64,
    pub write_failures: u64,
    pub degraded: bool,
}
fn add(counter: &AtomicU64, count: u64) {
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        Some(old.saturating_add(count))
    });
}
impl Producer {
    pub fn new(
        clock: Arc<RuntimeClock>,
        generation: u64,
        minimum: Severity,
    ) -> Result<(Self, Writer), crate::ports::Error> {
        let mut bytes = [0; 16];
        SystemEntropy::try_new()?.fill(&mut bytes)?;
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        Ok((
            Self {
                sender,
                clock,
                boot: BootId::from_bytes(bytes),
                generation,
                minimum,
                next: std::array::from_fn(|_| AtomicU64::new(0)),
                dropped: AtomicU64::new(0),
                failures: AtomicU64::new(0),
                degraded: AtomicBool::new(false),
            },
            Writer { receiver },
        ))
    }
    fn event(
        &self,
        kind: Kind,
        submission: Option<SubmissionId>,
        transaction: Option<crate::format::Sequence>,
    ) -> Event {
        Event {
            kind,
            context: Context {
                boot: self.boot,
                utc_ms: self.clock.sample().ok().map(|v| v.utc_ms),
                config_generation: self.generation,
                connection: None,
                request: None,
                transaction,
                submission,
            },
        }
    }
    /// One event per fixed code per minute. Correlation IDs never create buckets.
    pub fn emit(
        &self,
        kind: Kind,
        submission: Option<SubmissionId>,
        transaction: Option<crate::format::Sequence>,
    ) {
        if rank(kind.severity()) < rank(self.minimum) {
            return;
        }
        let Some(now) = self.clock.sample().ok().map(|v| v.monotonic.0) else {
            add(&self.dropped, 1);
            return;
        };
        let Some(bucket) = self.next.get(bucket(kind)) else {
            add(&self.dropped, 1);
            return;
        };
        let prior = bucket.load(Ordering::Relaxed);
        if now < prior
            || bucket
                .compare_exchange(
                    prior,
                    now.saturating_add(60_000),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_err()
        {
            add(&self.dropped, 1);
            return;
        }
        if self
            .sender
            .try_send(self.event(kind, submission, transaction))
            .is_err()
        {
            add(&self.dropped, 1);
        }
    }
    pub fn writer_failed(&self) {
        add(&self.failures, 1);
        self.degraded.store(true, Ordering::Relaxed);
    }
    pub fn status(&self) -> Status {
        Status {
            dropped: self.dropped.load(Ordering::Relaxed),
            write_failures: self.failures.load(Ordering::Relaxed),
            degraded: self.degraded.load(Ordering::Relaxed),
        }
    }
}
fn bucket(kind: Kind) -> usize {
    match kind {
        Kind::DeliveryNoticeDeferred { .. } => 25,
        Kind::BootStarted => 0,
        Kind::ConfigActivated => 1,
        Kind::ListenerReady {
            protocol: crate::observability::Listener::Smtp,
        } => 2,
        Kind::ListenerReady {
            protocol: crate::observability::Listener::Jmap,
        } => 24,
        Kind::ConnectionOpened { .. } => 3,
        Kind::ConnectionClosed => 4,
        Kind::TlsEstablished { .. } => 5,
        Kind::TlsFailed { .. } => 6,
        Kind::MailAccepted { .. } => 7,
        Kind::AdmissionRefused { .. } => 8,
        Kind::MetadataCommitted => 9,
        Kind::WalCheckpointed => 10,
        Kind::RelayOutcome {
            outcome: crate::observability::Delivery::Accepted,
        } => 11,
        Kind::RelayOutcome {
            outcome: crate::observability::Delivery::Retry,
        } => 20,
        Kind::RelayOutcome {
            outcome: crate::observability::Delivery::PermanentFailure,
        } => 21,
        Kind::RelayOutcome {
            outcome: crate::observability::Delivery::Unknown,
        } => 22,
        Kind::AuthenticationFailed => 12,
        Kind::CertificateRenewed { .. } => 13,
        Kind::CertificateRenewalFailed => 14,
        Kind::LogSuppressed { .. } => 15,
        Kind::LogWriteFailed { .. } => 16,
        Kind::RelayConnectionFailed { .. } => 17,
        Kind::ServiceStopped { failed: false } => 18,
        Kind::ServiceStopped { failed: true } => 23,
        Kind::RelayStepFailed { .. } => 19,
    }
}
impl Writer {
    fn collect(&self, batch: &mut Vec<Event>, stopping: impl FnOnce() -> bool) -> bool {
        let stopping = stopping();
        while batch.len() < postgres_logs::BATCH - 1 {
            match self.receiver.try_recv() {
                Ok(event) => batch.push(event),
                Err(_) => break,
            }
        }
        stopping
    }

    /// No SQL or waiting occurs on producers. Failures drop this batch instead
    /// of replaying an uncertain commit or recursively logging into the sink.
    pub fn run(
        self,
        producer: &Producer,
        loaded: &Loaded,
        executable: &std::path::Path,
        epoch: StoreEpoch,
        profile: &str,
        finished: &AtomicBool,
    ) {
        let mut batch = Vec::with_capacity(postgres_logs::BATCH);
        let mut last_maintenance = None;
        let mut last_summary = None;
        let mut reported = 0;
        loop {
            let stopping = self.collect(&mut batch, || finished.load(Ordering::Acquire));
            let dropped = producer.dropped.load(Ordering::Relaxed);
            if rank(Severity::Warning) >= rank(producer.minimum)
                && dropped != reported
                && last_summary
                    .is_none_or(|last: Instant| last.elapsed() >= Duration::from_secs(60))
            {
                batch.push(producer.event(
                    Kind::LogSuppressed {
                        count: dropped,
                        saturated: dropped == u64::MAX,
                    },
                    None,
                    None,
                ));
                last_summary = Some(Instant::now());
            }
            if !batch.is_empty()
                || last_maintenance
                    .is_none_or(|last: Instant| last.elapsed() >= Duration::from_secs(60))
            {
                let result = postgres_logs::append_resolved(
                    loaded,
                    executable,
                    Instant::now() + Duration::from_secs(2),
                    epoch,
                    profile,
                    &batch,
                );
                match result {
                    Ok(done) => {
                        producer
                            .degraded
                            .store(done.paused || done.suppressed > 0, Ordering::Relaxed);
                        // Database rate loss also persists in its global counter.
                        add(&producer.dropped, done.suppressed as u64);
                        if done.suppressed == 0
                            && batch
                                .iter()
                                .any(|event| matches!(event.kind, Kind::LogSuppressed { .. }))
                        {
                            reported = dropped;
                            last_summary = Some(Instant::now());
                        }
                    }
                    Err(_) => {
                        add(&producer.failures, 1);
                        add(&producer.dropped, batch.len() as u64);
                        producer.degraded.store(true, Ordering::Relaxed);
                    }
                }
                batch.clear();
                last_maintenance = Some(Instant::now());
            }
            if stopping {
                // A bounded final batch above; shutdown never waits for a full
                // backlog or an indefinitely unavailable diagnostic database.
                for _ in 0..QUEUE {
                    if self.receiver.try_recv().is_ok() {
                        add(&producer.dropped, 1);
                    } else {
                        break;
                    }
                }
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

fn rank(value: Severity) -> u8 {
    match value {
        Severity::Info => 0,
        Severity::Warning => 1,
        Severity::Error => 2,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn shutdown_publication_is_observed_before_the_final_drain() {
        let (producer, writer) =
            Producer::new(Arc::new(RuntimeClock::new()), 1, Severity::Info).unwrap();
        let mut batch = Vec::with_capacity(postgres_logs::BATCH);
        let finished = AtomicBool::new(false);
        let stopping = writer.collect(&mut batch, || {
            // Deterministically publish the terminal event immediately before
            // the writer observes shutdown; draining earlier loses this event.
            producer.emit(Kind::ServiceStopped { failed: false }, None, None);
            finished.store(true, Ordering::Release);
            finished.load(Ordering::Acquire)
        });
        assert!(stopping);
        assert_eq!(batch.len(), 1);
        assert_eq!(
            batch.first().unwrap().kind,
            Kind::ServiceStopped { failed: false }
        );
    }
    #[test]
    fn fixed_buckets_ignore_attacker_identity_and_keep_severity_classes() {
        let (producer, writer) =
            Producer::new(Arc::new(RuntimeClock::new()), 7, Severity::Info).unwrap();
        for n in 0..10_000u128 {
            producer.emit(
                Kind::AuthenticationFailed,
                Some(SubmissionId::from_bytes(n.to_be_bytes())),
                None,
            );
        }
        assert_eq!(producer.status().dropped, 9_999);
        assert_eq!(writer.receiver.try_iter().count(), 1);
        for outcome in [
            crate::observability::Delivery::Accepted,
            crate::observability::Delivery::Retry,
            crate::observability::Delivery::PermanentFailure,
            crate::observability::Delivery::Unknown,
        ] {
            producer.emit(Kind::RelayOutcome { outcome }, None, None);
        }
        assert_eq!(writer.receiver.try_iter().count(), 4);
        assert!(std::mem::size_of::<Option<Event>>() <= 256);
    }
    #[test]
    fn severity_queue_disconnect_and_saturated_loss_are_observable() {
        let (producer, writer) =
            Producer::new(Arc::new(RuntimeClock::new()), 1, Severity::Warning).unwrap();
        producer.emit(Kind::BootStarted, None, None);
        assert!(writer.receiver.try_recv().is_err());
        assert_eq!(producer.status().dropped, 0);
        for _ in 0..QUEUE {
            producer
                .sender
                .try_send(producer.event(Kind::AuthenticationFailed, None, None))
                .unwrap();
        }
        producer.emit(Kind::AuthenticationFailed, None, None);
        assert_eq!(producer.status().dropped, 1);
        drop(writer);
        producer.emit(Kind::CertificateRenewalFailed, None, None);
        assert_eq!(producer.status().dropped, 2);
        producer.dropped.store(u64::MAX - 1, Ordering::Relaxed);
        add(&producer.dropped, 7);
        assert_eq!(producer.status().dropped, u64::MAX);
        producer.writer_failed();
        assert!(producer.status().degraded);
        assert_eq!(producer.status().write_failures, 1);
    }
}
