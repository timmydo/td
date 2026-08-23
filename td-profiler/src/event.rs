#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Event {
    pub time_ns: u64,
    pub cpu: u32,
    pub sequence: u64,
    pub pid: u32,
    pub tid: u32,
    pub kind: Kind,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum StartIdentity {
    Unknown,
    ProcTicks(u64),
    PerfTimeNs(u64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Kind {
    Task {
        start: StartIdentity,
        generation: u64,
        comm: Vec<u8>,
        valid: bool,
    },
    Fork {
        parent_pid: u32,
        parent_tid: u32,
    },
    Exit,
    Comm {
        name: Vec<u8>,
        exec: bool,
    },
    Mmap {
        address: u64,
        length: u64,
        page_offset: u64,
        major: u32,
        minor: u32,
        inode: u64,
        inode_generation: u64,
        path: Vec<u8>,
        synthetic: bool,
    },
    Sample {
        ip: u64,
        callchain: Vec<u64>,
    },
    Switch {
        out: bool,
        preempt: bool,
    },
    Lost {
        count: u64,
        reason: Vec<u8>,
    },
    Error {
        message: Vec<u8>,
    },
    Ignored {
        perf_kind: u32,
    },
}

impl Event {
    pub fn ordering_key(&self) -> (u64, u8, u32, u64) {
        let synthetic_baseline = matches!(self.kind, Kind::Task { .. })
            || matches!(
                self.kind,
                Kind::Mmap {
                    synthetic: true,
                    ..
                }
            );
        (
            self.time_ns,
            u8::from(!synthetic_baseline),
            self.cpu,
            self.sequence,
        )
    }
}

pub const PERF_CONTEXT_MAX: u64 = u64::MAX - 4094;

pub fn user_frames(ip: u64, callchain: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(callchain.len().saturating_add(1));
    if ip != 0 {
        out.push(ip);
    }
    let mut in_user = true;
    let mut first_user_frame = true;
    for address in callchain {
        if *address >= PERF_CONTEXT_MAX {
            // PERF_CONTEXT_USER is -512, PERF_CONTEXT_KERNEL is -128. The
            // sample event excludes kernel execution, but retaining the context
            // transition logic makes malformed or future records fail closed.
            in_user = *address == (u64::MAX - 511);
        } else if in_user && *address != 0 {
            if first_user_frame {
                first_user_frame = false;
                if out.first() == Some(address) {
                    continue;
                }
            }
            out.push(*address);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::user_frames;

    #[test]
    fn only_the_kernel_repeated_leaf_is_removed() {
        assert_eq!(
            user_frames(0x10, &[0x10, 0x20, 0x20, 0x30]),
            vec![0x10, 0x20, 0x20, 0x30]
        );
    }
}
