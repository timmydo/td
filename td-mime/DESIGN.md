# Shared MIME parsing

`td-mime` owns byte-level MIME and mail-header interpretation. It uses std
and the local td-header, td-json and td-nfc primitives. It has no unsafe
code, cryptographic backend, accounts, file access, network access or
ambient clock. Cold corpus verification uses the engine SHA-256 source;
that verifier is outside the runtime library and adds no Cargo dependency.

The library parses immutable resident bytes and emits bounded cursor turns
into caller-owned storage. The delimiter scanner also accepts contiguous
fragments of a caller-pinned immutable extent without retaining input. Raw
bytes remain authoritative. Malformed content reports parser diagnostics;
resource refusal is distinct from malformed content and never publishes
a partial successful property.

Callers supply monotonic ticks, the original live work Meter and, where
needed, a shared aggregate HeaderBudget. Copying a source checkpoint never
copies admission authority. Every replay, interpretation and generated
output spends those original budgets; terminal refusal stays terminal.
Credit exposed across the crate boundary starts at zero, cannot be copied
or fabricated, and binds to the original meter and header budget. Lazy,
nonwrapping owner labels preserve that binding when owners move, without
allocation or address identity. A mismatch retires the credit before any
debit. Exhausting the label space refuses admission; labels convey no account,
file or publication authority. Private decoder credit stays with its owner.

The shared engine also owns bounded mail-header property grammar and passive
JSON projections for callers such as JMAP. Mail retains request selection,
source authorization and final response publication. The work meter includes
an unlink counter so the mail service can use the same original job across
parsing and storage; the MIME library performs no unlinks. Its deadlines use
shared error types; the mail adapter maps service deadline validation errors.

Structure Limits bound header bytes, part count and nesting depth. The
caller maps its service limits into these values. A structure cursor's
input accessor exposes only its immutable byte slice, base offset and
header ceiling; it conveys no account or file authority.

Unicode 17 inputs, generated tables, licenses and offline generation are
specified in [UNICODE.md](UNICODE.md). Charset, malformed-replacement,
unfolding, structured header and parameter behavior retain the existing
mail contracts in [the mail API](../td-mta/API.md). Their byte fixtures,
exact cost oracles and compile-fail ownership guards move with the shared
implementation. The mail allocation probe compiles those same files with
the service adapters; it does not maintain another parser copy.

Mail-specific source admission, descriptor custody, request selection and
response publication remain in td-mta. Mail re-exports the shared byte
parsers; the shared structure parser is named mime_structure. Service
response adapters use the separate flat mime_response module. Its sibling
adapters can access internal owner fields for composition; external
callers cannot construct those owners or extract their admission handles.
The shared crate owns no storage format, database index or transaction.

The raw MessageIds cursor exposes its consumed source offset so an enclosing
owner can retain an identifier's bracketed extent at Begin/End. The extent
is passive and provisional until the complete field validates. Replaying a
selected extent still uses the original source and live work meter; no copy
of parser progress duplicates source custody or admission.

Header value grammar classification precedes selection, but an unsupported
grammar refuses only when a matching field exists. Complete selection can
prove absence without interpreting a value and returns null or an empty
array. Source, budget and deadline refusals are never absence. Output remains
provisional, including an opening array bracket before a grammar refusal.
Only Text currently reports UnsupportedGrammar from classification. The shared
core defers that typed refusal for any projection that reports it; no other
validation error is deferred. In :all mode a provisional match can refuse
before a later scan failure; last-instance selection scans the whole section
first. This error ordering follows streaming selection and grants no partial
success or publication authority.

Fragmented delimiter scanning shares the resident scanner's line state,
CRLF/LF/bare-CR policy and offset calculation. The declared extent length
is exact; empty fragments request more input until that length is consumed,
and bytes after it remain unconsumed. Each poll reports its consumed prefix,
at most 128 body bytes, so callers retain the remainder. Initial boundary
validation is a separate turn. Every active turn spends one record;
boundary validation spends one I/O visit per byte and body scanning spends
two per byte. Events remain provisional through complete extent validation
and fresh consuming finish. Cached Complete is passive; explicit deadline
checks and finish use the original live meter. Source custody, fragment
ordering, native reads and EOF/truncation detection belong to the caller.

Delimiter preceding_end excludes the previous line ending and can precede
an empty child's start when delimiters are adjacent. Child owners clamp
that offset to their own start before constructing a child extent.

The composable delimiter Scanner owns its bounded boundary bytes and binds
to the original Meter's move-stable owner label. Construction funds the
boundary copy (one record and one I/O visit per byte); normal validation
and scanning retain their existing costs. Every poll, deadline check and
finish verifies the same meter, including after Complete. A replacement
meter retires the scanner before debiting its counters; owner-label
exhaustion refuses construction. This owner binding does not authenticate
source bytes: the caller retains source identity, extent and fragment order.
The borrowed Stream and resident Cursor keep their existing costs and
passive cached-completion behavior.

Structure classification shares one implementation between resident traversal
and consumers of retained entity headers. It accepts a completed metadata
selection with the same immutable header bytes and absolute base; it never
needs body bytes. Default context, known transfer encodings and unknown
encoding diagnostics are unchanged. Its public entry charges the original
Meter and aggregate HeaderBudget through owner-bound Credit, including fresh
zero-cost admission for defaults. One turn compares at most 31 source bytes;
each comparison byte spends one source visit and one interpretation step.
Values remain passive; the enclosing owner retains source custody and makes
any refusal terminal for its operation.

The shared entity-header interpreter selects metadata, classifies it and
retains at most 70 boundary bytes under the original Meter/HeaderBudget.
Resident structure traversal uses the same core. Input may be a retained
prefix with a definitive header boundary; incomplete prefixes refuse. The
interpreter never reads a MIME body or grants complete-tree authority. It
rejects encoded multipart entities and absent/invalid selected boundaries,
preserving the resident traversal's policy. Each active public turn funds
one record, and every retained boundary octet funds one output byte.
Public cached-completion polls and consuming finish freshly check admission.
The value accessor is a passive peek; a recorded failure hides it permanently.
Unspent local interpretation credit is prepaid and forfeited at public finish;
separate entity cursors can spend more records than one resident traversal,
which preserves its original cross-entity credit. No credit is renewed. The resident
traversal keeps its existing outer admission and passive completion policy.
The public interpreter has a compiled 2 KiB owner ceiling, with input backing
reserved separately. Resident traversal still fits its existing 16 KiB
cursor-plus-header-budget ceiling; no heap indirection stores phase state.

Complete MIME traversal now shares one explicit-frame implementation between
resident input and the fragmented structure cursor. Its requests identify
already-clipped entity headers or an exact body offset and end. Headers may
be a retained prefix only when the shared interpreter proves a definitive
boundary; EOF is accepted only for the entire declared entity. Body fragments
are borrowed for one turn and cannot exceed the requested range. Empty
fragments do not invent EOF. Quoted-printable replay may request an earlier
body offset; source retention and funded native reads belong to the caller.

The fragmented cursor binds its original Meter and HeaderBudget on first
admission, keeps interpretation credit across entities, and rejects replacement
owners before debit. Its polls and consuming finish are fresh even after
completion; descriptors remain passive and hidden until the whole tree
succeeds. The resident adapter preserves its passive cached completion API.
Both use identical outer-boundary clipping, digest defaults, depth/part/header
ceilings, decoded leaf sizing and diagnostics. No attached-message recursion
or source authority is introduced.

Each multipart frame now owns the shared bounded delimiter Scanner. Boundary
retention adds its one record and boundary-length I/O charge. After a closing
delimiter, traversal drains the epilogue through the declared body end before
finishing that scanner; epilogue events do not create children or diagnostics.
This adds funded turns and visits compared with the former resident early
exit, while preserving tree contents. Parent scanners retain consumed offsets,
not native read-ahead positions, so an adapter may discard and reread unused
input when switching entities. Per-turn parser bounds remain unchanged.
The shared cursor plus HeaderBudget and the resident adapter plus HeaderBudget
both fit the existing 16 KiB ceiling; descriptor/header/native-read backing is
reserved separately. The shared crate performs no I/O or whole-body allocation.

Every supplied header prefix must remain immutable for the whole fragmented
cursor lifetime, even after its entity phase finishes. A native adapter uses
cumulative disjoint header backing, not a refillable per-entity header buffer.
It reserves and enforces actual retained-prefix capacity separately from the
recognized-header ceiling. Delimiter lookahead and tentative non-header bytes
can exceed recognized-header accounting; no formula using that count alone
proves enough witness storage. Unused arena backing remains reusable for later
entities, and a short arena refuses instead of growing. Body backing alone can
be overwritten immediately after each turn. A complete supplied entity must
carry Eof; a shorter definitive header prefix must carry Prefix. Header or body
range overruns refuse as InvalidRange; inconsistent EOF labels refuse as
IncompleteSource. These passive labels do not authenticate the source.

Part-header metadata and its label/location JSON composition accept retained
header prefixes only when selection proves the entire header section. They
preserve the caller's Prefix/Eof label through all header replays; incomplete
prefixes refuse during polling. No body bytes are needed for filenames, charset, content IDs,
language or location. Whole-entity authorization remains external.
