# Operating a local read-only replica

[REPLICATION.md](REPLICATION.md) defines the synchronization and recovery
boundaries. This runbook covers an existing current-user installation made by
`--setup-replica`; do not run fresh setup over its data directory. The public
server has its own [production runbook](PRODUCTION.md).

## Paths and status

The examples use the default replica paths. Run them as the account that owns
the installation:

```sh
replica_root="$HOME/.local/state/td-mta-replica"
replica_bin="$HOME/.local/share/td-mta-replica/bin/td-mta"
replica_epoch=$(cat "$replica_root/store-epoch")
replica_account=$(cat "$replica_root/account-id")

"$replica_bin" status --json --require-ready \
  --runtime "$replica_root/frontend/runtime"
"$replica_bin" postgres replica-follow-status \
  --state-dir "$replica_root/copy-state" \
  --epoch "$replica_epoch" --account "$replica_account"
"$replica_bin" postgres replica-progress \
  --connection-file "$replica_root/owner-connection" \
  --epoch "$replica_epoch" --account "$replica_account"
```

These answer different questions:

| Observation | What it establishes |
| --- | --- |
| Reader `ready` | The local service can accept requests. |
| `recent_success:true` | The copy worker recorded a sufficiently recent successful round; this is not a process-liveness check. |
| `phase:idle` | Its last recorded round observed no further Email changes. |
| `inventory_complete:true` | The initial ID enumeration finished. |
| Zero `pending_ids` and `unfinished_ids` | No pending work remains in that observed copy page. |
| `copy_complete:false`, `recovery_verified:false` | These are deliberately conservative, currently unconditional fields; they do not diagnose missing messages. |

No combination of these observations proves source parity or authorizes
promotion. A recent body audit proves integrity of the bytes it examined;
an isolated restore proves that particular archive can be restored. Keep
those receipts separate from synchronization status. Synchronization propagates
source deletions, so a replica is not a versioned backup.

## Size and verify the replica volume

Schema 44 charges the receipt quota for live Emails and Blobs. PostgreSQL
also retains a length and digest for every distinct copied Blob, including
those whose bytes have been collected. Size the replica for body data, live
metadata, indexes, these lifetime identity records, WAL, temporary space
and schema upgrades. The source body total alone cannot predict the space
required. On an existing install, measure both the mounted filesystem
(`df -B1`) and the private cluster (`du -sx -B1 "$replica_root/postgres"`).
Allow for the expected net growth until the next capacity review and free
space for a maintenance copy or index rebuild. Keep backups on another disk.

Give the replica its own **fixed-size filesystem**. Record the chosen maximum
in bytes, the device UUID and the free-space threshold that leaves time to
stop copying or expand. One Linux configuration is an ext4 filesystem on a
dedicated partition or thick logical volume. For example, an administrator
can allocate a new 64 GiB thick LV with `lvcreate -L 64G -n tdreplica VG`
and initialize that **new, empty** device with `mkfs.ext4 /dev/VG/tdreplica`.
64 GiB is an example budget of 68719476736 bytes. Choose a size from the
actual mailbox and available storage. Configure the mount by UUID in the
host's boot configuration. Give the replica user ownership of a private
directory on it. Keep the *underlying*, unmounted mountpoint empty, owned
by root and mode 000 so services cannot fall back to writing the host disk.

For a fresh replica, choose a short path such as `/srv/tdreplica/mail` for
`--root`. Mount the volume and prepare its parent first. Setup creates the
final, previously absent directory. For an installed replica, preserve its
absolute paths, which the services and clients retain. While every service
is stopped and after the cold backup below, copy the complete root with
ownership and modes intact into a private directory on the new volume.
Extracting the cold archive into that empty directory and comparing it with
`tar -df` before switching preserves the full root and its permissions.
Keep the original offline. Bind-mount that directory at the old absolute
path over an empty, inaccessible underlying mountpoint. Persist both mounts
in the host configuration before re-enabling the user services.

Verify the budget on the database host, before the first copy, after a move
or resize and before restarting an upgraded copier. For an ext4 volume use
the following checks with the recorded UUID and byte limit. For a fresh
setup, check the mount containing the parent before setup and repeat these
checks on the completed installation before enabling either supervisor.

```sh
# Supply replica_root, replica_uuid and replica_budget_bytes from this install.
(
  set -eu
  device=$(readlink -e "/dev/disk/by-uuid/$replica_uuid")
  devno=$(lsblk -dnro MAJ:MIN "$device")
  test -n "$devno"
  test "$(lsblk -bdnro SIZE "$device")" -le "$replica_budget_bytes"
  for path in "$replica_root" "$replica_root/postgres" "$replica_root/postgres/pg_wal"; do
    test "$(findmnt -nro MAJ:MIN --target "$path")" = "$devno"
    test "$(findmnt -nro FSTYPE --target "$path")" = ext4
  done
  # The generated cluster has no relocated tablespaces or WAL.
  test "$(postgres -D "$replica_root/postgres" -c "config_file=$replica_root/postgresql.conf" -C data_directory)" = "$replica_root/postgres"
  test -d "$replica_root/postgres/pg_wal"
  test ! -L "$replica_root/postgres/pg_wal"
  entries=$(find "$replica_root/postgres/pg_tblspc" -mindepth 1 -print -quit)
  test -z "$entries"
  findmnt --submounts --target "$replica_root/postgres" -o TARGET,SOURCE,MAJ:MIN
  df -B1 --output=size,used,avail,target "$replica_root"
)
```

Inspect the mount list for nested mounts under the cluster, including its
temporary and index directories. Require all of PGDATA to reside on that
one device, and keep PostgreSQL archive commands, temporary tablespaces
and external logging destinations disabled in this profile. Inspect the
selected PostgreSQL service and its configuration if customized. With the
generated definitions, the cluster is `ROOT/postgres` and uses the built-in
tablespaces. Shepherd writes service logs under `ROOT`. With systemd,
stdout and stderr go to the host journal, which has its own disk budget.
The block device
enforces the capacity even when the copy process restarts. Mail quotas,
`df` free bytes on a shared home filesystem and copy-worker status do not
establish a physical cap. A client on another host cannot establish it.

Monitor free blocks and inodes (`df -B1`, `df -i`) independently of copy
status. At the recorded low-water mark stop synchronization and scheduled
search work. On exhaustion, keep the copier stopped and enlarge the volume
before allowing PostgreSQL recovery or a retry. For the example LV an
administrator can run `lvextend -L +16G /dev/VG/tdreplica` followed by
`resize2fs /dev/VG/tdreplica`. Record and verify the new budget. A fixed
partition can be replaced with a larger dedicated volume using the stopped
copy procedure. Preserve WAL, Blob identities and pending-page records.
Start PostgreSQL, check its log and `replica-progress`, then verify reader
readiness and a fresh successful copy round. An interrupted transaction can
be retried against the durable page.

## Services and local access

There are three services: `td-mta-replica-postgres`, `td-mta-replica` (the JMAP
reader), and `td-mta-replica-sync`. Use one supervisor. For example:

```sh
herd status td-mta-replica-postgres
herd status td-mta-replica
herd status td-mta-replica-sync
```

The equivalent systemd commands use `systemctl --user status NAME`.
Stopping only `td-mta-replica-sync` leaves copied mail available for reading.
Restart it after a manual copy operation. Do not run a second copy worker
against the same state directory. Source failures back off without stopping
the independent local reader.

Current installations use mode-0600 sockets beneath private directories:
`ROOT/jmap/socket` and `ROOT/socket/.s.PGSQL.PORT`. JMAP uses the logical
URL `http://localhost/.well-known/jmap` with td-mail's explicit `unix_socket`
setting. There is no DNS/TCP fallback. The local device password and PostgreSQL
SCRAM passwords remain required. The source connection still verifies HTTPS.
No local certificate or renewal job is needed.

`ROOT/mail.toml` contains the generated client account. Keep its account name
distinct from cloud accounts because td-mail caches are keyed by account name.
The normal installed td-mail launcher grants the configured private socket;
restart td-mail after changing that path. See
[td-mail's configuration](../td-mail/README.md) for the exact account fields.

Guix Home users must keep the three service definitions in their own Home
source. Installer output and a runtime `herd load` do not amend that source.
Remove a migrated replica's old renewal service from it so a later reconfigure
does not restore that job. Keep the currently working package profile and
runtime GC roots; a binary update is not a reason to replace the Home profile.
User services start according to the user manager's login/boot policy; this
is not automatically an unattended system service.

## Search preparation and Home configuration

Installations that enabled automatic search preparation also have
`td-mta-maint-search` and `td-mta-maint-search-status`. These are optional
scheduled jobs, separate from the three replica services. Keep both generated
Shepherd definitions in your own Home source alongside the replica services;
a helper that reconfigures the installed Home does not edit that source.
Before your next reconfigure, build your Home configuration and check that
all five service definitions remain present and that the old replica renewal
job is absent. Retain the existing packages and runtime GC roots.

Check the durable search progress independently of synchronization:

```sh
"$replica_bin" postgres search-prepare-status \
  --state-dir "$replica_root/search-index" --epoch "$replica_epoch"
herd status td-mta-maint-search
herd status td-mta-maint-search-status
```

For systemd, inspect both `.timer` units and the last result of their
`.service` units with `systemctl --user`. A scheduled job may be inactive
between successful runs. Its durable status and supervisor log distinguish
that normal state from failure. The generated Shepherd logs are under
`ROOT/.operations/`; protect them along with the other private state.

The worker visits missing or incompatible search evidence in bounded pages;
it does not reread every already-prepared body each minute. A completed cycle
can include unsupported messages. Those remain stored and cause explicitly
incomplete search results when td-mail encounters them, not silent empty
results. New synchronized messages can use the bounded fallback before their
search evidence is prepared. A healthy search cycle does not establish that
synchronization is current or that the replica is source-complete.

See [automatic search preparation](DEPLOYMENT.md#automatic-search-preparation)
for schedule generation, limits and status. Preserve the progress file after
an interruption. The worker resumes on its next scheduled run. Status checks
record failures; mailbox alert delivery requires separate operator configuration.

## Update and rollback

A Git merge does not update installed binaries. For a reviewed revision with
compatible schema, build and install from its checkout:

```sh
./install-td-mta --prefix "$HOME/.local/share/td-mta-replica"
./install-apps
herd stop td-mta-replica-sync
herd restart td-mta-replica
herd start td-mta-replica-sync
```

For systemd, replace the three `herd` commands with the corresponding
`systemctl --user stop`, `restart`, and `start` commands. Restart td-mail, then
run the readiness and synchronization checks above. Verify that a successful
upstream contact occurred after the restart, and open a folder and a message.
A copy worker may still be finishing its bounded operation when asked to stop.

The installer retains releases and selects `PREFIX/bin/td-mta` with a symlink;
running processes keep their old executable until restarted. Record the
previous symlink target before updating. If a binary-only update fails, stop
the reader and copier, select that retained previous executable, start both,
and repeat the checks. Preserve runtime libraries and protected configuration.
Do not reinitialize PostgreSQL or replace the replica's epoch to fix startup.

If search schedules are enabled, pause both schedules and wait for any active
search job before switching binaries. Restart their schedules after verifying
the reader and copier. On systemd this means stopping the timers and their
services; on Shepherd stop the two timed services. Preserve search progress.

Schema changes need an explicit compatible upgrade procedure and a retained
backup. Stop all users of the database, including search preparation and other
maintenance jobs, before upgrading; binary rollback alone
does not undo a schema upgrade. PostgreSQL package updates are a separate
operation. Public-server deployment and ACME renewal are independent of local
replica updates.

### Fenced desktop upgrade from schema 43 to 44

This procedure is for a private Unix-socket replica with the three generated
services. Keep the public deployment tooling on the primary. Schedule a
desktop read outage. Run the mail commands as the installation user.

1. Record the current `replica-progress` output, local epoch, source origin
   and epoch, account ID, generation, cursor, pending-page counts and current
   executable (`readlink -e "$replica_bin"`). Stop synchronization, the reader, optional search timers **and their active
   jobs**, and any manual copy command. With writers now fenced, recapture
   the progress values so the comparison uses the last committed page. Prevent supervisor respawn and interactive access for the
   duration of the cutover. With Shepherd use `herd stop td-mta-replica-sync`,
   `herd stop td-mta-replica`, then `herd stop td-mta-replica-postgres`.
   The corresponding systemd commands are `systemctl --user stop NAME`.
   Confirm the three services have stopped. Require PostgreSQL 17
   `pg_controldata "$replica_root/postgres"` to report a clean shut down.
2. Save the entire stopped root to a new mode-0700 recovery directory on a
   **different volume**. Include PGDATA with WAL, local passwords and
   connection files, configuration, copy-state and supervisor definitions.
   First confirm the old PGDATA has a local pg_wal directory, an empty
   pg_tblspc and no relocated cluster files. An installation that relocated
   any of these needs a cold backup covering those locations as well and a
   verified relocation into the capped volume before this procedure.
   With the single-volume layout above, `tar -cpf "$backup/root.tar" -C
   "$replica_root" .` and `tar -df "$backup/root.tar" -C "$replica_root"`
   create and compare the archive. Run under `umask 077` and require success.
   Protect the backup, record the previous executable beside it and retain
   its release, runtime libraries and PostgreSQL 17 installation. If moving
   from an unbounded filesystem, complete the volume move above now. Verify
   the budget, identity of the volume, unused space and absence of relocated
   WAL or tablespaces.
3. Install the reviewed candidate with `./install-td-mta --prefix PREFIX`,
   using the existing prefix. Leave supervisor services stopped. In the
   private recovery directory create `owner.hba` containing exactly
   `local td_mta td_mta_owner scram-sha-256` on one line and
   `local all all reject` on the next. As the installation user, start
   PostgreSQL in a separate foreground terminal. Its private owner-only
   socket remains at the address in the existing connection file.

   ```sh
   postgres -D "$replica_root/postgres" -c "config_file=$replica_root/postgresql.conf" \
     -c "hba_file=$backup/owner.hba" -c "listen_addresses=" \
     -c "unix_socket_directories=$replica_root/socket"
   ```

4. Read the plan with the candidate. Require current 43, target 44, exactly
   one pending migration (44), and the recorded local store epoch. Allow a
   bounded maintenance window for the identity backfill. For example:

   ```sh
   owner_file="$replica_root/owner-connection"
   "$replica_bin" postgres schema-plan --connection-file "$owner_file"
   ```

   Read the plan output and verify all four values above. On a match,
   run the upgrade in the same fenced maintenance window.

   ```sh
   "$replica_bin" postgres schema-upgrade --connection-file "$owner_file" --timeout-seconds 900
   "$replica_bin" postgres schema-inspect --connection-file "$owner_file"
   "$replica_bin" postgres replica-progress --connection-file "$owner_file" \
     --epoch "$replica_epoch" --account "$replica_account"
   ```

   Require version 44 and the same epoch, source binding, account, generation,
   cursor, pending ID count and unfinished ID count as before. The live
   receipt count can decrease.
   Schema 44 requires no additional grants. Keep the replica reader and
   publisher privilege sets. After a lost upgrade response, keep writers
   fenced, wait for the migration lock to release and inspect the ledger.
   A confirmed 43 may be retried after diagnosis. A confirmed 44 goes through
   the same verification before serving.
5. Stop the private foreground server with `pg_ctl -D "$replica_root/postgres"
   -m fast -w stop`. Confirm clean shutdown. Start the normal PostgreSQL
   service with its original HBA, then start the reader. Check
   `status --json --require-ready` and read one saved message. Start the
   copier, require a successful round recorded *after* startup, then resume
   the search schedules. Keep the rollback copy until verification finishes.

If the cutover fails, stop the copier, reader, scheduled jobs and **both**
possible PostgreSQL launches. Preserve the failed cluster separately.
Restore the verified schema-43 cold archive with `tar -xpf "$backup/root.tar"
-C "$replica_root"` into an **empty, mode-0700 root** at the original absolute
path on the capped volume. Select the saved old executable by atomically
replacing the installation's `bin/td-mta` symlink. Start PostgreSQL, verify
schema 43, the saved local epoch, source binding and checkpoints with the old
binary, then restart and verify the reader and copier. Recheck freshness.
Copy progress made after the archive must be fetched again. If the required source history has expired,
keep this copy for reading and provision a separate fresh replica from the
source before switching clients. Never start the old binary on ledger 44.

## Integrity and recovery evidence

The native owner command can audit one bounded page without changing mail:

```sh
"$replica_bin" postgres blob-scrub-page \
  --connection-file "$replica_root/owner-connection" \
  --epoch "$replica_epoch" --limit 256
```

Retain its `through` key and pass both `--after` and `--through` from the last
successful page until `done:true`. Check every exit status and `failed` count;
a deadline or operational error is not a completed page. The native audit
verifies chunk layout, manifests, length and whole-body SHA-256. Each page has
its own snapshot: copying or deletion during the sweep prevents treating it as
one point-in-time inventory. An integrity audit neither repairs data nor
certifies source completeness.

For exact restore evidence, follow the
[isolated logical restore drill](DEPLOYMENT.md#local-logical-backup-and-an-isolated-restore-drill).
For the Unix setup, libpq uses the private socket directory as `host`, the
socket filename's port, and `sslmode=disable`; retain SCRAM authentication
through a protected password file. Do not apply that setting to a TCP host.
A consistent exported PostgreSQL snapshot can bind both `pg_dump --snapshot`
and the source row inventory while live services continue. Keep its exporting
transaction open until both finish, with a finite timeout. Compare the restored
mail tables against that same snapshot and audit the restored bodies. Protect
the archive and temporary credentials, remove temporary source passwords after
use, and confirm the disposable cluster stopped before publishing final success.

A restore drill must use a fresh private socket and data directory with no TCP
listener and no mail service. Budget archive, restored database, WAL, indexes
and inventory scratch before starting; avoid putting an unbounded drill on the
live service filesystem. Never activate copied supervisor definitions or run
`serve` against the drill. Actual recovery still requires separate fencing,
fresh authority, credentials/configuration and outbound-delivery handling.
The account replica omits those operational resources. There is no supported
replica promotion command today.

## Performance diagnosis

Capture `performance-status --runtime ROOT/frontend/runtime` immediately before
and after the operation and correlate it with td-mail's log. The server phases
are nested, so do not add their durations. See [OBSERVABILITY.md](OBSERVABILITY.md).
Client logs identify HTTP status, attempts, retry wait and method time without
logging query text or message content. A one-second retry can dominate a fast
successful request; distinguish it from parsing or database time.

Measure with a separate empty client cache, recording connection setup, folder
query, summary batches, message opening, and search separately. State whether
server caches are already warm and how many readers are active. Report samples,
median and tail observations; performance targets are not load-sensitive gate
assertions. A broad text search can still hit the bounded search work limit:
search uses indexed candidates for eligible terms when schema-40 search
backfill is complete, uses qualified stored fields when available, and verifies
bodies on fallback. Unindexed rows remain candidates; short or ineligible
terms retain the bounded scan. Do not describe that refusal as an empty result or
silently raise the work limit to conceal it.
