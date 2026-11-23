//! Private online base backup, named recovery point and migration rehearsal.
use crate::activate::Deployment;
use crate::common::{
    child, digest, durable, fmt, free_bytes, owned, private_dir, random_hex, require, set, Host,
    DEBIAN_POSTGRES,
};
use crate::{isolation, service_database, R};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use td_json::Json;

#[path = "physical_receiver.rs"]
mod receiver;
use receiver::Receiver;

pub(super) const MAIL: &str = "/var/lib/td-mta/mail";
pub(super) const PGDATA: &str = "/var/lib/td-mta/mail/postgres";
pub(super) const CONTROL: &str = "/var/lib/td-mta-operator/physical-backup";
pub(super) const SYSTEM: &str = "7698635942461075926";
pub(super) const SOCKET_DIR: &str = "/var/lib/td-mta-postgres";
pub(super) const PG_DROPIN: &str =
    "/etc/systemd/system/td-mta-postgres.service.d/90-td-mta-socket.conf";
const VOLUME: &str = "/data/td-mta-physical-recovery";
pub(super) const GIB: u64 = 1024 * 1024 * 1024;

const SETTINGS: &str = "SELECT current_setting('wal_level') IN ('replica','logical'),
    current_setting('full_page_writes'),current_setting('synchronous_standby_names')='',
    current_setting('max_wal_senders')::int-(SELECT count(*) FROM pg_stat_replication)>=3,
    NOT EXISTS (SELECT FROM pg_tablespace WHERE spcname NOT IN ('pg_default','pg_global')),
    NOT EXISTS (SELECT FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
                WHERE n.nspname='td_mta' AND c.relpersistence<>'p'),
    NOT EXISTS (SELECT FROM pg_database WHERE NOT datistemplate AND datname NOT IN ('td_mta','postgres'))";

pub(super) struct Recovery {
    pub(super) work: PathBuf,
    pub(super) runtime: PathBuf,
    point: String,
    receipt: Json,
    timeline: u32,
    start_lsn: u64,
    receiver: Option<Receiver>,
    unit: Option<String>,
    holder: Option<String>,
}

fn private_subdir(path: &Path) -> R<()> {
    fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}

fn remove_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

pub(super) fn check_no_links(root: &Path) -> R<()> {
    require(
        fs::symlink_metadata(root)?.is_dir(),
        "external PostgreSQL data directory",
    )?;
    for item in fs::read_dir(root)? {
        let item = item?;
        let kind = item.file_type()?;
        require(!kind.is_symlink(), "external PostgreSQL data pathname")?;
        if kind.is_dir() {
            check_no_links(&item.path())?;
        }
    }
    Ok(())
}

fn is_hex(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
}

fn parse_lsn(lsn: &str) -> R<u64> {
    let (hi, lo) = lsn.split_once('/').ok_or("invalid recovery LSN")?;
    require(
        hi.len() <= 8 && lo.len() <= 8 && is_hex(hi) && is_hex(lo),
        "invalid recovery LSN",
    )?;
    Ok((u64::from(u32::from_str_radix(hi, 16)?) << 32) | u64::from(u32::from_str_radix(lo, 16)?))
}

fn parse_segment(name: &str) -> R<(u32, u32, u32)> {
    require(name.len() == 24 && is_hex(name), "invalid WAL segment")?;
    let (a, tail) = name.split_at(8);
    let (b, c) = tail.split_at(8);
    Ok((
        u32::from_str_radix(a, 16)?,
        u32::from_str_radix(b, 16)?,
        u32::from_str_radix(c, 16)?,
    ))
}

fn check_continuity(wal: &Path, timeline: u32, start: u64, segment: &str, size: u64) -> R<()> {
    require(
        (1024 * 1024..=GIB).contains(&size) && size.is_power_of_two(),
        "invalid WAL segment size",
    )?;
    let (last_timeline, log, part) = parse_segment(segment)?;
    let per = (1u64 << 32) / size;
    require(
        last_timeline == timeline && u64::from(part) < per,
        "invalid WAL segment timeline or offset",
    )?;
    let last = u64::from(log) * per + u64::from(part);
    let first = start / size;
    require(
        last.checked_sub(first).is_some_and(|n| n <= GIB / size),
        "archive continuity range",
    )?;
    for i in first..=last {
        let name = fmt(format_args!("{timeline:08X}{:08X}{:08X}", i / per, i % per))?;
        let meta = fs::symlink_metadata(child(wal, &name)?)?;
        require(
            meta.is_file() && meta.len() == size,
            "incomplete WAL archive",
        )?;
    }
    Ok(())
}

fn read_manifest(path: &Path, source: &str, timeline: u32) -> R<(Json, u64)> {
    // pg_verifybackup checks file integrity. Independently pin the cluster and
    // timeline before accepting this manifest as the WAL continuity boundary.
    let mut manifest =
        td_json::parse(&fs::read_to_string(path)?).map_err(|_| "invalid backup manifest")?;
    require(
        manifest.get("System-Identifier").and_then(Json::as_u64) == Some(source.parse()?),
        "backup source identity",
    )?;
    let Some(Json::Arr(mut ranges)) = manifest.remove("WAL-Ranges") else {
        return Err("missing backup WAL range".into());
    };
    require(ranges.len() == 1, "backup WAL range count")?;
    let range = ranges.pop().ok_or("missing backup WAL range")?;
    require(
        range.get("Timeline").and_then(Json::as_u64) == Some(u64::from(timeline)),
        "backup WAL timeline",
    )?;
    let start = parse_lsn(
        range
            .get("Start-LSN")
            .and_then(Json::as_str)
            .ok_or("missing WAL range start")?,
    )?;
    Ok((range, start))
}

impl Recovery {
    pub(super) fn new(h: &Deployment) -> R<Self> {
        let volume = Path::new(VOLUME);
        let meta = fs::symlink_metadata(volume)?;
        require(
            meta.is_dir()
                && meta.uid() == 0
                && meta.dev() != fs::metadata(PGDATA)?.dev()
                && meta.mode() & 0o777 == 0o700,
            "private recovery mount",
        )?;
        let point = fmt(format_args!("td_cut_{}", random_hex(16)?))?;
        let mut receipt = Json::Obj(Vec::new());
        set(&mut receipt, "point", Json::Str(owned(&point)?))?;
        set(&mut receipt, "status", Json::from("preparing"))?;
        let work = private_dir(volume, "deploy-")?;
        let runtime = private_dir(Path::new("/run"), "td-mta-recovery-")?;
        let state = Self {
            work,
            runtime,
            point,
            receipt,
            timeline: 0,
            start_lsn: 0,
            receiver: None,
            unit: None,
            holder: None,
        };
        let initialize = || -> R<()> {
            fs::set_permissions(&state.runtime, fs::Permissions::from_mode(0o711))?;
            private_subdir(&child(&state.work, "wal")?)?;
            let archive = child(&state.work, "archive")?;
            private_subdir(&archive)?;
            h.host
                .run(h.host.cmd("chown").arg("root:td-mta").arg(&archive))?;
            fs::set_permissions(archive, fs::Permissions::from_mode(0o750))?;
            // Each trial serves the production socket form from its own directory.
            let socket = child(&state.runtime, "socket")?;
            private_subdir(&socket)?;
            h.host.run(
                h.host
                    .cmd("chown")
                    .arg(fmt(format_args!("td-mta:{}", isolation::DB_GROUP))?)
                    .arg(&socket),
            )?;
            fs::set_permissions(socket, fs::Permissions::from_mode(0o750))?;
            Ok(())
        };
        if let Err(error) = initialize() {
            let _ = fs::remove_dir_all(&state.runtime);
            return Err(error);
        }
        Ok(state)
    }

    fn source(h: &Deployment) -> R<u32> {
        let reply = h.sql("replication", "IDENTIFY_SYSTEM", &[], false)?;
        let mut fields = reply.split('|');
        require(fields.next() == Some(SYSTEM), "primary system identity")?;
        let timeline = fields.next().ok_or("missing timeline")?.parse::<u32>()?;
        require(
            timeline > 0 && fields.count() == 2,
            "primary replication identity",
        )?;
        Ok(timeline)
    }

    fn streaming(h: &Deployment) -> R<bool> {
        let pid = h.sql(
            "owner",
            "SELECT pid FROM pg_stat_replication WHERE application_name='td_cutover_wal'
             AND usename='td_mta_cutover_backup'",
            &[],
            false,
        )?;
        let Ok(pid) = pid.parse::<u32>() else {
            return Ok(false);
        };
        let path = fmt(format_args!("/proc/{pid}/cmdline"))?;
        match fs::read(path) {
            Ok(bytes) => Ok(bytes.windows(9).any(|b| b == b"walsender")
                && bytes.windows(9).any(|b| b == b"streaming")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn wait(
        &self,
        h: &Host,
        seconds: u64,
        what: &'static str,
        mut predicate: impl FnMut() -> R<bool>,
    ) -> R<()> {
        let end = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < end {
            h.remaining()?;
            if let Some(receiver) = &self.receiver {
                receiver.healthy()?;
            }
            if predicate()? {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        Err(what.into())
    }

    pub(super) fn prepare(&mut self, h: &Deployment) -> R<()> {
        self.timeline = Self::source(h)?;
        h.check_source()?;
        require(
            h.sql("owner", SETTINGS, &[], false)? == "t|on|t|t|t|t|t",
            "physical backup prerequisites",
        )?;
        let size = h
            .sql(
                "owner",
                "SELECT sum(pg_database_size(oid)) FROM pg_database",
                &[],
                false,
            )?
            .parse::<u64>()?;
        require(
            free_bytes(&self.work)?
                > size
                    .checked_mul(4)
                    .and_then(|n| n.checked_add(9 * GIB))
                    .ok_or("physical backup size overflow")?,
            "backup capacity",
        )?;
        self.receiver = Some(Receiver::start(&h.host, &self.work)?);
        self.wait(
            &h.host,
            20,
            "streaming replication sender timed out",
            || Self::streaming(h),
        )?;
        self.base_backup(h)?;
        let archive = child(&self.work, "bootstrap.tar")?;
        self.archive_bootstrap(h, &archive)?;
        set(
            &mut self.receipt,
            "bootstrap_sha256",
            Json::Str(digest(&archive)?),
        )?;

        self.start_copy(h, false)?;
        let ns = self.hold(h, "migration", &[])?;
        self.wait(&h.host, 40, "migration rehearsal startup timed out", || {
            h.ready_db(&ns)
        })?;
        require(
            h.sql("owner", "SELECT pg_is_in_recovery()", &ns, false)? == "f",
            "writable rehearsal",
        )?;
        h.upgrade(&ns, self)?;
        self.stop_copy(&h.host)?;
        self.start_copy(h, true)?;
        self.wait(&h.host, 45, "archive recovery startup timed out", || {
            let output = h.host.exec(
                h.host
                    .cmd(h.host.tool("pg_isready")?)
                    .args(["-q", "-h"])
                    .arg(child(&self.runtime, "socket")?)
                    .args(["-p", "35432", "-U", "setup_admin", "-d", "td_mta"]),
                &[],
                Duration::from_secs(4),
                false,
            )?;
            Ok(output.status.success())
        })?;
        let identity = self.local(
            &h.host,
            "SELECT pg_is_in_recovery(),system_identifier,timeline_id
             FROM pg_control_system(),pg_control_checkpoint()",
        )?;
        require(
            identity == fmt(format_args!("t|{SYSTEM}|{}", self.timeline))?,
            "archive recovery identity",
        )?;
        let (lsn, segment) = self.marker(h, &fmt(format_args!("td_probe_{}", random_hex(16)?))?)?;
        self.feed(&h.host, &segment)?;
        let query = fmt(format_args!(
            "SELECT pg_is_in_recovery() AND pg_last_wal_replay_lsn()>='{lsn}'::pg_lsn"
        ))?;
        self.wait(
            &h.host,
            60,
            "intermediate recovery replay timed out",
            || Ok(self.local(&h.host, &query)? == "t"),
        )?;
        set(&mut self.receipt, "intermediate_lsn", lsn.into())?;
        durable(&child(&self.work, "receipt.json")?, &self.receipt)
    }

    /// A fresh, verified online base backup of the primary in `base`.
    pub(super) fn base_backup(&mut self, h: &Deployment) -> R<()> {
        if self.timeline == 0 {
            self.timeline = Self::source(h)?;
        }
        let base = child(&self.work, "base")?;
        h.host.exec(
            h.host
                .cmd(h.host.tool("pg_basebackup")?)
                .args(["-w", "--no-slot", "--wal-method=stream", "-D"])
                .arg(&base)
                .args(["-d", "service=cutover_repl"])
                .env("PGSERVICEFILE", child(Path::new(CONTROL), "service.conf")?),
            &[],
            Duration::from_secs(1200),
            true,
        )?;
        h.host.exec(
            h.host.cmd(h.host.tool("pg_verifybackup")?).arg(&base),
            &[],
            Duration::from_secs(300),
            true,
        )?;
        let manifest = child(&base, "backup_manifest")?;
        let (range, start) = read_manifest(&manifest, SYSTEM, self.timeline)?;
        self.start_lsn = start;
        set(&mut self.receipt, "system_identifier", SYSTEM.into())?;
        set(
            &mut self.receipt,
            "timeline",
            Json::Str(fmt(format_args!("{}", self.timeline))?),
        )?;
        set(
            &mut self.receipt,
            "manifest_sha256",
            Json::Str(digest(&manifest)?),
        )?;
        set(&mut self.receipt, "wal_range", range)?;
        Ok(())
    }

    fn archive_bootstrap(&self, h: &Deployment, archive: &Path) -> R<()> {
        let mut tar = h.host.cmd("tar");
        tar.arg("-cf").arg(archive).args(["-C", "/"]);
        let mut include = |path: &Path| -> R<()> {
            tar.arg(path.strip_prefix("/")?);
            Ok(())
        };
        let mail = Path::new(MAIL);
        for name in [
            "postgresql.conf",
            "owner-connection",
            "owner-password",
            "runtime-connection",
            "runtime-password",
            "bootstrap.conf",
            "config-version",
            "account-id",
        ] {
            include(&child(mail, name)?)?;
        }
        for entry in fs::read_dir(mail)? {
            let path = entry?.path();
            if path.extension() == Some(OsStr::new("toml")) {
                include(&path)?;
            }
        }
        for path in [
            Path::new("/var/lib/td-mta/backup-control"),
            Path::new(DEBIAN_POSTGRES),
            Path::new(PG_DROPIN),
            h.previous.parent().ok_or("release parent")?,
        ] {
            include(path)?;
        }
        // The td-built server and its closure once selected: those two store
        // paths, not the rest of /td/store.
        if !h.host.debian_tools() {
            include(Path::new(crate::activate::BINARIES_DROPIN))?;
            let backup = Path::new(crate::activate::BACKUP_DROPIN);
            if backup.try_exists()? {
                include(backup)?;
            }
            for path in crate::activate::selected_store(&h.host)? {
                include(&path)?;
            }
        }
        for file in ["service.conf", "marker.pass", "repl.pass"] {
            include(&child(Path::new(CONTROL), file)?)?;
        }
        if h.isolated || h.isolation_change {
            include(Path::new(service_database::VAULT))?;
            for file in ["config.json", "isolated.json"] {
                include(&child(Path::new(isolation::NOTIFY), file)?)?;
            }
        }
        if h.isolated {
            for path in isolation::backup_files()? {
                include(&path)?;
            }
        }
        h.host.exec(&mut tar, &[], Duration::from_secs(180), true)?;
        Ok(())
    }

    // A private, writable copy of the base owned by the server account.
    fn copy_base(&self, h: &Deployment, name: &str, standby: bool) -> R<PathBuf> {
        let data = child(&self.work, name)?;
        h.host.exec(
            h.host
                .cmd("cp")
                .args(["-a", "--reflink=auto"])
                .arg(child(&self.work, "base")?)
                .arg(&data),
            &[],
            Duration::from_secs(180),
            true,
        )?;
        check_no_links(&data)?;
        for signal in ["recovery.signal", "standby.signal"] {
            remove_file(&child(&data, signal)?)?;
        }
        fs::write(child(&data, "postgresql.auto.conf")?, b"")?;
        if standby {
            fs::write(child(&data, "standby.signal")?, b"")?;
        }
        h.host
            .run(h.host.cmd("chown").args(["-R", "td-mta:td-mta"]).arg(&data))?;
        for dir in [
            child(&self.runtime, "cluster")?,
            child(&self.runtime, "wal")?,
        ] {
            fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(dir)?;
        }
        Ok(data)
    }

    fn bind(&self) -> R<&str> {
        let bind = self.runtime.to_str().ok_or("non-UTF8 trial path")?;
        require(unit_path(bind), "invalid trial directory")?;
        Ok(bind)
    }

    fn start_copy(&mut self, h: &Deployment, standby: bool) -> R<()> {
        let name = if standby { "replay" } else { "migration" };
        let scoped = h
            .plan
            .as_ref()
            .and_then(|p| p.get("service_grants"))
            .is_some();
        let data = self.copy_base(h, name, standby)?;
        let hba = trial_hba(standby, scoped)?;
        self.config_file(&h.host, "hba", hba.as_bytes())?;
        self.config_file(&h.host, "ident", b"trial root setup_admin\n")?;
        if !standby && scoped {
            self.config_file(
                &h.host,
                "admin-hba",
                service_database::private_hba(&child(Path::new(PGDATA), "pg_hba.conf")?)?
                    .as_bytes(),
            )?;
            self.config_file(&h.host, "admin-ident", b"trial root setup_admin\n")?;
        }
        let bind = self.bind()?;
        let mut config = fmt(format_args!(
            "data_directory='{bind}/cluster'\nhba_file='{bind}/hba'\nident_file='{bind}/ident'\n\
             listen_addresses=''\nport='35432'\nunix_socket_directories='{bind}/socket'\n\
             unix_socket_group='{}'\nunix_socket_permissions='0660'\nssl='off'\n\
             shared_preload_libraries=''\narchive_mode='off'\n\
             archive_command=''\nprimary_conninfo=''\nprimary_slot_name=''\nhot_standby='on'\n\
             log_statement='none'\nlog_min_error_statement='panic'\n",
            isolation::DB_GROUP,
        ))?;
        for limit in [
            "max_connections",
            "max_worker_processes",
            "max_wal_senders",
            "max_locks_per_transaction",
            "max_prepared_transactions",
            "track_commit_timestamp",
        ] {
            let value = h.sql("owner", &fmt(format_args!("SHOW {limit}"))?, &[], false)?;
            require(
                matches!(value.as_str(), "on" | "off")
                    || (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())),
                "unexpected PostgreSQL setting",
            )?;
            td_alloc::push_str(&mut config, &fmt(format_args!("{limit}='{value}'\n"))?)?;
        }
        if standby {
            let recovery = fmt(format_args!(
                "recovery_target_timeline='current'\nrecovery_target_action='pause'\n\
                 recovery_target_name='{}'\nrestore_command='cp -- {bind}/wal/%f %p'\n",
                self.point,
            ))?;
            td_alloc::push_str(&mut config, &recovery)?;
        }
        self.config_file(&h.host, "conf", config.as_bytes())?;
        let file = fmt(format_args!("config_file={bind}/conf"))?;
        let postgres = h.host.tool("postgres")?;
        self.launch(h, name, &data, &postgres, &[owned("-c")?, file])
    }

    /// A production configuration, run by `postgres`, on a fresh copy of the
    /// base. The command line overrides only where the trial and its files
    /// live, keeps its socket open whatever the fence include says, and stops
    /// archiving and replication. Listen and TLS settings stay in effect.
    pub(super) fn start_staged(
        &mut self,
        h: &Deployment,
        name: &str,
        postgres: &Path,
        conf: &[u8],
        hba: &[u8],
        ident: &[u8],
    ) -> R<()> {
        let data = self.copy_base(h, name, false)?;
        self.config_file(&h.host, "conf", conf)?;
        self.config_file(&h.host, "hba", hba)?;
        self.config_file(&h.host, "ident", ident)?;
        let bind = self.bind()?;
        let mut args = Vec::new();
        for setting in [
            fmt(format_args!("config_file={bind}/conf"))?,
            fmt(format_args!("data_directory={bind}/cluster"))?,
            fmt(format_args!("hba_file={bind}/hba"))?,
            fmt(format_args!("ident_file={bind}/ident"))?,
            fmt(format_args!("unix_socket_directories={bind}/socket"))?,
            owned("unix_socket_permissions=0660")?,
            owned("archive_mode=off")?,
            owned("primary_conninfo=")?,
        ] {
            td_alloc::push(&mut args, owned("-c")?)?;
            td_alloc::push(&mut args, setting)?;
        }
        self.launch(h, name, &data, postgres, &args)
    }

    fn launch(
        &mut self,
        h: &Deployment,
        name: &str,
        data: &Path,
        postgres: &Path,
        args: &[String],
    ) -> R<()> {
        let bind = self.bind()?;
        let mount = child(&self.runtime, "cluster")?;
        let job = h
            .work
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or("invalid job name")?;
        let unit = fmt(format_args!("td-mta-trial-{}-{name}", self.point))?;
        let mut command = h.host.cmd("systemd-run");
        command
            .args(["--quiet", "--collect"])
            .arg(fmt(format_args!("--unit={unit}"))?)
            .arg("--property=Type=exec");
        for property in [
            fmt(format_args!("BindsTo=td-mta-{job}.service"))?,
            fmt(format_args!("After=td-mta-{job}.service"))?,
            fmt(format_args!(
                "BindPaths={}:{}",
                data.display(),
                mount.display()
            ))?,
            fmt(format_args!(
                "BindReadOnlyPaths={}/archive:{bind}/wal",
                self.work.display()
            ))?,
            fmt(format_args!(
                "ReadWritePaths={bind}/socket {}",
                mount.display()
            ))?,
            fmt(format_args!("SupplementaryGroups={}", isolation::DB_GROUP))?,
        ] {
            command.arg(fmt(format_args!("--property={property}"))?);
        }
        command
            .args([
                "--property=User=td-mta",
                "--property=Group=td-mta",
                "--property=PrivateNetwork=yes",
                "--property=PrivateTmp=yes",
                "--property=PrivateIPC=yes",
                "--property=PrivateDevices=yes",
                "--property=NoNewPrivileges=yes",
                "--property=ProtectSystem=strict",
                "--property=ProtectHome=yes",
                "--property=KillSignal=SIGINT",
                "--property=TimeoutStopSec=45",
                "--property=UMask=0077",
                "--property=InaccessiblePaths=/var/lib/td-mta /var/lib/td-mta-operator /data",
            ])
            .arg(postgres)
            .arg("-D")
            .arg(mount)
            .args(args);
        self.unit = Some(unit); // systemd may accept the start before losing the reply.
        h.host.run(&mut command)?;
        let pid = self.trial_pid(&h.host)?;
        require(
            fs::metadata(fmt(format_args!("/proc/{pid}/ns/net"))?)?.ino()
                != fs::metadata("/proc/self/ns/net")?.ino(),
            "trial network isolation",
        )
    }

    pub(super) fn trial_pid(&self, h: &Host) -> R<u32> {
        let unit = self.unit.as_deref().ok_or("missing trial unit")?;
        let pid = h
            .out(
                h.cmd("systemctl")
                    .args(["show", unit, "-p", "MainPID", "--value"]),
            )?
            .parse::<u32>()?;
        require(pid > 0, "trial has no MainPID")?;
        Ok(pid)
    }

    /// Enter the running trial's socket, and `files` over their production
    /// paths, through a mount namespace holder stopped with the trial.
    pub(super) fn hold(
        &mut self,
        h: &Deployment,
        name: &str,
        files: &[(&Path, &Path)],
    ) -> R<Vec<String>> {
        let unit = fmt(format_args!("td-mta-holder-{}-{name}", self.point))?;
        self.holder = Some(owned(&unit)?);
        hold(h, &unit, &child(&self.runtime, "socket")?, files)
    }

    fn config_file(&self, h: &Host, name: &str, data: &[u8]) -> R<()> {
        let path = child(&self.runtime, name)?;
        fs::write(&path, data)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;
        h.run(h.cmd("chown").arg("root:td-mta").arg(path))
    }

    fn admin(&self, h: &Host, database: &str) -> R<std::process::Command> {
        let mut cmd = h.cmd(h.tool("psql")?);
        cmd.args(["-XwqAt", "-v", "ON_ERROR_STOP=1", "-h"])
            .arg(child(&self.runtime, "socket")?)
            .args(["-p", "35432", "-U", "setup_admin", "-d", database]);
        Ok(cmd)
    }

    fn local(&self, h: &Host, query: &str) -> R<String> {
        h.out(self.admin(h, "td_mta")?.args(["-c", query]))
    }

    /// `script` as the trial's superuser in `database`, through the trial's
    /// own peer rule: only a trial whose HBA has that rule admits it.
    pub(super) fn superuser(
        &self,
        h: &Host,
        database: &str,
        script: &str,
        timeout: Duration,
    ) -> R<String> {
        let out = h.exec(
            &mut self.admin(h, database)?,
            script.as_bytes(),
            timeout,
            true,
        )?;
        owned(std::str::from_utf8(&out.stdout)?.trim())
    }

    pub(super) fn create_service_logins(
        &self,
        h: &Deployment,
        values: &service_database::Passwords,
    ) -> R<()> {
        let mut names = String::new();
        for name in service_database::ROLES {
            if !names.is_empty() {
                td_alloc::push_str(&mut names, ",")?;
            }
            td_alloc::push_str(&mut names, &fmt(format_args!("'td_mta_{name}'"))?)?;
        }
        let count = self
            .local(
                &h.host,
                &fmt(format_args!(
                    "SELECT count(*) FROM pg_roles WHERE rolname IN ({names})"
                ))?,
            )?
            .parse::<usize>()?;
        if count != 0 {
            require(
                count == service_database::ROLES.len()
                    && self.local(
                        &h.host,
                        "SELECT to_regclass('td_mta_service.material_bindings') IS NOT NULL",
                    )? == "t",
                "unexpected pre-existing service logins",
            )?;
        } else {
            let socket = child(&self.runtime, "socket")?;
            let args = [
                "-Xwq",
                "-v",
                "ON_ERROR_STOP=1",
                "-h",
                socket.to_str().ok_or("invalid socket directory")?,
                "-p",
                "35432",
                "-U",
                "setup_admin",
                "-d",
                "td_mta",
            ];
            service_database::create_logins(&h.host, &h.host.tool("psql")?, &args, values)?;
        }
        Ok(())
    }

    fn marker(&self, h: &Deployment, name: &str) -> R<(String, String)> {
        require(
            Self::source(h)? == self.timeline && Self::streaming(h)?,
            "source or receiver changed",
        )?;
        let script = fmt(format_args!(
            "SELECT pg_create_restore_point('{name}') AS lsn \\gset\n\
             SELECT :'lsn',pg_walfile_name(:'lsn');\nSELECT pg_switch_wal();\n"
        ))?;
        let result = h.sql("marker", &script, &[], true)?;
        let mut lines = result.lines();
        let (lsn, segment) = lines
            .next()
            .and_then(|l| l.split_once('|'))
            .ok_or("restore point result lost")?;
        require(
            lines.next().is_some() && lines.next().is_none(),
            "restore point result lost",
        )?;
        parse_lsn(lsn)?;
        require(
            parse_segment(segment)?.0 == self.timeline,
            "restore point timeline",
        )?;
        Ok((owned(lsn)?, owned(segment)?))
    }

    fn feed(&self, h: &Host, segment: &str) -> R<()> {
        let wal = child(&self.work, "wal")?;
        let archive = child(&self.work, "archive")?;
        let target = child(&wal, segment)?;
        self.wait(h, 60, "completed WAL segment timed out", || {
            Ok(target.is_file())
        })?;
        for item in fs::read_dir(&wal)? {
            let item = item?;
            let filename = item.file_name();
            let Some(name) = filename.to_str() else {
                continue;
            };
            if parse_segment(name).is_err() {
                continue;
            }
            let dest = child(&archive, name)?;
            if dest.try_exists()? {
                continue;
            }
            let tmp = child(&archive, "segment.pending")?;
            h.run(h.cmd("cp").arg("--reflink=auto").arg(item.path()).arg(&tmp))?;
            h.run(h.cmd("chown").arg("root:td-mta").arg(&tmp))?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o640))?;
            fs::rename(tmp, dest)?;
        }
        Ok(())
    }

    pub(super) fn seal(&mut self, h: &Deployment) -> R<()> {
        let (lsn, segment) = self.marker(h, &self.point)?;
        set(&mut self.receipt, "lsn", Json::Str(owned(&lsn)?))?;
        set(&mut self.receipt, "segment", Json::Str(owned(&segment)?))?;
        let receipt = child(&self.work, "receipt.json")?;
        durable(&receipt, &self.receipt)?;
        self.feed(&h.host, &segment)?;
        let condition = fmt(format_args!(
            "SELECT pg_is_in_recovery() AND pg_get_wal_replay_pause_state()='paused'
             AND pg_last_wal_replay_lsn()='{lsn}'::pg_lsn"
        ))?;
        self.wait(&h.host, 60, "exact recovery pause timed out", || {
            Ok(self.local(&h.host, &condition)? == "t")
        })?;
        let compare = fs::read_to_string(child(Path::new(CONTROL), "compare-selected.sql")?)?;
        let before = h.sql("owner", &compare, &[], true)?;
        require(
            !before.is_empty() && self.local(&h.host, &compare)? == before,
            "selected mail comparison",
        )?;
        require(
            Self::source(h)? == self.timeline && Self::streaming(h)?,
            "final source identity",
        )?;
        self.stop_receiver()?;
        let size = h
            .sql(
                "owner",
                "SELECT pg_size_bytes(current_setting('wal_segment_size'))",
                &[],
                false,
            )?
            .parse::<u64>()?;
        check_continuity(
            &child(&self.work, "wal")?,
            self.timeline,
            self.start_lsn,
            &segment,
            size,
        )?;
        require(
            self.local(&h.host, &condition)? == "t",
            "trial lost recovery pause",
        )?;
        self.stop_copy(&h.host)?;
        for (k, v) in [
            ("status", "verified".into()),
            ("selected", before.into()),
            ("exact_lsn", true.into()),
            ("receiver_stopped", true.into()),
            ("trial_stopped", true.into()),
        ] {
            set(&mut self.receipt, k, v)?;
        }
        durable(&receipt, &self.receipt)?;
        h.host.run(h.host.cmd("sync").arg("-f").arg(&self.work))
    }

    fn stop_receiver(&mut self) -> R<()> {
        match self.receiver.take() {
            Some(receiver) => receiver.stop(),
            None => Ok(()),
        }
    }

    pub(super) fn stop_copy(&mut self, h: &Host) -> R<()> {
        if let Some(unit) = &self.holder {
            stop_unit(h, unit)?;
            self.holder = None;
        }
        if let Some(unit) = &self.unit {
            stop_unit(h, unit)?;
            self.unit = None;
        }
        Ok(())
    }

    pub(super) fn close(&mut self, h: &Deployment) -> R<()> {
        let receiver = self.stop_receiver();
        self.stop_copy(&h.host)?;
        for name in ["migration", "replay", "transport", "binaries", "archive"] {
            let path = child(&self.work, name)?;
            if path.try_exists()? {
                fs::remove_dir_all(path)?;
            }
        }
        fs::remove_dir_all(&self.runtime)?;
        receiver
    }
}

pub(super) fn stop_unit(h: &Host, unit: &str) -> R<()> {
    h.exec(
        h.cmd("systemctl").arg("stop").arg(unit),
        &[],
        Duration::from_secs(50),
        false,
    )?;
    let state =
        h.out(
            h.cmd("systemctl")
                .arg("show")
                .arg(unit)
                .args(["-p", "ActiveState", "--value"]),
        )?;
    require(
        matches!(state.as_str(), "inactive" | "failed"),
        "isolated PostgreSQL or its holder still running",
    )
}

// A pathname systemd's space- and colon-separated path properties carry intact.
fn unit_path(path: &str) -> bool {
    path.starts_with('/')
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))
}

fn same_inode(a: &Path, b: &Path) -> R<bool> {
    let (a, b) = (fs::metadata(a)?, fs::metadata(b)?);
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}

/// Start `unit`, a root process with no other sandboxing in a mount namespace
/// where `socket` is the production socket directory and each `(source,
/// target)` file is bound read-only over its production path. Returns the
/// prefix that runs a command there: native commands, root's psql and
/// runuser then reach the trial with the production connection settings.
/// The holder is bound to the job and must be stopped with its trial.
pub(super) fn hold(
    h: &Deployment,
    unit: &str,
    socket: &Path,
    files: &[(&Path, &Path)],
) -> R<Vec<String>> {
    let job = h
        .work
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or("invalid job name")?;
    let text = |path: &Path| -> R<String> {
        let text = path.to_str().ok_or("non-UTF8 holder path")?;
        require(unit_path(text), "invalid holder path")?;
        owned(text)
    };
    let mut command = h.host.cmd("systemd-run");
    command
        .args(["--quiet", "--collect"])
        .arg(fmt(format_args!("--unit={unit}"))?)
        .arg("--property=Type=exec")
        .arg(fmt(format_args!(
            "--property=BindsTo=td-mta-{job}.service"
        ))?)
        .arg(fmt(format_args!("--property=After=td-mta-{job}.service"))?)
        .arg(fmt(format_args!(
            "--property=BindPaths={}:{SOCKET_DIR}",
            text(socket)?
        ))?);
    for (source, target) in files {
        command.arg(fmt(format_args!(
            "--property=BindReadOnlyPaths={}:{}",
            text(source)?,
            text(target)?
        ))?);
    }
    command.args(["/usr/bin/sleep", "infinity"]);
    h.host.run(&mut command)?;
    let pid = h.pid(unit)?;
    require(
        fs::metadata(fmt(format_args!("/proc/{pid}/ns/mnt"))?)?.ino()
            != fs::metadata("/proc/self/ns/mnt")?.ino(),
        "holder mount namespace",
    )?;
    let root = |target: &Path| fmt(format_args!("/proc/{pid}/root{}", target.display()));
    require(
        same_inode(Path::new(&root(Path::new(SOCKET_DIR))?), socket)?,
        "holder socket directory",
    )?;
    for (source, target) in files {
        require(
            same_inode(Path::new(&root(target)?), source)?,
            "holder file binding",
        )?;
    }
    let mut prefix = td_alloc::with_capacity(5)?;
    for word in [
        owned("nsenter")?,
        owned("--target")?,
        fmt(format_args!("{pid}"))?,
        owned("--mount")?,
        owned("--")?,
    ] {
        td_alloc::push(&mut prefix, word)?;
    }
    Ok(prefix)
}

fn trial_hba(standby: bool, scoped: bool) -> R<String> {
    if standby {
        return owned("local all setup_admin peer map=trial\n");
    }
    let mut rules = String::new();
    if scoped {
        td_alloc::push_str(&mut rules, "local td_mta setup_admin peer map=trial\n")?;
        td_alloc::push_str(&mut rules, &service_database::hba_rules()?)?;
    }
    td_alloc::push_str(
        &mut rules,
        "local td_mta td_mta_owner scram-sha-256\n\
         local td_mta td_mta_runtime scram-sha-256\n",
    )?;
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn replay_archive_requires_every_segment_across_a_log_boundary() -> R<()> {
        let dir = td_fs::TempDir::new("recovery-wal")?;
        let size = 16 * 1024 * 1024;
        let start = parse_lsn("1/FF010010")?;
        let first = child(dir.path(), "0000000700000001000000FF")?;
        let final_segment = "000000070000000200000000";
        let last = child(dir.path(), final_segment)?;
        fs::File::create(&first)?.set_len(size)?;
        fs::File::create(&last)?.set_len(size)?;
        check_continuity(dir.path(), 7, start, final_segment, size)?;
        assert!(check_continuity(dir.path(), 8, start, final_segment, size).is_err());
        assert!(check_continuity(dir.path(), 7, start, "000000070000000100000100", size).is_err());
        assert!(check_continuity(dir.path(), 7, start, "0000000700000001000000FE", size).is_err());
        fs::File::create(&first)?.set_len(size - 1)?;
        assert!(check_continuity(dir.path(), 7, start, final_segment, size).is_err());
        fs::remove_file(&first)?;
        symlink(&last, &first)?;
        assert!(check_continuity(dir.path(), 7, start, final_segment, size).is_err());
        assert!(check_no_links(dir.path()).is_err());
        Ok(())
    }

    #[test]
    fn manifest_pins_one_source_timeline_and_a_valid_lsn() -> R<()> {
        let dir = td_fs::TempDir::new("recovery-manifest")?;
        let manifest = child(dir.path(), "backup_manifest")?;
        for (system, timeline, start, valid) in [
            (SYSTEM, 7, "A/FF000010", true),
            ("1", 7, "A/FF000010", false),
            (SYSTEM, 8, "A/FF000010", false),
            (SYSTEM, 7, "A/1;0", false),
            (SYSTEM, 7, "100000000/1", false),
        ] {
            fs::write(
                &manifest,
                format!(
                    r#"{{"System-Identifier":{system},"WAL-Ranges":[{{"Timeline":{timeline},"Start-LSN":"{start}"}}]}}"#
                ),
            )?;
            let result = read_manifest(&manifest, SYSTEM, 7);
            assert_eq!(result.is_ok(), valid);
            if valid {
                assert_eq!(result?.1, 0xA_FF00_0010);
            }
        }
        Ok(())
    }

    #[test]
    fn trial_logins_use_scram_on_the_socket_and_admin_peer_only_privately() -> R<()> {
        assert_eq!(
            trial_hba(true, true)?,
            "local all setup_admin peer map=trial\n"
        );
        for (scoped, count) in [(false, 2), (true, 2 + service_database::ROLES.len())] {
            let rules = trial_hba(false, scoped)?;
            assert_eq!(
                rules
                    .lines()
                    .filter(|line| line.starts_with("local td_mta td_mta_")
                        && line.ends_with(" scram-sha-256"))
                    .count(),
                count
            );
            for line in rules.lines() {
                assert!(
                    line == "local td_mta setup_admin peer map=trial"
                        || line.ends_with(" scram-sha-256")
                );
                assert!(line.starts_with("local "));
            }
            assert_eq!(rules.contains("setup_admin"), scoped);
        }
        Ok(())
    }

    #[test]
    fn holder_paths_are_absolute_and_free_of_unit_separators() {
        for valid in ["/run/td-mta-recovery-ab12/socket", SOCKET_DIR, PG_DROPIN] {
            assert!(unit_path(valid), "{valid}");
        }
        for invalid in ["relative", "/a b", "/a:b", "/a\\b", "/a\nb", ""] {
            assert!(!unit_path(invalid), "{invalid}");
        }
    }
}

#[cfg(test)]
#[path = "tests/physical_pg.rs"]
mod postgres_test;
