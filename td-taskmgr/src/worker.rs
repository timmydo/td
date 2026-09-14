//! A single cancellable collector and one replaceable pending observation.
use crate::budget::{Budget, Charge};
use crate::collector::{Batch, Collector};
use crate::history::Interval;
use std::io;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    Memory,
    InvalidObservation,
    Read(io::ErrorKind),
}
#[derive(Debug)]
pub struct Update {
    pub batch: Option<Batch>,
    pub skipped: u64,
    pub failure: Option<Failure>,
}
#[derive(Debug)]
struct Pending {
    update: Update,
    interval: Interval,
}
#[derive(Debug)]
struct Shared {
    pending: Mutex<Pending>,
    wake: Condvar,
    cancel: AtomicBool,
    _charge: Charge,
}
#[derive(Debug)]
pub struct Worker {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
    origin: Instant,
}
fn missed_intervals(elapsed: Duration, previous: Option<Interval>, current: Interval) -> u64 {
    if previous != Some(current) {
        return 0;
    }
    (elapsed.as_nanos() / u128::from(current.nanoseconds()))
        .saturating_sub(1)
        .min(u128::from(u64::MAX)) as u64
}
fn publish(pending: &mut Pending, result: io::Result<Batch>, missed: u64) -> Option<Batch> {
    pending.update.skipped = pending.update.skipped.saturating_add(missed);
    match result {
        Ok(batch) => {
            let old = pending.update.batch.replace(batch);
            if old.is_some() {
                pending.update.skipped = pending.update.skipped.saturating_add(1);
            }
            pending.update.failure = None;
            old
        }
        Err(error) => {
            pending.update.failure = Some(
                if error.get_ref().is_some_and(|error| {
                    error.is::<crate::budget::Error>()
                        || error.is::<std::collections::TryReserveError>()
                }) {
                    Failure::Memory
                } else {
                    Failure::Read(error.kind())
                },
            );
            pending.update.skipped = pending.update.skipped.saturating_add(1);
            None
        }
    }
}
fn run(shared: &Shared, mut sample: impl FnMut(&AtomicBool) -> io::Result<Batch>) {
    let mut last_started: Option<Instant> = None;
    let mut last_finished: Option<Instant> = None;
    let mut last_interval = None;
    loop {
        let mut pending = match shared.pending.lock() {
            Ok(pending) => pending,
            Err(_) => return,
        };
        loop {
            if shared.cancel.load(Ordering::Relaxed) {
                return;
            }
            let delay = last_started
                .and_then(|start| {
                    let interval = Duration::from_nanos(pending.interval.nanoseconds());
                    let due = start.checked_add(interval)?;
                    if let Some(finished) = last_finished.filter(|finished| *finished > due) {
                        finished.checked_add(interval)
                    } else {
                        Some(due)
                    }
                })
                .and_then(|due| due.checked_duration_since(Instant::now()));
            let Some(delay) = delay.filter(|delay| !delay.is_zero()) else {
                break;
            };
            pending = match shared.wake.wait_timeout(pending, delay) {
                Ok((pending, _)) => pending,
                Err(_) => return,
            };
        }
        let interval = pending.interval;
        drop(pending);
        let now = Instant::now();
        let missed = last_started
            .map(|last| {
                missed_intervals(now.saturating_duration_since(last), last_interval, interval)
            })
            .unwrap_or(0);
        last_interval = Some(interval);
        last_started = Some(now);
        let result = sample(&shared.cancel);
        last_finished = Some(Instant::now());
        if shared.cancel.load(Ordering::Relaxed) {
            return;
        }
        let mut pending = match shared.pending.lock() {
            Ok(pending) => pending,
            Err(_) => return,
        };
        let old = publish(&mut pending, result, missed);
        drop(pending);
        drop(old);
    }
}
impl Worker {
    pub fn start(budget: &Arc<Budget>, generation: u64, interval: Interval) -> io::Result<Self> {
        let collector = Collector::new(budget, generation)?;
        let origin = collector.origin();
        let mut collector = collector;
        let mut worker =
            Self::start_with(budget, interval, move |cancel| collector.sample(cancel))?;
        worker.origin = origin;
        Ok(worker)
    }
    fn start_with(
        budget: &Arc<Budget>,
        interval: Interval,
        sample: impl FnMut(&AtomicBool) -> io::Result<Batch> + Send + 'static,
    ) -> io::Result<Self> {
        let charge = budget
            .charge(
                std::mem::size_of::<Shared>()
                    + std::mem::size_of::<Self>()
                    + 2 * std::mem::size_of::<usize>(),
            )
            .map_err(io::Error::other)?;
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending {
                update: Update {
                    batch: None,
                    skipped: 0,
                    failure: None,
                },
                interval,
            }),
            wake: Condvar::new(),
            cancel: AtomicBool::new(false),
            _charge: charge,
        });
        let worker = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("td-taskmgr-collector".into())
            .spawn(move || run(&worker, sample))?;
        Ok(Self {
            shared,
            handle: Some(handle),
            origin: Instant::now(),
        })
    }
    pub fn elapsed_ns(&self) -> io::Result<u64> {
        u64::try_from(self.origin.elapsed().as_nanos()).map_err(io::Error::other)
    }
    pub fn interval(&self, interval: Interval) -> io::Result<()> {
        self.shared
            .pending
            .lock()
            .map_err(|_| io::Error::other("collector handoff poisoned"))?
            .interval = interval;
        self.shared.wake.notify_one();
        Ok(())
    }
    pub fn take(&mut self) -> io::Result<Update> {
        if self
            .handle
            .as_ref()
            .is_some_and(|handle| handle.is_finished())
        {
            if let Some(handle) = self.handle.take() {
                handle
                    .join()
                    .map_err(|_| io::Error::other("collector thread failed"))?;
            }
        }
        let mut pending = self
            .shared
            .pending
            .lock()
            .map_err(|_| io::Error::other("collector handoff poisoned"))?;
        Ok(std::mem::replace(
            &mut pending.update,
            Update {
                batch: None,
                skipped: 0,
                failure: None,
            },
        ))
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
        // A filesystem read may stall; dropping JoinHandle never waits for it.
        self.handle.take();
    }
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    #[test]
    fn closing_does_not_wait_for_a_stalled_collector() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let (entered, wait) = std::sync::mpsc::sync_channel(1);
        let (release, blocked) = std::sync::mpsc::sync_channel(1);
        let worker = Worker::start_with(&budget, Interval::HalfSecond, move |_| {
            entered.send(()).unwrap();
            blocked.recv().unwrap();
            Err(io::ErrorKind::Interrupted.into())
        })
        .unwrap();
        wait.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        drop(worker);
        assert!(start.elapsed() < Duration::from_millis(200));
        release.send(()).unwrap();
    }
    #[test]
    fn pending_handoff_replaces_old_samples_and_reports_the_gap() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut pending = Pending {
            update: Update {
                batch: None,
                skipped: 0,
                failure: None,
            },
            interval: Interval::HalfSecond,
        };
        assert!(publish(&mut pending, Ok(Batch::fixture(&budget, 1, &[])), 0).is_none());
        let replaced = publish(&mut pending, Ok(Batch::fixture(&budget, 2, &[])), 0).unwrap();
        assert_eq!(replaced.ended_ns, 1);
        assert_eq!(pending.update.batch.as_ref().unwrap().ended_ns, 2);
        assert_eq!(pending.update.skipped, 1);
        let error = io::Error::other(crate::budget::Error::Limit);
        assert!(publish(&mut pending, Err(error), 3).is_none());
        assert_eq!(pending.update.failure, Some(Failure::Memory));
        assert_eq!(pending.update.skipped, 5);
        assert_eq!(pending.update.batch.as_ref().unwrap().ended_ns, 2);
    }

    #[test]
    fn changing_cadence_never_invents_previously_unscheduled_observations() {
        assert_eq!(
            missed_intervals(
                Duration::from_secs(4),
                Some(Interval::FiveSeconds),
                Interval::HalfSecond
            ),
            0
        );
        assert_eq!(
            missed_intervals(
                Duration::from_secs(4),
                Some(Interval::HalfSecond),
                Interval::HalfSecond
            ),
            7
        );
        assert_eq!(
            missed_intervals(Duration::from_secs(4), None, Interval::HalfSecond),
            0
        );
        assert_eq!(
            missed_intervals(
                Duration::from_secs(4),
                Some(Interval::HalfSecond),
                Interval::FiveSeconds
            ),
            0
        );
    }
}
