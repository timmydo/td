//! Host installation, independent of the host's service manager.
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

type Result<T> = std::result::Result<T, String>;
const HELP: &str = "Usage: ./install-td-mta [--prefix PATH]
       ./install-td-mta --services-only --connection-file PATH --state-dir PATH
           --profile NAME [--profile NAME ...] [--prefix PATH]

Builds a native release for this host using the pinned mail dependencies.
Default prefix: $HOME/.local. Retains debug information and dependency notices.
Service configuration requires all three connection/state/profile options.
Generates both systemd user units and Shepherd definitions; does not enable them.
Database preparation and credentials remain explicit (td-mta/POSTGRES.md).
--services-only uses the already installed executable without rebuilding.
";

#[derive(Debug)]
struct Options {
    prefix: PathBuf,
    connection: Option<PathBuf>,
    state: Option<PathBuf>,
    profiles: Vec<String>,
    services_only: bool,
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
    };
    let mut args = args.iter();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(flag) = args.next() {
        if flag != "--profile" && !seen.insert(flag) {
            return Err(format!("duplicate option: {flag}"));
        }
        if flag == "--services-only" {
            out.services_only = true;
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
                if !value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                    || value.len() > 64
                    || !value.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'
                    })
                    || out.profiles.contains(value)
                {
                    return Err("profiles must start with a lowercase letter and contain only lowercase letters, digits, - or _ (unique, at most 64 bytes)".into());
                }
                out.profiles.push(value.clone());
            }
            _ => return Err(format!("unknown option: {flag}")),
        }
    }
    let service = out.connection.is_some()
        || out.state.is_some()
        || !out.profiles.is_empty()
        || out.services_only;
    if service && (out.connection.is_none() || out.state.is_none() || out.profiles.is_empty()) {
        return Err(
            "service configuration needs --connection-file, --state-dir and --profile".into(),
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
    let exec = argv
        .iter()
        .map(|s| systemd(s))
        .collect::<Vec<_>>()
        .join(" ");
    let unit = format!("[Unit]\nDescription=td-mta {profile}\n\n[Service]\nType=exec\nExecStart=:{exec}\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec=120\nUMask=0077\n\n[Install]\nWantedBy=default.target\n");
    let command = argv.iter().map(|s| scheme(s)).collect::<Vec<_>>().join(" ");
    let log = state.join(profile).join("service.log");
    let scm = format!("(use-modules (shepherd service))\n(register-services\n (service '(td-mta-{profile})\n  #:documentation \"td-mta {profile}; check readiness with td-mta status.\"\n  #:respawn? #t\n  #:start (make-forkexec-constructor\n           '({command})\n           #:log-file {}\n           #:file-creation-mask #o077)\n  #:stop (make-kill-destructor SIGTERM #:grace-period 120)))\n", scheme(text(&log)?));
    Ok((unit, scm))
}

fn install(root: &Path, opts: &Options) -> Result<()> {
    let bin_dir = opts.prefix.join("bin");
    let bin = bin_dir.join("td-mta");
    if !opts.services_only {
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
            let (unit, scm) = service_files(&bin, connection, state, profile)?;
            // Refuse overwriting operator edits. Reinstalling the binary is independent.
            for (suffix, content) in [("service", unit), ("scm", scm)] {
                let path = services.join(format!("td-mta-{profile}.{suffix}"));
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
            println!("Profile {profile}: {}", services.display());
            println!(
                "  systemd: systemctl --user enable --now {}",
                shell(&services.join(format!("td-mta-{profile}.service")))
            );
            println!(
                "  Shepherd: herd load root {} ; herd start td-mta-{profile}",
                shell(&services.join(format!("td-mta-{profile}.scm")))
            );
            println!(
                "  readiness: {} status --json --require-ready --runtime {}",
                shell(&bin),
                shell(&instance.join("runtime"))
            );
        }
    }
    Ok(())
}

pub(crate) fn run(args: &[String]) -> ExitCode {
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
        };
        install(tmp.path(), &opts).unwrap();
        install(tmp.path(), &opts).unwrap();
        let file = prefix.join("share/td-mta/services/td-mta-relay.service");
        fs::write(&file, "operator edit").unwrap();
        assert!(install(tmp.path(), &opts).is_err());
        assert_eq!(fs::read_to_string(file).unwrap(), "operator edit");
    }
}
