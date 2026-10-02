# td-mta storage

This is the normative storage companion to [DESIGN.md](DESIGN.md). Read both
before changing persistence, queries, submission records, migration or backup.
It specifies an unimplemented target. [FORMAT.md](FORMAT.md) owns the numeric
registry and byte layout. M02 completes row codecs and golden fixtures before
any production data is written; scalar/key codecs alone do not open a store.
[API.md](API.md) owns adapter/state contracts; [QUEUE.md](QUEUE.md) owns the
submission transition and restart contract.
[ADMISSION.md](ADMISSION.md) owns disk/reservation ceilings, maintenance work
and deadlines, and private request-result retention.

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
      CURRENT.NNNNNNNNNNNNNNNNNNNN.tmp  incomplete selector output
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
      gc-cursor                  disposable reclamation scheduling hint
      000042/email-offsets.idx
      000042/mailbox-order.idx
      000042/search.idx
    tmp/                         unpublished bodies/checkpoints/sort scratch
      NNNNNNNNNNNNNNNNNNNN.tmp  exclusively created private output
      requests/REQUEST/          disposable private JMAP response/creation map
  devices/                       private device verifiers, separate schema
  acme/                          private keys, orders and certificate generations
```

Mail and upload shards use the first byte of the ID (two hex characters),
distributing files over 256 directories without putting mailbox names in paths.
Generation and segment numbers follow FORMAT section 1's fixed-width canonical
encoding; the shortened paths above are illustrative. Journal filenames append
exactly `.log` to the numeric component.
`store_paths` generates root-relative account paths from typed IDs, checked
numbers and known table/blob kinds in a fixed 128-byte buffer plus NUL. Its
borrowed path/C-string views allocate nothing. Directory-entry blob parsing
requires the exact lowercase ID, namespace suffix and expected shard. This
establishes only canonical names, not trusted roots, descriptor confinement,
account authorization, selected-file authority or permission to delete a blob.
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

Use local storage with working atomic same-filesystem rename and file/directory
sync. XFS is the deployment target; ext4 and Btrfs are also test targets. The
adapter uses the same std filesystem operations on each. NFS and external live
writers are unsupported. There is no filesystem-type, quota-profile or kernel
syscall admission probe. Actual durability still requires the crash/error tests
below; a successful ordinary unit test is not power-loss evidence.

### Std filesystem boundary

Use `std::fs` and its safe Unix extensions. No owned syscall assembly, raw
handle adoption, C bindings or filesystem-specific ioctl is required.
`Directory::from_path` checks each component with `symlink_metadata`, rejects
non-directories (including symlinks), opens a File, and compares the final
metadata identity before/after open. Metadata reads use that retained File.
`Directory::open` builds a fixed absolute path from a generated Name and uses
the same checks. New lookups use the stored pathname, not the retained handle.

Deployment keeps every root, ancestor and mount stable for the process
lifetime. Root, the dedicated service identity and mount authority are trusted;
no external process may rename, replace or mutate service namespaces while it
runs. These checks detect existing mistakes; they do not atomically confine
lookup or prevent a concurrent symlink/rename attack. A retained File survives
rename, but future pathname lookup can reach a replacement. Stop the service
before moving storage, changing mounts or repairing directories.

`PrivateRoot::open` additionally requires a non-root-owned mode-0700 data root,
no special bits, and ancestors owned by root or that directory's owner without
group/other write permission (including sticky directories). The supervisor
must run the service as that dedicated owner without elevated capabilities.
This adapter delegates process identity to deployment rather than reading
procfs credentials. A successful path check is not proof of process identity. Deployments using user namespaces must keep
root/service identities mapped and distinct from the overflow UID. Permissions
and ownership are deployment checks, not authorization derived from mail input.
The persistent writer lock below and store recovery remain required before serving.

Directory-adapter paths are limited to 383 UTF-8 bytes; a private root is
at most 254 bytes, leaving a separator and the 128-byte generated-name budget.
Configuration syntax retains its general 4095-byte bound; the storage adapter
rejects roots exceeding its tighter bound before I/O. Structural config parsing
alone does not perform storage admission. The future complete `config check`
must apply that same root check before reporting the configuration usable.
Other file adapters (including logs/runtime/operator inputs) must establish
their own bounded path and allocation contracts before activation. Names and path assembly
use fixed buffers. Host and static-musl allocation probes must demonstrate
zero Rust allocations for successful and failing lookups at the full bound;
requalify after toolchain changes rather than relying on an undocumented std
conversion threshold. Directory iteration has separate, pending measurements.

### Disk exhaustion and durable operations

ADMISSION.md defines logical byte/file quotas and bounded pending reservations.
They do not represent reserved filesystem blocks or promise write completion.
Std supplies no free-space/inode query: report those observations as unavailable,
never synthesize a passing sample. No external command or host probe is needed.
Operators provision disk headroom and may monitor it with their ordinary tools.

Create private files with `OpenOptions::create_new` and mode 0600, directories
with `DirBuilder` and mode 0700. Blob/metadata publication uses `hard_link` to publish
completed files without replacing or modifying an existing destination, then syncs
the destination directory before removing/syncing the temporary link. Source
and destination must share a filesystem. CURRENT uses same-directory temporary
creation, file sync, atomic `rename` replacement and parent-directory sync.
Checkpoint generations use exclusive directory creation and are authoritative
only after CURRENT selection. Mutation/admission integration and selected-store recovery remain M05
implementation work.

### Persistent writer lock

`PrivateRoot::try_lock` consumes the checked root and returns `LockedRoot`.
It opens or exclusively creates an empty mode-0600 LOCK file, validating type,
owner, exact private mode, link count one, and final opened identity. Existing
symlinks, directories, special files, nonempty files and hard links refuse before
read/write open; no contents are truncated or rewritten. It uses std's
nonblocking `File::try_lock`: contention is an explicit Busy result, other
errors retain their I/O cause. Sync the locked file and root directory before
returning success, including when the inode already exists.

The returned owner retains the File without cloning or exposing unlock/raw-file
access. Drop or process death releases the lock; neither removes the inode.
Independent opens/processes must contend, and restarts reopen the same inode.
This is cooperative exclusion among writers honoring LOCK. The stable-path and
trusted-writer deployment assumptions still apply; the lock does not prevent
privileged/noncooperating modification or replacement of its pathname. Root
validation and file policy do not authenticate process credentials. Actual
store-format validation/recovery must finish before service activation.

Acquisition is startup-only and uses the typed LOCK name and fixed path buffer.
A newly created file has owner permissions restored to 0600 after umask; shared
permissions are never granted. Existing files are not chmodded. A failed
creation-policy/permission/sync step can leave an empty, possibly invalid LOCK
inode. Stop the service and correct its owner/mode before retrying; the helper
never removes or replaces that inode to retry.
Tests exercise the std lock primitive in private temporary directories even
when the harness identity/ancestry cannot satisfy production root admission.
These fixtures do not waive any production root policy.

### Exclusive directory creation

`LockedRoot::create_accounts_directory` and `create_account_directory` create
one typed directory beneath an already durable, private parent. The latter
accepts only directory variants of AccountEntry; file variants refuse before
I/O. There is no recursive creation and no adoption of an existing name. Existing
directories, files and symlinks refuse without chmod, removal or replacement.
Every parent from the data root down must have its owner and exact mode 0700.
These shared destination checks also apply to temporary files.

Creation requests mode 0700, restores owner bits through the new pathname after
umask (under the stable-namespace contract), verifies its owner/type/mode, and
opens the checked directory. Sync the new directory and then its parent before
success. Return a retained Directory for subsequent checked lookups. A newly
created checkpoint directory gains no generation-selection authority.

Use the same CreateError stages as private files: Uncreated before issuing
creation, Attempted after a failed create call, Created for a later permission,
validation, open or sync failure. Failures can leave a directory and never
remove it automatically. Keep the operation's logical charges until cleanup or
recovery establishes the effect. A collision or an incomplete hierarchy is not
permission to repair it implicitly. The caller holds LOCK, charges the work and
authorizes the account before entering this low-level primitive.

### Private temporary output

`LockedRoot::create_temporary` exclusively creates
`accounts/ACCOUNT/tmp/NNNNNNNNNNNNNNNNNNNN.tmp`, with the positive canonical
twenty-digit Number codec. The serialized writer selects an unused number;
an existing name refuses without truncation or replacement. Startup recovery
must account for and clean abandoned output before reusing names. Parent
directories must already exist, belong to the root owner and have exact mode
0700. The adapter checks the root and every component below it. Creation uses
mode 0600 and restores owner bits after umask; the opened file must be empty,
regular, singly linked and owned by that same identity.

This primitive borrows LockedRoot for its whole lifetime. Its caller must
first acquire logical file/byte charges and account authorization; an arbitrary
numeric limit or AccountId is not an admission token. Limits above the signed
64-bit file-offset bound refuse before creation. `CreateError::Uncreated`
means refusal before any create call (including an observed existing name).
`Attempted` means an exclusive open was issued and failed: std retries and
post-create filesystem errors can leave an inode even on failure, including
AlreadyExists. Keep the pending charge until reconciliation. `Created` means
open succeeded but preparation failed, so the new inode needs orphan accounting.
No outcome authorizes deleting an existing collision based on its error kind.

Sequential writes reject a chunk exceeding the remaining byte allowance or
64 KiB before I/O and keep the handle usable after that refusal. Short writes
advance only confirmed bytes; Interrupted is retried within a maximum of 64
write attempts per call. Exhaustion returns WouldBlock and retires output,
including after positive partial progress. A zero write or other I/O failure
permanently retires the writable handle; its reported length is only a confirmed
lower bound on possible disk effects. Do not replay that chunk or release its
logical charge. Dropping a handle closes descriptors and leaves its pathname
for explicit cleanup/recovery.

After a write error, `is_failed()` is authoritative for retirement. InvalidInput
can mean either a pre-I/O refusal or a filesystem error; error kind alone cannot
authorize retry. A retired handle rejects all later writes and sync.

Consuming `sync` checks the length, syncs the file, then syncs its existing
temporary parent. Success yields a read-only `SyncedTemporary` which retains
the lock borrow and reads into caller slices within the completed extent and
64 KiB per call. The worker must check its meter/deadline between calls; these
count/byte bounds cannot interrupt a blocking filesystem syscall. The reader
reports unexpected early EOF and never treats a beyond-end offset as success.
A sync error closes the handle and leaves the output charged. The type does
not promise durable newly created ancestors, hash/content validity, immutable
publication, journal commit or service readiness. Quota integration and cleanup
are later M05 work. No unlink runs in Drop.

### Immutable blob publication

`SyncedTemporary::publish_blob` consumes completed private output and binds the
message/upload destination to the account recorded at temporary-file creation.
The caller supplies the typed blob kind and ID, admits/charges the operation,
and verifies bytes/digest before publication. Parents must already be durable.
Source and destination ancestors are rechecked for private owner/mode; the
source pathname must match the retained regular mode-0600 file, have one link,
and retain its completed length. The temporary parent must still match its
retained handle. These are checks under the stable-namespace contract above.

Refuse every existing destination, including symlinks; never replace, adopt or
deduplicate a collision. Issue `hard_link`, sync the destination parent, remove
the temporary link, then sync its retained parent. Cross-filesystem errors
propagate; there is no copying fallback. Success returns a `PublishedFile` with
a bounded caller-buffer read API and the LOCK borrow; it exposes no writable
handle. The temporary parent is released. This primitive establishes neither
hash validation nor quota authority, and does not produce `ports::PublishedBlob`
or commit a transaction. No success acknowledgement follows from it alone.

`SyncedTemporary::publish_metadata` uses the same operation sequence and error
stages. Its MetadataDestination admits only a generation's table/manifest or a
fresh journal segment, always in the originating account. No directory, CURRENT,
LOCK or FORMAT name is expressible. Checkpoint/journal parents must already be
private and durable; missing generations refuse rather than being created.
The caller proves the name fresh/unselected and validates the table, manifest
or initial journal before publishing. Opaque temporary bytes can be published
by this low-level adapter; it does not certify their format. Journal append
access, graph validation and CURRENT selection remain separate operations.

`PublishError` records the last established boundary:

| Variant | Meaning |
| --- | --- |
| Rejected | No link was issued by this attempt; existing state may remain. |
| LinkAttempted | Link call errored; destination creation may have occurred. |
| Linked | Link succeeded; destination-parent sync failed. |
| DestinationSynced | Destination-parent sync succeeded; temporary unlink errored and may have occurred. |
| TemporaryUnlinked | Temporary unlink succeeded; temporary-parent sync failed. |

Every error consumes the handle and retains logical charges for explicit
cleanup/recovery. No rollback or Drop unlink runs. Failed link/unlink calls,
including AlreadyExists/NotFound after possible internal retries, do not prove
absence of effects. After destination sync succeeds, the destination is durable
even if temporary cleanup fails, but this low-level call still returns an error.
Recovery must reconcile orphan links before reuse; a caller cannot replay the
mutation or release charges by interpreting only the underlying I/O error kind.

### Bounded private store inputs

`LockedRoot::open_format` and `open_account_file` admit typed read-only names.
Account roles are limited to CURRENT, tables, manifests, journals and blobs;
directories, LOCK and temporary output refuse before I/O. The caller authorizes
the account, pins the selected file/generation and charges read work. FORMAT is
capped at its 80-byte container; account files take an explicit byte ceiling
no larger than i64::MAX. These are I/O limits, not proof of valid format.

Check every private ancestor, then non-following file metadata: regular type,
data-root owner, exact mode 0600, one hard link and length within the ceiling.
Open read-only, repeat file policy and compare device/inode and length. Retain
that File and the LOCK borrow; no raw handle, cloning or writes are exposed.
The trusted stable-namespace contract still applies. A file with an extra orphan
link must be reconciled before selected-store reading; input does not repair it.

StoreReader reads sequentially into caller slices, at most one explicit read
and 64 KiB per call. Empty slices return zero without establishing EOF. Any
actual read error, zero before the recorded end or impossible read count retires
the reader; subsequent calls return BrokenPipe. A short read advances by only
its confirmed bytes, and the caller returns to its work/deadline meter between
calls. This does not interrupt a blocking std operation.

Consuming `finish` requires every recorded byte to have been returned, unchanged
file length and a successful one-byte physical EOF probe. Incomplete consumption,
truncation, growth or a failed probe yields no completion. Success returns a
CompleteFile retaining the descriptor, name, length and LOCK borrow, with the
same bounded random-read API as completed output. It proves extent consumption
and observed EOF only: the caller must feed the exact bytes to the format/hash
verifiers and check their summaries against selection metadata. It proves no
current pathname binding, read-view pin, parser validity or authorization.

Use this completion path for quiescent recovery or pinned immutable files.
Reading a concurrently growing active journal uses the
[captured prefix adapter](#captured-journal-prefix-io); full physical EOF is
not that prefix. Secret
and operator-config files also retain their separate SCHEMA.md policy and loader
work. No public config-check/service readiness is granted by this store reader.

### Verifying referenced blob bytes

`LockedRoot::open_blob_input` takes an account, typed BlobId, supplied final
BlobRow and explicit admitted byte ceiling. Require the row length to fit
that ceiling, private path/file policy and exact physical length before
returning input. The caller supplies authorization and the real pinned view
or stopped-store barrier. A supplied descriptor is not proof of a live owning
reference. Deleted historical PUTs do not require old bodies merely because
they occur in retained history; the replayed final view determines live blobs.

BlobInput reads at most one explicit 64 KiB chunk into caller storage and
hashes exactly the bytes returned. Bytes are provisional until completion.
Empty reads never establish EOF. Any I/O or digest failure retires the input;
the caller's buffer may already have changed when an error is returned.
Consuming finish requires whole consumption, unchanged length and observed
physical EOF, then final SHA-256 equality with the supplied digest.
CompleteBlob retains the read-only file, account/ID/kind and observed digest.
This verifies supplied blob bytes only. Full selected-graph, replay and final
owning-reference validation remain required before mutation or serving.

### Loading the selected metadata

`LockedRoot::load_selection` is the first recovery input step. Its caller must
hold quiescent store access or the actual writer/selection barrier throughout
loading; the cooperative LOCK alone does not serialize threads. Read FORMAT
and CURRENT through the private reader, consume their entire extents and
observe physical EOF. Validate their encodings/checksums, expected account and
shared store epoch before deriving a manifest path. Read only the generation
named by CURRENT, with the fixed maximum manifest size. Bind that complete
manifest's account, epoch, generation and whole-file digest using the existing
Selection codec. Missing or invalid selected files fail; no directory scan,
newer-generation fallback or repair runs.

The caller supplies reusable SelectionScratch (80 + 120 + 4832 bytes). Load at
most 64 explicit extent reads per file plus its completion probe; short reads
consume the attempt allowance, and I/O errors return immediately. Attempt
exhaustion returns WouldBlock without a selection. This bounds explicit work,
not the duration of a blocking std call. Only one input descriptor is live at
a time, and loading returns a Selection borrowing the manifest scratch. Error
values identify FORMAT, CURRENT, manifest validation or final binding and retain the
underlying I/O/container error. Returned metadata proves only this selection's
container/identity/digest binding and the consumed file extents; every table,
journal, row invariant, replay boundary and read-view pin still requires its
own validation. It grants no serving, repair or transaction authority.

### Streaming selected tables

`LockedRoot::open_table` takes a supplied Selection, expected table tag, admitted
byte ceiling and caller-owned 66608-byte record scratch. The caller provides
quiescent recovery access or the real selected-view pin/barrier. Derive the
account/generation/table name from that selection. Require the descriptor's
size within the admitted ceiling and the actual private file's size equal to
it. Read the 112-byte header into fixed scratch, start the existing table stream
verifier and compare table/account/epoch/generation/sequence, record count and
file size with the manifest before returning a TableInput.

`next_record` consumes at most one declared record. Its exact 16-byte prefix
bounds the remainder before reading into caller scratch. A prefix claiming
more bytes than the recorded remaining extent is format corruption; an actual
read failure still retains its I/O error. Prefix and remainder
share at most 64 explicit read calls; short reads consume the allowance. The
existing stream verifier checks each complete record's checksum, row grammar,
sequence, strict key order and running count/extent. Return the row as a borrow
of scratch. The caller regains control to charge/check work between records;
blocking std operations retain the same limitation as other reads. Any row
read/parse/verification error retires TableInput; subsequent calls refuse.
None means only that no declared records remain, and rows are provisional.

Consuming `finish` requires all declared records, stream completion, unchanged
physical extent plus EOF, and the selected descriptor's whole-file digest.
Success retains a read-only CompleteFile and table Summary in CompleteTable.
No unverified rows may become visible through a serving view or transaction.
A checked table alone proves no cross-table references, journal replay,
selected graph completeness, durable publication or runtime pin ownership.

### Streaming retained history

`LockedRoot::open_history` takes a supplied Selection, history descriptor index,
admitted byte ceiling and caller-owned 1 MiB frame buffer. Derive only the
selected segment's typed account/journal path. Require its declared size within
the admitted ceiling and the 96-byte journal header plus 4 MiB frame ceiling,
then require that exact physical private-file size. Exceeding the format ceiling
is a format Limit; insufficient caller admission is I/O InvalidInput. Consume
the header and compare its account, epoch, segment and base sequence before
returning input.
The existing journal verifier owns digest, sequence and operation-count checks.

`next_frame` consumes one provisional frame. Read/check the exact 64-byte header
checksum before using its declared length; a recorded suffix shorter than that
header is format corruption before any read. Require the next sequence and a
frame that fits the recorded remaining extent. Only then read the remainder
into caller scratch and run the complete frame/journal verifier. Header and
remainder share 64 explicit read attempts; the opening journal header has its
own 64-call allowance. I/O errors retain their kind; invalid frame contents or
lengths retain format errors. Every failure retires the input. The caller
regains control to meter/check work between frames; blocking std calls cannot
be interrupted by these bounds. None only marks the recorded extent's end.

Consuming `finish` requires successful stream completion, unchanged physical
extent and EOF, then selected history identity, through-sequence, exact byte
size and whole-file digest binding. CompleteHistory retains a read-only
CompleteFile and journal Summary. Retained histories are immutable: incomplete
bytes always fail here, with no truncation or repair. Active-journal committed
prefixes and incomplete-tail recovery need separate adapters/evidence. Frames
remain provisional until whole-graph/final-view validation and actual pin,
barrier and admission integration; this primitive grants no serving authority.
This is full-segment recovery/verification input. ReadView::next_change still
needs its own streaming cursor that skips PUT bodies with bounded I/O; this
frame-buffer adapter does not implement that API.

Metadata, table and both journal readers share a private exact-read helper. It
consumes the caller's existing attempt counter across framing phases, advances
only by confirmed returned bytes, and never retries errors. Empty destinations
consume no attempt. Existing metadata/table completion rules remain unchanged.

### Captured journal prefix I/O

`LockedRoot::open_journal_prefix` opens only a typed account/journal name with
an explicit captured prefix and physical byte ceiling. The caller authorizes
the account, admits read work and holds the actual committed-prefix pin or
quiescent recovery barrier. A supplied number is not a pin. Require prefix
bytes <= physical ceiling <= signed 64-bit file offset maximum before I/O.
Apply the same private owner, mode, regular-file, single-link, parent and inode
checks as whole-file input. The prefix must already exist before opening.
Permit only monotonic observed growth of that same inode between metadata
checks, within the physical ceiling. Whole-file input retains its exact-length
check. These observations do not detect a shrink followed by regrowth; real
writer/pin serialization must keep committed bytes immutable.

PrefixReader reuses bounded sequential input internally, with its length set
to the captured end. Each read makes at most one explicit 64 KiB call and can
never return a later suffix. Read failures retire it. Consuming `finish`
requires the full prefix consumed and the current file length at least that
prefix. It does not read for EOF or require an unchanged whole-file size.
Later growth beyond the opening physical ceiling is outside this reader's
scope; the journal writer separately enforces the segment limit.

Success returns CompletePrefix, a distinct read-only handle with random reads
bounded to the captured extent. It cannot be converted into CompleteFile or
its physical EOF evidence. Neither type alone establishes journal grammar,
selection/sequence binding, a runtime pin, recovery validity or serving
permission. [Active-prefix frame validation](#validating-captured-active-frames)
uses this completion; active-tail repair remains a separate step. Empty byte
prefixes are expressible at this raw I/O layer; valid journal prefixes must
contain the format header.

### Validating captured active frames

`LockedRoot::open_active_prefix` takes a supplied Selection and ViewIdentity,
an admitted prefix-byte ceiling and caller-owned 1 MiB frame scratch. Before
I/O, require the view's account, epoch, generation, checkpoint sequence and
active segment to match that selection. Check the captured offset includes the
96-byte header and no more than 4 MiB of frame bytes. Its sequence must not
precede the checkpoint; the frame count cannot exceed 8192, and the byte range
must fit the count times the minimum/maximum frame size. An empty prefix has
exactly the checkpoint sequence and header bytes. Insufficient caller byte
admission is InvalidInput; invalid identity/range retains a format error.

Only those active identity fields are validated. ViewIdentity is caller data,
not proof of pin ownership or committed bytes. Its history_floor is outside
this adapter's meaning: complete ReadView acquisition must validate history
retention and own the pins. The caller holds actual prefix ownership or a
quiescent recovery barrier that keeps captured bytes immutable.

Open the selected segment through PrefixReader, with the journal format cap
as its opening physical ceiling. Caller admission covers only the captured
prefix; ordinary suffix growth does not exhaust it. A physical file exceeding
the format cap refuses as I/O InvalidData. Require the checksummed journal
header to match the selected account, epoch, segment and base before returning
ActiveInput. Existing or newly appended suffix bytes
are outside the captured prefix, including an incomplete append.

ActiveInput and HistoryInput share one private frame reader and error type.
The common reader retains header-checksum-before-length, contiguous sequences,
short-recorded-suffix rejection, one borrowed provisional frame per call, and
one shared 64-read allowance for each frame header/remainder. The underlying
input type fixes completion: history still requires physical EOF; active input
requires only the captured prefix present and consumed. Any frame/read error
retires the stream. None marks only the captured end.

Consuming active `finish` checks the completed stream against the supplied
captured sequence and offset through Selection::check_active_prefix. Success
returns CompleteActive retaining CompletePrefix and the journal Summary. No
active-prefix digest is stored in the manifest; per-frame integrity and actual
writer/pin ownership establish the captured byte boundary. This operation
neither validates live row references nor replays transactions, repairs tails,
acquires runtime pins, implements ReadView::next_change or activates service.
Full-prefix recovery/verification reuses an already admitted frame arena; it
adds no per-view MiB reservation.

### Scanning a stopped active journal

`LockedRoot::scan_active_journal` is read-only recovery input. The caller holds
actual stopped-store exclusion with no live readers or writers; LOCK alone
does not serialize threads. Open only the selected active journal as a whole
private file under the admitted byte ceiling and format cap. Require a complete
checksummed 96-byte journal header matching selected account, epoch, segment
and checkpoint base. A short journal header is corruption, never an empty
recoverable journal. Caller ceilings below the header are InvalidInput.

RecoveryInput borrows the existing 1 MiB frame arena. One next_frame call
returns a provisional checked frame, or None at the recorded end or after
consuming an incomplete recorded tail. Before any further frame/tail read,
require a sequence successor, an available operation and capacity for a minimum
frame. Exhaustion refuses even a physically short header. For a remainder
shorter than 64 bytes,
consume those bytes without feeding them to the journal verifier. Otherwise
validate the entire fixed frame header and exact next sequence first. Require
its declared complete size and operation count to fit the remaining segment
budgets, even if its body is incomplete. Consume at most its recorded physical
remainder; only a complete frame is passed to the full verifier. A complete
invalid header/footer, sequence gap or impossible frame is corruption.

Each frame or tail shares 64 explicit reads across header/remainder. Read
failures, exhausted attempt bounds and format failures retire the scanner.
A short physical read relative to the opening extent is an I/O failure, not
repair evidence. The stopped file may not change during scanning. Recovery's
incomplete-tail classification stays separate from live FrameInput, whose
history and active-prefix reads continue to reject short recorded frames.

Consuming `finish` requires the whole recorded extent consumed, unchanged
length and observed physical EOF. ScannedJournal retains the CompleteFile,
summary of only complete frames, and exact valid byte boundary. Its incomplete
flag means physical bytes remain beyond that boundary; those bytes have been
read but have not been mutated or included in the verified prefix digest.
No scanning operation truncates, syncs repaired content, publishes committed
state, validates final row references or grants serving authority. Explicit
repair must use this observed file identity and boundary under exclusive
recovery before complete graph/replay validation and activation.

### Explicit incomplete-tail repair

`ScannedJournal::repair` consumes a completed stopped-journal scan with an
incomplete physical suffix. The caller keeps actual stopped-store exclusion
through scanning and repair, with no live readers or writers. LOCK alone is
not this thread barrier. The scanner retains the selected CURRENT value;
repair re-encodes it and compares all 120 bytes against a fresh, private,
fully consumed CURRENT with observed EOF. A missing, changed or invalid
selector refuses before truncation. This recheck is not complete selected
graph, replay or final-reference validation.

Reopen the generated journal path read/write without creation or automatic
truncation. Apply the same ancestor/private-file policy as input; compare its
device/inode and exact physical length with the retained scanned descriptor.
Reject changed lengths, replacement files or nonprivate links. The trusted
stable-namespace contract is still required: these checks are not an atomic
confinement mechanism or protection against same-size external writes.

Call std File::set_len only with the scanner's verified prefix boundary, then
File::sync_all. No arbitrary offset, writable descriptor or implicit Drop
repair is exposed. This changes an existing inode, so no namespace publication
or parent-directory sync is performed. Require the original retained file to
have exactly the repaired length and return EOF at that boundary before
returning RepairedJournal, which retains its read-only CompleteFile and the
unchanged verified prefix summary.

RepairError identifies the last effect boundary: Rejected means no truncation
was attempted; TruncateAttempted means truncation returned an error and its
effects are uncertain; Truncated means set_len succeeded but sync failed;
Synced means sync succeeded but final extent/EOF confirmation failed. Every
error consumes the scan. Never retry or roll back automatically, and never
resume writer admission after an attempted mutation error until recovery
establishes the file state. Success proves this tail repair only. Complete
selected-graph, replay and live-reference validation still precede pins,
mutation admission and serving. Ordinary fault-injection tests do not qualify
power-loss persistence on any deployment filesystem.

### Expected CURRENT replacement

`CurrentUpdate::prepare` encodes a next CURRENT and either expected absence or
an exact previous CURRENT into fixed buffers. Replacement must retain the same
account and epoch and strictly advance the generation; zero generation and
encoding/hash failures refuse before filesystem effects. Epoch-changing restore
is a separate stopped-store operation. This intent grants no graph validity,
quota, checkpoint barrier or commit authority. The caller must validate and
sync the entire selected graph and its ancestors before using it, and serialize
all mutations while holding the actual writer/view barrier.

`LockedRoot::replace_current` checks private metadata parents, then checks the
expected selector before creating output. Initialization requires absence;
replacement requires a same-owner mode-0600 regular single-link file of exactly
120 bytes. Compare opened identity, read the complete bounded extent plus EOF,
and require exact equality with the encoded expected selector. Reads make at
most 64 explicit extent calls plus one EOF call; a blocking std call remains
outside that scheduling bound. Missing, corrupt or stale selectors refuse;
there is no fallback from replacement to initialization. This is an expected-
state check under serialized trusted mutation, not an atomic compare-and-swap
against external writers.

Create `metadata/CURRENT.NNNNNNNNNNNNNNNNNNNN.tmp` exclusively using the typed
positive Number and shared private-file policy. Write the 120-byte selector,
sync the file and its metadata parent, verify source identity/private policy
and a freshly opened source parent matching both retained parents, then `rename`
it over CURRENT and sync that same
metadata directory. Source and target are siblings. The old opened inode stays
readable after replacement, but new lookups use CURRENT. Existing temporary
names refuse without modification; failed attempts are never auto-cleaned.
Recovery must include these same-directory temporary files in orphan accounting.

Apply ADMISSION.md's existing format/job limit for temporary control files:
one outstanding selector temporary per account, at most 120 bytes, within the
checkpoint job's format-overhead allowance. A private failure retains that job
slot and charge until cleanup is proven and its directory synced; choosing a
fresh number is not permission to retry while the old attempt is unresolved.
Startup must reconcile recognized selector temporaries before reopening the
job slot. This bound and accounting coupling remain M05/M08 runtime work; the
low-level replacement helper does not grant a retry or allocate quota itself.

CurrentError preserves effects: Rejected precedes private creation; Create
carries the existing Uncreated/Attempted/Created stages; Private means failure
after temporary creation and before issuing rename; RenameAttempted means a
failed rename may already have replaced CURRENT; Renamed means rename returned
success but directory sync failed. Retain all logical charges. Either rename
error stage stops writer admission until recovery determines the selected graph;
a missing temporary pathname or an I/O error kind cannot establish selection.
Success establishes durable selector replacement only: publish new reader views
and reconcile the writer's reservation ledger before reopening admission.
No MTA listener or service readiness follows from this primitive alone.

Any create/write/flush/sync/publication error must prevent a new acceptance
acknowledgement. Keep already acknowledged state; never delete live mail to
make room. Partial private output stays charged until cleanup. Uncertain journal
or CURRENT publication stops writer admission until recovery establishes the
committed boundary. Disk exhaustion can require operator cleanup before even a
checkpoint or deletion transaction can proceed. A response-spool failure after
commit closes the response; it cannot undo or report failure of known commits.

Test the std adapter with short/failing I/O, full-disk/quota/read-only errors,
failed file and parent-directory sync, non-replacing publication, competing
writer processes and process death. Deterministic injection and temporary
folders are normal development tests. Disposable filesystem/VM power-loss tests
are release durability evidence; record the filesystem/kernel/mount settings
used. XFS tools are not a runtime or ordinary test prerequisite, and installing
any new external test tool still requires approval.

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
generations may remain pinned in the default profile; a checkpoint that would
exceed that count waits or defers admission. Replacing an unpinned current
generation does not add a retired pin. ADMISSION.md also bounds closed journals
independently of advertised history. Read
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
Use a fixed window of at most 128 blob IDs, selected from current inventory
and validated message/upload shard entries. ADMISSION.md fixes its scratch,
work and restart cursor. For each candidate window, stream all authoritative
owning references from current email rows, submission rows and valid upload
leases, marking candidate IDs live. A missing/corrupt reference scan never
proves absence. MIME attachments remain inside their parent immutable message;
assembly finishes before upload leases can be released.

Only after a complete scan under that same exclusive window may an unmarked
candidate be reclaimed. Remove its inventory and expired lease rows in a
durable transaction, then unlink its file and sync the affected directory.
Batches obey both the frame limit and the 100-file work ceiling. A crash may
leave an orphan file but never a live reference to deleted bytes. Candidate
selection also enumerates messages/ and uploads/ so files never registered by
an interrupted publication are eventually found. Report unexpected names/types;
never follow symlinks or remove a file merely because of its age.

Both drain and exclusive phases have finite configured deadlines and I/O work
budgets in ADMISSION.md. On reaching an exclusive-phase limit, finish the current
admitted durable batch, retain safe cursor progress, and resume service. If a
candidate window's proof is incomplete, unlink none of its unproved IDs. The
next window rescans current references after reacquiring exclusivity; a saved
cursor is only a scheduling hint, never a saved liveness result. Progress uses
validated namespace/shard/blob-ID ordering, not a directory-entry offset that
can become invalid after unlink. New IDs before the cursor are visited after
wraparound. Concurrent reclamation remains outside v1.

Generic query/index external sorts still use private tmp/sort/RUN/ directories,
exclusive names and the 64 MiB default quota. Their preallocated buffers,
merge fan-in and overlap charges come from ADMISSION.md. Sort exhaustion is an
explicit failure and cannot authorize deleting a live blob. Abandoned runs are
removed under LOCK at startup, separately from publication/checkpoint files.

Logical reservations include temporary bodies, journal/history, active/retired
checkpoints, backup pins and merge scratch. Count logical bytes and files;
physical free bytes/inodes are unavailable through this adapter. Every
write/sync handles exhaustion errors. Maintenance cannot consume the logical
completion budget held for an admitted commit, but this reserves no physical
blocks or inodes and cannot ensure that commit can finish on a full disk.

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
