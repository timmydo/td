//! Shared bounded EHLO syntax driver; wrappers own policy and capability custody.
use crate::{
    config::values::certificate_name,
    ports::{Clock, Deadline, Error, FlushProgress, IoProgress, Transport},
    smtp_wire::{EhloReader, ReplyReader, LINE_BYTES},
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
pub(crate) struct Exchange<'a, T: Transport> {
    connection: T,
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

pub(crate) enum Progress<'a, T: Transport> {
    Pending(Exchange<'a, T>),
    Ready(T, EhloReader<'a>),
}
pub(crate) struct Refusal<T> {
    pub connection: T,
    pub error: Error,
}
impl<'a, T: Transport> Exchange<'a, T> {
    pub fn new(
        connection: T,
        hostname: &str,
        scratch: Scratch<'a>,
        clock: Arc<dyn Clock>,
        deadline: Deadline,
        initial_greeting: bool,
    ) -> Result<Self, Refusal<T>> {
        let init = (|scratch: Scratch<'a>| {
            check_time(clock.as_ref(), deadline)?;
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
            let phase = if initial_greeting {
                Phase::Greeting
            } else {
                Phase::Command(0)
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

    /// One control-line parse and at most one transport operation per turn.
    /// Both deadline checks use the original absolute exchange deadline.
    pub fn advance(mut self) -> Result<Progress<'a, T>, Refusal<T>> {
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
                Ok(Progress::Ready(connection, ehlo))
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
    pub fn into_connection(self) -> T {
        self.connection
    }
}
fn check_time(clock: &dyn Clock, deadline: Deadline) -> Result<(), Error> {
    if deadline.expired(clock.sample()?.monotonic) {
        Err(Error::Deadline)
    } else {
        Ok(())
    }
}
fn refuse<T: Transport>(mut connection: T, error: Error) -> Refusal<T> {
    connection.abort();
    Refusal { connection, error }
}
