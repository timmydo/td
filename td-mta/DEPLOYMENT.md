# PostgreSQL service deployment notes

The executable serves receive and JMAP profiles from PostgreSQL. The former
SQLite receiver procedure and its local initialization, verification,
backup/restore and file-configuration commands are removed. There is no
existing deployment or SQLite data conversion requirement.
[POSTGRES.md](POSTGRES.md) owns database preparation and current support;
[CONFIG.md](CONFIG.md#foreground-postgresql-service) owns foreground serving.
Relay and reload remain inactive.

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

## Prepare the database and instance

Use the explicit PostgreSQL preparation commands described in POSTGRES.md:
initialize or inspect the schema with its owner login, initialize the
account, import validated material and configuration versions, and publish
a retained configuration-selection request. Those commands do not start
service. Use a separate non-owner runtime login with the privileges in
POSTGRES.md section 55; `postgres runtime-check` verifies that login.
Do not give the serving process the schema-owner connection file.

Run the service as a dedicated unprivileged identity. Provision its protected
connection file and CA as POSTGRES.md specifies. Provision separate private
mode-0700 runtime and scratch directories owned by that identity, under
trusted physical ancestors. Each instance owns its own runtime and scratch;
the shared database owns durable mail, configuration and device credentials.
The runtime directory must be recreated after reboot if it is volatile.

After selecting a validated receive or JMAP profile, start it explicitly:

```text
/opt/td-mta/bin/td-mta serve --connection-file /etc/td-mta/runtime-connection --profile cloud --runtime /run/td-mta --scratch /var/lib/td-mta-scratch
```

The profile name must match the selected database configuration. JMAP-only
profiles need no SMTP listener. A supervisor must consume stdout/stderr and
use the flushed ready result or the readiness command below; successful
exec alone does not establish listener readiness. Low-port binding remains
a host provisioning responsibility for the unprivileged service identity.

## Observe and stop

Run local control commands as the service identity:

```text
/opt/td-mta/bin/td-mta status --json --require-ready --runtime /run/td-mta
/opt/td-mta/bin/td-mta stop --runtime /run/td-mta
```

Status reports lifecycle, not database integrity or future delivery success.
A stop acknowledgement records the request; wait for the serving process
to exit. SIGTERM and SIGINT also request orderly shutdown after startup.
The control socket provides status and stop only. Administer credentials
through `postgres device-create`, `postgres device-list` and `postgres
device-revoke`, using the explicit database/account arguments in POSTGRES.md.

Configuration and certificate changes require validated database versions,
selection and process restart. Automatic certificate renewal and live reload
are not implemented. The native executable fixture qualifies separate
receive/JMAP processes, durable SMTP delivery, verified HTTPS credential
acceptance and revocation, control stop and signal-based restart cycles.
It does not qualify a deployment host or supervisor.

## Database backup and recovery

Use PostgreSQL backup and recovery tooling for the database and retain its
required connection/bootstrap material. There is no td-mta local backup or
restore command. POSTGRES.md owns maintenance, restored-epoch and uncertain
external-delivery constraints; recovery activation remains separate work.
Neither a historical audit report nor copying a local runtime/scratch
directory is a database backup or recovery authorization.
