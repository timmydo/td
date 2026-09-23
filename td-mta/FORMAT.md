# Store format v1 registry

This is the normative byte-layout companion to [STORAGE.md](STORAGE.md).
M02a and M02b implement allocation-free scalar, primary-key and row codecs in
`src/format/`, with literal row/container fixtures. Container integrity
validation, publication and recovery are unimplemented. M02c freezes semantic
APIs. Production persistence waits for those contracts
and the M05 store implementation. Nothing here advertises a usable store.

## 1. Conventions

Offsets are zero-based decimal byte offsets, never native Rust layouts.
`u16`, `u32`, `u64`, `i64` mean fixed-width little-endian integers. `be32`
means big-endian u32. `ID` is 16 opaque bytes. `H` is a 32-byte SHA-256
digest. Hash input is exactly the stated bytes, without implicit domain text,
alignment, terminators or padding. The eight-byte magic already identifies
each container. No digest is a credential or protection against a store writer.

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
validation and presentation are separate. Checkpoint sequence 0 denotes an
empty store; the first transaction is 1. Every later transaction is the exact
checked successor. On u64 exhaustion stop mutation, keep reads available and
require explicit migration to a fresh epoch. Never wrap or reset in place.
Generation and segment numbers are nonzero u64, independently allocated using
exclusive creation. Their exhaustion likewise refuses publication. Numeric
path components are 20 zero-padded decimal digits; shortened examples in
STORAGE.md are illustrative, not alternative encodings.

The checked scalar readers borrow their input; writers use caller-owned
buffers. After any scalar codec error discard that cursor and its partial
output. Key encoding checks the entire key and output capacity before writing,
so it leaves output unchanged on error. Parsing a key grants no authorization
and does not prove that a referenced row exists.

## 2. Table and primary-key registry

Table tags are u16. A table file and a journal operation identify the table;
the primary-key bytes do not contain another table tag. Keys are unique and
sorted by unsigned bytewise comparison of their complete encoded form.
Descriptor ordering uses `Table::tag()`, never Rust enum declaration order.
M05 supplies the path mapping from these tags to the fixed filenames below.

| Tag | Table filename | Key bytes, in order | Exact / maximum bytes |
| ---: | --- | --- | ---: |
| 1 | `blobs.tbl` | blob ID | 16 |
| 2 | `mailboxes.tbl` | mailbox ID | 16 |
| 3 | `emails.tbl` | email ID | 16 |
| 4 | `memberships.tbl` | email ID, mailbox ID | 32 |
| 5 | `keywords.tbl` | email ID, nonempty UTF-8 keyword bytes, no inner length | 17..271 |
| 6 | `threads.tbl` | thread ID | 16 |
| 7 | `thread-anchors.tbl` | `text(1004)` header Message-ID, email ID | 21..1024 |
| 8 | `submissions.tbl` | submission ID | 16 |
| 9 | `recipients.tbl` | submission ID, `be32` recipient ordinal | 20 |
| 10 | `leases.tbl` | upload blob ID | 16 |
| 11 | `imports.tbl` | source instance ID, u8 source kind, source account bytes, source object bytes | 27..1024 |

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
well-formed key, not evidence of a truncated record; outer record framing and
checksum supply that evidence. Header-ID keys use the parsed bytes specified
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

## 3. Fixed containers

Every container starts with its listed eight-byte ASCII magic. Common fields
at offsets 8, 10 and 12 are container version u16=1, schema u16=1, and flags
u32=0, except the table header's explicit table/flags pair. Flags cannot turn
off integrity checks. Numeric sizes below include all digest bytes.

### FORMAT: 80 bytes

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | `TDMTAFMT` |
| 8 | 2 | Container version |
| 10 | 2 | Store schema |
| 12 | 4 | Reserved flags |
| 16 | 16 | Instance ID |
| 32 | 16 | Store epoch |
| 48 | 32 | SHA-256 of bytes 0..48 |

FORMAT is a whole-service identity, not a checkpoint selector. Restore changes
the epoch before any listeners open; selected account metadata must match it.

### CURRENT: 120 bytes

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | `TDMTCUR1` |
| 8 | 2 | Container version |
| 10 | 2 | Store schema |
| 12 | 4 | Reserved flags |
| 16 | 16 | Account ID |
| 32 | 16 | Store epoch |
| 48 | 8 | Selected generation |
| 56 | 32 | SHA-256 of the **entire** selected manifest, including its footer |
| 88 | 32 | SHA-256 of bytes 0..88 |

Only CURRENT chooses a generation. A valid newer unselected directory cannot
override it. Every selected account, epoch and generation must match its
manifest and tables. Invalid CURRENT is a diagnosis/repair condition.

### Table header: 112 bytes

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | `TDMTTBL1` |
| 8 | 2 | Container version |
| 10 | 2 | Row schema |
| 12 | 2 | Table tag |
| 14 | 2 | Reserved flags, zero |
| 16 | 16 | Account ID |
| 32 | 16 | Store epoch |
| 48 | 8 | Generation |
| 56 | 8 | Checkpoint through-sequence C |
| 64 | 8 | Record count |
| 72 | 8 | Payload byte length |
| 80 | 32 | SHA-256 of bytes 0..80 |

The payload begins at 112. Exact file size is 112 + payload length; no trailer
follows the last record. Manifest descriptors hash the entire file. A record
is `u32 key_len, u32 value_len, u64 last_change, key, value, H`, with H covering
the preceding 16-byte record prefix plus key and value. Key length is 16..1024
and then must satisfy its table's stricter key grammar; value length is
0..65536 subject to the row schema. Last-change is 1..C. An empty initial
table has C=0, count=0 and payload length=0. Duplicate/out-of-order keys fail
verification. The 48-byte record overhead is independent of its row kind.

### Journal header: 96 bytes

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | `TDMTJNL1` |
| 8 | 2 | Container version |
| 10 | 2 | Store schema |
| 12 | 4 | Reserved flags |
| 16 | 16 | Account ID |
| 32 | 16 | Store epoch |
| 48 | 8 | Segment number |
| 56 | 8 | Base sequence |
| 64 | 32 | SHA-256 of bytes 0..64 |

The active journal's base equals its selecting checkpoint's C. Frames start
at offset 96, with first sequence C+1. Header bytes are outside the 4 MiB
committed-frame budget. History segments are sealed immutable files and obey
their manifest descriptors. An active journal has no preallocated disk tail.

## 4. Transaction framing

| Header offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | `TDMTFRM1` |
| 8 | 2 | Container version |
| 10 | 2 | Store schema |
| 12 | 4 | Reserved flags |
| 16 | 4 | Total frame length, including 64-byte header and 40-byte footer |
| 20 | 4 | Stored operation count, including change descriptors |
| 24 | 8 | Transaction sequence |
| 32 | 32 | SHA-256 of bytes 0..32 |

Read exactly the 64-byte header and verify its digest before trusting lengths.
Total length is 132..1048576: 104 bytes of header/footer plus at least a
12-byte operation header and 16-byte key. Count is 1..4096. Read the bounded payload
and 40-byte footer: eight-byte `TDMTEND1`, then SHA-256 of the entire header,
payload and end magic. The footer digest excludes itself. Verify both hashes,
sequence continuity, count, all operations and final-row invariants before
publishing a recovered frame. Checksummed arbitrary bytes are not valid rows.

Every operation begins with the following 12 bytes, then key and value:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 1 | Opcode: PUT=1, DELETE=2, CHANGE=3 |
| 1 | 1 | Action: zero for PUT/DELETE; created=1, updated=2, destroyed=3 for CHANGE |
| 2 | 2 | Table tag for PUT/DELETE; object-type tag for CHANGE |
| 4 | 4 | Key length |
| 8 | 4 | Value length |

PUT carries a complete bounded row value. DELETE carries no value. Their row
last-change sequence comes from the frame, not another embedded field. CHANGE
uses a 16-byte object ID as key and no value; its type tags are Mailbox=1,
Thread=2, Email=3, Identity=4, EmailSubmission=5. Unknown tags fail closed.
Both row operations and CHANGE count toward frame/journal operation caps.
CHANGE is evidence for state APIs, not a row mutation; M02c defines required
coalescing from the final transaction view. A transaction may affect several
JMAP types. Repeated writes to a key retain stored ordinal order; the final
operation wins. Byte/operation reservations include the entire frame and all
descriptors, preventing apparently small metadata changes from exceeding caps.

The complete-header/short-body distinction and every sync boundary are owned
by STORAGE sections 5-6. A checksum-invalid complete final frame is corruption,
not a recoverable incomplete tail. No forward magic search is permitted.

## 5. Manifest

The fixed prefix is 88 bytes:

| Offset | Width | Field |
| ---: | ---: | --- |
| 0 | 8 | `TDMTMAN1` |
| 8 | 2 | Container version |
| 10 | 2 | Store schema |
| 12 | 4 | Reserved flags |
| 16 | 16 | Account ID |
| 32 | 16 | Store epoch |
| 48 | 8 | Generation |
| 56 | 8 | Checkpoint through-sequence C |
| 64 | 8 | Active segment number |
| 72 | 8 | Active segment base, equal to C |
| 80 | 4 | Table descriptor count, exactly 11 |
| 84 | 4 | Retained history descriptor count, 0..64 |

Then exactly 11 table descriptors in ascending table-tag order, 56 bytes each:
`u16 table, u16 row_schema=1, u32 flags=0, u64 record_count, u64 file_bytes,
H whole_file`. Table names are derived from the registry; the manifest cannot
supply paths. Header and descriptor counts/schema must agree. Then the history
descriptors, 64 bytes each: `u64 segment, u64 base, u64 through, u64 file_bytes,
H whole_file`. Each is nonempty with through > base. They are in increasing
sequence order, contiguous (each base equals previous through), ending at C
when any history is retained. They may start after zero because history expires.
No selected segment number repeats or equals the active segment. Every header
and validated frame range must agree with its descriptor. History files remain
pinned for readers even after a later manifest drops them.

The final 32 bytes are SHA-256 of all preceding manifest bytes. Exact total
length is `88 + 11*56 + history_count*64 + 32`, at most 4832 bytes. CURRENT
hashes that complete file, including this digest. The active journal's changing
extent is never given a fictitious immutable whole-file digest; read views
capture its committed offset/sequence under the publication lock instead.

## 6. Positional row values

The table tag selects the row; values carry no repeated kind/schema tag.
Numbered fields below are concatenated in order without padding. `?T` means
one 0/1 presence byte followed by T only when present. For example,
`?bytes(N)` is 0 alone, or 1 followed by u32 length and payload. `Row::decode` checks
local field rules and full consumption; `Row::validate_key` additionally checks
matching table, canonical keywords, direct self-parenting, recipient ordinal
below 1000, and the import-kind/blob-presence rule. The shared
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
DATA results. Full reply syntax, multiline normalization and attribution are
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

## 7. Oracles and implementation boundary

`tests/format_rows.rs` compares all table value types against checked-in
literal hex fixtures, in both directions, including options, origins, IPv4
and IPv6. It refuses every truncated prefix and appended byte and verifies
that insufficient output leaves caller storage unchanged. Both encoding and
decoding borrow/use caller storage; only the tests allocate their corpora.
The writer's internal measuring mode shares the validated field emitter with
encoding, so whole-row capacity checks cannot drift from an independent size
formula. Decode enforces the same local rules as encode.

`tests/fixtures/format-v1/README.md` defines the complete container oracles and
preimages. They were calculated independently of the Rust codecs with Python
struct/hashlib during development; Python is not a build/runtime dependency.
M02b tests row bytes and fixture structure, not SHA-256 or crash durability.
M02c supplies state, queue and API meanings. M05 verifies SHA-256 through the
reviewed provider, implements exact encoders/decoders for containers, validates
cross-file bindings and exercises fault I/O. M07 provider tests independently
hash the golden preimages. A successful row test must never be reported as a
successful integrity, replay, synchronization or crash-recovery test.
