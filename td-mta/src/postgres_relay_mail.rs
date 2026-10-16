//! MAIL command exchange bound to an admitted native envelope.
use crate::{
    admission::work::Meter,
    ports,
    postgres_attempt_phases::Outcome,
    postgres_dispatch::{self as dispatch, EnvelopePlan, MailPlan},
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
/// Every failure closes transport; actual negative replies retain publication
/// custody when the database cannot settle them.
pub struct Refusal<'w, 'a, 'r, B: TlsWireStorage> {
    pub error: Error,
    pub retained: Option<dispatch::CommandFailure<'w, 'a, 'r>>,
    pub buffers: Result<(B, B), ports::Error>,
}
/// Successful MAIL keeps the original envelope, same session and MAIL mode.
pub struct Accepted<'w, 'a, 'r, B: TlsWireStorage> {
    envelope: EnvelopePlan<'w, 'a, 'r>,
    session: Session<B>,
    mode: Body,
}
impl<'w, 'a, 'r, B: TlsWireStorage> Accepted<'w, 'a, 'r, B> {
    /// The trusted next stage owns RCPT sequencing and fresh output checks.
    pub fn into_parts(self) -> (EnvelopePlan<'w, 'a, 'r>, Session<B>, Body) {
        (self.envelope, self.session, self.mode)
    }
    pub fn into_buffers(self) -> Result<(B, B), ports::Error> {
        self.session.into_buffers()
    }
}
pub struct Rejected<B: TlsWireStorage> {
    pub outcome: Outcome,
    pub buffers: Result<(B, B), ports::Error>,
}
#[must_use = "drive MAIL or close its transport"]
pub struct Exchange<'m, 'w, 'a, 'r, B: TlsWireStorage> {
    plan: MailPlan<'w, 'a, 'r>,
    command: command::Exchange<'m, B>,
    database: &'m mut Meter,
    mode: Body,
    limit: Instant,
}
#[must_use = "continue MAIL or retain its result"]
#[allow(
    clippy::large_enum_variant,
    reason = "Inline TLS custody avoids per-turn allocation."
)]
pub enum Progress<'m, 'w, 'a, 'r, B: TlsWireStorage> {
    Pending(Exchange<'m, 'w, 'a, 'r, B>),
    Accepted(Accepted<'w, 'a, 'r, B>),
    Rejected(Rejected<B>),
}
/// The caller supplies the authenticated route session for this attempt. The
/// command sender, clock and original deadline come only from the MailPlan.
pub fn begin<'m, 'w, 'a, 'r, B: TlsWireStorage>(
    plan: MailPlan<'w, 'a, 'r>,
    session: Session<B>,
    mode: Body,
    scratch: Scratch<'m>,
    wire: &'m mut Meter,
    database: &'m mut Meter,
) -> Result<Exchange<'m, 'w, 'a, 'r, B>, Refusal<'w, 'a, 'r, B>> {
    let limit = plan.work_limit();
    if Instant::now() >= limit {
        return Err(Refusal {
            error: Error::Protocol(ports::Error::Deadline),
            retained: None,
            buffers: session.into_buffers(),
        });
    }
    let sender = &plan.envelope().reverse_path;
    let mail = match mode {
        Body::SevenBit => command::Command::MailFrom(sender),
        Body::EightBit => command::Command::MailFromEightBit(sender),
    };
    let command = command::Exchange::new(
        session,
        mail,
        scratch,
        plan.runtime_clock(),
        plan.original_deadline(),
        wire,
    )
    .map_err(|failure| Refusal {
        error: Error::Protocol(failure.error),
        retained: None,
        buffers: failure.into_buffers(),
    })?;
    Ok(Exchange {
        plan,
        command,
        database,
        mode,
        limit,
    })
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
            Err(failure) => Err(Refusal {
                error: Error::Protocol(failure.error),
                retained: None,
                buffers: failure.into_buffers(),
            }),
            Ok(command::Progress::Pending(command)) => {
                let next = Self { command, ..self };
                if Instant::now() >= next.limit {
                    return Err(next.refuse(Error::Protocol(ports::Error::Deadline)));
                }
                Ok(Progress::Pending(next))
            }
            Ok(command::Progress::Ready(reply)) => {
                let code = match reply.code() {
                    Ok(code) => code,
                    Err(error) => {
                        return Err(Refusal {
                            error: Error::Protocol(error),
                            retained: None,
                            buffers: reply.into_buffers(),
                        })
                    }
                };
                let text = match reply.text() {
                    Ok(text) => text,
                    Err(error) => {
                        return Err(Refusal {
                            error: Error::Protocol(error),
                            retained: None,
                            buffers: reply.into_buffers(),
                        })
                    }
                };
                if matches!(code / 100, 4 | 5) {
                    // Capture complete actual refusal before any further admission.
                    let result = self.plan.record_refusal(text, self.database);
                    let buffers = reply.into_buffers();
                    return match result {
                        Ok(outcome) => Ok(Progress::Rejected(Rejected { outcome, buffers })),
                        Err(failure) => Err(Refusal {
                            error: Error::Publication(failure.error),
                            retained: Some(failure),
                            buffers,
                        }),
                    };
                }
                if code / 100 != 2 {
                    return Err(Refusal {
                        error: Error::Protocol(ports::Error::Invalid),
                        retained: None,
                        buffers: reply.into_buffers(),
                    });
                }
                if Instant::now() >= self.limit {
                    return Err(Refusal {
                        error: Error::Protocol(ports::Error::Deadline),
                        retained: None,
                        buffers: reply.into_buffers(),
                    });
                }
                match self.plan.accepted(text) {
                    Ok(envelope) => Ok(Progress::Accepted(Accepted {
                        envelope,
                        session: reply.into_session(),
                        mode: self.mode,
                    })),
                    Err(error) => Err(Refusal {
                        error: Error::Publication(error),
                        retained: None,
                        buffers: reply.into_buffers(),
                    }),
                }
            }
        }
    }
    pub fn into_buffers(self) -> Result<(B, B), ports::Error> {
        self.command.into_buffers()
    }
    fn refuse(self, error: Error) -> Refusal<'w, 'a, 'r, B> {
        Refusal {
            error,
            retained: None,
            buffers: self.command.into_buffers(),
        }
    }
}
