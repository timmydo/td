//! Explicit host schedules for existing mail operations; no service activation.
use super::install_mta_backup::Bounds;
use super::{absolute, directory, profile_name, scheme, shell, systemd, text, write_new, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HELP: &str = "Usage: ./install-td-mta --operations-services --state-dir PATH [--prefix PATH]
  [--mail-profile NAME ...]
  [--scrub-connection-file OWNER_PATH --scrub-epoch HEX --scrub-state-dir PATH]
  [--certificate-profile NAME ... --connection-file PATH]
  [--tls-certificate PATH ... --openssl-bin PATH]
  [--backup-service-file PATH --backup-service NAME --backup-destination PATH
   --postgres-bin PATH [--tool-library-dir PATH]
   [--backup-max-archive-bytes N] [--backup-max-total-bytes N] [--backup-retain N]]
  [--postgres-tls-dir PATH --postgres-data-dir PATH --postgres-port N
   --postgres-bin PATH --openssl-bin PATH [--tool-library-dir PATH]
   [--replica-root PATH --replica-https-port N]]

Generates independent systemd timers and Shepherd timed services, without starting
anything. Body scrubbing runs one bounded page per minute, starts a new cycle
a day after completion, and has a separate freshness check. Mail, certificate and backup freshness checks run every 15 minutes;
backups and private PostgreSQL leaf renewal run daily at 03:15 in the supervisor's local timezone. Certificate checks
require 30 days remaining; backups must be at most two days old. PostgreSQL 17
and protected backup credentials must already be provisioned. Backup defaults are
1 GiB/archive, 8 GiB/directory and 30 archives; override with --backup-* bounds. See td-mta/DEPLOYMENT.md.
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
    scrub: Option<Scrub>,
    renewal: Option<Renewal>,
}
struct Scrub {
    connection: PathBuf,
    epoch: String,
    state: PathBuf,
}
struct Renewal {
    argv: Vec<String>,
    environment: Vec<String>,
}
struct Backup {
    service_file: PathBuf,
    service: String,
    destination: PathBuf,
    postgres_bin: PathBuf,
    library: Option<PathBuf>,
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
            | "--backup-service-file"
            | "--backup-service"
            | "--backup-destination"
            | "--backup-max-archive-bytes"
            | "--backup-max-total-bytes"
            | "--backup-retain"
            | "--postgres-bin"
            | "--tool-library-dir"
            | "--postgres-tls-dir"
            | "--postgres-data-dir"
            | "--postgres-port"
            | "--replica-root"
            | "--replica-https-port" => {
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
    let backup_keys = [
        "--backup-service-file",
        "--backup-service",
        "--backup-destination",
        "--backup-max-archive-bytes",
        "--backup-max-total-bytes",
        "--backup-retain",
    ];
    let backup = if backup_keys.iter().any(|k| values.contains_key(k)) {
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
        let postgres_bin = required("--postgres-bin")?;
        let library = path("--tool-library-dir")?;
        if text(&postgres_bin)?.contains(':')
            || library
                .as_ref()
                .is_some_and(|p| p.to_string_lossy().contains(':'))
        {
            return Err("tool directories cannot contain PATH separators".into());
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
            postgres_bin,
            library,
            bounds,
        })
    } else {
        None
    };
    let mut renewal = if [
        "--postgres-tls-dir",
        "--postgres-data-dir",
        "--postgres-port",
    ]
    .iter()
    .any(|key| values.contains_key(key))
    {
        let postgres = required("--postgres-bin")?;
        let openssl = required("--openssl-bin")?;
        let tls_dir = required("--postgres-tls-dir")?;
        let data = required("--postgres-data-dir")?;
        let port = values
            .get("--postgres-port")
            .ok_or("missing --postgres-port")?
            .parse::<u16>()
            .map_err(|_| "invalid PostgreSQL port")?;
        if port == 0 {
            return Err("PostgreSQL port must be nonzero".into());
        }
        let mut environment = vec![format!("PATH={}", text(&postgres)?)];
        if let Some(library) = path("--tool-library-dir")? {
            environment.push(format!("LD_LIBRARY_PATH={}", text(&library)?));
        }
        if environment.iter().any(|value| value.contains(':')) {
            return Err("tool directories cannot contain PATH separators".into());
        }
        Some(Renewal {
            argv: vec![
                "install-td-mta".into(),
                "--postgres-tls-renew".into(),
                "--tls-dir".into(),
                text(&tls_dir)?.into(),
                "--data-dir".into(),
                text(&data)?.into(),
                "--postgres-bin".into(),
                text(&postgres)?.into(),
                "--openssl-bin".into(),
                text(&openssl)?.into(),
                "--port".into(),
                port.to_string(),
            ],
            environment,
        })
    } else {
        None
    };
    if values.contains_key("--replica-root") || values.contains_key("--replica-https-port") {
        let root = required("--replica-root")?;
        let port = values
            .get("--replica-https-port")
            .ok_or("missing --replica-https-port")?
            .parse::<u16>()
            .map_err(|_| "invalid replica HTTPS port")?;
        if port == 0 {
            return Err("replica HTTPS port must be nonzero".into());
        }
        let renewal = renewal
            .as_mut()
            .ok_or("replica renewal requires PostgreSQL TLS settings")?;
        let prefix = path("--prefix")?.unwrap_or(absolute(home)?.join(".local"));
        renewal.argv.extend([
            "--replica-root".into(),
            text(&root)?.into(),
            "--mail-bin".into(),
            text(&prefix.join("bin/td-mta"))?.into(),
            "--https-port".into(),
            port.to_string(),
        ]);
    }
    if backup.is_none()
        && renewal.is_none()
        && ["--postgres-bin", "--tool-library-dir"]
            .iter()
            .any(|key| values.contains_key(key))
    {
        return Err("unused PostgreSQL tool settings".into());
    }
    let connection = path("--connection-file")?;
    let openssl = path("--openssl-bin")?;
    if certificates.is_empty() != connection.is_none()
        || (tls.is_empty() && renewal.is_none()) != openssl.is_none()
    {
        return Err("certificate profiles need --connection-file; TLS files need --openssl-bin (no unused options)".into());
    }
    if mail.is_empty()
        && certificates.is_empty()
        && tls.is_empty()
        && scrub.is_none()
        && backup.is_none()
        && renewal.is_none()
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
        scrub,
        renewal,
    })
}

#[derive(Clone, Copy)]
enum Schedule {
    Daily,
    QuarterHour,
    Minute,
}
struct Job {
    name: String,
    argv: Vec<String>,
    environment: Vec<String>,
    schedule: Schedule,
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
        let mut env = vec![format!("PATH={}", text(&backup.postgres_bin)?)];
        if let Some(library) = &backup.library {
            env.push(format!("LD_LIBRARY_PATH={}", text(library)?));
        }
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
        add("backup".into(), argv, env, Schedule::Daily);
        add(
            "backup-status".into(),
            vec![
                text(&helper)?.into(),
                "install-td-mta".into(),
                "--backup-status".into(),
                "--destination".into(),
                text(&backup.destination)?.into(),
                "--maximum-age-seconds".into(),
                "172800".into(),
            ],
            vec![],
            Schedule::QuarterHour,
        );
    }
    if let Some(renewal) = &opts.renewal {
        let mut argv = vec![text(&helper)?.into()];
        argv.extend(renewal.argv.iter().cloned());
        add(
            if renewal.argv.iter().any(|v| v == "--replica-root") {
                "replica-tls-renew".into()
            } else {
                "postgres-tls-renew".into()
            },
            argv,
            renewal.environment.clone(),
            Schedule::Daily,
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
    let mut names = BTreeSet::new();
    if jobs.iter().any(|j| !names.insert(&j.name)) {
        return Err("colliding operation names".into());
    }
    Ok(jobs)
}

fn files(job: &Job, state: &Path) -> Result<Vec<(String, String)>> {
    let seconds = if matches!(job.schedule, Schedule::Daily) {
        2700
    } else {
        120
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
    let unit = format!("[Unit]\nDescription={}\n\n[Service]\nType=oneshot\nExecStart=:{exec}\n{env}UMask=0077\nTimeoutStartSec={seconds}\nNoNewPrivileges=yes\nPrivateTmp=yes\n", job.name);
    let schedule = match job.schedule {
        Schedule::Daily => "OnCalendar=*-*-* 03:15:00\nPersistent=true\n",
        Schedule::QuarterHour => "OnBootSec=2min\nOnUnitActiveSec=15min\n",
        Schedule::Minute => "OnBootSec=2min\nOnUnitActiveSec=1min\n",
    };
    let timer = format!("[Unit]\nDescription=Schedule {}\n\n[Timer]\n{schedule}Unit={}.service\n\n[Install]\nWantedBy=timers.target\n", job.name, job.name);
    let event = match job.schedule {
 Schedule::Daily => "(calendar-event #:hours '(3) #:minutes '(15))",
 Schedule::QuarterHour => "(calendar-event #:minutes '(0 15 30 45))",
 Schedule::Minute => "(calendar-event #:minutes '(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59))",
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
    let scm = format!("(use-modules (shepherd service) (shepherd service timer))\n(register-services\n (service '({name})\n  #:documentation \"Scheduled mail operation; inspect timer past runs and logs.\"\n  #:respawn? #f\n  #:start (make-timer-constructor\n    {event}\n    (command '({argv})\n      #:directory {state}\n      #:environment-variables (append '({env}) (filter (lambda (entry) (and {keep})) (default-environment-variables))))\n    #:log-file {log}\n    #:max-duration {seconds}\n    #:termination-signal SIGKILL\n    #:wait-for-termination? #t)\n  #:stop (make-timer-destructor)\n  #:actions (list timer-trigger-action)))\n", name=job.name, state=scheme(text(state)?), log=scheme(text(&log)?));
    Ok(vec![
        (format!("{}.service", job.name), unit),
        (format!("{}.timer", job.name), timer),
        (format!("{}.scm", job.name), scm),
    ])
}

fn install(opts: &Options) -> Result<()> {
    let jobs = jobs(opts)?;
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
                ))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.to_string()),
        }
    }
    if let Some(scrub) = &opts.scrub {
        directory(&scrub.state, 0o700)?;
    }
    directory(&opts.state, 0o700)?;
    directory(&opts.state.join(".operations"), 0o700)?;
    directory(&services, 0o700)?;
    if opts.backup.is_some() || opts.renewal.is_some() {
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
    println!("No services started. Inspect each job's last result; active timers alone do not prove success. Off-host copies and failure notification remain explicit.");
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
    fn renewal_is_daily_without_forcing_issuance() {
        let opts = options(
            &args(&[
                "--state-dir",
                "/state",
                "--postgres-tls-dir",
                "/tls",
                "--postgres-data-dir",
                "/pg",
                "--postgres-port",
                "35432",
                "--postgres-bin",
                "/tools",
                "--openssl-bin",
                "/openssl",
            ]),
            "/home/a",
        )
        .unwrap();
        let jobs = jobs(&opts).unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(matches!(jobs[0].schedule, Schedule::Daily));
        assert!(jobs[0].argv.iter().any(|v| v == "--postgres-tls-renew"));
        assert!(!jobs[0].argv.iter().any(|v| v == "--force"));
        assert!(files(&jobs[0], &opts.state).unwrap()[1]
            .1
            .contains("OnCalendar=*-*-* 03:15:00"));
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
}
