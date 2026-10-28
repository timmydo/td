# Bounded diagnostic records

M04d1 implements `src/observability.rs`: versioned event and explicit inspection
JSON Lines encoders. M04d2 adds fixed event buffering/loss counters and bounded
health/status snapshots in `observability/queue.rs` and `observability/health.rs`.
They borrow caller storage and allocate nothing. The PostgreSQL runtime sink described below now owns synchronization,
retention and loss reporting. Full health aggregation remains planned. These
records are diagnostics, never the authoritative
metadata store, an acceptance condition or an authorization token.

## Envelope

Every complete record is one ASCII JSON object followed by one LF. Keys below
are emitted in this order; readers must use key names, not ordering. Unknown
future fields may be ignored. Unknown versions, record types and codes must be
reported as unsupported rather than assigned a guessed meaning. Version 1 uses
JSON integer numbers without floating-point conversion: consumers must preserve
u64/i64 exactly. Optional correlation fields are omitted when irrelevant.

| Field | Meaning |
| --- | --- |
| `version` | Integer 1, independent of config/storage versions |
| `record` | `event`, `inspection`, or `status` |
| `boot_id` | 32 lowercase hex digits identifying this process boot |
| `utc_ms` | Signed UTC milliseconds since Unix epoch; null if unavailable |
| `config_generation` | Unsigned effective config generation; zero before activation |
| `connection_id` | Optional nonzero process-local connection number |
| `request_id` | Optional nonzero process-local request number |
| `transaction_sequence` | Optional unsigned committed transaction sequence |
| `submission_id` | Optional 32-digit lowercase hex submission ID |
| `facts` | Trusted fixed schema values and typed numeric facts |
| `recommended_actions` | Array of fixed advisory codes; never executable commands |
| `untrusted` | Array: empty for events/status; one labeled source-text object for inspection |

`BootId` is distinct from persistent `InstanceId`. The runtime obtains a fresh
boot ID from its entropy adapter before emitting events; parsing/constructing an
ID does not prove entropy, uniqueness or authority. Connection/request issuers
must refuse counter exhaustion, never wrap within a boot. The encoder accepts
already supplied values and does not mint IDs, sample clocks or access secrets.
A failed clock sample is null, never a fabricated epoch-zero time. These records
do not persist monotonic deadlines or compare ticks across boots.

Runtime producers supply relevant correlations: mail/metadata events use the
committed transaction, relay events use the submission, protocol events use the
connection and request where applicable. An encoder cannot prove the underlying
operation happened. Config generation identifies the snapshot actually used by
that operation, including an old retained snapshot during reload.

## Default events

`Event` contains only `Context` and the closed `Kind` enum. It has no free-form
strings, peer addresses, credential/AUTH/Authorization contents, subject,
recipient address or message body fields. Redaction is enforced by the default
input shape; it is not a substring filter. Normal events always have empty
`untrusted`. Their `facts` object begins with `code` and `severity`.

| Code | Additional facts | Severity | Recommended action |
| --- | --- | --- | --- |
| `performance` | fixed `phase`, unsigned `calls`, `failures`, `busy`, `unavailable`, `total_us`, `max_us`, `lost` | info | none |
| `boot_started` | none | info | none |
| `config_activated` | none | info | none |
| `listener_ready` | `protocol` | info | none |
| `connection_opened` | `protocol`, `security` | info | none |
| `connection_closed` | none | info | none |
| `tls_established` | `protocol`, `security` (`tls12` or `tls13`) | info | none |
| `tls_failed` | `protocol` | warning | none |
| `mail_accepted` | unsigned `bytes` | info | none |
| `admission_refused` | `reason` | warning | mapped below |
| `metadata_committed` | none | info | none |
| `wal_checkpointed` | none | info | none |
| `relay_outcome` | `outcome` | info for accepted, otherwise warning | inspect_submission for permanent_failure/unknown |
| `authentication_failed` | none | warning | none |
| `certificate_renewed` | signed `expires_utc_ms` | info | none |
| `certificate_renewal_failed` | none | warning | inspect_certificate |
| `log_suppressed` | unsigned `count`, boolean `saturated` | warning | inspect_logging |
| `delivery_notice_deferred` | fixed `error` class | warning | inspect_relay |
| `relay_connection_failed` | fixed `stage` and `error` class | warning | inspect_relay |
| `relay_step_failed` | fixed `stage` | warning | inspect_relay |
| `service_stopped` | boolean `failed` | error if failed, otherwise info | inspect_service if failed |
| `log_write_failed` | signed `os_code` or null | error | inspect_logging |

Protocol values are `smtp`, `jmap`, `relay`; listener-ready records accept only
`smtp` or `jmap`, because relay is outbound. Security values are `plain`,
`tls12`, `tls13`; TLS version alone makes no assertion about peer authentication.
`connection_opened.security` records transport state at that observation, not
listener policy. Both implicit TLS and STARTTLS emit `tls_established` after
a successful handshake; handshake failure emits `tls_failed`, with the same
connection ID. No peer-supplied handshake error text is logged. Runtime counters
are independent of these lossy events: classify a closed session as TLS if it
established TLS at any point, otherwise plain. A failed implicit-TLS handshake
never authorizes plaintext application traffic.
Relay outcomes are `accepted`, `retry`, `permanent_failure`, `unknown`.
Unknown means a potentially accepted relay outcome requiring inspection, not
proof of failure or an instruction to resend. The queue state machine owns
retry policy. `mail_accepted` means locally durable acceptance; it never implies
relay acceptance. Neither an event nor its absence proves durable storage.

Refusal reasons/actions are: `capacity` and `deadline` -> `retry_later`,
`quota` -> `inspect_quota`, `storage` -> `inspect_storage`, `configuration` ->
`check_config`, and `recovery` -> `inspect_recovery`. Actions are advice, not
authorization to remove data, change settings, retry uncertain delivery or run
shell commands. `log_suppressed.count` is cumulative within the boot;
`saturated` is true exactly at u64::MAX, where the count is a lower bound.
An inconsistent pair refuses encoding with InvalidCount and unchanged visible
output. Runtime loss aggregation is also available through `logging-status`.

## Authorized inspection

`UntrustedText` is deliberately separate from `Event`. Its explicit inspection
encoder emits empty `facts` and `recommended_actions`, and one `untrusted`
array element with `source`, `truncated` and `text`. Source labels are `smtp_reply`,
`message_header`, `configuration_input`; the label supplies provenance only.

Inspection records are returned only to the authorized requesting caller. They
are never enqueued for or written to the default log. The administrative output
region and log encoder region are disjoint; inspection does not borrow the log
buffer. `Debug` reports only source, retained length and truncation, never text.

The caller must authorize the inspection and decide which data may be exposed.
This type does not grant permission or detect secrets. In particular, ordinary
config-check errors must use fixed redacted diagnostics rather than echoing
input through this encoder. Peer text can contain instructions, credentials or
personal data even after escaping; consumers treat it solely as data. No output
consumer may execute instructions obtained from that field.

Retain at most the first 256 UTF-8 source bytes, shortened to a character
boundary. `truncated` is true exactly when any input was omitted. Construction
examines at most the boundary's three preceding bytes; it does not scan the
whole input. Quotes and backslashes are escaped, ASCII controls and every
non-ASCII scalar use lowercase `\uXXXX` escapes, with a surrogate pair for
non-BMP scalars. This also escapes terminal controls, bidi controls and Unicode
line separators. Source length/truncation are measured before JSON escaping.
There is no automatic trimming, normalization or interpretation.

## Bounds and failure

A default event is at most 1024 encoded bytes including its LF. Inspection is
at most 4096 bytes: the same bounded context/envelope plus at most 256 * 6
escaped text bytes. Tests enumerate event variants with maximal integers/IDs
and cover worst-case inspection expansion. `Option<Event>` fits 256 bytes,
allowing the planned 96 KiB event queue to hold 384 fixed cells. Encoders alone
do not instantiate that queue or claim runtime allocation/RSS measurements.

Both APIs append through `TextBuffer::format` with closed trusted formatters.
The entire line is appended or visible output remains unchanged. Insufficient
output is a typed capacity error; it never yields truncated valid-looking JSON.
Rollback restores visible length, not overwritten tail bytes: the owner must
not expose unused buffer storage. Inspection truncation applies only to the
source field and is explicit; it does not turn output exhaustion into success.
A caller cannot provide arbitrary `Display` implementations to these encoders.

## Fixed event queue and loss accounting

M04d2 implements `observability::queue::EventQueue` over caller-owned
`Option<Event>` cells. Construction accepts 1..384 empty cells, verifies the
actual cell layout is at most 256 bytes, and checks the complete slice fits
96 KiB before borrowing it. Oversized or occupied storage refuses unchanged.
This uses the existing log reservation; it allocates no cells and starts no
writer thread. `try_emit` never waits: full capacity returns false and increases
the dropped-event counter without changing FIFO contents. `pop` transfers one
event to the sole sink, which must retain it and its encoded offset through
partial writes. Runtime synchronization, deadlines, rotation and sink I/O remain
M19. Event validity and admission/durability remain independent of logging.

Loss snapshots contain cumulative boot-local `dropped` and `write_failures`
counters. `note_write_failure` increments the latter directly, even if the
queue is full or the file sink is broken; it never recursively emits a log.
A write failure increments `write_failures`; if the sink ultimately abandons
its popped event, it also calls `note_discarded(1)` exactly once. Retryable
failures do not themselves count as discarded events. `discard_remaining`
drains/counts queued events, including at shutdown; it excludes any event
already owned by the sink. Repeating it on an empty queue changes nothing.
The runtime must retain this queue/counter owner for the boot: constructing a
new one resets its counters and is not a recovery shortcut. Snapshots do not
clear counters, so an attempted suppression report cannot erase unreported
loss. M19 compares successive snapshots for rate-limited fallback reporting;
a saturated counter still signals a lower bound even when its numeric value
cannot change. Queue Drop drains remaining cells without allocating; shutdown
must explicitly flush or account for discarded events before dropping it.

`Counter` starts at zero, adds with saturation and derives `saturated` from
value == u64::MAX; its only stored field is the 8-byte count. `ZERO` permits
constant initialization, and `from_value` wraps an observed unsigned total
with the same saturation meaning. It never wraps and supplies the consistent pair
required by `log_suppressed`. Exact values are available below the ceiling;
at the ceiling the value is a lower bound. Counters have no reset method.
They do not own atomic synchronization; the coordinator or
sink owns the exclusive mutation boundary.

## Health/status snapshots

M04d2 also implements `observability::health::HealthSnapshot`. Its `status`
record uses the same version-1 envelope; `untrusted` is empty. Producers supply
observations from their coordinated runtime state. The encoder neither samples
the system nor grants admission, performs cleanup, authorizes operations or
proves a reported state. The configured disk index plus config generation is a
diagnostic label, not the descriptor-backed filesystem identity of ADMISSION.md.

`facts` has seven members:

| Member | Meaning |
| --- | --- |
| `state` | `serving`, `degraded`, `recovering`, or `refusing_mutations` |
| `ready` | Computed local readiness, independent of external dependencies |
| `local` | Booleans `configuration`, `storage`, `listeners`, `writer_admission_open`, `recovering` |
| `dependencies` | Conditions for `relay`, `certificate_renewal`, `logging`, `index` |
| `counters` | Cumulative boot-local observations listed below |
| `gauges` | Point-in-time observations listed below |
| `disks` | At most 16 distinct configured disk indices and observed headroom |

Readiness requires valid effective configuration, usable local storage,
configured listeners ready, the writer admission gate open and recovery
finished. Recovery
has first priority for state; otherwise any failed local requirement yields
`refusing_mutations`, meaning new external mutations are unavailable. The
writer gate is only one local prerequisite: it can be open while a missing
listener/config/storage prerequisite still prevents service admission. Already
admitted operations retain their separate completion contract. With local
readiness true, an unknown/degraded dependency
yields `degraded`; otherwise state is `serving`. Dependency conditions are
`unknown`, `healthy`, `degraded`, `disabled`. Disabled means intentionally not
used, not a suppressed error. A relay outage or renewal failure does not change
local readiness. If a certificate problem prevents a required local listener
from serving, the producer clears `local.listeners` separately. The runtime
must not infer readiness solely from healthy external services.

Counters are `accepted_mail`, `refused_mail`, `tls_sessions`, `plain_sessions`,
`limit_refusals`, `authentication_failures`, `dropped_logs`, `log_write_failures`.
Each is null if unavailable or an object with unsigned `value` and boolean
`saturated`. Mail counts describe local acceptance/refusal attempts, not unique
messages or relay acceptance. Session counters classify closed sessions as
defined above. Log counters use the queue/sink loss snapshot. A historical
failure counter is not an active condition: the runtime separately reports
whether logging or another dependency is currently degraded. Successful
recovery may clear that condition without clearing cumulative evidence.

Gauges are unsigned `active_slots`, `queue_depth`, `oldest_queue_age_ms`,
`unknown_outcomes`, `index_lag_transactions`, and signed
`certificate_expires_utc_ms`; each is null when unavailable. Active slots count
occupied runtime service slots; queue depth and unknown outcomes count retained
submissions, and oldest age measures the oldest pending submission. Index lag
is committed transactions not yet reflected by the derived index. Empty queues
have depth and oldest age zero when known. A known zero depth paired with a
known nonzero oldest age refuses with InvalidMetrics before formatting.
Other observations remain producer-owned facts. Unknown is never silently zero.
Missing metric observations alone do not override supplied local readiness.

Each disk object has unsigned `index` (0..15), nullable unsigned
`headroom_bytes`, `inodes`, and nullable unsigned `age_ms`. Inodes is an object
with `state` (`unknown`, `unsupported`, `available`) and `available` (null for
unknown/unsupported, unsigned for available). Known zero is distinct from a
failed or unsupported probe. Age is the observation age in milliseconds, not a
persisted monotonic tick. Disk records are emitted in caller order; duplicate,
out-of-range or more than 16 indices refuse before touching visible output.
Observed headroom is diagnostic only: a stale health snapshot cannot authorize
growth. The std-only runtime has no free-space/inode sampler: publish null
headroom/age and unknown inodes. These optional diagnostic fields do not
authorize writes; M19 owns the namespace index mapping.

Recommended actions are derived from supplied facts in fixed order:
`check_config`, `inspect_storage`, `inspect_listeners`, `inspect_admission`,
`inspect_recovery`, `inspect_relay`, `inspect_certificate`, `inspect_logging`,
`inspect_index`. Emit each only for its failed local flag, active recovery or
unknown/degraded dependency. They confer no repair permission and never embed
peer text or commands.

Status fits a 4 KiB encoded cache slot; the typed snapshot and 16 disk entries
fit a 2 KiB observation cell. Tests cover maximal fields and all local-flag
combinations. The full atomic encoder runs on the control worker, which owns
one inactive observation/output slot until its checked completion. Main keeps
serving the previous immutable encoded slot while an update waits behind
ACME/control work; a health poll never dispatches its own worker job. Completed
publication switches the current slot, without copying/re-encoding a full line
on main. Main transmits at most 2 KiB of cached control output per turn.

RESOURCES.md partitions the existing 16 KiB health region into two 4 KiB
encoded slots, two 2 KiB observation cells, 2 KiB emergency output and 2 KiB
ownership/pin descriptors. Pinned or worker-owned slots cannot be overwritten.
If no inactive slot is free, skip the update or refuse the bounded request;
never allocate another slot. Snapshot UTC and observation ages disclose cached
state; runtime freshness policy and checked publication/pins remain M19.

`encode_unavailable` constructs a minimal refusing/recovering status from a
fixed empty-metric/disk shape. It fits 2 KiB even with maximal context, so main
can report unavailable/stale/failed-worker health without waiting for control.
It never claims readiness or fabricates zero metrics. Its emergency output is
also exclusively owned/pinned; M19 must preserve that ownership when sending.

Encoding borrows bounded observations and caller output without allocating.
The complete JSON line appears or the prior visible output remains unchanged,
including invalid disk sets, inconsistent metrics and capacity errors. Offline
administrative commands may use their existing output scratch. Runtime
aggregation, cache ownership and synchronization remain M19; these helpers do
not establish a functioning health endpoint or measured service RSS.

## Initial receiving lifecycle command

The SMTP-only foreground profile has a separate schema-1 operator command
response for `status --json --runtime PATH`, specified in CONFIG.md. It
reports only the receiving Control lifecycle and explicitly names that
profile. It does not emit the full health envelope above, claim whole-service
readiness, or fabricate unavailable counters. Full status aggregation and
cached publication remain M19.

## PostgreSQL service logging

Schema 29 stores application events in `application_logs`, with serialized
admission metadata in `application_log_state`. `serve` starts one diagnostic
writer per instance. The default retention is **up to 30 days**, measured using
the database ingestion clock, subject to capacity. A fixed ring holds at most
65,536 rows, each with at most 1,024 event bytes (64 MiB of live event payload).
Capacity eviction is counted separately from suppression. This is diagnostic
history, not an immutable audit trail or a delivery receipt.

Producers accept only the typed event vocabulary, honor `minimum_severity`,
and use a 384-cell nonblocking queue. A fixed bucket admits at most one event
of each kind per minute; relay outcome classes and listener protocols have
separate buckets. Submission IDs and other peer-dependent values never allocate
buckets. Local counters saturate. One cumulative loss summary is attempted per
minute. The database serializes a shared limit of 600 events per minute across
all instances, retained across process restarts. Consequently, sustained traffic
can shorten the retained history; at the global maximum the ring retains about
109 minutes. Suppression and eviction are intentional and visible.

Writers flush at most 16 records per transaction with a two-second overall
DNS/connection/operation deadline, a 50 ms row-lock wait, and one-second statement
timeouts. The writer re-resolves the configured endpoint for every connection. Only a
transport failure before entering the append operation can try another address
within that same deadline. Logging uses asynchronous commit and never retries
an uncertain commit. Mail acceptance never depends on diagnostic persistence. Each idle
writer runs incremental age cleanup at least once per minute while operational,
deleting at most 1,024 expired rows per pass. Inspection hides all expired rows
immediately even when physical cleanup is behind or no service is running.

Before appending, the serialized writer measures both log relations, including
their indexes and TOAST. At 96 MiB it suppresses new records and metadata updates;
age deletion remains allowed. One bounded admitted batch may exceed that
threshold. This limits log-table growth even if vacuum cannot reclaim dead
versions. The physical-pressure losses remain in the process counter because
updating the database loss counter could itself grow the pressured relation.
Autovacuum is tuned for the ring. A relation that stays above the threshold
needs operator inspection of long transactions/vacuum; reclaiming it may need
`VACUUM FULL` during a suitable maintenance window.

This threshold is **not a PostgreSQL cluster disk quota**: WAL, archive slots,
backups, unrelated tables and filesystem headroom remain database operations.
Do not disable vacuum or let replication/archive retention grow without a disk
budget. `postgres logs` reports the measured relation size and threshold.

The current service producers cover boot/config activation, listener readiness,
orderly/failed lifecycle completion, SMTP metadata commit (with committed
transaction sequence), JMAP authentication/capacity refusals, and relay
connection/attempt failures (with submission ID and fixed stage/error codes),
and deferred local delivery notices (fixed error class, without a submission
ID when the notice producer has not returned one).
Connection/request correlation is not yet emitted by these producers; the
broader protocol/TLS/outcome event vocabulary above remains available to future
instrumentation. No peer SMTP text, email addresses, credentials, subjects or
message bodies enter these default records.

The writer remains alive through the normal service-worker drain, then performs
one bounded final batch and counts the discarded remainder. Logging failures do
not recursively log or write to potentially blocking stderr. The local private
control command `td-mta logging-status --runtime PATH` reads cumulative boot-local
`dropped`, `write_failures`, and the current `degraded` condition independently of
PostgreSQL. A failed writer stays degraded. Filtered severities are intentional
omissions, not dropped records. Abrupt process termination can lose queued
records and unpersisted counters.

`td-mta postgres logs --connection-file PATH --epoch HEX [--limit N]
[--before ID]` returns at most 100 records, newest first, with an exclusive
numeric cursor, database suppression and capacity-eviction counts. It uses the
same protected connection and schema/epoch checks as other operator commands.
The cursor does not pin a snapshot across requests: concurrent expiry/eviction
can create gaps. Returned JSON remains data, never executable instructions.

## Performance summaries

The service records fixed process-local counters at operation boundaries, never
per byte or poll. The counter table is bounded to 4 KiB (2,736 bytes on the
current x86-64 host); status encoding remains bounded to 16 KiB. After one initial minute, then hourly, the diagnostic writer
submits one `performance` event for each changed phase. Each phase has its own fixed rate bucket; neither
addresses, account IDs, object IDs, URLs nor SQL can create labels. The same queue,
severity filter, global ingestion limit, retention ring and physical-pressure
stop apply. There is no synchronous journal fallback and no per-request event
flood. These info records require `minimum_severity = info`. Additional summaries
can shorten history under the existing ring; 30 days remains an age ceiling,
not a guaranteed retention interval. At the current 38-phase ceiling, four
profiles add at most 3,648 hourly summary rows per day (plus initial snapshots).
The 65,536-row ring can hold about 18 days of those rows alone; other events,
frequent restarts and physical pressure can shorten that interval.

The four JMAP interpretation spans are sequential subspans of Email/get, not
additional time to add to that enclosing span. Each includes work-budget and
snapshot checks and any response-buffer flush occurring during it. Shared
setup and final flush/checks can fall outside these subspans. A completed
subspan can be counted successful even if a later phase or final publication
fails. Cache hits increment header_reuse and skip interpretation spans.
Timers start once per selected phase per message, never per scalar or parser
turn. They retain no account, message, subject or body data. Process-wide
samples still cannot attribute overlapping requests to a particular click.

Each sample is a cumulative process observation: `calls` counts finished spans,
`total_us` sums their monotonic elapsed microseconds and `max_us` is their largest
elapsed time. `failures`, `busy` and `unavailable` partition nonsuccess outcomes;
ordinary success is the remaining count. `lost` counts observations skipped by
nonblocking counter contention. All values saturate at u64::MAX, where they are
lower bounds. Samples are coherent per phase, not across phases. There is no
snapshot reset, so a later summary recovers cumulative observations across a
lost intermediate summary. Abrupt termination can lose the final partial hour;
a quiet phase whose last summary was lost need not produce another record.
Process counters survive in-process configuration reload; a new process starts
at zero. The event boot ID identifies a logger generation, so a new boot ID
requires a new baseline even when process counters survived a reload. They are diagnostic evidence, not an operation or delivery ledger.

For two records from one process and phase, subtract `calls` and `total_us` to
compute interval throughput and mean latency. `max_us` is a process maximum,
not a percentile or interval maximum. Do not calculate exact rates after lost
observations or saturation. Read records using `postgres logs` and its existing
pagination; the database profile and event boot/configuration metadata identify
the producer. No request identifier or mailbox name is recorded.


For immediate diagnostics, `td-mta performance-status --runtime PATH` reads the
current counters through the existing private mode-0600 local control socket.
It needs neither PostgreSQL nor info-level event logging and does not reset any
counter. Capture before and after the operation, seconds apart, to measure its
cost without waiting for historical persistence. It returns schema-1 JSON with
`command`, `status`, `boot_id`, `config_generation`, nullable `utc_ms` and a
`phases` object keyed by the closed names below. Every value is the same numeric
sample as an event; a contended/poisoned cell is null, never a fabricated zero.
The response is at most 16 KiB including LF, with unchanged two-second control
exchange limits. A service lacking a logging owner returns `unavailable`.
It is not exposed over public HTTP and supplies no mutation operation.

| Phase | Timed boundary |
| --- | --- |
| `http_session`, `http_api`, `http_download`, `http_upload` | Complete parsed head through final response flush, including authentication queue/lookup and worker waiting; excludes TCP/TLS and header arrival |
| `http_other` | Other parsed endpoints; event streams measure preparation only, not stream lifetime |
| `http_tls` | First observed TCP input through TLS handshake completion, including handshake admission waiting; excludes silent accepted sockets and TCP establishment. Retirement before completion counts as failure |
| `http_body_busy` | Zero-duration decisions refusing an upload, download or API request because the shared body/API slot is occupied; recorded before attempting the response |
| `http_auth_refused` | Zero-duration authentication-attempt admission refusals: per-peer/global rate bounds, full peer table or backwards admission clock; recorded before attempting the response |
| `http_auth` | One authentication worker attempt, including credential parsing and database authorization |
| `db_resolve` | The owned resolver helper and its complete reply |
| `db_connect` | One attempted PostgreSQL connection, including local TLS setup, TCP and startup; excludes DNS and runtime qualification |
| `db_tcp` | Numeric TCP connection attempt |
| `db_handshake` | PostgreSQL SSL negotiation, TLS and startup/SCRAM together |
| `db_tls` | TLS construction and handshake within `db_handshake` |
| `db_schema` | Version, migration-lock and schema-ledger qualification |
| `db_runtime` | Runtime role, privileges and maintenance qualification |
| `db_work` | One driver-run operation, possibly containing multiple SQL statements and qualification; success means the driver returned its operation result, not that an inner SQL/application result succeeded |
| `db_close` | Worker cleanup, including bounded Terminate/close progress; failure includes already-failed or deadline-retired sessions |
| `db_reuse` | Zero-duration retained authentication, relay idle-read, HTTPS API-read, download authorization and SMTP attempts, including failed turns |
| `relay_idle_wait` | Fully empty queue waits, including socket waits, fallback expiry and control interruptions; not database work |
| `relay_wake` | Zero-duration wake hints: success means a matching notification, unavailable means missing/retired subscription, failure means subscription transport/driver failure; each triggers a fresh scan |
| `body_verify` | Complete body chunk reads and digest verification |
| `chunk_verify` | Qualified Email/get batch SQL, length/ordinal and SHA-256 verification before bytes become readable; resident cache hits do not repeat it. |
| `body_replay` | Verified download callback, including chunk replay and socket output |
| `smtp_admission` | One storage-worker admission attempt, including database readiness; retry is busy |
| `smtp_data` | Successful DATA admission through recognition of its terminating dot, including the 354 reply, peer arrival and network scheduling; retirement or refusal before completion counts as failure |
| `smtp_commit_wait` | Completed DATA queued through the first storage-worker publication attempt; expiry or retirement before service counts as failure; excludes later busy retry delays |
| `jmap_email_get` | Email/get projection time after argument validation through retained result publication; failed JMAP methods count even when the enclosing HTTP response is 200 |
| `jmap_headers` | Per-message header scratch preparation and root-header decoding/serialization on a cache miss. Excludes prior body verification. |
| `jmap_structure` | MIME traversal, part metadata, locators, body-list classification and selected part JSON, before body values or preview. |
| `jmap_preview` | Preview selection, decoding, text extraction and JSON output for one message requesting a preview. |
| `jmap_body_values` | Body-value selection, decoding and JSON output for one message requesting bodyValues; includes all selected parts. |
| `header_reuse` | Completed copies of previously verified header projections into provisional response storage; zero-duration count, not body verification or final request success. |
| `summary_reuse` | Persisted common-header summaries copied into provisional response storage without body verification; zero-duration count. |
| `search_fields` | Lookup, bounded loading and validation of persisted search fields. Success means compatible fields were loaded; unavailable means parser fallback; failures mean storage access failed. It does not establish a successful query or body integrity. |
| `jmap_search` | Complete text-search page, including candidate selection, field lookup, full-body verification and matching or parser fallback. Failures include explicit work/deadline/interpretation refusals. Query text is not recorded. Nested phase times are not additive. |
| `structure_reuse` | Qualified persisted MIME tables reused after manifest qualification and verified entity-header rechecks; zero-duration count. Included work remains inside `jmap_structure`. |
| `jmap_work_io`, `jmap_work_records`, `jmap_work_output`, `jmap_work_deadline`, `jmap_work_other` | Email/get projections whose shared MIME work meter stopped, split by its sticky reason; duration is the whole projection. Other storage capacity failures do not increment these counters. No message IDs or content are retained. |
| `smtp_prepare` | Resident SMTP input validation, header preparation and digests for the complete account fanout; excludes DNS and database publication |
| `smtp_publish` | SMTP receipt inspection and database publication; one-shot paths include connection setup and pre-effect address fallback, while retained SMTP session setup is measured by `db_connect` inside `smtp_commit`; an unknown COMMIT outcome counts as failure without changing settlement or replay policy |
| `smtp_commit` | One storage-worker preparation/publication attempt; known durable success remains success even when ancillary cleanup stops service |

For timed HTTP endpoint spans, `busy` means a flushed 429 response and
`unavailable` means a flushed 503. The two zero-duration admission counters
record `busy` at the refusal decision even if the peer disconnects before
response completion. They explain these two causes, not every possible 429.
Other outer HTTP refusals, interrupted requests and unfinished spans count as
failures. A streamed API completion counts as success even for its HTTP400
request-problem response; this aggregate is not a complete HTTP status ledger.
HTTP success only establishes transport completion: JMAP method errors inside a
successful HTTP response remain JMAP errors. A failed response transmission does
not count as a flushed refusal. HTTP authentication busy retries count separately
from final HTTP responses. SMTP admission and commit spans exclude DATA
arrival and storage-worker queue wait; the separate DATA and initial commit
wait spans expose those boundaries. Admission success is permission to
receive DATA, not acceptance.
SMTP commit success reflects the retained durable outcome; failures include
indeterminate outcomes and do not authorize replay. Neither a counter nor a log
entry proves that the peer received its final reply. Database timings include the diagnostic writer's own
connections. Qualification/driver/connection and body/request spans nest: **do not
sum them as independent components**. Compare their counts and means to isolate
connection setup, repeated qualification, body work and waiting; subtract only
when the nesting and matching sample population are known.

Recording uses a try-lock on one fixed
cell, without allocation, I/O or waiting; contention loses only a diagnostic
observation. The logger retains one fixed previous-sample array. Neither metrics
nor their loss can renew a deadline, retry a mutation, hold account authority or
change a known publication result.
