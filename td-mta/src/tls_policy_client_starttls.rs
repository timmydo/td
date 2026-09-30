//! This owner keeps the prepared ClientHello private until a complete 220.
use super::{Binding, SessionRefusal, SessionReservation, TlsConnection, TlsPolicies};
use crate::{
    config::outbound,
    ports::{Clock, Deadline, Error, FlushProgress, IoProgress, Transport},
    smtp_wire::{ReplyReader, StartTlsOffer, LINE_BYTES},
    tls_io::TlsWireStorage,
    transport::TcpTransport,
};
use std::sync::Arc;

const COMMAND: &[u8] = b"STARTTLS\r\n";

/// Borrow two distinct startup reservations; only their first 512 bytes are used.
pub struct UpgradeScratch<'a> {
    pub input: &'a mut [u8],
    pub reply: &'a mut [u8],
}

enum Phase {
    Command(usize),
    Flush,
    Reply,
}

#[must_use = "drive the upgrade or recover its reserved buffers"]
pub struct ClientStartTls<'a, B: TlsWireStorage> {
    plain: TcpTransport,
    prepared: SessionReservation<B>,
    clock: Arc<dyn Clock>,
    deadline: Deadline,
    handshake_deadline: Deadline,
    phase: Phase,
    input: &'a mut [u8],
    used: usize,
    consumed: usize,
    reply: ReplyReader<'a>,
}

#[must_use = "continue progress or drive the returned TLS handshake"]
pub enum ClientUpgradeProgress<'a, B: TlsWireStorage> {
    Pending(ClientStartTls<'a, B>),
    /// SMTP 220 has been consumed without a tail; TLS is not authenticated yet.
    Tls(TlsConnection<B>),
}

impl<'a, B: TlsWireStorage> ClientStartTls<'a, B> {
    /// The driver supplies an offer from this socket's EHLO reply and owns no
    /// other plaintext input/output. Only configured required-STARTTLS relays fit.
    pub fn new(
        _offer: StartTlsOffer,
        prepared: SessionReservation<B>,
        plain: TcpTransport,
        scratch: UpgradeScratch<'a>,
        clock: Arc<dyn Clock>,
        deadline: Deadline,
        handshake_deadline: Deadline,
    ) -> Result<Self, SessionRefusal<B>> {
        let init = (|scratch: UpgradeScratch<'a>| {
            if handshake_deadline.tick() > deadline.tick() {
                return Err(Error::Invalid);
            }
            let policy = TlsPolicies::resolve(&prepared.lease, prepared.id)?;
            if !matches!(
                policy.binding,
                Binding::Relay {
                    transport: outbound::Transport::RequiredStartTls,
                    ..
                }
            ) {
                return Err(Error::Invalid);
            }
            check_time(clock.as_ref(), deadline, handshake_deadline)?;
            let input = scratch.input.get_mut(..LINE_BYTES).ok_or(Error::Capacity)?;
            let reply = ReplyReader::new(scratch.reply)?;
            Ok((input, reply))
        })(scratch);
        match init {
            Err(error) => Err(refuse(plain, prepared, error)),
            Ok((input, reply)) => Ok(Self {
                plain,
                prepared,
                clock,
                deadline,
                handshake_deadline,
                phase: Phase::Command(0),
                input,
                used: 0,
                consumed: 0,
                reply,
            }),
        }
    }

    /// At most one socket operation and one line of parsing, with fixed pre/post
    /// deadlines. Reply continuation bytes remain in the same caller reservation.
    pub fn advance(mut self) -> Result<ClientUpgradeProgress<'a, B>, SessionRefusal<B>> {
        let result = (|| {
            check_time(self.clock.as_ref(), self.deadline, self.handshake_deadline)?;
            let ready = self.step()?;
            check_time(self.clock.as_ref(), self.deadline, self.handshake_deadline)?;
            Ok(ready)
        })();
        match result {
            Err(error) => Err(refuse(self.plain, self.prepared, error)),
            Ok(false) => Ok(ClientUpgradeProgress::Pending(self)),
            Ok(true) => {
                let Self {
                    plain,
                    prepared,
                    clock,
                    deadline,
                    handshake_deadline,
                    ..
                } = self;
                prepared
                    .handoff(plain, &[], clock, deadline, handshake_deadline)
                    .map(ClientUpgradeProgress::Tls)
            }
        }
    }

    fn step(&mut self) -> Result<bool, Error> {
        match self.phase {
            Phase::Command(written) => {
                let remaining = COMMAND.get(written..).ok_or(Error::Invalid)?;
                match self.plain.write(remaining)? {
                    IoProgress::Pending => {}
                    IoProgress::Bytes(count) if count > 0 && count <= remaining.len() => {
                        let next = written.checked_add(count).ok_or(Error::Invalid)?;
                        self.phase = if next == COMMAND.len() {
                            Phase::Flush
                        } else {
                            Phase::Command(next)
                        };
                    }
                    _ => return Err(Error::Invalid),
                }
                Ok(false)
            }
            Phase::Flush => {
                if self.plain.flush()? == FlushProgress::Complete {
                    self.phase = Phase::Reply;
                }
                Ok(false)
            }
            Phase::Reply => self.read_reply(),
        }
    }

    fn read_reply(&mut self) -> Result<bool, Error> {
        if self.consumed == self.used {
            match self.plain.read(self.input)? {
                IoProgress::Pending => return Ok(false),
                IoProgress::Bytes(count) if count > 0 && count <= self.input.len() => {
                    self.used = count;
                    self.consumed = 0;
                }
                IoProgress::Closed => return Err(Error::Tls),
                _ => return Err(Error::Invalid),
            }
        }
        let input = self
            .input
            .get(self.consumed..self.used)
            .ok_or(Error::Invalid)?;
        let progress = self.reply.feed(input)?;
        self.consumed = self
            .consumed
            .checked_add(progress.consumed)
            .ok_or(Error::Invalid)?;
        if !progress.complete {
            return Ok(false);
        }
        if self.reply.line().is_none_or(|line| line.code != 220) {
            return Err(Error::Tls);
        }
        if self.reply.complete() {
            if self.consumed != self.used {
                return Err(Error::Invalid);
            }
            return Ok(true);
        }
        self.reply.advance()?;
        Ok(false)
    }

    pub fn into_buffers(mut self) -> (B, B) {
        self.plain.abort();
        self.prepared.into_buffers()
    }
}

fn refuse<B: TlsWireStorage>(
    mut plain: TcpTransport,
    prepared: SessionReservation<B>,
    error: Error,
) -> SessionRefusal<B> {
    plain.abort();
    let (input, output) = prepared.into_buffers();
    SessionRefusal {
        error,
        input,
        output,
    }
}

fn check_time(clock: &dyn Clock, deadline: Deadline, handshake: Deadline) -> Result<(), Error> {
    let now = clock.sample()?.monotonic;
    if deadline.expired(now) || handshake.expired(now) {
        Err(Error::Deadline)
    } else {
        Ok(())
    }
}
