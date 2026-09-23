# JMAP wire contracts

This companion freezes protocol representations separately from the disk
format. The identifier/locator codecs in `src/wire.rs` are implemented;
endpoints, authorization, MIME parsing and capability advertisement remain
unimplemented. Normative protocol references are [RFC
8620](https://www.rfc-editor.org/rfc/rfc8620.html), [RFC
8621](https://www.rfc-editor.org/rfc/rfc8621.html) and [RFC
2046](https://www.rfc-editor.org/rfc/rfc2046.html). M02c's later contract
increments and owning protocol milestones must finish before the service can
publish a JMAP capability.

## 1. IDs

RFC 8620 section 1.2 permits 1..255 ASCII alphanumeric, hyphen and
underscore bytes. The borrowed `Id` parser enforces that grammar, not our
local allocation scheme. A syntactically valid foreign-format ID is an
absent local object; methods report their standard notFound result rather
than invalidArguments. Creation references beginning with `#` are resolved
by the method dispatcher before interpreting the referenced local ID.
Request call IDs are Strings, not this Id type; never apply the ID grammar
to every protocol string.

Locally generated object IDs have exactly 33 bytes: one lowercase prefix
then 32 lowercase hexadecimal digits of the stored 16-byte ID, in byte
order. Prefixes are Account `a`, Mailbox `m`, Email `e`, Thread `t`, file
Blob `b`, Identity `i`, EmailSubmission `s`. Their wire spellings are
distinct across kinds and from purely numeric values. The sealed `LocalId`
trait attaches each prefix to its existing storage ID type:
`encode_id(email, output)` cannot select a mailbox prefix, and
`Id::local::<EmailId>()` returns an `EmailId`. The public API does not
accept a caller-selected kind paired with untyped storage bytes. Allocation
still uses the entropy adapter plus exclusive collision detection; these
codecs generate no randomness. Wire IDs are stable across restarts and
retained when restoring a store. Epoch changes invalidate synchronization
states, not stable object identities.

`Id::local::<T>()` returns None for a different prefix, width, or
noncanonical hex, even if the string remains a valid generic JMAP ID. Do not
lowercase incoming IDs, strip prefixes permissively, or accept storage hex
as a second local wire spelling. Parsing an ID never authorizes its account
or proves it exists. No wire string or caller-supplied filename becomes a
filesystem path. `Id::blob()` classifies whole-file and supported part forms
and returns None for every other syntactically valid ID, including
unsupported locator versions or tags, uppercase hex and overflowing extents.
Blob lookup maps that absence to notFound (HTTP downloads to 404). Do not
map the low-level locator decoder error to invalidArguments after the
generic Id grammar has already passed.

For example, stored Email bytes 00..0f encode as:

```text
e000102030405060708090a0b0c0d0e0f
```

## 2. MIME part blob locators

File blob IDs above name immutable whole files. A direct part blob ID is a
distinct 69-byte form, all in the generic JMAP ID alphabet:

```text
p1_ PPPPPPPPPPPPPPPPPPPPPPPPPPPPPPPP OOOOOOOOOOOOOOOO LLLLLLLLLLLLLLLL EE
```

Spaces above are visual separators and are absent on the wire. P is the
parent's 16 raw blob-ID bytes as 32 lowercase hex digits, O and L are
unsigned u64 encoded-body offset and length as 16 lowercase hex digits each,
most significant digit first, and E is a one-byte encoding tag as two hex
digits. Tags: 00 identity, 01 base64, 02 quoted-printable. Unknown
content-transfer encodings use JMAP's identity-decoding rule and tag 00;
their original header remains available. Future locator versions/tags cannot
be silently reinterpreted. Issued blob IDs always identify the same decoded
octets, including malformed base64/QP handling. M02c3 freezes that decoding
policy before any IDs are issued. Later parser/decoder changes that alter
bytes require a new locator version and preservation of the old version's
decoding for its live parents; they cannot assign different contents to an
old ID.

The locator ranges over the encoded body bytes in the immutable parent file,
excluding MIME delimiters. In particular, the CRLF immediately preceding a
multipart boundary belongs to the delimiter under RFC 2046 section 5.1.1;
exclude those two bytes from the preceding part's length. For a
non-multipart root body, ordinary final body bytes including a final CRLF
remain included. Decoded lengths do not enter the locator. Offset plus
length must not overflow u64. Zero length is legal at a valid body boundary.
`checked_end(parent_length)` also refuses an extent past the file; that
range check alone is insufficient authorization or proof of a MIME part.

Resolve only against a live parent accessible in the request's account/view,
or an authorized unexpired upload lease when parsing an uploaded raw
message. The MIME parser must reproduce an exact descriptor match for the
parent, offset, length and encoding. Multipart containers have null blobId
under RFC 8621 section 4.1.4 and cannot authorize a locator; only parts with
a non-null blobId may do so. A forged in-range substring is not a valid
part. The caller keeps the parent pinned for the entire
decode/download/attachment reuse operation. Device revocation and mutation
authorization are rechecked at their normal commit boundary. Never interpret
the locator as an arbitrary seek request on any file the service can open.

The MIME part ID contains no independent stored blob and adds no owning
reference. A generated structured message that reuses it copies/decodes the
part into its own immutable bytes before commit. Deleting the old parent may
invalidate the locator once no live object/lease/view still protects it.
Malformed transfer encoding gets the JMAP parsing/encoding outcome specified
by the MIME contract; the locator codec itself never decodes message
content.

## 3. Nested attached messages

Email/parse may parse any authorized part blob as an RFC 5322 message,
including a transfer-encoded attached .eml file with
application/octet-stream type. Its inner leaves need blob IDs even though
their offsets may not exist in the original encoded file. Do not return
notParsable merely because the source was a part or used a known transfer
encoding.

The nested form is `p2_`, the same 32-digit root file ID, then 2..6
consecutive steps. Each step is O(16 hex), L(16 hex), E(2 hex) exactly as
above. There is no count byte: the remaining length must be an exact
multiple of 34. Total wire length is 35 + 34*steps, from 103 through 239
bytes, within the generic 255-byte Id ceiling. A one-step `p2_` form is
invalid; use `p1_` instead. `NestedLocator` stores six fixed step cells,
never a heap list. Constructors check count and overflow; decode rejects
partial steps and unknown tags.

Step one selects and transfer-decodes a leaf in the original whole-file
message. To validate each next step, parse the preceding decoded blob as a
fresh message under the same MIME limits, then require an exact leaf
descriptor match in that decoded stream before applying the next transfer
decoding. Offsets and lengths are relative to each preceding decoded stream,
not the original file. Range checks apply separately at every stage. A
message/rfc822 attachment remains a leaf in its containing Email's
bodyStructure; descendants are validated by this separate message parse, not
by pretending they were parts of the outer Email.

Use this form for identity-encoded attached messages too; do not flatten
their offsets and lose the parser context. A nested parse producing another
message's parts appends one step. The MIME implementation must determine
before publishing the parse result whether all required leaf IDs fit.
Parsing a six-step source that needs another step reports that source as
notParsable under the documented nesting limit, with a local diagnostic. It
must not return a successful partial Email with missing required blob IDs.
The existing source blob remains downloadable. This depth is independent of
multipart depth inside each message.

Resolution is a bounded streaming decode/parse pipeline. It must not create
authoritative decoded files or allocate one message-sized buffer per step.
Each repeated parsing/decoding pass consumes the M02c3 operation work
budget; exhaustion produces a method/resource error, never a fabricated
descriptor match. Root authorization/lease validity and the root file pin
cover the complete chain. No locator grants new rights at an intermediate
step.

M06/M14 fixtures must parse an identity message/rfc822 attachment and a
base64/QP application/octet-stream .eml attachment through Email/parse,
download their returned inner leaf IDs, and compare independent expected
octets. Include bad intermediate offsets, wrong encoding, foreign/expired
parents and the six-step limit. The codec's literal step tests do not claim
that this parser/resolver is implemented.

## 4. Evidence and remaining work

Tests compare literal wire forms, exact output capacities, syntax classes,
all transfer tags, nested steps/depth, truncated/trailing forms, canonical
hex and checked ranges. All 256 byte values are compared against the
existing storage-ID display spelling. Output refusal leaves caller storage
unchanged. Production codecs allocate no memory and use checked
slicing/arithmetic. These tests prove neither parent access checks nor
parser matching; M06/M13-M15 must exercise those with actual MIME and
account fixtures. API.md now owns state/error mappings and adapter contracts;
QUEUE.md owns queue transitions. Work budgets and the remaining M02c wire
fixture inventory belong to M02c3.
