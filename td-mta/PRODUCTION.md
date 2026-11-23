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
names them. The `ADMIN` account can administer
the services with `sudo`. The daemon runs as `td-mta`. SSH does not grant
access to the former Stalwart host merely because its old DNS name now
points here.

| Item | Location or name |
| --- | --- |
| Source checkout | `/home/ADMIN/src/td`: `src/td` under `host.json`'s `build_home` |
| Supervisor's host description | `/var/lib/td-mta-operator/host.json` |
| Selected executable | `/opt/td-mta/bin/td-mta` (symlink) |
| Retained binaries and build notices | `/opt/td-mta/lib/td-mta/releases/` |
| Installed maintenance helper | `/opt/td-mta/bin/td-mta-operator` |
| Mail state root | `/var/lib/td-mta/mail` |
| Private PostgreSQL connection files | `/var/lib/td-mta/mail/runtime-connection`, `/var/lib/td-mta/mail/owner-connection` |
| PostgreSQL socket directory | `/var/lib/td-mta-postgres` (`td-mta:td-mta-db` 0750) |
| Database writer fence (present only while fenced) | `/etc/td-mta/postgres-fence.conf` |
| PostgreSQL server and tools, once switched | `/td/store/HASH-postgresql-x86-64-17.N/bin`, with td glibc in `/td/store/HASH-glibc-x86-64-VERSION` |
| Retained Debian PostgreSQL | `/opt/td-mta/postgresql/usr/lib/postgresql/17/bin` |
| Binary selection (present only once switched) | `/etc/systemd/system/td-mta-postgres.service.d/91-td-mta-binaries.conf` |
| Per-profile readiness and diagnostics | `/var/lib/td-mta/mail/PROFILE/runtime` |
| System service definitions | `/etc/systemd/system/td-mta-*.service` |
| Root-owned deployment receipts and diagnostics | `/var/lib/td-mta-operator/` |
| Native notification configuration and bounded state | `/var/lib/td-mta/mail/.operations/notifications/` |
| Scheduled database backups | `/var/lib/td-mta/backups/scheduled/` |
| Managed data disk | 64 GiB, Azure data LUN 0, ext4 mounted at `/data` |
| Backup and recovery archive storage | `/data/td-mta/backups/` and `/data/td-mta/migration/`, bind-mounted at their existing `/var/lib/td-mta/` paths |
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

On this host the selected frontend profile sets `event_streams = 4` in
`[limits]`: four stream slots of their own, beside the frontend's
`https_connections` for ordinary requests, which default to sixteen. The
default is eight streams; selecting a configuration without the setting
adopts it. Each graphical
td-mail process opens one stream per eligible watched account, up to two
streams per process. Change this host setting through the versioned
[configuration selection procedure](POSTGRES.md#configuration-selection-commands).
To roll back, select the retained previous version against the current
generation and verify each profile is ready.

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

The inbound SMTP listener permits eight simultaneous sessions, so four peer
keys at two sessions each can hold all of them. The listener's explicit
`session_limit` governs: the default pool of 32 sessions (DESIGN.md section 5)
takes effect only once a selected configuration raises `session_limit` to 32,
and `smtp_sessions` too if the profile sets it. The SMTP session
and DATA limits share a peer key: an IPv4 address (including IPv4-mapped
addresses) or an IPv6 /64. DATA admission has its own fixed limits of four
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

`td-mta-postgres.service` separately runs PostgreSQL 17 on the private Unix
socket `/var/lib/td-mta-postgres/.s.PGSQL.35432`, with no TCP listener and
no TLS. The socket is `td-mta:td-mta-db` mode 0660 in a 0750 directory;
PostgreSQL runs as `td-mta` and joins the memberless system group
`td-mta-db` through its `90-td-mta-socket.conf` drop-in, so it can give the
socket that group. Each login authenticates with SCRAM. Its cluster is
`/var/lib/td-mta/mail/postgres` and its configuration is
`/var/lib/td-mta/mail/postgresql.conf`. It runs td's source-built
PostgreSQL 17 from `/td/store` once switched (see "Switch PostgreSQL to
the td-built binaries"), and the Debian binaries extracted under
`/opt/td-mta/postgresql/usr/lib/postgresql/17/bin` before that; they stay
there for rollback. The deployment controller uses the client tools of
whichever server the `91-td-mta-binaries.conf` drop-in selects. The configuration ends
by including `/etc/td-mta/postgres-fence.conf` if it exists; only a
deployment writes that file, while it holds the writer fence. An MTA binary
update does not require restarting or replacing PostgreSQL.

### Search planner setting

The runtime login in the production mail database has this scoped setting:

```sql
ALTER ROLE td_mta_runtime IN DATABASE td_mta SET random_page_cost = 2;
```

This is an operator setting for this host, not a td-mta schema invariant or a
recommended default for every installation. PostgreSQL's cluster default
remains `4`; owner maintenance sessions retain that default. The setting
applies to all runtime queries, including SMTP and relay work, when their
database sessions next connect. Reloading PostgreSQL does not replace the
defaults of already connected sessions. The frontend was restarted after
applying it; other workers adopt it as their sessions reconnect.

On 2026-10-30, after search backfill and vacuum/analyze, the default planner
scanned all roughly 33,000 search rows for `guix`. The runtime-login query
took 699–774 ms. With cost `2`, PostgreSQL chose the trigram index and took
154–168 ms. Five subsequent HTTPS searches for the first 50 matches had a
299 ms median, versus 863 ms before the setting, with identical ordered
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

The prior role/database setting was absent. Roll back this change with:

```sql
ALTER ROLE td_mta_runtime IN DATABASE td_mta RESET random_page_cost;
```

Reconnect affected sessions and check the effective setting through a fresh
runtime login. Execute these operator statements through a private
connection file or protected libpq service, never a password argument.
A database-only backup does not reproduce role/database defaults: after a
restore, re-evaluate this setting explicitly. It is not needed for data
correctness or schema admission.

## What updates automatically

MTA software does **not** update automatically from Git. A merge, fetch or pull
does not change the running executable. `./install-td-mta` builds and installs
for the chosen prefix; it changes that prefix's executable link but does not
restart services. The procedure below deliberately builds into a candidate
prefix before touching the live installation.

Public certificates renew through `td-mta-acme-production.service`, without
certbot. Leave public TCP 80 reachable for HTTP-01 renewal, as well as SMTP 25
and HTTPS 443. The database has no certificate to renew: the private socket
cutover disabled and removed the former
`td-mta-maint-postgres-tls-renew.timer` and its service. Certificate
management and software updates are independent.

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

The older `td-mta-backup.timer`, `td-mta-acme.service` and
`td-mta-acme-public.service` are disabled; they are not the production workers.
Daily notifications are configured separately below. The scheduled dumps live
on the VM's attached disk. The designated off-host recovery destination is the
development machine. Its first production drill is recorded below. Unattended pulls require a
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

The earlier Python `td-mta-smtp-monitor` and `td-mta-digest` jobs must be disabled
before enabling the native jobs. Retain their private state and deployment
receipts; seed the native collector cursor and last acknowledgement during
migration so old events are not silently lost and the day's digest is not
sent twice. They are historical operator helpers, not part of future updates.
The native helper updates through the standard installer; compare generated
units before replacing operator adaptations such as system User/Group,
filesystem restrictions and resource limits. No Python runtime is needed.

Backup retention is already bounded to seven archives and 4 GiB total; do not
increase it without checking disk capacity. Installed release directories and
candidate builds are not automatically pruned. Keep current and known-good
previous releases, then remove only identified inactive build trees. The
November 2026 cleanup removed the duplicate user-owned `source-archive` only
after checking every file against the retained copy under
`/var/lib/td-mta/migration/stalwart-20261023/source-archive`. Its receipt is
`/var/lib/td-mta-operator/duplicate-archive-cleanup-20261104.json`. Other
migration copies differ and remain retained. Do not treat migration data as
build cache.

## Backup and recovery data disk

The 64 GiB managed data disk uses ext4 UUID
`8fb95fa6-d31b-466f-ba0c-f9174ed941c7`. `/etc/fstab` mounts it at `/data`
with `nodev,nosuid,nofail`; device names such as `/dev/sdc1` are not boot
identities. `/data/td-mta` is owned by `td-mta` with mode 0700. Its
`backups` and `migration` directories retain the original owners, modes,
modification times, extended attributes and file contents. Bind mounts expose them
at `/var/lib/td-mta/backups` and `/var/lib/td-mta/migration`, so backup and
recovery commands retain their existing paths. PostgreSQL, live mail state,
runtime directories and build trees remain on the OS disk.

Both bind mounts explicitly require `data.mount`. Their underlying OS-disk
mountpoint directories are root-owned mode 000, preventing service-user
writes into a fallback directory when a mount is absent. The current and
retired backup services, backup-status service, aggregate health service
and notification digest carry `data-disk.conf` drop-ins with
`RequiresMountsFor` and `AssertPathIsMountPoint` for the backup mount. The
retired Stalwart catch-up service has the equivalent migration-mount
dependency; it remains disabled. Preserve these drop-ins when updating
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

The private migration receipt and SHA-256 verification manifest are retained
under `/var/lib/td-mta-operator/data-disk-migration-20261114/`. Removing the
old OS-disk copies requires verified content and metadata equality and
working persistent mounts; mounting over an old directory alone reclaims
no space. Recovery archives remain retained data, not disposable cache.

## Retired Stalwart source and recovery copies

The former AWS source, whose address is in the operator's private notes,
is retained for recovery, with `stalwart.service` disabled and stopped.
Its old public mail listeners are closed;
`td-mta-stalwart-defer.service` remains enabled on port 25 and sends
only SMTP `421`, so a sender using a stale address must retry instead of
leaving mail on the retired source. The precutover HTTP-01 proxy
`td-mta-acme-precutover.service` is disabled; Azure handles public certificate
renewal directly.

On Azure, `td-mta-stalwart-catch-up.timer` is disabled and its oneshot service
is inactive. The final fenced export, verification, inventory and import
reports are retained under
`/var/lib/td-mta/migration/stalwart-20261023/dns-first-catch-up/`.
The source database, export archives, import mappings and recovery copies
remain intact. The final source contained 32,621 messages; every source ID,
body digest and size matched the destination's retained import mapping, with
no missing current Email. This was a mail-preservation cutover: source
deletions and state reconciliation were intentionally omitted. The retained
provisional import receipts therefore do not claim full state equality.

The old source's original configuration and applied network fence are retained
under `/var/lib/td-mta/operator/cutover-preparation/`; retirement receipts are
under `/var/lib/td-mta/operator/retirement-20261027/` on that host. Its SSH
identity is separate from Azure's: use its address from the private notes
with its retained, verified host key, not the MX DNS names that now resolve
to Azure. The VM has not been terminated. Restarting Stalwart or restoring old DNS is not a rollback of
mail accepted by td-mta; follow "Rolling back to the retired source" below
before reopening a source writer.

The 2026-10-27 isolated restore drill used scheduled archive
`backup-1793082157-3498609.dump`. PostgreSQL restored all tables and constraints
successfully into a private Unix-socket-only cluster with disabled source
login roles. It contained three accounts, 27 mailboxes, 32,758 Emails and
32,770 bodies. Independent verification checked every body's chunk ordering,
length and SHA-256: 751,836,999 bytes passed. No mail service ran against the
restore, and the disposable cluster was stopped and removed. These checks cover
this archive's database restore and body integrity; they do not activate a
recovered service or cover mail arriving after its snapshot. [DEPLOYMENT.md](DEPLOYMENT.md#local-logical-backup-and-an-isolated-restore-drill)
owns the repeatable drill procedure.

On 2026-11-20 UTC the development machine restored the production archive
`backup-1795058100-3649257.dump` from the preceding day's 03:15 UTC backup.
The 528,164,058-byte file and its original receipt were copied over pinned SSH
using read-only access on the VM. The local restore verified 3 accounts, 27
mailboxes, 34,029 Emails, 34,049 bodies and 816,826,743 body bytes. The
archive remains in `/mnt/backup/test/td-mta/copies`. The JSON report
`/mnt/backup/test/td-mta/verified-20261120T024604Z.json` identifies the
exact digest. Scratch under the same private recovery root was removed.
This was a one-time drill. The development machine's daily pull and weekly
restore schedules still require provisioning and enablement.

### Rolling back to the retired source

Keep the retired source and the rollback materials until the owner accepts
the migration and its retention period; terminating the VM or deleting them
waits for that decision.

Fence td-mta writers and delivery before taking the rollback export.
Preserve all target-only messages and local changes; restoring only the old
Stalwart snapshot would lose mail accepted after the switch. Export and
inventory the quiet td-mta account, retain exact raw bodies and metadata,
and reconcile into the rollback server using retained object mappings to
avoid duplicate imports. Independently export and compare the rollback
destination. The td-mta side uses MIGRATION.md's export, verify, inventory
and compare commands; writing into Stalwart has no command in this
repository and uses tooling kept outside it.

Stalwart 0.15.2 returns received times at whole-second precision:
`2026-10-22T07:47:21.998Z` becomes `2026-10-22T07:47:21Z`. The owner
accepts that loss of fractional seconds for an emergency rollback. Report
it as an accepted difference while preserving the exact td-mta export; do
not round comparison timestamps to conceal it. `migration compare` checks
received times exactly and fails with one fixed diagnostic that does not say
what differs, so it fails on this loss too. Establishing that received-time
precision is the only difference takes a separate manual comparison of the
two manifests. Every other message and metadata difference requires
investigation and resolution before routing changes.

Do not send queued submissions twice while moving responsibility between
services. DNS caches can deliver to either host during the transition, so
keep a deliberate queue, defer and drain policy on the one that is not
accepting. Stop `td-mta-stalwart-defer.service` before Stalwart accepts on
port 25 again. Never leave two independent writable mail stores and assume
a later export will merge them. Restore routing and client endpoints only
after the destination and queue ownership are verified. Keep both stores
and the exact rollback archive for investigation; rollback does not
authorize deleting evidence.

## Choose and build an update

Use a reviewed revision whose required checks passed. Normal updates select a
landed commit from `main`; testing an unlanded rolling revision requires its
own exact source and gate receipt. Do not deploy a moving branch name.

The host checkout's `origin` currently names `/srv/git/td.git`, the development
machine's local handoff path. Do not assume that remote is usable on Azure.
Fetch the public backup explicitly (its `main` must contain the chosen commit):

```text
cd /home/ADMIN/src/td
git fetch https://github.com/timmydo/td.git main
git log -1 FETCH_HEAD
```

In Bash, set `revision` to the full reviewed commit hash. The following
commands prepare an isolated checkout and candidate installation. Keep this
shell's variables for the later steps, and stop on any failed command.

```bash
revision=REPLACE_WITH_REVIEWED_FULL_COMMIT_HASH
git cat-file -e "$revision^{commit}"
git merge-base --is-ancestor "$revision" FETCH_HEAD
checkout="$HOME/src/td-deploy-$revision"
candidate="$HOME/.local/state/td-mta-candidates/$revision"
git worktree add --detach "$checkout" "$revision"
cd "$checkout"
./install-td-mta --prefix "$candidate"
candidate_release=$(readlink -f "$candidate/bin/td-mta")
"$candidate_release" --version
sha256sum "$candidate_release"
```

Run the build as `ADMIN`, with the host Rust/C toolchain on PATH. No custom
compiler store paths or secret build environment are needed. Retain the host
libraries used by this native build. Do not use `sudo` for compilation. The
candidate release includes the build input receipt and dependency notices.

During incident work, exact source archives and small deployment drivers were
retained under `/var/lib/td-mta/operator/*/deployment-*`. Those are historical
receipts, not scripts to rerun: they pin old commits, configuration versions
and staging locations. This runbook does not depend on them to build or update.

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
and the backup role. Keep the credential with the backup operator. The streaming procedure uses no permanent replication
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

## Preflight and stage the binary

This procedure is for a **binary-only** update with a compatible database and
configuration. If the change requires a schema upgrade, stop here and follow
the migration contract in [POSTGRES.md](POSTGRES.md): fence application
and maintenance sessions, verify the pre-migration recovery point and
apply the documented migration. Do not run
`schema-upgrade` as a speculative fix. Binary rollback does not undo a schema
or configuration change.

Read the current schema epoch and locate the selected configuration version
in the retained configuration-selection/deployment receipt. Validate that
same version with the candidate. The version is an operator input, not the
output of `config-version` (which generates a new identifier).

```text
sudo -u td-mta /opt/td-mta/bin/td-mta postgres schema-inspect --connection-file /var/lib/td-mta/mail/owner-connection
```

Copy the whole candidate release, including notices, to a new root-owned
release directory. Do not overwrite a retained release:

```bash
release="/opt/td-mta/lib/td-mta/releases/git-$revision"
test ! -e "$release"
sudo cp -a "$(dirname "$candidate_release")" "$release"
sudo chown -R root:root "$release"
sudo chmod -R a+rX,go-w "$release"
sudo sha256sum "$release/td-mta"
sudo -u td-mta "$release/td-mta" postgres config-check \
  --connection-file /var/lib/td-mta/mail/owner-connection \
  --epoch CURRENT_EPOCH --version SELECTED_CONFIG_VERSION
```

The copied binary hash must equal the candidate hash. Require successful
`config-check`. Confirm disk space and recent backup status before activation:

```text
df -h /var/lib/td-mta
sudo systemctl start td-mta-maint-backup-status.service
sudo systemctl start td-mta-health.service
```

Keep a private deployment record containing the commit, source tree hash,
candidate binary hash, gate receipt, selected configuration version and
previous executable path. Save `performance-status` before restarting if
comparing performance: process-local counters reset on restart.

## Activate and verify

The commands in this section use the shared production service identity.
After the identity migration, use the role-specific UIDs and runtime paths in
[Isolating production service accounts](#isolating-production-service-accounts).

Updates briefly interrupt listeners. PostgreSQL and stored mail remain in
place. Pause the aggregate health timer for this window, stop all five MTA
processes, then switch the link. Do not continue if any service fails to stop.
Use a unique temporary link and an atomic rename within the same directory.
Pause both search timers and stop their service jobs before switching binaries;
resume the timers after verifying the new release, or after a verified rollback.
Preserve their durable progress. Schema upgrades additionally fence every
other database user, including backup, scrub and status jobs.

The activation block runs in a subshell: any failed command ends activation
and resumes the health timer. It preserves the previous executable path in a
private receipt before stopping services. A failure after stopping requires
operator rollback; it is not an automatic rollback implementation.

```bash
mail_units=(td-mta-frontend td-mta-receive td-mta-relay td-mta-migadu td-mta-acme-production)
previous=$(readlink -f /opt/td-mta/bin/td-mta)
record="/var/lib/td-mta-operator/manual-$revision"
(
  set -euo pipefail
  sudo mkdir -m 700 "$record"
  printf '%s\n' "$previous" | sudo tee "$record/previous-release" >/dev/null
  trap 'sudo systemctl start td-mta-health.timer' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  sudo systemctl stop td-mta-health.timer
  sudo systemctl stop "${mail_units[@]}"
  for unit in "${mail_units[@]}"; do
    state=$(systemctl show "$unit" -p ActiveState --value)
    case "$state" in
      inactive|failed) ;;
      *) echo "$unit has not stopped" >&2; exit 1 ;;
    esac
  done
  sudo ln -s "$release/td-mta" "/opt/td-mta/bin/.update-$revision"
  sudo mv -T "/opt/td-mta/bin/.update-$revision" /opt/td-mta/bin/td-mta
  sudo systemctl start "${mail_units[@]}"
  wait_ready() {
    local end=$((SECONDS + 120))
    until "$@"; do
      if (( SECONDS >= end )); then return 1; fi
      sleep 2
    done
  }
  for profile in frontend receive relay migadu; do
    wait_ready sudo -u td-mta /opt/td-mta/bin/td-mta status --json --require-ready \
      --runtime "/var/lib/td-mta/mail/$profile/runtime"
  done
  # A previous ACME owner's lease can outlive its process by 180 seconds.
  # The HTTP-01 listener is opened only after the new owner acquires it.
  acme_end=$((SECONDS + 240))
  while :; do
    acme_pid=$(systemctl show td-mta-acme-production -p MainPID --value)
    if [[ "$acme_pid" =~ ^[1-9][0-9]*$ ]] &&
       sudo ss -H -ltnp 'sport = :80' | grep -Fq "pid=$acme_pid,"; then
      break
    fi
    if (( SECONDS >= acme_end )); then exit 1; fi
    sleep 2
  done
  wait_ready sudo -u td-mta /opt/td-mta/bin/td-mta acme-status \
    --connection-file /var/lib/td-mta/mail/runtime-connection \
    --certificate-profile production --require-ready --minimum-validity-seconds 2592000
  test "$(systemctl show td-mta-acme-production -p MainPID --value)" = "$acme_pid"
  sudo systemctl start td-mta-health.service
)
```

ACME process startup has a four-minute retry window because the previous
owner's 180-second database lease survives shutdown. During that interval,
systemd can retry with `runtime-claim` errors; do not clear the lease by hand.
`acme-status` reports persisted certificate health, so it cannot alone prove
that the new renewal worker is running. Require its HTTP-01 listener first,
then the same PID after the certificate check. The process hash checks below
remain required.

Each persisted or runtime readiness check has a two-minute retry window; the command's own bounded
execution time may extend the final attempt. A service being `active` alone is
not success. If startup or readiness fails, inspect the journal and use the
rollback procedure below. Confirm that the health timer resumed even after a
failed attempt.

Check every running process, not merely the selected link. For each of the
five units, read its nonzero `MainPID` and compare `/proc/PID/exe` with the
candidate SHA-256:

```text
systemctl show td-mta-frontend -p MainPID -p NRestarts -p ActiveState
sudo sha256sum /proc/REPLACE_WITH_MAINPID/exe
```

Repeat for receive, relay, migadu and acme-production. Record the hashes and
readiness results with the deployment receipt. Check public TLS/JMAP access
from td-mail and the changed behavior with an appropriate acceptance test.
Readiness does not prove a remote recipient accepted an outbound message.

## Roll back a binary-only update

To return across the Linux and database identity cutover, use the recorded
job's shared-service rollback procedure in
[Isolating production service accounts](#isolating-production-service-accounts).

Use the **recorded** previous executable, not whichever release happens to be
second in a directory listing. Returning to a release older than upload lease
reclamation also needs that section's `REVOKE` before the old executable
starts. Preserve the failed candidate and its logs.
Stop all five services and verify their stopped states as above, then:

If reconnecting after a failed update, recover `previous` from the private
`manual-COMMIT/previous-release` receipt and set `revision` and `mail_units`
again. The following block refuses to switch while any worker remains active:

```bash
(
  set -euo pipefail
  test -x "$previous"
  trap 'sudo systemctl start td-mta-health.timer' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  sudo systemctl stop td-mta-health.timer
  sudo systemctl stop "${mail_units[@]}"
  for unit in "${mail_units[@]}"; do
    state=$(systemctl show "$unit" -p ActiveState --value)
    case "$state" in
      inactive|failed) ;;
      *) echo "$unit has not stopped" >&2; exit 1 ;;
    esac
  done
  sudo ln -s "$previous" "/opt/td-mta/bin/.rollback-$revision"
  sudo mv -T "/opt/td-mta/bin/.rollback-$revision" /opt/td-mta/bin/td-mta
  sudo systemctl start "${mail_units[@]}"
)
```

Repeat all readiness, certificate, process-hash and aggregate health checks;
resume `td-mta-health.timer`. A rollback is complete only after those checks
pass. Do not restore a database backup to roll back an executable: that would
discard mail accepted since the backup.

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


## Persistent header summary upgrade and backfill

Schema 33 adds `blob_summaries`. This is a schema update, so the binary-only
activation procedure above is insufficient. Follow POSTGRES.md's explicit
owner migration procedure: verify a backup, stop all five MTA processes and
other database maintenance jobs, upgrade with the candidate binary and owner
connection preserving existing explicit service grants, then add
SELECT/INSERT/UPDATE/DELETE on `blob_summaries` for the runtime role. The upgrader
rejects PUBLIC access, expanded explicit grants and more than 4096 nonowner
schema/table/column ACL entries; do not change grants concurrently. Newly added
tables still require their explicit grants before runtime qualification.
Run candidate `schema-inspect`, `runtime-check` and `config-check` before
starting the candidate. Keep the schema-32 backup and old binary together as
pre-upgrade recovery evidence; restoring that backup after new mail arrives
would lose that mail unless it is separately preserved and reconciled.

After readiness, fill historical summaries using the normal owner command:

```text
sudo -u td-mta /opt/td-mta/bin/td-mta postgres summary-backfill --connection-file /var/lib/td-mta/mail/owner-connection --epoch CURRENT_EPOCH --account ACCOUNT_ID --limit 16
```

Keep each successful JSON report privately. If `done` is false, pass its
`after` value as `--after BLOB_ID` on the next invocation. Run one page at a
time; reduce `--limit` if a page exhausts its finite work allowance. On failure,
do not advance the cursor. This command rebuilds existing rows as well as
filling absent ones, so replay is safe and restarting without a cursor is an
explicit full rebuild. `skipped` identifies unrenderable summaries by count;
those messages retain verified-parser fallback and their original raw bytes.
Repeat for each account. New messages get summaries during publication and do
not require a scheduled backfill. Compare `summary_reuse` and `body_verify`
performance-counter deltas around cold client reads after the backfill.

Schema 34 adds `blob_interpretations` using the same fenced owner upgrade
procedure. Include that table in restored runtime mail grants and retain a
schema-33 backup with its compatible binary. After restart, add `--structure`
to the summary-backfill command to rebuild MIME evidence as well as headers.
Track `structures_written` and `structures_skipped` in its reports; skipped
messages retain verified parsing. Compare `structure_reuse` and
`jmap_structure` counter deltas for individual message opens. MIME evidence alone does not eliminate attachment reads during a message
open; that requires the selective verification path below.

Selective Email/get verification uses existing schema-34 MIME evidence. Its
binary update needs no additional schema upgrade or backfill if
`summary-backfill --structure` has already completed. Missing evidence keeps
the complete-body fallback. Raw downloads and owner integrity audits still
verify the whole message; successful text views do not certify unread
attachments. The independent whole-body scrub below checks those bytes.

## Persistent search fields

Schema 35 adds `blob_search`. Use the same fenced owner upgrade procedure,
including a verified backup and restoring ordinary runtime mail grants on
this table. Check the selected configuration with the candidate before
activation: search-field buffers add 20 MiB per body/search job to the resource
plan (60 MiB for the default three jobs), which can invalidate a tightly
configured memory budget. The default remains within its 512 MiB budget.

Fill search evidence with the standard owner command, one page at a time:

```text
sudo -u td-mta /opt/td-mta/bin/td-mta postgres summary-backfill --connection-file /var/lib/td-mta/mail/owner-connection --epoch CURRENT_EPOCH --account ACCOUNT_ID --limit 16 --search
```

Use each successful `after` cursor until `done`, as for summary backfill, and
repeat for each account listed by `postgres account-list`. Record
`searches_written` and `searches_skipped` separately from header-summary counts.
The search-output page cap may produce fewer than sixteen entries. A skipped
interpretation retains the original parser fallback; completion of the scan
does not mean every message acquired search evidence. On failure, preserve the
last successful cursor and inspect the error; an unknown COMMIT is not a
safe automatic retry. `--structure` may be combined with `--search` when both
kinds of evidence need rebuilding.

Compare `search_fields`, `jmap_search` and `body_verify` counter deltas around
the same scoped search before and after backfill. Cache hits avoid repeated
MIME decoding and full-body reads after validating retained search evidence.
Missing or invalid evidence uses verified-body fallback. Large searches can
still exceed request work or connection bounds. Schema 40 uses pg_trgm for
eligible literal terms, then checks candidates with the existing matcher;
unindexed messages and ineligible terms retain bounded fallback.
Production now runs `td-mta-maint-search.timer` once per minute and
`td-mta-maint-search-status.timer` every fifteen minutes, with durable progress
in `/var/lib/td-mta/search-index/`. Missing or incompatible search rows are
prepared in bounded pages; compatible existing rows are skipped. New messages
use bounded parser fallback until prepared. Completed cycles wait five minutes
before starting again. Unsupported messages remain visible as explicit partial
search results; an operational failure is not an unsupported-message skip.

Check the last service result and durable progress, not only timer activity:

```text
systemctl status td-mta-maint-search.timer td-mta-maint-search-status.timer
sudo journalctl -u td-mta-maint-search.service -u td-mta-maint-search-status.service --since today
sudo systemctl start td-mta-maint-search-status.service
```

See [automatic search preparation](DEPLOYMENT.md#automatic-search-preparation)
for generation, bounds and uncertainty recovery. These jobs do not replace
integrity scrubbing or backups. The daily digest includes configured search-status inspection results.

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

## Upload lease reclamation grant

Each upload deletes a bounded batch of the account's expired upload leases
and releases their quota (POSTGRES.md, [Expired upload lease
reclamation](POSTGRES.md#expired-upload-lease-reclamation)), so the runtime
login needs DELETE on `td_mta.leases`. A binary with this reclamation
refuses a runtime login without that grant, and an older binary refuses one
with it, so this update is not binary-only. With all five MTA processes
stopped as for activation, grant it as the schema owner on the primary, and
to a replica database's runtime login too, then run the candidate's
`runtime-check` and `config-check` before switching the link. Run
`usage-inspect` for each account first: an account whose usage counters
disagree with its leases refuses every upload until repaired, because
reclamation releases counters before charging:

```sql
GRANT DELETE ON td_mta.leases TO td_mta_runtime;
```

Rolling back to an older binary requires `REVOKE DELETE ON td_mta.leases
FROM td_mta_runtime;` with the services stopped, before starting the
previous executable. Leave the replica reader's grants unchanged. An account
whose upload quota filled under an older binary recovers as it uploads again
(each attempt, even one refused for quota, reclaims one batch) and, from
relay retention turns, on its own while a relay route serves it
(POSTGRES.md, [Relay retention cycle](POSTGRES.md#relay-retention-cycle)).
Do not delete leases by hand.

## Inbound SMTP commit replies (schema 43)

The fenced schema upgrade removes `td_mta.receiving_receipts`, the old
commit-recovery ledger. Existing messages and their SMTP envelope metadata
stay in the mail tables. The receiving worker returns 250 after PostgreSQL
confirms COMMIT and 451 on an uncertain result. A sender retry may produce
a duplicate if the first delivery committed. Use the consistent pre-upgrade
backup for rollback across this schema change.

## Sender authentication results (schema 46)

Schema 46 adds `td_mta.email_authentication` and
`td_mta.email_dkim_signatures` (POSTGRES.md, [Direct-MX sender authentication results](POSTGRES.md#direct-mx-sender-authentication-results)). The supervised
`./td-mta/deploy` path treats the pending migration and the new
`service_grants` entries (INSERT for the receiving login, SELECT for the
frontend login) like any other schema migration, so no manual grant is
needed there. A single-runtime-login installation follows the fenced owner
upgrade procedure above and then grants the runtime role SELECT, INSERT,
UPDATE and DELETE on both tables before `runtime-check`; the role refuses
to qualify until it holds them. The migration is two empty tables, so it
needs no backfill and little time or space.

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
substituting the printed identifier. After an interrupted host boot,
`/var/lib/td-mta-operator/deployment-fence` names the owning job. Check
its event record, unit states, selected executable, both PGDATA directories
and the recovery receipt before reopening writers. Never restore a retained
cluster after writers may have reopened without reconciling subsequent mail.
ACME may need its separate four-minute lease budget after SMTP and JMAP
return. Keep recovery inputs and the original data through the rollback
window.


### Deploy target configuration

The tool takes its host, accounts, key and public names only from two
small JSON files the operator provisions outside the repository. Each is
one object holding exactly the fields below, as strings. A missing,
unknown, duplicate, mistyped or invalid field refuses with a fixed
message that never includes a value. Nothing falls back to an
environment variable or a built-in value.

On the development machine, `target.json` says how to reach the host. The
launcher reads the file `--config FILE` names, else
`$XDG_CONFIG_HOME/td-mta-deploy/target.json` when that variable is an
absolute path, else `~/.config/td-mta-deploy/target.json`. For example:

```json
{
  "host": "192.0.2.10",
  "host_name": "mx1",
  "account": "alice",
  "identity": "~/.ssh/td-mta-deploy",
  "jmap_account": "alice",
  "jmap_url": "https://mx.example.org/.well-known/jmap"
}
```

- `host`: the SSH host, a DNS name or IPv4 address. There is no port
  field: for another port, name a `Host` alias from `~/.ssh/config`.
- `host_name`: the host name the host's own `host.json` must state; the
  launcher refuses before building when they differ.
- `account`: the administrator account (`ADMIN`), which logs in, runs
  `sudo` and pushes into the build mirror. Lowercase letters, digits, `_`,
  `.` and `-`, starting with a letter or `_`.
- `identity`: the private key file (`KEY`), an absolute path or one
  starting with `~/` for the invoking user's home, without `%` or `$`.
  SSH uses only this key and requires the host's key to be known already.
- `jmap_account`: the td-mail account of the public JMAP checks;
  `--jmap-account` overrides it.
- `jmap_url`: the `https://` discovery URL that account must use.

On the host, the root-owned `/var/lib/td-mta-operator/host.json`, mode
0600, describes the host to the supervisor, which refuses every mode
without it:

```json
{
  "host_name": "mx1",
  "build_account": "alice",
  "build_home": "/home/alice",
  "public_name": "mx.example.org"
}
```

- `host_name`: the kernel host name the supervisor requires before it
  acts.
- `build_account`: the unprivileged account that builds each candidate;
  root is refused.
- `build_home`: that account's home directory, which the account must
  own. Its source mirror is `src/td` there, candidates build under
  `.local/state/td-mta-candidates`, which must exist with no symbolic
  link anywhere in its path, and Cargo comes from `.cargo/bin`. The
  launcher pushes into that mirror as `account`, and Git refuses a push
  into another account's repository unless that repository is listed in
  `safe.directory`, so use the same account for both.
- `public_name`: the DNS name, not an address, that the public SMTP and
  HTTPS listeners' certificate presents. The listener probes send it as
  SNI and HTTP `Host`. An activation probes both listeners before it
  builds, so a deployment, even one with nothing newer to deploy,
  checks the name before anything changes. A shared-services rollback
  does not require the listeners to answer before it starts, so check a
  changed `public_name` with a deployment first.

Install it with `sudo install -o root -g root -m 0600 host.json
/var/lib/td-mta-operator/host.json`. The launcher reads it with `sudo
cat` over SSH before building, to compare host names, find the mirror and
print the host name; when that read fails it says so without telling an
absent file from a refused login. A current controller run in an older
job directory (see the next section) needs the file too.

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
run `sudo ./td-mta-deploy --refresh-notifications` from
the root-owned isolated deployment job directory. It validates those eight
commands and atomically refreshes the derived private configuration. The
next monitor and digest invocation use the new settings.

1. The controller saves root-only credentials in
   `/var/lib/td-mta-operator/service-passwords.json`. Back up that file
   alongside the protected bootstrap archive. On the private rehearsal
   cluster it creates the five logins, binds the selected configuration's
   secrets and qualifies each grant set. Each of the four serving logins
   cold-loads its selected profile, TLS policy and inputs before writer
   admission. Their profile names and single-role assignments must match
   this roster. ACME uses its own database login. The controller repeats
   the checks on
   the final cold copy. The installed HBA admits each login only on the
   private socket, by SCRAM, and each service unit's identity drop-in adds
   the `td-mta-db` group that reaches the socket. Readiness jobs never
   connect and keep no supplementary group. The local setup-admin peer
   rule exists only on the private copies.
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
   the certificate profile and minimum validity used above.
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
`marker.pass` and the `postgres` database. Both name
`host=/var/lib/td-mta-postgres` and `port=35432`, and their passfile
entries name that directory as the host. The controller passes each
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

### Move PostgreSQL to the private socket

Before this change the production database listened on loopback TCP
35432 with verified TLS. The current controller manages only the private
socket and refuses any other operation on a host still using loopback
TLS, saying so. Move a host once with the fenced transport cutover.

The live release must admit a group-shared PostgreSQL socket
(`td_fs::check_server_socket`, which td-mta's connection worker calls)
and still reach PostgreSQL over loopback TLS. An older release refuses
the socket, and the cutover's rehearsal fails before anything in
production changes; a release that reaches PostgreSQL only over the
socket refuses the host's TCP connection files and cannot start there.
If the live release is older, first deploy, with a controller from before
this change, a release that admits the group-shared socket and still
speaks loopback TLS, then run the cutover from the current checkout:

```text
./td-mta/deploy --postgres-socket
```

The job keeps the live release; it accepts `--window-seconds` and
`--jmap-account` as for a deployment. The launcher records the
`postgres-socket` operation in `job.json`, and the controller refuses a
job whose recorded operation differs from the one it was asked to run. A
second run on a host already on the socket first finishes retiring the
loopback TLS units and material, as below, if a failed cutover left any,
then verifies health and reports `already on the private socket`.

Before the window, check on the host that the owner and runtime
connection passwords use only letters, digits, `-`, `.`, `_` and `~`
(the forms the socket URI carries verbatim; provisioned passwords are
hexadecimal), and that `/etc/td-mta`, if it exists, is root-owned mode
0755: the server configuration includes the fence file there, so it must
stay so afterwards.

Unfenced, the controller checks the release and primary as for an
activation, creates the system group `td-mta-db` if it is missing
(nonzero, unique and memberless), creates or verifies `/etc/td-mta`
(root-owned 0755), creates the empty socket directory
`/var/lib/td-mta-postgres` (`td-mta:td-mta-db` 0750) that no
configuration yet names, and stages each new file in
`postgres-socket/staged/` under the job directory with the owner, group
and mode of the file it replaces:

- `postgresql.conf` without its listen, TLS and socket settings, ending
  with `listen_addresses = ''`, `ssl = off`, the socket directory,
  `unix_socket_group = 'td-mta-db'`, `unix_socket_permissions = 0660` and
  the fence include. An include or another `ssl_*` setting is refused.
- `pg_hba.conf` with each `hostssl DB USER 127.0.0.1/32 scram-sha-256`
  rule rewritten in place as `local DB USER scram-sha-256` (DB is
  `td_mta`, `replication`, `postgres` or `all`). TCP `reject` rules stay;
  a socket rule must already use SCRAM or `reject`; anything else is
  refused.
- The owner and runtime connection files and, if isolated, each service's
  `runtime-connection`, as
  `postgresql://USER:PASSWORD@:35432/td_mta?host=%2Fvar%2Flib%2Ftd-mta-postgres`
  with the same credentials.
- Both libpq service files and every passfile they name: `host` becomes
  the socket directory, `hostaddr` and the TLS settings go, and passfile
  entries name the socket directory as their host.
- The `td-mta-postgres.service.d/90-td-mta-socket.conf` drop-in
  (`SupplementaryGroups=td-mta-db`), the service identity drop-ins with
  that group if the services are isolated, and the aggregate health
  drop-in without its two database certificate checks.

It refuses if any other loaded `td-mta-*` unit's command names a
`/var/lib/td-mta*/.../tls` path, such as a generated certificate expiry
check of the database leaf, or a `--*connection-file` outside the files
above, and if a notification configuration's `connection` is another
file: such a job would fail once the transport moves. Remove or
regenerate it first.

It then takes a fresh online base backup through the existing TLS
service and starts a trial from it with the staged configuration on its
own socket directory, with no network. Through a holder that also binds
each staged connection, service and passfile read-only over its
production path, it requires: readiness, an owner query and a
replication `IDENTIFY_SYSTEM` as root; the owner, runtime and, if
isolated, every service login through the live release exactly as
activations check them; the staged settings as `postgres -C` reports
them; a `td-mta:td-mta-db` 0660 socket in a 0750 directory; and no TCP
listener in the trial's network namespace. The trial and holder stop and
the copy and its base backup are removed. Any failure up to here leaves
production as it was, apart from the group and `/etc/td-mta`, and
removes the socket directory again. The trial runs with production's
memory settings beside the live server; schedule the window when the
host has that much memory free.

The controller then fences the writers as an activation does, writes the
database fence file, stops PostgreSQL and requires a clean shutdown. It
checks that every file is unchanged since staging, saves each original
in `postgres-socket/original/` with its metadata, installs the staged
files atomically and starts PostgreSQL. It requires the socket closed
(mode 0000), root's checks through it, an empty `listen_addresses`,
`ssl` off and no listener on TCP 35432. It opens the socket, starts the
services and the recorded timers except the renewal timer, and verifies
the live release, every login and health as after an activation. Only
then it runs `systemctl disable --now` on
`td-mta-maint-postgres-tls-renew.timer` and its service, which also
removes the links the operations installer's printed commands made. It
moves the unit files that installer generated under
`PREFIX/share/td-mta/services` (the `.service`, `.timer` and `.scm`) into
`postgres-socket/retired/units/`, removes any link left to them, reloads
systemd and requires both units to be gone. A renewal unit file found
anywhere else was not td-mta's: the job leaves it in place and records
it as `renewal_units_left`. Finally it moves `/var/lib/td-mta/mail/tls`
and each service's `tls` directory into `postgres-socket/retired/`. The
receipt records the staged files, the originals, the group, the renewal
timer, the retired and left unit files and the moved material.

Verify the receipt's `verified private socket transport` and `retired
loopback TLS` events, then:

```text
sudo ls -l /var/lib/td-mta-postgres
sudo ss -ltnp 'sport = :35432'
systemctl show td-mta-frontend.service -p SupplementaryGroups
sudo -u td-mta /opt/td-mta/bin/td-mta postgres runtime-check \
  --connection-file /var/lib/td-mta/mail/runtime-connection
```

The listing shows `.s.PGSQL.35432` as `srw-rw---- td-mta td-mta-db`,
`ss` prints nothing, and each isolated service shows `td-mta-db`. Repeat
the service login checks of the isolation section with `runuser -u
td-mta-ROLE -g td-mta-ROLE -G td-mta-db`, and the public SMTP and JMAP
checks.

A failure before writer admission is reversed automatically: PostgreSQL
stops, every original file and setting returns, the new drop-in is
removed, PostgreSQL starts on loopback TLS, the fence is removed and the
services are verified; the receipt records `transport restored`. After
admission the job recovers forward and keeps the socket. To return to
loopback TLS later, fence the writers by hand, stop PostgreSQL, copy each
file in `postgres-socket/original/` back over its path with its owner,
group and mode, remove `90-td-mta-socket.conf`, move the retired `tls`
directories back, copy the files in `postgres-socket/retired/units/`
back to the paths `renewal_units_retired` records, run `systemctl
daemon-reload`, start PostgreSQL, `systemctl link` the service and
`systemctl enable --now` the timer by those paths, and use a controller
from before this change. td-mta refuses a TCP connection file, so the
live release must also be one that still reaches PostgreSQL over
loopback TLS.
Check the certificates' validity first: nothing renews them meanwhile.

If the receipt's `renewal_units_left` is not empty, remove those files
and run `systemctl daemon-reload` after the rollback window; td-mta's
tooling did not install them. Renewal unit files the operations
installer generated but nobody ever linked are not loaded, so the job
cannot find them: delete any `td-mta-maint-postgres-tls-renew.*` left
under the operations prefix's `share/td-mta/services`. Keep the job
directory, its originals and the retired TLS material, including the CA
key, through that window; delete them only after the operator accepts
the cutover.

If the controller itself dies mid-cutover (killed, power loss), every
later mode refuses on the deployment fence or the stale socket fence
file. Recover by hand from the receipt: if it lacks `private socket
installed`, stop PostgreSQL, copy each file in
`postgres-socket/original/` back with its owner, group and mode (if that
directory is absent nothing was installed: skip the copy and the
removal), remove every installed file the receipt's `socket_files` lists
that has no original (the new drop-ins), run `systemctl daemon-reload`,
remove `/etc/td-mta/postgres-fence.conf`, start PostgreSQL and confirm
it listens on loopback TLS again; otherwise the socket is live, so
remove the fence file and run `sudo -u td-mta chmod 0660
/var/lib/td-mta-postgres/.s.PGSQL.35432`. Then, as for any interrupted
job above, remove `/var/lib/td-mta-operator/deployment-fence` (the
services' start condition), start the services and timers the receipt
recorded, require health, and run `--postgres-socket` again; on the
socket it finishes the retirement.

### Switch PostgreSQL to the td-built binaries

The server first ran PostgreSQL 17 binaries extracted from Debian
packages under `/opt/td-mta/postgresql`. td builds its own
(`postgresql-x86-64`: no ICU, OpenSSL, lz4 or zstd), and TRUST.md, "td
store on a foreign deployment host", says what enters the host's
`/td/store` and what runs it. The switch keeps the live release and the
data directory in place. The host must already use the private socket.

First read the PostgreSQL 17 release notes from the live Debian minor
up to td's release, the version in the `postgresql-x86-64` store name
(17.12 at this writing), for steps an update needs, such as a reindex.
Find the live minor with:

```text
sudo LD_LIBRARY_PATH=/opt/td-mta/postgresql/usr/lib/x86_64-linux-gnu \
  /opt/td-mta/postgresql/usr/lib/postgresql/17/bin/postgres --version
```

Then run, from the reviewed development checkout:

```text
./td-mta/deploy --postgres-binaries --postgres-minor-reviewed 17.N
```

`--window-seconds` and `--jmap-account` work as for a deployment. Before
building anything the launcher reads the live Debian `postgres
--version` over its SSH connection (read-only) and refuses unless it
reports the reviewed minor. In the clean checkout of the controller
revision, it then builds td-builder and td-recipe-eval and runs
`td-recipe-eval build-run postgresql-x86-64 postgresql-x86-64
glibc-x86-64`. When the development machine built that revision's graph
before, this is a cache hit; otherwise it is a cold build of up to six
hours. The reviewed minor must not be newer than the built one. The
launcher refuses an output whose ELF interpreter or run path leaves the
two store paths, and resolves what each executable and shared object of
the PostgreSQL output loads as td's loader would: every NEEDED library
must be one already loaded or one the needing object's own run path
finds inside the closure, never the loader's default directories or
cache. It writes each output with `td-builder nar-dump` and uploads the
NAR into the job directory (root 0600), checking its SHA-256 there.
job.json records the `postgres-binaries` operation, each basename with
its NAR hash and the reviewed minor.

The controller then checks, unfenced, what an activation checks, the
private socket, that the live server's executable is the Debian
`postgres`, and gate 6 below. It requires that the retained
`/opt/td-mta/bin/td-mta-operator` has the `nar-hash` and `nar-restore`
verbs, and installs td glibc and then the PostgreSQL output. A path
already in `/td/store` must be a root-owned directory, writable by root
alone, and hash as built. A missing one is unpacked from its NAR after
the file's digest matches, made root's (directories and executables
0755, other files 0644), hashed again and renamed into place. Before
placing a path the controller removes the partial trees
`/td/store/.BASENAME.partial-*` that an interrupted run left for that
basename, and nothing else. No store path is replaced or removed,
rollback included. The job's uploaded NAR files are deleted however the
job ends. Before any td program runs, td glibc's loader, with an empty
environment, lists (`--list`) the server and every client below and
every shared object under the PostgreSQL output's `lib`; every library
it opens, links followed, must lie inside the two store paths. From
here the controller's own clients are td's (`psql`, `pg_basebackup`,
`pg_verifybackup`, `pg_receivewal`, `pg_isready`, `pg_controldata`,
`pg_dump`). `postgres --version` must report the store name's release.

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
4. After the Debian server stops, td's `pg_controldata` reports `shut
   down`.
5. The td server's first start is on a fresh physical copy.
6. `--postgres-minor-reviewed` names the live Debian minor, both as its
   `postgres --version` and its `server_version_num` report it.

Gates 1 to 3 run on the live server through the owner login (`td_mta`)
and the marker login (`postgres`). For gate 5 the controller takes a
fresh online base backup, checks it with td's `pg_controldata`, and
starts td's server on it with the production `postgresql.conf` and
`pg_hba.conf`, overriding only the trial's paths and socket, plus a peer
rule that lets root in as `setup_admin` on that copy alone. Through a
holder as in the socket cutover, root, the replication and marker logins
and every login of the live release must work; the trial's executable
must be td's, with every executable mapping inside the closure and no
loader variable. As the copy's superuser it repeats gates 1 and 2 in
every database that takes connections, scans every stored value for lz4
(gate 2's scan; the error and the receipt's `postgres_lz4_values` name
each offending column), and requires that each database's collation version
and each used libc collation's version are what td glibc reports, and
that every installed extension is available to td's server. Through the
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
`UnsetEnvironment=LD_LIBRARY_PATH LD_PRELOAD LD_AUDIT`. After
`daemon-reload` systemd's command must match it. It starts PostgreSQL
and requires a closed socket, the production primary, td's executable
with the closure's mappings and no loader variable, td's release in
`server_version_num`, and the trigram probe. Then it opens the socket,
starts the services and timers, and verifies the live release, every
login and health as after an activation. The receipt records the store
entries (`installed` or `present`), `postgres_debian_minor`, and the
events `td-built PostgreSQL started` and `verified td-built
PostgreSQL`.

A failure before PostgreSQL stops leaves production as it was, apart
from the verified store paths. A failure after it stops and before
writer admission is reversed automatically: PostgreSQL stops, the
drop-in goes, the Debian server starts on the same data directory and
must be the running executable, the fence opens and the services are
verified; the receipt records `Debian PostgreSQL restored` and, as td's
`pg_controldata` read it before the Debian server started,
`postgres_td_control_state`. After admission the job recovers forward on
td's server. If the host reboots inside the window, the deployment
fence keeps the services stopped and PostgreSQL starts on whichever
binary the drop-in selects, with its socket closed; inspect the receipt
before reopening writers.

If the controller itself dies mid-switch (killed, power loss), recover
by hand from the receipt; once it records `fencing`, every later mode
refuses on the deployment fence or the stale socket fence file:

- If it lacks `fencing`, the Debian server still runs unfenced and only
  the store paths and the inert fence conditions changed: run
  `--postgres-binaries` again.
- Otherwise, if it lacks `td-built PostgreSQL started`, return to the
  Debian server: stop PostgreSQL; if `91-td-mta-binaries.conf` exists,
  remove it and run `systemctl daemon-reload`; remove
  `/etc/td-mta/postgres-fence.conf`; start PostgreSQL and check that
  `/proc/MAINPID/exe` is the Debian `postgres`.
- Otherwise td's server runs on the data directory: keep the drop-in,
  remove the fence file if present and run `sudo -u td-mta chmod 0660
  /var/lib/td-mta-postgres/.s.PGSQL.35432`.

Then, as for any interrupted job, remove
`/var/lib/td-mta-operator/deployment-fence`, start the services and
timers the receipt recorded and require health. The next controller run
of any mode selects its PostgreSQL tools from the drop-in: td's while
`91-td-mta-binaries.conf` exists, the Debian tree otherwise. Run
`--postgres-binaries` again to finish or verify the switch; on a host
the drop-in already switched it only verifies.

Verify:

```text
systemctl show td-mta-postgres.service -p ExecStart -p UnsetEnvironment
sudo readlink /proc/$(systemctl show -p MainPID --value td-mta-postgres.service)/exe
```

The executable is under `/td/store/`, and the effective `ExecStart` names
the same path.

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
inspect the receipt and the trial's evidence. A gate 6 refusal means the
named minor is not the live one: read the notes from the live minor and
run again.

`--postgres-binaries` moves only from the Debian binaries to a td
closure. Moving from one td closure to another, as after the
`postgresql-x86-64` pin changes, is not supported yet: the controller
refuses a host whose drop-in selects other td-built binaries. Roll back
to the Debian binaries by hand as below, then run the switch again with
the new closure.

To return to the Debian binaries later, fence the writers by hand as for
the socket rollback, remove `91-td-mta-binaries.conf`, run `systemctl
daemon-reload`, restart `td-mta-postgres.service`, check that its
executable is the Debian `postgres`, and reopen the writers. The data
directory stays; moving back to an older 17 minor is supported unless
the release notes between them say otherwise. The next controller run
uses the Debian tools again. The store paths stay.

Once switched, the bootstrap archive that each activation's recovery
copy keeps includes the drop-in and the two selected store paths, not
the rest of `/td/store`.

The scheduled backup's tools are an operations setting. After the
switch, regenerate the backup job with `--postgres-bin
/td/store/HASH-postgresql-x86-64-17.N/bin` and without
`--tool-library-dir`, whose library path would override td's run path
(DEPLOYMENT.md: retain the existing definitions, regenerate, then
replace and reload the installed unit). Until then the Debian `pg_dump`
still dumps the 17.N server. The restore drill runs on the recovery
host, not here; its supported layouts do not include `/td/store`, so a
drill that should use td's tools needs that layout admitted first.
