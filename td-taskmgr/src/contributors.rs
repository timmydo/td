//! Bounded peak contributors with stable process keys and explicit gaps.
use crate::hierarchy::ProcessKey;
use crate::snapshot::{Process, Snapshot};
pub const NAMED: usize = 8;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Metric {
    Cpu,
    Rss,
}
impl Metric {
    pub(crate) fn value(self, process: &Process) -> Option<u64> {
        match self {
            Self::Cpu => process.cpu,
            Self::Rss => process.rss,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Contributor {
    pub key: ProcessKey,
    pub peak: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Contributors {
    named: [Option<Contributor>; NAMED],
    metric: Metric,
}
fn outranks(a: Contributor, b: Contributor) -> bool {
    a.peak > b.peak || (a.peak == b.peak && a.key < b.key)
}
impl Contributors {
    pub fn selected(key: ProcessKey, metric: Metric) -> Self {
        let mut named = [None; NAMED];
        if let Some(slot) = named.first_mut() {
            *slot = Some(Contributor { key, peak: 0 });
        }
        Self { named, metric }
    }

    pub fn choose<'a>(
        samples: impl IntoIterator<Item = &'a Snapshot>,
        metric: Metric,
        selected: Option<ProcessKey>,
    ) -> Self {
        let mut named = [None; NAMED];
        let mut selected_peak = None;
        for sample in samples {
            for process in sample.processes() {
                let value = metric.value(process);
                if selected == Some(process.key) {
                    selected_peak = Some(selected_peak.unwrap_or(0).max(value.unwrap_or(0)));
                }
                let Some(peak) = value else { continue };
                if let Some(Some(existing)) = named.iter_mut().find(|slot| {
                    slot.as_ref()
                        .is_some_and(|item: &Contributor| item.key == process.key)
                }) {
                    existing.peak = existing.peak.max(peak);
                } else {
                    let next = Contributor {
                        key: process.key,
                        peak,
                    };
                    if let Some(empty) = named.iter_mut().find(|slot| slot.is_none()) {
                        *empty = Some(next);
                    } else {
                        let worst = named
                            .iter()
                            .enumerate()
                            .filter_map(|(index, slot)| slot.map(|item| (index, item)))
                            .max_by(|(_, a), (_, b)| b.peak.cmp(&a.peak).then(a.key.cmp(&b.key)))
                            .map(|(index, _)| index);
                        if let Some(slot) = worst.and_then(|index| named.get_mut(index)) {
                            if slot.is_some_and(|old| outranks(next, old)) {
                                *slot = Some(next);
                            }
                        }
                    }
                }
            }
        }
        if let Some((key, peak)) = selected.zip(selected_peak) {
            if !named.iter().flatten().any(|item| item.key == key) {
                let index = named.iter().position(Option::is_none).or_else(|| {
                    named
                        .iter()
                        .enumerate()
                        .filter_map(|(index, slot)| slot.map(|item| (index, item)))
                        .max_by(|(_, a), (_, b)| b.peak.cmp(&a.peak).then(a.key.cmp(&b.key)))
                        .map(|(index, _)| index)
                });
                if let Some(slot) = index.and_then(|index| named.get_mut(index)) {
                    *slot = Some(Contributor { key, peak });
                }
            }
        }
        named.sort_unstable_by_key(|slot| {
            slot.map(|item| (false, item.key)).unwrap_or((
                true,
                ProcessKey {
                    generation: 0,
                    pid: 0,
                    start_ticks: 0,
                },
            ))
        });
        Self { named, metric }
    }
    pub fn named(&self) -> impl Iterator<Item = Contributor> + '_ {
        self.named.iter().flatten().copied()
    }
    pub fn metric(&self) -> Metric {
        self.metric
    }
    /// Named observations keep their order; the final slot is Other observed.
    /// Missing named observations or unknown contributing metrics remain gaps.
    pub fn values(&self, sample: &Snapshot) -> [Option<u64>; NAMED + 1] {
        let mut result = [None; NAMED + 1];
        let mut other = Some(0u64);
        for process in sample.processes() {
            let value = self.metric.value(process);
            let slot = self
                .named
                .iter()
                .position(|slot| slot.is_some_and(|item| item.key == process.key));
            if let Some(slot) = slot.and_then(|index| result.get_mut(index)) {
                *slot = value;
            } else {
                other = other
                    .zip(value)
                    .and_then(|(sum, value)| sum.checked_add(value));
            }
        }
        if let Some(slot) = result.last_mut() {
            *slot = other;
        }
        result
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Colors {
    slots: [Option<ProcessKey>; NAMED],
}
impl Colors {
    /// Keep colors for retained named keys; new names take unused slots.
    pub fn update(&mut self, contributors: &Contributors) {
        for slot in &mut self.slots {
            if slot.is_some_and(|key| !contributors.named().any(|item| item.key == key)) {
                *slot = None;
            }
        }
        for item in contributors.named() {
            if !self.slots.contains(&Some(item.key)) {
                if let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_none()) {
                    *slot = Some(item.key);
                }
            }
        }
    }
    pub fn slot(&self, key: ProcessKey) -> Option<usize> {
        self.slots.iter().position(|slot| *slot == Some(key))
    }
}
