# Local account replication

## Status and scope

The source inventory and metadata API is implemented. The local synchronizer,
read-only service and recovery procedure remain design direction, not available
commands or recovery claims. Search and td-mail qualification precede the local
synchronization and read service described here. PostgreSQL remains the
only backend. Synchronization is application-level over authenticated HTTPS;
it does not expose PostgreSQL or require replication credentials on clients.

The cloud server remains authoritative. A local td-mta instance retains full
message and attachment bytes for explicitly selected accounts and serves
read-only JMAP to td-mail on loopback. No SMTP listener, relay worker, ACME
worker, mutation forwarding, offline action queue or automatic promotion runs
in this mode. Reading must not implicitly mark a message seen. The local
Session and Mailbox rights must agree with method-level mutation refusal;
td-mail must suppress unsupported actions rather than repeatedly trying them.
Users can retain a separate cloud account configuration for writes and sending.

## Storage and identity

Reuse the core account, mailbox, thread, Email, immutable body, membership and
keyword schema and its validators. Preserve source object identities, raw body
bytes, received dates and complete memberships/keywords. Do not recompute
threads from headers, deduplicate distinct Emails by digest, or rewrite trace
headers as if synchronized mail were newly received by SMTP.

Local synchronization tables bind each account to a source identity, protocol
version and opaque source checkpoints. Source and local state tokens are
separate: local JMAP changes describe committed local visibility, not a claim
that the local database is the source snapshot. The local store has its own
epoch and credentials. A source reset or changed identity must trigger explicit
reconciliation, never silently join a different mailbox history.

An exact source-ID preservation scheme must be qualified before accepting two
source instances into one local database. Local account isolation alone does
not resolve every cross-source collision in the existing schema. The initial
implementation may restrict one local store to one upstream td-mta instance.

Search fields, summaries and MIME evidence are versioned, rebuildable derived
data. The same implementation serves local and cloud reads; a replica may
rebuild compatible evidence from verified raw bytes. Remote derived data is
not silently granted local integrity authority.

## Synchronization contract

### Source inventory adapter

The authenticated PostgreSQL snapshot adapter provides
`replica_inventory(cursor, maximum)`, exposed by the HTTPS method below. It is
not yet connected to a local synchronizer. Each call
uses the snapshot's existing authentication, account, deadline and bounded
database worker. At most 256 IDs and one lookahead row are read. No body bytes
or derived interpretation are returned.

The initial page records the account's committed sequence and maximum Email
ID in the same snapshot. Continuations preserve that starting state and upper
ID and advance strictly by Email ID, never by received date or OFFSET. Each
later page may use a fresh authenticated snapshot and requires the starting state
to belong to the same account and epoch and remain within retained history.
Changed dates, mailbox moves and deletion of an already visited ID cannot
shift remaining IDs out of a page. Arrivals above the fixed upper ID cannot
extend the scan indefinitely; arrivals behind the cursor are deliberately left
to change catch-up. All rows, including lookahead, must have revisions no newer
than their observed snapshot.

Cursors are passive continuation values, never authorization credentials. Finishing this enumeration proves neither a
historical snapshot nor a current full copy. Metadata/body fetching, changes
since the starting state, cross-object reconciliation and final coverage
validation remain required. An expired or foreign starting state refuses with
`CannotCalculateChanges`; the synchronizer must restart reconciliation while
preserving its last usable local view. No enumeration page grants deletion
authority or advances a durable replica checkpoint by itself.

### Authenticated source API, version 1

The Session global and account capabilities advertise
`https://timmydouglas.com/ns/td-mta/replication`. Following RFC 8620 §1.8,
clients must include that capability and the core and mail capabilities in
`using`. Both methods use the ordinary authenticated account read snapshot,
request deadlines, work and response limits. This capability neither creates
read-only credentials nor grants additional account access. Credentials still
carry their existing rights. Ordinary public HTTPS verification is required.

Account capability values `maxInventoryIds` and `maxMetadataIds` are respectively
the smaller of configured query-page/256 and configured objects-per-method/16.

`TdReplica/inventory` accepts `accountId`, optional `cursor` (null or omitted
starts an inventory), and optional `limit` (defaults to the advertised bound).
Zero or malformed limits refuse with `invalidArguments`; exceeding the bound
refuses with `requestTooLarge`. Unknown arguments refuse. The result contains
`accountId`, `sourceId`, `baseState`, `state`, `ids`, and `nextCursor`. A null
next cursor ends enumeration only; it is not a completeness checkpoint.
`baseState` remains the initial Email state; `state` identifies this page's
observed snapshot. Mailbox synchronization needs its own state.

The opaque cursor's version-1 wire form is 153 ASCII bytes: `ri1_`, the existing
85-byte Email DataState encoding, the upper Email ID's 32 lowercase hex digits,
and the last visited ID's 32 lowercase hex digits. No separators or trailing
bytes are allowed. Foreign account/epoch, expired or future base states refuse
with `cannotCalculateChanges`. Invalid syntax, non-Email state and inverted
bounds refuse with `invalidArguments`. It is not signed and grants no authority;
a client changing its own cursor may omit its own data. Authentication and
account filtering apply independently to every call.

`TdReplica/metadata` accepts `accountId` and an explicit `ids` array (empty is
allowed), bounded by `maxMetadataIds`. Unknown arguments and null/missing IDs
refuse. The result contains `accountId`, `sourceId`, `state`, `list` and
`notFound`. Each list entry contains exactly `id`, `blobId`, `threadId`,
`mailboxIds`, `keywords`, `size`, `receivedAt`, `receivedAtMs` and `bodySha256`.
The first seven are the ordinary stored Email fields. `receivedAtMs` is a signed
decimal string preserving the exact stored millisecond timestamp without JSON
number precision loss. `bodySha256` is 64 lowercase hex digits for the complete
raw message, including stored trace headers. IDs are deduplicated and valid
absent/foreign-format IDs are returned in `notFound` as for Email/get. Internal
SMTP envelope recipients, delivery attempts and credentials are not exposed.

Both methods report `sourceId` as the store epoch's 32 lowercase hex digits.
Clients bind it to the configured HTTPS source and account; it is not a global
server certificate or proof of continuity across unqualified database restores.
Existing restore procedures must establish a fresh epoch. A changed source ID
requires reconciliation, never an automatic continuation of the old history.

Metadata and digest are authenticated stored values from one snapshot; neither
method reads or verifies raw body chunks. This is the same stored-metadata trust
boundary as Email/get, not a verified-body capability. The local synchronizer
must download the complete raw blob through the existing authenticated download
URL and verify both size and digest before publication. A missing body after
metadata was fetched requires fresh reconciliation, never silent omission.
No source transaction remains open across API calls.

### Passive source-response validation

The client decoder validates complete bounded source responses before they can
be offered to a future local publisher. It does not open HTTPS connections,
create accounts, write mail, or advance durable checkpoints. Source identity
values are checked against an explicit account/epoch binding; verified HTTPS
origin and credential handling remain the transport caller's responsibility.

Inventory responses must preserve scope, starting state and continuation
bounds, contain strictly increasing unique IDs, and return a next cursor that
matches the last ID and requested page size. The first page's observed state
must equal its base. A completed page alone still grants no deletion authority.

Metadata responses must exactly partition the unique requested IDs between
list and notFound. Unknown or duplicate fields/IDs, unexpected rows, omitted
coverage, noncanonical digests and inconsistent exact/formatted timestamps
refuse. Mailbox and keyword sets are bounded and validated; duplicate values refuse
rather than being silently dropped. The caller chooses body and value bounds,
within 64 MiB per body and 4096 values of each kind per Email; resource refusal
must pause copying rather than skip data. Parsed keywords borrow the bounded
response storage, and sorted set validation avoids quadratic large-set scans.

Single-call response matching checks method and invocation ID before decoding.
cannotCalculateChanges is a reset outcome; malformed and other remote failures
are distinct errors, never successful empty lists. Raw bodies still require
separate complete length/digest verification before publication.

### Remaining synchronization work

Qualify the existing JMAP Email/Mailbox get and changes surfaces and raw
downloads alongside the source extension. Reuse those where they meet the requirements; specify and review
any missing snapshot or digest capability before implementing an extension.
JMAP discovery or pagination alone does not establish a coherent full snapshot.

Fetch into private, bounded staging. Check declared lengths and the source's
authenticated digest when the protocol provides one, and compute local body
digests. A locally computed digest alone proves neither source completeness
nor source-side equality. Publish a complete Email's body and metadata
transactionally. Commit the corresponding progress checkpoint only after all
effects it covers are durable. Restart may repeat work but must not duplicate
Emails or skip uncommitted objects. Never serve a partial body as complete.

Track independent Email and Mailbox progress and explicitly reconcile their
cross-object dependencies. Account creation and the initial inventory are
resumable. Concurrent arrival, move and deletion must converge without relying
on OFFSET pagination over a changing inventory. Expired change history invokes
a bounded full reconciliation. Apply deletions only after complete authoritative
evidence; an interrupted inventory must not erase objects it did not visit.
Preserve the last usable local view during reconciliation and report its lag.

Full copy means every current message and attachment in each selected account;
there is no silent eviction or header-only completion. Enforce configured disk,
message, batch, concurrency and staging limits. Exhaustion pauses progress with
an explicit incomplete status and retains already committed mail. It must not
advance a checkpoint, silently omit large messages, or grow server retention
without bound. No source transaction or database lease spans a disconnected
client. Retry uses bounded backoff and fresh authentication.

## Local access and diagnostics

Upstream credentials use protected files and verified HTTPS. Local access has
separate credentials; binding loopback is not authentication against other
local users. Qualify a supported local transport/trust setup before shipping;
do not weaken td-mail's public HTTPS verification. Offline reading of previously
copied mail remains possible when upstream credentials expire or are revoked;
revocation prevents future synchronization but cannot retract downloaded data.

Report source binding, last successful contact, last fully applied checkpoint,
copied/pending objects and bytes, integrity failures, retry state and incomplete
coverage. Do not label an initial or interrupted copy as current. td-mail must
show read-only and synchronization status, including that local search can be
incomplete while the initial copy is unfinished. Logging follows
OBSERVABILITY.md and excludes credentials, subjects, query text and bodies.

## Recovery boundary

Shared mail schema and preserved identities support a future explicit recovery
procedure; they do not make a replica a ready standby. Account replication does
not copy device verifiers, domains/routing, provider passwords, certificates,
server configuration, operational leases or outbound delivery attempts.
Separate protected backups remain necessary for those resources.

A future promotion must prove completeness at a named checkpoint, report
unreplicated changes and its recovery point, restore and validate operational
configuration, fence the old primary, establish fresh local authority and
credentials, then explicitly enable listeners. Accepted outbound work requires
separate recovery evidence; absence from the replica is not proof it was never
submitted or delivered. Never replay outbound messages just because a Sent
copy exists. Automatic failover and zero-loss recovery are not initial claims.

Synchronization propagates legitimate source deletions and cannot replace
versioned backups or a tested restore. Record recovery-point and recovery-time
measurements only after a real drill, not from schema compatibility alone.

## Qualification and implementation order

1. Finish reusable exact search interpretation and td-mail search/error tests.
2. Qualify source inventory/change semantics, body download integrity and ID
   preservation; freeze protocol/checkpoint encodings and schema changes.
3. Implement resumable full synchronization with deterministic interruption,
   history-expiry, concurrent-edit, disk-refusal and account-isolation tests.
4. Expose the authenticated loopback read profile and td-mail read-only/status
   behavior; verify full body and attachment reads while upstream is offline.
5. Measure initial-copy and catch-up throughput, storage overhead and freshness.
   Timing measurements are diagnostic, not host-load-sensitive gate assertions.
6. Design and rehearse explicit recovery separately before claiming BCDR support.
