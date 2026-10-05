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

## Build policy

The mail portable build pins this crate's manifest and lock in
`builder/src/crypto_policy.rs` and refuses an automatic `build.rs`.
Manifest or lock changes require a matching reviewed policy amendment;
ordinary source changes do not change those pins. The crate remains std-only
and shared by other td consumers. Unsafe code is forbidden crate-wide; the
incremental string module denies warnings during Clippy runs.
