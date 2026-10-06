# Bounded header and MIME lexical primitives

This std-only crate owns resident lexical syntax and passive source-view
helpers, not protocol or admission policy. It has no dependencies, clock,
crypto, I/O or source ownership. Lexical cursors receive from their
enclosing grammar a complete immutable field-value or parameter-name/value
slice, one selected URI wire spelling slice or one logical delimiter line,
any required exact start offset, and permission for the lexical token at
that location. The MIME line matcher owns no body traversal, transfer
decoding, active-boundary precedence or source authorization.

cfws scans optional comments and folding whitespace, leaves the first
non-CFWS byte untouched, and returns original top-level comment extents.
Comments nest to depth 32 including the outer pair. Escapes suppress
delimiter interpretation; UTF-8 is validated without decoding display words.
SP/HTAB, CRLF folds and bare-LF folds followed by SP/HTAB are admitted.
Literal and escaped obsolete ASCII controls are preserved; literal NUL,
invalid/truncated UTF-8, unfinished escapes, unclosed comments and nonfold
line endings inside comments fail. Outside comments, a nonfold ending is
left for the enclosing grammar.

delimited validates one raw quoted string or domain literal, including its
delimiters in the returned extent. It preserves escapes and folds with the
same UTF-8/obsolete-control rules. A literal opening square bracket inside a
domain literal is invalid. Trailing source is left to the caller.

mime_token_octet supplies the shared ASCII MIME-token octet predicate used
by the classifier and enclosing MIME field grammar. It grants no placement
or complete-token authority.

mime_attribute classifies one complete parameter-name slice. It preserves a
base-name extent and ordinary, single extended, numbered section or
malformed form. Ordinary names use ASCII MIME-token syntax; extended bases
additionally exclude apostrophe and percent. Section numbers use checked u64
arithmetic, start with a nonzero digit or the single digit zero, and may end
in one star for encoding. It validates spelling only: no gaps/duplicate
checks, segment assembly, charset/language or percent decoding, selection or
fallback occurs. Malformed form retains the base extent for the caller's
candidate rejection. No name is normalized or granted filesystem/path
authority.

The classifier scans at most 32 octets per turn with 32 source visits and 32
records; EOF is a charged transition. These bounds fit the shared maximum
below. It owns no buffer or collection. Generic state size depends on E;
with the fixed mail error, its bound is recorded below. Shared work failure
remains sticky across replacement callbacks; completion is cached and inert.
Explicit check_work calls the caller's live admission with a zero-count
charge; a refusal retires cached classification and latches in the same
owner. The enclosing caller owns complete-field validation and binds fresh
admission.

language_tag::Tag checks passive tag spelling with a 1..8-letter primary and
optional hyphenated 1..8-letter/digit subtags. Its fixed three-byte feed
state owns no source or admission; the caller charges reads before feed. A
provisional trailing hyphen is incomplete; rejected input stays invalid.
Copying passive Tag state duplicates no source, admission or live cursor.
Empty is separate from Complete so callers may explicitly admit an omitted
language. Encoded-word qualifiers and MIME value prefixes share this
grammar; no registry lookup or locale/decoding policy follows.

mime_value validates the complete raw MIME token or quoted spelling before
projecting provisional octet events. Extended prefix, language and percent
syntax is checked during projection; no event is final before Complete.
Modes distinguish ordinary, initial extended and later extended section
spelling. It removes quoted pairs/delimiters and unfolds logical line
endings followed by WSP, including escaped line endings after unquoting.
Escaped CR/LF without following WSP remain literal octets for later caller
control filtering. Ordinary bytes remain literal. Initial extended spelling
requires charset and language apostrophe delimiters; Charset/Language/Data
event roles preserve that separation. Empty labels remain empty. Charset
labels use attribute-char bytes; language labels feed the shared passive tag
validator. Extended data uses attribute chars or complete percent triplets,
with both hexadecimal cases. Quoted extended wrappers are explicitly
admitted but their logical content still follows extended spelling. Decoded
NUL and invalid UTF-8 remain octets for the caller's charset/control policy.

mime_value projection owns no output buffer or collection and emits at most
one octet per turn. Its visits/records fit the common shared maxima. All
octet events remain provisional until Complete and retire after any later
value or candidate refusal. check_work performs the same sticky zero-count
fresh admission as the name classifier. Enclosing field validation, numbered
assembly, charset conversion, filtering and display-word placement remain
external; projected octets never authorize word recognition.

projection::atom is a stateless logical reader shared by MIME values and
mail phrase/comment display readers. The caller supplies a position, quoted
flag and bounded read callback, retaining source bounds and exact EOF
admission policy. One invocation makes at most six callback calls and
returns at most one octet, next source position and escaped provenance.
Unquote first, then unfold logical CRLF/LF followed by WSP; retain following
whitespace and OR provenance across all fold contributors. Escaped nonfold
line endings remain literal. The callback admits reads before access and
propagates refusal immediately. Incomplete pairs and checked position
overflow remain distinct failures. No source, work owner, clock, buffer or
collection is retained. The caller supplies complete validated spelling,
maps invariant errors and latches failure in its existing owner. No lexical
proof or original-source encoded-word placement follows from projected
octets. This helper has no poll/cached state and does not change the lexical
cursor limits below.

projection::character assembles one strict UTF-8 scalar using at most four
atoms and a four-byte local array. The original position, quoted flag and
caller-owned context are passed to separate read and verification
callbacks; they borrow that same context sequentially, never a copied
allowance. The read callback retains the atom source/EOF contract. After
assembly, the verification callback admits the separate local UTF-8
inspection of one to four bytes before that inspection occurs. At most 24
read calls and one verification call occur. Source-read and
local-verification refusals are separate typed errors and propagate
immediately; truncated or invalid logical UTF-8 is a typed failure. EOF
before the first atom returns absence. The returned next position and
first logical octet's escape/fold provenance are passive; later
continuation escapes do not change the first octet's provenance. NUL,
controls and noncharacters remain scalars for caller policy. There is no
retained state, control filtering, normalization, placement proof or
change to the lexical cursor maxima.

None of these helpers discovers raw-message field boundaries, selects
fields, decodes encoded words or charsets, normalizes Unicode, assembles
addresses/parameters, creates protocol output or grants
identity. Provisional comment events retire on any later failure; lexical
Complete validates only the helper's scope, not its enclosing field.

For resident lexical cursors, Work::charge runs before each access or
transition. Charge reports visits and records separately, including
lead-byte rereads and failed fold lookahead. Each poll performs at most 32
transitions, 160 source-byte visits and 32 record charges, emits no retained
output bytes and owns no growing buffer or collection. The caller implements
the bounded callback and binds current clock, cancellation and aggregate/job
budgets to it. No replacement callback can revive a failed cursor; Error<E>
retains the original Copy error. Cached completion performs no new work.
Live final admission belongs to the caller. Generic cursor size depends on
E; with td-mime's fixed errors CFWS and delimited cursors remain within 64
bytes, and the parameter-name cursor within 128 bytes and the
parameter-value cursor within 160 bytes. Live cursors remain neither Clone
nor Copy: healthy pure progress may be replayed through snapshots, while
refused state cannot be copied this way. Constructor allocation and
complete-field authorization are external.

CFWS, delimited, parameter-name and parameter-value cursors expose opaque
Copy Checkpoint snapshots of healthy pure lexical progress. Snapshots retain
exact immutable source identity, positions, phases, escape/nesting/tag state
and nested pure checkpoints; they retain no work, credit, clock, refusal or
output owner. Refused cursors cannot produce snapshots. resume reconstructs
provisional lexical progress, not field validity or publication authority.
Every subsequent source/transition access is admitted again. The caller must
keep the original allowances across replay; a historically healthy snapshot
cannot revive a failed enclosing owner or replace its admission context.
Restoring cached completion is inert and requires enclosing fresh admission.
td-mime keeps snapshots private inside its normalization source; its live owner
structurally retains the original Meter/HeaderBudget/credit and sticky refusal.
Shared event/cost and all per-turn callback-cut fixtures qualify snapshot
fidelity. These snapshots change no lexical work or live-cursor size ceiling.

td-mime wrappers preserve their public errors and original budget/clock
binding. The source grammars migrate atomically: no old parser remains
alongside the shared implementation. Shared fixtures exercise
lexical/refusal contracts; td-mime tests and the mail allocation probe retain
consumer composition qualification. These bounds do not establish total worker/native
memory or scheduler latency.

The mime_protocol Validator classifies already admitted logical parameter
bytes with four bytes of pure fixed state. Boundary implements RFC 2046
section 5.1.1's one-through-70 ASCII bchars rule with a non-space final byte;
Token implements RFC 2045 section 5.1's nonempty ASCII MIME token grammar.
No trimming, decoding, charset default or Unicode normalization occurs.
Alphabet/length failure is sticky; a trailing space is evaluated only at
complete-value EOF. Prefix validity is passive, never field completeness,
source admission or multipart authority. Callers fund each bounded feed,
consume the entire selected value, and retain their original budgets.
Normative grammar: https://datatracker.ietf.org/doc/html/rfc2046#section-5.1.1
and https://datatracker.ietf.org/doc/html/rfc2045#section-5.1.

mime_boundary::Line recognizes a single logical delimiter line with <=24
bytes of pure fixed state including a borrowed 1..70-byte boundary view.
Its caller validates boundary grammar and funds each transition plus at
most one boundary comparison. Match only the leading -- and complete
boundary prefix. The first two bytes of a suffix beginning with --
close; other non-SP/HTAB suffix bytes are diagnostic. EOF classifies a
lone suffix hyphen as opening with a diagnostic. The enclosing caller
excludes accepted line endings before feeding and owns file line-ending
CR/LF, active-boundary precedence, complete entity extent, original work
and publication. Prefix Match evidence does not authorize multipart
structure. reset clears only pure line progress and retains the same
boundary view; boundary() returns that passive view, neither validating
it nor admitting another scan. Enclosing owner refusal remains sticky
across line resets.

Detached mime_boundary::State holds at most eight bytes of the same pure
transition state. Its enclosing owner supplies the same immutable boundary
on every feed; differing lengths suppress Match until a pure reset. A live
owner needing a refusal checks that contract before calling State.
It has no borrowed source, clock or admission handle. Line wraps State and
pins its borrowed boundary identity. Neither detached progress nor a reset
can authorize a source extent or recover a retired live mail owner.

resident::slice maps one absolute half-open extent into an immutable caller
window using checked base subtraction, checked usize conversion and slice
bounds. Reversed, before-base, beyond-window and unrepresentable extents
return None. Any empty in-window range, including at either endpoint, is
valid. It checks only the selected extent, not whether the complete window
has an absolute endpoint representable in u64. It reads and copies no
octets, allocates nothing and retains no state, source identity, admission
or publication authority. The caller binds its source, maps failure to its
existing typed error, and funds all later byte access under its original
owners. Consumers map their admitted header/MIME extents through this shared
view helper; enclosing lexical and protocol decisions remain external.

language_list validates one complete Content-Language field value under
RFC 3282 section 2 and RFC 3066 section 2.1 spelling. It composes the existing
passive Tag and bounded CFWS cursors. Require one or more comma-separated
tags, with CFWS before/after each complete tag; tag octets remain contiguous.
Emit original-case tag extents in order, preserving duplicates and excluding
comments/whitespace. Tag events remain provisional until the whole value
completes, and every event retires after later malformed syntax, nesting or
work refusal. There is no registry lookup, preference/quality syntax, locale
selection, charset inference or metadata/publication authority.

A poll invokes at most one bounded CFWS turn or one admitted tag/separator
read, including charged EOF attempts. Visits/records stay within the shared
160/32 maxima. With mail's fixed error, cursor state fits 192 bytes; it owns
no growing tag list or output buffer. Healthy cached Complete is inert;
check_work freshly admits zero-count work and retires cached success after
refusal, sticky across replacement callbacks. Live cursors are neither Copy
nor Clone. Enclosing field-name/colon syntax, selected-value ownership and
output retention admission remain external.
Normative grammar: https://www.rfc-editor.org/rfc/rfc3282.html#section-2
and https://www.rfc-editor.org/rfc/rfc3066.html#section-2.1.

uri::Validator consumes already admitted ASCII octets. new requires a scheme
under RFC 3986 section 3, including fragments. This is the URI rule; section
4.3's narrower absolute-URI rule excludes fragments. reference admits
URI-reference spelling, including relative and empty references. Both retain
fixed scheme/hierarchy, authority/userinfo/host/port, path/query/fragment
and percent-triplet state. Neither supplies scheme policy, header
whitespace/CFWS, resolution, fetching, source bounds or output authority.
Empty hierarchies and empty generic authorities remain permitted. Percent
triplets retain spelling and are never decoded; non-ASCII input fails. Query
and fragment brackets require percent spelling; raw [ and ] reject under
this generic URI grammar.

The reference constructor uses fixed prefix state: before the first
slash/query/fragment delimiter, a colon selects a scheme only after ALPHA
followed by scheme characters; otherwise reject it under path-noscheme.
Percent spelling cannot manufacture a scheme. Paths retain dot segments, and
callers decide whether empty references are valid in their field.

The caller funds each bounded feed and EOF decision before providing an
owned octet or calling finish. A feed performs one fixed state transition;
IPv6-literal closure additionally prepays 64 records before inspecting at
most 45 retained bytes with std's IPv6 parser. This is a feed validator, not
one of the resident 32-record poll cursors. IPvFuture spelling is checked
incrementally. With mail's fixed error the validator fits 128 bytes and
allocates nothing. It owns no source slice, work handle, output collection,
clock or admission identity. Syntax and internal work refusals latch across
replacement callbacks; successful finish is cached and inert, and further
feed retires success with InvalidState. check_work freshly admits zero-count
work and retires completion on refusal. It grants no enclosing complete
source proof. Live state is neither Copy nor Clone; enclosing callers retain
their original allowances and latch their own refused source reads.

Normative grammar: https://www.rfc-editor.org/rfc/rfc3986.html#section-3,
https://www.rfc-editor.org/rfc/rfc3986.html#section-4.1
and https://www.rfc-editor.org/rfc/rfc3986.html#section-4.2.

uri::unfold::Cursor removes wire SP/HTAB and CRLF or bare-LF folds followed
by WSP from a caller-selected immutable URI spelling. Exclude surrounding
CFWS and the final header ending. Nonfold CR/LF and unfinished folds fail.
Emit all other octets literally, including percent spelling, encoded-word
markers, comments, quotes, NUL and 8-bit octets. Report offsets within the
supplied spelling slice; the caller rebases them to field/message positions.
No quoted-pair removal, UTF-8/URI validation, encoded-word
placement/decoding, scheme or empty-label policy follows. RFC 2557 section
4.4.3 describes unfolding before header decoding; RFC 2017 section 3.1
supplies wire-whitespace removal. These references do not make this helper a
complete Content-Location reader or MIME parameter unquoter.

One poll admits one source byte or EOF before access with at most one visit
and one record. Whitespace yields instead of an unbounded scan. With mail's
public URL error the cursor fits 64 bytes; generic size depends on E. It
retains source, position, fold phase and sticky error, no work/clock/output
owner or collection. Octets remain provisional until complete unfolding and
retire after later refusal. Cached Complete is inert; check_work admits
zero-count fresh work and retires success on refusal, sticky across
callbacks. Live state is neither Copy nor Clone. Source/field selection,
original allowances, retained output and final publication admission remain
external. Normative folding:
https://www.rfc-editor.org/rfc/rfc2557.html#section-4.4.2,
https://www.rfc-editor.org/rfc/rfc2557.html#section-4.4.3 and
https://www.rfc-editor.org/rfc/rfc2017.html#section-3.1.

uri::spelling::Cursor selects a source-relative range under explicit caller
permission for greedy leading CFWS and terminal CFWS beginning with WSP or an
admitted fold. The complete supplied field-value slice excludes its final
header ending. After leading CFWS, parentheses adjoining URI bytes stay
literal. Partial optional CFWS, malformed optional comments and optional
comment nesting beyond 32 stay in the URI spelling; leading CFWS errors reject
selection. After a malformed or over-nested optional comment or malformed
whitespace probe, retain its entire remaining suffix literally, including
inner balanced parentheses and trailing whitespace, and fund a plain scan to
EOF. Optional syntax recovery never masks Work or InvalidState. A whitespace
probe learns the first non-WSP position before attempting a comment suffix, so
failure does not retry a long leading whitespace run one byte at a time. All
probes and CFWS replay are separately funded. No total linear-work claim
follows.

Selection alone proves no URI/fold/word-placement validity or nonempty field.
An empty range remains a caller presence decision. The resulting offsets refer
to the original supplied slice, not a copied/unfolded buffer. Caller policy
decides ambiguous comment versus URI parentheses; percent spelling can
preserve intended leading parentheses. This helper grants no raw-file field
discovery, source authorization or retained output authority.

One poll performs one bounded CFWS turn or one source/whitespace-probe step,
including admitted EOF attempts, within 160 visits and 32 records. For E=u8
and mail's public literal-reader error, state fits 160 bytes; generic size
depends on E. Exclusive child state holds CFWS or unfolding, never both. No
source-sized scratch, growing collection, clock or work owner is retained.
Live cursors are neither Copy nor Clone. Cached Complete is inert; check_work
freshly admits zero-count work and retires completion on refusal, sticky
across replacement callbacks. Consuming finish requires healthy completion but
performs no fresh admission; callers separately check original work before
consuming/retaining offsets. Normative surrounding grammar:
https://www.rfc-editor.org/rfc/rfc2557.html#section-4.1.

uri::word_token::Token passively frames one logical encoded-word
candidate beginning with =? through its first payload ?= after charset
and encoding separators. It retains at most four bytes of state and
enforces the shared 75-octet logical ceiling shared by every RFC 2047
word context. URI runs are the first framing consumer; the state is
reusable over any already-funded logical octets, independently of
URI-reference validation. It does not validate charset, encoding or
payload syntax, unfold wire bytes, recognize words or grant placement.
The enclosing owner funds each octet before feeding it. Copies retain
framing progress, not work or source authority. An extra feed after
completion rejects; refusal is sticky. A payload beginning with = does
not close at its preceding encoding separator. Normative word ceiling
and separators: https://www.rfc-editor.org/rfc/rfc2047.html#section-2.
