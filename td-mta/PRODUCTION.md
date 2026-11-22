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
private notes outside this repository. `./td-mta/deploy` compiles in its
own copies of them (`td-mta/src/bin/td-mta-deploy/`), so the notes must
agree with that source. The `ADMIN` account can administer
the services with `sudo`. The daemon runs as `td-mta`. SSH does not grant
access to the former Stalwart host merely because its old DNS name now
points here.

| Item | Location or name |
| --- | --- |
| Source checkout | `/home/ADMIN/src/td` |
| Selected executable | `/opt/td-mta/bin/td-mta` (symlink) |
| Retained binaries and build notices | `/opt/td-mta/lib/td-mta/releases/` |
| Installed maintenance helper | `/opt/td-mta/bin/td-mta-operator` |
| Mail state root | `/var/lib/td-mta/mail` |
| Private PostgreSQL connection files | `/var/lib/td-mta/mail/runtime-connection`, `/var/lib/td-mta/mail/owner-connection` |
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

On claw1 the selected frontend profile sets `event_streams = 4` in
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

`td-mta-postgres.service` separately runs PostgreSQL 17 on loopback port 35432.
Its cluster is `/var/lib/td-mta/mail/postgres`, its configuration is
`/var/lib/td-mta/mail/postgresql.conf`, and its retained binaries are under
`/opt/td-mta/postgresql/usr/lib/postgresql/17/bin`. An MTA binary update does
not require restarting or replacing PostgreSQL.

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
and HTTPS 443. The private PostgreSQL TLS material has a separate generated
`td-mta-maint-postgres-tls-renew.timer`. Certificate management and software
updates are independent.

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
file and TLS material; retain those separately and give the trial its
own configuration. Budget space for two cluster copies and the WAL spool
before each window.

For physical backup access, provision a dedicated `LOGIN REPLICATION
NOSUPERUSER` PostgreSQL role with a private SCRAM credential. Restrict
its HBA entry to `hostssl replication`, that role and the loopback client
address. Reload HBA and verify a replication connection using
`sslmode=verify-full`, the configured CA, the server certificate name as
`host`, and `hostaddr=127.0.0.1`. The mode-0600 passfile entry names that
host, the PostgreSQL port, `replication` and the backup role. Keep the credential with
the backup operator. The streaming procedure uses no permanent replication
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
and sends that exact Git object to claw1 using the production SSH identity.
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
td-mail account xyz882015 (override with `--jmap-account NAME`). The
second check records both observations in public-jmap.json beside the receipt.

A fresh unprivileged checkout builds the candidate with install-td-mta.
The operator stages the release, validates the epoch, selected configuration
and runtime login, and checks scheduled backup and aggregate health.
Older candidates without schema-plan must pass strict schema-inspect.
A recognized pending migration or a change to required runtime grants
also needs the private PostgreSQL profiles and comparison SQL described
in the online recovery procedure. The deployment takes a fresh verified
base, rehearses the candidate on an independent writable copy inside a
private network namespace, and proves archive replay on another copy.

The default forward activation budget is 60 seconds. On expiry the job
restores service and records the observed outage. Recovery and clean
PostgreSQL shutdown can extend the interruption beyond the budget. Use
`./td-mta/deploy --window-seconds N` to set it for an invocation.
The job records the active timers and conditions the mail and scheduled
writer services on a durable deployment fence. During schema cutover a
temporary nftables output rule reserves the loopback database connection
for the root operator. Before pausing mail the job validates the complete
firewall batch with `nft --check`. It applies the same batch atomically
after stopping the writers. It verifies the named restore point and selected
mail evidence before attempting DDL. A preseeded, primary-device copy
is synchronized by content after clean PostgreSQL shutdown. Budget
space for this additional data directory and time to read both copies.
The actual upgrade and runtime checks use the candidate data directory
on private loopback. The original shut-down primary remains in the private
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
   the final cold copy. The installed HBA uses verified loopback TLS and
   SCRAM. The local setup-admin peer rule exists only on the private copies.
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

On claw1 the root-only libpq service file is
`/var/lib/td-mta-operator/physical-backup/service.conf`. Its
`cutover_repl` profile uses `td_mta_cutover_backup` and `repl.pass`.
The ordinary `cutover_marker` profile uses `td_mta_cutover_marker`,
`marker.pass` and the `postgres` database. Both use verified loopback TLS.
The same operator directory supplies `compare-selected.sql`, a bounded
read-only transaction producing deterministic rows for schema, epoch,
selected configuration, sampled emails, recipients and body chunks.
The backup volume directory is `/data/td-mta-physical-recovery`, root:root
0700 on the separate data disk. The owner login uses the scheduled backup
libpq profile in `/var/lib/td-mta/backup-control/service.conf`.
The host uses its installed PostgreSQL 17 tools, systemd,
nftables, rsync with --fsync, tar, util-linux and the native build
toolchain. Database passwords stay in protected files on claw1.
