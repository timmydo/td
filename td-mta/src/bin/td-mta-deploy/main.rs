//! Local launcher and independently retained production deployment supervisor.
#![deny(unsafe_code)]
mod activate;
mod common;
mod isolation;
mod launcher;
mod physical;
#[cfg(test)]
#[path = "tests/postgres_fixture.rs"]
mod postgres_fixture;
mod probes;
mod service_database;
#[path = "../../signal_sys.rs"]
mod signal_sys;
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
fn load_job(work: &Path, mode: &str, owner: u32) -> R<td_json::Json> {
    let path = child(work, "job.json")?;
    let (source, legacy) = match td_fs::open_real_file(&path, "job") {
        Ok(source) => (source, false),
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && matches!(mode, "restore-shared-services" | "refresh-notifications") =>
        {
            // The original controller retained these job fields in its private
            // receipt. Recovery reads the receipt and never executes the archive.
            let archive = fs::symlink_metadata(child(work, "deploy.pyz")?)?;
            require(
                archive.is_file() && archive.uid() == owner && archive.mode() & 0o7777 == 0o600,
                "invalid legacy deployment archive",
            )?;
            (
                td_fs::open_real_file(&child(work, "receipt.json")?, "legacy receipt")?,
                true,
            )
        }
        Err(error) => return Err(error.into()),
    };
    let (mut file, meta) = source;
    let limit = if legacy { 1024 * 1024 } else { 4096 };
    require(
        meta.uid() == owner && meta.mode() & 0o7777 == 0o600 && meta.len() <= limit,
        "invalid production job",
    )?;
    let mut bytes = td_alloc::filled(usize::try_from(meta.len())?, 0)?;
    file.read_exact(&mut bytes)?;
    require(file.read(&mut [0])? == 0, "job changed while reading")?;
    let mut record = td_json::parse_slice(&bytes)?;
    if legacy {
        let mut job = td_json::Json::Obj(Vec::new());
        for key in [
            "revision",
            "previous",
            "tree",
            "main",
            "controller",
            "budget",
            "verdict",
            "skip_tests",
        ] {
            let value = record
                .remove(key)
                .ok_or("incomplete legacy deployment receipt")?;
            common::set(&mut job, key, value)?;
        }
        return Ok(job);
    }
    Ok(record)
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
    let job = load_job(&work, mode, 0)?;
    let lock = OpenOptions::new()
        .write(true)
        .truncate(false)
        .create(true)
        .mode(0o600)
        .open(child(operator, "activation.lock")?)?;
    File::try_lock(&lock).map_err(|_| "another production operation owns the activation lock")?;
    common::block_signals()?;
    activate::run(job, mode)
}
fn run() -> R<()> {
    let mut args = Vec::new();
    for a in std::env::args().skip(1) {
        td_alloc::push(&mut args, a)?;
    }
    match args.as_slice() {
        [s, rest @ ..] if s == "__isolation" => isolation::helper(rest),
        [s] if s == "--activate" => supervise("activate"),
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
    fn archived_jobs_only_supply_recovery_fields() -> R<()> {
        let scratch = td_fs::TempDir::new("deploy-job-record")?;
        let work = scratch.path();
        let owner = fs::metadata(work)?.uid();
        let archive = work.join("deploy.pyz");
        let _file = File::create_new(&archive)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o600))?;
        let receipt = work.join("receipt.json");
        let legacy = r#"{"controller":"c","revision":"r","previous":"p","tree":"t","main":"m","budget":60,"verdict":null,"skip_tests":false,"status":"verified deployment","events":[]}"#;
        fs::write(&receipt, legacy)?;
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600))?;
        let job = load_job(work, "restore-shared-services", owner)?;
        assert!(job.get("events").is_none());
        assert_eq!(
            job.get("revision").and_then(td_json::Json::as_str),
            Some("r")
        );
        assert!(load_job(work, "activate", owner).is_err());
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o644))?;
        assert!(load_job(work, "refresh-notifications", owner).is_err());
        Ok(())
    }
}
