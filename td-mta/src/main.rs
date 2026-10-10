//! Offline account verification and packaging entry point.
#![forbid(unsafe_code)]

use std::{
    ffi::OsStr,
    io::{self, Write},
    process::ExitCode,
    sync::Arc,
};
use td_mta::{
    account_checks::CompleteChecks,
    clock::RuntimeClock,
    ids::AccountId,
    limits::SQLITE_DATABASE_BYTES,
    metadata_sweep,
    ports::{self, Clock, Deadline},
    store_fs::{
        AccountCheckError, AccountCheckLimits, BodyCheckLimits, IndexStore, LockError, PrivateRoot,
    },
};

const HELP: &str = "Usage: td-mta --version | --help\n       td-mta store verify --root PATH --account ID [--timeout-seconds N]\n\nVerify database integrity and the selected account's metadata and body digests.\nRun offline with access to the private store; the writer lock must be free.\nJSON output describes only the selected account. Default timeout: 600 seconds.\nNo repair or serving commands are available.\n";

struct Verify {
    root: String,
    account: AccountId,
    timeout_ms: u64,
}

fn arguments(mut args: impl Iterator<Item = std::ffi::OsString>) -> Option<Verify> {
    let mut root = None;
    let mut account = None;
    let mut timeout = None;
    while let Some(flag) = args.next() {
        let value = args.next()?.into_string().ok()?;
        match flag.to_str()? {
            "--root" if root.is_none() => root = Some(value),
            "--account" if account.is_none() => account = Some(AccountId::parse(&value).ok()?),
            "--timeout-seconds" if timeout.is_none() => {
                if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                let seconds = value.parse::<u64>().ok()?;
                if seconds == 0 {
                    return None;
                }
                timeout = Some(seconds.checked_mul(1000)?);
            }
            _ => return None,
        }
    }
    Some(Verify {
        root: root?,
        account: account?,
        timeout_ms: timeout.unwrap_or(600_000),
    })
}

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

fn verify(options: Verify) -> Result<CompleteChecks, Failure> {
    let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
    verify_with_clock(options, clock)
}

fn verify_with_clock(options: Verify, clock: Arc<dyn Clock>) -> Result<CompleteChecks, Failure> {
    let started = clock.sample().map_err(|error| adapter("clock", error))?;
    let deadline = Deadline::after(started.monotonic, options.timeout_ms).map_err(|_| Failure {
        stage: "arguments",
        code: "invalid-arguments",
    })?;
    let root = PrivateRoot::open(&options.root).map_err(|_| Failure {
        stage: "root",
        code: "root-policy-or-io",
    })?;
    let mut root = root.try_lock().map_err(|error| Failure {
        stage: "lock",
        code: match error {
            LockError::Busy => "busy",
            LockError::Policy => "lock-policy",
            LockError::Io(_) => "io",
        },
    })?;
    let store = IndexStore::open(&mut root, Arc::clone(&clock), 1, deadline)
        .map_err(|error| adapter("open", error))?;
    store
        .validate_integrity(deadline)
        .map_err(|error| adapter("physical", error))?;
    let mut view = store
        .maintenance_view(options.account, deadline)
        .map_err(|error| adapter("account", error))?;
    let utc_ms = clock
        .sample()
        .map_err(|error| adapter("clock", error))?
        .utc_ms;
    // The captured maintenance VM allowance and absolute deadline bound all passes.
    let limits = AccountCheckLimits {
        metadata: metadata_sweep::Limits {
            rows: u64::MAX,
            parent_reads: u64::MAX,
        },
        bodies: BodyCheckLimits {
            blobs: u64::MAX,
            bytes: SQLITE_DATABASE_BYTES,
        },
    };
    view.verify_account(&td_crypto::Provider, utc_ms, limits, &mut [0; 65536])
        .map_err(|error| match error {
            AccountCheckError::Bodies(error) => adapter("bodies", error),
            AccountCheckError::Metadata(_) => Failure {
                stage: "metadata",
                code: "verification-failed",
            },
            AccountCheckError::Reports(_) => Failure {
                stage: "reports",
                code: "verification-failed",
            },
        })
}

fn failure(output: &mut impl Write, command: &str, error: Failure) -> io::Result<()> {
    writeln!(output, "{{\"schema\":1,\"command\":\"{}\",\"status\":\"error\",\"stage\":\"{}\",\"error\":\"{}\"}}", command, error.stage, error.code)
}

fn run() -> io::Result<ExitCode> {
    let mut args = std::env::args_os();
    let _ = args.next();
    let first = args.next();
    if first.as_deref() == Some(OsStr::new("store")) {
        if args.next().as_deref() != Some(OsStr::new("verify")) {
            failure(
                &mut io::stdout().lock(),
                "store",
                Failure {
                    stage: "arguments",
                    code: "invalid-arguments",
                },
            )?;
            return Ok(ExitCode::from(2));
        }
        let Some(options) = arguments(args) else {
            failure(
                &mut io::stdout().lock(),
                "store.verify",
                Failure {
                    stage: "arguments",
                    code: "invalid-arguments",
                },
            )?;
            return Ok(ExitCode::from(2));
        };
        return match verify(options) {
            Ok(report) => {
                let identity = report.identity();
                let metadata = report.metadata();
                let bodies = report.bodies();
                writeln!(io::stdout().lock(), "{{\"schema\":1,\"command\":\"store.verify\",\"status\":\"ok\",\"scope\":\"account\",\"physical_integrity\":true,\"account\":\"{}\",\"epoch\":\"{}\",\"sequence\":{},\"history_floor\":{},\"checked_at_ms\":{},\"metadata_rows\":{},\"mailboxes\":{},\"submissions\":{},\"recipients\":{},\"blobs\":{},\"body_bytes\":{}}}", identity.account, identity.epoch, identity.committed_sequence.number(), identity.history_floor.number(), metadata.references().utc_ms(), metadata.references().rows(), metadata.mailboxes().mailboxes(), metadata.recipients().submissions(), metadata.recipients().recipients(), bodies.blobs(), bodies.bytes())?;
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => {
                let code = error.exit_code();
                failure(&mut io::stdout().lock(), "store.verify", error)?;
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use td_mta::ports::{Tick, Time};

    struct FixedClock(u64);
    impl Clock for FixedClock {
        fn sample(&self) -> Result<Time, ports::Error> {
            Ok(Time {
                monotonic: Tick(self.0),
                utc_ms: 17,
            })
        }
    }

    #[test]
    fn absolute_timeout_overflow_is_usage_before_root_access() {
        for (tick, stage, error_code, code) in [
            (615, "root", "root-policy-or-io", 1),
            (616, "arguments", "invalid-arguments", 2),
        ] {
            let error = verify_with_clock(
                Verify {
                    root: String::new(),
                    account: AccountId::from_bytes([1; 16]),
                    timeout_ms: 18_446_744_073_709_551_000,
                },
                Arc::new(FixedClock(tick)),
            )
            .err()
            .unwrap();
            assert_eq!(error.stage, stage);
            assert_eq!(error.code, error_code);
            assert_eq!(error.exit_code(), ExitCode::from(code));
        }
    }
}
