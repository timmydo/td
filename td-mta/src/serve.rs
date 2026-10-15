//! Initial foreground receiving profile. All authority comes from one protected
//! configuration load and retained store/spool locks.
use super::{
    adapter, config_failure, control::Endpoint, load_configuration, lock_root, scope,
    CheckedConfiguration, ConfigFailure,
};
use std::{
    io::{self, Write},
    net::TcpListener,
    os::unix::fs::MetadataExt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use td_mta::{
    admission::logical::Cell,
    clock::RuntimeClock,
    config::listener::Kind,
    ownership::SlotState,
    ports::{self, Clock},
    smtp_receiving::{BoundListener, Control, Receiving},
    store_fs::{AuxiliaryUsage, IngressSpool, LedgerInitError, RequestSpool, StoreCoordinator},
    store_fs::{DeviceStore, IndexStore, PrivateRoot},
    tls_admission::HandshakePool,
};

pub(super) fn run(path: &str, mailbox_access: bool) -> Result<(), ConfigFailure> {
    let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
    let CheckedConfiguration {
        resolved,
        prepared,
        mut generations,
        root,
        ..
    } = load_configuration(path, Arc::clone(&clock))?;
    let candidate = resolved.candidate();
    let globals = candidate
        .globals()
        .map_err(|_| config_failure("configuration", "invariant"))?;
    let owner = root
        .directory()
        .metadata()
        .map_err(|_| config_failure("data-root", "io"))?
        .uid();
    let runtime_path = globals
        .runtime()
        .map_err(|_| config_failure("configuration", "invariant"))?;
    super::control::check_runtime_path(runtime_path)?;
    let runtime = same_owner_root(runtime_path, owner, "runtime-root")?;
    let _logs = same_owner_root(
        globals
            .logs()
            .map_err(|_| config_failure("configuration", "invariant"))?,
        owner,
        "log-root",
    )?;
    let data = globals
        .data()
        .map_err(|_| config_failure("configuration", "invariant"))?;
    let spool_path = format!("{data}/ingress");
    // Deployment provisions this directory, just as it provisions data/runtime/logs.
    let spool_root = same_owner_root(&spool_path, owner, "spool-root")?;
    let plans = candidate.resources();
    let resources = plans.resources();
    let limits = resources.limits();
    let deadline = scope(clock.as_ref(), 600_000)?;
    drop(
        generations
            .publish(prepared)
            .map_err(|error| adapter("tls", error.error()))?,
    );
    let policies = generations
        .current()
        .ok_or_else(|| config_failure("tls", "invariant"))?;
    candidate
        .with_graph(|records, graph| {
            let view = records
                .view(graph)
                .map_err(|_| config_failure("configuration", "invariant"))?;
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
                    _ => return Err(config_failure("listeners", "unsupported-listener")),
                }
            }
            if direct.is_empty() {
                return Err(config_failure("listeners", "missing-direct-smtp"));
            }
            let mut root = lock_root(root, "store-lock")?;
            let mut spool_root = lock_root(spool_root, "spool-lock")?;
            let mut device_root = if mailbox_access {
                Some(lock_root(
                    same_owner_root(&format!("{data}/devices"), owner, "device-root")?,
                    "device-lock",
                )?)
            } else {
                None
            };
            let devices = match device_root.as_mut() {
                Some(root) => Some(Mutex::new(
                    DeviceStore::open(root, records.routes().account())
                        .map_err(|_| config_failure("devices", "unavailable"))?,
                )),
                None => None,
            };
            // Recover a previous mailbox run even when this run is SMTP-only.
            let request_path = format!("{data}/tmp/requests");
            let request_exists = std::path::Path::new(&request_path)
                .try_exists()
                .map_err(|_| config_failure("request-root", "unavailable"))?;
            if mailbox_access {
                td_mta::jmap_request::Request::validate_resources(resources)
                    .map_err(|_| config_failure("request-limits", "invalid"))?;
            }
            let mut request_root = if mailbox_access || request_exists {
                Some(lock_root(
                    same_owner_root(&request_path, owner, "request-root")?,
                    "request-lock",
                )?)
            } else {
                None
            };
            let request_spool = match request_root.as_mut() {
                Some(root) => Some(
                    RequestSpool::open(root, Arc::clone(&clock), deadline)
                        .map_err(|error| adapter("request-spool", error))?,
                ),
                None => None,
            };
            let store = IndexStore::open(
                &mut root,
                Arc::clone(&clock),
                limits.storage_views,
                deadline,
            )
            .map_err(|error| adapter("store-open", error))?;
            let accounts = store
                .account_ids(deadline)
                .map_err(|error| adapter("accounts", error))?;
            if accounts.as_slice() != [records.routes().account()] {
                return Err(config_failure("accounts", "configured-account-mismatch"));
            }
            let spool =
                IngressSpool::open(&mut spool_root, resources, Arc::clone(&clock), deadline)
                    .map_err(|error| adapter("spool-open", error))?;
            // SMTP deliveries, one HTTPS body job and the serial publication ticket.
            let count = limits.smtp_sessions + 1 + usize::from(mailbox_access);
            let mut states = Vec::new();
            let mut cells = Vec::new();
            states
                .try_reserve_exact(count)
                .map_err(|_| config_failure("admission", "capacity"))?;
            cells
                .try_reserve_exact(count)
                .map_err(|_| config_failure("admission", "capacity"))?;
            states.resize_with(count, || SlotState::EMPTY);
            cells.resize_with(count, || Cell::EMPTY);
            let coordinator = StoreCoordinator::new(
                store,
                plans.admission(),
                AuxiliaryUsage {
                    // Any existing request spool was cold-recovered before zero usage.
                    sort_bytes: 0,
                    response_bytes: 0,
                    cache_bytes: 0,
                    log_bytes: 0,
                    cold_bytes: 0,
                },
                &mut states,
                &mut cells,
                devices.as_ref().map(|devices| {
                    td_mta::http_auth::DevicePolicy::new(
                        devices,
                        clock.as_ref(),
                        policies.id().get(),
                    )
                }),
                deadline,
            )
            .map_err(|error| match error {
                LedgerInitError::Store(error) => ConfigFailure::from(adapter("admission", error)),
                LedgerInitError::Ledger(_) => config_failure("admission", "quota-or-capacity"),
            })?;
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
            let upload_service = td_mta::https_upload::Service {
                resources: plans.resources(),
                coordinator: &coordinator,
                spool: &spool,
                account: records.routes().account(),
                maximum: u64::try_from(limits.message_bytes)
                    .map_err(|_| config_failure("https", "capacity"))?,
                clock: clock.as_ref(),
                timeouts: plans.timeouts(),
            };
            let session = if mailbox_access {
                Some(
                    td_mta::jmap_session::Session::mail_access(
                        view.origin()
                            .map_err(|_| config_failure("configuration", "invariant"))?,
                        candidate
                            .identities()
                            .map_err(|_| config_failure("configuration", "invariant"))?
                            .account()
                            .map_err(|_| config_failure("configuration", "invariant"))?,
                        resources,
                        td_mta::jmap_session::Concurrency {
                            uploads: 1,
                            requests: 1,
                        },
                    )
                    .map_err(|error| adapter("session", error))?,
                )
            } else {
                None
            };
            let api_service = match (devices.as_ref(), request_spool.as_ref(), session.as_ref()) {
                (Some(devices), Some(spool), Some(session)) => Some(td_mta::https_api::Service {
                    transfers: &upload_service,
                    drafts: &upload_service,
                    coordinator: &coordinator,
                    spool,
                    policy: td_mta::http_auth::DevicePolicy::new(
                        devices,
                        clock.as_ref(),
                        policies.id().get(),
                    ),
                    resources,
                    session,
                    response_maximum: plans.admission().disk().response_bytes,
                    clock: clock.as_ref(),
                    timeouts: plans.timeouts(),
                }),
                (None, _, None) => None,
                _ => return Err(config_failure("https", "invariant")),
            };
            let https_ready = AtomicBool::new(false);
            let https = if let Some(devices) = devices.as_ref() {
                Some(td_mta::https::Https {
                    listeners: &https_listeners,
                    policies: &policies,
                    handshakes: &handshakes,
                    authentication: devices,
                    origin: view
                        .origin()
                        .map_err(|_| config_failure("configuration", "invariant"))?,
                    username: candidate
                        .identities()
                        .map_err(|_| config_failure("configuration", "invariant"))?
                        .account()
                        .map_err(|_| config_failure("configuration", "invariant"))?
                        .username,
                    resources,
                    timeouts: plans.timeouts(),
                    clock: Arc::clone(&clock),
                    uploads: Some(
                        api_service
                            .as_ref()
                            .ok_or_else(|| config_failure("https", "invariant"))?,
                    ),
                    ready: &https_ready,
                })
            } else {
                None
            };
            let receiving = Receiving {
                coordinator: &coordinator,
                spool: &spool,
                crypto: &td_crypto::Provider,
                routes: records.routes(),
                resources,
                admission: plans.admission(),
                timeouts: plans.timeouts(),
                policies: &policies,
                handshakes: &handshakes,
                listeners: &listeners,
                clock: Arc::clone(&clock),
            };
            super::signal_sys::block().map_err(|_| config_failure("signals", "io"))?;
            let endpoint = Endpoint::bind(runtime, runtime_path)?;
            let control = Control::default();
            if super::signal_sys::requested().map_err(|_| config_failure("signals", "io"))? {
                control.stop();
            }
            let device_control = devices.as_ref().map(|store| super::control::Devices {
                store,
                clock: clock.as_ref(),
            });
            let finished = AtomicBool::new(false);
            std::thread::scope(|scope| {
                let worker = std::thread::Builder::new()
                    .name("smtp-control".into())
                    .spawn_scoped(scope, || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            endpoint.run(&control, &finished, device_control.as_ref())
                        }));
                        if !matches!(result, Ok(Ok(()))) {
                            control.stop();
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
                                control.stop();
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
                let result = receiving
                    .run_with_ready(&control, || {
                        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
                        while mailbox_access && !https_ready.load(Ordering::Acquire) {
                            if control.stop_requested() || std::time::Instant::now() >= until {
                                control.stop();
                                return Err(ports::Error::WriterStopped);
                            }
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        announce(listeners.len(), mailbox_access)
                    })
                    .map_err(|error| ConfigFailure::from(adapter("receiving", error)));
                drop(finish);
                control.stop();
                let joined = worker.join();
                if let Some(worker) = http_worker {
                    if !matches!(worker.join(), Ok(Ok(Ok(())))) {
                        return Err(config_failure("https", "worker-stopped"));
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
fn announce(listeners: usize, mailbox_access: bool) -> Result<(), ports::Error> {
    let mut output = io::stdout().lock();
    let profile = if mailbox_access {
        "mailbox-access"
    } else {
        "smtp-only"
    };
    writeln!(output, "{{\"schema\":1,\"command\":\"serve\",\"status\":\"ready\",\"profile\":\"{profile}\",\"smtp_listeners\":{listeners},\"https\":{mailbox_access},\"outbound\":false,\"reload\":false,\"signal_drain\":true}}")?;
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
