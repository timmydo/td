//! Fixed server policy shared by the boot provisioner and realized recipe test.

// Both /etc paths are reviewed persistent-state links in the image recipe.
pub const SSHD_HOST_KEY: &str = "/etc/ssh/ssh_host_ed25519_key";
pub const SSHD_AUTHORIZED_KEYS: &str = "/etc/ssh/authorized_keys";
pub const SSHD_SELFTEST_AUTHORIZED_KEYS: &str = "/run/td-ssh-selftest-authorized_keys";
/// Relative to the admitted account's passwd home, never the server HOME.
pub const SSHD_USER_AUTHORIZED_KEYS: &str = ".ssh/authorized_keys";
/// Root-owned keys a server image's provisioning seeds for the primary
/// account; only a server's policy names them (td-login/TOKEN-LOGIN.md, "SSH").
pub const SSHD_PROVISIONED_AUTHORIZED_KEYS: &str = "/var/lib/td/authorized-keys";
/// Root-owned keys a server's instance metadata offered this boot, which
/// `td-firstboot metadata` or `cloud-metadata` writes after this policy is
/// rendered.
pub const SSHD_METADATA_AUTHORIZED_KEYS: &str = "/run/td-metadata-authorized-keys";
pub const OPENSSH_KEX_ALGORITHMS: &str =
    "mlkem768x25519-sha256,sntrup761x25519-sha512,curve25519-sha256";
pub const OPENSSH_KEY_ALGORITHMS: &str = "ssh-ed25519";
pub const OPENSSH_CIPHERS: &str = "chacha20-poly1305@openssh.com";

/// td-login/TOKEN-LOGIN.md, "SSH": the login state selects the form. Both
/// refuse root (APPLICATIONS.md §L.1, L7); only the ordinary form also reads
/// the primary account's own remote keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// Only a verifiably unenrolled machine.
    Ordinary,
    /// Enrolled or unavailable: no account but the primary.
    Enforced,
}

/// Whether the primary's `Match` block also names the provisioned keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provisioned {
    /// The desktop: the self-test key and, in the ordinary form, the
    /// account's own keys.
    No,
    /// A server: the root-owned provisioned and instance-metadata keys too,
    /// in both forms. The image's builder and the instance's metadata wrote
    /// them, not the account, so the enforced form's withdrawal of
    /// owner-managed keys does not reach them.
    Server,
}

/// The caller supplies the already admitted primary account name, never raw input.
pub fn config(primary_name: &str, form: Form, provisioned: Provisioned) -> String {
    let provisioned_keys = match provisioned {
        Provisioned::No => String::new(),
        Provisioned::Server => {
            format!(" {SSHD_PROVISIONED_AUTHORIZED_KEYS} {SSHD_METADATA_AUTHORIZED_KEYS}")
        }
    };
    let allow = match form {
        Form::Ordinary => String::new(),
        Form::Enforced => format!("AllowUsers {primary_name}\n"),
    };
    let remote_keys = match form {
        Form::Ordinary => format!(" {SSHD_USER_AUTHORIZED_KEYS}"),
        Form::Enforced => String::new(),
    };
    format!(
        "Port 22\n\
         ListenAddress 0.0.0.0\n\
         HostKey {SSHD_HOST_KEY}\n\
         AuthorizedKeysFile {SSHD_AUTHORIZED_KEYS}\n\
         AuthenticationMethods publickey\n\
         PubkeyAuthentication yes\n\
         PasswordAuthentication no\n\
         KbdInteractiveAuthentication no\n\
         ChallengeResponseAuthentication no\n\
         HostbasedAuthentication no\n\
         PermitEmptyPasswords no\n\
         PermitRootLogin no\n\
         {allow}\
         StrictModes yes\n\
         KexAlgorithms {OPENSSH_KEX_ALGORITHMS}\n\
         HostKeyAlgorithms {OPENSSH_KEY_ALGORITHMS}\n\
         PubkeyAcceptedAlgorithms {OPENSSH_KEY_ALGORITHMS}\n\
         Ciphers {OPENSSH_CIPHERS}\n\
         Compression no\n\
         DisableForwarding yes\n\
         PermitTTY yes\n\
         PermitUserEnvironment no\n\
         PermitUserRC no\n\
         UseDNS no\n\
         PrintMotd no\n\
         LoginGraceTime 30\n\
         MaxAuthTries 3\n\
         MaxSessions 4\n\
         Subsystem sftp internal-sftp\n\
         PidFile /run/sshd.pid\n\
         Match User {primary_name}\n\
         \tAuthorizedKeysFile {SSHD_SELFTEST_AUTHORIZED_KEYS}{provisioned_keys}{remote_keys}\n"
    )
}
