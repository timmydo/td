use super::*;

impl Deployment {
    fn in_namespace(&self, ns: &[String], program: impl AsRef<std::ffi::OsStr>) -> Command {
        if let Some((head, rest)) = ns.split_first() {
            let mut cmd = self.host.cmd(head);
            cmd.args(rest).arg(program);
            cmd
        } else {
            self.host.cmd(program)
        }
    }

    pub(crate) fn sql(&self, role: &str, query: &str, ns: &[String], script: bool) -> R<String> {
        let service = match role {
            "owner" => "service=td-mta-backup",
            "marker" => "service=cutover_marker",
            "replication" => "service=cutover_repl dbname=replication replication=true",
            _ => return Err("unknown SQL role".into()),
        };
        let mut cmd = self.in_namespace(ns, child(Path::new(TOOLS), "psql")?);
        cmd.args(["-XwqAt", "-v", "ON_ERROR_STOP=1", service]);
        if role != "owner" {
            cmd.env("PGSERVICEFILE", child(Path::new(CONTROL), "service.conf")?);
        }
        if script {
            let result =
                self.host
                    .exec(&mut cmd, query.as_bytes(), Duration::from_secs(40), true)?;
            owned(std::str::from_utf8(&result.stdout)?.trim())
        } else {
            self.host.out(cmd.args(["-c", query]))
        }
    }

    pub(crate) fn ready_db(&self, ns: &[String]) -> R<bool> {
        let mut cmd = self.in_namespace(ns, child(Path::new(TOOLS), "psql")?);
        cmd.args(["-XwqAt", "service=td-mta-backup", "-c", "select 1"]);
        Ok(self
            .host
            .exec(&mut cmd, &[], Duration::from_secs(4), false)?
            .status
            .success())
    }

    fn native_command(
        &self,
        bin: &Path,
        op: &str,
        extra: &[&str],
        ns: &[String],
        login: Login<'_>,
    ) -> R<Command> {
        let mut cmd = self.in_namespace(ns, "runuser");
        let (uid, file) = match login {
            Login::Owner => (
                "td-mta",
                PathBuf::from("/var/lib/td-mta/mail/owner-connection"),
            ),
            Login::Runtime => (
                "td-mta",
                PathBuf::from("/var/lib/td-mta/mail/runtime-connection"),
            ),
            Login::Service(role) => (
                isolation::role_user(role)?,
                PathBuf::from(isolation::connection(role)?),
            ),
        };
        cmd.args(["-u", uid, "--"])
            .arg(bin)
            .args(["postgres", op, "--connection-file"])
            .arg(file)
            .args(extra);
        Ok(cmd)
    }

    pub(crate) fn check_source(&self) -> R<()> {
        let pid = self.pid(PG)?;
        require(
            fs::canonicalize(fmt(format_args!("/proc/{pid}/cwd"))?)? == Path::new(PGDATA),
            "unexpected primary data directory",
        )?;
        let control = self.host.out(
            self.host
                .cmd(child(Path::new(TOOLS), "pg_controldata")?)
                .arg(PGDATA),
        )?;
        require(
            control
                .lines()
                .filter_map(|line| line.split_once(':'))
                .any(|(key, value)| key == "Database system identifier" && value.trim() == SYSTEM)
                && self.sql("owner", "SELECT pg_is_in_recovery()", &[], false)? == "f",
            "production primary",
        )
    }

    pub(super) fn native(
        &self,
        bin: &Path,
        op: &str,
        extra: &[&str],
        ns: &[String],
        login: Login<'_>,
    ) -> R<Json> {
        let json = td_json::parse(
            &self
                .host
                .out(&mut self.native_command(bin, op, extra, ns, login)?)?,
        )?;
        require(
            field(&json, "status")? == "ok",
            &fmt(format_args!("candidate {op} qualification"))?,
        )?;
        Ok(json)
    }

    pub(super) fn config(&self, bin: &Path, ns: &[String], scoped: bool) -> R<()> {
        for (op, login) in [
            ("schema-inspect", Login::Owner),
            ("runtime-check", Login::Runtime),
        ] {
            let check = self.native(bin, op, &[], ns, login)?;
            require(
                field(&check, "store_epoch")? == self.epoch,
                "store epoch changed",
            )?;
        }
        if scoped {
            for &role in ROLES {
                let op = if role == "acme" {
                    "runtime-check"
                } else {
                    "service-profile-check"
                };
                let result = self.native(bin, op, &[], ns, Login::Service(role))?;
                require(
                    field(&result, "store_epoch")? == self.epoch,
                    "service epoch changed",
                )?;
            }
        }
        require(
            self.sql("owner", SELECTED, ns, false)? == self.selected,
            "selected configuration changed",
        )?;
        self.native(
            bin,
            "config-check",
            &["--epoch", &self.epoch, "--version", &self.selected],
            ns,
            Login::Owner,
        )?;
        Ok(())
    }

    pub(super) fn pid(&self, unit: &str) -> R<u32> {
        let pid = self
            .host
            .out(
                self.host
                    .cmd("systemctl")
                    .args(["show", unit, "-p", "MainPID", "--value"]),
            )?
            .parse::<u32>()?;
        require(pid > 0, "service has no MainPID")?;
        Ok(pid)
    }

    pub(super) fn acme_online(&self) -> R<bool> {
        let pid = self
            .host
            .out(
                self.host
                    .cmd("systemctl")
                    .args(["show", ACME, "-p", "MainPID", "--value"]),
            )?
            .parse::<u32>()?;
        Ok(pid != 0
            && self
                .host
                .out(self.host.cmd("ss").args(["-H", "-ltnp", "sport = :80"]))?
                .contains(&fmt(format_args!("pid={pid},"))?))
    }

    pub(super) fn healthy(&self, bin: &Path) -> R<()> {
        for unit in std::iter::once(PG).chain(UNITS.iter().copied()) {
            self.host.run(
                self.host
                    .cmd("systemctl")
                    .args(["is-active", "--quiet", unit]),
            )?;
            if unit != PG {
                let pid = self.pid(unit)?;
                require(
                    fs::canonicalize(fmt(format_args!("/proc/{pid}/exe"))?)? == bin,
                    "wrong running release",
                )?;
            }
        }
        if self.isolated {
            isolation::verify(&self.host, true)?;
        }
        for &role in PROFILES {
            let (uid, runtime) = if self.isolated {
                (
                    isolation::role_user(role)?,
                    owned(isolation::runtime(role)?)?,
                )
            } else {
                ("td-mta", fmt(format_args!("{MAIL}/{role}/runtime"))?)
            };
            self.wait(120, "profile readiness", || {
                Ok(self
                    .host
                    .exec(
                        self.host
                            .cmd("runuser")
                            .args(["-u", uid, "--"])
                            .arg(bin)
                            .args(["status", "--json", "--require-ready", "--runtime", &runtime]),
                        &[],
                        Duration::from_secs(40),
                        false,
                    )?
                    .status
                    .success())
            })?;
        }
        let pid = self.pid(ACME)?;
        require(self.acme_online()?, "ACME listener")?;
        let (user, connection) = if self.isolated {
            (
                isolation::role_user("acme")?,
                isolation::connection("acme")?,
            )
        } else {
            ("td-mta", "/var/lib/td-mta/mail/runtime-connection")
        };
        self.host.run(
            self.host
                .cmd("runuser")
                .args(["-u", user, "--"])
                .arg(bin)
                .args([
                    "acme-status",
                    "--connection-file",
                    connection,
                    "--certificate-profile",
                    "production",
                    "--require-ready",
                    "--minimum-validity-seconds",
                    "2592000",
                ]),
        )?;
        require(self.pid(ACME)? == pid, "ACME changed")?;
        self.host.run(self.host.cmd("systemctl").args([
            "start",
            "td-mta-maint-backup-status.service",
            "td-mta-health.service",
        ]))?;
        require(
            self.host
                .out(self.host.cmd("systemctl").args([
                    "list-units",
                    "--state=failed",
                    "--no-legend",
                    "td-mta-*",
                ]))?
                .is_empty(),
            "failed td-mta units",
        )
    }

    pub(super) fn has_service_grants(&self) -> bool {
        self.plan
            .as_ref()
            .and_then(|p| p.get("service_grants"))
            .is_some()
    }
    pub(super) fn service_grants(&self) -> R<&Json> {
        self.plan
            .as_ref()
            .and_then(|p| p.get("service_grants"))
            .ok_or_else(|| "missing service grants".into())
    }

    pub(super) fn classify(&self) -> R<Option<Json>> {
        let out = self.host.exec(
            &mut self.native_command(&self.candidate, "schema-plan", &[], &[], Login::Owner)?,
            &[],
            Duration::from_secs(40),
            false,
        )?;
        let plan = parse_output(&out)?;
        if out.status.code() == Some(2)
            && plan.get("error").and_then(Json::as_str) == Some("invalid-arguments")
        {
            self.config(&self.candidate, &[], self.isolated)?;
            return Ok(None);
        }
        let target = uint(&plan, "target")?;
        let pending = plan
            .get("pending")
            .and_then(Json::as_arr)
            .ok_or("invalid migration plan")?;
        require(
            out.status.success()
                && field(&plan, "status")? == "ok"
                && field(&plan, "store_epoch")? == self.epoch
                && uint(&plan, "current")? == self.old_version
                && target.checked_sub(self.old_version) == Some(u64::try_from(pending.len())?)
                && pending.iter().enumerate().all(|(i, version)| {
                    self.old_version
                        .checked_add(i as u64)
                        .and_then(|v| v.checked_add(1))
                        == version.as_u64()
                }),
            "unsupported ledger",
        )?;
        Ok(Some(plan))
    }

    pub(super) fn needs_migration(&self) -> R<bool> {
        let Some(ref plan) = self.plan else {
            return Ok(false);
        };
        let pending = plan
            .get("pending")
            .and_then(Json::as_arr)
            .ok_or("missing migration list")?;
        if !pending.is_empty() || self.isolation_change {
            return Ok(true);
        }
        let check = |login| -> R<bool> {
            let out = self.host.exec(
                &mut self.native_command(&self.candidate, "runtime-check", &[], &[], login)?,
                &[],
                Duration::from_secs(40),
                false,
            )?;
            if out.status.success() {
                return Ok(false);
            }
            require(
                field(&parse_output(&out)?, "error")? == "incorrect-data-privileges",
                "runtime candidate preflight",
            )?;
            Ok(true)
        };
        if check(Login::Runtime)? {
            return Ok(true);
        }
        if self.has_service_grants() {
            for &role in ROLES {
                if check(Login::Service(role))? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    pub(crate) fn upgrade(&self, ns: &[String], recovery: &Recovery) -> R<()> {
        let plan = self.plan.as_ref().ok_or("missing migration plan")?;
        require(
            self.native(&self.candidate, "schema-plan", &[], ns, Login::Owner)? == *plan,
            "migration plan changed",
        )?;
        let receipt = self.native(&self.candidate, "schema-upgrade", &[], ns, Login::Owner)?;
        require(
            field(&receipt, "store_epoch")? == self.epoch
                && uint(&receipt, "schema_version")? == uint(plan, "target")?,
            "upgrade receipt",
        )?;
        self.sql("owner", &primary_grants(plan)?, ns, true)?;
        if self.has_service_grants() {
            require(!ns.is_empty(), "isolated database rehearsal required")?;
            if self.isolation_change {
                recovery.create_service_logins(
                    self,
                    self.passwords
                        .as_ref()
                        .ok_or("missing service credentials")?,
                )?;
            }
            self.native(
                &self.candidate,
                "service-material-bind",
                &["--epoch", &self.epoch],
                ns,
                Login::Owner,
            )?;
            self.sql(
                "owner",
                &service_database::grant_sql(self.service_grants()?)?,
                ns,
                true,
            )?;
            self.config(&self.candidate, ns, true)?;
        } else {
            self.config(&self.candidate, ns, self.isolated)?;
        }
        Ok(())
    }
}

fn primary_grants(plan: &Json) -> R<String> {
    require(
        field(plan, "schema_usage")? == "td_mta",
        "schema grant namespace",
    )?;
    let grants = plan
        .get("primary_grants")
        .and_then(Json::as_obj)
        .ok_or("missing primary grants")?;
    let mut sql = owned("BEGIN;\n")?;
    for (table, value) in grants {
        let grant = value.as_str().ok_or("invalid primary grant")?;
        require(
            table.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                && table
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
                && matches!(
                    grant,
                    "SELECT"
                        | "SELECT, INSERT, DELETE"
                        | "SELECT, INSERT, UPDATE, DELETE"
                        | "SELECT, UPDATE (singleton)"
                ),
            "unsupported grant",
        )?;
        td_alloc::push_str(
            &mut sql,
            &fmt(format_args!(
                "GRANT {grant} ON TABLE td_mta.{table} TO td_mta_runtime;\n"
            ))?,
        )?;
    }
    td_alloc::push_str(&mut sql, "COMMIT;\n")?;
    Ok(sql)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn schema_rehearsal_only_passes_supported_grants_to_owner() {
        let valid = td_json::parse(
            r#"{"schema_usage":"td_mta","primary_grants":{"account_2":"SELECT, INSERT, DELETE"}}"#,
        )
        .unwrap();
        assert_eq!(primary_grants(&valid).unwrap(), "BEGIN;\nGRANT SELECT, INSERT, DELETE ON TABLE td_mta.account_2 TO td_mta_runtime;\nCOMMIT;\n");
        for plan in [
            r#"{"schema_usage":"public","primary_grants":{}}"#,
            r#"{"schema_usage":"td_mta","primary_grants":{"a.b":"SELECT"}}"#,
            r#"{"schema_usage":"td_mta","primary_grants":{"account":"ALL PRIVILEGES"}}"#,
            r#"{"schema_usage":"td_mta","primary_grants":{"1table":"SELECT"}}"#,
        ] {
            assert!(primary_grants(&td_json::parse(plan).unwrap()).is_err());
        }
    }
}
