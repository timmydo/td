//! Body transfer with exact native phase custody and reserved final capacity.
use crate::{
    admission::work::Meter,
    ports::{self, BlobReader, Clock, Deadline},
    postgres_dispatch::{self as dispatch, BodyPlan, BodyTransferPlan, FinalResult},
    postgres_relay_finish as finish,
    smtp_data_encode::Body,
    smtp_relay_auth::Session,
    smtp_relay_body::{self as body, BodySent, Scratch},
    smtp_relay_command,
    tls_io::TlsWireStorage,
};
use std::{sync::Arc, time::Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Protocol(ports::Error),
    Publication(dispatch::Error),
}
pub struct Refusal<B: TlsWireStorage> {
    pub error: Error,
    pub buffers: Result<(B, B), ports::Error>,
}
/// Matching completed body and known new final-result custody. The pair cannot
/// be separated or attached to another attempt through this owner.
pub struct Completed<'w, 'a, 'r, B: TlsWireStorage> {
    body: BodySent<B>,
    result: FinalResult<'w, 'a, 'r>,
}
impl<'w, 'a, 'r, B: TlsWireStorage> Completed<'w, 'a, 'r, B> {
    pub fn begin_final<'m>(
        self,
        scratch: smtp_relay_command::Scratch<'m>,
        wire: &'m mut Meter,
        database: &'m mut Meter,
    ) -> Result<finish::Exchange<'m, 'w, 'a, 'r, B>, finish::Refusal<'w, 'a, 'r, B>> {
        finish::begin(self.result, self.body, scratch, wire, database)
    }
    pub fn into_buffers(self) -> Result<(B, B), ports::Error> {
        self.body.into_buffers()
    }
}
#[must_use = "drive body transfer or close its transport"]
pub struct Exchange<'m, 'w, 'a, 'r, B: TlsWireStorage> {
    inner: Engine<'m, B, BodyTransferPlan<'w, 'a, 'r>>,
}
#[must_use = "continue body transfer or retain its final handoff"]
#[allow(
    clippy::large_enum_variant,
    reason = "Inline TLS custody avoids per-turn heap allocation."
)]
pub enum Progress<'m, 'w, 'a, 'r, B: TlsWireStorage> {
    Pending(Exchange<'m, 'w, 'a, 'r, B>),
    Ready(Completed<'w, 'a, 'r, B>),
}
/// The trusted caller owns the matching immutable source, authenticated
/// session, actual 354 and compatible MAIL mode. Final capacity is already
/// bound to Body; both original work meters remain borrowed through transfer.
pub fn begin<'m, 'w, 'a, 'r, B: TlsWireStorage>(
    plan: BodyPlan<'w, 'a, 'r>,
    session: Session<B>,
    source: &'m mut dyn BlobReader,
    mode: Body,
    scratch: Scratch<'m>,
    wire: &'m mut Meter,
    database: &'m mut Meter,
) -> Result<Exchange<'m, 'w, 'a, 'r, B>, Refusal<B>> {
    let plan = match plan.reserve_checks(database) {
        Ok(plan) => plan,
        Err(error) => {
            return Err(Refusal {
                error: Error::Publication(error),
                buffers: session.into_buffers(),
            })
        }
    };
    Engine::new(plan, session, source, mode, scratch, wire, database)
        .map(|inner| Exchange { inner })
}
impl<'m, 'w, 'a, 'r, B: TlsWireStorage> Exchange<'m, 'w, 'a, 'r, B> {
    pub fn advance(self) -> Result<Progress<'m, 'w, 'a, 'r, B>, Refusal<B>> {
        match self.inner.advance()? {
            State::Pending(inner) => Ok(Progress::Pending(Self { inner })),
            State::Ready(done) => Ok(Progress::Ready(Completed {
                body: done.body,
                result: done.result,
            })),
        }
    }
    pub fn into_buffers(self) -> Result<(B, B), ports::Error> {
        self.inner.exchange.into_buffers()
    }
}
trait Authority: Sized {
    type Final;
    fn clock(&self) -> Arc<dyn Clock>;
    fn deadline(&self) -> Deadline;
    fn limit(&self) -> Instant;
    fn current(&mut self, meter: &mut Meter) -> Result<Instant, dispatch::Error>;
    fn possible(self, meter: &mut Meter) -> Result<Self::Final, dispatch::Error>;
}
impl<'w, 'a, 'r> Authority for BodyTransferPlan<'w, 'a, 'r> {
    type Final = FinalResult<'w, 'a, 'r>;
    fn clock(&self) -> Arc<dyn Clock> {
        self.runtime_clock()
    }
    fn deadline(&self) -> Deadline {
        self.original_deadline()
    }
    fn limit(&self) -> Instant {
        self.work_limit()
    }
    fn current(&mut self, meter: &mut Meter) -> Result<Instant, dispatch::Error> {
        self.check_current(meter)
    }
    fn possible(self, meter: &mut Meter) -> Result<Self::Final, dispatch::Error> {
        self.publish_possible(meter)
    }
}
struct Engine<'m, B: TlsWireStorage, A: Authority> {
    plan: A,
    exchange: body::Exchange<'m, B>,
    database: &'m mut Meter,
    limit: Instant,
}
struct Done<B: TlsWireStorage, F> {
    body: BodySent<B>,
    result: F,
}
#[allow(
    clippy::large_enum_variant,
    reason = "Inline TLS custody avoids per-turn heap allocation."
)]
enum State<E, B: TlsWireStorage, F> {
    Pending(E),
    Ready(Done<B, F>),
}
impl<'m, B: TlsWireStorage, A: Authority> Engine<'m, B, A> {
    fn new(
        plan: A,
        session: Session<B>,
        source: &'m mut dyn BlobReader,
        mode: Body,
        scratch: Scratch<'m>,
        wire: &'m mut Meter,
        database: &'m mut Meter,
    ) -> Result<Self, Refusal<B>> {
        let limit = plan.limit();
        if Instant::now() >= limit {
            return Err(Refusal {
                error: Error::Protocol(ports::Error::Deadline),
                buffers: session.into_buffers(),
            });
        }
        let exchange = body::Exchange::new(
            session,
            source,
            mode,
            scratch,
            plan.clock(),
            plan.deadline(),
            wire,
        )
        .map_err(|e| Refusal {
            error: Error::Protocol(e.error),
            buffers: e.into_buffers(),
        })?;
        Ok(Self {
            plan,
            exchange,
            database,
            limit,
        })
    }
    fn advance(mut self) -> Result<State<Self, B, A::Final>, Refusal<B>> {
        if Instant::now() >= self.limit {
            return Err(self.refuse(Error::Protocol(ports::Error::Deadline)));
        }
        if self.exchange.writes_pending() {
            match self.plan.current(self.database) {
                Ok(limit) => self.limit = self.limit.min(limit),
                Err(error) => return Err(self.refuse(Error::Publication(error))),
            }
            if Instant::now() >= self.limit {
                return Err(self.refuse(Error::Protocol(ports::Error::Deadline)));
            }
        }
        match self.exchange.advance() {
            Err(e) => Err(Refusal {
                error: Error::Protocol(e.error),
                buffers: e.into_buffers(),
            }),
            Ok(body::Progress::Pending(exchange)) => {
                let next = Self { exchange, ..self };
                if Instant::now() >= next.limit {
                    return Err(next.refuse(Error::Protocol(ports::Error::Deadline)));
                }
                Ok(State::Pending(next))
            }
            Ok(body::Progress::Ready(body)) => {
                if Instant::now() >= self.limit {
                    return Err(Refusal {
                        error: Error::Protocol(ports::Error::Deadline),
                        buffers: body.into_buffers(),
                    });
                }
                match self.plan.possible(self.database) {
                    Ok(result) => Ok(State::Ready(Done { body, result })),
                    Err(error) => Err(Refusal {
                        error: Error::Publication(error),
                        buffers: body.into_buffers(),
                    }),
                }
            }
        }
    }
    fn refuse(self, error: Error) -> Refusal<B> {
        Refusal {
            error,
            buffers: self.exchange.into_buffers(),
        }
    }
}

#[cfg(test)]
#[path = "postgres_relay_body_tests.rs"]
mod tests;
