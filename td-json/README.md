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

## Build policy

The mail portable build pins this crate's manifest and lock in
`builder/src/crypto_policy.rs` and refuses an automatic `build.rs`.
Manifest or lock changes require a matching reviewed policy amendment;
ordinary source changes do not change those pins. The crate remains std-only
and shared by other td consumers. Unsafe code is forbidden crate-wide; the
incremental string module denies warnings during Clippy runs.
