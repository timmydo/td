# td-json

Shared std-only JSON support for td applications. The owned `Json` value,
parser, writer and `json!` macro allocate for their values. They preserve
number lexemes and object insertion order; parsing rejects duplicate keys
and limits nesting to `MAX_DEPTH`.

## Incremental strings

`string::Frame<E>` serializes a caller-owned scalar source into one JSON
string without allocating. It retains six pending bytes, offsets, phase and
an optional sticky error. Its total size depends on the caller's error type
and target layout. The ordinary value writer uses the same scalar escaping.

Implement `string::Source` to provide at most one scalar per poll and an
output-admission policy. Its copyable context is opaque to td-json: callers
may carry a timestamp or other admission context. A zero-byte output charge
must still check live admission. Bound source work separately; the framer
supplies no clock, scheduler, normalization, character filtering or I/O.
All Rust chars, including Unicode noncharacters, can be serialized. Consumers
requiring a stricter character policy must enforce it at the scalar source.

Keep one logical source and admission policy attached to each frame for its
entire lifetime. The API uses short mutable borrows so an owner can retain
source and frame as separate fields; it does not check source identity.
Do not replace the source or wrap an advanced source again after abandonment.

Each poll advances at most one source poll or copies at most six paid bytes.
Quotes, escapes and UTF-8 are charged for their exact serialized length before
publication. Fragmented drains do not repeat those charges. Empty output
only checks admission and returns `NeedOutput`; source yields preserve state.
A cached `Complete` is inert. Use `check_admission` for a final live check.
Every source or admission error latches, including a failed final check after
completion, and later calls return it without consulting the source.

Output is provisional until successful completion and final admission. Any
refusal invalidates all output for that value; retention and atomic publication
belong to the caller. Framing provides neither a whole response transaction
nor a parser for incoming JSON.

The library tests exhaust Unicode scalar escaping and exercise short drains,
source yields, empty output, exact charges, partial output allowances,
independent error latching and final admission. td-mta separately qualifies
its budget adapters, fixed memory ceilings and allocation-free composed paths.

## Incremental string arrays

`string_array::Frame<E>` composes the same string framer with bounded array
punctuation. Its `Source` emits `Begin`, zero or more `Scalar(char)` events,
then `End` for each string, and `Complete` for the whole array. `Yield` may
occur between events. Empty arrays and empty strings are supported. The frame
rejects scalars/end outside a string, nested begin, and whole-array completion
inside an unfinished string, retiring the complete result.

Array punctuation and per-string work carry an explicit `Role` into source
callbacks, so consumers can preserve contextual error policy while sharing
one original source and admission owner. A role is no new allowance or source.

One poll advances at most one source poll, one shared string turn, or one
punctuation byte. Quotes, commas and brackets spend exact serialized-length
output admission before copying; short drains do not repay them. Empty output
performs fresh admission without advancing input. Cached `Complete` is inert;
`check_admission` performs the final live check and can retire completion.
`is_complete` is false after every refusal. Later polls/checks return the
same sticky error without consulting the source. Source and admission errors
retain their original typed cause; protocol errors use `InvalidState(Role)` to preserve their context.

Keep one logical source and policy attached across all strings and final
admission. The frame owns one optional fixed string frame, phase, first-item
flag and sticky error; it allocates no value/list storage. Size depends on
error/target layout. A small-error fixture pins the frame within 96 bytes;
that is not a bound on arbitrary caller error types. Per-string frames restart
only after a balanced end and complete serialization of that string. All
array bytes remain provisional through whole-array completion and final live
admission; source bounds, source identity, retention and atomic publication
remain caller responsibilities. No normalization or character filtering is
added to ordinary JSON escaping.

Fixtures check literal arrays, empty/multiple strings, selected Unicode and
control escapes, short/empty drains, exact charges, malformed event sequences,
every output/source/admission cut, mid-string yields, callback roles and
fresh retirement at every progress cut.
They reuse the shared scalar escaper rather than adding another serializer.

## Caller-reserved fragment retention

`retain::Window` owns one caller-reserved mutable backing and a checked
prefix length. `tail` supplies only the remaining storage; `advance` records
the producer's actually written bytes. Empty/full storage refuses further
turns with `Capacity`, and oversized/overflow reports latch `InvalidState`.
Writing capacity/progress refusals are sticky. `provisional` hides the prefix after refusal;
`into_slice` consumes the same backing identity and returns the retained
prefix without allocating. The type is neither Copy nor Clone and keeps
within four machine words. It supplies no writer, JSON validation, source,
work allowance, presence/null policy or completion/publication authority.
Bind it to one producer throughout; the enclosing owner must prove whole
producer completion and fresh final admission before retaining a result.
Provisional bytes are not wiped on refusal.

`string::capacity_bound` computes at most six wire bytes per UTF-8 source
byte plus two quotes, with checked overflow. A transforming source must
independently prove any bound it uses for its projected output. The sizing
helper grants neither backing allocation nor serialized output allowance.
Fixtures pin fragmented backing identity/prefixes, untouched tail bytes,
empty/exact-full retention, oversized/overflow sticky refusal, capacity
extremes and escaping bounds.

## Build policy

The mail portable build pins this crate's manifest and lock in
`builder/src/crypto_policy.rs` and refuses an automatic `build.rs`.
Manifest or lock changes require a matching reviewed policy amendment;
ordinary source changes do not change those pins. The crate remains std-only
and shared by other td consumers. Unsafe code is forbidden crate-wide; the
incremental string module denies warnings during Clippy runs.

## Borrowed token arena

The `arena` module parses without allocation into caller-owned mutable bytes
and 16-byte token cells. Its `Document` borrows both backings immutably after
in-place string/key decoding; failed parsing invalidates both. Arrays retain
input order, while object iteration sorts decoded keys using token links and
rejects duplicates without a separate key arena. `ParseLimits` bounds bytes,
container depth (capped by `MAX_DEPTH`) and tokens. Inputs
and token storage must each have fewer than `u32::MAX` entries. Errors are
redacted, allocation-free classifications with raw byte offsets; insufficient
caller token storage is distinct from the configured token ceiling. Storage
also reports caller misuse when finish consumes an incomplete cursor. This is
ordinary JSON; protocol-specific I-JSON character restrictions stay with the
caller. There is no heap tree or copying conversion in this API.

`arena::Cursor` advances the same parser in fixed steps of at most 256
transitions, including UTF-8 validation, string decoding, number grammar,
container entry/exit and linked key sorting. Each transition inspects at most
12 input bytes or compares one key byte. Fixed cursor state fits 4 KiB;
caller byte/token arenas are separate. No whole-string validation or sort is
deferred to finish. A partial or failed cursor cannot yield a Document;
errors latch. `arena::parse` drives this cursor synchronously to completion.
Neither API owns a clock, cancellation or a scheduler. A caller checks its
original admission between steps and before using the completed document.
Object lookup, scalar views and JSON writing remain synchronous helpers;
this scheduling bound applies only to cursor parsing.

For bounded protocol validation, `Document::string_bytes_at` visits one
indexed token, `Value::as_string_bytes` returns one decoded string span,
and `Object::next_bytes` advances one member without UTF-8 rescanning.
Those byte slices contain validated decoded UTF-8 and borrow the immutable
Document backing. A consumer decoding chunks must preserve scalar boundaries;
no owned string or new authority is created. Ordinary string views and
object iteration retain their existing synchronous string conversion.

`OwnedCursor` runs the same generic parser over preallocated boxed byte and
token slices. Moving it between worker turns does not move or copy their
contents. `with_input_length` inspects only the occupied byte prefix and
returns both original backings on constructor refusal; its redacted Refusal
never formats their contents. `finish` returns an `OwnedDocument` on success,
or a Refusal retaining both boxes on failure or premature completion.
The document lends immutable views and recovers its arenas through consuming
`into_buffers`; the parser can also abandon work through `into_buffers`.
No path allocates replacement storage or wipes old contents. Callers must
replace the next occupied prefix and apply their own secret-erasure policy.
The convenience `new` constructor exists only for borrowed storage; owned
pools use `with_input_length`. Both storage forms share the same
4 KiB state bound and step budget. Storage is sealed to mutable slices and
boxed slices, preventing custom backing substitution between parser turns.
Only two borrowed slices or two owned boxes may form a parser; mixed pairs
cannot be constructed.

Document-relative token indices support owned consumers whose state must
survive across worker turns. `value_at`, `member_at` and `next_index` inspect
one token/member or traversal link without scanning. Array walkers must also
retain the element count: the last element can continue past its container
within the document. Object key links are sorted and terminated. `Value::token_index` and
`first_child_index` provide positions from borrowed views. A member index
names its key; `value_at` excludes key tokens. Out-of-range positions refuse.
Positions are not capabilities or persistent IDs: a consumer must retain
indices with the same immutable document and discard them when recycling its
arenas. Reusing a position with another document selects that document's
position and carries no source-identity guarantee.

OwnedDocument retains its exact parser ceilings and exposes token capacity
for a protocol handoff to verify the configured resource plan. These are
observations of successful parsing, not new admission or deadline grants.

## Owned arena emission

`arena_emit::OwnedEmitter` keeps one immutable OwnedDocument and a fixed
writer with at most MAX_DEPTH container frames. It serializes a selected
value using the shared scalar escaper, preserving number lexemes, array order
and the arena's decoded-key object order. There is no recursive traversal,
heap tree, whole-string UTF-8 scan or allocation. A number byte view exposes
only parser-validated ASCII without rescanning its complete lexeme.

Each poll performs at most 256 transitions. One transition copies at most
256 number bytes or five literal bytes, or advances at most 32 shared string-frame
steps (one scalar or at most six pending bytes each). Small or empty caller output
buffers never hide a drain or unbounded scan. Complete is cached; failures
latch. Total includes staged string bytes and equals emitted length only at
completion. A per-value byte cap charges the same encoder that emits output;
it is not external storage, time or work admission.

The owner fits 8 KiB, excluding its existing byte/token boxes and caller
output. It can move between worker turns and lend immutable views without
moving its source. Successful completion permits restarting another value
from that document with a fresh per-value limit; external admission is never
renewed. Constructor/completion refusals retain the document. Abandonment
returns it unchanged, and its consuming into_buffers recovers the arenas.
All emitted bytes remain provisional until successful completion and the
caller's final live admission check. A failure retires the entire value,
including output from earlier polls; no partial success is implied.
