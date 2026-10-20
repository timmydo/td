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
                let until = Instant::now() + Duration::from_secs(10);
                let addresses = td_mta::postgres_resolver::resolve(
                    &executable,
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
                        profile: &options.profile,
                        settings: settings_hash,
                        owner,
                    },
                    &initial,
                )?;
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
                    deadline: Instant::now() + Duration::from_secs(100),
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

pub(super) fn status(mut args: impl Iterator<Item = OsString>) -> Result<(), ConfigFailure> {
    let (mut connection, mut profile) = (None, None);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .and_then(|v| v.into_string().ok())
            .ok_or_else(|| config_failure("arguments", "invalid-arguments"))?;
        match flag.to_str() {
            Some("--connection-file") if connection.is_none() => connection = Some(value),
            Some("--certificate-profile") if profile.is_none() => profile = Some(value),
            _ => return Err(config_failure("arguments", "invalid-arguments")),
        }
    }
    let connection = connection.ok_or_else(|| config_failure("arguments", "invalid-arguments"))?;
    let profile = profile.ok_or_else(|| config_failure("arguments", "invalid-arguments"))?;
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
        let health = if status.generation == 0 {
            "pending"
        } else if status.expires_ms <= now {
            "expired"
        } else if !status.leased || status.failures > 0 {
            "degraded"
        } else {
            "ready"
        };
        use std::io::Write;
        writeln!(std::io::stdout().lock(),"{{\"schema\":1,\"command\":\"acme-status\",\"status\":\"{health}\",\"generation\":{},\"expires_utc_ms\":{},\"next_attempt_utc_ms\":{},\"failures\":{},\"leased\":{}}}",status.generation,status.expires_ms,status.next_ms,status.failures,status.leased).map_err(|_|config_failure("acme","output"))?;
        return Ok(());
    }
    Err(config_failure("acme", "connection"))
}
