//! I/O plumbing: a per-shell descriptor table, redirection application,
//! pipelines, subshells and external-command spawning.
//!
//! The shell keeps its OWN file-descriptor table (`Fds`) rather than dup2'ing
//! real kernel descriptors, because `std` exposes no `dup2`/`fork` and the
//! crate's one `unsafe` surface is `sys.rs`'s four syscalls -- `umask(2)`, a
//! disposition-only `rt_sigaction(2)`, `ioctl(2)` and `poll(2)` -- and nothing
//! else.
//! Builtins and shell functions read and write through
//! this table; only when an *external* program runs is the table translated into
//! `std::process::Command` stdio. A pipeline's stages run CONCURRENTLY, on
//! threads joined by real `std::io::pipe` descriptors, so a consumer runs while
//! its producer does and an infinite producer is bounded by the pipe rather than
//! by memory. That is also what makes every piece of PROCESS-global state below
//! -- the umask, signal dispositions, the interrupt guard -- something stages
//! share rather than own.
//!
//! The virtual table is why `exec cmd` hands the child only descriptors 0/1/2:
//! passing a higher one across an `execve` needs a `pre_exec` `dup2` (unsafe) or a
//! real `fork`, so `exec 3>f; cmd >&3` works (3 is remapped onto a standard
//! descriptor for the child) while `exec 3>f; cmd` cannot let `cmd` see fd 3.
//! A `Fd::Closed` likewise reaches the child as `/dev/null` rather than closed.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::ast::{AndOr, Cmd, List, Redir, RedirKind, Sep, Stage, Word};
use crate::exec::{self, Shell, Sig, R};

/// One entry in the shell's descriptor table. Everything shareable is behind an
/// `Arc` so a subshell or pipeline stage inherits the same open file (and its
/// offset), the way a real dup'd descriptor would.
///
/// An open FILE needs no `Mutex` with it: `&File` is both `Read` and `Write`,
/// so the kernel serialises what two stages do to one descriptor, exactly as it
/// does for two processes sharing one. A lock here would be held across a
/// BLOCKING read, and a sibling's `read -t 5` would then wait on the mutex
/// without ever reaching `poll(2)` -- missing the deadline that option exists
/// to keep. The shell's own in-memory entries still take one, since nothing but
/// this process can serialise a `Vec`.
#[derive(Clone)]
pub enum Fd {
    /// The process's real stdin/stdout/stderr (0/1/2).
    Inherit(u8),
    File(Arc<File>),
    ReadBuf(Arc<Mutex<Cursor<Vec<u8>>>>),
    WriteBuf(Arc<Mutex<Vec<u8>>>),
    Null,
    Closed,
}

pub struct Fds {
    map: HashMap<u32, Fd>,
}

impl Fds {
    pub fn new() -> Self {
        let mut map = HashMap::new();
        map.insert(0, Fd::Inherit(0));
        map.insert(1, Fd::Inherit(1));
        map.insert(2, Fd::Inherit(2));
        Fds { map }
    }

    fn get(&self, fd: u32) -> Option<&Fd> {
        self.map.get(&fd)
    }

    /// Whether descriptor `fd` is a terminal. Only an INHERITED descriptor can
    /// be: the shell's own table means a redirection to a file or to an internal
    /// pipeline buffer must answer no, which `IsTerminal` on the process's real
    /// stream would get wrong.
    pub fn is_terminal(&self, fd: u32) -> bool {
        use std::io::IsTerminal;
        match self.get(fd) {
            Some(Fd::Inherit(0)) => std::io::stdin().is_terminal(),
            Some(Fd::Inherit(1)) => std::io::stdout().is_terminal(),
            Some(Fd::Inherit(2)) => std::io::stderr().is_terminal(),
            // An opened `/dev/tty` is one too, which `read -p` turns on.
            Some(Fd::File(f)) => f.is_terminal(),
            _ => false,
        }
    }

    fn set(&mut self, fd: u32, target: Fd) {
        self.map.insert(fd, target);
    }

    /// Give fd 0 `/dev/null`, which POSIX 2.9.3 requires of an asynchronous
    /// list. Exposed as this one operation rather than by widening `set`, since
    /// a background job is the only thing outside this module that rewires a
    /// descriptor it was handed.
    pub(crate) fn detach_stdin(&mut self) {
        self.set(0, Fd::Null);
    }

    /// Let go of every descriptor this shell holds, which is what a shell about
    /// to JOIN its jobs has to do first -- see the `jobs` field's note.
    pub(crate) fn release(&mut self) {
        self.map.clear();
    }
}

impl Default for Fds {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for Fds {
    fn clone(&self) -> Self {
        Fds {
            map: self.map.clone(),
        }
    }
}

/// Write bytes to a shell descriptor. An unwritable or closed descriptor is an
/// error, surfaced to the caller as an `io::Error` (EBADF-like).
pub fn write_fd(sh: &Shell, fd: u32, bytes: &[u8]) -> std::io::Result<()> {
    write_target(sh.fds.get(fd), bytes)
}

/// Write to a descriptor's target directly, for a caller holding one that is no
/// longer in the table -- dash's `preverrout`.
pub fn write_target(target: Option<&Fd>, bytes: &[u8]) -> std::io::Result<()> {
    match target {
        Some(Fd::Inherit(0)) | Some(Fd::Closed) | None => Err(std::io::Error::other("bad file descriptor")),
        Some(Fd::Inherit(1)) => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(bytes)?;
            lock.flush()
        }
        Some(Fd::Inherit(_)) => {
            let stderr = std::io::stderr();
            let mut lock = stderr.lock();
            lock.write_all(bytes)?;
            lock.flush()
        }
        Some(Fd::File(f)) => file_write(f, bytes),
        Some(Fd::WriteBuf(b)) => {
            if let Ok(mut v) = b.lock() {
                v.extend_from_slice(bytes);
                Ok(())
            } else {
                Err(std::io::Error::other("poisoned buffer"))
            }
        }
        Some(Fd::ReadBuf(_)) => Err(std::io::Error::other(
            "descriptor not open for writing",
        )),
        Some(Fd::Null) => Ok(()),
    }
}

/// `&File` is `Write`, so no lock is taken and none is HELD across the write.
/// Two stages writing one entry interleave exactly as two processes sharing a
/// descriptor do, which is what a forking shell gives them anyway.
fn file_write(f: &File, bytes: &[u8]) -> std::io::Result<()> {
    let mut sink: &File = f;
    sink.write_all(bytes)?;
    sink.flush()
}

/// ONE unbuffered handle on the real stdin, taken once and kept.
///
/// `std::io::Stdin` is a `BufReader`, so a builtin reading through it takes up
/// to 8 KiB off a descriptor the shell usually SHARES -- `sh -c 'read a; cat'`
/// on a pipe left `cat` with nothing -- and hides those bytes from `poll(2)`,
/// which can only see what is still in the kernel. `read -t` turns on that
/// answer, so a partial line read ahead into the shell's own buffer would time
/// out with the bytes already in hand. One byte at a time is what this module
/// already does for every OTHER descriptor and what `line.rs` already does in
/// raw mode, for the same reason: it leaves the rest in the kernel where the
/// next reader finds it.
///
/// Taken once rather than per byte because a dup is a syscall too. It stays
/// valid for the shell's lifetime: `Fd::Inherit(0)` names the real descriptor 0,
/// and a redirection replaces the shell's TABLE entry rather than that.
static STDIN_RAW: Mutex<Option<Arc<File>>> = Mutex::new(None);

/// The handle above, taken on first use. The lock is held only long enough to
/// CLONE the `Arc` -- never across the read itself, which blocks: a sibling
/// stage's `read -t 5` would otherwise wait on this mutex without ever reaching
/// `poll(2)`, and miss the deadline it was given.
///
/// Shared with `line.rs`'s cooked reader rather than duplicated, so the script
/// and the `read` builtin cannot disagree about where in stdin they are.
pub fn stdin_raw() -> std::io::Result<Arc<File>> {
    let mut slot = STDIN_RAW
        .lock()
        .map_err(|_| std::io::Error::other("poisoned stdin"))?;
    if let Some(f) = slot.as_ref() {
        return Ok(Arc::clone(f));
    }
    let dup = std::io::stdin().as_fd().try_clone_to_owned()?;
    let f = Arc::new(File::from(dup));
    *slot = Some(Arc::clone(&f));
    Ok(f)
}

/// Read the next byte from a shell descriptor, or `None` at end of input.
pub fn read_byte(sh: &Shell, fd: u32) -> std::io::Result<Option<u8>> {
    let mut one = [0u8; 1];
    let n = match sh.fds.get(fd) {
        Some(Fd::Inherit(0)) => {
            let handle = stdin_raw()?;
            let mut src: &File = &handle;
            src.read(&mut one)?
        }
        Some(Fd::File(f)) => {
            let mut src: &File = f;
            src.read(&mut one)?
        }
        Some(Fd::ReadBuf(b)) => {
            let mut cur = b
                .lock()
                .map_err(|_| std::io::Error::other("poisoned buffer"))?;
            cur.read(&mut one)?
        }
        Some(Fd::Null) => 0,
        _ => {
            return Err(std::io::Error::other(
                "descriptor not open for reading",
            ))
        }
    };
    Ok(if n == 0 { None } else { Some(one[0]) })
}

/// Whether a read on `fd` would return without waiting, blocking up to
/// `timeout_ms` for that to become true.
///
/// NOT "bytes remain": a file at EOF, an exhausted buffer and a closed
/// descriptor are all ready, because a read on them returns (0, or an error)
/// immediately. That is `poll(2)`'s question and hence exactly what `read -t`
/// asks, which is why this is the one thing on td-sh's syscall surface that
/// exists for a single builtin.
///
/// Only the entries backed by a REAL descriptor are asked. The rest are the
/// shell's own memory -- a cursor over a here-document, a capture buffer, the
/// two sinks -- where a read cannot block whatever the kernel would say about
/// anything, so they answer at once and never spend a syscall.
pub fn read_ready(sh: &Shell, fd: u32, timeout_ms: i32) -> Result<bool, String> {
    // No lock is taken for any of this, which is what makes the deadline hold:
    // the wait can be seconds long, and a stage blocked on a sibling's lock
    // would never reach `poll` at all. What remains is a race rather than a
    // wait -- a sibling can take the byte between the poll and the read, and
    // then this read blocks past the deadline -- and that one is inherent to
    // sharing a descriptor at all, since two PROCESSES race the same way.
    let raw = match sh.fds.get(fd) {
        // The real descriptor, not the dup `read_byte` reads through: a dup
        // names the same open file description, so readiness is the same
        // question asked of either.
        Some(Fd::Inherit(0)) => 0,
        // The shell can only READ the stdin it inherited; `read -u 1` fails at
        // once whatever fd 1 holds. Answering "ready" is what makes that true
        // rather than spending a deadline waiting for a descriptor this builtin
        // would refuse anyway -- `read -u 1 -t 5` reports its error now instead
        // of in five seconds.
        Some(Fd::Inherit(_)) => return Ok(true),
        Some(Fd::File(f)) => std::os::fd::AsRawFd::as_raw_fd(&**f),
        // The shell's own memory, and the absent entry that reads as EOF: a
        // read cannot block on any of them whatever the kernel would say.
        Some(Fd::Null | Fd::ReadBuf(_) | Fd::WriteBuf(_) | Fd::Closed) | None => {
            return Ok(true);
        }
    };
    crate::sys::poll_readable(raw, timeout_ms)
}

/// A descriptor saved by `apply_redirs` so `restore_redirs` can put it back.
pub struct Saved {
    entries: Vec<(u32, Option<Fd>)>,
}

impl Saved {
    /// The fd 2 that was in effect BEFORE this command's redirections, if they
    /// touched it. dash saves it as `preverrout` (REDIR_SAVEFD2) so `set -x`
    /// still reports a command that sends its own stderr elsewhere.
    ///
    /// The outer `Option` says whether fd 2 was redirected at all; the inner one
    /// distinguishes "had no entry before" from "had this target", so a caller
    /// cannot mistake an absent prior stderr for an untouched one and write the
    /// trace into the very file the command redirected to.
    pub fn prev_stderr(&self) -> Option<Option<&Fd>> {
        // The FIRST entry for fd 2 is the pre-command one; a later `2>` in the
        // same command saved only the value an earlier one had just installed.
        for (fd, prev) in &self.entries {
            if *fd == 2 {
                return Some(prev.as_ref());
            }
        }
        None
    }
}

/// What `$?` becomes when a redirection cannot be opened or duped, and one of the
/// few places the two shells td-sh grades against give different answers to the
/// same question. dash's `redirectsafe` returns `setjmp(jmploc.loc) * 2` and
/// `evalcommand` assigns that to `exitstatus`; busybox ash carries the same
/// function with the doubling dropped, so it returns 1. td-sh is ash's
/// replacement and its chain is ash-first, so 1 is the answer here — and the
/// corpus records both sides rather than leaving it to be inferred, `toysh-posix`'s
/// "Failed redirect in assignment" naming `OK dash status: 2` beside
/// `OK ash status: 1`.
///
/// A special builtin then makes it fatal, POSIX's rule, but with this status
/// rather than an `sh_error`'s own.
const REDIR_ERROR: i32 = 1;

/// What ash's `ash_msg_and_raise_error` exits with, for the redirection failures
/// it treats as FATAL rather than as a status a command reports.
///
/// Two unrelated rules reach it. One is `expredir`'s "redir error": a `>&word`
/// target that is not a descriptor, on a descriptor that is not 1 (ash.c:9667).
/// It runs BEFORE `redirectsafe`, which is what makes it fatal rather than the
/// recoverable kind.
///
/// The other is a redirection ash applies in a FORKED CHILD, which is
/// `evalsubshell`'s doing: it uses the plain `redirect` where everything else
/// uses `redirectsafe`, so a failure there is a fatal shell error the child dies
/// of rather than a status it reports.
///
/// Two node types reach that path and both are served. A subshell is one, from
/// `run_subshell`. The other is a bare compound that is `&`'s direct operand:
/// ash's `&` wraps its operand in an `NREDIR` only if it is not one already, so
/// `{ :; } <missing &` keeps its redirect list and is RETYPED to `NBACKGND`,
/// which `evalsubshell` also runs — see `run_background_operand`. Anything with
/// a node of its own is wrapped and keeps `REDIR_ERROR`.
const FATAL_REDIR_ERROR: i32 = 2;

/// What `open("")` reports, which is the one target the shell has to answer for
/// itself. See `open_file`.
const ENOENT: i32 = 2;

/// The two descriptors `>&word` writes, named because the rule turns on WHICH
/// one is redirected rather than on the direction. See `classify_dup`.
const STDOUT: u32 = 1;
const STDERR: u32 = 2;

/// The largest descriptor number a `>&n` target may name. ash parses the digits
/// with `bb_strtou` into an `int` and refuses a negative result (ash.c:12017), so
/// the boundary is `INT_MAX` and not the width of whatever td-sh parses into --
/// measured: `>&2147483647` is a dup that fails, `>&2147483648` is fatal.
const MAX_FD: u32 = i32::MAX as u32;

/// ash's `raise_error_syntax("bad fd number")` (ash.c:12026): a digit string that
/// cannot be a descriptor. Fatal, so it is `Sig::Abort` rather than a status --
/// the same 2 as the two above, for a third unrelated reason.
const BAD_FD_NUMBER: i32 = 2;

/// The result of applying one command's redirections.
pub enum RedirOutcome {
    /// All redirections applied; here is what to restore afterward.
    Applied(Saved),
    /// A redirection failed to open/dup a target (message already printed, `$?`
    /// set to `REDIR_ERROR`). The command must NOT run. POSIX makes this fatal
    /// only for a special built-in; for every other command the caller skips it
    /// and keeps the shell alive. (Contrast an *expansion* error in a target —
    /// `>${x:?}` — which is always fatal and propagates as `Err(Sig)`.)
    Failed,
}

/// Apply a command's redirections to the descriptor table, returning what to
/// restore. Order matters: `2>&1 1>file` differs from `1>file 2>&1`.
pub fn apply_redirs(sh: &mut Shell, redirs: &[Redir]) -> R<RedirOutcome> {
    // Phase 1 for the WHOLE list before phase 2 opens anything, which is what
    // makes `: >victim 2>&/nope/x` leave `victim` alone and what puts the
    // diagnostic on the stderr the command started with. Nothing is saved yet, so
    // a fatal error needs no rollback.
    let plans = plan_redirs(sh, redirs)?;
    let mut saved = Saved {
        entries: Vec::with_capacity(redirs.len()),
    };
    for (fd, plan) in &plans {
        let fd = *fd;
        let prev = sh.fds.map.get(&fd).cloned();
        match open_planned(sh, plan) {
            Ok(Ok(Opened::To(target))) => {
                sh.fds.set(fd, target);
                saved.entries.push((fd, prev));
            }
            // Nothing to install and so nothing to restore.
            Ok(Ok(Opened::Unchanged)) => {}
            // `>&word`: both streams go to the same open file, so they share one
            // offset and interleave rather than overwrite. Saved as two entries so
            // `restore_redirs` puts both back, in reverse, exactly as two separate
            // redirections would. Both descriptors are NAMED rather than taken
            // from `fd` — `classify_dup` decides it only for `STDOUT`, and naming
            // them is what keeps the arm meaning one thing if that ever changes,
            // where using `fd` would silently pair some other descriptor with
            // stderr.
            Ok(Ok(Opened::ToBoth(target))) => {
                let prev_out = sh.fds.map.get(&STDOUT).cloned();
                let prev_err = sh.fds.map.get(&STDERR).cloned();
                sh.fds.set(STDOUT, target.clone());
                saved.entries.push((STDOUT, prev_out));
                sh.fds.set(STDERR, target);
                saved.entries.push((STDERR, prev_err));
            }
            Ok(Err(())) => {
                // A recoverable open/dup failure: roll back this command's earlier
                // redirections, set the failure status, and report "skip".
                restore_redirs(sh, saved);
                sh.set_status(REDIR_ERROR);
                return Ok(RedirOutcome::Failed);
            }
            Err(sig) => {
                // Since phase 1 settles the target words, the only fatal error left
                // here is a HERE-DOCUMENT body's (`<<EOF` containing `${x:?}`),
                // which ash also leaves at `openhere` time. It still rolls back the
                // redirections already applied so the fd table is not left
                // corrupted before the error unwinds.
                restore_redirs(sh, saved);
                return Err(sig);
            }
        }
    }
    Ok(RedirOutcome::Applied(saved))
}

pub fn restore_redirs(sh: &mut Shell, saved: Saved) {
    // Restore in reverse so a doubly-redirected fd lands on its original value.
    for (fd, prev) in saved.entries.into_iter().rev() {
        match prev {
            Some(target) => sh.fds.set(fd, target),
            None => {
                sh.fds.map.remove(&fd);
            }
        }
    }
}

fn default_fd(r: &Redir) -> u32 {
    if let Some(fd) = r.fd {
        return fd;
    }
    match &r.kind {
        RedirKind::In | RedirKind::DupIn | RedirKind::Here(_) | RedirKind::ReadWrite => 0,
        _ => 1,
    }
}

/// What a redirection installs on its descriptor.
enum Opened {
    To(Fd),
    /// `n>&n`: dash's `redirect` skips a dup of a descriptor onto itself, so the
    /// fd is left exactly as it was — which is the whole difference between
    /// `: 3>&3` printing nothing and printing what follows it, since fd 3 is
    /// usually closed and duping a closed descriptor is an error.
    Unchanged,
    /// `>&word`: the target is a FILE, and BOTH stdout and stderr go to it. See
    /// `classify_dup` for whose rule that is and which descriptors it holds for.
    ToBoth(Fd),
}

/// One redirection with everything decided that can be decided before any
/// descriptor moves — ash's `expredir` product (ash.c:9621), which settles the
/// WHOLE list and only then lets `redirect` open anything.
///
/// A here-document is deliberately not part of it: `expredir`'s switch has no
/// `NHERE`/`NXHERE` arm, so the body expands at `openhere` time and keeps its
/// place among the opens.
enum Plan<'a> {
    Here(&'a Word),
    Read(String),
    /// `>`, which `set -C` protects -- as it does `BothFile`, the other kind
    /// that truncates.
    Write(String),
    /// `>|`, the spelling that overrides it.
    Clobber(String),
    Append(String),
    ReadWrite(String),
    /// `>&-`
    Close,
    /// `n>&n`
    Same,
    /// `>&n`. Whether `n` is OPEN is not asked here: a failed dup is
    /// `redirect`-time in ash, which is why `: >victim 2>&7` truncates there too.
    From(u32),
    /// `>&word` on fd 1: the target is a FILE and both streams go to it.
    BothFile(String),
}

/// Phase 1. Expands every target that HAS one exactly once, in list order, and
/// classifies the dup ones — so a fatal error is raised before the first open
/// rather than after some of them, and its diagnostic goes to the stderr the
/// command started with rather than through a redirection the same command
/// already applied. Each plan carries the descriptor it is for, so phase 2 pairs
/// them structurally rather than by position.
fn plan_redirs<'a>(sh: &mut Shell, redirs: &'a [Redir]) -> R<Vec<(u32, Plan<'a>)>> {
    let mut plans = Vec::with_capacity(redirs.len());
    for r in redirs {
        let dest = default_fd(r);
        let plan = match &r.kind {
            RedirKind::Here(body) => Plan::Here(body),
            RedirKind::DupIn | RedirKind::DupOut => {
                let target = exec::redir_target(sh, r)?;
                // `>&-` closes only spelled BARE. ash tests the lone dash in the
                // PARSER, on the unexpanded word (`LONE_DASH`, ash.c:12012), and
                // a word needing expansion never reaches that test -- so `>&'-'`
                // and `>&$dash` are ordinary non-digit targets, a file on fd 1
                // and an error anywhere else. `plain` is that same "one unquoted
                // literal".
                let closes = r.word.plain() == Some("-");
                classify_dup(sh, dest, target, closes)?
            }
            // `&>file` is NOT `>&file`, and the difference is the fd. ash
            // lexes `&>` straight to NTO2 (ash.c:12815) and reapplies the digit
            // prefix, so `2&>f` is NTO2 on fd 2; `>&file` is NTOFD promoted to
            // NTO2 at `expredir`, and it is that promotion that raises for a
            // prefix other than 1 (ash.c:9663). Whether stderr follows is then
            // the DESTINATION's question at redirect time (ash.c:5893), so on
            // any other fd this is an ordinary truncating write -- measured:
            // `2&>out` puts stderr alone in the file where `2>&out` is fatal.
            RedirKind::OutBoth => {
                let target = exec::redir_target(sh, r)?;
                if dest == STDOUT {
                    Plan::BothFile(target)
                } else {
                    Plan::Write(target)
                }
            }
            RedirKind::In => Plan::Read(exec::redir_target(sh, r)?),
            RedirKind::Out => Plan::Write(exec::redir_target(sh, r)?),
            RedirKind::Clobber => Plan::Clobber(exec::redir_target(sh, r)?),
            RedirKind::Append => Plan::Append(exec::redir_target(sh, r)?),
            RedirKind::ReadWrite => Plan::ReadWrite(exec::redir_target(sh, r)?),
        };
        plans.push((dest, plan));
    }
    Ok(plans)
}

/// Phase 2. Opens what phase 1 decided. A failure here is recoverable: the
/// message is printed and `Ok(Err(()))` is returned so `apply_redirs` can turn it
/// into a skipped command.
fn open_planned(sh: &mut Shell, plan: &Plan) -> R<Result<Opened, ()>> {
    match plan {
        Plan::Here(body) => {
            let text = exec::here_body(sh, body)?;
            Ok(Ok(Opened::To(Fd::ReadBuf(Arc::new(Mutex::new(
                Cursor::new(text.into_bytes()),
            ))))))
        }
        Plan::Read(name) => Ok(open_file(sh, name, OpenOptions::new().read(true))),
        Plan::Write(name) => Ok(truncating_open(sh, name, sh.opts.noclobber)),
        Plan::Clobber(name) => Ok(truncating_open(sh, name, false)),
        Plan::Append(name) => {
            let mut opts = OpenOptions::new();
            opts.write(true).create(true).append(true);
            Ok(open_file(sh, name, &opts))
        }
        Plan::ReadWrite(name) => {
            let mut opts = OpenOptions::new();
            opts.read(true).write(true).create(true);
            Ok(open_file(sh, name, &opts))
        }
        Plan::Close => Ok(Ok(Opened::To(Fd::Closed))),
        Plan::Same => Ok(Ok(Opened::Unchanged)),
        // A descriptor CLOSED by `>&-` is not a descriptor. It stays in the table
        // as `Fd::Closed` rather than leaving it -- a child needs to tell closed
        // from absent -- so the two have to be refused together HERE, or `3>&-
        // 2>&3` dups the marker and the command runs on a descriptor ash calls
        // bad. Recoverable, not fatal: ash's is `dup2_or_raise` at `redirect`
        // time, inside the `redirectsafe` that catches it.
        Plan::From(n) => match sh.fds.get(*n) {
            Some(Fd::Closed) | None => {
                let _ = exec::write_stderr(sh, &format!("{n}: bad file descriptor"));
                Ok(Err(()))
            }
            Some(fd) => Ok(Ok(Opened::To(fd.clone()))),
        },
        Plan::BothFile(name) => Ok(truncating_open(sh, name, sh.opts.noclobber).map(to_both)),
    }
}

/// The one open that TRUNCATES, which three plans want and only `>|` wants
/// unguarded. Written once so `set -C` is an argument rather than an `if` each
/// of them has to remember: `>` and the `>&word` that writes both streams pass
/// the option, `>|` passes false because overriding it is what that spelling is.
fn truncating_open(sh: &mut Shell, name: &str, guarded: bool) -> Result<Opened, ()> {
    if guarded {
        return noclobber_open(sh, name);
    }
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    open_file(sh, name, &opts)
}

/// The refusal `set -C` exists for, worded once because two arms report it.
fn noclobber_refuse(sh: &mut Shell, name: &str) -> Result<Opened, ()> {
    let _ = exec::write_stderr(sh, &format!("{name}: cannot overwrite existing file"));
    Err(())
}

/// `set -C` for the two redirections that truncate -- `>` and the `>&word` that
/// writes both streams -- as ONE operation rather than a look followed by a
/// create. Looking first follows a dangling symlink: `ln -s missing lnk;
/// set -C; echo x >lnk` created `missing`, where the `O_EXCL` below refuses the
/// link, and it races a target swapped between the two. ash.c:5554.
fn noclobber_open(sh: &mut Shell, name: &str) -> Result<Opened, ()> {
    let mut opts = OpenOptions::new();
    opts.write(true);
    match std::fs::metadata(sh.resolve(name)) {
        // A regular file is exactly what the option protects.
        Ok(m) if m.is_file() => noclobber_refuse(sh, name),
        // There and NOT regular: opened without creating or truncating, then
        // re-checked, since the name may have become a regular file in between
        // -- and that is the file being protected.
        Ok(_) => match open_file(sh, name, &opts)? {
            Opened::To(Fd::File(f)) if f.metadata().is_ok_and(|m| m.is_file()) => {
                noclobber_refuse(sh, name)
            }
            opened => Ok(opened),
        },
        // Nothing there when we looked, so create it and let `O_EXCL` refuse if
        // that stopped being true. It refuses a SYMLINK too, which is the
        // dangling-link case: the kernel will not follow one under `O_EXCL`.
        Err(_) => open_file(sh, name, opts.create_new(true)),
    }
}

/// `>&word` installs the same open file on BOTH descriptors.
fn to_both(opened: Opened) -> Opened {
    match opened {
        Opened::To(fd) => Opened::ToBoth(fd),
        // `open_file` returns only `To`, so this arm is unreachable; passed
        // through rather than asserted, since the crate does not panic.
        other => other,
    }
}

/// `>&2`, `<&0`, `>&-` (close), for the redirection whose own descriptor is
/// `dest`. A numeric target dups that descriptor; a BARE `-` closes, which is
/// `closes` -- the caller decides it from the unexpanded word. Both fatal
/// spellings are raised HERE, in phase 1, which is where ash raises them.
fn classify_dup(sh: &mut Shell, dest: u32, target: String, closes: bool) -> R<Plan<'static>> {
    if closes {
        return Ok(Plan::Close);
    }
    // ash classifies on ALL-DIGITS FIRST (`isdigit_str`, ash.c:560) and only then
    // asks what the digits mean, so a digit string is a descriptor spelling
    // whatever its length: one too large to BE a descriptor is fatal rather than
    // a filename, and the empty string satisfies that predicate too and takes the
    // same arm. Digits also exclude the leading `+` that `u32::from_str` accepts,
    // which the self-dup below would turn from a wrong descriptor into a SILENT
    // one: `3>&+3` would read as `3>&3` and succeed on a closed fd 3.
    if target.bytes().all(|b| b.is_ascii_digit()) {
        let Some(n) = target.parse::<u32>().ok().filter(|n| *n <= MAX_FD) else {
            let _ = exec::write_stderr(sh, "syntax error: bad fd number");
            return Err(Sig::Abort(BAD_FD_NUMBER));
        };
        return Ok(if n == dest { Plan::Same } else { Plan::From(n) });
    }
    // Not a descriptor. On fd 1 that is not an error but busybox ash's
    // `BASH_REDIR_OUTPUT`, which td's defconfig enables for the same reason
    // `[[` is available: the word names a FILE, and both stdout and stderr
    // are pointed at it -- bash's `&>` under another spelling. The gate is
    // the DESTINATION and not the direction, measured: `1<&f` does exactly
    // what `1>&f` does, while `0<&f`, `<&f` and `2>&f` are all errors.
    //
    // It truncates, so `set -C` guards it as it guards `>` -- in `open_planned`,
    // since that is an open. There is no `>|` spelling of this operator to
    // override with, so the check there is unconditional.
    if dest == STDOUT {
        return Ok(Plan::BothFile(target));
    }
    // Not a descriptor and not on fd 1, so there is nothing it can name: ash
    // raises this from `expredir`, before `redirectsafe` and before any
    // redirection is applied, so it is fatal rather than a status.
    let _ = exec::write_stderr(sh, &format!("{target}: ambiguous redirect"));
    Err(Sig::Abort(FATAL_REDIR_ERROR))
}

fn open_file(sh: &mut Shell, name: &str, opts: &OpenOptions) -> Result<Opened, ()> {
    if name == "/dev/null" {
        return Ok(Opened::To(Fd::Null));
    }
    // `>"$empty"` is ENOENT to the kernel, but `resolve` joins a relative target
    // onto the cwd and joining "" yields the cwd ITSELF -- so this opened the
    // current directory and succeeded, and `<"$empty"` then handed a directory's
    // descriptor to the command instead of skipping it. Refused by name rather
    // than left to the open, since the open is what gets it wrong.
    if name.is_empty() {
        let e = std::io::Error::from_raw_os_error(ENOENT);
        let _ = exec::write_stderr(sh, &format!("{name}: {e}"));
        return Err(());
    }
    let path = sh.resolve(name);
    match opts.open(&path) {
        Ok(f) => Ok(Opened::To(Fd::File(Arc::new(f)))),
        Err(e) => {
            let _ = exec::write_stderr(sh, &format!("{name}: {e}"));
            Err(())
        }
    }
}

/// Why a stage did not simply return a status.
enum StageError {
    /// The interrupt that ended it, carried back to be re-raised on the joining
    /// thread where the enclosing shell's own disposition decides.
    Interrupted(i32),
    /// The OS refused a thread for it, so this stage never ran at all.
    Unstarted(String),
}

/// Whether the shell ASKING would itself have died of an interrupt. That is what
/// decides both raising one from a dead child and letting one out of a clone:
/// under `trap '' INT`, or a SIGINT ignored on entry, the signal would have done
/// nothing to this process and there is no death to stand in for.
fn dies_of_interrupt() -> bool {
    // A CONCURRENT stage may be holding the guard, and the guard's whole job is
    // to make the answer `Ignore` -- so asking the kernel while one is standing
    // reads this shell as uninterruptible when it is merely busy, and a Ctrl-C
    // during a pipeline would abort nothing. What the guard SAVED is the honest
    // answer: it only ever takes a signal it found at `Default`.
    if let Ok(g) = GUARD.lock() {
        if g.depth > 0 {
            // By POSITION in `HELD`, not by index 0: SIGINT happens to be first
            // today, and a signal prepended to that roster would otherwise
            // silently answer this question with SIGQUIT's disposition.
            let at = HELD.iter().position(|s| *s == SIGINT);
            return at.and_then(|i| g.restore.get(i).copied()).unwrap_or(false);
        }
    }
    matches!(
        crate::sys::signal_get(SIGINT),
        Ok(Some(crate::sys::Disposition::Default))
    )
}

/// End an in-process clone an interrupt reached, and answer whether the interrupt
/// comes out with it.
///
/// The clone is dropped FIRST, because that is what puts back any disposition it
/// changed. A clone can hold one of its own -- `trap '' INT; (trap - INT; sleep
/// 5); echo AFTER` resets SIGINT for the duration, and in a forked shell that
/// would be the SUBSHELL dying while the parent's ignore stands -- so asking
/// inside it answers for the wrong shell and ends one that had asked to be
/// uninterruptible. Neither way runs an EXIT trap: a clone killed by a signal
/// never reaches one.
pub(crate) fn leave_clone(sh: &mut Shell, clone: Subshell, code: i32) -> R<()> {
    drop(clone);
    if dies_of_interrupt() {
        return Err(Sig::Interrupt(code));
    }
    sh.set_status(code);
    Ok(())
}

/// Run a pipeline of two or more stages. Each stage's stdout is captured and
/// handed to the next stage as its stdin; the last stage keeps the shell's real
/// stdout. The pipeline's status is the last stage's (POSIX).
///
/// Every stage runs in its OWN subshell environment (a `fork_shell` clone), so a
/// stage's assignments, `cd`, `exit`, `break`/`continue`/`return`, and option
/// changes affect neither the parent shell nor a sibling stage — matching POSIX,
/// which specifies each pipeline command in a separate environment.
///
/// Stages run CONCURRENTLY, joined by real `std::io::pipe` descriptors. The
/// producer no longer runs to completion into a `Vec` first, which was correct
/// for every FINITE producer and pathological for the two shapes an operator
/// most often types: `cat /dev/zero | head -c 1` grew a buffer until the machine
/// died, and `tail -f log | grep x` never reached `grep` at all. A pipe is a
/// fixed kernel buffer, so the first is now bounded and ends when `head` closes
/// its end, and the second streams.
///
/// A pipe end is a `File` in the table like any other, which is what makes this
/// small: an external command already receives a `File` entry as a real
/// descriptor (`stdio_for`), so a stage that IS one now reads and writes the
/// pipe directly with no copy through the shell at all. `PipeReader`/
/// `PipeWriter` convert through `OwnedFd` with no `unsafe`, which is why the
/// module header lists concurrent pipelines as a refinement rather than a
/// syscall question. It needs no variant of its own: the single respect in which
/// a pipe is not a file — a read on it can block — is `read_ready`'s question,
/// and that is answered by asking the kernel rather than by the table's shape.
///
/// Threads rather than forks, since these stages are in-process clones: the
/// scope is what lets a stage borrow the `Cmd` it runs, and the shell state is
/// `Send` — asserted in `shell_state_can_cross_a_thread` rather than left to
/// `scope` to infer, so a field that quietly stops being `Send` names itself
/// here instead of somewhere downstream.
///
/// Everything PROCESS-GLOBAL is where threads-not-processes shows, and each is
/// handled at its own site: the interrupt guard is not a stage's to take, a
/// spawn installs the spawning stage's own signal intent, and a stage that sets
/// a umask still sets the whole process's for as long as it runs.
///
/// What a stage's own copy of a pipe end costs: the descriptor stays open until
/// the STAGE ends, not until the external command inside it exits. So
/// `cat /dev/zero | { head -c 1; sleep 100; }` leaves `cat` blocked on a full
/// pipe for the sleep, where a forking shell would have SIGPIPEd it at once.
/// Bounded and idle rather than unbounded and growing, which is the trade this
/// makes; closing it properly needs the stage to drop the ends it handed over,
/// and that is a redirection-restore question rather than a pipeline one.
pub fn run_pipeline(sh: &mut Shell, cmds: &[Stage]) -> R<()> {
    let mut stages: Vec<Subshell> = cmds
        .iter()
        .map(|_| {
            let mut stage = fork_shell(sh);
            stage.concurrent = true;
            stage
        })
        .collect();
    // Wire junction i: stage i's stdout to the write end, stage i+1's stdin to
    // the read end. The PARENT is deliberately given neither -- a copy there
    // would hold the pipe open after the producing stage ended, and the reader
    // would never see EOF.
    for i in 0..stages.len().saturating_sub(1) {
        let (r, w) = match std::io::pipe() {
            Ok(ends) => ends,
            Err(e) => return Err(sh.fatal(&format!("cannot create pipe: {e}"), 1)),
        };
        if let Some(stage) = stages.get_mut(i) {
            stage.fds.set(1, Fd::File(Arc::new(File::from(OwnedFd::from(w)))));
        }
        if let Some(stage) = stages.get_mut(i + 1) {
            stage.fds.set(0, Fd::File(Arc::new(File::from(OwnedFd::from(r)))));
        }
    }

    // Each stage answers with its status, or with the interrupt that ended it.
    let outcomes: Vec<Result<i32, StageError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = stages
            .into_iter()
            .zip(cmds)
            .map(|(stage, staged)| {
                // `Builder`, not `Scope::spawn`, because that one PANICS when the
                // OS cannot make a thread — and pipeline length is whatever the
                // operator typed, so a low thread limit would abort the shell
                // where a diagnostic belongs. This crate does not panic on an
                // error path.
                std::thread::Builder::new().spawn_scoped(scope, move || {
                    let mut stage = stage;
                    // Per STAGE, not per pipeline: `true |\n  echo $LINENO`
                    // reports 2 in dash, and each stage is its own `Shell` here
                    // so the cell is not shared between concurrent threads.
                    stage.set_lineno(staged.line);
                    // A non-local transfer (`exit`, break/continue/return) is
                    // confined to the stage's subshell; only its status
                    // survives. An interrupt is not -- it is carried back and
                    // re-raised on the joining thread, where the enclosing
                    // shell's own disposition is what decides.
                    let status = match exec::run_command(&mut stage, &staged.cmd) {
                        Ok(()) => stage.status,
                        Err(Sig::Exit(code) | Sig::Abort(code)) => code,
                        // A stage killed by a signal never reaches an EXIT trap,
                        // so this returns before `run_exit_trap` rather than
                        // through it.
                        Err(Sig::Interrupt(code)) => {
                            return Err(StageError::Interrupted(code))
                        }
                        Err(_) => stage.status,
                    };
                    Ok(exec::run_exit_trap(&mut stage, status))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| match h {
                // A thread that could not START is the stage failing to run at
                // all: 126, the status this shell already gives a command it
                // could not execute. Reported, not just counted -- a middle
                // stage that never ran leaves the consumer reading an immediate
                // EOF, which is indistinguishable from a producer with nothing
                // to say, so the pipeline would otherwise report SUCCESS for a
                // command that never happened.
                Err(e) => Err(StageError::Unstarted(e.to_string())),
                Ok(handle) => handle.join().unwrap_or(Err(StageError::Interrupted(128))),
            })
            .collect()
    });

    // A stage that never started is the shell failing, not the pipeline
    // reporting: say so, and let 126 stand as the status however the last stage
    // ended.
    let mut unstarted = false;
    for outcome in &outcomes {
        if let Err(StageError::Unstarted(e)) = outcome {
            let _ = exec::write_stderr(sh, &format!("td-sh: pipeline stage did not start: {e}"));
            unstarted = true;
        }
    }
    // UNWINDS rather than returning a status, exactly as a pipe this could not
    // create does. A status would be `!`'s to negate, and `! cmd | cmd` turning
    // the shell's own inability to run the pipeline into SUCCESS is the one
    // answer a caller must never get. Reported above, one line per stage.
    if unstarted {
        return Err(Sig::Abort(126));
    }

    // The pipeline's status is the LAST stage's (POSIX). An interrupt anywhere
    // in it ends the pipeline, and is asked the same boundary question every
    // other clone is -- once, out here, with every stage's clone already
    // dropped and its dispositions back.
    let interrupted = outcomes.iter().find_map(|o| match o {
        Err(StageError::Interrupted(code)) => Some(*code),
        _ => None,
    });
    let last_status = outcomes.last().map_or(0, |o| match o {
        Ok(code) | Err(StageError::Interrupted(code)) => *code,
        Err(StageError::Unstarted(_)) => 126,
    });
    if let Some(code) = interrupted {
        if dies_of_interrupt() {
            return Err(Sig::Interrupt(code));
        }
    }
    sh.set_status(last_status);
    Ok(())
}

/// `( list )`: run in a cloned shell so nothing the subshell does — variables,
/// cwd, options, traps — is visible afterward. Only `$?` comes back.
pub fn run_subshell(sh: &mut Shell, body: &List, redirs: &[Redir]) -> R<()> {
    // The redirections belong to the SUBSHELL's environment, so apply them to the
    // clone — never the parent. Otherwise a target-word side effect leaks out
    // (`unset x; (:) >${x:=/dev/null}; echo ${x-unset}` must print `unset`). The
    // clone is discarded afterward, so its fd table needs no restore.
    let mut child = fork_shell(sh);
    let status = match apply_redirs(&mut child, redirs) {
        Ok(RedirOutcome::Applied(_saved)) => match exec::run_list(&mut child, body) {
            Ok(()) => child.status,
            Err(Sig::Exit(code) | Sig::Abort(code)) => code,
            // An interrupt is NOT the subshell's to keep: the terminal signals
            // the whole foreground group, so a forked one would have died with
            // its parent rather than reporting a status to it.
            Err(Sig::Interrupt(code)) => return leave_clone(sh, child, code),
            // break/continue/return that escape a subshell are confined to it.
            Err(_) => child.status,
        },
        // A failed redirection skips the subshell body, and reports the fatal
        // status the child would have died of rather than the 1 `apply_redirs`
        // left — see `FATAL_REDIR_ERROR`. It also trips `set -e` where a brace
        // group's does not: ash's parser leaves a subshell's redirections on the
        // `NSUBSHELL` node instead of wrapping it in an `NREDIR`, and `NSUBSHELL`
        // reaches `checkexit`.
        Ok(RedirOutcome::Failed) => FATAL_REDIR_ERROR,
        // Anything FATAL in a target word is confined to the subshell here, and
        // that is a DIVERGENCE rather than the rule: ash calls `expredir` in
        // `evalsubshell` BEFORE it forks, so it ends the whole shell where this
        // ends a clone. Two rules reach this arm — an expansion error
        // (`( : ) >${nope:?}`) and, since the ambiguous target became fatal, a
        // classification one (`( : ) 2>&/nope/x`); both print and carry on here
        // where ash exits 2. Left as it is rather than fixed in passing: hoisting
        // the classification into the parent is the two-phase split, not a line.
        // bash and zsh side with td-sh on both, so ash is alone in the shape.
        Err(Sig::Exit(code) | Sig::Abort(code)) => code,
        Err(Sig::Interrupt(code)) => return leave_clone(sh, child, code),
        Err(_) => child.status,
    };
    let status = exec::run_exit_trap(&mut child, status);
    sh.set_status(status);
    Ok(())
}

/// Run `&`'s operand, which is not always the same as running the list. ash wraps
/// that operand in an `NREDIR` only when it is not one already, so a bare compound
/// carrying its OWN redirections keeps them and is merely retyped to `NBACKGND` —
/// a node `evalsubshell` runs, applying those redirections in the forked child
/// with the plain `redirect`. A failure is therefore the fatal one, and reports
/// what a subshell's does; the same compound backgrounded through anything with a
/// node of its own — a simple command, a pipeline, an and-or, a subshell — is
/// wrapped and keeps `REDIR_ERROR`.
///
/// The caller is already the job's own shell, so the redirections are applied to
/// it and never restored, as a subshell's are to its clone.
///
/// A DIVERGENCE rides on that, the same one `run_subshell` carries: ash calls
/// `expredir` in the PARENT, before it forks, so the target words expand there.
/// `unset x; { :; } >${x:=/dev/null} &` leaves `x` set under ash and unset here,
/// `{ :; } <${nope?} &` ends the whole shell there, and so does
/// `{ :; } 2>&/nope/x &` since the ambiguous target became fatal — the same
/// classification half `run_subshell` names. It is specific to this
/// node — `( : ) <${nope?} &` and `true <${nope?} &` both agree, because `&`
/// wraps those and the `NBACKGND` it makes carries no redirections at all.
/// This arm reaches the redirections before `run_pipeline` and `run_command`
/// would, so it owes the three things they do around one: `-n` must not run,
/// the command's own line must be published before anything expands under it,
/// and a diagnostic that met a broken stderr must still end the shell.
pub fn run_background_operand(sh: &mut Shell, and_or: &AndOr) -> R<()> {
    // Applying a redirection IS running — an output target would be created or
    // truncated — so `-n` takes the ordinary route rather than a second copy of
    // the policy that lives in `run_command`.
    if sh.opts.noexec {
        return exec::run_and_or(sh, and_or);
    }
    let Some((cmd, redirs, line)) = bare_redirected_compound(and_or) else {
        return exec::run_and_or(sh, and_or);
    };
    // Before the redirections, which expand under it: `{ :; } >"f$LINENO" &`
    // names the compound's own line, as it does in every other position.
    sh.set_lineno(line);
    match apply_redirs(sh, &redirs)? {
        RedirOutcome::Applied(_saved) => exec::run_command(sh, &cmd),
        RedirOutcome::Failed => {
            sh.set_status(FATAL_REDIR_ERROR);
            // `run_command`'s tail, which the failure skips past.
            exec::epipe_pending(sh)
        }
    }
}

/// `&`'s operand as ash's parser leaves it: `Some` only for the one shape it does
/// not wrap, a single un-negated one-stage pipeline whose command is a compound
/// carrying redirections. The compound comes back with them REMOVED, since the
/// caller applies them itself — a clone of one command node, on the path that
/// starts a thread anyway. The line comes back with it because the caller
/// publishes it, and it is the node ash retypes: the INNERMOST one, when a
/// transparent brace group was seen through to get here.
fn bare_redirected_compound(and_or: &AndOr) -> Option<(Cmd, Vec<Redir>, u32)> {
    let mut current = and_or;
    let stage = loop {
        if !current.rest.is_empty() || current.first.bang {
            return None;
        }
        let [stage] = current.first.cmds.as_slice() else {
            return None;
        };
        match &stage.cmd {
            // A brace group with no redirections of ITS OWN is not a node at all
            // — ash's `parse_command` returns the list for `{ list; }` and wraps
            // nothing — so `&` sees straight through it to what is inside. Only a
            // single sequential item: two would be an `NSEMI`, which does get
            // wrapped. Iterative because the nesting is the script's to choose.
            Cmd::Group { body, redirs } if redirs.is_empty() => {
                match body.items.as_slice() {
                    [(inner, Sep::Seq)] => current = inner,
                    _ => return None,
                }
            }
            _ => break stage,
        }
    };
    if !is_own_redirected_compound(&stage.cmd) {
        return None;
    }
    let mut cmd = stage.cmd.clone();
    let redirs = match &mut cmd {
        Cmd::Group { redirs, .. }
        | Cmd::If { redirs, .. }
        | Cmd::For { redirs, .. }
        | Cmd::Loop { redirs, .. }
        | Cmd::Case { redirs, .. } => std::mem::take(redirs),
        // Unreachable past the check above, which is the exhaustive one. The
        // roster being stated twice is what makes either statement of it safe
        // alone: each refuses what the other would admit, so widening it takes
        // an edit to both.
        _ => return None,
    };
    Some((cmd, redirs, stage.line))
}

/// Whether a command is a compound carrying redirections of its own, decided on a
/// BORROW: EVERY background job reaches the test above, and only some are the
/// shape, so a declined operand must not pay for a deep copy of its whole command
/// tree. Measured at 60 spawns of a 270 KB body, cloning first cost `( big ) &`
/// half as much again.
///
/// This is the exhaustive match, so a new `Cmd` variant is a compile error until
/// someone says which side of ash's rule it falls on.
fn is_own_redirected_compound(cmd: &Cmd) -> bool {
    match cmd {
        Cmd::Group { redirs, .. }
        | Cmd::If { redirs, .. }
        | Cmd::For { redirs, .. }
        | Cmd::Loop { redirs, .. }
        | Cmd::Case { redirs, .. } => !redirs.is_empty(),
        // `Simple`/`Cond` are `NCMD` and `Subshell` is `NSUBSHELL`; each has a node
        // of its own, so `&` wraps it. A subshell still reports the fatal status,
        // but through `run_subshell` rather than here.
        Cmd::Simple { .. } | Cmd::Cond { .. } | Cmd::Subshell { .. } | Cmd::FuncDef { .. } => false,
    }
}

/// The shell's own SIGINT and SIGQUIT, ignored while a foreground child runs and
/// restored after it — which is what lets Ctrl-C end the COMMAND without ending
/// the shell.
///
/// The child is unaffected, and that is the trick rather than an accident:
/// dispositions are copied when the process is created, and this is taken AFTER
/// `spawn` has returned, so the child already exists holding the `SIG_DFL` it was
/// created with. Ctrl-C therefore still kills `sleep 100`, and no longer kills the
/// shell waiting on it. The other order — ignoring first, spawning second — would
/// hand the child the ignore and make the command uninterruptible, which is the
/// failure this exists to avoid.
///
/// It covers the WAIT and nothing else. Between commands — expansion, PATH
/// resolution, the gap after `spawn` returns, and any builtin that blocks, `read`
/// most of all — the shell is back at `SIG_DFL` and a signal there still ends it.
/// That is inherent to installing no handler: a shell that ignored the signal
/// while doing its own work would lose the keystroke instead of surviving it. The
/// dividing line is not how LONG a command runs but whether an EXTERNAL one runs
/// at all — a loop of `sleep 0.02` survives a Ctrl-C as reliably as one of `sleep
/// 0.5`, while `while :; do true; done` takes no guard and dies every time. A
/// signal landing INSIDE the guard with nothing left to die of it is lost
/// outright, for the same reason: there is nowhere to record that it arrived.
///
/// A disposition is PROCESS-global, so the guard is REFERENCE-COUNTED rather than
/// per-command: the outermost holder takes it, the last one out puts it back,
/// and `dies_of_interrupt` reads what was SAVED rather than the kernel, since a
/// standing guard would have the kernel say this shell ignores SIGINT when it is
/// only busy.
///
/// Today nothing reaches a second `hold`, and saying otherwise would be the
/// second wrong claim about this mechanism. `hold` returns early for any
/// CONCURRENT shell BEFORE touching the count — a pipeline stage or a background
/// job — `fork_shell` carries `concurrent` into every nested shell of one, and
/// the sole non-test caller is `exec_external`,
/// between whose `hold` and drop no shell code runs. So the depth is 0 or 1 and
/// the saved-answer branch is unreachable outside tests. It is kept because the
/// property it enforces is what a SECOND caller would need and cannot be seen to
/// be missing from the call site that adds one — not because it fixes a live
/// failure.
///
/// A signal IGNORED ON ENTRY is left alone: POSIX says the shell cannot reset one,
/// `may_set_signal` is what remembers that, and a shell that already ignores
/// SIGINT already survives this.
///
/// Real job control — the child in its own process group with the terminal handed
/// to it — is the answer to both, and is deferred: it needs `setpgid(2)` and a
/// `TIOCSPGRP` ioctl, which is an amendment to `UNSAFE.md` rather than a use of
/// what is already there.
pub(crate) struct InterruptibleChild {
    /// Whether this guard holds a REFERENCE, and so owes one back. What was
    /// actually installed lives in `GUARD` rather than here: the dispositions
    /// are process-global, so with concurrent stages the record of what to put
    /// back has to be too.
    held: bool,
}

/// Spawn a child that does NOT inherit the interrupt guard's ignore.
///
/// A disposition is copied when a process is CREATED, and with pipeline stages
/// running at once one stage's guard is up while a SIBLING spawns. That child
/// would be created holding `SIG_IGN` and no keystroke could reach it: measured
/// as `/bin/sh -c 'echo go; sleep 10' | { read x; sleep 10; }` surviving a group
/// SIGINT outright, where every other shell dies. It is the same
/// uninterruptible-command failure the guard's ORDER was chosen to avoid,
/// arriving by the one route ordering cannot close — the guard is not this
/// stage's.
///
/// A sibling's `trap '' INT` is the same bug by another route: that one really
/// does change the process, and stage two's child inherits an ignore stage two
/// never asked for -- `{ trap '' INT; echo go; sleep .4; } | { read x; sleep
/// 10; }` hung outright.
///
/// So rather than undoing the guard specifically, the spawn installs what THIS
/// shell wants its child to have and puts the process state back after, under
/// the guard's own lock so two stages cannot interleave here. That is POSIX's
/// rule stated directly — a signal this shell ignores is inherited ignored, so
/// `trap '' INT; cmd` still works, and everything else is default — and it
/// covers the guard and a sibling's trap with one rule instead of two.
///
/// The signals considered are `HELD`, every one the trap table marks IGNORED,
/// and every one this shell has actually INSTALLED an ignore for — which are the
/// only three ways the process can be holding a disposition that is not this
/// stage's intent. The third is not implied by the first two: a stage records a
/// trap without installing it, so `trap '' TERM; { trap - TERM; cmd; } | …`
/// leaves the process ignoring TERM while the stage's table no longer mentions
/// it, and the child would inherit an ignore the stage asked to be rid of. Every
/// other signal wants `SIG_DFL`, which is what an untouched one already is.
///
/// Over-approximating the set is safe and under-approximating is not, because
/// every candidate is compared against the kernel before anything is issued: a
/// signal already where this stage wants it costs one query and no change.
///
/// A signal that is not the shell's to move is skipped rather than asked for,
/// which is what keeps `trap '' CHLD` from auto-reaping the child whose status
/// this is about to wait for. `signal_get` answering `None` for a
/// handler-bearing signal is the second half of that: Rust's own SEGV/BUS
/// handlers cannot be replaced by this loop even briefly.
///
/// The shell is briefly exposed while this window is open, which is the same
/// exposure the guard already documents between commands rather than a new one.
fn spawn_uninherited(sh: &mut Shell, cmd: &mut Command) -> std::io::Result<std::process::Child> {
    // What THIS shell wants its child to have, which is POSIX's rule: a signal
    // it ignores is inherited ignored, and everything else is default -- a
    // trapped signal included, since the handler is the shell's and the child
    // has none.
    let touched: Vec<u8> = sh
        .traps
        .iter()
        .filter(|(_, action)| action.is_empty())
        .map(|(sig, _)| *sig)
        .chain(sh.sig_installed.iter().copied())
        .filter(|sig| !HELD.contains(sig))
        .collect();
    let mut want: Vec<(u8, crate::sys::Disposition)> = Vec::with_capacity(HELD.len() + touched.len());
    for sig in HELD.iter().copied().chain(touched) {
        if want.iter().any(|(s, _)| *s == sig) || !crate::builtin::may_install(sh, sig) {
            continue;
        }
        want.push(if sh.traps.get(&sig).is_some_and(String::is_empty) {
            (sig, crate::sys::Disposition::Ignore)
        } else {
            (sig, crate::sys::Disposition::Default)
        });
    }

    // Under the guard's lock, so two stages cannot interleave here and neither
    // can race a guard being taken or given back.
    let Ok(_guard) = GUARD.lock() else {
        return cmd.spawn();
    };
    let mut saved = Vec::with_capacity(want.len());
    for (sig, want) in &want {
        match crate::sys::signal_get(*sig) {
            Ok(Some(now)) if now != *want && crate::sys::signal_set(*sig, *want).is_ok() => {
                saved.push((*sig, now));
            }
            _ => {}
        }
    }
    let spawned = cmd.spawn();
    for (sig, was) in saved {
        let _ = crate::sys::signal_set(sig, was);
    }
    spawned
}

/// The guard's process-global state, because the dispositions it moves are.
///
/// Pipeline stages run concurrently now, so two of them can be waiting on an
/// external command at once. Each taking and restoring the disposition
/// independently would have the FIRST to finish hand `SIG_DFL` back while the
/// second is still waiting — and the second's child, spawned after that, would
/// inherit it. The outermost holder takes it and the last one out puts it back.
struct GuardState {
    depth: u32,
    restore: [bool; HELD.len()],
}

static GUARD: Mutex<GuardState> = Mutex::new(GuardState {
    depth: 0,
    restore: [false; HELD.len()],
});

impl InterruptibleChild {
    pub(crate) fn hold(sh: &mut Shell) -> Self {
        // NOT inside a pipeline. A disposition is process-global and a pipeline's
        // stages are threads of one process, so a guard taken by the stage
        // running `cat` covers the stage running `while :; do :; done` as well —
        // and that one can only ever be stopped by a signal. The result was a
        // pipeline nothing could interrupt and no in-band way out of it, which is
        // strictly worse than the death this guard replaces: `while :; do :;
        // done | cat` needed a SIGKILL from another terminal.
        //
        // So a pipeline is simply not covered, which is what `bash --posix` does
        // too — it dies of the signal on every one of those shapes, as this shell
        // did before pipelines streamed. Covering it properly means either a
        // handler to record the signal with (the `SA_RESTORER` amendment) or
        // stages in processes of their own; neither is bought here, and pretending
        // otherwise costs the operator their only escape.
        if sh.concurrent {
            return Self { held: false };
        }
        let Ok(mut guard) = GUARD.lock() else {
            return Self { held: false };
        };
        if guard.depth > 0 {
            // Already standing: the dispositions are what the outermost holder
            // made them, and this stage adds only a reference.
            guard.depth = guard.depth.saturating_add(1);
            return Self { held: true };
        }
        let mut restore = [false; HELD.len()];
        for (slot, sig) in restore.iter_mut().zip(HELD) {
            // Not the shell's to change if someone handed it an ignore -- and
            // nothing to do in that case either, since an ignored signal cannot
            // kill it.
            if !crate::builtin::may_set_signal(sh, sig) {
                continue;
            }
            // ...and not this guard's to take if the SCRIPT has ignored it since:
            // `may_set_signal` answers about the disposition on ENTRY and is
            // cached there, so `trap '' INT` moves the kernel out from under it.
            // Restoring `SIG_DFL` over that would undo the operator's own trap on
            // the first external command it runs.
            //
            // This re-query SUBSUMES the entry check above for every reachable
            // state — a signal ignored on entry can never be moved to `Default`,
            // since `apply_disposition` refuses and this guard never installed it
            // — so deleting that check is invisible to any test. It stays because
            // it is the POSIX rule stated where the decision is made, and because
            // this one is about the kernel rather than about the shell's own
            // record; deleting THIS one is what breaks `trap '' INT`.
            if !matches!(
                crate::sys::signal_get(sig),
                Ok(Some(crate::sys::Disposition::Default))
            ) {
                continue;
            }
            // A kernel that refuses leaves the shell exactly as interruptible as
            // it was, which is the behaviour this replaces rather than a new one.
            *slot = crate::sys::signal_set(sig, crate::sys::Disposition::Ignore).is_ok();
        }
        guard.restore = restore;
        guard.depth = 1;
        Self { held: true }
    }
}

impl Drop for InterruptibleChild {
    fn drop(&mut self) {
        if !self.held {
            return;
        }
        let Ok(mut guard) = GUARD.lock() else {
            return;
        };
        guard.depth = guard.depth.saturating_sub(1);
        // Not the LAST one out: another stage is still waiting on a child, and
        // handing the disposition back now would leave that one exposed and its
        // next child holding an inherited default.
        if guard.depth > 0 {
            return;
        }
        let restore = guard.restore;
        guard.restore = [false; HELD.len()];
        for (taken, sig) in restore.iter().zip(HELD) {
            if *taken {
                // Only a signal the guard found at `SIG_DFL` and changed itself
                // is here, so `SIG_DFL` is what goes back. Asking for it outright
                // rather than replaying whatever the kernel handed back means an
                // answer this shell cannot express cannot leave a signal ignored
                // after the command.
                let _ = crate::sys::signal_set(sig, crate::sys::Disposition::Default);
            }
        }
    }
}

/// The signals a terminal's control characters raise at the whole foreground
/// process group, and so the ones that reach the shell beside the command the
/// operator meant them for. Ctrl-\ arrives exactly as Ctrl-C does, and a shell
/// that survived one and died on the other would be a coin toss from the
/// keyboard; dash and bash both outlive both.
const HELD: [u8; 2] = [SIGINT, SIGQUIT];

/// The signal a terminal's interrupt character raises. The one definition in
/// the crate: `main.rs` reads it to decide whether the editor may treat Ctrl-C
/// as a keystroke, and a second copy would be a second thing to keep right.
pub(crate) const SIGINT: u8 = 2;

/// ...and its quit character. Held with SIGINT but NOT abort-worthy: a child
/// killed by Ctrl-\ reports 131 and the enclosing script carries on, which is
/// what both references do -- only SIGINT unwinds a loop.
const SIGQUIT: u8 = 3;

/// The file-creation mask, saved and put back on `Drop`.
///
/// It is the one piece of subshell state that is NOT in `Shell`: the mask lives
/// in the kernel, one per process. ash forks, so a subshell's `umask` cannot
/// reach the parent; td-sh's subshells are in-process clones, so the
/// save/restore a fork gives for free has to be explicit. Restoring on `Drop`
/// is what carries it across the `?`-shaped exits those bodies take.
/// Read the real mask once, while this is still the only thread.
///
/// `sys::umask_get` answers from the shell's own record, which has to start out
/// right: the record is seeded here rather than on first use, because "first
/// use" is `fork_shell`, and by the time a pipeline is forking stages there is
/// no safe moment to clear the mask even briefly. Called from `main` before the
/// shell runs anything.
pub(crate) fn prime_umask() {
    crate::sys::umask_prime();
}

struct UmaskScope {
    mask: u32,
    armed: bool,
}

impl UmaskScope {
    fn capture() -> Self {
        Self { mask: crate::sys::umask_get(), armed: true }
    }

    /// Stand down, for a clone that never changed the mask. Sequentially that is
    /// a restore of the value already installed and so invisible; with stages
    /// running at once it is not, because the value installed may be a SIBLING's
    /// and putting the captured one back UNDOES that sibling's `umask` while it
    /// is still running. `umask 022; { umask 077; sleep .2; : >a; } | sleep .1`
    /// created `a` as 0644 rather than 0600 — the wrong direction, and asked for
    /// by neither stage.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for UmaskScope {
    fn drop(&mut self) {
        // Nothing to report to: the mask was this process's a moment ago, so a
        // refusal here would mean the kernel changed its mind about a value it
        // already accepted.
        if self.armed {
            let _ = crate::sys::umask_set(self.mask);
        }
    }
}

/// A cloned shell environment and the process-state guards it cannot be
/// separated from.
///
/// They are bundled here rather than left to each caller because there is no way
/// to tell from a subshell's OUTPUT that either leaked -- a stray mask shows up
/// later in the permissions of a file some unrelated command creates, and a
/// stray `SIG_IGN` shows up as a child that will not die. So `fork_shell` hands
/// out no bare `Shell`: a subshell construct added later gets the save/restore
/// whether or not its author knew to ask. Derefs to `Shell`, so callers use it
/// as one.
pub struct Subshell {
    shell: Shell,
    mask: UmaskScope,
}

/// Put back every signal disposition this subshell changed.
///
/// The same argument as `UmaskScope`, for the other piece of subshell state that
/// lives in the kernel rather than in `Shell`: ash forks, so a subshell's `trap
/// '' INT` cannot reach the parent; td-sh's subshells are in-process clones, so
/// the restore a fork gives for free has to be explicit. Written on `Subshell`
/// itself rather than as a third field because the record is IN the shell --
/// `trap` runs against a `&mut Shell` and has no other place to leave it -- and
/// because `drop` runs before the fields do, so `mask` is still standing.
impl Drop for Subshell {
    fn drop(&mut self) {
        // The clone's own jobs are joined FIRST, and its descriptors released
        // before that, both for reasons the field order gives the top-level
        // shell: a job blocked on a descriptor only this clone holds would never
        // finish, and a job still running must not have the dispositions pulled
        // out from under it below. `( trap '' INT; cmd & )` is the case --
        // POSIX has an async list started by a shell that ignores INT go on
        // ignoring it, and restoring first both breaks that and races the
        // save/restore a job's own spawn does under `GUARD`.
        self.shell.fds.release();
        self.shell.jobs.wait_all();
        // A clone that never set a mask has none to put back, and putting one
        // back anyway is how a stage came to undo a sibling's. `drop` runs
        // before the fields do, so the scope is still standing to be told.
        if !self.shell.umask_changed {
            self.mask.disarm();
        }
        // Nothing to report to, and nothing to decide: each entry holds what the
        // kernel itself handed back when this shell took the disposition over.
        while let Some((signo, ignored)) = self.shell.sig_undo.pop() {
            let want = if ignored {
                crate::sys::Disposition::Ignore
            } else {
                crate::sys::Disposition::Default
            };
            let _ = crate::sys::signal_set(signo, want);
        }
    }
}

impl std::ops::Deref for Subshell {
    type Target = Shell;
    fn deref(&self) -> &Shell {
        &self.shell
    }
}

impl std::ops::DerefMut for Subshell {
    fn deref_mut(&mut self) -> &mut Shell {
        &mut self.shell
    }
}

/// A child shell that shares the parent's open descriptors (so redirections and
/// captured output flow through) but owns an independent copy of the mutable
/// state a subshell must not leak back. Recursion/substitution counters and the
/// `errexit`-suppression depth are inherited: a subshell or command substitution
/// spawned while evaluating an `if`/`while` condition (or a non-final `&&`/`||`
/// operand) is still part of that suppressed context, so it must not exit on an
/// inner failure either.
pub fn fork_shell(sh: &Shell) -> Subshell {
    // Taken before the clone, so the guard's life spans the child's.
    let mask = UmaskScope::capture();
    let shell = Shell {
        vars: sh.vars.clone(),
        funcs: sh.funcs.clone(),
        params: sh.params.clone(),
        arg0: sh.arg0.clone(),
        // Carried, not reset: a subshell is the same script at the same point,
        // and `( echo $LINENO )` reports the line the subshell is written on.
        lineno: sh.lineno,
        funcline: sh.funcline,
        status: sh.status,
        last_bg: sh.last_bg,
        // CLEARED, not carried: ash does this in `forkchild` with the comment
        // "or else $RANDOM repeats in child" (ash.c:5344), so a subshell reseeds
        // from its own pid rather than replaying the parent's sequence. The
        // DYNAMIC flag rides along in `vars`, which is cloned above.
        random: None,
        opts: sh.opts,
        cwd: sh.cwd.clone(),
        logical_cwd: sh.logical_cwd.clone(),
        fds: sh.fds.clone(),
        localvar_depth: sh.localvar_depth,
        // Carried, not dropped: a fork copies the frame, so a subshell is still
        // inside it and a `local` there for a name the function already declared
        // is a REPEAT. Restoring these only ever touches the clone's own map.
        locals: sh.locals.clone(),
        pending_unwind: Vec::new(),
        pending_floor: 0,
        // A subshell's children inherit them too.
        opaque_env: sh.opaque_env.clone(),
        loop_depth: 0,
        run_depth: sh.run_depth,
        cmdsubst_count: sh.cmdsubst_count,
        errexit_suppressed: sh.errexit_suppressed,
        interactive: false,
        // Carried, so a subshell inside a stage is still inside the pipeline.
        concurrent: sh.concurrent,
        // Fresh, never cloned: see the field's note.
        jobs: crate::jobs::Jobs::new(),
        // Inherited: a `$(...)` inside $PS4 runs in one of these, and it must
        // still know it is inside PS4 or the guard buys nothing.
        in_ps4: sh.in_ps4,
        getopts_optind: sh.getopts_optind,
        getopts_off: sh.getopts_off,
        // A subshell inherits the aliases but cannot publish one back (POSIX).
        aliases: sh.aliases.clone(),
        cloned: true,
        trap_status: None,
        // POSIX 2.12: a subshell resets the traps it inherited to their defaults,
        // so only one it sets ITSELF runs when its environment ends -- but one set
        // to IGNORE (dash's empty action) stays ignored, and keeps being reported.
        // The kernel already holds SIG_IGN for those, so nothing is re-applied
        // here; the ones dropped were never more than table entries.
        traps: sh
            .traps
            .iter()
            .filter(|(_, action)| action.is_empty())
            .map(|(signo, action)| (*signo, action.clone()))
            .collect(),
        // Carried exactly as `fork(2)` copies dash's `sigmode`: the answer is
        // about how the PROCESS started, so a clone that re-derived it would
        // read back a disposition its own parent installed and mistake it for
        // one inherited from outside.
        sig_may_set: sh.sig_may_set.clone(),
        sig_installed: sh.sig_installed.clone(),
        stderr_epipe: std::sync::atomic::AtomicBool::new(false),
        umask_changed: false,
        sig_undo: Vec::new(),
    };
    Subshell { shell, mask }
}

/// `$(code)`: run `code` in a subshell with stdout captured to a buffer, and
/// return the captured bytes as text.
pub fn capture_stdout(sh: &mut Shell, code: &str, line: u32) -> R<String> {
    let list = match crate::parser::parse_aliased_at(code, &sh.aliases, line) {
        Ok(l) => l,
        Err(e) => return Err(sh.fatal(&e, 2)),
    };
    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut child = fork_shell(sh);
    child.fds.set(1, Fd::WriteBuf(buf.clone()));
    let outcome = exec::run_list(&mut child, &list);
    let status = match outcome {
        Ok(()) => child.status,
        Err(Sig::Exit(code) | Sig::Abort(code)) => code,
        // `x=$(sleep 100)` interrupted is the enclosing script's interrupt too,
        // for the same reason a subshell's is: one signal, one process group.
        // When it does not come out, the substitution ends with whatever it had
        // already written -- which is what a killed fork's pipe would hold.
        Err(Sig::Interrupt(code)) => {
            leave_clone(sh, child, code)?;
            return read_capture(sh, &buf);
        }
        Err(_) => child.status,
    };
    let status = exec::run_exit_trap(&mut child, status);
    // BEFORE the buffer is read, because dropping the clone is what joins the
    // background jobs it started and they write into that same buffer:
    // `x=$( { sleep .1; echo hi; } & )` is `hi` in bash, whose substitution reads
    // the pipe until the JOB closes its inherited end too. Reading first would
    // capture whatever had happened to arrive.
    drop(child);
    // Command substitution updates $? of the enclosing shell.
    sh.set_status(status);
    read_capture(sh, &buf)
}

/// The captured bytes of a command substitution, as text.
fn read_capture(sh: &mut Shell, buf: &Arc<Mutex<Vec<u8>>>) -> R<String> {
    let bytes = buf
        .lock()
        .map(|v| v.clone())
        .map_err(|_| sh.fatal("command substitution: poisoned capture buffer", 1))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `exec command …`: replace this shell process with `command`.
///
/// Returns only if the command cannot be run at all — a real `execve` never comes
/// back. Falls back to run-then-exit when a descriptor is one of the shell's
/// in-process buffers (a pipeline stage or command substitution): those bytes have
/// no kernel descriptor to hand over, so the command is run normally and the shell
/// exits with its status, which is what the caller would have observed anyway.
pub fn exec_replace(sh: &mut Shell, argv: &[String]) -> R<()> {
    use std::os::unix::process::CommandExt;

    let Some(program) = argv.first() else {
        return Ok(());
    };
    let Some(resolved) = resolve_program(sh, program, None) else {
        let _ = exec::write_stderr(sh, &format!("td-sh: exec: {program}: not found"));
        return failed_exec(sh, 127);
    };
    // Replacing the process is only safe from the real shell with real stdio. An
    // in-process clone (subshell, `&`, command substitution) would take the whole
    // script with it, and an in-process buffer has no kernel fd to hand over; both
    // run the command and exit instead, which is what the caller would have seen.
    let buffered = (0..=2).any(|fd| {
        matches!(sh.fds.get(fd), Some(Fd::ReadBuf(_)) | Some(Fd::WriteBuf(_)))
    });
    if sh.cloned || buffered {
        // A real `execve` replaces the image, taking the trap table with it, so the
        // emulation has to drop it too -- otherwise this shell runs an EXIT trap
        // the exec'd program could never have run.
        sh.traps.clear();
        exec_external(sh, argv, None)?;
        return Err(Sig::Exit(sh.status));
    }

    // `exec` is the one exit a join cannot follow: `execve` replaces the image,
    // so every job thread stops mid-instruction and whatever it had left to
    // write is gone. Joined here rather than lost, which is what "a shell
    // outlives the jobs it started" says everywhere else -- and the cost is the
    // same one that invariant carries, a shell that does not hand over while a
    // job runs.
    if sh.jobs.any_running() {
        sh.jobs.wait_all();
    }

    let mut cmd = Command::new(&resolved);
    cmd.args(argv.iter().skip(1));
    cmd.env_clear();
    for (k, v) in sh.exported_env() {
        cmd.env(k, v);
    }
    // Names the shell cannot spell still belong to the environment it was handed.
    for (k, v) in &sh.opaque_env {
        cmd.env(k, v);
    }
    cmd.current_dir(&sh.cwd);
    cmd.stdin(stdio_for(sh, 0)?);
    cmd.stdout(stdio_for(sh, 1)?);
    cmd.stderr(stdio_for(sh, 2)?);

    // Safe: `CommandExt::exec` returns the error rather than trapping it.
    let e = cmd.exec();
    let _ = exec::write_stderr(sh, &format!("td-sh: exec: {program}: {e}"));
    failed_exec(sh, 126)
}

/// A failed `exec` ends the shell, interactive or not: dash and busybox-ash both
/// clear `iflag` before handing over, and by the time `CommandExt::exec` reports
/// failure it has already applied the redirections to the REAL descriptors, so
/// carrying on would leave the shell rewired.
fn failed_exec(sh: &mut Shell, code: i32) -> R<()> {
    sh.set_status(code);
    Err(Sig::Exit(code))
}

/// Spawn an external program, wiring its stdio to the current descriptor table.
/// The seed corpus never reaches here (it is builtin-only), but real scripts do.
///
/// A buffered shell descriptor (a `ReadBuf` feeding stdin, or a `WriteBuf`
/// capturing stdout/stderr — as set up by command substitution and pipelines)
/// cannot be handed to a foreign process directly, so it is bridged through a real
/// OS pipe: the in-process bytes are pumped to/from the child on helper threads.
/// Without this an external consumer would read the shell's real inherited stdin
/// (blocking the shell forever on a live terminal) and `x=$(external)` would lose
/// the command's output.
pub fn exec_external(sh: &mut Shell, argv: &[String], path: Option<&str>) -> R<()> {
    let Some(program) = argv.first() else {
        sh.set_status(0);
        return Ok(());
    };
    let resolved = match resolve_program(sh, program, path) {
        Some(p) => p,
        None => {
            let _ = exec::write_stderr(sh, &format!("td-sh: {program}: not found"));
            sh.set_status(127);
            return Ok(());
        }
    };

    let mut cmd = Command::new(&resolved);
    cmd.args(argv.iter().skip(1));
    cmd.env_clear();
    for (k, v) in sh.exported_env() {
        cmd.env(k, v);
    }
    // Names the shell cannot spell still belong to the environment it was handed.
    for (k, v) in &sh.opaque_env {
        cmd.env(k, v);
    }
    cmd.current_dir(&sh.cwd);

    // A `ReadBuf` on stdin becomes piped input; a `WriteBuf` on stdout/stderr
    // becomes a captured pipe. Everything else maps to a `Stdio` directly.
    let stdin_bytes: Option<Vec<u8>> = match sh.fds.get(0) {
        Some(Fd::ReadBuf(b)) => b.lock().ok().map(|mut cur| {
            let mut v = Vec::new();
            let _ = cur.read_to_end(&mut v);
            v
        }),
        _ => None,
    };
    let stdout_buf = match sh.fds.get(1) {
        Some(Fd::WriteBuf(b)) => Some(b.clone()),
        _ => None,
    };
    let stderr_buf = match sh.fds.get(2) {
        Some(Fd::WriteBuf(b)) => Some(b.clone()),
        _ => None,
    };

    cmd.stdin(if stdin_bytes.is_some() {
        Stdio::piped()
    } else {
        stdio_for(sh, 0)?
    });
    cmd.stdout(if stdout_buf.is_some() {
        Stdio::piped()
    } else {
        stdio_for(sh, 1)?
    });
    cmd.stderr(if stderr_buf.is_some() {
        Stdio::piped()
    } else {
        stdio_for(sh, 2)?
    });

    let mut child = match spawn_uninherited(sh, &mut cmd) {
        Ok(c) => c,
        Err(e) => {
            let _ = exec::write_stderr(sh, &format!("td-sh: {program}: {e}"));
            sh.set_status(126);
            return Ok(());
        }
    };

    // The shell stops listening to Ctrl-C for exactly as long as the child is
    // alive. Both are in the terminal's foreground process group, so the driver's
    // SIGINT reaches BOTH -- and a shell whose SIGINT is `SIG_DFL` dies beside the
    // command the operator meant to interrupt. As the image's login shell that
    // costs the session, and getty respawning a fresh login is not an answer.
    //
    // Taken HERE rather than beside the wait, because the wait is not the only
    // place this blocks on the child: the stderr drain below reads to EOF on this
    // thread, so for `x=$(cmd 2>&1)` the child has already run and exited by the
    // time the wait is reached. Held until the stdout joiner is done too, since a
    // grandchild holding that pipe keeps the shell here just as long.
    let interrupt_guard = InterruptibleChild::hold(sh);

    // Feed stdin and drain stdout concurrently so a child that writes while reading
    // cannot deadlock a single-threaded pump. `Builder::spawn` is used (not
    // `thread::spawn`) so an OS thread-creation failure drops the pump closure —
    // closing that pipe end (child sees EOF on stdin / EPIPE on stdout) rather than
    // panicking; there is no inline fallback, only this safe degradation. The stdin
    // writer is DETACHED (never joined): if the consumer exits early it gets EPIPE
    // and ends, and if the child hands its stdin
    // to a lingering grandchild the writer blocks harmlessly in the background
    // instead of hanging the shell. stdout is captured on a joined thread; stderr is
    // drained on this thread.
    if let (Some(bytes), Some(mut si)) = (stdin_bytes, child.stdin.take()) {
        // Detached writer. On OS thread-exhaustion the closure is dropped, closing
        // `si` so the child sees empty stdin — degraded, but never a panic or hang.
        let _ = std::thread::Builder::new().spawn(move || {
            let _ = si.write_all(&bytes);
        });
    }
    let stdout_join = match (stdout_buf, child.stdout.take()) {
        (Some(buf), Some(mut so)) => {
            let drain = move || {
                let mut v = Vec::new();
                let _ = so.read_to_end(&mut v);
                if let Ok(mut b) = buf.lock() {
                    b.extend_from_slice(&v);
                }
            };
            // On OS thread-exhaustion the closure (owning `so`/`buf`) is dropped,
            // closing the pipe's read end; the child then sees EPIPE on stdout rather
            // than the shell hanging — degraded capture, never a panic or deadlock.
            std::thread::Builder::new().spawn(drain).ok()
        }
        _ => None,
    };
    // Drain stderr on this thread (the third concurrent stream).
    if let (Some(buf), Some(mut se)) = (stderr_buf, child.stderr.take()) {
        let mut v = Vec::new();
        let _ = se.read_to_end(&mut v);
        if let Ok(mut b) = buf.lock() {
            b.extend_from_slice(&v);
        }
    }

    let status = child.wait();
    if let Some(j) = stdout_join {
        let _ = j.join();
    }
    drop(interrupt_guard);

    match status {
        Ok(status) => {
            // A signal-terminated child reports 128 + signal number (POSIX).
            let code = status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
            sh.set_status(code);
            // A child the INTERRUPT killed abandons what the shell was doing, or
            // Ctrl-C would end one `sleep` of `for i in 1 2 3; do sleep 10; done`
            // and go straight on to the next -- a loop nothing can stop, which is
            // a worse answer than the death this guard was added to prevent. This
            // stands in for the SIGINT the shell itself was sent, and `Sig::Abort`
            // is exactly that shape: it ends a script, and returns an interactive
            // shell to its prompt.
            //
            // Standing in for it means asking whether the shell would have DIED of
            // it. The guard is already dropped, so the disposition here is the
            // shell's own: under `trap '' INT` -- or a SIGINT ignored on entry, the
            // `nohup` case -- the signal would have done nothing to this process,
            // and inferring an abort from the child's death would end a script the
            // operator asked to be uninterruptible.
            // Only a child the KERNEL killed is visible here. A nested td-sh
            // that aborted reports 130 as an ordinary exit, so this cannot see
            // it and `for i in 1 2 3; do sh -c '...'; done` runs on; dash and
            // bash avoid that by dying OF the signal, which needs `kill(2)`.
            if status.signal() == Some(i32::from(SIGINT)) && dies_of_interrupt() {
                return Err(Sig::Interrupt(code));
            }
            Ok(())
        }
        Err(e) => {
            let _ = exec::write_stderr(sh, &format!("td-sh: {program}: {e}"));
            sh.set_status(126);
            Ok(())
        }
    }
}

/// Translate a NON-buffered descriptor into a `Stdio` for a child. Buffered
/// descriptors (`ReadBuf`/`WriteBuf`) are handled by the pipe bridge in
/// `exec_external` before this is reached; the arms here remain a safe fallback
/// (inherit) for any fd the bridge does not special-case.
fn stdio_for(sh: &Shell, fd: u32) -> R<Stdio> {
    match sh.fds.get(fd) {
        // Map to the REAL stream the entry names, not to this position: after
        // `1>&2` fd 1 holds `Inherit(2)`, so the child's stdout must go to the
        // shell's stderr. `try_clone_to_owned` is the safe dup; if it fails the
        // positional inherit is the harmless fallback.
        Some(Fd::Inherit(n)) => Ok(inherit_stream(*n)),
        None => Ok(Stdio::inherit()),
        Some(Fd::Null) => Ok(Stdio::null()),
        Some(Fd::File(f)) => match f.try_clone() {
            Ok(c) => Ok(Stdio::from(c)),
            Err(_) => Ok(Stdio::inherit()),
        },
        Some(Fd::Closed) => Ok(Stdio::null()),
        Some(Fd::ReadBuf(_)) | Some(Fd::WriteBuf(_)) => Ok(Stdio::inherit()),
    }
}

/// A `Stdio` for the process's own stream `n` (0/1/2).
fn inherit_stream(n: u8) -> Stdio {
    use std::os::fd::AsFd;
    let cloned = match n {
        0 => std::io::stdin().as_fd().try_clone_to_owned(),
        1 => std::io::stdout().as_fd().try_clone_to_owned(),
        _ => std::io::stderr().as_fd().try_clone_to_owned(),
    };
    match cloned {
        Ok(owned) => Stdio::from(owned),
        Err(_) => Stdio::inherit(),
    }
}

/// `command -p`'s default utility path: ash's `bb_default_path`, which is
/// `BB_PATH_ROOT_PATH` (libbb.h) less its `/sbin` pair. The supplied busybox
/// leaves `BB_ADDITIONAL_PATH` -- the CFLAGS hook that can extend it -- empty, and
/// its strings confirm the result. A td image has `/bin` and no `/usr/bin`.
pub const DEFAULT_UTILITY_PATH: &str = "/bin:/usr/bin";

/// Locate an external program: a path containing `/` is used directly, otherwise
/// each element of `path` -- or of `PATH` when it is `None` -- is tried. Relative
/// elements resolve against the shell cwd (not the process cwd) so the lookup
/// agrees with the child, which runs with `current_dir(sh.cwd)`.
///
/// `path` is `command -p`'s override: only the LOOKUP moves, never the variable a
/// child inherits, as ash's `path` local does.
pub fn resolve_program(
    sh: &Shell,
    program: &str,
    path: Option<&str>,
) -> Option<std::path::PathBuf> {
    if program.contains('/') {
        let p = sh.resolve(program);
        return if p.is_file() { Some(p) } else { None };
    }
    let owned;
    let path = match path {
        Some(p) => p,
        None => {
            owned = sh.get_var("PATH").unwrap_or_default();
            &owned
        }
    };
    for dir in path.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let candidate = sh.resolve(dir).join(program);
        // Skip a non-executable match and keep searching, so a data file earlier in
        // PATH does not shadow a real executable later in it.
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Parse and run a program with stdout and stderr captured to buffers — the
/// harness used by unit and conformance tests.
#[cfg(test)]
pub fn run_capturing(src: &str) -> (i32, String, String) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let mut sh = Shell::new_for_test();
    sh.fds.set(1, Fd::WriteBuf(out.clone()));
    sh.fds.set(2, Fd::WriteBuf(err.clone()));
    let status = exec::run_program(&mut sh, src);
    // Dropped before the buffers are read, as `main` drops the shell before it
    // returns: that is what joins the background jobs, and their output goes
    // into these same buffers.
    drop(sh);
    let out_s = out
        .lock()
        .map(|v| String::from_utf8_lossy(&v).into_owned())
        .unwrap_or_default();
    let err_s = err
        .lock()
        .map(|v| String::from_utf8_lossy(&v).into_owned())
        .unwrap_or_default();
    (status, out_s, err_s)
}

/// `run_capturing`'s stdout as raw BYTES. `echo`/`printf` escapes can name a
/// byte that is not UTF-8, which the lossy `String` above folds to U+FFFD --
/// so an assertion about, say, `\377` cannot tell 0xff from any other bad byte.
#[cfg(test)]
pub fn run_capturing_bytes(src: &str) -> (i32, Vec<u8>) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let mut sh = Shell::new_for_test();
    sh.fds.set(1, Fd::WriteBuf(out.clone()));
    let status = exec::run_program(&mut sh, src);
    drop(sh);
    let bytes = out.lock().map(|v| v.clone()).unwrap_or_default();
    (status, bytes)
}

/// Drive several units through the INTERACTIVE handler, as the prompt loop does,
/// returning `$?` plus captured stdout/stderr. Distinct from `run_capturing`:
/// only this path can show that a shell survives an aborted command.
#[cfg(test)]
pub fn run_capturing_interactive_units(units: &[&str]) -> (i32, String, String) {
    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let mut sh = Shell::new_for_test();
    sh.interactive = true;
    sh.fds.set(1, Fd::WriteBuf(out.clone()));
    sh.fds.set(2, Fd::WriteBuf(err.clone()));
    for unit in units {
        match crate::parser::parse_aliased(unit, &sh.aliases) {
            Ok(list) => {
                if let Some(code) = exec::run_interactive_unit(&mut sh, &list) {
                    sh.set_status(code);
                    break;
                }
            }
            Err(_) => sh.set_status(2),
        }
    }
    let status = sh.status;
    drop(sh);
    let text = |b: &Arc<Mutex<Vec<u8>>>| {
        b.lock().map(|v| String::from_utf8_lossy(&v).into_owned()).unwrap_or_default()
    };
    (status, text(&out), text(&err))
}


#[cfg(test)]
mod thread_state {
    /// Stage state crosses a thread boundary, which `run_pipeline` needs and
    /// nothing else in the crate does. Spelled out so that adding a field which
    /// is not `Send` fails HERE, naming the property, rather than inside the
    /// pipeline's closure.
    #[test]
    fn shell_state_can_cross_a_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<crate::exec::Shell>();
        assert_send::<super::Subshell>();
        assert_send::<super::Fds>();
    }
}
