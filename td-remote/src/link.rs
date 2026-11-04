//! One connection: a handshake, then messages on numbered streams, each
//! sent as chunks so that a large message on one stream does not hold up a
//! small one on another. The bytes may be an `ssh` child's standard streams
//! or anything else that reads and writes; a test uses a socket pair.

use std::collections::BTreeMap;
use std::io::{Chain, Cursor, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::frame;
use crate::Error;

/// What either side writes first, before any frame, so that a shell's banner
/// is told from the protocol and named rather than parsed as a length.
const MAGIC: &[u8] = b"td-remote 1\n";
/// The most of a banner an error quotes.
const SEEN_BYTES: usize = 256;
const PROGRAM_BYTES: usize = 64;
/// The bytes of one message in one frame: a stream's header fits beside it,
/// and the writer passes to another stream at least this often.
pub const CHUNK_BYTES: usize = 256 * 1024;
const CHUNK_HEAD: usize = 5;
const LAST: u8 = 1;
/// How long a closed link waits for ssh's last words.
const TAIL_WAIT: Duration = Duration::from_millis(500);
/// The most of what ssh said an error keeps.
const TAIL_BYTES: usize = 1024;

/// Who speaks: a program's protocol, and its version. Both sides must say
/// exactly the same.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hello {
    pub program: &'static str,
    pub version: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub hello: Hello,
    /// The streams either side may send on; any other is a protocol error.
    pub streams: &'static [u32],
    /// The largest whole message either side sends or accepts.
    pub max_message: usize,
    /// Whole messages a stream's consumer may leave unread, at least one. One
    /// more ends the link: the reader never waits on a consumer, because every
    /// stream, and the news that the link ended, share it. Request and reply,
    /// one at a time per stream, never hold more than one.
    pub unread: usize,
}

/// Fair turns at the writer. A plain lock would let a large message's sender
/// take it again and again ahead of a waiting small one.
struct Turns<W: ?Sized> {
    tickets: Mutex<(u64, u64)>,
    turn: Condvar,
    /// A write failed, perhaps partway through a frame: nothing may follow it.
    broken: AtomicBool,
    writer: Mutex<W>,
}

impl<W: Write + ?Sized> Turns<W> {
    fn broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }

    fn write(&self, frame: &[u8]) -> Result<(), Error> {
        let poisoned = || Error::Io("a writer failed".into());
        let ticket = {
            let mut tickets = self.tickets.lock().map_err(|_| poisoned())?;
            let ticket = tickets.0;
            tickets.0 = ticket.wrapping_add(1);
            while tickets.1 != ticket {
                tickets = self.turn.wait(tickets).map_err(|_| poisoned())?;
            }
            ticket
        };
        let result = if self.broken() {
            Err(Error::Closed(String::new()))
        } else {
            match self.writer.lock() {
                Ok(mut writer) => writer
                    .write_all(frame)
                    .and_then(|()| writer.flush())
                    .map_err(Error::from),
                Err(_) => Err(poisoned()),
            }
        };
        if result.is_err() {
            self.broken.store(true, Ordering::Relaxed);
        }
        let mut tickets = self.tickets.lock().map_err(|_| poisoned())?;
        tickets.1 = ticket.wrapping_add(1);
        self.turn.notify_all();
        result
    }
}

/// The sending half, shared by a program's threads. Streams overlap; two
/// messages on one stream go one after the other, since a stream's chunks are
/// told apart only by their order.
#[derive(Clone)]
pub struct Sender {
    turns: Arc<Turns<dyn Write + Send>>,
    sending: Arc<BTreeMap<u32, Mutex<()>>>,
    max_message: usize,
}

impl Sender {
    /// An earlier write failed, so nothing sent now would be written.
    pub fn broken(&self) -> bool {
        self.turns.broken()
    }

    /// Sends one whole message on `stream`, a chunk at a time.
    pub fn send(&self, stream: u32, payload: &[u8]) -> Result<(), Error> {
        if payload.len() > self.max_message {
            return Err(Error::TooLong(payload.len()));
        }
        let _ours = self
            .sending
            .get(&stream)
            .ok_or_else(|| Error::Protocol(format!("no stream {stream}")))?
            .lock()
            .map_err(|_| Error::Io("a writer failed".into()))?;
        let mut chunks = payload.chunks(CHUNK_BYTES).peekable();
        if chunks.peek().is_none() {
            return self.chunk(stream, &[], LAST);
        }
        while let Some(chunk) = chunks.next() {
            let flags = if chunks.peek().is_none() { LAST } else { 0 };
            self.chunk(stream, chunk, flags)?;
        }
        Ok(())
    }

    fn chunk(&self, stream: u32, chunk: &[u8], flags: u8) -> Result<(), Error> {
        let mut payload = Vec::with_capacity(chunk.len().saturating_add(CHUNK_HEAD));
        payload.extend_from_slice(&stream.to_be_bytes());
        payload.push(flags);
        payload.extend_from_slice(chunk);
        self.turns.write(&frame::encode(&payload)?)
    }
}

/// One stream's whole messages, in order, then why the link ended.
pub struct Inbox {
    messages: Receiver<Vec<u8>>,
    end: Arc<Mutex<Option<Error>>>,
}

impl Inbox {
    /// Why the link ended, once the reader has said.
    fn said(&self) -> Option<Error> {
        self.end.lock().ok().and_then(|end| end.clone())
    }

    /// The reader has gone, having said why first.
    fn reason(&self) -> Error {
        self.said().unwrap_or_else(|| Error::Closed(String::new()))
    }

    /// The next message, waiting for it.
    pub fn recv(&self) -> Result<Vec<u8>, Error> {
        self.messages.recv().map_err(|_| self.reason())
    }

    /// Whether this request-and-reply stream's far side has nothing more to
    /// say, with nothing asked of it: the link has ended, or a message came
    /// that was not asked for, which is a protocol error.
    pub fn ended(&self) -> Option<Error> {
        match self.messages.try_recv() {
            Ok(_) => Some(Error::Protocol("a reply nothing asked for".into())),
            Err(mpsc::TryRecvError::Empty) => self.said(),
            Err(mpsc::TryRecvError::Disconnected) => Some(self.reason()),
        }
    }

    /// Why the link ended, after a write to it failed: the reader's reason is
    /// what ssh said, where the write knows only of a broken pipe. Waits a
    /// little longer than the reader may for ssh's last words.
    pub fn end_after_failed_write(&self, failed: Error) -> Error {
        let deadline = Instant::now() + TAIL_WAIT + TAIL_WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(left) {
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => return failed,
                Err(mpsc::RecvTimeoutError::Disconnected) => return self.reason(),
            }
        }
    }
}

/// What a child wrote to its standard error, kept for a closing error.
struct Tail {
    bytes: Arc<Mutex<Vec<u8>>>,
    ended: Receiver<()>,
}

impl Tail {
    fn start(mut stream: impl Read + Send + 'static) -> Result<Self, Error> {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (done, ended) = mpsc::channel();
        let kept = Arc::clone(&bytes);
        std::thread::Builder::new()
            .name("td-remote-stderr".into())
            .spawn(move || {
                let mut buffer = [0u8; 4096];
                loop {
                    let n = match stream.read(&mut buffer) {
                        Ok(n) => n,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    };
                    let Some(read) = buffer.get(..n).filter(|read| !read.is_empty()) else {
                        break;
                    };
                    if let Ok(mut kept) = kept.lock() {
                        kept.extend_from_slice(read);
                        let over = kept.len().saturating_sub(TAIL_BYTES);
                        kept.drain(..over);
                    }
                }
                let _ = done.send(());
            })?;
        Ok(Self { bytes, ended })
    }

    /// What was said, once the stream ends or the wait does.
    fn said(&self) -> String {
        let _ = self.ended.recv_timeout(TAIL_WAIT);
        let bytes = self.bytes.lock().map(|b| b.clone()).unwrap_or_default();
        printable_bytes(&bytes)
    }
}

/// A character that moves or hides what is around it, without showing itself:
/// a control, or a format character such as a bidirectional override.
fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{ad}'
                | '\u{61c}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{e0000}'..='\u{e007f}'
        )
}

/// `text` as one line a person can read: another machine's words, so nothing
/// of theirs that moves or hides text reaches a window or a terminal.
pub fn printable(text: &str) -> String {
    text.split(['\r', '\n'])
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("; ")
        .chars()
        .map(|c| if invisible(c) { ' ' } else { c })
        .collect()
}

fn printable_bytes(bytes: &[u8]) -> String {
    printable(&String::from_utf8_lossy(bytes))
}

/// An owned connection. Dropping it ends any child it started.
pub struct Link {
    sender: Sender,
    inboxes: BTreeMap<u32, Inbox>,
    child: Option<Child>,
    /// The child's input when there is a child.
    input: Option<Arc<Mutex<Option<ChildStdin>>>>,
}

impl Drop for Link {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A child's standard input, which `Link::finish` can close from outside.
struct ChildInput(Arc<Mutex<Option<ChildStdin>>>);

impl Write for ChildInput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self.0.lock().as_deref_mut() {
            Ok(Some(input)) => input.write(bytes),
            _ => Err(std::io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.lock().as_deref_mut() {
            Ok(Some(input)) => input.flush(),
            _ => Ok(()),
        }
    }
}

impl Link {
    /// Ends the child's input, so that it carries what it was given and ends of
    /// its own, and waits up to `within` for that: dropping the link kills it,
    /// with what it had yet to pass on. A write under way, one the far side has
    /// stopped reading, is not waited for.
    pub fn finish(mut self, within: Duration) {
        let closed = self
            .input
            .as_ref()
            .and_then(|input| input.try_lock().ok())
            .map(|mut input| input.take())
            .is_some();
        if !closed {
            return;
        }
        let deadline = Instant::now() + within;
        if let Some(child) = self.child.as_mut() {
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                    _ => return,
                }
            }
        }
    }

    /// Starts `command` with its standard streams as the link: `ssh` in
    /// production, the program itself in a test.
    pub fn spawn(mut command: Command, options: Options) -> Result<Self, Error> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::Io(format!("cannot run {:?}: {e}", command.get_program())))?;
        let input = Arc::new(Mutex::new(None));
        let streams = child
            .stderr
            .take()
            .map(Tail::start)
            .transpose()
            .and_then(|tail| match (child.stdin.take(), child.stdout.take()) {
                (Some(opened), Some(output)) => {
                    if let Ok(mut held) = input.lock() {
                        *held = Some(opened);
                    }
                    Ok((output, tail))
                }
                _ => Err(Error::Io("the child's streams are missing".into())),
            });
        let writer = ChildInput(Arc::clone(&input));
        let started = streams.and_then(|(output, tail)| Self::start(output, writer, options, tail));
        match started {
            Ok(mut link) => {
                link.child = Some(child);
                link.input = Some(input);
                Ok(link)
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }

    /// Both ends of a connection someone else made: the remote program's own
    /// standard streams, or a test's socket pair.
    pub fn over(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        options: Options,
    ) -> Result<Self, Error> {
        Self::start(reader, writer, options, None)
    }

    fn start(
        reader: impl Read + Send + 'static,
        mut writer: impl Write + Send + 'static,
        options: Options,
        tail: Option<Tail>,
    ) -> Result<Self, Error> {
        // Each side writes before it reads, and both hellos fit in any pipe.
        write_hello(&mut writer, options.hello).or_else(|error| match error {
            // A child that died at once is explained by its reader's end.
            Error::Io(_) if tail.is_some() => Ok(()),
            error => Err(error),
        })?;
        let reader = match read_hello(reader, options.hello) {
            Ok(reader) => reader,
            Err(error) => {
                return Err(match (error, tail.as_ref()) {
                    (Error::Closed(_), Some(tail)) => Error::Closed(tail.said()),
                    (error, _) => error,
                })
            }
        };
        let end = Arc::new(Mutex::new(None));
        let mut senders = BTreeMap::new();
        let mut inboxes = BTreeMap::new();
        let mut sending = BTreeMap::new();
        for &stream in options.streams {
            let (sender, messages) = mpsc::sync_channel(options.unread.max(1));
            senders.insert(stream, sender);
            sending.insert(stream, Mutex::new(()));
            inboxes.insert(
                stream,
                Inbox {
                    messages,
                    end: Arc::clone(&end),
                },
            );
        }
        let max_message = options.max_message;
        std::thread::Builder::new()
            .name("td-remote-reader".into())
            .spawn(move || {
                let reason = pump(reader, &senders, max_message);
                let say = |reason: Error| {
                    if let Ok(mut end) = end.lock() {
                        *end = Some(reason);
                    }
                };
                // Said at once, so nothing more is sent into it; ssh's last
                // words may take a moment, and improve it.
                say(reason.clone());
                if let (Error::Closed(_), Some(tail)) = (&reason, tail.as_ref()) {
                    say(Error::Closed(tail.said()));
                }
                // Only now do the inboxes see their senders go.
                drop(senders);
            })?;
        let turns: Arc<Turns<dyn Write + Send>> = Arc::new(Turns {
            tickets: Mutex::new((0, 0)),
            turn: Condvar::new(),
            broken: AtomicBool::new(false),
            writer: Mutex::new(writer),
        });
        Ok(Self {
            sender: Sender {
                turns,
                sending: Arc::new(sending),
                max_message,
            },
            inboxes,
            child: None,
            input: None,
        })
    }

    pub fn sender(&self) -> Sender {
        self.sender.clone()
    }

    /// `stream`'s messages, for its one consumer.
    pub fn take_inbox(&mut self, stream: u32) -> Option<Inbox> {
        self.inboxes.remove(&stream)
    }
}

fn write_hello(writer: &mut impl Write, hello: Hello) -> Result<(), Error> {
    if hello.program.len() > PROGRAM_BYTES {
        return Err(Error::TooLong(hello.program.len()));
    }
    let mut payload = hello.version.to_be_bytes().to_vec();
    payload.extend_from_slice(hello.program.as_bytes());
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&frame::encode(&payload)?);
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

/// Reads the other side's hello, and hands back the reader, holding whatever
/// came with it. A read takes what has arrived, so what a shell printed is
/// quoted without waiting on a far side that then says nothing more.
fn read_hello<R: Read>(mut reader: R, expected: Hello) -> Result<Chain<Cursor<Vec<u8>>, R>, Error> {
    let mut seen = Vec::with_capacity(SEEN_BYTES);
    let mut buffer = [0u8; SEEN_BYTES];
    while seen.len() < MAGIC.len() {
        let n = match reader.read(&mut buffer) {
            // All of it so far has been the magic: the connection was cut.
            Ok(0) => return Err(Error::Closed(String::new())),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        seen.extend_from_slice(buffer.get(..n).unwrap_or_default());
        let begun = seen.get(..MAGIC.len()).unwrap_or(&seen);
        if !MAGIC.starts_with(begun) {
            seen.truncate(SEEN_BYTES);
            return Err(Error::Garbage(printable_bytes(&seen)));
        }
    }
    let rest = seen.split_off(MAGIC.len());
    let mut reader = Cursor::new(rest).chain(reader);
    let payload = frame::read(&mut reader)
        .map_err(|error| match error {
            // Cut off partway is a connection that ended, which ssh explains.
            frame::Error::Torn => Error::Closed(String::new()),
            frame::Error::Io(error) => Error::Io(error.to_string()),
            error => Error::Protocol(error.to_string()),
        })?
        .ok_or_else(|| Error::Closed(String::new()))?;
    let (version, program) = payload
        .split_first_chunk::<4>()
        .ok_or_else(|| Error::Protocol("a short hello".into()))?;
    let version = u32::from_be_bytes(*version);
    if program.len() > PROGRAM_BYTES {
        return Err(Error::Protocol("a hello's name is too long".into()));
    }
    if program != expected.program.as_bytes() {
        return Err(Error::Refused(format!(
            "the remote program speaks {:?}, not {:?}",
            printable_bytes(program),
            expected.program
        )));
    }
    if version != expected.version {
        return Err(Error::Refused(format!(
            "{} protocol {} here and {version} on the remote machine; \
             install the same td on both",
            expected.program, expected.version
        )));
    }
    Ok(reader)
}

/// Reads frames into whole messages until the connection ends, and says why it
/// did. It never waits on a consumer, so nothing a far side sends on one stream
/// stops another stream hearing its reply, or that the link has ended.
fn pump(
    mut reader: impl Read,
    senders: &BTreeMap<u32, SyncSender<Vec<u8>>>,
    max_message: usize,
) -> Error {
    let mut partial: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    loop {
        let payload = match frame::read(&mut reader) {
            Ok(Some(payload)) => payload,
            Ok(None) | Err(frame::Error::Torn) => return Error::Closed(String::new()),
            Err(frame::Error::Io(error)) => return Error::Io(error.to_string()),
            Err(error @ frame::Error::TooLong(_)) => return Error::Protocol(error.to_string()),
        };
        let Some((head, chunk)) = payload.split_first_chunk::<CHUNK_HEAD>() else {
            return Error::Protocol("a short chunk".into());
        };
        let [a, b, c, d, flags] = *head;
        let stream = u32::from_be_bytes([a, b, c, d]);
        let Some(sender) = senders.get(&stream) else {
            return Error::Protocol(format!("a chunk on stream {stream}"));
        };
        if flags & !LAST != 0 {
            return Error::Protocol("a chunk's flags".into());
        }
        if chunk.len() > CHUNK_BYTES || (chunk.is_empty() && flags & LAST == 0) {
            return Error::Protocol(format!("a chunk of {} bytes", chunk.len()));
        }
        let message = partial.entry(stream).or_default();
        if message.len().saturating_add(chunk.len()) > max_message {
            return Error::TooLong(message.len().saturating_add(chunk.len()));
        }
        message.extend_from_slice(chunk);
        if flags & LAST != 0 {
            let mut whole = std::mem::take(message);
            whole.shrink_to_fit();
            match sender.try_send(whole) {
                // A consumer that left has no more use for the stream; the
                // others still do.
                Ok(()) | Err(TrySendError::Disconnected(_)) => {}
                Err(TrySendError::Full(_)) => {
                    return Error::Protocol(format!(
                        "more messages on stream {stream} than were asked for"
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;
    use std::io::Cursor;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

    const HELLO: Hello = Hello {
        program: "td-test",
        version: 3,
    };
    const OPTIONS: Options = Options {
        hello: HELLO,
        streams: &[1, 2],
        max_message: 8 << 20,
        unread: 16,
    };

    /// Two linked ends, the handshake run with each on a thread of its own,
    /// and a handle that ends the connection: a link over a socket someone
    /// else made does not close it, since its reader still holds a copy.
    fn pair(
        left: Options,
        right: Options,
    ) -> (Result<Link, Error>, Result<Link, Error>, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        let end = a.try_clone().unwrap();
        let far = std::thread::spawn(move || {
            let reader = b.try_clone().unwrap();
            Link::over(reader, b, right)
        });
        let reader = a.try_clone().unwrap();
        let near = Link::over(reader, a, left);
        (near, far.join().unwrap(), end)
    }

    fn linked() -> (Link, Link, UnixStream) {
        let (near, far, end) = pair(OPTIONS, OPTIONS);
        (near.unwrap(), far.unwrap(), end)
    }

    #[test]
    fn messages_of_every_size_cross_on_their_own_streams_in_order() {
        let (mut near, mut far, _end) = linked();
        let (one, two) = (far.take_inbox(1).unwrap(), far.take_inbox(2).unwrap());
        let sender = near.sender();
        let large: Vec<u8> = (0..(3 * CHUNK_BYTES + 17)).map(|i| i as u8).collect();
        let exact = vec![9u8; CHUNK_BYTES];
        sender.send(1, b"").unwrap();
        sender.send(2, b"small").unwrap();
        sender.send(1, &large).unwrap();
        sender.send(1, &exact).unwrap();
        assert_eq!(one.recv().unwrap(), b"");
        assert_eq!(two.recv().unwrap(), b"small");
        assert_eq!(one.recv().unwrap(), large);
        assert_eq!(one.recv().unwrap(), exact);
        // And the other way.
        let back = near.take_inbox(1).unwrap();
        far.sender().send(1, b"reply").unwrap();
        assert_eq!(back.recv().unwrap(), b"reply");
    }

    /// A writer that waits for a permit before each write, recording each
    /// frame's stream, and says when it has begun waiting.
    struct Gated {
        streams: Arc<Mutex<Vec<u32>>>,
        permits: Receiver<()>,
        waiting: mpsc::Sender<()>,
    }

    impl Write for Gated {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let _ = self.waiting.send(());
            self.permits.recv().unwrap();
            // A frame: its length, then the stream.
            let stream = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
            self.streams.lock().unwrap().push(stream);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_small_message_goes_right_after_the_chunk_being_written() {
        let streams = Arc::new(Mutex::new(Vec::new()));
        let (permit, permits) = mpsc::channel();
        let (waiting, wait) = mpsc::channel();
        let turns = Arc::new(Turns {
            tickets: Mutex::new((0, 0)),
            turn: Condvar::new(),
            broken: AtomicBool::new(false),
            writer: Mutex::new(Gated {
                streams: Arc::clone(&streams),
                permits,
                waiting,
            }),
        });
        let frame = |stream: u32| {
            let mut frame = vec![0, 0, 0, 4];
            frame.extend_from_slice(&stream.to_be_bytes());
            frame
        };
        // A sender coming straight back for the writer, with nothing to build
        // between turns, which is what a plain lock lets win again and again.
        let (large, chunk) = (Arc::clone(&turns), frame(1));
        let big = std::thread::spawn(move || {
            for _ in 0..24 {
                large.write(&chunk).unwrap();
            }
        });
        // Its first chunk is at the writer, held there.
        wait.recv().unwrap();
        let (small, message) = (Arc::clone(&turns), frame(2));
        let other = std::thread::spawn(move || small.write(&message).unwrap());
        // The small one has its turn before that chunk is let go.
        let deadline = Instant::now() + Duration::from_secs(30);
        while turns.tickets.lock().unwrap().0 < 2 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        for _ in 0..25 {
            permit.send(()).unwrap();
        }
        other.join().unwrap();
        big.join().unwrap();
        let streams = streams.lock().unwrap();
        assert_eq!(streams.len(), 25);
        assert_eq!(streams.get(..3).unwrap(), [1, 2, 1], "{streams:?}");
    }

    #[test]
    fn a_message_past_the_bound_is_refused_before_it_is_sent_or_held() {
        let (near, _far, _end) = linked();
        assert_eq!(
            near.sender().send(1, &vec![0u8; (8 << 20) + 1]),
            Err(Error::TooLong((8 << 20) + 1))
        );
        // A sender whose bound is larger is stopped by the receiver's.
        let small = Options {
            max_message: 16,
            ..OPTIONS
        };
        let (a, b, _end) = pair(OPTIONS, small);
        let (a, mut b) = (a.unwrap(), b.unwrap());
        let inbox = b.take_inbox(1).unwrap();
        a.sender().send(1, &[0u8; 32]).unwrap();
        assert_eq!(inbox.recv(), Err(Error::TooLong(32)));
    }

    #[test]
    fn the_program_and_version_must_be_the_same_on_both_sides() {
        let other = Options {
            hello: Hello {
                program: "td-other",
                version: 3,
            },
            ..OPTIONS
        };
        let (near, far, _end) = pair(OPTIONS, other);
        assert!(matches!(near, Err(Error::Refused(m)) if m.contains("td-other")));
        assert!(matches!(far, Err(Error::Refused(_))));
        let newer = Options {
            hello: Hello {
                program: "td-test",
                version: 4,
            },
            ..OPTIONS
        };
        let (near, far, _end) = pair(OPTIONS, newer);
        assert!(
            matches!(near, Err(Error::Refused(m)) if m.contains("protocol 3") && m.contains('4'))
        );
        assert!(matches!(far, Err(Error::Refused(_))));
    }

    #[test]
    fn a_banner_before_the_protocol_is_named_not_parsed() {
        let reader = Cursor::new(b"Welcome to \x1b[1mhost\x1b[0m\nlast login\n".to_vec());
        match read_hello(reader, HELLO).err() {
            Some(Error::Garbage(seen)) => {
                assert!(seen.contains("Welcome"), "{seen}");
                assert!(!seen.contains('\x1b'), "no control character survives");
                assert!(!seen.contains('\n'));
            }
            other => panic!("{other:?}"),
        }
        // Cut off inside the magic is a connection cut, which ssh explains.
        let short = Cursor::new(b"td-rem".to_vec());
        assert!(matches!(
            read_hello(short, HELLO).err(),
            Some(Error::Closed(_))
        ));
        let empty = Cursor::new(Vec::new());
        assert!(matches!(
            read_hello(empty, HELLO).err(),
            Some(Error::Closed(_))
        ));
    }

    /// Hands out one prepared read at a time, then fails the test if read
    /// again: a far side that has said this much and now says nothing.
    struct Said(Vec<Vec<u8>>);

    impl Read for Said {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            assert!(!self.0.is_empty(), "read past what had arrived");
            let said = self.0.remove(0);
            buffer[..said.len()].copy_from_slice(&said);
            Ok(said.len())
        }
    }

    #[test]
    fn a_banner_is_quoted_without_waiting_for_a_far_side_that_then_says_nothing() {
        let said = Said(vec![b"td-".to_vec(), b"Welcome to host\n".to_vec()]);
        match read_hello(said, HELLO).err() {
            Some(Error::Garbage(seen)) => assert!(seen.contains("Welcome to host"), "{seen}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nothing_a_far_machine_says_moves_or_hides_the_text_around_it() {
        for hidden in [
            "\x1b",
            "\u{202e}",
            "\u{2066}",
            "\u{200b}",
            "\u{feff}",
            "\u{ad}",
            "\u{2028}",
            "\u{2029}",
            "\u{61c}",
            "\u{206a}",
            "\u{fff9}",
            "\u{e0041}",
        ] {
            let shown = printable(&format!("Permission{hidden}denied"));
            assert_eq!(shown, "Permission denied", "{hidden:?}");
        }
        assert_eq!(printable("a\r\n\nb"), "a; b");
        assert_eq!(printable("héllo wörld"), "héllo wörld");
    }

    #[test]
    fn a_hello_past_any_name_is_refused_and_not_quoted() {
        let mut payload = 3u32.to_be_bytes().to_vec();
        payload.extend_from_slice(&[b'x'; PROGRAM_BYTES + 1]);
        let mut wire = MAGIC.to_vec();
        wire.extend_from_slice(&frame::encode(&payload).unwrap());
        assert!(matches!(
            read_hello(Cursor::new(wire), HELLO).err(),
            Some(Error::Protocol(_))
        ));
    }

    #[test]
    fn what_arrived_with_a_hello_stays_for_the_frames_after_it() {
        let mut wire = Vec::new();
        write_hello(&mut wire, HELLO).unwrap();
        wire.extend_from_slice(b"next");
        let mut reader = read_hello(Cursor::new(wire), HELLO).ok().unwrap();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"next");
    }

    #[test]
    fn every_stream_hears_why_the_link_ended() {
        let (mut near, _far, end) = linked();
        let (one, two) = (near.take_inbox(1).unwrap(), near.take_inbox(2).unwrap());
        assert_eq!(one.ended(), None);
        end.shutdown(Shutdown::Both).unwrap();
        assert_eq!(one.recv(), Err(Error::Closed(String::new())));
        assert_eq!(two.recv(), Err(Error::Closed(String::new())));
    }

    /// A near link with these options, and a far side that has done the
    /// handshake and writes `frames` as they are.
    fn fed(options: Options, frames: &[(u32, u8, Vec<u8>)]) -> (Link, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        let mut writer = b.try_clone().unwrap();
        let reader = a.try_clone().unwrap();
        let mut wire = Vec::new();
        write_hello(&mut wire, options.hello).unwrap();
        for (stream, flags, chunk) in frames {
            let mut payload = stream.to_be_bytes().to_vec();
            payload.push(*flags);
            payload.extend_from_slice(chunk);
            wire.extend_from_slice(&frame::encode(&payload).unwrap());
        }
        // Past a socket's buffer, this waits on the reader it is feeding.
        std::thread::spawn(move || {
            let _ = writer.write_all(&wire);
        });
        (Link::over(reader, a, options).unwrap(), b)
    }

    #[test]
    fn a_chunk_on_an_unknown_stream_or_of_unknown_shape_ends_the_link() {
        for (stream, flags, chunk, want) in [
            (9u32, LAST, Vec::new(), "stream 9"),
            (1, 2, Vec::new(), "flags"),
            (1, LAST, vec![0; CHUNK_BYTES + 1], "a chunk of"),
            // Costs its receiver a turn of the loop and says nothing.
            (1, 0, Vec::new(), "a chunk of 0"),
        ] {
            let (mut near, _far) = fed(OPTIONS, &[(stream, flags, chunk)]);
            let inbox = near.take_inbox(1).unwrap();
            match inbox.recv() {
                Err(Error::Protocol(detail)) => assert!(detail.contains(want), "{detail}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn chunks_of_two_streams_between_each_other_make_two_whole_messages() {
        let (mut near, _far) = fed(
            OPTIONS,
            &[
                (1, 0, b"one ".to_vec()),
                (2, 0, b"two ".to_vec()),
                (1, LAST, b"done".to_vec()),
                (2, LAST, b"done".to_vec()),
            ],
        );
        assert_eq!(near.take_inbox(1).unwrap().recv().unwrap(), b"one done");
        assert_eq!(near.take_inbox(2).unwrap().recv().unwrap(), b"two done");
    }

    #[test]
    fn two_threads_sending_on_one_stream_send_two_whole_messages() {
        let (mut near, far, _end) = linked();
        let inbox = near.take_inbox(1).unwrap();
        let senders: Vec<_> = (*b"ab")
            .into_iter()
            .map(|byte| {
                let sender = far.sender();
                std::thread::spawn(move || {
                    sender.send(1, &vec![byte; 8 * CHUNK_BYTES]).unwrap();
                })
            })
            .collect();
        for _ in 0..2 {
            let message = inbox.recv().unwrap();
            assert_eq!(message.len(), 8 * CHUNK_BYTES);
            assert!(message.iter().all(|byte| *byte == message[0]));
        }
        for sender in senders {
            sender.join().unwrap();
        }
    }

    #[test]
    fn a_stream_this_side_does_not_speak_is_refused_with_the_link_still_good() {
        let (mut near, far, _end) = linked();
        assert!(matches!(
            far.sender().send(9, b"x"),
            Err(Error::Protocol(_))
        ));
        far.sender().send(1, b"fine").unwrap();
        assert_eq!(near.take_inbox(1).unwrap().recv().unwrap(), b"fine");
    }

    /// Whatever `run` does on a thread of its own must finish: a hang in it
    /// fails this test rather than the whole run.
    fn finishes<T: Send + 'static>(run: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, wait) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(run());
        });
        wait.recv_timeout(Duration::from_secs(30)).unwrap()
    }

    const ONE_UNREAD: Options = Options {
        unread: 1,
        ..OPTIONS
    };

    #[test]
    fn messages_nobody_reads_end_the_link_rather_than_stopping_its_other_streams() {
        let (mut near, _far) = fed(
            ONE_UNREAD,
            &[
                (2, LAST, b"unasked".to_vec()),
                (2, LAST, b"and more".to_vec()),
                (1, LAST, b"a reply".to_vec()),
            ],
        );
        let (one, two) = (near.take_inbox(1).unwrap(), near.take_inbox(2).unwrap());
        match finishes(move || one.recv()) {
            Err(Error::Protocol(detail)) => assert!(detail.contains("stream 2"), "{detail}"),
            other => panic!("{other:?}"),
        }
        // What had come is still delivered, ahead of the reason.
        assert_eq!(two.recv().unwrap(), b"unasked");
        assert!(matches!(two.recv(), Err(Error::Protocol(_))));
    }

    #[test]
    fn a_stream_with_a_message_unread_does_not_keep_the_end_from_another() {
        let (mut near, far) = fed(ONE_UNREAD, &[(1, LAST, b"unread".to_vec())]);
        let (one, two) = (near.take_inbox(1).unwrap(), near.take_inbox(2).unwrap());
        far.shutdown(Shutdown::Both).unwrap();
        assert!(matches!(
            finishes(move || two.recv()),
            Err(Error::Closed(_))
        ));
        assert_eq!(one.recv().unwrap(), b"unread");
        assert!(matches!(one.recv(), Err(Error::Closed(_))));
        // The reason is kept for whoever asks next.
        assert!(matches!(one.ended(), Some(Error::Closed(_))));
        assert!(matches!(one.recv(), Err(Error::Closed(_))));
    }

    /// Fails its writes once `left` runs out, partway through one.
    struct Breaking {
        left: usize,
        wrote: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for Breaking {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let n = bytes.len().min(self.left);
            if n == 0 {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.left -= n;
            self.wrote.lock().unwrap().extend_from_slice(&bytes[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn nothing_follows_a_frame_torn_by_a_failed_write() {
        let wrote = Arc::new(Mutex::new(Vec::new()));
        let turns = Turns {
            tickets: Mutex::new((0, 0)),
            turn: Condvar::new(),
            broken: AtomicBool::new(false),
            writer: Mutex::new(Breaking {
                left: 3,
                wrote: Arc::clone(&wrote),
            }),
        };
        assert!(turns.write(b"torn").is_err());
        turns.writer.lock().unwrap().left = 100;
        assert!(turns.write(b"after").is_err());
        assert_eq!(*wrote.lock().unwrap(), b"tor");
    }

    /// `sh`'s printf for a hello of `HELLO`, for a child that speaks.
    const SAY_HELLO: &str = r"printf 'td-remote 1\n\000\000\000\013\000\000\000\003td-test'";

    /// What a link's child was given reaches the far side when it is finished,
    /// where dropping the link would kill the child with it unsent.
    #[test]
    fn finishing_a_link_lets_its_child_carry_what_it_was_given() {
        let scratch = td_fs::TempDir::new("td-remote-finish").unwrap();
        let (out, ended) = (scratch.path().join("out"), scratch.path().join("ended"));
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!(
                "{SAY_HELLO}; sleep 0.2; cat > '{}'; echo ended > '{}'",
                out.display(),
                ended.display()
            ),
        ]);
        let link = Link::spawn(command, OPTIONS).unwrap();
        link.sender().send(1, b"the last words").unwrap();
        link.finish(Duration::from_secs(30));
        // It saw the end of its input, and ended of itself rather than be killed.
        assert!(ended.exists());
        let got = std::fs::read(&out).unwrap();
        assert!(got.windows(14).any(|w| w == b"the last words"), "{got:?}");
    }

    #[test]
    fn dropping_a_link_ends_and_reaps_its_child() {
        let scratch = td_fs::TempDir::new("td-remote-child").unwrap();
        let pid_file = scratch.path().join("pid");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!(
                "{SAY_HELLO}; echo $$ > '{}'; while read -r line; do :; done",
                pid_file.display()
            ),
        ]);
        let link = Link::spawn(command, OPTIONS).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let pid = loop {
            let said = std::fs::read_to_string(&pid_file).unwrap_or_default();
            if said.ends_with('\n') {
                break said.trim().to_string();
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        };
        let proc = std::path::PathBuf::from("/proc").join(pid);
        assert!(proc.exists());
        drop(link);
        // Reaped, so not even a zombie.
        assert!(!proc.exists());
    }

    #[test]
    fn an_established_link_that_ends_is_explained_by_what_ssh_said() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!("{SAY_HELLO}; echo 'Timeout, server host not responding.' >&2; exit 255"),
        ]);
        let mut link = Link::spawn(command, OPTIONS).unwrap();
        let inbox = link.take_inbox(1).unwrap();
        match finishes(move || inbox.recv()) {
            Err(Error::Closed(detail)) => assert!(detail.contains("Timeout"), "{detail}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_end_is_known_while_ssh_s_last_words_are_awaited() {
        // Something else holds its standard error open, so the wait runs out.
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!("{SAY_HELLO}; (while read -r line; do :; done) <&0 >&- & exit 0"),
        ]);
        let mut link = Link::spawn(command, OPTIONS).unwrap();
        let inbox = link.take_inbox(1).unwrap();
        finishes(move || {
            while inbox.ended().is_none() {
                std::thread::yield_now();
            }
            // Heard well before the reader gives up waiting on those words.
            std::thread::sleep(TAIL_WAIT / 10);
            assert!(matches!(
                inbox.messages.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
        });
    }

    #[test]
    fn a_failed_write_is_explained_by_the_readers_reason() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!("{SAY_HELLO}; echo 'Connection reset by peer' >&2; exit 255"),
        ]);
        let mut link = Link::spawn(command, OPTIONS).unwrap();
        let inbox = link.take_inbox(1).unwrap();
        let failed = Error::Io("Broken pipe".into());
        match finishes(move || inbox.end_after_failed_write(failed)) {
            Error::Closed(detail) => assert!(detail.contains("reset by peer"), "{detail}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_connection_cut_during_the_hello_is_explained_by_what_ssh_said() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            r"printf 'td-remote 1\n\000'; echo 'ssh: broken' >&2; exit 255",
        ]);
        match Link::spawn(command, OPTIONS) {
            Err(Error::Closed(detail)) => assert!(detail.contains("ssh: broken"), "{detail}"),
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("no hello was said"),
        }
    }

    #[test]
    fn what_a_child_said_is_kept_to_its_last_kib() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "i=0; while [ $i -lt 400 ]; do echo 'filling up the tail' >&2; i=$((i+1)); done; \
             echo 'the last word' >&2; exit 1",
        ]);
        match Link::spawn(command, OPTIONS) {
            Err(Error::Closed(detail)) => {
                assert!(detail.ends_with("the last word"), "{detail}");
                assert!(detail.len() <= TAIL_BYTES + 512, "{}", detail.len());
            }
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("no hello was said"),
        }
    }

    #[test]
    fn a_child_that_exits_is_explained_by_what_it_said() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo 'td-x: command not found' >&2; exit 127"]);
        match Link::spawn(command, OPTIONS) {
            Err(Error::Closed(detail)) => assert!(detail.contains("not found"), "{detail}"),
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("no hello was said"),
        }
    }

    #[test]
    fn a_program_that_cannot_run_is_named() {
        match Link::spawn(Command::new("/nonexistent/td-remote-test"), OPTIONS) {
            Err(Error::Io(detail)) => assert!(detail.contains("td-remote-test"), "{detail}"),
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("it ran"),
        }
    }
}
