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

## Persistent source binding

The owner may initialize a replica identity on a freshly prepared local database:

```text
td-mta postgres replica-source-init --connection-file OWNER_FILE --epoch LOCAL_EPOCH --source-origin https://mail.example.test --source-epoch SOURCE_EPOCH
td-mta postgres replica-source-inspect --connection-file OWNER_FILE --epoch LOCAL_EPOCH
```

These commands prepare and inspect identity only. They do not contact the source,
copy mail, provision local HTTPS or credentials, activate a read service, certify
completeness, or approve recovery. Obtain the source epoch through authenticated
source discovery/inventory; an origin alone is not proof of the source identity.
The local epoch must differ from the source epoch.

Schema 36 adds one owner-writable singleton source row, containing the canonical
HTTPS origin, source epoch and protocol version. Empty means ordinary primary
mode. Initialization requires the schema owner and the exclusive migration lock,
and refuses existing accounts, a selected configuration or an ACME profile.
Matching repetition is idempotent; a different binding refuses. There is no
unbind or promotion command. After an indeterminate COMMIT, inspect this exact
local epoch and source binding before retrying; never assume rollback or choose
a new identity to resolve uncertainty.

Ordinary runtime database qualification refuses a bound replica, including
SMTP, relay, writable JMAP and ACME operations. Retained sessions hold the shared
migration lock, so initialization cannot change their mode while they run.
The ACME executable claims its persistent profile before binding its HTTP-01
listener; that row also prevents later replica initialization on the database.
Read-only local serving remains subsequent work; the replica publisher has a
separate qualification path described below.
Owner administrative access remains trusted and is not a read-only SQL sandbox.

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
shift remaining IDs out of a page. Inventory returns only rows whose last change
is no newer than its starting state; newer updates and creations are handled by
change catch-up. Every inventoried ID therefore existed by the starting state.
This matters because JMAP coalesces a later creation followed by deletion to no
change: copying that creation from a later inventory page could otherwise leave
a stale local Email. Arrivals above the fixed upper ID cannot extend the scan
indefinitely; arrivals behind the cursor likewise belong to change catch-up.
All selected rows, including lookahead, must have revisions no newer than their
observed snapshot. Future/corrupt revisions remain selected for integrity refusal,
not silently excluded by the starting-state filter.

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
The account capability also advertises `inventoryBaseFence: true`. Discovery
requires this exact boolean before using the source. An older source without
it, or one advertising false, is Unsupported; malformed/duplicate values
refuse. This distinguishes the starting-state inventory guarantee from older
servers using the same capability URI. The cursor encoding remains unchanged.

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

### Source Session negotiation

The passive discovery decoder requires advertised core, mail and replication
support before selecting source endpoints. It selects an explicitly bound
account, or the sole advertised account on first discovery; it never chooses
among multiple accounts implicitly. Missing required capability support is an
Unsupported outcome. Malformed or missing limits refuse instead of supplying
optimistic defaults. Advertised inventory/metadata/request bounds are capped
by local limits; a smaller source bound is retained.

API URLs must be HTTPS and match the configured source origin. The supported
download template contains exactly one each of accountId, blobId, name and
type using simple brace substitution. Expansion uses typed IDs, a fixed
message.eml name and percent-encoded message/rfc822 type, then rechecks the
complete URL against the same origin. Unknown, duplicate or missing variables,
malformed URLs, fragments and cross-origin endpoints refuse. No redirect or
cross-origin credential authority follows from discovery.

Relevant duplicate fields refuse; additional Session fields and capabilities
are ignored within a 64-field per-object bound. Strings are borrowed from the
bounded response owner. The caller must still bound JSON intake and allocation
before constructing that response. These checks establish routing and supported
limits only, not peer authentication, source epoch continuity or local progress.
Tests feed actual td-mta read and submission Session documents through discovery.

### Bounded reusable source transport

The outbound HTTPS helper has an explicit sequential reuse owner for the
synchronizer. This prepares transport only; no synchronization command or
local publisher is enabled. One owner borrows immutable source-origin and CA
trust configuration and has one absolute overall deadline. Each connection
has at most 32 requests and a fixed lifetime of at most 60 seconds, also capped
by the owner's deadline. It is retired before reuse after four seconds idle.
No operation extends the original TLS lifetime. Each request sends its own
authorization and must match the configured HTTPS origin; no redirect,
pipelining, cross-origin credential forwarding or automatic request replay
is provided.

Only a completely framed HTTP/1.1 200 response can retain a connection.
Content-Length or the admitted chunked subset is required; EOF framing,
HTTP/1.0, HEAD/no-body replies, Connection close or unknown options, Upgrade,
and buffered trailing bytes prevent reuse. A non-consuming socket probe before the
next request requires WouldBlock: any queued wire byte or closure prevents
reuse, as do buffered TLS input, plaintext or output. The probe never processes
a control record that could hide later queued bytes. Data arriving later can still
race that check; a resulting request failure is returned without replay.
Unexpected status, framing, deadline, transport or sink failure discards the
connection. The sink's bytes remain provisional until successful completion;
body length/digest verification is still required before local publication.

TLS input waits use bounded blocking socket readiness once buffered work is
exhausted. Pending output retains bounded progress turns. ACME and migration
callers retain their existing single-use request policy. This helper creates
no background workers or unbounded connection pool. Retry and credential
refresh belong to the synchronizer's bounded read-operation policy.

### Authenticated source reader

The source reader connects Session discovery, inventory, metadata and raw body
fetching, complete Mailbox/get observations and independent Email/Mailbox
changes over the bounded reusable HTTPS transport. It sends one closed read
method per request and obeys the smaller negotiated request and ID limits.
It has no local database access, publication authority or durable checkpoints;
there is still no synchronization command or local read service.

A new reader selects the sole authenticated account and learns its epoch only
after a complete valid initial inventory response. A resumed reader takes the
persisted account/epoch binding. Changed scope refuses without replacing that
binding. Metadata and body reads require a bound epoch. Metadata pages own their
immutable response storage, selected origin and binding; keyword views borrow
that storage. Passing a page across origins/accounts/epochs refuses. The caller
must also preserve its configured source origin with its persistent binding.

JSON intake has caller-chosen byte and value/key-node limits, capped at 8 MiB
and 262144 nodes, with depth 32. Discovery uses the smaller of these and 32 KiB /
8192 nodes. A nonallocating structural pass enforces those limits before the
owned JSON tree is built. This bounds both raw strings and tiny-value allocation
amplification; it is not a promise that heap use equals wire bytes. Media type
must be application/json. Byte/node/depth refusal never becomes an empty page.
The original absolute job deadline covers network, parsing and verification.

A body read takes an Email from a validated metadata page, reserves its declared
length within the configured body limit (at most 64 MiB), and incrementally
hashes the received raw bytes. No bytes are exposed until HTTP completion,
exact byte count and SHA-256 all succeed. The resulting immutable owner can be
read but not altered. A short, oversized, truncated or digest-mismatched body
cannot become a successful body, even if a prefix was received. HTTP error,
source reset, malformed data, resource refusal and integrity failure are explicit
outcomes. This reader does not automatically retry, redirect or skip objects.

The future worker must charge one complete provisional body plus raw/parsed
JSON and retained metadata pages against its memory plan. There is no new
worker or default memory allowance in this increment. Use one background reader
initially and measure before increasing concurrency. Current metadata is not a
snapshot of later body availability: deletion requires fresh reconciliation,
not an inferred successful copy. Inventory completion still proves enumeration
only; mailbox dependencies and change catch-up remain separate work.

The mailbox request selects the six stored properties and all IDs. Its decoder
requires complete coverage, valid core row values, unique IDs/roles, and a
bounded acyclic parent graph with no missing parent. It retains subscription
and ordering without importing computed counts or remote rights. The source's
256-mailbox ceiling is retained. If a source has a smaller get limit than its
complete graph, copying pauses explicitly; it does not truncate the graph.

Change pages bind oldState exactly to the requested Email or Mailbox state and
require a matching account/epoch/type and monotonic newState. Created, updated
and destroyed IDs are bounded in aggregate (at most 1024), unique and disjoint.
They are sorted locally; wire order confers no authority. A page with changes,
or hasMoreChanges, must advance. Empty pages may advance past unrelated account
transactions. Mailbox updatedProperties must be null: this reader requests full
metadata for changed objects, not a partial-property interpretation. A change
page carries no body, local commit evidence or checkpoint advancement authority.

### Local account initialization

The owner-side library can prepare a replica account from a validated first
inventory page and complete mailbox graph. This is not yet an operator copy
command. The caller authenticates the source; passive response constructors
validate supplied data but do not establish transport provenance. Preparation
binds canonical source origin, epoch, account, initial Email state, independent
Mailbox state, complete graph and explicit resource limits into an immutable,
domain-separated digest. The decoder retains the requested inventory cursor; a continuation refuses
initialization even when an idle source returns equal base and observed states.
The mailbox observation must be at least as new as
the first inventory. The source Inbox role is required; no folder is invented.

Schema 37 stores owner-controlled replica account limits and the initialization
receipt separately from mutable progress. All four new tables are SELECT-only
for the ordinary runtime role. Account creation, preserved
mailbox IDs and parent relationships, local change history, usage, source
mailbox receipts and the exact first pending inventory page commit together.
The local sequence starts at one; source sequences are never local JMAP states.
No Email, credential or operational configuration is copied at initialization.

A matching initialization retry returns success without overwriting later mail,
mailboxes or progress. A different seed for an existing account refuses.
Retain the same seed to resolve an uncertain COMMIT; fetching a new inventory
is not an equivalent retry. The stored Email cursor initially names the change
replay anchor, not a fully applied source checkpoint. Inventory remains explicitly
incomplete even when the initial pending page has no IDs.

A pending page retains its exact bounded IDs and completion bitmap before any
Email effects. This includes the first page and the all-zero Email ID; restarting
must not replace it with a new initial inventory and skip earlier work. Inventory
pages hold at most 256 IDs; change pages at most 1024. Generation and page ordinal
are positive and must match the publisher's locked current row. Passive page
helpers reject stale observations, foreign scope, duplicate IDs and nonzero
padding bits. Each effect's completion bit must commit with its local publication;
only all completed effects permit advancement to the page's original continuation.
A newer per-object observation never becomes the global change checkpoint.

The account initializer persists only the first page. Bounded receipt retention and restart orchestration
remain subsequent work. Limits reserve no bodies and cannot authorize an
unbounded writer. Exhaustion must pause a future publisher without skipping mail.
This increment does not make a replica usable or certify copy completeness.

### Reloading persisted work

The owner progress reader takes one repeatable-read snapshot of the source
binding, account limits and receipt, source cursors, generation, phase and exact
pending page. It validates source/local epoch separation, scoped inventory
continuation, quota counters and page identity before reconstructing unfinished
IDs. Duplicate or unsorted stored IDs, mismatched generations, malformed state
or bitmap padding refuse. No missing pending page may silently complete an
unfinished inventory. Completion bits reflect only the stored snapshot.

This library read does not contact the source, mutate progress, acquire a writer
lease or certify a current copy. Its Email cursor remains a replay anchor during
initial inventory. The worker must revalidate its generation/page against locked
current rows when publishing; a historical inspection is not permission to write.
No per-message synchronizer uses this cold owner connection. The retained
replica publisher also offers progress inspection through its qualified runtime
connection. It takes a repeatable-read read-only snapshot and rolls it back,
uses the same source/account binding and finite session/job budgets, and checks
the epoch and maintenance barrier. Failure retires that connection; a result
that exceeds its deadline is not returned. It neither mutates progress nor
requires the owner credential. The copy command remains subsequent work.

### Transactional Email publication

The library publisher applies one immutable source observation to the exact
locked pending generation/page. Present mail requires either a complete verified raw
body matching the metadata Blob ID, size and digest or a matching previously
verified local body as described below. Explicit metadata notFound
is deletion evidence; a missing download or absent inventory row is not.
Preparation derives local MIME evidence from the verified bytes and binds all
preserved metadata into a domain-separated receipt digest. Passive metadata
constructors validate structure and scope but do not authenticate the source;
the synchronizer must use the authenticated reader.

Publication locks the local account and commits the body, Email, source IDs,
thread membership, mailbox memberships, keywords, usage, local change history,
source receipt and pending completion bit together. It never advances the
page continuation or global source cursor. Missing mailbox dependencies pause
publication. An existing Email cannot change its immutable body, thread or
received timestamp. Local sequence numbers advance only when visible content
changes. Repeating the same observation is idempotent; older observations and
conflicting data at the same source state refuse. A newer observation with
unchanged content updates the receipt without inventing a local change.

Schema 38 adds bounded Email receipts and immutable Blob identity receipts.
Bodies no longer referenced by Emails, leases or submissions may be collected;
the identity receipt and core Blob-ID reservation survive. Reappearance may
restore only the exact original length and digest under that Blob ID. Ordinary
publication still refuses previously reserved Blob IDs. Body-byte, blob,
Email and receipt limits are checked transactionally. Receipts currently remain
until an explicit future retention implementation; reaching their configured
limit pauses copying and does not discard safety evidence or mail.

A purpose-specific runtime role requires write access to replica progress,
pending work, mailbox receipts, Email receipts and Blob identities while source
binding and account limits remain owner-only. Ordinary primary grants retain
SELECT-only access to those control tables, and ordinary writable runtime
qualification still refuses a bound replica. The publisher authenticates and
fully checks one PostgreSQL connection once, retains the shared migration lock,
and checks epoch/maintenance on each job. It admits at most 16 publication or page-transition jobs
within one 60-second connection lifetime. Each job has at most 4 MiB database
input, 72 MiB output and 4096 protocol frames; initial qualification consumes
the first window. Deadlines use the existing bounded import allowance, at most
30 seconds, without extending the connection lifetime. Failed jobs retire the
session, except a missing reusable body whose explicit rollback is acknowledged. An unknown COMMIT remains explicitly indeterminate; a confirmed COMMIT
remains success even if subsequent connection retirement fails.

This is a library primitive, not an operator copy command or a read service.
No resident worker, new default memory allowance, local credentials or source
configuration is enabled. Copy scheduling, retention
and completeness validation remain required before a usable full-copy claim.

### Reusing verified local bodies

A metadata-only observation may request reuse of the local immutable Blob.
Publication checks the account-scoped Blob identity receipt and live manifest
against the authenticated source Blob ID, exact length and digest. Matching
bytes can support flag/membership updates or another Email referring to the
same Blob without another download or MIME interpretation. The canonical
Email receipt digest is identical whether bytes were downloaded or reused.

If an existing Email's Blob or identity is inconsistent, publication refuses
with Integrity. If a new Email needs bytes absent from the live Blob store,
MissingBody means download and verify the complete source body, then retry the
same observation/ticket. A surviving identity receipt alone cannot publish an
absent body. MissingBody writes no placeholder, completion bit or receipt; its
transaction is explicitly rolled back. Acknowledged rollback keeps the finite
session reusable. Failed rollback, deadline or transport retires it, and every
attempt still consumes a turn. Other publication errors retain retirement.

This reuses the existing verified-write and trusted stored-metadata boundary;
it does not rehash local raw chunks or certify their present integrity. Ordinary
body verification and scrubbing remain responsible for detecting later storage
corruption. The copy worker uses verified downloads for initial inventory
and reuse during change catch-up, fetching only a cache miss. No automatic
network retry or silent body eviction is added.

### Durable page transitions

The publisher may stage one validated next inventory or Email changes page.
It reuses the same bounded, source-bound runtime session and account lock.
The current pending generation and page must match, and every current ID must
already have its completion bit. Advancing the old cursor and storing all next
IDs with a fresh zero bitmap occur in one transaction. A failed transaction
leaves both cursor and old work intact. No local Email/JMAP state changes.

Inventory continuation must equal the old page's original cursor, preserving
its base, upper bound and strictly advancing ID range. Its observation cannot
move backward. After the last inventory page, catch-up starts at the retained
initial replay anchor; finishing inventory alone does not certify a full copy.
Each changes page starts at the completed old page's original newState, never a
newer per-object metadata observation. This permits bounded partial catch-up
pages even when later metadata was fetched during inventory.

A transition names the previous ticket and deterministically uses the next
page ordinal in the same generation. A matching retry recognizes the exact
staged page and anchor without resetting completion bits accumulated since its
commit. A mismatched page, wrong generation, overflow or stale transition
refuses. Only the immediately next pending page can settle that retry; if work
has advanced further, reload progress rather than replaying an obsolete request.
An indeterminate COMMIT has the same inspect/retry boundary as Email publication.

One pending page remains stored even when empty or complete. The durable Email
cursor represents fully completed previous work, and the next page retains its
own original newState until a subsequent transition. Status must distinguish
these values and must not promote an object observation into a checkpoint.
This primitive adds no polling worker, mailbox reconciliation, expired-history
reset, receipt pruning, read service or copy-completeness claim.

### Full mailbox graph reconciliation

A replica graph operation binds a complete authenticated Mailbox/get observation
to the fixed source/account and current positive copy generation. It requires
the initialized source Inbox identity. Publication locks the account and refuses
older observations, stale generations, or conflicting metadata at the same
source state. Local mailbox rows, roles and parent relationships remain bounded
and valid; no folder is invented to satisfy a missing dependency.

Source-present folders are created or updated with their original IDs, metadata
and complete parent graph. Source-absent folders are marked absent in source
receipts. A locally referenced absent folder remains readable until its Email
memberships are reconciled; retired parents needed by such children remain too.
Retired roles are cleared so a replacement source folder can hold the role
without ambiguity. Other absent folders are removed under the deferred foreign
keys. No Email or membership is silently deleted by the graph operation.

The visible union, including retained retired folders, must fit the existing
256-mailbox bound. A source graph that fits by itself may temporarily exceed
that limit when combined with local removal dependencies. Capacity refusal
pauses reconciliation and leaves the previous view intact; this limitation is
explicit, not truncation or copy completion. Receipt accumulation shares the
configured receipt quota. Historical tombstones remain bounded by that quota.
Schema 39 adds latest-observation and present-folder receipt indexes so graph
checks do not scan all historical mailbox identities.

Changes, local history/sequence, source receipts, receipt usage and the Mailbox
checkpoint commit together. The checkpoint advances only once no retired
removal dependencies remain. Repeating the same source graph can remove folders
whose references have since cleared and then finish the checkpoint; repeating
an already applied graph emits no local change. Metadata-only observation
changes do not advance the Email cursor. The copy scheduler, retention, history
reset, read-only service and completeness qualification remain subsequent work.

### Bounded copy steps

The copy worker binds one source, account, distinct local epoch and immutable
local connection/trust references at construction. Changing the local endpoint
or credentials requires a new worker; a cached publisher cannot outlive that
configuration binding. Each step
reloads durable progress through the restricted runtime publisher, authenticates
fresh source discovery, and reconciles the complete mailbox graph before Email
work. It either applies at most 16 unfinished IDs (also bounded by negotiated
metadata limits), or advances a completed page. It never advances unfinished
work. Initial inventory downloads and verifies each complete body; change
catch-up reuses a matching local Blob and downloads only MissingBody results.
The source HTTP connection is reused within the step. No source or local SQL
transaction spans a network body download.

The caller supplies a step deadline at most 60 seconds away. Each local job
also obeys the existing 10–30 second import deadline and finite runtime session
turn/lifetime limits; an exhausted publisher is retired and requalified before
the next job. The worker retains at most one provisional body alongside bounded
metadata, discovery and mailbox pages. A future resident worker must account
for these allocations in its memory plan; this library adds no default allowance.

Each successful Email is independently durable. An error later in a batch
returns failure without undoing committed Emails; the next step reloads their
completion bits. Any error, including indeterminate COMMIT, retires the local
session. There is no blind retry of publication, source reset, or automatic
change of binding. Expired history and resource exhaustion pause with the last
usable view preserved. A caller must retire the publisher before an external
wait or pause; worker drop also releases it. An empty changes observation
retires it automatically so idle scheduling holds no database lease.

Step counters describe only a successful step, not historical totals. Empty
changes means only that one source Email changes page was empty; it is not a
full-copy certificate, simultaneous Email/Mailbox snapshot, or BCDR readiness.
Mailbox removal dependencies and independently observed source states remain
explicit. Scheduling/backoff, protected configuration, durable contact/error
status, reconciliation after history expiry, read-only serving and completeness
qualification are not enabled by this primitive.

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

## Operator copy commands

Prepare a fresh schema and bind it with `postgres replica-source-init` first.
`postgres replica-account-init` takes the owner connection, local epoch,
source origin/epoch, preserved account ID, username, protected password file,
optional CA file and explicit positive `--body-bytes`, `--blobs`, `--emails`
and `--receipts` quotas. It authenticates discovery and obtains the initial
inventory and mailbox graph before atomically creating the account. Existing
matching source/account/quotas are reported without reseeding, resetting work
or asserting that a newly fetched seed matches an older request. Inspect an
indeterminate initialization with `postgres replica-progress`; do not infer
absence from a lost command response. No credentials or routing are copied.

`postgres replica-copy` takes the same binding and source credential options,
with a dedicated replica runtime connection instead of the owner connection.
It runs `--steps 1..1024` (default 1) under `--timeout-seconds 1..3600` (default
60), stopping at an empty Email changes observation. Each step remains bounded
to 60 seconds and the existing per-job limits. JSON lines report successful
steps; the publisher is retired before each potentially blocking output write.
The timeout bounds copy work, not a blocked host stdout consumer. A later error
exits nonzero and leaves earlier committed progress intact.
Read-only inspection and initial owner preflights may try the next resolved
local address on connection failure. Copy errors and post-publication errors
never replay against another address. There is no background loop, automatic
history reset or binding replacement. The runtime login must have the restricted
replica grants; the ordinary primary login remains ineligible. Installer support
for provisioning the replica role and read-only service is subsequent work.

`postgres replica-progress --connection-file PATH --epoch HEX --account ID`
uses the owner connection to report a historical snapshot of stored copy work.
It contacts no upstream and proves neither freshness nor coverage. All commands
report `copy_complete:false` and `recovery_verified:false`; an inventory-complete
flag or empty pending page does not change those claims. Configured quotas,
source identity and account IDs contain no secret bytes. Passwords are accepted
only from protected files beneath a private parent, with one final LF or CRLF
removed. Explicit CA trust uses the same checked input inventory; public roots
remain the default. Passwords, Authorization headers and message content never
enter command output. There are no new listeners or service activation.

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

## Read-only HTTP policy preparation

`Session::mail_read_only` prepares an `isReadOnly: true` account with no
submission capability or top-level mailbox creation permission. Its closed
method allowlist admits Core echo and the implemented mail and replica reads;
new or unknown methods are refused. The PostgreSQL API service rejects an
entire disallowed batch after bounded request parsing and before opening a
storage view or invoking mutations. Passive dispatch reports no mailbox write
rights. HTTPS refuses authenticated uploads before reserving a body slot, and
the PostgreSQL upload adapter independently refuses them before ingress.
These guards do not qualify a database or activate a listener. Ordinary runtime
qualification still refuses a replica-bound store; a separately qualified local
read profile and installer remain required. Existing primary Sessions retain
their writable behavior. Client policy remains advisory; server refusal owns
the authorization boundary.
