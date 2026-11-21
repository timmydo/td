//! Dedicated HTTP-01 responder: exact host/token on bounded, independently serviced peer slots.
use super::Error;
use crate::{
    network_wake::{Notifications, Receiver},
    readiness_sys::{Connected, Interest, PollSet},
    smtp_rate::PeerKey,
};
use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        RwLock,
    },
    time::{Duration, Instant},
};
pub struct Current {
    pub names: Vec<String>,
    pub token: String,
    pub authorization: String,
    pub expires: Instant,
}
const PEER_SLOTS: usize = 8;
const LIFETIME: Duration = Duration::from_secs(5);
const ACCEPT_RETRY: Duration = Duration::from_millis(10);

#[derive(Default)]
pub struct Control {
    stopped: AtomicBool,
    wake: Notifications,
}

impl Control {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.wake.notify();
    }

    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

pub fn run(
    listener: TcpListener,
    current: &RwLock<Current>,
    control: &Control,
) -> Result<(), Error> {
    listener
        .set_nonblocking(true)
        .map_err(|_| Error::Transport)?;
    let receiver = control.wake.attach().map_err(|_| Error::Transport)?;
    let mut slots: [Option<Slot>; PEER_SLOTS] = [const { None }; PEER_SLOTS];
    let mut accept_after = Instant::now();
    while !control.stopped() {
        for slot in &mut slots {
            if let Some(active) = slot {
                if !active.step(current, Instant::now()).unwrap_or(false) {
                    *slot = None;
                }
            }
        }
        if Instant::now() >= accept_after {
            // Bound accept work even when the listener stays readable.
            for _ in 0..16 {
                match listener.accept() {
                    Ok((stream, address)) => {
                        let now = Instant::now();
                        let peer = PeerKey::from(address.ip());
                        if let Some(place) = available(&mut slots, peer, now) {
                            if stream.set_nonblocking(true).is_ok() {
                                *place = Some(Slot::new(stream, peer, now));
                            }
                        }
                    }
                    Err(e) if pending(&e) => break,
                    Err(e) if crate::transport::retry_accept(&e) => {
                        accept_after = Instant::now() + ACCEPT_RETRY;
                        break;
                    }
                    Err(_) => return Err(Error::Transport),
                }
            }
        }
        wait(&listener, &slots, &receiver, accept_after).map_err(|_| Error::Transport)?;
    }
    Ok(())
}

fn wait(
    listener: &TcpListener,
    slots: &[Option<Slot>; PEER_SLOTS],
    receiver: &Receiver<'_>,
    accept_after: Instant,
) -> io::Result<()> {
    let now = Instant::now();
    let mut poll = PollSet::new(&receiver.socket);
    let mut until = if now < accept_after {
        accept_after
    } else {
        poll.add_tcp_listener(listener)?;
        now + Duration::from_secs(3600)
    };
    for slot in slots.iter().flatten() {
        let read = matches!(slot.phase, Phase::Read);
        poll.add_connected(
            Connected::Tcp(&slot.stream),
            Interest { read, write: !read },
        )?;
        until = until.min(slot.opened + LIFETIME);
    }
    poll.wait(until)
}

struct Slot {
    stream: TcpStream,
    peer: PeerKey,
    opened: Instant,
    progress: Instant,
    bytes: [u8; 4096],
    length: usize,
    // The same fixed cell holds the response once the request is parsed.
    phase: Phase,
}

#[derive(Clone, Copy)]
enum Phase {
    Read,
    Answer,
    Write(usize),
}

fn available(slots: &mut [Option<Slot>], peer: PeerKey, now: Instant) -> Option<&mut Option<Slot>> {
    if slots.iter().flatten().any(|slot| slot.peer == peer) {
        return None;
    }
    let index = slots.iter().position(Option::is_none).or_else(|| {
        slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                let slot = slot.as_ref()?;
                (now.duration_since(slot.progress) >= Duration::from_secs(1))
                    .then_some((i, slot.opened))
            })
            .min_by_key(|(_, opened)| *opened)
            .map(|(i, _)| i)
    })?;
    slots.get_mut(index)
}

fn pending(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    )
}

impl Slot {
    fn new(stream: TcpStream, peer: PeerKey, now: Instant) -> Self {
        Self {
            stream,
            peer,
            opened: now,
            progress: now,
            bytes: [0; 4096],
            length: 0,
            phase: Phase::Read,
        }
    }

    /// Perform at most one nonblocking transfer. False releases the slot.
    fn step(&mut self, current: &RwLock<Current>, now: Instant) -> Result<bool, Error> {
        if now.duration_since(self.opened) >= LIFETIME {
            return Ok(false);
        }
        let transfer = match self.phase {
            Phase::Answer => return self.start_response(current, now),
            Phase::Write(sent) => self
                .stream
                .write(self.bytes.get(sent..self.length).ok_or(Error::Invalid)?),
            Phase::Read => self
                .stream
                .read(self.bytes.get_mut(self.length..).ok_or(Error::Invalid)?),
        };
        let count = match transfer {
            Ok(0) => return Ok(false),
            Ok(count) => count,
            Err(e) if pending(&e) => return Ok(true),
            Err(_) => return Err(Error::Transport),
        };
        self.progress = now;
        if let Phase::Write(sent) = &mut self.phase {
            *sent += count;
            return Ok(*sent < self.length);
        }
        let previous = self.length;
        self.length += count;
        let received = self.bytes.get(..self.length).ok_or(Error::Invalid)?;
        if !received
            .get(previous.saturating_sub(3)..)
            .ok_or(Error::Invalid)?
            .windows(4)
            .any(|b| b == b"\r\n\r\n")
        {
            return Ok(self.length < self.bytes.len());
        }
        self.phase = Phase::Answer;
        Ok(true)
    }

    fn start_response(&mut self, current: &RwLock<Current>, now: Instant) -> Result<bool, Error> {
        let received = self.bytes.get(..self.length).ok_or(Error::Invalid)?;
        let current = current.read().map_err(|_| Error::Conflict)?;
        let body = answer(received, &current, Instant::now()).unwrap_or_default();
        let status = if body.is_empty() {
            "404 Not Found"
        } else {
            "200 OK"
        };
        // Persisted key authorizations are at most 300 bytes.
        let mut response = [0; 512];
        let mut output = response.as_mut_slice();
        write!(output, "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}", body.len())
            .map_err(|_| Error::Capacity)?;
        let unused = output.len();
        let reply = response
            .get(..response.len() - unused)
            .ok_or(Error::Invalid)?;
        // Keep the live cache pinned until the first bytes reach the socket.
        match self.stream.write(reply) {
            Ok(0) => Ok(false),
            Ok(count) if count == reply.len() => Ok(false),
            Ok(count) => {
                let tail = reply.get(count..).ok_or(Error::Invalid)?;
                self.bytes
                    .get_mut(..tail.len())
                    .ok_or(Error::Capacity)?
                    .copy_from_slice(tail);
                self.length = tail.len();
                self.phase = Phase::Write(0);
                self.progress = now;
                Ok(true)
            }
            Err(e) if pending(&e) => Ok(true),
            Err(_) => Err(Error::Transport),
        }
    }
}
fn answer<'a>(request: &[u8], current: &'a Current, now: Instant) -> Option<&'a str> {
    if now >= current.expires || current.token.is_empty() || current.authorization.is_empty() {
        return None;
    }
    let request = std::str::from_utf8(request).ok()?;
    let mut lines = request.split("\r\n");
    let line = lines.next()?;
    let target = line.strip_prefix("GET ")?.strip_suffix(" HTTP/1.1")?;
    if target.strip_prefix("/.well-known/acme-challenge/")? != current.token {
        return None;
    }
    let mut host = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("host") {
            if host.is_some() {
                return None;
            }
            host = Some(value.trim_matches([' ', '\t']));
        }
        if name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("content-length")
        {
            return None;
        }
    }
    let host = host?;
    let host = host.strip_suffix(":80").unwrap_or(host);
    if !current
        .names
        .iter()
        .any(|name| name.eq_ignore_ascii_case(host))
    {
        return None;
    }
    Some(&current.authorization)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[allow(clippy::unwrap_used)]
    fn connected(peer: PeerKey, now: Instant) -> (TcpStream, Slot) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();
        (client, Slot::new(server, peer, now))
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn active_peers_keep_one_slot_and_only_idle_peers_can_be_replaced() {
        let now = Instant::now();
        let first = PeerKey::V4(1);
        let second = PeerKey::V4(2);
        let other = PeerKey::V4(3);
        let (_client1, older) = connected(first, now);
        let mut slots = [Some(older), None];
        assert!(available(&mut slots, first, now).is_none());
        let later = now + Duration::from_millis(100);
        let (_client2, newer) = connected(second, later);
        *available(&mut slots, second, later).unwrap() = Some(newer);
        assert!(available(&mut slots, other, now + Duration::from_millis(999)).is_none());
        let idle = later + Duration::from_secs(1);
        assert_eq!(
            available(&mut slots, other, idle)
                .unwrap()
                .as_ref()
                .unwrap()
                .peer,
            first
        );
        slots.iter_mut().flatten().next().unwrap().progress = idle;
        assert_eq!(
            available(&mut slots, other, idle)
                .unwrap()
                .as_ref()
                .unwrap()
                .peer,
            second
        );
        assert!(available(&mut slots, second, idle).is_none());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn fragmented_request_and_idle_connection_have_independent_turns_and_deadlines() {
        let now = Instant::now();
        let current = RwLock::new(Current {
            names: vec!["mail.example.test".into()],
            token: "abc".into(),
            authorization: "abc.key".into(),
            expires: now + Duration::from_secs(60),
        });
        let (_idle_peer, mut idle) = connected(PeerKey::V4(1), now);
        let (mut client, mut active) = connected(PeerKey::V4(2), now);
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let fragment =
            b"GET /.well-known/acme-challenge/abc HTTP/1.1\r\nHost: mail.example.test\r\n\r";
        client.write_all(fragment).unwrap();
        assert!(idle.step(&current, now).unwrap());
        while active.length < fragment.len() {
            assert!(active.step(&current, Instant::now()).unwrap());
            assert!(now.elapsed() < Duration::from_secs(2));
        }
        assert!(matches!(active.phase, Phase::Read));
        client.write_all(b"\n").unwrap();
        let until = now + Duration::from_secs(2);
        while active.step(&current, Instant::now()).unwrap() {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        drop(active);
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with("\r\n\r\nabc.key"));
        assert!(!idle.step(&current, now + Duration::from_secs(5)).unwrap());
    }
    #[test]
    #[allow(clippy::unwrap_used)]
    fn a_replaced_challenge_cannot_start_a_staged_response() {
        let now = Instant::now();
        let current = RwLock::new(Current {
            names: vec!["mail.example.test".into()],
            token: "abc".into(),
            authorization: "abc.key".into(),
            expires: now + Duration::from_secs(60),
        });
        let (mut client, mut active) = connected(PeerKey::V4(2), now);
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        client
            .write_all(
                b"GET /.well-known/acme-challenge/abc HTTP/1.1\r\nHost: mail.example.test\r\n\r\n",
            )
            .unwrap();
        while matches!(active.phase, Phase::Read) {
            assert!(active.step(&current, Instant::now()).unwrap());
            assert!(now.elapsed() < Duration::from_secs(2));
        }
        current.write().unwrap().token.clear();
        while active.step(&current, Instant::now()).unwrap() {
            assert!(now.elapsed() < Duration::from_secs(2));
        }
        drop(active);
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 404"));
        assert!(!response.contains("abc.key"));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn eight_validators_complete_independent_challenge_requests() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let control = Control::default();
        let receiver = control.wake.attach().unwrap();
        let now = Instant::now();
        let current = RwLock::new(Current {
            names: vec!["mail.example.test".into()],
            token: "abc".into(),
            authorization: "abc.key".into(),
            expires: now + Duration::from_secs(60),
        });
        let mut slots = [const { None }; PEER_SLOTS];
        let mut clients = Vec::new();
        // Use loopback socket pairs with distinct validator identities.
        for id in 1..=8 {
            let peer = PeerKey::V4(id);
            let (client, slot) = connected(peer, now);
            *available(&mut slots, peer, now).unwrap() = Some(slot);
            clients.push(client);
        }
        assert!(available(&mut slots, PeerKey::V4(9), now).is_none());
        for client in &mut clients {
            client.write_all(b"GET /.well-known/acme-challenge/abc HTTP/1.1\r\nHost: mail.example.test\r\n\r\n").unwrap();
        }
        while slots.iter().any(Option::is_some) {
            wait(&listener, &slots, &receiver, now).unwrap();
            for place in &mut slots {
                if let Some(slot) = place {
                    if !slot.step(&current, Instant::now()).unwrap() {
                        *place = None;
                    }
                }
            }
            assert!(now.elapsed() < LIFETIME);
        }
        for mut client in clients {
            let mut reply = String::new();
            client.read_to_string(&mut reply).unwrap();
            assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"));
            assert!(reply.ends_with("\r\n\r\nabc.key"));
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn backoff_masks_listener_until_retry_while_peer_expiry_and_stop_wake_earlier() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let _queued = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let control = Control::default();
        let receiver = control.wake.attach().unwrap();
        let mut slots = [const { None }; PEER_SLOTS];
        let first_retry = Instant::now() + ACCEPT_RETRY;
        wait(&listener, &slots, &receiver, first_retry).unwrap();
        assert!(Instant::now() >= first_retry);

        let now = Instant::now();
        let peer = PeerKey::V4(1);
        let deadline = now + ACCEPT_RETRY;
        let (_client, slot) = connected(peer, deadline - LIFETIME);
        *available(&mut slots, peer, now).unwrap() = Some(slot);
        let retry = now + Duration::from_secs(2);
        wait(&listener, &slots, &receiver, retry).unwrap();
        assert!(Instant::now() >= deadline && Instant::now() < retry);

        control.stop();
        wait(&listener, &[const { None }; PEER_SLOTS], &receiver, retry).unwrap();
        assert!(control.stopped());
        assert!(Instant::now() < retry);
    }

    #[test]
    fn challenge_is_exact_and_expires() {
        let now = Instant::now();
        let current = Current {
            names: vec!["mail.example.test".into()],
            token: "abc".into(),
            authorization: "abc.key".into(),
            expires: now + Duration::from_secs(1),
        };
        let good =
            b"GET /.well-known/acme-challenge/abc HTTP/1.1\r\nHost: mail.example.test\r\n\r\n";
        assert_eq!(answer(good, &current, now), Some("abc.key"));
        assert!(answer(good, &current, current.expires).is_none());
        for bad in ["GET /.well-known/acme-challenge/abc?x HTTP/1.1\r\nHost: mail.example.test\r\n\r\n","GET /.well-known/acme-challenge/abc HTTP/1.1\r\nHost: evil.test\r\n\r\n","GET /.well-known/acme-challenge/abc HTTP/1.1\r\nHost: mail.example.test\r\nHost: mail.example.test\r\n\r\n"] {assert!(answer(bad.as_bytes(),&current,now).is_none());}
    }
}
