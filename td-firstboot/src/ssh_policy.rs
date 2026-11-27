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

/// Reduced console state at boot. Only the server uses it for SSH admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// A verifiably unenrolled machine.
    Ordinary,
    /// Enrolled, protected or unavailable.
    Enforced,
}

/// Whether the primary's `Match` block also names the provisioned keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provisioned {
    /// The desktop: the self-test key and, in every login state, the
    /// account's own keys.
    No,
    /// A server: the root-owned provisioned and instance-metadata keys too,
    /// in both forms. The image's builder and the instance's metadata wrote
    /// them, not the account, so the enforced form's withdrawal of
    /// owner-managed keys does not reach them.
    Server,
}

/// `td-sshd -f`'s file, in exactly td-ssh/DESIGN.md §5's subset: td-sshd refuses
/// a whole file for one keyword outside it, and the rest of the policy is
/// compiled behaviour. The caller supplies the already admitted primary account
/// name, never raw input.
pub fn config(primary_name: &str, form: Form, provisioned: Provisioned) -> String {
    let provisioned_keys = match provisioned {
        Provisioned::No => String::new(),
        Provisioned::Server => {
            format!(" {SSHD_PROVISIONED_AUTHORIZED_KEYS} {SSHD_METADATA_AUTHORIZED_KEYS}")
        }
    };
    let allow = match (provisioned, form) {
        (Provisioned::No, _) => format!("AllowUsers {primary_name} td-admin\n"),
        (Provisioned::Server, Form::Ordinary) => String::new(),
        (Provisioned::Server, Form::Enforced) => format!("AllowUsers {primary_name}\n"),
    };
    let admin = match provisioned {
        Provisioned::No => "Match User td-admin\n\tAuthorizedKeysFile /var/lib/td-admin/authorized_keys\n\tForceCommand /bin/td-admin ssh\n\tPermitTTY no\n\tMaxSessions 1\n",
        Provisioned::Server => "",
    };
    let remote_keys = match (provisioned, form) {
        (Provisioned::No, _) | (Provisioned::Server, Form::Ordinary) => {
            format!(" {SSHD_USER_AUTHORIZED_KEYS}")
        }
        (Provisioned::Server, Form::Enforced) => String::new(),
    };
    format!(
        "Port 22\n\
         ListenAddress 0.0.0.0\n\
         HostKey {SSHD_HOST_KEY}\n\
         AuthorizedKeysFile {SSHD_AUTHORIZED_KEYS}\n\
         PermitRootLogin no\n\
         {allow}\
         LoginGraceTime 30\n\
         MaxAuthTries 3\n\
         MaxSessions 4\n\
         {admin}\
         Match User {primary_name}\n\
         \tAuthorizedKeysFile {SSHD_SELFTEST_AUTHORIZED_KEYS}{provisioned_keys}{remote_keys}\n"
    )
}
