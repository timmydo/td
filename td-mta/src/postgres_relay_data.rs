//! DATA command tied to complete ordered RCPT evidence and its same session.
use crate::{
    admission::work::Meter,
    ports,
    postgres_attempt_phases::Outcome,
    postgres_attempts::Dispatch,
    postgres_dispatch::{self as dispatch, BodyResult, RcptDataPlan},
    postgres_relay_rcpt,
    smtp_data_encode::Body,
    smtp_relay_auth::Session,
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
    /// The trusted body stage owns matching immutable source and reserved final
    /// capacity. This handoff cannot be built from historical phase receipts.
    pub fn into_parts(self) -> (BodyResult<'w, 'a, 'r>, Dispatch, Session<B>, Body) {
        (self.body, self.dispatch, self.session, self.mode)
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
