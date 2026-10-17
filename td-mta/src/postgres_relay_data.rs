//! DATA command tied to complete ordered RCPT evidence and its same session.
use crate::{
    admission::work::Meter,
    ports,
    postgres_attempt_phases::Outcome,
    postgres_attempts::{Dispatch, ReadError},
    postgres_dispatch::{self as dispatch, BodyResult, RcptDataPlan},
    postgres_read::ReadSlots,
    postgres_relay_body as transfer, postgres_relay_rcpt,
    smtp_data_encode::Body,
    smtp_relay_auth::Session,
    smtp_relay_body,
    smtp_relay_command::{self as command, Scratch},
    tls_io::TlsWireStorage,
};
use std::time::Instant;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Protocol(ports::Error),
    Publication(dispatch::Error),
}
impl Error {
    fn publication(self) -> dispatch::Error {
        match self {
            Self::Protocol(error) => dispatch::Error::Admission(error),
            Self::Publication(error) => error,
        }
    }
}
/// Every variant retains passive settlement/planning custody after closure.
pub enum Retained<'w, 'a, 'r> {
    Planning(dispatch::RcptFailure<'w, 'a, 'r>),
    Body(dispatch::RcptBodyFailure<'w, 'a, 'r>),
    Command(dispatch::RcptCommandFailure<'w, 'a, 'r>),
}
pub struct Refusal<'w, 'a, 'r, B: TlsWireStorage> {
    pub error: Error,
    pub retained: Retained<'w, 'a, 'r>,
    pub buffers: Result<(B, B), ports::Error>,
}
/// Known new observed Body publication and the session that supplied actual 354.
pub struct Completed<'w, 'a, 'r, B: TlsWireStorage> {
    body: BodyResult<'w, 'a, 'r>,
    dispatch: Dispatch,
    session: Session<B>,
    mode: Body,
}
impl<'w, 'a, 'r, B: TlsWireStorage> Completed<'w, 'a, 'r, B> {
    /// Reserve final capacity without separating actual DATA custody from its
    /// session and MAIL mode. Failure closes transport before returning.
    pub fn reserve_body(
        self,
        database: &mut Meter,
    ) -> Result<BodyReady<'w, 'a, 'r, B>, transfer::Refusal<B>> {
        let plan = match self.body.reserve_final(database) {
            Ok(plan) => plan,
            Err(error) => {
                return Err(transfer::Refusal {
                    error: transfer::Error::Publication(error),
                    buffers: self.session.into_buffers(),
                });
            }
        };
        Ok(BodyReady {
            plan,
            dispatch: self.dispatch,
            session: self.session,
            mode: self.mode,
        })
    }
    pub fn into_buffers(self) -> Result<(B, B), ports::Error> {
        self.session.into_buffers()
    }
}
/// Actual DATA custody and its exact final reservation, still tied to the same
/// session and MAIL mode. Source admission selects its exact immutable body.
pub struct BodyReady<'w, 'a, 'r, B: TlsWireStorage> {
    plan: dispatch::BodyPlan<'w, 'a, 'r>,
    dispatch: Dispatch,
    session: Session<B>,
    mode: Body,
}
impl<'w, 'a, 'r, B: TlsWireStorage> BodyReady<'w, 'a, 'r, B> {
    pub fn dispatch(&self) -> &Dispatch {
        &self.dispatch
    }
    pub fn check_current(&self, database: &mut Meter) -> Result<Instant, dispatch::Error> {
        self.plan.check_current(database)
    }
    /// Verify and replay this attempt's immutable body. Final SMTP exchange
    /// starts only after this scope succeeds and releases its source slots.
    /// The callback drives SourceProgress::Ready before returning Ok(()).
    pub fn with_source(
        mut self,
        slots: &ReadSlots,
        source_work: &mut Meter,
        maximum: u64,
        read: impl for<'s> FnOnce(
            BodySourceReady<'s, 'w, 'a, 'r, B>,
        ) -> Result<(), transfer::Refusal<B>>,
    ) -> Result<transfer::Completed<'w, 'a, 'r, B>, SourceRefusal<B>> {
        let work = match self.plan.reserve_source(slots, source_work) {
            Ok(work) => work,
            Err(error) => {
                return Err(SourceRefusal {
                    error: SourceError::Admission(error),
                    buffers: self.into_buffers(),
                })
            }
        };
        let dispatch = self.dispatch.clone();
        let mut owner = Some(self);
        let mut completion = None;
        let mut callback = None;
        let result = work.with_body(&dispatch, maximum, |_, source| {
            let ready = owner.take().ok_or(ports::Error::Invalid)?;
            callback = Some(read(BodySourceReady {
                ready,
                source,
                completion: &mut completion,
            }));
            Ok(())
        });
        if let Some(Err(refusal)) = callback {
            let buffers = completion.map_or(refusal.buffers, |done| done.into_buffers());
            return Err(SourceRefusal {
                error: SourceError::Transfer(refusal.error),
                buffers,
            });
        }
        match (result, completion) {
            (Ok(()), Some(done)) => Ok(done),
            (Err(error), Some(done)) => Err(SourceRefusal {
                error: SourceError::Read(error),
                buffers: done.into_buffers(),
            }),
            (result, None) => Err(SourceRefusal {
                error: match result {
                    Err(error) => SourceError::Read(error),
                    Ok(()) => {
                        SourceError::Admission(dispatch::Error::Admission(ports::Error::Invalid))
                    }
                },
                buffers: owner.map_or(Err(ports::Error::Invalid), Self::into_buffers),
            }),
        }
    }
    pub fn into_buffers(self) -> Result<(B, B), ports::Error> {
        self.session.into_buffers()
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceError {
    Admission(dispatch::Error),
    Read(ReadError),
    Transfer(transfer::Error),
}
pub struct SourceRefusal<B: TlsWireStorage> {
    pub error: SourceError,
    pub buffers: Result<(B, B), ports::Error>,
}
/// Callback-scoped verified source, inseparable from its actual DATA session.
pub struct BodySourceReady<'s, 'w, 'a, 'r, B: TlsWireStorage> {
    ready: BodyReady<'w, 'a, 'r, B>,
    source: &'s mut dyn ports::BlobReader,
    completion: &'s mut Option<transfer::Completed<'w, 'a, 'r, B>>,
}
impl<'s, 'w, 'a, 'r, B: TlsWireStorage> BodySourceReady<'s, 'w, 'a, 'r, B> {
    pub fn begin<'m>(
        self,
        scratch: smtp_relay_body::Scratch<'m>,
        wire: &'m mut Meter,
        database: &'m mut Meter,
    ) -> Result<SourceExchange<'m, 'w, 'a, 'r, B>, transfer::Refusal<B>>
    where
        's: 'm,
    {
        transfer::begin(
            self.ready.plan,
            self.ready.session,
            self.source,
            self.ready.mode,
            scratch,
            wire,
            database,
        )
        .map(|inner| SourceExchange {
            inner,
            completion: self.completion,
        })
    }
    pub fn into_buffers(self) -> Result<(B, B), ports::Error> {
        self.ready.into_buffers()
    }
}
/// A source-scoped transfer cannot expose final SMTP authority to its callback.
#[must_use = "drive scoped body transfer or close transport"]
pub struct SourceExchange<'m, 'w, 'a, 'r, B: TlsWireStorage> {
    inner: transfer::Exchange<'m, 'w, 'a, 'r, B>,
    completion: &'m mut Option<transfer::Completed<'w, 'a, 'r, B>>,
}
#[must_use = "continue scoped transfer or acknowledge completion"]
#[allow(
    clippy::large_enum_variant,
    reason = "Inline TLS custody avoids per-turn allocation."
)]
pub enum SourceProgress<'m, 'w, 'a, 'r, B: TlsWireStorage> {
    Pending(SourceExchange<'m, 'w, 'a, 'r, B>),
    Ready,
}
impl<'m, 'w, 'a, 'r, B: TlsWireStorage> SourceExchange<'m, 'w, 'a, 'r, B> {
    pub(crate) fn writes_pending(&self) -> bool {
        self.inner.writes_pending()
    }

    pub fn advance(self) -> Result<SourceProgress<'m, 'w, 'a, 'r, B>, transfer::Refusal<B>> {
        match self.inner.advance()? {
            transfer::Progress::Pending(inner) => Ok(SourceProgress::Pending(Self {
                inner,
                completion: self.completion,
            })),
            transfer::Progress::Ready(done) => {
                *self.completion = Some(done);
                Ok(SourceProgress::Ready)
            }
        }
    }
    pub fn into_buffers(self) -> Result<(B, B), ports::Error> {
        self.inner.into_buffers()
    }
}
pub struct Rejected<B: TlsWireStorage> {
    pub outcome: Outcome,
    pub buffers: Result<(B, B), ports::Error>,
}
#[must_use = "drive DATA or close transport and retain recipient evidence"]
pub struct Exchange<'m, 'w, 'a, 'r, B: TlsWireStorage> {
    plan: RcptDataPlan<'w, 'a, 'r>,
    command: command::Exchange<'m, B>,
    database: &'m mut Meter,
    mode: Body,
    limit: Instant,
}
#[must_use = "continue DATA or retain its bound result"]
#[allow(
    clippy::large_enum_variant,
    reason = "Inline TLS custody avoids per-turn allocation."
)]
pub enum Progress<'m, 'w, 'a, 'r, B: TlsWireStorage> {
    Pending(Exchange<'m, 'w, 'a, 'r, B>),
    Ready(Completed<'w, 'a, 'r, B>),
    Rejected(Rejected<B>),
}
pub fn begin<'m, 'w, 'a, 'r, B: TlsWireStorage>(
    completed: postgres_relay_rcpt::Completed<'w, 'a, 'r, B>,
    scratch: Scratch<'m>,
    wire: &'m mut Meter,
    database: &'m mut Meter,
) -> Result<Exchange<'m, 'w, 'a, 'r, B>, Refusal<'w, 'a, 'r, B>> {
    let (result, session, mode) = completed.into_parts();
    let plan = match result.reserve_data() {
        Ok(plan) => plan,
        Err(failure) => {
            return Err(Refusal {
                error: Error::Publication(failure.error),
                retained: Retained::Planning(failure),
                buffers: session.into_buffers(),
            })
        }
    };
    let limit = plan.work_limit();
    if Instant::now() >= limit {
        return Err(fenced(
            plan,
            Error::Protocol(ports::Error::Deadline),
            session.into_buffers(),
        ));
    }
    let command = match command::Exchange::new(
        session,
        command::Command::Data,
        scratch,
        plan.runtime_clock(),
        plan.original_deadline(),
        wire,
    ) {
        Ok(command) => command,
        Err(failure) => {
            let error = Error::Protocol(failure.error);
            return Err(fenced(plan, error, failure.into_buffers()));
        }
    };
    Ok(Exchange {
        plan,
        command,
        database,
        mode,
        limit,
    })
}
fn fenced<'w, 'a, 'r, B: TlsWireStorage>(
    plan: RcptDataPlan<'w, 'a, 'r>,
    error: Error,
    buffers: Result<(B, B), ports::Error>,
) -> Refusal<'w, 'a, 'r, B> {
    Refusal {
        error,
        retained: Retained::Body(plan.fenced(error.publication())),
        buffers,
    }
}
impl<'m, 'w, 'a, 'r, B: TlsWireStorage> Exchange<'m, 'w, 'a, 'r, B> {
    pub(crate) fn read_progress(&self) -> bool {
        self.command.read_progress()
    }
    pub fn advance(mut self) -> Result<Progress<'m, 'w, 'a, 'r, B>, Refusal<'w, 'a, 'r, B>> {
        if Instant::now() >= self.limit {
            return Err(self.refuse(Error::Protocol(ports::Error::Deadline)));
        }
        if self.command.writes_pending() {
            match self.plan.check_current(self.database) {
                Ok(limit) => self.limit = self.limit.min(limit),
                Err(error) => return Err(self.refuse(Error::Publication(error))),
            }
            if Instant::now() >= self.limit {
                return Err(self.refuse(Error::Protocol(ports::Error::Deadline)));
            }
        }
        match self.command.advance() {
            Err(failure) => {
                if let Some((_, text)) = failure
                    .reply()
                    .filter(|(code, _)| matches!(code / 100, 4 | 5))
                {
                    let result = self.plan.record_refusal(text, self.database);
                    let buffers = failure.into_buffers();
                    return match result {
                        Ok(outcome) => Ok(Progress::Rejected(Rejected { outcome, buffers })),
                        Err(retained) => Err(Refusal {
                            error: Error::Publication(retained.error),
                            retained: Retained::Command(retained),
                            buffers,
                        }),
                    };
                }
                let error = Error::Protocol(failure.error);
                Err(fenced(self.plan, error, failure.into_buffers()))
            }
            Ok(command::Progress::Pending(command)) => {
                let next = Self { command, ..self };
                if Instant::now() >= next.limit {
                    return Err(next.refuse(Error::Protocol(ports::Error::Deadline)));
                }
                Ok(Progress::Pending(next))
            }
            Ok(command::Progress::Ready(mut reply)) => {
                let code = match reply.code() {
                    Ok(code) => code,
                    Err(error) => {
                        return Err(fenced(
                            self.plan,
                            Error::Protocol(error),
                            reply.into_buffers(),
                        ))
                    }
                };
                if matches!(code / 100, 4 | 5) {
                    // Retain parser storage while fencing the socket before settlement.
                    reply.abort();
                    let text = match reply.text() {
                        Ok(text) => text,
                        Err(error) => {
                            return Err(fenced(
                                self.plan,
                                Error::Protocol(error),
                                reply.into_buffers(),
                            ))
                        }
                    };
                    let result = self.plan.record_refusal(text, self.database);
                    let buffers = reply.into_buffers();
                    return match result {
                        Ok(outcome) => Ok(Progress::Rejected(Rejected { outcome, buffers })),
                        Err(failure) => Err(Refusal {
                            error: Error::Publication(failure.error),
                            retained: Retained::Command(failure),
                            buffers,
                        }),
                    };
                }
                if code != 354 {
                    return Err(fenced(
                        self.plan,
                        Error::Protocol(ports::Error::Invalid),
                        reply.into_buffers(),
                    ));
                }
                let text = match reply.text() {
                    Ok(text) => text,
                    Err(error) => {
                        return Err(fenced(
                            self.plan,
                            Error::Protocol(error),
                            reply.into_buffers(),
                        ))
                    }
                };
                match self.plan.publish_body(text, self.database) {
                    Ok((body, dispatch)) => Ok(Progress::Ready(Completed {
                        body,
                        dispatch,
                        session: reply.into_session(),
                        mode: self.mode,
                    })),
                    Err(failure) => Err(Refusal {
                        error: Error::Publication(failure.error),
                        retained: Retained::Body(failure),
                        buffers: reply.into_buffers(),
                    }),
                }
            }
        }
    }
    /// Explicit cancellation closes transport and retains passive RCPT evidence.
    pub fn close(self) -> Refusal<'w, 'a, 'r, B> {
        self.refuse(Error::Protocol(ports::Error::Forbidden))
    }
    fn refuse(self, error: Error) -> Refusal<'w, 'a, 'r, B> {
        fenced(self.plan, error, self.command.into_buffers())
    }
}
