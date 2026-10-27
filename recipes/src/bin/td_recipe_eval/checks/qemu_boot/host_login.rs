//! A host-side SSH login into a booted server (`qemu-boot-server`, td-svc
//! DESIGN.md §10, "Profiles"). QEMU's user network forwards one loopback
//! port to the guest's sshd, and the host's OpenSSH client logs in with
//! keys made for this run: the provisioned one must be admitted as the
//! provisioned account and refused as root, one the server was never
//! given must be refused, and one only a NoCloud seed offers must be
//! admitted as that account on the boot the seed is attached to alone.

use std::cell::{Cell, RefCell};
use std::fs::{self, File};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::{find_qemu_tool, QEMU_USER_NETDEV};
use crate::checks::provisioned_account::{encoding, ProvisionedAccount};

/// One attempt's bound: connection, key exchange and the remote command.
const ATTEMPT_LIMIT: Duration = Duration::from_secs(60);
const PROVISIONED_KEY: &str = "provisioned";
const METADATA_KEY: &str = "metadata";
const STRANGER_KEY: &str = "stranger";

/// One login, in the order they run.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Attempt {
    /// The provisioned key as the provisioned account: the remote `id -un`
    /// prints its name.
    Provisioned,
    /// The seed's key as that account: admitted only on a seeded boot.
    Metadata,
    /// A key the server was never given, as that account: refused.
    Stranger,
    /// The provisioned key as root: refused, root having no SSH login.
    Root,
}

const ATTEMPTS: &[Attempt] = &[
    Attempt::Provisioned,
    Attempt::Metadata,
    Attempt::Stranger,
    Attempt::Root,
];

impl Attempt {
    fn key(self) -> &'static str {
        match self {
            Attempt::Provisioned | Attempt::Root => PROVISIONED_KEY,
            Attempt::Metadata => METADATA_KEY,
            Attempt::Stranger => STRANGER_KEY,
        }
    }

    /// Whether sshd must admit it, given whether the boot was seeded.
    fn admitted(self, seeded: bool) -> bool {
        match self {
            Attempt::Provisioned => true,
            Attempt::Metadata => seeded,
            Attempt::Stranger | Attempt::Root => false,
        }
    }

    /// Where its output goes, beside the keys.
    fn label(self) -> &'static str {
        match self {
            Attempt::Provisioned => "provisioned-login",
            Attempt::Metadata => "metadata-login",
            Attempt::Stranger => "stranger-login",
            Attempt::Root => "root-login",
        }
    }

    fn user(self, account: &str) -> &str {
        match self {
            Attempt::Provisioned | Attempt::Metadata | Attempt::Stranger => account,
            Attempt::Root => "root",
        }
    }
}

pub(crate) struct HostLogin {
    ssh: PathBuf,
    /// Chosen as the login boot's QEMU is started (`netdev`), so the window
    /// in which another process can take it is that boot's spawn alone.
    port: Cell<Option<u16>>,
    account: ProvisionedAccount,
    /// The client's keys, outputs and known_hosts. `-F /dev/null` and the
    /// explicit paths keep ssh from reading or creating `~/.ssh`.
    home: PathBuf,
    /// Filled by the boot that ran the attempts.
    outcome: RefCell<Option<Result<(), String>>>,
    /// Whether the boot carries a seed offering the metadata key.
    seeded: Cell<bool>,
}

impl HostLogin {
    /// Makes the run's three keys under `home` (which must not exist) with
    /// the host's `ssh-keygen`, and the account that admits only the first.
    pub(crate) fn prepare(home: &Path, stem: &str, name: &str) -> Result<Self, String> {
        let ssh = host_tool("ssh")?;
        let keygen = host_tool("ssh-keygen")?;
        if home
            .to_str()
            .is_none_or(|text| text.contains(char::is_whitespace))
        {
            return Err(format!(
                "the SSH client's home {} must be whitespace-free UTF-8: ssh reads its \
                 options as words",
                home.display()
            ));
        }
        fs::create_dir(home).map_err(|error| format!("create {}: {error}", home.display()))?;
        for key in [PROVISIONED_KEY, METADATA_KEY, STRANGER_KEY] {
            let status = Command::new(&keygen)
                .env_clear()
                .env("HOME", home)
                .args(["-q", "-t", "ed25519", "-N", "", "-C"])
                .arg(format!("td-qemu-boot-server-{key}"))
                .arg("-f")
                .arg(home.join(key))
                .stdin(Stdio::null())
                .status()
                .map_err(|error| format!("run {}: {error}", keygen.display()))?;
            if !status.success() {
                return Err(format!(
                    "{} failed for the {key} key: {status}",
                    keygen.display()
                ));
            }
        }
        let public = home.join(format!("{PROVISIONED_KEY}.pub"));
        let public = fs::read_to_string(&public)
            .map_err(|error| format!("read {}: {error}", public.display()))?;
        Ok(Self {
            ssh,
            port: Cell::new(None),
            account: ProvisionedAccount::new(stem, name, &public)?,
            home: home.to_owned(),
            outcome: RefCell::new(None),
            seeded: Cell::new(false),
        })
    }

    pub(crate) fn account(&self) -> &ProvisionedAccount {
        &self.account
    }

    /// The metadata key's public line, for a seed to offer; the boot's
    /// logins then expect it admitted.
    pub(crate) fn seed_key(&self) -> Result<String, String> {
        let path = self.home.join(format!("{METADATA_KEY}.pub"));
        let line = fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        self.seeded.set(true);
        Ok(line.trim_end().to_owned())
    }

    /// The shared user network plus the one loopback forward to port 22, on
    /// a port picked now, as the boot's QEMU is about to bind it.
    pub(crate) fn netdev(&self) -> Result<String, String> {
        let port = free_loopback_port()?;
        self.port.set(Some(port));
        Ok(forwarded_netdev(port))
    }

    fn port(&self) -> Result<u16, String> {
        self.port
            .get()
            .ok_or_else(|| "the SSH login ran on a boot with no forward".to_string())
    }

    /// The boot's verdict, and that the host key the client accepted is the
    /// one the guest's console reported (`fingerprint`, `SHA256:...`).
    pub(crate) fn verdict(&self, fingerprint: Option<&str>) -> Result<(), String> {
        self.outcome
            .borrow()
            .clone()
            .unwrap_or_else(|| Err("the boot ended before the host's SSH login ran".into()))?;
        let known = self.home.join("known_hosts");
        let text = fs::read_to_string(&known)
            .map_err(|error| format!("read {}: {error}", known.display()))?;
        let host = format!("[127.0.0.1]:{}", self.port()?);
        let accepted: Vec<String> = text
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_ascii_whitespace();
                (fields.next() == Some(host.as_str()) && fields.next() == Some("ssh-ed25519"))
                    .then(|| fields.next().and_then(key_fingerprint))
                    .flatten()
            })
            .collect();
        match (accepted.as_slice(), fingerprint) {
            ([accepted], Some(reported)) if accepted == reported => Ok(()),
            _ => Err(format!(
                "the host's SSH client accepted {accepted:?} for {host}, but the guest's \
                 console reported host key {fingerprint:?}"
            )),
        }
    }

    pub(crate) fn start(&self) -> HostLoginRun<'_> {
        HostLoginRun {
            login: self,
            next: 0,
            child: None,
        }
    }

    fn attempt(&self, attempt: Attempt) -> Result<Child, String> {
        let output = |stream: &str| {
            let path = self.home.join(format!("{}.{stream}", attempt.label()));
            File::create(&path).map_err(|error| format!("create {}: {error}", path.display()))
        };
        let mut command = Command::new(&self.ssh);
        command
            .env_clear()
            .env("HOME", &self.home)
            .args(["-F", "/dev/null", "-T"])
            .args(client_options(&self.home))
            .arg("-i")
            .arg(self.home.join(attempt.key()))
            .args(["-p", &self.port()?.to_string()])
            .args(["-l", attempt.user(self.account.name())])
            .args(["127.0.0.1", "--", "id", "-un"])
            .stdin(Stdio::null())
            .stdout(output("stdout")?)
            .stderr(output("stderr")?);
        command
            .spawn()
            .map_err(|error| format!("run {}: {error}", self.ssh.display()))
    }

    fn judge(&self, attempt: Attempt, code: Option<i32>) -> Result<(), String> {
        let read = |stream: &str| {
            let path = self.home.join(format!("{}.{stream}", attempt.label()));
            fs::read_to_string(path).unwrap_or_default()
        };
        judge(
            attempt,
            self.account.name(),
            attempt.admitted(self.seeded.get()),
            code,
            &read("stdout"),
            &read("stderr"),
        )
    }
}

/// Admitted means exit 0 and the account's name alone on stdout; refused
/// means ssh's own exit 255 with nothing on stdout and sshd's "Permission
/// denied", which a connection or host-key failure does not print.
fn judge(
    attempt: Attempt,
    account: &str,
    admitted: bool,
    code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> Result<(), String> {
    let passed = if admitted {
        code == Some(0) && stdout == format!("{account}\n")
    } else {
        code == Some(255) && stdout.is_empty() && stderr.contains("Permission denied")
    };
    if passed {
        return Ok(());
    }
    Err(format!(
        "the host's SSH login with the {} key as {:?} should have been {} \
         ({attempt:?}), but ssh exited {code:?} with stdout {stdout:?} and stderr \
         {stderr:?}",
        attempt.key(),
        attempt.user(account),
        if admitted {
            "admitted and printed the account's name"
        } else {
            "refused"
        }
    ))
}

/// The attempts in order, polled from the boot loop once the target is seen.
pub(crate) struct HostLoginRun<'a> {
    login: &'a HostLogin,
    next: usize,
    child: Option<(Child, Instant)>,
}

impl HostLoginRun<'_> {
    pub(crate) fn done(&self) -> bool {
        self.next >= ATTEMPTS.len() && self.child.is_none()
    }

    pub(crate) fn poll(&mut self, target: bool) -> Result<(), String> {
        let result = self.step(target);
        if let Err(error) = &result {
            self.stop();
            *self.login.outcome.borrow_mut() = Some(Err(error.clone()));
        } else if self.done() {
            self.login.outcome.borrow_mut().get_or_insert(Ok(()));
        }
        result
    }

    fn step(&mut self, target: bool) -> Result<(), String> {
        if !target || self.done() {
            return Ok(());
        }
        let attempt = *ATTEMPTS
            .get(self.next)
            .ok_or("no SSH login attempt remains")?;
        let Some((child, started)) = self.child.as_mut() else {
            self.child = Some((self.login.attempt(attempt)?, Instant::now()));
            return Ok(());
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.child = None;
                self.next += 1;
                self.login.judge(attempt, status.code())
            }
            Ok(None) if started.elapsed() >= ATTEMPT_LIMIT => Err(format!(
                "the host's SSH login ({attempt:?}) did not finish within {}s",
                ATTEMPT_LIMIT.as_secs()
            )),
            Ok(None) => Ok(()),
            Err(error) => Err(format!("wait on the host's SSH client: {error}")),
        }
    }

    fn stop(&mut self) {
        if let Some((mut child, _)) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for HostLoginRun<'_> {
    fn drop(&mut self) {
        self.stop();
    }
}

fn host_tool(name: &str) -> Result<PathBuf, String> {
    // The same PATH-then-standard-locations lookup the host's QEMU gets.
    find_qemu_tool(name).map(PathBuf::from).ok_or_else(|| {
        format!("{name} not found on PATH or the standard host locations: qemu-boot-server logs in from the host")
    })
}

/// The kernel picks a free port; QEMU binds it again as it starts.
fn free_loopback_port() -> Result<u16, String> {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| format!("pick a loopback port for the SSH forward: {error}"))
}

fn forwarded_netdev(port: u16) -> String {
    format!("{QEMU_USER_NETDEV},hostfwd=tcp:127.0.0.1:{port}-:22")
}

/// No agent, no config file, no password or keyboard prompt, and the first
/// host key accepted into the private known_hosts, which `verdict` then holds
/// to the console's fingerprint.
fn client_options(home: &Path) -> Vec<String> {
    [
        "BatchMode=yes".to_owned(),
        "IdentitiesOnly=yes".to_owned(),
        "IdentityAgent=none".to_owned(),
        "PreferredAuthentications=publickey".to_owned(),
        "HostKeyAlgorithms=ssh-ed25519".to_owned(),
        "StrictHostKeyChecking=accept-new".to_owned(),
        format!("UserKnownHostsFile={}", home.join("known_hosts").display()),
        "GlobalKnownHostsFile=/dev/null".to_owned(),
        "ConnectTimeout=20".to_owned(),
        "LogLevel=ERROR".to_owned(),
    ]
    .into_iter()
    .flat_map(|option| ["-o".to_owned(), option])
    .collect()
}

/// OpenSSH's `SHA256:` fingerprint of a base64 public key blob.
fn key_fingerprint(blob: &str) -> Option<String> {
    let bytes = encoding::from_base64(blob)?;
    let mut hash = crate::sha256::Sha256::new();
    hash.update(&bytes);
    let digest = encoding::base64(&hash.finalize());
    Some(format!("SHA256:{}", digest.trim_end_matches('=')))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    /// Only the server's boots forward, only from loopback, only to sshd.
    #[test]
    fn the_forward_is_one_loopback_port_to_the_guests_sshd() {
        assert_eq!(
            forwarded_netdev(40022),
            "user,id=net0,hostfwd=tcp:127.0.0.1:40022-:22"
        );
        assert_eq!(forwarded_netdev(1).matches("hostfwd").count(), 1);
    }

    /// Only the account's name alone admits; only sshd's refusal refuses.
    #[test]
    fn the_logins_are_judged_by_exit_and_output() {
        let denied = "ada@127.0.0.1: Permission denied (publickey).\r\n";
        assert!(judge(Attempt::Provisioned, "ada", true, Some(0), "ada\n", "").is_ok());
        for (code, stdout) in [
            (Some(0), "root\n"),
            (Some(0), "ada"),
            (Some(0), ""),
            (Some(1), "ada\n"),
            (None, "ada\n"),
        ] {
            assert!(judge(Attempt::Provisioned, "ada", true, code, stdout, "").is_err());
        }
        for attempt in [Attempt::Stranger, Attempt::Root] {
            assert!(judge(attempt, "ada", false, Some(255), "", denied).is_ok());
            for (code, stdout, stderr) in [
                (Some(0), "ada\n", ""),
                (
                    Some(255),
                    "",
                    "ssh: connect to host 127.0.0.1 port 1: Connection refused",
                ),
                (Some(255), "", "Host key verification failed."),
                (Some(255), "ada\n", denied),
                (Some(1), "", denied),
                (None, "", denied),
            ] {
                assert!(judge(attempt, "ada", false, code, stdout, stderr).is_err());
            }
        }
        assert_eq!(Attempt::Root.user("ada"), "root");
        assert_eq!(Attempt::Root.key(), PROVISIONED_KEY);
        assert_eq!(Attempt::Stranger.key(), STRANGER_KEY);
        assert_eq!(Attempt::Metadata.key(), METADATA_KEY);
        assert_eq!(Attempt::Metadata.user("ada"), "ada");
        // The seed's key is admitted on the seeded boot alone.
        assert!(Attempt::Metadata.admitted(true));
        assert!(!Attempt::Metadata.admitted(false));
        assert!(Attempt::Provisioned.admitted(false));
        assert!(!Attempt::Stranger.admitted(true) && !Attempt::Root.admitted(true));
        let labels: std::collections::BTreeSet<_> =
            ATTEMPTS.iter().map(|attempt| attempt.label()).collect();
        assert_eq!(labels.len(), ATTEMPTS.len());
    }

    /// ssh-keygen's fingerprint of a known key.
    #[test]
    fn the_fingerprint_is_openssh_s() {
        // ssh-keygen -lf of "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl".
        assert_eq!(
            key_fingerprint("AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl")
                .as_deref(),
            Some("SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU")
        );
        assert_eq!(key_fingerprint("not base64"), None);
    }

    #[test]
    fn the_client_reads_no_ambient_configuration() {
        let options = client_options(Path::new("/scratch/home"));
        for expected in [
            "BatchMode=yes",
            "IdentitiesOnly=yes",
            "IdentityAgent=none",
            "GlobalKnownHostsFile=/dev/null",
            "UserKnownHostsFile=/scratch/home/known_hosts",
        ] {
            assert!(
                options.iter().any(|option| option == expected),
                "{expected}"
            );
        }
        assert_eq!(options.iter().filter(|option| *option == "-o").count(), 10);
    }
}
