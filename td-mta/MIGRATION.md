# Provisional JMAP export

`td-mta migration export` creates a resumable archive from an authenticated
HTTPS JMAP account. `migration verify` checks that archive offline. These
commands do not import into PostgreSQL, modify the source, freeze delivery,
or approve a domain cutover. Final reconciliation and import remain pending.

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
