//! Inert configured relay setup retaining one native Prepared attempt.
#![allow(
    clippy::result_large_err,
    reason = "Inline passive custody avoids allocation on setup failure."
)]
use crate::{
    admission::work::{Charge, Meter, Stop},
    clock::RuntimeClock,
    config::outbound,
    dns_owner::Configured,
    ports::{self, Clock, Deadline, Entropy, Handshake, Resolver, TlsTransport},
    postgres_attempt_recovery as recovery,
    postgres_attempts::Dispatch,
    postgres_dispatch::{self as dispatch, FailedPrepared, MailPlan, PreparedResult},
    postgres_relay_mail as mail, postgres_route_health as health,
    postgres_runtime_profile::PublishedProfile,
    smtp_data_encode::Body,
    smtp_relay_auth as auth, smtp_relay_command as command, smtp_relay_ehlo as ehlo,
    tls_admission::HandshakePool,
    tls_io::{TlsWireStorage, TLS_WIRE_BYTES},
    tls_policy::{
        ClientUpgradeProgress, RelayStartTls, RelayStartTlsProgress, SessionRefusal, TlsConnection,
        TlsPolicies, UpgradeScratch,
    },
};
use std::{
    net::IpAddr,
    num::NonZeroU64,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Admission,
    Resolve,
    Dial,
    StartTls,
    Handshake,
    Ehlo,
    Auth,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Protocol(ports::Error),
    Publication(dispatch::Error),
}
/// No live SMTP authority. Retain this identity if recovery preparation fails.
#[must_use = "prepare and settle interrupted recovery"]
pub struct ClosedInterruption {
    dispatch: Dispatch,
}
impl ClosedInterruption {
    pub fn prepare(self) -> Result<recovery::Recovery, RecoveryRefusal> {
        self.dispatch
            .interrupted(&[])
            .map_err(|error| RecoveryRefusal {
                error,
                retained: self,
            })
    }
}
pub struct RecoveryRefusal {
    pub error: recovery::Error,
    pub retained: ClosedInterruption,
}
#[must_use = "settle the closed attempt"]
pub enum Closed<'w, 'a, 'r> {
    Interrupted(ClosedInterruption),
    /// Ordered route-health and attempt retirement (AUTH, TLS or network).
    Auth(FailedPrepared<'w, 'a, 'r>),
}
pub struct Refusal<'w, 'a, 'r, B: TlsWireStorage> {
    pub stage: Stage,
    pub error: Error,
    pub closed: Closed<'w, 'a, 'r>,
    pub buffers: Result<(B, B), ports::Error>,
}
/// Cells are reserved by the owning worker and reused between setup phases.
pub struct Scratch<'a> {
    pub input: &'a mut [u8],
    pub first: &'a mut [u8],
    pub second: &'a mut [u8],
    pub third: &'a mut [u8],
    pub command: &'a mut [u8],
    pub plain: &'a mut [u8],
    pub encoded: &'a mut [u8],
    pub dns: &'a mut [u8],
    pub dns_cache: &'a mut crate::dns_owner::Cache,
}
impl Scratch<'_> {
    fn ehlo(&mut self) -> ehlo::Scratch<'_> {
        ehlo::Scratch {
            input: self.input,
            greeting: self.first,
            reply: self.second,
            command: self.command,
        }
    }
    fn auth(&mut self) -> auth::Scratch<'_> {
        auth::Scratch {
            input: self.input,
            first_reply: self.first,
            second_reply: self.second,
            third_reply: self.third,
            plain: self.plain,
            encoded: self.encoded,
        }
    }
}
/// Only bound MAIL or explicit closure can consume this authenticated owner.
#[must_use = "enter bound MAIL or close the attempt"]
pub struct ConnectedPrepared<'w, 'a, 'r, B: TlsWireStorage> {
    plan: MailPlan<'w, 'a, 'r>,
    session: auth::Session<B>,
    passive: ClosedInterruption,
}
pub struct MailRefusal<'w, 'a, 'r, B: TlsWireStorage> {
    pub failure: mail::Refusal<'w, 'a, 'r, B>,
    pub closed: ClosedInterruption,
}
impl<'w, 'a, 'r, B: TlsWireStorage> ConnectedPrepared<'w, 'a, 'r, B> {
    pub(crate) fn driver_context(&self) -> (ClosedInterruption, std::time::Instant) {
        (
            ClosedInterruption {
                dispatch: self.passive.dispatch.clone(),
            },
            self.plan.work_limit(),
        )
    }

    pub fn begin_mail<'m>(
        self,
        mode: Body,
        scratch: command::Scratch<'m>,
        wire: &'m mut Meter,
        database: &'m mut Meter,
    ) -> Result<mail::Exchange<'m, 'w, 'a, 'r, B>, MailRefusal<'w, 'a, 'r, B>> {
        let Self {
            mut plan,
            session,
            passive,
        } = self;
        if let Err(error) = plan
            .restrict_deadline(wire.deadline())
            .and_then(|()| plan.check_current(database))
            .and_then(|_| plan.reserve_checks(database))
        {
            return Err(MailRefusal {
                failure: mail::Refusal {
                    error: mail::Error::Publication(error),
                    retained: None,
                    buffers: session.into_buffers(),
                },
                closed: passive,
            });
        }
        mail::begin(plan, session, mode, scratch, wire, database).map_err(|failure| MailRefusal {
            failure,
            closed: passive,
        })
    }
    pub fn close(self) -> (ClosedInterruption, Result<(B, B), ports::Error>) {
        (self.passive, self.session.into_buffers())
    }
}
struct SocketFailure<B: TlsWireStorage> {
    stage: Stage,
    error: Error,
    auth_rejected: bool,
    buffers: Result<(B, B), ports::Error>,
}
impl<B: TlsWireStorage> SocketFailure<B> {
    fn local(stage: Stage, error: ports::Error, buffers: Result<(B, B), ports::Error>) -> Self {
        Self {
            stage,
            error: Error::Protocol(error),
            auth_rejected: false,
            buffers,
        }
    }
    fn session(stage: Stage, value: SessionRefusal<B>) -> Self {
        Self::local(stage, value.error(), Ok(value.into_buffers()))
    }
}
/// Run on an already admitted outbound worker. This does not spawn a worker or
/// activate the PostgreSQL backend. DNS has its separately bounded owner meter.
#[allow(
    clippy::too_many_arguments,
    reason = "All worker reservations are explicit borrowed inputs."
)]
pub fn connect<'w, 'a, 'r, B: TlsWireStorage, E: Entropy>(
    mut prepared: PreparedResult<'w, 'a, 'r>,
    profile: &PublishedProfile,
    pool: &HandshakePool,
    input: B,
    output: B,
    mut scratch: Scratch<'_>,
    entropy: E,
    wire: &mut Meter,
    database: &mut Meter,
) -> Result<ConnectedPrepared<'w, 'a, 'r, B>, Refusal<'w, 'a, 'r, B>> {
    let passive = ClosedInterruption {
        dispatch: prepared.passive_dispatch(),
    };
    let plan = prepared
        .restrict_deadline(wire.deadline())
        .and_then(|()| prepared.admit_envelope(database))
        .and_then(|plan| plan.reserve_mail());
    let mut plan = match plan {
        Ok(plan) => plan,
        Err(error) => {
            return Err(Refusal {
                stage: Stage::Admission,
                error: Error::Publication(error),
                closed: Closed::Interrupted(passive),
                buffers: Ok((input, output)),
            })
        }
    };
    match setup(
        &mut plan,
        profile,
        pool,
        input,
        output,
        &mut scratch,
        entropy,
        wire,
        database,
    ) {
        Ok(session) => Ok(ConnectedPrepared {
            plan,
            session,
            passive,
        }),
        Err(failure) => {
            let remote_idle = failure.stage == Stage::Resolve
                && failure.error == Error::Protocol(ports::Error::Deadline)
                && deadline(&plan, wire)
                    .ok()
                    .zip(plan.runtime_clock().sample().ok())
                    .is_some_and(|(cap, now)| !cap.expired(now.monotonic));
            Err(Refusal {
                stage: failure.stage,
                error: failure.error,
                closed: if failure.auth_rejected {
                    Closed::Auth(plan.into_failed(health::Failure::Auth))
                } else if matches!(failure.error, Error::Protocol(ports::Error::Tls)) {
                    Closed::Auth(plan.into_failed(health::Failure::Tls))
                } else if matches!(
                    failure.error,
                    Error::Protocol(ports::Error::Dns | ports::Error::Io { .. })
                ) || remote_idle
                {
                    Closed::Auth(plan.into_failed(health::Failure::Network))
                } else {
                    Closed::Interrupted(passive)
                },
                buffers: failure.buffers,
            })
        }
    }
}
fn deadline(plan: &MailPlan<'_, '_, '_>, meter: &Meter) -> Result<Deadline, ports::Error> {
    let clock = plan.runtime_clock();
    let bounded = clock.deadline_before(plan.work_limit())?;
    Deadline::after(
        ports::Tick(0),
        bounded
            .tick()
            .0
            .min(plan.original_deadline().tick().0)
            .min(meter.deadline().tick().0),
    )
}
fn charge(
    clock: &RuntimeClock,
    cap: Deadline,
    meter: &mut Meter,
    io_bytes: u64,
) -> Result<(), ports::Error> {
    let now = clock.sample()?.monotonic;
    if cap.expired(now) {
        return Err(ports::Error::Deadline);
    }
    meter
        .charge(
            now,
            Charge {
                records: 1,
                io_bytes,
                ..Charge::default()
            },
        )
        .map_err(|stop| match stop {
            Stop::Deadline => ports::Error::Deadline,
            _ => ports::Error::Capacity,
        })
}
fn pause(clock: &RuntimeClock, cap: Deadline) -> Result<(), ports::Error> {
    let remaining = clock
        .deadline_instant(cap)?
        .checked_duration_since(Instant::now())
        .ok_or(ports::Error::Deadline)?;
    if remaining.is_zero() {
        return Err(ports::Error::Deadline);
    }
    std::thread::sleep(remaining.min(Duration::from_millis(5)));
    if cap.expired(clock.sample()?.monotonic) {
        return Err(ports::Error::Deadline);
    }
    Ok(())
}
#[allow(
    clippy::too_many_arguments,
    reason = "All setup custody remains on one owning worker."
)]
fn setup<B: TlsWireStorage, E: Entropy>(
    plan: &mut MailPlan<'_, '_, '_>,
    profile: &PublishedProfile,
    pool: &HandshakePool,
    input: B,
    output: B,
    scratch: &mut Scratch<'_>,
    entropy: E,
    wire: &mut Meter,
    database: &mut Meter,
) -> Result<auth::Session<B>, SocketFailure<B>> {
    let clock = plan.runtime_clock();
    let candidate = profile.configuration.candidate();
    let mut hostname_bytes = [0; 255];
    let before_socket = (|| {
        let policy = TlsPolicies::relay(&profile.policies)?;
        if !plan.accepts_policy(policy) {
            return Err(ports::Error::Forbidden);
        }
        let (host, port, transport) =
            TlsPolicies::destination(&profile.policies, policy)?.ok_or(ports::Error::Invalid)?;
        let outbound = candidate.outbound().map_err(|_| ports::Error::Invalid)?;
        let relay = outbound.relay().map_err(|_| ports::Error::Invalid)?;
        let password = profile
            .configuration
            .relay_password()
            .map_err(|_| ports::Error::Invalid)?;
        let cap = deadline(plan, wire)?;
        // Admission above is the fresh pre-resolution authority observation.
        charge(&clock, cap, wire, 0)?;
        let mut addresses = [IpAddr::from([0, 0, 0, 0]); crate::dns_reply::MAX_ADDRESSES];
        let (hostname, resolution) = candidate
            .with_graph(|records, text| {
                let graph = records.view(text).map_err(|_| ports::Error::Invalid)?;
                let hostname = graph
                    .domains()
                    .map_err(|_| ports::Error::Invalid)?
                    .hostname()
                    .map_err(|_| ports::Error::Invalid)?;
                let certificates = graph.certificates().map_err(|_| ports::Error::Invalid)?;
                let mut resolver = Configured::new(
                    outbound,
                    certificates,
                    NonZeroU64::new(profile.policies.id().get()).ok_or(ports::Error::Invalid)?,
                    &clock,
                    entropy,
                    scratch.dns,
                    scratch.dns_cache,
                )?;
                let result = resolver.resolve(host, cap, &mut addresses)?;
                hostname_bytes
                    .get_mut(..hostname.len())
                    .ok_or(ports::Error::Capacity)?
                    .copy_from_slice(hostname.as_bytes());
                Ok::<_, ports::Error>((hostname.len(), result))
            })
            .map_err(|_| ports::Error::Invalid)??;
        let hostname = std::str::from_utf8(
            hostname_bytes
                .get(..hostname)
                .ok_or(ports::Error::Invalid)?,
        )
        .map_err(|_| ports::Error::Invalid)?;
        Ok((
            policy,
            port,
            transport,
            hostname,
            relay.username,
            password,
            addresses,
            resolution.count,
        ))
    })();
    let (policy, port, transport, hostname, username, password, addresses, count) =
        match before_socket {
            Ok(value) => value,
            Err(error) => {
                return Err(SocketFailure::local(
                    Stage::Resolve,
                    error,
                    Ok((input, output)),
                ))
            }
        };
    if let Err(error) = plan.check_current(database) {
        return Err(SocketFailure {
            stage: Stage::Dial,
            error: Error::Publication(error),
            auth_rejected: false,
            buffers: Ok((input, output)),
        });
    }
    let cap = match deadline(plan, wire) {
        Ok(cap) => cap,
        Err(error) => {
            return Err(SocketFailure::local(
                Stage::Dial,
                error,
                Ok((input, output)),
            ))
        }
    };
    let plain = match addresses
        .get(..count)
        .ok_or(ports::Error::Invalid)
        .and_then(|addresses| crate::outbound_dial::connect(addresses, port, &clock, cap, wire))
    {
        Ok(plain) => plain,
        Err(error) => {
            return Err(SocketFailure::local(
                Stage::Dial,
                error,
                Ok((input, output)),
            ))
        }
    };
    let reserved =
        TlsPolicies::reserve_session(profile.policies.clone(), policy, pool, input, output)
            .and_then(|prepared| prepared.construct())
            .map_err(|error| SocketFailure::session(Stage::Handshake, error))?;
    let mut connection: TlsConnection<B> =
        match transport {
            outbound::Transport::ImplicitTls => reserved
                .handoff(plain, &[], clock.clone(), cap, cap)
                .map_err(|error| SocketFailure::session(Stage::Handshake, error))?,
            outbound::Transport::RequiredStartTls => {
                let mut initial = RelayStartTls::new(
                    reserved,
                    plain,
                    hostname,
                    scratch.ehlo(),
                    clock.clone(),
                    cap,
                    cap,
                )
                .map_err(|error| SocketFailure::session(Stage::StartTls, error))?;
                let ready = loop {
                    if let Err(error) = charge(&clock, cap, wire, 512) {
                        return Err(SocketFailure::local(
                            Stage::StartTls,
                            error,
                            Ok(initial.into_buffers()),
                        ));
                    }
                    match initial
                        .advance()
                        .map_err(|error| SocketFailure::session(Stage::StartTls, error))?
                    {
                        RelayStartTlsProgress::Ready(ready) => break ready,
                        RelayStartTlsProgress::Pending(next) => initial = next,
                    }
                    if let Err(error) = pause(&clock, cap) {
                        return Err(SocketFailure::local(
                            Stage::StartTls,
                            error,
                            Ok(initial.into_buffers()),
                        ));
                    }
                };
                let mut upgrade = ready
                    .upgrade(UpgradeScratch {
                        input: scratch.input,
                        reply: scratch.first,
                    })
                    .map_err(|error| SocketFailure::session(Stage::StartTls, error))?;
                loop {
                    if let Err(error) = charge(&clock, cap, wire, 512) {
                        return Err(SocketFailure::local(
                            Stage::StartTls,
                            error,
                            Ok(upgrade.into_buffers()),
                        ));
                    }
                    match upgrade
                        .advance()
                        .map_err(|error| SocketFailure::session(Stage::StartTls, error))?
                    {
                        ClientUpgradeProgress::Tls(connection) => break connection,
                        ClientUpgradeProgress::Pending(next) => upgrade = next,
                    }
                    if let Err(error) = pause(&clock, cap) {
                        return Err(SocketFailure::local(
                            Stage::StartTls,
                            error,
                            Ok(upgrade.into_buffers()),
                        ));
                    }
                }
            }
        };
    loop {
        let turn = charge(&clock, cap, wire, 2 * TLS_WIRE_BYTES as u64)
            .and_then(|()| connection.handshake(cap));
        match turn {
            Ok(Handshake::Complete(_)) => break,
            Ok(Handshake::Pending) => {}
            Err(error) => {
                return Err(SocketFailure::local(
                    Stage::Handshake,
                    error,
                    connection.into_buffers(),
                ))
            }
        }
        if let Err(error) = pause(&clock, cap) {
            return Err(SocketFailure::local(
                Stage::Handshake,
                error,
                connection.into_buffers(),
            ));
        }
    }
    let mut exchange =
        ehlo::Exchange::new(connection, hostname, scratch.ehlo(), clock.clone(), cap).map_err(
            |failure| SocketFailure::local(Stage::Ehlo, failure.error, failure.into_buffers()),
        )?;
    let session = loop {
        if let Err(error) = charge(&clock, cap, wire, 2 * TLS_WIRE_BYTES as u64) {
            return Err(SocketFailure::local(
                Stage::Ehlo,
                error,
                exchange.into_buffers(),
            ));
        }
        match exchange.advance().map_err(|failure| {
            SocketFailure::local(Stage::Ehlo, failure.error, failure.into_buffers())
        })? {
            ehlo::Progress::Ready(session) => break session,
            ehlo::Progress::Pending(next) => exchange = next,
        }
        if let Err(error) = pause(&clock, cap) {
            return Err(SocketFailure::local(
                Stage::Ehlo,
                error,
                exchange.into_buffers(),
            ));
        }
    };
    if let Err(error) = plan.check_current(database) {
        return Err(SocketFailure {
            stage: Stage::Auth,
            error: Error::Publication(error),
            auth_rejected: false,
            buffers: session.into_buffers(),
        });
    }
    let cap = match deadline(plan, wire) {
        Ok(cap) => cap,
        Err(error) => {
            return Err(SocketFailure::local(
                Stage::Auth,
                error,
                session.into_buffers(),
            ))
        }
    };
    let mut exchange = auth::Exchange::new(
        session,
        username,
        password,
        scratch.auth(),
        clock.clone(),
        cap,
        wire,
    )
    .and_then(auth::Exchange::with_initial_response)
    .map_err(|failure| SocketFailure::local(Stage::Auth, failure.error, failure.into_buffers()))?;
    loop {
        match exchange.advance() {
            Ok(auth::Progress::Ready(session)) => return Ok(session),
            Ok(auth::Progress::Pending(next)) => exchange = next,
            Err(failure) => {
                return Err(SocketFailure {
                    stage: Stage::Auth,
                    error: Error::Protocol(failure.error),
                    auth_rejected: failure.peer_rejection() == Some(535),
                    buffers: failure.into_buffers(),
                })
            }
        }
        if let Err(error) = pause(&clock, cap) {
            return Err(SocketFailure::local(
                Stage::Auth,
                error,
                exchange.into_buffers(),
            ));
        }
    }
}

const _: () = assert!(
    std::mem::size_of::<ConnectedPrepared<'static, 'static, 'static, Box<[u8; TLS_WIRE_BYTES]>>>()
        <= 8192
);
const _: () = assert!(std::mem::size_of::<Closed<'static, 'static, 'static>>() <= 2048);

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn setup_turns_share_finite_precharged_work() {
        let clock = RuntimeClock::new();
        let cap = Deadline::after(clock.sample().unwrap().monotonic, 1000).unwrap();
        let cost = 2 * TLS_WIRE_BYTES as u64;
        for allowance in [
            Charge {
                records: 0,
                io_bytes: cost,
                ..Charge::default()
            },
            Charge {
                records: 1,
                io_bytes: cost - 1,
                ..Charge::default()
            },
        ] {
            assert_eq!(
                charge(&clock, cap, &mut Meter::new(cap, allowance), cost),
                Err(ports::Error::Capacity)
            );
        }
        let mut meter = Meter::new(
            cap,
            Charge {
                records: 3,
                io_bytes: 512 + 2 * cost,
                ..Charge::default()
            },
        );
        charge(&clock, cap, &mut meter, 512).unwrap();
        charge(&clock, cap, &mut meter, cost).unwrap();
        charge(&clock, cap, &mut meter, cost).unwrap();
        assert_eq!(
            charge(&clock, cap, &mut meter, cost),
            Err(ports::Error::Capacity)
        );
        let expired = Deadline::after(ports::Tick(0), 0).unwrap();
        assert_eq!(
            charge(
                &clock,
                expired,
                &mut Meter::new(
                    cap,
                    Charge {
                        records: 1,
                        ..Charge::default()
                    }
                ),
                0
            ),
            Err(ports::Error::Deadline)
        );
    }
}
