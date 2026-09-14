//! Bounded Linux observations; all filesystem work belongs on one worker.
use crate::budget::{Budget, Charge, MemoryVec};
use crate::hierarchy::{Input, ProcessKey, ROWS};
use crate::linux_read::{runtime_units, Reader, Units};
use crate::parsers::{self, Cpu, CpuUsage, Memory, AGGREGATE_BYTES, PROCESS_BYTES};
use crate::snapshot::{IdentityStore, Observed, Snapshot};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Instant;
const NAME_BYTES: usize = 4 * 1024 * 1024;
#[derive(Clone, Copy, Debug)]
struct Baseline {
    key: ProcessKey,
    user: Option<u64>,
    system: Option<u64>,
    time_ns: u64,
}
impl Baseline {
    fn process(self) -> parsers::Process<'static> {
        parsers::Process {
            pid: self.key.pid,
            name: b"",
            state: b'?',
            parent: None,
            start_ticks: self.key.start_ticks,
            user_ticks: self.user,
            system_ticks: self.system,
            rss_pages: None,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Process {
    pub input: Input,
    pub uid: Option<u32>,
    pub state: u8,
    name_start: usize,
    name_len: usize,
    baseline: Baseline,
}
#[derive(Clone, Copy, Debug)]
pub struct LogicalCpu {
    pub id: u32,
    pub usage: Option<CpuUsage>,
}
#[derive(Clone, Copy, Debug)]
struct CpuBaseline {
    id: u32,
    counters: Cpu,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Coverage {
    pub unreadable: usize,
    pub malformed: usize,
    pub omitted_at_least: usize,
    pub enumeration_failed: bool,
    pub omitted_cpus: usize,
}
impl Coverage {
    pub fn partial(self) -> bool {
        self.unreadable > 0
            || self.malformed > 0
            || self.omitted_at_least > 0
            || self.enumeration_failed
    }
}
#[derive(Debug)]
pub struct Batch {
    pub started_ns: u64,
    pub ended_ns: u64,
    pub cpu: Option<CpuUsage>,
    pub memory: Option<Memory>,
    pub cpus: MemoryVec<LogicalCpu>,
    pub processes: MemoryVec<Process>,
    pub coverage: Coverage,
    pub devices: Option<crate::devices::Sample>,
    names: MemoryVec<u8>,
    budget: Arc<Budget>,
    _charge: Charge,
}
impl Batch {
    fn new(budget: &Arc<Budget>, started_ns: u64) -> io::Result<Self> {
        Ok(Self {
            started_ns,
            ended_ns: started_ns,
            cpu: None,
            memory: None,
            cpus: MemoryVec::new(budget, 4096).map_err(io::Error::other)?,
            processes: MemoryVec::new(budget, 256).map_err(io::Error::other)?,
            coverage: Coverage::default(),
            devices: None,
            names: MemoryVec::new(budget, 65536).map_err(io::Error::other)?,
            budget: Arc::clone(budget),
            _charge: budget
                .charge(std::mem::size_of::<Self>())
                .map_err(io::Error::other)?,
        })
    }
    pub fn into_sample(self, store: &IdentityStore) -> Result<Sample, crate::snapshot::Error> {
        let processes = self.snapshot(store)?;
        Ok(self.finish(processes))
    }
    pub(crate) fn finish(self, processes: Snapshot) -> Sample {
        Sample {
            previous: None,
            processes,
            cpu: self.cpu,
            memory: self.memory,
            cpus: self.cpus,
            devices: self.devices,
            coverage: self.coverage,
            _charge: self._charge,
        }
    }
    pub fn name(&self, process: &Process) -> Option<&str> {
        self.names
            .get(process.name_start..process.name_start.checked_add(process.name_len)?)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
    }
    pub fn snapshot(&self, store: &IdentityStore) -> Result<Snapshot, crate::snapshot::Error> {
        let mut rows = MemoryVec::new(&self.budget, self.processes.len())
            .map_err(crate::snapshot::Error::Budget)?;
        for process in self.processes.iter() {
            let name = self.name(process).ok_or(crate::snapshot::Error::Identity(
                crate::identities::Error::Invalid,
            ))?;
            rows.push(Observed {
                input: process.input,
                uid: process.uid,
                state: process.state,
                name,
            })
            .map_err(|_| crate::snapshot::Error::Budget(crate::budget::Error::Limit))?;
        }
        Snapshot::new(
            store,
            &rows,
            self.started_ns,
            self.ended_ns,
            self.coverage.partial(),
        )
    }
}
#[derive(Debug)]
pub struct Collector {
    budget: Arc<Budget>,
    reader: Reader,
    units: Units,
    origin: Instant,
    generation: u64,
    previous: MemoryVec<Baseline>,
    cpu: Option<Cpu>,
    cpus: MemoryVec<CpuBaseline>,
    cpu_next: MemoryVec<CpuBaseline>,
    devices: crate::devices::Devices,
    name: String,
    _charge: Charge,
}
fn canceled(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "collection canceled",
        ))
    } else {
        Ok(())
    }
}
fn record_process_result(coverage: &mut Coverage, result: io::Result<()>) -> io::Result<bool> {
    match result {
        Ok(()) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            coverage.malformed += 1;
            Ok(false)
        }
        Err(error) if error.kind() == io::ErrorKind::FileTooLarge => {
            coverage.omitted_at_least += 1;
            Ok(true)
        }
        // Allocation and internal failures must reach the worker, not become
        // a smaller successful snapshot that prevents history reclamation.
        Err(error) if error.kind() == io::ErrorKind::Other => Err(error),
        Err(_) => {
            coverage.unreadable += 1;
            Ok(false)
        }
    }
}
impl Collector {
    pub fn new(budget: &Arc<Budget>, generation: u64) -> io::Result<Self> {
        let mut reader = Reader::new(budget, AGGREGATE_BYTES)?;
        let units = runtime_units(&mut reader)?;
        let mut charge = budget
            .charge(std::mem::size_of::<Self>() + 4096)
            .map_err(io::Error::other)?;
        let mut name = String::new();
        name.try_reserve_exact(4096).map_err(io::Error::other)?;
        charge
            .additional(name.capacity().saturating_sub(4096))
            .map_err(io::Error::other)?;
        Ok(Self {
            budget: Arc::clone(budget),
            reader,
            units,
            origin: Instant::now(),
            generation,
            previous: MemoryVec::new(budget, ROWS).map_err(io::Error::other)?,
            cpu: None,
            cpus: MemoryVec::new(budget, 4096).map_err(io::Error::other)?,
            cpu_next: MemoryVec::new(budget, 4096).map_err(io::Error::other)?,
            devices: crate::devices::Devices::new(budget)?,
            name,
            _charge: charge,
        })
    }
    pub(crate) fn origin(&self) -> Instant {
        self.origin
    }
    pub fn elapsed_ns(&self) -> io::Result<u64> {
        u64::try_from(self.origin.elapsed().as_nanos()).map_err(io::Error::other)
    }
    fn system(&mut self, batch: &mut Batch) {
        if let Ok(bytes) = self.reader.path(Path::new("/proc/stat"), AGGREGATE_BYTES) {
            cpu_sample(
                bytes,
                &mut self.cpu,
                &mut self.cpus,
                &mut self.cpu_next,
                batch,
            );
        } else {
            self.cpu = None;
            self.cpus.clear();
        }
        batch.memory = self
            .reader
            .path(Path::new("/proc/meminfo"), AGGREGATE_BYTES)
            .ok()
            .and_then(|bytes| Memory::parse(bytes).ok());
    }
    fn process(&mut self, directory: &File, pid: u32, batch: &mut Batch) -> io::Result<()> {
        let bytes = self.reader.process_file(directory, "stat", PROCESS_BYTES)?;
        let current = parsers::process(bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if current.pid != pid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process directory identity changed",
            ));
        }
        let time_ns = u64::try_from(self.origin.elapsed().as_nanos()).map_err(io::Error::other)?;
        let key = ProcessKey {
            generation: self.generation,
            pid,
            start_ticks: current.start_ticks,
        };
        let cpu = self
            .previous
            .binary_search_by_key(&pid, |old| old.key.pid)
            .ok()
            .and_then(|index| self.previous.get(index))
            .filter(|old| old.key == key)
            .and_then(|old| {
                parsers::process_cpu(
                    current,
                    old.process(),
                    self.units.ticks_per_second,
                    time_ns.checked_sub(old.time_ns)?,
                )
            });
        let input = Input {
            key,
            parent_pid: current.parent,
            cpu,
            rss: parsers::resident_bytes(current.rss_pages, self.units.page_size),
        };
        let baseline = Baseline {
            key,
            user: current.user_ticks,
            system: current.system_ticks,
            time_ns,
        };
        let state = current.state;
        let truncated =
            parsers::escaped_text(current.name, &mut self.name, 4096).map_err(io::Error::other)?;
        if truncated {
            batch.coverage.malformed += 1;
        }
        let uid = self
            .reader
            .process_file(directory, "status", PROCESS_BYTES)
            .ok()
            .and_then(|bytes| parsers::real_uid(bytes).ok())
            .flatten();
        let required = batch
            .names
            .len()
            .checked_add(self.name.len())
            .filter(|bytes| *bytes <= NAME_BYTES)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::FileTooLarge, "process name arena limit")
            })?;
        if required > batch.names.capacity() {
            batch
                .names
                .reserve(
                    required
                        .max(batch.names.capacity().saturating_mul(2))
                        .min(NAME_BYTES),
                )
                .map_err(io::Error::other)?;
        }
        if batch.processes.len() == batch.processes.capacity() {
            batch
                .processes
                .reserve(batch.processes.capacity().saturating_mul(2).min(ROWS))
                .map_err(io::Error::other)?;
        }
        let start = batch.names.len();
        for byte in self.name.bytes() {
            batch
                .names
                .push(byte)
                .map_err(|_| io::Error::other("name buffer full"))?;
        }
        batch
            .processes
            .push(Process {
                input,
                uid,
                state,
                name_start: start,
                name_len: self.name.len(),
                baseline,
            })
            .map_err(|_| io::Error::other("process buffer full"))
    }
    pub fn sample(&mut self, cancel: &AtomicBool) -> io::Result<Batch> {
        canceled(cancel)?;
        let mut batch = Batch::new(&self.budget, self.elapsed_ns()?)?;
        self.system(&mut batch);
        batch.cpus.compact().map_err(io::Error::other)?;
        canceled(cancel)?;
        let origin = self.origin;
        batch.devices = Some(self.devices.sample(
            &mut self.reader,
            || u64::try_from(origin.elapsed().as_nanos()).map_err(io::Error::other),
            || canceled(cancel),
        )?);
        let entries = match std::fs::read_dir("/proc") {
            Ok(entries) => entries,
            Err(_) => {
                batch.coverage.enumeration_failed = true;
                batch.ended_ns = self.elapsed_ns()?;
                self.previous.clear();
                return Ok(batch);
            }
        };
        let mut arena_full = false;
        for (seen, entry) in entries.enumerate() {
            canceled(cancel)?;
            if seen >= ROWS + 256 {
                batch.coverage.enumeration_failed = true;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    batch.coverage.enumeration_failed = true;
                    break;
                }
            };
            let pid = parsers::unsigned(entry.file_name().as_bytes())
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid > 0);
            let Some(pid) = pid else { continue };
            if batch.processes.len() == ROWS || arena_full {
                batch.coverage.omitted_at_least += 1;
                continue;
            }
            let result = self
                .reader
                .process_directory(pid)
                .and_then(|directory| self.process(&directory, pid, &mut batch));
            arena_full = record_process_result(&mut batch.coverage, result)?;
        }
        batch.processes.sort_unstable_by_key(|process| {
            (
                process.input.key.pid,
                std::cmp::Reverse(process.baseline.time_ns),
            )
        });
        let mut last_pid = None;
        batch.processes.retain(|process| {
            let repeated = last_pid == Some(process.input.key.pid);
            last_pid = Some(process.input.key.pid);
            if repeated {
                batch.coverage.malformed += 1;
            }
            !repeated
        });
        self.previous.clear();
        for process in batch.processes.iter() {
            self.previous
                .push(process.baseline)
                .map_err(|_| io::Error::other("baseline buffer full"))?;
        }
        batch.ended_ns = self.elapsed_ns()?;
        Ok(batch)
    }
}

#[derive(Debug)]
pub struct Sample {
    pub(crate) previous: Option<crate::history::SampleId>,
    pub processes: Snapshot,
    pub cpu: Option<CpuUsage>,
    pub memory: Option<Memory>,
    pub cpus: MemoryVec<LogicalCpu>,
    pub devices: Option<crate::devices::Sample>,
    pub coverage: Coverage,
    _charge: Charge,
}

#[cfg(test)]
impl Batch {
    pub(crate) fn fixture(budget: &Arc<Budget>, time: u64, rows: &[Observed<'_>]) -> Self {
        #[allow(clippy::unwrap_used)]
        fn build(budget: &Arc<Budget>, time: u64, rows: &[Observed<'_>]) -> Batch {
            let mut batch = Batch::new(budget, time).unwrap();
            for row in rows {
                let name_start = batch.names.len();
                for byte in row.name.bytes() {
                    batch.names.push(byte).unwrap();
                }
                batch
                    .processes
                    .push(Process {
                        input: row.input,
                        uid: row.uid,
                        state: row.state,
                        name_start,
                        name_len: row.name.len(),
                        baseline: Baseline {
                            key: row.input.key,
                            user: None,
                            system: None,
                            time_ns: time,
                        },
                    })
                    .unwrap();
            }
            batch
        }
        build(budget, time, rows)
    }
}

fn cpu_sample(
    bytes: &[u8],
    aggregate_old: &mut Option<Cpu>,
    old: &mut MemoryVec<CpuBaseline>,
    next: &mut MemoryVec<CpuBaseline>,
    batch: &mut Batch,
) {
    let mut aggregate = None;
    next.clear();
    let mut count = 0;
    let mut last_id = None;
    let mut malformed = false;
    let mut aggregate_seen = false;
    let mut aggregate_repeated = false;
    for line in bytes
        .split(|b| *b == b'\n')
        .filter(|line| line.starts_with(b"cpu"))
    {
        let Ok((id, cpu)) = Cpu::parse(line) else {
            batch.coverage.omitted_cpus += 1;
            malformed = true;
            continue;
        };
        if let Some(id) = id {
            if last_id.is_some_and(|last| id <= last) {
                malformed = true;
                batch.coverage.omitted_cpus += 1;
                continue;
            }
            last_id = Some(id);
            if count == 4096 {
                batch.coverage.omitted_cpus += 1;
                continue;
            }
            let usage = old
                .binary_search_by_key(&id, |old| old.id)
                .ok()
                .and_then(|index| old.get(index))
                .and_then(|old| cpu.since(old.counters));
            let _ = next.push(CpuBaseline { id, counters: cpu });
            let _ = batch.cpus.push(LogicalCpu { id, usage });
            count += 1;
        } else {
            aggregate_repeated |= aggregate_seen;
            aggregate_seen = true;
            aggregate = Some(cpu);
        }
    }
    if aggregate_repeated {
        aggregate = None;
    }
    if malformed {
        batch.coverage.omitted_cpus += batch.cpus.len();
        batch.cpus.clear();
        next.clear();
    }
    next.sort_unstable_by_key(|cpu| cpu.id);
    let same_cpus =
        old.len() == next.len() && old.iter().zip(next.iter()).all(|(a, b)| a.id == b.id);
    batch.cpu = if same_cpus {
        aggregate
            .zip(*aggregate_old)
            .and_then(|(next, old)| next.since(old))
    } else {
        None
    };
    *aggregate_old = aggregate;
    std::mem::swap(old, next);
    next.clear();
}

#[cfg(test)]
mod cpu_tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    #[test]
    fn process_growth_pressure_propagates_instead_of_becoming_omission() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut collector = Collector::new(&budget, 1).unwrap();
        let mut batch = Batch::new(&budget, 0).unwrap();
        let pid = std::process::id();
        let directory = collector.reader.process_directory(pid).unwrap();
        collector.process(&directory, pid, &mut batch).unwrap();
        let first = *batch.processes.first().unwrap();
        while batch.processes.len() < batch.processes.capacity() {
            batch.processes.push(first).unwrap();
        }
        let held = budget.charge(budget.maximum() - budget.used()).unwrap();
        let error = collector.process(&directory, pid, &mut batch).unwrap_err();
        assert!(error.get_ref().unwrap().is::<crate::budget::Error>());
        let error = record_process_result(&mut batch.coverage, Err(error)).unwrap_err();
        assert!(error.get_ref().unwrap().is::<crate::budget::Error>());
        assert_eq!(batch.coverage.omitted_at_least, 0);
        drop(held);
        assert!(record_process_result(
            &mut batch.coverage,
            Err(io::ErrorKind::FileTooLarge.into())
        )
        .unwrap());
        assert_eq!(batch.coverage.omitted_at_least, 1);
        assert!(
            record_process_result(&mut batch.coverage, Err(io::Error::other("internal"))).is_err()
        );
    }
    #[test]
    fn hotplug_malformed_rosters_and_missing_predecessors_create_gaps() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut old = MemoryVec::new(&budget, 4096).unwrap();
        let mut next = MemoryVec::new(&budget, 4096).unwrap();
        let mut aggregate = None;
        let first = b"cpu 10 0 0 90 0 0 0 0\ncpu0 10 0 0 90 0 0 0 0\n";
        let second = b"cpu 20 0 0 180 0 0 0 0\ncpu0 20 0 0 180 0 0 0 0\n";
        let hotplug = b"cpu 30 0 0 270 0 0 0 0\ncpu0 30 0 0 270 0 0 0 0\ncpu1 0 0 0 0 0 0 0 0\n";
        for (bytes, expected) in [
            (first.as_slice(), None),
            (second.as_slice(), Some(1000)),
            (hotplug.as_slice(), None),
        ] {
            let mut batch = Batch::new(&budget, 0).unwrap();
            cpu_sample(bytes, &mut aggregate, &mut old, &mut next, &mut batch);
            assert_eq!(batch.cpu.map(|cpu| cpu.busy), expected);
            if bytes == hotplug {
                assert_eq!(batch.cpus.last().unwrap().usage, None);
            }
        }
        let mut batch = Batch::new(&budget, 0).unwrap();
        cpu_sample(
            b"cpu 50 0 0 450 0 0 0 0\ncpu0 50 0 0 450 0 0 0 0\ncpu0 51 0 0 450 0 0 0 0\n",
            &mut aggregate,
            &mut old,
            &mut next,
            &mut batch,
        );
        assert!(batch.cpus.is_empty());
        assert!(batch.coverage.omitted_cpus >= 2);
        assert!(old.is_empty());
        let mut batch = Batch::new(&budget, 0).unwrap();
        cpu_sample(
            b"cpu 60 0 0 500 0 0 0 0\ncpu 70 0 0 600 0 0 0 0\n",
            &mut aggregate,
            &mut old,
            &mut next,
            &mut batch,
        );
        assert_eq!(batch.cpu, None);
        assert_eq!(aggregate, None);
    }
}
