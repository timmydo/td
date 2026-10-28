# Production operations: mx.timmydouglas.com

This is the operator runbook for the Azure x86-64 host shared by
`mx.timmydouglas.com`, `mx.timmyd.info` and `mx.882015.xyz`. The inventory was
checked on 2026-10-27. Update this document when changing its paths or services.
[DEPLOYMENT.md](DEPLOYMENT.md) describes installation on other hosts;
[OBSERVABILITY.md](OBSERVABILITY.md) describes diagnostic output.

## Access and installed layout

From the development machine, use the dedicated SSH key:

```text
ssh -o IdentitiesOnly=yes -i ~/.ssh/td_mta_systemd_test timmy@172.179.0.6
```

The remote `timmy` account can administer the services with `sudo`. The daemon
runs as `td-mta`. SSH does not grant access to the former Stalwart host merely
because its old DNS name now points here.

| Item | Location or name |
| --- | --- |
| Source checkout | `/home/timmy/src/td` |
| Selected executable | `/opt/td-mta/bin/td-mta` (symlink) |
| Retained binaries and build notices | `/opt/td-mta/lib/td-mta/releases/` |
| Installed maintenance helper | `/opt/td-mta/bin/td-mta-operator` |
| Mail state root | `/var/lib/td-mta/mail` |
| Private PostgreSQL connection files | `/var/lib/td-mta/mail/runtime-connection`, `/var/lib/td-mta/mail/owner-connection` |
| Per-profile readiness and diagnostics | `/var/lib/td-mta/mail/PROFILE/runtime` |
| System service definitions | `/etc/systemd/system/td-mta-*.service` |
| Deployment receipts and diagnostics | `/var/lib/td-mta/operator/` |
| Scheduled database backups | `/var/lib/td-mta/backups/scheduled/` |
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

The inbound SMTP listener permits eight simultaneous sessions. DATA admission
remains bounded to four jobs overall and two per peer. These are separate
limits: increasing listener capacity does not remove the per-peer DATA bound.

`td-mta-postgres.service` separately runs PostgreSQL 17 on loopback port 35432.
Its cluster is `/var/lib/td-mta/mail/postgres`, its configuration is
`/var/lib/td-mta/mail/postgresql.conf`, and its retained binaries are under
`/opt/td-mta/postgresql/usr/lib/postgresql/17/bin`. An MTA binary update does
not require restarting or replacing PostgreSQL.

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
Local health results are not mailbox alerts, and a local backup is not an
off-host copy. Neither mail alerts nor off-host backup delivery is implied by
these timers. The Stalwart migration jobs are retired as described below.

## Retired Stalwart source and recovery copies

The former AWS source at `54.244.239.235` is retained for recovery, with
`stalwart.service` disabled and stopped. Its old public mail listeners are
closed; `td-mta-stalwart-defer.service` remains enabled on port 25 and sends
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
identity is separate from Azure's: use the old IP with its retained, verified
host key, not the MX DNS names that now resolve to Azure. The VM has not been
terminated. Restarting Stalwart or restoring old DNS is not a rollback of
mail accepted by td-mta; follow [MIGRATION.md](MIGRATION.md)'s reconciliation
requirements before reopening a source writer.

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

## Choose and build an update

Use a reviewed revision whose required checks passed. Normal updates select a
landed commit from `main`; testing an unlanded rolling revision requires its
own exact source and gate receipt. Do not deploy a moving branch name.

The host checkout's `origin` currently names `/srv/git/td.git`, the development
machine's local handoff path. Do not assume that remote is usable on Azure.
Fetch the public backup explicitly (its `main` must contain the chosen commit):

```text
cd /home/timmy/src/td
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

Run the build as `timmy`, with the host Rust/C toolchain on PATH. No custom
compiler store paths or secret build environment are needed. Retain the host
libraries used by this native build. Do not use `sudo` for compilation. The
candidate release includes the build input receipt and dependency notices.

During incident work, exact source archives and small deployment drivers were
retained under `/var/lib/td-mta/operator/*/deployment-*`. Those are historical
receipts, not scripts to rerun: they pin old commits, configuration versions
and staging locations. This runbook does not depend on them to build or update.

## Preflight and stage the binary

This procedure is for a **binary-only** update with a compatible database and
configuration. If the change requires a schema upgrade, stop here and follow
the migration contract in [POSTGRES.md](POSTGRES.md): fence all database users,
take a verified backup and apply the documented migration. Do not run
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

Updates briefly interrupt listeners. PostgreSQL and stored mail remain in
place. Pause the aggregate health timer for this window, stop all five MTA
processes, then switch the link. Do not continue if any service fails to stop.
Use a unique temporary link and an atomic rename within the same directory.

The activation block runs in a subshell: any failed command ends activation
and resumes the health timer. It preserves the previous executable path in a
private receipt before stopping services. A failure after stopping requires
operator rollback; it is not an automatic rollback implementation.

```bash
mail_units=(td-mta-frontend td-mta-receive td-mta-relay td-mta-migadu td-mta-acme-production)
previous=$(readlink -f /opt/td-mta/bin/td-mta)
record="/var/lib/td-mta/operator/manual-$revision"
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
  wait_ready sudo -u td-mta /opt/td-mta/bin/td-mta acme-status \
    --connection-file /var/lib/td-mta/mail/runtime-connection \
    --certificate-profile production --require-ready --minimum-validity-seconds 2592000
  sudo systemctl start td-mta-health.service
)
```

Each readiness check has a two-minute retry window; the command's own bounded
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

Use the **recorded** previous executable, not whichever release happens to be
second in a directory listing. Preserve the failed candidate and its logs.
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
other database maintenance jobs, retain and revoke runtime schema/table/column
grants, upgrade with the candidate binary and owner connection, then restore
runtime grants including SELECT/INSERT/UPDATE/DELETE on `blob_summaries`.
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
MIME decoding but still verify the complete original body. Large searches can
still exceed request work or connection bounds; this is not a full-text index.
The first implementation has no automatic search-row preparation or scheduled
rebuild, so new messages use parser fallback until an explicit backfill.

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
These are systemd/journal results, not automatic mailbox alerts.

```text
systemctl status td-mta-maint-scrub.timer td-mta-maint-scrub-status.timer
sudo journalctl -u td-mta-maint-scrub.service -u td-mta-maint-scrub-status.service --since today
sudo systemctl start td-mta-maint-scrub-status.service
```

Inspect the JSON progress and retained failure identifiers before taking any
repair action. A successful selective message read does not replace this
whole-body check; a clean scrub does not replace a backup or restore test.
