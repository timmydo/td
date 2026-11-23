use super::*;
use std::os::unix::fs::FileTypeExt;

fn unit_names(list: &str, suffix: &str) -> R<Vec<String>> {
    let mut names = Vec::new();
    for line in list.lines() {
        let unit = line
            .split_ascii_whitespace()
            .next()
            .ok_or("invalid systemd output")?;
        require(
            unit.starts_with("td-mta-")
                && unit.ends_with(suffix)
                && unit
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'@')),
            "unexpected maintenance unit",
        )?;
        td_alloc::push(&mut names, owned(unit)?)?;
    }
    names.sort_unstable();
    names.dedup();
    Ok(names)
}

fn timer_service(timer: &str) -> R<String> {
    fmt(format_args!(
        "{}.service",
        timer.strip_suffix(".timer").ok_or("invalid timer unit")?
    ))
}

fn install_condition(directory: &Path, filename: &str, content: &str) -> R<PathBuf> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(directory)?;
    let path = child(directory, filename)?;
    match create_file(&path, 0o644) {
        Ok(mut file) => {
            file.write_all(content.as_bytes())?;
            file.set_permissions(fs::Permissions::from_mode(0o644))?;
            file.sync_all()?;
        }
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::AlreadyExists) =>
        {
            require(
                td_fs::read_bounded(&path, 4096)? == content.as_bytes(),
                "conflicting service fence",
            )?;
            // The previous process may have died after writing, before fsync.
            File::open(&path)?.sync_all()?;
        }
        Err(error) => return Err(error),
    }
    sync_dir(directory)?;
    Ok(path)
}

pub(super) const PG_FENCE: &str = "/etc/td-mta/postgres-fence.conf";
pub(super) const FENCE_SETTING: &str = "unix_socket_permissions = 0\n";
pub(super) const SOCKET: &str = "/var/lib/td-mta-postgres/.s.PGSQL.35432";

// The database writer fence. The included file is its durable state: a
// PostgreSQL start while it exists creates the socket with mode 0000, which
// only root (CAP_DAC_OVERRIDE) connects through. The live socket's mode is
// the running server's half of the same fence.
pub(super) trait SocketFence {
    fn write_file(&mut self) -> R<()>;
    fn remove_file(&mut self) -> R<()>;
    fn set_mode(&mut self, mode: u32) -> R<()>;
}

// Persist the file before closing the live socket, so a restart between the
// two steps still starts fenced.
pub(super) fn close_socket(fence: &mut impl SocketFence) -> R<()> {
    fence.write_file()?;
    fence.set_mode(0)
}

// Remove the file before opening the live socket, so a restart between the
// two steps starts open instead of closing it again.
pub(super) fn open_socket(fence: &mut impl SocketFence) -> R<()> {
    fence.remove_file()?;
    fence.set_mode(0o660)
}

/// The root-owned 0755 directory of the fence file, created if `create`.
/// PostgreSQL's include must reach into it, and nobody else may write it.
pub(super) fn fence_directory(create: bool) -> R<&'static Path> {
    let directory = parent(Path::new(PG_FENCE))?;
    if create {
        match fs::DirBuilder::new().mode(0o755).create(directory) {
            Ok(()) => {
                // The supervisor's umask would leave PostgreSQL unable to stat it.
                fs::set_permissions(directory, fs::Permissions::from_mode(0o755))?;
                sync_dir(parent(directory)?)?;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
    }
    let meta = fs::symlink_metadata(directory)?;
    require(
        meta.is_dir() && meta.uid() == 0 && meta.gid() == 0 && meta.mode() & 0o7777 == 0o755,
        "the PostgreSQL fence directory must be root-owned 0755",
    )?;
    Ok(directory)
}

pub(super) struct LiveSocket<'a>(pub(super) &'a Host);

impl SocketFence for LiveSocket<'_> {
    fn write_file(&mut self) -> R<()> {
        let path = Path::new(PG_FENCE);
        let directory = fence_directory(true)?;
        match create_file(path, 0o644) {
            Ok(mut file) => {
                file.set_permissions(fs::Permissions::from_mode(0o644))?;
                file.write_all(FENCE_SETTING.as_bytes())?;
                file.sync_all()?;
            }
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::AlreadyExists) =>
            {
                let (file, meta) = td_fs::open_real_file(path, "PostgreSQL fence")?;
                require(
                    meta.uid() == 0 && meta.mode() & 0o7777 == 0o644,
                    "conflicting PostgreSQL fence",
                )?;
                require(
                    td_fs::read_bounded(path, 4096)? == FENCE_SETTING.as_bytes(),
                    "conflicting PostgreSQL fence",
                )?;
                // The previous process may have died after writing, before fsync.
                file.sync_all()?;
            }
            Err(error) => return Err(error),
        }
        sync_dir(directory)
    }

    fn remove_file(&mut self) -> R<()> {
        let path = Path::new(PG_FENCE);
        match fs::remove_file(path) {
            Ok(()) => sync_dir(parent(path)?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    // PostgreSQL removes its socket when it stops; the next start applies the
    // file. The server account owns the socket, so the change runs with its
    // credentials and a replaced pathname reaches nothing root alone could.
    fn set_mode(&mut self, mode: u32) -> R<()> {
        match fs::symlink_metadata(SOCKET) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
            Ok(meta) => require(meta.file_type().is_socket(), "unexpected PostgreSQL socket")?,
        }
        let host = self.0;
        host.run(
            host.cmd("runuser")
                .args(["-u", "td-mta", "--", "chmod"])
                .arg(fmt(format_args!("{mode:04o}"))?)
                .args(["--", SOCKET]),
        )?;
        let meta = fs::symlink_metadata(SOCKET)?;
        require(
            meta.file_type().is_socket() && meta.mode() & 0o7777 == mode,
            "PostgreSQL socket mode",
        )
    }
}

// The kernel may already have acted when a command loses its reply. Authorize
// rewind only when both the database and service fences are still observable.
fn open_writer_fences(
    admitting: &mut bool,
    database: &mut bool,
    fence: &Path,
    open: impl FnOnce() -> R<()>,
    observe: impl FnOnce() -> R<bool>,
) -> R<()> {
    if *database {
        *admitting = true;
        if let Err(error) = open() {
            if fence.try_exists()? && observe()? {
                *admitting = false;
            }
            return Err(error);
        }
        *database = false;
    }
    let previously_exposed = *admitting;
    *admitting = true;
    if let Err(error) = remove_file(fence) {
        if !previously_exposed && fence.try_exists()? {
            *admitting = false;
        }
        return Err(error);
    }
    Ok(())
}

fn revert_partial_identity_install(
    started: &mut bool,
    isolated: &mut bool,
    restore_units: impl FnOnce() -> R<()>,
) -> R<()> {
    if *started {
        restore_units()?;
        *started = false;
        *isolated = false;
    }
    Ok(())
}

fn start_services_and_timers(
    host: &Host,
    timers: &[String],
    mut start: impl FnMut(&mut Command) -> R<()>,
) -> R<()> {
    start(host.cmd("systemctl").arg("start").args(UNITS))?;
    start(
        host.cmd("systemctl")
            .args(["start", "td-mta-maint-backup.timer"]),
    )?;
    start(host.cmd("systemctl").arg("start").args(timers))
}

impl Deployment {
    pub(super) fn active_timers(&self) -> R<Vec<String>> {
        let units = self.host.out(self.host.cmd("systemctl").args([
            "list-units",
            "--type=timer",
            "--state=active",
            "--no-legend",
            "td-mta-*",
        ]))?;
        unit_names(&units, ".timer")
    }
    pub(super) fn prepare_fence(&mut self, require_ready: bool, recovery: bool) -> R<()> {
        if recovery {
            self.socket_preflight()?;
        }
        self.timers = self.active_timers()?;
        require(
            ["td-mta-maint-backup.timer", "td-mta-health.timer"]
                .iter()
                .all(|required| self.timers.iter().any(|u| u == required)),
            "timer preflight",
        )?;
        let mut recorded = Vec::new();
        for timer in &self.timers {
            td_alloc::push(&mut recorded, Json::Str(owned(timer)?))?;
        }
        set(&mut self.record, "timers", Json::Arr(recorded))?;
        let all = self.host.out(self.host.cmd("systemctl").args([
            "list-unit-files",
            "--type=timer",
            "--no-legend",
            "td-mta-*.timer",
        ]))?;
        let mut writers = Vec::new();
        for u in UNITS {
            td_alloc::push(&mut writers, owned(u)?)?;
        }
        for t in unit_names(&all, ".timer")? {
            td_alloc::push(&mut writers, timer_service(&t)?)?;
        }
        writers.sort_unstable();
        writers.dedup();
        for unit in writers {
            let dir = child(Path::new(UNIT_DIR), &fmt(format_args!("{unit}.d"))?)?;
            let content = fmt(format_args!("[Unit]\nConditionPathExists=!{FENCE}\n"))?;
            td_alloc::push(
                &mut self.drops,
                install_condition(&dir, "98-td-mta-deploy-fence.conf", &content)?,
            )?;
        }
        if recovery {
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .mode(0o600)
                .open(PERMIT)?
                .sync_all()?;
            let dir = child(Path::new(UNIT_DIR), &fmt(format_args!("{PG}.d"))?)?;
            let content = fmt(format_args!(
                "[Service]\nExecCondition=+/usr/bin/test ! -e {FENCE} -o -e {PERMIT}\n"
            ))?;
            td_alloc::push(
                &mut self.drops,
                install_condition(&dir, "98-td-mta-deploy-boot.conf", &content)?,
            )?;
        }
        // A new .service.d directory must survive a crash with its condition.
        // Persist its entry in systemd's unit directory before writing the fence.
        sync_dir(Path::new(UNIT_DIR))?;
        self.host
            .run(self.host.cmd("systemctl").arg("daemon-reload"))?;
        self.record_listeners = true;
        self.probes.start(require_ready)
    }

    pub(super) fn fence(&mut self, shared_rollback: bool, recovery: bool) -> R<()> {
        self.event("fencing")?;
        let mut fence = create_file(Path::new(FENCE), 0o600)?;
        self.fence_owned = true;
        writeln!(fence, "{}", self.work.display())?;
        fence.sync_all()?;
        sync_dir(Path::new(OP))?;
        self.host
            .run(self.host.cmd("systemctl").arg("stop").args(&self.timers))?;
        for timer in &self.timers {
            let service = timer_service(timer)?;
            self.wait(30, "maintenance sessions", || {
                let s = self.host.out(self.host.cmd("systemctl").args([
                    "show",
                    &service,
                    "-p",
                    "ActiveState",
                    "--value",
                ]))?;
                Ok(matches!(s.as_str(), "inactive" | "failed"))
            })?;
        }
        set(&mut self.record, "window_started_utc", now()?.into())?;
        let start = Instant::now();
        self.window = Some(start);
        self.host.deadline(Some(start + self.budget));
        self.host
            .run(self.host.cmd("systemctl").arg("stop").args(UNITS))?;
        for u in UNITS {
            require(
                self.host.out(self.host.cmd("systemctl").args([
                    "show",
                    u,
                    "-p",
                    "ActiveState",
                    "--value",
                ]))? == "inactive",
                "mail fence",
            )?;
        }
        self.config(&self.previous, &[], !shared_rollback && self.isolated)?;
        if recovery {
            self.socket_fence = true;
            close_socket(&mut LiveSocket(&self.host))?;
        }
        self.check_source()?;
        require(self.sql("owner","SELECT NOT EXISTS(SELECT FROM pg_stat_activity WHERE datname IS NOT NULL AND pid<>pg_backend_pid()),NOT EXISTS(SELECT FROM pg_prepared_xacts)",&[],false)?=="t|t","database writers")?;
        self.event("fenced")?;
        Ok(())
    }

    pub(super) fn switch(&self, bin: &Path) -> R<()> {
        let dir = parent(Path::new(ACTIVE))?;
        let link = child(dir, &fmt(format_args!(".{}", name(&self.work)?))?)?;
        std::os::unix::fs::symlink(bin, &link)?;
        fs::rename(link, ACTIVE)?;
        sync_dir(dir)
    }
    pub(super) fn admit(&mut self) -> R<()> {
        let operation = self.event("opening writers")?;
        self.host.remaining()?;
        let host = &self.host;
        open_writer_fences(
            &mut self.admitting,
            &mut self.socket_fence,
            Path::new(FENCE),
            || open_socket(&mut LiveSocket(host)),
            || {
                host.recover();
                Ok(Path::new(PG_FENCE).try_exists()?)
            },
        )?;
        sync_dir(Path::new(OP))?;
        start_services_and_timers(&self.host, &self.timers, |cmd| self.host.run(cmd))?;
        self.fence_owned = false;
        self.wait(20, "listener recovery", || {
            let samples = self.probes.samples()?;
            Ok(["smtp", "jmap"].iter().all(|name| {
                samples
                    .iter()
                    .find(|(key, _)| key == name)
                    .and_then(|(_, v)| v.last())
                    .is_some_and(|(t, ok)| *ok && *t >= operation)
            }))
        })?;
        set(&mut self.record, "listeners_up_utc", now()?.into())?;
        for (key, samples) in self.probes.samples()? {
            if let Some((first, last)) = failure_range(&samples) {
                if let (Some(before), Some(after)) = (
                    first.checked_sub(1).and_then(|i| samples.get(i)),
                    samples.get(last + 1),
                ) {
                    let mut bounds = Vec::new();
                    td_alloc::push(
                        &mut bounds,
                        Json::from(millis(
                            samples.get(last).ok_or("sample")?.0
                                - samples.get(first).ok_or("sample")?.0,
                        )),
                    )?;
                    td_alloc::push(&mut bounds, Json::from(millis(after.0 - before.0)))?;
                    set(
                        &mut self.record,
                        &fmt(format_args!("{key}_interruption_bounds_seconds"))?,
                        Json::Arr(bounds),
                    )?;
                }
            }
        }
        set(
            &mut self.record,
            "window_seconds",
            self.window
                .take()
                .map_or(Json::Null, |t| millis(t.elapsed().as_secs_f64()).into()),
        )?;
        self.host.deadline(None);
        Ok(())
    }

    /// A restart inside the fence must have created the socket closed.
    pub(super) fn fenced_start(&self) -> R<()> {
        if !self.socket_fence {
            return Ok(());
        }
        let meta = fs::symlink_metadata(SOCKET)?;
        require(
            meta.file_type().is_socket() && meta.mode() & 0o7777 == 0,
            "PostgreSQL restarted inside the fence with an open socket",
        )
    }

    pub(super) fn finish_admission(&mut self) -> R<()> {
        // The file is the database fence's state; reopening is idempotent.
        if self.socket_fence || Path::new(PG_FENCE).try_exists()? {
            open_socket(&mut LiveSocket(&self.host))?;
        }
        self.socket_fence = false;
        if Path::new(FENCE).try_exists()? {
            require(
                std::str::from_utf8(&td_fs::read_bounded(Path::new(FENCE), 4096)?)?.trim()
                    == self.work.to_str().ok_or("job path")?,
                "foreign deployment fence",
            )?;
            fs::remove_file(FENCE)?;
            sync_dir(Path::new(OP))?;
        }
        let start = |cmd: &mut Command| -> R<()> {
            self.host.exec(cmd, &[], Duration::from_secs(120), false)?;
            Ok(())
        };
        start_services_and_timers(&self.host, &self.timers, start)?;
        self.fence_owned = false;
        self.wait(240, "ACME lease after interrupted admission", || {
            self.acme_online()
        })?;
        self.healthy(&fs::canonicalize(ACTIVE)?)?;
        require(self.active_timers()? == self.timers, "timer recovery")?;
        set(
            &mut self.record,
            "recovery_status",
            "selected release healthy after interrupted admission".into(),
        )
    }

    pub(super) fn rollback(&mut self) -> R<()> {
        if !self.fence_owned || self.admitting {
            return Ok(());
        }
        self.event("restoring original primary before writer admission")?;
        if self.pg_stopped {
            self.host.exec(
                self.host.cmd("systemctl").args(["stop", PG]),
                &[],
                Duration::from_secs(120),
                true,
            )?;
            if self.original.try_exists()? {
                if Path::new(PGDATA).try_exists()? {
                    fs::rename(PGDATA, child(&self.work, "unreleased-pgdata")?)?;
                }
                fs::rename(&self.original, PGDATA)?;
                self.host.run(self.host.cmd("sync").args(["-f", PGDATA]))?;
            }
            self.host
                .run(self.host.cmd("systemctl").args(["start", PG]))?;
            self.wait(25, "original primary restart", || self.ready_db(&[]))?;
            // The next step opens the socket anyway; keep rolling back.
            if self.fenced_start().is_err() {
                set(&mut self.record, "rollback_socket_open", true.into())?;
            }
        }
        if self.socket_fence {
            open_socket(&mut LiveSocket(&self.host))?;
            self.socket_fence = false;
        }
        revert_partial_identity_install(&mut self.dropins_started, &mut self.isolated, || {
            isolation::rollback(&self.host)
        })?;
        self.config(&self.previous, &[], self.isolated)?;
        self.switch(&self.previous)?;
        self.admit()?;
        self.wait(240, "restored ACME lease", || self.acme_online())?;
        self.healthy(&self.previous)?;
        set(
            &mut self.record,
            "rollback",
            "original release and database verified".into(),
        )
    }

    pub(super) fn finish_probes(&mut self, filename: &str) -> R<()> {
        if self.record_listeners {
            self.probes.finish(&child(&self.work, filename)?)?;
            self.record_listeners = false;
        }
        if self.record.get("window_started_utc").is_some() {
            set(&mut self.record, "recovery_finished_utc", now()?.into())?;
            for (key, samples) in self.probes.samples()? {
                if let Some((first, last)) = failure_range(&samples) {
                    let mut detail = Json::Obj(Vec::new());
                    for (label, i) in [
                        ("first_failure", Some(first)),
                        ("last_failure", Some(last)),
                        ("last_success_before", first.checked_sub(1)),
                        ("first_success_after", last.checked_add(1)),
                    ] {
                        let val = i
                            .and_then(|idx| samples.get(idx))
                            .map_or(Json::Null, |row| row.0.into());
                        set(&mut detail, label, val)?;
                    }
                    set(
                        &mut self.record,
                        &fmt(format_args!("{key}_outage"))?,
                        detail,
                    )?;
                }
            }
            durable(&self.receipt, &self.record)?;
        }
        Ok(())
    }
}

fn millis(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}
fn failure_range(samples: &[(f64, bool)]) -> Option<(usize, usize)> {
    Some((
        samples.iter().position(|v| !v.1)?,
        samples.iter().rposition(|v| !v.1)?,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;

    #[derive(Default)]
    struct Recorded {
        steps: Vec<String>,
        fail: Option<&'static str>,
    }
    impl Recorded {
        fn step(&mut self, step: String) -> R<()> {
            let failed = self.fail.is_some_and(|f| f == step);
            self.steps.push(step);
            if failed {
                return Err("injected fault".into());
            }
            Ok(())
        }
    }
    impl SocketFence for Recorded {
        fn write_file(&mut self) -> R<()> {
            self.step("write".into())
        }
        fn remove_file(&mut self) -> R<()> {
            self.step("remove".into())
        }
        fn set_mode(&mut self, mode: u32) -> R<()> {
            self.step(format!("chmod {mode:04o}"))
        }
    }

    #[test]
    fn socket_fence_persists_before_closing_and_unlinks_before_opening() {
        let mut fence = Recorded::default();
        close_socket(&mut fence).unwrap();
        open_socket(&mut fence).unwrap();
        assert_eq!(fence.steps, ["write", "chmod 0000", "remove", "chmod 0660"]);
        // A failed durable step never changes the live socket.
        for (fail, close) in [("write", true), ("remove", false)] {
            let mut fence = Recorded {
                fail: Some(fail),
                ..Recorded::default()
            };
            let result = if close {
                close_socket(&mut fence)
            } else {
                open_socket(&mut fence)
            };
            assert!(result.is_err());
            assert_eq!(fence.steps, [fail]);
        }
    }

    #[test]
    fn missing_acknowledgement_requires_proof_of_both_writer_fences() {
        let scratch = td_fs::TempDir::new("mta-admission").unwrap();
        for (fenced, visible) in [
            (true, Some(true)),
            (true, Some(false)),
            (true, None),
            (false, Some(true)),
        ] {
            let fence = scratch.path().join("fence");
            if fenced {
                fs::write(&fence, b"job").unwrap();
            }
            let mut opening = false;
            let mut database = true;
            let result = open_writer_fences(
                &mut opening,
                &mut database,
                &fence,
                || Err("lost acknowledgement".into()),
                || visible.ok_or_else(|| "observation failed".into()),
            );
            assert!(result.is_err());
            assert!(database);
            assert_eq!(opening, !(fenced && visible == Some(true)));
            assert_eq!(fence.exists(), fenced);
            if fenced {
                fs::remove_file(&fence).unwrap();
            }
        }
    }

    #[test]
    fn service_fence_error_after_database_exposure_must_recover_forward() {
        let scratch = td_fs::TempDir::new("mta-admission-unlink").unwrap();
        let fence = scratch.path().join("cannot-unlink");
        fs::create_dir(&fence).unwrap();
        for database_guard in [true, false] {
            let mut database = database_guard;
            let mut opening = false;
            assert!(open_writer_fences(
                &mut opening,
                &mut database,
                &fence,
                || Ok(()),
                || panic!("acknowledged socket reopening")
            )
            .is_err());
            assert_eq!(opening, database_guard);
            assert!(!database);
        }
    }

    #[test]
    fn partial_identity_install_requires_successful_unit_restore_before_admission() {
        let mut started = true;
        let mut isolated = true;
        assert!(
            revert_partial_identity_install(&mut started, &mut isolated, || Err(
                "interrupted service manager reload".into()
            ))
            .is_err()
        );
        assert!(started && isolated);
        revert_partial_identity_install(&mut started, &mut isolated, || Ok(())).unwrap();
        assert!(!started && !isolated);
        revert_partial_identity_install(&mut started, &mut isolated, || panic!("already restored"))
            .unwrap();
    }

    #[test]
    fn interrupted_service_start_retries_all_writers_and_maintenance_timers() {
        let host = Host::default();
        let timers = vec![
            String::from("td-mta-health.timer"),
            String::from("td-mta-maint-backup.timer"),
        ];
        let mut calls = Vec::new();
        let mut record = |cmd: &mut Command| {
            calls.push(
                cmd.get_args()
                    .map(|s| s.to_str().unwrap().to_owned())
                    .skip_while(|arg| arg != "start")
                    .skip(1)
                    .collect::<Vec<_>>(),
            );
        };
        let interrupted = start_services_and_timers(&host, &timers, |cmd| {
            record(cmd);
            Err("interrupted starting mail services".into())
        });
        assert!(interrupted.is_err());
        start_services_and_timers(&host, &timers, |cmd| {
            record(cmd);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls[0], UNITS);
        assert_eq!(calls[1], UNITS);
        assert_eq!(calls[2], ["td-mta-maint-backup.timer"]);
        assert_eq!(calls[3], timers);
    }
}
