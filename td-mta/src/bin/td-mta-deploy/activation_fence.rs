use super::*;

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

fn has_firewall(host: &Host, table: &str) -> R<bool> {
    let tables = host.out(host.cmd("nft").args(["list", "tables"]))?;
    Ok(tables
        .lines()
        .any(|line| line.split_ascii_whitespace().eq(["table", "inet", table])))
}

fn firewall_command(host: &Host, table: &str, check: bool) -> R<Command> {
    let rules = fmt(format_args!(
        "create table inet {table}\n\
         add chain inet {table} output {{ type filter hook output priority -10; policy accept; }}\n\
         add rule inet {table} output ip daddr 127.0.0.1 tcp dport 35432 meta skuid != 0 reject with tcp reset\n"
    ))?;
    let mut cmd = host.cmd("nft");
    if check {
        cmd.arg("--check");
    }
    // nft reopens file input; the supervisor gives children a socket for stdin.
    cmd.arg(rules);
    Ok(cmd)
}

fn drop_firewall(host: &Host, table: &str) -> R<()> {
    host.run(host.cmd("nft").args(["delete", "table", "inet", table]))
}

// The kernel may already have acted when a command loses its reply. Authorize
// rewind only when both the database and service fences are still observable.
fn open_writer_fences(
    admitting: &mut bool,
    firewall: &mut bool,
    fence: &Path,
    delete: impl FnOnce() -> R<()>,
    observe: impl FnOnce() -> R<bool>,
) -> R<()> {
    if *firewall {
        *admitting = true;
        if let Err(error) = delete() {
            if fence.try_exists()? && observe()? {
                *admitting = false;
            }
            return Err(error);
        }
        *firewall = false;
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

// If installation or deletion was interrupted, act on the rule still in the
// kernel. A positive observation after a failed deletion keeps services shut.
fn reconcile_firewall(observe: impl Fn() -> R<bool>, delete: impl FnOnce() -> R<()>) -> R<()> {
    if observe()? {
        if let Err(error) = delete() {
            if observe()? {
                return Err(error);
            }
        }
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
            self.host
                .run(&mut firewall_command(&self.host, &self.table, true)?)?;
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
            let mut cmd = firewall_command(&self.host, &self.table, false)?;
            self.firewall = true;
            self.host.run(&mut cmd)?;
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
        let table = &self.table;
        open_writer_fences(
            &mut self.admitting,
            &mut self.firewall,
            Path::new(FENCE),
            || drop_firewall(host, table),
            || {
                host.recover();
                has_firewall(host, table)
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

    pub(super) fn finish_admission(&mut self) -> R<()> {
        if self.firewall {
            reconcile_firewall(
                || has_firewall(&self.host, &self.table),
                || drop_firewall(&self.host, &self.table),
            )?;
        }
        self.firewall = false;
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
        }
        if self.firewall {
            reconcile_firewall(
                || has_firewall(&self.host, &self.table),
                || drop_firewall(&self.host, &self.table),
            )?;
            self.firewall = false;
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
    use std::cell::Cell;

    #[test]
    fn nft_checks_and_applies_the_same_batch_as_one_argument() {
        let host = Host::default();
        let batch = "create table inet td_mta_deploy_Ab12\n\
                     add chain inet td_mta_deploy_Ab12 output { type filter hook output priority -10; policy accept; }\n\
                     add rule inet td_mta_deploy_Ab12 output ip daddr 127.0.0.1 tcp dport 35432 meta skuid != 0 reject with tcp reset\n";
        for check in [true, false] {
            let cmd = firewall_command(&host, "td_mta_deploy_Ab12", check).unwrap();
            let mut args = cmd.get_args().skip_while(|arg| *arg != "nft");
            assert_eq!(args.next(), Some("nft".as_ref()));
            if check {
                assert_eq!(args.next(), Some("--check".as_ref()));
            }
            assert_eq!(args.next(), Some(batch.as_ref()));
            assert_eq!(args.next(), None);
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
            let mut firewall = true;
            let result = open_writer_fences(
                &mut opening,
                &mut firewall,
                &fence,
                || Err("lost acknowledgement".into()),
                || visible.ok_or_else(|| "observation failed".into()),
            );
            assert!(result.is_err());
            assert!(firewall);
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
            let mut firewall = database_guard;
            let mut opening = false;
            assert!(open_writer_fences(
                &mut opening,
                &mut firewall,
                &fence,
                || Ok(()),
                || panic!("acknowledged rule deletion")
            )
            .is_err());
            assert_eq!(opening, database_guard);
            assert!(!firewall);
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
    fn reconcile_ambiguous_firewall_install_and_delete_from_actual_rules() {
        for (before, after) in [(false, false), (true, false), (true, true)] {
            let table = Cell::new(before);
            let tried = Cell::new(false);
            let result = reconcile_firewall(
                || Ok(table.get()),
                || {
                    tried.set(true);
                    table.set(after);
                    Err("lost acknowledgement".into())
                },
            );
            assert_eq!(result.is_err(), before && after);
            assert_eq!(tried.get(), before);
        }
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
