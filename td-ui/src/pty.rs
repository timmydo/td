//! A PTY and the threads around it, for a terminal that runs a child.
//!
//! `Pty` opens `/dev/ptmx` without acquiring it, unlocks it, sizes it and
//! hands out its slave by descriptor, through the four `ioctl(2)` requests
//! UNSAFE.md section 19 pins in the raw layer. `spawn` starts a caller-composed
//! command on the slave; one reader, one writer and one waiter thread then
//! carry output, keyboard input and the child's exit, each reporting its
//! own ending on a channel, since nothing may join a thread parked in a
//! read. Which account, environment and command a terminal runs is its
//! caller's policy (td-term/DESIGN.md), not this module's.

use crate::sys;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::SyncSender;
#[cfg(test)]
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::thread::JoinHandle;

/// `O_NOCTTY` — the child claims the terminal as it starts (`spawn`). This
/// process must not acquire it by opening the master or the peer.
const O_NOCTTY: i32 = 0o400;

/// The kernel's hangup once the last slave descriptor is gone.
const EIO: i32 = 5;

pub const DEV_PTMX: &str = "/dev/ptmx";

/// What a PTY reader puts on its channel: output, and then exactly one
/// ending. The ending is a MESSAGE rather than only the thread's return value
/// because nothing joins these threads — td-term/DESIGN.md §4 forbids it, the read cannot be
/// interrupted — so a terminal that learned of a fault only from a join handle
/// would never learn of it at all.
#[derive(Debug)]
pub enum Output {
    Bytes(Vec<u8>),
    /// The child's last slave closed, which is the ordinary hangup — or the
    /// read failed, which is the same end with a reason.
    Ended(Result<(), String>),
}

/// What a child waiter puts on its channel, exactly once, for the same reason.
#[derive(Debug)]
pub enum Waited {
    Exited(ExitStatus),
    Failed(String),
}

/// td-term/DESIGN.md §2's PTY-output ceiling, as whole read chunks. A full channel blocks the
/// reader thread, which is how the kernel's PTY buffer backpressures the child.
pub const READ_CHUNK: usize = 8 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
pub const MAX_OUTPUT_CHUNKS: usize = MAX_OUTPUT_BYTES / READ_CHUNK;

/// The kernel's `struct winsize`. The pixel pair is reported as zero,
/// because a terminal here publishes a character grid.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowSize {
    pub rows: u16,
    pub columns: u16,
    pub x_pixels: u16,
    pub y_pixels: u16,
}

/// The eight bytes the kernel reads and writes, in its field order. A
/// `[u16; 4]` rather than a `#[repr(C)]` struct, so the ORDER is an
/// ordinary tested function: a swapped pair is a well-formed resize to
/// another size.
fn winsize_words(size: WindowSize) -> [u16; 4] {
    [size.rows, size.columns, size.x_pixels, size.y_pixels]
}

fn winsize_from_words(words: [u16; 4]) -> WindowSize {
    let [rows, columns, x_pixels, y_pixels] = words;
    WindowSize {
        rows,
        columns,
        x_pixels,
        y_pixels,
    }
}

/// What a terminal reports its grid to be, asked fresh: the master or any
/// descriptor of its slave.
pub fn window_size(terminal: &impl AsFd) -> Result<WindowSize, String> {
    let terminal = terminal
        .as_fd()
        .try_clone_to_owned()
        .map(File::from)
        .map_err(|e| format!("duplicate terminal: {e}"))?;
    sys::window_size(&terminal)
        .map(winsize_from_words)
        .map_err(|e| format!("TIOCGWINSZ: {e}"))
}

/// An open PTY master whose slave has been unlocked but not yet handed out.
pub struct Pty {
    master: File,
}

impl Pty {
    /// Open a master on `/dev/ptmx`, the one device the ioctls below are
    /// issued on, and unlock its slave.
    pub fn open() -> Result<Pty, String> {
        let master = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY)
            .open(DEV_PTMX)
            .map_err(|e| format!("open {DEV_PTMX}: {e}"))?;
        sys::unlock_pty(&master).map_err(|e| format!("TIOCSPTLCK: {e}"))?;
        Ok(Pty { master })
    }

    pub fn master(&self) -> &File {
        &self.master
    }

    #[allow(dead_code)]
    pub fn into_master(self) -> File {
        self.master
    }

    /// The slave, obtained from the master rather than by name.
    pub fn peer(&self) -> Result<File, String> {
        sys::pty_peer(&self.master).map_err(|e| format!("TIOCGPTPEER: {e}"))
    }

    /// What the terminal currently IS, asked fresh. `resize` verifies its own
    /// write, so nothing in production needs this yet; it exists so a caller
    /// that was NOT the one to set the size can ask — which today is the
    /// integration test, and with resize handling will be the client.
    #[allow(dead_code)]
    pub fn window(&self) -> Result<WindowSize, String> {
        window_size(&self.master)
    }

    /// Publish a grid size and verify it before anything may observe it. An
    /// unverified `TIOCSWINSZ` is indistinguishable at the call site from one
    /// the kernel clamped or ignored, and the child would then lay out its
    /// screen for a size the terminal does not have.
    pub fn resize(&self, rows: usize, columns: usize) -> Result<WindowSize, String> {
        let requested = grid_size(rows, columns)?;
        sys::set_window_size(&self.master, winsize_words(requested))
            .map_err(|e| format!("TIOCSWINSZ: {e}"))?;
        let observed = window_size(&self.master)?;
        if observed.rows != requested.rows || observed.columns != requested.columns {
            return Err(format!(
                "published {}x{} but the terminal reports {}x{}",
                requested.rows, requested.columns, observed.rows, observed.columns
            ));
        }
        Ok(observed)
    }
}

/// A grid the kernel can represent. Zero is not a size a terminal can be laid
/// out for, and the winsize fields are sixteen bits wide.
pub fn grid_size(rows: usize, columns: usize) -> Result<WindowSize, String> {
    let rows = u16::try_from(rows)
        .ok()
        .filter(|rows| *rows > 0)
        .ok_or_else(|| format!("terminal row count {rows} is not a representable grid"))?;
    let columns = u16::try_from(columns)
        .ok()
        .filter(|columns| *columns > 0)
        .ok_or_else(|| format!("terminal column count {columns} is not a representable grid"))?;
    Ok(WindowSize {
        rows,
        columns,
        x_pixels: 0,
        y_pixels: 0,
    })
}

/// The cell grid a tile of this many pixels holds. A tile too small for one
/// cell still gets a logical 1-by-1 grid, whose pixels the renderer clips to
/// the actual surface; a zero-row terminal has no representable state.
pub fn grid_for_tile(
    width: usize,
    height: usize,
    cell_width: usize,
    cell_height: usize,
) -> Result<(usize, usize), String> {
    if cell_width == 0 || cell_height == 0 {
        return Err(format!("font cell {cell_width}x{cell_height} has no area"));
    }
    let columns = width.checked_div(cell_width).unwrap_or(0).max(1);
    let rows = height.checked_div(cell_height).unwrap_or(0).max(1);
    Ok((rows, columns))
}

/// What a terminal execs: literal argv values and no shell. A program named
/// without a slash is found on the `PATH` of the environment the child is
/// given, by std's rule; a policy that names programs by absolute path, as
/// td's does, never searches.
///
/// `leads_session` makes the child lead a new session whose controlling
/// terminal is the slave, as an interactive shell needs for job control and
/// the line discipline's signals. A child that leads none leaves the slave
/// unowned, for a program that takes a terminal in a session of its own.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildCommand {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub leads_session: bool,
}

/// Start the child on the slave. The slave and all three parent-side clones are
/// consumed here and dropped before this returns, so only the master remains
/// and closing it produces the kernel's normal hangup.
///
/// `directory` is the account's verified home unless the typed launcher chose
/// a fixed task directory. Setting `HOME` does not move the child, so without
/// this the shell would start wherever td-svc left the graphical service. A
/// directory the child cannot enter fails rather than silently landing in `/`.
pub fn spawn<K: AsRef<OsStr>, V: AsRef<OsStr>>(
    command: &ChildCommand,
    environment: &[(K, V)],
    directory: &Path,
    slave: File,
) -> Result<Child, String> {
    let output = slave
        .try_clone()
        .map_err(|e| format!("duplicate terminal for child stdout: {e}"))?;
    let errors = slave
        .try_clone()
        .map_err(|e| format!("duplicate terminal for child stderr: {e}"))?;
    let mut process = Command::new(&command.program);
    process
        .args(&command.arguments)
        .env_clear()
        .current_dir(directory)
        .stdin(Stdio::from(slave))
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(errors));
    for (name, value) in environment {
        process.env(name, value);
    }
    if command.leads_session {
        sys::lead_session(&mut process);
    }
    process.spawn().map_err(|e| {
        format!(
            "spawn {}{} in {}: {e}",
            command.program.display(),
            if command.leads_session {
                " (as a session leader on the terminal)"
            } else {
                ""
            },
            directory.display()
        )
    })
}

/// A channel of td-term/DESIGN.md §2's output length, for tests that drive a
/// reader on its own. td-term's own channel is its window's, because it
/// carries the child waiter's events too and one queue can only have one
/// bound.
#[cfg(test)]
pub fn output_channel<T>() -> (SyncSender<T>, Receiver<T>) {
    sync_channel(MAX_OUTPUT_CHUNKS)
}

/// Pump the master into the bounded channel until hangup. A full channel blocks
/// this thread rather than dropping bytes; `EIO` is the kernel's hangup once the
/// last slave descriptor is gone, not a fault to report.
///
/// This thread owns a master descriptor and is parked in `read` whenever the
/// child is idle, and safe `std` offers no way to interrupt that: no poll, no
/// timeout, and closing a descriptor another thread is reading is not something
/// this crate may express. So the retirement path is the child's — its exit
/// closes the last slave and the read returns — and there is no path that
/// retires the reader while the child lives.
///
/// That is sound only because td-term is one process per terminal (td-term/DESIGN.md §1): closing
/// "the terminal" IS exiting, process exit closes this descriptor, and the
/// kernel then sends `SIGHUP` to the session that holds the slave as its
/// controlling terminal, which a child `spawn` started with `leads_session`
/// leads. A child that leads none learns of the hangup only as `EIO` on its
/// next read or write of the slave, so its retirement is its own exit. The
/// caller must therefore NOT join this handle on a teardown path — a detached
/// thread cannot delay process exit, but a join would wait for a read that
/// never returns. Interrupting the reader for any other reason needs a separately
/// reviewed wakeup surface.
///
/// `notify` runs after every send, so a loop that waits on something other
/// than the channel (a Wayland connection's waker) learns there is output.
pub fn spawn_reader<T: Send + 'static>(
    mut master: File,
    sender: SyncSender<T>,
    wrap: fn(Output) -> T,
    notify: impl Fn() + Send + 'static,
) -> Result<JoinHandle<Result<(), String>>, String> {
    thread::Builder::new()
        .name("pty-output".into())
        .spawn(move || {
            let mut buffer = vec![0u8; READ_CHUNK];
            let ended = loop {
                match master.read(&mut buffer) {
                    Ok(0) => break Ok(()),
                    Ok(count) => {
                        let Some(bytes) = buffer.get(..count) else {
                            break Err(format!(
                                "PTY read reported {count} bytes of a short buffer"
                            ));
                        };
                        if sender.send(wrap(Output::Bytes(bytes.to_vec()))).is_err() {
                            return Ok(());
                        }
                        notify();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    // EIO is the hangup this thread retires on. Every other
                    // errno is a real fault, and reporting it as a clean child
                    // exit would make a broken terminal look like a closed one.
                    Err(error) if error.raw_os_error() == Some(EIO) => break Ok(()),
                    Err(error) => break Err(format!("read terminal: {error}")),
                }
            };
            // The ending goes on the CHANNEL and not only through the join
            // handle. Nobody joins this thread — td-term/DESIGN.md §4 forbids it, since the
            // read it may be parked in cannot be interrupted — so a handle is
            // somewhere an error goes to be lost.
            let _ = sender.send(wrap(Output::Ended(ended.clone())));
            notify();
            ended
        })
        .map_err(|e| format!("spawn PTY reader: {e}"))
}

/// One write, reporting what the kernel actually took. `write_all` would lose
/// the remainder of a partial write, and those bytes are keystrokes with
/// nowhere to come back from.
///
/// The master is blocking, so a child that stops reading blocks this call once
/// the line discipline fills. td-term/DESIGN.md §4 puts the writer on its own thread for exactly
/// that reason.
fn write_chunk(sink: &mut impl Write, bytes: &[u8]) -> Result<usize, String> {
    loop {
        match sink.write(bytes) {
            Ok(0) => return Err("terminal accepted no input bytes".into()),
            Ok(count) => return Ok(count),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("write terminal input: {error}")),
        }
    }
}

/// Drain a keyboard queue into the child, consuming only what was taken.
#[allow(dead_code)]
pub fn write_input(master: &File, queue: &mut crate::vt_keys::InputQueue) -> Result<(), String> {
    let mut sink = master;
    while !queue.is_empty() {
        let taken = write_chunk(&mut sink, queue.front(READ_CHUNK))?;
        queue.consume(taken);
    }
    Ok(())
}

/// The keyboard queue the main loop fills and the writer thread drains.
///
/// ONE bounded queue rather than a queue plus a channel: td-term/DESIGN.md §2 admits a key
/// sequence whole or drops it whole, and a second buffer downstream would be a
/// second place for half of one to sit. The lock is held to copy bytes out —
/// which may contiguate the ring first — or to put a consumption back, NEVER
/// across the write, because
/// td-term/DESIGN.md §4 requires the main loop to enqueue without blocking and the master is
/// blocking — a child that stops reading parks the writer indefinitely once the
/// line discipline fills.
pub struct Input {
    pending: Mutex<Pending>,
    ready: Condvar,
}

struct Pending {
    queue: crate::vt_keys::InputQueue,
    closed: bool,
    failed: Option<String>,
}

impl Input {
    pub fn new() -> Arc<Input> {
        Arc::new(Input {
            pending: Mutex::new(Pending {
                queue: crate::vt_keys::InputQueue::new(),
                closed: false,
                failed: None,
            }),
            ready: Condvar::new(),
        })
    }

    /// Admit a sequence whole, or refuse it whole. `false` is the caller's cue
    /// to ring the visual bell: half a `CSI` reaching the child would be worse
    /// than the key never having been pressed.
    ///
    /// A writer that has DIED is an error rather than a refusal. The two look
    /// identical from a full queue — bytes going nowhere either way — but they
    /// are not the same news: td-term/DESIGN.md §2 defines `false` as "that sequence did not
    /// fit, ring the bell", and a terminal that beeps at every keystroke
    /// because its writer is gone would be reporting the wrong one forever.
    /// An explicit [`Input::close`] outranks both.
    pub fn push(&self, bytes: &[u8]) -> Result<bool, String> {
        let mut pending = self.pending.lock().map_err(|_| POISONED)?;
        // An explicit close outranks a death: the caller that closed the queue
        // is the one ending the session, so `false` is the answer it asked for
        // and a writer dying afterwards is not news to it.
        if pending.closed {
            return Ok(false);
        }
        if let Some(failure) = &pending.failed {
            return Err(format!("the terminal stopped accepting input: {failure}"));
        }
        let admitted = pending.queue.push(bytes);
        if admitted {
            self.ready.notify_one();
        }
        Ok(admitted)
    }

    /// Test support, public because a consumer's tests are another crate:
    /// what the writer would send next, taken, for tests that have no
    /// writer. The production drain is the writer thread's.
    #[doc(hidden)]
    pub fn take_for_test(&self) -> Vec<u8> {
        let Ok(mut pending) = self.pending.lock() else {
            return Vec::new();
        };
        let bytes = pending.queue.front(usize::MAX).to_vec();
        pending.queue.consume(bytes.len());
        bytes
    }

    /// Retire the writer once it has drained. Unlike the reader — parked in a
    /// `read` nothing safe can interrupt — the writer parks in `Condvar::wait`,
    /// which this wakes, so it HAS a retirement path.
    ///
    /// It is not an interruption, though. This sets the predicate the writer
    /// checks BETWEEN writes; a writer already inside a blocking `write` stays
    /// there, and nothing safe cancels one. A child that never reads does not
    /// by itself cause that — in the kernel's default canonical mode the line
    /// discipline accepts and discards rather than blocking, which is what the
    /// tests cover — but a child in RAW mode that stops reading does, and that
    /// is every shell and editor.
    ///
    /// Nor does the child's exit free such a writer. The last slave closing
    /// hangs up the READER, which is its whole retirement; the writer stays
    /// parked in `write` on the same terminal at the same instant. So the
    /// handle is joinable only for a writer that is not inside a write, and
    /// td-term's teardown is process exit rather than a join, exactly as it is
    /// for the reader.
    // Not on td-term's own teardown path, which is process exit rather than a
    // close (td-term/DESIGN.md §4), so this is exercised by tests and by whatever ends a
    // terminal without ending the process.
    #[allow(dead_code)]
    pub fn close(&self) -> Result<(), String> {
        let mut pending = self.pending.lock().map_err(|_| POISONED)?;
        pending.closed = true;
        self.ready.notify_all();
        Ok(())
    }

    /// Record that the writer died, so the next `push` says so. Best effort by
    /// construction: this runs while already returning an error, and a
    /// poisoned lock is not news it can improve on.
    fn fail(&self, message: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.failed = Some(message.to_string());
        }
    }

    #[cfg(test)]
    fn queued(&self) -> Result<usize, String> {
        Ok(self.pending.lock().map_err(|_| POISONED)?.queue.len())
    }
}

const POISONED: &str = "the keyboard queue lock was poisoned by a panicking thread";
const CHILD_POISONED: &str = "the child handover lock was poisoned by a panicking thread";

/// Pump the keyboard queue into a sink until it is closed and drained.
///
/// Bytes are copied out under the lock and written WITHOUT it, so a `push` on
/// the main loop never waits on a child that has stopped reading. That is the
/// whole design constraint, and it is why this takes a sink rather than the
/// master directly: against a real PTY the claim cannot fail visibly — in
/// canonical mode the line discipline accepts and discards rather than
/// blocking — so holding the lock across the write would cost a millisecond
/// there and forever in raw mode, with every test still green.
///
/// Only this thread consumes, so a partial write's remainder stays at the
/// front of the queue in order, whatever arrived meanwhile.
fn pump(sink: &mut impl Write, input: &Input) -> Result<(), String> {
    // Set up once: this is the loop a held key runs through, and the copy
    // exists only to get the bytes out from under the lock.
    let mut chunk = Vec::with_capacity(READ_CHUNK);
    loop {
        {
            let mut pending = input.pending.lock().map_err(|_| POISONED)?;
            while pending.queue.is_empty() && !pending.closed {
                pending = input.ready.wait(pending).map_err(|_| POISONED)?;
            }
            if pending.queue.is_empty() {
                return Ok(());
            }
            chunk.clear();
            chunk.extend_from_slice(pending.queue.front(READ_CHUNK));
        }
        let taken = write_chunk(sink, &chunk)?;
        input
            .pending
            .lock()
            .map_err(|_| POISONED)?
            .queue
            .consume(taken);
    }
}

/// Run [`pump`] on its own thread, recording a death where the main loop can
/// see it.
///
/// The handle is joinable, but see [`Input::close`] for WHEN: closing wakes a
/// writer waiting for bytes, not one already inside a blocking `write`.
pub fn spawn_writer(
    master: File,
    input: Arc<Input>,
) -> Result<JoinHandle<Result<(), String>>, String> {
    spawn_pump(master, input)
}

/// The one place a writer thread is created, so the tests that drive a sink
/// exercise the same failure publication the master does rather than a
/// re-implementation of it.
fn spawn_pump<W: Write + Send + 'static>(
    mut sink: W,
    input: Arc<Input>,
) -> Result<JoinHandle<Result<(), String>>, String> {
    thread::Builder::new()
        .name("pty-input".into())
        .spawn(move || {
            let outcome = pump(&mut sink, &input);
            if let Err(failure) = &outcome {
                input.fail(failure);
            }
            outcome
        })
        .map_err(|e| format!("spawn PTY writer: {e}"))
}

/// One exit, for tests that drive a waiter on its own: there is exactly one
/// child and it reports exactly once.
#[cfg(test)]
pub fn exit_channel<T>() -> (SyncSender<T>, Receiver<T>) {
    sync_channel(1)
}

/// Wait for the child and report how it went.
///
/// A send failure is not an error: it means the main loop is already gone, and
/// the child having exited is the only thing this thread had to say.
///
/// The child is handed over through a cell rather than moved into the closure,
/// so a failed spawn can take it back and kill it: `Builder::spawn` drops the
/// closure on failure, and dropping a `Child` neither signals nor reaps. Losing
/// it that way would leave a live process holding the slave — no hangup for the
/// reader, and a zombie once it exits.
///
/// `notify` runs after the send, as the reader's does.
pub fn spawn_waiter<T: Send + 'static>(
    child: Child,
    sender: SyncSender<T>,
    wrap: fn(Waited) -> T,
    notify: impl Fn() + Send + 'static,
) -> Result<JoinHandle<Result<(), String>>, String> {
    let held = Arc::new(Mutex::new(Some(child)));
    let carried = Arc::clone(&held);
    let spawned = thread::Builder::new()
        .name("pty-child".into())
        .spawn(move || {
            let waited = carried
                .lock()
                .map_err(|_| CHILD_POISONED.to_string())
                .and_then(|mut held| {
                    held.take()
                        .ok_or_else(|| "the child waiter was handed no child".to_string())
                })
                .and_then(|mut child| child.wait().map_err(|e| format!("wait for child: {e}")));
            // Reported on the channel for the reader's reason: this thread is
            // never joined either, so a failure returned alone is one the
            // terminal cannot act on.
            let message = match &waited {
                Ok(status) => Waited::Exited(*status),
                Err(error) => Waited::Failed(error.clone()),
            };
            // Deliberately ignored: a closed receiver means the main loop is
            // already gone, which is not this thread's problem to report.
            let _ = sender.send(wrap(message));
            notify();
            waited.map(|_| ())
        });
    match spawned {
        Ok(handle) => Ok(handle),
        Err(e) => Err(reap_unwatched(&held, &format!("spawn child waiter: {e}"))),
    }
}

/// Kill and reap a child no thread will wait for. Both failures are reported
/// when the cleanup fails too, since either message alone would misdescribe
/// what was left behind.
fn reap_unwatched(held: &Mutex<Option<Child>>, cause: &str) -> String {
    let taken = match held.lock() {
        Ok(mut slot) => slot.take(),
        Err(_) => return format!("{cause}; the child could not be reclaimed: {CHILD_POISONED}"),
    };
    let Some(mut child) = taken else {
        return cause.to_string();
    };
    if let Err(e) = child.kill() {
        return format!("{cause}; and the child could not be killed: {e}");
    }
    match child.wait() {
        Ok(_) => cause.to_string(),
        Err(e) => format!("{cause}; and the killed child could not be reaped: {e}"),
    }
}

/// The packaged binary's own check of the grid arithmetic and the
/// `winsize` field order. It opens no device: the live ioctl round trip is
/// a host test, because a target selftest runs wherever the artifact does,
/// including where devpts is not mounted.
pub fn selftest() -> Result<(), String> {
    let size = grid_size(24, 80)?;
    if (size.rows, size.columns) != (24, 80)
        || grid_size(0, 80).is_ok()
        || grid_size(24, 0).is_ok()
        || grid_size(1, 65_536).is_ok()
        || grid_for_tile(512, 320, 8, 16)? != (20, 64)
        || grid_for_tile(3, 3, 8, 16)? != (1, 1)
        || winsize_words(size) != [24, 80, 0, 0]
        || winsize_from_words([24, 80, 0, 0]) != size
    {
        return Err("PTY selftest derived the wrong grid".into());
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {

    use super::*;
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    use std::time::Duration;

    /// A constructed environment, as a terminal's own policy would pass:
    /// `spawn` sets exactly this and nothing inherited.
    fn fixture_environment() -> Vec<(String, String)> {
        vec![
            ("PATH".into(), "/bin".into()),
            ("TERM".into(), "td-term".into()),
        ]
    }

    fn open_pty() -> Pty {
        Pty::open().unwrap_or_else(|error| {
            panic!("this host cannot provide a PTY, which td-term requires: {error}")
        })
    }

    #[test]
    fn a_grid_must_be_representable_by_the_kernel() {
        assert_eq!(grid_size(24, 80).unwrap().rows, 24);
        assert_eq!(grid_size(24, 80).unwrap().columns, 80);
        assert_eq!(grid_size(24, 80).unwrap().x_pixels, 0);
        assert!(grid_size(0, 80).is_err());
        assert!(grid_size(24, 0).is_err());
        assert!(grid_size(65_536, 80).is_err());
        assert!(grid_size(24, 65_536).is_err());
        assert_eq!(grid_size(65_535, 65_535).unwrap().rows, 65_535);
    }

    #[test]
    fn a_tile_smaller_than_one_cell_still_has_a_logical_grid() {
        assert_eq!(grid_for_tile(512, 320, 8, 16).unwrap(), (20, 64));
        // Partial cells are not shown, so they are not counted.
        assert_eq!(grid_for_tile(519, 335, 8, 16).unwrap(), (20, 64));
        assert_eq!(grid_for_tile(0, 0, 8, 16).unwrap(), (1, 1));
        assert_eq!(grid_for_tile(7, 15, 8, 16).unwrap(), (1, 1));
        assert!(grid_for_tile(512, 320, 0, 16).is_err());
        assert!(grid_for_tile(512, 320, 8, 0).is_err());
    }

    /// The peer comes from the master, and the size the master publishes is the
    /// size the slave — the child's own descriptor — reports back.
    #[test]
    fn a_published_grid_is_the_one_the_slave_reports() {
        let pty = open_pty();
        let observed = pty.resize(24, 80).unwrap();
        assert_eq!((observed.rows, observed.columns), (24, 80));
        let slave = pty.peer().unwrap();
        let from_slave = window_size(&slave).unwrap();
        assert_eq!((from_slave.rows, from_slave.columns), (24, 80));
        // A later resize reaches the same already-open slave.
        pty.resize(40, 100).unwrap();
        let from_slave = window_size(&slave).unwrap();
        assert_eq!((from_slave.rows, from_slave.columns), (40, 100));
        assert!(pty.resize(0, 80).is_err());
    }

    /// Bytes written to the master reach the slave, and the reader thread
    /// delivers what the slave writes back.
    #[test]
    fn the_reader_thread_delivers_slave_output_until_hangup() {
        let pty = open_pty();
        pty.resize(24, 80).unwrap();
        let mut slave = pty.peer().unwrap();
        let master = pty.master().try_clone().unwrap();
        let (sender, receiver) = output_channel();
        let reader = spawn_reader(master, sender, std::convert::identity, || {}).unwrap();

        // Through the bounded queue, as the writer thread will: the queue is
        // what makes a partial write recoverable.
        let mut queue = crate::vt_keys::InputQueue::new();
        assert!(queue.push(b"input\n"));
        write_input(pty.master(), &mut queue).unwrap();
        assert!(queue.is_empty(), "the writer consumed only what it wrote");
        let mut seen = Vec::new();
        while !seen.windows(6).any(|window| window == b"input\n") {
            let mut chunk = [0u8; 64];
            let count = slave.read(&mut chunk).unwrap();
            assert_ne!(count, 0, "the slave saw hangup before its input");
            seen.extend_from_slice(&chunk[..count]);
        }

        slave.write_all(b"output\n").unwrap();
        let mut delivered = Vec::new();
        while !delivered.windows(6).any(|window| window == b"output") {
            match receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("the reader thread stopped before delivering output")
            {
                Output::Bytes(chunk) => delivered.extend_from_slice(&chunk),
                ended => panic!("the reader ended before its output: {ended:?}"),
            }
        }

        // Dropping every slave descriptor is the kernel's hangup, and the
        // reader retires on it rather than reporting a fault. The channel is
        // observed FIRST: a leaked parent-side slave would leave the reader
        // parked in `read` forever, and joining first would hang the gate
        // instead of failing it — `cargo test` has no per-test timeout.
        drop(slave);
        // The hangup is REPORTED and not only survived. Nothing joins this
        // thread in production, so the message is the terminal's only way to
        // learn its child's output has ended — and the disconnect that
        // follows is what says it said so exactly once.
        loop {
            match receiver.recv_timeout(Duration::from_secs(30)) {
                // Whatever the line discipline still owed, which is not this
                // assertion's business.
                Ok(Output::Bytes(_)) => {}
                Ok(Output::Ended(Ok(()))) => break,
                other => panic!("the reader did not report the hangup: {other:?}"),
            }
        }
        assert!(
            matches!(
                receiver.recv_timeout(Duration::from_secs(30)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
            ),
            "the reader did not retire on hangup"
        );
        reader.join().unwrap().unwrap();
    }

    /// The child gets the slave on all three descriptors, sees the grid the
    /// master published, and receives exactly the constructed environment.
    #[test]
    fn a_spawned_child_inherits_the_slave_and_the_published_grid() {
        spawn_fixture(true, "leader slave");
    }

    /// A child told to lead no session leaves the slave unowned, for a
    /// program that takes a terminal in a session of its own.
    #[test]
    fn a_child_that_leads_no_session_leaves_the_terminal_unowned() {
        spawn_fixture(false, "member not-slave");
    }

    fn spawn_fixture(leads_session: bool, observed: &str) {
        let pty = open_pty();
        pty.resize(31, 97).unwrap();
        let slave = pty.peer().unwrap();
        let command = ChildCommand {
            program: std::env::current_exe().unwrap(),
            leads_session,
            arguments: vec![
                "--exact".into(),
                "pty::tests::pty_child_fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
        };
        let mut environment = fixture_environment();
        environment.push((FIXTURE.into(), "1".into()));
        let home = std::env::temp_dir();
        let mut child = spawn(&command, &environment, &home, slave).unwrap();

        let master = pty.into_master();
        let (sender, receiver) = output_channel();
        let reader = spawn_reader(master, sender, std::convert::identity, || {}).unwrap();
        let mut seen = String::new();
        let marker = loop {
            match receiver.recv_timeout(Duration::from_secs(30)) {
                Ok(Output::Bytes(chunk)) => {
                    seen.push_str(&String::from_utf8_lossy(&chunk));
                    // Complete lines only. `str::lines` yields an
                    // unterminated tail as a line, so a chunk boundary inside
                    // the marker would otherwise be accepted as a short one.
                    let mut marker = None;
                    let complete = seen.rsplit_once('\n').map_or("", |(head, _)| head);
                    for line in complete.lines() {
                        if let Some(tail) = line.trim_end().strip_prefix("TD-TERM-FIXTURE ") {
                            marker = Some(tail.to_string());
                        }
                    }
                    if let Some(marker) = marker {
                        break marker;
                    }
                }
                Ok(ended) => panic!("the reader ended before the marker: {ended:?}"),
                Err(error) => panic!("no fixture marker in {seen:?}: {error}"),
            }
        };
        // rows, columns, TERM, environment size, whether it leads its session
        // and holds a controlling terminal, and working directory, all as the
        // child itself observed them. The directory is the one passed to
        // `spawn`: setting HOME does not move a child, so this is what proves
        // the shell starts where its own environment says it does.
        let home = home.canonicalize().unwrap();
        assert_eq!(
            marker,
            format!("31 97 td-term 3 {observed} {}", home.display())
        );
        let status = child.wait().unwrap();
        assert!(
            status.success(),
            "the fixture failed its own checks: {status}"
        );
        // Same ordering as above: the child's exit closed the last slave, so
        // the channel must disconnect before the join can be safe.
        while receiver.recv_timeout(Duration::from_secs(30)).is_ok() {}
        reader.join().unwrap().unwrap();
    }

    const FIXTURE: &str = "TD_TERM_PTY_FIXTURE";
    const SILENT_FIXTURE: &str = "TD_TERM_PTY_SILENT_FIXTURE";

    /// The child half of the test above: it runs only when the parent asks for
    /// it, and reports what it sees on its own stdin.
    #[test]
    #[ignore]
    fn pty_child_fixture() {
        if std::env::var_os(FIXTURE).is_none() {
            return;
        }
        let size = window_size(&std::io::stdin()).unwrap();
        let term = std::env::var("TERM").unwrap_or_default();
        let count = std::env::vars_os().count();
        let directory = std::env::current_dir().unwrap();
        // `/proc/self/stat` after the command name: state, parent, group,
        // session, controlling terminal. The terminal is compared with the
        // slave on stdin rather than asked of `/dev/tty`, which opens for
        // whatever terminal the test runner's own session holds.
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let fields: Vec<&str> = stat.rsplit_once(") ").unwrap().1.split(' ').collect();
        let leader = if fields[3] == std::process::id().to_string() {
            "leader"
        } else {
            "member"
        };
        let tty_nr: u64 = fields[4].parse().unwrap();
        let rdev = std::fs::metadata("/proc/self/fd/0").unwrap().rdev();
        let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
        let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
        let slave = (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12);
        let tty = if tty_nr == slave {
            "slave"
        } else {
            "not-slave"
        };
        println!(
            "TD-TERM-FIXTURE {} {} {term} {count} {leader} {tty} {}",
            size.rows,
            size.columns,
            directory.display()
        );
    }

    /// A terminal another session already holds is not taken: the claim uses
    /// argument zero, which never steals, and its refusal fails the spawn
    /// rather than starting a child without the session it asked for.
    #[test]
    fn a_session_is_not_claimed_on_a_terminal_another_session_holds() {
        let pty = open_pty();
        let command = |millis: &str| {
            let mut environment = fixture_environment();
            environment.push((SILENT_FIXTURE.into(), millis.into()));
            let command = ChildCommand {
                program: std::env::current_exe().unwrap(),
                leads_session: true,
                arguments: vec![
                    "--exact".into(),
                    "pty::tests::pty_silent_child_fixture".into(),
                    "--ignored".into(),
                    "--nocapture".into(),
                ],
            };
            (command, environment)
        };
        let (first, environment) = command("30000");
        let slave = pty.peer().unwrap();
        let mut holder = spawn(&first, &environment, &std::env::temp_dir(), slave).unwrap();
        let (second, environment) = command("0");
        let slave = pty.peer().unwrap();
        let refused = spawn(&second, &environment, &std::env::temp_dir(), slave);
        // Reaped before anything is asserted, so a failure leaves no holder.
        holder.kill().unwrap();
        holder.wait().unwrap();
        let refused = refused.unwrap_err();
        assert!(refused.contains("(as a session leader"), "{refused}");
        assert!(refused.contains("os error 1"), "EPERM: {refused}");
    }

    /// A child that holds the terminal and never reads a byte of it, so a
    /// writer can be parked in `write` against a live process.
    #[test]
    #[ignore = "run as a child of the parked-writer test"]
    fn pty_silent_child_fixture() {
        let Some(millis) = std::env::var_os(SILENT_FIXTURE) else {
            return;
        };
        // The parent picks the lifetime: long enough to outlive a drain in one
        // test, and long enough in another that reaping a child NOT killed
        // would blow a deadline rather than quietly succeed.
        let millis: u64 = millis.to_string_lossy().parse().unwrap();
        thread::sleep(Duration::from_millis(millis));
    }

    /// Cross-thread waits here are bounded. `cargo test` has no per-test
    /// timeout, so an unbounded one turns a regression into a hung gate
    /// instead of a red test — the same policy the reader's tests follow.
    fn within<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        let (sender, receiver) = sync_channel(1);
        thread::spawn(move || {
            let _ = sender.send(work());
        });
        receiver.recv_timeout(Duration::from_secs(30)).ok()
    }

    /// Read one helping from a terminal, without waiting forever for it.
    fn read_within(file: &File) -> Option<Vec<u8>> {
        let mut clone = file.try_clone().unwrap();
        within(move || {
            let mut seen = vec![0u8; 64];
            let count = clone.read(&mut seen).unwrap();
            seen.truncate(count);
            seen
        })
    }

    /// A sink that parks INSIDE `write` until released, which is the state no
    /// real PTY can be held in from this crate.
    struct GatedSink {
        entered: SyncSender<()>,
        release: Receiver<()>,
        written: Arc<Mutex<Vec<u8>>>,
        outcome: Option<String>,
    }

    impl Write for GatedSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let _ = self.entered.send(());
            let _ = self.release.recv();
            if let Some(failure) = &self.outcome {
                return Err(std::io::Error::other(failure.clone()));
            }
            if let Ok(mut written) = self.written.lock() {
                written.extend_from_slice(bytes);
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The case the retirement contract is really about: a child that holds
    /// the terminal and never reads a byte. In the kernel's default canonical
    /// mode that does NOT park the writer — the line discipline discards what
    /// its buffer cannot hold — so the close still retires it, and the drain
    /// here completes long before the child's own exit. Raw mode is the case
    /// that can park a writer, and this crate cannot construct it: its ioctl
    /// roster carries no termios request, by design.
    #[test]
    fn a_child_that_never_reads_does_not_trap_the_writer() {
        let pty = open_pty();
        pty.resize(24, 80).unwrap();
        let slave = pty.peer().unwrap();
        let command = ChildCommand {
            program: std::env::current_exe().unwrap(),
            leads_session: false,
            arguments: vec![
                "--exact".into(),
                "pty::tests::pty_silent_child_fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
        };
        let mut environment = fixture_environment();
        environment.push((SILENT_FIXTURE.into(), "1500".into()));
        // `spawn` consumes the slave and both clones into the child's stdio,
        // so the child is the only holder and its exit is the last close.
        let child = spawn(&command, &environment, &std::env::temp_dir(), slave).unwrap();
        let input = Input::new();
        let master = pty.into_master();
        let writer = spawn_writer(master.try_clone().unwrap(), Arc::clone(&input)).unwrap();
        // The child writes nothing deliberately, but its harness does, and the
        // terminal echoes every byte pushed below back at the master. Both have
        // to keep moving or the CHILD blocks in its own write — a deadlock of
        // this test's making rather than anything about the writer.
        let (chunks, output) = output_channel();
        let reader = spawn_reader(master, chunks, std::convert::identity, || {}).unwrap();
        let drain = thread::spawn(move || while output.recv().is_ok() {});

        let sequence = vec![b'z'; 1024];
        let mut pushed = 0;
        while pushed < 1024 * 1024 && input.push(&sequence).unwrap() {
            pushed += sequence.len();
        }
        input.close().unwrap();
        // The close retires it: nothing here waits for the child, which is
        // still asleep and has read none of this.
        within(move || writer.join())
            .expect("the writer never retired")
            .unwrap()
            .unwrap();

        let (sender, receiver) = exit_channel();
        spawn_waiter(child, sender, std::convert::identity, || {}).unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(30)),
            Ok(Waited::Exited(_))
        ));
        // The child was the only slave holder, so its exit is the reader's EOF.
        within(move || reader.join())
            .expect("the reader never saw the hangup")
            .unwrap()
            .unwrap();
        drain.join().unwrap();
    }

    /// The commit's central invariant, and the only place it can fail: a push
    /// completes while the writer is INSIDE a write. Against a real PTY this
    /// cannot be observed — canonical mode accepts and discards rather than
    /// blocking — so moving `write_chunk` inside the lock scope leaves every
    /// other test green while making the main loop wait on the child forever.
    #[test]
    fn the_queue_lock_is_not_held_across_a_write() {
        let (entered, arrivals) = sync_channel(1);
        let (release, held) = sync_channel(1);
        let written = Arc::new(Mutex::new(Vec::new()));
        let sink = GatedSink {
            entered,
            release: held,
            written: Arc::clone(&written),
            outcome: None,
        };
        let input = Input::new();
        assert!(input.push(b"first").unwrap());

        let writer = spawn_pump(sink, Arc::clone(&input)).unwrap();
        arrivals
            .recv_timeout(Duration::from_secs(30))
            .expect("the writer never reached its write");

        // The writer is now parked in `write` holding nothing. A push that
        // waits on it never returns, so this is bounded rather than joined.
        let pushing = Arc::clone(&input);
        let admitted = within(move || pushing.push(b"second"))
            .expect("a push waited on a writer that was inside a write");
        assert!(admitted.unwrap());

        let _ = release.send(());
        let _ = release.send(());
        input.close().unwrap();
        drop(release);
        within(move || writer.join())
            .expect("the writer never retired")
            .unwrap()
            .unwrap();
        assert_eq!(written.lock().unwrap().as_slice(), b"firstsecond");
    }

    /// A writer that died is not a full queue. Both look like bytes going
    /// nowhere, but td-term/DESIGN.md §2's `false` means "ring the bell", and a session whose
    /// writer is gone would ring it at every keystroke forever.
    #[test]
    fn a_dead_writer_is_reported_to_the_next_push() {
        let (entered, arrivals) = sync_channel(1);
        let (release, held) = sync_channel(1);
        let sink = GatedSink {
            entered,
            release: held,
            written: Arc::new(Mutex::new(Vec::new())),
            outcome: Some("the terminal hung up".into()),
        };
        let input = Input::new();
        assert!(input.push(b"doomed").unwrap());
        let writer = spawn_pump(sink, Arc::clone(&input)).unwrap();
        arrivals
            .recv_timeout(Duration::from_secs(30))
            .expect("the writer never reached its write");
        let _ = release.send(());
        // The thread publishes the failure before it returns, so a join that
        // has completed is proof the record was written the production way.
        within(move || writer.join())
            .expect("the writer never retired")
            .unwrap()
            .unwrap_err();

        let failure = input.push(b"x").unwrap_err();
        assert!(
            failure.contains("hung up"),
            "a dead writer read as a full queue: {failure}"
        );
    }

    /// A close outranks a death that follows it. The main loop that closed
    /// the queue is the one ending the session, so the answer stays the `false`
    /// it asked for rather than becoming an error about a writer it retired.
    #[test]
    fn a_close_outranks_a_writer_that_dies_after_it() {
        let (entered, arrivals) = sync_channel(1);
        let (release, held) = sync_channel(1);
        let sink = GatedSink {
            entered,
            release: held,
            written: Arc::new(Mutex::new(Vec::new())),
            outcome: Some("the terminal hung up".into()),
        };
        let input = Input::new();
        assert!(input.push(b"doomed").unwrap());
        let writer = spawn_pump(sink, Arc::clone(&input)).unwrap();
        arrivals
            .recv_timeout(Duration::from_secs(30))
            .expect("the writer never reached its write");

        input.close().unwrap();
        let _ = release.send(());
        within(move || writer.join())
            .expect("the writer never retired")
            .unwrap()
            .unwrap_err();
        assert!(!input.push(b"x").unwrap());
    }

    /// The wakeup discipline itself. Both notifications are load-bearing and
    /// they mask each other: a push after the writer has parked needs `push`'s
    /// own `notify_one`, and nothing here closes, so `close`'s `notify_all`
    /// cannot stand in for it.
    #[test]
    fn a_push_wakes_a_writer_that_has_already_drained() {
        let pty = open_pty();
        let slave = pty.peer().unwrap();
        let input = Input::new();
        let master = pty.into_master();
        let writer = spawn_writer(master.try_clone().unwrap(), Arc::clone(&input)).unwrap();

        assert!(input.push(b"first\n").unwrap());
        assert_eq!(read_within(&slave).expect("no first line"), b"first\n");
        // The writer is parked in `wait` now, with an empty queue and no close
        // coming. Only the push below can move it.
        assert!(input.push(b"second\n").unwrap());
        assert_eq!(read_within(&slave).expect("no second line"), b"second\n");

        input.close().unwrap();
        within(move || writer.join())
            .expect("the writer never retired")
            .unwrap()
            .unwrap();
    }

    /// What the main loop enqueues is what the child reads, in order.
    #[test]
    fn the_writer_delivers_what_was_enqueued_and_retires_on_close() {
        let pty = open_pty();
        let slave = pty.peer().unwrap();
        let input = Input::new();
        // The writer takes a CLONE, as it will in the client: the reader owns
        // one too, and a writer holding the only master would hang the
        // terminal up the moment it retired.
        let master = pty.into_master();
        let writer = spawn_writer(master.try_clone().unwrap(), Arc::clone(&input)).unwrap();

        assert!(input.push(b"hello ").unwrap());
        assert!(input.push(b"world\n").unwrap());
        // Canonical input, so the slave sees the line once its newline lands.
        assert_eq!(
            read_within(&slave).expect("no line arrived"),
            b"hello world\n"
        );

        // Closing drains and retires, which is why this handle can be joined
        // where the reader's cannot.
        assert!(input.push(b"tail\n").unwrap());
        input.close().unwrap();
        within(move || writer.join())
            .expect("the writer never retired")
            .unwrap()
            .unwrap();
        assert_eq!(
            read_within(&slave).expect("the tail never drained"),
            b"tail\n"
        );
        // A push after the close is refused rather than queued for a writer
        // that has gone.
        assert!(!input.push(b"x").unwrap());
    }

    /// The queue is bounded, and a sequence that does not fit is refused whole
    /// rather than truncated — the caller rings the bell. Pushing until it
    /// refuses also shows that a push never waits on the writer: the child
    /// here never reads, so the writer is parked in `write` throughout.
    #[test]
    fn a_full_queue_refuses_whole_sequences_without_blocking_the_pusher() {
        let pty = open_pty();
        let _slave = pty.peer().unwrap();
        let input = Input::new();
        let master = pty.into_master();
        let writer = spawn_writer(master.try_clone().unwrap(), Arc::clone(&input)).unwrap();

        // A megabyte at a child that never reads one byte of it. WHETHER the
        // queue fills is up to the scheduler — in canonical mode the writer is
        // never blocked draining, since the line discipline takes bytes and
        // discards what it cannot hold — so nothing here asserts a refusal.
        // What must hold either way is the ceiling. Where that ceiling FALLS
        // is the next test's job, without a writer and without the kernel.
        let sequence = vec![b'z'; 1024];
        let mut pushed = 0;
        while pushed < 1024 * 1024 {
            pushed += sequence.len();
            if !input.push(&sequence).unwrap() {
                break;
            }
            assert!(input.queued().unwrap() <= crate::vt_keys::MAX_INPUT_BYTES);
        }
        input.close().unwrap();
        drop(_slave);
        let _ = within(move || writer.join()).expect("the writer never retired");
    }

    #[test]
    fn a_full_queue_refuses_a_sequence_whole_rather_than_admitting_part_of_it() {
        // No writer: nothing drains, so the ceiling is the queue's own and the
        // kernel has no say in where it falls.
        let input = Input::new();
        let sequence = vec![b'z'; 1000];
        let mut admitted = 0;
        loop {
            let before = input.queued().unwrap();
            if input.push(&sequence).unwrap() {
                admitted += 1;
                assert_eq!(input.queued().unwrap(), before + sequence.len());
                continue;
            }
            assert_eq!(
                input.queued().unwrap(),
                before,
                "a refused sequence left bytes behind"
            );
            break;
        }
        let queued = input.queued().unwrap();
        assert_eq!(queued, admitted * sequence.len());
        assert!(queued <= crate::vt_keys::MAX_INPUT_BYTES);
        // The refusal is the queue being unable to take a WHOLE sequence, not
        // it being full: there is room left, just not this much.
        assert!(queued + sequence.len() > crate::vt_keys::MAX_INPUT_BYTES);
    }

    #[test]
    fn the_waiter_reports_the_childs_own_exit() {
        let pty = open_pty();
        pty.resize(24, 80).unwrap();
        let slave = pty.peer().unwrap();
        let command = ChildCommand {
            program: std::env::current_exe().unwrap(),
            leads_session: false,
            arguments: vec![
                "--exact".into(),
                "pty::tests::pty_child_fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
        };
        let mut environment = fixture_environment();
        environment.push((FIXTURE.into(), "1".into()));
        let child = spawn(&command, &environment, &std::env::temp_dir(), slave).unwrap();

        let (sender, receiver) = exit_channel();
        let waiter = spawn_waiter(child, sender, std::convert::identity, || {}).unwrap();
        // Drain the master so the fixture's own output cannot fill the buffer
        // and stall the exit this is waiting for.
        let master = pty.into_master();
        let (chunks, output) = output_channel();
        let reader = spawn_reader(master, chunks, std::convert::identity, || {}).unwrap();

        let Waited::Exited(status) = receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("the child waiter reported no exit")
        else {
            panic!("the waiter reported a failure rather than an exit")
        };
        assert!(
            status.success(),
            "the fixture failed its own checks: {status}"
        );
        waiter.join().unwrap().unwrap();
        // Only a DISCONNECT ends this: a timeout here means the reader is
        // still parked, and joining it then would hang rather than fail.
        loop {
            match output.recv_timeout(Duration::from_secs(30)) {
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    panic!("the reader never saw the child's hangup")
                }
            }
        }
        reader.join().unwrap().unwrap();
    }

    #[test]
    fn a_child_no_thread_will_wait_for_is_killed_and_reaped() {
        let pty = open_pty();
        pty.resize(24, 80).unwrap();
        let slave = pty.peer().unwrap();
        let command = ChildCommand {
            program: std::env::current_exe().unwrap(),
            leads_session: false,
            arguments: vec![
                "--exact".into(),
                "pty::tests::pty_silent_child_fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
        };
        let mut environment = fixture_environment();
        // A child that outlives this test by a wide margin, because the kill
        // is the half a self-exiting fixture cannot prove: reaping one that
        // was never signalled would simply wait for it and still pass. In
        // production the child is a shell, which does not exit on its own.
        environment.push((SILENT_FIXTURE.into(), "600000".into()));
        let child = spawn(&command, &environment, &std::env::temp_dir(), slave).unwrap();
        let pid = child.id();

        let held = Mutex::new(Some(child));
        let cleaned = within(move || reap_unwatched(&held, "spawn child waiter: no"))
            .expect("reaping waited on a child that was never killed");
        assert_eq!(cleaned, "spawn child waiter: no");
        // Both mutations show here: a dropped child is still blocked on the
        // slave, and a killed but unreaped one is a zombie. /proc keeps a
        // directory for either.
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the child was left behind"
        );
        // Nothing to reclaim twice, and that is not itself a failure. (The
        // check above follows the reap, so a recycled pid would need a whole
        // cycle inside that window.)
        assert_eq!(reap_unwatched(&Mutex::new(None), "again"), "again");
    }

    #[test]
    fn the_selftest_covers_the_grid_and_the_field_order() {
        selftest().unwrap();
    }
}
