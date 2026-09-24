# Bounded diagnostic records

M04d1 implements `src/observability.rs`: versioned event and explicit inspection
JSON Lines encoders. M04d2 adds fixed event buffering/loss counters and bounded
health/status snapshots in `observability/queue.rs` and `observability/health.rs`.
They borrow caller storage and allocate nothing. Runtime aggregation,
synchronization, log files/rotation and stderr fallback remain M19. These
records are diagnostics, never the authoritative
journal, an acceptance condition or an authorization token.

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

Runtime producers supply relevant correlations: mail/journal events use the
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
| `boot_started` | none | info | none |
| `config_activated` | none | info | none |
| `listener_ready` | `protocol` | info | none |
| `connection_opened` | `protocol`, `security` | info | none |
| `connection_closed` | none | info | none |
| `tls_established` | `protocol`, `security` (`tls12` or `tls13`) | info | none |
| `tls_failed` | `protocol` | warning | none |
| `mail_accepted` | unsigned `bytes` | info | none |
| `admission_refused` | `reason` | warning | mapped below |
| `journal_committed` | none | info | none |
| `checkpoint_selected` | unsigned `generation` | info | none |
| `relay_outcome` | `outcome` | info for accepted, otherwise warning | inspect_submission for permanent_failure/unknown |
| `authentication_failed` | none | warning | none |
| `certificate_renewed` | signed `expires_utc_ms` | info | none |
| `certificate_renewal_failed` | none | warning | inspect_certificate |
| `log_suppressed` | unsigned `count`, boolean `saturated` | warning | inspect_logging |
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
output. M04d2 implements counting; runtime aggregation/fallback remains M19.

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
physical growth. M05/M19 own sampling, index mapping and freshness reporting.

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
