# td-mta storage

This is the normative physical storage contract. [FORMAT.md](FORMAT.md)
owns transient application key and row encodings; SQLite owns relational
storage, pages, WAL, locking, transactions and crash recovery. [API.md](API.md)
owns the typed adapter boundary and [QUEUE.md](QUEUE.md) submission policy.
The storage core is implemented; protocol handlers, mutation authorization,
service admission, administration and deployment remain separate.

## 1. Authority and layout

One locked root contains one SQLite database for all accounts:

```text
metadata.sqlite3                 authoritative bodies, metadata and indexes
metadata.sqlite3-wal             SQLite write-ahead log
metadata.sqlite3-shm             SQLite coordination state
LOCK                            cooperative process writer lock
```

Complete immutable message, upload and transmitted-copy bytes live in the
`blob_chunks.body` rows. Relational columns store mailbox names, membership,
keywords, object IDs, lengths, SHA-256 digests, threading, SMTP envelopes,
submissions, recipients, leases, import mappings and retained changes.
No generic binary record table or generic reference table is persisted.
Parsed-body/search caches may be rebuilt; bodies and metadata are authoritative.
There is no permanent body-file tree, custom journal, replay map or selector.

The safe std adapter requires an operator-controlled stable namespace,
caller-owned private directories, regular private files with exactly one
hard link and a retained cooperative LOCK. It does not defend against another
process with the same filesystem authority replacing paths. SQLite uses its
bundled Unix VFS; td-owned Rust adds no direct syscall or unsafe allowance.
A database or sidecar symlink, wrong owner/mode, extra hard link or oversized
file refuses startup. Creation refuses preexisting database/WAL/SHM paths.
Application ID, exact schema version 2, closed schema and 4096-byte pages
are checked before accepting the store. Full quick_check and foreign_key_check
are explicit validate_integrity maintenance, not an opening scan. SQLite
validates physical pages on access and body pins verify their full digest. Earlier formats
are refused; no automatic migration or overwrite is provided.

## 2. SQLite and resource policy

The private dependency is rusqlite 0.40.2 with bundled SQLite 3.53.2 and
hooks and limits features. Exact manifests, locks, active features and
archive checksums are pinned by builder/src/crypto_policy.rs. No system SQLite, extension, attachment,
external SQL or ambient pkg-config selection is accepted. Runtime statements
are closed, parameterized queries. Public interfaces expose typed rows, keys,
body streams, changes and errors, never native connections or caller SQL.

Use 4096-byte pages, at most 2097152 pages (8 GiB), WAL, synchronous FULL,
foreign_keys ON, defensive mode, trusted_schema OFF, in-memory temporary
storage, mmap disabled, cache spill enabled and zero busy timeout. Each
connection requests a 128 KiB page-cache target, not a hard ceiling. Spilling
permits a maximum body write without retaining its dirty pages in RAM.
Compile-time limits cap SQLite's shared requested heap at 16 MiB and each
allocation at 9 MiB. These limits do not measure allocator overhead or RSS.
SQL values are bounded to 69632 bytes (64 KiB chunks plus row headroom); SQL text is at most 8192 bytes. Application batches contain at most
4096 operations and 1 MiB transient encoded metadata, with 1024-byte keys and
65536-byte row values. Body content never enters these metadata buffers.

The complete encoded message or upload has a hard 32 MiB ceiling. Configured
message_bytes may reduce it. Body writes and reads use chunks of at most
64 KiB, with fixed caller/writer scratch and indexed chunk rows. At most two
chunk reads serve an unaligned 64 KiB request; no handle repeatedly walks a
message-sized overflow chain. Chunk insertion and final durable commit are
synchronous native operations: checks before and after them do not promise a
scheduling yield inside each page allocation or fsync. A supplied Read implementation must
itself obey its admitted I/O deadline; a synchronous call cannot forcibly
interrupt an arbitrary reader.

Each write reserves worst-case WAL room for a whole database's changed pages,
plus 32 extra frames for commit and sector padding, plus the WAL header:
`(2097152 + 32) * (4096 + 24) + 32` bytes. The WAL ceiling is twice that
bound, 17280796224 bytes. This conservative disk bound is
not preallocation or a free-space guarantee. The WAL index can map up to
34 MiB outside SQLite's 16 MiB heap, explicitly charged in the startup ledger.
Crash recovery can reread the entire WAL; checkpoint can write up to 8 GiB.
These native calls cannot guarantee a yield at a caller deadline. Return Busy
before writing when the existing WAL leaves insufficient room. SQLite reuses
already-spilled frames of the current transaction. journal_size_limit is not
used as a live hard ceiling. Explicit TRUNCATE checkpoint requires no live
views; automatic WAL-size checkpoints are disabled. Closing the last native
connection can still run SQLite's passive checkpoint and remove the WAL.
That synchronous drop-time work has no application deadline and may copy up
to the 8 GiB database ceiling; explicit maintenance before teardown avoids
leaving that work to connection destruction.

The 9 MiB individual allocation ceiling accommodates SQLite 3.53.2's
contiguous checkpoint iterator. At the admitted WAL ceiling there can be
4194368 frames and 1025 index segments. On x86-64 its request is
`8 + 1025*32 + 4194368*2 + 4096*2 = 8429736` bytes, including merge scratch;
it is already eight-byte aligned, and SQLite's fallback allocator adds an
eight-byte C header. The shared 16 MiB heap cap still applies to this buffer
and every retained connection together. Page-cache targets are not hard
reservations: this calculation alone does not qualify their combined peak.

Runtime scopes retain one original monotonic clock/deadline and 8000000
interruptible VM steps. Opening validates only fixed schema/header state under that same bound. Full
integrity maintenance has a separate 1099511627776-step ceiling derived from
the 8 GiB physical cap, under an explicitly supplied deadline. An interrupted
maintenance scan reports failure without making the store impossible to open. Clock reversal or work failure stays sticky.
COMMIT and ROLLBACK finish without progress interruption; a late clock sample
does not obscure the actual durable result. Busy never extends a deadline.

### Atomic body and metadata commit

IndexStore::commit accepts typed operations and mutable BlobSource inputs.
Each source identifies a BlobId and supplies a std::io::Read. Each source matches exactly one Blob PUT. The matching
BlobRow supplies the exact expected length, SHA-256, kind and creation time.
The writer admits the maximum length before inserting a row, then inserts and hashes bounded chunk rows. Exact length, physical
source EOF and digest must agree before COMMIT. Readers may wrap caller-owned
provisional files, but those files carry no durable store authority.

One writer mutex serializes BEGIN IMMEDIATE, expected account-sequence
comparison, body streaming, relational changes, final reference/parent checks,
sequence update and COMMIT. A rejected source or batch rolls back both body
and metadata. No separate publication proof, body rename or permanent-body orphan
collection is required. Provisional ingress staging, its quotas and crash
cleanup remain unimplemented service work. Existing body identity and bytes are immutable; an identical Blob PUT is a
no-op that retains its original changed sequence. Every chunk has at most
65536 bytes and a consecutive ordinal; empty bodies have no chunks. No SQL
statement assembles a complete message or rewrites an existing body. Blob IDs have a
permanent native registry and cannot be reused after deletion.

Native deferred foreign keys enforce final owning relationships even when a
target is deleted. Bounded parent walking also refuses mailbox cycles. Exact
unsigned account sequences use eight-byte big-endian blobs, preserving values
above i64::MAX; overflow refuses mutation. Account creation is bounded to 128.
Change actions must agree with pre/post existence, and a unique native index
refuses duplicate changes for one object within a transaction.

Successful synchronous FULL COMMIT plus autocommit proves durability and
returns its sequence, including when the deadline expires during completion.
Pre-COMMIT failures and deferred-constraint/busy refusals reject after rollback;
failed rollback retires the writer. Other COMMIT failures are indeterminate
and stop writes until reopen/recovery. New read snapshots remain available.
Never acknowledge before a proven successful COMMIT. Queue transitions,
recipient aggregates, authorization, result idempotence and ports::Store
coordination remain service work; the low-level core grants no permission.

### Snapshots, changes, reclamation and backup

The cold pool owns one to eight connections. Capturing a view under the writer
fence begins a SQLite read transaction and reads its account endpoint to
establish the snapshot. Every lookup and body read uses that same snapshot
and original work scope. Capture may return Busy while a commit streams a
body; already captured views remain usable. ViewIdentity contains account,
epoch, committed sequence and history floor. SQL columns are decoded into
caller buffers, without loading a mailbox into RAM.

A verified body input and completed PinnedBlob borrow the live view. Indexed
chunk reads use the retained read transaction to preserve identity. A read
error that ends that transaction permanently fails the view; later operations
cannot silently switch snapshots. Length/digest verification precedes completed random access. The
borrow and explicit destructor keep the pooled connection loan live until
the body owner is destroyed, including when held inside a MIME owner.
Drop rolls back the read transaction before returning the connection; failed
cleanup closes and retires the slot. Cleanup bypasses expired request fuel
without clearing its sticky failure. Reopen restores retired capacity.

Deleting an unreferenced blob removes body and metadata transactionally. Old
views can still read its old bytes through their WAL snapshot. SQLite reuses
freed database pages; logical deletion does not shrink the main file. Leases
retain upload bytes until the lease row is explicitly deleted, even after
expiry. Expiry or revocation removes permission to use an upload, not its
foreign-key ownership. No custom per-file pin registry is needed.

Changes use the native account/kind/sequence/operation indexes. History pruning
is not activated; floor remains zero and the hard database ceiling can refuse
writes until an explicit maintenance policy is implemented.

Backup must capture one consistent SQLite state. Stop service activity,
checkpoint successfully, close every connection, then copy the main database;
that snapshot contains bodies and metadata together. Copying only the live
main file can lose committed WAL contents. Online backup, operational restore,
verification/repair and history maintenance tools remain unimplemented.
SQLite integrity checks do not replace digest and domain validation.

## 3. Metadata records

Each domain has an explicit table and typed columns, with account-scoped
primary keys and native foreign keys. IDs use exact 16-byte BLOB columns;
ordinary metadata is TEXT or INTEGER, not an opaque encoded value. Logical
ReadView key ordering remains FORMAT.md's unsigned byte order. Anchor/import
length-rank indexes preserve its little-endian length-prefix order without
storing encoded shadow keys. No table is loaded in full into RAM.

| Table | Key | Authoritative value |
| --- | --- | --- |
| `blobs` | blob ID | Kind (message/upload), length, SHA-256, creation time |
| `blob_chunks` | blob ID + chunk ordinal | Immutable body bytes, at most 64 KiB per row |
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

SMTP receipt recipients occupy smtp_receipt_recipients(account,email_id,ordinal,
address), preserving accepted order and duplicates. Email replacement updates
these children atomically; their foreign key cascades on email deletion. Other
owning foreign keys are deferred NO ACTION so caller batches explicitly remove
relationships and remain order-independent. Historical submission/import IDs
have no foreign key. Every domain row carries its last changed sequence. Submission expiry
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
to submission, submission to transmitted blob, and every lease to upload blob.
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

A lease must name the view's account and retain its upload target until the
lease is explicitly removed. The validation completion retains the trusted
UTC sample, but ownership is independent of expiration. Device authorization,
revocation and unexpired-use checks remain separate. CompleteReferences grants
no proof of body integrity, source-row custody or complete database validation;
the coordinator composes these checks before activation.

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
Physical primary keys use explicit domain columns; FORMAT.md keys are the
transient caller representation.
That JSON is an assembled inspection view, not a JSON file on disk. Its fields
come from the email, membership and keyword rows in the captured SQLite snapshot. `m7`'s name comes from its mailbox row. Subject and attachment
names come from the message or its disposable parsing cache.

### 3.1 MIME part blob identities

Stored blob IDs and JMAP part blob IDs are distinct typed forms. A part ID is
a versioned encoding of its parent stored blob ID, encoded-body offset/length
and transfer-encoding tag; [WIRE.md](WIRE.md) freezes its canonical bounded
wire encoding within JMAP's ID length limit. Nested attached messages use its
bounded chain of decoded-stream ranges. A part never names an independently
stored body or an entry in `blobs`. Resolve it only in an authorized account and live parent
view, or against an authorized unexpired upload lease for a parsed raw message;
validate checked ranges and require an exact match to a parsed MIME part
descriptor, rebuilt boundedly if its cache is absent. A forged locator cannot
select arbitrary body bytes or bypass parent authorization.

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
rollback, owning references, parent cycles, atomic body writes and snapshot reads after deletion.
Native allocation/RSS qualification and complete crash/fault matrices remain
required before service activation. Rust allocation evidence for pure MIME
processing does not qualify SQLite or whole-service memory.

Opening checks fixed schema/header state without a full-database scan. Full
integrity validation is explicit bounded maintenance. Runtime commits use indexed deferred foreign-key enforcement instead of scanning
all accounts. The writer preallocates 128 KiB row/reference scratch at cold
startup; 64 KiB body chunks reuse it. Core blob metadata cannot admit a body
above 32 MiB; caller policy enforces any lower message/upload ceiling.
Writer fence acquisition refuses immediately when occupied.

The ignored `large_wal_checkpoint_fits_native_allocation_cap` fixture is an
explicit large-WAL qualification. It keeps all nine permitted native
connections, retains an old snapshot while replacing one 32 MiB body per
commit, then drops the snapshot and truncates the WAL. It bounds generation
at 160 commits (5 GiB input), a 6 GiB WAL extent and 15 minutes checked
between calls; native calls remain synchronous. It verifies the preserved
snapshot, checkpoint refusal while borrowed, successful truncation, a
sub-64-MiB database, and reopened sequence/integrity/body contents.
Ordinary gates leave this multi-gigabyte fixture ignored.

Build through the normal forced native driver with an optimized test profile
so hashing does not dominate the qualification:

```text
CARGO_PROFILE_TEST_OPT_LEVEL=2 target/release/td-builder gate-crates crypto-cargo test --manifest-path td-mta/Cargo.toml
```

This command runs the ordinary mail suite to produce its test executables.
Use the `td_mta-...` path on its `unittests src/lib.rs` line, not the
`src/main.rs` executable, to set `td_mta_lib_test` below. Cargo prints a
repository-relative path; run from the repository root. Select an existing
private, disk-backed TMPDIR with at least 6 GiB free; a tmpfs consumes RAM
outside the test process's RLIMIT_DATA. Apply an outer timeout for native calls:

```text
td_mta_lib_test=.td-build-cache/crypto-target/x86_64-unknown-linux-gnu/debug/deps/td_mta-LIB_TEST_HASH
TMPDIR=/path/on/disk timeout --kill-after=5s 1200 target/release/td-builder run-capped "$td_mta_lib_test" --ignored --exact store_fs::index::tests::large_wal_checkpoint_fits_native_allocation_cap --nocapture --test-threads=1
```

Require exit status zero and a libtest summary of exactly one passed test
and zero failures. Zero matched tests is a qualification failure. The fixture
prints its private root. If external termination prevents Drop cleanup, first
confirm that invocation and its descendants have exited, then remove only
that printed root; never sweep other agents' temporary directories.

The x86-64 GNU host run used rustc 1.99.0-nightly (6f72b5dd5), test
opt-level 2 and Linux 7.0.14/btrfs. It generated 1050433 valid frames
in a 4327783992-byte WAL. The original 2 MiB individual cap refused checkpoint
with Capacity. With the 9 MiB cap the checkpoint took about two seconds;
the complete fixture, including WAL generation and reopen/content checks,
passed in 83.14 seconds with the shared 16 MiB limit unchanged. This qualifies that
specific failure boundary, not maximum-WAL mapped memory, native stack/RSS,
the complete 8 GiB database, or combined service overlap.

Creation is exclusive but not crash-atomic. A failed initial creation can
leave an incomplete database or a complete durable database whose startup
validation or connection-pool preparation exceeded the deadline. Neither
create nor open overwrites it. With the service stopped and root exclusively
locked, first try opening it with a fresh startup scope. Inspect and remove
only an incomplete new database and its SQLite sidecars before retrying
creation.
Never apply this reset to a valid database with authoritative bodies.
The bundled SQLite compile retains upstream optional modules; closed runtime
queries expose none as an API. Runtime version admission requires 3.53.2.

Small body reads admit one bounded SQLite query per call and fetch at most two
64 KiB chunks, including for an unaligned request. SQLite may materialize a
complete chunk for a smaller returned slice; MIME byte meters count returned
logical bytes, not that native copying or B-tree work. Native VM/heap caps and
the original deadline apply separately. No per-pin chunk cache is reserved.
The WITHOUT ROWID chunk table favors one account/blob/ordinal key lookup;
its 64 KiB payloads may deepen native B-trees. This fixed per-call amplification
is admitted, not a claim of physical I/O equal to returned MIME bytes.

Full validate_integrity maintenance refuses a stopped writer and holds the
writer fence throughout its scan. Existing read views remain usable; new view
capture and commits return Busy until it finishes. It reports physical SQLite
and foreign-key consistency, not body digests or protocol authorization.
