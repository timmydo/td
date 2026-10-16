//! One bounded envelope/DATA command and its complete normalized SMTP reply.
use crate::{
    config::values::mailbox_key,
    format::row::MAX_ADDRESS,
    ports::{Clock, Deadline, Error, FlushProgress, IoProgress, Tick, Transport},
    smtp_relay_auth::Session,
    smtp_wire::{NormalizedReplyReader, LINE_BYTES},
    tls_io::TlsWireStorage,
};
use std::sync::Arc;
use td_mime::work::{Charge, Meter, Stop};

/// Only command framing; trusted runtime owns envelope and phase authority.
#[derive(Clone, Copy)]
pub enum Command<'a> {
    MailFrom(&'a str),
    MailFromEightBit(&'a str),
    RcptTo(&'a str),
    Data,
}
/// Reserved before network work. Output is provisional until a complete reply.
pub struct Scratch<'a> {
    pub input: &'a mut [u8],
    pub line: &'a mut [u8],
    pub reply: &'a mut [u8],
    pub command: &'a mut [u8],
}
enum Phase {
    Write(usize),
    Flush,
    Read,
}
#[must_use = "drive the command or recover its reserved buffers"]
pub struct Exchange<'a, B: TlsWireStorage> {
    session: Session<B>,
    clock: Arc<dyn Clock>,
    deadline: Deadline,
    meter: &'a mut Meter,
    phase: Phase,
    input: &'a mut [u8],
    command: &'a [u8],
    reply: NormalizedReplyReader<'a>,
}
#[must_use = "continue the exchange or retain its complete reply"]
#[allow(
    clippy::large_enum_variant,
    reason = "Inline TLS custody avoids per-turn heap allocation."
)]
pub enum Progress<'a, B: TlsWireStorage> {
    Pending(Exchange<'a, B>),
    Ready(Reply<'a, B>),
}
/// An actual complete reply bound to the same authenticated connection.
/// Neither positive status nor extracted custody grants durable queue authority.
pub struct Reply<'a, B: TlsWireStorage> {
    session: Session<B>,
    reply: NormalizedReplyReader<'a>,
}
impl<B: TlsWireStorage> Reply<'_, B> {
    /// Fence transport while retaining complete normalized parser storage.
    pub(crate) fn abort(&mut self) {
        self.session.connection_mut().abort();
    }
    pub fn code(&self) -> Result<u16, Error> {
        self.reply.code().ok_or(Error::Invalid)
    }
    pub fn text(&self) -> Result<&str, Error> {
        self.reply.value().ok_or(Error::Invalid)
    }
    /// Retain the reply before advancing. The runtime owns commit/fence ordering.
    pub fn into_session(self) -> Session<B> {
        self.session
    }
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.session.into_buffers()
    }
}
/// Closed transport; a fully parsed reply may survive a post-read refusal.
pub struct Refusal<'a, B: TlsWireStorage> {
    pub error: Error,
    buffers: Result<(B, B), Error>,
    reply: Option<(u16, &'a str)>,
}
impl<B: TlsWireStorage> Refusal<'_, B> {
    /// Passive actual evidence only; no session or output authority survives.
    pub fn reply(&self) -> Option<(u16, &str)> {
        self.reply
    }
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.buffers
    }
}
impl<'a, B: TlsWireStorage> Exchange<'a, B> {
    /// Only negotiated BODY=8BITMIME is supported; no DATA body bytes.
    /// The original attempt deadline, meter and current queue fences are required.
    pub fn new(
        session: Session<B>,
        command: Command<'_>,
        scratch: Scratch<'a>,
        clock: Arc<dyn Clock>,
        deadline: Deadline,
        meter: &'a mut Meter,
    ) -> Result<Self, Refusal<'a, B>> {
        Self::new_inner(session, Some(command), scratch, clock, deadline, meter)
    }
    pub(crate) fn new_final(
        session: Session<B>,
        scratch: Scratch<'a>,
        clock: Arc<dyn Clock>,
        deadline: Deadline,
        meter: &'a mut Meter,
    ) -> Result<Self, Refusal<'a, B>> {
        Self::new_inner(session, None, scratch, clock, deadline, meter)
    }
    fn new_inner(
        session: Session<B>,
        command: Option<Command<'_>>,
        scratch: Scratch<'a>,
        clock: Arc<dyn Clock>,
        deadline: Deadline,
        meter: &'a mut Meter,
    ) -> Result<Self, Refusal<'a, B>> {
        let init = (|scratch: Scratch<'a>| {
            let now = check(clock.as_ref(), deadline, meter)?;
            meter
                .charge(
                    now,
                    Charge {
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(work)?;
            if matches!(command, Some(Command::MailFromEightBit(_)))
                && !session.capabilities().eight_bit_mime()
            {
                return Err(Error::Forbidden);
            }
            let input = scratch.input.get_mut(..LINE_BYTES).ok_or(Error::Capacity)?;
            let reply = NormalizedReplyReader::new(scratch.line, scratch.reply)?;
            let command = if let Some(command) = command {
                frame(command, scratch.command)?
            } else {
                let output = scratch.command.get_mut(..3).ok_or(Error::Capacity)?;
                output.copy_from_slice(b".\r\n");
                output
            };
            check(clock.as_ref(), deadline, meter)?;
            Ok((input, reply, command))
        })(scratch);
        match init {
            Err(error) => Err(refuse(session, error)),
            Ok((input, reply, command)) => Ok(Self {
                session,
                clock,
                deadline,
                meter,
                phase: Phase::Write(0),
                input,
                command,
                reply,
            }),
        }
    }
    pub(crate) fn writes_pending(&self) -> bool {
        matches!(self.phase, Phase::Write(_) | Phase::Flush)
    }
    /// At most one TLS operation and 512 received bytes parsed per turn.
    pub fn advance(mut self) -> Result<Progress<'a, B>, Refusal<'a, B>> {
        let mut complete = false;
        let result = (|| {
            let now = check(self.clock.as_ref(), self.deadline, self.meter)?;
            self.meter
                .charge(
                    now,
                    Charge {
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(work)?;
            complete = self.step(now)?;
            check(self.clock.as_ref(), self.deadline, self.meter)?;
            Ok(complete)
        })();
        match result {
            Err(error) => {
                let mut failure = refuse(self.session, error);
                if complete {
                    failure.reply = self.reply.into_reply();
                }
                Err(failure)
            }
            Ok(false) => Ok(Progress::Pending(self)),
            Ok(true) => Ok(Progress::Ready(Reply {
                session: self.session,
                reply: self.reply,
            })),
        }
    }
    fn step(&mut self, now: Tick) -> Result<bool, Error> {
        match self.phase {
            Phase::Write(offset) => {
                let remaining = self.command.get(offset..).ok_or(Error::Invalid)?;
                self.meter
                    .charge(
                        now,
                        Charge {
                            io_bytes: remaining.len() as u64,
                            ..Charge::default()
                        },
                    )
                    .map_err(work)?;
                match self.session.connection_mut().write(remaining)? {
                    IoProgress::Pending => {}
                    IoProgress::Bytes(n) if n > 0 && n <= remaining.len() => {
                        let next = offset.checked_add(n).ok_or(Error::Invalid)?;
                        self.phase = if next == self.command.len() {
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
                if self.session.connection_mut().flush()? == FlushProgress::Complete {
                    self.phase = Phase::Read;
                }
                Ok(false)
            }
            Phase::Read => {
                self.meter
                    .charge(
                        now,
                        Charge {
                            io_bytes: LINE_BYTES as u64,
                            ..Charge::default()
                        },
                    )
                    .map_err(work)?;
                match self.session.connection_mut().read(self.input)? {
                    IoProgress::Pending => Ok(false),
                    IoProgress::Bytes(n) if n > 0 && n <= self.input.len() => {
                        let progress = self
                            .reply
                            .feed(self.input.get(..n).ok_or(Error::Invalid)?)?;
                        if progress.consumed != n {
                            return Err(Error::Invalid);
                        }
                        if progress.complete {
                            self.reply.finish()?;
                        }
                        Ok(progress.complete)
                    }
                    IoProgress::Closed => Err(Error::Tls),
                    _ => Err(Error::Invalid),
                }
            }
        }
    }
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.session.into_buffers()
    }
}
fn frame<'a>(command: Command<'_>, output: &'a mut [u8]) -> Result<&'a [u8], Error> {
    let output = output.get_mut(..LINE_BYTES).ok_or(Error::Capacity)?;
    let (prefix, address, suffix) = match command {
        Command::MailFrom(address) | Command::MailFromEightBit(address) => {
            if !address.is_empty() {
                mailbox_key(address, &mut [0; MAX_ADDRESS]).map_err(|_| Error::Invalid)?;
            }
            (
                b"MAIL FROM:<".as_slice(),
                address.as_bytes(),
                if matches!(command, Command::MailFromEightBit(_)) {
                    b"> BODY=8BITMIME\r\n".as_slice()
                } else {
                    b">\r\n".as_slice()
                },
            )
        }
        Command::RcptTo(address) => {
            mailbox_key(address, &mut [0; MAX_ADDRESS]).map_err(|_| Error::Invalid)?;
            (
                b"RCPT TO:<".as_slice(),
                address.as_bytes(),
                b">\r\n".as_slice(),
            )
        }
        Command::Data => (b"DATA\r\n".as_slice(), b"".as_slice(), b"".as_slice()),
    };
    let mut used = 0usize;
    for part in [prefix, address, suffix] {
        let end = used.checked_add(part.len()).ok_or(Error::Capacity)?;
        output
            .get_mut(used..end)
            .ok_or(Error::Capacity)?
            .copy_from_slice(part);
        used = end;
    }
    output.get(..used).ok_or(Error::Invalid)
}
fn work(stop: Stop) -> Error {
    if stop == Stop::Deadline {
        Error::Deadline
    } else {
        Error::Capacity
    }
}
fn check(clock: &dyn Clock, deadline: Deadline, meter: &mut Meter) -> Result<Tick, Error> {
    let now = clock.sample()?.monotonic;
    if deadline.expired(now) {
        return Err(Error::Deadline);
    }
    meter.charge(now, Charge::default()).map_err(work)?;
    Ok(now)
}
fn refuse<'a, B: TlsWireStorage>(mut session: Session<B>, error: Error) -> Refusal<'a, B> {
    session.connection_mut().abort();
    Refusal {
        error,
        buffers: session.into_buffers(),
        reply: None,
    }
}
const _: () = assert!(
    std::mem::size_of::<Progress<'static, Box<[u8; crate::tls_io::TLS_WIRE_BYTES]>>>() <= 4096
);

const _: () = assert!(
    std::mem::size_of::<Refusal<'static, Box<[u8; crate::tls_io::TLS_WIRE_BYTES]>>>() <= 128
);
