//! Process ownership and tool selection for disposable PostgreSQL 17 tests.
use crate::R;
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(crate) struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = Command::new("kill")
                .args(["-INT", &self.0.id().to_string()])
                .status();
            let end = Instant::now() + Duration::from_secs(10);
            while Instant::now() < end {
                if self.0.try_wait().ok().flatten().is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub(crate) fn checked(command: &mut Command) -> R<String> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "{:?}: {}",
            command.get_program(),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub(crate) fn run(command: &mut Command, log: &Path) -> R<Process> {
    let log = File::create(log)?;
    Ok(Process(
        command
            .stdin(Stdio::null())
            .stderr(log.try_clone()?)
            .stdout(log)
            .spawn()?,
    ))
}

pub(crate) fn wait(what: &str, mut predicate: impl FnMut() -> bool) -> R<()> {
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        if predicate() {
            return Ok(());
        }
        if Instant::now() >= end {
            return Err(format!("timed out waiting for {what}").into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub(crate) struct PgTools {
    pub(crate) bin: PathBuf,
}

impl PgTools {
    pub(crate) fn new() -> R<Self> {
        let initdb = td_fs::executable_on_path("initdb").ok_or("missing PostgreSQL 17 initdb")?;
        let bin = initdb
            .parent()
            .filter(|dir| dir.is_absolute())
            .ok_or("invalid PostgreSQL tools directory")?
            .to_path_buf();
        let tools = Self { bin };
        for name in [
            "postgres",
            "psql",
            "pg_receivewal",
            "pg_basebackup",
            "pg_verifybackup",
        ] {
            if td_fs::executable_in(Some(tools.bin.as_os_str()), name).is_none() {
                return Err(format!("missing {name} beside PostgreSQL initdb").into());
            }
        }
        if !checked(tools.cmd("postgres").arg("--version"))?
            .starts_with("postgres (PostgreSQL) 17.")
        {
            return Err("requires PostgreSQL 17".into());
        }
        Ok(tools)
    }

    pub(crate) fn cmd(&self, name: &str) -> Command {
        let mut command = Command::new(self.bin.join(name));
        for (key, _) in std::env::vars_os() {
            if key.to_str().is_some_and(|s| s.starts_with("PG")) {
                command.env_remove(key);
            }
        }
        command.env("PGCONNECT_TIMEOUT", "2");
        command
    }
}
