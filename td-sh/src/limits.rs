//! Linux process limits for the shell and its thread-backed subshells.
use crate::sys;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, PartialEq)]
#[repr(usize)]
pub enum Resource {
    Cpu = 0,
    File = 1,
    Data = 2,
    Stack = 3,
    Core = 4,
    Processes = 6,
    Descriptors = 7,
    AddressSpace = 9,
}

pub const RESOURCES: &[(char, Resource, &str, u64)] = &[
    ('t', Resource::Cpu, "cpu time (seconds)", 1),
    ('f', Resource::File, "file size (512-byte blocks)", 512),
    ('d', Resource::Data, "data size (KiB)", 1024),
    ('s', Resource::Stack, "stack size (KiB)", 1024),
    ('c', Resource::Core, "core file size (512-byte blocks)", 512),
    ('u', Resource::Processes, "processes", 1),
    ('n', Resource::Descriptors, "open files", 1),
    ('v', Resource::AddressSpace, "virtual memory (KiB)", 1024),
];

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Limit {
    pub soft: u64,
    pub hard: u64,
}

const _: () = assert!(std::mem::size_of::<Limit>() == 16);

// Only the initial interpreter, while it is alone, can change a kernel limit
// without also changing a sibling's ceiling. A discarded hard limit cannot
// be restored by an unprivileged process.
static LIVE_CLONES: AtomicUsize = AtomicUsize::new(0);

pub struct CloneGuard(());

impl CloneGuard {
    pub fn new() -> Self {
        LIVE_CLONES.fetch_add(1, Ordering::SeqCst);
        Self(())
    }
}

impl Drop for CloneGuard {
    fn drop(&mut self) {
        LIVE_CLONES.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn set(resource: Resource, value: Limit, owner: bool) -> Result<(), &'static str> {
    // Only the owning interpreter can create the first clone. A clone that
    // creates another stays counted until its new clone has its own guard.
    if !owner || LIVE_CLONES.load(Ordering::SeqCst) != 0 {
        return Err("changing a resource limit requires a separate shell process; use sh -c");
    }
    sys::set_limit(resource, value).map_err(|_| "cannot set resource limit")
}
