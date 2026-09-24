# Bounded diagnostic records

M04d1 implements `src/observability.rs`: versioned event and explicit inspection
JSON Lines encoders. They borrow caller output and allocate nothing. Runtime
aggregation, status, fixed queue ownership, log files/rotation and stderr fallback
are separate increments. These records are diagnostics, never the authoritative
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
| `record` | `event` or `inspection` |
| `boot_id` | 32 lowercase hex digits identifying this process boot |
| `utc_ms` | Signed UTC milliseconds since Unix epoch; null if unavailable |
| `config_generation` | Unsigned effective config generation; zero before activation |
| `connection_id` | Optional nonzero process-local connection number |
| `request_id` | Optional nonzero process-local request number |
| `transaction_sequence` | Optional unsigned committed transaction sequence |
| `submission_id` | Optional 32-digit lowercase hex submission ID |
| `facts` | Trusted fixed schema values and typed numeric facts |
| `recommended_actions` | Array of fixed advisory codes; never executable commands |
| `untrusted` | Array: empty for events; one labeled source-text object for inspection |

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
output. Aggregation/fallback behavior belongs to M04d2/M19.

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
