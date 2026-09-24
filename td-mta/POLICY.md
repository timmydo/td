# Message interpretation and query policy

This freezes the remaining M02 policies for M06, M08 and M13-M16. It does not
implement a MIME parser or JMAP handler. [CONFORMANCE.md](CONFORMANCE.md) owns
the complete standard surface, [WIRE.md](WIRE.md) owns stable blob locators,
[UNICODE.md](UNICODE.md) owns Unicode inputs, and [CASES.md](CASES.md) owns
the acceptance case inventory. [RFC
8621](https://www.rfc-editor.org/rfc/rfc8621.html) defines property
shapes/defaults; the policies here select implementation choices and recovery
behavior within those contracts.

## 1. Bounds, raw preservation and errors

Offsets and lengths are checked u64 values; only bounded chunks become usize.
Every scan, replay, decode and comparison consumes ADMISSION.md work. Parsed
values stream to admitted output; no Vec or String grows with a message or
mailbox. Header source bytes use the existing header arena. MIME descriptor
cells remain at most 64 bytes; variable strings are source extents, not
strings owned per descriptor. RESOURCES.md's six decode rings,
parser/conversion scratch and fixed sort buffers cover the operations below.

header_bytes bounds the sum of all entity headers in one parse, including the
root exactly once. SMTP can check that root alone before later MIME work; this
does not grant a second header arena or a second allowance. Each fresh
attached-message parse applies the same aggregate bound. mime_depth counts the
root as one; mime_parts includes containers and leaves. The six locator stages
are a separate bound. Nesting in comments/quoted constructs is capped at 32; a
parser uses explicit frames. Header field names have the RFC
printable-ASCII-except-colon grammar; accept RFC 5322 obsolete SP/HTAB between
the name and colon without including that whitespace in the reported field
name. A line need not fit an I/O chunk. Boundary values follow RFC 2046's
1..70 character grammar. Parameter/field/address/ID lists stream within
header_bytes; there is no unbounded heap list and no silent prefix accepted as
a complete property.

Malformed input and resource failure are distinct. For raw reception, root
headers exceeding header_bytes are a declared message-size refusal before
acceptance (SMTP 552 5.3.4). Resource exhaustion is temporary. A malformed
header line is retained in raw bytes. The first line that is neither a field
nor a continuation ends headers and begins the body at that line's first byte;
a continuation without a preceding field likewise begins the body. An empty
line ends headers. EOF ends a recognizable header block with an empty body.
For file parsing, CRLF and bare LF are line endings; exclude two or one bytes,
respectively, at a header terminator or before a MIME delimiter. Bare CR is
data, never a line ending. Unfold either accepted ending before SP/HTAB. This
liberal file policy never changes the strict CRLF SMTP transport parser.
Imported/parsed messages support UTF-8 headers under RFC 6532 even though the
SMTP envelope lacks SMTPUTF8.

SMTP can store a message whose body exceeds MIME interpretation limits. Its
raw blob, stored metadata and mutations independent of parsed content remain
usable. A requested interpretation exceeding a structural limit returns a
typed method failure, not a fabricated empty body or a successful partial
tree. Email/parse reports structurally unparseable/over-limit blobs as
notParsable; transient I/O/slot pressure remains a method resource error.
Email/get can still return an explicit projection of
id/blobId/threadId/mailboxIds/keywords/ size/receivedAt and top-level header
properties without parsing the body. A header normalization limit affects only
projections that need that header form. Download returns complete raw bytes.

Email/get has no per-object error, so an interpretation failure can fail a
whole batch. M14/M22 must make td-mail recover by fetching stored metadata
without derived fields, isolating failed objects in bounded single-ID fetches,
and showing an unreadable-content placeholder with raw download/export. Never
hide that email or substitute fabricated body/attachment properties. Likewise,
ordinary folder/date/keyword queries require no body interpretation. A content
search that needs an unparseable body fails explicitly; users can search
headers alone or inspect/remove the offending raw mail. This is a deliberate
resource policy and a compatibility limitation for MUAs lacking that recovery,
not an assertion that default projections always succeed. SearchSnippet/get
uses its standard null/null per-email fallback when interpretation is
deterministically unavailable; transient method resource errors remain typed.
Email/import uses invalidEmail for root message syntax/structural admission
refusal; raw SMTP retention is intentionally more permissive. No accepted raw
blob is rewritten to repair MIME or Unicode. ADMISSION.md owns partial Set,
response-spool failures and truthful resource error mapping.

## 2. Headers and character sets

Preserve original field capitalization/order in headers. Match names with
ASCII case insensitivity. Parameterized property names retain the request's
capitalization; without :all use the last field, with :all use every field in
wire order. Absence is null or [], respectively. Convenience fields use
exactly the RFC aliases, including the last Message-ID field for messageId;
thread-anchor policy in section 5 is separately defined.

Raw values exclude the terminating line ending, preserve leading whitespace
and folds, replace malformed UTF-8 using one U+FFFD per maximal invalid
subpart, and drop NUL. Text form unfolds an accepted line ending followed by
SP/HTAB, removes initial SP, decodes only properly placed RFC 2047 encoded
words with known charsets, removes their encoded NUL/control characters, then
applies NFC. Whitespace between adjacent valid encoded words is ignored as RFC
2047 requires. Bad placement or unknown charset leaves the encoded word
literal; malformed payload of an otherwise decodable word uses replacement and
resumes. Unfolding removes the accepted line ending, not the following
whitespace. Encoded words longer than RFC 2047's 75-character ceiling remain
literal. Decode adjacent words separately; do not join invalid split multibyte
sequences across words. Their invalid fragments produce replacement. No
blanket trim changes trailing spaces or an initial tab. All decoding is
independent of chunk size.

Supported charset labels (ASCII case insensitive) are utf-8, us-ascii,
iso-8859-1 and windows-1252; aliases utf8, ascii, latin1 and cp1252 map to
them. Also accept ansi_x3.4-1968 for ASCII and iso_8859-1 for Latin-1. This is
the complete v1 label set; other aliases use the documented unknown-label
policy. ISO-8859-1 maps all bytes directly to their Unicode scalar, including
C1 controls. Windows-1252 maps its defined 0x80..0x9f characters to Unicode
and replaces undefined bytes 81/8d/8f/90/9d with U+FFFD. ASCII high bytes each
become U+FFFD. UTF-8 uses maximal-subpart replacement, including split
sequences. Unknown body charsets use the UTF-8 replacement decoder and set
isEncodingProblem. Absent charset reports the MIME default us-ascii. For
absent/ASCII labels only, a bounded prescan selects UTF-8 exactly when the
entire transfer-decoded body is valid UTF-8 and contains high bytes. This
fixed heuristic sets isEncodingProblem for the label mismatch; invalid UTF-8
falls back to the ASCII rule. Explicit Latin-1 remains Latin-1, including C1
values. Do not guess a locale, try other charset heuristics or normalize
bodyValues to NFC. Count the prescan and subsequent decoding against work
budgets. The original charset label remains visible in the part property.

JMAP requires I-JSON (RFC 7493): reject request strings containing unpaired
surrogates or Unicode noncharacters, including escaped forms. When
interpreting raw mail for JSON, replace noncharacters with U+FFFD before any
NFC step and record a decoding diagnostic (isEncodingProblem for body values).
This means U+FDD0..FDEF and each plane's FFFE/FFFF. Raw/part downloads remain
byte-exact. The standalone Unicode normalizer still follows Unicode's identity
behavior for such scalars; JSON representability is a separate mail projection
step.

All seven header forms are supported only in their RFC 8621 sections 4.1.2.1-7
allowed combinations. Forbidden combinations are invalidArguments on reads and
invalidProperties on structured creation. Addresses and GroupedAddresses
support quoted pairs, comments, groups, UTF-8 display names and RFC 2047
placement; use an immediately trailing comment as a missing display name.
Recovery splits only at commas/semicolons outside quotes, comments and angle
brackets; an otherwise unparseable nonempty item becomes
{name:null,email:unfolded trimmed item}. An unmatched construct consumes the
remaining item, not an unbounded search for a closing token. Parsed display
names receive NFC; stored/rendered addresses never authorize SMTP recipients.
The stricter outbound addr-spec/envelope validator rejects malformed recovered
addresses and CR/LF/NUL rather than sending them.

MessageIds parses complete RFC 5322 msg-id lists, removes grammatical CFWS and
outer angle brackets, and returns null for an invalid list. For References and
In-Reply-To also accept their RFC 5322 section 4 obsolete forms and discard
obsolete phrases while retaining msg-ids. Whitespace inside quoted strings or
domain literals is data, not grammatical CFWS. Other malformed text still
invalidates the field rather than accepting a convenient prefix. Date uses RFC
5322 date-time including its obsolete numeric/year/zone rules; malformed or
out-of-range dates return null. Convert valid numeric offsets without local
timezone dependence; -0000 retains the unknown-local-offset meaning. URLs
parses RFC 2369 lists and returns null for invalid input. These forms never
fetch a URL. RFC date/header parsing does not broaden SMTP envelope grammar.
Header values unsupported by a form remain readable as Raw.

RFC 2231 MIME parameters support percent decoding, charset/language prefixes,
and numbered continuations beginning at zero without gaps. Duplicate segment
numbers or malformed percent escapes invalidate that extended candidate; do
not concatenate an attacker-selected subset. Prefer a valid extended filename,
then ordinary filename, then valid extended Content-Type name*, then ordinary
Content-Type name. Ordinary filename/name accept properly placed RFC 2047
encoded words as a compatibility rule. For duplicate ordinary parameters use
the first complete value. Unknown extended charset uses the same replacement
policy. Never use a supplied name as a filesystem path. Header names/values
generated from JMAP must reject injection; only the serializer adds folding
and delimiters.

## 3. MIME and stable transfer decoding

Root Content-Type defaults to text/plain; a missing child type in
multipart/digest defaults to message/rfc822. Use the first syntactically valid
Content-Type/Content-Disposition/Content-Transfer-Encoding field, ignoring
later duplicates; the full raw header list remains visible. Types, disposition
tokens and recognized encodings compare ASCII case insensitively. Emit type
and disposition tokens lowercase; parameter charset value retains its
spelling. Part IDs are decimal preorder ordinals starting at 1, counting
containers even though multipart partId/blobId are null. Ordinals are Strings
without leading zeros and stable for unchanged bytes under this parser
version. Leaf size counts exact transfer-decoded octets; multipart size counts
its identity body extent including its own delimiters and child headers,
excluding an enclosing delimiter's preceding CRLF. Never substitute a sum of
leaf sizes. Known base64/quoted-printable on a multipart container is invalid
MIME and makes that structure notParsable; do not decode it into a new
implicit locator stage. Unknown transfer tokens retain the identity rule with
a diagnostic.

Match a boundary prefix immediately after the leading -- at a line start, as
RFC 2046 section 5.1.1 recommends. Suffix -- means close; otherwise the line
is an opening delimiter. Ignore other suffix bytes with a diagnostic,
including bytes after a closing --. If multiple active boundaries match, the
outermost wins and terminates intervening children; such prefix collisions are
invalid MIME but their recovery is deterministic. Strip the preceding accepted
line ending from the previous part extent as WIRE.md specifies. Ignore
preamble/epilogue for body lists but preserve them in raw files. A missing
closing delimiter ends the last child at enclosing boundary/EOF with a local
diagnostic. A multipart without a valid opening boundary is notParsable; do
not mint arbitrary p1 extents to pretend that container was a leaf. A root
body with no multipart structure may be empty. Do not recurse into
message/rfc822 or message/global inside bodyStructure; Email/parse handles
those authorized leaf blobs in a fresh context.

The following decoding defines the octets of every issued p1/p2 blob ID. Do
not change it under those locator versions:

- Identity: 7bit, 8bit, binary and unknown transfer tokens copy bytes exactly.
  Unknown tokens set the body-value encoding diagnostic.
- Base64: ignore SP, HTAB, CR and LF. Discard other nonalphabet bytes and flag
  a problem. Decode full quartets. The first '=' terminates the alphabet; two
  pending sextets emit one byte, three emit two, one emits none. Validate
  expected padding count and zero pad bits; bad/missing padding or a single
  trailing sextet flags a problem. After the first '=', ignore remaining
  bytes; unexpected non-whitespace/non-required-padding bytes flag a problem.
  At EOF without padding decode a two/three-sextet tail and flag missing
  padding. A complete unpadded quartet is valid.
- Quoted-printable: decode =HH case insensitively. An equals sign, optional
  SP/HTAB transport padding, then CRLF is a soft break. Accept the same form
  ending in LF or EOF as a soft break with a diagnostic. In particular, a
  dangling equals sign is dropped: a part delimiter may have consumed its
  following CRLF. Other equals sequences emit the equals sign literally, then
  resume at the following byte with a diagnostic. Drop literal SP/HTAB at an
  encoded line end (CRLF, tolerated LF, or EOF); encoded =20/=09 remain data.
  Preserve hard line endings and all other literal bytes. A bare LF or
  prohibited literal/control byte is diagnosed. Determine a whitespace run's
  end by bounded lookahead/replay, not a growing pending string. Nested
  sources restore bounded per-stage source/decoder checkpoints, as
  RESOURCES.md specifies; never restart from the root's beginning for each
  whitespace run. All extra reads/steps are charged.

Blob downloads apply only transfer decoding, never charset conversion, NFC,
NUL removal or line-ending conversion. Body values additionally apply charset
decoding and CRLF-to-LF conversion, with true isEncodingProblem for unknown or
malformed transfer/charset data. maxBodyValueBytes=0 means unlimited by that
argument; disk/work caps still apply. A positive cap stops at a UTF-8 scalar
boundary and sets isTruncated only if text remains. For HTML, back up to the
last position outside a tag when a cap lands inside one, using a bounded
streaming tag/quote state; no valid/balanced HTML guarantee. Continue bounded
validation/counting as needed to establish flags and sizes before success.

Use RFC 8621 section 4.1.4's suggested parseStructure algorithm for the
initial textBody/htmlBody lists, implemented iteratively. Pin its fallback
between plain/HTML alternatives and related/inline-media behavior with the
RFC's A..K example. Derive attachments depth-first by the RFC's two
conditions, without duplicate entries. hasAttachment is true when this list
contains a part whose disposition is not inline; no hidden CID-image heuristic
in v1. Preview is the first 256 Unicode scalars of display text, preferring
textBody, falling back to HTML extraction below, collapsing ASCII whitespace
to one SP and trimming edges. It has no HTML markup or attachment metadata
appended.

For structured creation, generate missing Message-ID and Date once before
publication. Message-ID is <td-mta.HEX@HOST>, with 32 lowercase entropy hex
digits and the configured primary mail hostname; bound collision retries to
32. Date uses the injected current UTC instant, whole seconds, English RFC
5322 weekday/month spelling and +0000. Emit MIME-Version: 1.0. Account for all
generated headers in the encoded size reserve. Retry/restart or transmission
copying must not regenerate them.

Follow the RFC's bodyStructure versus textBody/ htmlBody/attachments
alternatives and property exclusivity checks. Emit CRLF, UTF-8 text, ASCII
MIME headers with RFC 2047 display text, and RFC 2231 UTF-8 filenames.
Ordinary leaf data uses base64, 76 columns; multipart and message/rfc822
remain identity encoded. A message/rfc822 attachment must have legal 7bit/8bit
line structure; otherwise reject that structured type with invalidProperties
(the client can explicitly attach application/octet-stream). Boundaries are
'td-' plus 32 lowercase entropy hex digits, with at most 32 candidate retries.
Scan identity children for delimiter collisions including nested multipart
content; reserve/check total encoded message_bytes before commit. Bcc stays in
the stored original and is absent from the separately frozen transmitted
message as QUEUE.md specifies. No uploaded filename, header or boundary
request can inject generated envelope commands.

## 4. Search, sort and query state

All standard filters in CONFORMANCE.md are required. No inThread extension is
accepted. An unknown filter property/operator returns unsupportedFilter; wrong
JSON shape/type returns invalidArguments. AND/OR/NOT follow RFC 8620, with
AND([])=true, OR([])=false and NOT([])=true. Filter depth uses json_depth and
total nodes use json_tokens. Arrays of mailbox/identity/email/thread IDs are
bounded by objects_per_method. A text operand is at most 4096 UTF-8 bytes, at
most 64 tokens/phrases; each token's decoded text fits that same total. Bound
distinct text operands to 64 per method. Valid filters beyond these compiled
complexity limits return unsupportedFilter before scanning; do not evaluate a
prefix. All standard scalar date/size comparisons use exact values:
before/maxSize exclusive, after/minSize inclusive. Threads in keyword filters
include all live account members, independently of an inMailbox condition.

For text/from/to/cc/bcc/subject/body/header-value searches, split ASCII
whitespace or U+00A0 outside matched single/double quotes. A quote opens a
phrase only at a token's start and only when a matching close exists;
otherwise it is literal. An apostrophe within an ordinary word is literal. The
matching quote ends the phrase and must be followed by whitespace or end of
input. Inside a phrase, either quote character as data and backslash must be
backslash-escaped; no other escape is supported. Unescaped other quotes and
unsupported escapes return unsupportedFilter. Every token must occur; tokens
are substring matches, without stemming. An empty quoted phrase matches every
string, just like an empty unquoted operand. Outside phrases, backslash is
literal; a Windows path needing literal backslashes inside a phrase must
escape them as the RFC requires. Empty text matches the empty token set.
Compare Unicode scalars after UNICODE.md's simple lowercase mapping. Do not
strip accents or promise sharp-s/ss or canonical-equivalence matching. Compare
the decoded field representation literally after simple lowercase; query
operands receive no implicit NFC. Header Text has mandatory NFC while
bodyValues does not. Therefore a decomposed query may not match a composed
header or body, even if they render alike. This quality limitation is explicit
and tested; a future equivalence policy needs an interpretation-version bump.
Phrase whitespace is collapsed to one SP in both query and searched text.
Ordinary field whitespace is also collapsed for matching; no phrase crosses a
field/body-part boundary. All tokens for a specific field condition may occur
in different instances of that field. text may distribute tokens across
From/To/Cc/Bcc/Subject and searchable body parts; body may distribute them
across searchable parts.

Decode valid encoded words and address display names before matching. Search
all instances of required headers, including Bcc in the stored original.
header:[name] tests existence; header:[name,value] requires an existing field
and applies the same text rules to unfolded/decoded header text; the public
header-form whitelist does not forbid internal search decoding. Body search
covers decoded text/* and message/* content. Recurse into message/rfc822 and
message/global as fresh message contexts under WIRE.md's six-stage limit,
including their displayed From/To/Cc/Bcc/Subject and text parts as separate
searchable fields. Other message/* types use their local
transfer/charset-decoded textual contents. Never follow message/external-body
URLs or extract binary documents. Exhaustion of a required nested parse is a
method error, never a nonmatch. Attachment filenames are included in text
search, not body search. Unknown charsets use section 2's fallback, never
silently skip their bytes. An unparseable body needed by a filter is a method
failure, not a nonmatch.

HTML extraction is a bounded tokenizer, not a renderer or sanitizer. Ignore
tags, comments, declarations and contents of head/script/style; include text
and decoded alt/title attribute values, with SP on either side of each
attribute value. Tag/attribute names use ASCII case insensitivity,
quoted/unquoted attributes and raw-text closing rules. Insert SP at
address/article/aside/blockquote/br/div/dl/dt/dd/fieldset/figcaption/figure/
footer/form/h1-h6/header/hr/li/main/nav/ol/p/pre/section/table/td/th/tr/ul
start/end tags only; inline/unknown tags add no separator. Treat U+00A0 as SP
for search/preview. In particular, <span>Hel</span>lo becomes Hello. Never
insert a space solely because an inline tag ended; never match tag names, URLs
or ordinary attribute names. Decode numeric character references and
amp/lt/gt/quot/apos/nbsp; unknown named references remain literal. Invalid
numeric scalars become U+FFFD. Malformed/unterminated markup consumes its
remaining bounded context, with a diagnostic; it never launches a browser,
fetches content or interprets CSS/JS. This is the declared v1 extraction
heuristic. BodyValues preserves the original decoded HTML instead. HTML,
Unicode and token work all count against the enclosing scan budget.

SearchSnippet/get uses the same token matches, HTML-escapes &, < and >, and
adds only generated balanced mark tags. Subject is the full escaped subject
with matching spans marked, or null if unmatched. SearchSnippet may retain at
most 4096 (start,end) u64 scalar-ordinal span pairs per email in charged sort
scratch, merge their overlap, then replay the projected text and stream the
escaped result. Ordinals are before lowercase mapping; one-to-one scalar case
mapping needs no retained UTF-8 byte-offset window. If exact spans or
interpretation cannot be determined within those limits, return null for both
properties as the standard requires; do not publish a partial subject
highlight set. Preview chooses the first body match and grows surrounding
context while the final UTF-8 string, including escapes and mark tags, stays
at most 255 bytes; do not cut an entity/scalar/tag. If a matched span alone
cannot fit, return null for that preview. Filters with no positive textual
terms return both fields null. Missing emails use notFound. No fabricated
match highlights are permitted.

Supported sort properties: Mailbox sortOrder/name; Email receivedAt/size;
Submission emailId/threadId/sentAt/sendAt. Publish exactly those Email
options. Other properties, unknown explicit string collations or more than
eight comparators return unsupportedSort. Default or empty sorts are Mailbox
sortOrder ascending then name ascending, Email receivedAt descending,
Submission sendAt descending. Every sort ends with raw local object ID
ascending as the stable tie break, even when the requested primary comparator
is descending.

Default string comparison is lexicographic order of Unicode simple-lowercase
scalars from the pinned data, ignoring Accept-Language. Explicit collations
are i;octet and i;ascii-casemap as RFC 4790 defines them; advertise just those
two. The default Unicode-aware algorithm has no claimed IANA name. Numeric/
Boolean/date comparisons ignore the collation field as RFC 8620 requires.
Mailbox name matching uses a lowercase substring without query phrase syntax;
names obey RFC 5198 Net-Unicode and the FORMAT.md 1024-byte encoded ceiling.
Mailbox hierarchy depth is capped at 32 (root is one). sortAsTree/filterAsTree
follow RFC 8621 section 2.3, including ancestor comparators/eligibility.

Filtering, ordering, thread collapsing, anchor/position adjustment and paging
operate in that order on one pinned read view. collapseThreads retains the
first sorted matching email per thread. Thread/get orders members by
receivedAt ascending, then ID ascending. Date comparisons normalize instants
to UTC with checked arithmetic; receivedAt comes from durable receipt/import
metadata, never the mutable wall clock at query time. calculateTotal=true
counts the complete post-collapse result, even for limit=0. An absent anchor
returns anchorNotFound, never the first page. Omitted query limit uses
query_page; larger limits return at most query_page with accurate position,
state and total. Include the response limit whenever the server selects or
clamps it, including an omitted request limit. Omitted get IDs enumerate all
objects only if the complete result fits objects_per_method, otherwise return
requestTooLarge.

Use streaming scans and bounded external sorts under ADMISSION.md. The sort
lease covers all runs, intermediate merges, collapse deduplication and snippet
span files; NFC itself uses no sort lease. Memory does not grow with matching
rows. Discarded indexes never change answers. A deterministic byte/work
ceiling returns serverFail; shared pressure or elapsed deadline returns
serverUnavailable. Neither returns a partial successful ID list, false total,
guessed ordering or empty match set.

V1 queryState is the ASCII concatenation q1_, type tag (two lowercase hex
digits from FORMAT.md), account (32), epoch (32), sequence (16, big-endian
hex) and interpretation version (eight hex digits, initially 00000001). Total
93 bytes; only Mailbox, Email and Submission type tags are issued. The global
sequence may conservatively change for irrelevant mutations. Equal tokens
promise stable matching/order only for identical query arguments under that
interpretation version; tokens need not hash query arguments. Any
parser/search/collation change that can alter results increments this version
and invalidates corresponding caches, even without a journal write. Versions
must not be reused for changed semantics. FORMAT store schema 1 binds
immutable Email projections to this interpretation policy. A version bump
alone cannot alter parsed headers, part IDs/structure, body lists or
hasAttachment for an existing Email ID. Preserve those derivations, or require
a separately reviewed offline migration to a new store schema that recreates
affected emails under new IDs and starts a fresh epoch before serving new
derivations. Such a migration must preserve raw bytes and reconcile all owning
references and immutable thread history; none is implemented or authorized by
v1. Refuse an incompatible upgrade until that migration exists. WIRE.md's old
locator decoding/validation still protects previously issued blob IDs.
Preview-only heuristic changes retain the RFC's explicit exception.
canCalculateChanges=false; well-formed supported queryChanges always returns
cannotCalculateChanges.

## 5. Immutable thread assignment

Use STORAGE.md's authoritative anchors, never a body rescan or search index as
the source of existing assignments. Scan Message-ID fields in wire order. For
each field require a complete valid MessageIds list; skip a malformed field
entirely. Select the first valid nonempty field and its first ID, then check
its parsed length. If that ID exceeds 1004 bytes, store no anchor: do not
select a later ID or field. Strip grammatical CFWS and angle brackets,
preserving other interior msg-id bytes, including quoted-string data. No
domain/local-part case folding or Unicode NFC changes ID matching. UTF-8
msg-ids are accepted under RFC 6532 grammar.

For References, concatenate complete valid lists from all fields in wire
order, retaining source extents for only the last 32 IDs in a fixed ring. Then
do the same separately for In-Reply-To. Malformed fields contribute nothing.
Oversize IDs occupy their ring position but cannot resolve, so they cannot
make the lookback unbounded. Try References newest to oldest, then In-Reply-To
newest to oldest. The first matching live anchor wins; duplicates choose the
smallest raw Email ID. Use only anchors committed before this email's
creation, then insert its own anchor in the same creation frame. Creation
order is request order across earlier committed objects; if several creations
share a frame, validate sequentially against earlier creations in that frame
as well as the committed base. Never consult a later creation.

If References/In-Reply-To have no live match, try the email's own bounded
anchor ID as a final candidate, applying the same smallest-live-ID rule. This
groups duplicate deliveries and a sent copy's mailing-list echo without
overriding explicit reply references. No candidate means a fresh thread. No
late arrival merges existing threads; cycles, self-reference and duplicate
Message-IDs never rewrite assignments. Removing an email removes its anchor
atomically. A later arrival may therefore choose differently, but surviving
emails retain their thread IDs. An empty thread is destroyed when its last
live email is removed; a later reference does not resurrect it. Queue
historical thread IDs do not own live threads. Index loss cannot change the
choice. Missing capacity, I/O failure or work exhaustion refuses creation
temporarily; it must not masquerade as no anchor.

## 6. Import receipt date

For Email/import, an explicit valid receivedAt wins. Otherwise scan Received
fields in wire order (most recent trace first). Parse the final semicolon
outside quoted/comment constructs as the start of an RFC 5322 date-time; use
the first syntactically valid trace timestamp, not the maximum claimed clock
value across headers. If none parses, use the injected import clock. This is
imported metadata, not authentication of a sender's claimed trace. CASES.md
E06 must cover conflicting, malformed and missing trace dates.

## 7. Single-account copy refusal

Email/copy requires different source and destination accounts (RFC 8620
section 5.4). V1 has one account, so no Email/copy can succeed. After argument
shape validation, an unavailable destination uses accountNotFound, an
unavailable source uses fromAccountNotFound, and the authorized sole account
as both source and destination uses invalidArguments. Do not run a mutation or
implicit destroy-original hook. The generic successful-copy rules retained in
ADMISSION.md describe the protocol but are unreachable in v1; supporting a
second account would require a separate reviewed design change. Blob/copy has
its own RFC 8620 section 6.3 contract and no such distinct-account
requirement.
