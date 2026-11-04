//! A connection that comes back: the next request after a link is lost makes
//! a new one, counted by generation. A program whose far side keeps state
//! compares generations, restores that state, and only then asks again.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::link::{Inbox, Sender};
use crate::{Error, Link};

/// Makes a new link: an `ssh` child in production. It may wait on a person.
pub type Connector = Box<dyn FnMut() -> Result<Link, Error> + Send>;

/// Why a request did not get its reply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestError {
    /// Nothing was sent: the far side has heard nothing of this request.
    Unsent(Error),
    /// The connection is not the generation the caller prepared for, and
    /// nothing was sent.
    Stale,
    /// The connection ended with the request on its way: the far side may
    /// have carried it out.
    Lost(Error),
}

struct Active {
    generation: u64,
    sender: Sender,
    inboxes: BTreeMap<u32, Mutex<Inbox>>,
    // Held for its drop, which ends the connection.
    _link: Mutex<Link>,
}

/// One remote program, shared by a window's threads. A stream takes one
/// request at a time; different streams overlap.
pub struct Client {
    streams: Vec<u32>,
    connect: Mutex<Connecting>,
    current: Mutex<Option<Arc<Active>>>,
    /// Connections tried, so one that fails is the answer to everyone who was
    /// waiting on it, and asks a person nothing twice.
    attempts: AtomicU64,
    made: AtomicU64,
}

struct Connecting {
    connector: Connector,
    failed: Option<Error>,
}

impl Client {
    pub fn new(streams: &[u32], connector: Connector) -> Self {
        Self {
            streams: streams.to_vec(),
            connect: Mutex::new(Connecting {
                connector,
                failed: None,
            }),
            current: Mutex::new(None),
            attempts: AtomicU64::new(0),
            made: AtomicU64::new(0),
        }
    }

    fn active(&self) -> Option<Arc<Active>> {
        self.current.lock().ok()?.clone()
    }

    /// Forgets `active`, unless a newer connection has already replaced it.
    fn lost(&self, active: &Arc<Active>) {
        if let Ok(mut current) = self.current.lock() {
            if current.as_ref().is_some_and(|c| Arc::ptr_eq(c, active)) {
                *current = None;
            }
        }
    }

    /// Gives up the connection of `generation`, whose far side can no longer
    /// be believed to be where the caller thinks: the next request makes another.
    pub fn abandon(&self, generation: u64) {
        if let Some(active) = self.active().filter(|a| a.generation == generation) {
            self.lost(&active);
        }
    }

    /// The generation of a live connection, making one when there is none.
    /// A connection `stream`'s reader has seen end counts as none, so such a
    /// link costs no request an unconfirmed outcome. One that broke the
    /// protocol is reported, once, instead of being replaced in silence.
    pub fn connected(&self, stream: u32) -> Result<u64, Error> {
        let attempts = self.attempts.load(Ordering::Acquire);
        if let Some(active) = self.active() {
            let inbox = active
                .inboxes
                .get(&stream)
                .ok_or_else(|| Error::Protocol(format!("no stream {stream}")))?;
            // Busy means another of this stream's requests is in flight, and
            // it will find out for itself.
            let ended = inbox.try_lock().ok().and_then(|inbox| inbox.ended());
            match ended {
                None => return Ok(active.generation),
                Some(Error::Closed(_) | Error::Io(_)) => self.lost(&active),
                // A far side that broke the protocol is said once, not papered
                // over by the next connection.
                Some(error) => {
                    self.lost(&active);
                    return Err(error);
                }
            }
        }
        self.connect_after(attempts)
    }

    /// Makes a connection, unless one attempted since `attempts` answers.
    fn connect_after(&self, attempts: u64) -> Result<u64, Error> {
        // Only one thread connects; another's wait ends with its outcome.
        let mut connecting = self
            .connect
            .lock()
            .map_err(|_| Error::Io("a connector failed".into()))?;
        if let Some(active) = self.active() {
            return Ok(active.generation);
        }
        if self.attempts.load(Ordering::Acquire) != attempts {
            if let Some(failed) = &connecting.failed {
                return Err(failed.clone());
            }
        }
        let made = (connecting.connector)();
        self.attempts.fetch_add(1, Ordering::Release);
        connecting.failed = made.as_ref().err().cloned();
        let mut link = made?;
        let mut inboxes = BTreeMap::new();
        for &stream in &self.streams {
            let inbox = link
                .take_inbox(stream)
                .ok_or_else(|| Error::Protocol(format!("no stream {stream}")))?;
            inboxes.insert(stream, Mutex::new(inbox));
        }
        let generation = self.made.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let active = Arc::new(Active {
            generation,
            sender: link.sender(),
            inboxes,
            _link: Mutex::new(link),
        });
        *self
            .current
            .lock()
            .map_err(|_| Error::Io("a connection failed".into()))? = Some(active);
        Ok(generation)
    }

    /// Asks over the connection of `generation` and waits for the reply.
    pub fn request_on(
        &self,
        generation: u64,
        stream: u32,
        payload: &[u8],
    ) -> Result<Vec<u8>, RequestError> {
        let active = self
            .active()
            .filter(|active| active.generation == generation)
            .ok_or(RequestError::Stale)?;
        let inbox = active
            .inboxes
            .get(&stream)
            .ok_or_else(|| RequestError::Unsent(Error::Protocol(format!("no stream {stream}"))))?
            .lock()
            .map_err(|_| RequestError::Unsent(Error::Io("a request failed".into())))?;
        // The wait for this stream may have outlasted the connection.
        if !self.active().is_some_and(|c| Arc::ptr_eq(&c, &active)) {
            return Err(RequestError::Stale);
        }
        // A link the reader already knows has ended hears nothing of this, and
        // whatever is queued now would be taken for this request's reply.
        if let Some(error) = inbox.ended() {
            self.lost(&active);
            return Err(RequestError::Unsent(error));
        }
        // A write already failed: this one would not happen.
        if active.sender.broken() {
            let reason = inbox.end_after_failed_write(Error::Closed(String::new()));
            self.lost(&active);
            return Err(RequestError::Unsent(reason));
        }
        // A message past the bound is refused whole, with the link still good.
        if let Err(error) = active.sender.send(stream, payload) {
            if matches!(error, Error::TooLong(_)) {
                return Err(RequestError::Unsent(error));
            }
            let reason = inbox.end_after_failed_write(error);
            self.lost(&active);
            return Err(RequestError::Lost(reason));
        }
        inbox.recv().map_err(|error| {
            self.lost(&active);
            RequestError::Lost(error)
        })
    }

    /// Asks over whatever connection there is, for a request the far side
    /// keeps no state about.
    pub fn request(&self, stream: u32, payload: &[u8]) -> Result<Vec<u8>, RequestError> {
        let generation = self.connected(stream).map_err(RequestError::Unsent)?;
        self.request_on(generation, stream, payload)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::{Hello, Options};
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

    const OPTIONS: Options = Options {
        hello: Hello {
            program: "td-test",
            version: 1,
        },
        streams: &[1, 2],
        max_message: 1 << 20,
        unread: 1,
    };

    /// A near end, and a far side answering each request on stream 1 as
    /// `answer` says on a thread of its own; `None` hangs up instead. The
    /// handle it returns hangs up from outside.
    fn linked(answer: impl Fn(Vec<u8>) -> Option<Vec<u8>> + Send + 'static) -> (Link, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        let end = a.try_clone().unwrap();
        let far = std::thread::spawn(move || {
            let hang_up = b.try_clone().unwrap();
            let reader = b.try_clone().unwrap();
            let mut far = Link::over(reader, b, OPTIONS).unwrap();
            let inbox = far.take_inbox(1).unwrap();
            let sender = far.sender();
            std::thread::spawn(move || {
                let _far = far;
                while let Ok(message) = inbox.recv() {
                    match answer(message) {
                        Some(reply) => {
                            if sender.send(1, &reply).is_err() {
                                return;
                            }
                        }
                        None => {
                            let _ = hang_up.shutdown(Shutdown::Both);
                            return;
                        }
                    }
                }
            });
        });
        let reader = a.try_clone().unwrap();
        let near = Link::over(reader, a, OPTIONS).unwrap();
        far.join().unwrap();
        (near, end)
    }

    /// A client whose every connection is a far side `answer` serves, how many
    /// it has made, and a handle on the latest that hangs it up.
    fn client(
        answer: impl Fn(Vec<u8>) -> Option<Vec<u8>> + Clone + Send + 'static,
    ) -> (Client, Arc<AtomicU64>, Arc<Mutex<Option<UnixStream>>>) {
        let connections = Arc::new(AtomicU64::new(0));
        let ends = Arc::new(Mutex::new(None));
        let (counted, kept) = (Arc::clone(&connections), Arc::clone(&ends));
        let connector: Connector = Box::new(move || {
            counted.fetch_add(1, Ordering::Relaxed);
            let (near, end) = linked(answer.clone());
            *kept.lock().unwrap() = Some(end);
            Ok(near)
        });
        (Client::new(&[1, 2], connector), connections, ends)
    }

    #[test]
    fn a_request_connects_once_and_reuses_the_connection() {
        let (client, connections, _ends) = client(|mut message| {
            message.push(b'!');
            Some(message)
        });
        assert_eq!(client.request(1, b"one").unwrap(), b"one!");
        assert_eq!(client.request(1, b"two").unwrap(), b"two!");
        assert_eq!(connections.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_lost_request_says_so_and_the_next_one_reconnects_with_a_new_generation() {
        let (client, connections, _ends) = client(|message| (message != b"die").then_some(message));
        let first = client.connected(1).unwrap();
        assert_eq!(client.request_on(first, 1, b"ok").unwrap(), b"ok");
        assert!(matches!(
            client.request_on(first, 1, b"die"),
            Err(RequestError::Lost(_))
        ));
        // The old generation is gone, and is not silently replaced under a
        // caller whose far side kept state.
        assert_eq!(client.request_on(first, 1, b"ok"), Err(RequestError::Stale));
        let second = client.connected(1).unwrap();
        assert_ne!(first, second);
        // Nor is a live new connection a home for a request meant for the old:
        // its far side would not know what the request refers to.
        assert_eq!(client.request_on(first, 1, b"ok"), Err(RequestError::Stale));
        assert_eq!(client.request_on(second, 1, b"ok").unwrap(), b"ok");
        assert_eq!(connections.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_link_the_reader_saw_end_while_idle_is_replaced_before_anything_is_sent() {
        let (client, connections, ends) = client(Some);
        let first = client.connected(1).unwrap();
        let end = ends.lock().unwrap().take().unwrap();
        end.shutdown(Shutdown::Both).unwrap();
        // The end reaches the inbox on the reader's thread, soon.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut second = client.connected(1).unwrap();
        while second == first {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
            second = client.connected(1).unwrap();
        }
        assert_eq!(client.request_on(second, 1, b"ok").unwrap(), b"ok");
        assert_eq!(connections.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_link_already_known_dead_is_unsent_for_a_caller_holding_its_generation() {
        let (client, connections, ends) = client(Some);
        let generation = client.connected(1).unwrap();
        let end = ends.lock().unwrap().take().unwrap();
        end.shutdown(Shutdown::Both).unwrap();
        // Once the reader has seen the end, nothing is sent into it.
        let active = client.active().unwrap();
        let quiet = active.inboxes.get(&2).unwrap().lock().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while quiet.ended().is_none() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        drop(quiet);
        assert!(matches!(
            client.request_on(generation, 1, b"x"),
            Err(RequestError::Unsent(Error::Closed(_)))
        ));
        assert_eq!(client.request(1, b"x").unwrap(), b"x");
        assert_eq!(connections.load(Ordering::Relaxed), 2);
    }

    /// A connection whose far side says `unasked` on stream 1, and then
    /// "after" on stream 2, of its own accord.
    fn pushing_client(unasked: &'static [u8]) -> Client {
        let connector: Connector = Box::new(move || {
            let (a, b) = UnixStream::pair().unwrap();
            let reader = a.try_clone().unwrap();
            let far = std::thread::spawn(move || {
                let reader = b.try_clone().unwrap();
                let far = Link::over(reader, b, OPTIONS).unwrap();
                let sender = far.sender();
                sender.send(1, unasked).unwrap();
                sender.send(2, b"after").unwrap();
                // Stays on the line, its reader holding its socket open.
                far
            });
            let near = Link::over(reader, a, OPTIONS);
            drop(far.join().unwrap());
            near
        });
        Client::new(&[1, 2], connector)
    }

    #[test]
    fn a_message_nobody_asked_for_is_not_taken_for_the_next_reply() {
        let client = pushing_client(b"unasked");
        let generation = client.connected(1).unwrap();
        // Stream 2's message came after stream 1's, so that one is here.
        let active = client.active().unwrap();
        let after = active.inboxes.get(&2).unwrap().lock().unwrap().recv();
        assert_eq!(after.unwrap(), b"after");
        assert!(matches!(
            client.request_on(generation, 1, b"x"),
            Err(RequestError::Unsent(Error::Protocol(_)))
        ));
    }

    #[test]
    fn connected_says_when_the_far_side_broke_the_protocol_then_connects_again() {
        let client = pushing_client(b"unasked");
        let first = client.connected(1).unwrap();
        let active = client.active().unwrap();
        let after = active.inboxes.get(&2).unwrap().lock().unwrap().recv();
        assert_eq!(after.unwrap(), b"after");
        assert!(matches!(client.connected(1), Err(Error::Protocol(_))));
        assert_ne!(client.connected(1).unwrap(), first);
    }

    /// A client whose connector says when it has begun, waits to be released,
    /// and then gives what `outcome` makes of its count of attempts.
    fn held_client(
        outcome: impl Fn(u64) -> Result<Link, Error> + Send + 'static,
    ) -> (
        Arc<Client>,
        Arc<AtomicU64>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (began, begun) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let attempts = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&attempts);
        let connector: Connector = Box::new(move || {
            let count = counted.fetch_add(1, Ordering::Relaxed) + 1;
            began.send(()).unwrap();
            released.recv().unwrap();
            outcome(count)
        });
        (
            Arc::new(Client::new(&[1, 2], connector)),
            attempts,
            begun,
            release,
        )
    }

    #[test]
    fn a_thread_waiting_on_a_connection_shares_it() {
        let (client, attempts, begun, release) = held_client(|_| Ok(linked(Some).0));
        let first = {
            let client = Arc::clone(&client);
            std::thread::spawn(move || client.connected(1).unwrap())
        };
        begun.recv().unwrap();
        // Asked while that attempt was under way, by what it saw then.
        let second = {
            let client = Arc::clone(&client);
            std::thread::spawn(move || client.connect_after(0).unwrap())
        };
        release.send(()).unwrap();
        assert_eq!(first.join().unwrap(), second.join().unwrap());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_thread_waiting_on_a_connection_that_fails_hears_so_and_asks_nothing_twice() {
        let (client, attempts, begun, release) =
            held_client(|count| Err(Error::Closed(format!("attempt {count}"))));
        let first = {
            let client = Arc::clone(&client);
            std::thread::spawn(move || client.connected(1))
        };
        begun.recv().unwrap();
        let second = {
            let client = Arc::clone(&client);
            std::thread::spawn(move || client.connect_after(0))
        };
        release.send(()).unwrap();
        let said = Err(Error::Closed("attempt 1".into()));
        assert_eq!(first.join().unwrap(), said);
        assert_eq!(second.join().unwrap(), said);
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        // Whoever asks next asks again.
        let third = {
            let client = Arc::clone(&client);
            std::thread::spawn(move || client.connected(1))
        };
        begun.recv().unwrap();
        release.send(()).unwrap();
        assert_eq!(
            third.join().unwrap(),
            Err(Error::Closed("attempt 2".into()))
        );
    }

    #[test]
    fn a_request_whose_send_fails_is_lost_and_says_why_the_link_ended() {
        // A writer that fails once armed, and says so; only then does the far
        // side go, so the reader cannot hear of the end ahead of the failure.
        struct Failing {
            armed: Arc<std::sync::atomic::AtomicBool>,
            failed: std::sync::mpsc::Sender<()>,
            stream: UnixStream,
        }
        impl std::io::Write for Failing {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.armed.load(Ordering::Relaxed) {
                    let _ = self.failed.send(());
                    return Err(std::io::ErrorKind::BrokenPipe.into());
                }
                self.stream.write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (failed, write_failed) = std::sync::mpsc::channel();
        let far = Arc::new(Mutex::new(None));
        let (arm, held) = (Arc::clone(&armed), Arc::clone(&far));
        let connector: Connector = Box::new(move || {
            let (near, theirs) = UnixStream::pair().unwrap();
            let mut payload = OPTIONS.hello.version.to_be_bytes().to_vec();
            payload.extend_from_slice(OPTIONS.hello.program.as_bytes());
            let mut wire = b"td-remote 1\n".to_vec();
            wire.extend_from_slice(&crate::frame::encode(&payload).unwrap());
            std::io::Write::write_all(&mut &theirs, &wire).unwrap();
            *held.lock().unwrap() = Some(theirs);
            let writer = Failing {
                armed: Arc::clone(&arm),
                failed: failed.clone(),
                stream: near.try_clone().unwrap(),
            };
            Link::over(near, writer, OPTIONS)
        });
        let client = Client::new(&[1, 2], connector);
        let generation = client.connected(1).unwrap();
        let theirs = far.lock().unwrap().take().unwrap();
        armed.store(true, Ordering::Relaxed);
        let gone = std::thread::spawn(move || {
            write_failed.recv().unwrap();
            theirs.shutdown(Shutdown::Write).unwrap();
            // Closed with bytes unread, a socket would say "reset", as a pipe
            // never does.
            theirs
        });
        // Not `Io("Broken pipe")`.
        match client.request_on(generation, 1, b"x") {
            Err(RequestError::Lost(Error::Closed(_))) => {}
            other => panic!("{other:?}"),
        }
        drop(gone.join().unwrap());
        armed.store(false, Ordering::Relaxed);
        // The link was given up, so the next one is another connection.
        assert_ne!(client.connected(1).unwrap(), generation);
    }

    #[test]
    fn a_link_whose_write_failed_hears_nothing_more_and_says_so() {
        let (client, _connections, ends) = client(Some);
        let generation = client.connected(1).unwrap();
        let active = client.active().unwrap();
        // The latch, as a failed write leaves it, ahead of the reader's news.
        let end = ends.lock().unwrap().take().unwrap();
        end.shutdown(Shutdown::Write).unwrap();
        let sender = active.sender.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !sender.broken() {
            assert!(std::time::Instant::now() < deadline);
            let _ = sender.send(2, b"into a closed socket");
        }
        assert!(matches!(
            client.request_on(generation, 1, b"x"),
            Err(RequestError::Unsent(_))
        ));
    }

    #[test]
    fn a_request_queued_behind_one_that_lost_the_link_is_stale_not_lost() {
        // The far side holds the first request until told, then hangs up.
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = Arc::new(Mutex::new(released));
        let (client, _connections, _ends) = client(move |_| {
            let _ = released.lock().unwrap().recv();
            None
        });
        let client = Arc::new(client);
        let generation = client.connected(1).unwrap();
        let first = {
            let client = Arc::clone(&client);
            std::thread::spawn(move || client.request_on(generation, 1, b"one"))
        };
        // The first holds the stream: `try_lock` fails while it waits.
        let active = client.active().unwrap();
        let inbox = active.inboxes.get(&1).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while inbox.try_lock().is_ok() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let second = {
            let client = Arc::clone(&client);
            std::thread::spawn(move || client.request_on(generation, 1, b"two"))
        };
        // Long enough to be queued; if it is not, it is refused as stale anyway.
        std::thread::sleep(std::time::Duration::from_millis(20));
        release.send(()).unwrap();
        assert!(matches!(first.join().unwrap(), Err(RequestError::Lost(_))));
        assert_eq!(second.join().unwrap(), Err(RequestError::Stale));
    }

    #[test]
    fn an_abandoned_connection_is_replaced_and_another_generations_is_not_touched() {
        let (client, connections, _ends) = client(Some);
        let first = client.connected(1).unwrap();
        client.abandon(first + 1);
        assert_eq!(client.connected(1).unwrap(), first);
        client.abandon(first);
        assert_eq!(client.request_on(first, 1, b"x"), Err(RequestError::Stale));
        assert_ne!(client.connected(1).unwrap(), first);
        assert_eq!(connections.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_failed_connection_sends_nothing_and_is_tried_again() {
        let attempts = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&attempts);
        let connector: Connector = Box::new(move || {
            if counted.fetch_add(1, Ordering::Relaxed) == 0 {
                return Err(Error::Closed("no route to host".into()));
            }
            Ok(linked(Some).0)
        });
        let client = Client::new(&[1, 2], connector);
        match client.request(1, b"x") {
            Err(RequestError::Unsent(Error::Closed(detail))) => {
                assert!(detail.contains("no route"))
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(client.request(1, b"x").unwrap(), b"x");
    }

    #[test]
    fn a_message_past_the_bound_is_unsent_and_leaves_the_connection_good() {
        let (client, connections, _ends) = client(Some);
        let generation = client.connected(1).unwrap();
        assert!(matches!(
            client.request_on(generation, 1, &vec![0u8; (1 << 20) + 1]),
            Err(RequestError::Unsent(Error::TooLong(_)))
        ));
        assert_eq!(client.request_on(generation, 1, b"ok").unwrap(), b"ok");
        assert_eq!(connections.load(Ordering::Relaxed), 1);
    }
}
