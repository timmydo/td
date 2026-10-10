//! Receiving protocol state, without sockets, credentials or a storage adapter.
use crate::{
    config::{routing::Routing, values::certificate_name},
    format::row::{MAX_ADDRESS, MAX_RECEIPT_BYTES, MAX_RECIPIENTS},
    ids::AccountId,
    limits::MAX_MESSAGE_BYTES,
    ports::{Commit, CommitFailure, Error},
};
use std::{
    fmt::Write,
    net::{Ipv4Addr, Ipv6Addr},
};

// SIZE adds 26 command octets; BODY adds 16. DATA includes one transparency dot.
const COMMAND_BYTES: usize = 512 + 26 + 16;
const DATA_BYTES: usize = 1001;

/// Pinned routing and validated limits; the trusted driver supplies transport
/// identity separately to delivery. Neither this context nor EHLO authenticates.
pub struct Settings<'a> {
    pub hostname: &'a str,
    /// Stored-message ceiling, including generated Received and Return-Path.
    pub message_bytes: usize,
    /// Delivery adapter's nonzero worst-case trace allowance. Its generated
    /// fields must fit this bound; the advertised incoming limit excludes it.
    pub trace_bytes: usize,
    pub recipients: usize,
    pub starttls: bool,
}

/// Accepted envelope spellings, without brackets or obsolete source routes.
/// All recipients resolve to this one account; delivery must file exactly once.
pub struct Envelope {
    pub account: AccountId,
    pub ehlo: String,
    pub reverse_path: String,
    pub recipients: Vec<String>,
    pub eight_bit: bool,
    pub declared_bytes: Option<u64>,
}

/// The driver must finish this operation before supplying more input. A Reply
/// remains pending until its entire wire image is flushed, including the banner.
/// For close replies the runtime must use a bounded transport drain/close after
/// flushing, without parsing discarded input, so unread DATA does not erase the
/// refusal through a TCP reset. This engine performs no transport shutdown.
#[derive(Eq, PartialEq)]
pub enum Pending<'a> {
    Input,
    Reply {
        bytes: &'a [u8],
        close: bool,
    },
    /// Reserve the full admitted message bound before sending 354.
    BeginData {
        maximum_bytes: usize,
    },
    /// Decoded complete line, including CRLF; valid until data_written.
    Data(&'a [u8]),
    /// All decoded bytes were written. Build trace fields and publish atomically.
    Commit,
    /// No 220 was generated. The transport owner reserves TLS, flushes its 220
    /// and handshakes; it must refuse buffered plaintext and never fall back.
    /// Feed these original bytes through smtp_wire::LineReader for the existing
    /// ServerStartTls owner. This is the exact command, not a reconstructed one.
    StartTls {
        command: &'a [u8],
    },
    Closed,
}
impl std::fmt::Debug for Pending<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input => f.write_str("Input"),
            Self::Reply { close, .. } => f.debug_struct("Reply").field("close", close).finish(),
            Self::BeginData { maximum_bytes } => f
                .debug_struct("BeginData")
                .field("maximum_bytes", maximum_bytes)
                .finish(),
            Self::Data(_) => f.write_str("Data(<redacted>)"),
            Self::Commit => f.write_str("Commit"),
            Self::StartTls { .. } => f.write_str("StartTls"),
            Self::Closed => f.write_str("Closed"),
        }
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum State {
    Command,
    Reply(Next),
    Begin,
    Data,
    Write,
    Commit,
    Tls,
    Closed,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Next {
    Command,
    Data,
    Closed,
}

pub struct Session<'a> {
    routes: &'a Routing<'a>,
    settings: Settings<'a>,
    state: State,
    reply: String,
    line: [u8; DATA_BYTES],
    used: usize,
    cr: bool,
    data_start: usize,
    data_bytes: usize,
    hello: String,
    extended: bool,
    tls: bool,
    envelope: Option<Envelope>,
    recipient_bytes: usize,
}
impl<'a> Session<'a> {
    pub fn new(routes: &'a Routing<'a>, settings: Settings<'a>) -> Result<Self, Error> {
        if certificate_name(settings.hostname).is_err()
            || !(1..=MAX_MESSAGE_BYTES).contains(&settings.message_bytes)
            || settings.trace_bytes == 0
            || settings.trace_bytes >= settings.message_bytes
            || !(100..=MAX_RECIPIENTS as usize).contains(&settings.recipients)
        {
            return Err(Error::Invalid);
        }
        let mut reply = String::new();
        reply.try_reserve(1024).map_err(|_| Error::Capacity)?;
        write!(reply, "220 {} ESMTP ready\r\n", settings.hostname).map_err(|_| Error::Capacity)?;
        Ok(Self {
            routes,
            settings,
            state: State::Reply(Next::Command),
            reply,
            line: [0; DATA_BYTES],
            used: 0,
            cr: false,
            data_start: 0,
            data_bytes: 0,
            hello: String::new(),
            extended: false,
            tls: false,
            envelope: None,
            recipient_bytes: 0,
        })
    }
    pub fn pending(&self) -> Pending<'_> {
        match self.state {
            State::Command | State::Data => Pending::Input,
            State::Reply(next) => Pending::Reply {
                bytes: self.reply.as_bytes(),
                close: next == Next::Closed,
            },
            State::Begin => Pending::BeginData {
                maximum_bytes: self.settings.message_bytes,
            },
            State::Write => self
                .line
                .get(self.data_start..self.used)
                .map(Pending::Data)
                .unwrap_or(Pending::Closed),
            State::Commit => Pending::Commit,
            State::Tls => Pending::StartTls {
                command: self.line.get(..self.used).unwrap_or(&[]),
            },
            State::Closed => Pending::Closed,
        }
    }
    /// Maximum decoded incoming bytes. The reservation additionally covers trace.
    pub fn incoming_limit(&self) -> usize {
        self.settings.message_bytes - self.settings.trace_bytes
    }
    pub fn envelope(&self) -> Option<&Envelope> {
        self.envelope.as_ref()
    }
    /// At most one command or DATA line, irrespective of input size. A framing
    /// failure is terminal; its unread tail must never be submitted elsewhere.
    pub fn feed(&mut self, input: &[u8]) -> Result<usize, Error> {
        let result = self.feed_inner(input);
        if result.is_err() && result != Err(Error::Conflict) {
            self.abort();
        }
        result
    }
    fn feed_inner(&mut self, input: &[u8]) -> Result<usize, Error> {
        if !matches!(self.state, State::Command | State::Data) {
            return Err(Error::Conflict);
        }
        let data = self.state == State::Data;
        for (offset, &byte) in input.iter().enumerate() {
            let cap = if data { DATA_BYTES } else { COMMAND_BYTES };
            if self.used == cap {
                let reply = if data {
                    "554 5.6.0 Message line too long\r\n"
                } else {
                    "500 5.5.2 Command too long\r\n"
                };
                self.respond(reply, Next::Closed);
                return Ok(offset + 1);
            }
            if (self.cr && byte != b'\n')
                || (!self.cr && byte == b'\n')
                || (!data
                    && !(byte == b'\r'
                        || (self.cr && byte == b'\n')
                        || byte == b'\t'
                        || (32..=126).contains(&byte)))
                || (data
                    && (byte == 0
                        || (!self.envelope.as_ref().is_some_and(|e| e.eight_bit) && byte >= 128)))
            {
                let reply = if data {
                    "554 5.6.0 Invalid message framing\r\n"
                } else {
                    "501 5.5.2 Invalid command framing\r\n"
                };
                self.respond(reply, Next::Closed);
                return Ok(offset + 1);
            }
            *self.line.get_mut(self.used).ok_or(Error::Capacity)? = byte;
            self.used += 1;
            if self.cr {
                let consumed = offset + 1;
                if data {
                    self.data_line()?;
                } else {
                    // Commands are tiny; copying frees the parser buffer for replies/state.
                    let mut command = [0; COMMAND_BYTES];
                    let bytes = self.line.get(..self.used - 2).ok_or(Error::Invalid)?;
                    let target = command.get_mut(..bytes.len()).ok_or(Error::Invalid)?;
                    target.copy_from_slice(bytes);
                    let command = std::str::from_utf8(target).map_err(|_| Error::Invalid)?;
                    self.command(command)?;
                    if self.state == State::Tls && consumed != input.len() {
                        self.respond("554 5.5.0 Buffered plaintext at STARTTLS\r\n", Next::Closed);
                    }
                }
                return Ok(consumed);
            }
            self.cr = byte == b'\r';
        }
        Ok(input.len())
    }
    pub fn reply_sent(&mut self) -> Result<(), Error> {
        let State::Reply(next) = self.state else {
            return Err(Error::Conflict);
        };
        self.reply.clear();
        self.clear_line();
        self.state = match next {
            Next::Command => State::Command,
            Next::Data => State::Data,
            Next::Closed => State::Closed,
        };
        Ok(())
    }
    pub fn data_ready(&mut self, result: Result<(), Error>) -> Result<(), Error> {
        if self.state != State::Begin {
            return Err(Error::Conflict);
        }
        if result.is_ok() {
            self.respond("354 Send message, end with a dot line\r\n", Next::Data);
        } else {
            self.reset();
            self.respond(
                "451 4.3.0 Delivery temporarily unavailable\r\n",
                Next::Command,
            );
        }
        Ok(())
    }
    pub fn data_written(&mut self, result: Result<(), Error>) -> Result<(), Error> {
        if self.state != State::Write {
            return Err(Error::Conflict);
        }
        if result.is_ok() {
            self.clear_line();
            self.state = State::Data;
        } else {
            self.respond(
                "451 4.3.0 Delivery temporarily unavailable\r\n",
                Next::Closed,
            );
        }
        Ok(())
    }
    /// Trusted adapter result, not a credential or proof by itself. Success must
    /// mean raw mail, trace/envelope, thread, Inbox and history are durable.
    pub fn committed(&mut self, result: Result<Commit, CommitFailure>) -> Result<(), Error> {
        if self.state != State::Commit {
            return Err(Error::Conflict);
        }
        match result {
            Ok(commit)
                if self
                    .envelope
                    .as_ref()
                    .is_some_and(|e| e.account == commit.account) =>
            {
                self.reset();
                self.respond("250 2.0.0 Message accepted\r\n", Next::Command);
            }
            Err(CommitFailure::Rejected(_)) => {
                self.reset();
                self.respond(
                    "451 4.3.0 Delivery temporarily unavailable\r\n",
                    Next::Command,
                );
            }
            // No final reply can accurately describe an uncertain publication.
            _ => self.abort(),
        }
        Ok(())
    }
    /// Call only after the transport owner completes TLS on this connection.
    /// No SMTP banner is repeated and all pre-TLS client knowledge is discarded.
    pub fn tls_established(&mut self) -> Result<(), Error> {
        if self.state != State::Tls {
            return Err(Error::Conflict);
        }
        self.reset();
        self.hello.clear();
        self.extended = false;
        self.tls = true;
        self.clear_line();
        self.state = State::Command;
        Ok(())
    }
    /// Queue a service-close notice at a legal command reply boundary. For
    /// gateway revocation, flush the DATA 451 first, then call this immediately
    /// after reply_sent and before feeding more input. Drain/overload policy can
    /// use the same boundary; partially received commands must instead abort.
    pub fn service_unavailable(&mut self) -> Result<(), Error> {
        if self.state != State::Command || self.used != 0 {
            return Err(Error::Conflict);
        }
        self.respond("421 4.3.2 Service unavailable\r\n", Next::Closed);
        Ok(())
    }
    /// Disconnect, timeout, failed upgrade or driver failure. The driver must
    /// separately retire its uncommitted spool/reservation; no retry is implied.
    pub fn abort(&mut self) {
        self.reset();
        self.clear_line();
        self.reply.clear();
        self.state = State::Closed;
    }
    fn clear_line(&mut self) {
        self.used = 0;
        self.cr = false;
        self.data_start = 0;
    }
    fn reset(&mut self) {
        self.envelope = None;
        self.recipient_bytes = 0;
        self.data_bytes = 0;
    }
    fn respond(&mut self, reply: &str, next: Next) {
        self.reply.clear();
        self.reply.push_str(reply);
        self.state = State::Reply(next);
        if next == Next::Closed {
            self.reset();
        }
    }
    fn data_line(&mut self) -> Result<(), Error> {
        let line = self.line.get(..self.used).ok_or(Error::Invalid)?;
        if line == b".\r\n" {
            self.state = State::Commit;
            return Ok(());
        }
        self.data_start = usize::from(line.first() == Some(&b'.'));
        let size = self.used - self.data_start;
        if size > 1000 {
            self.respond("554 5.6.0 Message line too long\r\n", Next::Closed);
        } else if self
            .data_bytes
            .checked_add(size)
            .is_none_or(|n| n > self.incoming_limit())
        {
            self.respond("552 5.3.4 Message too large\r\n", Next::Closed);
        } else {
            self.data_bytes += size;
            self.state = State::Write;
        }
        Ok(())
    }
    fn command(&mut self, line: &str) -> Result<(), Error> {
        let (verb, arg) = line.split_once(' ').unwrap_or((line, ""));
        let mail = verb.eq_ignore_ascii_case("MAIL");
        if line.len() + 2
            > if mail && self.extended {
                COMMAND_BYTES
            } else {
                512
            }
        {
            self.respond("500 5.5.2 Command too long\r\n", Next::Command);
        } else if verb.eq_ignore_ascii_case("EHLO") || verb.eq_ignore_ascii_case("HELO") {
            if !domain(arg) {
                self.respond("501 5.5.2 Invalid greeting\r\n", Next::Command);
                return Ok(());
            }
            let hello = copy(arg)?;
            self.reset();
            self.hello = hello;
            self.extended = verb.eq_ignore_ascii_case("EHLO");
            self.reply.clear();
            if self.extended {
                let incoming_limit = self.incoming_limit();
                write!(
                    self.reply,
                    "250-{}\r\n250-SIZE {}\r\n250-8BITMIME\r\n",
                    self.settings.hostname, incoming_limit
                )
                .map_err(|_| Error::Capacity)?;
                if self.settings.starttls && !self.tls {
                    self.reply.push_str("250-STARTTLS\r\n");
                }
                self.reply.push_str("250 ENHANCEDSTATUSCODES\r\n");
            } else {
                write!(self.reply, "250 {}\r\n", self.settings.hostname)
                    .map_err(|_| Error::Capacity)?;
            }
            self.state = State::Reply(Next::Command);
        } else if verb.eq_ignore_ascii_case("QUIT") && arg.is_empty() {
            self.respond("221 2.0.0 Closing connection\r\n", Next::Closed);
        } else if verb.eq_ignore_ascii_case("RSET") && arg.is_empty() {
            self.reset();
            self.respond("250 2.0.0 Reset\r\n", Next::Command);
        } else if verb.eq_ignore_ascii_case("NOOP") {
            self.respond("250 2.0.0 OK\r\n", Next::Command);
        } else if verb.eq_ignore_ascii_case("VRFY") && !arg.is_empty() {
            self.respond("252 2.0.0 Cannot verify; try delivery\r\n", Next::Command);
        } else if line.eq_ignore_ascii_case("STARTTLS") {
            if !self.extended || self.envelope.is_some() {
                self.respond("503 5.5.1 Bad command sequence\r\n", Next::Command);
            } else if !self.settings.starttls || self.tls {
                self.respond("502 5.5.1 STARTTLS unavailable\r\n", Next::Command);
            } else {
                self.state = State::Tls;
            }
        } else if mail {
            self.mail(arg)?;
        } else if verb.eq_ignore_ascii_case("RCPT") {
            self.rcpt(arg)?;
        } else if verb.eq_ignore_ascii_case("DATA") && arg.is_empty() {
            if self
                .envelope
                .as_ref()
                .is_some_and(|e| !e.recipients.is_empty())
            {
                self.state = State::Begin;
            } else {
                self.respond("503 5.5.1 Need MAIL and RCPT\r\n", Next::Command);
            }
        } else if ["QUIT", "RSET", "VRFY", "DATA", "STARTTLS"]
            .iter()
            .any(|known| verb.eq_ignore_ascii_case(known))
        {
            self.respond("501 5.5.4 Invalid command arguments\r\n", Next::Command);
        } else {
            self.respond("500 5.5.2 Command not recognized\r\n", Next::Command);
        }
        Ok(())
    }
    fn mail(&mut self, arg: &str) -> Result<(), Error> {
        if self.hello.is_empty() || self.envelope.is_some() {
            self.respond("503 5.5.1 Bad command sequence\r\n", Next::Command);
            return Ok(());
        }
        let Some((address, parameters)) = path(arg, "FROM:", true) else {
            self.respond("501 5.5.2 Invalid reverse path\r\n", Next::Command);
            return Ok(());
        };
        let mut size = None;
        let mut body = None;
        for parameter in parameters.split(' ').filter(|p| !p.is_empty()) {
            if !self.extended {
                self.respond("555 5.5.4 Extensions require EHLO\r\n", Next::Command);
                return Ok(());
            }
            let (key, value) = parameter.split_once('=').unwrap_or((parameter, ""));
            if key.eq_ignore_ascii_case("SIZE") {
                if size.is_some()
                    || value.is_empty()
                    || value.len() > 20
                    || !value.bytes().all(|b| b.is_ascii_digit())
                {
                    self.respond("501 5.5.2 Invalid SIZE\r\n", Next::Command);
                    return Ok(());
                }
                // Twenty valid digits can exceed u64; that is a size refusal.
                let Ok(number) = value.parse::<u64>() else {
                    self.respond("552 5.3.4 Message too large\r\n", Next::Command);
                    return Ok(());
                };
                size = Some(number);
            } else if key.eq_ignore_ascii_case("BODY") {
                if body.is_some()
                    || !(value.eq_ignore_ascii_case("7BIT")
                        || value.eq_ignore_ascii_case("8BITMIME"))
                {
                    self.respond("501 5.5.2 Invalid BODY\r\n", Next::Command);
                    return Ok(());
                }
                body = Some(value.eq_ignore_ascii_case("8BITMIME"));
            } else {
                self.respond("555 5.5.4 Unsupported MAIL parameter\r\n", Next::Command);
                return Ok(());
            }
        }
        if size.is_some_and(|n| n > self.incoming_limit() as u64) {
            self.respond("552 5.3.4 Message too large\r\n", Next::Command);
            return Ok(());
        }
        let mut recipients = Vec::new();
        recipients
            .try_reserve(self.settings.recipients)
            .map_err(|_| Error::Capacity)?;
        self.envelope = Some(Envelope {
            account: self.routes.account(),
            ehlo: copy(&self.hello)?,
            reverse_path: copy(address)?,
            recipients,
            eight_bit: body.unwrap_or(false),
            declared_bytes: size,
        });
        self.respond("250 2.1.0 Sender accepted\r\n", Next::Command);
        Ok(())
    }
    fn rcpt(&mut self, arg: &str) -> Result<(), Error> {
        if self.envelope.is_none() {
            self.respond("503 5.5.1 Need MAIL\r\n", Next::Command);
            return Ok(());
        }
        let Some((address, parameters)) = path(arg, "TO:", false) else {
            self.respond("501 5.5.2 Invalid forward path\r\n", Next::Command);
            return Ok(());
        };
        if !parameters.is_empty() {
            self.respond("555 5.5.4 Unsupported RCPT parameter\r\n", Next::Command);
            return Ok(());
        }
        match self.routes.resolve(address) {
            Ok(Some(account)) if account == self.routes.account() => (),
            Ok(_) => {
                self.respond("550 5.1.1 Unknown local recipient\r\n", Next::Command);
                return Ok(());
            }
            Err(_) => {
                self.respond("451 4.3.0 Routing unavailable\r\n", Next::Command);
                return Ok(());
            }
        }
        let envelope = self.envelope.as_mut().ok_or(Error::Invalid)?;
        // ReceiptRecipients stores a count and a u32 length before each address.
        let needed = self.recipient_bytes + 4 + address.len();
        if envelope.recipients.len() == self.settings.recipients || needed + 4 > MAX_RECEIPT_BYTES {
            self.respond("452 4.5.3 Too many recipients\r\n", Next::Command);
            return Ok(());
        }
        envelope.recipients.push(copy(address)?);
        self.recipient_bytes = needed;
        self.respond("250 2.1.5 Recipient accepted\r\n", Next::Command);
        Ok(())
    }
}
fn copy(input: &str) -> Result<String, Error> {
    let mut result = String::new();
    result
        .try_reserve_exact(input.len())
        .map_err(|_| Error::Capacity)?;
    result.push_str(input);
    Ok(result)
}
fn domain(input: &str) -> bool {
    if let Some(literal) = input.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        if literal
            .get(..5)
            .is_some_and(|s| s.eq_ignore_ascii_case("IPv6:"))
        {
            return literal
                .get(5..)
                .is_some_and(|s| s.parse::<Ipv6Addr>().is_ok());
        }
        return literal.parse::<Ipv4Addr>().is_ok();
    }
    certificate_name(input).is_ok()
}
fn path<'a>(input: &'a str, prefix: &str, null: bool) -> Option<(&'a str, &'a str)> {
    if !input.get(..prefix.len())?.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let input = input.get(prefix.len()..)?.strip_prefix('<')?;
    let mut quoted = false;
    let mut escaped = false;
    let mut end = None;
    for (index, byte) in input.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && byte == b'\\' {
            escaped = true;
            continue;
        }
        if byte == b'"' {
            quoted = !quoted;
        }
        if !quoted && byte == b'>' {
            end = Some(index);
            break;
        }
    }
    let end = end?;
    if end > MAX_ADDRESS {
        return None;
    }
    let mut address = input.get(..end)?;
    let tail = input.get(end + 1..)?;
    let parameters = if tail.is_empty() {
        tail
    } else {
        tail.strip_prefix(' ')?
    };
    // RFC 5321 obsolete source routes are accepted and ignored, never relayed.
    if address.starts_with('@') {
        let (route, mailbox) = address.split_once(':')?;
        if !route
            .split(',')
            .all(|hop| hop.strip_prefix('@').is_some_and(domain))
        {
            return None;
        }
        if mailbox.is_empty() {
            return None;
        }
        address = mailbox;
    }
    if address.is_empty() {
        return null.then_some((address, parameters));
    }
    if !null && address.eq_ignore_ascii_case("postmaster") {
        return Some((address, parameters));
    }
    let (local, host) = address.rsplit_once('@')?;
    if local.is_empty() || local.len() > 64 || !domain(host) {
        return None;
    }
    // Reuse the configured mailbox's ASCII local syntax with a fixed DNS suffix.
    // SMTP additionally permits the empty quoted local part (which has no route).
    if local != "\"\"" {
        let mut key = [0; MAX_ADDRESS];
        let mut mailbox = [0; 71];
        mailbox
            .get_mut(..local.len())?
            .copy_from_slice(local.as_bytes());
        mailbox
            .get_mut(local.len()..local.len() + 7)?
            .copy_from_slice(b"@x.test");
        let mailbox = std::str::from_utf8(mailbox.get(..local.len() + 7)?).ok()?;
        crate::config::values::mailbox_key(mailbox, &mut key).ok()?;
    }
    Some((address, parameters))
}

#[cfg(test)]
#[path = "smtp_session_tests.rs"]
mod tests;
