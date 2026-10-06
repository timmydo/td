# Shared MIME parsing

`td-mime` owns byte-level MIME and mail-header interpretation. It uses std
and the local td-header, td-json and td-nfc primitives. It has no unsafe
code, cryptographic backend, accounts, file access, network access or
ambient clock. Cold corpus verification uses the engine SHA-256 source;
that verifier is outside the runtime library and adds no Cargo dependency.

The library parses immutable resident bytes and emits bounded cursor turns
into caller-owned storage. Raw bytes remain authoritative. Malformed
content reports parser diagnostics; resource refusal is distinct from
malformed content and never publishes a partial successful property.

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
response publication remain in td-mta. Existing mail module names alias
these shared modules during adapter consolidation. The shared crate owns
no storage format, database index or delivery transaction.
