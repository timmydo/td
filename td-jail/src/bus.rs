//! The three calls td-jail makes to the session broker, and nothing else.
//!
//! `Hello`, `Register` and `Complete`. Two connections per launch and two
//! exchanges on each, since every connection owes the broker a `Hello` before
//! anything else — four call/reply pairs to place one jail.
//!
//! `APPLICATIONS.md` §D gives the jail two obligations on the bus: register an
//! instance before it unshares, and complete that registration with the
//! stage-2 pid once there is one. Both are ordinary D-Bus method calls, so
//! this module speaks enough of the wire to make them and to read what comes
//! back.
//!
//! # Why this is not a D-Bus library
//!
//! td-busd carries a general marshaller because a broker must read whatever a
//! peer sends. A launcher is the other case: three calls, with signatures
//! fixed at compile time, to one destination it already trusts. Copying two thousand lines of general encoding to place three
//! known messages would put a second copy of the wire rules in the tree, and
//! principle 2 says the dependency-free surfaces stay small rather than
//! convenient. So this encodes `""`, `"ssas"` and `"su"`, decodes a reply
//! header and a `"s"` body, and refuses everything else it meets.
//!
//! The asymmetry is deliberate in the other direction too. A malformed reply
//! here is a launch that fails, not a broker that misroutes, so every decode
//! below is bounds-checked and returns an error rather than tolerating what
//! it does not understand.
//!
//! # Two connections, not one
//!
//! Phase one runs in stage 0 and phase two in stage 1, and they cannot share a
//! connection: §A step 0 closes every descriptor above stderr between the
//! `unshare` and the spawn. So each call here opens its own connection and
//! drops it. What ties the phases together is the token and the fact that
//! both calls come from the same PROCESS — `unshare(CLONE_NEWPID)` does not
//! move the caller, so the pid the broker reads from `SO_PEERCRED` is the same
//! both times, which is exactly what §D's completion rule compares.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

const LITTLE_ENDIAN: u8 = b'l';
const BIG_ENDIAN: u8 = b'B';
const PROTOCOL_VERSION: u8 = 1;

const METHOD_CALL: u8 = 1;
const METHOD_RETURN: u8 = 2;
const ERROR: u8 = 3;

const FIELD_PATH: u8 = 1;
const FIELD_INTERFACE: u8 = 2;
const FIELD_MEMBER: u8 = 3;
const FIELD_ERROR_NAME: u8 = 4;
const FIELD_REPLY_SERIAL: u8 = 5;
const FIELD_DESTINATION: u8 = 6;
const FIELD_SENDER: u8 = 7;
const FIELD_SIGNATURE: u8 = 8;

const BUS_NAME: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const JAIL_INTERFACE: &str = "td.Jail1";
const JAIL_PATH: &str = "/td/Jail1";

/// A reply this launcher is willing to read. The broker's answers here are a
/// token or nothing, so anything approaching this is a broker that has stopped
/// making sense, and reading it is not a service to anybody.
const MAX_REPLY_BYTES: usize = 64 * 1024;

/// Messages that are not the reply, before giving up. td-busd sends no signals
/// at this rung, so the expected count is zero; the allowance is for the
/// version that does, rather than a promise about today.
const MAX_UNRELATED_MESSAGES: usize = 8;

/// How long any single read or write may block.
///
/// A launch that hangs is worse than a launch that fails: the caller is a
/// supervised unit with its own deadline, and a jail waiting for ever on a
/// wedged broker holds an application slot and reports nothing.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Phase one: the instance exists and has no pid yet. Returns its token.
pub fn register(
    socket: &Path,
    uid: u32,
    instance: &str,
    app_id: &str,
    services: &[String],
) -> io::Result<String> {
    let mut connection = Connection::open(socket, uid)?;
    let mut body = Encoder::default();
    body.string(instance)?;
    body.string(app_id)?;
    body.array_of_strings(services)?;
    let reply = connection.call(JAIL_PATH, JAIL_INTERFACE, "Register", "ssas", &body.bytes)?;
    reply.one_string()
}

/// Phase two: bind the stage-2 pid to the instance the token opened.
pub fn complete(socket: &Path, uid: u32, token: &str, pid: u32) -> io::Result<()> {
    let mut connection = Connection::open(socket, uid)?;
    let mut body = Encoder::default();
    body.string(token)?;
    body.u32(pid);
    let reply = connection.call(JAIL_PATH, JAIL_INTERFACE, "Complete", "su", &body.bytes)?;
    reply.no_body()
}

/// One connection, from the handshake to the last reply read on it.
struct Connection {
    stream: UnixStream,
    serial: u32,
    /// When this whole exchange gives up.
    until: Instant,
}

/// A `Read` over the socket that spends one deadline across every read.
///
/// The adapter exists because `read_message` is generic over `Read` so the
/// recorded conversations can be replayed through it. Putting the clock here
/// keeps that property and still bounds a real socket.
struct Timed<'a> {
    stream: &'a UnixStream,
    until: Instant,
}

impl Timed<'_> {
    /// What is left of the deadline, or the error that ends the exchange.
    fn left(&self) -> io::Result<Duration> {
        self.until
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the session bus did not finish this exchange in time",
                )
            })
    }
}

impl Read for Timed<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.left()?))?;
        (&*self.stream).read(buffer)
    }
}

/// Writes are on the same clock, because the doc above says the budget is for
/// the exchange and a per-write timeout carrying the FULL budget is not that.
/// A draft set it once at open, which made the worst case two budgets: twenty
/// seconds to connect and twenty more inside one write. Unreachable with two
/// hundred bytes to send and a real broker, and still not what was claimed.
impl Write for Timed<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.left()?))?;
        (&*self.stream).write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self.stream).flush()
    }
}

/// `connect(2)` with a deadline, which `std` does not offer for a Unix socket.
///
/// Done on a thread rather than with a non-blocking socket because the
/// non-blocking route needs `fcntl`, and this crate's whole `unsafe` surface
/// is one `syscall5` in `sys.rs` that `UNSAFE.md` §9 pins. A thread costs a
/// few microseconds on a path taken twice per launch. If the connect never
/// returns, the thread is left holding it and the process exits out from
/// under it, which is exactly what should happen to a launch that failed.
fn connect_within(socket: &Path, budget: Duration) -> io::Result<UnixStream> {
    let path = socket.to_path_buf();
    match within(budget, move || UnixStream::connect(&path)) {
        Some(Ok(stream)) => Ok(stream),
        Some(Err(error)) => Err(io::Error::other(format!(
            "connect to the session bus at {socket:?}: {error}"
        ))),
        None => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("connect to the session bus at {socket:?}: it never accepted"),
        )),
    }
}

/// `work`'s result, or `None` if it did not finish in `budget`.
///
/// Split out from the connect so the deadline can be tested on its own. The
/// alternative was a test that fills a listener's backlog to make a real
/// `connect` block, which needs upwards of 128 live sockets in a shared test
/// binary — and that churn made `sys`'s close/EBADF test start failing, since
/// a descriptor number it had just closed was being reallocated underneath it
/// by another thread. A test should not have to make the rest of the suite
/// racy to prove one deadline.
///
/// # The helper is JOINED, and that is load-bearing
///
/// `unshare(CLONE_NEWUSER)` fails with `EINVAL` for a multithreaded process,
/// and `launch_application` unshares a few statements after phase one returns.
/// A draft returned as soon as the helper SENT, leaving it alive but not yet
/// exited: a reviewer reproduced `EINVAL` in 194 of 200 runs with nothing
/// between the two, and 0 of 110 on the real path, where the handshake and
/// two round trips always close the window. So it was invisible, boot-only,
/// and one shortening of phase one — a cached connection, an earlier connect,
/// a retry — away from being a nondeterministic failure to launch anything.
///
/// Joining costs microseconds on the success path, because the helper has
/// already sent and is on its way out. On the TIMEOUT path there is nothing to
/// join — the helper is blocked in the syscall this deadline exists to escape
/// — so it is left running and the process exits out from under it, which is
/// what should happen to a launch that has already failed. Nothing unshares
/// after that: the error propagates out of `launch_application`.
fn within<T: Send + 'static>(
    budget: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (done, waiting) = std::sync::mpsc::channel();
    let helper = std::thread::spawn(move || {
        let _ = done.send(work());
    });
    let answer = waiting.recv_timeout(budget).ok();
    if answer.is_some() {
        let _ = helper.join();
    }
    answer
}

impl Connection {
    fn open(socket: &Path, uid: u32) -> io::Result<Self> {
        Self::open_within(socket, uid, TIMEOUT)
    }

    /// The budget is a parameter so a test can hold a broker open and watch
    /// this give up, rather than assert that a constant is spelled correctly
    /// and wait twenty seconds to find out it is not applied.
    ///
    /// `budget` is for the WHOLE exchange — connect, handshake, `Hello` and
    /// the one call — not for each read. Two things made that necessary. A
    /// draft set the socket options after `connect`, which does not return
    /// until the peer accepts: a listener that binds `/run/user/1000/bus`,
    /// fills its backlog and never accepts held stage 1 for ever, and since
    /// stage 1 never EXITED the fixture's `restart=always` could not recover
    /// it either. And `SO_RCVTIMEO` bounds one read, so a peer sending a byte
    /// every nineteen seconds stretched a 512-byte handshake line into hours
    /// without ever tripping it.
    fn open_within(socket: &Path, uid: u32, budget: Duration) -> io::Result<Self> {
        let until = Instant::now()
            .checked_add(budget)
            .ok_or_else(|| io::Error::other("the launch deadline is not representable"))?;
        let stream = connect_within(socket, budget)?;
        let mut connection = Self {
            stream,
            serial: 0,
            until,
        };
        connection.handshake(uid)?;
        connection.hello()?;
        Ok(connection)
    }

    /// The stream, read through what is left of the deadline.
    fn timed(&self) -> Timed<'_> {
        Timed {
            stream: &self.stream,
            until: self.until,
        }
    }

    /// `EXTERNAL`, which is the only mechanism the broker offers.
    ///
    /// The identity is the uid as TEXT, hex-encoded — "1000" becomes
    /// "31303030" — which is the spelling the specification gives and the one
    /// td-busd's authenticator checks against `SO_PEERCRED`. Sending the
    /// number itself authenticates as some other uid or as nobody.
    fn handshake(&mut self, uid: u32) -> io::Result<()> {
        self.timed().write_all(auth_line(uid).as_bytes())?;
        let line = self.read_line()?;
        if !line.starts_with("OK ") {
            return Err(io::Error::other(format!(
                "the session bus refused this identity: {}",
                line.trim()
            )));
        }
        self.timed().write_all(b"BEGIN\r\n")?;
        Ok(())
    }

    /// One CRLF-terminated line of the text handshake, read a byte at a time.
    ///
    /// Byte at a time because the handshake and the binary stream share the
    /// socket: reading ahead past the CRLF would swallow the first bytes of
    /// the first message. A bounded line, because the peer decides its length.
    fn read_line(&mut self) -> io::Result<String> {
        let mut line = Vec::new();
        loop {
            let mut buffer = [0u8; 1];
            self.timed().read_exact(&mut buffer)?;
            // Destructured rather than indexed or `.get()`-ed: the length is in
            // the type, so this is total, and the alternative was an error
            // branch that could not be reached and could not be tested.
            let [byte] = buffer;
            if byte == b'\n' {
                break;
            }
            if line.len() >= 512 {
                return Err(io::Error::other("the bus sent an oversized handshake line"));
            }
            line.push(byte);
        }
        String::from_utf8(line)
            .map_err(|e| io::Error::other(format!("the bus handshake line is not UTF-8: {e}")))
    }

    /// `Hello`, which every connection owes the broker before anything else.
    fn hello(&mut self) -> io::Result<()> {
        let reply = self.call(BUS_PATH, BUS_NAME, "Hello", "", &[])?;
        // The unique name comes back and is not kept: this connection addresses
        // the broker and nobody addresses it.
        reply.one_string().map(|_| ())
    }

    fn next_serial(&mut self) -> io::Result<u32> {
        let next = self
            .serial
            .checked_add(1)
            .ok_or_else(|| io::Error::other("this connection ran out of serials"))?;
        self.serial = next;
        Ok(next)
    }

    /// One method call to the broker, and the reply to it.
    fn call(
        &mut self,
        path: &str,
        interface: &str,
        member: &str,
        signature: &str,
        body: &[u8],
    ) -> io::Result<Reply> {
        let serial = self.next_serial()?;
        let frame = method_call(serial, path, interface, member, signature, body)?;
        self.timed().write_all(&frame)?;
        read_reply(&mut self.timed(), serial, member)
    }
}

/// The reply to serial `serial`, skipping anything that arrives before it.
///
/// A free function over `Read` rather than a method, so the recorded
/// conversations in td-busd's `spec/` can be replayed through it. Those bytes
/// are what a real `dbus-daemon` sent a real client, and they are the only
/// oracle here that is not this author's reading of the specification.
///
/// # A serial is not an authenticator
///
/// The bus is a shared session bus and every peer on it is uid 1000. Directed
/// routing means any of them may send a `METHOD_RETURN` to this connection's
/// unique name, and serials here are 1 then 2 on a fresh connection — so a
/// peer that wants to answer for the broker can, and a first draft of this
/// function let it: it matched on `REPLY_SERIAL` alone.
///
/// What that bought an attacker was not a forged token, which is useless on
/// its own, but a forged `Complete` SUCCESS. Stage 1 would then write the
/// proof, release a jail whose real registration is still pending, and let it
/// expire — and the application, which is genuinely confined, resolves
/// `Unconfined` and gets everything the portal has. §D concedes that a rogue
/// uid-1000 process can register a false instance FOR ITSELF; this was the
/// other direction, degrading somebody else's real jail, and nothing in §D
/// conceded it.
///
/// So the sender is checked. td-busd rebuilds `SENDER` rather than relaying
/// it — a peer's own value is discarded and replaced with that connection's
/// unique name — so `org.freedesktop.DBus` is a claim only the broker can
/// make. A message that fails either test is SKIPPED rather than refused,
/// because the genuine reply may still be behind it.
///
/// Skipping does NOT make a launch unkillable by a peer, and a draft of this
/// comment said it did. Nine well-formed forgeries exhaust the allowance, and
/// ONE that fails to decode — over the reply ceiling, a bad endianness byte, a
/// broken frame — ends the launch immediately, because `read_message` returns
/// an error rather than something to skip past and the stream's framing is
/// gone with it. A reviewer did exactly that with a 200 KB directed reply. The
/// skip is worth having anyway: what it protects is the launch that is merely
/// SHARING a bus with chatter, and what it cannot protect against is a peer
/// that is trying. Both outcomes are fail-CLOSED — a refused launch the
/// fixture's `restart=always` retries — which is the difference that matters,
/// since the alternative was releasing an unregistered jail.
fn read_reply(stream: &mut impl Read, serial: u32, member: &str) -> io::Result<Reply> {
    for _ in 0..=MAX_UNRELATED_MESSAGES {
        let reply = read_message(stream)?;
        if reply.reply_serial != Some(serial) || reply.sender.as_deref() != Some(BUS_NAME) {
            continue;
        }
        if let Some(name) = &reply.error {
            return Err(io::Error::other(format!(
                "the session bus refused {member}: {name}{}",
                match reply.one_string() {
                    Ok(text) if !text.is_empty() => format!(": {text}"),
                    _ => String::new(),
                }
            )));
        }
        return Ok(reply);
    }
    Err(io::Error::other(format!(
        "the session bus sent no reply to {member} after \
         {MAX_UNRELATED_MESSAGES} unrelated messages"
    )))
}

fn read_message(stream: &mut impl Read) -> io::Result<Reply> {
    let mut head = [0u8; 16];
    stream.read_exact(&mut head)?;
    // Destructured, for the reason `read_line` gives: a fixed-size array's
    // fields are known to be there, and `.get()` on one buys an error branch
    // that no input can reach.
    let [endianness, kind, _flags, version, ..] = head;
    let little = match endianness {
        LITTLE_ENDIAN => true,
        BIG_ENDIAN => false,
        other => {
            return Err(io::Error::other(format!(
                "the bus sent an unknown endianness byte {other:?}"
            )))
        }
    };
    if version != PROTOCOL_VERSION {
        return Err(io::Error::other("the bus sent an unknown protocol version"));
    }
    let body_length = u32_at(&head, 4, little)?;
    let fields_length = u32_at(&head, 12, little)?;
    // Padding sits between the header fields and the body: the body starts
    // at the next eight-byte boundary whether or not anything needs it.
    let padding = (8 - (fields_length as usize % 8)) % 8;
    let rest = (fields_length as usize)
        .checked_add(padding)
        .and_then(|n| n.checked_add(body_length as usize))
        .ok_or_else(|| io::Error::other("the bus announced an impossible message length"))?;
    if rest > MAX_REPLY_BYTES {
        return Err(io::Error::other(format!(
            "the bus sent a {rest}-byte message, over the {MAX_REPLY_BYTES}-byte ceiling"
        )));
    }
    let mut tail = vec![0u8; rest];
    stream.read_exact(&mut tail)?;

    // `rest` was computed as fields + padding + body, so this slice and the
    // body slice below both fit by construction. Written as `.get()` anyway:
    // panicking indexing is out under `AGENTS.md`, the construction is a dozen
    // lines away and a future change to `rest` would make these live. They are
    // the one kind of branch this file does not claim a test for.
    let mut fields = Decoder {
        bytes: tail.get(..fields_length as usize).ok_or_else(|| {
            io::Error::other("the bus announced more header fields than it sent")
        })?,
        at: 0,
        little,
    };
    let mut reply_serial = None;
    let mut error = None;
    let mut sender = None;
    let mut signature = String::new();
    while fields.at < fields.bytes.len() {
        fields.align(8)?;
        if fields.at >= fields.bytes.len() {
            break;
        }
        let code = fields.byte()?;
        let kind = fields.signature()?;
        match (code, kind.as_str()) {
            (FIELD_REPLY_SERIAL, "u") => reply_serial = Some(fields.u32()?),
            (FIELD_ERROR_NAME, "s") => error = Some(fields.string()?),
            (FIELD_SENDER, "s") => sender = Some(fields.string()?),
            (FIELD_SIGNATURE, "g") => signature = fields.signature()?,
            (_, "s" | "o") => {
                fields.string()?;
            }
            (_, "g") => {
                fields.signature()?;
            }
            (_, "u") => {
                fields.u32()?;
            }
            (_, other) => {
                return Err(io::Error::other(format!(
                    "the bus sent a header field carrying an unsupported type {other:?}"
                )))
            }
        }
    }
    let body_at = (fields_length as usize)
        .checked_add(padding)
        .ok_or_else(|| io::Error::other("the bus announced an impossible body offset"))?;
    let body = tail
        .get(body_at..)
        .ok_or_else(|| io::Error::other("the bus announced more body than it sent"))?
        .to_vec();
    if kind == ERROR && error.is_none() {
        return Err(io::Error::other("the bus sent an error with no error name"));
    }
    if kind != ERROR && kind != METHOD_RETURN {
        // Not a reply at all. `read_reply` skips it by its absent serial.
        return Ok(Reply {
            reply_serial: None,
            error: None,
            sender,
            signature,
            body,
            little,
        });
    }
    Ok(Reply {
        reply_serial,
        error,
        sender,
        signature,
        body,
        little,
    })
}

/// What came back.
#[derive(Debug)]
struct Reply {
    reply_serial: Option<u32>,
    error: Option<String>,
    /// Who the BROKER says sent this, which is not who the sender says.
    ///
    /// td-busd rebuilds this field rather than relaying it: a peer's own
    /// `SENDER` is discarded and replaced with that connection's unique name.
    /// So it is the one field on an incoming message that a peer cannot
    /// choose, which is what makes it usable as an authentication check.
    sender: Option<String>,
    signature: String,
    body: Vec<u8>,
    little: bool,
}

impl Reply {
    /// The single string this reply carries.
    fn one_string(&self) -> io::Result<String> {
        if self.signature != "s" {
            return Err(io::Error::other(format!(
                "expected one string back and the body is {:?}",
                self.signature
            )));
        }
        let mut decoder = Decoder {
            bytes: &self.body,
            at: 0,
            little: self.little,
        };
        let text = decoder.string()?;
        // The signature says one string and the body has to BE one string.
        // Checking the signature alone accepts a reply with the token followed
        // by anything at all, which is a body this module has not understood
        // being read as though it had.
        if decoder.at != self.body.len() {
            return Err(io::Error::other(format!(
                "the bus sent {} byte(s) after the string it announced",
                self.body.len().saturating_sub(decoder.at)
            )));
        }
        Ok(text)
    }

    /// Nothing, which is what a successful `Complete` answers.
    fn no_body(&self) -> io::Result<()> {
        // Both halves, because they are independently forgeable: a reply
        // with no SIGNATURE field and a non-empty body passes a signature
        // check on its own.
        if !self.body.is_empty() || !self.signature.is_empty() {
            return Err(io::Error::other(format!(
                "expected an empty body and the reply carries {} byte(s) under {:?}",
                self.body.len(),
                self.signature
            )));
        }
        Ok(())
    }
}

/// A message this launcher writes.
#[derive(Default)]
struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    fn align(&mut self, to: usize) {
        while !self.bytes.len().is_multiple_of(to) {
            self.bytes.push(0);
        }
    }

    fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.align(4);
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, value: &str) -> io::Result<()> {
        let length = u32::try_from(value.len())
            .map_err(|_| io::Error::other("a bus string is longer than the wire allows"))?;
        self.u32(length);
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.push(0);
        Ok(())
    }

    /// A signature, whose length is one byte rather than four.
    fn signature(&mut self, value: &str) -> io::Result<()> {
        let length = u8::try_from(value.len())
            .map_err(|_| io::Error::other("a signature is longer than the wire allows"))?;
        self.byte(length);
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.push(0);
        Ok(())
    }

    /// `as`, whose length is the byte count of its contents.
    ///
    /// The count is patched in afterwards rather than computed ahead, because
    /// what it measures is the encoded bytes and the encoding is what produces
    /// them. It excludes the length word itself and any padding before the
    /// first element — of which there is none here, since a string aligns to
    /// four and the length word just left the cursor there.
    fn array_of_strings(&mut self, values: &[String]) -> io::Result<()> {
        self.align(4);
        let slot = self.bytes.len();
        self.bytes.extend_from_slice(&0u32.to_le_bytes());
        let start = self.bytes.len();
        for value in values {
            self.string(value)?;
        }
        let length = u32::try_from(self.bytes.len().saturating_sub(start))
            .map_err(|_| io::Error::other("a bus array is longer than the wire allows"))?;
        // Unreachable by construction — `slot` was this vector's length four
        // bytes ago — and spelled as a check for the same reason the header
        // slices above are.
        let Some(target) = self.bytes.get_mut(slot..slot.saturating_add(4)) else {
            return Err(io::Error::other("the array length slot went missing"));
        };
        target.copy_from_slice(&length.to_le_bytes());
        Ok(())
    }

    /// One `(yv)` header field carrying a string-shaped value.
    fn field(&mut self, code: u8, kind: &str, value: &str) -> io::Result<()> {
        self.align(8);
        self.byte(code);
        self.signature(kind)?;
        match kind {
            "g" => self.signature(value),
            _ => self.string(value),
        }
    }
}

/// The first thing this client says on the socket.
///
/// EXTERNAL authenticates with the credentials the kernel already attached to
/// the socket, so the identity is not a secret and not a challenge — it is the
/// uid the client CLAIMS, which the broker checks against `SO_PEERCRED` and
/// refuses if it disagrees. It is sent as the uid's decimal TEXT, hex-encoded:
/// uid 1000 is "1000" is "31303030". Sending the number itself authenticates
/// as some other uid or as nobody, and nothing about the bytes says which.
///
/// The leading NUL is not part of AUTH. It is the byte the specification
/// requires before any authentication traffic, and on a Unix socket it is also
/// what some implementations attach credentials to.
///
/// A free function so a test can hold the line up against a recorded one.
fn auth_line(uid: u32) -> String {
    let identity: String = uid
        .to_string()
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("\0AUTH EXTERNAL {identity}\r\n")
}

/// One METHOD_CALL to the broker, encoded.
fn method_call(
    serial: u32,
    path: &str,
    interface: &str,
    member: &str,
    signature: &str,
    body: &[u8],
) -> io::Result<Vec<u8>> {
    // The field array's first element begins at offset 16, which is already
    // eight-aligned, so encoding it from zero and appending gives the same
    // padding the specification asks for.
    let mut fields = Encoder::default();
    fields.field(FIELD_PATH, "o", path)?;
    fields.field(FIELD_INTERFACE, "s", interface)?;
    fields.field(FIELD_MEMBER, "s", member)?;
    fields.field(FIELD_DESTINATION, "s", BUS_NAME)?;
    if !signature.is_empty() {
        fields.field(FIELD_SIGNATURE, "g", signature)?;
    }

    let mut out = Encoder::default();
    out.byte(LITTLE_ENDIAN);
    out.byte(METHOD_CALL);
    out.byte(0);
    out.byte(PROTOCOL_VERSION);
    out.u32(
        u32::try_from(body.len())
            .map_err(|_| io::Error::other("a bus body is longer than the wire allows"))?,
    );
    out.u32(serial);
    out.u32(
        u32::try_from(fields.bytes.len())
            .map_err(|_| io::Error::other("a bus header is longer than the wire allows"))?,
    );
    out.bytes.extend_from_slice(&fields.bytes);
    out.align(8);
    out.bytes.extend_from_slice(body);
    Ok(out.bytes)
}

/// A message this launcher reads. Every accessor is bounds-checked: a reply
/// that does not fit its own announced lengths is a failed launch, not a
/// panic.
struct Decoder<'a> {
    bytes: &'a [u8],
    at: usize,
    little: bool,
}

impl Decoder<'_> {
    fn align(&mut self, to: usize) -> io::Result<()> {
        while !self.at.is_multiple_of(to) {
            match self.bytes.get(self.at) {
                Some(0) => self.at = self.at.saturating_add(1),
                Some(_) => return Err(io::Error::other("the bus sent nonzero padding")),
                None => return Err(io::Error::other("the bus message ended inside its padding")),
            }
        }
        Ok(())
    }

    fn byte(&mut self) -> io::Result<u8> {
        let value = self
            .bytes
            .get(self.at)
            .copied()
            .ok_or_else(|| io::Error::other("the bus message ended early"))?;
        self.at = self.at.saturating_add(1);
        Ok(value)
    }

    fn u32(&mut self) -> io::Result<u32> {
        self.align(4)?;
        let value = u32_at(self.bytes, self.at, self.little)?;
        self.at = self.at.saturating_add(4);
        Ok(value)
    }

    fn string(&mut self) -> io::Result<String> {
        let length = self.u32()? as usize;
        self.take(length)
    }

    fn signature(&mut self) -> io::Result<String> {
        let length = self.byte()? as usize;
        self.take(length)
    }

    /// `length` bytes of text, then the NUL that follows every one of them.
    fn take(&mut self, length: usize) -> io::Result<String> {
        let end = self
            .at
            .checked_add(length)
            .ok_or_else(|| io::Error::other("the bus announced an impossible string"))?;
        let text = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| io::Error::other("the bus announced more text than it sent"))?;
        let text = String::from_utf8(text.to_vec())
            .map_err(|e| io::Error::other(format!("the bus sent text that is not UTF-8: {e}")))?;
        if self.bytes.get(end).copied() != Some(0) {
            return Err(io::Error::other("the bus sent text with no terminator"));
        }
        self.at = end.saturating_add(1);
        Ok(text)
    }
}

fn u32_at(bytes: &[u8], at: usize, little: bool) -> io::Result<u32> {
    let end = at
        .checked_add(4)
        .ok_or_else(|| io::Error::other("the bus announced an impossible offset"))?;
    let slice = bytes
        .get(at..end)
        .ok_or_else(|| io::Error::other("the bus message ended inside a number"))?;
    let mut word = [0u8; 4];
    word.copy_from_slice(slice);
    Ok(if little {
        u32::from_le_bytes(word)
    } else {
        u32::from_be_bytes(word)
    })
}


#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::unwrap_used)]

    use super::*;

    /// One conversation a real client and the reference `dbus-daemon` actually
    /// had, from td-busd's interop corpus; `td-busd/spec/README` carries the
    /// provenance and how it was recorded.
    ///
    /// Reached across the crate boundary the way `permissions` already is, and
    /// under `#[cfg(test)]` so the target build never expands it — the recipe
    /// stages `src/*.rs` and no `spec/`, which is the same trade td-busd's own
    /// `recorded.rs` documents.
    ///
    /// This is the point of the file: a hand-laid fixture proves this module
    /// agrees with the specification as its author read it, and a misreading
    /// passes that too. These bytes were produced by neither.
    const LISTNAMES: &str = include_str!("../../td-busd/spec/libdbus-listnames.conversation");

    /// This module's own text, for the one check that is about what is NOT
    /// here.
    const BUS_SOURCE: &str = include_str!("bus.rs");

    /// Every frame one side of a recorded conversation sent, in order.
    fn frames(conversation: &str, side: char) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for line in conversation.lines() {
            let Some(hex) = line
                .strip_prefix(side)
                .and_then(|rest| rest.strip_prefix(' '))
            else {
                continue;
            };
            let digits: Vec<char> = hex.trim().chars().collect();
            let mut bytes = Vec::with_capacity(digits.len() / 2);
            for pair in digits.chunks(2) {
                let text: String = pair.iter().collect();
                bytes.push(u8::from_str_radix(&text, 16).unwrap());
            }
            out.push(bytes);
        }
        out
    }


    /// A reply frame a broker would send, built with this module's own encoder.
    ///
    /// Only usable to check the CLIENT's side of an exchange, since a bug
    /// shared by both would cancel out. What it is for is the plumbing the
    /// recordings cannot reach: connect, handshake, `Hello`, serial
    /// allocation, and the two public functions end to end.
    fn reply_frame(kind: u8, reply_serial: u32, error: Option<&str>, text: Option<&str>) -> Vec<u8> {
        reply_frame_from(BUS_NAME, kind, reply_serial, error, text)
    }

    /// The same, from whoever the broker says sent it.
    ///
    /// A parameter because the interesting case is a reply that is not the
    /// broker's, and because a helper that could only produce well-formed
    /// broker replies would make the sender check untestable.
    fn reply_frame_from(
        sender: &str,
        kind: u8,
        reply_serial: u32,
        error: Option<&str>,
        text: Option<&str>,
    ) -> Vec<u8> {
        let mut body = Encoder::default();
        if let Some(text) = text {
            body.string(text).unwrap();
        }
        let mut fields = Encoder::default();
        fields.align(8);
        fields.byte(FIELD_REPLY_SERIAL);
        fields.signature("u").unwrap();
        fields.u32(reply_serial);
        fields.field(FIELD_SENDER, "s", sender).unwrap();
        if let Some(name) = error {
            fields.field(FIELD_ERROR_NAME, "s", name).unwrap();
        }
        if text.is_some() {
            fields.field(FIELD_SIGNATURE, "g", "s").unwrap();
        }
        let mut frame = Encoder::default();
        frame.byte(LITTLE_ENDIAN);
        frame.byte(kind);
        frame.byte(0);
        frame.byte(PROTOCOL_VERSION);
        frame.u32(u32::try_from(body.bytes.len()).unwrap());
        frame.u32(7);
        frame.u32(u32::try_from(fields.bytes.len()).unwrap());
        frame.bytes.extend_from_slice(&fields.bytes);
        frame.align(8);
        frame.bytes.extend_from_slice(&body.bytes);
        frame.bytes
    }

    /// A listener that plays broker for exactly one connection.
    ///
    /// It answers the handshake, then hands back every method call it read and
    /// sends whatever the test told it to send. Real socket, real `connect`,
    /// real timeouts — the parts of this module the recorded conversations
    /// cannot exercise, because a recording is bytes and this is plumbing.
    /// A directory no other run of this test binary will pick.
    ///
    /// pid and a counter are NOT enough: pids are recycled, the counter
    /// restarts with the process, and a socket left behind by an earlier run
    /// then makes `bind` fail with EADDRINUSE. A reviewer hit that once in
    /// three thousand runs and found thirteen leftover sockets on the machine
    /// to explain how. The clock is what makes it unique across runs.
    fn scratch() -> std::path::PathBuf {
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "td-jail-bus-test-{}-{}-{since}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_broker(
        replies: Vec<Vec<u8>>,
    ) -> (std::path::PathBuf, std::thread::JoinHandle<Vec<Vec<u8>>>, std::path::PathBuf) {
        use std::os::unix::net::UnixListener;
        let dir = scratch();
        let socket = dir.join("bus");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            let mut received = Vec::new();
            let Ok((mut peer, _)) = listener.accept() else {
                return received;
            };
            // The NUL and the AUTH line, read to its CRLF.
            let mut line = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                if peer.read_exact(&mut byte).is_err() {
                    return received;
                }
                line.push(byte[0]);
                if line.ends_with(b"\r\n") {
                    break;
                }
            }
            received.push(line);
            let _ = peer.write_all(b"OK 0123456789abcdef\r\n");
            // BEGIN, whose CRLF the client sends before the first frame.
            let mut begin = [0u8; 7];
            if peer.read_exact(&mut begin).is_err() {
                return received;
            }
            received.push(begin.to_vec());
            for reply in replies {
                match read_message(&mut peer) {
                    Ok(call) => received.push(call.body),
                    Err(_) => return received,
                }
                if peer.write_all(&reply).is_err() {
                    return received;
                }
            }
            received
        });
        (socket, handle, dir)
    }

    static NEXT_SOCKET: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// `register` end to end: connect, handshake, `Hello`, `Register`, token.
    #[test]
    fn a_registration_runs_from_connect_to_token() {
        let hello = reply_frame(METHOD_RETURN, 1, None, Some(":1.9"));
        let token = reply_frame(METHOD_RETURN, 2, None, Some("0f1e2d3c4b5a69788796a5b4c3d2e1f0"));
        let (socket, broker, dir) = fake_broker(vec![hello, token]);

        let got = register(&socket, 1000, "firefox-0011223344556677", "firefox", &[
            "org.mozilla.firefox".to_string(),
        ]);
        let received = broker.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(got.unwrap(), "0f1e2d3c4b5a69788796a5b4c3d2e1f0");
        // The AUTH line carried the uid this call claimed, and BEGIN followed.
        assert_eq!(received.first().unwrap(), auth_line(1000).as_bytes());
        assert_eq!(received.get(1).unwrap(), b"BEGIN\r\n");
        // Hello's body is empty; Register's is the three arguments.
        assert!(received.get(2).unwrap().is_empty());
        let mut body = Encoder::default();
        body.string("firefox-0011223344556677").unwrap();
        body.string("firefox").unwrap();
        body.array_of_strings(&["org.mozilla.firefox".to_string()])
            .unwrap();
        assert_eq!(received.get(3).unwrap(), &body.bytes);
    }

    /// `complete` end to end, and its reply carries no body.
    #[test]
    fn a_completion_runs_from_connect_to_an_empty_reply() {
        let hello = reply_frame(METHOD_RETURN, 1, None, Some(":1.9"));
        let done = reply_frame(METHOD_RETURN, 2, None, None);
        let (socket, broker, dir) = fake_broker(vec![hello, done]);

        let got = complete(&socket, 1000, "0f1e2d3c4b5a6978", 4242);
        let received = broker.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        got.unwrap();
        let mut body = Encoder::default();
        body.string("0f1e2d3c4b5a6978").unwrap();
        body.u32(4242);
        assert_eq!(received.get(3).unwrap(), &body.bytes);
    }

    /// A broker that refuses the registration fails the launch, with its own
    /// error name in the diagnostic rather than a bare "it did not work".
    #[test]
    fn a_refused_registration_carries_the_brokers_reason() {
        let hello = reply_frame(METHOD_RETURN, 1, None, Some(":1.9"));
        let refusal = reply_frame(
            ERROR,
            2,
            Some("td.Jail1.Error.Refused"),
            Some("instance \"firefox-1\" is already registered"),
        );
        let (socket, broker, dir) = fake_broker(vec![hello, refusal]);

        let got = register(&socket, 1000, "firefox-1", "firefox", &[]);
        let _ = broker.join();
        let _ = std::fs::remove_dir_all(&dir);

        let error = got.unwrap_err().to_string();
        assert!(error.contains("td.Jail1.Error.Refused"), "{error}");
        assert!(error.contains("already registered"), "{error}");
    }

    /// A broker that refuses the IDENTITY fails before any call is made.
    ///
    /// The handshake is where a uid mismatch surfaces, and the message has to
    /// say that rather than blaming whatever call came next.
    #[test]
    fn a_refused_identity_fails_the_handshake() {
        use std::os::unix::net::UnixListener;
        let dir = scratch();
        let socket = dir.join("bus");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((mut peer, _)) = listener.accept() {
                let mut line = Vec::new();
                loop {
                    let mut byte = [0u8; 1];
                    if peer.read_exact(&mut byte).is_err() {
                        return;
                    }
                    line.push(byte[0]);
                    if line.ends_with(b"\r\n") {
                        break;
                    }
                }
                let _ = peer.write_all(b"REJECTED EXTERNAL\r\n");
            }
        });

        let error = register(&socket, 1000, "firefox-1", "firefox", &[]).unwrap_err();
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&dir);
        let text = error.to_string();
        assert!(text.contains("refused this identity"), "{text}");
        assert!(text.contains("REJECTED"), "{text}");
    }

    /// A socket that is not there is a named failure, not a hang.
    #[test]
    fn an_absent_broker_is_named() {
        let missing = std::env::temp_dir().join("td-jail-bus-test-nothing-is-bound-here/bus");
        let error = register(&missing, 1000, "firefox-1", "firefox", &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("connect to the session bus"), "{error}");
    }
    /// A broker that accepts and then says nothing does not hang the launch.
    ///
    /// This is the claim the module doc makes at its head — a launch that
    /// hangs is worse than one that fails, because the caller is a supervised
    /// unit with its own deadline and a jail waiting on a wedged broker holds
    /// an application slot and reports nothing. Run against a real socket with
    /// a short deadline, so what is checked is that the timeout is APPLIED and
    /// not that the constant is spelled correctly.
    #[test]
    fn a_broker_that_never_answers_does_not_hang_the_launch() {
        use std::os::unix::net::UnixListener;
        let dir = scratch();
        let socket = dir.join("bus");
        let listener = UnixListener::bind(&socket).unwrap();
        // Accept and hold it open for far longer than this test will wait.
        // Holding matters: dropping the peer would CLOSE the connection, and
        // an EOF ends the read whether or not a deadline was ever set — which
        // is how a first draft of this test passed with both timeouts deleted.
        // Never joined, so the healthy case finishes as soon as the client
        // gives up rather than waiting this thread out.
        std::thread::spawn(move || {
            let peer = listener.accept();
            std::thread::sleep(Duration::from_secs(30));
            drop(peer);
        });

        let began = std::time::Instant::now();
        let outcome = Connection::open_within(&socket, 1000, Duration::from_millis(300));
        let waited = began.elapsed();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(outcome.is_err(), "a silent broker was treated as an answer");
        assert!(
            waited < Duration::from_secs(3),
            "the client waited {waited:?} on a broker that said nothing, so the \
             deadline it was given is not being applied"
        );
    }

    /// A reply whose body is not the shape the call asked for is refused.
    ///
    /// `Register` answers one string. A broker that answered a number, or
    /// nothing, would otherwise have its reply read as a token — and a token
    /// is what phase two authenticates with, so a wrong one fails later and
    /// somewhere else.
    #[test]
    fn a_reply_of_the_wrong_shape_is_refused() {
        let hello = reply_frame(METHOD_RETURN, 1, None, Some(":1.9"));
        let empty = reply_frame(METHOD_RETURN, 2, None, None);
        let (socket, broker, dir) = fake_broker(vec![hello, empty]);
        let error = register(&socket, 1000, "firefox-1", "firefox", &[]).unwrap_err();
        let _ = broker.join();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            error.to_string().contains("expected one string back"),
            "{error}"
        );
    }

    /// And `Complete`, which answers nothing, refuses a reply that carries
    /// something. A broker answering a different call's shape is a broker this
    /// client has misunderstood, and proceeding would bind the jail to
    /// whatever it happened to mean.
    #[test]
    fn a_completion_reply_that_carries_a_body_is_refused() {
        let hello = reply_frame(METHOD_RETURN, 1, None, Some(":1.9"));
        let chatty = reply_frame(METHOD_RETURN, 2, None, Some("sure"));
        let (socket, broker, dir) = fake_broker(vec![hello, chatty]);
        let error = complete(&socket, 1000, "0f1e2d3c", 4242).unwrap_err();
        let _ = broker.join();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            error.to_string().contains("expected an empty body"),
            "{error}"
        );
    }

    /// A reply in the other byte order decodes, because the header says so.
    ///
    /// Hand-laid, and nothing in the recorded corpus reaches it: every daemon
    /// on a little-endian host sends `l`. The specification lets a peer choose,
    /// this module reads the byte and branches on it, and a branch nothing
    /// exercises is a branch nobody knows the state of. Laid out big-endian
    /// throughout — that is the whole point — with each field's offset named.
    #[test]
    fn a_reply_in_the_other_byte_order_decodes() {
        let frame: Vec<u8> = vec![
            b'B', METHOD_RETURN, 0, PROTOCOL_VERSION, // 0..4
            0, 0, 0, 7,  // body length, big-endian
            0, 0, 0, 1,  // serial
            0, 0, 0, 15, // field array length
            // REPLY_SERIAL: code, signature "u", then the value four-aligned.
            FIELD_REPLY_SERIAL, 1, b'u', 0, // 16..20
            0, 0, 0, 1,                     // 20..24
            // SIGNATURE: code, signature "g", then the signature "s". The
            // field array ends at 31, which is 15 bytes of tail.
            FIELD_SIGNATURE, 1, b'g', 0, 1, b's', 0, // 24..31
            0, // 31: one byte of padding takes the body to 32, eight-aligned
            // The body: one string, whose length is big-endian too.
            0, 0, 0, 2, b'h', b'i', 0,
        ];
        let reply = read_message(&mut frame.as_slice()).unwrap();
        assert!(!reply.little, "the endianness byte was not read");
        assert_eq!(reply.reply_serial, Some(1));
        assert_eq!(reply.signature, "s");
        assert_eq!(reply.one_string().unwrap(), "hi");
    }

    /// A reply this module cannot interpret is refused rather than guessed at.
    ///
    /// All three of these are the broker contradicting the framing everything
    /// after them depends on. Reading past any of them is reading a message
    /// this module has already established it does not understand.
    #[test]
    fn a_reply_that_contradicts_its_own_framing_is_refused() {
        let good = reply_frame(METHOD_RETURN, 1, None, Some("hi"));

        let mut wrong_order = good.clone();
        wrong_order[0] = b'?';
        let error = read_message(&mut wrong_order.as_slice()).unwrap_err();
        assert!(error.to_string().contains("endianness"), "{error}");

        let mut wrong_version = good.clone();
        wrong_version[3] = PROTOCOL_VERSION.wrapping_add(1);
        let error = read_message(&mut wrong_version.as_slice()).unwrap_err();
        assert!(error.to_string().contains("protocol version"), "{error}");

        // And a string whose declared length runs to the byte before its NUL,
        // so the terminator this module requires is not there. Shortening the
        // length rather than lengthening it keeps the frame inside itself, so
        // what refuses it is the terminator check and not a bounds check.
        let mut unterminated = good.clone();
        let body_at = unterminated.len() - 7;
        unterminated[body_at] = 1;
        let reply = read_message(&mut unterminated.as_slice()).unwrap();
        let error = reply.one_string().unwrap_err();
        assert!(error.to_string().contains("no terminator"), "{error}");
    }

    /// A reply from a peer that is not the broker is not this call's reply.
    ///
    /// The attack this closes: every peer on the session bus is uid 1000,
    /// routing to a unique name is directed, and a fresh connection's serials
    /// are 1 then 2. So a hostile peer can address a `METHOD_RETURN` to this
    /// connection carrying serial 2 and have the broker deliver it.
    ///
    /// The prize is not a forged token — a token nobody issued fails at the
    /// next step. It is a forged `Complete` SUCCESS: stage 1 writes the proof,
    /// releases a jail whose real registration is still pending, that pending
    /// registration expires, and a genuinely confined application resolves
    /// `Unconfined`. §D concedes a rogue process can register a false instance
    /// for ITSELF and concedes nothing about degrading somebody else's jail.
    ///
    /// Here the forgery arrives FIRST and the broker's own reply behind it,
    /// which is the ordering an attacker would arrange. The genuine reply must
    /// still be the one returned.
    #[test]
    fn a_reply_from_another_peer_is_not_this_calls_reply() {
        let hello = reply_frame(METHOD_RETURN, 1, None, Some(":1.9"));
        let forged = reply_frame_from(":1.4", METHOD_RETURN, 2, None, None);
        let genuine = reply_frame(METHOD_RETURN, 2, None, None);
        let mut queued = forged.clone();
        queued.extend_from_slice(&genuine);
        let (socket, broker, dir) = fake_broker(vec![hello, queued]);

        let outcome = complete(&socket, 1000, "0f1e2d3c", 4242);
        let _ = broker.join();
        let _ = std::fs::remove_dir_all(&dir);
        outcome.expect("the broker's own reply was behind the forgery");

        // And with nothing behind it, the forgery is not an answer at all.
        let hello = reply_frame(METHOD_RETURN, 1, None, Some(":1.9"));
        let alone = reply_frame_from(":1.4", METHOD_RETURN, 2, None, None);
        let (socket, broker, dir) = fake_broker(vec![hello, alone]);
        let outcome = complete(&socket, 1000, "0f1e2d3c", 4242);
        let _ = broker.join();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            outcome.is_err(),
            "a peer answered for the broker and the jail was released"
        );
    }

    /// A reply carrying more than it declared is refused.
    ///
    /// Both directions are separately forgeable and neither is caught by
    /// checking the signature alone: `Register` answering a token followed by
    /// trailing bytes, and `Complete` answering with no SIGNATURE field and a
    /// body anyway.
    #[test]
    fn a_reply_carrying_more_than_it_declared_is_refused() {
        let mut padded = reply_frame(METHOD_RETURN, 2, None, Some("0f1e2d3c"));
        // One more byte of body, and the declared body length to match.
        let was = u32::from_le_bytes([padded[4], padded[5], padded[6], padded[7]]);
        let now = was.saturating_add(1).to_le_bytes();
        padded[4..8].copy_from_slice(&now);
        padded.push(0);
        let hello = reply_frame(METHOD_RETURN, 1, None, Some(":1.9"));
        let (socket, broker, dir) = fake_broker(vec![hello, padded]);
        let error = register(&socket, 1000, "firefox-1", "firefox", &[]).unwrap_err();
        let _ = broker.join();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            error.to_string().contains("after the string it announced"),
            "{error}"
        );

        // A Complete reply with no signature and a body behind it.
        let mut bodied = reply_frame(METHOD_RETURN, 2, None, None);
        bodied[4..8].copy_from_slice(&1u32.to_le_bytes());
        bodied.push(0);
        let hello = reply_frame(METHOD_RETURN, 1, None, Some(":1.9"));
        let (socket, broker, dir) = fake_broker(vec![hello, bodied]);
        let error = complete(&socket, 1000, "0f1e2d3c", 4242).unwrap_err();
        let _ = broker.join();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(error.to_string().contains("expected an empty body"), "{error}");
    }

    /// Work that does not finish inside its budget is given up on.
    ///
    /// This is what stands behind `connect(2)`, which `std` cannot bound on a
    /// Unix socket and which blocks until the peer accepts. A listener that
    /// binds `/run/user/1000/bus`, fills its backlog and never accepts would
    /// otherwise hold stage 1 for ever — and because stage 1 never EXITS, the
    /// fixture's `restart=always` would have nothing to restart. A draft
    /// installed the socket timeouts after connecting, which bounded every
    /// call except that one.
    #[test]
    fn work_that_overruns_its_budget_is_given_up_on() {
        let began = Instant::now();
        let answer = within(Duration::from_millis(200), || {
            std::thread::sleep(Duration::from_secs(30));
            "eventually"
        });
        let waited = began.elapsed();
        assert!(answer.is_none(), "a thirty-second answer arrived in time");
        assert!(
            waited < Duration::from_secs(3),
            "the caller waited {waited:?} for a budget of 200ms"
        );
        // And work that DOES finish is not thrown away, which is the half a
        // deadline that always fires would still pass.
        assert_eq!(within(Duration::from_secs(30), || "promptly"), Some("promptly"));
    }

    /// And the connect is the work that budget is spent on.
    ///
    /// `within` is tested directly above, which leaves one join un-pinned: that
    /// `connect_within` actually routes the connect through it. A mutation
    /// that called `UnixStream::connect` inline survived the whole suite —
    /// every deadline test still passed, because they test the deadline and
    /// not its application to the one call that needs it.
    ///
    /// Pinned in the source because the behavioural alternative was a test
    /// that fills a listener's backlog, which needs more than a hundred live
    /// sockets in a shared test binary and made `sys`'s close/EBADF test start
    /// failing on descriptor reuse. Same tier as the launch's ordering test,
    /// and for the same reason: the property is structural and no type carries
    /// it.
    /// Two properties of this module that no test can observe, pinned in its
    /// source instead.
    ///
    /// Neither is fastidiousness and both were caught by a mutation surviving.
    ///
    /// The JOIN: `unshare(CLONE_NEWUSER)` is `EINVAL` for a multithreaded
    /// process, and `launch_application` unshares a few statements after phase
    /// one returns. A `within` that returned before its helper had exited
    /// reproduced that failure in 194 of 200 runs with nothing in between —
    /// and 0 of 110 on the real path, where the round trips close the window.
    /// A test cannot see the difference: this binary is multithreaded by the
    /// harness, so counting threads proves nothing, and racing the helper's
    /// exit is the very nondeterminism at issue.
    ///
    /// The WRITE deadline: this module writes about two hundred bytes, which
    /// fit in the socket buffer, so a write never blocks and a missing
    /// deadline is unobservable — until the day something here writes more.
    #[test]
    fn the_deadline_and_the_join_are_where_they_have_to_be() {
        let shipped = BUS_SOURCE
            .split_once("#[cfg(test)]")
            .unwrap_or((BUS_SOURCE, ""))
            .0;
        assert!(
            shipped.contains("let _ = helper.join();"),
            "`within` no longer waits for its helper to exit, so a launch can \
             reach unshare(CLONE_NEWUSER) with two threads and fail with EINVAL"
        );
        assert!(
            shipped.contains("set_write_timeout(Some(self.left()?))"),
            "writes are no longer on the exchange's deadline"
        );
        // And no write goes around the adapter.
        assert_eq!(
            shipped.matches("self.stream.write_all(").count(),
            0,
            "a write bypasses the deadline adapter"
        );
    }

    #[test]
    fn the_connect_runs_under_the_budget() {
        let shipped = BUS_SOURCE
            .split_once("#[cfg(test)]")
            .unwrap_or((BUS_SOURCE, ""))
            .0;
        assert!(
            shipped.contains("within(budget, move || UnixStream::connect("),
            "connect_within no longer runs the connect under a deadline, so a \
             listener that never accepts holds the launch for ever"
        );
        // And nothing else reaches the raw connect: the one inside `within`
        // above is the only mention in the shipped half of this file.
        assert_eq!(
            shipped.matches("UnixStream::connect(").count(),
            1,
            "a second, unbounded connect appeared"
        );
    }

    /// A handshake line without an end is not read for ever.
    ///
    /// The peer chooses when to send a newline, so `read_line` is one of the
    /// two places a hostile listener can spend an unbounded amount of this
    /// process's memory; the reply ceiling is the other. The commit that added
    /// the ceiling claimed this one was tested and it was not.
    ///
    /// The overrun is refused before the line is complete, which is the
    /// property: waiting for the newline to find out how long the line was is
    /// the bug.
    #[test]
    fn a_handshake_line_without_an_end_is_refused() {
        use std::os::unix::net::UnixListener;
        let dir = scratch();
        let socket = dir.join("bus");
        let listener = UnixListener::bind(&socket).unwrap();
        let shouting = std::thread::spawn(move || {
            if let Ok((mut peer, _)) = listener.accept() {
                let mut line = Vec::new();
                loop {
                    let mut byte = [0u8; 1];
                    if peer.read_exact(&mut byte).is_err() {
                        return;
                    }
                    line.push(byte[0]);
                    if line.ends_with(b"\r\n") {
                        break;
                    }
                }
                // An "OK" that never ends.
                let _ = peer.write_all(&vec![b'O'; 64 * 1024]);
            }
        });

        let error = match Connection::open_within(&socket, 1000, Duration::from_secs(5)) {
            Ok(_) => panic!("an endless handshake line was accepted"),
            Err(error) => error,
        };
        let _ = shouting.join();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            error.to_string().contains("oversized handshake line"),
            "{error}"
        );
    }

    /// The header this module writes is the header libdbus writes.
    ///
    /// `ListNames` is the comparable one: dbus-send emits its fields in the
    /// order PATH, INTERFACE, MEMBER, DESTINATION, which is the order here. Its
    /// `Hello` in the same recording uses a different order — field order is
    /// the sender's choice, so matching one recording exactly is a real check
    /// of every rule around the order rather than of the order itself.
    ///
    /// What this pins: the endianness byte, the type and flag bytes, the
    /// protocol version, the body length, the serial, the field-array length,
    /// the eight-byte alignment before each field, the four-byte alignment and
    /// NUL terminator of every string, the one-byte length of a signature, and
    /// the absence of a SIGNATURE field on an empty body.
    #[test]
    fn a_method_call_is_encoded_the_way_a_real_client_encodes_it() {
        let client = frames(LISTNAMES, 'C');
        // 0 and 1 are the text handshake; 2 is BEGIN with Hello behind it; 3
        // is the ListNames call, which the recording's note names.
        let recorded = client.get(3).expect("the recording holds a ListNames call");
        let mine = method_call(
            2,
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "ListNames",
            "",
            &[],
        )
        .unwrap();
        assert_eq!(
            mine, *recorded,
            "this module's ListNames call differs from libdbus 1.15.8's"
        );
    }

    /// And the reply a real daemon sent decodes.
    #[test]
    fn a_recorded_reply_decodes() {
        let server = frames(LISTNAMES, 'S');
        // 0 and 1 are the handshake; 2 is the Hello reply with a NameAcquired
        // signal behind it; 3 is the ListNames reply.
        let hello = server.get(2).expect("the recording holds a Hello reply");
        let mut stream = hello.as_slice();
        let reply = read_message(&mut stream).unwrap();
        assert_eq!(reply.reply_serial, Some(1));
        assert_eq!(reply.signature, "s");
        assert_eq!(reply.one_string().unwrap(), ":1.7");
        // The daemon put a NameAcquired SIGNAL in the same write. It is not a
        // reply to anything, and the reader must say so rather than mistake it
        // for one.
        let signal = read_message(&mut stream).unwrap();
        assert_eq!(signal.reply_serial, None);
    }

    /// A message that is not the reply is skipped, and the reply behind it is
    /// still found.
    ///
    /// Three legs, because the first two on their own are satisfied by a
    /// `read_reply` that simply always fails — which is why an earlier version
    /// of this test, which had only the recorded leg, proved almost nothing.
    ///
    /// The recorded leg first: asking for a serial the recording never answers
    /// consumes both of its messages — a METHOD_RETURN for serial 1 and a
    /// NameAcquired signal — and runs out of stream. Running out is the proof
    /// that neither was taken for the reply.
    #[test]
    fn an_unrelated_message_is_skipped_and_the_reply_behind_it_is_found() {
        let server = frames(LISTNAMES, 'S');
        let hello = server.get(2).expect("the recording holds a Hello reply");
        let error = read_reply(&mut hello.as_slice(), 99, "Nothing").unwrap_err();
        assert!(
            !error.to_string().contains("1.7"),
            "another serial's reply reached the caller: {error}"
        );

        // Skipping has to FIND the reply, not just refuse the wrong one. Eight
        // unrelated messages is the allowance exactly.
        let mut stream = Vec::new();
        for _ in 0..MAX_UNRELATED_MESSAGES {
            stream.extend_from_slice(&reply_frame(METHOD_RETURN, 999, None, Some("no")));
        }
        stream.extend_from_slice(&reply_frame(METHOD_RETURN, 2, None, Some("yes")));
        let reply = read_reply(&mut stream.as_slice(), 2, "Register").unwrap();
        assert_eq!(reply.one_string().unwrap(), "yes");

        // And one more than the allowance is where it stops, rather than
        // reading a hostile peer's stream until one of them gives up.
        let mut flood = Vec::new();
        for _ in 0..=MAX_UNRELATED_MESSAGES {
            flood.extend_from_slice(&reply_frame(METHOD_RETURN, 999, None, Some("no")));
        }
        flood.extend_from_slice(&reply_frame(METHOD_RETURN, 2, None, Some("yes")));
        let error = read_reply(&mut flood.as_slice(), 2, "Register").unwrap_err();
        assert!(error.to_string().contains("unrelated messages"), "{error}");
    }

    /// `as` is encoded the way the daemon encodes it.
    ///
    /// The ListNames reply's body is an array of two strings, and its bytes are
    /// the daemon's. Taken as the frame's tail rather than through this
    /// module's own reader, so the array check does not rest on the header
    /// parse it is meant to be independent of.
    #[test]
    fn an_array_of_strings_is_encoded_the_way_a_real_daemon_encodes_it() {
        let server = frames(LISTNAMES, 'S');
        let reply = server.get(3).expect("the recording holds a ListNames reply");
        // The recording's own header says the body is 0x29 bytes.
        let body = reply.get(reply.len() - 0x29..).unwrap();
        let mut mine = Encoder::default();
        mine.array_of_strings(&[
            "org.freedesktop.DBus".to_string(),
            ":1.7".to_string(),
        ])
        .unwrap();
        assert_eq!(
            mine.bytes, body,
            "this module's array encoding differs from dbus-daemon's"
        );
    }

    /// The two bodies this module actually sends.
    ///
    /// Hand-laid, and worth stating why that is enough here: strings, `u32`s
    /// and arrays are each pinned above against bytes this module did not
    /// write, so what is left to check is the ORDER and alignment of the
    /// fields within these two signatures.
    #[test]
    fn the_registration_bodies_are_laid_out_by_the_specification() {
        let mut register = Encoder::default();
        register.string("one").unwrap();
        register.string("fixture").unwrap();
        register.array_of_strings(&[]).unwrap();
        assert_eq!(
            register.bytes,
            [
                // "one": length 3, three bytes, NUL — offsets 0..8.
                3, 0, 0, 0, b'o', b'n', b'e', 0,
                // "fixture" starts four-aligned at 8 with nothing to pad.
                7, 0, 0, 0, b'f', b'i', b'x', b't', b'u', b'r', b'e', 0,
                // An empty array is its length and nothing else, at 20.
                0, 0, 0, 0,
            ],
            "Register's ssas body is not laid out as the specification says"
        );

        let mut complete = Encoder::default();
        complete.string("abc").unwrap();
        complete.u32(7);
        assert_eq!(
            complete.bytes,
            [3, 0, 0, 0, b'a', b'b', b'c', 0, 7, 0, 0, 0],
            "Complete's su body is not laid out as the specification says"
        );
    }

    /// A body long enough to need padding between its strings gets it.
    ///
    /// Every string in the fixtures above happens to land four-aligned on its
    /// own, so a missing `align` would have passed all of them.
    #[test]
    fn a_string_that_does_not_land_aligned_is_padded() {
        let mut body = Encoder::default();
        body.string("ab").unwrap();
        body.string("cd").unwrap();
        assert_eq!(
            body.bytes,
            [
                2, 0, 0, 0, b'a', b'b', 0, // ends at offset 7
                0, // one byte of padding to reach 8
                2, 0, 0, 0, b'c', b'd', 0,
            ],
            "a string that did not land four-aligned was not padded"
        );
    }

    /// An error reply is an error, and carries what the broker said.
    #[test]
    fn an_error_reply_is_reported_with_its_name() {
        // Built here rather than recorded: the corpus holds no refusal, and
        // what this checks is the plumbing from ERROR_NAME to the caller.
        let mut body = Encoder::default();
        body.string("instance \"one\" is already registered").unwrap();
        let mut fields = Encoder::default();
        fields.field(FIELD_ERROR_NAME, "s", "td.Jail1.Error.Refused").unwrap();
        fields.align(8);
        fields.byte(FIELD_REPLY_SERIAL);
        fields.signature("u").unwrap();
        fields.u32(4);
        fields.field(FIELD_SENDER, "s", BUS_NAME).unwrap();
        fields.field(FIELD_SIGNATURE, "g", "s").unwrap();

        let mut frame = Encoder::default();
        frame.byte(LITTLE_ENDIAN);
        frame.byte(ERROR);
        frame.byte(0);
        frame.byte(PROTOCOL_VERSION);
        frame.u32(u32::try_from(body.bytes.len()).unwrap());
        frame.u32(11);
        frame.u32(u32::try_from(fields.bytes.len()).unwrap());
        frame.bytes.extend_from_slice(&fields.bytes);
        frame.align(8);
        frame.bytes.extend_from_slice(&body.bytes);

        let error = read_reply(&mut frame.bytes.as_slice(), 4, "Register").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("td.Jail1.Error.Refused"), "{text}");
        assert!(text.contains("already registered"), "{text}");
    }

    /// A reply that does not fit its own announced lengths is refused rather
    /// than read past.
    ///
    /// The frame ARRIVES WHOLE and lies about its contents, which is the case
    /// worth testing: a first draft announced a field array longer than the
    /// bytes that followed, so `read_exact` hit EOF and the test passed
    /// without any of this module's bounds logic running. On a real socket
    /// that frame would block rather than fail. This one declares an
    /// eight-byte field array and puts a string inside it whose length runs
    /// past the array's own end, so the decoder is what has to refuse it.
    #[test]
    fn a_reply_that_overruns_its_own_lengths_is_refused() {
        let mut fields = Encoder::default();
        fields.byte(FIELD_ERROR_NAME);
        fields.signature("s").unwrap();
        fields.u32(0xffff); // a string longer than the whole message
        let mut frame = Encoder::default();
        frame.byte(LITTLE_ENDIAN);
        frame.byte(ERROR);
        frame.byte(0);
        frame.byte(PROTOCOL_VERSION);
        frame.u32(0);
        frame.u32(1);
        frame.u32(u32::try_from(fields.bytes.len()).unwrap());
        frame.bytes.extend_from_slice(&fields.bytes);
        frame.align(8);
        let error = read_message(&mut frame.bytes.as_slice()).unwrap_err();
        assert!(
            error.to_string().contains("more text than it sent"),
            "an overrunning field was not refused: {error}"
        );

        // And one that announces more than the ceiling allows.
        let mut huge = Encoder::default();
        huge.byte(LITTLE_ENDIAN);
        huge.byte(METHOD_RETURN);
        huge.byte(0);
        huge.byte(PROTOCOL_VERSION);
        huge.u32(u32::try_from(MAX_REPLY_BYTES).unwrap() + 1);
        huge.u32(1);
        huge.u32(0);
        let error = read_message(&mut huge.bytes.as_slice()).unwrap_err();
        assert!(error.to_string().contains("ceiling"), "{error}");
    }

    /// The AUTH line this module sends is the AUTH line libdbus sends.
    ///
    /// Compared whole rather than by its parts, because every part of it has
    /// its own way of being wrong: a missing leading NUL, a mechanism name
    /// that is not EXTERNAL, the uid as a number instead of as hex-encoded
    /// decimal text, a bare LF for the CRLF. The recording was made under uid
    /// 1001, so that is the uid built here.
    #[test]
    fn the_auth_line_is_the_one_a_real_client_sends() {
        let client = frames(LISTNAMES, 'C');
        let recorded = client.first().expect("the recording holds an AUTH line");
        assert_eq!(
            auth_line(1001).as_bytes(),
            recorded.as_slice(),
            "this module's AUTH line differs from libdbus 1.15.8's"
        );
        // And the hex is of the decimal TEXT, which the line above would also
        // satisfy if 1001 happened to encode to itself. It does not, but the
        // canonical example is worth stating once.
        assert!(auth_line(1000).contains("31303030"), "{:?}", auth_line(1000));
    }

    /// This client does not negotiate file-descriptor passing.
    ///
    /// libdbus sends NEGOTIATE_UNIX_FD between AUTH and BEGIN; this module
    /// goes straight to BEGIN. That is deliberate and not an omission: nothing
    /// this module sends or receives carries a descriptor, and a client that
    /// negotiated the capability without handling it would be claiming to
    /// accept something it would then drop. The check is here so the day fd
    /// passing does arrive, this reads as a decision rather than an oversight.
    #[test]
    fn the_handshake_claims_no_descriptor_passing() {
        let shipped = BUS_SOURCE.split_once("#[cfg(test)]").unwrap_or((BUS_SOURCE, "")).0;
        assert!(
            !shipped.contains("NEGOTIATE_UNIX_FD"),
            "this client now negotiates fd passing but still cannot carry one"
        );
        // The recorded client did negotiate, so the string is one a real
        // client sends and not one nobody would have written anyway.
        let client = frames(LISTNAMES, 'C');
        let negotiate = client.get(1).expect("the recording holds a NEGOTIATE line");
        assert_eq!(negotiate.as_slice(), b"NEGOTIATE_UNIX_FD\r\n");
    }
}
