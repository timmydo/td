# Standalone SMTP receiver deployment

This guide covers `serve --smtp-only` on x86-64 Linux. It receives direct
IPv4 SMTP, offers STARTTLS, and durably stores accepted local mail. It does
not yet expose JMAP or send mail. The complete v1 configuration is still
required, including HTTPS, identities and relay inputs, even though those
services are inactive. [CONFIG.md](CONFIG.md) owns the exact runtime profile.

## Build and retain a release

On the build host, use the pinned archive preparation and isolated build in
[td-crypto/PORTABLE.md](../td-crypto/PORTABLE.md). The build host needs Linux
5.12 or newer for its namespace sandbox; the standalone runtime targets
Linux 5.6 or newer. aarch64 remains unqualified.

The successful build prints a content-addressed artifact directory. Preserve
that complete directory with its original name as the release archive.
Install its `td-mta`, `td-mta.debug`, `BUILD-INPUTS` and complete `notices/`
together. Keep the previous archive when upgrading the executable.
The other executables in that directory are test controllers and probes;
they are not installed dependencies. The runtime is static PIE and has no
shared-library or td-helper requirement. Its matching companion contains
ordinary symbols and source-line data. Keep the pair from the same build.

A release installation can use this layout, outside persistent state:

```text
/opt/td-mta/bin/td-mta
/opt/td-mta/lib/debug/bin/td-mta.debug
/opt/td-mta/BUILD-INPUTS
/opt/td-mta/ARTIFACT-ID
/opt/td-mta/notices/
```

Install the executable mode 0755, companion and receipt mode 0644, under
administrator-owned directories. Write the complete original
`crypto-artifact-<NAR-sha256>` directory name into the installation's
`ARTIFACT-ID`; this is an operator record, not a generated build file.
Compare the installed runtime and companion byte-for-byte with the archive
before starting. `BUILD-INPUTS` identifies inputs, not installed output bytes.
Preserve the complete notices tree. Do not install only the runtime and discard its companion or provenance receipt.
This portable build uses the pinned upstream Rust/musl kit described in
PORTABLE.md; it is not the source-bootstrapped td system-image artifact.
Prebuilt standard-library/libc code and provider assembly remain explicit
profiling boundaries. This layout does not configure automatic discovery by
host debuggers, and td-profiler currently indexes only canonical `/td/store`
objects. Explicitly select the retained companion when using external tools.

## Provision the service identity and inputs

Create a dedicated unprivileged `td-mta` account and group using the host's
account administration. Run every command below as that identity, except
administrator installation, directory provisioning and supervisor setup.
The executable does not switch UID and a root-run config check does not
prove that the service can read its files.

Provision these physical directories, each owned by the service UID and
mode 0700:

```text
/var/lib/td-mta
/var/lib/td-mta/ingress
/run/td-mta
/var/lib/td-mta-logs
```

Ancestors must be owned by root or the service UID and not writable by
other users or groups. No symlink component is allowed. Use storage that
supports the locking, hard links and file/directory sync in STORAGE.md.
The configured roots are distinct and nonnested; `ingress` is the fixed exception beneath
data. `/run/td-mta` must be recreated after boot. The log root is currently
validated but unused; serve writes redacted lifecycle JSON to stdout.
Set these paths explicitly in configuration, including the log-root override:

```text
[paths]
data = "/var/lib/td-mta"
runtime = "/run/td-mta"
logs = "/var/lib/td-mta-logs"
```

Using a private sibling under `/var/lib` avoids relying on `/var/log` being
non-group-writable. The ancestor checks still apply to every selected path.

Place configuration at `/etc/td-mta/config` with physical trusted ancestors.
Use [SCHEMA.md's complete example](SCHEMA.md#11-structural-fixture) as the
starting schema, substituting the real host/domain/account, aliases, paths,
resolver and material. The sample's `.test` names are not deployment DNS.
For this profile, use files-mode certificates and direct IPv4 SMTP listeners.
Keep the required HTTPS listener stanza: it is validated but inactive.
Gateway, ACME, HTTP-01 and IPv6 SMTP startup are unsupported. Do not enable
MTA-STS publication: this profile does not serve its required HTTPS policy.

The server key must be unencrypted P-256 PKCS#8 `PRIVATE KEY` PEM, separate
from its `CERTIFICATE` chain. The chain must cover every configured consumer
name, including the inactive JMAP origin. The relay password file is still
required and decoded, but no relay connection is made. It may contain an
unused placeholder while outbound delivery is inactive; replace it with the
intended credential before any future outbound activation.

Secrets normally belong to the service UID and must be mode 0400 or 0600.
Their directories must be traversable by that UID; for example, a root-owned
mode-0750 secrets directory with group `td-mta`, not root-owned mode 0700.
Nonsecret configuration, certificates and signatures may be root-owned and
readable by the service, with no group/other writing or executable/special
bits. No secret may alias a nonsecret input, including through a hard link.
[SCHEMA.md](SCHEMA.md#referenced-file-requirements) defines all file rules.

## Initialize and start

Choose one permanent account ID of 32 lowercase hex digits and use the
same ID in configuration and initialization. For the structural example:

```text
/opt/td-mta/bin/td-mta store init --root /var/lib/td-mta --account 01010101010101010101010101010101
/opt/td-mta/bin/td-mta config check --config /etc/td-mta/config
/opt/td-mta/bin/td-mta serve --smtp-only --config /etc/td-mta/config
```

Initialize only a fresh store. The command creates the account and Inbox;
it does not create the provisioned directories. Config check makes no
network calls and does not open the store. Startup performs locked recovery
before binding listeners. Only the flushed `serve` JSON with status `ready`
announces receiving readiness. A supervisor's successful exec is earlier.

Port 25 needs a host-provided low-port binding grant for the unprivileged
process through its supervisor or a reviewed launcher. Do not use setuid or
run the protocol engine as root. A private pilot can use an unprivileged
configured port; accepting Internet SMTP requires the proper
port, DNS and network setup. These are operator actions; no command here
changes DNS or firewall policy.

The receiver accepts only configured local recipients. It does not provide
mailbox access yet. Keep any previous mail system and its state until a
separately tested access/migration path exists; this receiving increment is
not a complete replacement for an existing mail service.

## Observe and stop

As the service UID:

```text
/opt/td-mta/bin/td-mta status --json --runtime /run/td-mta
/opt/td-mta/bin/td-mta stop --runtime /run/td-mta
```

Status reports receiving lifecycle, not quota or delivery metrics. It is
unavailable during cold storage recovery. `not-running` means the control
endpoint is unavailable; it does not prove the process has exited. Use the
supervisor's process state when waiting for exit. `stop` returning `requested`
means the shutdown flag was set. Wait for the serving process to exit
before maintenance or replacement; native storage I/O can extend that wait.
A supervisor should keep stdout/stderr consumed throughout the lifetime.
A supervisor readiness probe can run as the service UID:

```text
/opt/td-mta/bin/td-mta status --json --require-ready --runtime /run/td-mta
```

This exits zero only when the current receiver reports ready. It exits one
for another lifecycle or a control error, and two for invalid arguments.
It does not wait for readiness; let the supervisor retry within its startup
deadline, allowing time for cold storage recovery, during which the endpoint
is absent. Preserve the probe JSON in diagnostics: it distinguishes a
successful nonready observation from a permissions or configuration error.
Ordinary status still exits
zero for any successful query, including a stopping receiver. Neither form
guarantees that the next delivery will be admitted.

SIGTERM, SIGINT and SIGKILL currently terminate without protocol draining.
Restart recovers mail that received final SMTP 250 and discards incomplete
DATA spools. A committed message whose reply was lost may be retried and
stored again. Manual certificate/configuration updates require stopping,
waiting for exit, replacing regular files, running config check as the
service UID, and restarting. Do not change trusted ancestors or mutate
inputs in place while validation is running. There is no reload or
automatic certificate renewal in this profile. Replace material before the
admitted chain's validity window ends. Expiration or an invalid UTC observation
stops receiving with a TLS error; restarting with the same invalid material
does not recover service.

## Offline backup and restore

Stop and wait for exit first. Provision each destination as a fresh private
mode-0700 directory under trusted ancestors, owned by the service UID. Then:

```text
/opt/td-mta/bin/td-mta store verify --root /var/lib/td-mta --all
/opt/td-mta/bin/td-mta backup --root /var/lib/td-mta --destination /var/backups/td-mta-copy
/opt/td-mta/bin/td-mta store verify --root /var/backups/td-mta-copy --all
```

When a restore is needed, provision a different fresh private destination:

```text
/opt/td-mta/bin/td-mta restore --root /var/backups/td-mta-copy --destination /var/lib/td-mta-restored
```

Use fresh destination names for successive operations; existing databases
are not overwritten. Backup copies the stopped database, including message
bodies, but does not certify semantic integrity. The separate verification
checks metadata and body digests. Preserve configuration and secrets
separately with their permissions; backups exclude them and disposable
spools. Restore verifies the copy and renews its epoch without activating it.

To activate the restored root, provision its empty `ingress` subdirectory,
point `[paths] data` at it, and run config check and serve. Keep only one
receiver active for the account. Retain the old source and backup until the
restored service is verified. After new mail arrives, switching back to old
state alone would lose that mail. [STORAGE.md](STORAGE.md) owns verification,
backup, publication-failure and restore semantics.
