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
