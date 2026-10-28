//! Host installation, independent of the host's service manager.
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

type Result<T> = std::result::Result<T, String>;
#[path = "install_mta_backup.rs"]
mod install_mta_backup;
#[path = "install_mta_operations.rs"]
mod install_mta_operations;
#[path = "install_mta_setup.rs"]
mod install_mta_setup;
#[path = "install_mta_tls.rs"]
mod install_mta_tls;
const HELP: &str = "Usage: ./install-td-mta [--prefix PATH]
       ./install-td-mta --services-only --connection-file PATH --state-dir PATH
           --profile NAME [--profile NAME ...] [--prefix PATH]

Optional independent certificate worker (may replace --profile):
  --acme-profile NAME --acme-directory HTTPS-URL --acme-contact EMAIL
  --acme-name HOST [--acme-name HOST ...] --acme-bind IP:PORT --accept-acme-terms

Builds a native release for this host using the pinned mail dependencies.
Default prefix: $HOME/.local. Retains debug information and dependency notices.
Service configuration requires connection/state and a mail or certificate profile.
Generates both systemd user units and Shepherd definitions; does not enable them.
Database preparation and credentials remain explicit (td-mta/POSTGRES.md).
--services-only uses the already installed executable without rebuilding.
Use --setup-replica --help to provision a new read-only local account copy.
Use --setup-local --help to provision a new private local PostgreSQL account.
Use --postgres-tls-renew --help for local PostgreSQL leaf renewal.
Use --operations-services --help to generate backup and health schedules.
Use --backup --help for bounded host PostgreSQL logical backups.
Use --backup-status --help for read-only backup freshness checks.
";

#[derive(Debug)]
struct Options {
    prefix: PathBuf,
    connection: Option<PathBuf>,
    state: Option<PathBuf>,
    profiles: Vec<String>,
    services_only: bool,
    acme: Option<Acme>,
}

#[derive(Debug)]
struct Acme {
    profile: String,
    directory: String,
    contact: String,
    names: Vec<String>,
    bind: std::net::SocketAddr,
}

fn profile_name(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn absolute(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || path.components().any(|p| matches!(p, Component::ParentDir))
        || value.chars().any(|c| c.is_control())
    {
        return Err(
            "paths must be absolute, without control characters or parent traversal".into(),
        );
    }
    Ok(path)
}

fn options(args: &[String], home: &str) -> Result<Options> {
    let mut out = Options {
        prefix: absolute(home)?.join(".local"),
        connection: None,
        state: None,
        profiles: Vec::new(),
        services_only: false,
        acme: None,
    };
    let mut acme_values = std::collections::BTreeMap::new();
    let mut names = Vec::new();
    let mut terms = false;
    let mut args = args.iter();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(flag) = args.next() {
        if flag != "--profile" && flag != "--acme-name" && !seen.insert(flag) {
            return Err(format!("duplicate option: {flag}"));
        }
        if flag == "--services-only" {
            out.services_only = true;
            continue;
        }
        if flag == "--accept-acme-terms" {
            terms = true;
            continue;
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--prefix" => out.prefix = absolute(value)?,
            "--connection-file" => out.connection = Some(absolute(value)?),
            "--state-dir" => out.state = Some(absolute(value)?),
            "--profile" => {
                if !profile_name(value) || out.profiles.contains(value) {
                    return Err("profiles must start with a lowercase letter and contain only lowercase letters, digits, - or _ (unique, at most 64 bytes)".into());
                }
                out.profiles.push(value.clone());
            }
            "--acme-profile" | "--acme-directory" | "--acme-contact" | "--acme-bind" => {
                if value.is_empty()
                    || value.len() > 2048
                    || !value.bytes().all(|b| b.is_ascii_graphic())
                {
                    return Err(format!("{flag} requires a bounded nonempty ASCII value"));
                }
                acme_values.insert(flag.as_str(), value.clone());
            }
            "--acme-name" => {
                if names.len() >= 32
                    || names.contains(value)
                    || value.len() > 253
                    || value.split('.').any(|label| {
                        label.is_empty()
                            || label.len() > 63
                            || label.starts_with('-')
                            || label.ends_with('-')
                            || !label
                                .bytes()
                                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    })
                {
                    return Err("ACME names must be unique lowercase DNS names (at most 32)".into());
                }
                names.push(value.clone());
            }
            _ => return Err(format!("unknown option: {flag}")),
        }
    }
    if !acme_values.is_empty() || !names.is_empty() || terms {
        let required = |key| {
            acme_values
                .get(key)
                .cloned()
                .ok_or_else(|| format!("missing {key}"))
        };
        let profile = required("--acme-profile")?;
        let directory = required("--acme-directory")?;
        let contact = required("--acme-contact")?;
        let bind: std::net::SocketAddr = required("--acme-bind")?
            .parse()
            .map_err(|_| "ACME bind must be IP:PORT")?;
        if !profile_name(&profile) || out.profiles.contains(&format!("acme-{profile}")) {
            return Err("invalid ACME profile or colliding mail service name".into());
        }
        if !terms || names.is_empty() || bind.port() == 0 || !directory.starts_with("https://") {
            return Err("ACME needs --accept-acme-terms, --acme-name, HTTPS directory and a nonzero bind port".into());
        }
        out.acme = Some(Acme {
            profile,
            directory,
            contact,
            names,
            bind,
        });
    }
    let service = out.connection.is_some()
        || out.state.is_some()
        || !out.profiles.is_empty()
        || out.services_only
        || out.acme.is_some();
    if service
        && (out.connection.is_none()
            || out.state.is_none()
            || (out.profiles.is_empty() && out.acme.is_none()))
    {
        return Err(
            "service configuration needs --connection-file, --state-dir and --profile or --acme-profile".into(),
        );
    }
    Ok(out)
}

fn directory(path: &Path, mode: u32) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    if !fs::symlink_metadata(path)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err(format!("{} must be a real directory", path.display()));
    }
    if mode == 0o700 {
        let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if metadata.permissions().mode() & 0o777 != 0o700 {
            return Err(format!("{} must have mode 0700", path.display()));
        }
    }
    Ok(())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

// Service syntax has its own expansion rules, even inside quotes.
fn systemd(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}
fn shell(value: &Path) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "'\"'\"'"))
}
fn scheme(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}
fn text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or("installation paths must be UTF-8".into())
}

fn service_files(
    bin: &Path,
    connection: &Path,
    state: &Path,
    profile: &str,
) -> Result<(String, String)> {
    let runtime = state.join(profile).join("runtime");
    let scratch = state.join(profile).join("scratch");
    let argv = [
        text(bin)?,
        "serve",
        "--connection-file",
        text(connection)?,
        "--profile",
        profile,
        "--runtime",
        text(&runtime)?,
        "--scratch",
        text(&scratch)?,
    ];
    supervisor_files(
        &argv,
        &format!("td-mta-{profile}"),
        &state.join(profile).join("service.log"),
        120,
        "td-mta status",
    )
}

fn supervisor_files(
    argv: &[&str],
    name: &str,
    log: &Path,
    stop_seconds: u32,
    status: &str,
) -> Result<(String, String)> {
    let exec = argv
        .iter()
        .map(|s| systemd(s))
        .collect::<Vec<_>>()
        .join(" ");
    let description = name.replacen("td-mta-", "td-mta ", 1);
    let unit = format!("[Unit]\nDescription={description}\n\n[Service]\nType=exec\nExecStart=:{exec}\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec={stop_seconds}\nUMask=0077\n\n[Install]\nWantedBy=default.target\n");
    let command = argv.iter().map(|s| scheme(s)).collect::<Vec<_>>().join(" ");
    let scm = format!("(use-modules (shepherd service))\n(register-services\n (service '({name})\n  #:documentation \"{description}; check readiness with {status}.\"\n  #:respawn? #t\n  #:respawn-delay 5\n  #:start (make-forkexec-constructor\n           '({command})\n           #:log-file {}\n           #:file-creation-mask #o077)\n  #:stop (make-kill-destructor SIGTERM #:grace-period {stop_seconds})))\n", scheme(text(log)?));
    Ok((unit, scm))
}

fn acme_service_files(
    bin: &Path,
    connection: &Path,
    state: &Path,
    acme: &Acme,
) -> Result<(String, String)> {
    let bind = acme.bind.to_string();
    let mut argv = vec![
        text(bin)?,
        "acme",
        "--connection-file",
        text(connection)?,
        "--certificate-profile",
        &acme.profile,
        "--directory",
        &acme.directory,
        "--contact",
        &acme.contact,
        "--bind",
        &bind,
        "--accept-terms",
    ];
    for name in &acme.names {
        argv.extend(["--name", name]);
    }
    supervisor_files(
        &argv,
        &format!("td-mta-acme-{}", acme.profile),
        &state.join(".acme").join(&acme.profile).join("service.log"),
        180,
        "td-mta acme-status",
    )
}

fn retain_services(services: &Path, name: &str, files: (String, String)) -> Result<()> {
    for (suffix, content) in [("service", files.0), ("scm", files.1)] {
        let path = services.join(format!("{name}.{suffix}"));
        match fs::read(&path) {
            Ok(old) if old == content.as_bytes() => {}
            Ok(_) => {
                return Err(format!(
                    "{} differs; retain or remove it explicitly before regeneration",
                    path.display()
                ))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                write_new(&path, content.as_bytes())?
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    println!(
        "  systemd: systemctl --user enable --now {}",
        shell(&services.join(format!("{name}.service")))
    );
    println!(
        "  Shepherd: herd load root {} ; herd start {name}",
        shell(&services.join(format!("{name}.scm")))
    );
    Ok(())
}

fn install(root: &Path, opts: &Options) -> Result<()> {
    let bin_dir = opts.prefix.join("bin");
    let bin = bin_dir.join("td-mta");
    if !opts.services_only {
        crate::check_loop::warm_mail_sources(root)?;
        let built = crate::crypto_build::release(root)?;
        let executable = &built.executable;
        let releases = opts.prefix.join("lib/td-mta/releases");
        directory(&releases, 0o755)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let release = releases.join(format!("host-{stamp}-{}", std::process::id()));
        fs::create_dir(&release).map_err(|e| format!("create release: {e}"))?;
        let installed = release.join("td-mta");
        fs::copy(executable, &installed).map_err(|e| format!("install executable: {e}"))?;
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        crate::crypto_isolated::retain_notices(&built.vendor, &release.join("notices"), 0)?;
        for name in ["Cargo.lock", "Cargo.toml"] {
            fs::copy(root.join("td-mta").join(name), release.join(name))
                .map_err(|e| e.to_string())?;
        }
        fs::copy(
            root.join("td-crypto/DEPENDENCIES.md"),
            release.join("DEPENDENCIES.md"),
        )
        .map_err(|e| e.to_string())?;
        write_new(&release.join("BUILD-INPUTS"), built.receipt.as_bytes())?;
        directory(&bin_dir, 0o755)?;
        let staged = bin_dir.join(format!(".td-mta-{}", std::process::id()));
        std::os::unix::fs::symlink(&installed, &staged)
            .map_err(|e| format!("stage executable link: {e}"))?;
        if let Err(e) = fs::rename(&staged, &bin) {
            let _ = fs::remove_file(&staged);
            return Err(format!("publish executable: {e}"));
        }
        println!("Installed {} (previous releases retained)", bin.display());
    } else if !bin.is_file() {
        return Err(format!("{} is missing; install first", bin.display()));
    }
    if let (Some(connection), Some(state)) = (&opts.connection, &opts.state) {
        directory(state, 0o700)?;
        let services = opts.prefix.join("share/td-mta/services");
        directory(&services, 0o700)?;
        for profile in &opts.profiles {
            let instance = state.join(profile);
            directory(&instance, 0o700)?;
            for name in ["runtime", "scratch"] {
                directory(&instance.join(name), 0o700)?;
            }
            println!("Profile {profile}: {}", services.display());
            retain_services(
                &services,
                &format!("td-mta-{profile}"),
                service_files(&bin, connection, state, profile)?,
            )?;
            println!(
                "  readiness: {} status --json --require-ready --runtime {}",
                shell(&bin),
                shell(&instance.join("runtime"))
            );
        }
        if let Some(acme) = &opts.acme {
            directory(&state.join(".acme").join(&acme.profile), 0o700)?;
            println!(
                "Certificate worker {}: {}",
                acme.profile,
                services.display()
            );
            retain_services(
                &services,
                &format!("td-mta-acme-{}", acme.profile),
                acme_service_files(&bin, connection, state, acme)?,
            )?;
            println!("  certificate status: {} acme-status --connection-file {} --certificate-profile {}", shell(&bin), shell(connection), acme.profile);
            println!("Start this worker before importing managed mail profiles. Wait for ready certificate status, then import/check/select matching ACME configuration (td-mta/ACME.md). DNS and public port 80 must reach its listener; provision low-port access separately. The runtime validates the CA settings. Services were not started.");
        }
    }
    Ok(())
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    if args.first().is_some_and(|v| v == "--postgres-tls-renew") {
        return install_mta_tls::run(args.get(1..).unwrap_or_default());
    }
    if args.first().is_some_and(|v| v == "--operations-services") {
        return install_mta_operations::run(args.get(1..).unwrap_or_default());
    }
    if args.first().is_some_and(|v| v == "--backup-status") {
        return install_mta_backup::status(args.get(1..).unwrap_or_default());
    }
    if args.first().is_some_and(|v| v == "--backup") {
        return install_mta_backup::run(args.get(1..).unwrap_or_default());
    }
    if args.first().is_some_and(|v| v == "--setup-replica") {
        return install_mta_setup::run_replica(args.get(1..).unwrap_or_default());
    }
    if args.first().is_some_and(|v| v == "--setup-local") {
        return install_mta_setup::run(args.get(1..).unwrap_or_default());
    }
    if args == ["--help"] || args == ["-h"] {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    let result = (|| {
        let home = std::env::var("HOME").map_err(|_| "HOME must be an absolute UTF-8 path")?;
        let opts = options(args, &home)?;
        let root = std::env::current_dir().map_err(|e| e.to_string())?;
        install(&root, &opts)
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("install-td-mta: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn options_refuse_incomplete_or_injectable_profiles() {
        for args in [
            vec!["--profile", "x"],
            vec!["--services-only"],
            vec!["--profile", "a\nb"],
            vec!["--prefix", "/a/../b"],
        ] {
            assert!(options(
                &args.into_iter().map(String::from).collect::<Vec<_>>(),
                "/home/test"
            )
            .is_err());
        }
        for name in ["123", "Relay", "_relay", "-relay"] {
            let args = [
                "--services-only",
                "--connection-file",
                "/private/connection",
                "--state-dir",
                "/private/state",
                "--profile",
                name,
            ]
            .map(String::from);
            assert!(options(&args, "/home/test").is_err());
        }
        assert_eq!(shell(Path::new("/a ' $x")), "'/a '\"'\"' $x'");
        assert_eq!(
            options(&[], "/home/test").unwrap().prefix,
            Path::new("/home/test/.local")
        );
    }
    #[test]
    fn service_arguments_are_literal_and_profiles_are_isolated() {
        let (unit, scm) = service_files(
            Path::new("/home/a %u $x/td-mta"),
            Path::new("/private/connection"),
            Path::new("/state"),
            "relay",
        )
        .unwrap();
        assert!(unit.contains("\"/home/a %%u $x/td-mta\""));
        assert!(scm.contains("\"/home/a %u $x/td-mta\""));
        assert!(unit.contains("/state/relay/runtime"));
        assert!(unit.contains("/state/relay/scratch"));
        assert!(!unit.contains("User="));
        assert!(scm.contains("#:file-creation-mask #o077"));
    }
    fn acme_arguments() -> Vec<String> {
        [
            "--services-only",
            "--connection-file",
            "/private/connection",
            "--state-dir",
            "/private/state",
            "--acme-profile",
            "public",
            "--acme-directory",
            "https://ca.example/directory",
            "--acme-contact",
            "operator@example.org",
            "--acme-name",
            "mx.example.org",
            "--acme-bind",
            "0.0.0.0:80",
            "--accept-acme-terms",
        ]
        .map(String::from)
        .to_vec()
    }

    #[test]
    fn certificate_worker_can_bootstrap_without_mail_profiles() {
        let args = acme_arguments();
        let opts = options(&args, "/home/test").unwrap();
        assert!(opts.profiles.is_empty());
        let acme = opts.acme.unwrap();
        let (unit, scm) = acme_service_files(
            Path::new("/a %u $x/td-mta"),
            Path::new("/private/connection"),
            Path::new("/state"),
            &acme,
        )
        .unwrap();
        assert!(unit.contains("ExecStart=:\"/a %%u $x/td-mta\" \"acme\""));
        assert!(unit.contains("TimeoutStopSec=180"));
        assert!(unit.contains("\"--name\" \"mx.example.org\""));
        assert!(!unit.contains("--runtime"));
        assert!(scm.contains("/state/.acme/public/service.log"));
        assert!(scm.contains("#:grace-period 180"));
        assert!(scm.contains("#:respawn-delay 5"));
        assert!(options(&args[..args.len() - 1], "/home/test").is_err());
        for extra in [
            vec!["--acme-name", "mx.example.org"],
            vec!["--acme-name", "*.example.org"],
            vec!["--profile", "acme-public"],
            vec!["--accept-acme-terms"],
        ] {
            let mut invalid = args.clone();
            invalid.extend(extra.into_iter().map(String::from));
            assert!(options(&invalid, "/home/test").is_err());
        }
    }

    #[test]
    fn certificate_services_preserve_operator_edits() {
        let tmp = crate::test_temp::TempDir::new("install-acme").unwrap();
        let mut opts = options(&acme_arguments(), "/home/test").unwrap();
        opts.prefix = tmp.path().join("prefix");
        opts.state = Some(tmp.path().join("state"));
        directory(&opts.prefix.join("bin"), 0o755).unwrap();
        write_new(&opts.prefix.join("bin/td-mta"), b"fixture").unwrap();
        install(tmp.path(), &opts).unwrap();
        install(tmp.path(), &opts).unwrap();
        let file = opts
            .prefix
            .join("share/td-mta/services/td-mta-acme-public.scm");
        fs::write(&file, "operator edit").unwrap();
        assert!(install(tmp.path(), &opts).is_err());
        assert_eq!(fs::read_to_string(file).unwrap(), "operator edit");
    }

    #[test]
    fn services_only_preserves_operator_edits() {
        let tmp = crate::test_temp::TempDir::new("install-mta").unwrap();
        let prefix = tmp.path().join("prefix");
        directory(&prefix.join("bin"), 0o755).unwrap();
        write_new(&prefix.join("bin/td-mta"), b"fixture").unwrap();
        let opts = Options {
            prefix: prefix.clone(),
            connection: Some(tmp.path().join("connection")),
            state: Some(tmp.path().join("state")),
            profiles: vec!["frontend".into(), "relay".into()],
            services_only: true,
            acme: None,
        };
        install(tmp.path(), &opts).unwrap();
        install(tmp.path(), &opts).unwrap();
        let file = prefix.join("share/td-mta/services/td-mta-relay.service");
        fs::write(&file, "operator edit").unwrap();
        assert!(install(tmp.path(), &opts).is_err());
        assert_eq!(fs::read_to_string(file).unwrap(), "operator edit");
    }
}
