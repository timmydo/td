# Standalone SMTP receiver deployment

This guide covers the standalone `serve --smtp-only` receiver. It receives direct
IPv4 SMTP, offers STARTTLS, and durably stores accepted local mail. It does
not yet expose JMAP or send mail. The complete v1 configuration is still
required, including HTTPS, identities and relay inputs, even though those
services are inactive. [CONFIG.md](CONFIG.md) owns the exact runtime profile.

## Build and retain a release

On the build host, use the pinned archive preparation and isolated build in
[td-crypto/PORTABLE.md](../td-crypto/PORTABLE.md). The build host needs Linux
5.12 or newer for its namespace sandbox; the x86-64 standalone runtime
targets Linux 5.6 or newer. An aarch64 cross-build is also available through
`crypto-portable-cross-build`; its successful build alone does not qualify
the runtime. For Amazon Linux 2023 on Graviton, select that ARM artifact;
on x86-64, select the ordinary portable artifact. Both use static musl and
carry their own matching debug companion. Native configuration, STARTTLS,
local delivery and restart validation are still required on the deployment
host before taking production mail. ARM compilation and emulation do not
qualify a minimum kernel version or the Amazon Linux 2023 service setup.

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
`crypto-artifact-<NAR-sha256>` (x86-64) or
`crypto-artifact-aarch64-cross-<NAR-sha256>` directory name into the installation's
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

After cold startup, SIGTERM and SIGINT request orderly shutdown through the same path as
`td-mta stop`. New admission stops; existing SMTP sessions have the existing
five-second protocol allowance, and committed replies retain their final-reply
allowance. The process joins its workers before exiting. Native storage I/O
can delay exit. Repeated signals do not force it. Cold configuration and
storage recovery retain inherited signal behavior. A completed signal
stop exits zero, including when a launcher ignored that signal; it is not
re-raised. SIGKILL remains abrupt.
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

## Private pilot validation

Use a fresh disposable store and private high-port listener on the deployment
host, with test domains and certificate material. Run as the intended service
UID. Keep the pilot's configuration and state separate from existing mail.

1. Run the selected functional commands in PORTABLE.md for an ARM artifact.
   Check that each reports passing tests, rather than zero selected tests.
2. Initialize the pilot store, run `config check`, start `serve --smtp-only`,
   and require `status --json --require-ready` to succeed.
3. From an SMTP client that verifies the pilot certificate, negotiate
   STARTTLS. Confirm an outside recipient is refused, then deliver a unique
   message to a configured local alias and require the final SMTP 250.
4. Start another DATA transaction without completing it. Kill only the
   recorded pilot server PID, wait for exit, then restart the same store.
   Require readiness again; incomplete ingress must not become delivered mail.
5. Request `stop`, wait for process exit, then run `store verify --root PATH
   --all`. For this fresh one-message fixture, its blob count must be one.
6. Repeat startup, readiness and stop through the intended supervisor. Keep
   its logs and the artifact ID with the validation result.

These checks exercise the intended host and storage. They do not establish
load capacity, a tested minimum kernel, or compatibility with another server's
store. Preserve the existing server's store until migration is separately
implemented and verified.

## Supervisor examples

These are administrator-installed examples, not automatic installation or
qualification of a host's service manager. The td-svc stanza has passed its
table validator; neither example has an end-to-end supervisor fixture here.
Provision the identity, complete
configuration, initialized store and ingress/log roots above first. Do not
initialize or restore a store from an automatic restart command. Keep one
supervisor responsible for this receiver. The executable has no dependency
on either supervisor.

### systemd on another Linux distribution

Install the following as `/etc/systemd/system/td-mta.service`, adapting the
paths to the installation. The host must support the directives shown. The
`td-mta` account and group must already exist; use a fixed dedicated UID,
not `DynamicUser`. RuntimeDirectory recreates the volatile private root.
Persistent directories remain operator-provisioned.

```ini
[Unit]
Description=td-mta receiving-only SMTP server
After=network.target

[Service]
Type=exec
User=td-mta
Group=td-mta
UMask=0077
RuntimeDirectory=td-mta
RuntimeDirectoryMode=0700
ExecStart=/opt/td-mta/bin/td-mta serve --smtp-only --config /etc/td-mta/config
Restart=on-failure
RestartSec=5s
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
AmbientCapabilities=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
KillMode=control-group
TimeoutStopSec=30s
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

The capability grant lets the nonroot process bind port 25; for a private
unprivileged-port pilot, remove AmbientCapabilities and set an empty
CapabilityBoundingSet. This example uses the execution identity and runtime
directory behavior defined by [systemd.exec](https://github.com/systemd/systemd/blob/main/man/systemd.exec.xml).
It sets no application memory limit. The host's effective slice and unit
limits still apply.

`Type=exec` means the binary executed, not that storage recovery or listener
startup completed. Check readiness with the command above as the service
UID; do not make a dependent service treat systemd's active state as SMTP
readiness. There is no systemd notification or socket-activation support.
No ExecStartPost probe is used: a single immediate probe races startup.
No ExecStop command is used: the current stop command acknowledges a request
without waiting for process exit, while systemd proceeds to terminate
remaining processes when ExecStop returns. These lifecycle rules follow
[systemd.service](https://github.com/systemd/systemd/blob/main/man/systemd.service.xml).

Use the host's `systemd-analyze verify` on the installed unit before enabling
it, then `systemctl daemon-reload` and `systemctl enable --now td-mta.service`.
Inspect `journalctl -u td-mta.service` and the readiness probe. Failed startup
can enter the host's restart rate limit; repair configuration/material first,
then use `systemctl reset-failed td-mta.service` and start again.

For maintenance, `systemctl stop td-mta.service` suppresses automatic restart
and waits for termination. SIGTERM requests orderly receiver shutdown;
systemd may escalate to SIGKILL after the configured timeout. That escalation
uses abrupt recovery rather than finishing the orderly stop.
Confirm the unit stopped before replacing files or using
offline store commands. A manual `td-mta stop` exits cleanly and is not
restarted by this unit's on-failure policy, but its acknowledgement alone is
still not a maintenance barrier. The journal owns captured lifecycle output;
`[paths] logs` remains a required, unused private root.

### td-svc on a separately provisioned host

This is a high-port pilot example, not td system-image integration. The
current image does not package or provision td-mta. It requires installed
td-svc and td-login, an administrator-provisioned service identity with the
exact `!td-service` shadow class, and all mail roots provisioned before the
unit becomes eligible. Recreate `/run/td-mta` at each boot before starting
it. If provisioning is a td-svc oneshot, add that real unit as a `requires=`
dependency in the complete table. Do not reference an undeclared placeholder.

Choose a dedicated infrastructure UID distinct from UID 1000 and the
application UID range; td-login's service path retains the supervisor's
service cgroup for such an identity. Its verified credential switch clears
held capabilities, so this example cannot grant port-25 binding. Configure
a direct SMTP listener on an unprivileged port such as 2525. An Internet
port-25 deployment needs a separately reviewed binding arrangement.
[td-login's threat model](../td-login/THREAT-MODEL.md) owns the identity and
credential-switch rules; do not use a passwordless human account instead.

Merge this stanza into the host's single `/etc/td-svc.conf`:

```ini
[td-mta]
type=daemon
exec=/bin/td-login exec-service-as td-mta -- /opt/td-mta/bin/td-mta serve --smtp-only --config /etc/td-mta/config
ready=/bin/td-login exec-service-as td-mta -- /opt/td-mta/bin/td-mta status --json --require-ready --runtime /run/td-mta
ready-timeout=300
restart=on-failure
stop-timeout=30
log=/var/log/svc/td-mta.log
console=no
```

Validate the entire table with `td-svc check -f /etc/td-svc.conf` before
loading it. The readiness command runs as the service UID and probes the
actual receiver. Choose the startup timeout to cover measured cold recovery;
its expiry is a failed readiness decision, not evidence of a stopped process.
The example leaves resource limits unset. td-svc's own defaults and platform
accounting contract still apply.

`td-svc stop td-mta` requests signal-based stopping; poll `td-svc status td-mta`
until it reports stopped before maintenance. The stop command itself does
not wait. SIGTERM requests orderly receiver shutdown; a supervisor timeout
may force termination, with the accepted-mail recovery described above.
`td-svc start td-mta` starts it again; `td-svc
restart td-mta` likewise uses signal-based teardown. Automatic on-failure
restart can enter td-svc's capped hold; after repairing the cause, an explicit
start/restart clears that hold. The private mail stop command and a completed SIGTERM/SIGINT stop end
serve cleanly, so on-failure does not restart either, even when the signal
came from outside the supervisor. Neither supervisor example claims
end-to-end host qualification here. [td-svc/DESIGN.md](../td-svc/DESIGN.md) owns readiness,
restart, stop completion and capture behavior.

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
