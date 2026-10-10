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
        Arc,
    },
};
use td_mta::{
    admission::logical::Cell,
    clock::RuntimeClock,
    config::listener::Kind,
    ownership::SlotState,
    ports::{self, Clock},
    smtp_receiving::{BoundListener, Control, Receiving},
    store_fs::{AuxiliaryUsage, IngressSpool, LedgerInitError, StoreCoordinator},
    store_fs::{IndexStore, PrivateRoot},
    tls_admission::HandshakePool,
};

pub(super) fn run(path: &str) -> Result<(), ConfigFailure> {
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
                    // Required by the complete schema; explicitly inactive in this profile.
                    Kind::Https => (),
                    _ => return Err(config_failure("listeners", "unsupported-listener")),
                }
            }
            if direct.is_empty() {
                return Err(config_failure("listeners", "missing-direct-smtp"));
            }
            let mut root = lock_root(root, "store-lock")?;
            let mut spool_root = lock_root(spool_root, "spool-lock")?;
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
            // One lease per SMTP delivery plus the serial publication effect ticket.
            let count = limits.smtp_sessions + 1;
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
                    // No sort/cache/response/log/cold owners are active in this profile.
                    sort_bytes: 0,
                    response_bytes: 0,
                    cache_bytes: 0,
                    log_bytes: 0,
                    cold_bytes: 0,
                },
                &mut states,
                &mut cells,
                (),
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
            let endpoint = Endpoint::bind(runtime, runtime_path)?;
            let control = Control::default();
            let finished = AtomicBool::new(false);
            std::thread::scope(|scope| {
                let worker = std::thread::Builder::new()
                    .name("smtp-control".into())
                    .spawn_scoped(scope, || {
                        let result = std::panic::catch_unwind(|| endpoint.run(&control, &finished));
                        if !matches!(result, Ok(Ok(()))) {
                            control.stop();
                        }
                        result
                    })
                    .map_err(|_| config_failure("control", "worker-start"))?;
                let finish = ControlFinish(&finished);
                let result = receiving
                    .run_with_ready(&control, || announce(listeners.len()))
                    .map_err(|error| ConfigFailure::from(adapter("receiving", error)));
                drop(finish);
                let joined = worker.join();
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
fn announce(listeners: usize) -> Result<(), ports::Error> {
    let mut output = io::stdout().lock();
    writeln!(output, "{{\"schema\":1,\"command\":\"serve\",\"status\":\"ready\",\"profile\":\"smtp-only\",\"smtp_listeners\":{listeners},\"https\":false,\"outbound\":false,\"reload\":false,\"signal_drain\":false}}")?;
    output.flush()?;
    Ok(())
}

struct ControlFinish<'a>(&'a AtomicBool);
impl Drop for ControlFinish<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
