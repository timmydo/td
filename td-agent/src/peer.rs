//! One end of the connection between a window and its broker (DESIGN.md §2):
//! `td_remote`'s link over a unix socket, whichever way that socket came, or over
//! ssh to a `td-agent attach` on another machine, with its sends and its receipts
//! on threads of their own so that neither side's loop ever waits on the other's,
//! nor the link on either.

use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender as Queue, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use td_remote::{frame, Error, Hello, Host, Link, Options, RemoteProgram, Sender};

const STREAM: u32 = 1;
/// The last thing said, which must pass whatever is queued on `STREAM`.
const BYE: u32 = 2;

/// A replay brings a whole log, at most `store::MAX_LOG`, and its pictures.
const MAX_MESSAGE: usize = 512 * 1024 * 1024;

/// Messages the link may hold for the thread that takes them at once.
const UNREAD: usize = 16 * 1024;

/// Bytes taken from the link and not yet read here. A long log's opening is a
/// message an event, more than `UNREAD` of them, so what a turn of the loop has
/// yet to read waits here; past this the loop has stopped, and the link ends.
const READ_BEHIND: usize = 1024 * 1024 * 1024;

/// What may wait to be written, behind the message being written. Past this the
/// far side has stopped reading: a window's connection is dropped, and it can
/// attach again and be told everything. It is more than a whole log, which an
/// opening brings as a message an event, far faster than ssh carries it: a link
/// that is only slow is not one to let go. A message larger than `LARGE`, a
/// replay, counts as `LARGE`, so that what is said behind it fits.
const BEHIND: usize = 512 * 1024 * 1024;
const LARGE: usize = BEHIND / 8;

/// How long a far side has to say hello: one that says nothing must not be
/// waited for, nor keep a broker from ending.
const HELLO: Duration = Duration::from_secs(30);

/// How long a window that is closing waits for what it said to go.
const DRAIN: Duration = Duration::from_secs(2);

pub const OPTIONS: Options = Options {
    hello: Hello {
        program: "td-agent-window",
        // Raised by hand with the forms of `window_wire`.
        version: 4,
    },
    streams: &[STREAM, BYE],
    max_message: MAX_MESSAGE,
    unread: UNREAD,
};

// `frame` carries a message of any size in chunks: nothing here needs its limit.
const _: () = assert!(MAX_MESSAGE > frame::MAX_FRAME);

/// What a message counts for against `BEHIND`.
fn weight(message: &[u8]) -> usize {
    message.len().min(LARGE)
}

/// Whether `error` needs something other than time: more tries at a login that
/// fails would count against the person's address at the far end.
fn lasting(error: &Error) -> bool {
    match error {
        Error::Refused(_) | Error::Garbage(_) => true,
        Error::Closed(said) => {
            said.contains("Permission denied")
                || said.contains("Too many authentication failures")
                || said.contains("Host key verification failed")
        }
        _ => false,
    }
}

/// A connection to the other half.
pub struct Peer {
    heard: Receiver<Result<Vec<u8>, String>>,
    /// The bytes in `heard`.
    unread: Arc<AtomicUsize>,
    out: Option<Queue<Vec<u8>>>,
    /// What `out` holds, by `weight`, and how much it may.
    waiting: Arc<AtomicUsize>,
    behind: usize,
    writer: Option<JoinHandle<()>>,
    /// The socket, when there is one: ssh's streams end with its process.
    stream: Option<UnixStream>,
    link: Option<Link>,
}

/// Why a connection over ssh could not be made.
#[derive(Debug)]
pub struct Unreached {
    pub why: String,
    /// Trying again unasked will not help: the far side answered and is not this
    /// program of this version, or printed something else first, or ssh will not
    /// log in without a person.
    pub lasting: bool,
}

impl Peer {
    /// Says hello over `stream`, and waits for the far side's.
    pub fn over(stream: UnixStream) -> Result<Self, String> {
        stream
            .set_nonblocking(false)
            .map_err(|e| format!("the connection: {e}"))?;
        let clone = |what: &str| {
            stream
                .try_clone()
                .map_err(|e| format!("the connection's {what}: {e}"))
        };
        let (reader, writer) = (clone("reader")?, clone("writer")?);
        let (greeted, waiting_for_it) = mpsc::channel::<()>();
        let watch = clone("watch")?;
        std::thread::Builder::new()
            .name("td-agent-hello-watch".into())
            .spawn(move || {
                if waiting_for_it
                    .recv_timeout(HELLO)
                    .is_err_and(|e| e == RecvTimeoutError::Timeout)
                {
                    let _ = watch.shutdown(std::net::Shutdown::Both);
                }
            })
            .map_err(|e| format!("the connection's thread: {e}"))?;
        let linked = Link::over(reader, writer, OPTIONS);
        drop(greeted);
        let link = linked.map_err(|e| e.to_string())?;
        let ender = clone("end")?;
        Self::started(link, Some(stream), Some(ender))
    }

    /// Runs `td-agent attach` on `host` over ssh. When a person `asked` for this
    /// it takes as long as they take to answer ssh's questions; when none did, ssh
    /// asks nothing.
    pub fn ssh(host: &Host, program: &RemoteProgram, asked: bool) -> Result<Self, Unreached> {
        let unreached = |error: Error| Unreached {
            lasting: lasting(&error),
            why: error.to_string(),
        };
        let linked = if asked {
            Link::ssh(host, program, &["attach"], OPTIONS)
        } else {
            Link::ssh_unattended(host, program, &["attach"], OPTIONS)
        };
        let link = linked.map_err(unreached)?;
        Self::started(link, None, None).map_err(|why| Unreached {
            why,
            lasting: false,
        })
    }

    /// Serves a link that has said hello. `ender` shuts it when this side has
    /// stopped reading; without one that is for `recv`'s caller, dropping this.
    fn started(
        mut link: Link,
        stream: Option<UnixStream>,
        ender: Option<UnixStream>,
    ) -> Result<Self, String> {
        let inbox = link
            .take_inbox(STREAM)
            .ok_or("the connection has no stream")?;
        let (hear, heard) = mpsc::channel();
        let unread = Arc::new(AtomicUsize::new(0));
        let bye = link.take_inbox(BYE).ok_or("the connection has no stream")?;
        let (held, heard_bye, hear_bye) = (Arc::clone(&unread), Arc::clone(&unread), hear.clone());
        let bye = std::thread::Builder::new()
            .name("td-agent-peer-bye".into())
            .spawn(move || {
                while let Ok(message) = bye.recv() {
                    heard_bye.fetch_add(message.len(), Ordering::Relaxed);
                    let _ = hear_bye.send(Ok(message));
                }
            })
            .map_err(|e| format!("the connection's thread: {e}"))?;
        std::thread::Builder::new()
            .name("td-agent-peer-read".into())
            .spawn(move || loop {
                let message = match inbox.recv() {
                    Ok(message) => message,
                    Err(why) => {
                        // Its end comes after whatever was said on the other.
                        let _ = bye.join();
                        let _ = hear.send(Err(why.to_string()));
                        return;
                    }
                };
                let total = held
                    .fetch_add(message.len(), Ordering::Relaxed)
                    .saturating_add(message.len());
                if total > READ_BEHIND {
                    let _ = hear.send(Err("this side has stopped reading".to_string()));
                    if let Some(ender) = &ender {
                        let _ = ender.shutdown(std::net::Shutdown::Both);
                    }
                    return;
                }
                if hear.send(Ok(message)).is_err() {
                    return;
                }
            })
            .map_err(|e| format!("the connection's thread: {e}"))?;
        let (out, queued) = mpsc::channel::<Vec<u8>>();
        let waiting = Arc::new(AtomicUsize::new(0));
        let written = Arc::clone(&waiting);
        let sender: Sender = link.sender();
        let writer = std::thread::Builder::new()
            .name("td-agent-peer".into())
            .spawn(move || {
                while let Ok(message) = queued.recv() {
                    // Being written, it no longer waits behind anything.
                    written.fetch_sub(weight(&message), Ordering::Relaxed);
                    if sender.send(STREAM, &message).is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| format!("the connection's thread: {e}"))?;
        Ok(Self {
            heard,
            unread,
            out: Some(out),
            waiting,
            behind: BEHIND,
            writer: Some(writer),
            stream,
            link: Some(link),
        })
    }

    /// Queues `message`, false when the far side has gone or is not keeping up.
    pub fn send(&self, message: Vec<u8>) -> bool {
        let Some(out) = &self.out else {
            return false;
        };
        let weighs = weight(&message);
        let waiting = self.waiting.load(Ordering::Relaxed);
        if waiting.saturating_add(weighs) > self.behind {
            return false;
        }
        self.waiting.fetch_add(weighs, Ordering::Relaxed);
        out.send(message).is_ok()
    }

    /// The next message when one has come, or why the far side has gone, which
    /// comes after everything it said.
    pub fn recv(&self) -> Result<Option<Vec<u8>>, String> {
        match self.heard.try_recv() {
            Ok(Ok(message)) => {
                self.unread.fetch_sub(message.len(), Ordering::Relaxed);
                Ok(Some(message))
            }
            Ok(Err(why)) => Err(why),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err("the connection has ended".to_string()),
        }
    }

    /// Says `message` ahead of whatever is queued and closes, in a moment if the
    /// far side has stopped reading.
    pub fn goodbye(self, message: Vec<u8>) {
        let Some(sender) = self.link.as_ref().map(Link::sender) else {
            return;
        };
        let deadline = Instant::now() + DRAIN;
        let sending = std::thread::Builder::new()
            .name("td-agent-peer-bye".into())
            .spawn(move || {
                let _ = sender.send(BYE, &message);
            });
        if let Ok(sending) = sending {
            while !sending.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// Lets what was queued go, for a moment, before the connection closes.
    pub fn finish(mut self) {
        self.out = None;
        let deadline = Instant::now() + DRAIN;
        if let Some(writer) = self.writer.take() {
            while !writer.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            if writer.is_finished() {
                let _ = writer.join();
            }
        }
        // ssh's process has yet to carry it, and dropping the link would kill it.
        if let Some(link) = self.link.take() {
            link.finish(deadline.saturating_duration_since(Instant::now()));
        }
    }
}

impl Drop for Peer {
    /// Ends the threads, which a far side that stopped reading would hold.
    fn drop(&mut self) {
        if let Some(stream) = &self.stream {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn pair() -> (Peer, Peer) {
        let (a, b) = UnixStream::pair().unwrap();
        let far = std::thread::spawn(move || Peer::over(b).unwrap());
        let near = Peer::over(a).unwrap();
        (near, far.join().unwrap())
    }

    fn wait(peer: &Peer) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "nothing came");
            if let Some(message) = peer.recv()? {
                return Ok(message);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn messages_of_any_size_cross_in_order_and_each_way() {
        let (near, far) = pair();
        let large = vec![7u8; 3 * frame::MAX_FRAME];
        assert!(near.send(b"first".to_vec()));
        assert!(near.send(large.clone()));
        assert!(far.send(b"back".to_vec()));
        assert_eq!(wait(&far).unwrap(), b"first");
        assert_eq!(wait(&far).unwrap(), large);
        assert_eq!(wait(&near).unwrap(), b"back");
        assert_eq!(near.recv().unwrap(), None);
    }

    #[test]
    fn what_was_said_goes_before_a_closing_side_does_and_its_going_is_heard() {
        let (near, far) = pair();
        assert!(near.send(b"goodbye".to_vec()));
        near.finish();
        assert_eq!(wait(&far).unwrap(), b"goodbye");
        assert!(wait(&far).is_err());
    }

    /// A far side that has said hello and reads no more never stops this one,
    /// and what waits for it stays within the bound.
    #[test]
    fn a_side_that_stops_reading_fills_the_queue_rather_than_holding_the_sender() {
        let (a, far) = UnixStream::pair().unwrap();
        let mut hello = OPTIONS.hello.version.to_be_bytes().to_vec();
        hello.extend_from_slice(OPTIONS.hello.program.as_bytes());
        let mut wire = b"td-remote 1\n".to_vec();
        wire.extend_from_slice(&frame::encode(&hello).unwrap());
        std::io::Write::write_all(&mut &far, &wire).unwrap();
        let mut near = Peer::over(a).unwrap();
        near.behind = 16 * frame::MAX_FRAME;
        let message = vec![1u8; frame::MAX_FRAME];
        let mut sent = 0;
        while near.send(message.clone()) {
            sent += 1;
            assert!(sent <= 64, "the queue never filled");
        }
        assert!(near.waiting.load(Ordering::Relaxed) <= near.behind);
        drop(far);
    }

    /// A long log's opening is more messages than the link holds unread; a loop
    /// that reads them each turn gets all of them.
    #[test]
    fn more_messages_than_the_link_holds_unread_all_arrive() {
        let (near, far) = pair();
        let count = 3 * UNREAD;
        for number in 0..count {
            assert!(near.send(number.to_string().into_bytes()));
        }
        // Nothing is read until they have all reached the far side's thread.
        let deadline = Instant::now() + Duration::from_secs(60);
        while far.unread.load(Ordering::Relaxed) < count.to_string().len() * (count / 2) {
            assert!(Instant::now() < deadline, "they never came");
            std::thread::sleep(Duration::from_millis(5));
        }
        for number in 0..count {
            assert_eq!(wait(&far).unwrap(), number.to_string().into_bytes());
        }
    }

    /// A message larger than the bound, a replay, goes, with what is said behind
    /// it: neither is taken for falling behind.
    #[test]
    fn a_message_past_the_bound_goes_when_nothing_else_waits() {
        let (mut near, far) = pair();
        near.behind = 2 * LARGE;
        let large = vec![2u8; 2 * LARGE + 1];
        assert!(near.send(b"before".to_vec()));
        assert!(near.send(large.clone()));
        assert!(near.send(vec![3u8; 16]));
        assert_eq!(wait(&far).unwrap(), b"before");
        assert_eq!(wait(&far).unwrap(), large);
        assert_eq!(wait(&far).unwrap(), [3u8; 16]);
    }

    #[test]
    fn another_program_is_refused_at_the_hello() {
        let (a, b) = UnixStream::pair().unwrap();
        let other = Options {
            hello: Hello {
                program: "td-editor-files",
                version: 1,
            },
            ..OPTIONS
        };
        let (reader, writer) = (b.try_clone().unwrap(), b);
        let far = std::thread::spawn(move || Link::over(reader, writer, other).map(|_| ()));
        assert!(Peer::over(a).is_err());
        assert!(far.join().unwrap().is_err());
    }

    /// A window closing as it sends over ssh: what it sent gets there, where
    /// dropping the link would kill ssh with it unsent.
    #[test]
    fn what_was_sent_over_a_child_goes_before_it_is_finished() {
        let scratch = td_fs::TempDir::new("td-agent-peer").unwrap();
        let (hello, out) = (scratch.path().join("hello"), scratch.path().join("out"));
        let mut said = b"td-remote 1\n".to_vec();
        let mut greeting = OPTIONS.hello.version.to_be_bytes().to_vec();
        greeting.extend_from_slice(OPTIONS.hello.program.as_bytes());
        said.extend_from_slice(&frame::encode(&greeting).unwrap());
        std::fs::write(&hello, said).unwrap();
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            &format!(
                "cat '{}'; sleep 0.2; cat > '{}'",
                hello.display(),
                out.display()
            ),
        ]);
        let link = Link::spawn(command, OPTIONS).unwrap();
        let peer = Peer::started(link, None, None).unwrap();
        assert!(peer.send(b"the last words".to_vec()));
        peer.finish();
        let got = std::fs::read(&out).unwrap();
        assert!(got.windows(14).any(|w| w == b"the last words"), "{got:?}");
    }

    #[test]
    fn only_a_login_or_a_program_that_will_not_do_stops_the_tries() {
        for lasting_one in [
            Error::Refused("another version".into()),
            Error::Garbage("a banner".into()),
            Error::Closed("me@box: Permission denied (publickey).".into()),
            Error::Closed("Host key verification failed.".into()),
            Error::Closed("Received disconnect: Too many authentication failures".into()),
        ] {
            assert!(lasting(&lasting_one), "{lasting_one:?}");
        }
        for passing in [
            Error::Closed("ssh: connect to host box port 22: No route to host".into()),
            Error::Closed(String::new()),
            Error::Io("a pipe".into()),
        ] {
            assert!(!lasting(&passing), "{passing:?}");
        }
    }

    /// A message crawls over a slow link and the goodbye passes it, which
    /// a window needs to tell being let go from a link that failed.
    #[test]
    fn a_goodbye_passes_what_is_queued_on_a_slow_link() {
        let (near, relay_near) = UnixStream::pair().unwrap();
        let (far, relay_far) = UnixStream::pair().unwrap();
        let back = (
            relay_far.try_clone().unwrap(),
            relay_near.try_clone().unwrap(),
        );
        std::thread::spawn(move || {
            let (mut from, mut to) = back;
            let _ = std::io::copy(&mut from, &mut to);
        });
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut from, mut to) = (relay_near, relay_far);
            let mut piece = vec![0u8; 256 * 1024];
            while let Ok(read) = from.read(&mut piece) {
                if read == 0 || to.write_all(piece.get(..read).unwrap()).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = to.shutdown(std::net::Shutdown::Write);
        });
        let far = std::thread::spawn(move || Peer::over(far).unwrap());
        let near = Peer::over(near).unwrap();
        let far = far.join().unwrap();
        // One message longer than a goodbye gets to wait, a replay say, under way.
        assert!(near.send(vec![1u8; 64 * 1024 * 1024]));
        std::thread::sleep(Duration::from_millis(100));
        near.goodbye(b"bye".to_vec());
        let mut said = false;
        while let Ok(message) = wait(&far) {
            said |= message == b"bye";
        }
        assert!(said, "only the link's end came");
    }
}
