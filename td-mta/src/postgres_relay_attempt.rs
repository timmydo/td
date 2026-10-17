//! One inert configured attempt, preserving closed terminal evidence.
use crate::{
    admission::work::Meter,
    ports,
    postgres_attempt_phases::Outcome,
    postgres_read::ReadSlots,
    postgres_relay_body as body,
    postgres_relay_connect::{ClosedInterruption, ConnectedPrepared},
    postgres_relay_data as data, postgres_relay_finish as finish, postgres_relay_mail as mail,
    postgres_relay_rcpt as rcpt,
    smtp_data_encode::Body,
    smtp_relay_body, smtp_relay_command,
    tls_io::TlsWireStorage,
};
use std::time::{Duration, Instant};

/// Every variant has closed transport. Preserve its actual reply/unknown-commit
/// custody before considering generic interruption of remaining recipients.
#[allow(
    clippy::large_enum_variant,
    reason = "Inline terminal custody avoids failure-path allocation."
)]
pub enum Evidence<'w, 'a, 'r, B: TlsWireStorage> {
    Mail(mail::Refusal<'w, 'a, 'r, B>),
    Rcpt {
        error: Option<rcpt::Error>,
        closed: rcpt::Closed<'w, 'a, 'r, B>,
    },
    Data(data::Refusal<'w, 'a, 'r, B>),
    Body(body::Refusal<B>),
    Source(data::SourceRefusal<B>),
    Final(finish::Refusal<'w, 'a, 'r, B>),
}
#[must_use = "settle retained reply evidence and any unknown Pending before recovery"]
pub struct Stopped<'w, 'a, 'r, B: TlsWireStorage> {
    pub evidence: Evidence<'w, 'a, 'r, B>,
    /// Passive fallback for unresolved durable state only. RCPT evidence uses
    /// its positive-reply-aware interruption; DATA planning/Body failures use
    /// their existing evidence-aware conversions. Publish actual negative/final
    /// evidence and settle unknown Pending before fallback. If that retires the
    /// attempt, discard this owner. Durable Possible remains uncertain even if
    /// source postvalidation failed before any terminator could be sent.
    pub interruption: ClosedInterruption,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Mail,
    Data,
    Final,
}
/// Durable publication of an actual SMTP result; no session remains open.
pub struct Completed<B: TlsWireStorage> {
    pub stage: Stage,
    pub outcome: Outcome,
    pub buffers: Result<(B, B), ports::Error>,
}
/// Caller-reserved cells are reborrowed across commands, never copied per turn.
pub struct Scratch<'a> {
    pub input: &'a mut [u8],
    pub line: &'a mut [u8],
    pub reply: &'a mut [u8],
    pub command: &'a mut [u8],
    pub body_input: &'a mut [u8],
    pub body_output: &'a mut [u8],
}
impl Scratch<'_> {
    fn command(&mut self) -> smtp_relay_command::Scratch<'_> {
        smtp_relay_command::Scratch {
            input: self.input,
            line: self.line,
            reply: self.reply,
            command: self.command,
        }
    }
    fn body(&mut self) -> smtp_relay_body::Scratch<'_> {
        smtp_relay_body::Scratch {
            input: self.body_input,
            output: self.body_output,
        }
    }
}
/// Run synchronously on the connector's admitted owning worker. No phase gains
/// a new meter/deadline. This does not select jobs, retry, or settle failures.
#[allow(
    clippy::too_many_arguments,
    clippy::result_large_err,
    reason = "Explicit borrowed reservations and inline terminal custody."
)]
pub fn drive<'w, 'a, 'r, B: TlsWireStorage>(
    connected: ConnectedPrepared<'w, 'a, 'r, B>,
    mode: Body,
    mut scratch: Scratch<'_>,
    wire: &mut Meter,
    database: &mut Meter,
    source_work: &mut Meter,
    source_slots: &ReadSlots,
    maximum: u64,
) -> Result<Completed<B>, Stopped<'w, 'a, 'r, B>> {
    let (interruption, limit) = connected.driver_context();
    let mut exchange = match connected.begin_mail(mode, scratch.command(), wire, database) {
        Ok(exchange) => exchange,
        Err(failure) => {
            return Err(Stopped {
                evidence: Evidence::Mail(failure.failure),
                interruption: failure.closed,
            })
        }
    };
    let accepted = loop {
        match exchange.advance() {
            Ok(mail::Progress::Pending(next)) => {
                if !next.read_progress() {
                    wait(limit);
                }
                exchange = next;
            }
            Ok(mail::Progress::Accepted(accepted)) => break accepted,
            Ok(mail::Progress::Rejected(done)) => {
                return Ok(Completed {
                    stage: Stage::Mail,
                    outcome: done.outcome,
                    buffers: done.buffers,
                })
            }
            Err(failure) => {
                return Err(Stopped {
                    evidence: Evidence::Mail(failure),
                    interruption,
                })
            }
        }
    };
    let mut start = RcptStart::Mail(accepted);
    let recipients = 'recipients: loop {
        let begun = match start {
            RcptStart::Mail(accepted) => rcpt::begin(accepted, scratch.command(), wire, database),
            RcptStart::Cursor(cursor) => cursor.begin_next(scratch.command(), wire, database),
        };
        let mut exchange = match begun {
            Ok(exchange) => exchange,
            Err(failure) => {
                return Err(Stopped {
                    evidence: Evidence::Rcpt {
                        error: Some(failure.error),
                        closed: failure.closed,
                    },
                    interruption,
                })
            }
        };
        loop {
            match exchange.advance() {
                Ok(rcpt::Progress::Pending(next)) => {
                    if !next.read_progress() {
                        wait(limit);
                    }
                    exchange = next;
                }
                Ok(rcpt::Progress::Next(cursor)) => {
                    start = RcptStart::Cursor(cursor);
                    continue 'recipients;
                }
                Ok(rcpt::Progress::Ready(done)) => break 'recipients done,
                Ok(rcpt::Progress::Closed(closed)) => {
                    return Err(Stopped {
                        evidence: Evidence::Rcpt {
                            error: None,
                            closed,
                        },
                        interruption,
                    })
                }
                Err(failure) => {
                    return Err(Stopped {
                        evidence: Evidence::Rcpt {
                            error: Some(failure.error),
                            closed: failure.closed,
                        },
                        interruption,
                    })
                }
            }
        }
    };
    let mut exchange = match data::begin(recipients, scratch.command(), wire, database) {
        Ok(exchange) => exchange,
        Err(failure) => {
            return Err(Stopped {
                evidence: Evidence::Data(failure),
                interruption,
            })
        }
    };
    let ready = loop {
        match exchange.advance() {
            Ok(data::Progress::Pending(next)) => {
                if !next.read_progress() {
                    wait(limit);
                }
                exchange = next;
            }
            Ok(data::Progress::Ready(done)) => break done,
            Ok(data::Progress::Rejected(done)) => {
                return Ok(Completed {
                    stage: Stage::Data,
                    outcome: done.outcome,
                    buffers: done.buffers,
                })
            }
            Err(failure) => {
                return Err(Stopped {
                    evidence: Evidence::Data(failure),
                    interruption,
                })
            }
        }
    };
    let ready = match ready.reserve_body(database) {
        Ok(ready) => ready,
        Err(failure) => {
            return Err(Stopped {
                evidence: Evidence::Body(failure),
                interruption,
            })
        }
    };
    let completed = ready.with_source(source_slots, source_work, maximum, |source| {
        let mut exchange = source.begin(scratch.body(), wire, database)?;
        loop {
            let was_io = exchange.writes_pending();
            match exchange.advance()? {
                data::SourceProgress::Pending(next) => exchange = next,
                data::SourceProgress::Ready => return Ok(()),
            }
            if was_io && exchange.writes_pending() {
                wait(limit);
            }
        }
    });
    let completed = match completed {
        Ok(completed) => completed,
        Err(failure) => {
            return Err(Stopped {
                evidence: Evidence::Source(failure),
                interruption,
            })
        }
    };
    let mut exchange = match completed.begin_final(scratch.command(), wire, database) {
        Ok(exchange) => exchange,
        Err(failure) => {
            return Err(Stopped {
                evidence: Evidence::Final(failure),
                interruption,
            })
        }
    };
    loop {
        match exchange.advance() {
            Ok(finish::Progress::Pending(next)) => exchange = next,
            Ok(finish::Progress::Ready(done)) => {
                return Ok(Completed {
                    stage: Stage::Final,
                    outcome: done.outcome,
                    buffers: done.into_buffers(),
                })
            }
            Err(failure) => {
                return Err(Stopped {
                    evidence: Evidence::Final(failure),
                    interruption,
                })
            }
        }
        wait(limit);
    }
}
#[allow(
    clippy::large_enum_variant,
    reason = "Inline live custody avoids per-command allocation."
)]
enum RcptStart<'w, 'a, 'r, B: TlsWireStorage> {
    Mail(mail::Accepted<'w, 'a, 'r, B>),
    Cursor(rcpt::Cursor<'w, 'a, 'r, B>),
}
fn wait(limit: Instant) {
    if let Some(remaining) = limit.checked_duration_since(Instant::now()) {
        std::thread::sleep(remaining.min(Duration::from_millis(5)));
    }
    // Later admission may have shortened its own limit. This stale upper bound
    // controls at most a five-ms wait, never SMTP authority; the next existing
    // advance owns deadline refusal and its exact retained evidence.
}

const _: () = assert!(
    std::mem::size_of::<Stopped<'static, 'static, 'static, Box<[u8; crate::tls_io::TLS_WIRE_BYTES]>>>(
    ) <= 16 * 1024
);
