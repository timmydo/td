# Store format v1 registry

This is the normative byte-layout companion to [STORAGE.md](STORAGE.md).
M02a implements only the allocation-free scalar and primary-key codecs in
`src/format/`. Container integrity validation, publication and recovery are
unimplemented. M02b completes row values and full golden container fixtures;
M02c freezes semantic APIs. Production persistence waits for those contracts
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
will be positional within schema 1, with field numbers for documentation;
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

## 6. Implementation boundary

M02b supplies the positional value registry, their field bounds and complete
row/container golden fixtures against these envelopes. M02c supplies state,
queue and API meanings. M05 verifies SHA-256 through the reviewed provider,
implements exact encoders/decoders for containers, validates cross-file bindings
and exercises fault I/O. M07 provider tests independently hash the golden
preimages. A successful scalar/key test must never be reported as a successful
integrity, replay, synchronization or crash-recovery test.
