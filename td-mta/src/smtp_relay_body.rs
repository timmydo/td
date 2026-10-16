//! Bounded immutable DATA body transfer, stopping before the terminator.
use crate::{
    limits::MAX_MESSAGE_BYTES,
    ports::{BlobReader, Clock, Deadline, Error, FlushProgress, IoProgress, Tick, Transport},
    smtp_data_encode::{self as encode, Body, Encoder, Status},
    smtp_relay_auth::Session,
    tls_io::TlsWireStorage,
};
use std::sync::Arc;
use td_mime::work::{Charge, Meter, Stop};

pub const INPUT_BYTES: usize = 4096;
pub const OUTPUT_BYTES: usize = 16 * 1024;
pub struct Scratch<'a> {
    pub input: &'a mut [u8],
    pub output: &'a mut [u8],
}
enum Phase {
    Read,
    Encode,
    Write(usize),
    Flush,
}
/// The caller owns immutable-source admission, MAIL mode, actual 354 and the
/// durable Body subset fence. This owner supplies none of that authority.
#[must_use = "drive the body transfer or recover its buffers"]
pub struct Exchange<'a, B: TlsWireStorage> {
    session: Session<B>,
    source: &'a mut dyn BlobReader,
    length: u64,
    offset: u64,
    clock: Arc<dyn Clock>,
    deadline: Deadline,
    meter: &'a mut Meter,
    encoder: Option<Encoder>,
    phase: Phase,
    input: &'a mut [u8],
    used: usize,
    consumed: usize,
    output: &'a mut [u8],
    written: usize,
}
#[must_use = "continue transfer or retain the pre-terminator session"]
#[allow(
    clippy::large_enum_variant,
    reason = "Inline TLS custody avoids per-turn heap allocation."
)]
pub enum Progress<'a, B: TlsWireStorage> {
    Pending(Exchange<'a, B>),
    Ready(BodySent<B>),
}
/// Exact declared source bytes were encoded and flushed, without a terminator.
/// This is not AcceptancePossible, remote acceptance or a durable queue result.
pub struct BodySent<B: TlsWireStorage> {
    session: Session<B>,
    clock: Arc<dyn Clock>,
    deadline: Deadline,
}
impl<B: TlsWireStorage> BodySent<B> {
    pub(crate) fn into_parts(self) -> (Session<B>, Arc<dyn Clock>, Deadline) {
        (self.session, self.clock, self.deadline)
    }
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.session.into_buffers()
    }
}
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
    pub(crate) fn writes_pending(&self) -> bool {
        matches!(self.phase, Phase::Write(_) | Phase::Flush)
    }
    pub fn new(
        session: Session<B>,
        source: &'a mut dyn BlobReader,
        body: Body,
        scratch: Scratch<'a>,
        clock: Arc<dyn Clock>,
        deadline: Deadline,
        meter: &'a mut Meter,
    ) -> Result<Self, Refusal<B>> {
        let init = (|scratch: Scratch<'a>| {
            let now = check(clock.as_ref(), deadline, meter)?;
            if body == Body::EightBit && !session.capabilities().eight_bit_mime() {
                return Err(Error::Forbidden);
            }
            let length = source.len();
            if length > MAX_MESSAGE_BYTES as u64 {
                return Err(Error::Capacity);
            }
            let input = scratch
                .input
                .get_mut(..INPUT_BYTES)
                .ok_or(Error::Capacity)?;
            let output = scratch
                .output
                .get_mut(..OUTPUT_BYTES)
                .ok_or(Error::Capacity)?;
            let encoder = Encoder::new(length, body, now, meter).map_err(codec)?;
            check(clock.as_ref(), deadline, meter)?;
            Ok((length, input, output, encoder))
        })(scratch);
        match init {
            Err(error) => Err(refuse(session, error)),
            Ok((length, input, output, encoder)) => Ok(Self {
                session,
                source,
                length,
                offset: 0,
                clock,
                deadline,
                meter,
                encoder: Some(encoder),
                phase: Phase::Read,
                input,
                used: 0,
                consumed: 0,
                output,
                written: 0,
            }),
        }
    }
    /// One bounded source read, codec poll or TLS operation per turn. Source
    /// implementations must keep their own I/O within the original deadline.
    pub fn advance(mut self) -> Result<Progress<'a, B>, Refusal<B>> {
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
            let ready = self.step(now)?;
            check(self.clock.as_ref(), self.deadline, self.meter)?;
            Ok(ready)
        })();
        match result {
            Err(error) => Err(refuse(self.session, error)),
            Ok(false) => Ok(Progress::Pending(self)),
            Ok(true) => Ok(Progress::Ready(BodySent {
                session: self.session,
                clock: self.clock,
                deadline: self.deadline,
            })),
        }
    }
    fn step(&mut self, now: Tick) -> Result<bool, Error> {
        match self.phase {
            Phase::Read => {
                if self.source.len() != self.length {
                    return Err(Error::Invalid);
                }
                let remaining = self.length.checked_sub(self.offset).ok_or(Error::Invalid)?;
                let size = remaining.min(INPUT_BYTES as u64) as usize;
                self.used = 0;
                self.consumed = 0;
                if size > 0 {
                    self.meter
                        .charge(
                            now,
                            Charge {
                                io_bytes: size as u64,
                                ..Charge::default()
                            },
                        )
                        .map_err(work)?;
                    let input = self.input.get_mut(..size).ok_or(Error::Capacity)?;
                    let n = self.source.read_at(self.offset, input)?;
                    if n == 0 || n > size {
                        return Err(Error::Invalid);
                    }
                    self.offset = self.offset.checked_add(n as u64).ok_or(Error::Invalid)?;
                    self.used = n;
                }
                self.phase = Phase::Encode;
                Ok(false)
            }
            Phase::Encode => {
                let input = self
                    .input
                    .get(self.consumed..self.used)
                    .ok_or(Error::Invalid)?;
                let progress = self
                    .encoder
                    .as_mut()
                    .ok_or(Error::Invalid)?
                    .poll(
                        input,
                        self.output.get_mut(self.written..).ok_or(Error::Invalid)?,
                        now,
                        self.meter,
                    )
                    .map_err(codec)?;
                self.consumed = self
                    .consumed
                    .checked_add(progress.consumed)
                    .ok_or(Error::Invalid)?;
                self.written = self
                    .written
                    .checked_add(progress.written)
                    .ok_or(Error::Invalid)?;
                if progress.status == Status::Complete {
                    if self.offset != self.length || self.consumed != self.used {
                        return Err(Error::Invalid);
                    }
                    self.encoder
                        .take()
                        .ok_or(Error::Invalid)?
                        .finish(now, self.meter)
                        .map_err(codec)?;
                }
                self.phase = if self.written == self.output.len()
                    || (self.encoder.is_none() && self.written > 0)
                {
                    Phase::Write(0)
                } else {
                    self.next_phase()
                };
                Ok(false)
            }
            Phase::Write(offset) => {
                let remaining = self
                    .output
                    .get(offset..self.written)
                    .ok_or(Error::Invalid)?;
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
                        self.phase = if next == self.written {
                            self.written = 0;
                            self.next_phase()
                        } else {
                            Phase::Write(next)
                        };
                    }
                    _ => return Err(Error::Invalid),
                }
                Ok(false)
            }
            Phase::Flush => {
                if self.source.len() != self.length {
                    return Err(Error::Invalid);
                }
                Ok(self.session.connection_mut().flush()? == FlushProgress::Complete)
            }
        }
    }
    fn next_phase(&self) -> Phase {
        if self.encoder.is_none() {
            Phase::Flush
        } else if self.consumed == self.used && self.offset < self.length {
            Phase::Read
        } else {
            Phase::Encode
        }
    }
    pub fn into_buffers(self) -> Result<(B, B), Error> {
        self.session.into_buffers()
    }
}
fn work(stop: Stop) -> Error {
    if stop == Stop::Deadline {
        Error::Deadline
    } else {
        Error::Capacity
    }
}
fn codec(error: encode::Error) -> Error {
    match error {
        encode::Error::Work(stop) => work(stop),
        _ => Error::Invalid,
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
fn refuse<B: TlsWireStorage>(mut session: Session<B>, error: Error) -> Refusal<B> {
    session.connection_mut().abort();
    Refusal {
        error,
        buffers: session.into_buffers(),
    }
}
const _: () = assert!(
    std::mem::size_of::<Progress<'static, Box<[u8; crate::tls_io::TLS_WIRE_BYTES]>>>() <= 4096
);
