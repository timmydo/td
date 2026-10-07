# Store application format registry

This is the normative application-byte companion to [STORAGE.md](STORAGE.md).
SQLite owns physical pages, WAL, transactions and recovery. The allocation-free
scalar, key, row and operation codecs define only bounded application values.
No custom FORMAT/CURRENT/table/manifest/journal container is emitted.

## 1. Conventions

Offsets are zero-based decimal byte offsets, never native Rust layouts.
`u16`, `u32`, `u64`, `i64` mean fixed-width little-endian integers. `be32`
means big-endian u32. `ID` is 16 opaque bytes. `H` is a 32-byte SHA-256
digest. Hash input is exactly the stated bytes, without implicit domain text,
alignment, terminators or padding. SQLite identifies its own physical containers. No digest is a credential or protection against a store writer.

`bytes(N)` is `u32 length` followed by that many bytes, at most N. `text(N)`
also requires UTF-8; N counts bytes, not Unicode characters. A presence or
boolean tag is exactly one byte, 0 or 1. An absent optional has no following
value. A collection is `u32 count` followed by exactly that many items under
its field-specific bound. Row semantics additionally constrain text, such as
address or keyword syntax. Generic scalar text does not normalize or sanitize.

Every top-level decoder consumes exactly its input. Extra bytes, unknown tags,
unsupported versions, nonzero reserved flags and malformed lengths are errors.
There are no skippable unknown fields or implicit default values. Row fields
are positional within schema 1, with field numbers for documentation;
they are not an extensible TLV protocol. A changed layout needs a new schema
and explicit migration. Reuse of a retired numeric tag is forbidden.

All times are signed i64 UTC milliseconds since the Unix epoch. Protocol date
validation and presentation are separate. Account sequence 0 denotes an
empty store; the first transaction is 1. Every later transaction is the exact
checked successor. On u64 exhaustion stop mutation, keep reads available and
require explicit migration to a fresh epoch. Never wrap or reset in place.
Temporary-file numbers are nonzero u64, exclusively created and rendered
as 20 zero-padded decimal digits. They convey no database generation.

The checked scalar readers borrow their input; writers use caller-owned
buffers. After any scalar codec error discard that cursor and its partial
output. Key encoding checks the entire key and output capacity before writing,
so it leaves output unchanged on error. Parsing a key grants no authorization
and does not prove that a referenced row exists.

## 2. Table and primary-key registry

Table tags are u16. A SQLite record and an application operation identify the table;
the primary-key bytes do not contain another table tag. Keys are unique and
sorted by unsigned bytewise comparison of their complete encoded form.
Logical ordering uses `Table::tag()`, never Rust enum declaration order.

| Tag | Logical table | Key bytes, in order | Exact / maximum bytes |
| ---: | --- | --- | ---: |
| 1 | `blobs` | blob ID | 16 |
| 2 | `mailboxes` | mailbox ID | 16 |
| 3 | `emails` | email ID | 16 |
| 4 | `memberships` | email ID, mailbox ID | 32 |
| 5 | `keywords` | email ID, nonempty UTF-8 keyword bytes, no inner length | 17..271 |
| 6 | `threads` | thread ID | 16 |
| 7 | `thread-anchors` | `text(1004)` header Message-ID, email ID | 21..1024 |
| 8 | `submissions` | submission ID | 16 |
| 9 | `recipients` | submission ID, `be32` recipient ordinal | 20 |
| 10 | `leases` | upload blob ID | 16 |
| 11 | `imports` | source instance ID, u8 source kind, source account bytes, source object bytes | 27..1024 |

For imports, source kind 1 is Mailbox and 3 is Email, from the shared
`ObjectType` registry used by CHANGE below. Other object types are not legal
import kinds. Both source identifiers
are nonempty length-prefixed byte strings. Their combined byte lengths are
at most 999: 16-byte instance + one-byte kind + two four-byte prefixes + IDs
fit the 1024-byte ceiling. `MAX_SOURCE_IDS_BYTES` is the combined 999-byte
bound; `MAX_SOURCE_ID_BYTES` caps either single nonempty ID at 998. Call
`Key::encoded_len()` to validate the pair; passing each individual bound is
insufficient. Source kind is part of identity: identical account/object bytes in
Mailbox and Email namespaces must remain different mappings. Invalid source
IDs are reported, never silently truncated or replaced with a digest.

Keyword raw bytes are capped at 255. A shorter nonempty keyword key is another
well-formed key, not evidence of a truncated record; the SQLite record extent supplies that evidence. Header-ID keys use the parsed bytes specified
in STORAGE section 3; framing does not implement the header parser. Length
prefixes remain little-endian even in keys. Their byte order is canonical,
not a promise of semantic lexical string order; ordered queries use their own
comparators. Recipient ordinals are the only big-endian numeric key field.

### Key and scalar oracles

Inline tests in `format/key.rs` pin every table's key layout against literal
components. The same source IDs under two kinds are distinct. Ordinal 255
sorts before 256. Maximum-size source and anchor keys encode to exactly 1024
bytes. Truncated fixed/length-prefixed keys, unknown kinds, trailing bytes,
invalid UTF-8, empty IDs and oversized keys refuse. Caller output remains
unchanged when it cannot hold a key.

The scalar golden stream is the following hex, with no spaces on disk:

```text
3412 78563412 0100000000000000 feffffffffffffff 01 02000000 c3a9
```

It encodes u16 0x1234, u32 0x12345678, u64 1, i64 -2, true and `text(2)`
containing U+00E9. The core tests compare the literal bytes in both directions.
They do not claim to test a cryptographic provider or durable filesystem I/O.

## 3. Application operation batches

A locally checked operation has a 12-byte prefix: u8 opcode, u8 action,
u16 table/object tag, u32 key length, u32 value length; then exact key and
value bytes. PUT is (1,0), DELETE (2,0), CHANGE (3,1..3) for created,
updated and destroyed. DELETE/CHANGE have no value; CHANGE has a 16-byte
object ID. Table and object tags use their separate fixed registries.
The store refuses Identity changes. Local decoding grants no authorization
or live-reference proof. SQL transaction validation checks final rows,
owning references, parent cycles and change-action existence.

At most 4096 operations and 1048576 encoded bytes form one caller batch.
Repeated row keys are applied in caller order; their last effect determines
final reference validation. Each changed object has at most one CHANGE.
A batch is not a disk journal format and contains no custom frame header,
footer, checksum, generation or replay descriptor.

## 6. Positional row values

The table tag selects the row; values carry no repeated kind/schema tag.
Numbered fields below are concatenated in order without padding. `?T` means
one 0/1 presence byte followed by T only when present. For example,
`?bytes(N)` is 0 alone, or 1 followed by u32 length and payload. `Row::decode` checks
local field rules and full consumption; `Row::validate_key` additionally checks
matching table, direct self-parenting, and the import-kind/blob-presence rule.
It shares `Key::validate_local` with DELETE for canonical keywords and recipient
ordinals below 1000. Key encode/decode alone checks structural grammar. The shared
`decode_record(table, key, value)` entry point calls both; service, inspection,
verification and migration use it for stored records and PUTs. Neither checks live references, authorization, complete SMTP
address grammar, or queue transitions; those belong to the transaction/protocol
validators. A checksum alone never substitutes for those validators.

All text below forbids Unicode control characters. Addresses are ASCII with
at most 254 bytes; reverse-path may be empty, recipient addresses may not.
SMTP grammar and configured identity permission are additional checks, not
properties inferred from this framing codec. No input is silently truncated.
Blob length is u64 on disk; configured admission limits apply to new bodies,
not to the ability to read previously accepted bodies after limits decrease.

| Table | Ordered fields |
| --- | --- |
| blobs | 1 kind:u8; 2 length:u64; 3 raw-file SHA-256:H; 4 createdAt:i64 |
| mailboxes | 1 name:text(1024); 2 parent:?ID; 3 role:?text(64); 4 sortOrder:u32; 5 subscribed:bool |
| emails | 1 blob:ID; 2 thread:ID; 3 receivedAt:i64; 4 origin:u8; 5 receipt only for SMTP origin |
| memberships | Empty value; the key is the complete relationship |
| keywords | Empty value; the key is the complete set member |
| threads | Empty value; the key is the persisted grouping identity |
| thread-anchors | Empty value; the key is the complete header-ID relationship |
| submissions | 1 email:ID; 2 thread:ID; 3 identity:ID; 4 transmittedBlob:ID; 5 reversePath:text(254); 6 sendAt:i64; 7 expiresAt:i64; 8 recipientCount:u32; 9 completedAt:?i64; 10 notification:u8; 11 notificationEmail:?ID |
| recipients | 1 address:text(254); 2 state:u8; 3 uncertain:bool; 4 attempt:?ID; 5 attemptCount:u32; 6 lastAttemptAt:?i64; 7 phase:u8; 8 nextAttemptAt:?i64; 9 rcptReply:?text(4096); 10 dataReply:?text(4096); 11 reason:u8; 12 diagnostic:text(512) |
| leases | 1 account:ID; 2 device:ID; 3 expiresAt:i64; 4 uses:u8 |
| imports | 1 localObject:ID; 2 historicalBlob:?ID; 3 sourceDigest:H |

Mailbox name is nonempty; role, when present, is nonempty lowercase ASCII
letters/digits/hyphen. Role registry membership and uniqueness are checked at
configuration/mutation time. `sortOrder` is below 2^31. Stored keyword keys are
1..255 ASCII bytes, 0x21..0x7e excluding `(`, `)`, `{`, `]`, `%`, `*`, double
quote and backslash, with no uppercase letters. Normalize JMAP keyword input
to lowercase before key construction; decoding does not normalize stored data.

SMTP receipt fields, in order: peer-family:u8 (4 or 6), peer-octets (4 or 16
network-order bytes), gateway:?text(64), TLS:u8, EHLO/HELO:text(255),
reversePath:text(254), recipients. Gateway and EHLO/HELO are nonempty when
present. Peer preserves the socket address form, including IPv4-mapped IPv6 without
normalizing it to family 4; admission/allowlist comparison must independently
canonicalize addresses where required. Gateway is an admitted configured
identity, never an untrusted header. Recipients are u32 count followed by that
many nonempty address text fields. Count is 1..1000 and the entire count-plus-
addresses encoding is at most 32768 bytes. Both limits apply independently:
1000 maximum-length addresses cannot fit. RCPT admission reserves these bytes
before accepting each address; the SMTP engine may temporarily refuse further
recipients when the receipt budget is exhausted. Alias deduplication affects
local delivery, not the list of accepted envelope recipients. Values are
borrowed from caller storage, including the collection; iteration allocates
nothing. The collection's private representation is validated on construction.

Submission expiry applies to every recipient and must be at or after sendAt;
it is not duplicated per recipient. Recipient count is 1..1000; ordinals must
cover exactly 0..count in the final transaction view. completedAt records when
all recipients become resolved; unresolved outcomes prevent ordinary retention
expiry. Wall-clock movement may make completedAt earlier than sendAt, so the
codec does not impose chronological ordering on observations. Stored
notification requires notificationEmail; other notification states forbid it.
The notification ID is historical and does not keep a deleted email alive.

Attempt ID is a random 128-bit identifier shared by recipients in one network
attempt. Increment attemptCount with checked arithmetic; exhaustion refuses a
new attempt. Nonzero count requires attempt, lastAttemptAt and a non-None
phase; zero count forbids all three. Replies are optional, nonempty when
present, normalized single-line SMTP replies, separately preserving RCPT and
message-level results. dataReply also holds an actual negative MAIL/DATA-
command refusal under QUEUE.md's attribution rules; positive MAIL/interim
354 replies never substitute for final DATA acceptance. Full reply syntax, multiline normalization and attribution are
M02c/M17 contracts; the codec enforces bounds and rejects controls. Local
failures use reason/diagnostic and must not invent an SMTP response. Callers
normalize multiline replies per RFC 8621 and replace/escape control characters
in local diagnostics before constructing rows; do not copy raw OS error text
or wire lines directly into these fields.

`uncertain` latches possible acceptance across attempts; a retry must not erase
it. OutcomeUnknown requires it and Canceled forbids it. Persist phase
AcceptancePossible before emitting the DATA terminator; a crash in that phase
cannot prove nondelivery. A later attempt's rejection cannot prove that an
earlier uncertain attempt was not accepted. M02c owns the transition table,
recovery and JMAP mapping; field decoding alone permits states whose transition
would be illegal. No caller may infer queue validity from successful decoding.

### Enum tags

| Field | Tags |
| --- | --- |
| Blob kind | Message=1, Upload=2 |
| Email origin | SMTP=1, JMAP=2, Import=3, FailureNotice=4 |
| Receipt TLS | Plain=0, TLS1.2=1, TLS1.3=2 |
| Recipient state | Queued=1, InFlight=2, RetryWait=3, Accepted=4, Failed=5, Canceled=6, OutcomeUnknown=7 |
| Attempt phase | None=0, Prepared=1, Body=2, AcceptancePossible=3, Final=4 |
| Notification | None=0, Pending=1, Stored=2 |
| Lease uses | Import=1, Attachment=2, Both=3 |
| Failure reason | None=0, SmtpTemporary=1, SmtpPermanent=2, Network=3, Tls=4, Authentication=5, Expired=6, Canceled=7, Protocol=8, Uncertain=9 |

Lease uses is an exact enum, not extensible flags. Unknown values in every
enum refuse. Import Mailbox rows have no historicalBlob; Email rows require
one. Historical IDs do not pin objects. Source digest is SHA-256 over the
canonical source snapshot defined below, not a digest of local IDs or JMAP
JSON spelling. Import's source kind, instance/account/object IDs stay in its
key; compare key and digest together to resume an identical object.

### Import source snapshot preimages

These preimages are a migration interchange contract; the M21 exporter/importer
will implement them. All lengths/counts use the conventions in section 1.
Mailbox name, role and sortOrder have the same constraints as local rows;
RFC 8621 section 2 requires sortOrder below 2^31 despite the broader generic
UnsignedInt range. Unrepresentable source fields are reported, not truncated:

- Mailbox: u8=1, name:text(1024), parent:?bytes(998), role:?text(64),
  sortOrder:u32, subscribed:bool. Parent is the original source object ID,
  nonempty when present, not its local mapping.
- Email: u8=3, raw-message SHA-256:H, raw length:u64, receivedAt:i64,
  u32 mailboxCount, each source mailbox ID as nonempty bytes(998),
  u32 keywordCount, each canonical keyword as text(255).

Mailbox IDs are sorted by unsigned bytewise source-ID comparison, keywords by
ASCII byte comparison; duplicates are forbidden. Counts are at most 4096 each,
mailboxCount is at least one. These are streaming digest preimages, not row
values or in-memory lists, and may exceed 64 KiB. Export sorts using bounded
scratch/external sorting. Digest equality does not override limits on the
local transaction: an object whose relationships exceed the frame/reservation
budget is reported unrepresentable, never split into partial visible state.
Absent role and parent remain distinct from invalid empty values. No local
thread assignment, cache, SMTP receipt, or export time enters this digest.

## 7. Oracles

format_operations and format_rows exercise literal application values and
rejections; inline scalar/key tests pin numeric and primary-key encodings.
Actual SQLite and immutable-file tests exercise persistence separately.
A row codec does not establish SQLite integrity, authorization or durability.
