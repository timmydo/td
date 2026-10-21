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
  --tls-certificate /home/alice/.local/state/td-mta/postgres/server.pem \
  --tls-certificate /home/alice/.local/state/td-mta/postgres/ca.pem \
  --openssl-bin /usr/bin/openssl \
  --backup-service-file /home/alice/.config/td-mta/backup.pg_service.conf \
  --backup-service td-mta-backup \
  --backup-destination /home/alice/mail-backups --postgres-bin /usr/lib/postgresql/17/bin
```

Select any complete group independently: mail profiles, managed certificate
profiles with their connection file, private certificate files with an explicit
OpenSSL executable, the four backup arguments, or the local PostgreSQL renewal
group described below. Repeat profiles/certificate
files up to 32 per kind. All paths are absolute. The mail state directory is the
same one used when generating the foreground services. Provision the backup
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
is not archive integrity or a restore test. Backup bounds remain 1 GiB/archive,
8 GiB/destination and thirty completed archives; the command's lock also refuses
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
`pg_ctl`, `psql`, and OpenSSL must be available on the host PATH. These are
host provisioning tools, not target recipe dependencies. The command runs
them directly, without a shell. It creates a new mode-0700 root under
trusted physical ancestors; existing roots, symlinks, privileged local
ports and conflicting local port assignments refuse. The parent must already
exist. Roots are limited to 72 bytes for the MTA control socket.

Default ports are PostgreSQL 35432, HTTPS 54443 and incoming SMTP 55225;
`--postgres-port`, `--https-port` and `--smtp-port` override them. All three
bind IPv4 loopback only. Relay uses verified implicit TLS on port 465;
`--relay-port` overrides the port, not the transport. Supply the relay login
explicitly. Its password input must be a private, user-owned regular file;
its ancestors must be physical, root/user-owned and not shared-writable.
One final LF or CRLF is removed. An optional `--relay-ca-file` takes a
similarly protected trust bundle; otherwise the bundled public roots apply.
Account addresses, relay hostnames and relay users use simple ASCII names.
The resolver must be an explicit IP:port endpoint reachable by the service.

Setup creates a local CA and 90-day server leaf, initializes its own cluster,
briefly starts PostgreSQL, creates separate schema-owner and runtime logins,
initializes the schema/account/device, imports immutable materials and all
three profiles, publishes a retained selection request, and checks runtime
privileges. Temporary local trust authentication is confined to the private
socket and setup administrator. The final configuration disables Unix
sockets and accepts only TLS/SCRAM owner/runtime connections over loopback.
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
and device-password command. Use `td-net launch --allow-loopback` for this
local account. Startup persistence, certificate renewal and backups still
follow the procedures elsewhere in this document.

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

General configuration changes require validated database versions, selection
and process restart. Native managed certificate renewal is supervised
separately; serving processes drain and reload when their managed certificate
generation changes, as [ACME.md](ACME.md) specifies. The native executable fixture qualifies separate
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
required connection/bootstrap material. The host installer can run bounded
logical backups; the mail daemon has no restore command. POSTGRES.md owns
maintenance, restored-epoch and uncertain
external-delivery constraints and the explicit recovery-release command.
Release quarantines pending delivery and requires fresh credentials and
reimported configuration before service activation.
Neither a historical audit report nor copying a local runtime/scratch
directory is a database backup or recovery authorization.

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
separately from td-mta's connection-file syntax. The service contains `host`,
`port`, `dbname`, `user`, `sslmode=verify-full`, `sslrootcert`, `passfile`, and
`connect_timeout=10`; the referenced mode-0600 password file uses PostgreSQL's
`host:port:database:user:password` format, escaping colons and backslashes.
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

Retain protected bootstrap material separately: database owner/runtime login
credentials and role/grant provisioning, database TLS trust, the local CA key
if used, profile source files, current selection receipts, the td-mail device
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
backed up; use the owner login and verified local PostgreSQL TLS.

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
one. Public ACME health does not cover a private PostgreSQL certificate;
check that leaf and its CA with the expiry command in the next section.

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

## Automatic local PostgreSQL leaf renewal

`./install-td-mta --postgres-tls-renew` renews the setup profile's private
PostgreSQL leaf certificate under its existing CA and server key. It supports
only the localhost/127.0.0.1 certificate and a loopback PostgreSQL endpoint.
Run it as the cluster owner with PostgreSQL 17 and OpenSSL 3 or 4:

```text
./install-td-mta --postgres-tls-renew \
  --tls-dir /home/alice/.local/state/td-mta/tls \
  --data-dir /home/alice/.local/state/td-mta/postgres \
  --postgres-bin /usr/lib/postgresql/17/bin --openssl-bin /usr/bin/openssl \
  --port 35432
```

The TLS directory must contain private owned `ca.pem`, `ca.key`, `server.pem`
and `server.key` files. Both directories and their physical ancestors must
meet the private-path contract. PostgreSQL must already use that server
certificate/key pair. Host-library provisioning remains the operator's
responsibility, as for setup and backups. The helper checks key correspondence,
local subject-alternative names and the old chain, allowing an expired leaf
only for renewal. The CA must remain valid for at least 91 days; CA trust/key
rotation is always explicit. A leaf with fewer than 30 days remaining is
reissued for 90 days. `--force` exercises issuance immediately; never put that
flag in the unattended schedule.

The server key stays unchanged, so publication replaces only the certificate.
The helper retains one `server.previous.pem`, syncs the replacement, asks
`pg_ctl reload` to signal the named cluster, then establishes a new verified
PostgreSQL TLS connection and compares its first presented certificate with
the selected file. [OpenSSL's PostgreSQL STARTTLS probe](https://docs.openssl.org/3.0/man1/openssl-s_client/)
uses explicit CA/name verification and fails on verification errors. A failure
after publication leaves the candidate on disk and returns failure; it does
not silently roll back or claim reload succeeded. A subsequent invocation also
repairs a missed reload when a healthy on-disk certificate differs from the
one being served. It never restarts PostgreSQL or mail services.

One private lock excludes overlapping invocations. Tool output is bounded to
64 KiB per command, diagnostics to 16 KiB, and execution to 30 seconds per
command, at most 32 commands. Reserved `renew-work` scratch is reclaimed on
normal return and after interruption; unexpected entries refuse cleanup.
Private `renew-last-error` retains a tool diagnostic until a successful run.
Supervisor results also cover validation failures before a tool can run.
Previous certificates and the publication staging slot have fixed names;
unattended renewal does not accumulate generations or new private keys.

Generate its daily systemd/Shepherd schedule with the operations installer:

```text
./install-td-mta --operations-services --state-dir /home/alice/.local/state/td-mta \
  --postgres-tls-dir /home/alice/.local/state/td-mta/tls \
  --postgres-data-dir /home/alice/.local/state/td-mta/postgres \
  --postgres-port 35432 --postgres-bin /usr/lib/postgresql/17/bin \
  --openssl-bin /usr/bin/openssl
```

The generated `td-mta-maint-postgres-tls-renew` job uses the retained host
helper. Enable it through the printed supervisor commands, as for backups.
The daily job never forces issuance. It shares the daily schedule's bounded
45-minute supervisor ceiling; private certificate health checks remain
independent. For a hardened system service, allow writes only to its TLS
directory and retain read access to the PostgreSQL data directory. Provision
the service identity and tool runtime libraries explicitly.

This updates PostgreSQL's local certificate files only. It does not change
immutable certificate materials already imported into td-mta, a public ACME
profile, another account's trust, or the CA. Installations using the same old
leaf for mail listeners must separately update those stored materials using
the procedure below. Failure notifications and CA rotation remain explicit.

## Renew a local private certificate

A local `localhost` deployment uses its explicitly trusted private CA;
public ACME issuance is not involved. Check both the CA and leaf expiration
with `openssl x509 -in FILE -noout -dates`, and check the renewal threshold
with `openssl x509 -in FILE -checkend 2592000 -noout`. Nonzero means renewal
is due (or the input could not be checked); it is not an instruction to
ignore verification. Schedule the operator check before either certificate
expires. The commands in this section are manual. The local PostgreSQL-only helper
above provides automatic leaf renewal; other imported mail materials still
require explicit configuration selection.

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

Use `postgres material-version` to retain separate chain/key IDs, then import
`server.pem` as `certificate-chain` and `server.key` as `private-key`, using
`material-import` with the owner connection file and expected epoch. Replace
only the certificate material references in copies of every affected profile.
Generate/retain a configuration version; `config-import` it with every profile
and `config-check` it. Prepare and retain a `config-selection-prepare` request
using the currently selected generation, then publish that exact request with
`config-select`. Resolve uncertain results with `config-selection-inspect` and
the same retained request. POSTGRES.md sections 17, 26 and 198 specify these
commands; do not invent a fresh request to hide uncertainty.

Certificate files on disk do not update td-mta's immutable database materials.
Selecting a configuration advances the database-wide generation and fences
every instance using that selection. Restart all of them, including unchanged
listener-free relay profiles, and require ready status from each instance. When the private PostgreSQL server uses the same local leaf,
install its new certificate/key pair and reload or restart PostgreSQL too;
keep its bootstrap clients' trust valid throughout the transition. Verify new
connections to PostgreSQL, JMAP HTTPS and SMTP STARTTLS with name/chain checks,
and confirm that they present the intended certificate and expiration. Keep
the old files and selected configuration receipt for diagnosing a failed
rotation. Do not claim live reload or automatic ACME support from this manual
procedure.
