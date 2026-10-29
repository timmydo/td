//! PostgreSQL service and operator commands.
#![deny(unsafe_code)]
use std::{
    ffi::OsStr,
    io::{self, Write},
    process::ExitCode,
};
use td_mta::{
    ports::{self, Clock, Deadline},
    store_fs::{LockError, LockedRoot, PrivateRoot},
};
mod acme_command;
mod control;
mod listener_sys;
mod postgres;
mod serve;
mod signal_sys;

const HELP: &str = "Usage: td-mta --version | --help\n       td-mta acme --connection-file PATH --certificate-profile NAME\n           --directory HTTPS-URL --contact EMAIL --name HOST [--name HOST ...]\n           --bind ADDRESS:PORT --accept-terms\n       td-mta acme-status --connection-file PATH --certificate-profile NAME [--require-ready] [--minimum-validity-seconds N]\n       td-mta postgres probe --connection-file PATH --profile NAME\n       td-mta postgres runtime-check --connection-file PATH\n       td-mta postgres blob-scrub --connection-file PATH --epoch HEX --state-dir PATH [--limit N]\n       td-mta postgres blob-scrub-status --epoch HEX --state-dir PATH\n       td-mta postgres blob-scrub-page --connection-file PATH --epoch HEX [--limit N] [--after ACCOUNT:BLOB --through ACCOUNT:BLOB]\n       td-mta postgres maintenance-inspect --connection-file PATH --epoch HEX\n       td-mta postgres maintenance-enter --connection-file PATH --epoch HEX --instances-and-smtp-fenced\n       td-mta postgres recovery-release --connection-file PATH --epoch OLD_HEX --new-epoch FRESH_HEX --max-rows N --instances-and-smtp-fenced --configuration-reviewed\n       td-mta migration export --archive PATH --max-bytes N --session HTTPS_URL --account SOURCE_ID --username USER --password-file PATH [--ca-file PATH]\n       td-mta migration verify --archive PATH --max-bytes N\n       td-mta migration compare --archive PATH --against-archive PATH --max-bytes N\n       td-mta migration inventory --archive PATH --max-bytes N --session HTTPS_URL --account SOURCE_ID --username USER --password-file PATH [--ca-file PATH]\n       td-mta migration import --archive PATH --max-bytes N --connection-file PATH --epoch HEX --account ID [--only-unmapped]\n       td-mta migration plan --archive PATH --max-bytes N --connection-file PATH --epoch HEX --account ID [--mailboxes]\n       td-mta migration apply --archive PATH --max-bytes N --connection-file PATH --epoch HEX --account ID --plan-file PATH\n       td-mta migration deletion-plan|deletion-apply --archive PATH --previous-archive PATH --max-bytes N --connection-file PATH --epoch HEX --account TARGET_ID --session HTTPS_URL --username USER --password-file PATH [--ca-file PATH] [--plan-file PATH] [--mailboxes]\n       td-mta postgres replica-discover --source-origin HTTPS_ORIGIN --username USER --password-file PATH [--ca-file PATH]\n       td-mta postgres replica-progress --connection-file PATH --epoch HEX --account ID\n       td-mta postgres replica-account-init --connection-file PATH --epoch HEX --account ID\n           --source-origin HTTPS_ORIGIN --source-epoch HEX --username USER --password-file PATH [--ca-file PATH]\n           --body-bytes N --blobs N --emails N --receipts N\n       td-mta postgres replica-follow --connection-file PATH --epoch HEX --account ID\n           --source-origin HTTPS_ORIGIN --source-epoch HEX --username USER --password-file PATH [--ca-file PATH]\n           --state-dir PRIVATE_DIRECTORY [--idle-seconds N]\n       td-mta postgres replica-follow-status --state-dir PRIVATE_DIRECTORY --epoch HEX --account ID [--maximum-age-seconds N]\n       td-mta postgres replica-copy --connection-file PATH --epoch HEX --account ID\n           --source-origin HTTPS_ORIGIN --source-epoch HEX --username USER --password-file PATH [--ca-file PATH]\n           [--steps N] [--timeout-seconds N]\n       td-mta postgres replica-source-init --connection-file PATH --epoch HEX --source-origin HTTPS_ORIGIN --source-epoch HEX\n       td-mta postgres replica-source-inspect --connection-file PATH --epoch HEX\n       td-mta postgres schema-init --connection-file PATH\n       td-mta postgres schema-inspect --connection-file PATH\n       td-mta postgres schema-upgrade --connection-file PATH\n       td-mta postgres account-list --connection-file PATH [--epoch HEX]\n       td-mta postgres account-create --connection-file PATH --address EMAIL [--name NAME] [--username USER] --operation-file PATH\n       td-mta postgres account-update --connection-file PATH --account ID [--name NAME] [--username USER] [--outbound-profile NAME] --operation-file PATH\n       td-mta postgres (account-alias-add | account-alias-remove) --connection-file PATH --account ID --address EMAIL --operation-file PATH\n       td-mta postgres (account-apply | account-operation-inspect) --connection-file PATH --operation-file PATH\n       td-mta postgres account-identity-add --connection-file PATH --account ID --address EMAIL [--name NAME] --operation-file PATH\n       td-mta postgres account-identity-update --connection-file PATH --account ID --identity ID [--address EMAIL] [--name NAME] --operation-file PATH\n       td-mta postgres account-identity-remove --connection-file PATH --account ID --identity ID --operation-file PATH\n       td-mta postgres account-init --connection-file PATH --epoch HEX --account ID\n       td-mta postgres account-inspect --connection-file PATH --epoch HEX --account ID\n       td-mta postgres metadata-audit --connection-file PATH --epoch HEX --account ID --max-rows N --max-parent-reads N [--timeout-seconds N]\n       td-mta postgres blob-audit --connection-file PATH --epoch HEX --account ID --blob ID\n       td-mta postgres account-audit --connection-file PATH --epoch HEX --account ID --max-rows N --max-parent-reads N --max-blobs N --max-bytes N [--timeout-seconds N]\n       td-mta postgres logs --connection-file PATH --epoch HEX [--limit N] [--before ID]\n       td-mta logging-status --runtime PATH\n       td-mta performance-status --runtime PATH\n       td-mta postgres usage-inspect --connection-file PATH --epoch HEX --account ID\n       td-mta postgres device-list --connection-file PATH --epoch HEX --account ID\n       td-mta postgres device-create --connection-file PATH --epoch HEX --account ID --label LABEL\n       td-mta postgres device-revoke --connection-file PATH --epoch HEX --account ID --device ID\n       td-mta postgres config-selection-prepare --connection-file PATH --epoch HEX --version HEX --generation N\n       td-mta postgres (config-select | config-selection-inspect) --connection-file PATH --request HEX\n       td-mta postgres config-version\n       td-mta postgres config-import --connection-file PATH --epoch HEX --version HEX\n           --shared-file PATH --instance NAME ROLES PATH [--instance NAME ROLES PATH ...]\n       td-mta postgres config-snapshot --connection-file PATH --epoch HEX\n       td-mta postgres config-check --connection-file PATH --epoch HEX --version HEX\n       td-mta postgres material-version\n       td-mta postgres (material-import | material-check) --connection-file PATH\n           --input-file PATH --kind KIND --epoch HEX --version HEX\n       td-mta serve --connection-file PATH --profile NAME --runtime PATH --scratch PATH [--read-only]\n       td-mta status --json [--require-ready] --runtime PATH\n       td-mta stop --runtime PATH\n       td-mta queue-clock-status --runtime PATH\n       td-mta queue-clock-confirm --runtime PATH\n       td-mta queue-route-status --runtime PATH\n       td-mta queue-route-repair --runtime PATH\n\nServe loads its PostgreSQL profile with a dedicated runtime login.\nUse private per-instance runtime and scratch directories.\nReceive, JMAP and relay roles are supported; configuration changes require restart; managed certificate updates drain and reload.\nStatus --require-ready exits 1 unless all selected network workers are ready.\nStop requests orderly drain; SIGTERM and SIGINT do the same.\nQueue clock confirmation permits overdue expiry after five minutes of stable clock progress.\nQueue route repair requests release of a TLS/authentication pause after a newer configuration.\nBoth request commands acknowledge requested, not completion; inspect their status afterward.\nDevice administration uses the postgres device commands.\nPhysical backup and recovery use PostgreSQL tooling; no local store exists.\nPostgres probe checks verified TLS/SCRAM and primary durability settings.\nIt does not check a schema/profile, write mail, or start service.\nIts connection file needs a private parent; see POSTGRES.md.\nPostgres runtime-check verifies a dedicated non-owner login and its data privileges.\nIt does not grant privileges or activate a profile.\nPostgres maintenance-enter requires external instance and SMTP fencing.\nIt only enters maintenance. recovery-release requires restored devices revoked, quarantines pending delivery and changes epoch.\nRetain both epochs after uncertainty; reimport configuration and create fresh credentials before service.\nRetain the supplied epoch to resolve uncertain entry with maintenance-inspect.\nPostgres schema-init requires an owner login and creates a fresh preparation schema.\nPostgres schema-inspect checks that ledger; schema-upgrade applies known migrations.\nThese commands never enable mail serving.\nPostgres metadata-audit checks canonical rows and references with explicit scan bounds.\nIt excludes body digests, counters, mailbox-role uniqueness and restore approval.\nPostgres blob-audit verifies one historical body and reports metadata without downloading it.\nIt does not check counters or references, repair bytes, or approve recovery.\nPostgres account-audit checks canonical metadata, unique mailbox roles, counters and all body digests.\nAll four scan bounds are required; audits default to ten seconds, with --timeout-seconds 1..3600.\nIts historical report does not certify full semantic integrity or restore safety.\nPostgres usage-inspect compares historical usage counters without repair.\nIt does not certify body contents, full semantic integrity or restore safety.\nPostgres device-list reads historical public registry metadata for one account.\nIt never prints passwords, salts or verifiers and grants no serving authority.\nPostgres device-create prints its password only after confirmed creation.\nLost or uncertain creation output requires inspection and revocation before replacement.\nPostgres device-revoke disables the named preparation credential without deletion.\nAfter an indeterminate result, inspect that device; never assume revocation succeeded.\nSelection prepare emits a public request to retain before publication.\nSelect and inspect consume that exact request; uncertainty never permits replacement.\nSelection receipts are historical and do not activate service.\nConfig import validates every profile and stores one immutable source version.\nROLES is a comma-separated selection of receive,jmap,relay.\nConfig check reloads that version and revalidates current materials; neither activates it.\nRetain the config version ID after uncertainty; never substitute a new ID.\nMaterial import/check uses protected files and retained epoch/version IDs.\nIt stores or compares raw bytes; it does not validate or activate a profile.\nAn indeterminate schema operation must settle and be inspected before retry.\nNo repair command is available.\n";

struct Failure {
    stage: &'static str,
    code: &'static str,
}

impl Failure {
    fn exit_code(&self) -> ExitCode {
        if self.stage == "arguments" {
            ExitCode::from(2)
        } else {
            ExitCode::FAILURE
        }
    }
}

fn adapter(stage: &'static str, error: ports::Error) -> Failure {
    let code = match error {
        ports::Error::Capacity => "capacity",
        ports::Error::Quota => "quota",
        ports::Error::Busy => "busy",
        ports::Error::Deadline => "deadline",
        ports::Error::NotFound => "not-found",
        ports::Error::Forbidden => "forbidden",
        ports::Error::Invalid => "invalid",
        ports::Error::Corrupt => "corrupt",
        ports::Error::Conflict => "conflict",
        ports::Error::HistoryLost => "history-lost",
        ports::Error::WriterStopped => "writer-stopped",
        ports::Error::Entropy => "entropy",
        ports::Error::Crypto => "crypto",
        ports::Error::Tls => "tls",
        ports::Error::Dns => "dns",
        ports::Error::Io { .. } => "io",
    };
    Failure { stage, code }
}

fn scope(clock: &dyn Clock, timeout_ms: u64) -> Result<Deadline, Failure> {
    let started = clock.sample().map_err(|error| adapter("clock", error))?;
    Deadline::after(started.monotonic, timeout_ms).map_err(|_| Failure {
        stage: "arguments",
        code: "invalid-arguments",
    })
}

fn lock_root(root: PrivateRoot, stage: &'static str) -> Result<LockedRoot, Failure> {
    root.try_lock().map_err(|error| Failure {
        stage,
        code: match error {
            LockError::Busy => "busy",
            LockError::Policy => "lock-policy",
            LockError::Io(_) => "io",
        },
    })
}

struct ConfigFailure {
    failure: Failure,
}
impl From<Failure> for ConfigFailure {
    fn from(failure: Failure) -> Self {
        Self { failure }
    }
}
fn config_failure(stage: &'static str, code: &'static str) -> ConfigFailure {
    Failure { stage, code }.into()
}
fn json_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            c if c.is_control() => output.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => output.push(c),
        }
    }
    output
}

fn configuration_failure(
    output: &mut impl Write,
    command: &str,
    error: ConfigFailure,
) -> io::Result<()> {
    writeln!(output, "{{\"schema\":1,\"command\":\"{command}\",\"status\":\"error\",\"stage\":\"{}\",\"error\":\"{}\"}}", error.failure.stage, error.failure.code)
}

fn run() -> io::Result<ExitCode> {
    let mut args = std::env::args_os();
    let _ = args.next();
    let first = args.next();
    if first.as_deref() == Some(OsStr::new("--internal-postgres-resolve")) {
        if args.next().is_some() {
            return Ok(ExitCode::from(2));
        }
        td_mta::postgres_resolver::run_helper_and_exit();
    }
    if first.as_deref() == Some(OsStr::new("acme-status")) {
        return match acme_command::status(args) {
            Ok(true) => Ok(ExitCode::SUCCESS),
            Ok(false) => Ok(ExitCode::FAILURE),
            Err(error) => {
                let code = error.failure.exit_code();
                configuration_failure(&mut io::stdout().lock(), "acme-status", error)?;
                Ok(code)
            }
        };
    }
    if first.as_deref() == Some(OsStr::new("acme")) {
        let result = acme_command::arguments(args)
            .ok_or_else(|| config_failure("arguments", "invalid-arguments"))
            .and_then(acme_command::run);
        return match result {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(error) => {
                let code = error.failure.exit_code();
                configuration_failure(&mut io::stdout().lock(), "acme", error)?;
                Ok(code)
            }
        };
    }
    if first.as_deref() == Some(OsStr::new("migration")) {
        return td_mta::migration::command(args);
    }
    if first.as_deref() == Some(OsStr::new("postgres")) {
        return postgres::run(args);
    }
    if matches!(
        first.as_deref().and_then(|s| s.to_str()),
        Some(
            "queue-clock-confirm"
                | "queue-clock-status"
                | "queue-route-repair"
                | "queue-route-status"
        )
    ) {
        let command = first
            .as_deref()
            .and_then(|s| s.to_str())
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let result = (|| {
            if args.next().as_deref() != Some(OsStr::new("--runtime")) {
                return Err(config_failure("arguments", "invalid-arguments"));
            }
            let path = args
                .next()
                .and_then(|p| p.into_string().ok())
                .ok_or_else(|| config_failure("arguments", "invalid-arguments"))?;
            if args.next().is_some() {
                return Err(config_failure("arguments", "invalid-arguments"));
            }
            control::clock_request(&path, command)
        })();
        return match result {
            Ok(status) => {
                writeln!(
                    io::stdout().lock(),
                    "{{\"schema\":1,\"command\":\"{command}\",\"status\":\"{status}\"}}"
                )?;
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                let code = error.failure.exit_code();
                configuration_failure(&mut io::stdout().lock(), command, error)?;
                Ok(code)
            }
        };
    }
    if matches!(first.as_deref(), Some(value) if value == OsStr::new("logging-status") || value == OsStr::new("performance-status"))
    {
        let performance = first.as_deref() == Some(OsStr::new("performance-status"));
        let command = if performance {
            "performance-status"
        } else {
            "logging-status"
        };
        let runtime = (|| {
            if args.next().as_deref() != Some(OsStr::new("--runtime")) {
                return None;
            }
            let path = args.next()?.into_string().ok()?;
            args.next().is_none().then_some(path)
        })();
        return match runtime
            .ok_or_else(|| config_failure("arguments", "invalid-arguments"))
            .and_then(|path| {
                if performance {
                    control::performance_request(&path)
                } else {
                    control::logging_request(&path)
                }
            }) {
            Ok(json) => {
                writeln!(io::stdout().lock(), "{json}")?;
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                configuration_failure(&mut io::stdout().lock(), command, error)?;
                Ok(ExitCode::FAILURE)
            }
        };
    }
    if matches!(first.as_deref(), Some(value) if value == OsStr::new("status") || value == OsStr::new("stop"))
    {
        let stop = first.as_deref() == Some(OsStr::new("stop"));
        let command = if stop { "stop" } else { "status" };
        let mut require_ready = false;
        let runtime = (|| {
            if !stop && args.next().as_deref() != Some(OsStr::new("--json")) {
                return None;
            }
            let mut flag = args.next();
            if !stop && flag.as_deref() == Some(OsStr::new("--require-ready")) {
                require_ready = true;
                flag = args.next();
            }
            if flag.as_deref() != Some(OsStr::new("--runtime")) {
                return None;
            }
            let path = args.next()?.into_string().ok()?;
            args.next().is_none().then_some(path)
        })();
        let result = runtime
            .ok_or_else(|| config_failure("arguments", "invalid-arguments"))
            .and_then(|path| control::request(&path, stop));
        return match result {
            Ok((state, profile)) => {
                let mut output = io::stdout().lock();
                if stop {
                    writeln!(output, "{{\"schema\":1,\"command\":\"stop\",\"status\":\"requested\",\"profile\":\"{profile}\"}}")?;
                } else {
                    writeln!(output, "{{\"schema\":1,\"command\":\"status\",\"status\":\"ok\",\"profile\":\"{profile}\",\"state\":\"{state}\",\"ready\":{}}}", state == "ready")?;
                }
                Ok(if require_ready && state != "ready" {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                })
            }
            Err(error) => {
                let code = error.failure.exit_code();
                configuration_failure(&mut io::stdout().lock(), command, error)?;
                Ok(code)
            }
        };
    }
    if first.as_deref() == Some(OsStr::new("serve")) {
        let result = serve::arguments(args)
            .ok_or_else(|| config_failure("arguments", "invalid-arguments"))
            .and_then(serve::run);
        return match result {
            Ok(()) => Ok(ExitCode::SUCCESS),
            Err(error) => {
                let code = error.failure.exit_code();
                configuration_failure(&mut io::stdout().lock(), "serve", error)?;
                Ok(code)
            }
        };
    }
    let extra = args.next().is_some();
    if !extra && first.as_deref() == Some(OsStr::new("--version")) {
        writeln!(io::stdout().lock(), "td-mta {}", env!("CARGO_PKG_VERSION"))?;
        return Ok(ExitCode::SUCCESS);
    }
    if !extra && first.as_deref() == Some(OsStr::new("--help")) {
        io::stdout().lock().write_all(HELP.as_bytes())?;
        return Ok(ExitCode::SUCCESS);
    }
    io::stderr().lock().write_all(HELP.as_bytes())?;
    Ok(ExitCode::from(2))
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(_) => ExitCode::FAILURE,
    }
}
