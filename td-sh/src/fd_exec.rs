//! Install a shell environment's file descriptors for an external command.
//! Numbered destinations are installed on a short-lived Linux thread with
//! a private fd table. Only the PID or an error crosses back to the caller.
use crate::allocation;
use crate::process::{Fd, Fds};
use crate::sys;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::JoinHandle;

fn oom(_: allocation::Error) -> io::Error {
    io::ErrorKind::OutOfMemory.into()
}

struct Capture {
    buffer: Arc<Mutex<Vec<u8>>>,
    writer: std::io::PipeWriter,
    drain: JoinHandle<io::Result<()>>,
}

impl Capture {
    fn new(buffer: &Arc<Mutex<Vec<u8>>>) -> io::Result<Self> {
        let (mut reader, writer) = io::pipe()?;
        let sink = Arc::clone(buffer);
        let drain = std::thread::Builder::new().spawn(move || {
            let mut block = [0; 8192];
            loop {
                let n = match reader.read(&mut block) {
                    Ok(0) => return Ok(()),
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                };
                let mut out = sink.lock().map_err(|_| io::ErrorKind::Other)?;
                if let Some(bytes) = block.get(..n) {
                    allocation::extend_from_slice(&mut out, bytes).map_err(oom)?;
                }
            }
        })?;
        Ok(Self {
            buffer: Arc::clone(buffer),
            writer,
            drain,
        })
    }
}

fn capture_writer<'a>(
    captures: &'a mut Vec<Capture>,
    buffer: &Arc<Mutex<Vec<u8>>>,
) -> io::Result<BorrowedFd<'a>> {
    let index = match captures
        .iter()
        .position(|entry| Arc::ptr_eq(&entry.buffer, buffer))
    {
        Some(index) => index,
        None => {
            let index = captures.len();
            allocation::push(captures, Capture::new(buffer)?).map_err(oom)?;
            index
        }
    };
    captures
        .get(index)
        .map(|entry| entry.writer.as_fd())
        .ok_or(io::ErrorKind::Other.into())
}

// Standard streams remain open for the interpreter's lifetime. The private
// worker borrows a stream only when its numbered destination keeps it.
static STREAMS: LazyLock<(io::Stdin, io::Stdout, io::Stderr)> =
    LazyLock::new(|| (io::stdin(), io::stdout(), io::stderr()));

enum Source<'a> {
    Copy(usize),
    Borrowed(BorrowedFd<'a>),
    Closed,
}

struct Mapping<'a> {
    dest: i32,
    source: Source<'a>,
}

struct Io<'a> {
    mappings: Vec<Mapping<'a>>,
    copies: Vec<(Option<i32>, sys::Transfer)>,
    captures: Vec<Capture>,
}

// A closed destination only gains CLOEXEC, so it can still supply a dup.
fn source_stays_open(fds: &Fds, raw: i32) -> bool {
    match fds.get(raw as u32) {
        Some(Fd::File(file)) => file.as_raw_fd() == raw,
        Some(Fd::Inherit(n)) => i32::from(*n) == raw,
        Some(Fd::Closed) => raw > 2,
        None => true,
        _ => false,
    }
}

impl<'a> Io<'a> {
    fn prepare(fds: &'a Fds, cmd: &mut Command) -> io::Result<Self> {
        let numbered = fds.slots().any(|(n, _)| *n > 2);
        let mut io = Self {
            mappings: Vec::new(),
            copies: Vec::new(),
            captures: Vec::new(),
        };
        let mut floor = 3;
        let (stdin, stdout, stderr) = &*STREAMS;
        for (&dest, entry) in fds.slots() {
            if matches!(entry, Fd::Inherit(n) if dest == u32::from(*n)) {
                continue;
            }
            let retained = match entry {
                Fd::File(file) => Some(file.as_fd()),
                Fd::Inherit(0) => Some(stdin.as_fd()),
                Fd::Inherit(1) => Some(stdout.as_fd()),
                Fd::Inherit(2) => Some(stderr.as_fd()),
                _ => None,
            };
            if let Some(source) =
                retained.filter(|fd| numbered && source_stays_open(fds, fd.as_raw_fd()))
            {
                let dest = i32::try_from(dest).map_err(|_| io::ErrorKind::InvalidInput)?;
                allocation::push(
                    &mut io.mappings,
                    Mapping {
                        dest,
                        source: Source::Borrowed(source),
                    },
                )
                .map_err(oom)?;
                continue;
            }
            let temp;
            let source = match (retained, entry) {
                (Some(source), _) => source,
                (_, Fd::WriteBuf(buf)) => capture_writer(&mut io.captures, buf)?,
                (_, Fd::Closed) if dest > 2 => {
                    if let Ok(dest) = i32::try_from(dest) {
                        allocation::push(
                            &mut io.mappings,
                            Mapping {
                                dest,
                                source: Source::Closed,
                            },
                        )
                        .map_err(oom)?;
                    }
                    continue;
                }
                (_, Fd::Null | Fd::Closed) => {
                    temp = OpenOptions::new()
                        .read(dest == 0 || dest > 2)
                        .write(dest != 0)
                        .open("/dev/null")?;
                    temp.as_fd()
                }
                _ => return Err(io::ErrorKind::InvalidInput.into()),
            };
            if numbered {
                let dest = i32::try_from(dest).map_err(|_| io::ErrorKind::InvalidInput)?;
                // Persistent sources can share a saved copy. A temporary
                // /dev/null handle may reuse another temporary's number.
                let key = (!matches!(entry, Fd::Null | Fd::Closed)).then(|| source.as_raw_fd());
                let index = match key
                    .and_then(|k| io.copies.iter().position(|(saved, _)| *saved == Some(k)))
                {
                    Some(index) => index,
                    None => {
                        let copy = loop {
                            let copy = sys::Transfer::new(source, floor)?;
                            let raw = copy.fd().as_raw_fd();
                            floor = raw.checked_add(1).ok_or(io::ErrorKind::InvalidInput)?;
                            if matches!(fds.get(raw as u32), None | Some(Fd::Closed)) {
                                break copy;
                            }
                        };
                        let index = io.copies.len();
                        allocation::push(&mut io.copies, (key, copy)).map_err(oom)?;
                        index
                    }
                };
                allocation::push(
                    &mut io.mappings,
                    Mapping {
                        dest,
                        source: Source::Copy(index),
                    },
                )
                .map_err(oom)?;
            } else {
                let file = Stdio::from(source.try_clone_to_owned()?);
                match dest {
                    0 => cmd.stdin(file),
                    1 => cmd.stdout(file),
                    2 => cmd.stderr(file),
                    _ => return Err(io::ErrorKind::InvalidInput.into()),
                };
            }
        }
        Ok(io)
    }

    fn private<T: Send>(&self, launch: impl FnOnce() -> io::Result<T> + Send) -> io::Result<T> {
        crate::process::on_shell_stack(|| {
            let table = sys::PrivateTable::split()?;
            for map in &self.mappings {
                match &map.source {
                    Source::Copy(index) => {
                        let (_, source) =
                            self.copies.get(*index).ok_or(io::ErrorKind::InvalidInput)?;
                        table.install(map.dest, Some(source.fd()))?;
                    }
                    Source::Closed => table.install(map.dest, None)?,
                    Source::Borrowed(fd) => table.install(map.dest, Some(*fd))?,
                }
            }
            launch()
        })?
    }
}

enum Process {
    Native(std::process::Child),
    Pid(u32),
}

pub(super) struct Child {
    process: Process,
    drains: Vec<JoinHandle<io::Result<()>>>,
}

impl Child {
    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = match &mut self.process {
            Process::Native(c) => c.wait(),
            Process::Pid(pid) => {
                sys::wait_child(*pid, sys::Wait::Exit)?.ok_or(io::ErrorKind::Other.into())
            }
        };
        self.finish_drains()?;
        status
    }

    fn finish_drains(&mut self) -> io::Result<()> {
        for thread in self.drains.drain(..) {
            JoinHandle::join(thread).map_err(|_| io::ErrorKind::Other)??;
        }
        Ok(())
    }

    pub(super) fn id(&self) -> u32 {
        match &self.process {
            Process::Native(child) => child.id(),
            Process::Pid(pid) => *pid,
        }
    }
}

/// The child leads a group created by Command::process_group(0).
pub(super) struct TerminalJob {
    pub child: Child,
    pub stopped: Option<i32>,
    pub exited: Option<ExitStatus>,
    modes: Option<[u8; sys::TERMIOS_LEN]>,
}

impl TerminalJob {
    pub fn new(child: Child) -> Self {
        Self {
            child,
            stopped: None,
            exited: None,
            modes: None,
        }
    }

    pub fn observe(&mut self, poll: bool) -> io::Result<Option<ExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        if self.exited.is_some() {
            return Ok(self.exited);
        }
        let how = if poll {
            sys::Wait::PollJob
        } else {
            sys::Wait::Job
        };
        let status = sys::wait_child(self.child.id(), how)?;
        if let Some(status) = status {
            self.stopped = status.stopped_signal();
            if self.stopped.is_none() && !status.continued() {
                self.exited = Some(status);
                self.child.finish_drains()?;
            }
        }
        Ok(status)
    }

    pub fn resume(&mut self) -> io::Result<()> {
        if self.stopped.is_some() {
            sys::continue_job(self.child.id())?;
            self.stopped = None;
        }
        Ok(())
    }

    pub fn foreground(&mut self, resume: bool) -> io::Result<ExitStatus> {
        use std::os::unix::process::ExitStatusExt;
        let mut shell_modes = [0; sys::TERMIOS_LEN];
        sys::termios_get(0, &mut shell_modes).map_err(io::Error::other)?;
        if let Some(modes) = self.modes.as_ref().filter(|_| resume) {
            sys::termios_set(0, modes).map_err(io::Error::other)?;
        }
        if let Err(error) = sys::foreground(self.child.id()) {
            sys::termios_set(0, &shell_modes).map_err(io::Error::other)?;
            return Err(error);
        }
        let result = (|| {
            if resume {
                self.resume()?;
            }
            let mut status = self.observe(false)?.ok_or(io::ErrorKind::Other)?;
            // A newly spawned (or running background) reader may have reached
            // the tty just before handoff and already stopped for tty I/O.
            if matches!(status.stopped_signal(), Some(21 | 22)) {
                self.resume()?;
                status = self.observe(false)?.ok_or(io::ErrorKind::Other)?;
            }
            Ok(status)
        })();
        sys::foreground(std::process::id())?;
        if result
            .as_ref()
            .is_ok_and(|status| status.stopped_signal().is_some())
        {
            let mut modes = [0; sys::TERMIOS_LEN];
            sys::termios_get(0, &mut modes).map_err(io::Error::other)?;
            self.modes = Some(modes);
        }
        if !result.as_ref().is_ok_and(|status| status.code().is_some()) {
            sys::termios_set(0, &shell_modes).map_err(io::Error::other)?;
        }
        result
    }
}

pub(super) fn spawn(fds: &Fds, cmd: &mut Command) -> io::Result<Child> {
    let io = Io::prepare(fds, cmd)?;
    let mut drains = allocation::with_capacity(io.captures.len()).map_err(oom)?;
    let process = if io.mappings.is_empty() {
        let result = cmd.spawn();
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        Process::Native(result?)
    } else {
        // Drop the std Child and any OS handles in the table that created
        // them. Linux allows the interpreter thread to reap this exact PID.
        Process::Pid(io.private(|| cmd.spawn().map(|child| child.id()))?)
    };
    for capture in io.captures {
        // Capacity was reserved before spawning the child.
        allocation::push(&mut drains, capture.drain).map_err(oom)?;
    }
    Ok(Child { process, drains })
}

pub(super) fn replace(fds: &Fds, cmd: &mut Command) -> io::Error {
    let io = match Io::prepare(fds, cmd) {
        Ok(io) => io,
        Err(e) => return e,
    };
    if io.mappings.is_empty() {
        return cmd.exec();
    }
    match io.private(|| Err::<(), _>(cmd.exec())) {
        Err(e) => e,
        Ok(()) => io::ErrorKind::Other.into(),
    }
}
