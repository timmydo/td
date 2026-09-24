# Store and runtime boundaries

This normative M02c2 contract accompanies the compiling interfaces in
src/ports.rs and the state codecs in src/sync.rs. Interfaces do not supply a
store, network runtime, cryptographic provider or allocation/durability proof.
M05/M07–M09 implement and test the adapters; M13–M17 implement their protocol
consumers. RESOURCES.md fixes worker/buffer ownership; ADMISSION.md fixes
disk/work ledgers and exact request retention before consumers.

## 1. Ownership and I/O

Handles lease startup pools and release capacity on Drop; they cannot grow
collections per operation. Errors are fixed typed values, with OS kind/code
where relevant, never dynamically formatted diagnostics. Implementations may
use cold allocations only where RESOURCES.md budgets them. No trait call
authorizes access merely because it accepts a typed ID. The coordinator
checks account/device authorization and rechecks it at the operation boundary.

Clock returns UTC plus monotonic boot-local milliseconds. Deadline uses the
latter, checked addition and expiry at now >= deadline. Never persist Tick
or compare it across boots. Entropy fills the entire output or fails; callers
discard the entire output on failure. Crypto SHA-256 is streaming. P-256
keys are PKCS#8, public points uncompressed SEC1, ES256 signatures fixed
32-byte r followed by 32-byte s. The provider owns secure key storage;
generation/load are cold operations. equal_digest uses the reviewed provider
for constant-time equality of fixed 32-byte password verifiers; core Rust
comparison is not a substitute. TLS owns peer certificate verification.
Neither a fake digest nor these trait signatures prove cryptographic quality.

Transport methods are nonblocking and make bounded progress. Bytes(n) means
1 <= n <= supplied slice length; an empty slice returns Pending. Pending
does not consume bytes, and Closed on read is EOF. A failed/closed write is
an Error. flush drains all adapter-owned output, including TLS records,
through Pending to Complete; completion does not prove peer receipt. Drivers
flush before waiting for a reply and continue progress within the deadline.
close progresses close_notify through Pending/Complete and is idempotent
after completion. abort immediately fences I/O, closes the socket and discards
buffered output; failed/canceled attempts use abort. Caller buffers are not
retained after a call. TLS can buffer consumed plaintext, so queue
phase fences precede write calls, not socket flushes. Handshake Pending owns
one bounded handshake slot until completion/failure/deadline. No application
bytes are consumed before handshake success. TLS info remains tied to that
connection and verified policy: None for ordinary inbound TLS, ServerName
only after chain/time/name validation, Gateway only after the configured
gateway certificate identity check. Gateway bytes are SHA-256 of leaf DER;
authorization must also match configured gateway policy. A certificate alone
never enables arbitrary SMTP relay. Endpoint creation/configuration is a
cold runtime concern, not caller-supplied network input to these traits.

TlsFactory::upgrade consumes a plain transport under a cold-registered policy
generation/index, leases an existing handshake slot, and returns a handshaking
transport. It is used immediately for implicit TLS and after the exact
STARTTLS exchange for SMTP. Before replying 220, reserve the upgrade capacity;
flush that reply completely before the handoff. Reject/abort if any plaintext
bytes remain after the STARTTLS command or outbound 220 response. Never carry
pipelined plaintext into TLS parsing or discard a tail and continue. After
handshake success reset the SMTP parser, EHLO/extensions and authentication
state. Refusal/timeout aborts the consumed connection with no downgrade.
The supplied policy must still be a retained, authorized generation. These
operations reuse the same transport slot and add no unbudgeted connection.

Resolver runs on the one fixed resolver worker, accepts a configured hostname
and an absolute deadline, and writes addresses into caller capacity. Success
has count > 0 and <= output.len(), plus monotonic TTL expiry. Oversized answer
sets fail Capacity; no partial successful address list or allocating NSS
fallback. Implementations must stop at the deadline and validate DNS source,
question, name/compression limits and CNAME chains (M09). This port resolves
configured A/AAAA endpoints only; MX/TXT/policy resolution is outside v1.

## 2. Read views and change history

ReadView pins account/epoch, checkpoint generation and sequence, active segment,
exact committed byte prefix and committed sequence, and retained history floor.
It never observes later commits. Pins include history needed by next_change;
if unavailable, acquisition/iteration returns HistoryLost, never a partial
successful change page. Deadline/pool expiry invalidates further operations.
history_floor is the last sequence whose frame need not remain: all frames
strictly after it through the committed endpoint are retained. Thus a since
state equal to the floor is valid. GC/checkpoint retention obeys those pins.
Detached Record values borrow only
caller key/value storage; holding them does not keep a view or blob alive.

get and next use shared row/key semantic validation and verify checksums before
exposing bytes. Buffer shortage returns Capacity, not a truncated record.
next visits strictly increasing encoded keys in the specified table, with
None meaning exhaustion; after=None starts at the first key. Validate a supplied
after key for that table. next_change scans complete retained frames after its
cursor through the pinned committed sequence. It returns matching CHANGEs in
(sequence, operation ordinal) order, skipping PUT bytes with bounded I/O.
Read action from operation-header byte 1 (FORMAT.md); the empty CHANGE value
does not remove its created/updated/destroyed header action.
Operation ordinal is zero-based across all frame operations; u32::MAX denotes
the end of its sequence. A cursor below history_floor gives HistoryLost, above
the pinned end gives Invalid. Identity is not a journal-backed change kind.
No frame-sized allocation or arbitrary caller cursor may cross the pinned end.
Each call returns Record for one matching CHANGE, Advanced at the next
complete frame boundary (even with no matching changes), or Complete only
at the pinned endpoint. Stop scanning at that first record/boundary; a call
reads no more than one bounded frame after locating its cursor. The view
retains its streaming cursor so repeated calls do not rescan earlier frames.
After Advanced, the caller resumes at (through, u32::MAX). The coordinator
can publish a bounded advancing empty page or a prior fitting boundary when
the next frame exceeds its remaining budget; it never skips unseen changes.
A below-floor or invalid cursor fails before scanning, and deadline expiry
is an error, not Complete. Locating an initial cursor also has a work budget.

BlobReader is an already authorized, opened, immutable file with a live owner
pin held until it closes. Store::open_blob requires Access and borrows the
view, retaining that ownership until the reader drops. It checks live message/
submission references or an authorized unexpired upload lease before opening
the root blob; a known BlobId alone grants nothing. Opening private paths
belongs to M05; protocol handlers do not open files directly. read_at uses
checked offsets,
returns at most output.len() bytes, zero only at EOF or for empty output,
and refuses offsets beyond len. A positive short read is legal. The caller
loops within its work budget. Missing/truncated committed bodies are Corrupt.
Part locators additionally require WIRE.md's authorization/descriptor checks.

## 3. Reserve, publish, commit

Store::reserve runs before accepting a body or mutation responsibility.
Access carries the account, trusted principal and configuration generation.
It is constructed by the coordinator after authentication/admission, never by
deserializing a client field. Recheck principal permissions/device revocation
and the selected configuration at each access and at commit. SMTP, queue and
maintenance principals have only their named operation rights. A stale
configuration generation causes reauthorization against the selected snapshot;
the coordinator can refresh Access without releasing the reservation, its
body quota or pins. Generation mismatch alone is not device revocation. An
actually revoked device cannot commit a device-initiated mutation.

Queue outcome recording belongs to the durable attempt/fence that authorized
the transmission. A later route credential/identity configuration change may
stop new dispatch, but cannot revoke the queue principal's right to record
that already-authorized attempt's final result using its held reservation.
Refresh Access for that narrow recording operation; do not rerun sender
authorization as though it were a new submission. New dispatch uses the new
configuration. This also applies to recovery of the prior attempt.

The account/size/deadline lease has a ReservationId containing coordinator
instance ID, slot and checked generation; a different instance or expired/
reused slot is rejected even if it has the same Rust type.
Include every PUT, DELETE and retained CHANGE in frame bytes/operation count.
Capacity is global across reservations and survives checkpoint transfer under
STORAGE.md's barrier. No unaccounted frame/operation overrun may reach disk.
DiskBudget separately reserves physical new-blob bytes/files, metadata
bytes/files and logical upload/queue quota increases (including a new pin on
an existing body). Logical quotas may overlap physical bytes and are not
added to them a second time. When size is unknown reserve the admitted maximum.
ADMISSION.md fixes quota ceilings and checkpoint/free-space headroom; no blob creation
may bypass this reservation. Exceeding a logical quota returns Quota; temporary
pool/storage pressure returns Capacity/Busy. Dropping or expiring releases
unused capacity once; already-written orphan bytes remain charged until cleanup.
DiskBudget covers store operations. Request retention, sort/cache runs, logs
and cold-state files have separate typed leases from the same filesystem
admission coordinator; they cannot bypass its aggregate completion reserves.

Store::begin_blob borrows an active reservation exclusively and charges its
physical quota before creating the private file. BlobWriter writes whole
chunks or fails; after error the entire writer is poisoned and must be
dropped, never retried with the same chunk. publish
consumes it and returns only after file and publication-directory sync.
PublishedBlob has read-only accessors for reservation/account/id/length/digest;
only core persistence constructs it, and it is neither Clone nor Copy. It is
bound to the same still-active reservation that pins its pending blob against
GC. It is not an authorization credential. M05 owns root paths, publication
checks and orphan cleanup. A publication error may leave an orphan but cannot
create metadata. Dropping a writer ends its borrow, not the reservation.

M02c3a refines transaction handoff to TransactionInput: one immutable encoded
byte slice plus operation count, using all FORMAT.md PUT/DELETE/CHANGE
operation headers and key/value encodings without a journal frame header.
There is no separately allocated CHANGE slice. Validate every input byte,
tag, count, length and semantic key/row before append; TransactionInput
is not a proof of validity. Owned StagedOperation offsets (bounded to 32 bytes
per slot, including parsed operation kind/type/action) index the input arena.
Each Row/Key is decoded only while used.
Do not store an array of borrowed Mutations beside their mutable backing arena:
that prevents safe buffer reuse across requests. The single-record Mutation
enum remains an encoder input, not a pooled self-referencing object graph.
M05 supplies the operation encoder/parser; this increment only fixes handoff
and checked startup layout. Frame header/footer bytes must fit in addition
to the supplied operation bytes (which already include CHANGE). Input and
output buffers are distinct. Both arenas reserve `frame_bytes`, but valid
input is at most `frame_bytes - FRAME_HEADER_BYTES - FRAME_FOOTER_BYTES`;
reject a larger operation stream before encoding or append. Unused input
arena capacity does not increase the permitted journal frame size.
The extra input arena is deliberate: protocol-owned canonical input stays
immutable while the store builds and validates its own complete frame. This
keeps the commit interface independent of store-private mutable buffer leases
and gives inspection/fault adapters the same handoff. V1 pays one bounded copy
instead of requiring vectored append or exposing a writer arena to callers.
M05 uses the checked StagedOperation slot representation, or amends its size
contract before substituting a different internal descriptor.

Ports express the thread handoff: transports, digest state, reservations and
blob handles are Send; shared stores, clocks, crypto and read views are Sync.
This does not permit concurrent mutation or erase borrowed lifetimes. Startup
owners outlive scoped workers and their leased handles; queues still carry
slot IDs, never lifetime casts. TLS factories are worker-owned Send values.

Store::commit runs only on the serialized writer worker. Recheck reservation
ownership/deadline, expected account sequence, authorization supplied by the
coordinator through Access, record semantics/references, published-blob
reservation/account/digest/length and matching blob PUTs, and exact encoded size/count before any append.
Every newly referenced blob must be either live in the current view or in
this call's checked published handoff. A handoff alone does not grant access
to an existing blob in another account. Derive/validate CHANGEs from actual
object effects; do not trust protocol callers to omit or forge them. Identity
CHANGE is reserved by the byte registry but rejected in v1 mail transactions.
Repeated keys follow FORMAT.md ordering; changes describe the frame's final
object effect. Never allow a partially valid transaction into the journal.

Effects include derived properties even when their own table row is unchanged:
Email create/destroy changes Thread.emailIds and all affected Mailbox counts;
membership changes update that Email and affected Mailboxes; $seen/$draft
changes update that Email and affected unread counts. V1 chooses RFC 8621
§2's simple mailbox-local unreadThreads definition: count distinct threads
with an email in this mailbox having neither $seen nor $draft. totalThreads
is distinct threads with any member in this mailbox. Compare before/final
counts and emit a Mailbox CHANGE wherever any count or stored property changed.
Emit Thread-created/destroyed when its first/last email is added/removed,
otherwise Thread-updated on emailIds changes. Account for the complete derived
fan-out in the reservation before accepting the transaction. Never omit a
CHANGE to fit a frame. Mailbox/changes.updatedProperties is always null in v1,
because the journal does not track which individual properties changed.

expected is the sequence used to prepare the mutation. A different current
sequence yields Rejected(Conflict) without appending. The coordinator may
replan within its bounded work budget while retaining the same reservation,
including streamed/published body quota and GC pins; ifInState still compares
the client's token against the newly selected view before mutation. A commit returns Ok
only after complete frame sync and durable visibility publication. commit
borrows the reservation mutably. Rejected(Conflict) leaves it active; other
pre-append refusals leave it active unless its deadline/authorization/identity
is invalid. Ok consumes its reserved capacity into committed usage, and
Indeterminate poisons it until recovery. is_active becomes false in either
case; it can never be reused or double-released. An invalid/expired lease
returns unused capacity once and transfers written orphan charges to cleanup.

CommitFailure deliberately separates:

- Rejected: this attempt appended no frame; ordinary object/method error is
  safe if this method has no earlier commits. The active reservation and its
  pending-blob pins survive Conflict; dropping it makes unreferenced blobs
  eligible for orphan cleanup.
- Indeterminate: a partial or complete recoverable frame may have been written.
  Stop all writes and transmitting workers; recover before accepting another
  mutation. Do not fabricate an ordinary failed Set result claiming no effect,
  retry the append or roll back prior successful methods. If the HTTP response
  cannot accurately report the outcome, close the request; reconciliation
  uses committed IDs/history after recovery. Inbound SMTP closes without final
  acceptance, so a sender can retry and a duplicate is possible.

An error after frame durability but before local notification is still an
indeterminate caller outcome. Cancellation/deadline cannot revoke a committed
frame. The contract requires injected failure tests around every boundary;
the enum itself does not implement them.

## 4. Opaque state strings

src/sync.rs encodes into caller storage without allocation. All hex is lower
case, fixed width. Decoders consume exactly the specified length. State strings
are synchronization values, never authentication tokens.

| Kind | Exact encoding |
| --- | --- |
| DataState, 85 ASCII bytes | d1_ + two hex digits of ObjectType tag + 32 account hex + 32 epoch hex + 16 sequence hex (most significant byte first) |
| IdentityState, 131 ASCII bytes | c1_ + 32 account hex + 32 epoch hex + 64 identity snapshot SHA-256 hex |

DataState admits Mailbox=01, Thread=02, Email=03, EmailSubmission=05; not
Identity=04. Its sequence is the account-wide committed sequence of the view.
Thus it changes for every transaction, including unrelated data changes.
This deliberately trades RFC 8620 §5.1's SHOULD for stable unchanged-type
state for a small restart-safe state representation without per-type durable
counters. Empty /changes pages after unrelated changes are valid; a type's
real change can never leave its state unchanged. Restore changes the epoch.

For ifInState, any supplied string not equal to the current scoped token
gives stateMismatch, including an old epoch/type, future sequence, malformed
opaque spelling or a string from another account. A non-string argument is
invalidArguments. For /changes the corresponding unrecognized/out-of-range
token gives cannotCalculateChanges. Valid history range is floor <= sequence
<= current sequence with the same account/epoch/type. Codec range checking
does not acquire a view or prove retained history; the adapter must pin it.

For /changes select one target view, scan only (since, target], and coalesce
each ID by final effect: create then destroy disappears; create then update
is created; update then destroy is destroyed; repeated updates occur once.
No deliberate delete/recreate reuse is allowed. Return IDs disjoint across
created/updated/destroyed and ascending raw ID within each array. Restrict
maxChanges across their combined size. Page only at complete frame boundaries;
choose the latest scanned boundary that fits all effects, pinning that page's
old/new state. If no advancing boundary fits before the work budget expires,
return cannotCalculateChanges. Do not split a transaction or omit excess IDs.
hasMoreChanges means more frames remain through the pinned target; the next
request may see a newer target. An unchanged request at current returns empty
arrays, equal states and false. Retention loss forces explicit resynchronization.

V1 queries return canCalculateChanges=false; /queryChanges returns the standard
cannotCalculateChanges for supported well-formed queries. They do not rerun a
query and pretend its changed ordering is a delta. POLICY.md specifies the
queryState encoding and bounded sort/search semantics; CASES.md names the
wire fixtures. The queryState codec is still owned by M14/M16.

Identity data comes from one atomically selected validated configuration
snapshot, outside the mail journal. Digest preimage is ASCII td-mta-identities-v1
followed by a NUL byte, u32-LE count, then identities sorted by raw ID. Each
identity is raw 16-byte ID, name, email, replyTo, bcc, textSignature,
htmlSignature, mayDelete in that order. Strings are u32-LE UTF-8 byte length
plus bytes, booleans one byte 0/1. Nullable arrays use byte 0 for null or byte
1 + u32-LE count + ordered addresses; each address is nullable name (byte
0, or byte 1 + string) then email string. mayDelete is false in v1. Defaults
are materialized before hashing. Hash every visible property, including
signatures; exclude credentials and unrelated settings. Configuration loader
bounds all counts/text and refuses overflow before digesting/publication.
Unchanged visible identities produce the same digest. Identity/changes with
the exact current token returns an empty change result; any other string
returns cannotCalculateChanges. Identity/set is read-only with normal
preconditions/notFound and forbidden errors. Restore still changes its epoch.
`config::identity` implements the canonical preimage encoder. Its borrowed
`Identity`/`Address` values preserve visible strings and ordered arrays exactly;
the caller materializes defaults and supplies identities in strictly ascending
raw-ID order. Duplicate or descending IDs fail. V1 `mayDelete` is encoded as
false, with no caller override. The codec does not validate mailbox syntax,
authorize From addresses, bind an account, load files or publish a state token.
Empty snapshots are encodable for the empty digest oracle; service-required
identity declarations belong to the full configuration schema.

Representation ceilings are 64 identities, 16 addresses per replyTo/bcc array,
4096 UTF-8 bytes per name, 254 bytes per email, and 16384 bytes per signature.
The configuration line parser still caps decoded strings at 4096 bytes;
SCHEMA.md specifies signature-file references for all configured signatures,
including multiline values, with protected-file validation in M04b3/M05. The complete loader is
still unimplemented; this encoder provides no way around the input grammar.
The total encoded preimage is at most 192 KiB, including prefixes/counts. These
are simultaneous upper bounds, not a guarantee that every maximum combination
fits. Text borrows the snapshot's existing non-routing text region; the encoder
reserves no additional arena. The future loader must also fit identity
metadata and every other non-routing value inside RESOURCES.md's snapshot.

`encoded_len` validates all representation bounds and ordering. `write_preimage`
performs that complete check before any sink call, then streams the encoding
without heap allocation or a full preimage copy. Length prefixes use UTF-8 byte
counts. Null arrays and empty arrays, and null address names and empty names,
remain distinct. A sink failure ends emission immediately; its partial digest
or output must be discarded, with no state publication. The trusted sink owns
its own allocation/failure behavior. Sink calls never contain empty slices.
Default error Display/Debug and value Debug do not expose identity text; typed
sink errors remain accessible through the enum and error source, whose
explicit inspection is the trusted caller's responsibility. Validation reports
the first refusal in encoding traversal order; no cross-field diagnostic
precedence is promised. M07 owns hashing with the
real provider, and M15 owns state-token publication from a pinned snapshot.

Stable representation codes are `identity_preimage_count`,
`identity_preimage_address_count`, `identity_preimage_text_limit`,
`identity_preimage_size_limit` and `identity_preimage_order`. They identify
codec refusals; the schema/CLI must supply configuration source locations.

Identity digest oracles (literal preimages; M07 verifies with the real
provider, M04 verifies the canonical encoder):

- Empty snapshot preimage hex:
  `74642d6d74612d6964656e7469746965732d76310000000000`.
  SHA-256: `2804d97c45f30188d110f0e527fde929ee29404879c87d717b5085a4c8ca4830`.
- One identity: ID is sixteen 0x11 bytes, name Me, email me@example.org,
  replyTo null, bcc empty array, empty signatures, mayDelete false. Preimage:
  `74642d6d74612d6964656e7469746965732d7631000100000011111111111111111111111111111111020000004d650e0000006d65406578616d706c652e6f7267000100000000000000000000000000`.
  SHA-256: `5b5bb9f27cd7a82bbc8e09db63825ad730612515f87d873845e09808640747ce`.

Session state is a separate capability/account/endpoint/configuration
fingerprint (M13); it must not expose volatile queue counters or secrets.

## 5. Error translation

Authorization precedes existence lookup. Core Invalid/Corrupt must not turn
internal invariant failure into a client invalidArguments error. Validate
client syntax in the protocol layer, then map only documented user mistakes.
Unknown well-formed IDs have notFound semantics as specified by WIRE.md.

| Condition | Protocol behavior |
| --- | --- |
| No admission/read slot before HTTP headers | Retryable HTTP 503 with bounded Retry-After |
| Temporary Busy/Capacity/Deadline before this JMAP method has any effects | serverUnavailable; earlier method successes remain in the response |
| Deterministic per-request output/work ceiling, before this method's effects | serverFail with the exhausted limit; an identical retry is not promised to fit |
| Request-level maxSizeRequest/maxCallsInRequest/maxConcurrentRequests exceeded | HTTP problem details of type urn:ietf:params:jmap:error:limit with the limit property, before processing any method |
| Method object/count/size or logical quota limit | requestTooLarge or method-specific tooLarge/overQuota as the applicable RFC requires; never reuse requestTooLarge for arbitrary output size |
| Client ifInState mismatch | stateMismatch before object mutations |
| HistoryLost/unrecognized changes state | cannotCalculateChanges |
| Missing authorized object/blob | notFound / HTTP 404 |
| Forbidden per-object operation | forbidden or the specified method-specific SetError |
| Corrupt/WriterStopped/Indeterminate | Fail service health, stop mutations; never acknowledge an unproved commit |

Method-level errors other than serverPartialFail require no externally
visible change by that method (RFC 8620 §3.6.2). If earlier objects in a Set
method committed, return normal per-object results where the remaining
refusals have truthful defined SetError semantics. Predictable resource
exhaustion must be refused before mutation. An unavoidable later failure
without a suitable standard SetError uses serverPartialFail and requires
resync; never replace known effects with method-level serverUnavailable or
serverFail. RFC 8620 §5.3 does not enumerate those generic method failures
as SetErrors. ADMISSION.md specifies the exact retained-response behavior.
Reserve response capacity before mutation. An indeterminate journal outcome
follows section 3, never a guessed result.

SMTP rejects unavailable admission with temporary status before DATA and
returns final 250 only after proven durable commit. No local error maps to
acceptance. After HTTP headers are sent, a read failure closes the incomplete
response; never emit a second HTTP status or valid-looking truncated JSON.
ADMISSION.md supplies response retention so later method failures cannot erase
earlier method successes. Tests in M02c2 cover token bytes/bounds/scoping;
future adapter and protocol suites must prove these operational mappings.

Sources: [JMAP Core, RFC 8620 §5](https://www.rfc-editor.org/rfc/rfc8620.html#section-5)
and [JMAP Mail, RFC 8621](https://www.rfc-editor.org/rfc/rfc8621.html).
