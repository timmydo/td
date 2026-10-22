# Provisional JMAP migration

[CUTOVER.md](CUTOVER.md) connects these commands to the operational cutover
and rollback checklist, its current evidence and remaining acceptance gaps.

`td-mta migration export` creates a resumable archive from an authenticated
HTTPS JMAP account. `migration verify` checks that archive offline. These
commands do not modify the source, freeze delivery, or approve a domain
cutover. The insert-only PostgreSQL importer below consumes a retained archive;
owner-selected reconciliation is separate from importing.

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
deduplication. A correctly correlated Mailbox/query `unknownMethod` response
permits bounded Mailbox/get with `ids: null` instead. This supports td-mta
rollback exports. Other errors never trigger that fallback. The get-all result
must match the original Mailbox state, contain unique IDs within the folder
ceiling, and have no missing objects. It deliberately does not treat an empty unfiltered Email/query
as proof that the account is empty. This is per-mailbox coverage, not an
independent proof that the server has no orphaned or inaccessible messages.
The manifest and successful JSON report explicitly say `provisional` and
`cutoverReady: false`. Account Email and Mailbox states must match before,
during and after enumeration; changes cause failure and require a rerun.
There is no live changes-feed convergence. Export itself grants no deletion
authority; the explicit reconciliation commands below have separate checks.

# Account-wide inventory check

After exporting a quiet source, compare its retained message IDs with a
separate account-wide query:

```text
td-mta migration inventory --archive /home/operator/mail-archive \
  --max-bytes 10737418240 \
  --session https://mail.example.test/.well-known/jmap \
  --account SOURCE_ACCOUNT_ID --username user@example.test \
  --password-file /home/operator/private/source.password
```

The URL, account and username must match the retained source binding before
any network request. The command checks both retained Email and Mailbox
states before and after a paged Email/query with `minSize: 0`. RFC 8621's
nonnegative minimum includes every valid Email size, including zero. This
avoids relying on an empty unfiltered query; an unsupported filter fails the
check without falling back to a narrower enumeration. Every page must have
the same query state and total, the expected position, and unique IDs within
the existing limits. The final ID set must exactly equal the manifest's.
Omitted, additional or duplicated IDs and changed source states refuse.

The JSON report binds the result to the manifest digest, source and retained
states. Coverage is the authenticated account view, not proof of access to
hidden accounts or inaccessible messages. Confirm source access separately
before a cutover. It does not rewrite the manifest or import or delete mail.
`bodyDigestsVerified: false` means this command does not rehash archive bodies;
use `migration verify` for that. `cutoverReady: false` remains explicit.
A failed check grants no deletion authority. Refresh the export after source
changes, and resolve an ID-set mismatch before using it as a final inventory.

# Comparing independently exported accounts

Export and verify both quiet endpoints, then compare their private archives:

```text
td-mta migration compare --archive /home/operator/source-archive \
  --against-archive /home/operator/target-archive --max-bytes 10737418240
```

The two directories must be distinct; the command holds both cooperative
archive locks and applies `--max-bytes` independently to each. It contacts no
server and writes no mail. Both source bindings and manifests must validate.
A successful result also rehashes every distinct referenced body in both archives.
Missing, corrupt, public or linked body files refuse success.

Comparison ignores server-assigned object IDs. It matches folders by their
exact name and complete parent hierarchy, then compares roles, sort orders
and subscription flags, including empty folders. Separate path components
are encoded unambiguously; a slash inside a name is not a hierarchy separator.
Duplicate sibling paths are ambiguous and refused. Bounded SHA-256 parent
fingerprints avoid materializing every full path for deep hierarchies.

Messages compare by raw-body SHA-256, byte length, received time, full folder
membership and keywords. Counts are retained for identical messages, so a
missing duplicate cannot be hidden by set deduplication. Received times use
the importer's exact millisecond representation; submillisecond information
is refused. Keywords use the importer's ASCII case normalization, with
case-fold collisions refused. Message order and JSON property order do not
matter. Derived thread IDs, blob IDs and MIME part IDs are not portable
source metadata and are not compared.

`archives-match` binds both manifest digests and source bindings in its report,
with message/folder counts and `bodyDigestsVerified: true`. A mismatch exits
unsuccessfully with a fixed diagnostic; it never repairs either side or grants
deletion authority. An archive comparison is historical: run the separate
`migration inventory` check against each endpoint while it remains quiet.
It does not cover accounts outside those authenticated views, provider routes,
credentials, rules or server settings. `cutoverReady: false` remains explicit.

# Archive and bounds

Version 1 uses `source.json`, `manifest.json`, SHA-256-named `.eml` body files
and SHA-256-of-source-email-ID `.json` resume records. Remote identifiers are
never used directly as filenames. `manifest.json` contains the complete
source binding, baseline states, selected JMAP metadata and body digests.
Verification checks source binding, unique object IDs, folder references and
cycles, memberships, sizes and raw-body digests. During one locked verification,
each physical body file is hashed once; every repeated reference must declare
the same size, and message counts and logical bytes still include duplicates.
No successful-body cache survives the command. Digests detect accidental
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
24-hour invocation network deadline and a 200,000-request ceiling. Successive
requests wait at least 200 ms after the preceding response, within the same
invocation deadline, to stay below td-mta's per-peer authentication ceiling.
Other traffic can still exhaust that shared allowance: a non-200 response
stops the export, whose retained bodies can be resumed. HTTP headers are
limited to 16 KiB and 100 fields. Bodies accept Content-Length,
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
reconciliation or authority to change DNS. Exporting td-mta uses the bounded Mailbox/get fallback above; a verified
archive still requires a separate reverse import and reconciliation before
redirecting delivery to the old service.

# Retained plans for existing message metadata

`migration plan` compares a selected archive with already imported messages.
`migration apply` changes only their keywords and folder memberships, using
the exact retained plan. Raw bytes, received dates, blob ownership, thread
assignment and storage accounting remain unchanged. These commands create
and delete nothing, and do not rename or reparent folders.

Store the plan outside the archive, in an owner-only directory, with mode
0600. For example, after exporting, verifying and initially importing:

```text
umask 077
td-mta migration plan --archive /private/archive --max-bytes 1073741824 \
  --connection-file /private/owner.connection --epoch EPOCH --account ACCOUNT \
  > /private/plan.json
td-mta migration apply --archive /private/archive --max-bytes 1073741824 \
  --connection-file /private/owner.connection --epoch EPOCH --account ACCOUNT \
  --plan-file /private/plan.json
```

Inspect the plan before applying it. Each archive message has one status:

- `update`: supported source metadata changes with an unchanged local copy.
- `matches-at-observation`: source, receipt and local metadata matched when
  inspected; this is not a promise that clients cannot change them afterward.
- `historical-object-absent`: its mapped local message was deleted.
- `local-conflict`: local metadata or provenance no longer matches its receipt,
  including unmapped or ambiguous folder memberships.
- `immutable-source-change`: source bytes, length or received date changed.
- `folder-conflict`: a referenced folder or ancestor is missing, changed or
  ambiguously mapped, or its source metadata changed.
- `unmapped`: the message has not been imported.

Only `update` entries are applied. Other entries remain explicit skipped
observations, not successful reconciliation. An applied result distinguishes
`updated`, `current-receipt-matches`, and `historical-object-absent`.
The current-receipt result acknowledges the latest stored source digest; it
does not prove execution of this particular historical plan, nor reset later
local edits. Missing local messages are never recreated.

A plan binds the exact desired manifest, source instance/account, destination
epoch/account, and each update's local ID, historical blob, old source digest
and receipt sequence. It also retains local IDs for referenced folders and
their ancestors. Apply takes the account lock, checks these fences and current
policy, reconstructs the complete local canonical snapshot, and checks folder
metadata before publishing memberships, keywords, history and the advanced
source receipt together. Returning to an old source digest after intervening
updates does not make an old plan valid: its receipt sequence has changed.
Unrelated incoming mail does not invalidate a plan merely by advancing the
account's sequence.

Keep the same plan and archive through partial failure or
`migration-indeterminate-retry-same-plan`. Earlier successful object updates
remain committed when a later object fails. Repeating the retained plan
settles those receipts without refreshing old expectations. Generate a fresh
plan after apply to inspect current metadata. Each observation is independently
locked; the plan is not an account-wide snapshot or proof of source chronology.
The operator selects the desired archive; opaque JMAP states and export times
do not establish that it is newer than a previously selected archive.

Plan and apply inspect structural archive metadata and stored immutable-body
digests; they do not re-audit archive or database body extents. Use `migration
verify` and the database audit commands for those checks. Plans admit at most
100,000 entries, 16 MiB of accumulated serialized entries and a 32 MiB input
file. Each database operation has the existing 10-second deadline, with a
2 MiB outbound, 4 MiB inbound and 2,048-frame protocol allowance. Folder,
membership and keyword bounds remain 256; incomplete reconstruction refuses.

Ordinary `migration import` retains its historical insert-only semantics.
For supported present-message changes, import can establish new folders before
stopping at the first changed message; plan/apply can then update existing
messages, followed by import for new messages. An absent, locally changed or
otherwise unsupported mapped message can still stop ordinary import before
new messages are reached. Use the explicit new-object mode below to skip
mapped objects without claiming synchronization. Folder changes use the separate folder plan below. Explicit
source-deletion evidence remains separate work. Omission from an archive never authorizes deletion.
Every report continues to carry `cutoverReady: false`.

# Importing only unmapped objects

Add `--only-unmapped` to `migration import` when a newer archive contains
new mail alongside changed or locally deleted mapped objects:

```text
td-mta migration import --only-unmapped \
  --archive /private/archive --max-bytes 1073741824 \
  --connection-file /private/owner.connection --epoch EPOCH --account ACCOUNT
```

An existing exact destination-account/source-instance/source-account/kind/ID
receipt causes a skip, regardless of its digest or whether the local object
still exists. Skipping does not update the receipt, reset local metadata,
resurrect a deleted object, charge storage, or publish JMAP changes. A skip
means previously mapped, not currently synchronized. Reports distinguish
`skippedMappedMailboxes`, `skippedMappedMessages`, newly created objects and
`adoptedMailboxes` (a newly mapped preexisting Inbox).
`historicalObjectsAbsent` counts skipped mappings whose local objects were
absent when observed. Use retained plan/apply
to inspect or reconcile existing message metadata separately.

Archive structure and source binding remain validated. Skipped objects need
neither body reads nor conversion into td-mta's narrower local metadata
representation. Every body actually published is rehashed. New objects and
their required folder definitions must satisfy all local representation and
capacity limits; an unrelated skipped folder cannot become an unchecked
creation dependency.

A preliminary receipt lookup avoids unnecessary work. Creation rechecks
receipt existence under the account write lock, so a competing import that
establishes a receipt causes a skip, never duplication or overwrite. Under
that same lock, new messages validate every referenced folder and ancestor;
new child folders validate their parent and ancestors. Desired source folder
metadata must match both the receipt and reconstructed current local folder.
Missing, renamed, reparented or ambiguously mapped dependencies refuse the
new object. Unrelated changed folders may be skipped. Ordinary import keeps
its existing historical replay semantics.

Each guarded Email publication keeps the 10-second operation deadline and
40 MiB outbound allowance, with a 4 MiB inbound and 2,048-frame allowance for
full dependency metadata. Guarded mailbox publication has 2 MiB outbound and
the same inbound/frame limits. At most 256 required folder definitions and
ancestors are admitted. Publication otherwise shares ordinary import's
owner, epoch, selection, maintenance, quota and uncertain-commit handling.
Earlier creations remain committed if a later object fails; keep the same
archive after uncertainty. Reports retain `cutoverReady: false`. Skips and
source omissions never authorize deletion or a domain switch.

# Reconciling existing folders

Use `migration plan --mailboxes` with the same archive, connection, epoch and
account arguments to retain a folder-only plan. `migration apply --plan-file`
recognizes its versioned format. Existing Email plans retain their original
meaning. Keep each plan in a private file and keep its exact archive through
retry, as with Email plans.

Folder plans support names, parent changes, sort order and subscription state
of existing mapped folders. The folder ID stays unchanged, preserving Email
memberships and descendant parent references. Local-only descendants move
with their parent too. Raw bytes, received dates, Email/Blob/Thread identities,
keywords and body accounting are untouched. Role-bearing folders may be
renamed or moved, but changing their role is refused. There is no folder
creation, merging, deletion, temporary rename or automatic name-swap sequence.

Each update binds its local ID, old receipt digest and sequence, desired
digest and the destination ancestor IDs. Under the account write lock,
apply reconstructs current folder metadata and refuses local edits, stale
receipts, ambiguous source mappings or a folder shared with another source
namespace. It checks the desired destination chain and the complete candidate
local graph (at most 256 folders), rejecting missing parents, cycles,
duplicate sibling names and duplicate roles. Incoming mail may advance a
folder's ordinary change sequence without invalidating its metadata plan.

A successful update emits one Mailbox update and advances the source receipt
and account sequence together. Receipt replay acknowledges the current source
digest without resetting later local edits; a missing folder stays absent.
The existing uncertainty/retry rules apply. Folder operations use the same
10-second deadline and bounded reconciliation transport as Email operations.

Plan observations distinguish `role-source-change`, `folder-conflict`,
`local-conflict`, `historical-object-absent`, `unmapped`, and matches from
actual `update` entries. A destination ancestor needing its own update can
remain a conflict until that update is applied and a fresh plan is generated.
Earlier folder updates stay committed if a later one fails. Neither a mixed
result nor a replay establishes account-wide synchronization. Replan after
completion, resolve every exception, and verify the desired source inventory
separately. Reports retain `cutoverReady: false`; source omissions still do
not authorize deletion.

# Explicit source message deletions

Preserve a complete private copy of the preceding archive before refreshing
it. The previous and current archives must retain the identical `source.json`
binding; an unrelated fresh export is not a deletion baseline. Retain both
archives and the exact plan throughout reconciliation and recovery.

With the source quiescent, its account permissions checked, and its current
export complete, generate a separate deletion plan:

```text
td-mta migration deletion-plan --archive /home/operator/current-archive \
  --previous-archive /home/operator/previous-archive --max-bytes 10737418240 \
  --session https://mail.example.test/.well-known/jmap --username user@example.test \
  --password-file /home/operator/private/source.password \
  --connection-file /home/operator/private/owner.connection \
  --epoch TARGET_EPOCH --account TARGET_ACCOUNT
```

Here `--account` identifies the PostgreSQL target; the source account comes
from the matching archive binding. The explicit Session and username must
match that binding. Optional `--ca-file` retains command-local trust. Capture
successful JSON into a mode-0600 plan in a private parent, inspect every
entry, and use the identical arguments with `deletion-apply` and
`--plan-file /home/operator/private/deletions.json` to apply it.

Both commands verify both archives, compare the current archive with a fresh
account-wide query, and require explicit `Email/get` `notFound` responses for
every candidate at the retained state. Apply checks each candidate's absence and the retained Email state again
before its deletion. Both account states fence the complete pass; a final check
checks completion. Source omission alone never grants deletion authority.
`notFound` can also mean lost visibility: neither the protocol nor this tool
can prove that the source permissions remained sufficient. The operator must
verify access and deliberately select source deletions while the source is
quiet; these commands do not freeze it or synchronize two live writers.

A `delete` entry binds both manifest digests, source identity, target epoch
and account, local object, historical blob, old source digest and receipt
sequence. Under the target account lock, apply verifies the retained receipt,
exclusive source mapping and current message metadata. Changed local mail,
changed receipts and other sources refuse; unmapped and already absent
objects are observations. Target-only new mail is never a candidate.

Deletion reuses the ordinary Email destruction transaction for membership,
keywords, thread ownership, shared-body retention, quota release and JMAP
history. It keeps the import receipt permanently: repeating the plan observes
absence, and importing the old source object cannot resurrect it. A lost
commit response requires retrying the exact retained plan. Earlier deletions
remain committed if a later source fence or local conflict fails; preserve
the reports and resolve the failure rather than regenerating fences blindly.
Message plans leave folders untouched. Separate folder plans are described
below. Every report remains provisional.


# Explicit source folder deletions

Use the same retained previous/current archives and source-access conditions
as message deletion, adding `--mailboxes` to `deletion-plan`. The resulting
format-4 plan contains `mailboxes` rather than `messages`. `deletion-apply`
selects the folder operation from that retained plan; it does not take a new
`--mailboxes` switch or reinterpret an Email deletion plan.

The command checks account-wide Email inventory and independently enumerates
current Mailbox IDs at the retained states. Every candidate must also receive
an exact `Mailbox/get` `notFound` response. Apply refreshes the candidate's
absence and Mailbox state immediately before the operation, with both account
states fencing the complete pass. Permission loss is still indistinguishable
from absence and requires the same explicit operator review.

Only an unchanged, exclusively mapped, roleless empty leaf can be deleted.
Plans report `has-child`, `has-email`, `role-protected` and `local-conflict`
without selecting those objects. A new target membership or child after
planning causes apply to refuse. No message is removed to empty a folder.

Apply selected leaf entries, settle uncertainty by retrying that exact plan,
then generate a fresh plan for parents that have become empty leaves. Parent
observations in the first plan remain skipped even after a child deletion;
replay never silently upgrades an observation into a destructive operation.
Complete message moves/deletions before trying to remove their old folders.

The owner boundary reuses ordinary Mailbox destruction under the account
lock, preserving its role, child and membership checks. Deletion emits one
Mailbox tombstone and advances the account sequence while keeping the source
receipt. A repeated plan acknowledges absence, and import does not recreate
that historical source folder. Mail, blobs, quotas and other folders remain
untouched. Compare final inventories and archives after all observations and
conflicts are resolved; a mixed result still grants no cutover approval.
