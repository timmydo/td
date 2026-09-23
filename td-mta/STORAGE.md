# td-mta storage

This is the normative storage companion to [DESIGN.md](DESIGN.md). Read both
before changing persistence, queries, submission records, migration or backup.
It specifies an unimplemented target. [FORMAT.md](FORMAT.md) owns the numeric
registry and byte layout. M02 completes row codecs and golden fixtures before
any production data is written; scalar/key codecs alone do not open a store.
[API.md](API.md) owns adapter/state contracts; [QUEUE.md](QUEUE.md) owns the
submission transition and restart contract.

## 1. Storage model

Keep ordinary immutable `.eml` files and compact binary metadata inspectable
through td-mta commands. Live metadata is mutated only through the service's
transaction API. Bodies and metadata have separate publication steps, with
transactions and crash recovery binding them into one committed account view.

V1 uses a single sorted checkpoint plus a bounded recent-change journal,
with write admission paused during checkpoint publication. Metadata is streamed
from disk through bounded arenas; the service does not load the whole mailbox
into RAM. This favors an auditable implementation over sustained write
throughput. The following sections define authority, encoding, commit ordering,
read views, retention and recovery for this storage engine.

## 2. Files and authority

Example paths use shortened IDs for readability. Actual object IDs are random
128-bit values rendered as exactly 32 lowercase hex digits. Collision checks
never replace an existing object. IDs are not digests; identical deliveries
get distinct email/blob IDs. V1 does not deduplicate message contents.

```text
/etc/td-mta/
  config                         operator configuration, including aliases
  secrets/                       protected smart-host credential files
/var/lib/td-mta/
  FORMAT                         store version, instance ID, state epoch
  LOCK                           process-scoped exclusive writer lock
  accounts/ACCOUNT/
    messages/ab/ab91....eml       complete raw MIME message
    uploads/cd/cd22....blob       arbitrary uploaded attachment/message bytes
    metadata/
      CURRENT                    selected checkpoint and manifest digest
      checkpoints/000042/
        manifest                 table sizes/hashes, journal selection
        blobs.tbl
        mailboxes.tbl
        emails.tbl
        memberships.tbl
        keywords.tbl
        threads.tbl
        thread-anchors.tbl
        submissions.tbl
        recipients.tbl
        leases.tbl
        imports.tbl
      journal/000043.log         active file named by selected manifest
      journal/000041.log         retained change history, when selected
    cache/                       entirely rebuildable and version-tagged
      000042/email-offsets.idx
      000042/mailbox-order.idx
      000042/search.idx
    tmp/                         unpublished bodies/checkpoints/sort scratch
  devices/                       private device verifiers, separate schema
  acme/                          private keys, orders and certificate generations
```

Mail and upload shards use the first byte of the ID (two hex characters),
distributing files over 256 directories without putting mailbox names in paths.
An `.eml` file contains headers, body and encoded MIME attachments, with no
td-specific prefix or footer. SMTP decoding and locally added trace fields
follow DESIGN section 9. Flagging, folder moves and folder renames do not
change that file. Extracted attachment caches are optional and disposable.

These files have different authority:

| Data | Source of truth | Can be rebuilt without losing information? |
| --- | --- | --- |
| Message bytes | `.eml` and referenced upload files | No |
| Folder names/hierarchy, membership, keywords | Checkpoint plus committed journal | No |
| Stable object/thread IDs, receipt/envelope data | Checkpoint plus committed journal | No |
| Submission/recipient outcomes and import mappings | Checkpoint plus committed journal | No |
| Retained JMAP change history | Selected journal segments | No; expiry requires explicit client resync |
| Parsed headers, part offsets, folder ordering, search | `cache/` | Yes |

Cache files never contain the only copy of a read flag, folder membership,
thread assignment or submission result. Copying just `.eml` files salvages
content but does not restore the account. Configuration, devices and ACME state
have their own lifetimes; an account checkpoint does not claim to snapshot them.

Use trusted private roots on local ext4/Btrfs, tested for rename/file/directory
sync behavior. NFS and external live writers are unsupported. Filenames,
permissions and safe path construction obey DESIGN section 6. New-format files
are refused rather than interpreted as an older schema.

## 3. Metadata records

Each `.tbl` is an immutable flat file sorted by unsigned bytewise key order,
with unique keys and bounded records. Tables are not all loaded into memory.
Keys contain raw 16-byte IDs and bounded UTF-8 bytes, not displayed hex.

| Table | Key | Authoritative value |
| --- | --- | --- |
| `blobs` | blob ID | Kind (message/upload), length, SHA-256, creation time |
| `mailboxes` | mailbox ID | Name, parent ID, role, sort order, subscription state |
| `emails` | email ID | Message blob ID, thread ID, receivedAt, SMTP receipt/envelope metadata |
| `memberships` | email ID + mailbox ID | Empty; presence means membership |
| `keywords` | email ID + keyword bytes | Empty; presence means set |
| `threads` | thread ID | Persisted immutable grouping identity |
| `thread-anchors` | Length-prefixed Message-ID + email ID | Empty; authoritative lookup from message header ID to live email |
| `submissions` | submission ID | Email/thread/identity IDs, immutable transmitted blob ID, envelope sender, sendAt, lifecycle/notification state |
| `recipients` | submission ID + recipient ordinal | Address, attempt/phase, result, retry time, uncertainty, bounded diagnostic |
| `leases` | upload blob ID | Owning account/device, expiry and permitted use |
| `imports` | source-instance ID + source object kind + length-prefixed source-account and object bytes | Local IDs and verified source digest/mapping |

Every record carries its last changed transaction sequence. Submission expiry
is shared by its recipients; the exact positional fields
and enum tags are in FORMAT.md section 6. Fields needed for
submission remain in its record even if the visible email is later deleted.
Recipient ordinal is a big-endian u32 in the key so its byte order is numeric;
ordinary integer values use the encoding in section 4. Indexable timestamps and
addresses do not become primary keys solely for query speed.

The source account/object part of an import key, including its two u32 length
prefixes, is at most 1007 bytes; instance and kind take 17 bytes, giving a
total key ceiling of 1024 bytes. The kind distinguishes a mailbox and email
with the same source ID; source IDs are not globally unique across JMAP types.
Import must report an unrepresentable source ID; hashing without a
collision-resolving source record is not a substitute.
Keywords and addresses obey their more specific protocol/config limits.

Live owning references must resolve within the same account: email to
message blob/thread, membership to email/mailbox, keyword to email, recipient
to submission, submission to transmitted blob, and valid lease to upload blob.
Mailbox parents must resolve without cycles. Submission email/thread/identity
IDs are historical identifiers: creation validates them and authorization,
but later deletion or configuration changes need not leave their targets live.
Frozen transmission bytes and envelope fields remain authoritative for sending.
Import mappings likewise retain historical local IDs after deletion; inspection
and resumed import report a deleted target instead of dereferencing it or
silently recreating it. Historical IDs never pin their former targets or grant
authorization. Lease device IDs retain provenance; revocation prevents use.
Thread anchors must reference live emails and are removed with their email.

Thread assignment does not depend on disposable indexes or rescanning every
body. Store at most one anchor per email: its first syntactically valid
Message-ID within a 1004-byte ceiling (four-byte length plus ID plus 16-byte
email ID fits the key limit). Header ID matching is byte-exact after parsing
away delimiters/outer whitespace. Absent or oversize IDs produce no anchor;
raw headers remain unchanged. Examine at most the last 32 References IDs,
nearest first, then bounded In-Reply-To IDs. Use the first ID with a live
anchor; duplicate anchors select the lexicographically smallest email ID.
Join that email's persisted thread. If no candidate resolves, create a fresh
thread. Existing email/thread assignments are immutable, even when a later
arrival connects two conversations. M02 pins malformed-header behavior and
fixtures. Resource/I/O failure is an explicit temporary error, not permission
to silently choose a different thread. Authoritative anchor lookup remains
available without a cache, using bounded-work sorted-table access.

A committing writer validates these rules, blob kinds, keyword limits and
submission/blob pins. Derived counters are calculated or cached, never
independently authoritative. A transaction that updates folder membership also
declares the affected JMAP objects/state types for change APIs.

For example, `store inspect email e123 --json` might decode:

```json
{
  "emailId": "e123",
  "blobId": "b91",
  "threadId": "t55",
  "receivedAt": "2026-09-22T18:30:00Z",
  "mailboxIds": ["m1", "m7"],
  "keywords": ["$seen"],
  "viewSequence": 812
}
```

Inspection object-ID fields use the type-prefixed wire form from WIRE.md;
the shortened IDs in these worked examples are schematic, not valid inputs.
Physical primary keys remain the binary encodings in FORMAT.md.
That JSON is an assembled inspection view, not a JSON file on disk. Its fields
come from the email, membership and keyword tables overlaid with recent
committed updates. `m7`'s name comes from its mailbox row. Subject and attachment
names come from the message or its disposable parsing cache.

### 3.1 MIME part blob identities

File blob IDs and JMAP part blob IDs are distinct typed forms. A part ID is
a versioned encoding of its parent file blob ID, encoded-body offset/length
and transfer-encoding tag; [WIRE.md](WIRE.md) freezes its canonical bounded
wire encoding within JMAP's ID length limit. Nested attached messages use its
bounded chain of decoded-stream ranges. A part never names an independently
stored file or an entry in `blobs`. Resolve it only in an authorized account and live parent
view, or against an authorized unexpired upload lease for a parsed raw message;
validate checked ranges and require an exact match to a parsed MIME part
descriptor, rebuilt boundedly if its cache is absent. A forged locator cannot
select arbitrary filesystem bytes or bypass parent authorization.

Download streams transfer-decoded part contents from the immutable parent;
unknown transfer encodings follow the JMAP identity-decoding rule. A read view
pins the parent for the entire stream. Email/set attachment reuse resolves and
pins the parent while assembling the new immutable message, then commits its
own body; the new email must not depend on the original surviving. Part IDs
do not keep parents alive after all ordinary references expire. No decoded
attachment file or cache is authoritative. Parser/schema upgrades preserve
existing locator semantics or require an explicit format migration.

## 4. Binary encoding and limits

The v1 container rules are:

- Fixed-width unsigned integers are little-endian except numeric key components
  explicitly marked big-endian. Times are signed i64 UTC milliseconds. IDs are
  16 opaque bytes. No native `usize`, pointers, padding or Rust enum layouts.
- Byte strings have a u32 length and exactly that many bytes. Text fields must
  validate UTF-8. Optional values have a one-byte 0/1 presence tag; collections
  have a u32 count and field-specific limits. No recursive generic value tree.
- A table header carries magic, container/schema version, table ID, account ID,
  generation, through-sequence, record count and payload length. Each record
  contains key length, value length, last-change sequence, key, value and a
  SHA-256 checksum over that record's preceding fields/bytes.
- Table files have a SHA-256 digest and exact byte count recorded in the
  generation manifest. The manifest binds all table names, schema versions,
  account/epoch, checkpoint sequence C, selected active journal and retained
  history descriptors. CURRENT binds generation and manifest digest.
- Keys are at most 1024 bytes; values at most 64 KiB. Oversize operations are
  rejected before writing. Large raw headers and bodies remain in message files;
  they cannot be copied wholesale into an email row to evade these bounds.

The journal starts with an account/epoch/segment header and base sequence.
It is append-only and not preallocated or padded on disk: physical EOF is its
written extent. Memory arenas, rather than journal file extents, are reserved.
Each subsequent frame is exactly one transaction:

```text
header: magic, version, total frame length, sequence, operation count,
        header checksum
payload: bounded PUT/DELETE operations and JMAP change descriptors
footer: end magic and checksum of header + payload + end magic
```

PUT supplies a complete replacement value for one `(table, key)`; DELETE removes
that key. Payload rows use the same key/value codecs as checkpoint records.
Operations have a defined order; the last operation on a key wins within a
transaction. Cross-row invariants are checked on the transaction's final view.
There are no counter-increment or executable commands in journal records.
The complete, valid footer establishes a recoverable transaction boundary.

Header checksum validation precedes trusting its length. Frames are at most
1 MiB including framing and have at most 4096 operations; larger JMAP batches
use the protocol's per-object results and transactions rather than splitting
one indivisible storage transaction. A single object's operation that cannot
fit is refused. Reserve frame/overlay capacity before streaming a newly admitted
message so a final DATA commit cannot be stranded by an avoidable metadata cap.
Reservation accounting is global to the writer coordinator: committed frame
bytes/operations plus all outstanding reservations must fit the active-journal
ceilings. Refuse or wait before admitting work whose reservation cannot fit.
Reservations have bounded slot IDs and deadlines, not preassigned sequences
or journal file handles. Cancellation releases them; commit consumes them.

M02 must record the numeric table/field tags, exact header/footer byte offsets,
enum values, limits for every variable field, and full encode/decode golden
fixtures in this document or a normative referenced format table. All codecs
are shared by service, inspection, verification and migration. Until those
tables exist, no implementation may call its on-disk bytes format v1. Unknown
mandatory tags, inconsistent lengths, invalid enums or trailing bytes fail
closed. Checksums detect damage; they are not authenticity against a writer
who controls the store. SHA-256 comes through the reviewed crypto adapter.

## 5. Commit, acknowledgement and recovery

One writer serializes all account mutations. There is no transaction spanning
multiple accounts in v1. One transaction can update several rows/tables; a
folder move and submission creation are examples. Steps for a new message:

1. Reserve quota, frame capacity and bounded commit work; exclusively create
   its temporary body and stream bytes while computing size/digest.
2. Sync the completed body, publish its fresh ID path without replacement,
   and sync every affected directory (including new shard directories).
3. Build and validate a complete transaction in the preallocated frame buffer.
   Append it to the selected active journal and sync that file.
4. Advance the shared committed sequence AND byte offset together, expose the
   full transaction to readers, and acknowledge success to SMTP/JMAP.

The body precedes its reference. A metadata-only mutation starts at step 3.
External SMTP relay attempts have their own durable phase records as specified
in DESIGN section 11; this local commit does not make remote SMTP exactly-once.

| Failure point | Recovery and client meaning |
| --- | --- |
| Before complete body publication | Temporary/incomplete body, no accepted email |
| Body published, no complete journal frame | Orphan body, eligible for later proven-safe reclamation |
| Partial final frame | Ignore only the incomplete physical tail; no success was permitted |
| Complete valid frame, no observed success response | Replay it; the client may have an uncertain result |
| Journal synced, acknowledgement sent | Replay or checkpoint must contain the whole transaction |
| Writer sync returns error | Persistence is uncertain: stop new mutations and recover; do not append past it |

Recovery validates CURRENT, its manifest, table identities/digests and selected
journal. Replay only contiguous complete frames after checkpoint sequence C.
An incomplete final header/body/footer at physical EOF may be truncated to the
last complete frame under the exclusive lock, then synced before serving.
A complete frame with an invalid checksum, an interior truncation, a sequence
gap or an inconsistent manifest is corruption, not a tail to skip. Do not scan
for a later magic string and resume as if nothing was lost. Truncation or damage
to previously durable storage cannot always be distinguished from an incomplete
write; sync guarantees presume a functioning storage stack, and backups/verify
cover damage outside that model.

Incomplete means fewer physical bytes than the validated frame length (or a
physically short header). A full-length final frame with an invalid footer is
not incomplete, even if a torn write could have caused it. Preserve it and
refuse mutations for diagnosis; do not silently discard a possibly previously
acknowledged transaction. Fault tests cover both truncated and full-length
torn tails. This fail-closed rule prioritizes preserving evidence over automatic
availability when the two cases cannot be distinguished.

Recovery validates live owning references and historical identifier encodings
under section 3 before permitting mutation. Missing committed message bytes
require diagnosis; synthesizing empty messages is forbidden.
No filesystem scan overrides CURRENT with a newer-looking generation: that
directory might have been prepared but never selected. An explicit repair
command can report alternatives without silently selecting one.

## 6. Bounded read views and checkpointing

A read view is `(generation G, checkpoint sequence C, journal segment J,
committed byte offset E, committed sequence S)`. Capture it under the writer's
short publication lock. The view pins G/J and reads only the prefix through E.
An operation that reads /changes also pins its selected history files under
that lock, so concurrent retention cannot remove a file it is about to read.
An in-progress append beyond E is invisible. This permits a query to read several
tables consistently while later transactions commit. A new paginated JMAP
request obtains a new view unless a protocol state precondition pins its meaning;
we do not promise unchanged results across unrelated requests.

The active journal has at most 4 MiB of committed frame bytes and 8192 stored
operations. Limit both, not just distinct keys. Checkpoint before admitting a
frame that would cross either bound. Default storage-read concurrency is two;
each slot owns a 4 MiB journal arena and a separately budgeted fixed descriptor
array. Parse the prefix into borrowed operation views, sort descriptors by
table/key/sequence/operation ordinal, and use the latest operation for each key.
The ordinal is its position within the frame, preserving last-operation-wins
even when the same key is changed twice in one transaction. No per-record
heap allocation or whole-mailbox map is allowed. The writer/checkpointer has
its own bounded scratch reservation; these bytes are additional to the 8 MiB
combined index cache and must appear in the memory ledger.

Read an object from that bounded overlay or its sorted checkpoint table.
Listings merge a sequential table cursor with sorted overlay entries. Sparse
key/offset indexes and folder/date/search indexes are disposable disk files
with bounded caches; their sizes are not RAM reservations. An absent index
permits a bounded-work sequential scan or explicit temporary resource error.
It must never produce an empty successful result merely because rebuilding
has not finished. Older index candidates require overlay reconciliation;
changed rows and deletions cannot disappear from query results. Content search
may need body reads and retains its independent time/work limits.

Checkpointing pauses new mutation admission and installs a writer commit
barrier after the current commit finishes, freezing sequence S. No transaction,
including a completion from previously admitted work, may append while this
barrier is held. Streaming can continue within its existing reservation; its
completion waits boundedly in the fixed commit queue. Outstanding reservations
belong to the coordinator and transfer unchanged to the next active journal;
new admission stays closed until their byte/operation totals are accounted for.
Sequence numbers are assigned only when actually committing. Then:

1. Merge each old sorted table with the bounded sorted journal updates into
   fresh tables under `tmp/`. Stream unchanged records; omit deleted rows.
2. Validate, sync all tables, and write/sync the new generation manifest.
   Prepare and sync a fresh empty journal whose base sequence is S and whose
   first future transaction is S+1. Sync new paths/directories before selection.
3. Publish the new checkpoint directory and atomically replace CURRENT with
   its generation/manifest digest; sync CURRENT's directory.
4. Publish the new generation/empty journal to new read views and resume writes.
   Existing readers keep their old prefix and generation until they finish.

All new generation/segment names are created exclusively. An unselected file
left by a failed attempt is never overwritten or mistaken for an existing
commit; choose a fresh name or reclaim it after proving it unreachable.

Before CURRENT switches, restart chooses the old checkpoint/journal. After
the switch is durably synced, it chooses the new pair. A failed publication
sync stops the writer for recovery; it must not guess which journal to append
to. Checkpoints rewrite metadata only, never unchanged message bodies.

Readers and one admitted backup pin old generations. At most two retired
generations may remain pinned in the default profile; a further checkpoint
waits within its deadline or defers new mutations with temporary errors. Read
views have deadlines and release descriptors/buffers on cancellation. Do not
grow memory or delete pinned state to make checkpoint progress. Measure write
pause duration on the many-small-message corpus; if this architecture cannot
meet the workload, amend it with evidence before adding background compaction.

Checkpoint failure before CURRENT selection leaves the old pair valid. Scratch
is reclaimable after proving it unselected. If the active journal has no room,
new writes temporarily refuse until checkpointing can succeed; acknowledged
mail remains readable. Metadata size affects checkpoint I/O time, not the
maximum resident overlay size.

## 7. Worked mutations

Assume checkpoint 42 includes sequence 810. It contains email e123 referencing
blob ab91, membership `(e123,m1)`, and mailbox m7 named Projects.

```text
811 PUT keywords[(e123,$seen)] = empty
812 DELETE memberships[(e123,m1)]
    PUT memberships[(e123,m7)] = empty
813 PUT submissions[s88] = { email=e123, transmitted_blob=b772, ... }
    PUT recipients[(s88,0)] = { address=person@example.org, queued, ... }
```

Each numbered group is one frame; elided values represent the binary row schema.
Sequence 812 moves the email without moving or editing ab91.eml. Sequence 813
is permitted only after b772 is durable and registered in `blobs` (in this frame
or an earlier committed frame). It can contain the normal JMAP success-filing
changes in a subsequent transaction with their own method result.

Deleting e123 removes its email/membership/keyword rows. It does not delete
submission s88 or b772. A submission's per-recipient final post-DATA acceptance
updates its rows; RCPT success alone never releases its obligation. Explicit
retention/cancel/failure handling controls the submission's lifetime.

The next checkpoint incorporates 811-813 into its tables. `store inspect`
returns the same view before and after checkpointing, apart from provenance
fields naming where those values were read. Renaming Projects changes only
the mailbox row. Changing an incoming alias changes operator configuration,
not already stored message membership or receipt metadata.

## 8. History, retention and garbage collection

Journal frames include the changed/destroyed JMAP object IDs and types needed
for /changes. Retain old segments separately from current-state replay, selected
by the manifest. Default history targets seven days, subject to ceilings of
128 MiB and 64 retained segments. Publish the retained sequence floor; a state
older than it gets the protocol resynchronization error. A state token contains
account, store epoch, object type and account sequence (API.md). Restoration
changes the epoch. History
expiry is not permission to delete current rows or pending queue data.
Read retained segments one at a time within the history/work budget. Return
bounded change pages with hasMoreChanges and a state at a complete transaction
boundary; never assemble all retained history in RAM. API.md specifies
coalescing and cannotCalculateChanges behavior when no legal page fits.

Historical PUT records may name bodies that are no longer live. /changes needs
identity/change evidence, not historical body versions; retained history alone
does not pin those bodies. Conversely, a pinned read view or backup does pin
the bodies required to reconstruct its view. Checkpoint pruning and body
reclamation must use those different liveness rules.

V1 body reclamation uses two maintenance phases. First stop admitting new
reads, mutations, uploads and backups, and pause new outbound queue dispatch.
Already admitted work may finish, including its final durable journal commits;
maintenance does not hold the writer lock while draining it. Wait boundedly
for all read views, streams, outbound attempts and the backup to drain. If the
deadline expires, defer reclamation and restore normal admission/dispatch.
Only after draining enter the exclusive phase, with maintenance as the sole
writer and no new work admitted. Flush and checkpoint the current view.
Stream authoritative live references from email rows,
submission rows and valid upload leases into bounded external-sort scratch;
merge against blob inventory. Never rely solely on an in-memory reference count.
Referenced MIME attachments are already inside immutable message bytes; assembly
must finish before upload leases can be released.

Both drain and exclusive phases have finite configured deadlines and I/O work
budgets, frozen in M02. On reaching an exclusive-phase limit, finish the current
bounded durable batch, retain safe progress and resume admission/dispatch.
Storage faults instead enter the normal recovery/refusal path. Readiness and
logs expose maintenance; refusals are retryable. This intentionally accepts a
temporary service pause. Concurrent reclamation is outside v1: elapsed age
alone cannot prove that no read view or in-flight writer needs a body.

Use private `tmp/sort/RUN/` directories with exclusive run names and a 64 MiB
default account scratch quota. External sorting uses preallocated run buffers
and bounded merge fan-in/file descriptors from the M02 ledger. Charge every
spill byte before writing; exhaustion defers reclamation without touching live
blobs. Failed or abandoned sort runs are disposable and removed under the
exclusive lock at startup, before listeners open. Never treat checkpoint or
body publication files as sort scratch.

For unreferenced blobs, first remove inventory rows and expired lease rows in
a durable transaction, then unlink their files and sync directories. Use bounded
batches if one frame cannot hold the sweep. A crash may leave an orphan file,
which the next sweep can remove; it cannot leave a live reference to deleted
bytes. Orphans from interrupted publication are identified by inventory absence
only while the exclusive window proves no writer/upload can be using them.
Do not unlink files based only on age or a filename absent from one old index.

Inventory scanning alone cannot find files never registered by a commit.
In the exclusive phase, enumerate `messages/` and `uploads/` shards with a
bounded directory cursor, validating each filename/type and comparing IDs
against the current committed inventory. External sort batches use the same
scratch quota, fixed buffers and I/O budget. Only confirmed absent IDs can be
removed as orphans. Report unexpected entries; never follow symlinks. The
writer remains stopped through each comparison/unlink batch. On interruption
or budget exhaustion a later exclusive sweep may restart traversal; progress
must not depend on a directory-entry offset surviving directory changes.

Disk reservations include temporary bodies, journal/history, active/retired
checkpoints, backup pins and merge scratch. Free-space checks do not eliminate
ENOSPC races with other host processes; every write/sync still handles errors.
Admission counts inodes as well as bytes where the platform can report them;
inode exhaustion must also give a defined temporary failure. Maintenance cannot
consume the space reserved for completing an already admitted commit.

## 9. Inspection, verification and backup

Required bounded, paginated commands:

```text
td-mta store layout --json
td-mta store inspect email ID --json
td-mta store inspect mailbox ID --json
td-mta store inspect submission ID --json
td-mta store journal --after SEQUENCE --limit COUNT --json
td-mta store export email ID --output PATH
td-mta store verify --json
```

Inspection reports format version, captured view sequence, source
checkpoint/journal, blob length/digest/path and decoded authoritative fields.
Normal status remains redacted; explicit local mail inspection requires the
same protected administrator authority as reading the private mail files.
JSON encodes untrusted text and never exposes device verifiers, ACME private
keys or smart-host credentials. Journal inspection is read-only; commands do
not make raw binary editing a supported mutation mechanism.

Verify streams hashes, record order, bounds, references and selected journal
continuity. Cache corruption can trigger a rebuild; metadata corruption cannot
be relabelled cache damage. Full body hashing may be expensive and is separate
from the startup check that verifies existence/length and metadata integrity.

An online account backup occupies one of the two storage-read slots and
captures one read view. It pins its generation/journal prefix, selected history
files and all live blobs against reclamation, and copies only through the
captured offset. It includes selected change-history segments plus a backup
manifest recording the explicit replay stop, hashes and epoch. The backup must
not copy a later CURRENT or an unbounded active log while copying old tables.
At most one online backup is active by default, with bounded time/disk quotas;
failure aborts the incomplete archive rather than weakening its pins.

Restore while stopped into a new private root: verify every archive component,
replay through the declared stop, checkpoint into a fresh store epoch and
create a fresh active journal before serving. Stable email/mailbox IDs survive;
old JMAP state tokens do not. Configuration, devices and ACME secrets are
separately selected backup components with their own consistency/protection
rules. A stopped whole-service backup is the initial supported way to capture
all of them together. Never call a live directory copy a consistent backup.

## 10. Storage-specific acceptance

In addition to DESIGN section 15, require byte fixtures for every row/envelope,
all operation types, unsupported schemas, checksum failures and exact limits;
view equivalence before/after checkpoint; a read spanning concurrent commit
and checkpoint; journal admission at byte AND operation bounds; cache removal;
pin exhaustion; interrupted backup/restore; safe body reclamation after email
deletion with a pending submission; recovery with deleted historical targets;
maintenance drain with an admitted SMTP commit and an outbound completion;
maintenance timeout without reclamation; and every CURRENT publication crash
point.
Also cover duplicate-key operations within one frame; outstanding reservations
across checkpoint publication; event streams without held read views; bounded
read-slot contention; orphan files absent from inventory; and scratch quota,
interrupted-sort cleanup and exclusive-phase budget exhaustion.
Include cache-free reply assignment, conflicting/duplicate header IDs without
changing existing threadIds, forged part locators, streamed part decoding and
attachment reuse concurrent with parent deletion.
