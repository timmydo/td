//! What a child root started last wrote to stderr: a bounded tail root
//! reads without blocking and reports as one printable line in its own
//! log, never on the attention screen.

use std::io::{self, Read};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;

/// The bytes kept: enough for a last line, never a transcript.
pub(crate) const TAIL_BYTES: usize = 512;
/// What one drain reads at most, so a chatty child cannot hold root.
const READ_BUDGET: usize = 64 * 1024;
/// The drains `finish` makes at most: what an exited child left is finite,
/// unless something it started still holds the channel.
const FINISH_DRAINS: usize = 8;
/// What a Rust program prints last when it panics. The message above it
/// may quote the data it was handling, so a tail whose last line is this
/// says only that it panicked.
const PANIC_NOTE: &str = "note: run with `RUST_BACKTRACE=1`";
/// How often a wait looks again while it drains.
const WAIT_STEP: Duration = Duration::from_millis(10);

pub(crate) struct Tail {
    socket: UnixStream,
    bytes: Vec<u8>,
    eof: bool,
}

impl Tail {
    /// A tail and the child's end of it, to give the child as stderr.
    pub(crate) fn pair() -> Result<(Self, OwnedFd), String> {
        let (root, child) = UnixStream::pair().map_err(|_| "create diagnostic channel")?;
        root.set_nonblocking(true)
            .and_then(|()| root.shutdown(Shutdown::Write))
            .map_err(|_| "create diagnostic channel")?;
        let tail = Self {
            socket: root,
            bytes: Vec::with_capacity(TAIL_BYTES),
            eof: false,
        };
        Ok((tail, OwnedFd::from(child)))
    }

    /// Reads what waits, at most `READ_BUDGET`, keeping the last bytes.
    pub(crate) fn drain(&mut self) {
        let mut buffer = [0u8; 4096];
        let mut read = 0;
        while !self.eof && read < READ_BUDGET {
            match self.socket.read(&mut buffer) {
                Ok(0) => self.eof = true,
                Ok(count) => {
                    read += count;
                    push(&mut self.bytes, buffer.get(..count).unwrap_or_default());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                // An unreadable channel ends the reading, not the child.
                Err(_) => self.eof = true,
            }
        }
    }

    /// Drains what a child that has exited left, so its last line is its
    /// last and not one from the middle of a burst.
    pub(crate) fn finish(&mut self) {
        for _ in 0..FINISH_DRAINS {
            if self.eof {
                return;
            }
            self.drain();
        }
    }

    pub(crate) fn eof(&self) -> bool {
        self.eof
    }

    pub(crate) fn last_line(&self) -> Option<String> {
        last_line(&self.bytes)
    }

    /// What a failure message adds for the child: its last line quoted as
    /// its own words, or nothing when it said nothing.
    pub(crate) fn said(&mut self) -> String {
        self.finish();
        self.last_line()
            .map(|line| format!(", saying \"{line}\""))
            .unwrap_or_default()
    }

    /// Waits for `child` to exit, draining meanwhile so a child that writes
    /// more than the channel holds never stalls the wait.
    fn wait(&mut self, child: &mut Child) -> io::Result<ExitStatus> {
        loop {
            self.drain();
            if let Some(status) = child.try_wait()? {
                self.finish();
                return Ok(status);
            }
            std::thread::sleep(WAIT_STEP);
        }
    }
}

/// `Command::status` with a tail as the child's stderr, drained while root
/// waits for the exit it has no deadline on.
pub(crate) fn status(command: &mut Command) -> io::Result<(ExitStatus, Tail)> {
    let (mut tail, child_end) = Tail::pair().map_err(io::Error::other)?;
    let child = command.stderr(Stdio::from(child_end)).spawn();
    // The Command keeps what it was given; the null device in its place
    // leaves the child the tail's only writer, so its exit is EOF.
    command.stderr(Stdio::null());
    let mut child = child?;
    let status = tail.wait(&mut child)?;
    Ok((status, tail))
}

/// Appends `bytes`, keeping only the last `TAIL_BYTES`.
pub(crate) fn push(tail: &mut Vec<u8>, bytes: &[u8]) {
    tail.extend_from_slice(bytes);
    let excess = tail.len().saturating_sub(TAIL_BYTES);
    tail.drain(..excess);
}

/// The last nonempty line, every character but printable ASCII as `?`,
/// a quotation mark included so that it cannot end root's quotation of
/// it; for a panic, only that it panicked.
pub(crate) fn last_line(tail: &[u8]) -> Option<String> {
    line(tail, true)
}

/// `last_line` for text root shows unquoted, td-app's and td-software's
/// refusal reasons: a quotation mark is kept.
pub(crate) fn last_reason(tail: &[u8]) -> Option<String> {
    line(tail, false)
}

fn line(tail: &[u8], quoted: bool) -> Option<String> {
    let text = String::from_utf8_lossy(tail);
    let line = text.trim_end().rsplit('\n').next()?.trim();
    if line.is_empty() {
        return None;
    }
    if line.starts_with(PANIC_NOTE) {
        return Some("it panicked; the message is not kept".to_string());
    }
    Some(
        line.chars()
            .map(|c| match c {
                '"' if quoted => '?',
                ' '..='~' => c,
                _ => '?',
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::io::Write;
    use std::os::fd::AsFd;

    #[test]
    fn only_the_last_line_survives_and_is_printable() {
        let mut tail = Vec::new();
        push(&mut tail, b"first\nsecond\x1b[31m \xff\n\n");
        assert_eq!(last_line(&tail).as_deref(), Some("second?[31m ?"));
        push(&mut tail, &[b'x'; TAIL_BYTES + 7]);
        assert_eq!(tail.len(), TAIL_BYTES);
        assert_eq!(last_line(&tail).unwrap().len(), TAIL_BYTES);
        assert_eq!(last_line(b" \n\t\n"), None);
        assert_eq!(
            last_line(b"x\"; its vault service exited 0\n").as_deref(),
            Some("x?; its vault service exited 0")
        );
        assert_eq!(
            last_reason(b"x\"; its vault service exited 0\n").as_deref(),
            Some("x\"; its vault service exited 0")
        );
        assert_eq!(last_line(b""), None);
    }

    /// A panic's message may quote what the program held; it is not kept.
    #[test]
    fn a_panic_says_only_that_it_panicked() {
        let panic = b"thread 'main' panicked at src/app.rs:9:5:\n\
            byte index 3 is not a char boundary; it is inside 'secret-entry'\n\
            note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";
        assert_eq!(
            last_line(panic).as_deref(),
            Some("it panicked; the message is not kept")
        );
    }

    /// A child that writes more than the channel holds and then fails is
    /// waited for to its exit, its last line kept and quoted.
    #[test]
    fn status_drains_while_it_waits_and_quotes_the_last_line() {
        let (exit, mut tail) = status(&mut fixture("busy_helper")).unwrap();
        assert_eq!(exit.code(), Some(3));
        assert_eq!(tail.said(), ", saying \"umount: /x: busy\"");
        let (exit, mut tail) = status(&mut fixture("quiet_helper")).unwrap();
        assert!(exit.success());
        assert_eq!(tail.said(), "");
    }

    fn fixture(name: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", &format!("stderr_tail::tests::{name}")])
            .args(["--ignored", "--nocapture", "-q"])
            .stdout(Stdio::null());
        command
    }

    #[test]
    #[ignore = "child process: a helper that writes more than its channel holds"]
    fn busy_helper() {
        let mut stderr = std::io::stderr().lock();
        for _ in 0..4000 {
            writeln!(stderr, "padding line of some length here").unwrap();
        }
        writeln!(stderr, "umount: /x: busy").unwrap();
        std::process::exit(3);
    }

    #[test]
    #[ignore = "child process: a helper that succeeds and says nothing"]
    fn quiet_helper() {
        std::process::exit(0);
    }

    #[test]
    fn a_drain_reads_what_waits_without_blocking_and_sees_eof() {
        let (mut tail, child) = Tail::pair().unwrap();
        let mut writer = UnixStream::from(child.as_fd().try_clone_to_owned().unwrap());
        tail.drain();
        assert_eq!(tail.last_line(), None);
        assert!(!tail.eof);
        writer.write_all(b"td-pass: refused\n").unwrap();
        tail.drain();
        assert_eq!(tail.last_line().as_deref(), Some("td-pass: refused"));
        drop(writer);
        drop(child);
        // Another test's fork may hold a copy of the writer until it execs.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !tail.eof() {
            assert!(std::time::Instant::now() < deadline, "EOF never came");
            tail.finish();
        }
    }
}
