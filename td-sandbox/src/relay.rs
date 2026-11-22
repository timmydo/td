use crate::wire;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use td_ui::pty::{ChildCommand, Pty, RelayTerminal};

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn err(e: impl ToString) -> io::Error {
    io::Error::other(e.to_string())
}
fn exit_code(status: std::process::ExitStatus) -> u8 {
    status
        .code()
        .and_then(|v| u8::try_from(v).ok())
        .unwrap_or_else(|| u8::try_from(128 + status.signal().unwrap_or(1)).unwrap_or(1))
}

/// Relays the human's terminal or pipes over `channel`, the intake's
/// connection once root answered `ok`, until the exit frame, a stop, or a
/// signal. A channel that ends before its first frame, which the inside
/// sends empty once its command runs, means the command never started;
/// one that ends later, that it ended without saying how. Root's log says
/// why either way.
pub fn outside(
    mut channel: UnixStream,
    marker: &File,
    terminal: bool,
    banner: Option<String>,
) -> io::Result<u8> {
    let mut terminal = if terminal {
        Some(RelayTerminal::enter(File::from(
            io::stdin().as_fd().try_clone_to_owned()?,
        ))?)
    } else {
        None
    };
    let mut reader = channel.try_clone()?;
    let (tx, rx) = mpsc::sync_channel(128);
    std::thread::spawn(move || loop {
        let frame = wire::read(&mut reader);
        let done = frame.is_err();
        if tx.send(frame).is_err() || done {
            break;
        }
    });
    let (input_tx, input_rx) = mpsc::sync_channel::<(u8, Vec<u8>)>(128);
    std::thread::spawn(move || {
        for (kind, bytes) in input_rx {
            if wire::send(&mut channel, kind, &bytes).is_err() {
                break;
            }
        }
    });
    if terminal.is_none() {
        let tx = input_tx.clone();
        std::thread::spawn(move || {
            let mut bytes = [0; wire::LIMIT];
            let mut input = io::stdin();
            loop {
                match input.read(&mut bytes) {
                    Ok(0) => {
                        let _ = tx.send((wire::EOF, Vec::new()));
                        break;
                    }
                    Ok(n) => {
                        if tx
                            .send((wire::INPUT, bytes.get(..n).unwrap_or_default().to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    let (output_tx, output_rx) = mpsc::sync_channel::<(u8, Vec<u8>)>(128);
    let (written_tx, written_rx) = mpsc::sync_channel(128);
    std::thread::spawn(move || {
        for (kind, bytes) in output_rx {
            let result = if kind == wire::OUTPUT {
                io::stdout()
                    .write_all(&bytes)
                    .and_then(|()| io::stdout().flush())
            } else {
                io::stderr()
                    .write_all(&bytes)
                    .and_then(|()| io::stderr().flush())
            };
            let failed = result.is_err();
            if written_tx.send(result).is_err() || failed {
                break;
            }
        }
    });
    let mut size = None;
    let mut bytes = [0; wire::LIMIT];
    let mut pending_input = None;
    let mut pending_output = banner.map(|line| (wire::ERROR, line.into_bytes()));
    let mut outstanding = 0usize;
    let mut exit = None;
    let mut started = false;
    loop {
        if marker.metadata()?.nlink() == 0 {
            return Ok(143);
        }
        for result in written_rx.try_iter() {
            result?;
            outstanding = outstanding.saturating_sub(1);
        }
        if let Some(code) = exit {
            if outstanding == 0 && pending_output.is_none() {
                return Ok(code);
            }
        }
        if let Some(terminal) = &mut terminal {
            if let Some(signal) = terminal.signal()? {
                return Ok(128 + signal);
            }
            if pending_input.is_none() && exit.is_none() {
                let now = terminal.window().map_err(err)?;
                if size != Some(now) {
                    let [a, b] = now.rows.to_le_bytes();
                    let [c, d] = now.columns.to_le_bytes();
                    pending_input = Some((wire::SIZE, vec![a, b, c, d]));
                    size = Some(now);
                } else {
                    match terminal.read(&mut bytes) {
                        Ok(0) => {}
                        Ok(n) => {
                            pending_input =
                                Some((wire::INPUT, bytes.get(..n).unwrap_or_default().to_vec()))
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        if let Some(frame) = pending_input.take() {
            match input_tx.try_send(frame) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(frame)) => pending_input = Some(frame),
                Err(mpsc::TrySendError::Disconnected(_)) => {}
            }
        }
        for _ in 0..128 {
            if let Some(frame) = pending_output.take() {
                match output_tx.try_send(frame) {
                    Ok(()) => outstanding += 1,
                    Err(mpsc::TrySendError::Full(frame)) => {
                        pending_output = Some(frame);
                        break;
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        return Err(err("host output relay failed"))
                    }
                }
            }
            if exit.is_some() {
                break;
            }
            let frame = rx.try_recv();
            // Any frame, the inside's empty first one included, says the
            // command started.
            started |= matches!(frame, Ok(Ok(_)));
            match frame {
                Ok(Ok((kind @ (wire::OUTPUT | wire::ERROR), bytes))) => {
                    pending_output = Some((kind, bytes))
                }
                Ok(Ok((wire::EXIT, bytes))) if bytes.len() == 1 => exit = bytes.first().copied(),
                Ok(Ok(_)) => return Err(err("unexpected jail relay frame")),
                Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(ended(started));
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Why a channel ended without its exit frame.
fn ended(started: bool) -> io::Error {
    let log = crate::intake::LOG;
    if started {
        crate::failure(format_args!(
            "the sandbox's relay ended without an exit status; {log} says why"
        ))
    } else {
        crate::failure(format_args!("the sandbox did not start; {log} says why"))
    }
}

pub fn is_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Output readers: a command's stdout and stderr, or one terminal.
#[derive(Clone, Copy, Debug)]
enum Reader {
    First,
    Second,
}

/// A reader's progress. `events` is bumped once when a read returns bytes
/// and again when their frame is delivered, so odd means delivering and any
/// change means progress; `delivered` counts the bytes delivered.
#[derive(Default)]
struct Activity {
    events: AtomicU64,
    delivered: AtomicU64,
}

/// The inside's half of the channel, shared by every output reader, with
/// each reader's activity.
struct Output<W> {
    to: Mutex<W>,
    first: Activity,
    second: Activity,
}

impl<W> Output<W> {
    fn new(to: W) -> Self {
        Output {
            to: Mutex::new(to),
            first: Activity::default(),
            second: Activity::default(),
        }
    }

    fn activity(&self, reader: Reader) -> &Activity {
        match reader {
            Reader::First => &self.first,
            Reader::Second => &self.second,
        }
    }
}

fn output<W: Write>(to: &Output<W>, kind: u8, bytes: &[u8]) -> io::Result<()> {
    wire::send(&mut *to.to.lock().map_err(err)?, kind, bytes)
}

/// How often the drain looks at a reader.
const DRAIN_POLL_MS: u64 = 5;
/// Looks, in all, at which a reader may be found idle, not delivering a
/// frame, once the command has exited, before the drain
/// stops waiting for it. Counted in looks rather than elapsed time, so a
/// stall of the whole process does not spend them. Some 200ms: what
/// separates a descendant holding a quiet descriptor from output still on
/// its way to a slow consumer.
const DRAIN_IDLE_POLLS: u64 = 40;
/// Bytes a reader may deliver once the command has exited: above any pipe
/// (`pipe-max-size`, 1 MiB by default) or terminal buffer the command's own
/// output can still sit in, so only a descendant still writing reaches it.
const DRAIN_BYTES: u64 = 4 << 20;

/// An output reader and which of the shared `Output`'s activities is its.
struct Pump {
    handle: std::thread::JoinHandle<io::Result<()>>,
    reader: Reader,
}

fn pump<W: Write + Send + 'static>(
    mut from: impl Read + Send + 'static,
    to: Arc<Output<W>>,
    kind: u8,
    reader: Reader,
) -> Pump {
    let handle = std::thread::spawn(move || {
        let progress = to.activity(reader);
        let mut bytes = [0; wire::LIMIT];
        loop {
            match from.read(&mut bytes) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    progress.events.fetch_add(1, Ordering::SeqCst);
                    output(&to, kind, bytes.get(..n).unwrap_or_default())?;
                    let n = u64::try_from(n).unwrap_or(u64::MAX);
                    progress.delivered.fetch_add(n, Ordering::SeqCst);
                    progress.events.fetch_add(1, Ordering::SeqCst);
                }
                Err(e) if e.raw_os_error() == Some(5) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    });
    Pump { handle, reader }
}

/// After the command exits: wait for each reader to reach the end of its
/// output, to be found idle `DRAIN_IDLE_POLLS` times in all, or to deliver
/// `DRAIN_BYTES`. A reader delivering a frame is never idle, however slowly
/// the host consumes it, and the command's own output fits in the byte
/// allowance, so none of it is dropped. A descendant holding a descriptor
/// open delays teardown by about 200ms of idleness if it is quiet or writes
/// to a consumer keeping up, and by `DRAIN_BYTES` of delivery if it floods
/// one that does not. A consumer that goes away
/// fails the delivery, which ends the reader.
fn drain<W>(to: &Output<W>, pumps: Vec<Pump>) -> io::Result<()> {
    for pump in pumps {
        let activity = to.activity(pump.reader);
        let start = activity.delivered.load(Ordering::SeqCst);
        let mut idle = 0;
        while !pump.handle.is_finished() {
            std::thread::sleep(Duration::from_millis(DRAIN_POLL_MS));
            // Not delivering at this look. Progress between looks does not
            // excuse it: a descendant writing more often than the drain
            // looks would otherwise never be found idle.
            if activity.events.load(Ordering::SeqCst) & 1 == 0 {
                idle += 1;
            }
            let delivered = activity.delivered.load(Ordering::SeqCst);
            if idle >= DRAIN_IDLE_POLLS || delivered.saturating_sub(start) > DRAIN_BYTES {
                break;
            }
        }
        if pump.handle.is_finished() {
            pump.handle
                .join()
                .map_err(|_| err("output relay failed"))??;
        }
    }
    Ok(())
}

pub fn inside(arguments: Vec<OsString>) -> io::Result<u8> {
    if std::env::current_exe()? != Path::new("/opt/workspace/bin/td-sandbox") {
        return Err(err("internal entry requires the workspace jail"));
    }
    let mut args = arguments.into_iter();
    let mode = args.next().ok_or_else(|| err("missing relay mode"))?;
    let terminal = match mode.to_str() {
        Some("pty") => true,
        Some("pipes") => false,
        _ => return Err(err("invalid relay mode")),
    };
    let term = args.next().ok_or_else(|| err("missing terminal type"))?;
    if !term.to_str().is_some_and(|s| {
        !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
    }) {
        return Err(err("invalid terminal type"));
    }
    let program = PathBuf::from(args.next().ok_or_else(|| err("missing command"))?);
    let args: Vec<_> = args.collect();
    let home = PathBuf::from(std::env::var_os("HOME").ok_or_else(|| err("missing private home"))?);
    let mut path = home.join(".local/bin").into_os_string();
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    let mut environment: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    environment.retain(|(k, _)| k != "PATH" && k != "TERM");
    environment.extend([("PATH".into(), path), ("TERM".into(), term)]);
    for ca in ["/etc/ssl/certs/ca-certificates.crt", "/etc/ssl/cert.pem"] {
        if Path::new(ca).is_file() {
            environment.extend([
                ("SSL_CERT_FILE".into(), ca.into()),
                ("CURL_CA_BUNDLE".into(), ca.into()),
            ]);
            break;
        }
    }
    // Root names td's shell for shell mode; a command discovers shells on
    // the admitted PATH as usual.
    if program
        .file_name()
        .is_some_and(|n| n == "bash" || n == "zsh" || n == "sh" || n == "td-sh")
    {
        environment.push(("SHELL".into(), program.clone().into_os_string()));
    }
    let output_channel = Arc::new(Output::new(io::stdout()));
    let pty = if terminal {
        let p = Pty::open().map_err(err)?;
        p.resize(24, 80).map_err(err)?;
        Some(p)
    } else {
        None
    };
    let mut child;
    let input: Option<Box<dyn Write + Send>>;
    let mut pumps = Vec::new();
    if let Some(pty) = &pty {
        child = Running(
            td_ui::pty::spawn(
                &ChildCommand {
                    program,
                    arguments: args,
                    leads_session: true,
                },
                &environment,
                &std::env::current_dir()?,
                pty.peer().map_err(err)?,
            )
            .map_err(err)?,
        );
        input = Some(Box::new(pty.master().try_clone()?));
        pumps.push(pump(
            pty.master().try_clone()?,
            output_channel.clone(),
            wire::OUTPUT,
            Reader::First,
        ));
    } else {
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .envs(environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        child = Running(command.spawn()?);
        input = child
            .0
            .stdin
            .take()
            .map(|v| Box::new(v) as Box<dyn Write + Send>);
        pumps.push(pump(
            child.0.stdout.take().ok_or_else(|| err("missing stdout"))?,
            output_channel.clone(),
            wire::OUTPUT,
            Reader::First,
        ));
        pumps.push(pump(
            child.0.stderr.take().ok_or_else(|| err("missing stderr"))?,
            output_channel.clone(),
            wire::ERROR,
            Reader::Second,
        ));
    }
    // An empty first frame: the command runs, so a channel that ends from
    // here on is a session that ended, not one that never started.
    output(&output_channel, wire::OUTPUT, &[])?;
    let (tx, rx) = mpsc::sync_channel(128);
    std::thread::spawn(move || {
        let mut input = io::stdin();
        loop {
            let frame = wire::read(&mut input);
            let done = frame.is_err();
            if tx.send(frame).is_err() || done {
                break;
            }
        }
    });
    let (input_tx, input_rx) = mpsc::sync_channel::<Option<Vec<u8>>>(128);
    std::thread::spawn(move || {
        let mut input = input;
        for bytes in input_rx {
            match bytes {
                Some(bytes) => {
                    if let Some(to) = &mut input {
                        if to.write_all(&bytes).is_err() {
                            input.take();
                        }
                    }
                }
                None => {
                    input.take();
                }
            }
        }
    });
    let mut pending_input = None;
    let code = loop {
        if let Some(status) = child.0.try_wait()? {
            break exit_code(status);
        }
        if let Some(bytes) = pending_input.take() {
            match input_tx.try_send(bytes) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(bytes)) => {
                    pending_input = Some(bytes);
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return Err(err("input relay failed")),
            }
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Ok((wire::INPUT, bytes))) => pending_input = Some(Some(bytes)),
            Ok(Ok((wire::SIZE, bytes))) => {
                let words: [u8; 4] = bytes.try_into().map_err(|_| err("invalid resize"))?;
                let [a, b, c, d] = words;
                if u16::from_le_bytes([a, b]) == 0 || u16::from_le_bytes([c, d]) == 0 {
                    return Err(err("empty terminal size"));
                }
                if let Some(pty) = &pty {
                    pty.resize(
                        usize::from(u16::from_le_bytes([a, b])),
                        usize::from(u16::from_le_bytes([c, d])),
                    )
                    .map_err(err)?;
                }
            }
            Ok(Ok((wire::EOF, bytes))) if bytes.is_empty() => pending_input = Some(None),
            Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(err("relay channel closed or invalid"))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    // Descendants may retain output descriptors; PID 1 tears down every
    // descendant once this entry returns.
    drain(&output_channel, pumps)?;
    output(&output_channel, wire::EXIT, &[code])?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    const DRAIN_IDLE: Duration = Duration::from_millis(DRAIN_POLL_MS * DRAIN_IDLE_POLLS);

    /// After `delay`, reads frames from `from` until the exit frame and
    /// returns the output bytes in order.
    fn late_consumer(mut from: UnixStream, delay: Duration) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let mut received = Vec::new();
            loop {
                let (kind, bytes) = wire::read(&mut from).unwrap();
                match kind {
                    wire::OUTPUT => received.extend_from_slice(&bytes),
                    wire::EXIT => return received,
                    other => panic!("unexpected frame {other}"),
                }
            }
        })
    }

    /// Fills `to`'s buffer with empty output frames, one whole frame per
    /// write, so the next delivery blocks until its reader reads.
    fn fill(to: &UnixStream) {
        to.set_nonblocking(true).unwrap();
        let frame = [wire::OUTPUT, 0, 0, 0, 0];
        loop {
            match (&*to).write(&frame) {
                Ok(written) => assert_eq!(written, frame.len(), "a frame split"),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        to.set_nonblocking(false).unwrap();
    }

    /// Runs the drain on its own thread: its result and how long it took,
    /// or a failure rather than a hang past `limit`.
    fn drain_within(
        channel: &Arc<Output<UnixStream>>,
        pumps: Vec<Pump>,
        limit: Duration,
    ) -> (io::Result<()>, Duration) {
        let channel = Arc::clone(channel);
        let (done, result) = mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            let drained = drain(&channel, pumps);
            let _ = done.send((drained, started.elapsed()));
        });
        result
            .recv_timeout(limit)
            .expect("the drain did not end in time")
    }

    /// A writer standing for a descendant that keeps its output open and
    /// writes `chunk` every `gap` until told to stop or its reader goes.
    fn descendant(
        mut writer: io::PipeWriter,
        chunk: usize,
        gap: Duration,
    ) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let bytes = vec![7u8; chunk];
            while !stopped.load(Ordering::SeqCst) && writer.write_all(&bytes).is_ok() {
                std::thread::sleep(gap);
            }
        });
        (stop, handle)
    }

    /// The command has written everything and exited while its output is
    /// still in the pipe, behind a consumer that does not read for longer
    /// than `DRAIN_IDLE`: every byte still arrives before the exit frame.
    #[test]
    fn a_slow_consumer_receives_every_byte_before_the_exit_frame() {
        let (reader, mut writer) = io::pipe().unwrap();
        let (inside, outside) = UnixStream::pair().unwrap();
        fill(&inside);
        let channel = Arc::new(Output::new(inside));
        let pumps = vec![pump(
            reader,
            Arc::clone(&channel),
            wire::OUTPUT,
            Reader::First,
        )];
        let data: Vec<u8> = (0u32..48 * 1024).map(|i| (i % 251) as u8).collect();
        let consumer = late_consumer(outside, 2 * DRAIN_IDLE);
        // The command: it writes everything and exits, its pipe closed.
        let written = data.clone();
        let command = std::thread::spawn(move || {
            writer.write_all(&written).unwrap();
        });
        // The reader cannot finish before the consumer starts reading, so a
        // drain that gave up at DRAIN_IDLE would return well before this.
        let (drained, waited) = drain_within(&channel, pumps, Duration::from_secs(10));
        drained.unwrap();
        assert!(waited > DRAIN_IDLE * 3 / 2, "{waited:?}");
        command.join().unwrap();
        output(&channel, wire::EXIT, &[0]).unwrap();
        assert!(consumer.join().unwrap() == data, "output lost or reordered");
    }

    /// The command printed and exited, and a descendant holds its output
    /// open with nothing more to say: the exit frame waits about
    /// `DRAIN_IDLE`, not until it closes.
    #[test]
    fn a_quiet_held_descriptor_does_not_hold_the_drain() {
        let (reader, mut writer) = io::pipe().unwrap();
        let (inside, outside) = UnixStream::pair().unwrap();
        let channel = Arc::new(Output::new(inside));
        let consumer = late_consumer(outside, Duration::ZERO);
        let pumps = vec![pump(
            reader,
            Arc::clone(&channel),
            wire::OUTPUT,
            Reader::Second,
        )];
        writer.write_all(b"printed").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while channel.second.events.load(Ordering::SeqCst) != 2 {
            assert!(Instant::now() < deadline, "the output was never delivered");
            std::thread::sleep(Duration::from_millis(1));
        }
        let (drained, waited) = drain_within(&channel, pumps, Duration::from_secs(5));
        drained.unwrap();
        assert!(waited >= DRAIN_IDLE, "{waited:?}");
        output(&channel, wire::EXIT, &[0]).unwrap();
        assert_eq!(consumer.join().unwrap(), b"printed");
        drop(writer);
    }

    /// Reads frames from `from` until the exit frame, pausing `pace` after
    /// each output frame, counting the output bytes rather than keeping
    /// them.
    fn counting_consumer(mut from: UnixStream, pace: Duration) -> std::thread::JoinHandle<usize> {
        std::thread::spawn(move || {
            let mut received = 0;
            loop {
                let (kind, bytes) = wire::read(&mut from).unwrap();
                match kind {
                    wire::OUTPUT => received += bytes.len(),
                    wire::EXIT => return received,
                    other => panic!("unexpected frame {other}"),
                }
                std::thread::sleep(pace);
            }
        })
    }

    /// A descendant still writing ends the drain: one writing small pieces,
    /// more often than the drain looks or less, after about `DRAIN_IDLE`;
    /// a flood to a consumer keeping up, as soon as the reader is caught
    /// between frames often enough; and a flood to one that is not, which
    /// keeps the reader delivering, only through `DRAIN_BYTES`.
    #[test]
    fn a_descendant_still_writing_does_not_hold_the_drain() {
        let millisecond = Duration::from_millis(1);
        for (chunk, gap, pace, limit) in [
            (40, millisecond, Duration::ZERO, Duration::from_secs(2)),
            (
                1,
                Duration::from_millis(3 * DRAIN_POLL_MS),
                Duration::ZERO,
                Duration::from_secs(2),
            ),
            (
                wire::LIMIT,
                Duration::ZERO,
                Duration::ZERO,
                Duration::from_secs(10),
            ),
            (
                wire::LIMIT,
                Duration::ZERO,
                millisecond,
                Duration::from_secs(5),
            ),
        ] {
            let (reader, writer) = io::pipe().unwrap();
            let (inside, outside) = UnixStream::pair().unwrap();
            let channel = Arc::new(Output::new(inside));
            let consumer = counting_consumer(outside, pace);
            let pumps = vec![pump(
                reader,
                Arc::clone(&channel),
                wire::OUTPUT,
                Reader::First,
            )];
            let (stop, writing) = descendant(writer, chunk, gap);
            let (drained, _) = drain_within(&channel, pumps, limit);
            drained.unwrap();
            stop.store(true, Ordering::SeqCst);
            output(&channel, wire::EXIT, &[0]).unwrap();
            consumer.join().unwrap();
            writing.join().unwrap();
        }
    }

    /// The byte allowance stays above the largest pipe an unprivileged
    /// command can make by default, `pipe-max-size`'s 1 MiB, and a frame.
    #[test]
    fn the_byte_allowance_outlasts_a_full_pipe() {
        assert!(DRAIN_BYTES >= (1 << 20) + wire::LIMIT as u64);
    }

    /// A consumer that goes away while a frame is being delivered ends the
    /// reader with that failure, and the drain reports it promptly.
    #[test]
    fn a_consumer_gone_mid_delivery_ends_the_drain() {
        let (reader, mut writer) = io::pipe().unwrap();
        let (inside, outside) = UnixStream::pair().unwrap();
        fill(&inside);
        let channel = Arc::new(Output::new(inside));
        let pumps = vec![pump(
            reader,
            Arc::clone(&channel),
            wire::OUTPUT,
            Reader::First,
        )];
        writer.write_all(b"undelivered").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while channel.first.events.load(Ordering::SeqCst) != 1 {
            assert!(
                Instant::now() < deadline,
                "the reader never began delivering"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(outside);
        let (drained, _) = drain_within(&channel, pumps, Duration::from_secs(5));
        assert!(drained.is_err());
        drop(writer);
    }
}
