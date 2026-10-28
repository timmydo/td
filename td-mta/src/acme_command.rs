//! Native HTTP-01 certificate worker, independently available before mail startup.
use super::{config_failure, ConfigFailure};
use std::{
    ffi::OsString,
    net::{SocketAddr, TcpListener},
    sync::{
        atomic::{AtomicBool, Ordering},
        RwLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use td_crypto::{Entropy, SystemEntropy, TrustStore};
use td_mta::{
    acme::{
        self,
        order::{Persistence, Settings, State},
    },
    postgres_acme,
    postgres_probe::Mode,
};

pub(super) struct Options {
    connection: String,
    profile: String,
    settings: Settings,
    bind: SocketAddr,
}
pub(super) fn arguments(mut args: impl Iterator<Item = OsString>) -> Option<Options> {
    let (mut connection, mut profile, mut directory, mut contact, mut bind) =
        (None, None, None, None, None);
    let mut names = Vec::new();
    let mut terms = false;
    while let Some(flag) = args.next() {
        if flag == "--accept-terms" && !terms {
            terms = true;
            continue;
        }
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--certificate-profile" if profile.is_none() => profile = Some(value),
            "--directory" if directory.is_none() => directory = Some(value),
            "--contact" if contact.is_none() => contact = Some(value),
            "--bind" if bind.is_none() => bind = Some(value.parse().ok()?),
            "--name" if names.len() < 32 => names.push(value),
            _ => return None,
        }
    }
    if !terms {
        return None;
    }
    let settings = Settings {
        directory: directory?,
        contact: contact?,
        names,
    };
    settings.validate().ok()?;
    Some(Options {
        connection: connection?,
        profile: profile?,
        settings,
        bind: bind?,
    })
}
fn now() -> Result<i64, acme::Error> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| acme::Error::Deadline)?
            .as_millis(),
    )
    .map_err(|_| acme::Error::Deadline)
}
struct Stop<'a>(&'a AtomicBool);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
pub(super) fn run(options: Options) -> Result<(), ConfigFailure> {
    let loaded = td_mta::postgres_input::Loaded::open(&options.connection, "acme")
        .map_err(|_| config_failure("acme", "connection-file"))?;
    let executable = std::env::current_exe().map_err(|_| config_failure("acme", "executable"))?;
    let trust = TrustStore::public_roots().map_err(|_| config_failure("acme", "trust"))?;
    let mut owner = [0; 16];
    SystemEntropy::try_new()
        .and_then(|mut rng| rng.fill(&mut owner))
        .map_err(|_| config_failure("acme", "entropy"))?;
    let initial = State::initial().map_err(|_| config_failure("acme", "key"))?;
    let settings_hash = options
        .settings
        .identity()
        .map_err(|_| config_failure("acme", "settings"))?;
    let mut first_claim = Some(
        claim_turn(
            &loaded,
            &executable,
            &options.profile,
            settings_hash,
            owner,
            &initial,
        )
        .map_err(|_| config_failure("acme", "runtime-claim"))?,
    );
    super::signal_sys::block().map_err(|_| config_failure("acme", "signals"))?;
    let listener =
        TcpListener::bind(options.bind).map_err(|_| config_failure("acme", "http01-bind"))?;
    let current = RwLock::new(acme::challenge::Current {
        names: options.settings.names.clone(),
        token: String::new(),
        authorization: String::new(),
        expires: Instant::now(),
    });
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let _stop = Stop(&stop);
        let challenge = std::thread::Builder::new()
            .name("acme-http01".into())
            .spawn_scoped(scope, || {
                let result = acme::challenge::run(listener, &current, &stop);
                stop.store(true, Ordering::Release);
                result
            })
            .map_err(|_| config_failure("acme", "worker"))?;
        let mut next = Instant::now();
        while !stop.load(Ordering::Acquire)
            && !super::signal_sys::requested().map_err(|_| config_failure("acme", "signals"))?
        {
            if Instant::now() < next {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            next = Instant::now() + Duration::from_secs(30);
            let result = (|| -> Result<(), acme::Error> {
                let (request, receipt, lease) = match first_claim.take() {
                    Some(value) => value,
                    None => claim_turn(
                        &loaded,
                        &executable,
                        &options.profile,
                        settings_hash,
                        owner,
                        &initial,
                    )?,
                };
                let mut state = lease.state;
                {
                    let mut current = current.write().map_err(|_| acme::Error::Conflict)?;
                    current.token = state.token.clone();
                    current.authorization = state.key_authorization.clone();
                    current.expires = Instant::now() + Duration::from_secs(150);
                }
                let mut writer = postgres_acme::Writer {
                    request: &request,
                    trust: &loaded.trust,
                    epoch: receipt.store_epoch,
                    profile: &options.profile,
                    owner,
                    revision: lease.revision,
                    challenge: &current,
                };
                let mut https = acme::http::Https {
                    executable: &executable,
                    directory: &options.settings.directory,
                    trust: &trust,
                    deadline: Instant::now() + acme::http::ATTEMPT_DURATION,
                };
                let now = now()?;
                if let Err(error) = acme::order::advance(
                    &options.settings,
                    &mut state,
                    &mut https,
                    &mut writer,
                    now,
                ) {
                    // A failed write may have committed. Reclaim and read its outcome
                    // before persisting backoff; never save speculative in-memory state.
                    let recovered = postgres_acme::claim(
                        &request,
                        &loaded.trust,
                        Instant::now() + Duration::from_secs(10),
                        receipt.store_epoch,
                        postgres_acme::Claim {
                            profile: &options.profile,
                            settings: settings_hash,
                            owner,
                        },
                        &initial,
                    )?;
                    state = recovered.state;
                    writer.revision = recovered.revision;
                    if state.order.is_empty() && state.expires_ms > now && state.next_ms >= now {
                        return Ok(());
                    }
                    state.failed(now);
                    if let acme::Error::RetryAt(at) = error {
                        state.next_ms = state.next_ms.max(at);
                    }
                    writer.save(&state, None)?;
                    let event = td_mta::observability::Event {
                        context: td_mta::observability::Context {
                            boot: td_mta::ids::BootId::from_bytes(owner),
                            utc_ms: Some(now),
                            config_generation: 0,
                            connection: None,
                            request: None,
                            transaction: None,
                            submission: None,
                        },
                        kind: td_mta::observability::Kind::CertificateRenewalFailed,
                    };
                    let _ = td_mta::postgres_logs::append(
                        &request,
                        &loaded.trust,
                        Instant::now() + Duration::from_secs(2),
                        receipt.store_epoch,
                        "acme",
                        &[event],
                    );
                } else if state.next_ms <= now {
                    next = Instant::now() + Duration::from_secs(1);
                }
                Ok(())
            })();
            if result.is_err() {
                if let Ok(mut current) = current.write() {
                    current.expires = Instant::now();
                }
            }
        }
        stop.store(true, Ordering::Release);
        challenge
            .join()
            .map_err(|_| config_failure("acme", "worker"))?
            .map_err(|_| config_failure("acme", "http01"))
    })
}

struct StatusOptions {
    connection: String,
    profile: String,
    require_ready: bool,
    minimum_validity_ms: i64,
}

fn status_arguments(mut args: impl Iterator<Item = OsString>) -> Option<StatusOptions> {
    let (mut connection, mut profile, mut minimum) = (None, None, None);
    let mut require_ready = false;
    while let Some(flag) = args.next() {
        if flag == "--require-ready" {
            if require_ready {
                return None;
            }
            require_ready = true;
            continue;
        }
        let value = args.next()?.into_string().ok()?;
        match flag.to_str() {
            Some("--connection-file") if connection.is_none() => connection = Some(value),
            Some("--certificate-profile") if profile.is_none() => profile = Some(value),
            Some("--minimum-validity-seconds") if minimum.is_none() => {
                let seconds = value.parse::<u32>().ok()?;
                if seconds > 31_536_000 {
                    return None;
                }
                minimum = Some(i64::from(seconds) * 1000);
            }
            _ => return None,
        }
    }
    Some(StatusOptions {
        connection: connection?,
        profile: profile?,
        require_ready,
        minimum_validity_ms: minimum.unwrap_or(0),
    })
}

fn status_health(status: &postgres_acme::Status, now: i64, minimum_ms: i64) -> &'static str {
    if status.generation == 0 {
        "pending"
    } else if status.expires_ms <= now {
        "expired"
    } else if status.expires_ms < now.saturating_add(minimum_ms) {
        "expiring"
    } else if !status.leased || status.failures > 0 {
        "degraded"
    } else {
        "ready"
    }
}

pub(super) fn status(args: impl Iterator<Item = OsString>) -> Result<bool, ConfigFailure> {
    let options =
        status_arguments(args).ok_or_else(|| config_failure("arguments", "invalid-arguments"))?;
    let connection = options.connection;
    let profile = options.profile;
    let loaded = td_mta::postgres_input::Loaded::open(&connection, "acme")
        .map_err(|_| config_failure("acme", "connection-file"))?;
    let executable = std::env::current_exe().map_err(|_| config_failure("acme", "executable"))?;
    let addresses = td_mta::postgres_resolver::resolve(
        &executable,
        &loaded.bootstrap,
        Instant::now() + Duration::from_secs(5),
    )
    .map_err(|_| config_failure("acme", "resolution"))?;
    for address in addresses {
        let mut request = loaded.bootstrap.request(address);
        request.mode = Mode::Runtime;
        let receipt = match td_mta::postgres_runtime_access::inspect(
            &request,
            &loaded.trust,
            Instant::now() + Duration::from_secs(10),
        ) {
            Ok(value) => value,
            Err(td_mta::postgres_runtime_access::Error::Connection(
                td_mta::postgres_probe::Error::Transport,
            )) => continue,
            Err(_) => return Err(config_failure("acme", "runtime")),
        };
        let status = postgres_acme::status(
            &request,
            &loaded.trust,
            Instant::now() + Duration::from_secs(10),
            receipt.store_epoch,
            &profile,
        )
        .map_err(|_| config_failure("acme", "status"))?;
        let now = now().map_err(|_| config_failure("acme", "clock"))?;
        let health = status_health(&status, now, options.minimum_validity_ms);
        use std::io::Write;
        writeln!(std::io::stdout().lock(),"{{\"schema\":1,\"command\":\"acme-status\",\"status\":\"{health}\",\"generation\":{},\"expires_utc_ms\":{},\"next_attempt_utc_ms\":{},\"failures\":{},\"leased\":{}}}",status.generation,status.expires_ms,status.next_ms,status.failures,status.leased).map_err(|_|config_failure("acme","output"))?;
        return Ok(!options.require_ready || health == "ready");
    }
    Err(config_failure("acme", "connection"))
}

// Claiming before bind establishes an ACME row under the shared schema lock.
// Empty-database replica initialization subsequently refuses that row.
fn claim_turn<'a>(
    loaded: &'a td_mta::postgres_input::Loaded,
    executable: &std::path::Path,
    profile: &str,
    settings: [u8; 32],
    owner: [u8; 16],
    initial: &State,
) -> Result<
    (
        td_mta::postgres_probe::Request<'a>,
        td_mta::postgres_schema::Receipt,
        postgres_acme::Lease,
    ),
    acme::Error,
> {
    let until = Instant::now() + Duration::from_secs(10);
    let addresses = td_mta::postgres_resolver::resolve(
        executable,
        &loaded.bootstrap,
        Instant::now() + Duration::from_secs(5),
    )
    .map_err(|_| acme::Error::Transport)?;
    let mut selected = None;
    for address in addresses {
        let mut request = loaded.bootstrap.request(address);
        request.mode = Mode::Runtime;
        match td_mta::postgres_runtime_access::inspect(&request, &loaded.trust, until) {
            Ok(receipt) => {
                selected = Some((request, receipt));
                break;
            }
            Err(td_mta::postgres_runtime_access::Error::Connection(
                td_mta::postgres_probe::Error::Transport,
            )) => continue,
            Err(_) => return Err(acme::Error::Database),
        }
    }
    let (request, receipt) = selected.ok_or(acme::Error::Transport)?;
    let lease = postgres_acme::claim(
        &request,
        &loaded.trust,
        Instant::now() + Duration::from_secs(10),
        receipt.store_epoch,
        postgres_acme::Claim {
            profile,
            settings,
            owner,
        },
        initial,
    )?;
    Ok((request, receipt, lease))
}

#[cfg(test)]
mod status_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn status_arguments_bound_and_preserve_optional_readiness() {
        let base = [
            "--connection-file",
            "/private/runtime",
            "--certificate-profile",
            "public",
        ];
        let parse = |extra: &[&str]| {
            status_arguments(
                base.iter()
                    .chain(extra.iter())
                    .map(|value| OsString::from(*value)),
            )
        };
        let default = parse(&[]).unwrap();
        assert!(!default.require_ready);
        assert_eq!(default.minimum_validity_ms, 0);
        let strict = parse(&["--require-ready", "--minimum-validity-seconds", "2592000"]).unwrap();
        assert!(strict.require_ready);
        assert_eq!(strict.minimum_validity_ms, 2_592_000_000);
        for extra in [
            vec!["--require-ready", "--require-ready"],
            vec!["--minimum-validity-seconds"],
            vec!["--minimum-validity-seconds", "-1"],
            vec!["--minimum-validity-seconds", "31536001"],
            vec![
                "--minimum-validity-seconds",
                "1",
                "--minimum-validity-seconds",
                "2",
            ],
        ] {
            assert!(parse(&extra).is_none());
        }
    }

    #[test]
    fn status_health_detects_expiry_leases_failures_and_validity_margin() {
        let mut status = postgres_acme::Status {
            generation: 0,
            next_ms: 0,
            expires_ms: 2000,
            failures: 0,
            leased: true,
        };
        assert_eq!(status_health(&status, 1000, 0), "pending");
        status.generation = 1;
        assert_eq!(status_health(&status, 2000, 0), "expired");
        assert_eq!(status_health(&status, 1000, 1000), "ready");
        assert_eq!(status_health(&status, 1000, 1001), "expiring");
        status.leased = false;
        assert_eq!(status_health(&status, 1000, 0), "degraded");
        status.leased = true;
        status.failures = 1;
        assert_eq!(status_health(&status, 1000, 0), "degraded");
    }
}
