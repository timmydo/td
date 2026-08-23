use crate::event::{user_frames, Event, Kind, StartIdentity};
use crate::raw;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;

pub const MAX_ANALYSIS_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_ANALYSIS_PROCESSES: usize = 65_536;
pub const MAX_ANALYSIS_MAPPINGS: usize = 262_144;
pub const MAX_ANALYSIS_STACKS: usize = 262_144;
pub const MAX_ANALYSIS_ERRORS: usize = 4096;
pub const MAX_CARRY_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_CARRY_EVENTS: usize = MAX_ANALYSIS_PROCESSES + MAX_ANALYSIS_MAPPINGS;
const BTREE_ENTRY_OVERHEAD: usize = 64;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ImageKey {
    pub pid: u32,
    pub start: StartIdentity,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mapping {
    pub address: u64,
    pub length: u64,
    pub page_offset: u64,
    pub major: u32,
    pub minor: u32,
    pub inode: u64,
    pub inode_generation: u64,
    pub path: Vec<u8>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Frame {
    pub address: u64,
    pub relative: Option<u64>,
    pub major: u32,
    pub minor: u32,
    pub inode: u64,
    pub inode_generation: u64,
    pub path: Vec<u8>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum StackState {
    Complete,
    Truncated(Vec<u8>),
    Unresolved(Vec<u8>),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct StackKey {
    pub image: ImageKey,
    pub tid: u32,
    pub state: StackState,
    pub frames: Vec<Frame>,
}

#[derive(Clone, Copy)]
enum ProspectiveStackState<'a> {
    Complete,
    Truncated(&'a [u8]),
    Unresolved(&'a [u8]),
}

impl ProspectiveStackState<'_> {
    fn retained_bytes(self) -> usize {
        match self {
            Self::Complete => 0,
            Self::Truncated(reason) | Self::Unresolved(reason) => reason.len(),
        }
    }

    fn materialize(self) -> StackState {
        match self {
            Self::Complete => StackState::Complete,
            Self::Truncated(reason) => StackState::Truncated(reason.to_vec()),
            Self::Unresolved(reason) => StackState::Unresolved(reason.to_vec()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProcessSummary {
    pub key: ImageKey,
    pub comm: Vec<u8>,
    pub observed: bool,
    pub valid_baseline: bool,
    pub exited: bool,
    pub samples: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnalysisError {
    pub cpu: u32,
    pub start_ns: u64,
    pub end_ns: u64,
    pub count: u64,
    pub message: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct Analysis {
    pub processes: BTreeMap<ImageKey, ProcessSummary>,
    pub stacks: BTreeMap<StackKey, u64>,
    pub lost_records: u64,
    pub corrupt_records: u64,
    pub task_records: u64,
    pub mapping_records: u64,
    pub switch_records: u64,
    pub sample_records: u64,
    pub ignored_records: u64,
    pub omitted_errors: u64,
    pub errors: Vec<AnalysisError>,
    pub carry: Vec<Event>,
}

#[derive(Clone)]
struct LiveProcess {
    start: StartIdentity,
    generation: u64,
    comm: Vec<u8>,
    valid_baseline: bool,
    observed: bool,
    exited: bool,
    mappings: Vec<Mapping>,
    invalid_reason: &'static [u8],
    last_mutation: Option<(u64, u32)>,
}

impl Default for LiveProcess {
    fn default() -> Self {
        Self {
            start: StartIdentity::Unknown,
            generation: 0,
            comm: Vec::new(),
            valid_baseline: false,
            observed: false,
            exited: false,
            mappings: Vec::new(),
            invalid_reason: b"invalid-task-baseline",
            last_mutation: None,
        }
    }
}

#[derive(Default)]
struct AnalysisBudget {
    used: usize,
}

impl AnalysisBudget {
    fn claim(&mut self, bytes: usize, label: &str) -> Result<(), String> {
        let next = self
            .used
            .checked_add(bytes)
            .ok_or_else(|| format!("{label} analysis budget overflow"))?;
        if next > MAX_ANALYSIS_BYTES {
            return Err(format!(
                "{label} expands analysis beyond {MAX_ANALYSIS_BYTES} bytes"
            ));
        }
        self.used = next;
        Ok(())
    }

    fn release(&mut self, bytes: usize, label: &str) -> Result<(), String> {
        self.used = self
            .used
            .checked_sub(bytes)
            .ok_or_else(|| format!("{label} analysis budget underflow"))?;
        Ok(())
    }
}

pub fn analyze(events: &[Event]) -> Result<Analysis, String> {
    let mut budget = AnalysisBudget::default();
    let order_bytes = events
        .len()
        .checked_mul(size_of::<&Event>())
        .ok_or("event-order analysis budget overflow")?;
    budget.claim(order_bytes, "event order")?;
    let mut ordered: Vec<&Event> = events.iter().collect();
    ordered.sort_by_key(|event| event.ordering_key());
    let ambiguous_mutations = ambiguous_mutations(&ordered, &mut budget)?;
    let global_uncertainty = global_uncertainty(&ordered, &mut budget)?;
    let mut live: BTreeMap<u32, LiveProcess> = BTreeMap::new();
    let mut out = Analysis::default();
    let mut mapping_count = 0usize;
    let mut prior_time = None;

    for event in &ordered {
        if prior_time != Some(event.time_ns) {
            if let Some(reason) = prior_time.and_then(|time| global_uncertainty.get(&time)) {
                for process in live.values_mut() {
                    invalidate(process, reason);
                }
            }
            prior_time = Some(event.time_ns);
        }
        match &event.kind {
            Kind::Task {
                start,
                generation,
                comm,
                valid,
            } => {
                out.task_records = out.task_records.saturating_add(1);
                require_process_slot(&live, event.pid, &mut budget)?;
                budget.claim(comm.len(), "task name")?;
                let process = live.entry(event.pid).or_default();
                if process.start != StartIdentity::Unknown && process.start != *start {
                    finish_process(event.pid, process, &mut out, &mut budget)?;
                    mapping_count = mapping_count.saturating_sub(process.mappings.len());
                    *process = LiveProcess::default();
                }
                process.start = start.clone();
                process.generation = *generation;
                process.comm = comm.clone();
                process.valid_baseline = *valid;
                process.invalid_reason = if *valid {
                    b""
                } else {
                    b"invalid-task-baseline"
                };
                process.observed = true;
            }
            Kind::Fork {
                parent_pid,
                parent_tid: _,
            } => {
                out.task_records = out.task_records.saturating_add(1);
                let inherited = if event.pid == event.tid {
                    live.get(parent_pid).map(|parent| {
                        (
                            parent.generation,
                            parent.comm.clone(),
                            parent.valid_baseline,
                            parent.invalid_reason,
                            parent.mappings.clone(),
                        )
                    })
                } else {
                    None
                };
                if let Some((_, comm, _, _, mappings)) = &inherited {
                    let bytes = mappings.iter().try_fold(comm.len(), |total, mapping| {
                        total
                            .checked_add(size_of::<Mapping>())
                            .and_then(|value| value.checked_add(mapping.path.len()))
                    });
                    budget.claim(
                        bytes.ok_or("fork inheritance analysis budget overflow")?,
                        "fork inheritance",
                    )?;
                    if mapping_count.saturating_add(mappings.len()) > MAX_ANALYSIS_MAPPINGS {
                        return Err(format!(
                            "fork inheritance exceeds {MAX_ANALYSIS_MAPPINGS} live mappings"
                        ));
                    }
                }
                require_process_slot(&live, event.pid, &mut budget)?;
                let process = live.entry(event.pid).or_default();
                if event.pid == event.tid {
                    if process.observed {
                        finish_process(event.pid, process, &mut out, &mut budget)?;
                        mapping_count = mapping_count.saturating_sub(process.mappings.len());
                        *process = LiveProcess::default();
                    }
                    process.start = StartIdentity::PerfTimeNs(event.time_ns);
                    if let Some((generation, comm, valid, reason, mappings)) = inherited {
                        mapping_count = mapping_count.saturating_add(mappings.len());
                        process.generation = generation;
                        process.comm = comm;
                        process.valid_baseline = valid;
                        process.invalid_reason = reason;
                        process.mappings = mappings;
                    }
                }
                note_mutation(process, event);
                if ambiguous_mutations.contains(&(event.pid, event.time_ns)) {
                    invalidate(process, b"cross-cpu-state-ambiguity");
                }
                process.observed = true;
            }
            Kind::Exit => {
                out.task_records = out.task_records.saturating_add(1);
                require_process_slot(&live, event.pid, &mut budget)?;
                let process = live.entry(event.pid).or_default();
                note_mutation(process, event);
                if ambiguous_mutations.contains(&(event.pid, event.time_ns)) {
                    invalidate(process, b"cross-cpu-state-ambiguity");
                }
                process.observed = true;
                process.exited |= event.pid == event.tid;
            }
            Kind::Comm { name, exec } => {
                out.task_records = out.task_records.saturating_add(1);
                require_process_slot(&live, event.pid, &mut budget)?;
                budget.claim(name.len(), "comm name")?;
                let process = live.entry(event.pid).or_default();
                process.observed = true;
                if *exec || event.tid == event.pid {
                    process.comm = name.clone();
                }
                if *exec {
                    let ambiguous = note_mutation(process, event)
                        || ambiguous_mutations.contains(&(event.pid, event.time_ns));
                    process.generation = process.generation.saturating_add(1);
                    mapping_count = mapping_count.saturating_sub(process.mappings.len());
                    process.mappings.clear();
                    if !ambiguous {
                        validate(process);
                    }
                }
            }
            Kind::Mmap {
                address,
                length,
                page_offset,
                major,
                minor,
                inode,
                inode_generation,
                path,
                synthetic,
            } => {
                out.mapping_records = out.mapping_records.saturating_add(1);
                require_process_slot(&live, event.pid, &mut budget)?;
                let process = live.entry(event.pid).or_default();
                if !synthetic {
                    note_mutation(process, event);
                    if ambiguous_mutations.contains(&(event.pid, event.time_ns)) {
                        invalidate(process, b"cross-cpu-state-ambiguity");
                    }
                }
                process.observed = true;
                if *length == 0 || address.checked_add(*length).is_none() {
                    invalidate(process, b"corrupt-mapping");
                    out.corrupt_records = out.corrupt_records.saturating_add(1);
                    continue;
                }
                budget.claim(size_of::<Mapping>().saturating_add(path.len()), "mapping")?;
                let before = process.mappings.len();
                replace_mapping(
                    process,
                    Mapping {
                        address: *address,
                        length: *length,
                        page_offset: *page_offset,
                        major: *major,
                        minor: *minor,
                        inode: *inode,
                        inode_generation: *inode_generation,
                        path: path.clone(),
                    },
                    &mut budget,
                )?;
                mapping_count = mapping_count
                    .saturating_sub(before)
                    .saturating_add(process.mappings.len());
                if mapping_count > MAX_ANALYSIS_MAPPINGS {
                    return Err(format!(
                        "analysis exceeds {MAX_ANALYSIS_MAPPINGS} live mappings"
                    ));
                }
            }
            Kind::Sample { ip, callchain } => {
                out.sample_records = out.sample_records.saturating_add(1);
                require_process_slot(&live, event.pid, &mut budget)?;
                let process = live.entry(event.pid).or_default();
                if let Some(reason) = global_uncertainty.get(&event.time_ns) {
                    invalidate(process, reason);
                }
                if ambiguous_mutations.contains(&(event.pid, event.time_ns)) {
                    invalidate(process, b"cross-cpu-state-ambiguity");
                }
                process.observed = true;
                let image = key(event.pid, process);
                let address_bytes = callchain
                    .len()
                    .checked_add(1)
                    .and_then(|count| count.checked_mul(size_of::<u64>()))
                    .ok_or("sample address roster budget overflow")?;
                budget.claim(address_bytes, "sample address roster")?;
                let addresses = user_frames(*ip, callchain);
                let mapping_roster_bytes = addresses
                    .len()
                    .checked_mul(size_of::<Option<&Mapping>>())
                    .ok_or("sample mapping roster budget overflow")?;
                budget.claim(mapping_roster_bytes, "sample mapping roster")?;
                let mappings: Vec<Option<&Mapping>> = addresses
                    .iter()
                    .map(|address| mapping_for(*address, &process.mappings))
                    .collect();
                let state = if !process.valid_baseline {
                    ProspectiveStackState::Unresolved(process.invalid_reason)
                } else if addresses.is_empty() {
                    ProspectiveStackState::Unresolved(b"empty-callchain")
                } else if mappings.iter().any(Option::is_none) {
                    ProspectiveStackState::Truncated(b"unmapped-address")
                } else {
                    ProspectiveStackState::Complete
                };
                let stack_bytes = prospective_stack_heap_bytes(&mappings, &state)?;
                budget.claim(stack_bytes, "sample stack materialization")?;
                let frames: Vec<Frame> = addresses
                    .iter()
                    .zip(mappings.iter())
                    .map(|(address, mapping)| resolve(*address, *mapping))
                    .collect();
                let stack = StackKey {
                    image: image.clone(),
                    tid: event.tid,
                    state: state.materialize(),
                    frames,
                };
                drop(mappings);
                drop(addresses);
                let lookup_roster_bytes = address_bytes
                    .checked_add(mapping_roster_bytes)
                    .ok_or("sample lookup roster budget overflow")?;
                budget.release(lookup_roster_bytes, "sample lookup rosters")?;
                retain_sample_stack(&mut out.stacks, stack, stack_bytes, &mut budget)?;
                let summary = match out.processes.entry(image.clone()) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        budget.claim(
                            size_of::<ProcessSummary>()
                                .saturating_add(process.comm.len())
                                .saturating_add(BTREE_ENTRY_OVERHEAD),
                            "process summary",
                        )?;
                        entry.insert(ProcessSummary {
                            key: image,
                            comm: process.comm.clone(),
                            observed: true,
                            valid_baseline: process.valid_baseline,
                            exited: process.exited,
                            samples: 0,
                        })
                    }
                };
                summary.samples = summary.samples.saturating_add(1);
            }
            Kind::Switch { .. } => {
                out.switch_records = out.switch_records.saturating_add(1);
            }
            Kind::Lost { count, reason } => {
                out.lost_records = out.lost_records.saturating_add(*count);
                record_error(&mut out, &mut budget, event, *count, reason)?;
                for process in live.values_mut() {
                    invalidate(process, b"event-loss");
                }
            }
            Kind::Error { message } => {
                out.corrupt_records = out.corrupt_records.saturating_add(1);
                record_error(&mut out, &mut budget, event, 1, message)?;
                for process in live.values_mut() {
                    invalidate(process, b"corrupt-event");
                }
            }
            Kind::Ignored { .. } => {
                out.ignored_records = out.ignored_records.saturating_add(1);
            }
        }
    }
    if let Some(reason) = prior_time.and_then(|time| global_uncertainty.get(&time)) {
        for process in live.values_mut() {
            invalidate(process, reason);
        }
    }
    let end_ns = ordered.last().map(|event| event.time_ns).unwrap_or(0);
    match compact_live(&live, end_ns) {
        Ok(carry) => out.carry = carry,
        Err(error) => {
            let message = format!("carry-forward omitted: {error}");
            let diagnostic = Event {
                time_ns: end_ns,
                cpu: 0,
                sequence: u64::MAX,
                pid: 0,
                tid: 0,
                kind: Kind::Error {
                    message: message.as_bytes().to_vec(),
                },
            };
            if record_error(&mut out, &mut budget, &diagnostic, 1, message.as_bytes()).is_err() {
                out.omitted_errors = out.omitted_errors.saturating_add(1);
            }
        }
    }
    for (pid, process) in &live {
        finish_process(*pid, process, &mut out, &mut budget)?;
    }
    Ok(out)
}

fn global_uncertainty(
    events: &[&Event],
    budget: &mut AnalysisBudget,
) -> Result<BTreeMap<u64, &'static [u8]>, String> {
    let mut times = BTreeMap::new();
    for event in events {
        let reason: &'static [u8] = match event.kind {
            Kind::Lost { .. } => b"event-loss",
            Kind::Error { .. } => b"corrupt-event",
            _ => continue,
        };
        if let Entry::Vacant(entry) = times.entry(event.time_ns) {
            budget.claim(
                size_of::<(u64, &'static [u8])>().saturating_add(BTREE_ENTRY_OVERHEAD),
                "global uncertainty times",
            )?;
            entry.insert(reason);
        }
    }
    Ok(times)
}

fn ambiguous_mutations(
    events: &[&Event],
    budget: &mut AnalysisBudget,
) -> Result<BTreeSet<(u32, u64)>, String> {
    let mut first_cpu: BTreeMap<(u32, u64), (Option<u32>, Option<u32>)> = BTreeMap::new();
    let mut ambiguous = BTreeSet::new();
    for event in events {
        let mutation = matches!(
            event.kind,
            Kind::Fork { .. } | Kind::Exit | Kind::Comm { exec: true, .. }
        ) || matches!(
            event.kind,
            Kind::Mmap {
                synthetic: false,
                ..
            }
        );
        let sample = matches!(event.kind, Kind::Sample { .. });
        if !mutation && !sample {
            continue;
        }
        let key = (event.pid, event.time_ns);
        match first_cpu.entry(key) {
            Entry::Vacant(entry) => {
                budget.claim(96, "mutation ordering")?;
                entry.insert((mutation.then_some(event.cpu), sample.then_some(event.cpu)));
            }
            Entry::Occupied(mut entry) => {
                let (mutation_cpu, sample_cpu) = entry.get_mut();
                let conflicts = if mutation {
                    mutation_cpu.is_some_and(|cpu| cpu != event.cpu)
                        || sample_cpu.is_some_and(|cpu| cpu != event.cpu)
                } else {
                    mutation_cpu.is_some_and(|cpu| cpu != event.cpu)
                };
                if conflicts && ambiguous.insert(key) {
                    budget.claim(96, "ambiguous mutation ordering")?;
                }
                if mutation_cpu.is_none() && mutation {
                    *mutation_cpu = Some(event.cpu);
                }
                if sample_cpu.is_none() && sample {
                    *sample_cpu = Some(event.cpu);
                }
            }
        }
    }
    Ok(ambiguous)
}

fn record_error(
    out: &mut Analysis,
    budget: &mut AnalysisBudget,
    event: &Event,
    count: u64,
    message: &[u8],
) -> Result<(), String> {
    if out.errors.len() >= MAX_ANALYSIS_ERRORS {
        out.omitted_errors = out.omitted_errors.saturating_add(1);
        return Ok(());
    }
    budget.claim(
        size_of::<AnalysisError>().saturating_add(message.len()),
        "contextual diagnostic",
    )?;
    out.errors.push(AnalysisError {
        cpu: event.cpu,
        start_ns: event.time_ns,
        end_ns: event.time_ns,
        count,
        message: message.to_vec(),
    });
    Ok(())
}

fn compact_live(live: &BTreeMap<u32, LiveProcess>, time_ns: u64) -> Result<Vec<Event>, String> {
    let mut carry = Vec::new();
    let mut sequence = 0u64;
    let mut bytes = raw::FILE_HEADER_BYTES as u64;
    for (pid, process) in live {
        if !process.observed || process.exited {
            continue;
        }
        push_carry(
            &mut carry,
            &mut bytes,
            Event {
                time_ns,
                cpu: 0,
                sequence,
                pid: *pid,
                tid: *pid,
                kind: Kind::Task {
                    start: process.start.clone(),
                    generation: process.generation,
                    comm: process.comm.clone(),
                    valid: process.valid_baseline,
                },
            },
        )?;
        sequence = sequence.saturating_add(1);
        for mapping in &process.mappings {
            push_carry(
                &mut carry,
                &mut bytes,
                Event {
                    time_ns,
                    cpu: 0,
                    sequence,
                    pid: *pid,
                    tid: *pid,
                    kind: Kind::Mmap {
                        address: mapping.address,
                        length: mapping.length,
                        page_offset: mapping.page_offset,
                        major: mapping.major,
                        minor: mapping.minor,
                        inode: mapping.inode,
                        inode_generation: mapping.inode_generation,
                        path: mapping.path.clone(),
                        synthetic: true,
                    },
                },
            )?;
            sequence = sequence.saturating_add(1);
        }
    }
    Ok(carry)
}

fn push_carry(events: &mut Vec<Event>, bytes: &mut u64, event: Event) -> Result<(), String> {
    if events.len() >= MAX_CARRY_EVENTS {
        return Err(format!(
            "live state exceeds {MAX_CARRY_EVENTS} carry records"
        ));
    }
    let length = u64::try_from(raw::encoded_len(&event).map_err(|e| e.to_string())?)
        .map_err(|_| "carry record length does not fit u64")?;
    let next = bytes
        .checked_add(length)
        .ok_or("carry byte count overflow")?;
    if next > MAX_CARRY_BYTES {
        return Err(format!("live state exceeds {MAX_CARRY_BYTES} carry bytes"));
    }
    *bytes = next;
    events.push(event);
    Ok(())
}

fn note_mutation(process: &mut LiveProcess, event: &Event) -> bool {
    let ambiguous = process
        .last_mutation
        .map(|(time, cpu)| time == event.time_ns && cpu != event.cpu)
        .unwrap_or(false);
    if ambiguous {
        invalidate(process, b"cross-cpu-state-ambiguity");
    }
    process.last_mutation = Some((event.time_ns, event.cpu));
    ambiguous
}

fn invalidate(process: &mut LiveProcess, reason: &'static [u8]) {
    process.valid_baseline = false;
    process.invalid_reason = reason;
}

fn validate(process: &mut LiveProcess) {
    process.valid_baseline = true;
    process.invalid_reason = b"";
}

fn require_process_slot(
    live: &BTreeMap<u32, LiveProcess>,
    pid: u32,
    budget: &mut AnalysisBudget,
) -> Result<(), String> {
    if !live.contains_key(&pid) && live.len() >= MAX_ANALYSIS_PROCESSES {
        return Err(format!(
            "analysis exceeds {MAX_ANALYSIS_PROCESSES} live processes"
        ));
    }
    if !live.contains_key(&pid) {
        budget.claim(
            size_of::<LiveProcess>().saturating_add(BTREE_ENTRY_OVERHEAD),
            "live process",
        )?;
    }
    Ok(())
}

fn key(pid: u32, process: &LiveProcess) -> ImageKey {
    ImageKey {
        pid,
        start: process.start.clone(),
        generation: process.generation,
    }
}

fn finish_process(
    pid: u32,
    process: &LiveProcess,
    out: &mut Analysis,
    budget: &mut AnalysisBudget,
) -> Result<(), String> {
    if !process.observed {
        return Ok(());
    }
    let image = key(pid, process);
    match out.processes.entry(image.clone()) {
        Entry::Occupied(mut entry) => {
            let summary = entry.get_mut();
            summary.exited |= process.exited;
            summary.valid_baseline &= process.valid_baseline;
        }
        Entry::Vacant(entry) => {
            budget.claim(
                size_of::<ProcessSummary>()
                    .saturating_add(process.comm.len())
                    .saturating_add(BTREE_ENTRY_OVERHEAD),
                "process summary",
            )?;
            entry.insert(ProcessSummary {
                key: image,
                comm: process.comm.clone(),
                observed: true,
                valid_baseline: process.valid_baseline,
                exited: process.exited,
                samples: 0,
            });
        }
    }
    Ok(())
}

fn replace_mapping(
    process: &mut LiveProcess,
    new: Mapping,
    budget: &mut AnalysisBudget,
) -> Result<(), String> {
    let start = new.address;
    let end = start
        .checked_add(new.length)
        .ok_or("mapping end overflow after validation")?;
    let old = std::mem::take(&mut process.mappings);
    let mut next = Vec::with_capacity(old.len().saturating_add(1));
    for mut mapping in old {
        let old_end = mapping
            .address
            .checked_add(mapping.length)
            .ok_or("stored mapping end overflow")?;
        if old_end <= start || mapping.address >= end {
            next.push(mapping);
            continue;
        }
        let keep_left = mapping.address < start;
        let keep_right = old_end > end;
        if keep_left && keep_right {
            budget.claim(
                size_of::<Mapping>().saturating_add(mapping.path.len()),
                "split mapping",
            )?;
            let mut right = mapping.clone();
            right.address = end;
            right.length = old_end.saturating_sub(end);
            right.page_offset = right
                .page_offset
                .checked_add(end.saturating_sub(mapping.address))
                .ok_or("split mapping file offset overflow")?;
            mapping.length = start.saturating_sub(mapping.address);
            next.push(mapping);
            next.push(right);
        } else if keep_left {
            mapping.length = start.saturating_sub(mapping.address);
            next.push(mapping);
        } else if keep_right {
            mapping.page_offset = mapping
                .page_offset
                .checked_add(end.saturating_sub(mapping.address))
                .ok_or("trimmed mapping file offset overflow")?;
            mapping.address = end;
            mapping.length = old_end.saturating_sub(end);
            next.push(mapping);
        }
    }
    next.push(new);
    next.sort_by_key(|mapping| mapping.address);
    process.mappings = next;
    Ok(())
}

fn mapping_for(address: u64, mappings: &[Mapping]) -> Option<&Mapping> {
    let at = mappings
        .partition_point(|mapping| mapping.address <= address)
        .checked_sub(1)?;
    mappings.get(at).filter(|mapping| {
        mapping
            .address
            .checked_add(mapping.length)
            .is_some_and(|end| address < end)
    })
}

fn prospective_stack_heap_bytes(
    mappings: &[Option<&Mapping>],
    state: &ProspectiveStackState<'_>,
) -> Result<usize, String> {
    let state_bytes = state.retained_bytes();
    mappings
        .iter()
        .try_fold(
            size_of::<StackKey>()
                .checked_add(BTREE_ENTRY_OVERHEAD)
                .and_then(|value| value.checked_add(state_bytes))
                .ok_or("stack analysis budget overflow")?,
            |total, mapping| {
                total.checked_add(size_of::<Frame>()).and_then(|value| {
                    value.checked_add(mapping.map_or(0, |mapping| mapping.path.len()))
                })
            },
        )
        .ok_or("stack frame analysis budget overflow".into())
}

fn retain_sample_stack(
    stacks: &mut BTreeMap<StackKey, u64>,
    stack: StackKey,
    stack_bytes: usize,
    budget: &mut AnalysisBudget,
) -> Result<(), String> {
    let at_stack_limit = stacks.len() >= MAX_ANALYSIS_STACKS;
    match stacks.entry(stack) {
        Entry::Occupied(mut entry) => {
            let count = entry.get().saturating_add(1);
            *entry.get_mut() = count;
            budget.release(stack_bytes, "duplicate sample stack")
        }
        Entry::Vacant(entry) => {
            if at_stack_limit {
                return Err(format!(
                    "analysis exceeds {MAX_ANALYSIS_STACKS} distinct stacks"
                ));
            }
            entry.insert(1);
            Ok(())
        }
    }
}

fn resolve(address: u64, mapping: Option<&Mapping>) -> Frame {
    match mapping {
        Some(mapping) => Frame {
            address,
            relative: address
                .checked_sub(mapping.address)
                .and_then(|offset| mapping.page_offset.checked_add(offset)),
            major: mapping.major,
            minor: mapping.minor,
            inode: mapping.inode,
            inode_generation: mapping.inode_generation,
            path: mapping.path.clone(),
        },
        None => Frame {
            address,
            relative: None,
            major: 0,
            minor: 0,
            inode: 0,
            inode_generation: 0,
            path: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{
        analyze, retain_sample_stack, AnalysisBudget, Frame, ImageKey, StackKey, StackState,
        BTREE_ENTRY_OVERHEAD, MAX_ANALYSIS_BYTES,
    };
    use crate::event::{Event, Kind, StartIdentity};
    use std::collections::BTreeMap;

    fn event(time_ns: u64, cpu: u32, sequence: u64, kind: Kind) -> Event {
        Event {
            time_ns,
            cpu,
            sequence,
            pid: 7,
            tid: 7,
            kind,
        }
    }

    fn task(time_ns: u64) -> Event {
        event(
            time_ns,
            0,
            0,
            Kind::Task {
                start: StartIdentity::ProcTicks(5),
                generation: 0,
                comm: b"work".to_vec(),
                valid: true,
            },
        )
    }

    fn mapping(time_ns: u64, address: u64, path: &[u8]) -> Event {
        event(
            time_ns,
            0,
            1,
            Kind::Mmap {
                address,
                length: 0x1000,
                page_offset: 0,
                major: 1,
                minor: 2,
                inode: address,
                inode_generation: 0,
                path: path.to_vec(),
                synthetic: false,
            },
        )
    }

    #[test]
    fn exec_generations_do_not_rewrite_earlier_samples() {
        let events = vec![
            task(1),
            mapping(2, 0x1000, b"old"),
            event(
                3,
                0,
                2,
                Kind::Sample {
                    ip: 0x1010,
                    callchain: vec![],
                },
            ),
            event(
                4,
                0,
                3,
                Kind::Comm {
                    name: b"new".to_vec(),
                    exec: true,
                },
            ),
            mapping(5, 0x1000, b"new"),
            event(
                6,
                0,
                4,
                Kind::Sample {
                    ip: 0x1010,
                    callchain: vec![],
                },
            ),
        ];
        let analysis = analyze(&events).unwrap();
        assert_eq!(analysis.stacks.len(), 2);
        let generations: Vec<u64> = analysis
            .stacks
            .keys()
            .map(|stack| stack.image.generation)
            .collect();
        assert_eq!(generations, vec![0, 1]);
        let paths: Vec<&[u8]> = analysis
            .stacks
            .keys()
            .filter_map(|stack| stack.frames.first().map(|frame| frame.path.as_slice()))
            .collect();
        assert_eq!(paths, vec![b"old".as_slice(), b"new".as_slice()]);
    }

    #[test]
    fn merge_order_is_time_cpu_sequence_and_conflicts_are_explicit() {
        let mut exec = event(
            10,
            1,
            0,
            Kind::Comm {
                name: b"exec".to_vec(),
                exec: true,
            },
        );
        exec.pid = 7;
        let mut map = mapping(10, 0x2000, b"map");
        map.cpu = 0;
        let sample = event(
            10,
            2,
            0,
            Kind::Sample {
                ip: 0x2010,
                callchain: vec![],
            },
        );
        let analysis = analyze(&[sample, exec, map, task(1)]).unwrap();
        let stack = analysis.stacks.keys().next().unwrap();
        assert_eq!(
            stack.state,
            StackState::Unresolved(b"cross-cpu-state-ambiguity".to_vec())
        );
    }

    #[test]
    fn equal_time_conflicts_cover_earlier_cpu_samples_and_persist() {
        let mut map = mapping(10, 0x2000, b"map");
        map.cpu = 0;
        map.sequence = 0;
        let same_time_sample = event(
            10,
            0,
            1,
            Kind::Sample {
                ip: 0x2010,
                callchain: vec![],
            },
        );
        let exec = event(
            10,
            1,
            0,
            Kind::Comm {
                name: b"exec".to_vec(),
                exec: true,
            },
        );
        let later_sample = event(
            11,
            0,
            2,
            Kind::Sample {
                ip: 0x2010,
                callchain: vec![],
            },
        );
        let analysis = analyze(&[task(1), same_time_sample, exec, map, later_sample]).unwrap();
        assert_eq!(analysis.sample_records, 2);
        assert!(analysis.stacks.keys().all(|stack| {
            stack.state == StackState::Unresolved(b"cross-cpu-state-ambiguity".to_vec())
        }));
    }

    #[test]
    fn one_cross_cpu_mutation_is_ambiguous_with_a_same_time_sample() {
        let sample = event(
            10,
            0,
            0,
            Kind::Sample {
                ip: 0x1010,
                callchain: vec![],
            },
        );
        let exec = event(
            10,
            1,
            0,
            Kind::Comm {
                name: b"new".to_vec(),
                exec: true,
            },
        );
        let analysis = analyze(&[task(1), mapping(2, 0x1000, b"old"), sample, exec]).unwrap();
        assert!(analysis.stacks.keys().all(|stack| {
            stack.state == StackState::Unresolved(b"cross-cpu-state-ambiguity".to_vec())
        }));
    }

    #[test]
    fn thread_comm_does_not_rename_the_process_image() {
        let mut thread_comm = event(
            2,
            0,
            1,
            Kind::Comm {
                name: b"thread".to_vec(),
                exec: false,
            },
        );
        thread_comm.tid = 8;
        let analysis = analyze(&[
            task(1),
            thread_comm,
            event(
                3,
                0,
                2,
                Kind::Sample {
                    ip: 0x10,
                    callchain: vec![],
                },
            ),
        ])
        .unwrap();
        assert!(analysis
            .processes
            .values()
            .all(|process| process.comm == b"work"));
    }

    #[test]
    fn contextual_diagnostics_are_capped_without_losing_the_overflow_count() {
        let mut events = Vec::new();
        for sequence in 0..=super::MAX_ANALYSIS_ERRORS as u64 {
            events.push(event(
                sequence,
                0,
                sequence,
                Kind::Error {
                    message: b"bad".to_vec(),
                },
            ));
        }
        let analysis = analyze(&events).unwrap();
        assert_eq!(analysis.errors.len(), super::MAX_ANALYSIS_ERRORS);
        assert_eq!(analysis.omitted_errors, 1);
    }

    #[test]
    fn invalid_baselines_and_loss_never_look_complete() {
        let mut invalid = task(1);
        invalid.kind = Kind::Task {
            start: StartIdentity::ProcTicks(5),
            generation: 0,
            comm: b"work".to_vec(),
            valid: false,
        };
        let analysis = analyze(&[
            invalid,
            event(
                2,
                0,
                1,
                Kind::Sample {
                    ip: 0x10,
                    callchain: vec![],
                },
            ),
            event(
                3,
                0,
                2,
                Kind::Lost {
                    count: 9,
                    reason: b"ring-overrun".to_vec(),
                },
            ),
        ])
        .unwrap();
        assert_eq!(analysis.lost_records, 9);
        assert!(matches!(
            analysis.stacks.keys().next().unwrap().state,
            StackState::Unresolved(_)
        ));
    }

    #[test]
    fn loss_invalidates_live_state_and_the_compact_next_capture_baseline() {
        let events = vec![
            task(1),
            mapping(2, 0x1000, b"mapped"),
            event(
                3,
                0,
                2,
                Kind::Lost {
                    count: 2,
                    reason: b"ring-overrun".to_vec(),
                },
            ),
            event(
                4,
                0,
                3,
                Kind::Sample {
                    ip: 0x1010,
                    callchain: vec![],
                },
            ),
        ];
        let analysis = analyze(&events).unwrap();
        assert!(analysis
            .stacks
            .keys()
            .all(|stack| { stack.state == StackState::Unresolved(b"event-loss".to_vec()) }));
        let mut next = analysis.carry;
        next.push(event(
            5,
            0,
            4,
            Kind::Sample {
                ip: 0x1010,
                callchain: vec![],
            },
        ));
        let next = analyze(&next).unwrap();
        assert!(matches!(
            next.stacks.keys().next().map(|stack| &stack.state),
            Some(StackState::Unresolved(reason)) if reason == b"invalid-task-baseline"
        ));
    }

    #[test]
    fn equal_time_loss_invalidates_a_lower_cpu_sample_before_cpu_ordering() {
        let analysis = analyze(&[
            task(1),
            mapping(2, 0x1000, b"mapped"),
            event(
                3,
                0,
                2,
                Kind::Sample {
                    ip: 0x1010,
                    callchain: vec![],
                },
            ),
            event(
                3,
                1,
                0,
                Kind::Lost {
                    count: 1,
                    reason: b"ring-overrun".to_vec(),
                },
            ),
        ])
        .unwrap();
        assert!(matches!(
            analysis.stacks.keys().next().map(|stack| &stack.state),
            Some(StackState::Unresolved(reason)) if reason == b"event-loss"
        ));
    }

    #[test]
    fn carry_overflow_keeps_the_current_analysis_and_records_the_omission() {
        let mut huge = mapping(2, 0x1000, &vec![b'x'; super::MAX_CARRY_BYTES as usize]);
        if let Kind::Mmap { length, .. } = &mut huge.kind {
            *length = 0x1000;
        }
        let analysis = analyze(&[task(1), huge]).unwrap();
        assert!(analysis.carry.is_empty());
        assert!(analysis
            .errors
            .iter()
            .any(|error| error.message.starts_with(b"carry-forward omitted:")));
    }

    #[test]
    fn repeated_samples_charge_one_retained_stack() {
        let callchain: Vec<u64> = (0x1002..0x1029).collect();
        let mut events = vec![task(1), mapping(2, 0x1000, b"mapped")];
        for sequence in 0..30_000 {
            events.push(event(
                3 + sequence,
                0,
                2 + sequence,
                Kind::Sample {
                    ip: 0x1001,
                    callchain: callchain.clone(),
                },
            ));
        }
        let analysis = analyze(&events).unwrap();
        assert_eq!(analysis.stacks.len(), 1);
        assert_eq!(analysis.stacks.values().next(), Some(&30_000));
    }

    #[test]
    fn hostile_long_path_callchain_is_rejected_before_frame_clones() {
        let mut events = vec![task(1)];
        events.push(event(
            2,
            0,
            1,
            Kind::Mmap {
                address: 0x1000,
                length: 0x2000,
                page_offset: 0,
                major: 1,
                minor: 2,
                inode: 3,
                inode_generation: 0,
                path: vec![b'x'; 64 * 1024],
                synthetic: false,
            },
        ));
        events.push(event(
            3,
            0,
            2,
            Kind::Sample {
                ip: 0x1000,
                callchain: (0x1001..=0x2000).collect(),
            },
        ));
        assert!(analyze(&events)
            .unwrap_err()
            .contains("sample stack materialization expands analysis"));
    }

    #[test]
    fn duplicate_stack_releases_its_near_ceiling_materialization() {
        let stack = StackKey {
            image: ImageKey {
                pid: 7,
                start: StartIdentity::ProcTicks(5),
                generation: 0,
            },
            tid: 7,
            state: StackState::Complete,
            frames: vec![Frame {
                address: 1,
                relative: Some(1),
                major: 1,
                minor: 2,
                inode: 3,
                inode_generation: 0,
                path: b"mapped".to_vec(),
            }],
        };
        let stack_bytes = std::mem::size_of::<StackKey>()
            + std::mem::size_of::<Frame>()
            + stack.frames.first().unwrap().path.len()
            + BTREE_ENTRY_OVERHEAD;
        let mut stacks = BTreeMap::from([(stack.clone(), 1)]);
        let mut budget = AnalysisBudget {
            used: MAX_ANALYSIS_BYTES - stack_bytes,
        };
        budget
            .claim(stack_bytes, "duplicate stack materialization")
            .unwrap();
        retain_sample_stack(&mut stacks, stack, stack_bytes, &mut budget).unwrap();
        assert_eq!(stacks.values().next(), Some(&2));
        assert_eq!(budget.used, MAX_ANALYSIS_BYTES - stack_bytes);
        budget.claim(stack_bytes, "following stack").unwrap();
    }

    #[test]
    fn a_prior_loss_fence_invalidates_later_cross_cpu_samples() {
        let analysis = analyze(&[
            task(1),
            mapping(2, 0x1000, b"mapped"),
            event(
                3,
                1,
                2,
                Kind::Lost {
                    count: 1,
                    reason: b"discarded-final-ring".to_vec(),
                },
            ),
            event(
                4,
                0,
                3,
                Kind::Sample {
                    ip: 0x1010,
                    callchain: vec![],
                },
            ),
        ])
        .unwrap();
        assert!(matches!(
            analysis.stacks.keys().next().map(|stack| &stack.state),
            Some(StackState::Unresolved(reason)) if reason == b"event-loss"
        ));
    }

    #[test]
    fn leader_forks_inherit_the_parent_mapping_snapshot() {
        let mut fork = event(
            3,
            0,
            2,
            Kind::Fork {
                parent_pid: 7,
                parent_tid: 7,
            },
        );
        fork.pid = 8;
        fork.tid = 8;
        let mut sample = event(
            4,
            0,
            3,
            Kind::Sample {
                ip: 0x1010,
                callchain: vec![],
            },
        );
        sample.pid = 8;
        sample.tid = 8;
        let analysis = analyze(&[task(1), mapping(2, 0x1000, b"parent"), fork, sample]).unwrap();
        let stack = analysis.stacks.keys().next().unwrap();
        assert_eq!(stack.frames.first().unwrap().path, b"parent");
        assert_eq!(stack.frames.first().unwrap().relative, Some(0x10));
    }

    #[test]
    fn partial_mapping_replacement_preserves_both_old_remainders() {
        let old = event(
            2,
            0,
            1,
            Kind::Mmap {
                address: 0x1000,
                length: 0x3000,
                page_offset: 0x200,
                major: 1,
                minor: 2,
                inode: 1,
                inode_generation: 0,
                path: b"old".to_vec(),
                synthetic: false,
            },
        );
        let replacement = event(
            3,
            0,
            2,
            Kind::Mmap {
                address: 0x1800,
                length: 0x800,
                page_offset: 0x500,
                major: 1,
                minor: 2,
                inode: 2,
                inode_generation: 0,
                path: b"new".to_vec(),
                synthetic: false,
            },
        );
        let mut events = vec![task(1), old, replacement];
        for (sequence, address) in [0x1100, 0x1900, 0x2100].into_iter().enumerate() {
            events.push(event(
                4 + sequence as u64,
                0,
                3 + sequence as u64,
                Kind::Sample {
                    ip: address,
                    callchain: vec![],
                },
            ));
        }
        let analysis = analyze(&events).unwrap();
        let resolved: BTreeMap<u64, (&[u8], Option<u64>)> = analysis
            .stacks
            .keys()
            .map(|stack| {
                let frame = stack.frames.first().unwrap();
                (frame.address, (frame.path.as_slice(), frame.relative))
            })
            .collect();
        assert_eq!(
            resolved.get(&0x1100),
            Some(&(b"old".as_slice(), Some(0x300)))
        );
        assert_eq!(
            resolved.get(&0x1900),
            Some(&(b"new".as_slice(), Some(0x600)))
        );
        assert_eq!(
            resolved.get(&0x2100),
            Some(&(b"old".as_slice(), Some(0x1300)))
        );
    }

    #[test]
    fn a_leader_fork_replaces_a_reused_pid_but_thread_exit_does_not_exit_it() {
        let mut thread_exit = event(2, 0, 1, Kind::Exit);
        thread_exit.tid = 8;
        let leader_fork = event(
            4,
            0,
            3,
            Kind::Fork {
                parent_pid: 1,
                parent_tid: 1,
            },
        );
        let analysis = analyze(&[
            task(1),
            thread_exit,
            event(
                3,
                0,
                2,
                Kind::Sample {
                    ip: 0x10,
                    callchain: vec![],
                },
            ),
            leader_fork,
            event(
                5,
                0,
                4,
                Kind::Sample {
                    ip: 0x20,
                    callchain: vec![],
                },
            ),
        ])
        .unwrap();
        let starts: Vec<_> = analysis
            .processes
            .values()
            .map(|process| (process.key.start.clone(), process.exited))
            .collect();
        assert_eq!(
            starts,
            vec![
                (StartIdentity::ProcTicks(5), false),
                (StartIdentity::PerfTimeNs(4), false)
            ]
        );
    }

    #[test]
    fn carry_is_a_stable_snapshot_not_unbounded_history() {
        let events = vec![
            task(1),
            mapping(2, 0x1000, b"old"),
            event(
                3,
                0,
                2,
                Kind::Comm {
                    name: b"new".to_vec(),
                    exec: true,
                },
            ),
            mapping(4, 0x2000, b"new"),
        ];
        let first = analyze(&events).unwrap().carry;
        assert_eq!(first.len(), 2);
        assert!(matches!(
            first.first().map(|event| &event.kind),
            Some(Kind::Task {
                start: StartIdentity::ProcTicks(5),
                generation: 1,
                comm,
                valid: true,
            }) if comm == b"new"
        ));
        assert!(matches!(
            first.get(1).map(|event| &event.kind),
            Some(Kind::Mmap { address: 0x2000, path, synthetic: true, .. })
                if path == b"new"
        ));
        assert_eq!(analyze(&first).unwrap().carry, first);

        let mut exited = first;
        exited.push(event(5, 0, 3, Kind::Exit));
        assert!(analyze(&exited).unwrap().carry.is_empty());
    }
}
