# td-mta storage

This is the normative storage companion to [DESIGN.md](DESIGN.md). Read both
before changing persistence, queries, submission records, migration or backup.
It specifies an unimplemented target. M02 supplies the remaining field-tag
registry and golden byte fixtures before any production data is written.

## 1. Decision and alternatives

The selected design keeps ordinary immutable `.eml` files and compact binary
metadata inspectable through td-mta commands. It does not require Maildir
compatibility or human editing of live metadata. The metadata layer is a small
database we own: calling its contents files does not remove the obligation to
implement transactions and crash recovery.

Message-body storage and metadata storage are separate decisions:

| Alternative | Physical representation | What it supplies | Cost for this service |
| --- | --- | --- | --- |
| PostgreSQL | Database-managed relation/index files and WAL; bodies can be external | Transactions, SQL queries, indexes, recovery | Separate server and operational dependency; external bodies still need publication ordering |
| RocksDB | Key/value WAL, memory write buffers, sorted files, compaction | Embedded storage, atomic batch/transaction primitives | Native dependency and memory/compaction policy; application keys and indexes remain our work |
| SQLite + `.eml` | Embedded database, journal/WAL and ordinary body files | SQL metadata transactions without a server | A new dependency outside the current std-only policy |
| Maildir | Message per file, folder directories, filename flags | Familiar access with existing mail tools | Additional JMAP identity/change/queue metadata and multi-folder semantics |
| mbox | Messages concatenated into mailbox files | Simple interchange and sequential reading | Locking and potentially large rewrites for removal/modification |
| dbox | Individual or packed message files plus authoritative metadata indexes | Mail-specific storage | Its metadata indexes require preservation and format-aware tooling |
| This design | Immutable bodies, one metadata checkpoint, bounded recent journal | Explicit domain transactions and resource bounds | We implement and test the storage engine |

We choose the last row under the current dependency and allocation constraints.
If the dependency policy changes, SQLite plus external bodies is the first
alternative to reconsider. PostgreSQL/RocksDB are not inherently unable to run
small mailboxes; their actual memory use depends on configuration and workload.
The observed Stalwart RSS does not identify the database's individual cost.
Stalwart 0.15.2 has a filesystem blob backend as well as RocksDB: comparing
metadata engines does not establish where a particular installation puts bodies.

V1 deliberately uses a single sorted checkpoint plus a bounded recent-change
journal, with write admission paused during checkpoint publication. There is
no general mutable B-tree, multi-level LSM compactor, or whole-mailbox RAM map.
This favors an auditable implementation over sustained write throughput.

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
| `threads` | thread ID | Persisted grouping identity, bounded merge/alias information |
| `submissions` | submission ID | Email/thread/identity IDs, immutable transmitted blob ID, envelope sender, sendAt, lifecycle/notification state |
| `recipients` | submission ID + recipient ordinal | Address, attempt/phase, result, retry time, expiry, bounded diagnostic |
| `leases` | upload blob ID | Owning account/device, expiry and permitted use |
| `imports` | source-instance ID + length-prefixed source-account and object bytes | Local IDs and verified source digest/mapping |

Every record carries its last changed transaction sequence. Fields needed for
submission remain in its record even if the visible email is later deleted.
Recipient ordinal is a big-endian u32 in the key so its byte order is numeric;
ordinary integer values use the encoding in section 4. Indexable timestamps and
addresses do not become primary keys solely for query speed.

The source account/object part of an import key, including its two u32 length
prefixes, is at most 1008 bytes, giving a total key ceiling of 1024 bytes.
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

A committing writer validates these rules, blob kinds, keyword limits and
submission/blob pins. Derived counters are calculated or cached, never
independently authoritative. A transaction that updates folder membership also
declares the affected JMAP objects/state types for change APIs.

For example, `store inspect email e123 --json` might decode:

```json
{
  "emailId": "e123",
  "blobId": "ab91",
  "threadId": "t55",
  "receivedAt": "2026-09-22T18:30:00Z",
  "mailboxIds": ["m1", "m7"],
  "keywords": ["$seen"],
  "viewSequence": 812
}
```

That JSON is an assembled inspection view, not a JSON file on disk. Its fields
come from the email, membership and keyword tables overlaid with recent
committed updates. `m7`'s name comes from its mailbox row. Subject and attachment
names come from the message or its disposable parsing cache.

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
table/key/sequence, and use the latest operation for each key. No per-record
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

Checkpointing pauses new mutation admission, freezes sequence S, then:

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
account, store epoch and sequence. Restoration changes the epoch. History
expiry is not permission to delete current rows or pending queue data.

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

For unreferenced blobs, first remove inventory rows and expired lease rows in
a durable transaction, then unlink their files and sync directories. Use bounded
batches if one frame cannot hold the sweep. A crash may leave an orphan file,
which the next sweep can remove; it cannot leave a live reference to deleted
bytes. Orphans from interrupted publication are identified by inventory absence
only while the exclusive window proves no writer/upload can be using them.
Do not unlink files based only on age or a filename absent from one old index.

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

## 10. Storage-specific acceptance and sources

In addition to DESIGN section 15, require byte fixtures for every row/envelope,
all operation types, unsupported schemas, checksum failures and exact limits;
view equivalence before/after checkpoint; a read spanning concurrent commit
and checkpoint; journal admission at byte AND operation bounds; cache removal;
pin exhaustion; interrupted backup/restore; safe body reclamation after email
deletion with a pending submission; recovery with deleted historical targets;
maintenance drain with an admitted SMTP commit and an outbound completion;
maintenance timeout without reclamation; and every CURRENT publication crash
point.

The comparison in section 1 draws on these primary descriptions:

- [PostgreSQL physical layout](https://www.postgresql.org/docs/current/storage-file-layout.html)
  and [WAL](https://www.postgresql.org/docs/current/wal-intro.html).
- [RocksDB overview](https://github.com/facebook/rocksdb/wiki/RocksDB-Overview)
  and [memory accounting](https://github.com/facebook/rocksdb/wiki/Memory-usage-in-RocksDB).
- [SQLite use cases](https://www.sqlite.org/whentouse.html) and
  [WAL/backup considerations](https://www.sqlite.org/wal.html).
- [Dovecot Maildir](https://doc.dovecot.org/latest/core/config/mailbox_formats/maildir.html),
  [mbox](https://doc.dovecot.org/main/core/config/mailbox_formats/mbox.html), and
  [dbox metadata authority](https://doc.dovecot.org/2.3/admin_manual/mailbox_formats/dbox/).
- [Stalwart 0.15.2 filesystem blobs](https://raw.githubusercontent.com/stalwartlabs/stalwart/v0.15.2/crates/store/src/backend/fs/mod.rs)
  and [RocksDB backend](https://raw.githubusercontent.com/stalwartlabs/stalwart/v0.15.2/crates/store/src/backend/rocksdb/main.rs).

No external database dependency is introduced by this comparison or decision.
