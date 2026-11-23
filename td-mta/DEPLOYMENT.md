# PostgreSQL service deployment notes

The executable serves receive, JMAP and relay profiles from PostgreSQL. The former
SQLite receiver procedure and its local initialization, verification,
backup/restore and file-configuration commands are removed. No SQLite
conversion is supported; migration from an external mail server is described
in [MIGRATION.md](MIGRATION.md).
[POSTGRES.md](POSTGRES.md) owns database preparation and current support;
[CONFIG.md](CONFIG.md#foreground-postgresql-service) owns foreground serving.
Relay scheduling is active. A serving process checks about every 30 seconds
for a newly selected configuration or a managed certificate change. A selection
change drains and reloads the service. Managed certificates update in place.

The live Azure host serving `mx.timmydouglas.com`, `mx.timmyd.info` and
`mx.882015.xyz` has a separate [production runbook](PRODUCTION.md), including
SSH access, source selection, binary updates, verification and rollback.
Its supervised `./td-mta/deploy` path takes its target only from the
operator: the host, administrator account, SSH key and public names, in two
files kept outside the repository, `target.json` on the development
machine and `/var/lib/td-mta-operator/host.json` on the host, which its
`--write-target` and `--provision-host` commands write
([Deploy target configuration](PRODUCTION.md#deploy-target-configuration)).

Existing local replicas have a separate [operations runbook](REPLICA-OPERATIONS.md)
for updates, synchronization status, integrity checks and restore evidence.

## Install for the current host user

From the checkout, run `./install-td-mta`. It builds a native release with
verified, pinned mail dependencies, then installs `~/.local/bin/td-mta`.
`--prefix /absolute/path` selects another prefix. Cargo, Rust and a C
compiler must be available; the builder handles the same host toolchain
preparation as its mail checks. Dependency preparation may fetch declared
archives; compilation is frozen/offline.

This is a host build, not the portable release below or a td target-graph
artifact. Retain the host runtime libraries/toolchain generation it links
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

`serve`, `acme` and `postgres replica-follow` each first read one byte
from `/dev/random`, which on Linux 5.6 or newer waits only until the
kernel's random generator is initialized. The read precedes each one's
database connection and signal blocking, and in `serve` its control socket;
`acme` reads its connection file and public roots first. On a host whose
generator is still initializing, the generated services therefore show an
active process that is not ready, which SIGTERM ends at once (under an
inherited default disposition). Meanwhile `serve`'s `status` reports no
running process, and `acme-status` and `replica-follow-status` show their
previous state. With `Type=exec` and no Shepherd PID file, neither the
systemd nor the Shepherd definition bounds that wait. It normally ends within the first seconds of
boot, and every later read on the host returns at once. On an older kernel
`/dev/random` can block again when the kernel's entropy estimate runs low,
including after signals are blocked; use Linux 5.6 or newer.

## Native certificate service

For a public deployment with a prepared PostgreSQL runtime login, generate
an independent certificate worker before configuring mail:

```text
./install-td-mta --services-only --prefix /home/alice/.local \
  --connection-file /home/alice/.config/td-mta/runtime-connection \
  --state-dir /home/alice/.local/state/td-mta \
  --acme-profile public \
  --acme-directory https://acme-v02.api.letsencrypt.org/directory \
  --acme-contact operator@example.org --acme-name mx.example.org \
  --acme-bind 0.0.0.0:80 --accept-acme-terms
```

This writes `td-mta-acme-public.service` and `.scm` beside the mail service
files, with a 180-second stop grace period and five-second restart delay.
No selected mail profile or existing certificate is needed. The command
prints supervisor startup and certificate-status commands; it does not
start the worker, contact a CA, change the database or select mail profiles.
`--accept-acme-terms` explicitly records agreement to the selected CA's terms
in the generated worker command. The worker validates the full CA settings
when it starts. Repeat `--acme-name` for additional exact lowercase DNS
names, up to 32. One invocation generates one worker; separate certificate
profiles need separate reachable challenge listeners.

Provision the public DNS, inbound port 80 and the service user's permission
to bind that port before starting. A higher local `--acme-bind` port needs
external forwarding from public port 80. These remain host administration,
as for the mail listener ports. The same current-user and dedicated-system
identity guidance above applies. PostgreSQL must already be running. For
Guix Home, use the generated argv and log path in a `shepherd-service` with
the database requirement, as below; do not load the registration file as a
Home service expression.

Start only the certificate worker initially. Wait for `acme-status` to
report `ready`, then import/check/select matching managed mail configuration
as described in [ACME.md](ACME.md), and start the mail services. Certificate
readiness alone does not prove public mail readiness. Mail definitions may
also be generated in the same invocation using `--profile`; generation does
not order their startup or perform initial certificate issuance. The service
name `td-mta-acme-NAME` must not also be used by a mail profile. Private worker
logs live under `STATE/.acme/NAME/`; edited definitions are preserved.

## Generated backup and health schedules

Once mail profiles, local tools and backup credentials are prepared, generate
independent scheduled operations with the same installer:

```text
./install-td-mta --operations-services --prefix /home/alice/.local \
  --state-dir /home/alice/.local/state/td-mta \
  --mail-profile frontend --mail-profile receive --mail-profile relay \
  --certificate-profile public \
  --connection-file /home/alice/.config/td-mta/runtime-connection \
  --tls-certificate /home/alice/mail-test/tls/server.pem \
  --tls-certificate /home/alice/mail-test/tls/ca.pem \
  --openssl-bin /usr/bin/openssl \
  --backup-service-file /home/alice/.config/td-mta/backup.pg_service.conf \
  --backup-service td-mta-backup \
  --backup-destination /home/alice/mail-backups --postgres-bin /usr/lib/postgresql/17/bin
```

Select any complete group independently: mail profiles, managed certificate
profiles with their connection file, private certificate files with an explicit
OpenSSL executable (such as the local setup's listener leaf and CA), or the
four backup arguments. Recovery hosts can independently select the
[weekly restore drill and daily backup pull](#pull-backups-to-a-recovery-host).
Repeat profiles/certificate
files up to 32 per kind. All paths are absolute. On a mail host, use the state directory
configured for its foreground services. A recovery-only host has its own
private operations state directory. Provision the backup
destination and credentials as described below; generation does not create or
validate them. `--tool-library-dir` optionally supplies the PostgreSQL tools'
host library directory. Their PATH contains only `--postgres-bin`; retain that
host tool installation and its runtime libraries. No secret values enter units.

Files named `td-mta-maint-*` are retained beside the ordinary service definitions.
The installer refuses conflicting operator-edited definitions before writing the
batch. Backup generation installs the current host builder as a retained helper
under `PREFIX/lib/td-mta/host-tools/` and selects it with
`PREFIX/bin/td-mta-operator`; the source checkout is not a runtime dependency.
The helper is host control-plane code, not a target distribution artifact.
Prior helper versions remain available. This command does not rebuild td-mta.

For systemd, run each printed `systemctl --user link` for the oneshot service,
then the corresponding `enable --now` for its timer. Each check runs independently
at boot plus two minutes and every fifteen minutes thereafter. Backups run at
03:15 in the manager's local timezone with missed-run persistence. Inspect both
`systemctl --user list-timers` and the individual oneshot service results and
journals: an active timer does not mean its last job succeeded. System services
need the same explicit identity/ownership adaptation described above.

For Shepherd 1.0 or newer, load and start the generated timed services. Checks
run at minutes 0, 15, 30 and 45; backups at 03:15, using the supervisor's local
timezone. Unlike the systemd backup timer, these do not catch up missed runs.
Use each timer's `trigger` action for an immediate test and inspect its past-run
status and private log under `STATE/.operations/`. Each timer waits for its
previous invocation and limits it to 120 seconds for checks or 45 minutes for
backups. Load/start definitions from the user's persistent Shepherd configuration
for session startup; loading them once is not persistence. Guix Home requires
translating the generated timer constructor into its `shepherd-service` form,
as for foreground definitions below. Logs use Shepherd's rotation policy.

Health jobs never start or depend on a mail/backup worker: they observe existing
readiness, the managed certificate lease/failure state and thirty-day validity,
private certificate thirty-day expiry, and two-day backup freshness. Each failure
is independent. A recent archive does not prove the backup timer is enabled,
a private certificate expiry check is not a chain/hostname check, and freshness
is not archive integrity or a restore test. Backup bounds default to 1 GiB/archive,
8 GiB/destination and thirty completed archives. The schedule generator accepts
`--backup-max-archive-bytes`, `--backup-max-total-bytes` and `--backup-retain`;
omitted bounds keep their defaults. For example, `--backup-max-total-bytes
4294967296 --backup-retain 7` limits a small host to 4 GiB and seven completed
backups. The byte ceiling may retain fewer than seven; it must cover two maximum
archives plus 64 KiB. These are directory limits, not filesystem free-space
reservations. The same validation applies to direct backup runs and generated
systemd/Shepherd jobs. Unknown, duplicate, invalid or unused backup settings
refuse before generation. Retain existing definitions before regenerating a
changed schedule, then explicitly replace/reload the installed unit.
The command's lock also refuses
overlapping manual invocations. Supervisors cap wall time, and the backup helper
retains its own output and tool-time bounds. Configure host memory/process limits
as appropriate. Off-host copies, free-space checks and failure notifications
remain operator responsibilities; generation sends no mail or alerts.

## Persistent Guix Home services

Use `home-shepherd-service-type` in the Home configuration, rather than
editing Guix's generated `~/.config/shepherd/init.scm`. Preserve the existing
Home packages and services. If the source configuration is stale, recover the
installed `~/.guix-home/configuration.scm` into a new source file before
adding mail services; do not reconfigure from an older package list.

Add `(gnu home services shepherd)` and `(guix gexp)` to the configuration's
imports. The following constructor returns a Home service for one previously
prepared profile; `root` and `prefix` are absolute operator-selected paths.
Append its returned service to the existing `home-environment` services:

```scheme
(define (mail-profile root prefix name)
  (let* ((base (string-append root "/" name))
         (args (list (string-append prefix "/bin/td-mta") "serve"
                     "--connection-file" "/home/alice/.config/td-mta/runtime-connection"
                     "--profile" name
                     "--runtime" (string-append base "/runtime")
                     "--scratch" (string-append base "/scratch"))))
    (simple-service
      (string->symbol (string-append "mail-" name))
      home-shepherd-service-type
      (list
        (shepherd-service
          (provision (list (string->symbol (string-append "td-mta-" name))))
          (documentation "Local mail profile; td-mta status checks readiness.")
          (respawn-delay 5)
          (requirement '(local-mail-postgres))
          (start #~(make-forkexec-constructor '#$args
                     #:log-file #$(string-append base "/service.log")
                     #:file-creation-mask #o077))
          (stop #~(make-kill-destructor SIGTERM #:grace-period 120)))))))
```

`local-mail-postgres` must name a separately declared Home Shepherd service
for the private cluster. Its start constructor runs the retained PostgreSQL
`postgres -D /absolute/cluster` executable with a private log and mask 0077;
its stop constructor uses `SIGINT` and a 120-second grace period for PostgreSQL
fast shutdown. Use the actual provision name in `requirement`. For an external
or system-managed database, omit that requirement: a user Shepherd service
cannot depend on another supervisor's service. A process dependency orders
startup but does not prove PostgreSQL accepts authenticated connections yet;
MTA startup refusal and supervisor retry still apply. Set a five-second
respawn delay on both PostgreSQL and MTA Home services. Shepherd 1.0 defaults
to rapid retries and disables a service after five respawns within seven
seconds; the longer delay lets a recovering database become available
without exhausting that default retry limit. The installer-generated
Shepherd definitions use the same five-second delay. Existing service files
are preserved: explicitly add the delay to previously generated definitions.

Build the candidate with `guix home build /absolute/home.scm`. Inspect its
`profile` link and generated Shepherd definitions before applying
`guix home reconfigure /absolute/home.scm`. Reconfiguration changes the user's
Home generation; it need not replace a running manually registered service
of the same name. Stop and unload the old MTA registrations first, followed
by their private PostgreSQL registration, then load/register the newly built
service expressions and start them. Alternatively use a fresh login/session.
Never load the installer's `register-services` files as Guix Home
`shepherd-service` expressions: these are different interfaces.

Verify all selected profiles with `status --require-ready`, including after
restarting the private database. Keep the prior Home generation and release
for rollback. Home's default auto-start runs these services when its user
Shepherd starts at login; it is not a promise of unattended boot or service
survival outside the host's user-session policy. Do not run both the old
manual startup file and the Home-managed definitions on subsequent logins.

## Build and retain a release

On the build host, use the pinned archive preparation and isolated build in
[td-crypto/PORTABLE.md](../td-crypto/PORTABLE.md). The build host needs Linux
5.12 or newer for its namespace sandbox; the x86-64 standalone runtime
targets Linux 5.6 or newer. An aarch64 cross-build is also available through
`crypto-portable-cross-build`; its successful build alone does not qualify
the runtime, and `crypto-portable-emulated-runtime` runs its functional
cases under QEMU user-mode emulation, which is functional evidence only.
For Amazon Linux 2023 on Graviton, select that ARM artifact;
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
ordinary symbols and no source-line data. Keep the pair from the same build.

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
This portable build uses the pinned upstream Rust/musl kit and the pinned
Debian build root's GCC and binutils described in PORTABLE.md; it is not the
td system-image artifact.
Prebuilt standard-library/libc code and provider assembly remain explicit
profiling boundaries. This layout does not configure automatic discovery by
host debuggers, and td-profiler currently indexes only canonical `/td/store`
objects. Explicitly select the retained companion when using external tools.

## Prepare the database and instance

For a new current-user, single-host deployment, the host installer can
orchestrate the preparation below:

```text
./install-td-mta --setup-local --root /home/alice/mail-test \
  --account test@example.com --relay-host smtp.example.com \
  --relay-user test@example.com \
  --relay-password-file /home/alice/.config/mail/relay-password \
  --resolver 127.0.0.1:53
```

Install the binary first with `./install-td-mta`; use the same `--prefix` for
setup when overriding its default. PostgreSQL 17's `initdb`, `postgres`,
`pg_ctl`, `psql`, and OpenSSL must be available on the host PATH. OpenSSL
creates only the mail listeners' local CA and leaf. These are host
provisioning tools, not target recipe dependencies. The command runs
them directly, without a shell. It creates a new mode-0700 root under
trusted physical ancestors; existing roots, symlinks, privileged local
ports and conflicting local port assignments refuse. The parent must already
exist. Roots are limited to 72 bytes for the MTA control socket.

Default ports are HTTPS 54443 and incoming SMTP 55225; `--https-port` and
`--smtp-port` override them. Both bind IPv4 loopback only. PostgreSQL has no
TCP listener: it listens only on the private socket `ROOT/socket`, and
`--postgres-port` (default 35432) names its socket file
`ROOT/socket/.s.PGSQL.PORT`. Relay uses verified implicit TLS on port 465;
`--relay-port` overrides the port, not the transport. Supply the relay login
explicitly. Its password input must be a private, user-owned regular file;
its ancestors must be physical, root/user-owned and not shared-writable.
One final LF or CRLF is removed. An optional `--relay-ca-file` takes a
similarly protected trust bundle; otherwise the bundled public roots apply.
Account addresses, relay hostnames and relay users use simple ASCII names.
The resolver must be an explicit IP:port endpoint reachable by the service.

Setup creates a local CA and 90-day server leaf for the HTTPS and SMTP
listeners, initializes its own cluster, briefly starts PostgreSQL, creates
separate schema-owner and runtime logins, initializes the
schema/account/device, imports immutable materials and all three profiles,
publishes a retained selection request, and checks runtime privileges.
Temporary local trust authentication is confined to the private socket and
setup administrator. The final configuration has `listen_addresses = ''`
and `ssl = off`: PostgreSQL accepts only SCRAM owner and runtime logins
(`local` HBA lines) on its mode-0600 socket in the mode-0700 `ROOT/socket`,
the private form td-mta's Unix transport admits. The connection files use
the Unix form `postgresql://USER:PASSWORD@:PORT/td_mta?host=ROOT/socket`
(POSTGRES.md). The leaf and CA are not PostgreSQL's.
PostgreSQL is stopped before successful return. No MTA service starts and
no mail is sent. This does not provision an existing or remote database.

The root retains private command output/error receipts, credentials, source
profiles, material/configuration IDs and the exact selection request.
Passwords never enter process arguments or normal terminal output. On
failure, the owned PostgreSQL child is stopped and the partial directory
is retained for inspection; setup does not retry publication, resume partial
data or delete it. An uncertain device creation still requires inspection
and revocation before replacement, as POSTGRES.md specifies.
Each direct tool child is also armed to die if the installer is terminated;
PostgreSQL's postmaster-death handling then retires its own workers. An
interrupted cluster may need PostgreSQL crash recovery; it is never adopted
by another setup run.

`START.txt` supplies commands for the generated systemd user units or
Shepherd definitions under `ROOT/services/`, including the private PostgreSQL
service and MTA dependencies. These names support one such local deployment
per user service manager. Definitions use the installed MTA path and the
PostgreSQL executable found on PATH; retain those host installations.
Choose one manager, start the services explicitly, and require each profile's
readiness. `ROOT/mail.toml` configures td-mail with this account's private CA
(for the HTTPS listener) and device-password command. Use `td-net launch --allow-loopback` for this
local account. Startup persistence, certificate renewal and backups still
follow the procedures elsewhere in this document.

Use the explicit PostgreSQL preparation commands described in POSTGRES.md:
initialize or inspect the schema with its owner login, initialize the
account, import validated material and configuration versions, and publish a
retained configuration-selection request. Those commands do not start
service. Use a separate non-owner runtime login with the privileges in
POSTGRES.md, [Runtime database logins](POSTGRES.md#runtime-database-logins)
and [Shared runtime login
privileges](POSTGRES.md#shared-runtime-login-privileges);
`postgres runtime-check` verifies that login. Do not give the serving
process the schema-owner connection file.

Run the service as a dedicated unprivileged identity. Provision its protected
connection file as POSTGRES.md specifies. Provision separate private
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

General configuration changes require validated database versions and
selection; serving processes then drain and reload, and a restart applies them
sooner. A separately supervised ACME worker renews managed certificates.
Serving processes pick up the new generation on existing listeners, as
[ACME.md](ACME.md) specifies. The native executable fixture qualifies separate
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

The installed services keep the relay's `queue-clock` checkpoint in
`STATE/PROFILE/runtime`. Custom service managers must retain that
directory across stops to resume unattended expiry. After a clean
same-boot restart with clock continuity, expiry opens after the minimum
retry interval (five minutes by default) and status reports `confirmed`.
On first activation, following a crash, or after ambiguous downtime or a
clock jump, verify database UTC and run `queue-clock-confirm` after the
stable interval. An early request
returns `refused`. For a `requested` response, check status again for
`confirmed` or `refused`. Retry a refused request after stable progress.

For a route paused by TLS or authentication failure, publish a newer repaired
configuration, restart the relay with that selection, then request route
repair. The request does not bypass the newer-generation or minimum-backoff
checks. Route status reports the last local request as `idle`, `requested`,
`repaired` or `refused`; `requested` alone is not evidence of success.
[CONFIG.md](CONFIG.md#foreground-postgresql-service) and QUEUE.md own these
controls and their bounded recovery semantics.

## Database backup and recovery

Use PostgreSQL backup and recovery tooling for the database and retain its
required connection/bootstrap material. The host installer can run bounded
logical backups; the mail daemon has no restore command. POSTGRES.md owns
maintenance, restored-epoch and uncertain
external-delivery constraints and the explicit recovery-release command.
Release quarantines pending delivery and requires fresh credentials and
reimported configuration before service activation.
Neither a historical audit report nor copying a local runtime/scratch
directory is a database backup or recovery authorization.

## Online recovery point for a schema cutover

The recovery point for a schema migration is the old database after its
last accepted write. Prepare and verify the large part of the backup while
mail is online. At cutover only the remaining write-ahead log (WAL) needs
to reach the recovery copy. A logical dump taken earlier cannot replay that
interval. The daily logical backup and its freshness checks serve a separate
purpose.

**Qualify the host while it serves.** Use PostgreSQL 17 tools matching the
server. A physical restore rewinds every database and role in the cluster.
Use a dedicated mail cluster; a shared cluster needs a separately approved
whole-cluster recovery and writer fence. Keep the base
and WAL on protected recovery storage with private directories (0700),
independent of the production data disk. Keep bootstrap configuration,
extensions and any external keys separately. Reserve space for the base,
an independent working copy and a bounded WAL spool. Use a storage quota
or monitor the spool against a byte budget and free-space reserve for the
other backups. Set a preparation deadline and a separate maximum fence wait.
On failure, interruption or timeout, stop and wait for the receiver started
by this attempt, then stop its isolated trial if running. Account for the
incomplete attempt or remove only its disposable files; preserve any earlier
verified recovery set. Qualify this procedure on a single primary and
timeline, with ordinary logged mail tables and no
external tablespaces. A failover invalidates this cutover attempt.

Provide a dedicated physical-replication connection through local peer
authentication or verified TLS and a private libpq service/passfile. Keep
replication authority out of the mail runtime. Check `wal_level` is
`replica` or higher, full-page writes are enabled and three WAL senders
are free during preparation.
The receiver's application name must be excluded from synchronous standby
selection so a backup cannot delay mail commits. Have a separately
authorized SQL operator for the restore point and WAL switches. Rehearse
on a private copy using the installed binaries and extension libraries.

**Prepare while mail is online.** Use a new private directory per attempt
and a unique restore point name, such as `td_cut_` followed by a fresh UUID.
In the commands below, `work` is the absolute private recovery directory and
`PGSERVICEFILE` names the private file defining `cutover_repl`. Start the
receiver first; verify its PID is alive and its entry in
`pg_stat_replication` is streaming from the intended primary.

```sh
umask 077
set -eu
mkdir -m 700 "$work/wal"
pg_receivewal -w -n --synchronous -D "$work/wal" \
  -d 'service=cutover_repl application_name=td_cutover_wal' \
  >"$work/receiver.log" 2>&1 &
receiver_pid=$!
# After confirming the receiver is streaming:
pg_basebackup -w -d 'service=cutover_repl' -D "$work/base" \
  --wal-method=stream --no-slot
pg_verifybackup "$work/base"
```

Run the stages under the same supervisor with the cleanup above armed
throughout. Use default durable writes. Both streams are slotless; monitor
receiver health and lag. A recycled or missing segment invalidates the
attempt. Keep the verified base immutable. Make a fresh
independent copy for trial replay (a regular copy or copy-on-write reflink,
never hard links). On the copy use a private local socket and HBA, disable
TCP listeners and outbound connections, and run no mail services. Use a
reviewed recovery configuration, including compatible server limits and an
empty copy-local `postgresql.auto.conf`. Remove inherited recovery signals
on the copy. Keep the live data directory, external configuration paths,
archive commands, replication connections and tablespaces out of the
trial instance.

Create `standby.signal` on the copy. Start its PostgreSQL with hot standby
enabled, `archive_mode=off`, empty `primary_conninfo` and
`primary_slot_name`, `recovery_target_timeline='current'`,
`recovery_target_action='pause'`, and `recovery_target_name` set to the
chosen name. Set its `restore_command` to copy completed segments from the
private WAL directory, e.g. `cp /private/cutover/wal/%f %p` with a
shell-safe absolute path. The command must fail for unavailable segments.
Use only completed WAL segment names. Keep the receiver running. After
the base finishes, create a rehearsal restore point on the source with
`pg_create_restore_point` using a name distinct from the fence target.
Switch WAL, require the completed segment containing the returned LSN, and
verify the copy replays through that LSN while it is still in recovery.
This exercises the archive even when no mail arrived during preparation.
Verify source and copy system identifiers and
timeline against the base and record the base manifest checksum. A readable
copy alone does not prove it caught up.

**Seal at the writer fence.** Stop mail services and fence all application
and maintenance sessions as for an ordinary schema upgrade. Keep timers,
automatic restarts and other writers fenced through candidate qualification.
Verify zero
in-flight writers and zero prepared transactions. Keep only the authorized
operator and WAL capture connections. On the same primary create the
previously chosen restore point exactly once and save its returned LSN in
the private receipt. Use `psql -X -v ON_ERROR_STOP=1` with an explicit
source connection and the variable `point` set to the name. If the result
is lost, abandon this attempt and target name before retrying.

```sql
SELECT pg_create_restore_point(:'point') AS fence_lsn \gset
SELECT :'fence_lsn' AS fence_lsn, pg_walfile_name(:'fence_lsn') AS fence_segment;
SELECT pg_switch_wal();
```

Require the completed fence segment in the retained WAL archive. On the
isolated copy verify the following with `fence_lsn` set to the source result:

```sql
SELECT pg_is_in_recovery() AND pg_get_wal_replay_pause_state() = 'paused'
       AND pg_last_wal_replay_lsn() = :'fence_lsn'::pg_lsn AS at_fence;
```

Require true and compare the pre-upgrade schema, epoch and selected mail
invariants against the fenced source. Retain the base, contiguous completed
WAL through that point, external bootstrap material and private receipt
(source identity, timeline, base checksum, target name, LSN, segment and
comparison results). Preserve these inputs independently of the trial copy
through the rollback window. Send SIGINT to the recorded receiver PID,
wait for a successful exit and check its log. Persist the retained archive,
manifest and receipt, including directory entries, before migration (on
Linux, `sync -f "$work"` for that filesystem and separately for any other
recovery volume). Stop the trial copy while it is paused. Never resume it
as a mail server.
Only then run the migration on the original primary. A missing segment,
target mismatch or timed-out check leaves the migration unstarted. Resume
the old service on its unchanged database if the window cannot be met.
If WAL capture is unavailable, use a fresh logical dump in a window long
enough for its full restore and comparisons. For that cutover keep all mail
writers and maintenance jobs fenced continuously from before the dump
through the restore check, migration and candidate checks. Only a standalone
restore drill may reopen the source immediately after capturing its inventory.
For actual recovery, rebuild from the retained base and WAL on an isolated
copy, target the recorded timeline and restore point, and verify the LSN
and comparisons before any promotion. Fence the old primary and SMTP egress
and follow POSTGRES.md section 6 for a fresh epoch, credentials and delivery
reconciliation before serving. After reopening writes, restoring only to
the cutover point would discard subsequent accepted mail. Preserve and
reconcile that later history before considering such a restore.

## Local logical backup and an isolated restore drill

PostgreSQL's `pg_dump --format=custom` takes a consistent logical database
snapshot while service continues. See PostgreSQL 17
[pg_dump](https://www.postgresql.org/docs/17/app-pgdump.html) and
[pg_restore](https://www.postgresql.org/docs/17/app-pgrestore.html).
Use PostgreSQL 17 client tools for the
qualified local setup. This includes td-mta's bodies, metadata, history,
configuration, device verifiers and queue receipts; the archive contains
private keys and relay credentials too. Protect the backup directory as 0700
and its files as 0600. A backup on the service disk does not protect against
loss of that disk; copy retained backups to separately protected storage.

Provision a private libpq service file and password file for the owner login,
separately from td-mta's connection-file syntax. For a local cluster on its
private socket the service contains `host` (the absolute socket directory,
which makes libpq use the Unix socket), `port`, `dbname`, `user`, `passfile`,
and `connect_timeout=10`, with no SSL settings. The referenced mode-0600
password file uses PostgreSQL's `host:port:database:user:password` format,
escaping colons and backslashes. For a socket connection libpq matches the
`host` field against the socket directory exactly as `host` names it, and
also against `localhost` only when that directory is libpq's compiled-in
default; write the directory there.
Do not put the password or the full connection URI in command arguments or
shell history. Clear conflicting ambient `PG*` variables before using these
examples; retain only the explicit service selection for the dump:

```sh
(
  set -eu
  umask 077
  mkdir /home/alice/mail-backups/backup-001
  PGSERVICEFILE=/home/alice/.config/td-mta/backup.pg_service.conf \
  PGSERVICE=td-mta-backup \
  pg_dump --no-password --format=custom \
    --file=/home/alice/mail-backups/backup-001/mail.dump
  pg_restore --list /home/alice/mail-backups/backup-001/mail.dump
)
```

Create each backup directory anew; do not reuse the example's suffix or
truncate an older archive. Check command exit status before calling a backup
complete. Retain a checksum, timestamp, PostgreSQL version and application
release/schema version alongside the archive. `--list` alone checks the
archive directory, not whether its data can be restored.

Retain protected bootstrap material separately: database login credentials
for the owner and each service, role/grant provisioning, the mail
listeners' local CA key if used, profile
source files, current selection receipts, the td-mail device
password and its per-account CA configuration, and the matching executable
release. `pg_dump` does not include cluster roles, server configuration,
bootstrap files or mail accepted after its snapshot. A restored device
verifier cannot reconstruct the client password. Keeping these files beside
a dump is convenient, but an independently recoverable copy is still needed.

### Bounded local backups

The host installer also provides a bounded logical backup command. Provision
PostgreSQL 17 `pg_dump` and `pg_restore` on PATH, the private libpq service and
password files described above, and an existing owned mode-0700 destination
with trusted physical parent directories. Then run:

```sh
./install-td-mta --backup \
  --service-file /home/alice/.config/td-mta/backup.pg_service.conf \
  --service td-mta-backup --destination /home/alice/mail-backups
```

A retained host builder can run the same operation without compiling:
`/absolute/td-builder install-td-mta --backup ...`. This is host control-plane
backup orchestration, not a target distribution executable or a database
restore command. The operator's service selection controls which database is
backed up; use the owner login over the private socket.

Defaults retain at most 30 completed archives, cap each archive at 1 GiB and
cap total directory bytes at 8 GiB. Override these with `--retain`,
`--max-archive-bytes` and `--max-total-bytes`. The total must accommodate two
maximum-size archives plus 64 KiB of bounded receipts and diagnostics. A
private advisory lock excludes concurrent runs. Old completed archives are
pruned to admit the next attempt, but the newest completed archive is never
removed to make an attempt fit. Unknown regular files consume the budget;
links, non-private files and more than 4096 directory entries refuse a run.
Admission also reserves four entries for scratch, diagnostics and publication;
creating the lock cannot exceed the directory ceiling.
These are byte-count admission bounds, not filesystem block quotas.

Each PostgreSQL tool has a ten-minute deadline; its streamed output is capped
before being written. Failed tool diagnostics retain only the first 16 KiB
in private `last-error`; normal terminal output never includes their content.
The dump is published under a new filename only after `pg_restore --list`
succeeds, with a JSON receipt containing timestamp, archive bytes, SHA-256
and both tool versions. This checks the archive directory, not a full
restore; the receipt explicitly records `restored: false`. The next locked
run removes interrupted scratch in the command's reserved `backup-N-N`
namespace. Do not put operator files in that namespace.

A failed attempt returns nonzero and preserves the newest completed archive.
Schedule with a supervisor that records failures and inspect backup age as
well as exit status. Local retention alone does not protect against disk
loss. Retain protected bootstrap material separately and perform isolated
restore drills before treating the backup as recoverable.

### Restore the latest scheduled backup

Run the recovery helper as an unprivileged user on a Linux host with
PostgreSQL 17. Provide two separate owned mode-0700 directories with
trusted parents. The first holds scheduled dumps and their receipts. The
second holds the disposable database and needs space for its uncompressed
tables, indexes and PostgreSQL working files. Protect both volumes at rest.
A dump includes message bodies, TLS private keys and the relay credential.
The drill requires Linux user, mount, PID and network namespaces and a
PostgreSQL 17 server, client and extensions matching the dump (including
`pg_trgm` for indexed mail). It reserves 2 GiB of free space in addition to
a scratch allowance of 12 times the archive size, with a 4 GiB minimum and
64 GiB maximum. It monitors free space and scratch usage during its one-hour
run limit. Provide a filesystem quota when a hard disk limit is required.

Install the server and client from one PostgreSQL 17 package. The helper
finds `initdb` on PATH, resolves profile links, and checks the package's
`initdb`, `postgres`, `psql` and `pg_restore` versions. Generating a
drill schedule performs the same check on `--postgres-bin` before writing
the helper or service definitions. Supported binary directories are:

- Guix's `/gnu/store/<package>/bin`, including links from a user profile;
- Debian's `/usr/lib/postgresql/17/bin` and the extracted deployment's
  `/opt/td-mta/postgresql/usr/lib/postgresql/17/bin`;
- the RPM directory `/usr/pgsql-17/bin` and the source installation's
  `/usr/local/pgsql/bin`.

The jail mounts package binaries, libraries and shared resources read-only.
Keep private configuration and live cluster data outside those directories.
On Guix the runtime also uses the immutable public `/gnu/store` for its
dynamic loader and dependencies. The host account's profile and home
directory are outside the jail.

```text
./install-td-mta --restore-drill --destination /home/alice/mail-backups \
  --scratch-directory /home/alice/mail-drill
```

The helper selects the newest completed archive, checks its original byte
count and SHA-256, and restores the same open file. An empty destination or
a damaged newest archive fails the drill. Inside a disposable namespace,
PostgreSQL listens on a private Unix socket and restores in one transaction.
An independent reader checks the recorded migration checksums against the
application's immutable migrations and the common store epoch. It compares
the set of ordinary mail tables with the catalog for the restored version,
checks mail references, and verifies chunk order, byte lengths and each
stored body's SHA-256. The drill omits
original ownership, grants, subscriptions and tablespace placement. Keep the
source role/bootstrap material for the separate recovery procedure.
On success stdout contains one JSON record with the archive name, creation
time, SHA-256, archive bytes,
account, mailbox, Email and body counts, and total verified body bytes.
Save that record and the job result. Check backup age independently with
`--backup-status`: a successful restore can be from an old snapshot.

### Pull backups to a recovery host

Use a separately protected recovery volume on the designated development
machine. Start with an empty mode-0700 copies directory and a separate
mode-0700 scratch directory. Provision a dedicated SSH identity on the
recovery host. Configure its SSH alias with the backup source hostname,
the backup-owning user, `IdentityFile`, `IdentitiesOnly yes`, and a
`UserKnownHostsFile` containing the independently verified host key.
Allow this identity on the source **only** through a restricted forced
command. For example, prefix its public key in the source user's
`authorized_keys` with the following options, substituting the actual
retained helper path and scheduled backup directory:

```text
restrict,command="/opt/td-mta/bin/td-mta-operator install-td-mta --export-latest-backup --destination /var/lib/td-mta/backups/scheduled"
```

Install the export-capable helper on the source before enabling pulls.
Before upgrading the mail schema, install a recovery helper built with the
target migration pins on the recovery host. Its drill accepts earlier
supported ledgers as well as the new one.
The recovery host needs read access through this identity only. Keep the
production database login on the source. Run the first copy, age check
and drill under the identity that will own the scheduled jobs:

```text
./install-td-mta --pull-backup --host td-mta-backup-source \
  --destination /home/alice/recovery/copies
./install-td-mta --backup-status --destination /home/alice/recovery/copies \
  --maximum-age-seconds 172800
./install-td-mta --restore-drill --destination /home/alice/recovery/copies \
  --scratch-directory /home/alice/recovery/drill
```

The pull uses noninteractive SSH with strict host-key checking and no
forwarding. It verifies the original receipt and waits for the exporter's
successful exit before publishing the new dump. It keeps the latest two
completed generations. Allow room for a third during transfer. The
per-archive limit defaults to 1 GiB and can be raised with
`--max-archive-bytes` if the source limit rises. For an independently
protected transport, `--export-latest-backup --destination SOURCE` writes
the receipt and dump to stdout and `--receive-backup --destination COPIES`
reads that stream from stdin. Handle the stream as secret-bearing data.

To generate the recovery host's systemd or Shepherd jobs:

```text
./install-td-mta --operations-services --prefix /home/alice/.local \
  --state-dir /home/alice/.local/state/td-mta-recovery \
  --backup-destination /home/alice/recovery/copies \
  --backup-drill-scratch-directory /home/alice/recovery/drill \
  --postgres-bin /usr/lib/postgresql/17/bin --backup-pull-host td-mta-backup-source
```

If the source archive ceiling exceeds 1 GiB, supply the same
`--backup-max-archive-bytes` when generating the pull job.
Enable the generated jobs under the recovery user after the initial drill
succeeds. On systemd the pull runs at 04:15 local time Monday through
Saturday. On Sunday the pull and drill are both scheduled at 05:15, with the
drill ordered after the pull. Missed runs catch up. Starting the drill manually
also requests a pull. A failed pull still permits checking the last local copy.
On Shepherd the pull runs daily at 04:15 and the drill runs Sundays at 05:15.
Both supervisors check the source snapshot age every 15 minutes and report a
copy older than 48 hours. Inspect the individual job results. The newest
successful drill identifies exactly which snapshot was tested. A weekly success provides no
evidence about a later archive. To drill archives already supplied locally,
omit `--backup-pull-host`. Retain the separate bootstrap material listed
above, including credentials for the deployment's per-service roles. Actual
service recovery also requires fencing the old writer, establishing a fresh
epoch, reviewing configuration and quarantining uncertain deliveries under
[POSTGRES.md](POSTGRES.md).

### Compare a fenced source with a manual restore

A drill restores into a fresh cluster with no TCP listener and a private Unix
socket. Use absolute paths without whitespace for this example's `pg_ctl -o`
argument, a new mode-0700 root, and separate service/data directories. The two
NOLOGIN role names below match the local deployment; reproduce the actual
source role names when they differ. Do not enable a source runtime password
or copy supervisor definitions into this cluster.

```sh
(
  set -eu
  umask 077
  mkdir /home/alice/mail-restore-001
  mkdir /home/alice/mail-restore-001/socket
  initdb -D /home/alice/mail-restore-001/data -U restore_admin \
    --auth-local=trust --auth-host=reject --no-locale -E UTF8
  pg_ctl -D /home/alice/mail-restore-001/data \
    -l /home/alice/mail-restore-001/postgres.log \
    -o "-h '' -k /home/alice/mail-restore-001/socket -p 55439" -w start
)
```

The socket's private ancestor is the local authentication boundary. In a
separate shell with the source's `PG*` variables cleared, set only the drill's
connection variables and create the empty destination:

```sh
(
  set -eu
  export PGHOST=/home/alice/mail-restore-001/socket PGPORT=55439
  export PGUSER=restore_admin PGDATABASE=postgres
  psql -X -v ON_ERROR_STOP=1 -c 'CREATE ROLE td_mta_owner NOLOGIN'
  psql -X -v ON_ERROR_STOP=1 -c 'CREATE ROLE td_mta_runtime NOLOGIN'
  createdb --owner=td_mta_owner restored_mail
  pg_restore --exit-on-error --single-transaction --dbname=restored_mail \
    /home/alice/mail-backups/backup-001/mail.dump
  psql -X -v ON_ERROR_STOP=1 --dbname=restored_mail \
    -c 'ALTER DATABASE restored_mail SET default_transaction_read_only = on'
)
```

Verify the expected tables, row counts, message IDs, immutable bodies and
queue receipts, not just process exit. For an exact before/after comparison,
briefly stop all MTA writers before the dump and source inventory, then start
them again after those two observations. Compare every table's ordered row
encoding under identical PostgreSQL session settings (including UTC and ISO
dates), retaining only hashes/counts in the diagnostic report. Treat raw
inventory output as secret-bearing. This is a database-copy check, not
whole-mail semantic validation or permission to start a restored service.

Stop the drill with `pg_ctl -D /home/alice/mail-restore-001/data -m fast -w stop`
and remove only its disposable cluster after retaining the report. Never run
`td-mta serve` against the drill or point an existing profile at it. An actual
recovery additionally requires POSTGRES.md section 6's old-instance fencing,
fresh epoch, credential/configuration review and delivery quarantine; the
current local drill supplies none of that activation authority.

## Scheduled operator health

Check each selected mail profile with `td-mta status --json --require-ready`
and its private runtime directory. Check each native certificate worker with
`td-mta acme-status --connection-file PATH --certificate-profile NAME
--require-ready --minimum-validity-seconds 2592000`. Keep these as separate
supervisor commands so a failing profile cannot be hidden by another healthy
one. Public ACME health does not cover a local setup's private listener
certificate; check that leaf and its CA with the expiry commands in
"Renew a local private certificate". A local PostgreSQL uses no certificate.

For a bounded backup destination, use:

```text
./install-td-mta --backup-status --destination /home/alice/mail-backups \
  --maximum-age-seconds 172800
```

This read-only command exits zero only for a completed archive with a matching
receipt and file length, whose recorded creation time is no more than two days
old (the default), is not in the future, and has no retained `last-error` tool
diagnostic. The age bound accepts 1 through 31536000 seconds. An empty directory,
invalid receipt, missing archive, stale backup or I/O failure exits one. The
JSON result includes age, archive count and directory bytes. It does not hash
archive contents, restore data, or certify recoverability. Filename sequence
numbers do not substitute for the receipt's creation time after clock rollback.
A concurrent producer may change files during this observation; retry a failed
check after that bounded job finishes. Inspect the backup supervisor's result
as well: failures before tool startup need not create a `last-error` file.

Schedule checks independently of the services they observe, retain their exit
status in the supervisor, and arrange operator notification appropriate to the
host. A timer alone does not send an alert. Check free disk space and off-host
backup copies separately; a recent local archive does not cover VM loss.

## Existing TCP/TLS local installations

Local setups made before PostgreSQL moved to its private socket listen on
loopback TCP with a private TLS leaf, and may run the generated
`td-mta-maint-postgres-tls-renew` job. New operator helpers no longer have
that renewal mode, so disable the job in its supervisor and delete its
generated `td-mta-maint-postgres-tls-renew.service`, `.timer` and `.scm`
under `PREFIX/share/td-mta/services` before installing a new operations
helper; the retained older helper still serves it until then. td-mta
refuses a TCP connection file, so migrate before running it, retaining
credentials, epochs and mail data:

1. Stop the MTA services, scheduled jobs that use the database, then
   PostgreSQL.
2. In `ROOT/postgresql.conf` set `listen_addresses = ''`, `ssl = off`,
   `unix_socket_directories = 'ROOT/socket'` and `unix_socket_permissions =
   0600`, and remove `ssl_cert_file` and `ssl_key_file`. Create
   `ROOT/socket` mode 0700 if it does not exist.
3. Replace the `hostssl` lines in the cluster's `pg_hba.conf` with `local
   td_mta td_mta_ROLE scram-sha-256` for each login, ending with the
   existing reject rules.
4. Rewrite the owner and runtime connection files (mode 0600) to
   `postgresql://td_mta_ROLE:PASSWORD@:PORT/td_mta?host=ROOT/socket`, and
   the backup libpq service and password files as in "Local logical backup
   and an isolated restore drill".
5. Start PostgreSQL, run `postgres runtime-check` and `schema-inspect`, start
   the services and require each profile's readiness, then run a backup.

The leaf and CA remain the mail listeners' certificates; renew them as
below. Retain the old files until validation is complete.

## Local replica transport

Fresh `--setup-replica` installations use private Unix sockets for both
PostgreSQL and JMAP. They generate no CA, certificate, private TLS key, or
renewal job. The local JMAP socket is `ROOT/jmap/socket`; PostgreSQL uses
`ROOT/socket/.s.PGSQL.PORT` (default PORT 35433). Both directories are private,
and sockets are mode 0600. PostgreSQL still authenticates owner, publisher and
reader roles with SCRAM; td-mail still authenticates with its separate local
device password. The source synchronization connection remains verified HTTPS.

The previous replica-specific TLS renewal options and job generation have
been retired. Existing TLS installations need a deliberate transport migration
before their old job is disabled: retain their credentials, epochs, copy state
and mail data; stop the sync and reader; select a `local_jmap` profile; configure
private PostgreSQL sockets and SCRAM-only local role rules; update protected
connection files and the td-mail account; restart and verify discovery,
mailboxes, body reads and sync progress. Remove the old renewal service from
its persistent supervisor configuration. Retain recovery files until validation
is complete. Do not run fresh setup over an existing root: it intentionally
refuses existing data. A pending old renewal journal is retained for
inspection; no current helper renews or overwrites it.

Public HTTPS/ACME is independent of this local transport and remains enabled
where configured.

## Renew a local private certificate

A local `localhost` deployment uses its explicitly trusted private CA;
public ACME issuance is not involved. Check both the CA and leaf expiration
with `openssl x509 -in FILE -noout -dates`, and check the renewal threshold
with `openssl x509 -in FILE -checkend 2592000 -noout`. Nonzero means renewal
is due (or the input could not be checked); it is not an instruction to
ignore verification. Schedule the operator check before either certificate
expires; `--operations-services --tls-certificate` can schedule that
expiry check. Renewal is manual and, because the leaf is an imported mail
material, requires explicit configuration selection.

This leaf serves the HTTPS and SMTP listeners; PostgreSQL does not use it.
Retain the CA certificate/key and prior server generation privately. Create
new files in a new mode-0700 directory with umask 077. A 90-day leaf under an
existing CA can be prepared with the following commands; select a new random
positive serial and retain it before signing. The CA must remain valid for
the entire new leaf's intended lifetime:

```sh
(
  set -eu
  openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -noenc \
    -keyout server.key -out server.csr -subj /CN=localhost
  openssl x509 -req -in server.csr -CA /absolute/local-ca.pem \
    -CAkey /absolute/local-ca.key -set_serial 0xRETAINED_RANDOM_SERIAL \
    -days 90 -extfile extensions -out server.pem
  openssl verify -CAfile /absolute/local-ca.pem \
    -verify_hostname localhost server.pem
)
```

The `extensions` file for this local-only profile is:

```text
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,IP:127.0.0.1
```

The short-lived development CA may itself need replacement before issuing a
leaf. An explicit local CA renewal can retain its key and subject while
issuing a longer-lived self-signed CA certificate with critical CA:TRUE and
keyCertSign,cRLSign usage. Verify the old leaf against that candidate before
replacing clients' CA files; do not assume matching subject names imply
matching keys. A different CA key requires an explicit overlap/trust rotation
for every client. Neither operation changes bundled public roots or grants
trust to other td-mail accounts. Keep CA expiry on the operator's schedule.

Use `postgres material-version` to retain separate chain/key IDs, then
import `server.pem` as `certificate-chain` and `server.key` as
`private-key`, using `material-import` with the owner connection file and
expected epoch. Replace only the certificate material references in copies
of every affected profile. Generate/retain a configuration version;
`config-import` it with every profile and `config-check` it. Prepare and
retain a `config-selection-prepare` request using the currently selected
generation, then publish that exact request with `config-select`. Resolve
uncertain results with `config-selection-inspect` and the same retained
request. POSTGRES.md, [Material operator
commands](POSTGRES.md#material-operator-commands), [Configuration import,
check and snapshot
commands](POSTGRES.md#configuration-import-check-and-snapshot-commands) and
[Configuration selection
commands](POSTGRES.md#configuration-selection-commands) specify these
commands; do not invent a fresh request to hide uncertainty.

Certificate files on disk do not update td-mta's immutable database materials.
Selecting a configuration advances the database-wide generation and fences
every instance using that selection. Restart all of them, including unchanged
listener-free relay profiles, and require ready status from each instance.
Verify new JMAP HTTPS and SMTP STARTTLS connections with name/chain checks,
and confirm that they present the intended certificate and expiration. Keep
the old files and selected configuration receipt for diagnosing a failed
rotation. Do not claim live reload or automatic ACME support from this manual
procedure.

## Add or edit a mailbox

Run administration as the installation's service owner with its protected
owner connection file. Choose a fresh operation filename in a private
mode-0700 operator directory. For an already configured domain:

```text
td-mta postgres account-create --connection-file /private/owner-connection \
  --address person@example.net --name "Person" \
  --operation-file /private/operators/person-create.operation
```

The result identifies the new account and selected configuration. Restart the
frontend, receiving and relay instances using your supervisor. Then provision
a client password using the returned account ID and the store epoch reported
by `account-list`:

```text
td-mta postgres account-list --connection-file /private/owner-connection
td-mta postgres device-create --connection-file /private/owner-connection \
  --epoch EPOCH --account ACCOUNT --label "td-mail laptop"
```

Device creation outputs the generated password once. Keep it in the client's
private credential file; do not put it in a command line or ordinary log.
Configure td-mail with the HTTPS discovery URL and the new address as username.
Use `--username` during creation if a different login is wanted.

`account-update` changes display name/login and accepts `--outbound-profile NAME`
to select an existing relay-capable profile for that account. An empty NAME
clears the override. Previously queued mail retains its original route.
`account-alias-add` and `account-alias-remove` change receiving addresses. They each take a fresh
`--operation-file` and also require mail instance restart after selection.
Aliases are local mailbox routes, never forwarding destinations or additional
sending identities. The domain and smarthost sender authorization are configured
separately. These commands do not change DNS or provider configuration.

If a command is interrupted, retain its operation file and run:

```text
td-mta postgres account-operation-inspect --connection-file /private/owner-connection \
  --operation-file /private/operators/person-create.operation
td-mta postgres account-apply --connection-file /private/owner-connection \
  --operation-file /private/operators/person-create.operation
```

A receipt describes that operation's publication, not current service readiness.
Use `account-list` to inspect the current selection and service readiness checks
after restart. See POSTGRES.md for partial completion and concurrent-edit rules.

To grant an existing account another From address, use
`td-mta postgres account-identity-add --connection-file OWNER_CONNECTION
--account ACCOUNT_ID --address sender@example.test --name "Sender"
--operation-file PRIVATE_DIRECTORY/add-sender.operation`. List identities with
`postgres account-list`. Rename one with `account-identity-update --account
ACCOUNT_ID --identity ID --name "New name"`, or remove it with
`account-identity-remove --account ACCOUNT_ID --identity ID`; supply a fresh
private operation-file path and owner connection for each command. Restart the
affected instances after selection. Receiving aliases are separate; configure
one explicitly if the address should also accept incoming mail. Ensure your
outbound provider authorizes the new sender before using it.

## Scheduled body integrity checks

Periodic scrubbing verifies original message bytes outside the interactive
read path. It reports corruption and never repairs or deletes mail. Generate
its bounded worker and independent freshness check with:

```text
./install-td-mta --operations-services \
  --prefix /home/alice/.local --state-dir /home/alice/.local/state/td-mta \
  --scrub-connection-file /home/alice/.config/td-mta/owner-connection \
  --scrub-epoch STORE_EPOCH_HEX \
  --scrub-state-dir /home/alice/.local/state/td-mta-integrity
```

Use the actual epoch from `postgres schema-inspect`. The owner connection is
for this separate operator job, never a serving process. Inspect the generated
`td-mta-maint-scrub` and `td-mta-maint-scrub-status` definitions and activate
both using the printed supervisor commands. For a dedicated system identity,
apply the same user/group and private-path provisioning described above.
The scrub directory must be dedicated, private and writable by that identity.

One invocation checks up to 128 bodies / 64 MiB under a ten-second deadline.
The minute timer resumes the next page; a new cycle starts a day after the
previous completes. Failed reads retry without advancing. Known corruption
advances while retaining a failed result, so it cannot starve later messages.
The status check requires a clean completed cycle within two days; it will
report attention until the first cycle completes. A large store may need a
separately chosen schedule. Reduce `--limit` on manual commands if a page
reaches its deadline; never treat incomplete pages as a successful cycle.

Inspect the supervisor journal and private `state.json`, or run:

```text
/absolute/td-mta postgres blob-scrub-status \
  --state-dir /home/alice/.local/state/td-mta-integrity --epoch STORE_EPOCH_HEX
```

A corruption report names only the first failed account/blob ID and counts;
it does not expose message content. Preserve the report and investigate with
`postgres blob-audit` and a known backup. Freshness and failure status are local
checks; mailbox notifications and off-host alert delivery remain explicit.
Do not delete progress to hide a failure. After restoring a database to a new
epoch, retain old evidence and provision a fresh scrub directory.

## Read-only local replica setup

After installing the current td-mta binary, use `./install-td-mta
--setup-replica --help` for fresh local provisioning. Supply the source HTTPS
origin, a protected password file, your local mailbox address, resolver and
explicit body/blob/Email/receipt quotas. Place the database on a dedicated
size-limited volume and verify the mount before enabling the generated copy
worker. The [desktop replica runbook](REPLICA-OPERATIONS.md#size-and-verify-the-replica-volume)
covers new and installed volumes and the fenced upgrade to schema 46.
Source credentials stay separate from
the new local JMAP password. The installer discovers source IDs through HTTPS;
no custom SQL or PostgreSQL access to the source is needed.

PostgreSQL and JMAP use private Unix sockets and require no local certificate
renewal. `--postgres-port` (default 35433) selects only the PostgreSQL socket
filename; no local TCP listener is created. Its systemd and Shepherd service names are `td-mta-replica-postgres`, `td-mta-replica` and
`td-mta-replica-sync`, so they
can coexist with an ordinary local setup. Follow the generated `START.txt` to
start one supervisor and use `mail.toml` with td-mail. The sync worker continues
bounded rounds with idle polling and failure backoff; the reader stays usable
when the source is offline. Generated instructions include historical sync
status and a manual copy command to use with the worker stopped.
Services are not enabled automatically. Initial preparation is not a complete
copy. No SMTP, relay or local TLS renewal service is generated. [REPLICATION.md](REPLICATION.md#fresh-local-provisioning) records the
copy, credential and recovery boundaries.


## Indexed-search extension prerequisite

Schema 40 needs PostgreSQL 17's bundled `pg_trgm` 1.6 extension files on each
server or restore host. Check `pg_available_extension_versions` before an
upgrade. Packaging varies by host: the retained PostgreSQL installations used
by the Azure deployment and local Guix replica already include these files.
The owner migration creates the extension in private `td_mta_trgm`; serving
credentials do not install extensions. An existing conflicting extension or
namespace needs operator reconciliation before retrying the migration.

Use a whole-database backup and restore, including the extension declaration
and its namespace. Run the normal fenced schema upgrade, then resumable
`summary-backfill --search` to index existing messages. New messages retain
bounded fallback until explicitly or automatically prepared. See POSTGRES.md's indexed-search contract
for literal matching, negative-evidence trust and resource limits.

## Automatic search preparation

Generate optional systemd timers and Shepherd timed services with the ordinary
installer, using owner credentials and a dedicated private progress directory:

```text
./install-td-mta --operations-services \
  --prefix /home/alice/.local \
  --state-dir /home/alice/.local/state/td-mta \
  --search-connection-file /home/alice/.config/td-mta/owner-connection \
  --search-epoch STORE_EPOCH_HEX \
  --search-state-dir /home/alice/.local/state/td-mta-search
```

This generates `td-mta-maint-search` and `td-mta-maint-search-status`
definitions under the prefix's `share/td-mta/services`; activate them through
the same supervisor procedure as the other operation services. Generation
does not start services or reconfigure Guix Home. Keep the Shepherd definitions
in the user's existing Home source when adopting them there.

The worker prepares at most sixteen missing bodies / 64 MiB per minute,
continuing a finite cross-account cycle and waiting five minutes between
completed cycles. It works on a primary or read-only application replica using
that database's owner connection. It changes derived search evidence, not mail
content or source synchronization progress. For a large initial corpus, the
explicit bounded backfill remains the faster operator-driven preparation path.

Status runs every fifteen minutes without credentials. A successful cycle may
include skipped unsupported messages, which stay visible in the reported counts
and retain partial-search behavior. Operational failures, uncertainty or no
completed cycle within a day make status fail. The supervisor records JSON
output and exit status; mailbox alert routing remains a separate configuration.

After a worker interruption or a `commit-unknown` result, keep the progress
file. The next scheduled worker retries from the last confirmed cursor,
verifies the database epoch and skips rows already indexed. Status reports
the interruption until a page succeeds. Repeated failures appear in the
worker journal with their database, connection or preparation error.
An epoch or search-version mismatch requires preserving the old evidence and
provisioning a fresh dedicated progress directory. No certificates or packages
are needed beyond the existing PostgreSQL installation and schema-40 extension.

## Native operational notifications

`--operations-services` can generate a bounded collector and a local daily
mail digest and optional direct external alerts alongside the existing readiness, certificate, backup, scrub and
search jobs. These are host operator commands in `td-mta-operator`, built
from this repository; no Python helper or source checkout is needed at runtime.
They do not add a target distribution dependency.

Add the following options to the operations configuration:

```text
--notify-recipient timmy@example.test
--notify-connection-file /private/mail/owner-connection
--notify-epoch CURRENT_EPOCH
--notify-smtp 127.0.0.1:25
--notify-hour 8
```

The recipient must be a configured local ASCII mailbox. SMTP is restricted to
an explicit loopback address and nonzero port. The collector needs an owner
connection for native `postgres logs`; a runtime connection is insufficient.
Add `--notify-df-bin /usr/bin/df` to include free-space checks on the state
filesystem, using that explicit host tool's `-Pk` output. No shell is invoked.
Disk attention is reported at 90 percent usage or below 3 GiB available.
Mail profiles add native readiness and performance inspections; configured
certificate profiles and backup/scrub/search status jobs add those inspections
to the digest. A failed inspection is reported as unavailable, never as zero.

The generator writes `STATE/.operations/notifications/config.json` and
`td-mta-maint-notifications-monitor` and `td-mta-maint-notifications-digest`
service definitions. It refuses changed existing configuration/definitions.
The collector runs each minute; the digest runs at the specified hour in the
supervisor timezone. `--notify-systemd-timezone America/Los_Angeles` adds an
explicit timezone to the systemd calendar. Shepherd calendars use the
supervisor's timezone; that systemd-only option does not change Shepherd's
process-wide timezone. Generated jobs are not enabled automatically. As with
other operations, system services need the intended User/Group and private
state directory ownership; generation as root does not authorize a daemon to
read root-owned 0600 files. Preserve the installed helper and its previous
version when changing the selected operator executable.

Inspect or run the installed commands using the generated configuration:

```text
td-mta-operator install-td-mta --notifications-monitor --config-file PRIVATE_CONFIG
td-mta-operator install-td-mta --notifications-preview --config-file PRIVATE_CONFIG
td-mta-operator install-td-mta --notifications-digest --config-file PRIVATE_CONFIG
```

Preview performs the bounded inspections but sends no mail. The collector
initially starts at the newest native log ID. Later runs persist pagination
across at most sixteen pages of 100 records per invocation. The newest 512
selected diagnostic events are retained, with cumulative eviction counts and
native database suppression/eviction counters. Receive refusals, interrupted
DATA, process suppression from every profile, and selected relay/certificate/service failures
are samples, not a delivery ledger. PostgreSQL's independent bounded log
retention remains authoritative for its retained diagnostics. An epoch change
refuses until the operator provisions fresh state while retaining old evidence.

One private directory lock covers collection, rendering and delivery. A digest
waits at most 130 seconds for an ongoing collector (whose generated supervisor
bound is 120 seconds); other invocations refuse contention. A digest
advances its log-ID cursor only after acknowledged SMTP delivery; event
wall-clock times do not discard late-collected samples. Ring eviction, a stale
collector or an unfinished scan makes the report explicitly incomplete.
Performance differences are calculated within one process ID, across
in-process reloads that change the boot ID. When either snapshot lacks a
process ID (an older binary, or a null one after a failed entropy draw), they
are calculated within one boot ID. After restart, current-lifetime counters
are labeled as partial window evidence.

Local SMTP acceptance is recorded durably before connection cleanup. The same
UTC day's acknowledged digest is not sent twice. A lost final SMTP reply or
an interrupted state write can leave `pending.json`: subsequent delivery refuses
instead of sending a possible duplicate. Inspect its exact Message-ID in the
recipient mailbox and the supervisor journal before resolving it. Retain the
pending and last-report records as evidence. Resolve only after checking the
exact Message-ID (without angle brackets):

```text
td-mta-operator install-td-mta --notifications-resolve --config-file PRIVATE_CONFIG --message-id ID --outcome accepted
```

Use `not-delivered` instead of `accepted` only when non-delivery has been
established; absence from a client cache alone is not proof. Resolution saves
`resolution.json` before clearing pending state. Accepted resolution advances
the digest cursor; non-delivery keeps the previous cursor for a future retry.
Archive the evidence before another resolution overwrites that bounded file.
Do not delete the directory to
clear a delivery error. Configuration, monitor, report and acknowledgement
files are bounded to 3 MiB each; each generated message is at most 256 KiB.
Inspection subprocesses have 20-second deadlines and bounded stdout/stderr;
SMTP has a 30-second exchange deadline. The service supervisor additionally
bounds the overall invocation. No elapsed-time performance threshold is a test.

This digest uses the local receive path and does not need SES or Migadu. It
cannot reliably alert when that path or the host is unavailable. The failed
oneshot result and private diagnostics remain independent local evidence;
external uptime alerts and off-host backup copies are separate configuration.

### Capacity reports and direct external alerts

Generated notification configurations include `postgres capacity-inspect
--connection-file PATH --epoch HEX`. It reads selected policy without loading
secrets and observes account quota counters, bounded receipt/route/device/mailbox
counts, log storage and database connections. Final-result receipt occupancy
includes reserved claims. Counts stop at ceiling plus one; over-limit counts
are lower bounds. A policy selection change or incomplete inspection refuses;
missing data is never zero. The result is an observation, not a quota audit or
serving permission. Configured concurrency and protocol ceilings are included,
but instantaneous and peak occupancy are unavailable. Planned memory is labeled
as a reservation, not RSS. The digest shows usage, ceiling, percentage and
remaining capacity, warning at 80 percent. Backup counts are rotation targets.

The private notification configuration may additionally contain:

```json
"alerts": {
  "curl": "/usr/bin/curl",
  "credentials": "/private/mail/alert-smtp.conf",
  "host": "smtp.migadu.com",
  "from": "operations@example.test",
  "recipient": "external@example.net"
}
```

This is an optional host-only curl invocation, not a target build dependency.
The operator provisions curl with SMTP/TLS support and its CA trust store.
The credentials file must be owned by the service user and mode 0600 under
trusted ancestors. It is curl configuration containing only a quoted `user =
"username:password"`; never place the password in argv or notification JSON.
The helper uses implicit TLS on port 465 with certificate validation, a fixed
recipient and sender, and an 18-second transport timeout. It bypasses local
submission, PostgreSQL and the MTA's relay workers for delivery.

The minute collector checks configured readiness/freshness, disk and capacity,
and new SMTP commit/publication failures. Capacity warnings start at 80 percent,
critical at 95 percent; rotating log row occupancy is not an incident. Failure
counters are differenced within one process ID, across in-process reloads that
change the boot ID. A changed process ID is a new process whose counters began
at zero, so every failure it reports happened after the restart and alerts. The
collector's first observation only establishes a baseline; a status without a
process ID (an older binary) is differenced within one boot ID and becomes a
new baseline when the boot ID changes. Missing inspections are incidents.
Collection has a ten-second paging budget, inspections a 35-second scheduling
budget (one bounded child may finish afterward); incomplete work is reported.

Incidents remain active until ten quiet minutes, with hourly reminders and a
recovery message. Failed or uncertain sends retry after five minutes, so alerts
may duplicate after an ambiguous SMTP outcome. Private `alerts.json` retains
counters, active incident keys and delivery timestamps separately from digest
receipts. Neither preview nor a delivery test advances this incident state.
`--alerts-preview --config-file PATH` inspects without sending; `--alerts-test
--config-file PATH` explicitly sends a labeled configuration test. Both commands
use the same operator entry point as the notification commands above.

A host or network outage can prevent these alerts. An off-host heartbeat or
end-to-end delivery monitor is still required to detect that class of failure.

Capacity rendering admits 200 KiB within the 224 KiB daily report and 256 KiB
message bound. Repeated scope lists wrap at eight names per line so maximum
profile names stay within SMTP line bounds. External alert bodies admit 192 KiB.
The capacity database connection separately admits 6,144 frames, 1 MiB input
and 256 KiB output for the fixed roster across at most sixteen accounts; it
retains the existing ten-second operation deadline and per-frame validation.
These cold owner/host command paths retain bounded standard collection/JSON
allocation debt in the reviewed inventory; they do not claim allocation safety
or extend the serving path's allocation boundary.

The production systemd cutover provisions separate service identities and private
connection files. Follow “Isolating production service accounts” in PRODUCTION.md
for that upgrade and its rollback. The personal systemd and Shepherd definitions
continue to use the single-user installation described here.
