# td-mta: personal mail service

## 1. Status and scope

This is the normative target for a new service, not a claim that it exists.
`IMPLEMENTATION.md` divides it into independently reviewable increments.
An incomplete increment must not advertise capabilities it cannot provide.
The v1 release requires the acceptance evidence in section 15.

The M01 library skeleton provides typed local IDs, configuration versioning and
checked resource planning only. [RESOURCES.md](RESOURCES.md) records its checked startup
byte ledger; [CONFORMANCE.md](CONFORMANCE.md) inventories the unimplemented JMAP
contract and current client calls. There are no protocol handlers or listeners.
The M02a/M02b format module adds checked scalar/key/row codecs and literal
format fixtures. [FORMAT.md](FORMAT.md) fixes their byte layout and the
container registry. M05a1 adds exact FORMAT, CURRENT and journal-header
encoders/decoders with checked digests. These codecs do not validate selected
store bindings, replay transactions or perform filesystem I/O.
M05a2a adds checked table headers and individually checksummed borrowed records;
M05a2b1 checks supplied table streams for order/count/extent and computes
their digest. M05a2b2 adds bounded manifest structure codecs. M05a2b3 binds
selected metadata, completed table summaries and journal headers. M05a3a adds
checked fixed transaction headers and local PUT/DELETE/CHANGE codecs. M05a3b
validates complete frames and supplied journal streams with bounded counters,
sequence continuity and sticky failure; it seals caller-built payloads in place.
M05a3c binds history summaries to selected descriptors and active summaries to
the caller's pinned committed prefix.
M05b1 generates canonical storage paths. The std filesystem adapter checks
existing directory types, links and private-root permissions under an explicit
operator-controlled stable-path contract. It performs no direct syscalls or
free-space probes. A retained std file lock provides cooperative writer
exclusion. Typed private-file readers now require complete extent consumption
and physical EOF before returning a retained completion handle. Selection
loading reads FORMAT, CURRENT and only its named manifest, then binds their
account, epoch, generation and digests in caller-owned scratch. Selected-table
input checks headers against that manifest, yields one provisional record at
a time, and requires physical EOF and the complete table digest to finish.
Retained-history input likewise yields one checked, provisional frame at a time
and finishes only against the selected segment's extent, sequence and digest.
Captured-prefix I/O now opens private journal files, allows append growth and
returns distinct prefix completion without claiming physical EOF. Active
input validates supplied view identity/ranges, streams checked frames and binds
completion to the captured sequence/offset. It still requires actual caller
pin ownership. Read-only stopped-journal recovery scanning now distinguishes
physically incomplete final bytes from complete corruption and retains a
verified prefix boundary through physical EOF. Explicit tail repair rechecks
CURRENT and the scanned file identity/extent, truncates only that suffix and
syncs before confirming the repaired extent. Referenced-blob input now checks
exact length, streams SHA-256 and requires whole-file EOF and digest equality
against a supplied row. The supplied-byte replay overlay now validates complete
frames, sorts bounded descriptors and resolves latest operations/tombstones.
The active-overlay loader reads a captured private prefix into caller arenas,
binds its selected header/endpoint and retains the consumed prefix descriptor.
A provisional streaming merge now combines sorted checkpoint records with
overlay replacements and deletions using fixed key state and borrowed rows.
The table-replay adapter binds a fresh table input to the loaded prefix and
requires selected table digest/EOF completion before draining residual updates.
Point lookup now scans that complete replay and retains one matching row in
caller scratch; it returns a row or absence only after selected-table completion.
An ordered next step applies the same completion rule to the first final row,
or the first row strictly beyond an optional encoded cursor.
A bounded mailbox-parent walker now checks one supplied ReadView chain for
missing targets and cycles without retaining a visited-node collection.
Incremental frame validation now checks supplied operations and their final
footer without retaining a complete frame; entries remain provisional until
completion. A frame-change collector copies compact descriptors into separate
caller slots and exposes them only after checksum completion. Incremental
journal validation now connects those frames to a whole-journal checksum and
bounded sequence progress; an abandoned frame retires its parent. Retained-
history input can now read selected files with one operation buffer and compact
change slots, requiring EOF/digest binding at completion. Active change input
shares that operation reader while stopping at a captured prefix, allowing
append growth and binding completion to its sequence/offset. A supplied-frame
cursor now checks captured identity, retained floors and endpoint
boundaries while draining supplied checked frames. A selected-route helper
checks retained coverage and chooses a history descriptor or the captured active
segment. A bounded locator now reads at most one frame per step within that
source, hides earlier changes and returns reusable slots only after selected
completion. A sequential scan now coordinates cursor draining and checked
source transitions with the same arena. Actual view-pin integration remains
separate.
Direct owning references now have a bounded supplied-row checker with at most
two lookups. A supplied-view sweep now enumerates direct checks in bounded
steps with fixed counts; physical graph completeness and remaining aggregate
invariants still need coordination. An ordered recipient sweep now verifies exact
submission recipient-count/ordinal coverage and current queue state/group
consistency. Attempt transitions and worker fences remain separate.
A mailbox sweep now enumerates and roots every supplied parent chain with
separate row/get budgets and no growing visited set. A blob sweep connects
final rows to private-file, chunked digest and EOF verification under finite
row/byte budgets. A selected-table sweep now verifies and replays all 11
checkpoint tables using one reusable record buffer and finite byte/row limits.
A retained-history sweep checks all selected immutable segments with shared
record/change scratch and finite byte/frame admission.
A stopped-store owner now retains the cooperative lock behind read-only
validation operations; consuming it restores mutation access after borrows end.
Its journal capture now derives the complete prefix and retained-history floor
from a stopped scan, reports incomplete tails without repair, and loads the
same prefix for replay using caller storage.
A file-validation coordinator now couples that owner to the captured overlay,
all selected table replays and retained history before returning borrowed
physical-file evidence. An offline ReadView now connects this evidence to row
lookup, ordered iteration and retained change history with work/deadline limits.
An owned data-validation pass composes the reference, recipient, mailbox
and blob sweeps through that reader, compares their counts with physical
replay, and retains stopped ownership on completion. Recovery repair/accounting,
mutation policy and service activation remain separate.
A bounded account-verification entry point now loads actual CURRENT, captures
and replays the prefix, and drives selected-file and data checks under one
monotonic deadline. It returns owner-bound summaries with reusable scratch;
incomplete tails are reported without repair. The inspection CLI is still
unimplemented.
A consuming verification transition now retains the locked store with copied
CURRENT, view identity and journal summary, without scratch or mutation
access. It refuses incomplete tails, and failed verification releases the
lock for a later reacquisition. This prepares ownership for committed
visibility; it does not activate a service or grant live reader leases.
A scoped journal session now consumes that owner and its recovered ledger,
rechecks the selected journal, and serializes append before paired
sequence/offset publication. Bounded borrowed identity pins retain the
selected namespace and old prefixes. Any failure after reservation retires
its writer; a deadline failure following publication can leave visible
durable state without acknowledgment. Each pin can lend a bounded ReadView
scope over its captured prefix, with caller scratch, full physical-file checks
and one monotonic deadline. Later appended bytes stay invisible to old readers,
which do not hold the writer lock. A fixed read scratch pool now pairs each
lease with a captured pin and returns both on drop. Startup verifies and
clears caller backing; captures and reads allocate no new backing. A pooled
view can now lend an immutable body input bound to its captured BlobRow.
Sequential integrity checks produce a bounded random reader of the same
descriptor; one deadline and the view borrow survive that transition. Worker
scheduling, live checkpoint/retention transitions and protocol mutations
remain unimplemented.

The scoped session also accepts an irreversible coordinator write-stop
request. It fences new commits and reports Busy until an in-flight writer
can be confirmed idle. Bounded commit steps observe the request; existing
durable bytes or published state are retained for recovery. Readers keep
their pins. Service-health reporting and queue cancellation remain runtime
work.

Complete mutation-policy validation and mail publication remain unimplemented.
A one-frame append primitive now validates a successor against a complete
scan, rechecks CURRENT/inode/extent, and writes bounded chunks before sync
and EOF confirmation. Failed or abandoned appends retain uncertain effects
for recovery; the reservation-bound adapter stops admission on uncertainty
or abandonment and reconciles exact frame charges only after durable
completion. The scoped session now couples it to atomic identity visibility.
A reconciled boundary can start its successor without a full journal rescan,
while retaining the CURRENT/inode/extent checks and append exclusion contract.
Private temporary output now has exclusive creation, bounded I/O and explicit
file/parent sync. Typed private-directory creation also syncs the new directory
and parent. Completed private blobs and fresh table/manifest/journal files can
be published without replacement using std hard-link and directory-sync
operations. These low-level primitives grant
no transaction, hash or admission authority. Expected CURRENT replacement uses
a synced same-directory temporary and rename; graph validation, writer/view
barriers and recovery still gate any accepting service.
[WIRE.md](WIRE.md) pins implemented wire-ID and
MIME-part locator codecs separately from the future protocol handlers.
[API.md](API.md) defines the compiling M02c2 adapter contracts and implemented
state codecs; [QUEUE.md](QUEUE.md) freezes future queue/restart/JMAP semantics.
[POLICY.md](POLICY.md) freezes message interpretation, threading and queries;
[UNICODE.md](UNICODE.md) pins approved Unicode data and bounded normalization;
[leap-seconds/README.md](leap-seconds/README.md) pins approved IANA leap data;
[CASES.md](CASES.md) names the protocol acceptance oracles still to implement.
M04's `bounded` and `ownership` modules provide caller-owned buffer/queue/slot
primitives. Its `admission` module validates disk/work settings and derived
capacity requirements and supplies charged work meters, timer budgets and
fixed logical leases with grouped quota checks and effect tickets. The scalar
writer ledger derives checkpoint output bounds from those quotas and models a
closed writer barrier. Physical free-space accounting is absent; bounded logical
reservations cannot guarantee successful future I/O. M05/M08 still own actual
publication, orphan accounting and checkpoint reconciliation.
They do not instantiate service pools, perform live disk I/O or
implement protocol handlers.

M07d1 supplies an exclusively owned nonblocking TCP stream adapter and shared
runtime/TLS clock conversion, as specified in API.md §1.2. M07d2 composes
opaque shared TLS sessions with preallocated borrowed or owned record/tail
buffers and fixed deadlines (API.md §1.3). Local facade and TCP fixtures cover bounded progress,
backpressure, close and terminal failures. These foundations
do not open listeners, dial endpoints, admit slots or activate serving paths.

The initial deployment is one person's approximately 1 GB of mail, multiple
domains, and explicit aliases on each domain pointing into one account's
mailbox store. Account IDs remain explicit in every storage and authorization
interface; v1 need not provide shared accounts or delegated access.
Configuration validation rejects a second account and any alias targeting it.

The service receives Internet SMTP for local recipients, stores and serves
mail through JMAP, and submits outgoing messages through a configured smart
host. It also supports receiving from an upstream gateway MX. It ships as
one executable with all executable dependencies statically linked against
musl for x86-64 Linux 5.6 or newer. Files for
configuration, secrets, trust, and mail remain external. Data formats and interfaces must permit a future aarch64 build.

Included in v1:

- SMTP reception, STARTTLS, durable local delivery, and explicit aliases.
- JMAP Core, Mail, and Submission sufficient for their advertised contracts
  and the existing td-mail client, including attachment upload/download,
  MIME construction, text search, threading, and submission reconciliation.
- Durable smart-host queue, retry, cancellation, and failure visibility.
- HTTPS with per-device application passwords and automatic ACME renewal.
- File configuration, inspectable mail storage, bounded rolling logs,
  machine-readable administration, migration, backup, and recovery tools.

Deferred: SPF, DKIM, DMARC verification and catch-all delivery. Excluded from
v1: IMAP, POP, public SMTP submission, direct-to-recipient-MX outbound delivery,
external forwarding, plus addressing, Sieve, vacation replies, spam scoring,
antivirus, calendars, contacts, webmail, clustering, and attachment-content
indexing. MIME attachment metadata remains searchable where JMAP requires it.
An inbound message is not authenticated merely because its transport uses TLS.
V1 must report sender authentication as not evaluated, never as passed.

## 2. Invariants and trust boundaries

1. Final SMTP acceptance and successful JMAP writes follow durable publication
   of the data and metadata needed to recover that operation.
2. Only configured recipients receive inbound mail. Neither Internet clients
   nor a trusted gateway can turn the receiving listener into an open relay.
3. Every JMAP object, blob, identity, and submission is scoped to an authorized
   account. Possession of an opaque ID does not authorize its use.
4. A submission accepted by JMAP remains queryable after restart, including
   while its smart host is unavailable. Queue state is not held only in RAM.
5. Request processing has fixed memory, connection, descriptor, disk, and work
   limits. Exceeding a limit produces a defined refusal, never silent loss.
6. Production td-owned code follows the repository's panic/indexing rules.
   Checked arithmetic, bounded nesting, and explicit error handling extend
   the rule to lengths, counters, conversions, locks, and thread creation.
7. Cryptographic primitives and TLS come from the narrowly reviewed provider;
   application protocols and storage use std and td-owned interfaces. No
   shell subprocesses implement mail, certificates, configuration, or
   administration.
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

Use `td-mta/` for the service library and installed binary named `td-mta`.
The M03b2c packaging entry point supports only `--version` and `--help`;
service commands arrive with their implementations. Its direct
dependencies are the local `td-crypto`, `td-header`, `td-json` and
`td-nfc` crates. Application protocols, storage, configuration and
scheduling use std plus these local libraries. There is no separate
runtime package or td-net helper executable. `td-crypto/DESIGN.md` owns
the shared crypto/TLS API and private backend; `td-crypto/TLS.md`
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
The mail hot path uses only its incremental string API, whose state and
caller-provided output are fixed; its allocating Json value/parser API is not
admitted there. td-json owns no mail, clock, crypto or scheduler policy.

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
Unicode 17.0 inputs in UNICODE.md. They add no Cargo dependency or runtime
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
approved, checksummed IANA input in leap-seconds/README.md. Offline tooling
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
M03b2 pins the portable native inputs. No external async runtime, web
framework, database, mail parser, serialization or ACME framework rides along.
The backend increment demonstrates static musl linking and bounded TLS
behavior before consumers depend on it; exact versions belong in the
lock/review.

The portable musl artifact has a separate, host-only build manifest: pin Rust
and its target standard library, the musl C compiler/linker and sysroot needed
by the crypto provider, and their source/artifact checksums and provenance.
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

The production library and binary both retain `forbid(unsafe_code)`.
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

Targets for the default personal profile are RSS below 64 MiB idle and below
128 MiB under the workload in section 15. These are unverified release gates,
not measurements. Record RSS and cgroup memory separately: filesystem page
cache can affect the latter.

Use a fixed set of long-lived workers, preallocated connection slots, and
bounded queues of slot IDs. Allocate and touch application arenas before
opening listeners. Do not spawn a thread per connection/request or load the
mailbox's full metadata/content into RAM. Use bounded I/O chunks, sorted metadata files, and
disposable disk indexes with a fixed cache. No unbounded mmap or memory-sized-to-mail
strategy is permitted. Maintenance shares an explicit budget with live work.

Event-source streams hold connection slots but no storage read view between
events. RESOURCES.md fixes their nonblocking main-thread scheduling and the
eight fixed worker threads across six roles; event peers cannot occupy
request/commit/control workers.
Use safe std with a bounded round-robin socket scan and a deadline-based wait
of at most five milliseconds. Established TLS record work is bounded per
slot; disk, dial, handshake and DNS jobs use fixed workers. Safe std exposes
no poll/epoll; adding an OS readiness adapter would require the explicit
unsafe review from section 3 and a resource-contract amendment.
Read slots are acquired per store operation with a bounded fair wait queue;
exhaustion returns a retryable HTTP 503 before response headers or the mapped
JMAP method error. A streaming response that has begun cannot fabricate an
error object inside its body: finish from its admitted view or terminate it.
An online backup uses one read slot; interactive work shares the other. This
intentional concurrency limit preserves the memory budget.

[ADMISSION.md](ADMISSION.md) fixes disk quotas, checkpoint completion space,
maintenance drain/exclusive budgets, network timers and exact JMAP response
retention. These are admission contracts, not measured throughput or guarantees
against a host disk filling concurrently. They gate persistence and protocol
implementations alongside the RAM ledger.

Initial default ceilings (validated together at startup):

| Resource | Default limit |
| --- | --- |
| Simultaneous inbound SMTP sessions | 8, at most 2 per peer address |
| Active HTTPS connections | 8, with a separate cap of 2 TLS handshakes |
| Event-source connections within the HTTPS pool | 2 |
| Concurrent smart-host deliveries | 1 |
| Concurrent body/search jobs | 2 |
| Raw message or uploaded blob | 32 MiB, streamed |
| Aggregate header bytes / MIME nesting / MIME parts | 256 KiB / 32 / 1024 |
| SMTP recipients per transaction | 100 |
| JMAP JSON request / methods per request | 1 MiB / 16 |
| JMAP object IDs per get/set / query page | 256 / 256 |
| JSON nesting / parser tokens per request | 32 / 32768 |
| Combined resident index cache | 8 MiB |
| Storage read views | 2, each with a 4 MiB journal arena plus bounded descriptors |
| Unattached upload storage | 128 MiB per account, expiry after 24 hours |
| Retained submission storage | 256 MiB and 1000 submissions |
| Maintenance sort scratch on disk | 64 MiB per account |
| Active log plus retained generations | 8 MiB each, 4 retained |

Values are operator-adjustable within compiled maxima and an explicit startup
memory budget. SMTP command/path/line limits also obey their protocol minimums;
the resource table does not replace RFC limits. The implementation must add an
arena ledger with byte counts, worker stack sizes, scratch reservations, and
TLS headroom before committing a default profile. A larger configured pool
cannot silently retain the default memory claim.

The ledger reserves 96650496 bytes under the default 96 MiB planning budget,
including planned stack, TLS, reload and process allowances. Default connection
counts remain eight SMTP, eight HTTPS and one outbound delivery. Established
TLS processing has a separate allowance for the single main thread, alongside
the handshake and generation allowances. These are qualification targets, not
RSS evidence or enforced provider allocation limits. M02/M07 must fit concrete
structures and provider use within the ledger before enabling service admission.

The no-allocation contract covers td-owned hot processing: SMTP parsing and
streaming, MIME scanning, HTTP/JSON parsing, JMAP evaluation/serialization,
store commits, queue dispatch, and structured logging after slot admission.
Use reusable arenas, borrowed views, bounded formatting, and fallible capacity
checks. Allocating on every admitted request and calling it admission work is
not an exemption. Any std/platform allocation unavoidable in an I/O adapter is
named, bounded, and measured alongside TLS; it must not grow with message or
mailbox size. Configuration reload, startup, certificate rotation, and offline
migration are cold paths, still bounded and accounted for at peak overlap.

TLS library allocations are a separate measured budget, including handshake
records, certificate chains, session caches, and concurrent old/new certificate
generations. Application allocation counters alone do not establish whole-
process bounds. Disable unneeded resumption caches and 0-RTT.

All network operations have idle and total deadlines; slow progress must not
reset the total deadline forever. Bound keepalive requests and fairness between
SMTP and JMAP so unauthenticated connections cannot starve an authenticated
mail reader or health checks. Resolve configured outbound hostnames using a
bounded std-only DNS adapter with explicit UDP/TCP deadlines and A/AAAA/CNAME
limits; do not hide uninterruptible resolver work in unbounded threads.
Never perform recipient-MX lookups for outbound mail. Saturation produces SMTP
temporary failure, HTTP/JMAP limit responses, or bounded retry, as appropriate.

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
assumptions. Std writer locking and private temporary I/O are implemented;
committed mail publication and runtime admission remain pending. The service
uses logical quotas and handles disk-full/write/sync failures; it does not
measure or promise physical free space before admission.

Planned commands, with stable JSON output and exit codes:

| Command | Contract |
| --- | --- |
| `config check` | Offline syntax, references, permission, and resource validation |
| `config show --redacted` | Effective values, defaults, and configuration generation |
| `serve` | Foreground service, no daemonization |
| `status --json`, `doctor --json` | Health, bounds, certificates, storage, queue; no mutation |
| `dns-plan` | Expected records and MTA-STS policy; no DNS changes |
| `reload` | Validate candidate, report bounded pending issuance if needed, then atomically install or retain old config; SCHEMA.md owns cancellation/deadline rules |
| `queue list`, `queue inspect ID` | Paginated status and redacted reasons |
| `queue retry ID`, `queue cancel ID` | Named, journaled operation; never repeat accepted recipients |
| `device create`, `device revoke ID` | Local credential administration |
| `store layout`, `store inspect`, `store journal`, `store export` | Bounded read-only decoding/export under local administrator authority |
| `store verify`, `store repair` | Read-only verification; explicit offline repair with a manifest |
| `backup`, `restore`, `migrate` | Bounded, resumable tools using the storage contract |

Online mutations go through a private Unix control socket and the single
writer. The socket directory's permissions establish administrator authority;
there is no public administration HTTP API. Offline mutating commands require
the exclusive store lock. Readiness checks do not contact Migadu or a CA.
`doctor` network probes, if later added, require an explicit opt-in and cannot
send email. Configuration checks never expose supplied secrets in errors.

Listener address changes, data-root changes, and pool sizing require restart.
Aliases, device revocations, and validated certificates can reload. Revocation
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

Accept HTTP Basic authentication only over verified server HTTPS from clients;
td-mta presents the certificate and the client verifies it. Credentials do not
grant local OS login or td elevation. Service-owned ACME keys and smart-host
secrets are unattended server credentials, distinct from td's human-session
credential portal. A later td image integration must specify provisioning
under that platform's credential policy rather than using its su escape hatch.

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

## 8. On-disk store and crash consistency

[STORAGE.md](STORAGE.md) is the normative physical storage specification.
Read it before implementing persistence, queries, queues, migration or backup.
It owns the directory/file layout, authoritative row model, binary container
rules, transaction publication, checkpoint/replay, bounded read views,
reclamation and inspection commands.

The chosen representation is ordinary immutable `.eml` files plus compact
binary metadata inspected through td-mta commands. Folder names, membership,
keywords, JMAP IDs/history and submission outcomes are authoritative metadata;
message bytes alone cannot reconstruct them. Parsed headers, offsets and search
indexes are disposable caches. Live metadata changes use the transaction API;
direct file editing is unsupported. No database dependency is added.

One sorted metadata checkpoint plus a bounded recent-change journal forms the
current account state. The single writer pauses new mutation admission during
checkpointing; readers pin a checkpoint and an exact committed journal prefix.
This is a small purpose-built storage engine with explicit recovery obligations,
not a claim that filesystem rename alone supplies multi-object transactions.
Large message bytes are streamed and never included in metadata checkpoints.

The core publication rule remains: sync new immutable bytes and their published
directory entries, then append/sync the complete metadata transaction, then
expose it and acknowledge success. Preserve queued transmitted bytes independently
of the visible email's lifetime. A failed sync stops the writer for recovery.
Storage corruption is distinct from a rebuildable index failure. Account backups
pin one exact view; capturing all service secrets/configuration initially requires
a stopped whole-service backup.

## 9. SMTP receiving and message representation

Implement a bounded SMTP state machine with EHLO/HELO, MAIL, RCPT, DATA, RSET,
NOOP, QUIT, SIZE, 8BITMIME, STARTTLS, and enhanced status codes. Advertise
PIPELINING only after its ordered parsing and error paths are tested.
Do not advertise SMTPUTF8, CHUNKING, BINARYMIME, DSN, or AUTH in v1 reception.
Local configured addresses use ASCII domains/local-parts; domains compare
case-insensitively, while ordinary local-parts match aliases case-sensitively.
Reserve case-insensitive `postmaster` at every served domain and the SMTP
domainless Postmaster form, routing both to the sole account. Configuration
must validate that route; it cannot disable it or redirect it outside the
account. Support VRFY with the standard non-enumerating 252 response.
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
V1 has one account, so its final DATA success is one store transaction. Extending
to multiple recipient accounts requires a durable delivery manifest before
acknowledgement; independent partial writes cannot masquerade as atomic success.

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

## 10. HTTP and JMAP

Implement bounded HTTP/1.1 for discovery, authenticated method calls, uploads,
and downloads. Reject conflicting Content-Length/Transfer-Encoding, ambiguous
header syntax, oversized chunks, invalid request targets, and request smuggling
patterns. Bound headers independently of bodies. Stream chunked/content-length
uploads and downloads. No request compression, arbitrary proxy routing, or
cross-origin browser access by default. Protocol errors terminate the connection
when its framing cannot be trusted.

RFC 8620 and RFC 8621 define wire semantics. Maintain a capability/method/property
coverage table with normative section references and local acceptance fixtures.
Capability advertisement is a release gate: a td-mail-only happy path is not
proof of conformance. Unsupported optional capabilities are omitted; unsupported
methods/properties get the specified errors rather than fabricated empty success.
Publish truthful resource limits and read-only identity properties. Include
the standard session eventSourceUrl and a bounded authenticated event-source
endpoint, with coalesced account state, type filtering, ping and closeafter
behavior. Polling also works. External push-service registration is refused
by explicit policy using the standard method error; v1 never POSTs to a
client-supplied callback URL. The conformance table must pin that refusal
and the associated get/set behavior rather than silently omit Core methods.

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
and bounded status snapshots. Runtime synchronization, status aggregation
and file output remain M19.

Emit versioned JSON Lines with bounded event sizes, fixed event codes, severity,
UTC time, process boot ID, config generation, connection/request ID, transaction
sequence and submission ID when relevant. Escape/control-character encode all
peer strings. Structured fields have documented meaning and stable enum values.
Default logs omit credentials, Authorization/AUTH contents, message bodies,
subjects, and recipient addresses; authorized inspection can retrieve details.

One writer rotates by size using rename, retaining the configured generations.
Bound log buffering; a flooded peer cannot block durable mail storage behind
logging. Report suppressed-event counts and log-write failures in health plus
a rate-limited stderr fallback. Logs are diagnostic evidence, not the journal
or an acceptance condition. The supervisor must not rotate the same files.

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
recorded for resumability, not reused as local pathnames. No identities, rules,
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
Future image integration follows the source-bootstrap, profiling, service,
credential and packaging contracts in the repository. A host musl build is
useful for other distros but is not proof of a source-bootstrapped td artifact.

## 14. Implementation ownership

The implementation plan is the handoff contract. Tasks must name dependencies,
owned modules, typed interfaces, explicit exclusions, and executable acceptance
criteria. Freeze formats and API shapes before parallel consumers implement
them. Each independently landable increment stays green under DEVELOPMENT.md.
An unresolved design decision is written down before implementing dependent
code; it is not delegated to whichever worker arrives first.

The plan's initial schema/format/dependency tasks resolve exact encodings,
provider features, memory ledger and standard capability tables. They may
refine this document with measured evidence, but cannot silently relax its
durability, authentication, allocation, or no-live-service-test boundaries.

## 15. Acceptance evidence

V1 is ready only with reproducible evidence for all of the following:

- Static-musl executable inspection (no PT_INTERP or DT_NEEDED), operation in
  a clean Linux environment without td helpers, and host-independent data
  encoding fixtures. Build/test the core on aarch64 when tooling is available;
  td-owned x86-only assembly and native-usize serialization are prohibited.
  Provider assembly must have a supported implementation for each target; its
  presence alone does not prohibit reviewed per-architecture acceleration.
- Syntax/confinement lint coverage and malformed-input property/fuzz fixtures
  for SMTP, HTTP, JSON, MIME, config, journal, DNS and certificates. Fuzzing
  tooling is development-only and separately pinned/approved if external.
- Allocation instrumentation of admitted hot operations, including error and
  logging paths; separate TLS/cold-path measurements. Pool exhaustion refuses
  predictably, and a long run shows no resident growth with delivered count.
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
