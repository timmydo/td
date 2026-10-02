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

`store_fs::BlobSweep` connects final blob-row enumeration to that verifier.
Each advance performs one ReadView next, one private blob open, one bounded
read/hash chunk, or one blob completion. It never reads a whole message into
memory. Copy the typed ID/BlobRow before returning from enumeration, then
reuse the caller's value partition as the chunk buffer. A nonempty unread
blob requires nonempty scratch; zero-length input still opens and completes
through physical EOF and the empty digest. Drop the completed read-only handle
before proceeding to another blob.

Require strict blob-key order, locally valid rows and sequence ceilings under
one captured view. Check identity before/after every phase, with movement taking
precedence over its result. Admit finite row and cumulative declared-byte
allowances before each open, counting only verified blobs and lengths. An
excess row or byte requirement refuses before file I/O for that blob. Exact
budgets still permit enumeration EOF. All errors retire the coordinator;
incomplete/failed state cannot finish and repeated completion still checks
identity without more I/O. Completion requires every enumerated file verified
and blob-table EOF; report captured identity, blob count and byte count.

This verifies the files named by supplied final blob rows, including uploads.
It does not establish completeness of the ReadView, which blobs own live
references, actual pins or authorization. The caller must retain a real
immutable view or stopped-store exclusion and admit each next's full work plus
bounded chunk work and deadline checks. The cooperative LOCK alone does not
provide thread exclusion. Historical journal rows are not enumerated here.

### Owning a stopped store for validation

`store_fs::StoppedStore` consumes the sole LockedRoot. Existing borrowed
writable outputs must end before that transfer. Its public interface exposes
selected metadata loading, captured active-overlay loading and complete table
and history sweeps; it exposes no root, raw handle or mutation operation.
This gives offline validation read-only ownership through td-mta's API while
retaining the same cooperative LOCK. It neither clones nor reacquires the lock.

Selected metadata borrows both the owner and its caller scratch. Inputs and
loaded overlays also borrow the owner. Only consuming `into_locked` restores
mutation access, and Rust requires those borrows to end first. Dropping the
owner closes the existing descriptors and releases the lock. Transitioning
ownership performs no I/O, recovery, allocation, repair or publication.

For adapters reached through this owner, the transfer supplies stopped-store
exclusion through the API under the trusted-path policy. It does not remove
work admission, scratch overwrite, authorization or resource obligations; an
independently supplied overlay still needs its own immutable owner retained.
The existing operator-controlled path/authority assumptions remain: unrelated
filesystem access, privileged writers and noncooperating processes can still change bytes.
A separately decoded Selection is not proof it came from this owner; every
selected input retains its file/identity/digest checks. The selected graph and
final rows still require validation. This is an offline ownership boundary,
not a runtime view pin, serving adapter or store activation decision.

`StoppedStore::validate_files` connects the loaded active overlay with both
complete selected-file sweeps. Refuse an overlay whose retained prefix belongs
to another LockedRoot, even when scalar identities match. Validate retained
history routing against the supplied captured view and admit both table and
history allowances before either sweep opens a file. The active prefix was
already admitted and loaded by its separate bounded operation.

Advance table replay to selected completion, reclaim its original record
buffer, then reuse that buffer while completing all selected history. Preserve
one underlying sweep step per advance; a separate final advance completes the
coordinator. Any error retires the entire coordinator. No partial table/history
success can finish. Consuming finish returns CheckedFiles and the original full
record/change buffers. Repeated completion performs no additional I/O.

CheckedFiles borrows the stopped store, supplied selection and loaded active
overlay, retaining the read-only owner while its evidence is used. It records
exact supplied CURRENT (including manifest digest), captured identity and both
completed sweep summaries. These are the physical files named by that supplied
selection; the coordinator does not reread CURRENT or claim it loaded the
selection itself. Load the selection from this stopped owner for current-store
validation. Cross-row/aggregate/blob invariants, tail repair, orphan accounting
and service activation remain separate. Later validation must keep this owner
and overlay alive; detached scalar summaries alone are not ownership evidence.

`CheckedFiles::read_view` connects checked stopped-store files to ReadView for
offline logical validation. Bind one fixed object kind, starting change cursor,
clock and absolute deadline at construction; retain the CheckedFiles borrow.
Admit a per-table byte ceiling against all selected tables, a per-source byte
ceiling against all selected retained segments and the captured active prefix,
and a positive work-unit count per trait call. Metadata admission is bounded by
11 tables and at most 64 history descriptors. These byte limits are per input,
not cumulative allowances across calls.

Get and next perform the existing full selected-table lookup/ordered scan,
copying into caller key/value buffers only provisionally until complete digest,
extent and EOF checks succeed. Short result capacity reports Capacity. A call
charges one unit for opening, each replay advance and completion. Each replay
step keeps its existing bounded overlay work. Repeated ordered lookups may
rescan a table; callers admit that cost and the absolute deadline limits the
whole view lifetime. No index, whole-table buffer or new arena is introduced.

Change calls advance the retained ChangeScan until its next record, frame
boundary or completion, charging one unit per underlying advance. They require
the exact last returned continuation and the fixed kind. Locating a starting
cursor may read earlier bounded frames; continuing never restarts the scan.
Get/next can reuse record scratch between change calls because retained compact
changes live in their separate slots. Every selected source was already fully
validated under the same stopped owner before this view existed.

Sample the monotonic clock before each work unit and after the final operation.
Expiry, regression or sampling failure prevents a result from escaping, even
when caller output was already written. Clock/deadline failure takes precedence
over the underlying result at that final check. Any returned error retires all
view operations, and retired calls do no additional I/O or clock sampling.
Caller buffers may be changed on error; discard them. A new view can borrow the
same scratch after the failed view is dropped. An absolute deadline cannot
interrupt a blocking std filesystem operation. Runtime view leases, service
activation and complete logical validation remain separate.

### Capturing a stopped journal prefix

`StoppedStore::capture_journal` wraps the existing bounded whole-frame recovery
scanner without exposing its mutation-capable ScannedJournal. Supply selected
metadata, a physical byte ceiling and the caller's 1 MiB frame buffer. For
CURRENT authority, first load that metadata through the same stopped owner.
Construction binds the selected journal header and retained-history floor;
the floor is the first retained descriptor's base, or the checkpoint sequence
when no history is retained.

Each advance consumes at most one complete frame or observed incomplete tail,
returning only sequence/operation counts. No borrowed frame or mutation handle
escapes. End remains provisional: consuming finish requires End and verifies
physical EOF before deriving ViewIdentity's committed sequence and prefix byte
boundary from the scan summary. Any advance error retires the scan, and failed
or unfinished scans cannot finish. Repeated End does no I/O; final size/EOF
changes still refuse. The caller admits each whole-frame step and completion,
including surrounding deadline checks; this wrapper adds no clock policy.

CapturedJournal retains the stopped owner, selection and scanned read-only file.
It reports physical bytes and incomplete-tail status without truncating anything.
Its load_overlay method reopens only the captured valid prefix into the caller's
replay storage, using the same owner/selection/identity and comparing the full
prefix digest with the scan summary. Loading needs at least committed_offset
minus the 96-byte journal header in the existing 4 MiB replay arena. The scan's
1 MiB frame scratch is released at finish; it suffices for the replay only when
the captured frame bytes fit. The scan descriptor and new prefix descriptor
briefly coexist; drop CapturedJournal after handoff when its report is no
longer needed. No repair method or raw ScannedJournal escapes.
Table/history/blob/reference validation still follows before any service
activation, and incomplete tails still require separately authorized repair.

### Verifying a stopped account

`StoppedStore::verify_account` now performs the complete implemented offline
verification path for one account: read FORMAT and actual CURRENT/manifest,
capture the active complete prefix, load its digest-bound overlay, validate
all selected tables/history, then direct references, recipient queue states,
mailbox parent chains and every final blob's length/hash/EOF. No caller-supplied
selection or committed boundary can substitute for CURRENT in this entry point.
It uses the existing stable private namespace and cooperative lock contract.

VerifyLimits carries the physical journal ceiling, file/replay and read-work
limits, data row/parent/blob limits and one absolute deadline. Each phase admits
its own limits before its work; a malformed later-phase limit can follow earlier
I/O. Every constructor,
advance and consuming completion has an outer clock check before and after its
work. The nested data reader uses the same clock wrapper and monotonic high
watermark, so clock regression across phases also refuses. A late outer clock
error takes precedence over an operation error and reports Policy without phase
context, discarding that operation error. Deadline checks cannot interrupt
blocking std I/O; full-frame capture and overlay loading remain bounded but
indivisible work units. The offline reader still scans a selected table per
lookup; this entry point makes no query-performance claim.

VerifyScratch borrows the existing metadata, 1 MiB recovery frame, up to 4 MiB
replay frames/8192 overlay cells, operation record, change slots and key/value
partitions. Physical validation transfers record/change scratch to the data
reader. Capture's descriptor is dropped after overlay handoff; all remaining
read descriptors close before return. Other failures retain their
phase/source. Failure never yields a partial report and may overwrite scratch.
It performs no repair or
publication. Incomplete active tail bytes remain unchanged and are reported.

VerifiedAccount contains the selected CURRENT, derived identity, journal byte
and tail report, and physical/data completion counts. It borrows only the stopped
owner, so all scratch can be reused while the report keeps mutation access
unavailable. The scalar getters are inspection summaries, not runtime leases.
This proves the implemented structural and data checks only: complete mutation
policy, recovery accounting/orphan handling, authorized tail repair, runtime
leases and service activation remain separate. The planned store verify command
and JSON interface are not implemented by this library entry point.

### Validating final references and blob data

`CheckedFiles::validate_data` borrows one ValidationView and the same stopped
owner. Run the existing direct-reference, recipient, mailbox and blob sweeps
in that order. Each advance performs one underlying sweep step, with the
supplied key/value partitions reused throughout. The blob phase reuses value
storage for chunks. All references and files belong to the captured identity;
the complete physical-file check precedes this pass. This checks the direct
owning references, recipient coverage/current queue consistency, rooted mailbox
parent chains, and every final blob row's private file/digest/extent/EOF.

Before constructing the reader, admit the physically counted total final rows
against DataLimits.rows. That finite allowance also bounds each later sweep;
it is not a cumulative physical-read budget. Separate total parent-get and
blob-byte ceilings apply. ValidationReadRequest still supplies per-call work,
per-input table/source byte ceilings and one absolute monotonic deadline.
Reference expiry uses the UTC value from the view's first clock sample. It is
one captured observation, not a new authorization or a moving expiry cutoff.
Full-table get/next scans remain a bounded fallback, not a performance claim.

Check the deadline around every sweep step, including blob opens, chunks and
completion that do not call ReadView. A retired view already checked its final
result and is not sampled again; preserve that nested failure. Otherwise a
late clock/deadline failure takes precedence over the step result. Any error
retires the coordinator; later calls do no clock or I/O work. Repeated
successful completion checks the deadline without I/O. Consuming finish
also checks it,
so an expired completed coordinator cannot return success. Blocking std calls
still cannot be interrupted.

Completion requires all four sweep evidences. Compare direct-reference counts
for all eleven tables against physical replay counts, then compare recipient,
mailbox and blob totals with their respective reference counts. CheckedData
retains those evidences and the CheckedFiles borrow; its lifetime keeps stopped
ownership while the separate reader scratch lifetime ends. Caller record/change
scratch can therefore be reused while inspecting the result. Incomplete/error
state cannot produce CheckedData. No repair, cleanup, mutation or activation
is performed. Mailbox configuration/mutation policy, transaction history and
worker fences, tail repair, orphan/quota accounting, runtime leases and service
authorization remain separate obligations.

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

`LockedRoot::open_history_changes` provides the same selected immutable-
history completion checks using separate caller CHANGE cells. Both history
openers share selected-file admission and expected header identity
construction. The changes input validates the exact file size, admitted
ceiling and journal header before returning. `advance_frame` borrows
MAX_RECORD_BYTES operation scratch for that call only, discards the previous
frame, recovers the original full slot capacity and reads one complete
frame. `frame` exposes its checked compact changes until the next advance;
false clears that result and marks only recorded extent exhaustion. The
operation buffer is free for row lookups while changes are drained. Caller
code decides whether it has consumed a prior frame before advancing. This is
not ReadView::next_change.

Each frame checks the 64-byte header and aggregate limits, and requires its
whole declared extent to fit the recorded remainder before operation reads.
Read each exact 12-byte operation prefix, bound its extent by remaining payload
and operation scratch, then read/validate its remainder. Require zero remaining
payload before reading the 40-byte footer. Header, operation reads and footer
share 8258 explicit attempts (two per maximum operation, two fixed parts and
64 additional attempts shared by normal 64 KiB splits and short reads); opening
retains a separate 64-call allowance.
The existing helper never retries errors. Each std read retains the shared
64 KiB step cap; no whole-frame allocation or read-ahead inventory is created.
Blocking calls remain uninterruptible by these counters; callers meter work
between complete bounded frames.

A short slot buffer is a resource refusal, not corrupt bytes. All frame/I/O/
capacity failures retire the adapter and clear previous results. Failed input
cannot finish or resume. Finish requires the whole journal Summary, unchanged
physical extent/EOF and selected descriptor binding, returning CompleteHistoryChanges.
Frames remain provisional until that selected proof plus graph/final-view and
actual pin checks hold. This helper supplies no live serving cursor or repair
authority. Its record input and 96 KiB change slots reuse the distinct view
reservations in RESOURCES.md; no per-view MiB is added.

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

`LockedRoot::open_active_changes` applies the same selected active-field,
range and admitted-byte checks through a shared active-file opener, with
compact CHANGE cells and per-call
MAX_RECORD_BYTES operation scratch. The operation buffer is available to row
lookups while checked changes are retained. It shares the private operation reader with
HistoryChangesInput: identical header/operation/extent checks, slot reuse,
8258-call frame budget and terminal failure rules. `frame` exposes checked
provisional changes; `advance_frame` discards them before reading the next frame.
Only the captured byte range is read, even when a complete or incomplete suffix
exists at open or is appended later. A captured partial frame always fails.

Consuming completion returns CompleteActiveChanges only after the captured
prefix is consumed, remains physically present and matches the supplied
sequence/offset. It retains a CompletePrefix, never converts to whole-file EOF
proof and never hashes later appended bytes. A supplied endpoint with the wrong
sequence fails completion even if each frame validates. This is provisional
verification input: no frame becomes serving data before selected completion,
final-view validation and actual pin/barrier checks. Retained history-floor and
live next_change policy remain separate; no extra frame arena is reserved.

`ChangeRoute` binds selected metadata to supplied active ViewIdentity fields
using the same active-range validator. It rejects an inverted retained floor/end
and requires contiguous selected history coverage from the floor through the
checkpoint whenever the floor precedes that checkpoint. The decoded manifest
already guarantees ordered, contiguous history segments ending at the checkpoint;
the route checks that its first retained base reaches the requested floor.
Missing coverage is HistoryLost, never an empty successful history result.

`source` requires the identical full captured identity and a requested frame
sequence strictly after the floor and no later than the committed endpoint.
A sequence through the checkpoint maps to its selected history descriptor index;
a later sequence maps to Active. History bases are exclusive and through values
inclusive. At most 64 selected descriptors are examined; no directory scan,
filesystem call or new collection is used. The immutable lookup reports Conflict
on a changed view, HistoryLost below/at the floor and Invalid beyond the endpoint.
It grants no file-open authority or proof of physical availability, journal
integrity, final-view validity or real pins. Locating bytes within the selected
segment retains its own work budget; this helper supplies only a source choice.

`LockedRoot::open_changes_at` combines the selected route with the matching
history or active change reader for one target sequence and one source segment.
Validate route/view/target before opening; preserve that reader's caller-byte
admission and whole-file or captured-prefix checks. One `advance` borrows record
scratch, checks the exact captured identity, discards the prior frame and reads
at most one bounded complete frame. Until it reaches the target, it returns
Locating with the checked sequence and hides the frame's changes. Locating is
internal work progress; it cannot advance a user cursor or authorize a page.
Reading earlier frames, including those before the retained floor, is needed
for the selected segment's integrity and exposes no earlier changes.

At the target and subsequent frames in that segment, return Frame and expose
only checked provisional changes. The caller drains them before advancing.
Refuse a sequence beyond the selected segment/captured endpoint before exposing
it. End requires the target reached and the selected final sequence seen, but
is not a completion proof. All advance errors retire ChangeInput and hide its
frame; subsequent advance/finish returns Failed. Underlying I/O/format errors
remain Input errors; policy/view errors keep their Policy classification.

Consuming finish still requires selected history digest/physical EOF or exact
captured active-prefix completion. It returns a typed History/Active completion
and the original full mutable CHANGE-slot capacity. The underlying history and
active readers also expose finish_reuse, with their existing finish APIs
retaining the prior result type. Scratch is returned only after all completion
checks succeed, whether the final frame is still present or End cleared it.
An empty active input can likewise reclaim its untouched cells. No new arena or
per-frame allocation is introduced. This locates within one selected segment.
ChangeScan below coordinates transitions; live pin ownership, full final-view
validation and serving remain driver work. Every location step needs its own admitted full-frame work
unit/deadline check; blocking std calls retain their existing limitation.

`LockedRoot::change_scan` joins the cursor and locator for one object kind,
captured view and starting cursor. Construction validates metadata only. Each
advance first checks exact view identity and caller continuation, then performs
one bounded phase: open a selected source, read one frame, drain its retained
changes, or verify/close a consumed source. Progress reports internal work and
leaves the caller cursor unchanged. Record/Advanced preserve the cursor helper's
stored ordinals, filtering and explicit whole-frame boundaries. The current
frame is drained before the next read; locating cannot change the user cursor.

A source must pass selected completion before its cells transfer to another
source. Drop its completion descriptor before opening the next file. All errors
retire the entire scan, including opening, cursor, reading and final digest/EOF
errors. Complete is returned only after reaching the captured endpoint boundary
and verifying/closing every opened source. Consuming finish then returns the
original full scratch capacity. An initially exhausted range opens no file;
completion covers only sources actually consumed. The scan does not validate
unvisited history, an unused empty active prefix, the whole graph or live pins.

Records and boundaries remain provisional. Publication requires the surrounding
driver's selected-file/graph validation under actual immutable pins; this scan
cannot establish those requirements or authorize early JMAP pages. The caller
admits each work phase and its deadline; max_bytes caps each opened source,
not total scan work. No record scratch survives advance.

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

### Bounded frame append primitive

`ScannedJournal::append_frame` now consumes a completed physical scan with no
incomplete tail and borrows one immutable, complete candidate frame. An incomplete
tail refuses with InvalidInput (repair and rescan first), not a corruption error.
It validates
that frame's successor sequence, local operations and checksums, then admits the
combined 4 MiB frame-byte and 8192-operation ceilings. Before opening for append,
it rechecks selected CURRENT using the same bounded helper as explicit tail
repair. Opening uses std OpenOptions with read/append and the existing private
file policy; exact length and inode identity must match the scanned descriptor.
Constructor refusal appends no bytes and returns Rejected. A repaired tail must
be rescanned before using this entry point.

JournalAppend holds the writer-lock borrow, append descriptor, selected CURRENT,
frame borrow and fixed counters. The caller keeps actual stopped-store exclusion
from scan through append completion, with no live readers or writers. LOCK alone
is not that barrier. The stable private namespace remains required: reopen checks
do not protect against same-size external writes. The caller also owns logical
reservation and full final-view/blob validation. This
primitive grants none of those authorities. Each advance performs one write of
at most 64 KiB, a file sync, or final length/EOF confirmation. At most 64 write
calls are admitted for the complete frame; excessive short writes refuse with
WouldBlock. Zero writes, impossible counts, Interrupted and other I/O errors
retire the append. Retried advances on a failed state do no I/O.

Every advance error reports Indeterminate; even an apparently early write error
must not authorize retry, rollback or release of the reservation. Written byte
counts report confirmed progress only; a failing syscall may have additional
uncertain effects. The caller stops writer admission and preserves charges for
recovery. Dropping an unfinished append neither syncs nor truncates any bytes.
Premature finish returns Incomplete, and failed finish returns Failed. Both retain
the same uncertain-effect contract: stop the writer and keep charges until
recovery. Even Incomplete after successful sync cannot authorize acknowledgment
or reservation release. Only constructor Rejected proves no append writes. Caller
work/deadline checks bracket construction, every advance and finish; blocking
std calls remain uninterruptible and frame validation is one admitted work unit.

Only the sequence Write -> Sync -> Confirm -> Complete permits consuming finish
to return SyncedAppend. Repeated Complete does no I/O. That evidence retains the
lock/file and reports CURRENT, sequence, byte endpoint, appended bytes/operations
and cumulative operations. It is durable append evidence only; atomic reader
visibility, reservation reconciliation and protocol acknowledgment remain for
the writer coordinator. File namespace is unchanged, so no directory sync is
needed for the append itself. This foundation accepts one frame per fresh scan;
a retained multi-commit writer and publication are not implemented here.

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

`overlay::Overlay::decode` implements the supplied-byte replay index. Its
inputs are a separate 96-byte journal header, at most 4 MiB of immutable frame
bytes and caller-owned cells capped at 8192. It validates every contiguous
complete frame and operation through the shared journal verifier, records
checked key/value offsets with sequence and in-frame ordinal, and sorts cells
in place without allocating. Every stored operation consumes a cell, including
CHANGE and repeated updates to one key. Partial/corrupt supplied frames refuse;
this adapter performs no incomplete-tail repair. Frame-header and truncated
frame errors retain the shared verifier's Frame classification.

The returned overlay borrows both buffers and exposes the journal Summary for
selected-prefix binding. get returns the latest operation on a key; None means
no overlay entry, while DELETE is an explicit tombstone. next returns latest
operations in canonical key order, strictly after the caller's validated cursor,
including tombstones. CHANGE descriptors are sorted apart and never appear as
rows. Results preserve sequence and ordinal. This validates supplied bytes and
replay order only: the caller must separately establish physical extent,
selection, complete final-row invariants and actual view pins.

Construction synchronously bounds work by the supplied frame bytes and
operation count, then sorts at most 8192 descriptors. The coordinator admits
that entire work unit and checks its deadline around construction. Failed
construction may overwrite scratch cells and exposes no partial overlay. After
byte/cell admission, reuse clears cells before parsing. No whole checkpoint or mailbox map is constructed.

`LockedRoot::load_active_overlay` connects the replay index to private prefix
input. Before opening, validate the supplied view's selected active identity,
sequence/offset bounds, explicit admitted byte ceiling, frame capacity, cell
ceiling and one-cell-per-frame lower bound. Exact cell sufficiency is established
during decoding; every stored operation needs a slot. All undersized/oversized
caller buffers return Io(InvalidInput), distinct from malformed journal bytes.
Read and bind the 96-byte header before any frame bytes. Fill only the captured
frame extent in the caller arena, sharing at most 128 explicit read attempts
across header and payload (each at most 64 KiB). Short reads consume attempts;
errors, exhaustion or an unavailable captured prefix refuse with no returned
partial overlay. No automatic retry or incomplete-tail repair occurs.

After prefix consumption, require its extent still exists, decode/sort the
arena, then bind the complete summary to the captured sequence and offset.
Later suffix appends remain outside the view and do not require physical EOF.
LoadedOverlay retains the CompletePrefix and supplied ViewIdentity while
borrowing frame/cell storage; no extra frame arena is allocated. This data does
not create a runtime pin or validate history_floor. The caller supplies actual
pin/barrier ownership, stable namespace, admission for the entire bounded
read/hash/sort operation and deadline checks around it. Blocking std I/O is not
interruptible by that deadline. Full selected-graph and final-row/reference
validation remain separate prerequisites to serving.

Read an object from that bounded overlay or its sorted checkpoint table.
Listings merge a sequential table cursor with sorted overlay entries.
`merge::Merge` implements a provisional supplied-record merge for one table.
Its constructor validates table-header structure and binds account/epoch and
checkpoint sequence to the overlay's journal base. Push accepts one locally
validated checkpoint Record, checks ascending unique keys, table identity,
last-change ceiling and declared count/payload bounds, then emits intervening
latest overlay PUTs to a synchronous borrowed-row callback. DELETE emits no row;
a matching overlay key replaces or suppresses its checkpoint row. Unchanged
records retain their last-change sequence; replacements use the frame sequence.
No whole-table collection or per-row allocation is constructed. Its input
checks intentionally repeat the file verifier's count/extent/order/sequence
checks because a locally valid Record can be constructed without that verifier.
Both classify count/payload overflow as TrailingBytes. This duplication does
not replace file hashing or selected-table completion.

Finish requires exactly the declared checkpoint count/payload, drains remaining
overlay keys and returns the live row count. This does not prove table checksum,
physical EOF, selected generation or final references. The caller feeds every
checkpoint record in order, completes and binds that table input, and keeps all
callback effects provisional until complete final-view validation succeeds.
A callback can have produced earlier rows before a later error; no rollback is
promised. Input and callback errors permanently retire push; finish consumes the
merge. Any staging output is abandoned through the coordinator's normal orphan
handling. Admit one checkpoint record plus at most 8192 intervening overlay keys
per push, or at most 8192 remaining keys for finish; callbacks retain their own
bounded I/O and deadline accounting. Check deadlines around that entire work
unit. This is not yet a serving ReadView or checkpoint publication mechanism.

`TableInput::into_replay` connects a fresh selected table input to LoadedOverlay.
It refuses consumed input as InvalidInput and failed input as BrokenPipe, then
binds the supplied view's
active identity/generation/checkpoint to the table's Selection. TableReplay
retains that input, its fixed Merge state and a borrow of the loaded prefix.
Advance reads one checkpoint record and performs its bounded merge/callback
work; false establishes declared record exhaustion only. An input or callback
failure retires the replay, and subsequent advance/finish refuses.

Finish first requires the complete table digest, physical EOF and selected
manifest binding. Only then does it drain residual overlay rows and return
CompleteReplay with the CompleteTable, loaded-prefix borrow and live row count.
Earlier callbacks remain provisional if any later input, completion or sink
operation fails. This binds one supplied selected table and captured prefix;
actual view pins/barriers, all-table and final-reference validation, and staged
output durability/publication still belong to the coordinator. No ReadView,
transaction or recovery activation is granted by completion.

`TableInput::finish_reuse` returns the original record buffer only after the
same selected digest, exact extent and EOF checks as finish. TableReplay's
matching method additionally requires residual overlay sink work to succeed;
its callbacks remain provisional until that completion. Existing finish methods
use those same paths and discard the returned scratch borrow.

`store_fs::TableSweep` validates/replays all 11 selected checkpoint tables
against one supplied LoadedOverlay. Constructor checks selected active identity
and admits the checked sum of all descriptor file lengths before any table I/O.
Each advance opens one table, performs one replay advance, or completes one
selected table and its remaining overlay rows. Declared table exhaustion alone
is not verification. Only after digest/EOF and replay completion may it drop the
completed file and transfer the one original record buffer to the next table.

A finite total-row allowance counts final rows emitted by all merges, including
residual overlay rows, rather than checkpoint input rows. Deletions may yield
zero final rows despite nonempty checkpoint input. These are admission bounds,
not physical-call or elapsed-time bounds: one replay advance/completion can
process the existing bounded set of intervening/residual overlay operations.
The caller admits that full work and checks deadlines between advances.

Errors retire the sweep and cannot yield completion or reusable-scratch evidence.
After every table verified, a separate advance reports Complete; repeat completion
performs no I/O. Consuming finish returns CompleteTables and the original record
buffer. Evidence carries the loaded identity, per-table final-row counts, total
rows and selected table bytes. It proves all selected checkpoint files and their
replay against the supplied captured overlay; retained history, direct references,
blobs, aggregate rules, actual pins and publication remain separate obligations.
The caller retains immutable view or stopped-store exclusion throughout.

`store_fs::HistorySweep` verifies every retained immutable history descriptor
in the selected manifest, including frames below any client history cursor.
Admit checked sums of descriptor bytes and sequence-range frame counts before
opening files. Each advance opens one segment, reads one bounded complete frame,
or finishes one selected segment through digest/extent/EOF checks. Frame and
exhaustion steps report provisional progress, never selected completion. Reject
any remaining bytes at completion after reaching the selected through sequence;
never decode a further frame. The existing reader enforces contiguous frames
from the selected base and exact final sequence/extent. A short change-slot
slice returns ChangeCapacity when a frame cannot fit, not a corruption result.
Smaller caller slices are allowed; reserving MAX_FRAME_OPERATIONS slots avoids
this capacity refusal for format-valid frames.

Drop each completed file before transferring the original full change-slot
slice to the next segment; reuse caller record scratch for every frame. No
whole journal is buffered. All failures retire the sweep; incomplete/failed
state cannot finish. Completion requires every descriptor verified and the
observed frame count equal to the admitted descriptor sum. Empty retained
history needs zero byte/frame allowance and no file I/O. Consuming finish
returns CompleteHistorySweep and the full original slot slice; evidence records
the selected CURRENT (including manifest digest), checkpoint, segment/frame
counts and bytes. Repeated completion performs no I/O.

The caller retains real immutable ownership or stopped-store exclusion and
admits the existing operation reader's bounded full-frame work and deadlines.
A frame step is not one physical read. Selected tables, active-prefix recovery,
final rows/references/blobs, runtime pins and activation remain separate.

`TableInput::into_lookup` validates the requested key and table, then creates
a selected TableReplay with a borrowed target key and separate caller result
buffer. Each advance performs one replay step and copies only the matching
row value and last-change sequence. No match or absence is exposed until
finish validates the complete selected table and drains residual overlay rows.
CompleteLookup retains CompleteReplay and exposes a borrowed final row or
absence for that table/prefix. The row borrows caller result storage, so it may
outlive the completion wrapper; into_parts can transfer the proof and row
separately. Neither accessor creates a pin or prolongs one beyond its owner.
It does not validate cross-row references or
create a live view pin. Result bytes on an error remain provisional scratch.

Every lookup scans the whole selected table; it cannot stop at a candidate or
assume absence when the remaining input fails. The caller admits the table's
byte/record work and checks deadlines between steps and around finish. An
undersized result buffer fails with OutputFull when the matching row is copied;
absent and zero-byte row values need no result capacity. Such failure retires
the replay. RESOURCES.md reserves distinct maximum record and result buffers;
there is no allocation or whole-table inventory in lookup. This provides the
bounded scan fallback for final-reference validation; indexing and whole-graph
recovery coordination remain separate work.

`TableInput::into_next` provides the ordered scan fallback. None requests the
first final row; a supplied cursor must decode as a locally valid key for the
selected table. Compare canonical encoded bytes and retain only the first row
strictly greater than the cursor, with its last-change sequence. Replacements
and tombstones are handled by the same replay. Continue reading and validating
after capturing that candidate; exhaustion is successful only after complete
selected-table and residual-overlay validation. Invalid cursors and insufficient
key/value output refuse explicitly; callback errors retire the replay.

CompleteNext exposes the resulting Record or exhaustion and retains the replay
proof. Key and value borrow distinct caller output slices and may outlive the
wrapper; into_parts transfers the proof and row separately without granting a
pin. Both outputs remain provisional on error. One next operation scans the
entire table, with the same per-step/finish admission and deadline contract as
lookup. The next call must open fresh input with the same actual pinned view;
this adapter does not retain a file position or instantiate that view. Indexed
iteration and complete recovery/reference coordination remain separate work.

Sparse key/offset indexes and folder/date/search indexes are disposable disk files
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
