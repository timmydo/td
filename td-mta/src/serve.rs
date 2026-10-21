//! PostgreSQL foreground receiving and mailbox access.
use super::{adapter, config_failure, control::Endpoint, lock_root, scope, ConfigFailure};
use std::{
    io::{self, Write},
    net::TcpListener,
    os::unix::fs::MetadataExt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use td_mta::{
    admission::logical::Cell,
    clock::RuntimeClock,
    config::listener::Kind,
    ownership::SlotState,
    ports,
    smtp_receiving::{BoundListener, Control, Receiving},
    store_fs::{PrivateRoot, RequestSpool},
    tls_admission::HandshakePool,
};

pub(super) struct Options {
    connection: String,
    profile: String,
    runtime: String,
    scratch: String,
}
pub(super) fn arguments(mut args: impl Iterator<Item = std::ffi::OsString>) -> Option<Options> {
    let (mut connection, mut profile, mut runtime, mut scratch) = (None, None, None, None);
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--connection-file" if connection.is_none() => connection = Some(value),
            "--profile" if profile.is_none() => profile = Some(value),
            "--runtime" if runtime.is_none() => runtime = Some(value),
            "--scratch" if scratch.is_none() => scratch = Some(value),
            _ => return None,
        }
    }
    Some(Options {
        connection: connection?,
        profile: profile?,
        runtime: runtime?,
        scratch: scratch?,
    })
}

pub(super) fn run(options: Options) -> Result<(), ConfigFailure> {
    loop {
        let reload = AtomicBool::new(false);
        run_once(&options, &reload)?;
        if !reload.load(Ordering::Acquire)
            || super::signal_sys::requested().map_err(|_| config_failure("signals", "io"))?
        {
            return Ok(());
        }
    }
}

fn run_once(options: &Options, reload: &AtomicBool) -> Result<(), ConfigFailure> {
    let clock = Arc::new(RuntimeClock::new());
    let loaded = td_mta::postgres_input::Loaded::open(&options.connection, &options.profile)
        .map_err(|error| config_failure("postgres", error.code()))?;
    let executable =
        std::env::current_exe().map_err(|_| config_failure("postgres", "executable"))?;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let addresses = td_mta::postgres_resolver::resolve(
        &executable,
        &loaded.bootstrap,
        until.min(std::time::Instant::now() + std::time::Duration::from_secs(5)),
    )
    .map_err(|_| config_failure("postgres", "resolution"))?;
    let mut generations = td_mta::generations::GenerationSet::at_startup();
    let mut prepared = None;
    let mut selected_address = None;
    let mut certificate_revisions = Vec::new();
    for address in addresses {
        let mut request = loaded.bootstrap.request(address);
        request.mode = td_mta::postgres_probe::Mode::Runtime;
        let schema = match td_mta::postgres_schema::inspect(&request, &loaded.trust, until) {
            Ok(schema) => schema,
            Err(td_mta::postgres_schema::Error::Connection(
                td_mta::postgres_probe::Error::Transport,
            )) => continue,
            Err(_) => return Err(config_failure("postgres", "runtime-qualification")),
        };
        certificate_revisions =
            td_mta::postgres_acme::generations(&request, &loaded.trust, until, schema.store_epoch)
                .map_err(|_| config_failure("certificate", "database"))?;
        prepared = Some(
            td_mta::postgres_runtime_profile::load(
                &request,
                &loaded.trust,
                until,
                schema.store_epoch,
                &options.profile,
                generations
                    .reserve()
                    .map_err(|error| adapter("tls", error))?,
            )
            .map_err(|_| config_failure("postgres", "profile"))?,
        );
        selected_address = Some(address);
        break;
    }
    let (published, retired) = prepared
        .ok_or_else(|| config_failure("postgres", "connection"))?
        .publish(&mut generations)
        .map_err(|error| adapter("tls", error.error))?;
    drop(retired);
    let binding = published.binding();
    let resolved = published.configuration();
    let relay = binding.roles().relay();
    let mut relay_request = loaded
        .bootstrap
        .request(selected_address.ok_or_else(|| config_failure("postgres", "connection"))?);
    relay_request.mode = td_mta::postgres_probe::Mode::Runtime;
    let mailbox_access = binding.roles().jmap();
    let candidate = resolved.candidate();
    let runtime_path = options.runtime.as_str();
    super::control::check_runtime_path(runtime_path)?;
    let runtime = PrivateRoot::open(runtime_path)
        .map_err(|_| config_failure("runtime-root", "root-policy-or-io"))?;
    let owner = runtime
        .directory()
        .metadata()
        .map_err(|_| config_failure("runtime-root", "io"))?
        .uid();
    let plans = candidate.resources();
    let resources = plans.resources();
    let limits = resources.limits();
    if mailbox_access && limits.storage_views < 2 {
        return Err(config_failure("profile", "jmap-read-capacity"));
    }
    let deadline = scope(clock.as_ref(), 10_000)?;
    let policies = generations
        .current()
        .ok_or_else(|| config_failure("tls", "invariant"))?;
    let slots = td_mta::postgres_read::ReadSlots::new(limits.storage_views)
        .map_err(|_| config_failure("postgres", "read-capacity"))?;
    let smtp_pending = td_mta::postgres_smtp_receiving::Pending::default();
    let http_pending = td_mta::postgres_http_updates::Pending::default();
    candidate
        .with_graph(|records, graph| {
            let view = records
                .view(graph)
                .map_err(|_| config_failure("configuration", "invariant"))?;
            let certificates = view
                .certificates()
                .map_err(|_| config_failure("certificate", "configuration"))?;
            let mut managed_names = Vec::new();
            for index in 0..certificates.len() {
                let certificate = certificates
                    .profile(index)
                    .map_err(|_| config_failure("certificate", "configuration"))?
                    .ok_or_else(|| config_failure("certificate", "configuration"))?;
                if certificate.mode == td_mta::config::certificate::Mode::Acme {
                    managed_names.push(certificate.name.to_owned());
                }
            }
            certificate_revisions.retain(|(name, _)| managed_names.contains(name));
            let rows = view
                .listeners()
                .map_err(|_| config_failure("configuration", "invariant"))?;
            let mut direct = Vec::new();
            let mut https_rows = Vec::new();
            direct
                .try_reserve_exact(rows.len())
                .map_err(|_| config_failure("listeners", "capacity"))?;
            for index in 0..rows.len() {
                let row = rows
                    .listener(index)
                    .map_err(|_| config_failure("configuration", "invariant"))?
                    .ok_or_else(|| config_failure("configuration", "invariant"))?;
                match row.kind {
                    Kind::DirectSmtp if row.bind.is_ipv4() => direct.push(row),
                    Kind::Https if mailbox_access && row.bind.is_ipv4() => https_rows.push(row),
                    Kind::Https if !mailbox_access => (),
                    Kind::Http01 => (),
                    _ => return Err(config_failure("listeners", "unsupported-listener")),
                }
            }
            if binding.roles().receive() && direct.is_empty() {
                return Err(config_failure("listeners", "missing-direct-smtp"));
            }
            let mut request_root = if mailbox_access {
                td_mta::jmap_request::Request::validate_resources(resources)
                    .map_err(|_| config_failure("request-limits", "invalid"))?;
                Some(lock_root(
                    same_owner_root(&options.scratch, owner, "request-root")?,
                    "request-lock",
                )?)
            } else {
                None
            };
            let mut states = [const { SlotState::EMPTY }; 4];
            let mut cells = [const { Cell::EMPTY }; 4];
            let responses = match request_root.as_mut() {
                Some(root) => {
                    let spool = RequestSpool::open(root, clock.clone(), deadline)
                        .map_err(|error| adapter("request-spool", error))?;
                    Some(
                        td_mta::store_fs::ResponseCoordinator::new(
                            spool,
                            plans.admission(),
                            &mut states,
                            &mut cells,
                            clock.clone(),
                        )
                        .map_err(|_| config_failure("responses", "capacity"))?,
                    )
                }
                None => None,
            };
            let handshakes =
                HandshakePool::new(limits.tls_handshakes).map_err(|error| adapter("tls", error))?;
            let mut listeners = Vec::new();
            listeners
                .try_reserve_exact(direct.len())
                .map_err(|_| config_failure("listeners", "capacity"))?;
            for row in direct {
                let socket = TcpListener::bind(row.bind).map_err(|error| {
                    config_failure(
                        "listen",
                        match error.kind() {
                            io::ErrorKind::AddrInUse => "address-in-use",
                            io::ErrorKind::PermissionDenied => "permission-denied",
                            _ => "io",
                        },
                    )
                })?;
                listeners.push(
                    BoundListener::new(socket, row, &policies)
                        .map_err(|error| adapter("listen", error))?,
                );
            }
            let mut https_listeners = Vec::new();
            for row in https_rows {
                let socket = TcpListener::bind(row.bind)
                    .map_err(|_| config_failure("https-listen", "io"))?;
                https_listeners.push(
                    td_mta::https::BoundListener::new(socket, row, &policies)
                        .map_err(|error| adapter("https-listen", error))?,
                );
            }
            let accounts = candidate
                .identities()
                .map_err(|_| config_failure("configuration", "invariant"))?;
            let mut sessions = Vec::new();
            let mut authentication_accounts = Vec::new();
            if mailbox_access {
                sessions
                    .try_reserve_exact(accounts.account_count())
                    .map_err(|_| config_failure("session", "capacity"))?;
                authentication_accounts
                    .try_reserve_exact(accounts.account_count())
                    .map_err(|_| config_failure("authentication", "capacity"))?;
                for index in 0..accounts.account_count() {
                    let account = accounts
                        .account_at(index)
                        .map_err(|_| config_failure("configuration", "invariant"))?
                        .ok_or_else(|| config_failure("configuration", "invariant"))?;
                    let binding = published
                        .binding_for(account.id)
                        .ok_or_else(|| config_failure("configuration", "invariant"))?;
                    let session = td_mta::jmap_session::Session::mail_submission(
                        view.origin()
                            .map_err(|_| config_failure("configuration", "invariant"))?,
                        account,
                        resources,
                        td_mta::jmap_session::Concurrency {
                            uploads: 1,
                            requests: 1,
                        },
                    )
                    .map_err(|error| adapter("session", error))?;
                    sessions.push((binding, session));
                    authentication_accounts.push((account.username, binding));
                }
            }
            let mut api_services = Vec::new();
            api_services
                .try_reserve_exact(sessions.len())
                .map_err(|_| config_failure("https", "capacity"))?;
            for (binding, session) in &sessions {
                let responses = responses
                    .as_ref()
                    .ok_or_else(|| config_failure("https", "invariant"))?;
                api_services.push(td_mta::postgres_http_api::Service {
                    loaded: &loaded,
                    executable: &executable,
                    binding,
                    slots: &slots,
                    pending: &http_pending,
                    responses,
                    resources,
                    session,
                    response_maximum: plans.admission().disk().response_bytes,
                    clock: clock.as_ref(),
                    admission: plans.admission(),
                    timeouts: plans.timeouts(),
                });
            }
            let authentication = if mailbox_access {
                Some(
                    td_mta::postgres_http_auth::Authentication::new(
                        &loaded,
                        &executable,
                        &authentication_accounts,
                        clock.as_ref(),
                    )
                    .map_err(|error| adapter("authentication", error))?,
                )
            } else {
                None
            };
            let https_ready = AtomicBool::new(false);
            let relay_ready = AtomicBool::new(false);
            let mut backends = Vec::new();
            backends
                .try_reserve_exact(api_services.len())
                .map_err(|_| config_failure("https", "capacity"))?;
            backends.extend(
                api_services
                    .iter()
                    .map(|service| service as &dyn td_mta::https_upload::Backend),
            );
            let https = if let Some(authentication) = authentication.as_ref() {
                Some(td_mta::https::Https {
                    listeners: &https_listeners,
                    policies: &policies,
                    handshakes: &handshakes,
                    authentication,
                    origin: view
                        .origin()
                        .map_err(|_| config_failure("configuration", "invariant"))?,
                    resources,
                    timeouts: plans.timeouts(),
                    clock: clock.clone(),
                    backends: &backends,
                    ready: &https_ready,
                })
            } else {
                None
            };
            let mut receiving_bindings = Vec::new();
            receiving_bindings
                .try_reserve_exact(accounts.account_count())
                .map_err(|_| config_failure("receiving", "capacity"))?;
            receiving_bindings.extend(published.bindings().skip(1));
            let storage = if binding.roles().receive() {
                Some(
                    td_mta::postgres_smtp_storage::Backend::new(
                        td_mta::postgres_smtp_receiving::Receiving {
                            loaded: &loaded,
                            executable: &executable,
                            binding,
                            additional: &receiving_bindings,
                            slots: &slots,
                            clock: clock.as_ref(),
                            pending: &smtp_pending,
                        },
                        clock.clone(),
                        policies.id().get(),
                    )
                    .map_err(|error| adapter("storage", error))?,
                )
            } else {
                None
            };
            let receiving = storage.as_ref().map(|storage| Receiving {
                storage,
                routes: records.routes(),
                resources,
                admission: plans.admission(),
                timeouts: plans.timeouts(),
                policies: &policies,
                handshakes: &handshakes,
                listeners: &listeners,
                clock: clock.clone(),
            });
            super::signal_sys::block().map_err(|_| config_failure("signals", "io"))?;
            let endpoint = Endpoint::bind(runtime, runtime_path)?;
            let (logging, log_writer) = td_mta::application_log::Producer::new(
                clock.clone(),
                binding.selection().generation,
                candidate.minimum_severity(),
            )
            .map_err(|error| adapter("logging", error))?;
            let control = Control::with_logging(logging);
            control.log(td_mta::observability::Kind::BootStarted, None, None);
            control.log(td_mta::observability::Kind::ConfigActivated, None, None);
            let log_finished = AtomicBool::new(false);
            if super::signal_sys::requested().map_err(|_| config_failure("signals", "io"))? {
                control.stop();
            }
            let finished = AtomicBool::new(false);
            let queue_control = td_mta::postgres_relay_worker::QueueControl::default();
            std::thread::scope(|scope| {
                let _log_finish = LogFinish {
                    finished: &log_finished,
                    control: &control,
                };
                let _log_worker = std::thread::Builder::new()
                    .name("database-logs".into())
                    .spawn_scoped(scope, || {
                        if let Some(producer) = control.logging() {
                            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                log_writer.run(
                                    producer,
                                    &loaded,
                                    &executable,
                                    binding.epoch(),
                                    binding.profile(),
                                    &log_finished,
                                )
                            }))
                            .is_err()
                            {
                                producer.writer_failed();
                            }
                        }
                    })
                    .map_err(|_| config_failure("logging", "worker-start"))?;
                let _early_finish = ControlFinish {
                    finished: &finished,
                    control: &control,
                };
                let certificate_worker = std::thread::Builder::new()
                    .name("certificate-watch".into())
                    .spawn_scoped(scope, || {
                        if managed_names.is_empty() {
                            return;
                        }
                        let mut next = std::time::Instant::now();
                        while !control.stop_requested() {
                            if std::time::Instant::now() < next {
                                std::thread::sleep(std::time::Duration::from_millis(100));
                                continue;
                            }
                            next = std::time::Instant::now() + std::time::Duration::from_secs(30);
                            let until =
                                std::time::Instant::now() + std::time::Duration::from_secs(10);
                            let observation = (|| {
                                let addresses = td_mta::postgres_resolver::resolve(
                                    &executable,
                                    &loaded.bootstrap,
                                    until.min(
                                        std::time::Instant::now()
                                            + std::time::Duration::from_secs(5),
                                    ),
                                )
                                .ok()?;
                                for address in addresses {
                                    let mut request = loaded.bootstrap.request(address);
                                    request.mode = td_mta::postgres_probe::Mode::Runtime;
                                    if let Ok(mut revisions) = td_mta::postgres_acme::generations(
                                        &request,
                                        &loaded.trust,
                                        until,
                                        binding.epoch(),
                                    ) {
                                        revisions.retain(|(name, _)| managed_names.contains(name));
                                        return Some(revisions);
                                    }
                                }
                                None
                            })();
                            if observation
                                .is_some_and(|revisions| revisions != certificate_revisions)
                                && !control.stop_requested()
                            {
                                reload.store(true, Ordering::Release);
                                control.stop();
                            }
                        }
                    })
                    .map_err(|_| config_failure("certificate", "worker-start"))?;
                let worker = std::thread::Builder::new()
                    .name("smtp-control".into())
                    .spawn_scoped(scope, || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            endpoint.run(
                                &control,
                                &finished,
                                mailbox_access,
                                relay.then_some(&queue_control),
                            )
                        }));
                        if !matches!(result, Ok(Ok(()))) {
                            control.fail();
                        }
                        result
                    })
                    .map_err(|_| config_failure("control", "worker-start"))?;
                let finish = ControlFinish {
                    finished: &finished,
                    control: &control,
                };
                let http_worker = if let Some(https) = https.as_ref() {
                    match std::thread::Builder::new()
                        .name("https-network".into())
                        .spawn_scoped(scope, || {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    https.run(&control)
                                }));
                            if !matches!(result, Ok(Ok(()))) {
                                control.fail();
                            }
                            result
                        }) {
                        Ok(worker) => Some(worker),
                        Err(_) => {
                            control.stop();
                            drop(finish);
                            let _ = worker.join();
                            return Err(config_failure("https", "worker-start"));
                        }
                    }
                } else {
                    None
                };
                let relay_worker = if relay {
                    let relay_service = td_mta::postgres_relay_worker::Worker {
                        queue: &queue_control,
                        request: &relay_request,
                        trust: &loaded.trust,
                        profile: &published,
                        clock: clock.clone(),
                        handshakes: &handshakes,
                        maximum: limits.message_bytes as u64,
                    };
                    Some(
                        std::thread::Builder::new()
                            .name("smtp-relay".into())
                            .spawn_scoped(scope, {
                                let control = &control;
                                let relay_ready = &relay_ready;
                                move || {
                                    let result =
                                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                                            || relay_service.run(control, relay_ready),
                                        ));
                                    if !matches!(result, Ok(Ok(()))) {
                                        control.fail();
                                    }
                                    result
                                }
                            })
                            .map_err(|_| config_failure("relay", "worker-start"))?,
                    )
                } else {
                    None
                };
                let ready = || {
                    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while (mailbox_access && !https_ready.load(Ordering::Acquire))
                        || (relay && !relay_ready.load(Ordering::Acquire))
                    {
                        if control.stop_requested() || std::time::Instant::now() >= until {
                            control.stop();
                            return Err(ports::Error::WriterStopped);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    if !listeners.is_empty() {
                        control.log(
                            td_mta::observability::Kind::ListenerReady {
                                protocol: td_mta::observability::Listener::Smtp,
                            },
                            None,
                            None,
                        );
                    }
                    if mailbox_access {
                        control.log(
                            td_mta::observability::Kind::ListenerReady {
                                protocol: td_mta::observability::Listener::Jmap,
                            },
                            None,
                            None,
                        );
                    }
                    announce(listeners.len(), mailbox_access, relay)
                };
                let result = match receiving.as_ref() {
                    Some(receiving) => receiving.run_with_ready(&control, ready),
                    None => control.wait_with_ready(ready),
                }
                .map_err(|error| ConfigFailure::from(adapter("serving", error)));
                drop(finish);
                control.stop();
                let joined = worker.join();
                if certificate_worker.join().is_err() {
                    return Err(config_failure("certificate", "worker-stopped"));
                }
                if control.user_stopped() {
                    reload.store(false, Ordering::Release);
                }
                if let Some(worker) = http_worker {
                    if !matches!(worker.join(), Ok(Ok(Ok(())))) {
                        return Err(config_failure("https", "worker-stopped"));
                    }
                }
                if let Some(worker) = relay_worker {
                    if !matches!(worker.join(), Ok(Ok(Ok(())))) {
                        return Err(config_failure("relay", "worker-stopped"));
                    }
                }
                result?;
                match joined {
                    Ok(Ok(Ok(()))) => Ok(()),
                    _ => Err(config_failure("control", "worker-stopped")),
                }
            })
        })
        .map_err(|_| config_failure("configuration", "invariant"))?
}

fn same_owner_root(
    path: &str,
    owner: u32,
    stage: &'static str,
) -> Result<PrivateRoot, ConfigFailure> {
    let root = PrivateRoot::open(path).map_err(|_| config_failure(stage, "root-policy-or-io"))?;
    if root
        .directory()
        .metadata()
        .map_err(|_| config_failure(stage, "io"))?
        .uid()
        != owner
    {
        return Err(config_failure(stage, "root-owner"));
    }
    Ok(root)
}
fn announce(listeners: usize, mailbox_access: bool, relay: bool) -> Result<(), ports::Error> {
    let mut output = io::stdout().lock();
    let profile = if mailbox_access {
        "mailbox-access"
    } else {
        "smtp-only"
    };
    writeln!(output, "{{\"schema\":1,\"command\":\"serve\",\"status\":\"ready\",\"profile\":\"{profile}\",\"smtp_listeners\":{listeners},\"https\":{mailbox_access},\"outbound\":{relay},\"reload\":false,\"signal_drain\":true}}")?;
    output.flush()?;
    Ok(())
}

struct ControlFinish<'a> {
    finished: &'a AtomicBool,
    control: &'a Control,
}
impl Drop for ControlFinish<'_> {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
        self.control.stop();
    }
}

struct LogFinish<'a> {
    finished: &'a AtomicBool,
    control: &'a Control,
}
impl Drop for LogFinish<'_> {
    fn drop(&mut self) {
        self.control.log(
            td_mta::observability::Kind::ServiceStopped {
                failed: std::thread::panicking()
                    || self.control.state() == td_mta::smtp_receiving::State::Failed,
            },
            None,
            None,
        );
        self.finished.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::panic)]
    fn coordinator_unwind_stops_scoped_workers() {
        let control = Control::default();
        let finished = AtomicBool::new(false);
        let observed = AtomicBool::new(false);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
                    while !control.stop_requested() && std::time::Instant::now() < until {
                        std::thread::yield_now();
                    }
                    observed.store(
                        control.stop_requested() && finished.load(Ordering::Acquire),
                        Ordering::Release,
                    );
                });
                let _finish = ControlFinish {
                    finished: &finished,
                    control: &control,
                };
                let _worker = worker;
                panic!("injected coordinator failure");
            });
        }));
        assert!(result.is_err());
        assert!(observed.load(Ordering::Acquire));
        assert!(control.stop_requested());
        assert!(finished.load(Ordering::Acquire));
    }
}
