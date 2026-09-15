//! Typed process intent and bounded replies; no kernel authority in display keys.
use crate::budget::MemoryVec;
use crate::format::Text;
use crate::hierarchy::ProcessKey;

pub const TARGETS: usize = 256;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Signal {
    Hangup,
    Interrupt,
    Quit,
    Illegal,
    Trap,
    Abort,
    Bus,
    Arithmetic,
    Kill,
    User1,
    Segmentation,
    User2,
    Pipe,
    Alarm,
    Terminate,
    StackFault,
    Child,
    Continue,
    Stop,
    TerminalStop,
    TerminalInput,
    TerminalOutput,
    Urgent,
    CpuLimit,
    FileLimit,
    VirtualAlarm,
    Profile,
    Window,
    Io,
    Power,
    System,
}
impl Signal {
    pub const ALL: [Self; 31] = [
        Self::Hangup,
        Self::Interrupt,
        Self::Quit,
        Self::Illegal,
        Self::Trap,
        Self::Abort,
        Self::Bus,
        Self::Arithmetic,
        Self::Kill,
        Self::User1,
        Self::Segmentation,
        Self::User2,
        Self::Pipe,
        Self::Alarm,
        Self::Terminate,
        Self::StackFault,
        Self::Child,
        Self::Continue,
        Self::Stop,
        Self::TerminalStop,
        Self::TerminalInput,
        Self::TerminalOutput,
        Self::Urgent,
        Self::CpuLimit,
        Self::FileLimit,
        Self::VirtualAlarm,
        Self::Profile,
        Self::Window,
        Self::Io,
        Self::Power,
        Self::System,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Hangup => "SIGHUP",
            Self::Interrupt => "SIGINT",
            Self::Quit => "SIGQUIT",
            Self::Illegal => "SIGILL",
            Self::Trap => "SIGTRAP",
            Self::Abort => "SIGABRT",
            Self::Bus => "SIGBUS",
            Self::Arithmetic => "SIGFPE",
            Self::Kill => "SIGKILL",
            Self::User1 => "SIGUSR1",
            Self::Segmentation => "SIGSEGV",
            Self::User2 => "SIGUSR2",
            Self::Pipe => "SIGPIPE",
            Self::Alarm => "SIGALRM",
            Self::Terminate => "SIGTERM",
            Self::StackFault => "SIGSTKFLT",
            Self::Child => "SIGCHLD",
            Self::Continue => "SIGCONT",
            Self::Stop => "SIGSTOP",
            Self::TerminalStop => "SIGTSTP",
            Self::TerminalInput => "SIGTTIN",
            Self::TerminalOutput => "SIGTTOU",
            Self::Urgent => "SIGURG",
            Self::CpuLimit => "SIGXCPU",
            Self::FileLimit => "SIGXFSZ",
            Self::VirtualAlarm => "SIGVTALRM",
            Self::Profile => "SIGPROF",
            Self::Window => "SIGWINCH",
            Self::Io => "SIGIO",
            Self::Power => "SIGPWR",
            Self::System => "SIGSYS",
        }
    }
    pub fn parent_first(self) -> bool {
        matches!(self, Self::Stop | Self::Continue)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
    Selected,
    Descendants,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Intent {
    pub revision: u64,
    pub key: ProcessKey,
    pub scope: Scope,
    pub signal: Signal,
}
#[derive(Debug)]
pub struct Details {
    pub intent: Intent,
    pub rows: MemoryVec<Text<4096>>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Delivery {
    Sent,
    Exited,
    Permission,
    Error(i32),
    Cancelled,
}
#[derive(Debug)]
pub struct Results {
    pub revision: u64,
    pub deliveries: MemoryVec<Delivery>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    Prepare(Intent),
    Confirm(u64),
    Cancel(u64),
}
#[derive(Debug)]
pub enum Update {
    Available,
    Unavailable(Text<256>),
    Prepared(Details),
    Failed { revision: u64, reason: Text<256> },
    Finished(Results),
    Cancelled(u64),
}
