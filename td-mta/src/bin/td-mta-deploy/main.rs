//! Local launcher and independently retained production deployment supervisor.
#![deny(unsafe_code)]
mod activate;
mod common;
mod isolation;
mod journald;
mod launcher;
mod physical;
#[cfg(test)]
#[path = "tests/postgres_fixture.rs"]
mod postgres_fixture;
mod postgres_minors;
mod probes;
mod readiness;
mod service_database;
#[path = "../../signal_sys.rs"]
mod signal_sys;
mod target;
#[cfg(test)]
#[path = "tests/tls_fixture.rs"]
mod tls_fixture;
use common::{child, require};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    process::ExitCode,
};
pub type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
fn load_job(work: &Path, owner: u32) -> R<td_json::Json> {
    let path = child(work, "job.json")?;
    let (mut file, meta) = td_fs::open_real_file(&path, "job")?;
    require(
        meta.uid() == owner && meta.mode() & 0o7777 == 0o600 && meta.len() <= 4096,
        "invalid production job",
    )?;
    let mut bytes = td_alloc::filled(usize::try_from(meta.len())?, 0)?;
    file.read_exact(&mut bytes)?;
    require(file.read(&mut [0])? == 0, "job changed while reading")?;
    Ok(td_json::parse_slice(&bytes)?)
}
// Every job but an activation names its operation.
fn check_mode(job: &td_json::Json, mode: &str) -> R<()> {
    let declared = match job.get("mode") {
        None => None,
        Some(value) => Some(value.as_str().ok_or("invalid job mode")?),
    };
    require(
        declared
            == match mode {
                "postgres-binaries"
                | "postgres-to-debian"
                | "retire-rollback"
                | "recover"
                | "rollback"
                | "restore-shared-services"
                | "refresh-notifications"
                | "converge-config" => Some(mode),
                _ => None,
            },
        "job operation does not match the requested mode",
    )
}
fn supervise(mode: &str) -> R<()> {
    let work = std::env::current_dir()?;
    let operator = Path::new("/var/lib/td-mta-operator");
    let meta = fs::symlink_metadata(&work)?;
    require(
        work.parent() == Some(operator)
            && meta.is_dir()
            && meta.uid() == 0
            && meta.mode() & 0o7777 == 0o700,
        "private production job required",
    )?;
    let result = operate(&work, operator, mode);
    if let Err(error) = &result {
        // The refusal matters more than a failure to record it.
        if let Err(e) = common::fmt(format_args!("{error}")).and_then(|text| refused(&work, &text))
        {
            eprintln!("td-mta-deploy: the refusal was not recorded: {e}");
        }
    }
    result
}
/// The launcher reads a job's end from its receipt. A job refused before
/// it reserved one records why beside where the receipt would be.
fn refused(work: &Path, error: &str) -> R<()> {
    if child(work, "receipt.json")?.try_exists()? {
        return Ok(());
    }
    let mut record = td_json::Json::Obj(Vec::new());
    common::set(
        &mut record,
        "status",
        td_json::Json::Str(common::owned("refused")?),
    )?;
    common::set(
        &mut record,
        "error",
        td_json::Json::Str(common::owned(error)?),
    )?;
    common::durable(&child(work, REFUSAL)?, &record)
}
/// The record of a job refused before it reserved its receipt.
const REFUSAL: &str = "refusal.json";
/// Another job holds the host; the launcher reports it as busy.
pub(crate) const LOCKED: &str = "another production operation owns the activation lock";
fn operate(work: &Path, operator: &Path, mode: &str) -> R<()> {
    let job = load_job(work, 0)?;
    check_mode(&job, mode)?;
    let lock = OpenOptions::new()
        .write(true)
        .truncate(false)
        .create(true)
        .mode(0o600)
        .open(child(operator, "activation.lock")?)?;
    File::try_lock(&lock).map_err(|_| LOCKED)?;
    common::block_signals()?;
    let result = activate::run(job, mode);
    if mode == "postgres-binaries" {
        // Once this job holds the lock its uploaded NARs go whatever the
        // outcome; a store path keeps none.
        result.and(activate::discard_nars(work))
    } else {
        result
    }
}
fn run() -> R<()> {
    let mut args = Vec::new();
    for a in std::env::args().skip(1) {
        td_alloc::push(&mut args, a)?;
    }
    match args.as_slice() {
        [s, rest @ ..] if s == "__isolation" => isolation::helper(rest),
        [s] if s == "--activate" => supervise("activate"),
        [s] if s == "--postgres-binaries" => supervise("postgres-binaries"),
        [s] if s == "--postgres-to-debian" => supervise("postgres-to-debian"),
        [s] if s == "--retire-rollback" => supervise("retire-rollback"),
        [s] if s == "--recover" => supervise("recover"),
        [s] if s == "--rollback" => supervise("rollback"),
        [s] if s == "--converge-config" => supervise("converge-config"),
        [s] if s == "--restore-shared-services" => supervise("restore-shared-services"),
        [s] if s == "--refresh-notifications" => supervise("refresh-notifications"),
        [s, rest @ ..]
            if Path::new(s)
                .file_name()
                .is_some_and(|name| name == "deploy") =>
        {
            launcher::run(rest)
        }
        _ => launcher::run(&args),
    }
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("td-mta-deploy: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_before_the_receipt_is_recorded_beside_it() -> R<()> {
        let dir = td_fs::TempDir::new("deploy-refusal")?;
        refused(dir.path(), LOCKED)?;
        let record = td_json::parse(&fs::read_to_string(dir.path().join(REFUSAL))?)?;
        assert_eq!(
            record.get("error").and_then(td_json::Json::as_str),
            Some(LOCKED)
        );
        assert_eq!(
            record.get("status").and_then(td_json::Json::as_str),
            Some("refused")
        );
        // Once the job has its receipt, the receipt says how it ended.
        fs::remove_file(dir.path().join(REFUSAL))?;
        fs::write(dir.path().join("receipt.json"), "{}")?;
        refused(dir.path(), "later")?;
        assert!(!dir.path().join(REFUSAL).exists());
        Ok(())
    }

    #[test]
    fn only_a_job_naming_a_postgres_operation_runs_it() -> R<()> {
        let plain = td_json::parse(r#"{"revision":"r"}"#)?;
        let other = td_json::parse(r#"{"revision":"r","mode":"activate"}"#)?;
        check_mode(&plain, "activate")?;
        assert!(check_mode(&other, "activate").is_err());
        for mode in ["restore-shared-services", "refresh-notifications"] {
            let named = td_json::parse(&format!(r#"{{"revision":"r","mode":"{mode}"}}"#))?;
            check_mode(&named, mode)?;
            assert!(check_mode(&plain, mode).is_err());
            assert!(check_mode(&named, "activate").is_err());
        }
        let binaries = td_json::parse(r#"{"revision":"r","mode":"postgres-binaries"}"#)?;
        check_mode(&binaries, "postgres-binaries")?;
        assert!(check_mode(&plain, "postgres-binaries").is_err());
        assert!(check_mode(&binaries, "activate").is_err());
        assert!(check_mode(&td_json::parse(r#"{"mode":1}"#)?, "postgres-binaries").is_err());
        let retire = td_json::parse(r#"{"revision":"r","mode":"retire-rollback"}"#)?;
        check_mode(&retire, "retire-rollback")?;
        assert!(check_mode(&plain, "retire-rollback").is_err());
        assert!(check_mode(&retire, "activate").is_err());
        assert!(check_mode(&retire, "postgres-binaries").is_err());
        let back = td_json::parse(r#"{"revision":"r","mode":"postgres-to-debian"}"#)?;
        check_mode(&back, "postgres-to-debian")?;
        assert!(check_mode(&back, "postgres-binaries").is_err());
        assert!(check_mode(&binaries, "postgres-to-debian").is_err());
        assert!(check_mode(&plain, "postgres-to-debian").is_err());
        let converge = td_json::parse(r#"{"revision":"r","mode":"converge-config"}"#)?;
        check_mode(&converge, "converge-config")?;
        assert!(check_mode(&converge, "activate").is_err());
        assert!(check_mode(&plain, "converge-config").is_err());
        assert!(check_mode(&binaries, "converge-config").is_err());
        for other in ["recover", "rollback"] {
            let job = td_json::parse(&format!(r#"{{"revision":"r","mode":"{other}"}}"#))?;
            assert!(check_mode(&job, "converge-config").is_err());
            assert!(check_mode(&converge, other).is_err());
        }
        Ok(())
    }
}
