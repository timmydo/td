//! Bounded SMTP AUTH on a fresh EHLO session with retained-policy credentials.
use crate::{
    config::{material::MAX_PASSWORD_BYTES, outbound::MAX_USERNAME_BYTES},
    ports::{Clock, Deadline, Error, FlushProgress, IoProgress, Tick, TlsPolicyId, Transport},
    smtp_relay_ehlo,
    smtp_wire::{RelayCapabilities, ReplyReader, LINE_BYTES},
    tls_io::TlsWireStorage,
    tls_policy::TlsConnection,
};
use std::sync::Arc;
use td_mime::{
    base64, base64_encode,
    work::{Charge, Meter, Stop},
};

pub const PLAIN_BYTES: usize = MAX_USERNAME_BYTES + MAX_PASSWORD_BYTES + 2;
pub const RESPONSE_BYTES: usize = PLAIN_BYTES.div_ceil(3) * 4 + 2;

/// Startup-reserved cells. Contents may include credentials; do not log them.
pub struct Scratch<'a> {
    pub input: &'a mut [u8],
    pub first_reply: &'a mut [u8],
    pub second_reply: &'a mut [u8],
    pub third_reply: &'a mut [u8],
    pub plain: &'a mut [u8],
    pub encoded: &'a mut [u8],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mechanism {
    Plain,
    Login,
}
enum Phase {
    Write(usize),
    Flush,
    Read,
    Decode,
    Prepare,
    Encode,
}

#[must_use = "drive authentication or recover its reserved buffers"]
pub struct Exchange<'a, B: TlsWireStorage> {
    connection: TlsConnection<B>,
    mechanism: Mechanism,
    capabilities: RelayCapabilities,
    clock: Arc<dyn Clock>,
    deadline: Deadline,
    meter: &'a mut Meter,
    username: &'a str,
    password: &'a str,
    phase: Phase,
    round: usize,
    peer_rejection: Option<u16>,
    input: &'a mut [u8],
    used: usize,
    consumed: usize,
    replies: [ReplyReader<'a>; 3],
    decoder: base64::Decoder,
    challenge_consumed: usize,
    plain: &'a mut [u8],
    plain_len: usize,
    plain_consumed: usize,
    encoded: &'a mut [u8],
    encoded_len: usize,
    encoder: Option<base64_encode::Encoder>,
}
#[must_use = "continue authentication or retain its authenticated session"]
#[allow(
    clippy::large_enum_variant,
    reason = "Inline TLS custody avoids per-turn heap allocation."
)]
pub enum Progress<'a, B: TlsWireStorage> {
    Pending(Exchange<'a, B>),
    Ready(Session<B>),
}
/// Remote authentication evidence on this exclusive socket, not queue authority.
pub struct Session<B: TlsWireStorage> {
    connection: TlsConnection<B>,
    mechanism: Mechanism,
    capabilities: RelayCapabilities,
}
impl<B: TlsWireStorage> Session<B> {
    pub fn mechanism(&self) -> Mechanism {
        self.mechanism
    }
    /// PLAIN and LOGIN install no SASL security layer; retain this TLS EHLO.
    pub fn capabilities(&self) -> RelayCapabilities {
        self.capabilities
    }
    pub fn policy_id(&self) -> TlsPolicyId {
        self.connection.policy_id()
    }
    pub(crate) fn connection_mut(&mut self) -> &mut TlsConnection<B> {
        &mut self.connection
    }
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.connection.into_buffers()
    }
}
/// TLS and socket are already torn down. Only reusable buffers remain.
pub struct Refusal<B: TlsWireStorage> {
    pub error: Error,
    peer_rejection: Option<u16>,
    buffers: Result<(B, B), Error>,
}
impl<B: TlsWireStorage> Refusal<B> {
    /// Complete isolated 4xx/5xx reply evidence, never local-policy refusal.
    pub fn peer_rejection(&self) -> Option<u16> {
        self.peer_rejection
    }
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.buffers
    }
}
impl<'a, B: TlsWireStorage> Exchange<'a, B> {
    pub fn new(
        session: smtp_relay_ehlo::Session<B>,
        username: &'a str,
        password: &'a str,
        scratch: Scratch<'a>,
        clock: Arc<dyn Clock>,
        deadline: Deadline,
        meter: &'a mut Meter,
    ) -> Result<Self, Refusal<B>> {
        let capabilities = session.capabilities();
        let mechanisms = capabilities.auth();
        let connection = session.into_connection();
        let init = (|scratch: Scratch<'a>| {
            check_time(clock.as_ref(), deadline, meter)?;
            connection.check_relay_credentials(username, password)?;
            let mechanism = if mechanisms.plain() {
                Mechanism::Plain
            } else if mechanisms.login() {
                Mechanism::Login
            } else {
                return Err(Error::Forbidden);
            };
            Ok((
                mechanism,
                scratch.input.get_mut(..LINE_BYTES).ok_or(Error::Capacity)?,
                [
                    ReplyReader::new(scratch.first_reply)?,
                    ReplyReader::new(scratch.second_reply)?,
                    ReplyReader::new(scratch.third_reply)?,
                ],
                scratch
                    .plain
                    .get_mut(..PLAIN_BYTES)
                    .ok_or(Error::Capacity)?,
                scratch
                    .encoded
                    .get_mut(..RESPONSE_BYTES)
                    .ok_or(Error::Capacity)?,
            ))
        })(scratch);
        match init {
            Err(error) => Err(refuse(connection, error)),
            Ok((mechanism, input, replies, plain, encoded)) => Ok(Self {
                connection,
                mechanism,
                capabilities,
                clock,
                deadline,
                meter,
                username,
                password,
                phase: Phase::Write(0),
                round: 0,
                peer_rejection: None,
                input,
                used: 0,
                consumed: 0,
                replies,
                decoder: base64::Decoder::default(),
                challenge_consumed: 0,
                plain,
                plain_len: 0,
                plain_consumed: 0,
                encoded,
                encoded_len: 0,
                encoder: None,
            }),
        }
    }
    /// One transport operation, one reply line, or one bounded codec poll.
    /// Every turn consumes the original work budget, including pending I/O.
    pub fn advance(mut self) -> Result<Progress<'a, B>, Refusal<B>> {
        let result = (|| {
            let now = check_time(self.clock.as_ref(), self.deadline, self.meter)?;
            self.meter
                .charge(
                    now,
                    Charge {
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(work_error)?;
            let ready = self.step(now)?;
            check_time(self.clock.as_ref(), self.deadline, self.meter)?;
            Ok(ready)
        })();
        match result {
            Err(error) => {
                let mut refusal = refuse(self.connection, error);
                refusal.peer_rejection = self.peer_rejection;
                Err(refusal)
            }
            Ok(false) => Ok(Progress::Pending(self)),
            Ok(true) => Ok(Progress::Ready(Session {
                connection: self.connection,
                mechanism: self.mechanism,
                capabilities: self.capabilities,
            })),
        }
    }
    fn step(&mut self, now: Tick) -> Result<bool, Error> {
        match self.phase {
            Phase::Write(offset) => {
                let bytes = if self.round == 0 {
                    match self.mechanism {
                        Mechanism::Plain => b"AUTH PLAIN\r\n".as_slice(),
                        Mechanism::Login => b"AUTH LOGIN\r\n".as_slice(),
                    }
                } else {
                    self.encoded.get(..self.encoded_len).ok_or(Error::Invalid)?
                };
                let rest = bytes.get(offset..).ok_or(Error::Invalid)?;
                self.meter
                    .charge(
                        now,
                        Charge {
                            io_bytes: rest.len() as u64,
                            ..Charge::default()
                        },
                    )
                    .map_err(work_error)?;
                match self.connection.write(rest)? {
                    IoProgress::Pending => {}
                    IoProgress::Bytes(n) if n > 0 && n <= rest.len() => {
                        let next = offset.checked_add(n).ok_or(Error::Invalid)?;
                        self.phase = if next == bytes.len() {
                            Phase::Flush
                        } else {
                            Phase::Write(next)
                        };
                    }
                    _ => return Err(Error::Invalid),
                }
                Ok(false)
            }
            Phase::Flush => {
                if self.connection.flush()? == FlushProgress::Complete {
                    self.phase = Phase::Read;
                }
                Ok(false)
            }
            Phase::Read => self.read_reply(now),
            Phase::Decode => {
                self.decode_challenge(now)?;
                Ok(false)
            }
            Phase::Prepare => {
                self.prepare(now)?;
                Ok(false)
            }
            Phase::Encode => {
                self.encode(now)?;
                Ok(false)
            }
        }
    }
    fn read_reply(&mut self, now: Tick) -> Result<bool, Error> {
        if self.consumed == self.used {
            self.meter
                .charge(
                    now,
                    Charge {
                        io_bytes: LINE_BYTES as u64,
                        ..Charge::default()
                    },
                )
                .map_err(work_error)?;
            match self.connection.read(self.input)? {
                IoProgress::Pending => return Ok(false),
                IoProgress::Bytes(n) if n > 0 && n <= self.input.len() => {
                    self.used = n;
                    self.consumed = 0;
                }
                IoProgress::Closed => return Err(Error::Tls),
                _ => return Err(Error::Invalid),
            }
        }
        let reply = self.replies.get_mut(self.round).ok_or(Error::Invalid)?;
        let progress = reply.feed(
            self.input
                .get(self.consumed..self.used)
                .ok_or(Error::Invalid)?,
        )?;
        self.consumed = self
            .consumed
            .checked_add(progress.consumed)
            .ok_or(Error::Invalid)?;
        if !progress.complete {
            return Ok(false);
        }
        let line = reply.line().ok_or(Error::Invalid)?;
        if (400..600).contains(&line.code)
            && line.last
            && reply.complete()
            && self.consumed == self.used
        {
            self.peer_rejection = Some(line.code);
        }
        let final_reply =
            self.round == 2 || (self.mechanism == Mechanism::Plain && self.round == 1);
        if final_reply {
            if line.code != 235 {
                return Err(Error::Forbidden);
            }
            if !reply.complete() {
                reply.advance()?;
                return Ok(false);
            }
            if self.consumed != self.used {
                return Err(Error::Invalid);
            }
            return Ok(true);
        }
        if line.code != 334 {
            return Err(Error::Forbidden);
        }
        if !line.last || self.consumed != self.used {
            return Err(Error::Invalid);
        }
        if self.mechanism == Mechanism::Plain {
            if !line.text.is_empty() {
                return Err(Error::Invalid);
            }
            self.phase = Phase::Prepare;
        } else {
            if !line
                .text
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
            {
                return Err(Error::Invalid);
            }
            self.decoder = base64::Decoder::default();
            self.challenge_consumed = 0;
            self.phase = Phase::Decode;
        }
        Ok(false)
    }
    fn decode_challenge(&mut self, now: Tick) -> Result<(), Error> {
        let text = self
            .replies
            .get(self.round)
            .and_then(ReplyReader::line)
            .ok_or(Error::Invalid)?
            .text;
        let input = text.get(self.challenge_consumed..).ok_or(Error::Invalid)?;
        let progress = self
            .decoder
            .poll(input, self.encoded, true, now, self.meter)
            .map_err(work_error)?;
        self.challenge_consumed = self
            .challenge_consumed
            .checked_add(progress.consumed)
            .ok_or(Error::Invalid)?;
        if progress.status == base64::Status::Complete {
            if self.decoder.is_encoding_problem() || self.challenge_consumed != text.len() {
                return Err(Error::Invalid);
            }
            self.phase = Phase::Prepare;
        }
        Ok(())
    }
    fn prepare(&mut self, now: Tick) -> Result<(), Error> {
        self.plain.fill(0);
        self.encoded.fill(0);
        self.plain_len = 0;
        let fields: &[&[u8]] = match (self.mechanism, self.round) {
            (Mechanism::Plain, 0) => &[
                b"\0",
                self.username.as_bytes(),
                b"\0",
                self.password.as_bytes(),
            ],
            (Mechanism::Login, 0) => &[self.username.as_bytes()],
            (Mechanism::Login, 1) => &[self.password.as_bytes()],
            _ => return Err(Error::Invalid),
        };
        for field in fields {
            let end = self
                .plain_len
                .checked_add(field.len())
                .ok_or(Error::Capacity)?;
            self.plain
                .get_mut(self.plain_len..end)
                .ok_or(Error::Capacity)?
                .copy_from_slice(field);
            self.plain_len = end;
        }
        self.plain_consumed = 0;
        self.encoded_len = 0;
        self.encoder = Some(
            base64_encode::Encoder::new_unwrapped(self.plain_len as u64, now, self.meter)
                .map_err(encode_error)?,
        );
        self.phase = Phase::Encode;
        Ok(())
    }
    fn encode(&mut self, now: Tick) -> Result<(), Error> {
        let encoder = self.encoder.as_mut().ok_or(Error::Invalid)?;
        let input = self
            .plain
            .get(self.plain_consumed..self.plain_len)
            .ok_or(Error::Invalid)?;
        let output = self
            .encoded
            .get_mut(self.encoded_len..RESPONSE_BYTES - 2)
            .ok_or(Error::Capacity)?;
        let progress = encoder
            .poll(input, output, now, self.meter)
            .map_err(encode_error)?;
        self.plain_consumed = self
            .plain_consumed
            .checked_add(progress.consumed)
            .ok_or(Error::Invalid)?;
        self.encoded_len = self
            .encoded_len
            .checked_add(progress.written)
            .ok_or(Error::Invalid)?;
        if progress.status == base64_encode::Status::Complete {
            self.encoder
                .take()
                .ok_or(Error::Invalid)?
                .finish(now, self.meter)
                .map_err(encode_error)?;
            if self.plain_consumed != self.plain_len {
                return Err(Error::Invalid);
            }
            let end = self.encoded_len.checked_add(2).ok_or(Error::Capacity)?;
            self.encoded
                .get_mut(self.encoded_len..end)
                .ok_or(Error::Capacity)?
                .copy_from_slice(b"\r\n");
            self.encoded_len = end;
            self.round = self.round.checked_add(1).ok_or(Error::Invalid)?;
            self.phase = Phase::Write(0);
        } else if progress.status == base64_encode::Status::NeedOutput {
            return Err(Error::Capacity);
        }
        Ok(())
    }
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.connection.into_buffers()
    }
}
fn work_error(error: Stop) -> Error {
    if error == Stop::Deadline {
        Error::Deadline
    } else {
        Error::Capacity
    }
}
fn encode_error(error: base64_encode::Error) -> Error {
    match error {
        base64_encode::Error::Work(stop) => work_error(stop),
        _ => Error::Invalid,
    }
}
fn check_time(clock: &dyn Clock, deadline: Deadline, meter: &mut Meter) -> Result<Tick, Error> {
    let now = clock.sample()?.monotonic;
    if deadline.expired(now) {
        return Err(Error::Deadline);
    }
    meter.charge(now, Charge::default()).map_err(work_error)?;
    Ok(now)
}
fn refuse<B: TlsWireStorage>(mut connection: TlsConnection<B>, error: Error) -> Refusal<B> {
    connection.abort();
    Refusal {
        error,
        peer_rejection: None,
        buffers: connection.into_buffers(),
    }
}
const _: () = assert!(
    std::mem::size_of::<Progress<'static, Box<[u8; crate::tls_io::TLS_WIRE_BYTES]>>>() <= 8192
);
