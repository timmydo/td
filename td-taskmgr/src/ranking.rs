//! All observed contributors at one pinned time, independently of tree filters.
use crate::budget::{Budget, Charge, MemoryVec};
use crate::contributors::Metric;
use crate::format::Text;
use crate::history::SampleId;
use crate::snapshot::Snapshot;
use std::cell::RefCell;
use std::sync::Arc;
#[derive(Debug)]
pub struct Ranking {
    pub sample: SampleId,
    pub metric: Metric,
    rows: MemoryVec<usize>,
    pub selected: usize,
    pub first: usize,
    pub(crate) cells: RefCell<MemoryVec<Text<64>>>,
    _charge: Charge,
}
impl Ranking {
    pub fn new(
        budget: &Arc<Budget>,
        sample: SampleId,
        snapshot: &Snapshot,
        metric: Metric,
    ) -> Result<Self, crate::budget::Error> {
        let mut rows = MemoryVec::new(budget, snapshot.processes().len())?;
        for index in 0..snapshot.processes().len() {
            rows.push(index).map_err(|_| crate::budget::Error::Limit)?;
        }
        rows.sort_unstable_by(|a, b| {
            let a = snapshot.processes().get(*a);
            let b = snapshot.processes().get(*b);
            let value = |p: Option<&crate::snapshot::Process>| {
                p.and_then(|p| match metric {
                    Metric::Cpu => p.cpu,
                    Metric::Rss => p.rss,
                })
            };
            value(b)
                .cmp(&value(a))
                .then_with(|| a.map(|p| p.key).cmp(&b.map(|p| p.key)))
        });
        Ok(Self {
            sample,
            metric,
            rows,
            selected: 0,
            first: 0,
            cells: RefCell::new(MemoryVec::new(budget, 0)?),
            _charge: budget.charge(std::mem::size_of::<Self>())?,
        })
    }
    pub(crate) fn reserve_paint(&mut self, rows: usize) -> Result<(), crate::budget::Error> {
        self.cells.get_mut().reserve(rows)
    }
    pub fn rows(&self) -> &[usize] {
        &self.rows
    }
}
