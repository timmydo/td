# Provisional JMAP migration

`td-mta migration export` creates a resumable archive from an authenticated
HTTPS JMAP account. `migration verify` checks that archive offline. These
commands do not modify the source, freeze delivery, or approve a domain
cutover. The insert-only PostgreSQL importer below consumes a retained archive;
final reconciliation remains pending.

Create an empty mode-0700 archive directory under a private, stable parent.
The password file must be mode 0400 or 0600 in a mode-0700 directory owned by
the invoking non-root operator. One final LF or CRLF is removed. Credentials
are never accepted as command arguments or printed in diagnostics.

```text
td-mta migration export --archive /home/operator/mail-archive \
  --max-bytes 10737418240 \
  --session https://mail.example.test/.well-known/jmap \
  --account SOURCE_ACCOUNT_ID --username user@example.test \
  --password-file /home/operator/private/source.password

td-mta migration verify --archive /home/operator/mail-archive \
  --max-bytes 10737418240
```

Use the source JMAP Session's account ID, not an email address unless that
server uses the address as its ID. An optional `--ca-file PATH` supplies PEM
roots for this command only. Otherwise it uses the normal public roots.
Session, API and expanded download URLs must all use the original HTTPS
origin. Redirects, plaintext HTTP and cross-origin credential forwarding are
refused. TLS hostname and chain verification remain enabled.

Repeat the identical export command after an interruption. The archive binds
its generated source-instance identifier to the session URL, account and
username. Reusing it for another account or source is refused. The operator
must start a new archive if the same endpoint/account is replaced with a
new server database; JMAP has no universally available server-instance ID.

The exporter re-enumerates metadata and reuses a completed body's retained
source email ID/blob ID/size mapping only after checking its byte count and
SHA-256. It preserves identical messages with distinct source IDs as separate
manifest objects, even when they share the same physical body file. Folder
hierarchy, full memberships, received-date strings and keywords are retained.
Raw files are streamed without MIME rewriting. No settings, identities,
passwords, rules, contacts or calendars are exported.

Enumeration uses Mailbox/query and per-mailbox Email/query, with source-ID
deduplication. It deliberately does not treat an empty unfiltered Email/query
as proof that the account is empty. This is per-mailbox coverage, not an
independent proof that the server has no orphaned or inaccessible messages.
The manifest and successful JSON report explicitly say `provisional` and
`cutoverReady: false`. Account Email and Mailbox states must match before,
during and after enumeration; changes cause failure and require a rerun.
There is no live changes-feed convergence or final-deletion authority yet.

# Archive and bounds

Version 1 uses `source.json`, `manifest.json`, SHA-256-named `.eml` body files
and SHA-256-of-source-email-ID `.json` resume records. Remote identifiers are
never used directly as filenames. `manifest.json` contains the complete
source binding, baseline states, selected JMAP metadata and body digests.
Verification checks source binding, unique object IDs, folder references and
cycles, memberships, sizes and raw-body digests. Digests detect accidental
corruption; an operator who can rewrite an archive can rewrite its digests.

A cooperative exclusive `LOCK` protects export and verification. Files are
private regular files, never symlinks, under the existing trusted stable-path
contract. The same owner must not concurrently replace directory components.
New files are synced before rename and the directory is synced afterward.
A completed body precedes its resume record. A new manifest replaces the old
one only after successful enumeration and unchanged final source states.
An abandoned `partial` file is removed under the lock on the next invocation.
A failed run can leave reusable bodies and the previous completed manifest;
verification always describes that retained snapshot, not the current server.

`--max-bytes` is required and bounds all retained archive file bytes plus a
new temporary file, up to 1 TiB. It includes old bodies and resume records;
there is no automatic deletion of historical bodies. Filesystem metadata and
allocation rounding are outside this logical-byte budget. Leave disk space
for those overheads. Replacing a manifest temporarily needs room for both
versions. There are at most 210,000 archive entries, 100,000 distinct source
messages and 4,096 folders. A message is at most 32 MiB; JSON responses are
at most 8 MiB, accumulated source metadata 16 MiB, and a serialized manifest
32 MiB. Oversize or unrepresentable objects fail the command, never disappear
silently from a successful snapshot.

Requests have a 60-second deadline, bounded DNS and connect attempts, a
24-hour invocation network deadline and a 200,000-request ceiling. HTTP
headers are limited to 16 KiB and 100 fields. Bodies accept Content-Length,
chunked framing without extensions/trailers (at most 65,536 chunks), or clean
TLS EOF. Compression, ambiguous framing and truncated bodies are refused.
Sink bytes are provisional until the entire response succeeds. ACME uses
the same transport with its existing shorter deadline and smaller body cap.

Compatibility checks against an independently run Stalwart instance are
manual operator evidence. Stalwart is not a repository build or test input.

# Insert-only PostgreSQL import

Create the destination account with the supported `postgres account-create`
workflow first. Select its configuration and retain the current store epoch.
Import uses the schema-owner connection file, never an application password:

```text
td-mta migration import --archive /home/operator/mail-archive \
  --max-bytes 10737418240 \
  --connection-file /home/operator/private/owner.connection \
  --epoch STORE_EPOCH_HEX --account DESTINATION_ACCOUNT_ID
```

The command retains the archive lock, validates the manifest and converts its
metadata before writing. It rehashes each bounded raw body immediately before
publication. Folder parents precede children. The existing Inbox is adopted
only if its name, parent, role, sort order and subscription exactly match.
Other role or sibling-name conflicts are refused. Source folders cannot share
one local mapping. Names must satisfy the normal NFC and control-character
rules. Local limits are 256 folders, 256 memberships and 256 keywords per
message; the archive can represent more than this importer can publish.
Unrepresentable objects fail explicitly. Partial progress remains resumable.
One import connection has a ten-second deadline, 40 MiB outbound budget,
1 MiB inbound budget and 2,048 protocol-frame budget. Body writes and the
full supported membership/keyword sets share those finite allowances.

Each object publishes in one durable account transaction. Email publication
includes its exact raw bytes, imported origin, received timestamp, complete
memberships, canonical lowercase keywords, locally selected thread, quota
charge, JMAP history and source receipt. No Received header is inserted and
no original header is removed. Thread evidence uses the same bounded header
parser as receiving. Submillisecond timestamps, leap seconds, non-UTCDate
spellings and keyword case-fold collisions are refused rather than changed
silently. Millisecond dates before the Unix epoch remain exact. Root headers
must fit the existing 1 MiB parser bound. Parsing never rewrites raw bytes.

Source receipts use the instance/account/object key and FORMAT.md's canonical
snapshot digest. Distinct source email IDs always create distinct local
emails, including identical bodies. Repeating an unchanged archive returns
historical success without resetting flags, folders, dates or quota counters.
A locally deleted mapped object is reported in `historicalObjectsAbsent` and
is never recreated. A missing mapped folder prevents new messages from using
it. Changed source snapshots fail with `migration-source-changed`; this
increment does not update or delete previously imported objects.

Owner authorization, the store epoch, current selected configuration,
maintenance state and account quotas fence new writes. Receipt inspection
waits behind an outstanding account writer. An uncertain COMMIT stops the
command with `migration-indeterminate-retry-same-archive`; retain the archive
and repeat that command to settle the same source receipts. Do not replace
its source instance to bypass an error. A successful report is explicitly
`imported-provisional` with `cutoverReady: false`. It is not final inventory
reconciliation or authority to change DNS. Exporting td-mta for rollback still
needs a Mailbox/get fallback for its unsupported Mailbox/query method.
