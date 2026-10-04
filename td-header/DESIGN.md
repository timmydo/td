# Bounded header lexical primitives

This std-only crate owns resident lexical syntax, not protocol or admission
policy. It has no dependencies, clock, crypto, I/O or source ownership. The
enclosing grammar supplies a complete immutable field-value or
parameter-name/value slice, any required exact start offset, and permission for
the lexical token at that location.

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
E; with td-mta's fixed errors CFWS and delimited cursors remain within 64
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
Mail keeps snapshots private inside its normalization source; its live owner
structurally retains the original Meter/HeaderBudget/credit and sticky refusal.
Shared event/cost and all per-turn callback-cut fixtures qualify snapshot
fidelity. These snapshots change no lexical work or live-cursor size ceiling.

Mail wrappers preserve their public errors and original budget/clock
binding. The source grammars migrate atomically: no old parser remains
alongside the shared implementation. Shared fixtures exercise
lexical/refusal contracts; mail tests and allocation probes retain consumer
composition qualification. These bounds do not establish total worker/native
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
