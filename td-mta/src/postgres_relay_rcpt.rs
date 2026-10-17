//! Ordered RCPT commands tied to the successful MAIL session.
use crate::{
    admission::work::Meter,
    ports,
    postgres_dispatch::{self as dispatch, RcptPlan, RcptResult},
    postgres_relay_mail::Accepted,
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
/// Transport is already fenced. Evidence is passive; settle preceding negative
/// replies before ordinary recovery using the same worker's Pending owner.
pub struct Closed<'w, 'a, 'r, B: TlsWireStorage> {
    pub evidence: Result<RcptResult<'w, 'a, 'r>, dispatch::Error>,
    pub buffers: Result<(B, B), ports::Error>,
}
pub struct Refusal<'w, 'a, 'r, B: TlsWireStorage> {
    pub error: Error,
    pub closed: Closed<'w, 'a, 'r, B>,
}
/// The next exact recipient, prior reply evidence, same session and MAIL mode.
pub struct Cursor<'w, 'a, 'r, B: TlsWireStorage> {
    plan: RcptPlan<'w, 'a, 'r>,
    session: Session<B>,
    mode: Body,
}
/// Full RCPT collection with at least one positive reply. No DATA was sent.
///
/// ```compile_fail,E0624
/// use td_mta::postgres_relay_rcpt::Completed;
/// fn split(value: Completed<'_, '_, '_, Box<[u8; td_mta::tls_io::TLS_WIRE_BYTES]>>) {
///     let _ = value.into_parts();
/// }
/// ```
pub struct Completed<'w, 'a, 'r, B: TlsWireStorage> {
    result: RcptResult<'w, 'a, 'r>,
    session: Session<B>,
    mode: Body,
}
impl<'w, 'a, 'r, B: TlsWireStorage> Completed<'w, 'a, 'r, B> {
    /// Internal DATA handoff keeps evidence, mode and session paired.
    pub(crate) fn into_parts(self) -> (RcptResult<'w, 'a, 'r>, Session<B>, Body) {
        (self.result, self.session, self.mode)
    }
    pub fn close(self) -> Closed<'w, 'a, 'r, B> {
        Closed {
            buffers: self.session.into_buffers(),
            evidence: Ok(self.result),
        }
    }
}
#[must_use = "drive this RCPT or close its transport and retain earlier replies"]
pub struct Exchange<'m, 'w, 'a, 'r, B: TlsWireStorage> {
    plan: RcptPlan<'w, 'a, 'r>,
    command: command::Exchange<'m, B>,
    database: &'m mut Meter,
    mode: Body,
    ordinal: u32,
    limit: Instant,
}
#[must_use = "continue ordered RCPT or retain its collected evidence"]
#[allow(
    clippy::large_enum_variant,
    reason = "Inline TLS custody avoids per-turn allocation."
)]
pub enum Progress<'m, 'w, 'a, 'r, B: TlsWireStorage> {
    Pending(Exchange<'m, 'w, 'a, 'r, B>),
    Next(Cursor<'w, 'a, 'r, B>),
    Ready(Completed<'w, 'a, 'r, B>),
    Closed(Closed<'w, 'a, 'r, B>),
}
/// Reserve every RCPT reply cell before starting the first command, using only
/// the successful MAIL owner. No independently supplied session is accepted.
pub fn begin<'m, 'w, 'a, 'r, B: TlsWireStorage>(
    accepted: Accepted<'w, 'a, 'r, B>,
    scratch: Scratch<'m>,
    wire: &'m mut Meter,
    database: &'m mut Meter,
) -> Result<Exchange<'m, 'w, 'a, 'r, B>, Refusal<'w, 'a, 'r, B>> {
    let (envelope, session, mode) = accepted.into_parts();
    let plan = match envelope.reserve_rcpt() {
        Ok(plan) => plan,
        Err(error) => {
            return Err(Refusal {
                error: Error::Publication(error),
                closed: Closed {
                    evidence: Err(error),
                    buffers: session.into_buffers(),
                },
            })
        }
    };
    Cursor {
        plan,
        session,
        mode,
    }
    .begin_next(scratch, wire, database)
}
impl<'w, 'a, 'r, B: TlsWireStorage> Cursor<'w, 'a, 'r, B> {
    /// A separate scratch borrow per command prevents reuse while its parser
    /// still owns the cells. Identity, order, session and deadlines are retained.
    pub fn begin_next<'m>(
        self,
        scratch: Scratch<'m>,
        wire: &'m mut Meter,
        database: &'m mut Meter,
    ) -> Result<Exchange<'m, 'w, 'a, 'r, B>, Refusal<'w, 'a, 'r, B>> {
        let limit = self.plan.work_limit();
        if Instant::now() >= limit {
            return Err(Refusal {
                error: Error::Protocol(ports::Error::Deadline),
                closed: self.close(),
            });
        }
        let Some(recipient) = self.plan.next_recipient() else {
            return Err(Refusal {
                error: Error::Protocol(ports::Error::Invalid),
                closed: self.close(),
            });
        };
        let ordinal = recipient.ordinal;
        let command = command::Exchange::new(
            self.session,
            command::Command::RcptTo(&recipient.address),
            scratch,
            self.plan.runtime_clock(),
            self.plan.original_deadline(),
            wire,
        );
        match command {
            Ok(command) => Ok(Exchange {
                plan: self.plan,
                command,
                database,
                mode: self.mode,
                ordinal,
                limit,
            }),
            Err(failure) => {
                let error = Error::Protocol(failure.error);
                let buffers = failure.into_buffers();
                Err(Refusal {
                    error,
                    closed: Closed {
                        evidence: self.plan.finish(),
                        buffers,
                    },
                })
            }
        }
    }
    pub fn close(self) -> Closed<'w, 'a, 'r, B> {
        let buffers = self.session.into_buffers();
        Closed {
            evidence: self.plan.finish(),
            buffers,
        }
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
                let mut error = Error::Protocol(failure.error);
                if let Some((_, text)) = failure
                    .reply()
                    .filter(|(code, _)| matches!(code / 100, 2 | 4 | 5))
                {
                    if let Err(record) = self.plan.record_reply(self.ordinal, text) {
                        error = Error::Publication(record);
                    }
                }
                let buffers = failure.into_buffers();
                Err(Refusal {
                    error,
                    closed: Closed {
                        evidence: self.plan.finish(),
                        buffers,
                    },
                })
            }
            Ok(command::Progress::Pending(command)) => {
                let next = Self { command, ..self };
                if Instant::now() >= next.limit {
                    return Err(next.refuse(Error::Protocol(ports::Error::Deadline)));
                }
                Ok(Progress::Pending(next))
            }
            Ok(command::Progress::Ready(reply)) => {
                let record = reply.text().map_err(Error::Protocol).and_then(|text| {
                    self.plan
                        .record_reply(self.ordinal, text)
                        .map_err(Error::Publication)
                });
                if let Err(error) = record {
                    let buffers = reply.into_buffers();
                    return Err(Refusal {
                        error,
                        closed: Closed {
                            evidence: self.plan.finish(),
                            buffers,
                        },
                    });
                }
                if reply.code() == Ok(421) {
                    let buffers = reply.into_buffers();
                    return Ok(Progress::Closed(Closed {
                        evidence: self.plan.finish(),
                        buffers,
                    }));
                }
                // Preserve complete actual evidence before refusing an expired continuation.
                if Instant::now() >= self.limit {
                    let buffers = reply.into_buffers();
                    return Err(Refusal {
                        error: Error::Protocol(ports::Error::Deadline),
                        closed: Closed {
                            evidence: self.plan.finish(),
                            buffers,
                        },
                    });
                }
                if self.plan.next_recipient().is_some() {
                    return Ok(Progress::Next(Cursor {
                        plan: self.plan,
                        session: reply.into_session(),
                        mode: self.mode,
                    }));
                }
                let result = match self.plan.finish() {
                    Ok(result) => result,
                    Err(error) => {
                        return Err(Refusal {
                            error: Error::Publication(error),
                            closed: Closed {
                                evidence: Err(error),
                                buffers: reply.into_buffers(),
                            },
                        })
                    }
                };
                if result.evidence().accepted().is_empty() {
                    return Ok(Progress::Closed(Closed {
                        evidence: Ok(result),
                        buffers: reply.into_buffers(),
                    }));
                }
                Ok(Progress::Ready(Completed {
                    result,
                    session: reply.into_session(),
                    mode: self.mode,
                }))
            }
        }
    }
    pub fn close(self) -> Closed<'w, 'a, 'r, B> {
        let buffers = self.command.into_buffers();
        Closed {
            evidence: self.plan.finish(),
            buffers,
        }
    }
    fn refuse(self, error: Error) -> Refusal<'w, 'a, 'r, B> {
        Refusal {
            error,
            closed: self.close(),
        }
    }
}
