//! Explicit host schedules for existing mail operations; no service activation.
use super::install_mta_backup::{transfer, Bounds};
use super::{absolute, directory, profile_name, scheme, shell, systemd, text, write_new, Result};
use crate::json::Json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use td_engine::allocation;

const HELP: &str = "Usage: ./install-td-mta --operations-services --state-dir PATH [--prefix PATH]
  [--mail-profile NAME ...]
  [--notify-recipient EMAIL --notify-connection-file OWNER_PATH --notify-epoch HEX
   [--notify-smtp LOOPBACK:PORT] [--notify-hour 0..23] [--notify-df-bin PATH]
   [--notify-systemd-timezone IANA_NAME]]
  [--scrub-connection-file OWNER_PATH --scrub-epoch HEX --scrub-state-dir PATH]
  [--search-connection-file OWNER_PATH --search-epoch HEX --search-state-dir PATH]
  [--certificate-profile NAME ... --connection-file PATH]
  [--tls-certificate PATH ... --openssl-bin PATH]
  [--backup-service-file PATH --backup-service NAME --backup-destination PATH
   --postgres-bin PATH [--tool-library-dir PATH]
   [--backup-max-archive-bytes N] [--backup-max-total-bytes N] [--backup-retain N]]
  [--backup-drill-scratch-directory PATH --backup-destination PATH
   --postgres-bin PATH [--tool-library-dir PATH]
   [--backup-pull-host SSH_ALIAS [--backup-max-archive-bytes N]]]

Notification collection runs every minute; digests run at 08:00 (or --notify-hour)
in the supervisor local timezone. Notification SMTP is loopback only; credentials
are not needed. Optional df must be an explicit absolute host tool path.
Generates independent systemd timers and Shepherd timed services, without starting
anything. Search preparation runs one bounded page per minute, waits five minutes
after each cycle, and has a separate freshness check. Body scrubbing runs one bounded page per minute, starts a new cycle
a day after completion, and has a separate freshness check. Mail, certificate and backup freshness checks run every 15 minutes;
backups run daily at 03:15 in the supervisor's local timezone. Certificate checks
require 30 days remaining; backups must be at most two days old. The local backup
writer requires PostgreSQL 17 and protected credentials. Its default limits are
1 GiB/archive, 8 GiB/directory and 30 archives; override with --backup-* bounds.
An optional restore drill runs Sundays at 05:15 local time. Drill-only schedules
use an archive destination and a separate private scratch directory. An optional
SSH pull refreshes the recovery copy daily at 04:15 using the operator's SSH config.
On systemd, the Sunday pull and drill share the 05:15 activation.
The recovery copy is checked every 15 minutes using the original backup time.
See td-mta/DEPLOYMENT.md.
";

struct Options {
    prefix: PathBuf,
    state: PathBuf,
    mail: Vec<String>,
    certificates: Vec<String>,
    connection: Option<PathBuf>,
    tls: Vec<PathBuf>,
    openssl: Option<PathBuf>,
    backup: Option<Backup>,
    drill: Option<RestoreDrill>,
    scrub: Option<Scrub>,
    search: Option<Scrub>,
    notification: Option<Notification>,
}
struct Notification {
    recipient: String,
    connection: PathBuf,
    epoch: String,
    smtp: String,
    hour: u8,
    timezone: Option<String>,
    df: Option<PathBuf>,
}
struct Scrub {
    connection: PathBuf,
    epoch: String,
    state: PathBuf,
}
struct BackupPull {
    host: String,
    ceiling: Option<u64>,
}
struct RestoreDrill {
    pull: Option<BackupPull>,
    destination: PathBuf,
    scratch: PathBuf,
    tools: PostgresTools,
}
struct PostgresTools {
    bin: PathBuf,
    library: Option<PathBuf>,
}
impl PostgresTools {
    fn environment(&self) -> Result<Vec<String>> {
        let mut env = Vec::new();
        for (key, dir) in [
            ("PATH", Some(&self.bin)),
            ("LD_LIBRARY_PATH", self.library.as_ref()),
        ] {
            if let Some(dir) = dir {
                let entry = allocation::format(format_args!("{key}={}", text(dir)?))
                    .map_err(|_| String::new())?;
                allocation::push(&mut env, entry).map_err(|_| String::new())?;
            }
        }
        Ok(env)
    }
}
struct Backup {
    service_file: PathBuf,
    service: String,
    destination: PathBuf,
    tools: PostgresTools,
    bounds: Option<Bounds>,
}

fn options(args: &[String], home: &str) -> Result<Options> {
    let mut values = BTreeMap::new();
    let mut mail = Vec::new();
    let mut certificates = Vec::new();
    let mut tls = Vec::new();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("missing {flag} value"))?;
        match flag.as_str() {
            "--mail-profile" | "--certificate-profile" => {
                let list = if flag == "--mail-profile" {
                    &mut mail
                } else {
                    &mut certificates
                };
                if !profile_name(value) || list.contains(value) || list.len() >= 32 {
                    return Err("profiles must be valid, unique and at most 32 per kind".into());
                }
                list.push(value.clone());
            }
            "--tls-certificate" => {
                let path = absolute(value)?;
                if tls.len() >= 32 || tls.contains(&path) {
                    return Err("certificate paths must be unique and at most 32".into());
                }
                tls.push(path);
            }
            "--prefix"
            | "--state-dir"
            | "--connection-file"
            | "--openssl-bin"
            | "--scrub-connection-file"
            | "--scrub-epoch"
            | "--scrub-state-dir"
            | "--search-connection-file"
            | "--search-epoch"
            | "--search-state-dir"
            | "--backup-service-file"
            | "--backup-service"
            | "--backup-destination"
            | "--backup-pull-host"
            | "--backup-drill-scratch-directory"
            | "--backup-max-archive-bytes"
            | "--backup-max-total-bytes"
            | "--backup-retain"
            | "--postgres-bin"
            | "--tool-library-dir"
            | "--notify-recipient"
            | "--notify-connection-file"
            | "--notify-epoch"
            | "--notify-smtp"
            | "--notify-hour"
            | "--notify-systemd-timezone"
            | "--notify-df-bin" => {
                if values.insert(flag.as_str(), value.as_str()).is_some() {
                    return Err(format!("duplicate {flag}"));
                }
            }
            _ => return Err(format!("unknown operations option: {flag}")),
        }
    }
    let path =
        |key| -> Result<Option<PathBuf>> { values.get(key).map(|s| absolute(s)).transpose() };
    let required = |key| -> Result<PathBuf> { path(key)?.ok_or_else(|| format!("missing {key}")) };
    let scrub = if [
        "--scrub-connection-file",
        "--scrub-epoch",
        "--scrub-state-dir",
    ]
    .iter()
    .any(|key| values.contains_key(key))
    {
        let epoch = values.get("--scrub-epoch").ok_or("missing --scrub-epoch")?;
        if epoch.len() != 32
            || !epoch
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid scrub epoch".into());
        }
        Some(Scrub {
            connection: required("--scrub-connection-file")?,
            epoch: (*epoch).into(),
            state: required("--scrub-state-dir")?,
        })
    } else {
        None
    };
    let search = if [
        "--search-connection-file",
        "--search-epoch",
        "--search-state-dir",
    ]
    .iter()
    .any(|key| values.contains_key(key))
    {
        let epoch = values
            .get("--search-epoch")
            .ok_or("missing --search-epoch")?;
        if epoch.len() != 32
            || !epoch
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid search epoch".into());
        }
        let state = required("--search-state-dir")?;
        if state == required("--state-dir")? || scrub.as_ref().is_some_and(|v| v.state == state) {
            return Err("search progress needs a dedicated state directory".into());
        }
        Some(Scrub {
            connection: required("--search-connection-file")?,
            epoch: (*epoch).into(),
            state,
        })
    } else {
        None
    };
    let postgres_tools = || -> Result<PostgresTools> {
        let bin = required("--postgres-bin")?;
        let library = path("--tool-library-dir")?;
        if text(&bin)?.contains(':')
            || library
                .as_ref()
                .is_some_and(|p| p.to_string_lossy().contains(':'))
        {
            return Err("tool directories cannot contain PATH separators".into());
        }
        Ok(PostgresTools { bin, library })
    };
    let writer = [
        "--backup-service-file",
        "--backup-service",
        "--backup-max-total-bytes",
        "--backup-retain",
    ]
    .iter()
    .any(|key| values.contains_key(key));
    let pull = if let Some(host) = values.get("--backup-pull-host") {
        if writer || !values.contains_key("--backup-drill-scratch-directory") {
            return Err(
                "backup pull requires a recovery drill without local backup writer options".into(),
            );
        }
        if !transfer::valid_host(host) {
            return Err("invalid backup SSH host alias".into());
        }
        let ceiling = values
            .get("--backup-max-archive-bytes")
            .map(|raw| {
                raw.parse::<u64>()
                    .ok()
                    .filter(|n| transfer::valid_ceiling(*n))
                    .ok_or("invalid backup pull archive byte limit")
            })
            .transpose()?;
        Some(BackupPull {
            host: allocation::string(host).map_err(|_| String::new())?,
            ceiling,
        })
    } else {
        None
    };
    let drill = if let Some(scratch) = path("--backup-drill-scratch-directory")? {
        let destination = required("--backup-destination")?;
        if scratch.starts_with(&destination) || destination.starts_with(&scratch) {
            return Err("backup and drill scratch directories must be separate".into());
        }
        Some(RestoreDrill {
            pull,
            destination,
            scratch,
            tools: postgres_tools()?,
        })
    } else {
        None
    };
    let backup = if writer
        || (values.contains_key("--backup-max-archive-bytes")
            && !values.contains_key("--backup-pull-host"))
        || (drill.is_none() && values.contains_key("--backup-destination"))
    {
        let service = values
            .get("--backup-service")
            .ok_or("missing --backup-service")?;
        if service.is_empty()
            || service.len() > 64
            || !service
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err("invalid libpq service name".into());
        }
        let bounds = if [
            "--backup-max-archive-bytes",
            "--backup-max-total-bytes",
            "--backup-retain",
        ]
        .iter()
        .any(|key| values.contains_key(key))
        {
            let number = |key, default| -> Result<u64> {
                values.get(key).map_or(Ok(default), |v| {
                    v.parse().map_err(|_| format!("invalid {key}"))
                })
            };
            Some(Bounds::new(
                number("--backup-max-archive-bytes", Bounds::DEFAULT.archive_bytes)?,
                number("--backup-max-total-bytes", Bounds::DEFAULT.total_bytes)?,
                number("--backup-retain", Bounds::DEFAULT.retain as u64)?,
            )?)
        } else {
            None
        };
        Some(Backup {
            service_file: required("--backup-service-file")?,
            service: (*service).into(),
            destination: required("--backup-destination")?,
            tools: postgres_tools()?,
            bounds,
        })
    } else {
        None
    };
    if backup.is_none()
        && drill.is_none()
        && ["--postgres-bin", "--tool-library-dir"]
            .iter()
            .any(|key| values.contains_key(key))
    {
        return Err("unused PostgreSQL tool settings".into());
    }
    let connection = path("--connection-file")?;
    let openssl = path("--openssl-bin")?;
    if certificates.is_empty() != connection.is_none() || tls.is_empty() != openssl.is_none() {
        return Err("certificate profiles need --connection-file; TLS files need --openssl-bin (no unused options)".into());
    }
    let notification = if values.keys().any(|k| k.starts_with("--notify-")) {
        let recipient = values
            .get("--notify-recipient")
            .ok_or("missing --notify-recipient")?
            .to_string();
        let epoch = values
            .get("--notify-epoch")
            .ok_or("missing --notify-epoch")?
            .to_string();
        let smtp = values
            .get("--notify-smtp")
            .copied()
            .unwrap_or("127.0.0.1:25")
            .to_string();
        let hour = values
            .get("--notify-hour")
            .copied()
            .unwrap_or("8")
            .parse::<u8>()
            .map_err(|_| "invalid notification hour")?;
        if hour > 23 {
            return Err("notification hour must be 0..23".into());
        }
        let timezone = values
            .get("--notify-systemd-timezone")
            .map(|v| v.to_string());
        if timezone.as_ref().is_some_and(|v| {
            v.is_empty()
                || v.len() > 128
                || v.starts_with('/')
                || v.contains("..")
                || !v
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/_+-".contains(&b))
        }) {
            return Err("invalid systemd timezone".into());
        }
        Some(Notification {
            recipient,
            epoch,
            smtp,
            hour,
            timezone,
            connection: required("--notify-connection-file")?,
            df: path("--notify-df-bin")?,
        })
    } else {
        None
    };
    if mail.is_empty()
        && certificates.is_empty()
        && tls.is_empty()
        && scrub.is_none()
        && search.is_none()
        && backup.is_none()
        && drill.is_none()
        && notification.is_none()
    {
        return Err("select at least one operation".into());
    }
    Ok(Options {
        prefix: path("--prefix")?.unwrap_or(absolute(home)?.join(".local")),
        state: required("--state-dir")?,
        mail,
        certificates,
        connection,
        tls,
        openssl,
        backup,
        drill,
        scrub,
        search,
        notification,
    })
}

#[derive(Clone, Copy)]
enum Schedule {
    Daily,
    DailyPull,
    WeeklyDrill { pull_first: bool },
    Digest(u8),
    QuarterHour,
    Minute,
}
struct Job {
    name: String,
    argv: Vec<String>,
    environment: Vec<String>,
    schedule: Schedule,
    timezone: Option<String>,
}

fn jobs(opts: &Options) -> Result<Vec<Job>> {
    let bin = opts.prefix.join("bin/td-mta");
    let helper = opts.prefix.join("bin/td-mta-operator");
    let mut jobs = Vec::new();
    let mut add = |name: String, argv: Vec<String>, environment: Vec<String>, schedule| {
        jobs.push(Job {
            name: format!("td-mta-maint-{name}"),
            argv,
            environment,
            schedule,
            timezone: None,
        });
    };
    for profile in &opts.mail {
        add(
            format!("mail-{profile}"),
            vec![
                text(&bin)?.into(),
                "status".into(),
                "--json".into(),
                "--require-ready".into(),
                "--runtime".into(),
                text(&opts.state.join(profile).join("runtime"))?.into(),
            ],
            vec![],
            Schedule::QuarterHour,
        );
    }
    if let Some(connection) = &opts.connection {
        for profile in &opts.certificates {
            add(
                format!("acme-{profile}"),
                vec![
                    text(&bin)?.into(),
                    "acme-status".into(),
                    "--connection-file".into(),
                    text(connection)?.into(),
                    "--certificate-profile".into(),
                    profile.clone(),
                    "--require-ready".into(),
                    "--minimum-validity-seconds".into(),
                    "2592000".into(),
                ],
                vec![],
                Schedule::QuarterHour,
            );
        }
    }
    if let Some(openssl) = &opts.openssl {
        for (i, certificate) in opts.tls.iter().enumerate() {
            add(
                format!("tls-{i}"),
                vec![
                    text(openssl)?.into(),
                    "x509".into(),
                    "-in".into(),
                    text(certificate)?.into(),
                    "-noout".into(),
                    "-checkend".into(),
                    "2592000".into(),
                ],
                vec![],
                Schedule::QuarterHour,
            );
        }
    }
    if let Some(backup) = &opts.backup {
        let mut argv = vec![
            text(&helper)?.into(),
            "install-td-mta".into(),
            "--backup".into(),
            "--service-file".into(),
            text(&backup.service_file)?.into(),
            "--service".into(),
            backup.service.clone(),
            "--destination".into(),
            text(&backup.destination)?.into(),
        ];
        if let Some(bounds) = backup.bounds {
            argv.extend([
                "--max-archive-bytes".into(),
                bounds.archive_bytes.to_string(),
                "--max-total-bytes".into(),
                bounds.total_bytes.to_string(),
                "--retain".into(),
                bounds.retain.to_string(),
            ]);
        }
        add(
            "backup".into(),
            argv,
            backup.tools.environment()?,
            Schedule::Daily,
        );
    }
    let host_command = |args: &[&str]| -> Result<Vec<String>> {
        let mut argv = Vec::new();
        for arg in [text(&helper)?, "install-td-mta"].iter().chain(args) {
            let arg = allocation::string(arg).map_err(|_| String::new())?;
            allocation::push(&mut argv, arg).map_err(|_| String::new())?;
        }
        Ok(argv)
    };
    let recovery = opts
        .drill
        .as_ref()
        .and_then(|drill| drill.pull.as_ref().map(|pull| (drill, pull)));
    if let Some((drill, pull)) = recovery {
        let mut argv = host_command(&[
            "--pull-backup",
            "--host",
            &pull.host,
            "--destination",
            text(&drill.destination)?,
        ])?;
        if let Some(limit) = pull.ceiling {
            let flag = allocation::string("--max-archive-bytes").map_err(|_| String::new())?;
            let value = allocation::format(format_args!("{limit}")).map_err(|_| String::new())?;
            allocation::push(&mut argv, flag).map_err(|_| String::new())?;
            allocation::push(&mut argv, value).map_err(|_| String::new())?;
        }
        add("backup-pull".into(), argv, Vec::new(), Schedule::DailyPull);
    }
    if let Some(destination) = opts
        .backup
        .as_ref()
        .map(|backup| &backup.destination)
        .or_else(|| recovery.map(|(drill, _)| &drill.destination))
    {
        add(
            "backup-status".into(),
            host_command(&[
                "--backup-status",
                "--destination",
                text(destination)?,
                "--maximum-age-seconds",
                "172800",
            ])?,
            Vec::new(),
            Schedule::QuarterHour,
        );
    }
    if let Some(drill) = &opts.drill {
        let destination = text(&drill.destination)?;
        add(
            "backup-drill".into(),
            host_command(&[
                "--restore-drill",
                "--destination",
                destination,
                "--scratch-directory",
                text(&drill.scratch)?,
            ])?,
            drill.tools.environment()?,
            Schedule::WeeklyDrill {
                pull_first: drill.pull.is_some(),
            },
        );
    }
    if let Some(scrub) = &opts.scrub {
        let mut argv = vec![
            text(&bin)?.into(),
            "postgres".into(),
            "blob-scrub".into(),
            "--state-dir".into(),
            text(&scrub.state)?.into(),
            "--epoch".into(),
            scrub.epoch.clone(),
            "--connection-file".into(),
            text(&scrub.connection)?.into(),
        ];
        add("scrub".into(), argv.clone(), vec![], Schedule::Minute);
        argv.truncate(7);
        if let Some(command) = argv.get_mut(2) {
            *command = "blob-scrub-status".into();
        }
        add("scrub-status".into(), argv, vec![], Schedule::QuarterHour);
    }
    if let Some(search) = &opts.search {
        let mut argv = vec![
            text(&bin)?.into(),
            "postgres".into(),
            "search-prepare".into(),
            "--state-dir".into(),
            text(&search.state)?.into(),
            "--epoch".into(),
            search.epoch.clone(),
            "--connection-file".into(),
            text(&search.connection)?.into(),
        ];
        add("search".into(), argv.clone(), vec![], Schedule::Minute);
        argv.truncate(7);
        if let Some(command) = argv.get_mut(2) {
            *command = "search-prepare-status".into();
        }
        add("search-status".into(), argv, vec![], Schedule::QuarterHour);
    }
    if let Some(notification) = &opts.notification {
        let config = opts.state.join(".operations/notifications/config.json");
        for (name, flag, schedule) in [
            (
                "notifications-monitor",
                "--notifications-monitor",
                Schedule::Minute,
            ),
            (
                "notifications-digest",
                "--notifications-digest",
                Schedule::Digest(notification.hour),
            ),
        ] {
            add(
                name.into(),
                vec![
                    text(&helper)?.into(),
                    "install-td-mta".into(),
                    flag.into(),
                    "--config-file".into(),
                    text(&config)?.into(),
                ],
                vec![],
                schedule,
            );
        }
        if let Some(job) = jobs.last_mut() {
            job.timezone = notification.timezone.clone();
        }
    }
    let mut names = BTreeSet::new();
    if jobs.iter().any(|j| !names.insert(&j.name)) {
        return Err("colliding operation names".into());
    }
    Ok(jobs)
}

fn files(job: &Job, state: &Path) -> Result<Vec<(String, String)>> {
    let seconds = match job.schedule {
        Schedule::WeeklyDrill { .. } => 4500,
        Schedule::Daily | Schedule::DailyPull | Schedule::Digest(_) => 2700,
        Schedule::QuarterHour | Schedule::Minute => 120,
    };
    let exec = job
        .argv
        .iter()
        .map(|v| systemd(v))
        .collect::<Vec<_>>()
        .join(" ");
    let env = job
        .environment
        .iter()
        .map(|v| format!("Environment={}\n", systemd(v)))
        .collect::<String>();
    let ordering = if matches!(job.schedule, Schedule::WeeklyDrill { pull_first: true }) {
        "Wants=td-mta-maint-backup-pull.service\nAfter=td-mta-maint-backup-pull.service\n"
    } else {
        ""
    };
    let unit = format!(
        "[Unit]\nDescription={}\n{ordering}\n[Service]\nType=oneshot\nExecStart=:{exec}\n{env}UMask=0077\nTimeoutStartSec={seconds}\nNoNewPrivileges=yes\nPrivateTmp=yes\n",
        job.name
    );
    let digest_calendar = if let Schedule::Digest(hour) = job.schedule {
        format!(
            "OnCalendar=*-*-* {hour:02}:00:00{}\nPersistent=true\n",
            job.timezone
                .as_ref()
                .map(|v| format!(" {v}"))
                .unwrap_or_default()
        )
    } else {
        String::new()
    };
    let schedule = match job.schedule {
        Schedule::Digest(_) => &digest_calendar,
        Schedule::WeeklyDrill { .. } => "OnCalendar=Sun *-*-* 05:15:00\nPersistent=true\n",
        Schedule::DailyPull => {
            "OnCalendar=Mon..Sat *-*-* 04:15:00\nOnCalendar=Sun *-*-* 05:15:00\nPersistent=true\n"
        }
        Schedule::Daily => "OnCalendar=*-*-* 03:15:00\nPersistent=true\n",
        Schedule::QuarterHour => "OnBootSec=2min\nOnUnitActiveSec=15min\n",
        Schedule::Minute => "OnBootSec=2min\nOnUnitActiveSec=1min\n",
    };
    let timer = format!(
        "[Unit]\nDescription=Schedule {}\n\n[Timer]\n{schedule}Unit={}.service\n\n[Install]\nWantedBy=timers.target\n",
        job.name, job.name
    );
    let digest_event = if let Schedule::Digest(hour) = job.schedule {
        format!("(calendar-event #:hours '({hour}) #:minutes '(0))")
    } else {
        String::new()
    };
    let event = match job.schedule {
        Schedule::Digest(_) => &digest_event,
        Schedule::WeeklyDrill { .. } => "(calendar-event #:days-of-week '(sunday) #:hours '(5) #:minutes '(15))",
        Schedule::DailyPull => "(calendar-event #:hours '(4) #:minutes '(15))",
        Schedule::Daily => "(calendar-event #:hours '(3) #:minutes '(15))",
        Schedule::QuarterHour => "(calendar-event #:minutes '(0 15 30 45))",
        Schedule::Minute => {
            "(calendar-event #:minutes '(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59))"
        }
    };
    let argv = job
        .argv
        .iter()
        .map(|v| scheme(v))
        .collect::<Vec<_>>()
        .join(" ");
    let env = job
        .environment
        .iter()
        .map(|v| scheme(v))
        .collect::<Vec<_>>()
        .join(" ");
    let keep = job
        .environment
        .iter()
        .filter_map(|value| value.split_once('='))
        .map(|(key, _)| {
            format!(
                "(not (string-prefix? {} entry))",
                scheme(&format!("{key}="))
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let log = state.join(".operations").join(format!("{}.log", job.name));
    let scm = format!(
        "(use-modules (shepherd service) (shepherd service timer))\n(register-services\n (service '({name})\n  #:documentation \"Scheduled mail operation; inspect timer past runs and logs.\"\n  #:respawn? #f\n  #:start (make-timer-constructor\n    {event}\n    (command '({argv})\n      #:directory {state}\n      #:environment-variables (append '({env}) (filter (lambda (entry) (and {keep})) (default-environment-variables))))\n    #:log-file {log}\n    #:max-duration {seconds}\n    #:termination-signal SIGKILL\n    #:wait-for-termination? #t)\n  #:stop (make-timer-destructor)\n  #:actions (list timer-trigger-action)))\n",
        name = job.name,
        state = scheme(text(state)?),
        log = scheme(text(&log)?)
    );
    Ok(vec![
        (format!("{}.service", job.name), unit),
        (format!("{}.timer", job.name), timer),
        (format!("{}.scm", job.name), scm),
    ])
}

fn notification_config(opts: &Options, jobs: &[Job]) -> Result<Option<Json>> {
    let Some(notification) = &opts.notification else {
        return Ok(None);
    };
    let mut checks = Vec::new();
    let mut check = |name: String, kind: &str, argv: Vec<String>| {
        checks.push(Json::Obj(vec![
            ("name".into(), Json::Str(name)),
            ("kind".into(), Json::Str(kind.into())),
            (
                "argv".into(),
                Json::Arr(argv.into_iter().map(Json::Str).collect()),
            ),
        ]));
    };
    for job in jobs {
        let name = job
            .name
            .strip_prefix("td-mta-maint-")
            .ok_or("invalid operation name")?;
        if name.starts_with("mail-")
            || name.starts_with("acme-")
            || name.starts_with("tls-")
            || ["backup-status", "search-status", "scrub-status"].contains(&name)
        {
            check(name.into(), "status", job.argv.clone());
        }
    }
    for profile in &opts.mail {
        check(
            format!("performance-{profile}"),
            "performance",
            vec![
                text(&opts.prefix.join("bin/td-mta"))?.into(),
                "performance-status".into(),
                "--runtime".into(),
                text(&opts.state.join(profile).join("runtime"))?.into(),
            ],
        );
    }
    if let Some(df) = &notification.df {
        check(
            "disk".into(),
            "disk",
            vec![text(df)?.into(), "-Pk".into(), text(&opts.state)?.into()],
        );
    }
    check(
        "capacity".into(),
        "capacity",
        vec![
            text(&opts.prefix.join("bin/td-mta"))?.into(),
            "postgres".into(),
            "capacity-inspect".into(),
            "--connection-file".into(),
            text(&notification.connection)?.into(),
            "--epoch".into(),
            notification.epoch.clone(),
        ],
    );
    if let Some(backup) = &opts.backup {
        let bounds = backup.bounds.unwrap_or(Bounds::DEFAULT);
        for item in &mut checks {
            if item.get("name").and_then(Json::as_str) == Some("backup-status") {
                let Json::Obj(fields) = item else {
                    return Err("invalid backup check".into());
                };
                fields.retain(|(name, _)| name != "kind");
                fields.extend([
                    ("kind".into(), Json::Str("backup".into())),
                    ("retain".into(), Json::Num(bounds.retain.to_string())),
                    (
                        "total_bytes".into(),
                        Json::Num(bounds.total_bytes.to_string()),
                    ),
                    (
                        "archive_bytes".into(),
                        Json::Num(bounds.archive_bytes.to_string()),
                    ),
                ]);
            }
        }
    }
    let mut fields = vec![("checks".into(), Json::Arr(checks))];
    for (name, value) in [
        ("mta", text(&opts.prefix.join("bin/td-mta"))?.to_string()),
        (
            "state",
            text(&opts.state.join(".operations/notifications"))?.to_string(),
        ),
        ("connection", text(&notification.connection)?.to_string()),
        ("epoch", notification.epoch.clone()),
        ("recipient", notification.recipient.clone()),
        ("smtp", notification.smtp.clone()),
    ] {
        fields.push((name.into(), Json::Str(value)));
    }
    let config = Json::Obj(fields);
    super::install_mta_notifications::validate(&config)?;
    Ok(Some(config))
}

fn install(opts: &Options) -> Result<()> {
    if let Some(drill) = &opts.drill {
        super::restore_layout::qualify(
            &drill.tools.bin,
            drill.tools.library.as_ref().map(|path| path.as_os_str()),
        )?;
    }
    let jobs = jobs(opts)?;
    let notification = notification_config(opts, &jobs)?;
    let config_path = opts.state.join(".operations/notifications/config.json");
    if let Some(config) = &notification {
        if config_path.exists()
            && fs::read_to_string(&config_path).map_err(|e| e.to_string())? != config.to_canonical()
        {
            return Err("notification configuration differs; retain or remove explicitly".into());
        }
    }
    let services = opts.prefix.join("share/td-mta/services");
    let mut output = Vec::new();
    for job in &jobs {
        output.extend(files(job, &opts.state)?);
    }
    // Refuse every conflicting definition before writing any of this batch.
    for (name, content) in &output {
        let path = services.join(name);
        match fs::read(&path) {
            Ok(old) if old == content.as_bytes() => (),
            Ok(_) => {
                return Err(format!(
                    "{} differs; retain or remove explicitly",
                    path.display()
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
    }
    if let Some(scrub) = &opts.scrub {
        directory(&scrub.state, 0o700)?;
    }
    if let Some(search) = &opts.search {
        directory(&search.state, 0o700)?;
    }
    directory(&opts.state, 0o700)?;
    directory(&opts.state.join(".operations"), 0o700)?;
    directory(&services, 0o700)?;
    if let Some(config) = notification {
        directory(config_path.parent().ok_or("missing config parent")?, 0o700)?;
        if !config_path.exists() {
            write_new(&config_path, config.to_canonical().as_bytes())?;
        }
    }
    if opts.backup.is_some() || opts.drill.is_some() || opts.notification.is_some() {
        // A stable command path keeps schedules independent of the source checkout.
        let releases = opts.prefix.join("lib/td-mta/host-tools");
        directory(&releases, 0o755)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let release = releases.join(format!("operator-{stamp}-{}", std::process::id()));
        fs::create_dir(&release).map_err(|e| e.to_string())?;
        fs::set_permissions(&release, fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        let binary = release.join("td-builder");
        fs::copy(std::env::current_exe().map_err(|e| e.to_string())?, &binary)
            .map_err(|e| e.to_string())?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        let bin = opts.prefix.join("bin");
        directory(&bin, 0o755)?;
        let stage = bin.join(format!(".td-mta-operator-{}", std::process::id()));
        std::os::unix::fs::symlink(&binary, &stage).map_err(|e| e.to_string())?;
        if let Err(e) = fs::rename(&stage, bin.join("td-mta-operator")) {
            let _ = fs::remove_file(stage);
            return Err(format!("publish operator helper: {e}"));
        }
    }
    for (name, content) in output {
        let path = services.join(name);
        if !path.exists() {
            write_new(&path, content.as_bytes())?;
        }
    }
    for job in jobs {
        println!(
            "systemd: systemctl --user link {}",
            shell(&services.join(format!("{}.service", job.name)))
        );
        println!(
            "systemd: systemctl --user enable --now {}",
            shell(&services.join(format!("{}.timer", job.name)))
        );
        println!(
            "Shepherd: herd load root {} ; herd start {}",
            shell(&services.join(format!("{}.scm", job.name))),
            job.name
        );
    }
    println!(
        "No services started. Inspect each job's last result; active timers alone do not prove success. Off-host copies and external failure notification remain explicit. Local notification SMTP is not an independent uptime alert."
    );
    Ok(())
}

pub(super) fn configure(args: &[String], home: &str) -> Result<()> {
    install(&options(args, home)?)
}

pub(super) fn run(args: &[String]) -> ExitCode {
    if args == ["--help"] {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let result = (|| {
        let home = std::env::var("HOME").map_err(|_| "HOME must be an absolute UTF-8 path")?;
        configure(args, &home)
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("install-td-mta operations: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn args(input: &[&str]) -> Vec<String> {
        input.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn options_require_complete_groups_and_bound_repeated_values() {
        for input in [
            vec!["--state-dir", "/state"],
            vec!["--state-dir", "/state", "--backup-service", "mail"],
            vec!["--state-dir", "/state", "--certificate-profile", "public"],
            vec![
                "--state-dir",
                "/state",
                "--mail-profile",
                "front",
                "--mail-profile",
                "front",
            ],
            vec!["--state-dir", "/state", "--tls-certificate", "/ca"],
        ] {
            assert!(options(&args(&input), "/home/a").is_err());
        }
    }
    #[test]
    fn independent_jobs_quote_arguments_and_never_start_mail() {
        let opts = options(
            &args(&[
                "--state-dir",
                "/a %u $x/quote\"",
                "--mail-profile",
                "frontend",
                "--certificate-profile",
                "public",
                "--connection-file",
                "/private/connection",
            ]),
            "/home/a",
        )
        .unwrap();
        let jobs = jobs(&opts).unwrap();
        assert_eq!(jobs.len(), 2);
        let generated = files(&jobs[0], &opts.state).unwrap();
        assert!(generated[0]
            .1
            .contains("/a %%u $x/quote\\\"/frontend/runtime"));
        assert!(generated[0].1.contains("\"--require-ready\""));
        assert!(!generated[0].1.contains("Requires="));
        assert!(generated[1].1.contains("OnUnitActiveSec=15min"));
        assert!(generated[2].1.contains("#:wait-for-termination? #t"));
        assert!(generated[2]
            .1
            .contains("/a %u $x/quote\\\"/frontend/runtime"));
    }
    #[test]
    fn backup_has_default_limits_and_explicit_tool_environment() {
        let opts = options(
            &args(&[
                "--state-dir",
                "/state",
                "--backup-service-file",
                "/private/service",
                "--backup-service",
                "mail",
                "--backup-destination",
                "/backup",
                "--postgres-bin",
                "/tools %u/bin",
                "--tool-library-dir",
                "/tools/lib",
            ]),
            "/home/a",
        )
        .unwrap();
        let jobs = jobs(&opts).unwrap();
        assert_eq!(jobs.len(), 2);
        let backup = files(&jobs[0], &opts.state).unwrap();
        assert!(backup[0].1.contains("Environment=\"PATH=/tools %%u/bin\""));
        assert!(backup[0].1.contains("TimeoutStartSec=2700"));
        assert!(backup[1].1.contains("Persistent=true"));
        assert!(backup[2]
            .1
            .contains("(not (string-prefix? \"PATH=\" entry))"));
        assert!(backup[2]
            .1
            .contains("(not (string-prefix? \"LD_LIBRARY_PATH=\" entry))"));
        assert!(backup[2].1.contains("#:hours '(3) #:minutes '(15)"));
        assert_eq!(jobs[1].argv.last().unwrap(), "172800");
        assert!(!jobs[0].argv.iter().any(|a| a == "--max-archive-bytes"));
    }

    #[test]
    fn weekly_drill_can_run_on_a_recovery_host_or_beside_a_backup() {
        let mut input = args(&[
            "--state-dir",
            "/state",
            "--prefix",
            "/operator",
            "--backup-destination",
            "/offline/archives",
            "--backup-drill-scratch-directory",
            "/scratch/restore space",
            "--postgres-bin",
            "/tools/bin",
            "--tool-library-dir",
            "/tools/lib",
        ]);
        let opts = options(&input, "/home/a").unwrap();
        let generated = jobs(&opts).unwrap();
        assert_eq!(generated.len(), 1);
        let drill = generated.first().unwrap();
        assert_eq!(drill.name, "td-mta-maint-backup-drill");
        assert_eq!(
            drill.argv,
            [
                "/operator/bin/td-mta-operator",
                "install-td-mta",
                "--restore-drill",
                "--destination",
                "/offline/archives",
                "--scratch-directory",
                "/scratch/restore space"
            ]
        );
        assert_eq!(
            drill.environment,
            ["PATH=/tools/bin", "LD_LIBRARY_PATH=/tools/lib"]
        );
        let generated = files(drill, &opts.state).unwrap();
        assert!(generated[0].1.contains("TimeoutStartSec=4500"));
        assert!(!generated[0].1.contains("backup-pull.service"));
        assert!(generated[0]
            .1
            .contains("\"--scratch-directory\" \"/scratch/restore space\""));
        assert!(generated[1]
            .1
            .contains("OnCalendar=Sun *-*-* 05:15:00\nPersistent=true"));
        assert!(generated[2]
            .1
            .contains("#:days-of-week '(sunday) #:hours '(5) #:minutes '(15)"));
        assert!(generated[2].1.contains("#:max-duration 4500"));
        assert!(generated[2].1.contains("#:wait-for-termination? #t"));
        let mut incomplete = input.clone();
        incomplete.extend(args(&["--backup-retain", "3"]));
        assert!(options(&incomplete, "/home/a").is_err());
        for alias in ["/offline/archives", "/offline/archives/nested", "/offline"] {
            let mut overlap = input.clone();
            *overlap
                .iter_mut()
                .find(|v| *v == "/scratch/restore space")
                .unwrap() = alias.into();
            assert!(options(&overlap, "/home/a").is_err());
        }
        input.extend(args(&[
            "--backup-service",
            "mail",
            "--backup-service-file",
            "/private/service",
        ]));
        let opts = options(&input, "/home/a").unwrap();
        let generated = jobs(&opts).unwrap();
        assert_eq!(
            generated
                .iter()
                .map(|job| job.name.as_str())
                .collect::<Vec<_>>(),
            [
                "td-mta-maint-backup",
                "td-mta-maint-backup-status",
                "td-mta-maint-backup-drill"
            ]
        );
        assert!(generated[2]
            .argv
            .iter()
            .all(|arg| arg != "/private/service"));
    }

    #[test]
    fn recovery_pull_precedes_the_drill_and_inherits_the_ssh_environment() {
        let mut input = args(&[
            "--state-dir",
            "/state",
            "--prefix",
            "/operator",
            "--backup-destination",
            "/archives",
            "--postgres-bin",
            "/pg17/bin",
            "--tool-library-dir",
            "/pg17/lib",
            "--backup-pull-host",
            "mx-recovery",
        ]);
        assert!(options(&input, "/home/a").is_err());
        input.extend(args(&["--backup-drill-scratch-directory", "/scratch"]));
        let opts = options(&input, "/home/a").unwrap();
        let jobs_default = jobs(&opts).unwrap();
        assert_eq!(jobs_default.len(), 3);
        let pull = jobs_default.first().unwrap();
        assert_eq!(pull.name, "td-mta-maint-backup-pull");
        assert_eq!(
            pull.argv,
            [
                "/operator/bin/td-mta-operator",
                "install-td-mta",
                "--pull-backup",
                "--host",
                "mx-recovery",
                "--destination",
                "/archives",
            ]
        );
        assert!(pull.environment.is_empty());
        let status = &jobs_default[1];
        assert_eq!(status.name, "td-mta-maint-backup-status");
        assert_eq!(
            status.argv,
            [
                "/operator/bin/td-mta-operator",
                "install-td-mta",
                "--backup-status",
                "--destination",
                "/archives",
                "--maximum-age-seconds",
                "172800",
            ]
        );
        assert!(status.environment.is_empty());
        assert!(matches!(status.schedule, Schedule::QuarterHour));
        let drill = &jobs_default[2];
        assert_eq!(drill.name, "td-mta-maint-backup-drill");
        let weekly = files(drill, &opts.state).unwrap();
        assert!(weekly[0].1.contains(
            "Wants=td-mta-maint-backup-pull.service\nAfter=td-mta-maint-backup-pull.service\n"
        ));
        assert!(!weekly[0].1.contains("Requires="));
        assert!(weekly[1].1.contains("OnCalendar=Sun *-*-* 05:15:00"));
        assert!(!weekly[2].1.contains("backup-pull"));
        for (flag, value) in [
            ("--backup-service-file", "/private/service"),
            ("--backup-service", "mail"),
            ("--backup-max-total-bytes", "1000"),
            ("--backup-retain", "4"),
            ("--backup-max-archive-bytes", "0"),
            ("--backup-max-archive-bytes", "1099511627777"),
        ] {
            let mut conflict = input.clone();
            conflict.extend(args(&[flag, value]));
            assert!(options(&conflict, "/home/a").is_err(), "{flag} {value}");
        }
        let mut invalid = input.clone();
        *invalid.iter_mut().find(|a| *a == "mx-recovery").unwrap() = "-oCommand".into();
        assert!(options(&invalid, "/home/a").is_err());

        input.extend(args(&["--backup-max-archive-bytes", "4294967296"]));
        let opts = options(&input, "/home/a").unwrap();
        let generated = files(&jobs(&opts).unwrap()[0], &opts.state).unwrap();
        for index in [0, 2] {
            let text = &generated[index].1;
            assert!(text.contains("\"--pull-backup\" \"--host\" \"mx-recovery\""));
            assert!(text.contains("\"--max-archive-bytes\" \"4294967296\""));
            assert!(!text.contains("/pg17/"));
        }
        assert!(generated[1].1.contains(
            "OnCalendar=Mon..Sat *-*-* 04:15:00\nOnCalendar=Sun *-*-* 05:15:00\nPersistent=true"
        ));
        assert!(generated[2].1.contains("#:hours '(4) #:minutes '(15)"));
        assert!(generated[2].1.contains("#:wait-for-termination? #t"));
    }

    #[test]
    fn backup_overrides_share_runtime_validation_and_reach_both_supervisors() {
        let base = args(&[
            "--state-dir",
            "/state",
            "--backup-service-file",
            "/private/service",
            "--backup-service",
            "mail",
            "--backup-destination",
            "/backup",
            "--postgres-bin",
            "/tools",
        ]);
        let mut custom = base.clone();
        custom.extend(args(&[
            "--backup-max-archive-bytes",
            "1073741824",
            "--backup-max-total-bytes",
            "4294967296",
            "--backup-retain",
            "7",
        ]));
        let opts = options(&custom, "/home/a").unwrap();
        let jobs = jobs(&opts).unwrap();
        let backup = jobs.first().unwrap();
        for (flag, value) in [
            ("--max-archive-bytes", "1073741824"),
            ("--max-total-bytes", "4294967296"),
            ("--retain", "7"),
        ] {
            assert!(backup
                .argv
                .windows(2)
                .any(|pair| pair.first().is_some_and(|v| v == flag)
                    && pair.get(1).is_some_and(|v| v == value)));
            let generated = files(backup, &opts.state).unwrap();
            for index in [0, 2] {
                let text = &generated.get(index).unwrap().1;
                assert!(text.contains(&format!("\"{flag}\" \"{value}\"")), "{text}");
            }
        }
        for (flag, value) in [
            ("--backup-retain", "0"),
            ("--backup-retain", "1001"),
            ("--backup-retain", "-1"),
            ("--backup-retain", "invalid"),
            ("--backup-max-archive-bytes", "0"),
            ("--backup-max-archive-bytes", "1099511627777"),
            ("--backup-max-total-bytes", "2147483648"),
        ] {
            let mut invalid = base.clone();
            invalid.extend(args(&[flag, value]));
            assert!(options(&invalid, "/home/a").is_err());
        }
        let mut duplicate = custom;
        duplicate.extend(args(&["--backup-retain", "7"]));
        assert!(options(&duplicate, "/home/a").is_err());
        assert!(options(
            &args(&[
                "--state-dir",
                "/state",
                "--mail-profile",
                "front",
                "--backup-retain",
                "7"
            ]),
            "/home/a"
        )
        .is_err());
    }

    #[test]
    fn postgres_tls_renewal_options_are_gone() {
        for flag in [
            "--postgres-tls-dir",
            "--postgres-data-dir",
            "--postgres-port",
        ] {
            let error = options(
                &args(&[
                    "--state-dir",
                    "/state",
                    "--mail-profile",
                    "frontend",
                    flag,
                    "/x",
                ]),
                "/home/a",
            )
            .err();
            assert_eq!(
                error.as_deref(),
                Some(format!("unknown operations option: {flag}").as_str())
            );
        }
        // An OpenSSL path no longer stands for renewal: it needs certificates.
        assert!(options(
            &args(&[
                "--state-dir",
                "/state",
                "--mail-profile",
                "front",
                "--openssl-bin",
                "/openssl"
            ]),
            "/home/a"
        )
        .is_err());
    }

    #[test]
    fn unsupported_drill_package_refuses_installation_before_creating_paths() {
        let tmp = crate::test_temp::TempDir::new("mail-unsupported-postgres").unwrap();
        let prefix = tmp.path().join("prefix");
        let state = tmp.path().join("state");
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("initdb"), "").unwrap();
        let opts = options(
            &args(&[
                "--prefix",
                prefix.to_str().unwrap(),
                "--state-dir",
                state.to_str().unwrap(),
                "--backup-drill-scratch-directory",
                "/restore/scratch",
                "--backup-destination",
                "/restore/backups",
                "--postgres-bin",
                bin.to_str().unwrap(),
            ]),
            "/home/a",
        )
        .unwrap();
        assert!(install(&opts)
            .unwrap_err()
            .contains("unsupported PostgreSQL restore package"));
        assert!(!prefix.exists());
        assert!(!state.exists());
    }

    #[test]
    fn generation_preserves_edited_definitions_before_writing_batch() {
        let tmp = crate::test_temp::TempDir::new("mail-operations").unwrap();
        let prefix = tmp.path().join("prefix");
        let state = tmp.path().join("state");
        let opts = options(
            &args(&[
                "--prefix",
                prefix.to_str().unwrap(),
                "--state-dir",
                state.to_str().unwrap(),
                "--mail-profile",
                "frontend",
            ]),
            "/home/a",
        )
        .unwrap();
        install(&opts).unwrap();
        let services = prefix.join("share/td-mta/services");
        let unit = services.join("td-mta-maint-mail-frontend.service");
        fs::write(&unit, "operator edit").unwrap();
        fs::remove_file(services.join("td-mta-maint-mail-frontend.timer")).unwrap();
        assert!(install(&opts).is_err());
        assert_eq!(fs::read_to_string(unit).unwrap(), "operator edit");
        assert!(!services.join("td-mta-maint-mail-frontend.timer").exists());
    }
    #[test]
    fn search_has_separate_worker_and_credential_free_status() {
        let base = args(&[
            "--state-dir",
            "/state",
            "--search-connection-file",
            "/private/owner",
            "--search-state-dir",
            "/private/search",
            "--search-epoch",
            "01010101010101010101010101010101",
        ]);
        let opts = options(&base, "/home/a").unwrap();
        let jobs = jobs(&opts).unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs[0].argv.iter().any(|v| v == "search-prepare"));
        assert!(jobs[1].argv.iter().any(|v| v == "search-prepare-status"));
        assert!(!jobs[1].argv.iter().any(|v| v == "--connection-file"));
        let worker = files(&jobs[0], &opts.state).unwrap();
        assert!(worker[1].1.contains("OnUnitActiveSec=1min"));
        assert!(worker[2].1.contains("#:minutes '(0 1 2 3"));
        assert!(files(&jobs[1], &opts.state).unwrap()[1]
            .1
            .contains("OnUnitActiveSec=15min"));
        for omitted in 1..4 {
            let input = base
                .chunks_exact(2)
                .enumerate()
                .filter(|(i, _)| *i != omitted)
                .flat_map(|(_, pair)| pair.iter().cloned())
                .collect::<Vec<_>>();
            assert!(options(&input, "/home/a").is_err());
        }
        let mut conflict = base.clone();
        conflict.extend(args(&[
            "--scrub-connection-file",
            "/private/owner",
            "--scrub-state-dir",
            "/private/search",
            "--scrub-epoch",
            "01010101010101010101010101010101",
        ]));
        assert!(options(&conflict, "/home/a").is_err());
    }
    #[test]
    fn scrub_has_separate_bounded_worker_and_credential_free_status() {
        let base = args(&[
            "--state-dir",
            "/state",
            "--scrub-connection-file",
            "/private/owner",
            "--scrub-state-dir",
            "/private/scrub",
            "--scrub-epoch",
            "01010101010101010101010101010101",
        ]);
        let opts = options(&base, "/home/a").unwrap();
        let jobs = jobs(&opts).unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs[0].argv.iter().any(|v| v == "blob-scrub"));
        assert!(jobs[1].argv.iter().any(|v| v == "blob-scrub-status"));
        assert!(!jobs[1].argv.iter().any(|v| v == "--connection-file"));
        assert_eq!(
            jobs[1].argv.last().unwrap(),
            "01010101010101010101010101010101"
        );
        let worker = files(&jobs[0], &opts.state).unwrap();
        assert!(worker[1].1.contains("OnUnitActiveSec=1min"));
        assert!(worker[2].1.contains("#:minutes '(0 1 2 3"));
        assert!(files(&jobs[1], &opts.state).unwrap()[1]
            .1
            .contains("OnUnitActiveSec=15min"));
        for omitted in 1..4 {
            let input = base
                .chunks_exact(2)
                .enumerate()
                .filter(|(i, _)| *i != omitted)
                .flat_map(|(_, pair)| pair.iter().cloned())
                .collect::<Vec<_>>();
            assert!(options(&input, "/home/a").is_err());
        }
    }
    #[test]
    fn notifications_generate_bounded_native_jobs_and_local_delivery_configuration() {
        let input = args(&[
            "--state-dir",
            "/state",
            "--prefix",
            "/prefix",
            "--mail-profile",
            "receive",
            "--notify-recipient",
            "timmy@example.test",
            "--notify-connection-file",
            "/private/owner",
            "--notify-epoch",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--notify-systemd-timezone",
            "America/Los_Angeles",
            "--notify-df-bin",
            "/usr/bin/df",
        ]);
        let opts = options(&input, "/home/test").unwrap();
        let jobs = jobs(&opts).unwrap();
        let config = notification_config(&opts, &jobs).unwrap().unwrap();
        assert_eq!(
            config.get("smtp").and_then(Json::as_str),
            Some("127.0.0.1:25")
        );
        let job = jobs
            .iter()
            .find(|job| job.name == "td-mta-maint-notifications-digest")
            .unwrap();
        let output = files(job, &opts.state).unwrap();
        let timer = output
            .iter()
            .find(|(name, _)| name.ends_with(".timer"))
            .unwrap();
        assert!(timer
            .1
            .contains("OnCalendar=*-*-* 08:00:00 America/Los_Angeles"));
        assert!(output
            .iter()
            .any(|(name, body)| name.ends_with(".scm") && body.contains("#:hours '(8)")));
        assert!(!output.iter().any(|(_, body)| body.contains("python")));
        assert!(config.to_canonical().contains("performance-status"));
        for extra in [["--notify-smtp", "192.0.2.1:25"], ["--notify-hour", "24"]] {
            let mut modified = input.clone();
            modified.extend(extra.map(String::from));
            assert!(options(&modified, "/home/test")
                .and_then(|o| notification_config(&o, &jobs))
                .is_err());
        }
    }
}
