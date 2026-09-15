//! Budgeted chart inputs with snapshot-bound process contribution identities.
use crate::budget::{Budget, Charge, MemoryVec};
use crate::collector::Sample;
use crate::contributors::{Colors, Contributors, Metric};
use crate::device_selection::Selection;
use crate::format::Text;
use crate::hierarchy::ProcessKey;
use crate::history::{History, WINDOW_NS};
use std::fmt::Write;
use std::sync::Arc;
use td_ui::charts::{self, Axis, Chart, Mode, Series, Time};
use td_ui::raster::{Rect, Surface};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Cpu,
    Memory,
    Network,
    Disk,
    ProcessCpu,
    ProcessRss,
    Swap,
    Core(u32),
}
impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Cpu => "System CPU",
            Self::Memory => "RAM used / available",
            Self::Network => "Network: current namespace",
            Self::Disk => "Disk read / write",
            Self::ProcessCpu => "Process CPU (one-core basis)",
            Self::ProcessRss => "Process RSS (shared pages counted)",
            Self::Swap => "Swap used",
            Self::Core(_) => "Logical CPU",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Id {
    System(u8),
    Process(ProcessKey),
    Other,
}
const PALETTE: [u32; 8] = [
    0xff3b6e82, 0xff98763c, 0xff677d46, 0xff975f67, 0xff77609b, 0xff528d83, 0xffa76443, 0xff5e76a0,
];
#[derive(Debug)]
struct Line {
    id: Id,
    label: Text<128>,
    color: u32,
    values: MemoryVec<Option<u64>>,
}
#[derive(Debug)]
pub struct Plot {
    pub kind: Kind,
    lines: MemoryVec<Line>,
    times: MemoryVec<u64>,
    labels: MemoryVec<Text<24>>,
    maximum: u64,
    maximum_label: Text<128>,
    divisor: u64,
    unit: &'static str,
    decimals: u8,
    _charge: Charge,
}
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
impl Plot {
    pub fn new(
        budget: &Arc<Budget>,
        history: &History<Sample>,
        kind: Kind,
        selected: Option<ProcessKey>,
        colors: &mut Colors,
        devices: &Selection,
    ) -> Result<Self, String> {
        let latest = history.samples().last().map(|s| s.time_ns).unwrap_or(0);
        let visible = || {
            history
                .samples()
                .iter()
                .filter(|s| latest.saturating_sub(s.time_ns) < WINDOW_NS)
        };
        // One explicit break between observations with a missed handoff.
        let count = visible().count().saturating_mul(2).saturating_sub(1);
        let mut this = Self {
            kind,
            lines: MemoryVec::new(budget, 9).map_err(error)?,
            times: MemoryVec::new(budget, count).map_err(error)?,
            labels: MemoryVec::new(budget, count).map_err(error)?,
            maximum: 1,
            maximum_label: Text::default(),
            divisor: 1,
            unit: "B/s",
            decimals: 0,
            _charge: budget.charge(std::mem::size_of::<Self>()).map_err(error)?,
        };
        let metric = match kind {
            Kind::ProcessCpu => Some(Metric::Cpu),
            Kind::ProcessRss => Some(Metric::Rss),
            _ => None,
        };
        let contributors = metric.map(|metric| {
            if let Some(key) = selected {
                Contributors::selected(key, metric)
            } else {
                Contributors::choose(visible().map(|s| &s.value.processes), metric, None)
            }
        });
        let mut add = |id, label: Text<128>, color| -> Result<(), String> {
            this.lines
                .push(Line {
                    id,
                    label,
                    color,
                    values: MemoryVec::new(budget, count).map_err(error)?,
                })
                .map_err(|_| "chart series limit".to_owned())
        };
        if let Some(contributors) = &contributors {
            if selected.is_none() {
                colors.update(contributors);
            }
            let keys = contributors.named().map(|item| item.key);
            for key in keys {
                let mut label = Text::<128>::new("Process");
                for sample in visible().rev() {
                    if let Ok(index) = sample
                        .value
                        .processes
                        .processes()
                        .binary_search_by_key(&key, |p| p.key)
                    {
                        label = sample
                            .value
                            .processes
                            .with_identities(|names| {
                                Text::truncated(
                                    names.get(index).map(|n| n.name()).unwrap_or("Process"),
                                )
                            })
                            .map_err(error)?;
                        let name = Text::<12>::truncated(label.as_str());
                        label = Text::new(name.as_str());
                        break;
                    }
                }
                let _ = write!(label, " [{}]", key.pid);
                let color = colors
                    .slot(key)
                    .and_then(|i| PALETTE.get(i))
                    .copied()
                    .unwrap_or(PALETTE.first().copied().unwrap_or(0xff333333));
                add(Id::Process(key), label, color)?;
            }
            if selected.is_none() {
                add(Id::Other, Text::new("Other observed"), 0xffa39a8d)?;
            }
        } else {
            let labels: &[&str] = match kind {
                Kind::Cpu | Kind::Core(_) => &["Busy", "I/O wait", "Steal"],
                Kind::Memory => &["Used", "Available"],
                Kind::Network => &["Receive", "Send"],
                Kind::Disk => &["Read", "Write"],
                Kind::Swap => &["Used"],
                _ => &[],
            };
            for (index, label) in labels.iter().enumerate() {
                add(
                    Id::System(index as u8),
                    Text::new(label),
                    PALETTE.get(index).copied().unwrap_or(0xff333333),
                )?;
            }
        }
        let mut previous = None;
        for sample in visible() {
            let sparse = previous.is_some() && sample.value.previous != previous;
            previous = Some(sample.id);
            if sample.skipped > 0 || sparse {
                if let Some(gap) = this
                    .times
                    .last()
                    .and_then(|at| at.checked_add(1))
                    .filter(|at| *at < sample.time_ns)
                {
                    this.times.push(gap).map_err(|_| "chart gap limit")?;
                    this.labels
                        .push(Text::new(if sample.skipped > 0 {
                            "Skipped"
                        } else {
                            "Sparse history"
                        }))
                        .map_err(|_| "chart gap label limit")?;
                    for line in this.lines.iter_mut() {
                        line.values
                            .push(None)
                            .map_err(|_| "chart gap value limit")?;
                    }
                }
            }
            this.times
                .push(sample.time_ns)
                .map_err(|_| "chart time limit")?;
            let mut label = Text::default();
            let seconds = sample.time_ns / 1_000_000_000;
            let _ = write!(
                label,
                "{}:{:02}.{}",
                seconds / 60,
                seconds % 60,
                (sample.time_ns / 100_000_000) % 10
            );
            this.labels
                .push(label)
                .map_err(|_| "chart time label limit")?;
            let mut values = [None; 9];
            if let Some(contributors) = &contributors {
                if selected.is_none() {
                    let observed = contributors.values(&sample.value.processes);
                    for (index, (slot, line)) in
                        values.iter_mut().zip(this.lines.iter()).enumerate()
                    {
                        *slot = if line.id == Id::Other {
                            observed.last().copied().flatten()
                        } else {
                            observed.get(index).copied().flatten()
                        };
                    }
                } else {
                    for (slot, line) in values.iter_mut().zip(this.lines.iter()) {
                        *slot = match line.id {
                            Id::Process(key) => sample
                                .value
                                .processes
                                .processes()
                                .binary_search_by_key(&key, |process| process.key)
                                .ok()
                                .and_then(|index| sample.value.processes.processes().get(index))
                                .and_then(|process| contributors.metric().value(process)),
                            _ => None,
                        };
                    }
                }
            } else {
                let a = &sample.value;
                let cpu = match kind {
                    Kind::Cpu => a.cpu,
                    Kind::Core(id) => a
                        .cpus
                        .iter()
                        .find(|cpu| cpu.id == id)
                        .and_then(|cpu| cpu.usage),
                    _ => None,
                };
                let (first, second, third) = match kind {
                    Kind::Cpu | Kind::Core(_) => (
                        cpu.map(|v| v.busy),
                        cpu.map(|v| v.iowait),
                        cpu.map(|v| v.steal),
                    ),
                    Kind::Memory => (
                        a.memory.and_then(|v| v.used()),
                        a.memory.and_then(|v| v.available),
                        None,
                    ),
                    Kind::Swap => (a.memory.and_then(|v| v.swap_used()), None, None),
                    Kind::Network => {
                        let (a, b) = a
                            .devices
                            .as_ref()
                            .map(|d| devices.network_rates(d))
                            .unwrap_or((None, None));
                        (a, b, None)
                    }
                    Kind::Disk => {
                        let (a, b) = a
                            .devices
                            .as_ref()
                            .map(|d| devices.disk_rates(d))
                            .unwrap_or((None, None));
                        (a, b, None)
                    }
                    _ => (None, None, None),
                };
                for (slot, value) in values.iter_mut().zip([first, second, third]) {
                    *slot = value;
                }
            }
            for (line, value) in this.lines.iter_mut().zip(values) {
                line.values.push(value).map_err(|_| "chart value limit")?;
                if let Some(value) = value {
                    this.maximum = this.maximum.max(value);
                }
            }
        }
        if matches!(kind, Kind::Cpu | Kind::Core(_)) {
            this.maximum = 10000;
        }
        if matches!(kind, Kind::Cpu | Kind::Core(_) | Kind::ProcessCpu) {
            this.unit = "%";
            this.divisor = 100;
            this.decimals = 2;
            this.maximum_label = Text::percent(Some(this.maximum), false);
        } else {
            let memory = matches!(kind, Kind::Memory | Kind::Swap | Kind::ProcessRss);
            let (divisor, unit) = if this.maximum >= 1 << 30 {
                (1u64 << 30, if memory { "GiB" } else { "GiB/s" })
            } else if this.maximum >= 1 << 20 {
                (1 << 20, if memory { "MiB" } else { "MiB/s" })
            } else if this.maximum >= 1 << 10 {
                (1 << 10, if memory { "KiB" } else { "KiB/s" })
            } else {
                (1, if memory { "B" } else { "B/s" })
            };
            this.divisor = divisor;
            this.unit = unit;
            this.decimals = 1;
            if let Some(maximum) = this.maximum.div_ceil(divisor).checked_mul(divisor) {
                this.maximum = maximum;
                this.maximum_label = Text::number(Some(maximum / divisor));
            } else {
                this.divisor = 1;
                this.unit = if memory { "B" } else { "B/s" };
                this.decimals = 0;
                this.maximum_label = Text::number(Some(this.maximum));
            }
        }
        Ok(this)
    }
    pub fn selection(&self, at: u64, series: Option<Id>) -> charts::Selection<Id> {
        let series = series
            .filter(|id| self.lines.iter().any(|line| line.id == *id))
            .or_else(|| self.lines.first().map(|line| line.id));
        charts::Selection { at, series }
    }
    pub fn latest_selection(&self) -> Option<charts::Selection<Id>> {
        Some(charts::Selection {
            at: *self.times.last()?,
            series: self.lines.first().map(|line| line.id),
        })
    }
    pub fn with_chart<T>(
        &self,
        surface: Surface,
        rect: Rect,
        run: impl FnOnce(&Chart<'_, Id>) -> T,
    ) -> Result<T, charts::Error> {
        let mut times = [Time { at: 0, label: "" }; 2 * crate::history::SAMPLES];
        for ((slot, time), label) in times
            .iter_mut()
            .zip(self.times.iter())
            .zip(self.labels.iter())
        {
            *slot = Time {
                at: *time,
                label: label.as_str(),
            };
        }
        let mut series = [Series {
            id: Id::Other,
            label: "",
            color: 0,
            values: &[],
        }; 9];
        for (slot, line) in series.iter_mut().zip(self.lines.iter()) {
            *slot = Series {
                id: line.id,
                label: line.label.as_str(),
                color: line.color,
                values: &line.values,
            };
        }
        let chart = Chart::new(
            surface,
            rect,
            Mode::Lines,
            Axis {
                maximum: self.maximum,
                divisor: self.divisor,
                decimal_places: self.decimals,
                maximum_label: self.maximum_label.as_str(),
                unit: self.unit,
            },
            times.get(..self.times.len()).ok_or(charts::Error::Limit)?,
            series.get(..self.lines.len()).ok_or(charts::Error::Limit)?,
        )?;
        Ok(run(&chart))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::collector::Batch;
    use crate::hierarchy::Input;
    use crate::history::Interval;
    use crate::model::{Admission, Model};
    use crate::snapshot::Observed;
    use crate::worker::Update;
    #[test]
    fn missed_observations_break_independent_lines_without_summing_them() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut model = Model::new(&budget, Interval::Second).unwrap();
        let rows = [1, 2].map(|pid| Observed {
            cpu_time_ms: None,
            input: Input {
                key: ProcessKey {
                    generation: 1,
                    pid,
                    start_ticks: 1,
                },
                parent_pid: Some(0),
                cpu: Some(u64::MAX),
                rss: Some(10),
            },
            name: "worker",
            uid: Some(1000),
            state: b'R',
        });
        for (at, skipped) in [(1_000_000_000, 0), (3_000_000_000, 1)] {
            model.receive(Update {
                batch: Some(Batch::fixture(&budget, at, &rows)),
                skipped,
                failure: None,
            });
            assert!(matches!(model.admit_pending(), Admission::Admitted(_)));
        }
        let devices = Selection::new(&budget).unwrap();
        let mut colors = Colors::default();
        let cpu = Plot::new(
            &budget,
            model.history(),
            Kind::ProcessCpu,
            None,
            &mut colors,
            &devices,
        )
        .unwrap();
        assert_eq!(cpu.times.len(), 3);
        assert_eq!(
            cpu.lines.first().unwrap().values.first(),
            Some(&Some(u64::MAX))
        );
        assert!(cpu
            .lines
            .iter()
            .all(|line| line.values.get(1) == Some(&None)));
        let rss = Plot::new(
            &budget,
            model.history(),
            Kind::ProcessRss,
            None,
            &mut colors,
            &devices,
        )
        .unwrap();
        assert_eq!(rss.labels.get(1).unwrap().as_str(), "Skipped");
        assert!(rss
            .lines
            .iter()
            .all(|line| line.values.get(1) == Some(&None)));
        assert_eq!(rss.lines.first().unwrap().values.first(), Some(&Some(10)));
        let surface = Surface::new(800, 600, Default::default()).unwrap();
        assert!(cpu.with_chart(surface, surface.bounds(), |_| ()).is_ok());
        assert!(rss.with_chart(surface, surface.bounds(), |_| ()).is_ok());
    }
    #[test]
    fn unrelated_unknown_process_does_not_blank_known_history() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut model = Model::new(&budget, Interval::Second).unwrap();
        let rows = [Some(2500), None].map(|cpu| Observed {
            cpu_time_ms: None,
            input: Input {
                key: ProcessKey {
                    generation: 1,
                    pid: if cpu.is_some() { 1 } else { 2 },
                    start_ticks: 1,
                },
                parent_pid: Some(0),
                cpu,
                rss: Some(10),
            },
            name: "a-very-long-process-name-that-must-not-hide-the-graph",
            uid: Some(1000),
            state: b'R',
        });
        for second in 1..=3 {
            model.receive(Update {
                batch: Some(Batch::fixture(&budget, second * 1_000_000_000, &rows)),
                skipped: 0,
                failure: None,
            });
            assert!(matches!(model.admit_pending(), Admission::Admitted(_)));
        }
        let devices = Selection::new(&budget).unwrap();
        let mut colors = Colors::default();
        let plot = Plot::new(
            &budget,
            model.history(),
            Kind::ProcessCpu,
            None,
            &mut colors,
            &devices,
        )
        .unwrap();
        let known = plot
            .lines
            .iter()
            .find(|line| line.id == Id::Process(rows[0].input.key))
            .unwrap();
        assert_eq!(&*known.values, &[Some(2500); 3]);
        let comparison_colors = colors;
        let selected = Plot::new(
            &budget,
            model.history(),
            Kind::ProcessCpu,
            Some(rows[0].input.key),
            &mut colors,
            &devices,
        )
        .unwrap();
        assert_eq!(colors, comparison_colors);
        assert_eq!(selected.lines[0].color, known.color);
        let compared_again = Plot::new(
            &budget,
            model.history(),
            Kind::ProcessCpu,
            None,
            &mut colors,
            &devices,
        )
        .unwrap();
        assert_eq!(colors, comparison_colors);
        assert_eq!(compared_again.lines[0].color, plot.lines[0].color);
        let surface = Surface::new(400, 500, Default::default()).unwrap();
        assert!(selected
            .with_chart(surface, surface.bounds(), |_| ())
            .is_ok());
        assert_eq!(selected.lines.len(), 1);
        assert_eq!(selected.lines[0].id, Id::Process(rows[0].input.key));
    }
    #[test]
    fn evicted_predecessors_break_lines_after_a_cadence_change() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut model = Model::new(&budget, Interval::Second).unwrap();
        for second in 1..=30 {
            model.receive(Update {
                batch: Some(Batch::fixture(&budget, second * 1_000_000_000, &[])),
                skipped: 0,
                failure: None,
            });
            assert!(matches!(model.admit_pending(), Admission::Admitted(_)));
        }
        assert!(model.inspect(1_000_000_000));
        model.set_interval(Interval::FiveSeconds);
        let plot = Plot::new(
            &budget,
            model.history(),
            Kind::Cpu,
            None,
            &mut Colors::default(),
            &Selection::new(&budget).unwrap(),
        )
        .unwrap();
        assert_eq!(plot.labels.get(1).unwrap().as_str(), "Sparse history");
        assert!(plot
            .lines
            .iter()
            .all(|line| line.values.get(1) == Some(&None)));
    }
    #[test]
    fn byte_axis_ceiling_and_label_use_the_same_rounded_scale() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut model =
            crate::model::Model::new(&budget, crate::history::Interval::Second).unwrap();
        let mut batch = crate::collector::Batch::fixture(&budget, 1, &[]);
        batch.memory = Some(crate::parsers::Memory {
            total: Some(3 << 29),
            available: Some(0),
            swap_total: Some(0),
            swap_free: Some(0),
        });
        model.receive(crate::worker::Update {
            batch: Some(batch),
            skipped: 0,
            failure: None,
        });
        assert!(matches!(
            model.admit_pending(),
            crate::model::Admission::Admitted(_)
        ));
        let mut colors = Colors::default();
        let devices = crate::device_selection::Selection::new(&budget).unwrap();
        let plot = Plot::new(
            &budget,
            model.history(),
            Kind::Memory,
            None,
            &mut colors,
            &devices,
        )
        .unwrap();
        assert_eq!(plot.maximum, 2 << 30);
        assert_eq!(plot.maximum_label.as_str(), "2");
        assert_eq!(plot.divisor, 1 << 30);
    }
}
