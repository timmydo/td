# Production operations: mx.timmydouglas.com

This is the operator runbook for the Azure x86-64 host shared by
`mx.timmydouglas.com`, `mx.timmyd.info` and `mx.882015.xyz`. The base inventory was
checked on 2026-10-27; search schedules were enabled on 2026-10-30. Update this document when changing its paths or services.
[DEPLOYMENT.md](DEPLOYMENT.md) describes installation on other hosts;
[OBSERVABILITY.md](OBSERVABILITY.md) describes diagnostic output.

## Access and installed layout

From the development machine, use the dedicated SSH key:

```text
ssh -o IdentitiesOnly=yes -i KEY ADMIN@HOST
```

`HOST`, `ADMIN` and `KEY` stand for the host's address, its administrator
account and the dedicated key file. The operator keeps their values in
private notes outside this repository, and `./td-mta/deploy` reads them
from the operator's own [deploy target
configuration](#deploy-target-configuration); nothing in the repository
names them. The `ADMIN` account can administer the services with `sudo`.
The daemon runs as `td-mta`. The former Stalwart source VM on AWS is
retained pending the owner's decision: Stalwart is disabled and stopped,
and `td-mta-stalwart-defer.service` answers port 25 with SMTP 421. Its
address is in the private notes; its SSH identity and retained host key
are separate from this host's. The MX names no longer reach it. `/var/lib/td-mta/migration` holds the Stalwart exports, import
mappings and final catch-up reports; deleting them waits on the same
decision.

| Item | Location or name |
| --- | --- |
| Source checkout | `/home/ADMIN/src/td`: `src/td` under `host.json`'s `build_home` |
| Supervisor's host description | `/var/lib/td-mta-operator/host.json` |
| Selected executable | `/opt/td-mta/bin/td-mta` (symlink) |
| Retained binaries and build notices | `/opt/td-mta/lib/td-mta/releases/` |
| Installed maintenance helper | `/opt/td-mta/bin/td-mta-operator` |
| Mail state root | `/var/lib/td-mta/mail` |
| Private PostgreSQL connection files | `/var/lib/td-mta/mail/runtime-connection`, `/var/lib/td-mta/mail/owner-connection` |
| PostgreSQL socket directory, once cut over | `/var/lib/td-mta-postgres` (`td-mta:td-mta-db` 0750) |
| Database writer fence (present only while fenced) | `/etc/td-mta/postgres-fence.conf` |
| PostgreSQL server and tools, once switched | `/td/store/HASH-postgresql-x86-64-17.N/bin`, with td glibc in `/td/store/HASH-glibc-x86-64-VERSION` |
| Retained Debian PostgreSQL, until retired | `/opt/td-mta/postgresql/usr/lib/postgresql/17/bin` |
| Binary selection (present only once switched) | `/etc/systemd/system/td-mta-postgres.service.d/91-td-mta-binaries.conf` and `/etc/systemd/system/td-mta-maint-backup.service.d/91-td-mta-binaries.conf` |
| Per-profile readiness and diagnostics | `/var/lib/td-mta/mail/PROFILE/runtime` |
| System service definitions | `/etc/systemd/system/td-mta-*.service` |
| Root-owned deployment receipts and diagnostics | `/var/lib/td-mta-operator/` |
| Native notification configuration and bounded state | `/var/lib/td-mta/mail/.operations/notifications/` |
| Scheduled database backups | `/var/lib/td-mta/backups/scheduled/` |
| Managed data disk | 64 GiB, Azure data LUN 0, ext4 mounted at `/data` |
| Backup and recovery archive storage | `/data/td-mta/backups/` and `/data/td-mta/migration/`, bind-mounted at `/var/lib/td-mta/backups/` and `/var/lib/td-mta/migration/` |
| Whole-body integrity progress | `/var/lib/td-mta/integrity/` |

Keep connection files and credentials private; no password belongs in a
command argument, Git, or this runbook. Configuration, account records, mail,
managed certificates and durable application logs are in PostgreSQL. Installing
a binary does not replace them.

Five processes use the selected MTA executable:

- `td-mta-frontend.service`: JMAP/HTTPS.
- `td-mta-receive.service`: inbound SMTP.
- `td-mta-relay.service`: relay profile, including the test-domain SES route.
- `td-mta-migadu.service`: production outbound relay profile.
- `td-mta-acme-production.service`: certificate issuance/renewal for all three MX names.

The selected configuration is to be td-mta's defaults plus the reviewed
`td-mta/production-overrides.conf`, which states no difference; it changes
only through [`--converge-config`](#converge-the-configuration). A
configuration selected before convergence first ran can still set pool
sizes explicitly; `./td-mta/deploy --converge-config --plan` lists every
such setting, and the latest receipt's `config_profiles` the pools in
force. Once converged, the frontend serves the default eight event
streams in slots of their own, beside its sixteen `https_connections` for
ordinary requests. Each graphical td-mail process opens one stream per
eligible watched account, up to two streams per process.

If this frontend returns HTTP 429 for a valid EventSource URL and
credentials, compare its `http_auth_refused` counters in
`performance-status` before and after the request. Unchanged, unsaturated
`calls` and `lost` in the same process identify the stream ceiling as the
cause. An increase in `calls` means the frontend refused at least one
pre-authentication attempt during the interval. That attempt could belong
to another client. Lost observations leave the cause uncertain.
An ordinary request refused with 429 before authentication, on any
target, means all `https_connections` ordinary admissions were held; its
log record is a capacity `AdmissionRefused`. Event streams never hold
ordinary admissions.

Once converged, the inbound SMTP listener takes the default pool of 32
sessions (DESIGN.md section 5), at two per peer key, so filling it takes
sixteen peer keys: convergence sets the listener's required
`session_limit` and `per_peer_limit` to the profile's `smtp_sessions` and
`smtp_per_peer`.
The SMTP session and DATA limits share a peer key: an IPv4 address
(including IPv4-mapped addresses) or an IPv6 /64. DATA admission has
its own fixed limits of four
jobs overall and two per peer. Each peer key may also deliver bursts of 100
messages and 256 MiB, then 120 messages an hour and about 1 GiB a day; beyond
that its MAIL commands receive `421 4.7.0` until the balance refills, and the
sender retries. `smtp_rate_refused` performance counters show refusals (busy)
and forgotten keys of a full table (unavailable). A legitimate high-volume
sender that is throttled still delivers, only more slowly. A large provider
sending over one IPv6 /64 shares one budget, so sustained busy counts with
delayed mail from that provider mean its traffic, or one of its users, has
reached the limit. The bounds are fixed constants, so raising them is a code
change (DESIGN.md section 9).

`td-mta-postgres.service` separately runs PostgreSQL 17. Production
still serves it on loopback TCP 35432 with TLS, verified against a
private CA under `/var/lib/td-mta/mail/tls`, until the pending cutover
([Leaving loopback TLS](#leaving-loopback-tls)) moves it to the private
Unix socket `/var/lib/td-mta-postgres/.s.PGSQL.35432`, with no TCP
listener and no TLS. The socket is `td-mta:td-mta-db` mode 0660 in a
0750 directory; PostgreSQL runs as `td-mta` and joins the memberless
system group `td-mta-db` through its `90-td-mta-socket.conf` drop-in, so
it can give the socket that group. Each login authenticates with SCRAM.
Its cluster is `/var/lib/td-mta/mail/postgres` and its configuration is
`/var/lib/td-mta/mail/postgresql.conf`. It runs td's source-built
PostgreSQL 17 from `/td/store` once switched (see "Switch PostgreSQL to
the td-built binaries"), and the Debian binaries extracted under
`/opt/td-mta/postgresql/usr/lib/postgresql/17/bin` before that; they
stay there for rollback until `--retire-rollback` removes them. The
deployment controller uses the client tools of whichever server the
`91-td-mta-binaries.conf` drop-in selects. The configuration ends by
including `/etc/td-mta/postgres-fence.conf` if it exists; only a
deployment writes that file, while it holds the writer fence. An MTA
binary update does not require restarting or replacing PostgreSQL.

### Search planner setting

The deployment controller gives the frontend's login a scoped planner
setting when it creates the service logins:

```sql
ALTER ROLE td_mta_frontend IN DATABASE td_mta SET random_page_cost = 2;
```

It is a deployment setting for this host, not a td-mta schema invariant
or a recommended default for every installation. PostgreSQL's cluster
default remains `4`; the other service logins and owner maintenance
sessions keep that default. A session adopts a role default when it
connects.

On 2026-10-30, after search backfill and vacuum/analyze, the default planner
scanned all roughly 33,000 search rows for `guix`. The query, then run
as the shared runtime login, took 699–774 ms. With cost `2`, PostgreSQL
chose the trigram index and took 154–168 ms. Five subsequent HTTPS
searches for the first 50 matches had a 299 ms median, versus 863 ms before the setting, with identical ordered
results. These are diagnostic samples, not latency guarantees or test
thresholds. Rare, absent, common, and mailbox-scoped candidate comparisons
also preserved results. Very broad terms can still appropriately use a
sequential scan; no scan method is disabled.

Before retuning on another host, compare bounded read-only query plans and
end-to-end requests under its workload. Keep statement timeouts and check
normal reads, writes and service health as well as searches. Recheck after
large imports or backfills; statistics, index maintenance and cache state
affect the plan. Do not infer the right value from idle memory or SSD
branding alone.

A database-only backup does not carry role/database defaults: after a
restore, check this setting through a fresh frontend login. It is not
needed for data correctness or schema admission.

## What updates automatically

MTA software does **not** update automatically from Git. A merge, fetch or pull
does not change the running executable. `./install-td-mta` builds and installs
for the chosen prefix; it changes that prefix's executable link but does not
restart services. A deployment builds into its own release directory
before touching the live installation.

Public certificates renew through `td-mta-acme-production.service`, without
certbot. Leave public TCP 80 reachable for HTTP-01 renewal, as well as SMTP 25
and HTTPS 443. Until the socket cutover the database's loopback TLS leaf
renews through `td-mta-maint-postgres-tls-renew.timer`, which the cutover
retires; on the private socket the database has no certificate. Certificate management
and software updates are independent.

Scheduled backups use `td-mta-maint-backup.timer`, retaining at most seven
archives within a 4 GiB total bound and a 1 GiB per-archive bound. These bounds
are generated operations settings, not PostgreSQL storage quotas. Health checks include
`td-mta-health.timer` and the generated `td-mta-maint-*` timers. Check the
oneshot results, not just whether a timer is active:

```text
systemctl list-timers 'td-mta-*' --all
systemctl --failed
sudo journalctl -u td-mta-health.service -u td-mta-maint-backup.service --since today
```

The older `td-mta-backup.timer`, `td-mta-acme.service`,
`td-mta-acme-public.service` and `td-mta-stalwart-catch-up.timer` with its
service are disabled; they are not the production workers.
Daily notifications are configured separately below. The scheduled dumps live
on the VM's attached disk. The designated off-host recovery destination is the
development machine; its daily pull and weekly restore schedules still
require provisioning and enablement. The last hand copy is under
`/mnt/backup/test/td-mta/copies` on that machine, with its verification
report in `/mnt/backup/test/td-mta/`. Unattended pulls require a
dedicated export-only SSH identity and an installed export-capable helper.
The recovery-host jobs and their enablement procedure are in [DEPLOYMENT.md](DEPLOYMENT.md#pull-backups-to-a-recovery-host).

## Daily notifications and bounded disk growth

The native operations collector and daily digest are generated by
`td-mta-operator install-td-mta --operations-services`; see
[notification configuration](DEPLOYMENT.md#native-operational-notifications).
Use the same operations options that generate readiness, backup, certificate,
search and scrub status jobs, adding:

```text
--notify-recipient OPERATOR_MAILBOX
--notify-connection-file /var/lib/td-mta/mail/owner-connection
--notify-epoch CURRENT_EPOCH
--notify-smtp 127.0.0.1:25
--notify-hour 8
--notify-systemd-timezone America/Los_Angeles
--notify-df-bin /usr/bin/df
```

The installed jobs are `td-mta-maint-notifications-monitor.timer` (each minute)
and `td-mta-maint-notifications-digest.timer` (08:00 Pacific). The digest is
local SMTP mail to the Inbox with subject prefix `[td-mta daily]`; it does not
use an external relay. It reports sampled refusals and interruptions, native
suppression, readiness, certificate and configured maintenance checks,
process-counter differences, operational capacity and free disk space. Inspect
their oneshot results and journal, not only enabled timers. Direct external
alerts use Migadu SMTP to the operator's external address, bypassing local
submission; that address is recorded with the other private values. The
private `alerts` configuration and `alert-smtp.conf` credential live beside
the notification state. Capacity pressure, new SMTP publication failures,
failed inspections and recovery follow the bounded policy in DEPLOYMENT.md.
A broken host or network still requires an off-host heartbeat monitor.
The daily digest remains local mail to `OPERATOR_MAILBOX`, the operator's
mailbox on this host, recorded with the other private values.

The private configuration and bounded receipts are in
`/var/lib/td-mta/mail/.operations/notifications/`, owned by `td-mta`. Run the
installed helper as that user. Preview with `--notifications-preview` and the
private `--config-file` path. An uncertain SMTP outcome leaves `pending.json`;
inspect the exact Message-ID and use the documented `--notifications-resolve`
command. Do not remove state to force a resend. Acknowledged digests are
limited to one per UTC day, separately from the Pacific timer schedule.

The native helper updates through the standard installer; compare generated
units before replacing operator adaptations such as system User/Group,
filesystem restrictions and resource limits.

Backup retention is already bounded to seven archives and 4 GiB total; do not
increase it without checking disk capacity. Installed release directories and
candidate builds are not automatically pruned. Keep current and known-good
previous releases, then remove only identified inactive build trees. Do
not treat migration data as build cache.

## Backup and recovery data disk

The 64 GiB managed data disk uses ext4 UUID
`8fb95fa6-d31b-466f-ba0c-f9174ed941c7`. `/etc/fstab` mounts it at
`/data` with `nodev,nosuid,nofail`; device names such as `/dev/sdc1` are
not boot identities. `/data/td-mta` is owned by `td-mta` with mode 0700.
Bind mounts expose its `backups` and `migration` directories at
`/var/lib/td-mta/backups` and `/var/lib/td-mta/migration`, the paths
backup and recovery commands use. PostgreSQL, live mail state, runtime
directories and build trees are on the OS disk.

Both bind mounts explicitly require `data.mount`. Their underlying OS-disk
mountpoint directories are root-owned mode 000, preventing service-user
writes into a fallback directory when a mount is absent. The current and
retired backup services, backup-status service, aggregate health service
and notification digest carry `data-disk.conf` drop-ins with
`RequiresMountsFor` and `AssertPathIsMountPoint` for the backup mount.
Preserve these drop-ins when updating
generated operations units. `nofail` permits host recovery when the data
disk is unavailable; it does not permit dependent maintenance jobs to
proceed without their storage.

The notification configuration includes a `disk-data` inspection of `/data`
alongside the existing OS-disk inspection. Monitor both filesystems. Backup
retention remains seven scheduled archives within 4 GiB, and the data disk
does not replace off-host backups.

Inspect the mounts and existing backup health with:

```text
findmnt --mountpoint /data
findmnt --mountpoint /var/lib/td-mta/backups
findmnt --mountpoint /var/lib/td-mta/migration
df -h / /data
systemctl status td-mta-maint-backup-status.service
```

Recovery archives are retained data, not disposable cache.

## Update and roll back

Updates and their rollback run from the development machine through
`./td-mta/deploy`
([Deploy from the development machine](#deploy-from-the-development-machine)):
it selects a reviewed revision of main, the host's build account builds
it, and the controller preflights, stages, activates and verifies it,
rolling back on its own before writer admission. `--rollback`
([Rolling back an activation](#rolling-back-an-activation)) returns to
the release it replaced, and `--recover` finishes any job interrupted
inside its fence. There is no manual build or activation path; a
rollback keeps the selected configuration, which
[`--converge-config`](#converge-the-configuration) sets.

## Shorten a schema maintenance window

For schema cutovers, prepare an online physical backup and an isolated
recovery copy while mail is serving. Keep streaming PostgreSQL WAL until
the copy reaches the exact pre-migration restore point. The [online cutover
procedure](DEPLOYMENT.md#online-recovery-point-for-a-schema-cutover) covers
host prerequisites, commands and evidence. Qualify it on the installed host
before booking the short window. If it cannot be qualified, allow time for
the fenced logical backup and restore drill. Scheduled backups continue on
their existing policy.

Use the PostgreSQL 17 client binaries alongside the installed server.
Place the private base, WAL spool and isolated trial on the recovery
volume under `/data`. The live cluster uses an external configuration
file; retain it separately and give the trial its own configuration and
socket directory. Budget space for two cluster copies and the WAL spool
before each window.

For physical backup access, provision a dedicated `LOGIN REPLICATION
NOSUPERUSER` PostgreSQL role with a private SCRAM credential. Restrict
its HBA entry to `local replication` and that role. Reload HBA and verify
a replication connection whose libpq service names
`host=/var/lib/td-mta-postgres` and `port=35432`. The mode-0600 passfile
entry names that directory as its host, the PostgreSQL port, `replication`
and the backup role. Until the socket cutover
([Leaving loopback TLS](#leaving-loopback-tls)) production's entry is a
loopback `hostssl` rule and its service and passfile name the loopback
TLS server; the cutover rewrites them. Keep the credential with the backup operator. The streaming procedure uses no permanent replication
slot and requires no change to `archive_mode`.

Stage both binaries, the migration commands and the comparison queries.
Rehearse the migration on a separate isolated copy with mail services
disabled. Prepare the cutover commands to advance on each successful check
and stop on failure.

Start a timestamped SMTP and JMAP probe before stopping services. Use a probe
location that can reach each listener while it is healthy. Record the SMTP
greeting and EHLO after STARTTLS, and the expected HTTPS authentication
challenge for JMAP. For each protocol, retain the last success before the
interruption and the first success after restart, with the probe interval
and location. These bound the interruption observed by clients on that path.

During the fence, confirm recovery through the last accepted write, run the
migration and qualify the candidate against the upgraded database. Start the
mail services as soon as these checks pass.
Record elapsed times for backup, restore, comparison, migration and restart
in the activation receipt. Verify ACME, timers and aggregate health as usual
and record their completion separately from listener recovery.

## Diagnose and retain evidence

```text
sudo journalctl -u td-mta-frontend -u td-mta-receive -u td-mta-relay -u td-mta-migadu -u td-mta-acme-production --since '30 minutes ago'
sudo journalctl -u td-mta-postgres --since '30 minutes ago'
sudo -u td-mta /opt/td-mta/bin/td-mta performance-status --runtime /var/lib/td-mta/mail/frontend/runtime
sudo -u td-mta /opt/td-mta/bin/td-mta logging-status --runtime /var/lib/td-mta/mail/frontend/runtime
```

Repeat the private status commands for other profiles as needed. Durable
application logs are queried with `postgres logs` using the owner connection
file and current epoch; their bounds and retention are in OBSERVABILITY.md.
The journal, process counters and database records cover different failures.
Do not clear failed units or delete logs before recording their cause.

The installer retains releases without automatic pruning. Keep the current
and known-good previous release, their notices and deployment records. After
verification, remove only explicitly identified obsolete candidate build trees
and inactive checkouts; never delete database, backup or migration directories
as build cleanup. Maintenance helper upgrades are separate from the MTA binary:
regenerating operations definitions may refuse operator-edited units, which
must be compared before replacement. Do not silently overwrite those units.

## Search preparation

`td-mta-maint-search.timer` prepares missing or incompatible search
evidence in bounded pages once a minute, with durable progress in
`/var/lib/td-mta/search-index/`, and `td-mta-maint-search-status.timer`
checks it every fifteen minutes. [Automatic search
preparation](DEPLOYMENT.md#automatic-search-preparation) covers their
bounds, status and uncertainty recovery. Check them with:

```text
systemctl status td-mta-maint-search.timer td-mta-maint-search-status.timer
sudo journalctl -u td-mta-maint-search.service -u td-mta-maint-search-status.service --since today
sudo systemctl start td-mta-maint-search-status.service
```

## Scheduled whole-body integrity verification

`td-mta-maint-scrub.timer` runs one bounded page each minute. Each page checks
at most 128 bodies and 64 MiB of attempted body bytes using the original
whole-message SHA-256 and chunk layout. It neither repairs nor deletes mail.
After completion, the next cycle starts no sooner than 24 hours later.
Progress survives restarts in the private `/var/lib/td-mta/integrity/` directory,
bound to the database epoch. Do not delete that directory to clear a failure.

`td-mta-maint-scrub-status.timer` checks every 15 minutes. It fails when no
clean cycle has completed within 48 hours, a cycle finds corruption, an
operation fails, or the clock moves backward relative to retained observations.
These are systemd/journal results; the daily digest includes the configured
scrub-status inspection. A failed timer alone does not send immediate mail.

```text
systemctl status td-mta-maint-scrub.timer td-mta-maint-scrub-status.timer
sudo journalctl -u td-mta-maint-scrub.service -u td-mta-maint-scrub-status.service --since today
sudo systemctl start td-mta-maint-scrub-status.service
```

Inspect the JSON progress and retained failure identifiers before taking any
repair action. A successful selective message read does not replace this
whole-body check; a clean scrub does not replace a backup or restore test.

## Exhausted HTTP write receipt ledgers

From schema 41 an account's Email mutation, folder, draft, upload,
submission and cancellation receipts expire a minute after publication, and
the family's next publication deletes them (POSTGRES.md, [Receipt retention
for HTTP writes](POSTGRES.md#receipt-retention-for-http-writes)). A full
ledger refuses that family with `storageWorkLimit` only while 1,024 receipts
younger than a minute remain; it needs no operator action. Receipts an older
binary left expire on the account's next publication of their family after
the fenced upgrade. Do not delete receipts by hand.

## Inbound SMTP commit replies (schema 43)

The fenced schema upgrade removes `td_mta.receiving_receipts`, the old
commit-recovery ledger. Existing messages and their SMTP envelope metadata
stay in the mail tables. The receiving worker returns 250 after PostgreSQL
confirms COMMIT and 451 on an uncertain result. A sender retry may produce
a duplicate if the first delivery committed. Use the consistent pre-upgrade
backup for rollback across this schema change.

## Sender authentication results (schema 46)

Schema 46 adds `td_mta.email_authentication` and
`td_mta.email_dkim_signatures` (POSTGRES.md,
[Direct-MX sender authentication
results](POSTGRES.md#direct-mx-sender-authentication-results)). The
supervised `./td-mta/deploy` path treats the pending migration and the
new `service_grants` entries (INSERT for the receiving login, SELECT for
the frontend login) like any other schema migration, so no manual grant
is needed there. A single-runtime-login installation follows
POSTGRES.md's fenced owner migration procedure and then grants the
runtime role SELECT, INSERT, UPDATE and DELETE on both tables before
`runtime-check`; the role refuses to qualify until it holds them. The
migration is two empty tables, so it needs no backfill and little time
or space.

Existing Emails keep their stored bodies and gain no rows. From the first
delivery under the candidate, the receiver writes one
`Authentication-Results` field instead of its former private trace fields
and `Received-SPF`; scripts that scraped the stored trace should read the
rows instead. Rolling back to a schema-45 binary requires the retained
pre-upgrade backup, as for every schema change, and loses mail received
after the upgrade unless that mail is separately preserved.

## Deploy from the development machine

Run `./td-mta/deploy` from the root of the reviewed development checkout. The command
fetches shared main, selects the newest first-parent descendant of the
installed release with a passing main-integration verdict on this machine,
and pushes that exact Git object into the host's build mirror over the SSH
identity its [target configuration](#deploy-target-configuration) names.
It checks the deployed tree and refuses divergent history. When no newer
revision qualifies it checks the running release and leaves mail online.
A host whose PostgreSQL still uses loopback TLS first takes a stepping
stone instead ([Leaving loopback TLS](#leaving-loopback-tls)).
The main-integration runner owns the verdicts (see DEVELOPMENT.md).
To deploy the fetched main tip even when its integration verdict is missing
or failed, run `./td-mta/deploy --skip-tests`. The deployment receipt records
the override. Ancestry, production preflight, schema recovery, and service
verification still run as applicable.
The launcher builds the Rust controller from a clean checkout of its
submitted commit, using Cargo's frozen dependency set, the
`x86_64-unknown-linux-musl` Rust standard library and `rust-lld`.
The development machine needs the cached Cargo dependencies, the host C
compiler, GNU readelf, Git, SSH, td-mail and the checkout's built
`target/release/td-builder` Cargo runner. The launcher checks the executable for dynamic dependencies,
checks its SHA-256 after transfer, and retains the verified executable
separately from the candidate mail release. Each private job directory
links to its controller for recovery.

A detached systemd job owns each activation through SSH disconnects.
Each job runs one activation. The launcher prints the private receipt
path before activation and creates a new job for a retry. The development
machine checks authenticated public JMAP before and after the job using
the td-mail account the target configuration names (override with
`--jmap-account NAME`), which must be configured with the target's
`jmap_url`. The second check records both observations in
public-jmap.json beside the receipt.

A fresh unprivileged checkout builds the candidate with install-td-mta.
The operator stages the release, validates the epoch, selected configuration
and runtime login, and checks scheduled backup and aggregate health.
Older candidates without schema-plan must pass strict schema-inspect.
A recognized pending migration or a change to required runtime grants
also needs the private PostgreSQL profiles and comparison SQL described
in the online recovery procedure. The deployment takes a fresh verified
base, rehearses the candidate on an independent writable copy inside a
private network namespace on its own socket directory, and proves
archive replay on another copy. The controller reaches each trial through
a holder: a transient unit whose mount namespace shows the trial's socket
directory at `/var/lib/td-mta-postgres`, so the production connection
files and libpq services reach the trial and nothing else changes.

A migration (a schema change, the service identity change or a grant
change) cannot be rolled back once writers return except by restoring
the recovery copy the deployment took and replayed first, losing the
mail accepted since. A deployment that migrates therefore refuses after
building the candidate, before that backup and before any fence, unless
it runs as `./td-mta/deploy --irreversible`; the refused receipt names
`schema_before` and `schema_after`. The flag is recorded in job.json and
applies to that run only.

The default forward activation budget is 60 seconds. On expiry the job
restores service and records the observed outage. Recovery and clean
PostgreSQL shutdown can extend the interruption beyond the budget. Use
`./td-mta/deploy --window-seconds N` to set it for an invocation.
The job records the active timers and conditions the mail and scheduled
writer services on a durable deployment fence. During schema cutover the
job also closes the database to everyone but root after stopping the
writers: it writes and syncs `/etc/td-mta/postgres-fence.conf`
(`unix_socket_permissions = 0`), then sets the live socket to mode 0000.
Root still connects; `td-mta` and the service accounts cannot. A PostgreSQL
restart while the file exists creates the socket closed. Admission removes
the file first, then sets the socket back to 0660. A start after an
interrupted step therefore follows the file, which is the fence's state. It verifies the named restore point and selected
mail evidence before attempting DDL. A preseeded, primary-device copy
is synchronized by content after clean PostgreSQL shutdown. Budget
space for this additional data directory and time to read both copies.
The actual upgrade and runtime checks use the candidate data directory
on its own trial socket. The original shut-down primary remains in the private
job directory. Before writer admission a failed migration can reopen that
original cluster and binary. Once admission begins the job preserves the
selected database, including accepted mail, for operator recovery.

Watch the job and its receipt with `systemctl status td-mta-deploy-XXXXXXXX`
and `sudo cat /var/lib/td-mta-operator/deploy-XXXXXXXX/receipt.json`,
substituting the printed identifier. A job whose controller died or
failed inside its fence is finished with `--recover`
([Recovering an interrupted job](#recovering-an-interrupted-job)).
ACME may need its separate four-minute lease budget after SMTP and JMAP
return. The original data stays in the job directory and the recovery
inputs in `/data/td-mta-physical-recovery` through the rollback window
([Retiring rollback material](#retiring-rollback-material)).

### Rolling back an activation

```text
./td-mta/deploy --rollback
```

The controller, not the launcher, picks the release: the latest
activation or rollback in `/var/lib/td-mta-operator` that installed the
live release and left it selected (verified, or kept because it failed
after writers returned) names, as `previous`, the job's `revision`, with
the tree its retained release's `deployment-inputs.json` names;
`rollback_of` names that job. It refuses when that activation changed
the service identities (run `--restore-shared-services`, see
[Isolating production service accounts](#isolating-production-service-accounts))
or migrated anything (`recovery`, or `schema_before` other than
`schema_after`): a binary rollback cannot undo a migration, and a
forward fix is the path. The job is otherwise an ordinary activation of
a retained release whose provenance the controller checks: the same
fence, budget, verification and automatic rollback, with no
main-integration verdict: that release was the one live before. Unlike an update it does not require the live release healthy
first: it records `live_unhealthy` and the launcher's first JMAP check
may fail. It refuses if the release rolled back to would itself
migrate the database, or its runtime check refuses the current grants:
a release older than upload lease reclamation refuses the runtime
DELETE on `td_mta.leases` every later one needs, or its `config-check`
refuses the selected configuration, which a convergence under the live
release may have written (select that job's `config_previous_version`
first). Retirement removes job
directories after seven days, and with them what a rollback reads. A
second `--rollback` returns to the release the first one replaced, and
a plain `./td-mta/deploy` selects the newest verified main again, so
land the fix first. Rolling back the executable never restores a
database backup: that would discard mail accepted since. A rollback does
not change the selected configuration; `--converge-config --plan`
afterwards shows what the rolled-back release's defaults and policy
would change, and refuses a release older than its policy file.

### Recovering an interrupted job

```text
./td-mta/deploy --recover
```

Every later job refuses while a deployment fence is held. `--recover`
is its own job that keeps the live release; under the activation lock
no controller runs, so what the fence's job left is what its receipt and
the host say. The launcher skips its first public JMAP check, which a
held fence fails. `/var/lib/td-mta-operator/deployment-fence` names that
job; a fence naming no job directory (empty or torn: its controller died
writing it) belongs to the most recent job that recorded `fencing` and
since neither began admission nor recorded an end, and recovery rewrites
it with that job's path; with no such job it stops. The receipt records
`recovered_job` and `recovering JOB MODE: STATE`. Before anything else
a rollback leaves `recover-rollback` in that job's directory, so that a
later recovery never goes forward on what this one half restored:

- `Admitting`: the last step was `opening writers` and the database
  socket fence, which admission removes first, is gone. It recovers
  forward as the controller does: reopens the socket, removes the
  fence, starts the services and the timers that job recorded and
  requires the selected release healthy.
- `Switch` (`--postgres-binaries`, `--to-debian`): the rollback before
  admission, from the receipt. Each switch records, before it fences,
  the source's drop-in paths (`postgres_source_dropins`) and the Debian
  tree's window (`postgres_window`); recovery rebuilds the source's
  and the switch's drop-ins from them and the unit's arguments and
  restores path by path, then starts the source server, requires it to
  be the running executable, admits writers and puts the window back.
- `Activation`: the rollback before admission. Once the receipt records
  `exact recovery point verified` the migration may have stopped the
  primary or moved its data directory: the job's `original-pgdata`,
  the shut-down primary the migration never changes, goes back in
  place once `pg_controldata` reads it shut down with the production
  system identifier (a migrated or half-swapped one goes aside as
  `unreleased-pgdata`; `original primary data directory restored`).
  An isolation cutover's identity settings go, the previous release is
  selected, the socket reopens, and the primary must hold the epoch and
  `schema_before` the job recorded before writers are admitted and the
  release verified (`rollback`). Writers never returned while the fence
  stood, so the original holds everything; the recovery copy is not
  needed and stays for its window.
- `Shared`: a `--restore-shared-services` fence changes no data, so the
  restore is completed against the deployment's recorded epoch.

After a host reboot the fence keeps the services and, for a migration,
PostgreSQL stopped; recovery recreates the start permit. A recovery that
fails before admission leaves the fence for the next `--recover`; once
it begins admission it records `opening writers` first, so the next one
recovers forward. Without a fence, recovery finishes the admission of the
latest job that fenced if its last step was `opening writers` and it
recorded neither its listeners' return nor a finished rollback or
recovery (a controller dying after removing the fence and before
starting the services); else it reopens a socket fence left alone over
the production primary, requires the live release healthy and clears
every job's candidate copy and what the latest job left (`nothing to
recover`). Once the fence is gone
it removes the candidate copy, the start permit, the fence conditions,
the working parts of a recovery copy and the decision, then records
`recovered`. The
loopback-TLS socket cutover is the one fenced job it refuses
([Leaving loopback TLS](#leaving-loopback-tls)). A switch recovered forward leaves no Debian tree
window: `--postgres-binaries` naming the running closure verifies it and
records one.

### Retiring rollback material

Rollback material stays for seven days after its last write: each
earlier job directory under `/var/lib/td-mta-operator` (its receipt, the
original data directory a migration left, uploaded closures, and the
loopback TLS material and CA key the socket cutover moved there), and
each physical recovery copy under `/data/td-mta-physical-recovery` (base
backup, WAL and `bootstrap.tar`; copies taken before the socket cutover
hold the old TLS directory and its CA key). While td's server is
selected, the retained Debian tree `/opt/td-mta/postgresql` is material
too, from the time the switch to the running td closure verified, which
the switch records in
`/var/lib/td-mta-operator/debian-postgresql-retained-since`. Every
switch to a td closure, between td closures included, clears that record
before it stops the server and writes it, with that time, only once
verified; `--to-debian` clears it once the drop-ins are gone, and its
rerun on the Debian server too. Either one's rollback before writer
admission to a td closure puts back the time that closure's own switch
recorded (the receipt says whether), so a switch that fails after
writers returned leaves no window, and nor does a rollback whose source
had none or whose put-back failed: a verified rerun on td's server
(`--postgres-binaries` naming the running closure) writes it when none
is, or none readable. Nothing is written once the tree is gone.
Retirement never writes it: without a record, or unless the running
server is the selected td one mapping only its closure and the backup
job runs its tools, the tree stays, the rest still goes, and the
receipt's `debian_tree_kept` names what to run. Once the tree is gone
the record goes too, `--to-debian` refuses, and the server moves only
between td closures. Removing material cannot be undone, so the command
names that:

```text
./td-mta/deploy --retire-rollback --irreversible
```

It keeps the live release, takes the activation lock, refuses while a
deployment or PostgreSQL fence is held, and first checks the live system
as the binary switch does (source, journal identity, configuration,
health); on td's server it also checks that the running server is the
selected one mapping only its closure and the scheduled backup runs its
tools, and keeps the tree (event and `debian_tree_kept`) when not. It
then checks every piece past its seven days (a root-owned directory with
nothing mounted inside) before removing any, renames each to
`.retired-NAME` and removes it; a rerun finishes such a name whatever
its age. Each removal is a receipt event first, and the receipt lists
what went (`rollback_retired`) and, while any remains,
`rollback_retire_not_before`, the Unix time the next piece may go. When
everything is inside its window it refuses and records that time. A
piece it may not remove refuses the run with its path.

Retiring a job directory also removes what host commands run from it:
the identity change's job directory is where `--restore-shared-services`
returns to the shared identities, so that return ends with it, and
`--refresh-notifications` runs from an activation job directory, which
a fresh `./td-mta/deploy` creates when none is left. A configuration
convergence job's receipt is the only copy of its
`config_previous_version` and, after `uncertain`, of the retained
selection request: resolve an uncertain request before its job
directory is seven days old.


### Leaving loopback TLS

Production's move to the private socket is pending. Its PostgreSQL
still listens on loopback TCP 35432 with TLS (`listen_addresses =
'127.0.0.1'`, `ssl = on`, no Unix socket), `td-mta-maint-postgres-tls-renew.timer`
renews the server's leaf, and the live release predates daceb7309, the
first main commit whose td-mta admits the group-shared socket
(`td_fs::check_server_socket`). Every main revision from c8f8e087d on
refuses TCP+TLS connection files, and this controller activates no
release on a loopback-TLS host; under a release older than daceb7309 the
isolated services could not reach the socket `--postgres-socket`
installs. The launcher therefore enforces one order, from the
development checkout:

1. `./td-mta/deploy --irreversible`, adding `--skip-tests` when no
   stepping stone has a verdict: deploys the stepping stone. Its outcome
   names step 2.
2. `./td-mta/deploy --postgres-socket`: the fenced transport cutover,
   under the stepping stone.
3. `./td-mta/deploy`: the newest verified main, as for any update. With
   no passing verdict after 290c6de7d here it stays in the stepping-stone
   range and says so; wait for the runner's verdict on a newer head, or
   add `--skip-tests` to take the tip untested.
4. `./td-mta/deploy --postgres-binaries`
   ([Switch PostgreSQL to the td-built binaries](#switch-postgresql-to-the-td-built-binaries)).

The launcher reads the host's state before building anything. The
transport is the runtime connection file the controller judges, which
`sudo sed` prints on the host with its password removed and the launcher
parses as the controller does; the live release's capabilities are its
ancestry, in the fetched history, against three reviewed constants in
`td-mta/src/bin/td-mta-deploy/launcher_transport.rs`: daceb7309, 290c6de7d
(c8f8e087d's parent, the last release that still reads TCP+TLS files)
and the loopback-TLS controller below. While the host is on loopback TLS
and the live release lacks daceb7309, every operation but a deploy
refuses and names step 1; once it has it, every operation but
`--postgres-socket` refuses and names step 2; `--converge-config` is
among them, since the controller judges the transport before any mode.
On the socket, `--postgres-binaries` and `--converge-config` refuse and
name step 3 while the live release is a stepping stone (`--to-debian`
stays open; convergence reads `td-mta/production-overrides.conf` from
the live release, which no stepping stone has), adding `--skip-tests`
when no main revision after 290c6de7d has a verdict here. Each refusal prints a
`td-mta-deploy-outcome` line ([Command outcomes](#command-outcomes)).

The stepping stone is the newest first-parent origin/main revision after
the live release that contains daceb7309, is 290c6de7d or older, and has
a passing main-integration verdict on this machine. The runner records
verdicts only for the newest main head, so none can be produced for an
older revision; with none in that range the deploy refuses and names
`--skip-tests`, which takes the newest revision in the range, 290c6de7d,
and records the override as for main. Newest, as for main: the oldest,
daceb7309, would change the least, while every revision in the range
from e691e72b7 on reaches the database through td-postgres's TLS client,
which production has not run; the controller qualifies the candidate
against the live database, and rehearses its migration on a copy, before
any fence. It runs under the controller of daceb7309, the last
one that activates a release on a loopback-TLS host, whose controller
sources are the live release's own. The launcher builds it like any
controller, from Cargo's frozen dependency set; that set includes
tokio-postgres and its closure, which main no longer locks, so when the
Cargo cache has lost them the deploy refuses and says to run `cargo
fetch --locked --manifest-path td-mta/Cargo.toml` in a checkout of
daceb7309. That controller predates `host.json` and fixes this host's
name, build account and source mirror in its sources; the launcher reads
them from there and refuses, naming the `host.json` field that differs,
before anything reaches the host. Its probes also verify the listeners'
certificate against its fixed public name (`mx.timmydouglas.com`), which
the launcher does not compare with `host.json`, since a certificate may
carry several names; if the certificate lacks it, the job fails at its
preflight probes, before any fence. It also predates the
`--irreversible` consent and migrates the database without asking
(schema 45 to 46 here), so the launcher requires the flag; budget the
writer window as for any migration (`--window-seconds`). Budget disk
too: the stepping stone keeps its `original-pgdata` and recovery copy,
step 2 takes another base backup, and `--retire-rollback`, refused on a
loopback-TLS host, retires only material older than seven days.

It has no `--recover`. While its fence is held, its writer fence also
includes an nftables table `td_mta_deploy_SUFFIX` (the job directory's
suffix), which reserves the loopback database connection for root and
does not survive a reboot. A stepping-stone job whose controller dies
inside its fence is outside what `--recover` models, as is the socket
cutover's, and while a fence is held on a loopback-TLS host every
operation refuses and names no next step. While a `td-mta-deploy-*` unit
is still active the launcher says so and prints it instead: the job owns
its fence, so follow its journal and wait; touch nothing by hand while
it runs. A job that hangs stays listed until its unit is stopped
(`systemctl stop UNIT`), and a controller run by hand in a job
directory is no unit, so its fence reads as held. Once none is
running, recover it by hand:
`/var/lib/td-mta-operator/deployment-fence` names the job. Check its
receipt's events, the unit states, the selected executable, both PGDATA
directories (the live one and the job's `original-pgdata`) and the
recovery copy's receipt before reopening writers, and never restore a
retained cluster after writers may have reopened without reconciling
the mail accepted since.

`--postgres-socket` then rehearses the cutover with the live release on
a fresh copy before it changes production, and refuses there too if
that release cannot reach the group-shared socket. Its job directory
under `/var/lib/td-mta-operator` keeps the replaced settings
(`postgres-socket/original/`) and the retired TLS material, CA key
included (`postgres-socket/retired/`), for the rollback window
([Retiring rollback material](#retiring-rollback-material)).

### Converge the configuration

Production's td-mta configuration is the defaults of the release it runs
plus the reviewed `td-mta/production-overrides.conf`. Every deliberate
difference lives in that file, with its reason, so changing production's
configuration is a code review, never an edit on the host. Operator
identity stays out of it: host names, addresses, accounts and keys stay in
the selected configuration and in `target.json` and `host.json`.

```text
./td-mta/deploy --converge-config --plan [--config FILE] [--jmap-account A]
./td-mta/deploy --converge-config --expect-digest HEX [--config FILE] [--jmap-account A]
```

The command takes the policy from the live release's revision with
`git cat-file blob`, so the defaults it differs from and the file were
reviewed together: a change to the file takes effect after the release
that carries it is deployed, and a release older than the file refuses.
The job runs the live release's `postgres config-converge` (POSTGRES.md,
"Configuration convergence command") over the selected version. That
refuses a policy naming a profile production lacks, a profile with
several SMTP listeners, and any plan that exceeds its memory budget or
leaves less than one 32-MiB message buffer unassigned (RESOURCES.md,
"Connection pool sizing"), and validates the complete candidate. The job
first refuses an unfinished deployment or PostgreSQL socket fence, which
`--recover` resolves, then runs an activation's configuration and health
checks and the public SMTP and JMAP probes, and changes nothing
(`refused`) if any fails.

- `--plan` derives the configuration, records the changes and its digest,
  publishes nothing and ends `planned`. Its `next` is the apply command,
  bound to that digest.
- `--expect-digest HEX` applies only that configuration: if what the job
  derives now differs, because the selection, release or materials
  changed, it ends `failed` before publishing and the operator plans
  again. Without it the
  job applies whatever it derives.
- Nothing differs: the job waits for every profile to serve the selected
  generation and ends `unchanged`. Running it again is always safe.
- Otherwise it retains a new version ID in the receipt, publishes the
  converged version under it (settling an uncertain publication under the
  same ID), checks it with `config-check`, retains a selection request for
  the observed generation in the receipt and selects it. A select that
  fails without a definite refusal is inspected once. Each serving profile
  reloads within its 30-second poll, with a brief listener gap. The job
  waits up to 150 seconds for each to serve the new generation, repeats
  the configuration, health and public probe checks, and ends
  `converged`.
- If that verification fails, the job records `config_verify_error`,
  selects the previous version back at the new generation with a second
  retained request, verifies it the same way, ends `restored` and reports
  the original failure.

The receipt records `config_changes` (profile, section, listener label,
key, `before` and `after`, where null is the release default),
`config_profiles` (pool sizes, planned, budget and unassigned bytes),
`config_digest`, `config_generation`, `config_selected` (the version the
job last knows selected, null while a selection is in flight or its
outcome is unknown), `config_reload_seconds` (the 150-second bound on
each profile's reload), `config_restarted_units`, the new and previous
versions and both requests. The launcher prints it and then ends stdout
with the outcome line of "Command outcomes"; a failure also prints
`td-mta-deploy: ERROR` on stderr. Its object holds:

- `result`, one of:
  - `planned`, `unchanged`, `converged`: as above;
  - `restored`: the previous version is selected and verified again;
  - `failed`: the job stopped; `selected_version` is what is selected,
    which is the version it started with unless a restore could not
    select (the new one) or could not verify (the previous one, with
    both failures in `error`);
  - `uncertain`: a selection's outcome is unknown and `request` is its
    retained request;
  - `refused`: the launcher, or the host job's preflight, refused
    before changing anything;
  - `unknown`: the job settled no result the launcher could read: the
    connection was lost, the receipt could not be read, or the job died
    or still runs. Read `receipt` on the host and the journal of `unit`;
    a receipt whose `config_selected` is null had a selection in flight.
- `error`, when the job, or the launcher's public JMAP check after it,
  failed. `converged` with an `error` means the new version stays
  selected though that check failed.
- `selected_version`, `changes`, `config_version`,
  `previous_config_version`, `reload_seconds`, `restarted_units`,
  `request`, `receipt` and `unit`, when known.
- `next`: after a successful plan, the apply command bound to its digest
  with the plan's `--config` and `--jmap-account`, needing nothing; for a
  refusal by the host's transport, the step that comes first ([Leaving
  loopback TLS](#leaving-loopback-tls)), with `--config` only; else null.

To return to the previous configuration by hand, select the receipt's
`config_previous_version` against the current generation with
`config-selection-prepare` and `config-select` (POSTGRES.md,
"Configuration selection commands"). The next convergence reapplies the
policy, so a lasting difference belongs in the reviewed file. After
`uncertain`, resolve the retained request with `config-selection-inspect`
before running the command again, and never prepare a replacement for it
by hand. `--recover` does not settle an `uncertain` or `unknown`
convergence: a convergence holds no fence, so recovery ends with nothing
to recover without reading the request. A serving profile that cannot
load a selected version exits. Once
a converged selection has failed verification, the restore resets and
starts every failed serving unit (`td-mta-frontend`, `-receive`,
`-relay`, `-migadu` and the ACME unit) before verifying the previous
version, and again before it ends `failed` or `uncertain`, and lists them
in `restarted_units`; an `error` naming the restart means a unit would
not start. The job waits up to `reload_seconds`, 150, for each profile in
turn to serve a new generation: its 30-second poll plus the drain; a
slower reload fails verification and is restored. Preflight restarts
nothing: a serving unit that had already failed refuses the job until
`systemctl reset-failed` and `systemctl start` bring it back.

### Deploy target configuration

The tool takes its host, accounts, key and public names only from two
small JSON files outside the repository, which two of its own commands
write from explicit flags. Each file is one object holding exactly the
fields below, as strings. A missing, unknown, duplicate, mistyped or
invalid value refuses with a fixed message that never includes a value,
whether it comes from a flag or a file; no option takes a value starting
with `--`. Nothing falls back to an environment variable or a built-in
value. Neither command changes a file that already says something else
unless given `--replace`, and one that already says the same in a
private file is left as it is. Both are recognized only as the first
argument, and both end with an outcome line ([Command
outcomes](#command-outcomes)).

These come from the operator and the host's administration, not from
the tool. The two commands check each but the last before writing; a
deployment checks the last:

- the values themselves;
- the SHA-256 fingerprint of the host's SSH key, obtained over a channel
  other than SSH (the cloud console or the host's serial log): trusting
  it is the operator's decision, and `--host-key-sha256` enrolls the key
  that has it;
- the key authorized for `ADMIN` on the host, and passwordless `sudo -n`
  for `ADMIN`;
- Cargo in `.cargo/bin` under the build home;
- the td-mail account named below, configured with
  `jmap_url`.

On the development machine, `target.json` says how to reach the host:

```text
./td-mta/deploy --write-target --host HOST --host-name NAME --account ADMIN \
  --identity KEY --jmap-account ACCOUNT --jmap-url URL \
  [--host-key-sha256 SHA256:FINGERPRINT] [--config FILE] [--replace]
```

It writes the file `--config FILE` names, else
`$XDG_CONFIG_HOME/td-mta-deploy/target.json` when that variable is an
absolute path, else `~/.config/td-mta-deploy/target.json`, mode 0600,
creating the default directory mode 0700. Every launcher mode reads the
same file. Its result is `target-written`, `target-unchanged`,
`target-repaired` (the same values in a file others could read, now
private) or `target-replaced`; its `next` is `--provision-host` with the
same `--config`.

With `--host-key-sha256` (a lowercase `--host`, and a target path
without white space, quotes, backslashes, `%`, `$` or `~`), before
writing anything it fetches the keys the host offers with `ssh-keyscan`
on SSH's port and picks the one whose fingerprint (`ssh-keygen -l -E
sha256`) is the given one, refusing if the host offers none or none has
it. The key goes, alone, into the tool's own known_hosts file beside
target.json (its name and `.known_hosts`, mode 0600), written after
target.json; a file that holds anything else refuses with
`refused-differs` unless `--replace`, and anything but a regular file
there refuses. The outcome's `host_key` is `host-key-enrolled`,
`host-key-unchanged`, `host-key-replaced`, or `host-key-repaired` when
the file held the key but others could read it. While that file exists,
every SSH connection of the launcher, the build mirror push included,
requires it to be a regular file, not a link, only its owner can read
and uses it as the only known_hosts file: the global file is empty, ssh
learns no further keys and records no address (`UpdateHostKeys=no`,
`CheckHostIP=no`), DNS fingerprints are off, and StrictHostKeyChecking
refuses any other key. A `KnownHostsCommand` in the operator's own ssh
configuration is not overridden. Without the file, ssh's own
`~/.ssh/known_hosts` decides, as before; deleting it returns there.
Changing `--host` with `--replace` keeps the old host's key in force, so
logins fail until the new one is enrolled.

- `--host` (`host`): the SSH host, a DNS name or IPv4 address, reached
  on SSH's port.
- `--host-name` (`host_name`): the host name the host's own `host.json`
  must state; the launcher refuses before building when they differ.
- `--account` (`account`): the administrator account (`ADMIN`), which
  logs in, runs `sudo -n` and pushes into the build mirror. Lowercase
  letters, digits, `_`, `.` and `-`, starting with a letter or `_`.
- `--identity` (`identity`): the private key file (`KEY`), an absolute
  path or one starting with `~/` for the invoking user's home, without
  `%` or `$`. It must exist as a regular file only its owner can read.
  SSH uses only this key.
- `--jmap-account` (`jmap_account`): the td-mail account of the public
  JMAP checks; a deployment's `--jmap-account` overrides it.
- `--jmap-url` (`jmap_url`): the `https://` discovery URL that account
  must use.

On the host, the root-owned `/var/lib/td-mta-operator/host.json`, mode
0600, describes the host to the supervisor, which refuses every mode
without it. The launcher provisions it over the target's SSH login:

```text
./td-mta/deploy --provision-host --build-account ADMIN --build-home DIR \
  --public-name NAME [--config FILE] [--replace]
```

`host_name` is target.json's. Before changing anything the command
requires the SSH login and `sudo -n` to work, the host's kernel host
name to be `host_name`, the build account to be target.json's `account`,
unprivileged, with `--build-home` as its home directory, Cargo under it,
no symbolic link in the candidate directory's path, the mirror to be
absent or the top of its own Git work tree, `/var/lib/td-mta-operator`
to be absent or a root-owned directory, and an existing `host.json` to
say the same unless `--replace`. Then, as the build account, it creates
the candidate directory and, if absent, an empty Git repository for the
mirror; creates `/var/lib/td-mta-operator` root-owned 0700 if absent;
and places `host.json` root-owned 0600 through `sudo -n`, staged under a
fresh name and moved into place (where none existed, without replacing
one that appeared meanwhile), reading it back. Its result is
`host-provisioned`, `host-unchanged`, `host-repaired` (the same values
in a file of another owner or mode) or `host-replaced`; its `next` is
`null`: provisioning starts no deployment.

- `host_name`: the kernel host name the supervisor requires before it
  acts.
- `--build-account` (`build_account`): the unprivileged account that
  builds each candidate; root is refused. The launcher pushes into its
  mirror as `account`, and Git refuses a push into another account's
  repository, so the two are the same account.
- `--build-home` (`build_home`): that account's home directory, which it
  must own; the supervisor checks the ownership before each build. Its
  source mirror is `src/td` there, candidates build under
  `.local/state/td-mta-candidates`, and Cargo comes from `.cargo/bin`.
- `--public-name` (`public_name`): the DNS name, not an address, that
  the public SMTP and HTTPS listeners' certificate presents. The listener
  probes send it as SNI and HTTP `Host`. An activation probes both
  listeners before it builds, so a deployment, even one with nothing
  newer to deploy, checks the name before anything changes. A
  shared-services rollback does not require the listeners to answer
  before it starts, so check a changed `public_name` with a deployment
  first.

Every launcher mode first requires the SSH login, then reads `host.json`
with `sudo cat` before building, to compare host names, find the mirror
and print the host name. A current controller run in an older job
directory (see "Isolating production service accounts") needs the file
too.

### Command outcomes

`--write-target`, `--provision-host` and `--converge-config` end their
standard output with one line, `td-mta-deploy-outcome` followed by a JSON
object, whether they changed something, found nothing to do or refused.
All hold the following; `--converge-config` adds the members listed under
"Converge the configuration":

- `result`: what happened, one of the results named with each command,
  `refused-differs` when only `--replace` would proceed (a decision for
  whoever supplied the values), or `refused` for any other refusal;
- `error`: on a refusal, the fixed message, which the command also
  prints on standard error before exiting nonzero;
- `next`: the command that follows, as `{"argv": [...], "needs":
  [...]}` with the options whose values the operator supplies, or
  `null`;
- `host_key`: with `--write-target --host-key-sha256`, what happened to
  the enrolled key.

A deployment operation prints the same line when its host's transport
refuses it (`refused`, naming the step to run instead) and when a
stepping-stone deploy verifies (`stepping-stone-deployed`, naming
`--postgres-socket`); see [Leaving loopback TLS](#leaving-loopback-tls).

### Isolating production service accounts

Use the supervised `td-mta/deploy` path for the first release with
`service_grants` in `postgres schema-plan`. It treats the identity change
as a database migration even if the ledger version is already current.
Reserve the same recovery volume, time and primary-device space as for a
schema migration. The personal systemd and Shepherd installer continues
to use its existing operating-system account and runtime login.

The deployment creates five locked Linux accounts and five PostgreSQL
SCRAM logins. For each of `frontend`, `receive`, `relay`, `migadu` and
`acme`, the Linux name is `td-mta-ROLE` and the database name is
`td_mta_ROLE`. PostgreSQL and trusted maintenance continue to run as
`td-mta`. The original owner and shared runtime connection files belong
to that trusted account. Each public process sees its own private state
at the path its unit already names. On the host the corresponding
connection file is `/var/lib/td-mta-services/ROLE/runtime-connection`.
The four serving profiles have their control socket and scratch directory
under `/var/lib/td-mta-services/ROLE/ROLE/`.

On the production systemd host, the deployment controller installs the four
`td-mta-maint-mail-ROLE.service` readiness jobs and makes
`td-mta-health.service` require and wait for them before running its
database, ACME, certificate and backup checks. During preflight it compares
systemd's effective command arguments and flags with the managed checks.
A changed or additional command requires reconciling the aggregate unit
before deployment. The readiness jobs run as the shared account until
the service identity drop-ins are installed. Rolling those identities back
restores the shared account for the same jobs. Both layouts use the same
health timer.
The controller stages the notification monitor and digest configuration from
the existing private configuration. Its sudoers entry permits only the
eight fixed readiness and performance commands as the four service UIDs.
After changing notification recipients or checks in the managed `config.json`,
run `sudo ./td-mta-deploy --refresh-notifications` from a root-owned
isolated deployment job directory (after rollback material is retired,
the one a fresh `./td-mta/deploy` leaves). It validates those eight
commands and atomically refreshes the derived private configuration. The
next monitor and digest invocation use the new settings.

1. The controller saves root-only credentials in
   `/var/lib/td-mta-operator/service-passwords.json`. Every activation
   that takes a recovery copy (a schema migration, this identity change
   or a grant change) includes it in that job's `bootstrap.tar` under
   `/data/td-mta-physical-recovery/`; a binary-only activation writes
   none, and the archive stays on this host. On the private
   rehearsal cluster it creates the five logins, binds the selected
   configuration's secrets and qualifies each grant set. Each of the
   four serving logins cold-loads its selected profile, TLS policy and
   inputs before writer admission. Their profile names and single-role
   assignments must match this roster. ACME uses its own database login.
   The controller repeats the checks on the final cold copy. The
   installed HBA admits each login only on the private socket, by SCRAM,
   and each service unit's identity drop-in adds the `td-mta-db` group
   that reaches the socket. Readiness jobs never connect and keep no
   supplementary group. The local setup-admin peer rule exists only on
   the private copies.
2. With writers fenced, the controller installs the systemd identity
   drop-ins and verifies effective accounts and filesystem permissions.
   After opening writers it checks all five database sessions, listener
   PIDs, Linux UIDs, capabilities, control sockets, ACME certificate,
   public SMTP/JMAP and scheduled health. The original binary and primary
   can both be restored automatically before writer admission.
3. To repeat the checks, run `postgres runtime-check --connection-file
   /var/lib/td-mta-services/ROLE/runtime-connection` with the installed
   td-mta binary as each service's Linux user. Inspect
   `systemctl show UNIT -p User -p Group -p MainPID`. For each mail
   profile run its named readiness job, then start
   `td-mta-maint-backup-status.service` and `td-mta-health.service`.
   Run `acme-status` as `td-mta-acme` with its own connection file and
   `--certificate-profile production --require-ready
   --minimum-validity-seconds 2592000`.
4. After writer admission, keep the selected primary and subsequent mail.
   To return to the recorded shared deployment, run the following from
   the root-owned directory of the **identity cutover** job while its
   candidate is still the selected release. Inspect `shared-rollback.json`
   and repeat the public JMAP and SMTP checks afterward.

   ```sh
   sudo ./td-mta-deploy --restore-shared-services
   ```

   Older job directories contain a private receipt and a deployment archive.
   For rollback or notification refresh in one of those directories, stay
   in the original job directory and invoke the absolute path to
   `td-mta-deploy` from a newer verified Rust job under
   `/var/lib/td-mta-operator/`. The Rust controller recovers the original
   job fields from the private receipt.

   This command qualifies the previous binary against the current epoch,
   configuration and shared grants, fences the writers, restores the
   original unit identities and switches the binary on the same primary.
   Both directions transfer the two relay clock checkpoints under the
   writer fence. The relay validates the database identity, checksum,
   boot and elapsed interval as on an ordinary restart. Check
   `queue-clock-status` for **both** `relay` and `migadu` after the
   retry interval, using the corresponding UID and runtime directory.
   If either remains held, verify database UTC independently, run
   `queue-clock-confirm` for that relay and check for `confirmed`
   as described in DEPLOYMENT.md.
   An incompatible ledger or grant set requires a compatible forward fix.
   The role logins, private directories and root vault are retained for
   a subsequent isolated cutover. A binary rollback between two
   isolated releases keeps the identities and must qualify all five logins.
   A logical restore that only includes the `td_mta` schema requires the
   owner binding step and service grants again before admitting writers.

On this host the root-only libpq service file is
`/var/lib/td-mta-operator/physical-backup/service.conf`. Its
`cutover_repl` profile uses `td_mta_cutover_backup` and `repl.pass`.
The ordinary `cutover_marker` profile uses `td_mta_cutover_marker`,
`marker.pass` and the `postgres` database. On the private socket both
name `host=/var/lib/td-mta-postgres` and `port=35432`, and their
passfile entries name that directory as the host; until then they name
the loopback TLS server, and the socket cutover rewrites them. The controller passes each
service file to `psql` explicitly through `PGSERVICEFILE`.
The same operator directory supplies `compare-selected.sql`, a bounded
read-only transaction producing deterministic rows for schema, epoch,
selected configuration, sampled emails, recipients and body chunks.
The backup volume directory is `/data/td-mta-physical-recovery`, root:root
0700 on the separate data disk. The owner login uses the scheduled backup
libpq profile in `/var/lib/td-mta/backup-control/service.conf`.
The controller uses the selected PostgreSQL 17 tools, systemd,
rsync with --fsync, tar, util-linux and the native build
toolchain. Database passwords stay in protected files on the host.

### Switch PostgreSQL to the td-built binaries

The server first ran PostgreSQL 17 binaries extracted from Debian
packages under `/opt/td-mta/postgresql`. td builds its own
(`postgresql-x86-64`: no ICU, OpenSSL, lz4 or zstd), and TRUST.md, "td
store on a foreign deployment host", says what enters the host's
`/td/store` and what runs it. The switch keeps the live release and the
data directory in place. The host must already use the private socket,
and its live release must not be the loopback-TLS stepping stone (one
from daceb7309 through 290c6de7d); the launcher refuses otherwise and
names the step.

What updating across each PostgreSQL 17 minor release asks of an
installation is a reviewed table in the controller,
`td-mta/src/bin/td-mta-deploy/postgres_minors.rs`, transcribed from each
release's "Migration to Version 17.N" notes and the changelog entries
they name. Each note is one of: a count over the catalogs that must be
zero, in every database of the trial copy or once on the live server;
the rebuild of every GIN index; or an exemption whose reason the table
states. The switch evaluates the notes of every release after the live
minor (Debian's or the live td closure's) up to td's, the version in the
`postgresql-x86-64` store name (17.12 at this writing), and refuses a
release the table does not list, naming the table: extending it, from
that release's notes, is a reviewed change that precedes the switch.
From the reviewed development checkout run:

```text
./td-mta/deploy --postgres-binaries
```

`--window-seconds` and `--jmap-account` work as for a deployment. Before
building anything the launcher reads, over its SSH connection and
read-only, which server the host runs: the td closure that
`91-td-mta-binaries.conf` names, or the Debian server without it. It
reads that server's `postgres --version` and refuses a release the table
does not list. In the clean checkout of the controller
revision, it then builds td-builder and td-recipe-eval and runs
`td-recipe-eval build-run postgresql-x86-64 postgresql-x86-64
glibc-x86-64`. When the development machine built that revision's graph
before, this is a cache hit; otherwise it is a cold build of up to six
hours. The live minor must not be newer than the built one, and the
table must list every release up to it; a unit test of td-mta-deploy
fails while it does not list the pinned release, so a pin bump extends
the table before it reaches a host. The launcher refuses an output
whose ELF interpreter or run path leaves the two store paths, and
resolves what each executable and shared object of
the PostgreSQL output loads as td's loader would: every NEEDED library
must be one already loaded or one the needing object's own run path
finds inside the closure, never the loader's default directories or
cache. It writes each output with `td-builder nar-dump` and uploads the
NAR into the job directory (root 0600), checking its SHA-256 there.
job.json records the `postgres-binaries` operation, each basename with
its NAR hash, the live server it read (`postgres_launcher_source`:
`debian` or the PostgreSQL output's basename) and its minor
(`postgres_launcher_minor`).

The controller then checks, unfenced, what an activation checks, the
private socket, that the live server's executable is the selected one
and the launcher's, and gate 6 below. From a td closure the two drop-ins
in place must be exactly what this controller writes for it. It requires
that the retained `/opt/td-mta/bin/td-mta-operator` has the `nar-hash`
and `nar-restore` verbs, and installs td glibc and then the PostgreSQL
output. A path already in `/td/store` must be a root-owned directory,
writable by root alone, and hash as built. A missing one is unpacked
from its NAR after the file's digest matches, made root's (directories
and executables 0755, other files 0644), hashed again and renamed into
place. Before placing a path the controller removes the partial trees
`/td/store/.BASENAME.partial-*` that an interrupted run left for that
basename, and nothing else. No store path is replaced or removed,
rollback included. The job's uploaded NAR files are deleted however the
job ends. Before any td program runs, td glibc's loader, with an empty
environment, lists (`--list`) the server and every client below and
every shared object under the PostgreSQL output's `lib`; every library
it opens, links followed, must lie inside the two store paths. From here
the controller's own clients are td's (`psql`, `pg_basebackup`,
`pg_verifybackup`, `pg_receivewal`, `pg_isready`, `pg_controldata`,
`pg_dump`), and the loader check also covers `pg_restore`, which the
scheduled backup runs. `postgres --version` must report the store name's
release.

The in-place switch needs all six gates:

1. No database uses the ICU locale provider, and in no database does a
   column, an index or any other object (a view, default, constraint or
   partition key, through `pg_depend`) use an ICU collation.
2. `default_toast_compression` is `pglz`, no column is set to lz4, no
   database has an unlogged table with a variable-length column (a base
   backup copies it empty, so the scan below cannot read its values),
   and a `pg_column_compression` scan of every variable-length column of
   every table and populated materialized view in every database finds
   no lz4 value.
3. `wal_compression` is `off`, `on` or `pglz`, never `lz4` or `zstd`.
4. After the live server stops, td's `pg_controldata` reports `shut
   down`.
5. The td server's first start is on a fresh physical copy.
6. The live server and its minor, both as its `postgres --version` and
   its `server_version_num` report it, are the ones the launcher read,
   a live td server maps code only from its own closure, and
   every minor-release note from it up to td's release holds: each
   count is zero, and a GIN rebuild finds GIN indexes only in `td_mta`.

Gates 1 to 3 run on the live server through the owner login (`td_mta`)
and the marker login (`postgres`), and the notes' cluster counts
through the owner login. When a note rebuilds GIN indexes, the
filesystem of the data directory must have free twice their size and a
GiB more; that refusal leaves the live server running until space is
freed. For gate 5 the controller takes a fresh online base backup,
checks it with td's `pg_controldata`, and
starts td's server on it with the production `postgresql.conf` and
`pg_hba.conf`, overriding only the trial's paths and socket, plus a peer
rule that lets root in as `setup_admin` on that copy alone. Through a
holder, root, the replication and marker logins and every login of the
live release must work; the trial's executable
must be td's, with every executable mapping inside the closure and no
loader variable. As the copy's superuser it repeats gates 1 and 2 in
every database that takes connections, scans every stored value for lz4
(gate 2's scan; the error and the receipt's `postgres_lz4_values` name
each offending column), and requires that each database's collation version
and each used libc collation's version are what td glibc reports, and
that every installed extension is available to td's server. It runs
every database count of the minor-release notes, and when a note
rebuilds GIN indexes it rebuilds, through the owner login as production
will, every GIN index of `td_mta` (td-mta's trigram search index) in one
transaction of at most 600 seconds, timing it, and requires none in
another database. That time sizes the production rebuild: unless
`--window-seconds` is at least twice it and thirty seconds more, the
switch refuses before fencing and names a window a quarter longer, in
whole minutes, to run again with.
The receipt's `postgres_minor_notes` lists each note with `verified`,
`GIN rebuild rehearsed` or `exempt` and its reason, and
`postgres_gin_rebuild_trial_ms` the trial's time. Through the
owner login `pg_trgm` must answer and serve a probe of td-mta's search
through its trigram index, and td's `pg_dump --schema-only` of
`td_mta` must complete. The copy is then removed.

The unit must be `Type=exec` or `Type=simple` (td's server does not
notify systemd), run exactly
`/opt/td-mta/postgresql/usr/lib/postgresql/17/bin/postgres` with plain
arguments, and name the Debian tree in no other command (`ExecCondition`,
`ExecStartPre`, `ExecStartPost`, `ExecReload`, `ExecStop`,
`ExecStopPost`) and no `Environment` variable other than the loader
variables the drop-in unsets; the controller checks this before
installing anything. After the rehearsal the controller fences
the writers as an activation does, writes the database fence
file, stops PostgreSQL, checks gate 4, and installs
`91-td-mta-binaries.conf`: `ExecStart=` emptied and set to the td
`postgres` with the same arguments, and
`UnsetEnvironment=LD_LIBRARY_PATH LD_PRELOAD LD_AUDIT`, and the same
name under `td-mta-maint-backup.service.d`, which runs the scheduled
backup with `PATH` set to td's `bin` alone and the same variables unset,
so td's `pg_dump` dumps td's server whatever tools the generated unit
names. The backup job must exist, and a backup drop-in an interrupted
run left must say the same, with no `EnvironmentFile` in the job,
all checked before fencing. After `daemon-reload` systemd's command
must match the first, and the backup job's `Environment` and
`UnsetEnvironment` the second. It starts PostgreSQL and requires a
closed socket, the production primary, td's executable with the
closure's mappings and no loader variable, and td's release in
`server_version_num`; when the notes rebuild GIN indexes it rebuilds
them through the owner login, requiring the trial's count, and records
`rebuilt GIN indexes`; then the trigram probe. Then it opens the socket,
starts the services and timers, and verifies the live release, every
login and health as after an activation. The receipt records the store
entries (`installed` or `present`), `postgres_source` and
`postgres_source_minor`, and the
events `td-built PostgreSQL started` and `verified td-built
PostgreSQL`.

A failure before PostgreSQL stops leaves production as it was, apart
from the verified store paths. A failure after it stops and before
writer admission is reversed automatically: PostgreSQL stops, the new
drop-ins go, the source's go back (none for the Debian server), the
source server starts on the same data directory and must be the running
executable, the fence opens and the services are verified; the receipt
records `source PostgreSQL restored: NAME` and, as td's `pg_controldata`
read it before the source started, `postgres_td_control_state`. After
admission the job recovers forward on td's server. If the controller
dies or the host reboots inside the window, the deployment fence keeps
the services stopped: run `--recover`
([Recovering an interrupted job](#recovering-an-interrupted-job)).
The next controller run of any mode selects its PostgreSQL tools from
the drop-in: td's while `91-td-mta-binaries.conf` exists, the Debian
tree otherwise. Run `--postgres-binaries` again to finish or verify the
switch; on a host the drop-in already switched it verifies, after
installing the backup job's drop-in if a switch made before the backup
followed the server left it out.

The receipt's events are the evidence: `scheduled backup moved to td's
PostgreSQL tools` after both drop-ins verify, and `verified td-built
PostgreSQL`; a rerun on a switched host records `already on the
td-built PostgreSQL`.

If gate 1, 2 or 3, a collation version or an extension refuses, the
in-place switch is unsafe: td's server cannot read lz4 values, has no
ICU, and could order text differently from the indexes. Stay on the
Debian server and plan a logical move instead, in its own maintenance
window: with the writers stopped, take a logical backup with the
operator's backup tooling (DEPLOYMENT.md, "Local logical backup and an
isolated restore drill"), restore it into a new cluster that td's
`initdb` creates (without ICU, and with `default_toast_compression` and
`wal_compression` td supports), qualify it as the restore drill does,
and only then point the service at it. This controller does not automate
that move. A gate 4 or 5 failure leaves the Debian server running;
inspect the receipt and the trial's evidence. A gate 6 refusal names the
release and note: a release the table does not list needs the table
extended in a reviewed change first; a nonzero count means the cluster
holds what that note concerns, which the switch does not repair, so stay
on the Debian server; a live release other than the launcher's means it
changed meanwhile, so run the switch again.

From a td closure, as after the `postgresql-x86-64` pin changes, the
same command moves to the new one with the same gates, rehearsal,
minor-release table and rollback: after the old server's clean shutdown
the controller replaces each drop-in in one rename, and a failure
before writer admission puts the old ones back, path by path from what
each holds, and restarts the old server. A host switched before the
backup job followed the server has no backup drop-in; the switch adds
one and its rollback removes it. The old store paths stay.

To return to the retained Debian server, run:

```text
./td-mta/deploy --postgres-binaries --to-debian
```

The launcher reads the live td server and the Debian `postgres
--version`, which the table must list, and records them in a
`postgres-to-debian` job (`postgres_launcher_source`,
`postgres_launcher_minor`, `postgres_target_minor`). The controller
checks as the switch does, fences, stops td's server, requires its clean
shutdown, removes both drop-ins, checks that systemd runs the unit's
own Debian command and that the backup job names no store path
(`scheduled backup moved to the Debian PostgreSQL tools`), starts the
Debian server and requires its executable and release, then the
trigram probe, admission and health (`verified Debian PostgreSQL`).
Before admission it also requires every database's collation version
and every used collation's to be what the Debian C library reports,
as the switch to td requires of td glibc. A failure before admission
puts td's drop-ins back and restarts td's server. Minor releases of 17
share the data format, so the Debian release may be older than td's,
and Debian's server reads everything td's can; no rehearsal runs.
Before fencing it requires the Debian tree's server and clients to be
root-owned executables. A rerun on the Debian server
records `already on the Debian PostgreSQL`. The store paths stay. The
launcher refuses `--to-debian` once the Debian tree is retired.

Once switched, the bootstrap archive that each activation's recovery
copy keeps includes the server's drop-in, the backup job's where
present, and the two selected store paths, not the rest of `/td/store`;
the Debian tree only while it is selected or present.

A regenerated backup job keeps the drop-in's tools: its own
`--postgres-bin` and `--tool-library-dir` matter again only on the
Debian binaries, which no longer exist once the tree is retired. The
restore drill runs on the recovery host, not here; its supported layouts
do not include `/td/store`, so a drill that should use td's tools needs
that layout admitted first.
