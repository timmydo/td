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

Schema changes need an explicit compatible upgrade procedure and a retained
backup. Stop all users of the database before upgrading; binary rollback alone
does not undo a schema upgrade. PostgreSQL package updates are a separate
operation. Public-server deployment and ACME renewal are independent of local
replica updates.

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
