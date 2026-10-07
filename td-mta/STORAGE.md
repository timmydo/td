# td-mta storage

This is the normative physical storage contract. [FORMAT.md](FORMAT.md)
owns application key and row encodings; SQLite owns database pages, WAL,
locking, transactions and crash recovery. [API.md](API.md) owns the public
adapter boundary, and [QUEUE.md](QUEUE.md) owns submission transitions.
The storage core is implemented; protocol handlers, mutation authorization,
service admission, operational inspection and deployment remain separate.

## 1. Authority and layout

One locked root contains one SQLite database for all accounts:

```text
metadata.sqlite3                 authoritative metadata and native indexes
metadata.sqlite3-wal             SQLite write-ahead log
metadata.sqlite3-shm             SQLite coordination state
LOCK                             cooperative process writer lock
accounts/ACCOUNT/messages/ab/ab....eml
accounts/ACCOUNT/uploads/cd/cd....blob
accounts/ACCOUNT/tmp/00000000000000000001.tmp
```

Account and object components are exactly 32 lowercase hex digits; the shard
is the first object byte. Body contents stay in immutable regular files.
SQLite stores mailbox names, membership, flags, object IDs, blob lengths and
SHA-256 digests, threading, submissions, recipients, leases, imports and
retained changes. Files alone cannot reconstruct these authoritative values.
Parsed-body/search caches may be rebuilt; authoritative metadata may not be
silently discarded as a cache. There is no custom checkpoint, journal-frame,
manifest, selector, replay overlay or table-file engine.

The safe std adapter requires an operator-controlled stable namespace,
caller-owned private directories, regular private files with exactly one
hard link and a retained cooperative LOCK. It does not defend against another
process with the same filesystem authority replacing paths. SQLite uses its
bundled Unix VFS; td-owned Rust adds no direct syscall or unsafe allowance.
A database or sidecar symlink, wrong owner/mode, additional hard link or
oversized file refuses startup. Creation refuses any existing WAL/SHM path before
creating the database.
Metadata schema/application ID/version and 4096-byte page size,
quick_check and foreign_key_check are checked before accepting the store.
An old FORMAT store refuses creation; no automatic migration exists.

## 2. SQLite and resource policy

The sole private dependency is rusqlite 0.40.2 with bundled SQLite 3.53.2,
hooks and limits. The exact manifest, lock, active graph and archive bytes
are pinned by builder/src/crypto_policy.rs. No system SQLite, extension,
external SQL, attachment or ambient pkg-config selection is accepted.
All runtime statements are closed parameterized queries owned by the core.
Public interfaces expose typed rows, keys, changes and errors, never a native
connection or caller-provided SQL.

Use 4096-byte pages, at most 8192 pages (32 MiB), WAL, synchronous FULL,
foreign_keys ON, defensive mode, trusted_schema OFF, in-memory temporary
storage, mmap disabled, cache spill disabled and zero busy timeout. Native
connections request a 128 KiB page-cache target, not a hard per-connection
ceiling. With spill disabled, dirty pages may consume more of the shared heap;
Capacity is possible for a maximum batch, without partial metadata commit.
Compile-time limits cap SQLite's process-wide requested heap at 16 MiB and individual allocations
at 2 MiB; these are not process RSS or allocator-overhead measurements.
Application batches contain at most 4096 operations and 1 MiB encoded data;
keys are at most 1024 bytes and values at most 65536 bytes.

Each transaction reserves WAL room for all 8192 pages plus frame headers;
the WAL ceiling is twice that bound (67502144 bytes). If there is insufficient
room, return Busy before starting the write. Checkpoint explicitly when all
views have ended; TRUNCATE must report success. Automatic checkpointing is
disabled. Connection setup and runtime read/write scopes retain a monotonic
clock/deadline and at most 8000000 interruptible VM steps. Cold-start schema
and integrity validation uses a separate 128000000-step budget under the
same deadline.
A failed view stays failed even if the caller clock later moves back.
COMMIT and ROLLBACK finish without progress interruption; a late sample
records the deadline failure without obscuring the actual durable result.
Busy never extends a deadline.

## Commit and immutable body publication

Create body output exclusively in the account's tmp directory. Bound writes,
sync the completed file, publish without replacement by hard link, and sync
both affected directories. Only the resulting owner-bound PublishedFile
proves this publication. Before inserting a fresh BlobRow, commit checks
root, name, inode, owner/mode/link count, exact length, whole-file SHA-256 and
physical EOF. Raw bodies never enter the database operation payload.

One writer mutex serializes BEGIN IMMEDIATE, expected account-sequence
comparison, final-row/reference validation, SQL changes, sequence update and
COMMIT. Foreign keys enforce final owning references even when a referenced
row is deleted; bounded mailbox-parent walking additionally refuses cycles.
Sequences are unsigned 64-bit values encoded as big-endian eight-byte blobs,
so SQLite ordering is unsigned and does not truncate values above i64::MAX.
Overflow refuses mutation. Account creation is bounded to 128 accounts.
Blob IDs are registered permanently in SQLite and cannot be reused after
deleting their current row. Change actions must agree with pre/post existence;
a unique native index refuses duplicate changes for one object per transaction.

COMMIT with synchronous FULL precedes success. A failure before COMMIT is a
known rejection only after rollback; failed rollback retires the writer.
Deferred constraint and busy refusals with a still-open transaction reject
only after rollback. Successful FULL COMMIT plus autocommit proves durability
and returns its sequence even if the deadline expired during completion.
Other COMMIT failures are indeterminate and retire writes; rollback cleanup
releases the pending transaction when possible. New read snapshots remain
available. Reopen for SQLite recovery and resolve durable state before retrying
an operation with external effects. Published but unreferenced
bodies remain orphans; a rejection does not prove their files vanished.
Queue mutation policy, recipient aggregate validation, authorization,
request-result idempotence and complete ports::Store integration remain
service work; the low-level core grants none of those permissions.

## Snapshots, changes and reclamation

The fixed pool owns one to eight connections prepared at cold startup.
Capturing a view under the writer fence begins a SQLite read transaction
and reads its account endpoint, establishing the WAL snapshot. Every get,
ordered-next and change query uses that snapshot and its original work scope.
Captures share the coarse writer fence with commits, checkpoints and body
collection. A commit verifies up to 128 MiB of body data while holding that
fence; simultaneous captures can return Busy immediately. Already captured
views remain usable. Finer capture/maintenance locking requires separate
qualification. Each VM opcode samples the clock and fuel; statement
preparation remains bounded work. Sampling or statement-cache optimizations
require measured native evidence before changing these checks.
Later commits remain invisible. ViewIdentity contains account, store epoch,
committed sequence and history floor; it carries no custom file-generation
or journal-prefix identity. Rows are decoded into caller-provided buffers.
SQLite performs indexed lookup; metadata is never replayed into a RAM map.

A verified body input borrows the live view. Whole-file verification returns
random access to the same retained descriptor, with the original view clock
and deadline. This borrow prevents returning the view slot while a body is
held. Dropping a view rolls back its read transaction before returning the
connection; rollback failure closes and retires that slot. Rollback cleanup
bypasses expired request fuel without clearing its sticky failure. A failed
connection is closed before its slot becomes retired; retired slots reduce
read capacity but do not hold the maintenance fence. Reopen restores capacity.

Delete the BlobRow durably before collecting a raw file. collect_orphan
holds the writer fence, verifies no current blob record exists, and refuses
while any view is live. It checks the typed immutable path, unlinks it and
syncs its parent. Failed cleanup remains charged. This coarse exclusion
protects every old snapshot without a custom per-file generation registry.
A full bounded orphan scan and service cleanup scheduler remain separate.

Changes live in indexed SQLite rows with account, sequence and operation
order. They support the existing fixed-kind change cursor through its captured
endpoint. History pruning is not activated: floor remains zero and the
hard database ceiling can refuse further writes until an explicit maintenance
policy is implemented. No unbounded in-memory history is built.

Backup must preserve one consistent SQLite database/WAL state and all bodies
referenced by that state. Copying only the main database during writes is
invalid. A stopped whole-service backup initially supplies this boundary;
online backup, offline verification/repair and legacy import tools remain
unimplemented. SQLite checks are not a substitute for verifying referenced
raw-file digests or domain invariants.

## 3. Metadata records

SQLite records use an account/table/key primary key with unique bounded
values and unsigned bytewise key ordering. Tables are not all loaded into RAM.
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
ordinary integer values use FORMAT.md section 1. Indexable timestamps and
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

`mailbox_parents::ParentWalk` checks one mailbox's parent chain through a
caller-owned ReadView. Capture the full view identity and admit an explicit
maximum number of lookups. Each advance performs at most one get, verifies the
view identity before and after it, validates the returned mailbox row and
sequence ceiling, then copies only its parent ID. Check the post-get identity
before interpreting a found row, absence or an error. ChangedView takes
precedence when the view moved. A missing row, invalid
row, changed view, lookup error or exhausted read budget retires the walk. Budget
exhaustion is a resource refusal, never proof of a cycle or a valid chain.

The walker uses Brent's cycle detection: advance one current ID and compare
against a saved ID replaced at power-of-two intervals. Reaching a root completes
the chain. A valid chain containing N mailboxes, including the root, needs
exactly N gets. At most 3*N gets suffice to detect a cycle among N reachable
mailboxes; callers deriving a budget from a mailbox count must use checked
arithmetic. Smaller admitted budgets may refuse a valid store. These are
lookup counts, not physical-read or time bounds, and this helper does not
enforce the separate configured mailbox-depth policy.
Direct self-parent rows already fail local row validation. This uses fixed
state rather than a growing visited-ID set. Completion reports the captured
identity, start ID and lookup count. It owns no pin and validates no other
mailbox chain or cross-row invariant. The coordinator must check every final
mailbox under the same actual view, preserve lookup failures as failures, and
retain its pins. The view owns each get's full work/scratch/deadline contract;
the caller also checks deadlines between advances. An advance after completion
performs no get, but still refuses a changed view. Failed or unfinished walks
cannot produce completion.

`mailbox_sweep::Sweep` enumerates every supplied final mailbox and runs
ParentWalk from each ID before advancing enumeration. Each advance performs
one next or one get, never a whole chain. Validate local source encoding,
sequence and strictly increasing mailbox IDs before admitting another row.
Check full captured identity before/after enumeration and through each get;
movement always reports the same top-level ChangedView. A finite row allowance
limits completed mailboxes and a separate total-get allowance spans all walks.
Give each new walk only the remaining allowance; exhausted work refuses rather
than declaring a cycle or a valid forest. Empty views need neither allowance.

Retain only one walker, previous mailbox ID, identity and scalar counts. No
visited forest or chain cache is built. Each walk re-reads its starting row,
even though enumeration already validated it. Total gets are the sum of every
chain length including its start and root: N root mailboxes need N gets.
Shared ancestors are read again, and a long chain can require quadratic total
gets across its starting mailboxes.
The caller admits those logical lookup counts and each lookup's full physical
work and deadline. Completed chains report their own lookup count; only EOF
after all chains rooted yields CompleteForest with identity, mailbox and total
get counts. Every error retires the sweep; incomplete/failed state cannot finish.
Repeated completion checks identity without I/O. This covers enumerated chains
only: physical enumeration completeness, actual pins, configured depth policy
and other graph invariants remain separate requirements.

`row_references::ReferenceCheck` validates direct owning references of one
supplied final row. Validate the source key/value and sequence ceiling first,
then retain only the borrowed source key and at most two copied typed targets.
Each advance performs at most one get through the supplied ReadView, checks
exact identity before and after it, validates the target row/key and sequence,
and requires the expected blob kind or recipient ordinal below the owning
submission's recipient_count. View movement takes precedence over a returned
row, absence or lookup error. Missing targets, invalid kinds/counts, malformed
rows, future sequences and lookup failures retire the whole check.

Email requires a message blob and thread; membership requires email/mailbox;
keyword and thread anchor require email; submission requires its transmitted
message blob; recipient requires submission; a mailbox's immediate parent must
exist. This direct check does not detect parent cycles; ParentWalk handles the
full chain separately. Submission email/thread/identity/notification IDs and
import mappings remain historical and cause no lookup. Blob and thread rows
have no outgoing owning references.

A lease must name the view's account even when expired. Its upload blob is a
required target only while expires_at is strictly later than the supplied UTC
millisecond sample. The driver supplies a trusted clock sample and refuses
clock failure; completion retains that sample, so it is not timeless lease
validity or permission to reclaim bytes. Device authorization/revocation remains
separate. CompleteReferences binds the supplied source key/sequence, identity,
time and successful lookup count. It grants no proof that the supplied source
was read from disk, that a blob file exists/hashes correctly, that all rows were
checked or that actual pins are held. The coordinator checks every final row,
blob contents, mailbox chains and aggregate invariants before activation.

Source key bytes must remain valid until completion; target lookup uses a
separate caller result buffer and releases borrowed target rows before the next
advance. ReadView::next ties the returned Record's key and value lifetimes
together. To reuse that value buffer for reference lookups, copy/re-derive the
source key into independent cursor/key scratch or separately charged stack
storage and decode from that separate borrow before creating ReferenceCheck.
The helper does not detach a Record's shared borrow or obtain another arena
implicitly. A zero-target check still verifies identity on its first advance.
Completed checks perform no more lookups but continue rejecting identity change;
failed/unfinished checks cannot finish. At most two gets are performed, each
subject to the view's work/scratch limits and caller deadline admission.

`reference_sweep::Sweep` enumerates the supplied ReadView's final rows in all
11 table kinds and runs ReferenceCheck for each. One advance performs one next
and at most two gets. Validate view identity before and after next, with changed
view taking precedence over row/absence/error; then require the requested table,
strict canonical key progression, local source validity and sequence ceiling.
Copy the source key into a fixed independent key buffer and decode it there,
releasing next's shared key/value borrow before reusing the value output for
target lookups. No source row or key borrow survives the advance.

Local source/table/order/sequence errors precede resource refusal. A found row
beyond the caller's finite max_rows limit then refuses before target lookups.
Count only fully checked rows; checked counters and fixed per-table counts
cannot wrap. An exhausted next reports TableComplete and moves to the
next table on the following advance. After all tables, a separate identity-
checked advance reports Complete. Zero allowed rows can still prove all tables
empty. Failures permanently retire the sweep and forbid finish; repeated
Complete performs no lookups but still rejects changed identity.

Completion retains captured identity, the single UTC sample, total and per-table
row counts. It proves direct checks over rows returned by the supplied view,
not that the view enumerated physical files completely. Selected-file integrity,
actual pins, blob byte verification, every mailbox parent chain and aggregate
rules such as missing recipient rows remain separate coordinator work. Every
step needs admission for the view's one-next-plus-two-get work and a deadline;
these are logical lookup bounds, not a physical I/O or time bound. Identity
movement during either next or a target get reports the same top-level
ChangedView error; other reference failures retain their nested classification.

`recipient_sweep::Sweep` verifies FORMAT's exact recipient coverage over the
supplied view. Walk ordered submissions and recipients with one next per
advance. For each locally valid submission, copy its ID/count and require
recipient ordinals exactly 0 through count-1 for that ID. Read the next
submission only after the current group is complete. Never skip forward to a
matching recipient: a later group/ordinal or early EOF reports the missing
expected ordinal; an earlier group or rows after all submissions reports an
unexpected recipient. Require strict order in both streams independently.

Check full identity before/after each next, with movement preceding all results.
Validate source table/key/value and sequence ceiling, copy only scalar IDs/counts
and release all row strings before returning. A finite total-row allowance covers
both streams. Local corruption and coverage errors precede row-budget refusal;
a valid row beyond admission refuses before advancing progress. EOF remains
checkable at the exact allowance, including an empty view at zero rows.
Complete requires EOF on both streams after every exact group. It retains the
captured identity and submission/recipient counts. Errors retire the checker;
failed/unfinished state cannot finish, and repeated completion still checks view
identity without another read.

Each recipient also obeys QUEUE.md's current-state phase, retry-presence,
uncertainty and failure-reason rules. Accepted requires positive RCPT and final
DATA reply codes; definitive SMTP failure/retry requires the applicable stored
negative code. Unattempted recipients cannot carry replies. Code/separator
checks do not replace wire parsing or full JMAP reply normalization. Historical
replies alone never authorize an attempt result or prove its fence.

For each exact group, completedAt must be present exactly when no recipient
has a future dispatch obligation. Retryable OutcomeUnknown remains pending;
terminal OutcomeUnknown contributes a failure requiring a Pending/Stored notice.
Unknown is terminal exactly for Expired/SmtpPermanent; other allowed reasons
must retain a next attempt.
A notice is required for a completed group containing Failed or OutcomeUnknown.
A completed, wholly Canceled group may retain an earlier Pending/Stored failure
notice; cancellation alone creates no notice. Remaining groups require None. Cancellation is
submission-wide: a group cannot mix Canceled with other states. Only scalar
flags are retained; completedAt is not compared with sendAt because a wall-clock
step can put completion earlier. Queue errors carry the submission and an
optional recipient ordinal; a group error appears at its final recipient.

CompleteCoverage establishes these current-row/group rules, not physical view
completeness, direct owning references, transition history, worker fencing,
creation authorization or actual pins. The caller composes those validations
under the same immutable view. Each step needs admission and
deadline checks for the supplied view's full next operation; one logical lookup
is not a physical I/O/time limit. Fixed progress and 20-byte cursor scratch use
the worker stack; caller key/value result partitions are reused.

Thread assignment does not depend on disposable indexes or rescanning every
body. Store at most one anchor per email: its first syntactically valid
Message-ID within a 1004-byte ceiling (four-byte length plus ID plus 16-byte
email ID fits the key limit). Header ID matching is byte-exact after parsing
away delimiters and grammatical CFWS. Absent or oversize IDs produce no anchor;
raw headers remain unchanged. Examine at most the last 32 References IDs,
nearest first, then bounded In-Reply-To IDs, then the email's own anchor ID.
Use the first ID with a live
anchor; duplicate anchors select the lexicographically smallest email ID.
Join that email's persisted thread. If no candidate resolves, create a fresh
thread. Existing email/thread assignments are immutable, even when a later
arrival connects two conversations. POLICY.md section 5 pins malformed and
repeated-field behavior; CASES.md T01-T04 pin the fixture outcomes.
Resource/I/O failure is an explicit temporary error, not permission
to silently choose a different thread. Authoritative anchor lookup remains
available without a cache, using bounded-work indexed SQLite access.

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
come from the email, membership and keyword rows in the captured SQLite snapshot. `m7`'s name comes from its mailbox row. Subject and attachment
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

## 4. Acceptance boundary

Storage tests exercise actual SQLite snapshots, reopen, unsigned sequences,
rollback, owning references, parent cycles, body publication and collection.
Native allocation/RSS qualification and complete crash/fault matrices remain
required before service activation. Rust allocation evidence for pure MIME
processing does not qualify SQLite or whole-service memory.

Startup uses a separate 128000000-step schema/integrity budget. Runtime
commits use indexed deferred foreign-key enforcement instead of scanning
all accounts. The writer preallocates 128 KiB row/reference scratch at cold
startup; 64 KiB body chunks reuse it. Core blob metadata cannot admit a body
above 128 MiB; caller policy enforces any lower message/upload ceiling.
Writer fence acquisition refuses immediately when occupied.

Creation is exclusive but not crash-atomic. A failed initial creation can
leave an incomplete database or a complete durable database whose startup
validation or connection-pool preparation exceeded the deadline. Neither
create nor open overwrites it. With the service stopped and root exclusively
locked, first try opening it with a fresh startup scope. Inspect and remove
only an incomplete new database and its SQLite sidecars before retrying
creation.
Never apply this reset to a valid database or referenced body set.
The bundled SQLite compile retains upstream optional modules; closed runtime
queries expose none as an API. Runtime version admission requires 3.53.2.
