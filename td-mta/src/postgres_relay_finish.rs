//! Final DATA handoff with retained native result custody and live admission.
use crate::{
    admission::work::Meter,
    ports::{self, Clock},
    postgres_attempt_phases::Outcome,
    postgres_dispatch::{self as dispatch, FinalResult},
    smtp_relay_body::BodySent,
    smtp_relay_command::{self as command, Scratch},
    tls_io::TlsWireStorage,
};
use std::{sync::Arc, time::Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Protocol(ports::Error),
    Publication(dispatch::Error),
}
/// A closed transport and, when available, retained actual final evidence.
pub struct Refusal<'w, 'a, 'r, B: TlsWireStorage> {
    pub error: Error,
    pub retained: Option<dispatch::FinalFailure<'w, 'a, 'r>>,
    pub buffers: Result<(B, B), ports::Error>,
}
struct Failure<B: TlsWireStorage, F> {
    error: Error,
    retained: Option<F>,
    buffers: Result<(B, B), ports::Error>,
}
impl<'w, 'a, 'r, B: TlsWireStorage> From<Failure<B, dispatch::FinalFailure<'w, 'a, 'r>>>
    for Refusal<'w, 'a, 'r, B>
{
    fn from(value: Failure<B, dispatch::FinalFailure<'w, 'a, 'r>>) -> Self {
        Self {
            error: value.error,
            retained: value.retained,
            buffers: value.buffers,
        }
    }
}
/// A durably published actual final reply. Neither this receipt nor buffers
/// permit another SMTP transaction; the connection has been closed.
pub struct Completed<B: TlsWireStorage> {
    pub outcome: Outcome,
    buffers: Result<(B, B), ports::Error>,
}
impl<B: TlsWireStorage> Completed<B> {
    pub fn into_buffers(self) -> Result<(B, B), ports::Error> {
        self.buffers
    }
}
#[must_use = "drive the final exchange or close its transport"]
pub struct Exchange<'m, 'w, 'a, 'r, B: TlsWireStorage> {
    inner: Engine<'m, B, FinalResult<'w, 'a, 'r>>,
}
#[must_use = "continue final handoff or retain its durable result"]
#[allow(
    clippy::large_enum_variant,
    reason = "Inline TLS custody avoids per-turn heap allocation."
)]
pub enum Progress<'m, 'w, 'a, 'r, B: TlsWireStorage> {
    Pending(Exchange<'m, 'w, 'a, 'r, B>),
    Ready(Completed<B>),
}
/// The trusted runtime must supply the completed body transaction for this
/// exact attempt/subset. Body-source and pre-body phase admission remain its
/// responsibility. Both original meters are borrowed through completion.
pub fn begin<'m, 'w, 'a, 'r, B: TlsWireStorage>(
    result: FinalResult<'w, 'a, 'r>,
    body: BodySent<B>,
    scratch: Scratch<'m>,
    wire: &'m mut Meter,
    database: &'m mut Meter,
) -> Result<Exchange<'m, 'w, 'a, 'r, B>, Refusal<'w, 'a, 'r, B>> {
    Engine::new(result, body, scratch, wire, database)
        .map(|inner| Exchange { inner })
        .map_err(Into::into)
}
impl<'m, 'w, 'a, 'r, B: TlsWireStorage> Exchange<'m, 'w, 'a, 'r, B> {
    /// One bounded database admission before a write/flush turn, or one bounded
    /// reply-read turn. Only an actual final reply reaches result publication.
    pub fn advance(self) -> Result<Progress<'m, 'w, 'a, 'r, B>, Refusal<'w, 'a, 'r, B>> {
        match self.inner.advance()? {
            State::Pending(inner) => Ok(Progress::Pending(Self { inner })),
            State::Ready(done) => Ok(Progress::Ready(done)),
        }
    }
    pub fn into_buffers(self) -> Result<(B, B), ports::Error> {
        self.inner.command.into_buffers()
    }
}
trait Authority: Sized {
    type Failure;
    fn error(failure: &Self::Failure) -> dispatch::Error;
    fn clock(&self) -> Arc<dyn Clock>;
    fn limit(&self) -> Instant;
    fn current(&self, meter: &mut Meter) -> Result<Instant, dispatch::Error>;
    fn record(self, reply: &str) -> Result<Outcome, Self::Failure>;
}
impl<'w, 'a, 'r> Authority for FinalResult<'w, 'a, 'r> {
    type Failure = dispatch::FinalFailure<'w, 'a, 'r>;
    fn error(failure: &Self::Failure) -> dispatch::Error {
        failure.error
    }
    fn clock(&self) -> Arc<dyn Clock> {
        self.runtime_clock()
    }
    fn limit(&self) -> Instant {
        self.work_limit()
    }
    fn current(&self, meter: &mut Meter) -> Result<Instant, dispatch::Error> {
        self.check_current(meter)
    }
    fn record(self, reply: &str) -> Result<Outcome, Self::Failure> {
        FinalResult::record(self, reply)
    }
}
struct Engine<'m, B: TlsWireStorage, A: Authority> {
    result: A,
    command: command::Exchange<'m, B>,
    database: &'m mut Meter,
    limit: Instant,
}
#[allow(
    clippy::large_enum_variant,
    reason = "Inline TLS custody avoids per-turn heap allocation."
)]
enum State<E, B: TlsWireStorage> {
    Pending(E),
    Ready(Completed<B>),
}
impl<'m, B: TlsWireStorage, A: Authority> Engine<'m, B, A> {
    fn new(
        result: A,
        body: BodySent<B>,
        scratch: Scratch<'m>,
        wire: &'m mut Meter,
        database: &'m mut Meter,
    ) -> Result<Self, Failure<B, A::Failure>> {
        let (session, clock, deadline) = body.into_parts();
        let limit = result.limit();
        if !Arc::ptr_eq(&clock, &result.clock()) {
            return Err(Failure {
                retained: None,
                error: Error::Protocol(ports::Error::Invalid),
                buffers: session.into_buffers(),
            });
        }
        if Instant::now() >= limit {
            return Err(Failure {
                retained: None,
                error: Error::Protocol(ports::Error::Deadline),
                buffers: session.into_buffers(),
            });
        }
        let command = command::Exchange::new_final(session, scratch, clock, deadline, wire)
            .map_err(|e| Failure {
                retained: None,
                error: Error::Protocol(e.error),
                buffers: e.into_buffers(),
            })?;
        Ok(Self {
            result,
            command,
            database,
            limit,
        })
    }
    fn advance(mut self) -> Result<State<Self, B>, Failure<B, A::Failure>> {
        if Instant::now() >= self.limit {
            return Err(self.refuse(Error::Protocol(ports::Error::Deadline)));
        }
        if self.command.writes_pending() {
            match self.result.current(self.database) {
                Ok(limit) => self.limit = self.limit.min(limit),
                Err(error) => return Err(self.refuse(Error::Publication(error))),
            }
            if Instant::now() >= self.limit {
                return Err(self.refuse(Error::Protocol(ports::Error::Deadline)));
            }
        }
        match self.command.advance() {
            Err(e) => Err(Failure {
                retained: None,
                error: Error::Protocol(e.error),
                buffers: e.into_buffers(),
            }),
            Ok(command::Progress::Pending(command)) => {
                let next = Self { command, ..self };
                if Instant::now() >= next.limit {
                    return Err(next.refuse(Error::Protocol(ports::Error::Deadline)));
                }
                Ok(State::Pending(next))
            }
            Ok(command::Progress::Ready(reply)) => Self::finish(self.result, reply),
        }
    }
    fn finish(
        result: A,
        reply: command::Reply<'_, B>,
    ) -> Result<State<Self, B>, Failure<B, A::Failure>> {
        // A complete actual final reply belongs to retained publication even
        // if scheduling consumed the remaining deadline after parsing it.
        let valid = (|| {
            let code = reply.code()?;
            if !matches!(code / 100, 2 | 4 | 5) {
                return Err(ports::Error::Invalid);
            }
            reply.text()
        })();
        let text = match valid {
            Ok(text) => text,
            Err(error) => {
                return Err(Failure {
                    retained: None,
                    error: Error::Protocol(error),
                    buffers: reply.into_buffers(),
                })
            }
        };
        let outcome = result.record(text);
        let buffers = reply.into_buffers();
        match outcome {
            Ok(outcome) => Ok(State::Ready(Completed { outcome, buffers })),
            Err(failure) => Err(Failure {
                error: Error::Publication(A::error(&failure)),
                retained: Some(failure),
                buffers,
            }),
        }
    }
    fn refuse(self, error: Error) -> Failure<B, A::Failure> {
        Failure {
            retained: None,
            error,
            buffers: self.command.into_buffers(),
        }
    }
}

#[cfg(test)]
#[path = "postgres_relay_finish_tests.rs"]
mod tests;
