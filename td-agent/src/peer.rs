//! One end of the connection between a window and its broker (DESIGN.md §2):
//! `td_remote`'s link over a unix socket, whichever way that socket came, with
//! its sends and its receipts on threads of their own so that neither side's
//! loop ever waits on the other's, nor the link on either.

use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender as Queue, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use td_remote::{frame, Hello, Link, Options, Sender};

const STREAM: u32 = 1;

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
        version: 1,
    },
    streams: &[STREAM],
    max_message: MAX_MESSAGE,
    unread: UNREAD,
};

// `frame` carries a message of any size in chunks: nothing here needs its limit.
const _: () = assert!(MAX_MESSAGE > frame::MAX_FRAME);

/// What a message counts for against `BEHIND`.
fn weight(message: &[u8]) -> usize {
    message.len().min(LARGE)
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
    stream: UnixStream,
    _link: Link,
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
        let mut link = linked.map_err(|e| e.to_string())?;
        let inbox = link
            .take_inbox(STREAM)
            .ok_or("the connection has no stream")?;
        let (hear, heard) = mpsc::channel();
        let unread = Arc::new(AtomicUsize::new(0));
        let (held, ender) = (Arc::clone(&unread), clone("end")?);
        std::thread::Builder::new()
            .name("td-agent-peer-read".into())
            .spawn(move || loop {
                let message = match inbox.recv() {
                    Ok(message) => message,
                    Err(why) => {
                        let _ = hear.send(Err(why.to_string()));
                        return;
                    }
                };
                let total = held
                    .fetch_add(message.len(), Ordering::Relaxed)
                    .saturating_add(message.len());
                if total > READ_BEHIND {
                    let _ = hear.send(Err("this side has stopped reading".to_string()));
                    let _ = ender.shutdown(std::net::Shutdown::Both);
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
            _link: link,
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
    }
}

impl Drop for Peer {
    /// Ends the threads, which a far side that stopped reading would hold.
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
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
}
