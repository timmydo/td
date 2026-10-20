# PostgreSQL service deployment notes

The executable serves receive, JMAP and relay profiles from PostgreSQL. The former
SQLite receiver procedure and its local initialization, verification,
backup/restore and file-configuration commands are removed. There is no
existing deployment or SQLite data conversion requirement.
[POSTGRES.md](POSTGRES.md) owns database preparation and current support;
[CONFIG.md](CONFIG.md#foreground-postgresql-service) owns foreground serving.
Relay scheduling is active; configuration reload requires process restart.

## Install for the current host user

From the checkout, run `./install-td-mta`. It builds a native release with
verified, pinned mail dependencies, then installs `~/.local/bin/td-mta`.
`--prefix /absolute/path` selects another prefix. Cargo, Rust and a C
compiler must be available; the builder handles the same host toolchain
preparation as its mail checks. Dependency preparation may fetch declared
archives; compilation is frozen/offline.

This is a host build, not the portable release below or a source-bootstrapped
td artifact. Retain the host runtime libraries/toolchain generation it links
against (including Guix profile packages). Each installation retains a new
release under `PREFIX/lib/td-mta/releases/`, with debug information inside
the executable, the build command receipt, manifests, lock and dependency
notices. The `PREFIX/bin/td-mta` link selects the new release. Running services
keep their executable until restarted. Older releases are not deleted.

After preparing the database and selecting profiles, generate service files:

```text
./install-td-mta --services-only --connection-file /home/alice/.config/td-mta/runtime-connection --state-dir /home/alice/.local/state/td-mta --profile frontend --profile relay
```

Omit `--services-only` to build/install and generate in one invocation.
Each profile gets separate private runtime/scratch directories. Both systemd
user units and Shepherd service definitions are written under
`PREFIX/share/td-mta/services/`. Matching files may be reused; edited files
are never overwritten. Only paths enter the definitions, never credentials.
Installation does not initialize a database, grant privileges, change its
selected configuration, or start a service.

For systemd, use the printed `systemctl --user enable --now /absolute/unit`
command; `systemctl --user status`, `stop`, and `restart td-mta-PROFILE` manage
it, and `journalctl --user -u td-mta-PROFILE` reads its logs. Persistence beyond
logout/reboot requires the host's user-manager policy, such as lingering.
For Guix/Shepherd 1.0 or newer, load the printed `.scm` with `herd load root PATH`, then
`herd start td-mta-PROFILE`. Use `herd status`, `stop`, and `restart` for
management. Logs go to `STATE/PROFILE/service.log`; Shepherd owns their
rotation. For login startup, load the definition and start the service from
your user Shepherd configuration (or equivalent Guix Home service). Loading
at runtime alone does not persist it across a Shepherd restart.

These definitions run as the current user. A system deployment uses a dedicated
unprivileged identity: install/build as that identity and adapt the unit with
`User=`, `Group=` and the system manager's target, or the Shepherd constructor's
`#:user`/`#:group`. Provision ownership and low-port access separately; do not
run the engine as root. Other supervisors can directly run the foreground
command below and send SIGTERM to stop it.

Supervisor `active` means the process started, not that mail is ready. Require
`td-mta status --json --require-ready --runtime STATE/PROFILE/runtime` before
using it. PostgreSQL must already be available; use the host's PostgreSQL
service, or a separately supervised private cluster. The MTA installer never
assumes ownership of an existing cluster. A failed startup is retried by the
service manager. Readiness does not depend on remote delivery succeeding.

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

After selecting a validated receive, JMAP or relay profile, start it explicitly:

```text
/opt/td-mta/bin/td-mta serve --connection-file /etc/td-mta/runtime-connection --profile cloud --runtime /run/td-mta --scratch /var/lib/td-mta-scratch
```

The profile name must match the selected database configuration. JMAP-only
profiles need no SMTP listener and can submit mail to a selected relay profile.
A relay-only profile needs no inbound listener. It runs one route worker;
concurrent instances coordinate through the database route lease. Profiles
may run on the same host, each with separate runtime and scratch directories.
A supervisor must consume stdout/stderr and
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
The private control socket also provides the relay controls below. Administer credentials
through `postgres device-create`, `postgres device-list` and `postgres
device-revoke`, using the explicit database/account arguments in POSTGRES.md.

Configuration and certificate changes require validated database versions,
selection and process restart. Automatic certificate renewal and live reload
are not implemented. The native executable fixture qualifies separate
receive/JMAP processes, durable SMTP delivery, verified HTTPS credential
acceptance and revocation, control stop and signal-based restart cycles.
It does not qualify a deployment host or supervisor.

## Relay clock and route controls

Run these commands as the relay service identity against its runtime path:

```text
/opt/td-mta/bin/td-mta queue-clock-status --runtime /run/td-mta-relay
/opt/td-mta/bin/td-mta queue-clock-confirm --runtime /run/td-mta-relay
/opt/td-mta/bin/td-mta queue-route-status --runtime /run/td-mta-relay
/opt/td-mta/bin/td-mta queue-route-repair --runtime /run/td-mta-relay
```

Clock status reports `held` or `confirmed`. Confirmation is eligible only
after five minutes of stable database UTC/monotonic progress. Its immediate
`requested` result acknowledges the request; inspect status afterward and
request again if it was too early. Restart or clock discontinuity restores
the hold. Confirmation permits bounded overdue expiry; it does not move
retry deadlines earlier.

For a route paused by TLS or authentication failure, publish a newer repaired
configuration, restart the relay with that selection, then request route
repair. The request does not bypass the newer-generation or minimum-backoff
checks. Route status reports the last local request as `idle`, `requested`,
`repaired` or `refused`; `requested` alone is not evidence of success.
[CONFIG.md](CONFIG.md#foreground-postgresql-service) and QUEUE.md own these
controls and their bounded recovery semantics.

## Database backup and recovery

Use PostgreSQL backup and recovery tooling for the database and retain its
required connection/bootstrap material. There is no td-mta local backup or
restore command. POSTGRES.md owns maintenance, restored-epoch and uncertain
external-delivery constraints; recovery activation remains separate work.
Neither a historical audit report nor copying a local runtime/scratch
directory is a database backup or recovery authorization.
