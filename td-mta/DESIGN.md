# td-mta: personal mail service

[POSTGRES.md](POSTGRES.md) owns the active PostgreSQL backend and
multi-instance authority model. [STORAGE.md](STORAGE.md) owns the local runtime
and response scratch. PostgreSQL is the sole mail and configuration store.

## 1. Status and scope

The foreground service receives SMTP and serves the implemented td-mail JMAP
flows from named database profiles. It retains fixed transport workers,
bounded requests, database snapshots and verified immutable body readers.
Relay profiles run a PostgreSQL route worker; JMAP profiles accept submission
and cancellation, including when the relay runs in a separate process.
Structured drafts support text with uploaded attachments, including forwarded
messages. Terminal delivery failures produce local Inbox notices with retained replies
and explicit uncertainty, without outbound bounces.
[CONFIG.md](CONFIG.md) describes startup; [CONFORMANCE.md](CONFORMANCE.md)
states supported client flows and qualification limits. The v1 release still
requires section 15's evidence; functioning receive/JMAP service is not a
claim that every target feature below is implemented.

The SQLite backend, local-store administration and file-backed device registry
are removed. No production cutover or data migration is required.
[FORMAT.md](FORMAT.md) owns transient application encodings, [WIRE.md](WIRE.md)
owns wire identifiers and [QUEUE.md](QUEUE.md) owns submission policy.
Shared MIME, header, reference and recipient validation remain independent of
storage. [RESOURCES.md](RESOURCES.md) records planning and component evidence;
[ADMISSION.md](ADMISSION.md) records bounded work and response retention.

## 2. Invariants and trust boundaries

1. Final SMTP acceptance and successful JMAP writes follow durable publication
   of the data and metadata needed to recover that operation.
2. Only configured recipients receive inbound mail. Neither Internet clients
   nor a trusted gateway can turn the receiving listener into an open relay.
3. Every JMAP object, blob, identity, and submission is scoped to an authorized
   account. Possession of an opaque ID does not authorize its use.
4. A submission accepted by JMAP remains queryable after restart, including
   while its smart host is unavailable. Queue state is not held only in RAM.
5. Request processing bounds untrusted input, concurrency, retained state,
   descriptors, disk use and work. Enforced limits have defined refusal
   behavior, never silent loss. Memory performance follows section 5.
6. Production td-owned code follows the repository's panic/indexing rules.
   Checked arithmetic, bounded nesting, and explicit error handling extend
   the rule to lengths, counters, conversions, locks, and thread creation.
7. Cryptographic primitives and TLS come from the narrowly reviewed provider;
   application protocols use std and td-owned interfaces. Metadata storage
   uses the explicitly admitted private PostgreSQL boundary. No shell subprocesses
   implement mail, certificates, configuration, or administration.
8. Protocol tests run offline against local fixtures. Neither Migadu nor a
   public CA nor the deployment host is a test endpoint.

Adversaries include unauthenticated SMTP/HTTPS peers, malicious message and
MIME contents, authenticated clients sending malformed requests, forged
gateway identity, and hostile files outside the service's private roots.
Root, the kernel, and a compromised service UID are outside the confidentiality
boundary. Disk corruption, full disks, process termination, power loss, clock
changes, and third-party outages are explicit failure inputs.

Trust is not inherited from message headers. `Received`, `Authentication-Results`,
HTTP forwarding headers, and a claimed EHLO name never establish peer identity.
Email and logs are untrusted data for an AI operator, never instructions.

## 3. Code and dependency boundaries

Shared byte-level MIME/header parsing, transfer decoding, charset handling,
Unicode tables and normalization adapters live in `td-mime`. Its
[design](../td-mime/DESIGN.md) owns the shared contract. Mail retains admitted
body extents, pooled snapshot custody, scheduling and protocol publication.
The former module names re-export the shared implementations; there is one
parser implementation. Service adapters continue to borrow the same live
work meter and aggregate header budget across parsing and serialization.

Service response owners live in the flat `mime_response` module. Named
adapters such as `part_json`, `body_json`, `metadata_json` and `subparts_json`
refer directly to one another; ownership transitions do not define a public
module ancestry. Shared structure parsing is `mime_structure`. Sibling-only
fields and helpers are restricted to `mime_response`; callers cannot fabricate
live owners or extract their original admission handles. Each adapter keeps
its tests in one named child directory. The previous nested response module
tree is removed in the same change.

Use `td-mta/` for the service library and installed binary named `td-mta`.
The executable serves named PostgreSQL profiles and provides PostgreSQL
schema, account, credential and configuration commands. CONFIG.md owns
foreground serving and local status/stop; POSTGRES.md owns database
administration. Local SQLite initialization, verification, backup/restore,
file-configuration checks and file-backed device commands are removed.
Its direct
dependencies are the local `td-civil`, `td-crypto`, `td-header`, `td-html`,
`td-json`, `td-mime`
and `td-nfc` crates, plus the approved private tokio-postgres 0.7.18 closure
with default features disabled. The sole test-only dependency is std-only
`td-fs`, which owns disposable fixture directories. POSTGRES.md section 8
owns the TLS/SCRAM connection probe and PostgreSQL serving contract.
The driver's private SCRAM
cryptography/Unicode preparation do not replace td-crypto or td-mime.
Application protocols, configuration and scheduling use std
plus the local libraries; the private PostgreSQL adapters own SQL. There is
no separate runtime package or td-net helper executable.
`td-crypto/DESIGN.md` owns the shared crypto/TLS API and private backend; `td-crypto/TLS.md`
specifies the TLS policy and session contract. ClientConfig, ServerConfig,
shared clock and public client/server sessions are implemented. The mail
record pump composes them; admitted transport integration and
service/resource qualification remain pending.

The M03a boundary moves the existing Crypto/Entropy/Digest traits and fixed
crypto errors into td-crypto. Mail ports re-export those traits and translate
shared errors. M03b1 admits the approved Rustls/AWS-LC closure inside td-crypto
only. M07a1 implements opaque SHA-256; M07a2 qualifies it against the existing
mail-format digest fixtures. M07a3 adds worker-local entropy initialization;
M07a4 implements the Crypto factory and opaque P-256 operations.
TLS sessions and bounded mail record progress are implemented; admitted
service integration and resource qualification remain pending.
Direct streaming digests use td-owned inline SHA-256 without heap allocation
or provider calls.
Other native operations still require resource qualification. No
Rustls/AWS-LC public types, re-exports or configuration escape hatches cross
its facade.
Mail transport adapters consume opaque td-crypto configurations/session state.
The service's transitive lock/executable still includes the backend dependencies;
it is not described as dependency-free once they are added.

Reusable protocol-independent components belong in shared std-only td
libraries. The bounded JSON string framer and scalar escaping live in td-json.
Existing streaming mail adapters use its incremental string API with fixed
state and caller-provided output. Future request dispatch chooses bounded
representations under section 5; it need not eliminate every allocation.
td-json owns no mail, clock, crypto or scheduler policy.

The deterministic NFC ordering/composition engine lives in td-nfc. Pure
source/decoder/decomposition checkpoints and fixed Unicode tables remain
caller-owned; a separate live context binds the original Meter, HeaderBudget,
private credit and the real Tick supplied for each poll. The shared engine
holds an exclusive borrow of
fixed scratch, four pure checkpoints and fixed positions, flags, bitmaps
and sticky failure. Existing mail source/cursor/scratch
ceilings and charge counts remain unchanged. Different future source types
must qualify their own bounds without widening existing header sources.

MIME parameter display NFC uses a separate qualified source over private
pure lexical/field/family/octet/scalar/display checkpoints. Its live owner
retains original work/header budgets and private credit; the real supplied
Tick is fixed within each poll, without being stored in checkpoints;
copying replay progress duplicates none of them. This composition fits the
existing parser/conversion reservations and preserves the smaller header
source enum. Completion remains provisional metadata, with filename
precedence/retention and complete MIME traversal still separate increments.

M06cb retains derived filenames in caller-reserved backing under one original
work/header owner. Disposition filename precedes type name; a present empty
plan wins. Only absent plans continue to the next field. Whole-field syntax,
work, interpretation and output-capacity failures retire retained bytes rather
than choosing a lower-priority candidate. Completed display bytes are passive
metadata, never paths or locator authority; enclosing publication needs fresh
admission. This adds no per-part heap strings or resource arena.

M06cc admits selected boundary/charset logical bytes as ASCII protocol
metadata, preserving spelling and complete extended-family precedence.
Invalid selected values are passive diagnostics, never an ordinary retry or
multipart/charset-default authority. The shared td-header validator owns
pure bounded grammar; mail owns selected source replay, original work/header
admission, fixed charset-alias classification, output and sticky refusal.
No display decoding/NFC can introduce structural syntax. Healthy handoff
returns original budgets while retained bytes remain provisional. Strict
70-byte/alphabet admission and extended-family precedence may differ from
permissive or RFC 2231-unaware clients; POLICY.md owns that parser differential
and the explicit unreadable-content/raw-download recovery, not a fallback to
an alternative boundary interpretation.

M06cd supplies raw resident delimiter events before child header parsing.
Pure shared line matching uses selected boundary prefixes; mail owns CRLF/LF
versus bare CR, checked absolute offsets and original job admission. Events
are provisional and the later explicit-frame traversal must discard them on
any final refusal. The later traversal must exclude delimiter-leading endings from child headers;
parent clipping, header admission and descriptor authority remain separate.

M06ce separates private protocol and delimiter progress from public owners.
A composing owner can retain both progress and original admission in its
own fields, borrowing its immutable boundary and budgets only during polls.
The public cursors use these same cores; no duplicate parsing mechanism or
fresh allowance is introduced. Detached lexical progress is not authority:
the enclosing owner pins source/boundary identity, prepaid header credit,
sticky retirement, provisional retention and fresh final publication.

M06cf composes complete resident MIME traversal with fixed explicit frames,
parent-first child clipping and caller-backed compact descriptors. Root
headers count once; every child spends the remaining aggregate entity-header
cap and original email interpretation/job admission. Outer delimiters bound
an entire child before its headers or inner delimiters are interpreted, so
outermost active prefixes win. Missing closes diagnose their containers;
absent/invalid boundaries, no opening or encoded multipart refuse structure.
Descriptors stay hidden until the entire parse succeeds and are passive
source evidence. Fresh handoff returns original budgets, never blob IDs.
Leaf sizing uses stable transfer decoding without charset/NFC or attached
message recursion. Full metadata projection, locators, worker integration
and native/RSS qualification remain separate.

M06cg composes retained selected part headers after traversal releases its
parser state. Original job/header budgets and conversion scratch survive
selection, lowercase token output, exact charset and normalized filename
projection. Separate caller windows retain complete passive views only after
all phases succeed. Duplicate/default/parameter choice remains the existing
policy; replay counts work without counting the same raw headers twice.
Content-ID/language/location, body-list/JSON composition, authenticated
locators and streaming/worker qualification remain separate.

M06ch classifies retained selected headers and derives resident body lists
with explicit frames, scope-local alternative masks and bounded fallback.
Completed preorder evidence and classifications remain caller-pinned. A final
membership sweep applies both RFC attachment conditions after fallback;
results stay hidden until all tree validation/work completes. Original job
and email budgets survive fresh completion. No locator or wire authority is
introduced; automatic composition and body JSON remain separate.

M06cj adds bounded Content-Language field-value parsing with the original job
and email budgets. Shared CFWS and passive tag spelling produce ordered
original-case extents, provisional until whole-list success. Malformed tails,
nesting and late refusals retire every event; no locale, charset, selected
metadata or publication authority follows. First-valid part-header selection
and retained language JSON remain separate.

M06ck adds one complete Content-ID value through the existing identifier
validation/replay and text conversion engines. Its private purpose admits
exactly one bracketed identifier; ordinary MessageIds lists keep their
existing behavior. The mail adapter borrows original work/header owners,
retains sticky failure and performs fresh consuming handoff. CFWS and outer
angles are omitted, while token spelling, folds and Unicode diagnostics use
the existing conversion policy. Selected-header retention, JSON, reference
resolution and publication remain separate.

M06cl moves the existing URI syntax validator into std-only td-header,
retaining its required scheme and allowed fragments. Mail retains URL-list
placement, whitespace, complete-field validation/replay, original job/email
admission and typed errors. Internal IPv6 parsing keeps the same prepaid
work and bounded buffer. No second URI parser remains in mail; relative
Content-Location spelling and its projection are subsequent work.

M06cm adds generic URI-reference spelling to the shared validator with
fixed scheme-prefix disambiguation. Existing mail URL consumers retain the
scheme-required constructor and original grammar/work; no base resolution,
Content-Location field placement or retained metadata authority is added.

M06cn adds a shared bounded URI wire-whitespace remover with literal octet
provenance within the supplied spelling slice and sticky refusal.
Content-Location composition must select the URI spelling outside CFWS and
unfold before decoding encoded words; this primitive alone grants neither
field validity nor label/publication authority. Encoded-word placement and
trailing-CFWS selection remain open composer policy: whitespace removal
erases spacing, and parentheses are URI characters. Gaps between octet
offsets show removed whitespace but grant no placement proof; callers
separately rebase those offsets to their field/message.

M06co separates private encoded-word transfer/charset progress from its
borrowed Word wrapper. Public consumers retain their source and original
budgets; progress can also follow caller-owned fixed scratch across moves,
provided the same recognized logical bytes remain immutable. Shape checks
cover charset, encoding and length, not byte identity or placement. This
supplies no new field grammar or Content-Location label authority.

M06cp composes URI wire unfolding and complete encoded-word recognition
for one caller-selected, placement-authorized candidate. A fixed 75-octet
scratch remains immutable during decoding; private relative word metadata
reconstructs views after moves without self references or repeated scans.
Oversized or unknown candidates request whole literal replay. All wire folds
are validated before scalar emission, including tails after scratch fills.
Recognition uses the Text Q alphabet because the authorized candidate is
neither a phrase nor a comment; parentheses and quotes remain payload data,
without granting placement. The original job/header owners charge source,
scratch transitions, recognition, decoding and output. Whole-field
CFWS/placement and URI label validity remain
open; the helper grants no Content-Location or publication authority.

M06db extends the selected URI word reader with caller-authorized
complete encoded-word runs. Shared passive framing locates bounded
candidates while existing mail recognition supplies grammar and charset
policy. Complete wire fold validation and whole-run classification
precede every scalar. Unknown, oversized, touching or mixed literal runs
request whole-spelling fallback without decoded prefix output. Healthy
runs replay through one fixed 75-octet buffer, funding all three wire
passes and both recognitions per word through the same original owners.
Diagnostics accumulate across words. Decoded labels preserve spaces,
non-ASCII and decomposed spelling without NFC or URI validation.
Whole-field placement, label matching and publication remain enclosing
responsibilities.

M06dc composes original-owner URI spelling selection, whole encoded-word
runs and complete literal fallback for one authorized field-value slice.
Word placement is limited to the entire selected spelling. Only completely
recognized runs decode; other spellings retain all word-looking markers in
literal replay after URI validation. Decoded labels receive no URI check or
normalization. Child phases remain exclusive under the original owners;
fresh handoff never adds charge or renews credit. No metadata escapes before
whole healthy completion, and every refusal hides the End and retires its
parser. Only M06df's consuming malformed-syntax path retains original owner
references for freshly admitted discard. Empty-reference presence, discovery,
label matching and retained response publication remain external.

M06dd binds complete authorized location scalars to shared td-json framing.
One bound source and original owner set funds both scalar output and exact
serialized JSON bytes. Framing adds no source scans, interpretation credit,
normalization or URI policy. Empty output and pending escape drains still
freshly admit; whole JSON completion alone exposes passive field metadata.
Any refusal retires source/frame and hides metadata, with bytes provisional
through fresh consuming finish. Null selection, retained buffers and response
publication remain separate obligations.

M06de retains an optional caller-selected complete location JSON fragment in
one fixed caller-reserved window. Absence and a present empty reference stay
distinct. Share checked backing identity/prefix bookkeeping in std-only
td-json and migrate CID/language pair retention to it in the same landing.
The shared primitive supplies no source or completion authority. Enclosing
cursors keep original exclusive admission owners, freshly consume completing
children without added charge and hide all fragment metadata on any refusal.
Whole retained views remain provisional through fresh final admission and
whole-job publication. M06df owns first-valid location discovery;
M06dg owns source-bound retention; traversal response integration remains
separate composition.

M06df selects first-valid resident Content-Location fields with the existing
raw scanner and complete authorized field child. Only malformed needed
occurrences can be skipped; first empty references remain present. A syntax
refusal retires its parser/value and retains only original owner references
for a freshly admitted internal consuming discard. Work, interpretation,
nesting and internal refusals never enter that path. Later duplicates are
scanned without value interpretation; no selection/diagnostic escapes before
the complete raw boundary. Original scalar output costs remain funded even
when selection discards provisional scalars. Selected extents preserve the
original resident source; M06dg binds retained composition while publication
remains external.

M06dg binds the completed resident first-valid location selector and optional
JSON retention to one immutable authorized Input and original allowances.
Only that Input's selected raw extent is replayed into distinct reserved
backing. No replacement source can enter between phases. Whole-view
visibility includes both completed header selection and retained JSON;
capacity or later admission refusal hides both. Fresh consuming finish
returns original reusable allowances, with publication and label resolution
still owned by the enclosing response job. This composition introduces no
new parser arena or retained-storage grant.

M06cq composes shared URI unfolding and reference validation into a selected
literal spelling reader. It validates the complete spelling before replaying
literal ASCII octets and slice-relative provenance under original job/header
owners. No source-sized scratch, normalization, percent decoding or encoded
word interpretation is introduced. Whole-field CFWS/placement, presence,
label retention and publication remain separate composer obligations.

M06cr shares a fixed URI spelling selector with caller-authorized surrounding
CFWS policy. POLICY.md freezes the ambiguous parentheses rule. Exclusive CFWS
or whitespace-probe child state returns original slice offsets without
copying the selected value. Leading grammar failure rejects; optional suffix
grammar can remain literal, but work/internal refusal never falls back. The
helper owns no original allowance or field/word/URI authority. Complete mail
composition, presence, retained metadata and publication remain open.

M06cs binds the shared URI spelling selector to original job/header owners,
with no new CFWS grammar or field authority. Every probe/replay remains
charged; consuming fresh handoff returns the same owners and slice offsets
for the next phase. Empty or invalid URI spelling can complete selection
without granting URI/presence validity. Complete Content-Location word/literal
composition, retained metadata and publication remain separate work.

M06ct composes whole field-value CFWS selection with the literal URI reader
under exclusive child ownership. Fresh consuming phase handoff transfers
the same original job/header owners without a selected-value copy. Complete
selection and URI/fold validation precede literal octets; source provenance
is rebased to the original supplied field-value slice. Caller explicitly
chooses literal interpretation and owns encoded-word placement/path choice,
field discovery/presence and retained metadata/publication. Empty references
remain permitted. Complete Content-Location word/literal dispatch remains
open; this path grants no resolution or source authority.

M06cu selects first-valid Content-ID and Content-Language raw fields from a
complete resident entity header section. Existing strict single-identifier
and shared complete-language-list syntax run as exclusive children of the
same original job/header owners. Malformed needed occurrences are skipped;
nesting, raw-header limits, incomplete source and admission refusal retire
all selected fields. Later duplicates are scanned without interpreting
values. Passive absolute extents remain hidden until complete section
success and provisional through fresh admission. Projection/retention and
traversal coordination remain separate; selection grants no source or blob
authority and allocates no label string or list.

M06cv retains those two passive raw extents in completed part-header Views.
The label selector runs after filename completion with the same authorized
entity and original owners; both scans must agree on section/body bounds.
No View is exposed until labels complete. Later label failure retires prior
heads/charset/filename results. The original parser reservation and caller
windows suffice; label strings/lists, location dispatch and automatic
traversal/JSON integration remain separate.

M06cw binds selected Content-ID projection to shared std-only JSON string
framing under the same original job/header owners. Whole-value grammar
success precedes identifier scalars; any opening quote and subsequent JSON
fragments remain provisional through fresh completion admission. Ordinary
JSON escaping adds serialized output charges to existing conversion work.
The fixed framer/source pair allocates no retained string, applies no NFC or
encoded-word decoding, and returns original owners with the existing
encoding-problem diagnostic. Missing-field mapping, label arrays and whole
part/response composition remain separate.

The CID JSON binding uses the generic framer directly to preserve its typed
CID error domain without widening the older header-property json_string
error roster. CID Begin/End are non-scalar structural events from its fixed
single-identifier projector; they yield while the child enforces its own
sequence. Premature JSON finish and child source-state refusal retain their
distinct outer/source error contexts. Future metadata composition must adapt
those contexts explicitly rather than silently flattening them.

M06cx extracts generic string-array framing into std-only td-json and
atomically replaces the mail MessageIds/URLs framing mechanism. Mail retains
whole-field validation before array output, mode/null policy, original
admission owners and contextual errors. Shared punctuation and string
escaping retain fixed paid progress without retaining values or granting
source/publication authority. Content-Language binding remains separate.

M06cy binds selected Content-Language grammar to the shared string-array
frame under original job/header owners. Ordered, original-case tag extents
replay one charged ASCII byte at a time without a retained list. Grammar and
replay share prepaid credit; exact JSON bytes consume original output work.
Earlier serialized tags retire on malformed tails or any later refusal.
Fresh whole-list/framing completion precedes consuming original-owner
handoff. Null/presence mapping and retained MIME/response composition remain
separate; the binding grants no locale, source or publication authority.

M06cz retains the selected CID/language JSON pair in separate caller-reserved
windows. Exclusive child phases consume and return the original job/header
owners without another output charge or source-sized temporary. No fragment
escapes before complete healthy pair finish; a later language, capacity or
admission refusal retires the earlier CID too. Missing values remain absent.
Field selection, source authorization, optional/null policy and enclosing
response publication remain separate. The child handoff may forfeit prepaid
credit; it cannot renew any allowance.

M06da composes complete retained part headers with the selected label JSON
pair under the original job/header/scratch owners. It checks selected field
extents against the recognized header section and maps them only into the
same authorized immutable entity. One live child owns the budgets at a time;
no completed header metadata escapes before whole healthy label completion.
Failure hides both Views without wiping provisional caller backing. Healthy
fresh finish returns passive metadata and the original reusable owners.
M06dh adds source-bound location in a third exclusive phase. All retained
metadata remains hidden until its boundary agrees with the earlier header
view. Location refusal retires previous groups too; original scratch stays
parked during that child. Traversal and response publication remain separate.

M06di retains the traversal's per-part default context in an existing
packed descriptor flag, independent of selected type. Later metadata must
use that context with the same original source extents; passive descriptors
cannot authorize or bind a source. Digest defaulting is not inferred from
a child's final media classification.

M06dj binds complete traversal to its original immutable source and
keeps its original job/header owners through per-part metadata. The
binding alone maps descriptors to clipped entities/context and
constructs the retained child; callers cannot replace source or budgets
between phases. Healthy metadata correlates raw body starts before
exposure. A child refusal retires the binding. Part construction marks
it Abandoned until successful consumption, including after safe
forgetting. Fresh binding finish precedes original-owner release.
Passive earlier copies remain provisional; source/blob authority,
body-list/response serialization and publication stay external. No
simultaneous traversal frame set or metadata table is added.

M06dk consumes bound retained headers into compact body-list nodes with
original descriptor parent/depth and original job/header/scratch owners.
Fresh metadata consumption precedes fixed classification comparisons and
whole-node output admission. Classification refusal keeps the binding
retired; success alone restores it. Return original metadata and Part
ordinal beside the node for one-pass response consumption. Caller nodes
share the existing body slot reservation, without another metadata
table. Complete preorder, whole-part visitation, source authority and
response publication remain external. Existing body-list traversal
checks supplied depth/parent shape but cannot certify sibling order or
completeness against the source.

M06dl owns complete ordered classification. A private source-bound owner
chooses each original ordinal exactly once, writes only freshly consumed
nodes into separately admitted fixed slots and exposes a complete node
set only after all descriptors finish under original owners. Part
consumption also returns original metadata for one-pass response use.
Safe forgetting, capacity/child refusal and extra-part requests cannot
leave healthy completion. Finishing ordered construction and consuming
the completed owner freshly admit; passive views do not confer source or
publication authority. Source-bound body-list/response serialization
remains a separate next boundary.

M06dm consumes private complete classification into original-source body-list
selection. The existing iterative selector reads only that owner's original
nodes and retains the original job/header allowances. No replacement node
slice, source or budget enters the transition. Fresh admission precedes local
constructor checks; complete selection creates a private owner retaining the
same immutable source binding, original header limit, cells and passive lists.
Caller selection limits only add refusal policy over already bounded nodes;
they do not renew traversal or job allowances. Explicit
checks and consuming finish retire all visibility after refusal. Caller list
windows remain separately admitted; there is no new parsing, charge,
allowance, source-sized scratch or selection algorithm. Response composition
must consume that private selected owner, never reconstruct it from passive
Views. Serialization, authenticated locators and publication remain external.

M06dn consumes private selected ownership into original-source response
metadata replay. Fresh constructor and next-part admission precede local
state checks. Every original ordinal is visited once using the original
source, context, header limit and job/header owners. Existing metadata
projection supplies the only live child, with one reused caller window and
scratch; rereads and output spend original allowances. No second
classification, node output charge, metadata table or renewed credit is
introduced. Only successful fresh consumption advances visitation; child
refusal or safe forgetting retires the complete owner. Fresh whole replay
finish produces a private completed owner retaining original source and
allowances. It proves metadata visitation, not response bytes or publication;
JSON framing, locators and output admission must compose with that visitation
in later increments. The next framer wraps each live Part atomically before
its window is reused; Projected alone contains no retained part metadata or
serialization proof. Copied cells and previously emitted metadata remain
provisional through final whole-job admission.

M06do wraps a live complete replay Part atomically in fixed metadata
member framing before window reuse. Its parent remains abandoned until
fresh whole serialization finish; refusal, premature finish or safe
forgetting retires visitation. It frames selected scalar fields through
the shared JSON string engine, copies only privately retained complete
CID/language/location JSON, and uses original allowances for scalar
rereads, bounded control steps and every wire byte of the new fragment,
including copied retained metadata. Apply final charset
defaults/non-text absence, preserve selected label spelling, and enforce
the JMAP unsigned-size ceiling. No new normalization, field selection,
metadata arena, source authority or parsing algorithm enters. The member
fragment excludes braces, blob IDs and subparts; tree/body-list
composition, property selection, locators and publication remain later
boundaries. Completion returns passive descriptor/node evidence and
original owners, not a publication capability.

M06dp binds the original live replay child and a distinct
caller-reserved fragment window to private framing plus shared fixed
retention. It accepts no advanced framer that could omit an
already-emitted prefix. Admission precedes storage capacity; for a
retention-bound child, only fresh whole retention consumption advances
visitation. Failure, premature finish or safe forgetting retires the
parent, including after Complete. No output is charged a second time by
retention. The retained fragment/end are passive and provisional; whole
composition must wrap this live handoff with the original replay owner
before publication. Bare Part/framer finishes still advance visitation
without retention; later whole composition must consume the live
retention path to prove storage. No new parser, arena allocation,
allowance or source identity enters.

M06dq wraps original selected replay, metadata framing and fixed
retention in one whole collection. Exactly one separately admitted
cell/window belongs to each original ordinal. Its child cannot detach
either phase; only a fresh whole retained handoff stores that cell and
clears collection abandonment. Retained passive bytes and Projected
visitation cannot reconstruct this proof. Whole Serialized completion
preserves original selection, preorder fragments and original
work/header owners through fresh checks/consumption. Control steps and
cell records use the original allowances, with no repeated wire debit.
The collection adds no normalization, metadata parser, JSON serializer,
source authority or publication algorithm. Enclosing body-list/tree
composition, braces, locators and publication remain later boundaries.

M06dr consumes exclusive whole Serialized ownership to compose either
original bodyStructure metadata or original selected body-list members.
It never reconstructs authority from passive fragments or visitation.
Generated fragments are copied unchanged; original preorder/depth/parent
identity drives a fixed 64-frame iterative tree. Original allowances
fund bounded control and every newly emitted byte, with no source/I/O
replay charge. Fresh complete consumption yields exclusive Composed,
which proves emission only. Whole fixed retention must construct its own
fresh composer from Serialized; an advanced composer or Composed cannot
prove that an omitted prefix was retained. Locators, property selection
and complete response publication remain subsequent boundaries.

M06ds binds original whole collection to fresh private composition and a
fixed separately admitted member window. An advanced composer or completed
emission cannot provide constructor authority, so an omitted prefix cannot
become retained proof. Shared Window records only reported bytes, without
additional wire debit. Fresh complete retention yields exclusive Retained
with original Composed and full bytes; passive ViewBytes cannot reconstruct
that owner. Fresh checks/consumption preserve original allowance identity
and sticky refusal. These members remain provisional through authenticated
locators, request property selection and complete job publication.

M06dt consumes original whole retained metadata to derive provisional
p1 candidates from every original descriptor. Relative encoded extents,
transfer tags and multipart nulls use the frozen wire contract; no
passive imported descriptor can enter the owner. Each bounded turn funds
canonical bytes and its stored-cell record in original allowances.
Exclusive Mapped preserves original retained completion through fresh
checks/consumption. Its parent BlobId is untrusted: source-identity and
live account/lease authorization must bind this proof before issuance,
download or reuse. This adds no permission, parent pin, body decoding,
nested locator context or complete response publication.

M06du binds that original Mapped to a real complete PinnedBlob by parent
ID, full length and an incremental digest of all original resident bytes.
Bound keeps both exclusive owners, including the actual pooled-view pin.
The pin's own query clock fences digest work and final consumption; a
healthy supplied parsing Tick cannot revive expired body access. Root
authorization remains the existing caller obligation before opening the
pin, and source matching alone grants no permission. Hash visits are
bounded and funded in original work, with no replacement allowance.
This adds source identity, not authenticated issuance or publication;
binding-path allocation/whole-service qualification remains open.

M06dv qualifies only eight source-binding Rust allocation intervals using
actual retained body pins and original MIME owners in one test-only
source graph. Cold setup and original mapping precede counting; binding
construction, digest work and binding-owned snapshot-pin teardown are
measured; enclosing pooled-view/scratch-lease release stays outside.
Successful release, actual-clock refusal, source mismatch and original
work refusal preserve every counter. This leaves native/whole-service
resource qualification and authenticated publication open. Unmeasured
source-binding cases are explicitly listed in API §1.127.

M06dw emits an original part metadata fragment plus its source-bound
leaf blobId or multipart null while retaining original Bound and the actual
snapshot-bound body pin. Each turn funds at most 64 new wire bytes in original
work, with no retained-source reread. Actual pin fences surround work;
complete emission creates an exclusive Member, freshly checked before
releasing original Bound. Partial/error bytes remain provisional and
must be discarded. This is one part-member building block; current
access policy, whole tree/list composition and publication remain open.

M06dx binds a fresh original source-bound member emitter to a fixed
caller window and retains the whole fragment plus blobId before granting
exclusive Retained ownership. Deadline checks precede capacity, with
original output charging and post-work pin fences preserved. Checked
fragment length plus 81 reserves the worst suffix while all original
backing and actual snapshot pin overlap. Fresh final release preserves
original Bound; passive retained bytes alone cannot establish source
custody or publication. Whole collection/composition and current access
policy remain later boundaries.

M06dy collects all original source-bound whole members into separately
admitted fixed cells. Child handoff retains exclusive source and actual
pin custody; abandonment cannot skip an ordinal. Fresh complete child
acceptance funds original work and samples the actual pin before
installing its original cell and returning Bound. Whole Serialized
ownership requires every original ordinal and stays fresh through
release. Cells remain provisional; composition, request selection and
current access policy/publication still follow. Only the eight allocation
intervals named in API §1.130 are qualified.

M06dz emits original source-bound whole tree/list members while keeping
every collected member and the actual pin. One shared bounded Frame
serves the existing metadata adapter and the new source-bound adapter;
its ordinal/depth/parent walk and list framing remain identical. Every
turn funds original work and samples the actual pin after work even on
quota refusal. Exclusive Composed preserves collection custody through
fresh release; emitted prefixes remain provisional. Whole response
retention, request selection and current access/publication follow.
Only the eight allocation intervals in API §1.131 are qualified.

M06ea retains whole source-bound Structure/Lists property members in a
separately admitted fixed window. It starts only from original complete
collection, privately creates the fresh composer and reuses td-json
Window; only successful reported bytes advance, with no additional work
fees. Original collection and actual snapshot pin remain held through
fresh complete Retained and final release. Capacity/deadline failures
are sticky; whole output stays provisional through current authenticated
publication. Only API §1.132's eight allocation intervals are qualified.
Whole ledger admission, request selection and publication remain later.

M06eb selects requested textBody, htmlBody, attachments and hasAttachment
members through the same bounded Frame while retaining original complete
collection and actual body pin. A pure four-boolean selection supplies no
permission; full-list callers retain their prior ALL behavior and fees.
Empty selection freshly admits, then completes without output or charge;
all explicit/final owner boundaries remain fresh. Exclusive selected
Composed carries selection and original collection through release.
Only API §1.133's eight allocation intervals are measured. Whole selected
retention, broader request routing and current publication remain later.

M06ec shares the existing source-bound retention core to retain whole
requested list members from a fresh
original Serialized and Properties into the shared Window. Empty
selection freshly completes into zero capacity. Original collection,
selection and actual pin remain exclusive through Retained and fresh
release. Advance only successful reported bytes with no additional work
fee; unfinished freshness precedes capacity and all refusals are sticky.
Only API §1.134's eight intervals are allocation-qualified. Whole ledger
admission, wider request routing and current publication remain later.

M06ed routes pure requested bodyStructure and list properties through
the same bounded Frame, retaining original collection and actual pin.
Tree first then selected lists yields canonical keys and separators;
single-mode legacy bytes/turns/funding remain unchanged. Exclusive
requested Composed carries selection through fresh original release.
Only API §1.135's eight intervals are allocation-qualified. Whole
requested retention, per-part bodyProperties, JSON request decoding,
full admission and authorized publication remain later.

M06ee retains whole requested source-bound tree/list members through
the shared Window while holding original collection, requested selection
and actual snapshot pin through fresh Retained/release. NONE accepts zero
capacity; nonempty final-copy exact fit completes. Advance only
successful reported output, with no additional work fee, fresh
unfinished checks before capacity and sticky hiding. Only API §1.136's
eight intervals are qualified. Per-part properties, request JSON
parsing, whole admission and current publication remain later.

M06ef selects ten existing source-bound part fields while retaining the
original Bound, ordinal and actual snapshot pin. ALL preserves original
emission; NONE freshly completes without work. Partial selections use a
fixed bounded, funded index of original generated metadata, not a JSON
request/source parser or reconstructed owner. Preserve selected strings,
arrays, blob wire/null and canonical commas; freshly finalize both owners.
Only API §1.137's eight intervals are allocation-qualified. Whole selected
retention/collection/tree integration, headers/subParts, JSON requests,
full admission and authenticated publication remain later.

M06eg whole selected part-field retention privately configures a fresh
original-bound emitter from Bound, ordinal and pure ten-field
Properties. Legacy ALL and the selected adapter share the existing fixed
Window, polling, sticky failure and fresh finalization; no advanced
emitter enters whole retention. The adapter holds only immutable
selection beside the shared core and maps whole views/release labels.
Exact fit completes on the final selected copy; NONE admits empty
complete into zero capacity with original custody and fresh consuming
boundaries. Generated-index/copy work keeps §1.137's exact five-cost
charges. API §1.138 defines passive bytes and measured Rust intervals;
collection/tree integration, headers, header:*, subParts, request
defaults/parsing, full admission and current authenticated publication
remain later.

M06eh original selected-field collection privately threads one immutable
ten-field Properties through the existing source-bound collection core
and each fresh whole-retained child. Legacy public construction passes
ALL. The selected facade preserves labels while sharing exact
cardinality, ordinal custody, abandonment, sticky child/parent refusal
and fresh complete acceptance. NONE needs zero-capacity child windows
and one Cell per original ordinal. It pays no emission work but still
pays the original ordinal handoff. Complete Serialized/released slots
remain passive and keep original
Bound/descriptor custody; no public conversion strips selection into a
legacy complete owner. API §1.139 defines the bounded state and eight Rust
intervals. Tree/list composition, request parsing and current
authenticated publication follow.

M06ei selected-field composition retains §1.139's immutable metadata
selection and original collection custody while delegating to the shared
source-bound frame. The provisional tree/list structural subParts
scaffold is independent of those ten fields; NONE removes metadata, not
nodes or list entries. Suppress its leading comma for empty whole
metadata. ALL keeps legacy turns and charges. Fresh consuming/explicit
checks and passive labels remain the existing boundaries. API §1.140
scopes eight Rust intervals; request parsing/defaults,
headers/header:*/subParts selection, full admission and current
authenticated publication remain later.

M06ej requested outer properties preserve independent selected part
metadata through the existing requested source-bound frame. Immutable
outer Properties remain in the original requested owner and metadata
Properties remain in its facade; complete views/releases preserve both.
Outer NONE spends no composition work but keeps complete original
collection custody and fresh consuming boundaries. Structural subParts
remain the provisional internal scaffold. API §1.141 scopes eight Rust
intervals; whole output retention, request parsing/defaults,
configurable headers/header:*/subParts selection, full admission and
current authenticated publication follow.

M06ek whole requested selected-field retention reuses the original
fixed-window core under exclusive selected facades. Complete Serialized
alone starts fresh requested composition; immutable part and outer
selections survive complete output and fresh release alongside original
backing/cells/descriptor custody. Capacity and work refusal prevent
completion; original-job/actual-pin refusal hides completed views and
remains sticky. API §1.142 scopes eight Rust allocation intervals and
caller-owned windows. Structural subParts remains a provisional
scaffold; configurable headers/header:*/subParts selection, request
parsing/defaults, full admission and current authenticated publication
remain later.

The M06ei–M06ek constructors above retain their legacy structural
scaffold. API §1.147 adds explicit subParts selection through the same
engine; header rendering, full admission and authenticated publication
still remain later.

M06el decoded body-part key recognition distinguishes all twelve
standard EmailBodyPart fields and reuses the existing bounded header
selector in BodyPart context. Result tags and borrowed header plans
remain passive; unknown keys stay for the enclosing dispatcher, never
become ALL. API §1.143 scopes bounded paid comparisons and eight Rust
allocation intervals. JSON/argument parsing, defaults, duplicate policy,
full selection aggregation and configurable header/subParts composition
remain later; this recognition does not change the provisional
structural scaffold or grant current publication authority.

M06em aggregates decoded body-part keys through the BodyPart recognizer
into ten metadata flags, structural flags and a caller-owned borrowed
header-cell prefix. Exact requested header keys deduplicate in bounded
paid two-sided comparisons while preserving first spelling/loan and
request order. Unknown/form/capacity/work failures forbid whole success.
Explicit empty stays NONE; the pure omitted-argument default constant
contains only the ten metadata fields. API §1.144 scopes passive result
semantics and eight Rust intervals. JSON shapes/default dispatch,
configurable structural/header rendering, full admission and current
authenticated publication remain later.

M06en decodes an admitted UTF-8 bodyProperties JSON fragment into fixed
caller-preallocated Strings and key-reference slots. Omitted differs
from explicit empty/null; complete syntax and scalar escapes precede
semantic selection. Input, record and scalar-output work are prepaid;
capacity refusal never grows storage. Whole success borrows only the
completed prefix. Cells are cold/reset storage tied to decoded loans,
not a persistent request-arena pool. API §1.145 defines exact turn and
EOF costs and the eight isolated Rust intervals. Full request decoding,
composition wiring/admission and authenticated publication remain later.

M06eo joins admitted JSON fragment decoding and original BodyPart
semantic selection through a single passive request cursor. No whole
view escapes between stages; omission alone immediately selects the
ten-field default. Transfer complete decoded key loans without copying
or adding a second parser turn. Original sticky errors, exact debits,
first header loans and caller storage stay intact. API §1.146 scopes
the eight Rust intervals and fixed cursor footprint. Whole request
framing, configurable composition/admission and authenticated
publication remain later.

M06ep selects structural subParts through the existing requested framing
engine and original selected collection custody. False closes root-only
bodyStructure and classified list leaves after metadata; true preserves
legacy tree/null bytes and costs. Immutable structural/outer selection
survives completion and original owner release without widening the ten
metadata flags. Both freshness domains remain enforced. API §1.147
qualifies eight Rust intervals; header rendering, whole admitted request
wiring and authenticated publication remain later.

M06eq retains complete selected subParts composition in the original
shared fixed Window. Original fresh selected collection custody begins
retention; advanced/emitted-only/passive inputs cannot recreate a lost
prefix. Selection, metadata and original pointers survive whole values
and consuming release. Short capacity and explicitly checked expiry in
either freshness domain hide whole success stickily. Completed passive
values survive expiry until a check or consuming operation. API §1.148
scopes eight Rust intervals. Header rendering, full admitted request
wiring and authenticated publication remain later.

M06er streams source-ordered EmailHeader JSON through the original resident
header Scanner, Raw scalar engine and JSON string frame. Names preserve case
and duplicates; values preserve folding/leading whitespace and existing Raw
filtering/replacement policy. Original job and interpretation budgets move
exclusively between scanning and projection. Emitted fragments remain
provisional; fresh complete handoff returns only the original budgets and
passive header boundary/diagnostic. API §1.149 scopes its bounded cursor and
eight Rust allocation intervals. M06es below qualifies whole-array retention;
configurable body-part header wiring and original pin/authenticated publication
admission remain later.

M06es retains a whole resident header array in one original caller-reserved
fixed Window by constructing its own fresh streamed header owner. Advanced
streams and emitted-only completions cannot reconstruct an already-lost
prefix. Fresh admission precedes capacity refusal; a full healthy Window
refuses before the next producer turn can expose a source failure, with no
refund of paid work. Capacity refusal is sticky and retires original budget
custody;
whole passive bytes require healthy completion. Exact capacity preserves
original Raw/order/case semantics and all streamed charges. Completed polls
remain inert; explicit checked expiry hides values and consuming finish
requires fresh original job/interpretation admission. Passive retained bytes
and budget Completion grant no original pin or publication authority.

The four-list-only selected source-bound adapters retain a narrower type
contract than requested tree/list selection: their Properties cannot
select bodyStructure. They remain public facades over the same shared
Frame and retention core, with no independent polling or capacity engine.
The broader requested facade keeps its immutable five-property label;
the Frame's mode is an internal active walk, a legacy constructor label
only. These are selection types, not JSON request default policy.

The reusable CFWS, delimited-token and MIME parameter-name/value cursors
live in td-header with generic caller-owned admission. Its passive resident
extent helper shares checked absolute slice mapping without reading bytes or
granting source authorization. Mail grammar placement, scalar decoding,
normalization, deadlines and budget adapters remain in td-mta. The shared
lexical crate owns no field selection, display policy, clock or crypto and
retains the existing fixed-work/source-extent contract.
The value cursor validates complete raw token/quoted spelling before
projecting provisional octets with ordinary/extended prefix roles. Candidate
assembly and charset/display/boundary conversion remain in mail. Zero-count
fresh admission is shared by the name/value wrappers without spending
parsing steps or prepaid credit.
The passive language-tag feed state is shared by parameter prefixes and
encoded-word qualifiers; recognition retains its precharged 75-byte ceiling.
Stateless shared projection now unquotes/folds MIME value, phrase and
comment octets through one reader. Caller callbacks preserve source
bounds, distinct errors and EOF charges; escaped-fold provenance remains
available to mail display consumers. Strict logical character assembly is
also shared: separate read/local verification callbacks borrow the
original work context sequentially. The first logical octet retains escape
provenance; filtering and placement remain mail policy. All mail
display/body noncharacter checks use one table-free Unicode predicate
without changing their distinct control/output rules. Original placement,
charset decoding/filtering, normalization and sticky admission/failure
ownership remain in mail. Projected bytes create no lexical validity or
display-word placement proof.

Complete parameter-family selection remains mail policy. The fixed-state
owner validates the whole field, classifies names, then replays every numbered
index under the original work and header allowance. It retains only source
extents and passive rejection evidence; no section table or decoded output.
POLICY.md owns precedence and fallback. All derived output remains provisional
until a later charged drain and conversion completes. This selector fits the
existing parser reservation; ancestor frames must not each retain one.
The octet owner reuses that inline selector for complete validation and then
charged replay, retaining its original budgets and credit throughout. It
accepts source/kind/attribute rather than a caller-manufactured plan; rejected
family octets never escape selection. The complete field grammar proves an
ordinary value before replay: it shares td-header's delimited quoted-string
and MIME token rules with mime_value's Ordinary validator. Replay rechecks
that complete spelling before octets. Replay failure retires all provisional
output and cannot choose a second fallback. Charset/language/data roles are
lexical evidence only; original-source placement and metadata publication
remain with later owners.
Literal Name/Filename scalar conversion composes validated octet replay with
one decoder across all data sections. A fixed incremental matcher and slice
lookup share the exact existing mail charset alias table; no label buffer or
registry is introduced. Unknown/empty declared labels recover as POLICY.md
specifies. One held byte separates replay and decoding turns, and original
budget/private credit persist through both. Charset diagnostics remain
separate from rejected-family evidence. Scalar controls/noncharacters, raw
word placement, filtering/NFC and display publication stay with later owners.
Display conversion branches to a private original-raw Ordinary decoder only
after complete family choice and before data conversion. Raw quote/LWS edges
and contiguous unescaped words establish filename compatibility; neither
projection nor decoded punctuation supplies placement. Extended/numbered data
remain literal. Filtering precedes later NFC, and original allowances/credit
span the inline handoff, word recognition and charset work. All scalar output
and diagnostics remain provisional until fresh display completion; response
retention and final metadata authority are still later-owner boundaries.
New extractions must preserve the caller's bounded-work and memory contract.

The core may contain owned tables generated from the approved, checksummed
Unicode 17.0 inputs in ../td-mime/UNICODE.md. They add no Cargo dependency or runtime
data fetch. The approved corpus and license now live in the checkout and are
verified by the ordinary offline test suite and a cold tooling example. M06
now has reproducible offline table generation, fixed runtime lookups and
bounded NFC over resident valid UTF-8, verified against the complete official
NFC equations. M06t composes resident unstructured-header decoding with NFC
and bounded charged replay. Keywords and List-Id Text now retain source
punctuation while decoding original phrase/comment words with fixed lexical
placement state and shared normalization. POLICY.md owns malformed display
recovery. Content-Type and Content-Disposition Text decode original comment
words while retaining parameter spelling. A bounded resident MIME syntax
cursor now reuses the CFWS and quoted-string validators, returning raw token
and parameter extents under original job/email admission. Events remain
provisional until whole-field validation. A fixed-state metadata selector
now retains the first completely valid occurrence of each MIME field, then
applies normal or digest-child Content-Type defaults after the entire header
section validates. Header, nesting and work refusal retire all candidates.
Derived parameter metadata, MIME part traversal and protocol integration
remain open.

The core also contains 27 positive leap insertion dates generated from the
approved, checksummed IANA input in ../td-mime/leap-seconds/README.md. Offline tooling
verifies the complete pin before generation; runtime Date projection uses
the 108-byte static table without file access or allocation. Expiration is
provenance metadata and does not invalidate historical insertions. Unlisted
second-60 dates remain unverified. Updates require a reviewed source pin
and regenerated table.

These are named data dependencies, not permission to import a Unicode or
mail parsing library.

The backend admission and gate contract lives in td-crypto/DESIGN.md. All
three packages stay in the test roster. The user approved Rustls with AWS-LC;
M03b1 records exact transitive pins, features, licenses, roots and rationale;
M03b2 pins the portable native inputs. The separate, user-approved private
PostgreSQL storage closure is pinned in td-crypto/DEPENDENCIES.md and
POSTGRES.md. No external async runtime, web framework, mail parser,
serialization or ACME framework rides along.
The backend increment demonstrates static musl linking and bounded TLS
behavior before consumers depend on it; exact versions belong in the
lock/review.

The portable musl artifact has a separate, host-only build manifest: pin Rust
and its target standard library, the musl headers, the C compiler and binutils
needed by the crypto provider (the pinned Debian build root's), and their
source/artifact checksums and provenance.
Provision them before offline builds; ambient host tools cannot silently fill
missing inputs. M03b2 owns these exact pins and a clean build fixture. This
portability workflow is outside td's source-built glibc target artifact graph;
it must not import host-built outputs into that graph or claim its provenance.

Core adapters provide typed errors and caller-owned buffers for transport,
clock, cryptography and fault-injected persistence. Network workers cannot
bypass the store's commit API. TLS framing/cryptographic verification belongs
to td-crypto; mail STARTTLS transitions, deadline/lease accounting and gateway
allowlist authorization belong to td-mta. API section 1.1 owns that integration.

Future td-owned cryptographic primitives replace AWS-LC internally within the
same td-crypto facade. Its private Rustls provider bridge changes there too;
mail/storage code and the direct dependency remain stable. F04 requires
cryptographic, interoperability and resource qualification before deployment.
Replacing only the direct digest/signing operations does not replace TLS's
cryptography. Retaining Rustls means td-crypto still has an external TLS
implementation even after AWS-LC is removed.

The production library retains `forbid(unsafe_code)`. The binary denies it
with one function-scoped exception in `signal_sys.rs`, recorded in
UNSAFE.md section 22. On x86-64 and aarch64 Linux, serve blocks SIGINT and
SIGTERM before starting workers, then consumes them synchronously with
zero-timeout `rt_sigtimedwait` in its control loop. The two fixed raw
operations are `rt_sigprocmask(SIG_BLOCK)` and `rt_sigtimedwait`, over an
eight-byte mask containing only those two signals. No handler, descriptor,
FFI, allocation, pointer retention or signal-disposition change is involved.
The mask remains in force until this one-command process exits. Other CLI
commands never install it. Source confinement pins the full raw module and
its callers. Process-directed supervisor/terminal signals use the same
orderly stop path as the private control command; thread-directed signals
are not a control interface. Cold configuration and storage setup retain inherited signal behavior;
blocking begins immediately before control binding and worker creation.
Once blocked, both signals request stop even with inherited SIG_IGN; no
disposition is rewritten or signal re-raised. Blocking native I/O during
serving retains its existing limits. CONFIG.md owns shutdown timing and observations.
The process test has the separate UNSAFE.md T3 boundary to signal only its
retained children on both architectures.
The separate test executable
`tests/rust_alloc_probe.rs` has the user-approved allocation instrumentation
exception specified in UNSAFE.md T1. Its single scoped GlobalAlloc
implementation forwards all four operations to System without changing
layouts, pointers, results or failure behavior. Fixed atomic counters observe
requested Rust bytes; native allocation and RSS need separate measurements.
The probe has its own main and no libtest workers. It is never a library
module, runtime feature or installed executable. Source confinement and the
portable artifact inventory/symbol check enforce this boundary.
A separate native allocation qualification executable uses the approved
UNSAFE.md T2 boundary: six libc forwarding wrappers linked only into that
static musl test. Its observations remain diagnostic C boundary counts, may
include Rust System calls, and do not establish whole-service memory.
Any further platform or test instrumentation surface must amend UNSAFE.md and
this design, name each operation, and add confinement tests before landing.

## 4. Deployment and transport

### 4.1 Direct MX

Publish MX records for every served domain pointing to a configured
hostname, for example `mx.example.net`, with A/AAAA records for reachable
addresses. Internet SMTP uses port 25 with STARTTLS; port 25 is not a
plaintext fallback after trying port 465. The receiving listener offers TLS
1.2/1.3 with the explicit reviewed algorithm policy recorded by M07 (API
section 1.1). Dependency upgrades must not silently change this policy. It
accepts plaintext delivery for peers that do not negotiate TLS; selecting
the direct role chooses this v1 compatibility policy. There is no separate
plaintext-toggle setting. Once STARTTLS begins, a failed handshake closes
the connection; it never continues that session in plaintext. Reset SMTP
state after successful TLS.

Port 443 serves JMAP HTTPS and configured MTA-STS policy hosts. Port 80 serves
ACME HTTP-01 challenges and
may redirect ordinary GET requests to a fixed configured HTTPS origin; it
never accepts credentials or JMAP writes. URLs in JMAP discovery are generated
from configured origins, never an untrusted Host header.

Support serving a static MTA-STS policy from the configured
`mta-sts.DOMAIN` HTTPS names. `dns-plan` emits the necessary MX, address, and
`_mta-sts` TXT records and policy text; it does not edit DNS. Start in testing
mode, then allow the operator to select enforcement after certificate/DNS
validation. This is policy publication for senders that support MTA-STS,
not outbound MX lookup or a claim that all inbound sessions use TLS.

### 4.2 Gateway MX

In gateway mode public MX records point to another server. The gateway owns
Internet acceptance/retry and sends to a dedicated td-mta listener. The
deployment restricts that listener using firewall rules AND explicit peer
address admission in td-mta. Require STARTTLS with verified client certificates
for a network gateway; a private VPN can additionally protect the route.
Plaintext is allowed only in an explicitly configured loopback fixture mode.
Trusted peer admission never permits nonlocal recipients.

Configure the gateway listener's server hostname explicitly. The upstream
gateway verifies that name and certificate chain; certificate provisioning and
renewal cover it even when this host is not the public MX. An operator-provided
certificate/private CA is permitted for a private gateway, with the same name
and expiry checks and an explicit operator renewal responsibility.

Gateway client authentication uses a dedicated configured private trust root
and exact allowed certificate identities mapped to gateway IDs. Verify chain,
validity, client-auth usage and identity in addition to peer address admission.
The bundled outbound public roots never authorize gateway clients. A valid
client certificate for another identity is insufficient. Bound trust reload
and define revocation/rotation fixtures before enabling the listener.

The authenticated identity is the gateway, not the original sender. Record
both the socket peer and configured gateway ID. V1 does not implement PROXY
protocol, XCLIENT, or trust arbitrary original-IP headers. Future SPF checks
must not mistake the gateway's address for the originating sender's address.
Gateway-provided authentication results require a separately specified trust
contract before they influence policy.

Direct and gateway listeners have separate limits, credentials, and admission
policies. A deployment may enable either or both for migration. A gateway must
share the valid-recipient policy or validate recipients before accepting mail;
otherwise its accepted unknown-recipient mail can generate backscatter. This
service's gateway support is a receiving role, not a general forwarding MTA.

### 4.3 Smart host and local conformance fixtures

The initial provider is `smtp.migadu.com`, port 465, implicit TLS, and mailbox
password authentication, matching Migadu's published settings. Provider MTA
software/version is unverified. Do not infer it from unrelated deployments or
make compatibility depend on a banner. No live SMTP probing is needed.

Support configured implicit TLS and required STARTTLS submission endpoints.
Verify certificate chains, validity, and the configured hostname; send SNI.
Never send credentials before verified TLS or downgrade after a TLS failure.
Support advertised AUTH PLAIN and LOGIN with bounded challenge/reply handling.
Do not advertise or use an AUTH mechanism merely because a banner suggests it.
The shared EHLO reader retains STARTTLS, supported AUTH mechanism flags and
8BITMIME. Recognize only a bare case-insensitive 8BITMIME keyword; parameters
make that line unadvertised. Valid repeated advertisements are unioned. AUTH keywords are case-insensitive; SASL names are exact uppercase
ASCII names of 1 through 20 letters, digits, hyphens or underscores (RFC 4422).
Unknown valid mechanisms are ignored. An invalid mechanism invalidates that
AUTH line's entire list; valid repeated lists are unioned. The greeting line
is never an advertisement. Capabilities are available only after a complete,
successful 250 reply with no unread tail, under the shared 512-byte line and
16 KiB aggregate bounds. These flags are syntax evidence only: the driver
must discard pre-TLS capabilities and obtain a fresh EHLO on the verified TLS
transport before selecting AUTH. This parser increment does not send AUTH or
activate the relay driver.

The prepared relay EHLO owner accepts only a completed TLS connection with
verified server-name evidence and the configured relay role. It consumes the
initial 220 greeting for implicit TLS, or sends a fresh EHLO immediately after
required STARTTLS. It retains supported AUTH and 8BITMIME flags together with that same
exclusive connection. Wrong codes, malformed/overlong replies, EOF, unread
bytes after a final reply in the current input cell, clock failure and the
original absolute deadline abort the socket. Each turn performs at most one
TLS transport operation and parses at most one bounded control line; four
caller-reserved 512-byte cells hold input, greeting, EHLO reply and command.
The returned session has no authentication, account or queue authority.
The production relay scheduler remains separate work.

The prepared required-STARTTLS relay owner retains the original plaintext
socket and configured relay TLS reservation through the initial 220 and EHLO.
Only a required-STARTTLS policy is admitted. It shares the bounded EHLO wire
driver with the verified-TLS owner, but exposes only a completed STARTTLS
offer tied to that same socket and reservation through an opaque upgrade
owner. No plaintext capabilities or socket can escape it. Upgrade consumes
that owner into the existing bounded STARTTLS/220 driver; native ClientHello
bytes remain private until the successful 220. Initial EHLO, STARTTLS and
handshake retain the original handshake deadline, which may not exceed the
original job deadline. A fresh verified TLS EHLO still determines AUTH and
8BITMIME; pre-TLS advertisements confer neither capability. This is a
prepared owner, not activation of the runtime relay scheduler.

Each compiled relay policy retains a private domain-separated SHA-256 digest
of the exact decoded username/password, framed by lengths. The completed EHLO
session can compare supplied bounded printable-ASCII credentials to that
retained policy using the provider's fixed-digest constant-time comparison.
It retains no second plaintext password copy and sends no AUTH bytes. This is
a local configuration-binding check, not a persisted password verifier,
current-generation admission or successful remote authentication. Credential
rotation changes new policies; old sessions continue to identify their old
snapshot until the runtime retires them. The prepared AUTH owner performs this check before writing any AUTH command;
the runtime must still enforce live queue/route fences. The prepared native
PostgreSQL MAIL adapter additionally requires the authenticated session's
process-local policy ID to match the TLS generation created with the attempt's
selected-profile Binding. That ID is passive provenance, never persisted
queue authority. POSTGRES.md section 183 owns this bound-path check.

The prepared AUTH exchange consumes that fresh-EHLO session and borrows the
original work meter and credential strings. Prefer advertised PLAIN; select
LOGIN only when PLAIN is absent. Never retry with another mechanism after
refusal. The connector prefers a PLAIN initial response when the complete
command including CRLF fits 512 bytes. It encodes the RFC 4616 empty
authorization identity form before output and requires 235 immediately.
Longer PLAIN responses use one empty, single-line 334 challenge before sending
that same form. LOGIN requires two single-line 334 challenges with strictly
valid padded
base64 (empty is allowed); ignore decoded prompt contents and send username,
then password. Both mechanisms require a complete 235 reply after the final
credential response. Early 235, unexpected codes, malformed challenges, EOF,
and current-input-cell reply tails refuse and tear down the connection.

The shared unwrapped base64 encoder finishes each bounded response before any
of its bytes are written. Maximum response storage is 1710 bytes including
CRLF; it is a SASL response, not a 512-byte initial SMTP command. Each turn
performs at most one transport operation and one reply-line parse, or one
bounded codec poll, or one bounded credential preparation. Pre/post clock
checks and the same borrowed meter apply throughout, including pending I/O.
No codec, read or write receives a refreshed meter or deadline. Refusal
releases TLS, socket and retained policy before returning reserved buffers.
Success retains exclusive connection custody, mechanism evidence and the
verified TLS EHLO capabilities. PLAIN and LOGIN install no SASL security
layer and do not reset this capability state. This is not account, route,
queue or current-generation authority. This prepares
local protocol machinery; production relay scheduling is not activated.
The inert native Prepared connector now composes configured resolution,
numeric dialing, implicit TLS or required STARTTLS, fresh EHLO and AUTH.
It binds same-load profile provenance before network work and rechecks native
authority before credentials, retaining an opaque same-session MAIL handoff.
POSTGRES.md section 185 owns its custody and failure contract. AUTH refusals
may expose an isolated complete peer rejection code; partial, multiline,
malformed or current-cell-tailed input supplies no such classification.
Only actual 535 selects this connector's Auth repair continuation.
The inert single-attempt driver consumes that opaque connector and composes
bound MAIL, ordered RCPT, DATA, verified-source Body and final publication.
It retains closed phase-specific evidence and passive interruption custody
on failure; callers settle actual replies and unknown Pending before any
recovery. It does not select jobs, schedule retries or activate PostgreSQL.
POSTGRES.md §186 specifies the handoff and its bounded qualification.


Credential cells and caller strings must not enter logs; no secure-erasure
claim is made for caller scratch or native TLS buffers.

The operator configures permitted From/envelope identities; aliases accepted
inbound are not automatically authorized outbound by Migadu.

The default local fixture offers implicit TLS, AUTH, EHLO extensions, envelope
validation, DATA, and a durable captured message. Its profile models published
Migadu settings, not an asserted clone of Migadu's private configuration.
Scripted peers cover refusal, retry, TLS, and ambiguous outcomes (section 15).
An independently implemented local MTA may be an additional interoperability
oracle after its test-only source pin is reviewed; it is not a runtime input
or a prerequisite silently downloaded by the test suite.

## 5. Resource and execution model

Resource policy separates enforceable limits from performance goals. Bound
untrusted message/request sizes, parser depth, operation counts, concurrency,
queues, caches and retained generations. Streaming bodies and disk-backed
indexes must avoid retaining the mailbox or full messages in RAM. Check
lengths and capacity before growth; use fallible reservation where available
and defined refusal at enforced limits. Durability, authorization and bounded
work remain correctness requirements regardless of memory performance.

The initial performance goals are RSS below 64 MiB idle and below 128 MiB
under the section 15 workload. They are provisional tuning goals, not a
byte-exact architectural budget or a prerequisite for implementing adapters.
Measure the integrated service before choosing a release memory claim or
changing these goals. Record RSS and cgroup memory separately, including
page-cache effects, workload, concurrency and sampling limitations. A ledger
sum or isolated allocation probe establishes neither result.

Prefer reusable buffers and borrowed views where they simplify ownership and
streaming. Bounded per-request allocations are permitted. There is no global
zero-allocation contract and no requirement to allocate and touch every
application arena before listening. Existing APIs with caller-owned fixed
buffers keep their specific capacity/ownership contracts until explicitly
refactored; their allocation tests are local regressions, not a template
required for every new adapter. Do not add arenas, replay passes or ownership
facades solely to satisfy speculative byte accounting. This does not remove
capacity admission needed for correctness: quota/effect reservations, complete
response capacity before mutation, TLS handoff buffers and completion credits
remain governed by their specific contracts.

Use bounded worker concurrency and queues; do not spawn an unbounded thread
per connection/request. Buffer capacities, worker roles/counts, stack sizes,
cache sizes and scheduling quanta are implementation choices to validate
against progress, fairness and measured workloads. RESOURCES.md records the
current proposal and implemented limits. Its byte partitions and TLS
allowances are planning estimates, not mandatory per-component ceilings.
Limits::plan retains its process planning validation. PostgreSQL owns server
heap, WAL and page resources; the process ledger does not bound that server. Maintenance must share bounded capacity with
live work and preserve foreground progress.

Event-source streams hold connection slots but no storage read view between
events. RESOURCES.md proposes nonblocking main-thread scheduling and eight
workers across six roles; event peers cannot occupy request/commit/control
workers. Use safe std with a bounded round-robin socket scan and a wait
based on deadlines, using the current proposed five-millisecond quantum.
Established TLS record work is bounded per slot; disk, dial, handshake and DNS jobs use
fixed workers. Safe std exposes no poll/epoll; adding an OS readiness
adapter would require the explicit unsafe review from section 3 and a design
amendment. Read slots are acquired per store operation with a bounded fair
wait queue; exhaustion returns a retryable HTTP 503 before response headers
or the mapped JMAP method error. A streaming response that has begun cannot
fabricate an error object inside its body: finish from its admitted view or
terminate it. An online backup uses one read slot; interactive work shares
the other. This default reserves foreground capacity during background work.

[ADMISSION.md](ADMISSION.md) fixes disk quotas, checkpoint completion space,
maintenance drain/exclusive budgets, network timers and exact JMAP response
retention. These are admission contracts, not measured throughput or guarantees
against a host disk filling concurrently. They gate persistence and protocol
implementations; they do not impose the speculative RAM ledger.

Initial default ceilings (validated together at startup):

| Resource | Default limit |
| --- | --- |
| Simultaneous inbound SMTP sessions | 8, at most 2 per peer address |
| Active HTTPS connections | 8, with a separate cap of 2 TLS handshakes |
| Event-source connections within the HTTPS pool | 2 |
| Concurrent smart-host deliveries | 1 |
| Concurrent body/search jobs | 2 |
| Incoming message or uploaded blob | 32 MiB hard maximum, streamed |
| Owner-imported legacy message and stored-body reads | 64 MiB hard maximum |
| Aggregate header bytes / MIME nesting / MIME parts | 256 KiB / 32 / 1024 |
| SMTP recipients per transaction | 100 |
| JMAP JSON request / methods per request | 1 MiB / 16 |
| JMAP object IDs per get/set / query page | 256 / 256 |
| JSON nesting / parser tokens per request | 32 / 32768 |
| Combined resident index cache | 4 MiB |
| Storage read views | 2, each with bounded row/key/cursor scratch and a database snapshot |
| Bodies retained by upload leases | 128 MiB per account, expiry after 24 hours |
| Retained submission storage | 256 MiB and 1000 submissions |
| Maintenance sort scratch on disk | 64 MiB per account |
| Active log plus retained generations | 8 MiB each, 4 retained |

Values are operator-adjustable within implemented compiled maxima. SMTP
command/path/line limits also obey protocol minimums; the table does not
replace RFC limits. Advertise only limits the service enforces. Changes to
concurrency or cache settings require representative measurements before
retaining a published memory claim.

TLS measurements include handshakes, decoded peer material, session caches
and old/new certificate overlap. Bounded wire input alone does not bound
provider temporaries; application counters alone do not bound the process.
Keep provider input/concurrency restrictions and disable unneeded resumption
caches and 0-RTT. Measure representative and adversarial cases when integrating
or changing those paths, then resolve concrete excessive growth.

Qualification has a stopping rule: cover each changed correctness boundary,
enforced exhaustion/refusal and a representative lifecycle. Add a resource
scenario for a new production behavior, a reproduced regression, a changed
enforced limit or a named integration/release question. Do not enumerate
every combination of account count, body size, backup epoch, live reader,
history pruning and fence ownership merely because it can be tested. Existing
fixture measurements remain evidence for their exact inputs; their exclusions
are limits on claims, not an instruction to implement every excluded case.
Whole-service memory/fairness evaluation belongs to M23 after functional
integration, with targeted earlier measurement where a concrete risk warrants
it. A measured leak, attacker-controlled unbounded growth or broken refusal
must be fixed when found; deferring performance tuning never excuses those.

All network operations have idle and total deadlines; slow progress must not
reset the total deadline forever. Bound keepalive requests and fairness between
SMTP and JMAP so unauthenticated connections cannot starve an authenticated
mail reader or health checks. Resolve configured outbound hostnames using a
bounded std-only DNS adapter with explicit UDP/TCP deadlines and A/AAAA/CNAME
limits; do not hide uninterruptible resolver work in unbounded threads.
The PostgreSQL bootstrap/writer endpoint is a separate cold-path exception:
POSTGRES.md section 10 uses a disposable, deadline-owned system-resolver
process, with a five-second cap and fresh lookup on each call. It does not
implement the mail-routing Resolver port or supply a TTL; trusted platform
resolver configuration and cache policy apply within that child.
Never perform recipient-MX lookups for outbound mail. Saturation produces SMTP
temporary failure, HTTP/JMAP limit responses, or bounded retry, as appropriate.

The prepared `dns_wire` layer emits only one recursive IN A or AAAA question
for an already configured hostname. Its fixed name representation retains
label boundaries and folds only ASCII letters. Reply-name decoding accepts
at most 255 expanded bytes, 63 bytes per label and 32 backward compression
pointers in a packet of at most 65535 bytes. It reports the original field
extent separately from referenced suffixes. This is structural parsing, not
proof that a pointer targets a previously parsed name field. Network peer,
transaction/question matching, answer and CNAME limits, TTL handling, cache
ownership and UDP/TCP deadlines remain the resolver owner's responsibility;
the wire helper performs no lookup and supplies no authenticated identity.

The prepared `dns_reply` decoder matches the response bit, standard opcode,
reserved Z bit, transaction ID and complete single IN A/AAAA question before
accepting even a truncated-response indication. A nontruncated packet must
frame exactly, with at most 256 records across all sections. Known IN address
and CNAME records are structurally checked even outside the answer section;
unknown records remain opaque. An unsolicited OPT record is refused because
the corresponding query advertises no EDNS, preventing an extended error code
from being silently ignored. AD/CD bits grant no DNS authentication claim.

Only answer-section addresses reachable from the question through at most
eight CNAME hops contribute a result. Each call accepts the original chain's
remaining allowance, never more than eight, and enforces it through terminal
address results. Subtract the consumed hops before following an incomplete
chain; a zero allowance still admits a direct address answer. At most 16
distinct addresses survive;
duplicates still lower the minimum chain/address TTL. A TTL with its high bit
set becomes zero. Conflicting CNAME targets, CNAME/address coexistence and
alias cycles refuse. An incomplete chain returns its last name, consumed hops
and minimum TTL for the resolver owner to continue under the original global
hop/deadline budget. Alias and terminal address results also retain the
complete bounded local path, including the initial question name, so the
owner can reject cross-response cycles through any intermediate name. Truncation, header error codes and no-data remain passive
lookup results, never permanent recipient failures. The owner must match the
network peer, retain the original configured TLS identity and treat TTL zero
as usable only for the current lookup, without caching it.

The prepared `dns_transport` exchange runs synchronously on the fixed
resolver worker against a caller-admitted numeric server. A connected UDP
socket filters the peer; at most 16 datagrams are considered. Queries omit
EDNS, so a receive cell of 513 bytes detects and refuses oversized UDP
responses. Only a response matching the full question may request TCP
fallback. Drop UDP before opening one TCP connection to that same endpoint;
write the length-prefixed query and read exactly one length-prefixed response,
refusing early EOF, invalid framing and another truncation indication.

Every operation shares the original runtime clock, job deadline, work meter
and remaining CNAME allowance. Blocking socket waits use the earlier job or
meter deadline with a one-second idle cap; partial progress never renews the
total. Check time before and after I/O and parsing. Standard socket calls
retain the host-kernel failure boundary, rather than promising recovery from
an arbitrarily stalled kernel. The caller supplies an unpredictable query ID.
This exchange does not yet activate the Resolver port, cache, configured-name
selection or worker admission; those owners must preserve the original TLS
identity, chain TTL and alias allowance across queries.

The prepared `dns_lookup` owner combines A and AAAA for one caller-admitted
configured host using one to four caller-admitted numeric resolvers. Each
question tries servers in configuration order with a fresh entropy-sourced
transaction ID. Transport or server errors may try the next server under the
same total deadline and meter; an expired total, exhausted capacity or entropy
failure stops. NXDOMAIN and no-data terminate that family's search, without
inventing a negative-cache TTL. An all-empty result refuses as DNS failure,
not a permanent recipient outcome.

Each family starts at the original configured name with its own supplied
allowance of at most eight CNAME hops. Incomplete chains consume that
allowance across responses and retain every traversed name, including
intermediate names hidden within a response. Join each decoded path at the
previous response's final name and reject any other repeated name, including
inside terminal address responses. Terminal decoding uses only the remainder. The two families share the original job/meter and
at most 16 combined addresses. No presentation conversion or configured-host
revalidation changes decoded alias octets: a bounded Name can directly form
the next Question. The original configured host remains the TLS identity.

Convert each accepted response TTL to an absolute monotonic expiry before
issuing another query, and retain the earliest alias/address expiry. Never
restart an earlier TTL at final completion. TTL zero and an empty family make
the combined result current-lookup-only: its valid_until may already be at or
before completion, and cannot authorize a later cache hit. An empty family
has no decoded negative TTL, so caching that absence alongside positive data
would be unjustified. Stage addresses privately and publish the caller prefix
only after both families complete and the original budget still permits it;
errors leave caller addresses unchanged. Configured-name admission, worker
scheduling and cache integration remain separate owners.

The prepared `dns_owner::Configured` implements the Resolver port for one
exclusively owned fixed worker. Construct it from the active configuration's
outbound and certificate views, a nonzero configuration generation, the shared
runtime clock, worker-local entropy and the single reserved packet cell. It
admits only the configured relay hostname and optional ACME directory hostname,
case-insensitively. No raw-host registration API admits incoming names, alias
targets or arbitrary application destinations. The service owner has two fixed
endpoint slots; offline migration's separately authorized temporary destination
is outside this service owner. Constructing the port does not start a worker
or activate outbound delivery.

The cache key is endpoint slot plus configuration generation. Entries retain
only final addresses and absolute expiry, never alias paths, and a hit requires
strictly future expiry. Unknown names, expired caller deadlines and insufficient
output capacity refuse before network work. Expired entries are retired before
refresh; failures never serve stale data. Every miss creates one bounded work
meter under the caller's original deadline and uses the configured resolver
order. Zero-TTL and missing-family results are returned for the current call
but never inserted. A cache hit that expires before final publication refuses.
The caller output prefix changes only after the final clock check; errors
preserve it. Configuration borrows, resolver order and generation stay fixed
for the owner's lifetime, consistent with restart-only resolver changes.

The prepared `outbound_dial` helper runs only on the admitted outbound
handshake worker while its caller retains the sole outbound slot and configured
endpoint/TLS policy. It tries at most 16 numeric addresses in order, with no
ambient name lookup. Each attempt uses the earlier original job/meter deadline
and a two-second connect cap; fallback never renews either budget. Prepay dial
and socket-setup work before opening a socket, and check time before and after
connect and conversion to the exclusively owned nonblocking TcpTransport.
Failure or late completion drops the socket. No per-peer thread or retained
failed connection is created. Standard blocking connect retains the documented
host-kernel failure boundary.

TCP success grants no TLS identity, credential permission or queue authority.
The caller retains the configured hostname/policy for the existing TLS handoff,
which must verify it before SMTP authentication. DNS expiry controls cache
reuse, not the identity of a connection made from the current lookup. Dialing
itself does not activate delivery or establish a fresh attempt fence.

No design can promise survival of arbitrary dependency panic, kernel OOM kill,
or storage failure. Do not use catch_unwind as storage recovery. An unexpected
worker failure makes health fail and the service stop accepting new mutations;
restart recovers committed state. A supervisor restarts the process. Release
tests cover both orderly shutdown and abrupt termination.

## 6. Configuration and local administration

Use a documented small line/stanza configuration grammar with versioned schema,
quoted strings and integer/boolean values. It resembles td's existing service
files but is not advertised as general TOML. Reject duplicate/unknown keys,
invalid UTF-8, embedded NUL, conflicting aliases, dangling account references,
and incompatible listener policies. No includes, environment expansion, shell
evaluation, arbitrary hooks, or network-loaded configuration.
[CONFIG.md](CONFIG.md) specifies the implemented bounded syntax and resource
stanzas, typed local routing, reader completion and identity preimage helpers.
[SCHEMA.md](SCHEMA.md) specifies the remaining complete operator schema and
snapshot partitions. Its typed loader, complete candidate validation and
command wiring remain planned.

Separate operator configuration (`/etc/td-mta/`) from service-managed data
(`/var/lib/td-mta/`), runtime control (`/run/td-mta/`), and logs
(`/var/log/td-mta/`). All paths can be configured absolutely for other distros.
Secret values come from protected files, not command-line arguments or logs.
Check file type, ownership, permissions, and trusted ancestors before use;
the service does not support a data root with an untrusted concurrent writer.
Create secret/mail files as 0600 and private directories as 0700, without a
permissive creation window. Never derive a filesystem pathname from a mailbox
name, address, attachment filename, or arbitrary client ID.
STORAGE.md defines the std path checks, deployment identity and stable-path
assumptions. Local locks protect runtime and response scratch only. Database
transactions own durable publication. The service
uses logical quotas and handles disk-full/write/sync failures; it does not
measure or promise physical free space before admission.

Current commands use stable JSON output and exit codes:

| Command | Contract |
| --- | --- |
| `postgres config-import`, `postgres config-check`, `postgres config-select` | Validate, store and select database configuration |
| `serve --connection-file PATH --profile NAME --runtime PATH --scratch PATH` | Foreground PostgreSQL receive/JMAP service |
| `status --json --runtime PATH` | Selected service lifecycle; `--require-ready` makes its exit code a readiness probe |
| `stop --runtime PATH` | Request orderly shutdown through the private control socket |
| `postgres device-create`, `postgres device-revoke`, `postgres device-list` | Database credential administration |
| `postgres schema-init`, `postgres schema-inspect`, `postgres schema-upgrade` | Explicit database schema administration |
| `postgres account-init`, `postgres account-inspect` | Account initialization and inspection |

The owner-side `postgres account-create`, `account-update`, `account-alias-add`
and `account-alias-remove` commands edit the selected immutable source set,
validate every profile and retain an operation file before database writes.
Creation provisions the Inbox, receiving address and one sending identity;
alias edits change receiving only. The first account remains the implicit
postmaster when a second is added. `account-apply` resumes the same operation;
`account-operation-inspect` resolves its historical selection receipt.
Concurrent edits conflict rather than overwriting a newer generation.
`account-list` reports selected account, alias and identity configuration.
`account-identity-add`, `account-identity-update` and `account-identity-remove`
use the same retained workflow for account-owned sending identities. Updates
preserve omitted fields and identity IDs; removal also deletes its address-list
entries, but never receiving aliases or already accepted outgoing messages.
None of these commands starts or restarts a service or provisions a device
password. POSTGRES.md owns their retention and partial-completion contract.

POSTGRES.md specifies arguments and the remaining database commands. The
private Unix control socket carries status and stop only; database changes
use authenticated PostgreSQL connections. Its directory permissions establish
local control authority. There is no public administration HTTP API.
Physical backup and recovery use PostgreSQL tooling. Readiness does not
contact Migadu or a CA. Configuration errors do not echo supplied secrets.

Listener address changes, data-root changes, and pool sizing require restart.
Account, alias and sending identity configuration changes require restart. Device
revocations are observed from PostgreSQL; managed certificates trigger bounded
drain and reload. Revocation
is rechecked before a mutation commits; a connection is not permanent authority.
Reload has bounded coexistence memory and records the effective generation.

## 7. Identity, passwords, and TLS lifecycle

Generate application passwords with at least 256 bits of provider entropy,
display once to the local administrator, and store only a versioned salted
verifier. Use the provider for digests and constant-time verification.
High-entropy generated tokens do not need a human-password derivation scheme;
v1 does not accept operator-chosen weak passwords. A device record binds an
account, label, stable ID, creation/revocation state, and verifier. Labels are
untrusted display strings. Bound authentication work globally and by peer.

Device creation, listing and revocation use the PostgreSQL commands in
POSTGRES.md. Every serving instance observes the same database registry;
no local credential files or control-socket credential commands are active.

Accept HTTP Basic authentication only over verified server HTTPS from clients;
td-mta presents the certificate and the client verifies it. Credentials do not
grant local OS login or td elevation. Service-owned ACME keys and smart-host
secrets are unattended server credentials, distinct from td's human-session
credential portal. A later td image integration must specify provisioning
under that platform's credential policy rather than a root shell.

ACME runs inside the executable using HTTPS, JWS, and the crypto provider.
Start with HTTP-01 on port 80; DNS-01 and TLS-ALPN-01 are deferred. The operator
controls the deployment host's ports and DNS; this development host is not
configured as the mail server. Certificate identifiers include the MX/JMAP
hostname, configured gateway listener names selected for ACME, and enabled
MTA-STS policy hostnames, not automatically every alias. ACME-managed gateway
names require reachable HTTP-01 validation; private names use provisioned
certificates rather than starting an order that cannot be validated.

Persist the ACME account key and resumable order state privately. Bound
directory/nonces/order responses and certificate chain sizes. Validate nonce,
URL, challenge, CSR, key, identifier set, and returned chain. JWS/CSR encoding
is bounded std code; signing/key generation is provider code. CA endpoints
must be HTTPS; follow only validated directory/order endpoints, with no
credential-bearing redirect to arbitrary hosts or protocol downgrade.

Schedule renewal early based on actual certificate lifetime, with jitter and
bounded exponential retry; do not hardcode a 90-day lifetime. Honor CA retry
instructions within explicit limits. Test clock jumps and suspend/resume.
Write a new certificate generation, sync, validate key/chain/names, then
atomically select it. Existing sessions finish on their bounded old generation.
A failed renewal retains the old certificate and exposes a health error.
Never silently replace an expired/missing public certificate with self-signed
TLS. During initial issuance, serve ACME and local diagnostics; do not expose
JMAP or accept new mail until a usable certificate exists. After expiry, refuse
new TLS service and temporarily refuse inbound mail rather than quietly hiding
loss of the deployment's advertised transport security.

Use bundled reviewed public roots for outbound TLS, with an explicit protected
CA-file override for local tests/private gateways. A root-data update is a
reviewed artifact update. A test trust override must not disable verification.

## 8. Durable storage and crash consistency

[POSTGRES.md](POSTGRES.md) owns schema, transactions, snapshot custody,
commit uncertainty, recovery and database administration. Immutable bodies and
metadata are published together. Full length and digest verification grants
body custody only within the original live snapshot. Passive historical audit
reports grant neither current freshness nor body-read authority.

[STORAGE.md](STORAGE.md) owns process-local locks and disposable response
files. No local mail database, body directory, backup command or migration
interface remains. Operators use PostgreSQL backup and restore facilities;
restore/epoch and uncertain external delivery requirements remain governed by
POSTGRES.md. Tests exercise real database and process failures, but do not
claim full power-loss, managed-provider or whole-service resource qualification.

## 9. SMTP receiving and message representation

The current `smtp_session` engine consumes one command or DATA line per
turn. Pending replies require an explicit flush acknowledgement; reservation,
body writes and publication require separate trusted driver results before
input resumes. A final DATA 250 follows only a successful commit result for
the routed account. An indeterminate result closes without a final reply.
A supplied result alone is not durability proof. PostgreSQL receiving
stages bounded ingress, replaces Return-Path, generates
Received from trusted connection facts, resolves immutable threads and
publishes one Inbox email with its body, envelope and history in one
transaction. A delivery job owns its connection authorization adapter,
so a worker can construct it and transfer the job without borrowing that
worker's stack. Borrowed adapters remain supported through the same
interface. Publication still acquires a fresh policy guard; retaining
the adapter does not retain permission. Tests between workers verify
durable publication and refusal after revocation. Transcript fixtures
connect actual outcomes to the session and verify reopened mail. The
receiving network driver additionally exercises real local TCP delivery
and reopened storage. The direct receiving runtime described below
connects these adapters with fixed workers on caller-bound listeners.
The `serve` command activates direct receiving for receive profiles. The engine binds
routing for its lifetime, accepts only configured recipients, retains accepted
envelope spellings with resolved account IDs, and publishes every distinct local
recipient account in one PostgreSQL transaction. It
advertises SIZE, 8BITMIME and
enhanced status codes; optional STARTTLS hands the original command to
the transport owner without generating 220. Only successful TLS
completion resets SMTP state. No PIPELINING or other unimplemented
extension is advertised.
The advertised incoming SIZE excludes the delivery adapter's declared trace
allowance; admission reserves the stored-message ceiling including that
allowance. The native adapter derives the allowance from its trace format and protocol
bounds; its generated fields fit within it. Root headers are bounded both
before filtering and after adding local trace fields. Header overflow yields
552 5.3.4; work or storage refusal remains temporary. The shared raw scanner
preserves malformed body boundaries, and MessageIds source extents retain
only the bounded policy lookback.
Framing, line-size, message-size and streaming-write failures during
DATA close the connection, so the remaining stream cannot become
commands. The runtime must flush a close reply and use bounded transport
draining so unread DATA is discarded before closing where the shutdown
bounds permit; drained bytes never reenter the parser. The session does
not implement this shutdown.
The `smtp_network` owner performs one bounded transport or parser turn
per call. It distinguishes progress from a socket wait, retains unread
input across worker handoffs, tracks partial reply writes, and advances
the session only after flush completes. Typed worker results make one
transition without exposing a mutable session. Storage and TLS handoffs
yield without further socket work. Client idle timing pauses during
dispatched work and restarts on completion; the worker still owes its
original DATA/session deadline and its own execution budget.
The native delivery preparation call requires its trusted caller to
supply the finalization deadline captured when that work is queued. The
direct receiving runtime captures the configured commit budget at that
dispatch and takes the minimum with the enclosing DATA/session deadline.
The call narrows the job's
enforcement deadline once, covering header completion and publication;
coordination retries retain that cap and refuse once it expires even
while the lock is busy. The ledger reservation, work meter and spool
retain their original receiving bounds. It checks again after spool
preparation; only the native transaction can accept the message. A known
successful commit remains successful when completion is observed after
that deadline.

The `smtp_receiving` runtime accepts caller-bound IPv4 direct listeners
and retains one immutable routing/TLS configuration snapshot. It verifies
the actual local socket address and complete TLS listener binding,
including listener limits, greeting/trace compatibility and ingress spool
capacity, and checks the account's single Inbox before readiness. It does
not serve the configuration's HTTPS listener, enable gateway receiving
or implement hot reload. A connection-specific direct
adapter binds the real socket peer and actual negotiated TLS version;
plaintext and TLS direct SMTP grant no remote identity authentication.
The runtime checks each bound direct certificate chain's admitted UTC
validity window before readiness, on every scan and before new-session
admission. Failure stops new admissions and fails health; workers abandon
queued work without a final reply. Committed mail remains durable and
existing final replies retain their bounded allowance. The runtime reports
a TLS failure. This gate also applies to
plaintext SMTP, which would otherwise never construct a TLS session.

One network loop visits bounded connection slots in rotating order. One
fixed storage worker performs admission, streaming writes and publication;
one fixed TLS worker constructs sessions and advances handshakes. Idle
connections and pending handshakes retain slots but do not occupy a worker.
Each slot retains its own queued operation and completion, so dispatch
reserves result capacity before effects without a separate completion
queue. Main uses try_lock and skips slots owned by workers. Known native
results are applied to the retained protocol owner before releasing the
slot, including when ancillary accounting stops further admission.

Global, per-listener and per-peer counts include queued, handshaking,
closing and retiring connections. Saturation attempts one bounded 421
write before closure. Runtime status distinguishes startup, ready,
stopping, stopped and failed. Shutdown stops accepting and permits up to
five seconds of existing connection progress before retirement; a known
final DATA reply retains its existing bounded finishing allowance. It never
accepts incomplete DATA. Native cleanup runs on fixed workers during
normal operation. An unexpected worker failure stops new mutations and
fails status; terminal teardown after joining workers may block and never
resumes a poisoned transaction. No thread replacement or kernel-I/O
cancellation is promised. RESOURCES records the concrete slot ownership.

Accepted MAIL commands count toward the fixed 100-transaction limit even
after RSET, EHLO or STARTTLS. Expiry at a legal command boundary queues
421; a partial command or incomplete DATA aborts. Known final DATA
results get up to five seconds to finish, capped at five seconds after
the original one-hour session deadline, even when completion arrives
after the old DATA or session work deadline. This grants no new mutation
lease and does not reclassify a committed result as a rejection. If the
session expired, flush the final result followed by 421, then shut down
and drain under that same allowance. Plaintext and TLS share this fixed
transport cap, established at connection creation; late completion
cannot renew it. A result arriving at or after the cap cannot be
acknowledged, but remains committed. A stalled reply or failed
clock/transport can likewise lose acknowledgement.

Worker refusals that close the session and expiry notices also share a
five-second reply/shutdown/drain allowance, within the fixed transport
cap. Other close replies use their ordinary reply deadlines, then five
seconds for write shutdown and discard. Draining stops after 64 KiB of
additional reads or EOF; buffered tails are discarded immediately. This
is best effort: a bounded close can still reset with unread peer input
and cannot guarantee receipt of a refusal. Drained bytes never become
commands. Storage job disposal remains on a worker. These bounded turns
do not establish runtime queue admission or fairness.

A service-unavailable operation queues 421 at an empty command boundary;
for gateway revocation the driver flushes DATA's 451, then queues 421 before
feeding more input. STARTTLS's exact original command can be fed through the
existing control LineReader for ServerStartTls; the combined command/DATA
framer remains private to this engine.

The `smtp_starttls` upgrade consumes the network owner and its same TCP
socket on a TLS worker. It accepts only an already-dispatched STARTTLS
handoff with no buffered plaintext, passes the exact received command to
ServerStartTls, and owns both reply flushing and the actual handshake.
Native construction and handshake capacity admission precede its 220.
The caller supplies the original handshake deadline, including queue and
preparation time, within the unchanged session work deadline. The TLS
transport receives the separate fixed finishing cap. Bounded turns
preserve both deadlines and detect clock regression. Only a successful
TLS handshake authorized by its retained policy returns the session and
its TLS transport for further SMTP. Failure or cancellation closes the
connection and releases native state and handshake capacity on the
worker; there is no plaintext fallback. Borrowed wire buffers are
released and owned buffers are freed on refusal. A local real-TLS
fixture exercises post-handshake EHLO reset and encrypted commands;
the fixed receiving workers and `serve --smtp-only` now activate that path.

Implement a bounded SMTP state machine with EHLO/HELO, MAIL, RCPT, DATA, RSET,
NOOP, QUIT, SIZE, 8BITMIME, STARTTLS, and enhanced status codes. Advertise
PIPELINING only after its ordered parsing and error paths are tested.
Do not advertise SMTPUTF8, CHUNKING, BINARYMIME, DSN, or AUTH in v1 reception.
Local configured addresses use ASCII domains/local-parts; domains compare
case-insensitively, while ordinary local-parts match aliases case-sensitively.
Reserve case-insensitive `postmaster` at every served domain and the SMTP
domainless Postmaster form, routing both to the configured postmaster account.
Configuration defaults that account only for a singleton account registry;
multiple accounts require an explicit declared `server.postmaster_account`.
Configuration cannot disable the route or redirect it outside that account. Support VRFY with the standard non-enumerating 252 response.
Unicode message display names and MIME content remain supported.

Reject unknown/nonlocal recipients at RCPT. Resolve accepted aliases to stable
account IDs and deduplicate delivery to the same account within one transaction;
retain the accepted envelope recipients for inspection. Delivery decisions are
pinned for that transaction rather than changing halfway through a reload.
Gateway admission revocation is a separate commit-time fence: SCHEMA.md defines
which trust/pin/prefix changes temporarily refuse an uncommitted transaction
and require reconnecting. Recipient routing remains pinned; it cannot extend
revoked gateway authority.
Every v1 inbound delivery files once in the account's Inbox, including when
several accepted aliases match. Aliases select accounts, not folders; alias
names remain envelope metadata. Provision exactly one Inbox before receipt
and forbid its deletion or role reassignment while receiving is enabled.
Final DATA success requires one PostgreSQL transaction covering every distinct
accepted recipient account. Account-scoped receiving receipts share an operation
and a digest of the complete target set and private provenance; inspection must
find all matching receipts or none. Independent partial writes cannot masquerade
as success. Each account's stored receipt contains only its accepted envelope
recipients, preserving recipient privacy across accounts.

Parse command/data boundaries across arbitrary network chunks. Strict CRLF,
dot-stuffing and termination rules prevent SMTP smuggling. Never interpret DATA
bytes as commands after a framing/size failure; drain only within a bounded
policy or close. Preserve original message content apart from SMTP transport
decoding and required locally generated trace fields. Do not rewrite content
just because MIME parsing is incomplete or a character set is unsupported.

Store envelope information separately from message headers. Add a truthful
Received trace and handle Return-Path according to final-delivery semantics;
do not trust an incoming Return-Path as the SMTP reverse path. Accept null
reverse paths for delivery-status messages. Permanent recipient refusals occur
before acceptance; temporary resource/storage trouble returns 4xx. After final
acceptance, ordinary local delivery must not depend on a later fallible task.

MIME scanning is incremental, with bounded header unfolding, part descriptors,
nesting, transfer decoding and work. Support multipart alternatives/mixed,
base64, quoted-printable, encoded header words, common address/date forms,
UTF-8/ASCII and a documented initial legacy-charset set. Unrenderable text is
reported through JMAP's encoding/problem semantics and raw bytes remain
downloadable; raw storage does not depend on rendering success. Pathological
messages hitting processing limits expose a typed error without killing the
service or hiding an accepted raw message. POLICY.md defines decoding,
charsets, opaque-message access and property behavior; CASES.md names their
independent fixtures. Their implementation still gates the Mail capability.

The first transfer-decoding primitive implements the frozen base64 octets
using fixed inline state and caller buffers. It charges source/output work,
yields after bounded transitions and preserves malformed-input diagnostics.
It supplies no MIME structure, nested-source ownership, charset conversion
or protocol projection yet.

A bounded transfer-input owner now connects identity/base64 decoding to a
borrowed immutable body reader and a checked encoded extent. It reuses one
source partition and brackets refill/decode turns with clock and work
checks. It retains the source pin and supports QP rewind and replay.
Optional exclusive stage checkpoint storage supports charged save/restore of
the same source, without copying ring bytes or resetting the clock/work
state. Part authorization, nested source ownership and MIME structure remain
separate.

The raw header scanner emits bounded name/value source extents, preserves
folded bytes, and identifies the body boundary under the file parsing policy.
It enforces the supplied header allowance and charges bounded work without
allocating per-field strings. Source retention, aggregate MIME admission and
header normalization remain with later integration.

The bounded charset decoder covers the policy's four charset families with
fixed state across source fragments. It preserves valid scalars and reports
replacement diagnostics. A bounded body prescan supplies the frozen
absent/ASCII-label UTF-8 heuristic and unknown-label fallback; the owner
replays the same immutable transfer-decoded source for final decoding. Header
filtering, NFC and JSON projection remain separate.

A fixed-state unfolding primitive removes accepted line endings only before
space or tab, retaining the whitespace and every nonfold octet. It supports
fragmentation and output backpressure within the 32 KiB conversion region;
form-specific text processing and protocol integration remain separate.

The resident Raw header cursor composes UTF-8 replacement with NUL removal
and I-JSON noncharacter replacement, preserving folds and original text.
It streams scalars from the existing header arena, with charged work and no
owned strings. Header selection, source collection and JSON/JMAP output
remain separate.

The transfer-to-charset owner retains one authorized immutable body borrow
through prescan, checkpoint rewind and final scalar decoding. Both passes
share charged work and a monotonic clock watermark, preserving diagnostics
and retirement. It stages one byte within fixed state; body-value projection
and protocol output remain separate.

A fixed quoted-printable cursor now implements the stable transfer policy
with explicit source-position rewind requests. It scans and replays long
whitespace runs using fixed state, with charged lookahead and bounded turns.
The transfer reader binds those requests to its immutable raw extent, reuses
resident input or refills from the exact run start. Caller checkpoints
retain QP scan/replay state, and charset prescan uses the same owned source.
Nested decoded-source checkpoint integration remains separate.

The plain body-value filter converts CRLF, replaces noncharacters for I-JSON
and applies a UTF-8 scalar-boundary byte cap with fixed pending state. It keeps
validating the remaining scalars after truncation so trailing diagnostics are
retained. HTML truncation, response serialization and JMAP integration remain
separate.

JMAP preview, body-value serialization and text search may group at most 64
existing scalar decoder turns between native snapshot checks. Every child turn still
uses its original work meter and freshly sampled job clock; no deadline or
budget is renewed. A fresh native check brackets each batch, including
failure and completion, and all copied bytes remain provisional. Metadata
preparation and the single-turn cursor interfaces retain their prior bounds.
This avoids polling the PostgreSQL connection for every decoded scalar.

## 10. HTTP and JMAP

The `http_request` module implements transport-independent HTTP/1.1 request
framing. Its head storage may be borrowed or owned; the HTTPS runtime uses
owned bounded buffers without self-referential state. A caller-owned head
buffer admits at most 32 KiB, 8192 bytes per line including CRLF, and 100
fields. Parsing requires strict CRLF, one Host, origin-form or absolute-form
targets and unambiguous framing. Duplicate security/framing fields,
obs-fold, Content-Length with Transfer-Encoding, request content coding,
upgrades and declared trailers refuse. Only chunked transfer coding is
supported. Host is passive syntax, never configured-origin authority.
Authorization, optional Origin and Last-Event-ID are borrowed without Debug
output; duplicate security/event fields refuse. Other unrecognized fields
are discarded; future endpoints needing additional fields must add explicit
handling. Retiring the reader clears its backing. This is ordinary buffer
hygiene, not guaranteed erasure of compiler or caller copies; a black_box
barrier discourages dead-store removal without adding unsafe code. The
caller must clear consumed TLS/plaintext input too and must not log or
retain credentials.

BodyReader streams length-delimited or chunked bytes into caller output,
with independent decoded and aggregate wire ceilings chosen before reading.
Chunk-size lines have a 1024-byte cap excluding CRLF, validated ignored
extensions and checked size arithmetic. The initial profile requires an
empty trailer section; nonempty trailers terminate the request. Each feed
consumes at most 16 KiB and leaves the next request's bytes untouched.
Backpressure does not consume body bytes without output space. Syntax,
limit and premature-EOF errors are sticky; provisional body bytes cannot
be committed after a framing error. No parser reset can revive failure.

The enclosing HTTPS driver must enforce TLS, configured authority, route and
method policy, cross-origin refusal, authentication, deadlines,
connection/request counts and scheduling. Routes match the raw target; only
validated endpoint components may be decoded, never used as arbitrary
filesystem paths. It must authorize and admit body storage before sending
100 Continue, check decoder completion before publication, and close on
framing failure. The current HTTPS runtime serves authenticated uploads, raw
downloads, the Core API and EventSource with these bounds. Authenticated
discovery advertises Core and the td-mail Mail profile. Device persistence
and local administration are implemented under section 7. Framing tests
cover split input, pipelined tails, binary bodies, output backpressure,
smuggling patterns and exact/refused limits without a network listener.

This profile accepts origin-form and absolute-form request targets; absolute
URI authority controls routing even when Host differs, as RFC 9112 section
3.2.2 requires. It refuses leading empty lines and asterisk-form targets,
non-ASCII field values and Connection options other than close/keep-alive.
No general-purpose proxy or complete HTTP/1.1 implementation is claimed.
Bracketed IPv6 Host values reject zone identifiers. Chunk extensions accept
grammar-defined whitespace around semicolons and equals signs, but refuse
standalone trailing whitespace before CRLF. Malformed request-line syntax is
distinct from a well-formed unsupported HTTP version. Browser compatibility
and Mail capability conformance remain unclaimed. These fixed parser caps
are independent of the proposed slot partitions in RESOURCES and ADMISSION.
The current HTTPS runtime uses the configured connection count and shared
handshake cap; RESOURCES.md records its concrete buffers. Whole-service
memory measurement remains section-5/M23 work; the existing Limits::plan
arithmetic is unchanged.

`serve --mailbox-access` runs the existing receiver plus the IPv4 `https`
driver. IPv6 listeners remain refused pending the audited socket adapter.
A fixed HTTPS network thread scans bounded slots, with a separate fixed TLS
worker. Native session construction and pending handshakes stay on that
worker; idle sockets retain connection slots without taking handshake permits.
The worker peeks nonblocking for initial TLS bytes before reserving a shared
permit, and retries a busy pool within the original handshake deadline.
All socket operations
are nonblocking. Request-head parsing uses at most 16 KiB per turn. The current
profile permits one request per connection and sends Connection: close with
all responses. Authenticated upload POSTs use the shared bounded body worker
specified in ADMISSION.md, with quota admission before 100 Continue and
commit-time device authorization. Other refusals need not consume the body.
After responding it applies RESOURCES.md's bounded write-half closure.
Refusals may drain bounded unread data; successful uploads already consumed
their body and do not drain. Downloads also close without drain. Slots remain
active until retired.
Authenticated GET /.well-known/jmap returns Core and Mail Session
metadata; unknown targets get 404. Submission remains unadvertised.
Non-POST upload methods get 405 with Allow: POST. Missing, invalid and revoked
credentials receive the same 401 Basic challenge. No credential enters error
output. A failed local credential publication
marks the service failed and initiates orderly shutdown; restart must recover
a healthy registry before serving again.

TLS completes before HTTP parsing. The configured origin certificate's
window is checked before readiness and every network scan; its failure stops
the combined profile. Secondary HTTPS identities are checked by the provider
at handshake time and do not govern the origin's readiness. Host must match the configured JMAP
DNS origin and port (case-insensitive host, omitted port means 443). An Origin
header must match that HTTPS origin; no CORS permission is emitted. A bounded
strict Basic decoder retains the exact username and password in a bounded,
request-owned buffer. Authentication selects the account by case-sensitive
username and verifies its generated device token. The resulting account/device
principal selects the request backend; caller-supplied account IDs do not.
Session discovery exposes only that account. Each request observes current
revocation state.
Upload publication acquires a fresh commit-time authorization guard; future
mutations must do the same.

At most two active HTTPS connections per IP and the configured global count
are admitted. Authentication uses fixed one-second monotonic windows: 64
attempts globally and eight per IP, with a 64-entry peer table that never
evicts an unexpired entry. Invalid credential syntax consumes an attempt too;
wrong authority or cross-origin requests are refused before verification.
Exhaustion receives 429 with Retry-After: 1. This permits boundary bursts,
not a rolling-window rate guarantee. Header total/idle, TLS, response idle
and absolute transfer/session caps use the configured timer plan. Shutdown
stops HTTPS admission and cancels unfinished upload transfers. It joins any
publication already running; a durable commit can outlive its connection or
exchange deadline without an observed receipt. Nonmutating exchanges close
immediately, while SMTP retains its orderly final-reply behavior. Fixed
native/kernel work already running must return before joins complete.

Authenticated raw downloads now use GET on the fixed download template.
The body worker is shared with uploads and holds one transfer permit through
flushing. It verifies the blob before headers, then streams under the same
snapshot and exchange deadline; storage owns account/device access. Filename
and media template values are decoded once and cannot inject response
headers. Pre-output errors return an HTTP refusal; after output starts an
error retires the connection. ADMISSION.md owns exact bounds and status
mapping. Other methods return 405 with Allow: GET. Discovery is authenticated
and exposes the supported td-mail read profile.

Implement bounded HTTP/1.1 for discovery, authenticated method calls, uploads,
and downloads. Reject conflicting Content-Length/Transfer-Encoding, ambiguous
header syntax, oversized chunks, invalid request targets, and request smuggling
patterns. Bound headers independently of bodies. Stream chunked/content-length
uploads and downloads. No request compression, arbitrary proxy routing, or
cross-origin browser access by default. Protocol errors terminate the connection
when its framing cannot be trusted.

RFC 8620 and RFC 8621 define wire semantics. Maintain a capability/method/property
coverage table with normative section references and local acceptance fixtures.
Activation requires real td-mail workflow and refusal tests. This client
compatibility profile makes no full RFC conformance claim. Unsupported
methods/properties get the specified errors rather than fabricated empty success.
Publish truthful resource limits and read-only identity properties. Include
the standard session eventSourceUrl and a bounded authenticated event-source
endpoint, with coalesced account state, type filtering, ping and closeafter
behavior. Polling also works. External push-service registration is refused
by explicit policy using the standard method error; v1 never POSTs to a
client-supplied callback URL. The conformance table must pin that refusal
and the associated get/set behavior rather than silently omit Core methods.

The passive Mailbox projection reads typed rows and emits every standard
object property or a validated selection, with caller-supplied account rights
and same-view batch counts. API.md owns its snapshot, provisional-output and
fixed-buffer contract. The retained dispatcher implements Mailbox/get,
including result references, bounded enumeration and same-view counts. The
live HTTPS capability preflight admits Core and Mail for the supported
td-mail read flow qualified in CONFORMANCE.md.

The retained Mailbox/changes dispatcher validates scoped state and exact
UnsignedInt limits, consumes one retained change-page cursor and encodes
transaction-aligned changes under the original request work and deadline.
It keeps the read view through final publication and emits null
updatedProperties because history does not track per-field changes.
This method remains behind admitted Mail opt-in, including live requests.

The retained change-page cursor coalesces typed object history into bounded
transaction-aligned pages. API.md owns its token scope, limit, snapshot and
completion contract. Wire argument handling and changes-method dispatch are
separate from this passive cursor.

The passive thread member reader returns bounded received-date windows
from one retained snapshot without an SQL sort or account-sized collection.
API.md and STORAGE.md own the fixed-memory and repeated-scan tradeoff.
The storage reader itself performs no method dispatch; Mail advertisement
remains separate.

Retained Thread/get handles explicit IDs or bounded all-thread enumeration
and streams received-date member
windows from one original snapshot. Its argument, projection and rollback
contract is in API.md; CONFORMANCE.md qualifies its live td-mail read path.

Retained Thread/changes shares the checked changes encoder with Mailbox,
selecting Thread state and wire IDs and omitting Mailbox-only fields.
API.md owns this contract; the live read profile requires Mail opt-in.

The compatibility baseline includes the current td-mail JMAP implementation,
particularly its existing sending flow:

- Discovery, HTTP Basic, account/capability and URL templates.
- Mailbox get/set; Email query/get/set; Thread get; filtering, pagination,
  date ordering, text search, headers, body values and attachment download.
- Identity/get, attachment blob upload, structured Email/set creation.
- In one request, creation ID `#draft` used by EmailSubmission/set, followed by
  onSuccessUpdateEmail and its implicit Email/set response under the submission
  call ID. td-mail already checks this response for filing failures.
- EmailSubmission/query by identityIds/after/before and EmailSubmission/get,
  used to reconcile a lost sending response. Preserve message IDs and filing
  metadata used in the client's subsequent reconciliation.

The repository baseline was inspected at
`248054be6` (including td-mail sending commit `c2c343bf3`). Implementers recheck
current client calls before freezing the wire fixtures. Existing td-mail mock
tests remain unit/integration coverage, but v1 additionally runs the real client
against the real service through its actual td-fetch transport.

JMAP changes, state preconditions, partial set results, creation/result
references, and account isolation must be explicit. A multi-method request is
not one global transaction: earlier successful methods remain successful if
later methods fail. Thread assignment is deterministic and persisted using
Message-ID, References and In-Reply-To; duplicate or cyclic headers cannot
merge accounts or cause unbounded work. An existing Email's threadId is
immutable. New arrivals may join one existing thread, but never merge or
renumber existing Email threadIds. STORAGE section 3 owns authoritative lookup
and deterministic conflict rules; M02 freezes their byte encodings/fixtures.

Text search streams decoded searchable data with bounded working memory and
uses disk indexes for candidate selection where available. Queries have stable
ordering with an ID tie-break and a committed snapshot. Do not silently truncate
results or report an exact total before computing it. Exhausted work/time
budgets return the appropriate explicit error; attachment binary contents are
not decoded as documents. Index build, rebuild and query must obey section 5.

Email/query accepts the identity predicate `minSize: 0` for account-wide
migration inventory. Positive minimum sizes remain unsupported filters;
negative sizes are invalid arguments. This does not change query ordering,
account isolation, snapshot state or work limits.

The active PostgreSQL search walks date/mailbox-filtered candidates in windows
of 32 within one authenticated snapshot. One method work budget and deadline
cover all candidate verification, header decoding, MIME traversal and matching;
neither a page nor a message renews them. Only the result page and bounded
decoded headers are retained, with streamed text bodies and fixed HTML
extraction state. Matching uses at most 64 substring automata over a 4096-byte
operand, with field-local phrase progress and simple Unicode lowercase.
Identity-encoded message/rfc822 attachments use an iterative worklist of
separate message contexts, retaining one context's parsing buffers at a time.
The six-boundary nesting limit, aggregate part/header ceilings, original
work meter and header budget apply across the traversal. POLICY.md owns
supported sources and explicit refusal of other embedded-message encodings.
This is a functional bounded scan, not a full-text index or throughput claim.

## 11. Submission and retry semantics

[QUEUE.md](QUEUE.md) owns the exact transition, cancellation, retry and standard
JMAP projection contract; the following summarizes its service behavior.

Authorize identity, header From/Sender and envelope sender/recipients before
creating a submission. Bound recipients, sizes, header injection and recipient
expansion. Serialize structured JMAP messages into immutable MIME, including
attachments and reply references. Remove Bcc headers from the transmitted copy
while retaining Bcc envelope recipients and the user's sent-copy metadata.
Pin the transmitted blob and envelope at submission; later email edits or
deletion do not alter an accepted queued message.

Creating a JMAP submission means accepting responsibility into the local durable
queue, not proving remote delivery. Commit queue intent and the submission's
queryable record before acknowledging creation. Apply onSuccessUpdateEmail
according to the JMAP method's success semantics, and report its own result;
filing in Sent is not proof of smart-host or final-recipient delivery.
The queue retains its blob even if the client removes the corresponding email.
V1 does not offer FUTURERELEASE: sendAt is the submission creation time and
never moves with retries or eventual relay acceptance. Identity/time queries
therefore find the record in td-mail's original reconciliation interval even
during a prolonged relay outage.

Internal states are queued, in-flight, retry-wait, accepted-by-relay,
permanent-failure, canceled, and outcome-unknown. Persist per-recipient states,
attempt IDs, last bounded diagnostic, next-attempt time and expiry. Document
their mapping to standard JMAP undoStatus/deliveryStatus without adding made-up
standard fields or claiming accepted-by-relay means delivered-to-recipient.
Local CLI/logs may expose the richer internal state.

Before sending bytes, persist the attempt and recipient set. SMTP 4xx, connect
failure, and timeout schedule retry. SMTP 5xx marks the relevant recipients
permanently failed. If some RCPT commands succeed and others fail, DATA covers
only those with successful RCPT replies. A successful RCPT reply alone never
marks a message delivered or releases its queue reference: responsibility
transfers only after the relay's final positive reply following DATA. A failed
DATA leaves those recipients pending or failed according to that final reply.
Never resend recipients whose final DATA acceptance is durably recorded.
Refused authentication or bad certificates pause the affected route,
retain pending mail, and expose an actionable error instead of a tight loop.

Smart-host DNS failures, including timeout, SERVFAIL and NXDOMAIN, are route
transport failures. Apply bounded exponential route backoff and health/log
diagnostics; no DNS response by itself permanently fails a recipient. Pending
mail remains queued subject to its normal expiry. Manual retry cannot bypass
the route's concurrency or minimum retry interval.

Default automatic retry starts at five minutes, doubles to an hourly cap with
jitter, and expires after five days. V1 ignores free-text server retry hints.
Persist UTC scheduling data; use monotonic time within a running attempt and
bound catch-up after clock changes/restart. No sleeping thread per queue item.
Use a disk due-time index and fixed queue window, not an in-memory full queue.

A disconnect after the DATA terminator, or a crash after remote acceptance but
before its durable local record, leaves an uncertain outcome. Preserve the same
transmitted Message-ID and body and retry under the normal policy, recording
the duplicate risk. SMTP cannot guarantee exactly-once delivery. Do not use
Message-ID as a provider deduplication promise. An in-flight recovered attempt
is uncertain until its recorded phase proves that message acceptance could
not have occurred.

Local retry/cancel operations are serialized with the worker. Cancellation
is successful only before an irrevocable remote attempt; never report a
message recalled after it may have been accepted. Keep final submission records
for at least 30 days by default and do not automatically expire unresolved
failures. Expired/permanent failures retain inspectable metadata and notify
the account through a locally generated failure message plus health/log state;
never send unsolicited error mail to an unverified inbound reverse path.
If failure notification cannot be committed, retain a pending notification
record and retry locally. Explicit deletion/retention operations reclaim data.

## 12. Logs and observability

[OBSERVABILITY.md](OBSERVABILITY.md) specifies the implemented M04d1 event
and explicit inspection encoders and M04d2 fixed event queue/loss counters
and bounded status snapshots. Schema 29 adds the bounded PostgreSQL service
sink and independent local logging status; full status aggregation remains
planned.

Emit versioned JSON Lines with bounded event sizes, fixed event codes, severity,
UTC time, process boot ID, config generation, connection/request ID, transaction
sequence and submission ID when relevant. Escape/control-character encode all
peer strings. Structured fields have documented meaning and stable enum values.
Default logs omit credentials, Authorization/AUTH contents, message bodies,
subjects, and recipient addresses; authorized inspection can retrieve details.

Service writers retain up to 30 days in PostgreSQL, bounded by a 65,536-row
ring, 1,024-byte events, a shared ingestion rate and a 96 MiB relation-size
stop threshold. Fixed per-kind suppression and bounded nonblocking queues keep
peer floods off the storage path. Capacity can evict records before 30 days.
`postgres logs` reports retention, loss and capacity eviction; `logging-status`
reads boot-local losses and write failures even when PostgreSQL is unavailable.
No file sink or stderr fallback is used by this writer. PostgreSQL WAL and
cluster disk budgeting remain operational responsibilities. Logs are diagnostic
evidence, not metadata authority or an acceptance condition. See
OBSERVABILITY.md for exact bounds and implemented event coverage.

Expose counters for accepted/refused mail, TLS/plain sessions, active slots,
limit refusals, queue age/depth, unknown outcomes, logical disk usage/quotas, dropped logs,
index lag, authentication failures and certificate expiry/renewal. `status`
distinguishes serving, degraded, recovering and refusing mutations. Readiness
requires valid local configuration, usable storage, configured listeners,
an open writer admission gate and completed recovery. Smart-host or CA health
does not determine readiness.
Agent-facing output clearly separates facts, recommended actions, and untrusted
data. No API executes instructions obtained from messages or logs.

## 13. Migration and operational lifecycle

Migrate from Stalwart 0.15.2 through its authenticated JMAP interface and raw
blob downloads, not by reverse-engineering its database. Preserve raw message
bytes, folder names/hierarchy, membership, and received dates; preserve ordinary
keywords/read state when available at little extra cost. Stable remote IDs are
recorded for resumability, not reused as local pathnames. The provisional
export/verify commands and their archive bounds are specified in
`MIGRATION.md`, including its resumable insert-only PostgreSQL importer and
retained plans for existing-message keyword/membership and folder metadata reconciliation,
and explicit only-unmapped import with same-transaction folder checks;
they do not grant final-cutover authority. No identities, rules,
contacts, calendars, account passwords, or server configuration are migrated.

Stream a paginated export into an archive containing raw files and a versioned
manifest with source account/object IDs, byte counts, digests and mailbox
mapping. Resume by source instance/account/object ID plus verification, not
Message-ID or byte digest alone: identical messages can be distinct objects.
Repeated imports must not duplicate a previously mapped object; preserve a
single source email's multiple folder memberships. Do not log access tokens.

A bulk copy while Stalwart is live is provisional. Final cutover requires a
documented quiescent interval: freeze client writes and receipt on the source,
reconcile the final source state, verify per-folder/object counts and digests,
then switch service/DNS. Upstream SMTP senders queue during temporary refusal.
Account for DNS cache overlap by keeping the old endpoint able to temporarily
refuse, or explicitly deliver to the new gateway listener after cutover; do
not leave two independent writable mail stores. Never silently skip failed or
oversize imports. The importer emits a bounded paginated exception report.

Retain an untouched source backup and tested archive. Rollback before new mail
or writes reach td-mta can restore the old endpoint directly; rollback after
that point must export and reconcile td-mta's new mail and changes first.
Changing DNS back alone loses work and is not a rollback procedure.

Run in the foreground as a dedicated unprivileged service identity. Deployment
provisions directories and grants only low-port binding through its supervisor
or a reviewed launcher. Do not run the protocol engine as root or introduce
setuid behavior. Inherited-listener support, if selected, must validate socket
types, addresses and counts, and obey the unsafe contract. Systemd/td-svc
examples are packaging tasks, not executable dependencies. SIGTERM draining
and abrupt-death recovery both need tests; signal integration cannot silently
add an unsafe surface. Persistent state is outside the executable's directory.

This standalone artifact does not automatically enter td's system image.
Future image integration follows the target-graph provenance, profiling,
service, credential and packaging contracts in the repository. A host musl
build is useful for other distros but is not a td target-graph artifact.

## 14. Implementation ownership

The implementation plan is the handoff contract. Tasks must name dependencies,
owned modules, typed interfaces, explicit exclusions, and executable acceptance
criteria. Freeze formats and API shapes before parallel consumers implement
them. Each independently landable increment stays green under DEVELOPMENT.md.
An unresolved design decision is written down before implementing dependent
code; it is not delegated to whichever worker arrives first.

The plan's initial schema/format/dependency tasks resolve exact encodings,
provider features, enforced resource limits and standard capability tables.
They may refine this document with measured evidence, but cannot silently
relax its durability, authentication, bounded-input/work, or no-live-service-test
boundaries. Resource tuning follows section 5.

## 15. Acceptance evidence

V1 is ready only with reproducible evidence for all of the following:

- Static-musl executable inspection (no PT_INTERP or DT_NEEDED), operation in
  a clean Linux environment without td helpers, and host-independent data
  encoding fixtures. Build/test the core on aarch64 when tooling is available;
  td-owned x86-only assembly and native-usize serialization are prohibited.
  Provider assembly must have a supported implementation for each target; its
  presence alone does not prohibit reviewed per-architecture acceleration.
- Syntax/confinement lint coverage and malformed-input property/fuzz fixtures
  for SMTP, HTTP, JSON, MIME, config, database rows, DNS and certificates. Fuzzing
  tooling is development-only and separately pinned/approved if external.
- Measure retained memory across repeated successful and failed operations,
  including logging, TLS and configuration/certificate replacement. Use
  allocation instrumentation to investigate concrete growth where useful;
  allocation itself is not a failure. Pool exhaustion refuses predictably,
  and a long run shows no unexplained retained growth with delivered count.
- A deterministic 1 GiB corpus of 10000 messages with mixed folders, MIME,
  Unicode, attachments, duplicate Message-IDs and distinct identical messages.
  Measure idle and peak RSS after restart and during 4 concurrent SMTP streams,
  2 JMAP readers, one bounded search, queue retry and certificate rotation.
  Repeat resource tests with many small messages and a full queue. Record
  compiler/profile, kernel, filesystem, corpus seed, limits and page-cache
  accounting. Corpus bytes and object count are separate scaling dimensions.
- Kill/fault injection at each publication/sync boundary for inbound receipt,
  JMAP mutation, submission creation/attempt, checkpointing, renewal, migration
  and restore. Verify acknowledgements against recovered state and prove no
  committed object is reclaimed. Include disk full, short I/O and failed sync.
- Real td-mail + real td-fetch + real td-mta: receive/read/search/move/flag/
  delete/thread/download; send with attachments and Bcc; query a submission
  after a lost HTTP response; inspect the local SMTP capture and Sent filing.
- Local SMTP scripts: implicit TLS, required STARTTLS, untrusted/expired/wrong-
  name certificates, missing STARTTLS, bad AUTH, multiline/fragmented replies,
  SIZE/8BITMIME differences, 421/450/451/452/550, mixed recipient outcomes,
  disconnects before and after DATA, restart, duplicate-risk recording,
  cancellation races, route pause, retry expiry and local failure notification.
- Direct and gateway reception: nonlocal recipients denied in both modes,
  peer/certificate admission, forged headers ignored, SMTP smuggling refused,
  slow-client fairness, and fresh TLS use after certificate rotation.
- Local ACME protocol fixture: initial issuance, nonce retry, renewal, rate
  limits, malformed/mismatched certificates, interrupted publication, expired
  certificate behavior and clock changes. A mocked signer alone is insufficient.
- A Stalwart 0.15.2 compatibility fixture or sanitized captured JMAP exchanges,
  a restartable export/import oracle, and count/hash/folder verification. Exact
  source data/version must be identified; a generic mock cannot be labelled
  proof against that release. No production mailbox is modified by tests.
- All integration endpoints resolve within the isolated fixture network, with
  egress denied. The fixture rejects configured public provider names before
  connecting. No live Migadu/CA/DNS/deployment-host dependency.

## 16. References

- [SMTP and responsibility transfer, RFC 5321](https://www.rfc-editor.org/rfc/rfc5321.html)
- [STARTTLS, RFC 3207](https://www.rfc-editor.org/rfc/rfc3207.html)
- [Submission TLS, RFC 8314](https://www.rfc-editor.org/rfc/rfc8314.html)
- [JMAP Core, RFC 8620](https://www.rfc-editor.org/rfc/rfc8620.html)
- [JMAP Mail and Submission, RFC 8621](https://www.rfc-editor.org/rfc/rfc8621.html)
- [Internet message format, RFC 5322](https://www.rfc-editor.org/rfc/rfc5322.html)
- [MIME, RFC 2045](https://www.rfc-editor.org/rfc/rfc2045.html)
- [HTTP/1.1 framing, RFC 9112](https://www.rfc-editor.org/rfc/rfc9112.html)
- [ACME, RFC 8555](https://www.rfc-editor.org/rfc/rfc8555.html)
- [MTA-STS, RFC 8461](https://www.rfc-editor.org/rfc/rfc8461.html)
- [Migadu's documented client settings](https://www.migadu.com/guides/)
- [Let's Encrypt challenge types](https://letsencrypt.org/docs/challenge-types/)

Migadu settings are documentation-derived, not tested against its live server.
Implementation fixtures cite the additional extension RFCs they exercise.

The mailbox-access Core API now runs on the bounded HTTPS body worker.
ADMISSION.md owns its request/response lifetime and quota contract; STORAGE.md
owns the separate data/tmp/requests lock and cold cleanup. Core Session metadata
is exposed by authenticated discovery now that its required endpoints are
active. Mail reads, read/star assignments, mailbox moves and root-folder
creation and empty-folder deletion are exposed for td-mail. The account permits those writes; POLICY.md
specifies its Mailbox rights.
CONFORMANCE.md records the actual client/TLS evidence. EventSource
still emits only pings: td-mail does not consume it. Future Mail state delivery
requires passive state capture without blocking the network reactor or holding
a store view across socket work; this profile makes no such claim.

## Device Email update transaction foundation

The storage coordinator now accepts bounded, device-authorized single-mailbox
replacement plus `$seen` and `$flagged` assignments through the typed
update_emails boundary documented in
STORAGE.md. Memberships, flags and corresponding Email/Mailbox history
commit together,
and known commit outcomes survive later accounting refusal. Authenticated
HTTPS Email/set activates td-mail keyword patches for these two flags and
single-destination mailbox replacement, with matching Session and Mailbox
rights. Pure Email/set destruction now removes messages and last-owner bodies.
Structured Email creation remains absent.

Authenticated Mailbox/set accepts the root-folder name-only create objects
td-mail emits. The bounded native creation coordinator owns validation,
uniqueness, IDs, authorization and commit outcomes. The adapter prepays the
entire response before calling it, preserves per-object refusals and exposes
mayCreateTopLevelMailbox only with this active endpoint. Singleton deletion
uses the same coordinator and deletes only empty roleless leaves, with one
Mailbox tombstone and sequence increment. Role, child and membership refusals
leave state unchanged. The endpoint advertises mayDelete for roleless folders
and confirms only known commits. Hierarchy creation, rename and role mutation
remain absent; POLICY.md owns the request restrictions.

## Prepared sending identities

The sending identity catalog is a cold preparation boundary for td-mail's
default Identity/get objects. It validates the existing bounded canonical
visible-identity preimage before hashing it, then retains only JSON-visible
identity data and IDs. Relay credentials and signature paths are excluded.
The cold encoder reserves at most six times the 192-KiB preimage ceiling
plus 512 bytes per permitted identity. It fallibly copies the used bytes
into a retained allocation and releases that scratch; at most 64 entries
index the immutable bytes. The temporary and retained allocations overlap
only during that bounded copy. JSON escaping expands a source byte at most
sixfold, while the per-identity slack covers object framing. This is an
explicit cold allocation under section 5, not a claim that the proposed
1-MiB configuration-generation partition already covers activation. It
provides immutable object slices and configuration-digest identity state
under a serving snapshot's store epoch after checking its account ID.
Noncharacters in any visible text refuse preparation, matching Session's
I-JSON policy. It confers no authority and does not by itself enable Identity/get,
submission capability or sending.

The prepared identity dispatcher implements td-mail's default Identity/get
projection under an explicitly supplied catalog and Submission declaration.
It does not enter the HTTPS service or advertise Submission yet. Omitted or
null ids selects all configured identities; explicit IDs preserve request
order and deduplicate under maxObjectsInGet. Missing IDs are notFound. The
td-mail profile accepts omitted/null properties and refuses property lists.
Account validation precedes access, catalog and storage accounts must agree,
and identity state binds the catalog digest to the live store epoch. Output
uses the original retained-response quota and deadline, with bounded copying
and a final snapshot check. Other calls in a batch retain normal error and
result-reference handling.

## Plain-text draft composition

The inert `compose_text` worker composes td-mail's plain-text structured
creation fields into caller-owned private staging. It writes ASCII MIME
headers, UTF-8 encoded display words, validated ASCII mailboxes and single
strict Message-IDs. Generated IDs admit visible ASCII only, forbid escapes
and quoted local parts, and retain the shared parser's dot-atom or
non-folding domain-literal grammar. Reply-ID lists retain order. Each identifier occupies
one continuation line and is limited to 995 bytes; address and identifier
lists have a 1000-item ceiling. Caller-supplied Date and Message-ID are not
regenerated. Date is whole-second UTC in the supported 1900..9999 range.
The stored copy retains Bcc. This worker does not prepare a relay copy,
construct attachments, authorize an account, create an Email, or advertise
Submission; those are separate service steps.

Text preserves its UTF-8 bytes, canonicalizing CRLF, bare CR and bare LF to
CRLF before bounded base64 encoding. The worker counts that extent first,
then streams it using fixed scratch, the original work meter and freshly
sampled monotonic time between bounded turns. Each 1024-byte newline
conversion buffer prepays up to 1025 input bytes for CRLF lookahead,
forfeiting unused credit. It rejects clock regression.
Validation, encoding and sink publication spend separately; no replay
renews the meter. A caller-supplied total encoded-byte ceiling is enforced
before each sink write. Any failure leaves only provisional bytes: the
caller must discard staging and publish nothing. Success requires a fresh
final budget/deadline check. Durable quota admission and the native
transaction remain the future creation adapter's responsibility.

The native Rust allocation probe compiles the same composition source and
checks complete Unicode/text output, size refusal and invalid-header refusal
without allocation; caller-owned text and sink backing are outside it.

## Native plain-text draft publication

The worker-side `create_text_draft` coordinator stages a single composed
message under body-byte/blob admission, then atomically publishes its Blob,
JMAP-origin Email, one existing mailbox membership, `$draft` and `$seen`,
thread anchor and Email/Thread/Mailbox history. The original request work
meter and deadline cover composition and native planning. It authorizes
the device at admission and again under a revocation-excluding guard through
publication; no coordinator lock or policy guard spans composition.

Publication rechecks the policy generation, account epoch, optional state
precondition and mailbox. References, In-Reply-To and the supplied Message-ID select an
existing immutable thread using the existing last-32 nearest-first rule;
otherwise entropy supplies a new thread. Supplied visible metadata is
validated by composition before it becomes an anchor. Bcc stays in the
stored copy. Native transaction/accounting settlement preserves known
commit outcomes, stops admission on uncertainty, and releases private
staging and unused quota on refusal. The API caller must reserve its full
response before entry. This typed worker does not expose Email/set creation,
construct attachments, authorize sending or advertise Submission.

New draft Email and Thread IDs must be absent from live rows and retained
change history. The format-compatible history check walks the existing
account/kind sequence index once, paying each examined row from the
original work meter; exhausted work refuses creation. This is linear in
retained kind history, not an ID-point lookup. Pruned history is outside
this collision claim, matching QUEUE.md's random-ID policy. Composition
enforces a shorter execution-phase deadline without renewing its meter.

## td-mail structured draft input

The inert `draft_input` decoder borrows text from an already parsed request
and retains only bounded address and identifier descriptor vectors. It
accepts td-mail's single From, optional recipient/reply lists, Subject,
Message-ID, whole-second UTC sentAt in years 1900 through 9999 with seconds
00 through 59, one `text`/`text/plain` body part,
complete untruncated body value, one local-form mailbox ID, and exactly
`$draft` plus `$seen`. Nonempty attachments, HTML and other creation fields
are refused in this increment. Message header syntax and MIME composition
remain `compose_text` work; decoding confers no authorization or storage.

Each descriptor list has the composer's 1000-entry bound. The admitted JSON
byte ceiling also bounds aggregate decoded strings. Text validation is paid
from the original work meter before UTF-8 scanning, with fresh monotonic
and phase checks before and after; each input member spends a record. A
shorter execution deadline never renews the meter. The decoder does not
activate HTTPS creation or Submission.

## HTTPS plain-text draft creation

Authenticated Mail dispatch accepts td-mail's singleton literal Email/set
creation shape through `draft_input` and `create_text_draft`. It refuses
nonempty mixed create/update/destroy calls, multiple creations and resolved
`#create` arguments in this profile. Existing flag, move and destruction
calls retain their own paths. Account and scoped ifInState checks precede
creation; publication rechecks conditional state and fresh device authority.

The dispatcher materializes and prepays the complete bounded result before
calling the body-worker adapter. Input refusal returns notCreated with
invalidProperties and a known field where available. Ambiguous native Invalid
or NotFound errors return per-object serverFail, never a guessed bad property;
this can conservatively include a composition or missing-mailbox refusal.
Native message-size Quota returns tooLarge, distinct from work capacity and
logical account quota admission. Exhausted staging slots are normalized to
Busy before composition so they remain temporary service pressure. Success returns Email, Blob and Thread IDs,
size and the native before/after Email state. Known publication survives
accounting stop flags and staging cleanup errors; an indeterminate commit
becomes serverPartialFail. Draft staging cleanup failure retires the fully
charged ingress slot until quiescent restart; it does not itself retire the
service. A stopped coordinator still retires the service after the response. An outer deadline or failed physical response write can
still lose the response and never makes creation safe to replay.

The original parsed arena supplies borrowed body text, while retained
arguments still enforce ordinary argument validation and result-reference
resolution. No second JSON tree or full composed-message buffer is created.
Draft creation does not advertise Submission, authorize a sending identity,
resolve creation IDs for later calls, or take responsibility for delivery.

## Transmitted-copy header filtering

The inert `transmitted_headers` worker removes root Bcc and Resent-Bcc fields,
including folded continuations and duplicates, from a private outgoing copy.
It accepts exactly a complete resident root-header section and CRLF separator,
bounded by the configuration's 1 MiB header ceiling plus separator. The shared
header scanner defines fields; all other header bytes and the separator are
preserved. The caller separately streams the original immutable body without
rewriting it. The locally stored draft retains its blind-recipient headers.

No output is publishable until success. Invalid or incomplete extents,
original work exhaustion, phase expiry, clock regression and sink failure
refuse the operation, potentially after provisional output. Scanner turns,
short name comparisons and at-most-4096-byte sink writes spend the original
request meter under fresh monotonic checks. No message-sized buffer is built.
This worker grants neither source custody nor envelope/sending authority;
native queue admission, identity authorization and transactional publication
remain required before responsibility exists.

Outgoing header filtering requires CRLF-only line endings throughout the
section, checked in charged 4096-byte chunks before emission. Bare CR or LF
is refused rather than letting an incoming-parser/receiver difference hide
a blind-recipient field in a preserved value.

## Verified native transmitted copies

`transmitted_copy::write` retains the caller's verified native blob pin while
collecting bounded root headers, applying blind-recipient filtering and
streaming the remaining immutable body in at most 4096-byte chunks. Header
scratch belongs to the caller; no full-message buffer or new work allowance
is created. The configured header and transmitted-size ceilings, original
meter, native snapshot deadline and a non-extending execution phase cover
copying. Every header-filter clock sample also checks the retained native pin,
so header scanning and emission cannot outlive its original snapshot deadline
even when the caller supplies a longer phase. Native integrity verification
precedes entry under its own funding.

The filter changes only complete root Bcc/Resent-Bcc fields. The body is copied
byte for byte, including a body prefix already read while finding the header
boundary. That prefix is reread from the same immutable pin and charged again.
Clock regression, expiry, work or size exhaustion, missing native bytes and
sink failures refuse completion; all emitted bytes remain private and must
be discarded on failure. A successful copy supplies no sending identity,
envelope authority, ingress slot or queue publication. Its consumer still
owns those reservations and fresh authorization before accepting delivery.

## Implicit submission envelopes

The inert `submission_envelope` worker derives the implicit envelope used by
`td-mail` from a complete immutable root-header section and the selected
identity's configured mailbox. It requires exactly one single-address From
matching that identity under `config::values::mailbox_key`: decoded local
case is significant and only the domain is folded. An optional single Sender
must also match that identity before supplying the reverse path. The current
single-address identity policy cannot authorize another Sender header merely
by substituting the envelope reverse path. Duplicate From/Sender fields or multiple addresses
in either refuse. This supplies header evidence, not account, device or
configuration-generation authority; the native publication consumer still
holds the same source and freshly authorizes the selected identity.

Recipients come from To/Cc/Bcc in source order. Shared address parsing supplies
provisional extents, then each SMTP mailbox must independently pass the strict
configuration syntax; raw recovery candidates, groups, address literals and
international addresses refuse. No permissive display fallback grants sending
authority. Equivalent quoted local spellings and domain case deduplicate while
preserving the first address spelling and order. Reply-To and Resent fields do
not add recipients. Explicit envelopes and SMTP parameters are outside this
worker's interface.

At most the configured 1..1000 unique recipients are retained. A fallibly
reserved array plus sorted indices bounds retention and comparisons; insertion
movement and canonicalization spend the original work meter. Header scanning,
CRLF validation, address parsing and comparisons use fresh monotonic checks
under a phase that cannot extend the meter deadline. No envelope escapes an
invalid header, exhausted budget, deadline or clock failure. The transmitted
copy and all queue rows still require separate admission and atomic publication.


PostgreSQL submission admission, source verification, transmitted-copy
publication and durable receipt settlement are specified in POSTGRES.md.
The shared header and envelope algorithms above do not grant publication
authority. The foreground route worker schedules these durable submissions.

## Request-local creation map

Authenticated Email and Mailbox creation retain successful creation IDs in
one bounded request-local overlay. The original request arena retains the
initial createdIds map. A reused key points to the latest successful creation
across both types; refused and indeterminate creations preserve its previous
mapping. The final encoder merges the sorted overlay with the original map
in linear order, skipping empty reservations. It includes createdIds only
when the request supplied it, including an empty object.

ADMISSION.md's active-profile exception owns the aggregate overlay bound and
resident-storage contract. Before creation, the dispatcher reserves each new
slot and conservative response growth from its remaining allowance. Lookup
uses a bounded binary index and the original meter under fresh admission.
Known publication copies a bounded ID into the prepared stable slot without
allocating or searching. Initial mappings need no second allocation or
quadratic seed scan. Counting and output stream one bounded pair per turn.
This profile records creation IDs but does not yet resolve #draft in
EmailSubmission/set or advertise Submission capability.

## HTTPS authentication worker handoff

HTTPS origin checks and bounded attempt admission remain on the network
reactor. Credential decoding and verifier lookup run on one fixed
`https-auth` worker through `http_auth::Authenticator`. Pending work occupies
only existing bounded connection slots; no task or connection is spawned per
authentication. Busy retries keep the same attempt charge and original
absolute/header/idle deadline. The worker checks monotonic time before and
after lookup; an expired or regressed result cannot enter routing.

Decoded credentials remain in the bounded client owner, with zero-on-drop
storage, for future fresh body/method authorization. Ordinary responses and
EventSource clear them with the original credential head before output;
other client retirement also clears them. A successful device ID is
only the initial observation; mutations and body admission keep their existing
fresh authorization boundaries. The PostgreSQL verifier owns the database-backed device check. The backend contract requires bounded completion
under the supplied deadline. Service shutdown joins the verifier along with
the existing workers; database lookup must never run on the network reactor.

## Native ACME deployment profile

[ACME.md](ACME.md) owns the implemented single-endpoint native HTTP-01 worker,
its supported CA subset, persistence bounds, renewal scheduling and operational
commands. It starts independently of selected mail profiles, so initial issuance
can run before a usable certificate exists. Mail profile import/activation still
requires a valid certificate. The native worker owns the declared HTTP-01
listener. Managed certificate changes currently cause bounded mail-service drain
and internal reload with a brief listener gap; uninterrupted generation swapping
remains outside this deployment profile. Explicit external certificate inputs
remain supported. No Certbot subprocess or dependency is introduced.

Cold profile preparation retains one bounded binding, private identity
catalog and Session per configured account. Authentication selects the account
before dispatch, and all backends share the process admission limits. Every
configured account must be bootstrapped before the profile can publish.
Receiving uses one retained fanout and one shared prepared body. Relay scheduling
rotates account-scoped leases through one foreground worker and one shared
queue-clock guard. RESOURCES.md accounts for aggregate startup and relay
metadata independently of message-body reservations.
