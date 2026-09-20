//! Fixed server policy shared by the boot provisioner and realized recipe test.

// Both /etc paths are reviewed persistent-state links in the image recipe.
pub const SSHD_HOST_KEY: &str = "/etc/ssh/ssh_host_ed25519_key";
pub const SSHD_AUTHORIZED_KEYS: &str = "/etc/ssh/authorized_keys";
pub const SSHD_SELFTEST_AUTHORIZED_KEYS: &str = "/run/td-ssh-selftest-authorized_keys";
pub const OPENSSH_KEX_ALGORITHMS: &str =
    "mlkem768x25519-sha256,sntrup761x25519-sha512,curve25519-sha256";
pub const OPENSSH_KEY_ALGORITHMS: &str = "ssh-ed25519";
pub const OPENSSH_CIPHERS: &str = "chacha20-poly1305@openssh.com";

/// The caller supplies the already admitted primary account name, never raw input.
pub fn config(primary_name: &str) -> String {
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
         PermitRootLogin prohibit-password\n\
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
         PidFile /run/sshd.pid\n\
         Match User {primary_name}\n\
         \tAuthorizedKeysFile {SSHD_SELFTEST_AUTHORIZED_KEYS}\n"
    )
}
