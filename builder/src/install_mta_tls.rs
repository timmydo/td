//! Renew only the setup profile's local PostgreSQL leaf under its existing CA.
use super::{absolute, install_mta_backup, install_mta_setup, text, write_new, Result};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};
use std::time::Duration;

const HELP: &str = "Usage: ./install-td-mta --postgres-tls-renew --tls-dir PATH --data-dir PATH
  --postgres-bin PATH --openssl-bin PATH --port N [--force]

Renews the local-only localhost/127.0.0.1 PostgreSQL certificate below 30 days.
Uses the existing CA and server key; issues a 90-day leaf, retains the previous
certificate, reloads PostgreSQL and verifies its presented certificate. Does not
rotate CA trust, change database materials, restart mail or send mail. Run as the
cluster owner. --force exercises renewal even when the current leaf is not due.
";
const LIMIT: u64 = 65536;
const MARGIN: u64 = 30 * 86400;
struct Options {
    tls: PathBuf,
    data: PathBuf,
    postgres: PathBuf,
    openssl: PathBuf,
    port: u16,
    force: bool,
}
fn options(args: &[String]) -> Result<Options> {
    let mut values = BTreeMap::new();
    let mut force = false;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        if flag == "--force" {
            if force {
                return Err("duplicate --force".into());
            }
            force = true;
            continue;
        }
        if ![
            "--tls-dir",
            "--data-dir",
            "--postgres-bin",
            "--openssl-bin",
            "--port",
        ]
        .contains(&flag.as_str())
        {
            return Err(format!("unknown renewal option: {flag}"));
        }
        let value = args.next().ok_or_else(|| format!("missing {flag} value"))?;
        if values.insert(flag.as_str(), value.as_str()).is_some() {
            return Err(format!("duplicate {flag}"));
        }
    }
    let required = |key| {
        values
            .get(key)
            .copied()
            .ok_or_else(|| format!("missing {key}"))
    };
    let port = required("--port")?
        .parse::<u16>()
        .map_err(|_| "invalid PostgreSQL port")?;
    if port == 0 {
        return Err("PostgreSQL port must be nonzero".into());
    }
    Ok(Options {
        tls: absolute(required("--tls-dir")?)?,
        data: absolute(required("--data-dir")?)?,
        postgres: absolute(required("--postgres-bin")?)?,
        openssl: absolute(required("--openssl-bin")?)?,
        port,
        force,
    })
}
fn private_dir(path: &Path) -> Result<()> {
    install_mta_setup::trusted_ancestors(path)?;
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_dir() || meta.uid() != crate::sys::getuid() || meta.mode() & 0o777 != 0o700 {
        return Err(format!(
            "{} must be an owned mode-0700 directory",
            path.display()
        ));
    }
    Ok(())
}
fn private_file(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file()
        || meta.uid() != crate::sys::getuid()
        || meta.mode() & 0o077 != 0
        || meta.len() > LIMIT
    {
        return Err("invalid private renewal file".into());
    }
    Ok(())
}
fn lock(tls: &Path) -> Result<File> {
    let path = tls.join("renew.lock");
    match fs::symlink_metadata(&path) {
        Ok(meta)
            if !meta.is_file()
                || meta.uid() != crate::sys::getuid()
                || meta.mode() & 0o077 != 0 =>
        {
            return Err("invalid renewal lock".into())
        }
        Ok(_) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.to_string()),
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| e.to_string())?;
    if !crate::sys::flock_try_exclusive(lock.as_raw_fd()).map_err(|e| e.to_string())? {
        return Err("another renewal owns this directory".into());
    }
    Ok(lock)
}

struct Work {
    path: PathBuf,
    diagnostic: PathBuf,
    serial: usize,
}
impl Work {
    fn new(tls: &Path) -> Result<Self> {
        let path = tls.join("renew-work");
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                private_dir(&path)?;
                let entries = fs::read_dir(&path).map_err(|e| e.to_string())?;
                let mut files = Vec::new();
                for item in entries.take(65) {
                    let item = item.map_err(|e| e.to_string())?;
                    let name = item.file_name();
                    let name = name.to_str().ok_or("invalid renewal scratch entry")?;
                    if files.len() >= 64 || !scratch_name(name) {
                        return Err("unexpected renewal scratch entries; inspect manually".into());
                    }
                    private_file(&item.path())?;
                    files.push(item.path());
                }
                for file in files {
                    fs::remove_file(file).map_err(|e| e.to_string())?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&path)
                    .map_err(|e| e.to_string())?;
            }
            Err(e) => return Err(e.to_string()),
        }
        Ok(Self {
            path,
            diagnostic: tls.join("renew-last-error"),
            serial: 0,
        })
    }
    fn write(&self, name: &str, data: &[u8]) -> Result<PathBuf> {
        let path = self.path.join(name);
        write_new(&path, data)?;
        Ok(path)
    }
    fn execute(&mut self, mut cmd: Command) -> Result<(ExitStatus, Vec<u8>)> {
        self.serial += 1;
        if self.serial > 32 {
            return Err("renewal command limit exceeded".into());
        }
        let path = self.path.join(format!("tool-{}", self.serial));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| e.to_string())?;
        cmd.current_dir(&self.path);
        let (_, status) = install_mta_backup::bounded_tool_output(
            cmd,
            &mut file,
            LIMIT,
            &self.diagnostic,
            Duration::from_secs(30),
        )?;
        let output = fs::read(path).map_err(|e| e.to_string())?;
        Ok((status, output))
    }
    fn checked(&mut self, cmd: Command) -> Result<Vec<u8>> {
        let (status, output) = self.execute(cmd)?;
        if !status.success() {
            return Err(format!(
                "certificate tool failed ({status}); inspect {}",
                self.diagnostic.display()
            ));
        }
        Ok(output)
    }
}
fn scratch_name(name: &str) -> bool {
    [
        "ca.pem",
        "ca.key",
        "server.key",
        "server.pem",
        "request.csr",
        "candidate.pem",
        "extensions",
    ]
    .contains(&name)
        || name
            .strip_prefix("tool-")
            .is_some_and(|v| v.parse::<usize>().is_ok_and(|n| (1..=32).contains(&n)))
}
impl Drop for Work {
    fn drop(&mut self) {
        if let Ok(entries) = fs::read_dir(&self.path) {
            for item in entries.take(64).flatten() {
                if item.file_name().to_str().is_some_and(scratch_name) {
                    let _ = fs::remove_file(item.path());
                }
            }
        }
        let _ = fs::remove_dir(&self.path);
    }
}
fn openssl(opts: &Options, args: &[&str]) -> Command {
    let mut command = install_mta_setup::command(&opts.openssl);
    command.args(args);
    command
}
fn local_names(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let Some((header, names)) = text.split_once('\n') else {
        return false;
    };
    if header.trim() != "X509v3 Subject Alternative Name:" {
        return false;
    }
    let mut names = names.split(',').map(str::trim).collect::<Vec<_>>();
    names.sort_unstable();
    names == ["DNS:localhost", "IP Address:127.0.0.1"]
}
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())
}
fn publish(tls: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let stage = tls.join("renew-publish");
    match fs::symlink_metadata(&stage) {
        Ok(_) => {
            private_file(&stage)?;
            fs::remove_file(&stage).map_err(|e| e.to_string())?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.to_string()),
    }
    write_new(&stage, bytes)?;
    File::open(&stage)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    fs::rename(stage, tls.join(name)).map_err(|e| e.to_string())?;
    sync_dir(tls)
}
fn presented(opts: &Options, work: &mut Work, certificate: &[u8]) -> Result<bool> {
    let endpoint = format!("127.0.0.1:{}", opts.port);
    let (status, output) = work.execute(openssl(
        opts,
        &[
            "s_client",
            "-starttls",
            "postgres",
            "-connect",
            &endpoint,
            "-servername",
            "localhost",
            "-verify_hostname",
            "localhost",
            "-verify_return_error",
            "-CAfile",
            "ca.pem",
            "-no-CApath",
            "-no-CAstore",
            "-showcerts",
            "-no_ign_eof",
        ],
    ))?;
    if !status.success() {
        return Ok(false);
    }
    let expected = std::str::from_utf8(certificate)
        .map_err(|_| "certificate is not PEM")?
        .trim();
    let actual = std::str::from_utf8(&output).map_err(|_| "invalid TLS probe output")?;
    let Some(begin) = actual.find("-----BEGIN CERTIFICATE-----") else {
        return Ok(false);
    };
    let Some(pem) = actual.get(begin..) else {
        return Ok(false);
    };
    let Some(end) = pem.find("-----END CERTIFICATE-----") else {
        return Ok(false);
    };
    Ok(pem.get(..end + "-----END CERTIFICATE-----".len()) == Some(expected))
}
fn renew(opts: &Options) -> Result<()> {
    private_dir(&opts.tls)?;
    private_dir(&opts.data)?;
    let _lock = lock(&opts.tls)?;
    // Validate the diagnostic target before the bounded tool runner can truncate it.
    match fs::symlink_metadata(opts.tls.join("renew-last-error")) {
        Ok(meta)
            if !meta.is_file()
                || meta.uid() != crate::sys::getuid()
                || meta.mode() & 0o077 != 0 =>
        {
            return Err("invalid renewal diagnostic".into())
        }
        Ok(_) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.to_string()),
    }
    let mut work = Work::new(&opts.tls)?;
    for name in ["ca.pem", "ca.key", "server.key", "server.pem"] {
        let bytes = install_mta_setup::protected_input(&opts.tls.join(name), LIMIT)?;
        work.write(name, &bytes)?;
    }
    let mut version = install_mta_setup::command(&opts.postgres.join("pg_ctl"));
    version.arg("--version");
    if !work
        .checked(version)?
        .starts_with(b"pg_ctl (PostgreSQL) 17.")
    {
        return Err("PostgreSQL 17 tools are required".into());
    }
    let version = work.checked(openssl(opts, &["version"]))?;
    if !version.starts_with(b"OpenSSL 3.") && !version.starts_with(b"OpenSSL 4.") {
        return Err("OpenSSL 3 or 4 is required".into());
    }
    let old = fs::read(work.path.join("server.pem")).map_err(|e| e.to_string())?;
    let public = work.checked(openssl(
        opts,
        &["x509", "-in", "server.pem", "-pubkey", "-noout"],
    ))?;
    if public != work.checked(openssl(opts, &["pkey", "-in", "server.key", "-pubout"]))? {
        return Err("server certificate and key differ".into());
    }
    if !local_names(&work.checked(openssl(
        opts,
        &[
            "x509",
            "-in",
            "server.pem",
            "-noout",
            "-ext",
            "subjectAltName",
        ],
    ))?) {
        return Err("renewal only supports the localhost/127.0.0.1 setup certificate".into());
    }
    work.checked(openssl(
        opts,
        &[
            "verify",
            "-no_check_time",
            "-purpose",
            "sslserver",
            "-verify_hostname",
            "localhost",
            "-CAfile",
            "ca.pem",
            "-no-CApath",
            "-no-CAstore",
            "server.pem",
        ],
    ))?;
    let ca_public = work.checked(openssl(
        opts,
        &["x509", "-in", "ca.pem", "-pubkey", "-noout"],
    ))?;
    if ca_public != work.checked(openssl(opts, &["pkey", "-in", "ca.key", "-pubout"]))? {
        return Err("CA certificate and key differ".into());
    }
    work.checked(openssl(
        opts,
        &["x509", "-in", "ca.pem", "-checkend", "7862400", "-noout"],
    ))?;
    let (valid, _) = work.execute(openssl(
        opts,
        &[
            "x509",
            "-in",
            "server.pem",
            "-checkend",
            &MARGIN.to_string(),
            "-noout",
        ],
    ))?;
    if !valid.success() && valid.code() != Some(1) {
        return Err("cannot inspect certificate expiry".into());
    }
    let changed = opts.force || !valid.success();
    let certificate = if changed {
        let request = work.checked(openssl(
            opts,
            &[
                "req",
                "-new",
                "-key",
                "server.key",
                "-subj",
                "/CN=localhost",
            ],
        ))?;
        work.write("request.csr", &request)?;
        work.write("extensions", b"basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n")?;
        let serial = format!("0x01{}", install_mta_setup::random_hex(15)?);
        let candidate = work.checked(openssl(
            opts,
            &[
                "x509",
                "-req",
                "-in",
                "request.csr",
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key",
                "-set_serial",
                &serial,
                "-days",
                "90",
                "-extfile",
                "extensions",
            ],
        ))?;
        work.write("candidate.pem", &candidate)?;
        work.checked(openssl(
            opts,
            &[
                "verify",
                "-purpose",
                "sslserver",
                "-verify_hostname",
                "localhost",
                "-CAfile",
                "ca.pem",
                "-no-CApath",
                "-no-CAstore",
                "candidate.pem",
            ],
        ))?;
        work.checked(openssl(
            opts,
            &[
                "verify",
                "-purpose",
                "sslserver",
                "-verify_ip",
                "127.0.0.1",
                "-CAfile",
                "ca.pem",
                "-no-CApath",
                "-no-CAstore",
                "candidate.pem",
            ],
        ))?;
        if old != install_mta_setup::protected_input(&opts.tls.join("server.pem"), LIMIT)? {
            return Err("server certificate changed during renewal".into());
        }
        publish(&opts.tls, "server.previous.pem", &old)?;
        publish(&opts.tls, "server.pem", &candidate)?;
        candidate
    } else {
        old
    };
    let mut reloaded = changed;
    if changed || !presented(opts, &mut work, &certificate)? {
        reloaded = true;
        let mut reload = install_mta_setup::command(&opts.postgres.join("pg_ctl"));
        reload.args(["reload", "-D", text(&opts.data)?]);
        work.checked(reload)?;
        let mut verified = false;
        for _ in 0..3 {
            if presented(opts, &mut work, &certificate)? {
                verified = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if !verified {
            return Err("PostgreSQL did not present the selected certificate after reload; certificate retained, inspect and retry".into());
        }
    }
    match fs::remove_file(&work.diagnostic) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.to_string()),
    }
    println!("{{\"status\":\"ready\",\"renewed\":{changed},\"reloaded\":{reloaded},\"peer_certificate_verified\":true}}");
    Ok(())
}

pub(super) fn run(args: &[String]) -> ExitCode {
    if args == ["--help"] {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    match options(args).and_then(|opts| renew(&opts)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("install-td-mta PostgreSQL TLS: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn names_refuse_other_hostnames_and_extra_addresses() {
        assert!(local_names(
            b"X509v3 Subject Alternative Name:\n    DNS:localhost, IP Address:127.0.0.1\n"
        ));
        for text in [b"DNS:localhost".as_slice(), b"X509v3 Subject Alternative Name:\nDNS:localhost, IP Address:127.0.0.1, DNS:mail.example.org", b"X509v3 Subject Alternative Name:\nDNS:localhost, IP Address:127.0.0.2"] { assert!(!local_names(text)); }
    }
    #[test]
    fn interrupted_publication_is_bounded_and_refuses_links() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = crate::test_temp::TempDir::new("mail-leaf-publish").unwrap();
        write_new(&tmp.path().join("renew-publish"), b"").unwrap();
        publish(tmp.path(), "server.pem", b"first certificate").unwrap();
        publish(tmp.path(), "server.previous.pem", b"first certificate").unwrap();
        publish(tmp.path(), "server.pem", b"second certificate").unwrap();
        assert_eq!(
            fs::read(tmp.path().join("server.previous.pem")).unwrap(),
            b"first certificate"
        );
        assert_eq!(
            fs::metadata(tmp.path().join("server.pem"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::os::unix::fs::symlink(
            tmp.path().join("server.pem"),
            tmp.path().join("renew-publish"),
        )
        .unwrap();
        assert!(publish(tmp.path(), "server.pem", b"refused").is_err());
        assert_eq!(
            fs::read(tmp.path().join("server.pem")).unwrap(),
            b"second certificate"
        );
    }

    #[test]
    fn parser_requires_explicit_cluster_tools_and_port() {
        let args = [
            "--tls-dir",
            "/tls",
            "--data-dir",
            "/pg",
            "--postgres-bin",
            "/tools",
            "--openssl-bin",
            "/openssl",
            "--port",
            "5432",
        ]
        .map(String::from)
        .to_vec();
        assert!(options(&args).is_ok());
        let mut invalid = args.clone();
        invalid.push("--force".into());
        invalid.push("--force".into());
        assert!(options(&invalid).is_err());
        let mut invalid = args;
        *invalid.last_mut().unwrap() = "0".into();
        assert!(options(&invalid).is_err());
    }
}
