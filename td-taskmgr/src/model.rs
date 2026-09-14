//! Live/history selection and bounded, incremental admission on the UI thread.
use crate::budget::{Budget, Charge};
use crate::collector::{Batch, Sample};
use crate::hierarchy::ProcessKey;
use crate::history::{History, Interval, SampleId};
use crate::snapshot::{Error as SnapshotError, IdentityStore};
use crate::worker::{Failure, Update};
use std::io;
use std::sync::Arc;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    Idle,
    Admitted(SampleId),
    Reclaimed,
    Blocked,
    Rejected,
}
#[derive(Debug)]
pub struct Model {
    store: IdentityStore,
    history: History<Sample>,
    pending: Option<Batch>,
    selected: Option<ProcessKey>,
    skipped: u64,
    failure: Option<Failure>,
    _charge: Charge,
}
fn memory_error(error: SnapshotError) -> bool {
    matches!(
        error,
        SnapshotError::Budget(_)
            | SnapshotError::Hierarchy(crate::hierarchy::Error::Budget(_))
            | SnapshotError::Identity(
                crate::identities::Error::Budget(_) | crate::identities::Error::Limit
            )
    )
}
impl Model {
    pub fn new(budget: &Arc<Budget>, interval: Interval) -> io::Result<Self> {
        Ok(Self {
            store: IdentityStore::new(budget).map_err(io::Error::other)?,
            history: History::new(budget, interval).map_err(io::Error::other)?,
            pending: None,
            selected: None,
            skipped: 0,
            failure: None,
            _charge: budget
                .charge(std::mem::size_of::<Self>())
                .map_err(io::Error::other)?,
        })
    }
    pub fn history(&self) -> &History<Sample> {
        &self.history
    }
    /// Reclaim one older unpinned snapshot for the visible UI working set.
    /// Preserve the newest observation as well as an explicit inspection.
    pub fn reclaim_for_view(&mut self) -> bool {
        let last = self.history.samples().len().saturating_sub(1);
        let eligible = self
            .history
            .samples()
            .iter()
            .take(last)
            .any(|sample| Some(sample.id) != self.history.pinned());
        eligible && self.history.reclaim_one()
    }
    pub fn clear_selection(&mut self) {
        self.selected = None;
    }
    pub fn selected(&self) -> Option<ProcessKey> {
        self.selected
    }
    pub fn failure(&self) -> Option<Failure> {
        self.failure
    }
    pub fn set_interval(&mut self, interval: Interval) {
        self.history.set_interval(interval);
    }
    pub fn receive(&mut self, update: Update) {
        self.skipped = self.skipped.saturating_add(update.skipped);
        let has_batch = update.batch.is_some();
        if let Some(batch) = update.batch {
            if self.pending.replace(batch).is_some() {
                self.skipped = self.skipped.saturating_add(1);
            }
        }
        if has_batch || update.failure.is_some() {
            self.failure = update.failure;
        }
    }
    /// One bounded model construction and at most one eviction per call.
    /// The caller returns to input dispatch between failed admission attempts.
    pub fn admit_pending(&mut self) -> Admission {
        let Some(batch) = self.pending.as_ref() else {
            if self.failure == Some(Failure::Memory) {
                if self.history.reclaim_one() {
                    self.failure = None;
                    return Admission::Reclaimed;
                }
                return Admission::Blocked;
            }
            return Admission::Idle;
        };
        match batch.snapshot(&self.store) {
            Ok(processes) => {
                let Some(batch) = self.pending.take() else {
                    return Admission::Idle;
                };
                let time = batch.ended_ns;
                let mut sample = batch.finish(processes);
                sample.previous = self.history.samples().last().map(|sample| sample.id);
                match self.history.admit(time, self.skipped, sample) {
                    Ok(id) => {
                        self.skipped = 0;
                        Admission::Admitted(id)
                    }
                    Err(_) => {
                        self.skipped = self.skipped.saturating_add(1);
                        self.failure = Some(Failure::InvalidObservation);
                        Admission::Rejected
                    }
                }
            }
            Err(error) if memory_error(error) => {
                if self.history.reclaim_one() {
                    Admission::Reclaimed
                } else {
                    Admission::Blocked
                }
            }
            Err(_) => {
                self.pending = None;
                self.skipped = self.skipped.saturating_add(1);
                self.failure = Some(Failure::InvalidObservation);
                Admission::Rejected
            }
        }
    }
    pub fn live(&mut self) {
        self.history.live();
    }
    pub fn inspect(&mut self, time_ns: u64) -> bool {
        let Some(id) = self.history.at(time_ns).map(|sample| sample.id) else {
            return false;
        };
        self.history.inspect(id)
    }
    pub fn select(&mut self, key: ProcessKey) -> bool {
        if !self.history.selected().is_some_and(|sample| {
            sample
                .value
                .processes
                .processes()
                .iter()
                .any(|process| process.key == key)
        }) {
            return false;
        }
        self.selected = Some(key);
        true
    }
    pub fn select_contributor(&mut self, time_ns: u64, key: ProcessKey) -> bool {
        let Some(sample) = self.history.at(time_ns) else {
            return false;
        };
        if !sample
            .value
            .processes
            .processes()
            .iter()
            .any(|process| process.key == key)
        {
            return false;
        }
        let id = sample.id;
        self.history.inspect(id);
        self.selected = Some(key);
        true
    }
    pub fn historical(&self) -> bool {
        self.history.pinned().is_some()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::hierarchy::Input;
    use crate::snapshot::Observed;
    fn row(pid: u32) -> Observed<'static> {
        Observed {
            input: Input {
                key: ProcessKey {
                    generation: 1,
                    pid,
                    start_ticks: 1,
                },
                parent_pid: Some(0),
                cpu: Some(100),
                rss: Some(4096),
            },
            name: "worker",
            uid: Some(1000),
            state: b'R',
        }
    }
    fn update(batch: Batch) -> Update {
        Update {
            batch: Some(batch),
            skipped: 0,
            failure: None,
        }
    }
    #[test]
    fn graph_selection_pins_its_snapshot_and_live_selection_survives_disappearance() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut model = Model::new(&budget, Interval::Second).unwrap();
        model.receive(update(Batch::fixture(&budget, 1, &[row(1), row(2)])));
        assert!(matches!(model.admit_pending(), Admission::Admitted(_)));
        assert!(model.select_contributor(1, row(2).input.key));
        model.receive(update(Batch::fixture(&budget, 2, &[row(1)])));
        assert!(matches!(model.admit_pending(), Admission::Admitted(_)));
        assert!(model.historical());
        assert_eq!(model.history().selected().unwrap().time_ns, 1);
        assert_eq!(model.selected(), Some(row(2).input.key));
        assert!(!model.select_contributor(2, row(2).input.key));
        assert_eq!(model.history().selected().unwrap().time_ns, 1);
        model.live();
        assert!(!model.historical());
        assert_eq!(model.history().selected().unwrap().time_ns, 2);
        assert_eq!(model.selected(), Some(row(2).input.key));
    }
    #[test]
    fn resource_pressure_reclaims_one_unpinned_sample_without_moving_the_pin() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut model = Model::new(&budget, Interval::Second).unwrap();
        for time in 1..=3 {
            model.receive(update(Batch::fixture(&budget, time, &[row(1)])));
            assert!(matches!(model.admit_pending(), Admission::Admitted(_)));
        }
        assert!(model.inspect(1));
        for expected in [
            Admission::Reclaimed,
            Admission::Reclaimed,
            Admission::Blocked,
        ] {
            model.receive(Update {
                batch: None,
                skipped: 1,
                failure: Some(Failure::Memory),
            });
            assert_eq!(model.admit_pending(), expected);
            assert_eq!(model.history().selected().unwrap().time_ns, 1);
        }
        model.live();
        assert_eq!(model.admit_pending(), Admission::Reclaimed);
        assert!(model.history().samples().is_empty());
    }
    #[test]
    fn invalid_observation_is_distinct_from_memory_and_read_failures() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut model = Model::new(&budget, Interval::Second).unwrap();
        model.receive(update(Batch::fixture(&budget, 2, &[row(1)])));
        assert!(matches!(model.admit_pending(), Admission::Admitted(_)));
        model.receive(update(Batch::fixture(&budget, 1, &[row(1)])));
        assert_eq!(model.admit_pending(), Admission::Rejected);
        assert_eq!(model.failure(), Some(Failure::InvalidObservation));
        model.receive(Update {
            batch: None,
            skipped: 0,
            failure: None,
        });
        assert_eq!(model.failure(), Some(Failure::InvalidObservation));
        assert_eq!(model.history().selected().unwrap().time_ns, 2);
    }
    #[test]
    fn refused_snapshot_allocation_returns_to_dispatch_before_retrying() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut model = Model::new(&budget, Interval::Second).unwrap();
        for time in 1..=2 {
            model.receive(update(Batch::fixture(&budget, time, &[row(1)])));
            assert!(matches!(model.admit_pending(), Admission::Admitted(_)));
        }
        model.inspect(1);
        model.receive(update(Batch::fixture(&budget, 3, &[row(1)])));
        let held = budget.charge(budget.maximum() - budget.used() - 1).unwrap();
        assert_eq!(model.admit_pending(), Admission::Reclaimed);
        assert_eq!(model.history().samples().len(), 1);
        assert_eq!(model.history().selected().unwrap().time_ns, 1);
        assert!(matches!(model.admit_pending(), Admission::Admitted(_)));
        assert_eq!(model.history().selected().unwrap().time_ns, 1);
        drop(held);
        assert!(budget.peak() <= budget.maximum());
    }
}
