//! Fresh EHLO capability custody on one verified configured relay connection.
use crate::{
    config::{outbound, values::certificate_name},
    ports::{Clock, Deadline, Error, FlushProgress, IoProgress, TlsPolicyId, Transport},
    smtp_wire::{AuthMechanisms, EhloReader, RelayCapabilities, ReplyReader, LINE_BYTES},
    tls_io::TlsWireStorage,
    tls_policy::TlsConnection,
};
use std::sync::Arc;

/// Four distinct startup-reserved control cells; each uses its first 512 bytes.
pub struct Scratch<'a> {
    pub input: &'a mut [u8],
    pub greeting: &'a mut [u8],
    pub reply: &'a mut [u8],
    pub command: &'a mut [u8],
}

enum Phase {
    Greeting,
    Command(usize),
    Flush,
    Reply,
}

#[must_use = "drive the EHLO exchange or recover its reserved buffers"]
pub struct Exchange<'a, B: TlsWireStorage> {
    connection: TlsConnection<B>,
    clock: Arc<dyn Clock>,
    deadline: Deadline,
    phase: Phase,
    input: &'a mut [u8],
    used: usize,
    consumed: usize,
    command: &'a [u8],
    greeting: ReplyReader<'a>,
    ehlo: EhloReader<'a>,
}

#[must_use = "continue the exchange or retain the ready relay session"]
#[allow(
    clippy::large_enum_variant,
    reason = "Inline TLS custody avoids per-turn heap allocation."
)]
pub enum Progress<'a, B: TlsWireStorage> {
    Pending(Exchange<'a, B>),
    Ready(Session<B>),
}

/// The completed post-handshake EHLO and its exclusive verified connection.
/// This is not account/queue authority or proof that authentication succeeded.
pub struct Session<B: TlsWireStorage> {
    connection: TlsConnection<B>,
    capabilities: RelayCapabilities,
}
impl<B: TlsWireStorage> Session<B> {
    pub fn mechanisms(&self) -> AuthMechanisms {
        self.capabilities.auth()
    }
    pub fn capabilities(&self) -> RelayCapabilities {
        self.capabilities
    }
    /// Match exact credentials to this retained relay policy. No AUTH command,
    /// current-generation admission or successful authentication is conferred.
    pub fn check_credentials(&self, username: &str, password: &str) -> Result<(), Error> {
        self.connection.check_relay_credentials(username, password)
    }
    pub fn policy_id(&self) -> TlsPolicyId {
        self.connection.policy_id()
    }
    pub(crate) fn into_connection(self) -> TlsConnection<B> {
        self.connection
    }
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.connection.into_buffers()
    }
}

/// Refusal has torn down TLS and the socket; only reserved buffers remain.
pub struct Refusal<B: TlsWireStorage> {
    pub error: Error,
    buffers: Result<(B, B), Error>,
}
impl<B: TlsWireStorage> Refusal<B> {
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.buffers
    }
}

impl<'a, B: TlsWireStorage> Exchange<'a, B> {
    /// Accept only a completed configured-relay TLS handshake. Implicit TLS
    /// consumes the initial 220; required STARTTLS begins with a fresh EHLO.
    /// Supply the connection clock origin and the original exchange deadline.
    pub fn new(
        connection: TlsConnection<B>,
        hostname: &str,
        scratch: Scratch<'a>,
        clock: Arc<dyn Clock>,
        deadline: Deadline,
    ) -> Result<Self, Refusal<B>> {
        let init = (|scratch: Scratch<'a>| {
            check_time(clock.as_ref(), deadline)?;
            let transport = connection.verified_relay_transport()?;
            certificate_name(hostname).map_err(|_| Error::Invalid)?;
            let input = scratch.input.get_mut(..LINE_BYTES).ok_or(Error::Capacity)?;
            let greeting = ReplyReader::new(scratch.greeting)?;
            let ehlo = EhloReader::new(scratch.reply)?;
            let command = scratch
                .command
                .get_mut(..LINE_BYTES)
                .ok_or(Error::Capacity)?;
            let length = hostname.len().checked_add(7).ok_or(Error::Capacity)?;
            let command = command.get_mut(..length).ok_or(Error::Capacity)?;
            command
                .get_mut(..5)
                .ok_or(Error::Capacity)?
                .copy_from_slice(b"EHLO ");
            command
                .get_mut(5..length - 2)
                .ok_or(Error::Capacity)?
                .copy_from_slice(hostname.as_bytes());
            command
                .get_mut(length - 2..)
                .ok_or(Error::Capacity)?
                .copy_from_slice(b"\r\n");
            let phase = match transport {
                outbound::Transport::ImplicitTls => Phase::Greeting,
                outbound::Transport::RequiredStartTls => Phase::Command(0),
            };
            Ok((input, greeting, ehlo, command, phase))
        })(scratch);
        match init {
            Ok((input, greeting, ehlo, command, phase)) => Ok(Self {
                connection,
                clock,
                deadline,
                phase,
                input,
                used: 0,
                consumed: 0,
                command,
                greeting,
                ehlo,
            }),
            Err(error) => Err(refuse(connection, error)),
        }
    }

    /// One control-line parse and at most one TLS transport operation per turn.
    /// Both deadline checks use the original absolute exchange deadline.
    pub fn advance(mut self) -> Result<Progress<'a, B>, Refusal<B>> {
        let result = (|| {
            check_time(self.clock.as_ref(), self.deadline)?;
            let ready = self.step()?;
            check_time(self.clock.as_ref(), self.deadline)?;
            Ok(ready)
        })();
        match result {
            Err(error) => Err(refuse(self.connection, error)),
            Ok(false) => Ok(Progress::Pending(self)),
            Ok(true) => {
                let Self {
                    connection, ehlo, ..
                } = self;
                match ehlo.into_relay_capabilities(&[]) {
                    Ok(capabilities) => Ok(Progress::Ready(Session {
                        connection,
                        capabilities,
                    })),
                    Err(error) => Err(refuse(connection, error)),
                }
            }
        }
    }
    fn step(&mut self) -> Result<bool, Error> {
        match self.phase {
            Phase::Command(written) => {
                let remaining = self.command.get(written..).ok_or(Error::Invalid)?;
                match self.connection.write(remaining)? {
                    IoProgress::Pending => {}
                    IoProgress::Bytes(count) if count > 0 && count <= remaining.len() => {
                        let next = written.checked_add(count).ok_or(Error::Invalid)?;
                        self.phase = if next == self.command.len() {
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
                if self.connection.flush()? == FlushProgress::Complete {
                    self.phase = Phase::Reply;
                }
                Ok(false)
            }
            Phase::Greeting | Phase::Reply => self.read_reply(),
        }
    }
    fn read_reply(&mut self) -> Result<bool, Error> {
        if self.used == self.consumed {
            match self.connection.read(self.input)? {
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
        let greeting = matches!(self.phase, Phase::Greeting);
        let progress = if greeting {
            self.greeting.feed(input)?
        } else {
            self.ehlo.feed(input)?
        };
        self.consumed = self
            .consumed
            .checked_add(progress.consumed)
            .ok_or(Error::Invalid)?;
        if !progress.complete {
            return Ok(false);
        }
        if greeting && self.greeting.line().is_none_or(|line| line.code != 220) {
            return Err(Error::Tls);
        }
        let complete = if greeting {
            self.greeting.complete()
        } else {
            self.ehlo.complete()
        };
        if complete {
            if self.consumed != self.used {
                return Err(Error::Invalid);
            }
            if greeting {
                self.phase = Phase::Command(0);
                Ok(false)
            } else {
                Ok(true)
            }
        } else {
            if greeting {
                self.greeting.advance()?;
            } else {
                self.ehlo.advance()?;
            }
            Ok(false)
        }
    }
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.connection.into_buffers()
    }
}
fn check_time(clock: &dyn Clock, deadline: Deadline) -> Result<(), Error> {
    if deadline.expired(clock.sample()?.monotonic) {
        Err(Error::Deadline)
    } else {
        Ok(())
    }
}
fn refuse<B: TlsWireStorage>(mut connection: TlsConnection<B>, error: Error) -> Refusal<B> {
    connection.abort();
    Refusal {
        error,
        buffers: connection.into_buffers(),
    }
}

const _: () = assert!(
    std::mem::size_of::<Progress<'static, Box<[u8; crate::tls_io::TLS_WIRE_BYTES]>>>() <= 4096
);
